#![cfg(any(target_os = "linux", target_os = "macos"))]

use agentfs_sdk::error::{Error, Result};
use agentfs_sdk::{AgentFS, AgentFSOptions, BoxedFile, FileSystem, HostFS, OverlayFS, Stats};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::{mpsc, Arc};
use std::time::Duration;

macro_rules! errnos {
    ($($name:ident),*) => { [$((stringify!($name), libc::$name)),*] };
}
const CASES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../conformance/overlay/cases.json"
);
const ERRNOS: [(&str, i32); 12] = errnos![
    ENOENT, EEXIST, ENOTEMPTY, ENOTDIR, EISDIR, EINVAL, ELOOP, EXDEV, ENOTSUP, EBUSY, ESTALE,
    ENOSYS
];

fn err(errno: i32) -> Error {
    std::io::Error::from_raw_os_error(errno).into()
}

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

fn build_base(root: &Path, entries: &[Value]) {
    std::fs::create_dir(root).unwrap();
    for entry in entries {
        let w: Vec<&str> = entry.as_str().unwrap().splitn(4, ' ').collect();
        let path = root.join(&w[0][1..]);
        match w[1] {
            "f" => std::fs::write(&path, w[3]).unwrap(),
            "d" => std::fs::create_dir(&path).unwrap(),
            "l" => std::os::unix::fs::symlink(w[2], &path).unwrap(),
            _ => drop(Command::new("mkfifo").arg(&path).status().unwrap()),
        }
        if matches!(w[1], "f" | "d") {
            let mode = u32::from_str_radix(w[2], 8).unwrap();
            std::fs::set_permissions(&path, PermissionsExt::from_mode(mode)).unwrap();
        }
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
    let mut parts: VecDeque<String> = path
        .split('/')
        .filter(|c| !c.is_empty())
        .map(Into::into)
        .collect();
    let root = fs.getattr(1).await?.unwrap();
    let (mut dir, mut links) = (root.clone(), 0);
    while let Some(name) = parts.pop_front() {
        let stats = fs.lookup(dir.ino, &name).await?.ok_or(err(libc::ENOENT))?;
        if stats.is_symlink() && (follow_last || !parts.is_empty()) {
            let target = fs.readlink(stats.ino).await?.unwrap_or_default();
            links += 1;
            if links > 40 {
                return Err(err(libc::ELOOP));
            }
            if target.starts_with('/') {
                dir = root.clone();
            }
            target
                .rsplit('/')
                .filter(|c| !c.is_empty())
                .for_each(|c| parts.push_front(c.into()));
        } else if parts.is_empty() {
            return Ok(stats);
        } else if stats.is_directory() {
            dir = stats;
        } else {
            return Err(err(libc::ENOTDIR));
        }
    }
    Ok(dir)
}

/// Returns the inode of the merged parent directory of `path` and its last component.
async fn parent<'a>(fs: &OverlayFS, path: &'a str) -> Result<(i64, &'a str)> {
    let (dir, name) = path.rsplit_once('/').unwrap();
    Ok((resolve(fs, dir, true).await?.ino, name))
}

async fn slurp(file: &BoxedFile) -> Result<String> {
    let data = file.pread(0, file.fstat().await?.size as u64).await?;
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// Runs one step. Steps that the Rust SDK cannot serve yet (the management
/// operations and `diff`) fail with `ENOSYS`.
async fn run_step(
    fs: &OverlayFS,
    base: &Path,
    hs: &mut HashMap<String, BoxedFile>,
    w: &[&str],
) -> Result<String> {
    let ino = |i: usize, follow: bool| async move {
        Ok::<_, Error>(resolve(fs, w[i], follow).await?.ino)
    };
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
            let entry = list
                .unwrap_or_default()
                .into_iter()
                .find(|e| e.name == w[2]);
            out = field(&entry.ok_or(err(libc::ENOENT))?.stats, w[3]);
        }
        "read" => out = slurp(&fs.open(ino(1, true).await?, libc::O_RDONLY).await?).await?,
        "readlink" => out = fs.readlink(ino(1, false).await?).await?.unwrap_or_default(),
        "hostRead" => out = std::fs::read_to_string(base.join(&w[1][1..]))?,
        "hread" => out = slurp(&hs[w[1]]).await?,
        "open" => {
            let file = fs.open(ino(2, true).await?, libc::O_RDWR).await?;
            hs.insert(w[1].into(), file);
        }
        "write" => {
            let file = match ino(1, true).await {
                Ok(ino) => fs.open(ino, libc::O_RDWR).await?,
                Err(_) => {
                    let (dir, name) = parent(fs, w[1]).await?;
                    fs.create_file(dir, name, 0o100644, 0, 0).await?.1
                }
            };
            file.truncate(0).await?;
            file.pwrite(0, w[2].as_bytes()).await?;
        }
        "chmod" => {
            let mode = u32::from_str_radix(w[2], 8).unwrap();
            fs.chmod(ino(1, true).await?, mode).await?;
        }
        "mkdir" | "rmdir" | "unlink" => {
            let (dir, name) = parent(fs, w[1]).await?;
            match w[0] {
                "mkdir" => _ = fs.mkdir(dir, name, 0o755, 0, 0).await?,
                "rmdir" => fs.rmdir(dir, name).await?,
                _ => fs.unlink(dir, name).await?,
            }
        }
        "symlink" => {
            let (dir, name) = parent(fs, w[2]).await?;
            fs.symlink(dir, name, w[1], 0, 0).await?;
        }
        "link" => {
            let (dir, name) = parent(fs, w[2]).await?;
            fs.link(ino(1, false).await?, dir, name).await?;
        }
        "rename" => {
            let (from, to) = (parent(fs, w[1]).await?, parent(fs, w[2]).await?);
            fs.rename(from.0, from.1, to.0, to.1).await?;
        }
        _ => return Err(err(libc::ENOSYS)),
    }
    Ok(out)
}

/// Runs the steps of one case over a fresh temporary base and overlay database.
async fn run_steps(case: &Value, base_entries: &[Value]) -> std::result::Result<(), String> {
    let dir = tempfile::tempdir().unwrap();
    build_base(&dir.path().join("base"), base_entries);
    let base = dir.path().join("base").canonicalize().unwrap();
    let db = dir.path().join("overlay.db");
    let mut fs = open_overlay(&base, &db).await;
    let (mut handles, mut vars) = (HashMap::new(), HashMap::<&str, String>::new());
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
            handles.clear();
            drop(fs);
            fs = open_overlay(&base, &db).await;
            continue;
        }
        let got = run_step(&fs, &base, &mut handles, &words).await;
        let out = got.as_ref().map_or_else(errno_name, |v| v.clone());
        let want = want.map(|w| {
            vars.iter()
                .fold(w.to_string(), |w, (k, v)| w.replace(&format!("${k}"), v))
        });
        let ok = match want {
            None => got.is_ok(),
            Some(w) => w.strip_prefix('!').map_or(out == w, |n| out != n),
        };
        if !ok {
            return Err(format!("`{step}` gave `{out}`"));
        }
        vars.extend(var.map(|v| (v, out)));
    }
    Ok(())
}

/// Runs a case on its own thread, so that a hang fails the case and not the suite.
fn run_case(case: &Value, base_entries: &[Value]) -> std::result::Result<(), String> {
    let (case, base_entries) = (case.clone(), base_entries.to_vec());
    let timeout = Duration::from_secs(case["timeout"].as_u64().unwrap_or(30));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _ = tx.send(rt.block_on(run_steps(&case, &base_entries)));
    });
    rx.recv_timeout(timeout)
        .unwrap_or_else(|e| Err(e.to_string()))
}

#[test]
fn overlay_conformance() {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(CASES).unwrap()).unwrap();
    let (mut pass, mut xfail, mut failures) = (0, Vec::new(), Vec::new());
    for case in doc["cases"].as_array().unwrap() {
        let (id, o) = (&case["id"], &case["xfail"]);
        let owned = o.as_array().is_some_and(|a| !a.is_empty());
        match (run_case(case, doc["base"].as_array().unwrap()), owned) {
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
