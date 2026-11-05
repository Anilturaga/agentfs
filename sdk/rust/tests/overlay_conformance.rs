#![cfg(any(target_os = "linux", target_os = "macos"))]

use agentfs_sdk::{error::*, *};
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

macro_rules! errnos {
    ($($name:ident),*) => { [$((stringify!($name), libc::$name)),*] };
}
const CASES: &str = include_str!("../../../conformance/overlay/cases.json");
const ERRNOS: [(&str, i32); 11] = errnos![
    ENOENT, EEXIST, ENOTEMPTY, ENOTDIR, EISDIR, EINVAL, ELOOP, EXDEV, ENOTSUP, EBUSY, ESTALE
];

fn errno_name(e: &Error) -> String {
    let code = match e {
        Error::Fs(f) => Some(f.to_errno()),
        Error::Io(io) => io.raw_os_error(),
        _ => None,
    };
    let known = ERRNOS.iter().find(|(_, c)| Some(*c) == code);
    known.map_or_else(|| e.to_string(), |(name, _)| name.to_string())
}

fn field(s: &Stats, name: &str) -> String {
    match name {
        "mode" => format!("{:o}", s.mode & 0o7777),
        "size" => s.size.to_string(),
        "ino" => s.ino.to_string(),
        _ if s.is_file() => "f".into(),
        _ if s.is_directory() => "d".into(),
        _ => if s.is_symlink() { "l" } else { "?" }.into(),
    }
}

async fn open_overlay(base: &Path, db: &Path) -> OverlayFS {
    let options = AgentFSOptions::with_path(db.to_str().unwrap()).with_base(base);
    let agent = AgentFS::open(options).await.unwrap();
    let overlay = OverlayFS::new(Arc::new(HostFS::new(base).unwrap()), agent.fs);
    overlay.load().await.unwrap();
    overlay
}

/// Resolves `path` in the merged view. Symlinks are followed at every component,
/// and at the last one only if `follow_last` is set.
async fn resolve(fs: &OverlayFS, path: &str, follow_last: bool) -> Result<Stats> {
    let mut parts: Vec<String> = Vec::new();
    parts.extend(path.rsplit('/').filter(|c| !c.is_empty()).map(Into::into));
    let (mut dir, mut links) = (1, 0);
    while let Some(name) = parts.pop() {
        let stats = fs.lookup(dir, &name).await?.ok_or(FsError::NotFound)?;
        if stats.is_symlink() && (follow_last || !parts.is_empty()) {
            let target = fs.readlink(stats.ino).await?.unwrap_or_default();
            links += 1;
            if links > 40 {
                return Err(FsError::SymlinkLoop.into());
            }
            if target.starts_with('/') {
                dir = 1;
            }
            parts.extend(target.rsplit('/').filter(|c| !c.is_empty()).map(Into::into));
        } else if parts.is_empty() {
            return Ok(stats);
        } else if stats.is_directory() {
            dir = stats.ino;
        } else {
            return Err(FsError::NotADirectory.into());
        }
    }
    Ok(fs.getattr(dir).await?.unwrap())
}

async fn parent<'a>(fs: &OverlayFS, path: &'a str) -> Result<(i64, &'a str)> {
    let (dir, name) = path.rsplit_once('/').unwrap();
    Ok((resolve(fs, dir, true).await?.ino, name))
}

async fn slurp(file: &BoxedFile) -> Result<String> {
    let size = file.fstat().await?.size as u64;
    Ok(String::from_utf8_lossy(&file.pread(0, size).await?).into())
}

/// Runs one step. Steps the Rust SDK cannot serve yet (management operations, `diff`) fail with `ENOSYS`.
async fn run_step(fs: &OverlayFS, h: &mut Option<BoxedFile>, w: &[&str]) -> Result<String> {
    let ino = |i, f| async move { Ok::<_, Error>(resolve(fs, w[i], f).await?.ino) };
    let mut out = String::new();
    match w[0] {
        "readdir" => {
            let names = fs.readdir(ino(1, true).await?).await?;
            out = names.unwrap_or_default().join(" ");
        }
        "stat" | "lstat" => {
            let stats = resolve(fs, w[1], w[0] == "stat").await?;
            out = field(&stats, w.get(2).unwrap_or(&"type"));
        }
        "plus" => {
            let list = fs.readdir_plus(ino(1, true).await?).await?;
            let list = list.unwrap_or_default();
            let entry = list.iter().find(|e| e.name == w[2]);
            out = field(&entry.ok_or(FsError::NotFound)?.stats, w[3]);
        }
        "read" => out = slurp(&fs.open(ino(1, true).await?, libc::O_RDONLY).await?).await?,
        "readlink" => out = fs.readlink(ino(1, false).await?).await?.unwrap_or_default(),
        "hread" => out = slurp(h.as_ref().unwrap()).await?,
        "hostRead" => out = std::fs::read_to_string(&w[1][1..])?,
        "open" => *h = Some(fs.open(ino(1, true).await?, libc::O_RDWR).await?),
        "write" => {
            let (dir, name) = parent(fs, w[1]).await?;
            let file = match ino(1, true).await {
                Ok(ino) => fs.open(ino, libc::O_RDWR).await?,
                Err(_) => fs.create_file(dir, name, 0o100644, 0, 0).await?.1,
            };
            file.truncate(0).await?;
            file.pwrite(0, w[2].as_bytes()).await?;
        }
        "chmod" => {
            let mode = u32::from_str_radix(w[2], 8).unwrap();
            fs.chmod(ino(1, true).await?, mode).await?;
        }
        "mkdir" | "rmdir" | "unlink" | "symlink" | "link" | "rename" => {
            let (dir, name) = parent(fs, w[w.len() - 1]).await?;
            match w[0] {
                "mkdir" => _ = fs.mkdir(dir, name, 0o755, 0, 0).await?,
                "rmdir" => fs.rmdir(dir, name).await?,
                "unlink" => fs.unlink(dir, name).await?,
                "symlink" => _ = fs.symlink(dir, name, w[1], 0, 0).await?,
                "link" => _ = fs.link(ino(1, false).await?, dir, name).await?,
                _ => {
                    let from = parent(fs, w[1]).await?;
                    fs.rename(from.0, from.1, dir, name).await?
                }
            }
        }
        _ => return Err(Error::Internal("ENOSYS".into())),
    }
    Ok(out)
}

/// `!x` needs a successful step with a result other than `x`; else `want` is the result or error name.
fn satisfied(want: Option<&str>, step_ok: bool, out: &str) -> bool {
    match want.map(|w| w.strip_prefix('!').ok_or(w)) {
        None => step_ok,
        Some(Err(w)) => out == w,
        Some(Ok(n)) => step_ok && out != n,
    }
}

async fn run_steps(case: &Value, script: &str) -> std::result::Result<(), String> {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    std::fs::create_dir(&base).unwrap();
    std::env::set_current_dir(&base).unwrap();
    let setup = Command::new("sh").args(["-ec", script]).status().unwrap();
    assert!(setup.success(), "base setup failed");
    let db = dir.path().join("overlay.db");
    let mut fs = open_overlay(&base, &db).await;
    let (mut handle, mut vars) = (None, Vec::<(&str, String)>::new());
    for step in case["steps"].as_array().unwrap() {
        let step = step.as_str().unwrap();
        let (cmd, want) = step
            .split_once("=>")
            .map_or((step, None), |(c, w)| (c, Some(w.trim())));
        let (var, cmd) = cmd
            .split_once(" = ")
            .map_or((None, cmd), |(v, c)| (Some(v), c));
        let words: Vec<&str> = cmd.split_whitespace().collect();
        if words[0] == "reopen" {
            handle = None;
            drop(fs);
            fs = open_overlay(&base, &db).await;
            continue;
        }
        let got = run_step(&fs, &mut handle, &words).await;
        let out = got.as_ref().map_or_else(errno_name, |v| v.clone());
        let want = want.map(|w| {
            vars.iter()
                .fold(w.to_string(), |w, (k, v)| w.replace(&format!("${k}"), v))
        });
        if !satisfied(want.as_deref(), got.is_ok(), &out) {
            return Err(format!("`{step}` gave `{out}`"));
        }
        vars.extend(var.map(|v| (v, out)));
    }
    Ok(())
}

/// Runs `cmd` in its own process group and kills the group at the deadline and after the
/// child exits. Exit code 1 is a failed case; any other failure is a runner error.
fn run_group(cmd: &mut Command, secs: u64) -> std::result::Result<(), String> {
    let log = tempfile::NamedTempFile::new().unwrap();
    cmd.process_group(0).stdout(Stdio::null());
    cmd.stderr(log.reopen().unwrap());
    let mut child = cmd.spawn().unwrap();
    let (deadline, mut status) = (Instant::now() + Duration::from_secs(secs), None);
    while status.is_none() && Instant::now() < deadline {
        status = child.try_wait().unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    child.wait().unwrap();
    let msg = std::fs::read_to_string(log.path()).unwrap();
    match status.map(|s| s.code()) {
        None => Err("timed out".into()),
        Some(Some(0)) => Ok(()),
        Some(Some(1)) => Err(msg.trim().into()),
        _ => panic!("runner error: {}", msg.trim()),
    }
}

#[test]
fn overlay_conformance() {
    let doc: Value = serde_json::from_str(CASES).unwrap();
    if let Ok(id) = std::env::var("CONFORMANCE_CASE") {
        let case = &doc["cases"][id.parse::<usize>().unwrap()];
        let rt = tokio::runtime::Runtime::new().unwrap();
        if let Err(e) = rt.block_on(run_steps(case, doc["base"].as_str().unwrap())) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    let (mut pass, mut xfail, mut failures) = (0, Vec::new(), Vec::new());
    for (index, case) in doc["cases"].as_array().unwrap().iter().enumerate() {
        let (id, o) = (&case["id"], &case["xfail"]);
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "overlay_conformance", "--nocapture"]);
        let secs = case["timeout"].as_u64().unwrap_or(30);
        let ran = run_group(cmd.env("CONFORMANCE_CASE", index.to_string()), secs);
        match (ran, o.is_array()) {
            (Ok(()), false) => pass += 1,
            (Err(e), true) => xfail.push(format!("case {id} xfail {o}: {e}")),
            (Ok(()), true) => failures.push(format!("case {id} passes: remove xfail {o}")),
            (Err(e), false) => failures.push(format!("case {id} fails: {e}")),
        }
    }
    let (nx, nf) = (xfail.len(), failures.len());
    println!("{}\npass {pass}, xfail {nx}, fail {nf}", xfail.join("\n"));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn inequality_needs_a_successful_step() {
    assert!(!satisfied(Some("!1"), false, "ENOENT"));
    assert!(satisfied(Some("!1"), true, "2"));
}

#[test]
fn timeout_kills_the_whole_process_group() {
    let marker = tempfile::NamedTempFile::new().unwrap();
    let script = format!("(sleep 2; echo late > {}) & wait", marker.path().display());
    let start = Instant::now();
    let result = run_group(Command::new("sh").args(["-c", &script]), 1);
    assert!(result == Err("timed out".into()) && start.elapsed() < Duration::from_secs(2));
    std::thread::sleep(Duration::from_secs(3));
    assert!(std::fs::read(marker.path()).unwrap().is_empty());
}
