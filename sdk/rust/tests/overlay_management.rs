#![cfg(any(target_os = "linux", target_os = "macos"))]

use agentfs_sdk::{
    create_overlay, delete_overlay,
    error::{Error, Result},
    list_overlays, open_merged, rebind_overlay, AgentFS, AgentFSOptions, Base, BoxedFile, DirEntry,
    FileSystem, FilesystemStats, HostFS, PathFS, Stats, SyncOptions, TimeChange,
};
use async_trait::async_trait;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn opts(path: &Path) -> AgentFSOptions {
    AgentFSOptions::with_path(path.to_str().unwrap())
}

fn errno<T>(result: Result<T>) -> Option<i32> {
    result.err().and_then(|e| e.errno())
}

/// `None` when the call failed with `EBUSY`, which the caller retries.
fn retried<T>(result: Result<T>) -> Option<Result<T>> {
    match result {
        Err(e) if e.errno() == Some(libc::EBUSY) => None,
        other => Some(other),
    }
}

/// The base directory `name` under `dir`, holding the file `f`.
fn make_base(dir: &Path, name: &str) -> Base {
    let path = dir.join(name);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("f"), "base").unwrap();
    Base::host(&path).unwrap()
}

async fn overlay(dir: &Path, name: &str) -> (PathBuf, Base) {
    let (db, base) = (dir.join(name), make_base(dir, "base"));
    create_overlay(opts(&db), &base).await.unwrap();
    (db, base)
}

/// The test binary, run again as a helper process: `helper` does the work named by the
/// `OVERLAY_HELPER` variable and does nothing when it is unset.
fn spawn(mode: &str, db: &Path, base: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "helper", "--nocapture"])
        .env("OVERLAY_HELPER", mode)
        .env("OVERLAY_DB", db)
        .env("OVERLAY_BASE", base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Waits until the helper prints `line`.
fn wait_for(child: &mut Child, line: &str) {
    let mut out = BufReader::new(child.stdout.as_mut().unwrap());
    let mut text = String::new();
    while text.trim() != line {
        text.clear();
        assert!(out.read_line(&mut text).unwrap() > 0, "helper ended early");
    }
}

fn stdin(child: &mut Child) -> &mut ChildStdin {
    child.stdin.as_mut().unwrap()
}

fn finish(mut child: Child) {
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

fn lock(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) == 0 }
}

/// Whether another process can lock the database file now.
fn child_can_lock(db: &Path) -> bool {
    let status = spawn("trylock", db, db).wait().unwrap();
    status.code() == Some(0)
}

#[test]
fn helper() {
    let Ok(mode) = std::env::var("OVERLAY_HELPER") else {
        return;
    };
    let (db, base) = (
        std::env::var("OVERLAY_DB").unwrap(),
        std::env::var("OVERLAY_BASE").unwrap(),
    );
    let rt = tokio::runtime::Runtime::new().unwrap();
    let wait_for_input = || std::io::stdin().lines().for_each(drop);
    let file = || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&db)
            .unwrap()
    };
    match mode.as_str() {
        "trylock" => std::process::exit(if lock(&file()) { 0 } else { 2 }),
        "hold" => {
            let _held = rt
                .block_on(open_merged(opts(Path::new(&db)), None))
                .unwrap();
            println!("ready");
            wait_for_input();
        }
        "pause_delete" => {
            let held = file();
            assert!(lock(&held));
            println!("ready");
            std::io::stdin().lines().next();
            for suffix in [
                "-wal",
                "-shm",
                "-log",
                "-wal-revert",
                "-info",
                "-changes",
                "",
            ] {
                let _ = std::fs::remove_file(format!("{db}{suffix}"));
            }
            println!("removed");
            wait_for_input();
        }
        "read_added" => {
            let merged = rt
                .block_on(open_merged(opts(Path::new(&db)), None))
                .unwrap();
            let paths = PathFS(Arc::new(merged.fs.clone()));
            let data = rt.block_on(paths.read_file("/added")).unwrap();
            println!(
                "{}",
                data.map_or("missing".to_string(), |d| String::from_utf8(d).unwrap())
            );
        }
        "open_loop" => loop {
            let opened = rt.block_on(open_merged(
                opts(Path::new(&db)),
                Some(Base::host(&base).unwrap()),
            ));
            if opened.is_ok() {
                println!("opened");
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        },
        _ => unreachable!(),
    }
}

/// A lower layer that counts the directory entries it returns.
struct Counting(HostFS, Arc<AtomicUsize>);

#[async_trait]
impl FileSystem for Counting {
    async fn lookup(&self, parent: i64, name: &str) -> Result<Option<Stats>> {
        self.0.lookup(parent, name).await
    }
    async fn getattr(&self, ino: i64) -> Result<Option<Stats>> {
        self.0.getattr(ino).await
    }
    async fn readlink(&self, ino: i64) -> Result<Option<String>> {
        self.0.readlink(ino).await
    }
    async fn readdir(&self, ino: i64) -> Result<Option<Vec<String>>> {
        let names = self.0.readdir(ino).await?;
        self.1
            .fetch_add(names.as_ref().map_or(0, Vec::len), Ordering::Relaxed);
        Ok(names)
    }
    async fn readdir_plus(&self, ino: i64) -> Result<Option<Vec<DirEntry>>> {
        let entries = self.0.readdir_plus(ino).await?;
        self.1
            .fetch_add(entries.as_ref().map_or(0, Vec::len), Ordering::Relaxed);
        Ok(entries)
    }
    async fn chmod(&self, ino: i64, mode: u32) -> Result<()> {
        self.0.chmod(ino, mode).await
    }
    async fn chown(&self, ino: i64, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        self.0.chown(ino, uid, gid).await
    }
    async fn utimens(&self, ino: i64, atime: TimeChange, mtime: TimeChange) -> Result<()> {
        self.0.utimens(ino, atime, mtime).await
    }
    async fn open(&self, ino: i64, flags: i32) -> Result<BoxedFile> {
        self.0.open(ino, flags).await
    }
    async fn mkdir(&self, p: i64, name: &str, mode: u32, uid: u32, gid: u32) -> Result<Stats> {
        self.0.mkdir(p, name, mode, uid, gid).await
    }
    async fn create_file(
        &self,
        p: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(Stats, BoxedFile)> {
        self.0.create_file(p, name, mode, uid, gid).await
    }
    async fn mknod(
        &self,
        p: i64,
        name: &str,
        mode: u32,
        rdev: u64,
        uid: u32,
        gid: u32,
    ) -> Result<Stats> {
        self.0.mknod(p, name, mode, rdev, uid, gid).await
    }
    async fn symlink(&self, p: i64, name: &str, target: &str, uid: u32, gid: u32) -> Result<Stats> {
        self.0.symlink(p, name, target, uid, gid).await
    }
    async fn unlink(&self, parent: i64, name: &str) -> Result<()> {
        self.0.unlink(parent, name).await
    }
    async fn rmdir(&self, parent: i64, name: &str) -> Result<()> {
        self.0.rmdir(parent, name).await
    }
    async fn link(&self, ino: i64, parent: i64, name: &str) -> Result<Stats> {
        self.0.link(ino, parent, name).await
    }
    async fn rename(&self, p: i64, name: &str, new_p: i64, new_name: &str) -> Result<()> {
        self.0.rename(p, name, new_p, new_name).await
    }
    async fn statfs(&self) -> Result<FilesystemStats> {
        self.0.statfs().await
    }
}

#[tokio::test]
async fn diff_reads_under_100_base_entries() {
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path().join("base/files");
    std::fs::create_dir_all(&files).unwrap();
    for i in 0..10_000 {
        std::fs::write(files.join(i.to_string()), "x").unwrap();
    }
    let host = Base::host(dir.path().join("base")).unwrap();
    let db = dir.path().join("o.db");
    create_overlay(opts(&db), &host).await.unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let lower = Counting(HostFS::new(&host.path).unwrap(), seen.clone());
    let base = Base {
        fs: Arc::new(lower),
        ..host
    };
    let merged = open_merged(opts(&db), Some(base)).await.unwrap();
    let paths = PathFS(Arc::new(merged.fs.clone()));
    paths.write_file("/files/0", b"new").await.unwrap();
    paths.remove("/files/1").await.unwrap();

    seen.store(0, Ordering::Relaxed);
    let changes = merged.diff().await.unwrap();
    let text: Vec<String> = changes.iter().map(ToString::to_string).collect();
    assert_eq!(text, ["M d /files", "M f /files/0", "D f /files/1"]);
    assert!(seen.load(Ordering::Relaxed) < 100);
}

#[tokio::test]
async fn listing() {
    let dir = tempfile::tempdir().unwrap();
    let (held, base) = overlay(dir.path(), "held.db").await;
    let (idle, _) = overlay(dir.path(), "idle.db").await;
    std::fs::write(dir.path().join("garbage.db"), "not a database").unwrap();
    std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
    let key = "b1bbfda4f589dc9daaf004fe21111e00dc00c98237102f5c7002a5669fc76327";
    let encrypted = opts(&dir.path().join("secret.db")).with_encryption_key(key, "aegis256");
    drop(AgentFS::open(encrypted).await.unwrap());

    let merged = open_merged(opts(&held), None).await.unwrap();
    let found = list_overlays(dir.path(), None).await.unwrap();
    let paths: Vec<_> = found.iter().map(|i| i.path.clone()).collect();
    assert_eq!(paths, [held.clone(), idle]);
    assert_eq!(found[0].base_path, base.path);
    assert_eq!(found[0].version, 1);
    let other = make_base(dir.path(), "other");
    assert!(list_overlays(dir.path(), Some(&other))
        .await
        .unwrap()
        .is_empty());

    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::hard_link(&held, elsewhere.join("alias.db")).unwrap();
    let found = list_overlays(&elsewhere, None).await.unwrap();
    assert_eq!(found[0].path, elsewhere.join("alias.db"));
    drop(merged);
}

#[tokio::test]
async fn hard_link_alias_is_busy() {
    let dir = tempfile::tempdir().unwrap();
    let (db, base) = overlay(dir.path(), "o.db").await;
    let alias = dir.path().join("alias.db");
    std::fs::hard_link(&db, &alias).unwrap();
    let merged = open_merged(opts(&db), None).await.unwrap();
    assert_eq!(
        errno(open_merged(opts(&alias), None).await),
        Some(libc::EBUSY)
    );
    assert_eq!(errno(delete_overlay(opts(&alias)).await), Some(libc::EBUSY));
    assert_eq!(
        errno(rebind_overlay(opts(&alias), &base).await),
        Some(libc::EBUSY)
    );
    drop(merged);
}

#[tokio::test]
async fn path_fs_resolves_dot_dot_and_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let agent = AgentFS::open(opts(&dir.path().join("p.db"))).await.unwrap();
    let fs = Arc::new(agent.fs.clone());
    let paths = PathFS(fs.clone());
    paths.mkdir("/a").await.unwrap();
    paths.mkdir("/a/b").await.unwrap();
    paths.write_file("/a/b/f", b"data").await.unwrap();
    let a = paths.resolve("/a", true).await.unwrap().ino;
    FileSystem::symlink(&*fs, a, "rel", "b", 0, 0)
        .await
        .unwrap();
    FileSystem::symlink(&*fs, 1, "abs", "/a/b", 0, 0)
        .await
        .unwrap();
    FileSystem::symlink(&*fs, 1, "loop", "loop", 0, 0)
        .await
        .unwrap();
    FileSystem::symlink(&*fs, a, "dangling", "b/made", 0, 0)
        .await
        .unwrap();
    FileSystem::symlink(&*fs, 1, "dangling_abs", "/a/b/made_abs", 0, 0)
        .await
        .unwrap();

    let read = |path: &'static str| paths.read_file(path);
    assert_eq!(read("/a/b/../b/./f").await.unwrap().unwrap(), b"data");
    assert_eq!(read("/../../a/b/f").await.unwrap().unwrap(), b"data");
    assert_eq!(read("/a/rel/f").await.unwrap().unwrap(), b"data");
    assert_eq!(read("/abs/../b/f").await.unwrap().unwrap(), b"data");
    assert!(read("/a/missing").await.unwrap().is_none());
    assert!(paths.resolve("/abs", false).await.unwrap().is_symlink());
    assert!(paths.resolve("/abs", true).await.unwrap().is_directory());
    let err = paths.resolve("/loop", true).await.unwrap_err();
    assert_eq!(err.errno(), Some(libc::ELOOP));
    assert_eq!(
        paths.resolve("/a/b/f/x", true).await.unwrap_err().errno(),
        Some(libc::ENOTDIR)
    );
    paths.write_file("/a/dangling", b"one").await.unwrap();
    paths.write_file("/dangling_abs", b"two").await.unwrap();
    assert_eq!(read("/a/b/made").await.unwrap().unwrap(), b"one");
    assert_eq!(read("/a/b/made_abs").await.unwrap().unwrap(), b"two");
    assert!(paths
        .resolve("/a/dangling", false)
        .await
        .unwrap()
        .is_symlink());
    for links in [39, 40, 41] {
        for i in 0..links {
            let target = if i + 1 < links {
                format!("chain{}_{}", links, i + 1)
            } else {
                format!("end{links}")
            };
            let name = format!("chain{links}_{i}");
            FileSystem::symlink(&*fs, 1, &name, &target, 0, 0)
                .await
                .unwrap();
        }
        let written = paths.write_file(&format!("/chain{links}_0"), b"x").await;
        let expected = (links > 40).then_some(libc::ELOOP);
        assert_eq!(written.err().and_then(|e| e.errno()), expected);
    }
    let into_file = paths.rename("/a/b/f", "/a/b/f/g").await;
    assert_eq!(into_file.unwrap_err().errno(), Some(libc::ENOTDIR));
    assert!(paths.stat("/a/b/f").await.unwrap().is_some());
    paths.rename("/a/b/f", "/g").await.unwrap();
    paths.remove("/g").await.unwrap();
    paths.remove("/abs").await.unwrap();
    assert!(paths.stat("/a/b").await.unwrap().is_some());
}

#[tokio::test]
async fn delete_accepts_an_overlay_with_an_empty_config_table() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = overlay(dir.path(), "o.db").await;
    let raw = turso::Builder::new_local(db.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let conn = raw.connect().unwrap();
    conn.execute("DELETE FROM fs_overlay_config", ())
        .await
        .unwrap();
    drop((conn, raw));
    delete_overlay(opts(&db)).await.unwrap();
    assert!(!db.exists());
}

#[tokio::test]
async fn hard_link_alias_sees_the_data_after_the_handles_drop() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = overlay(dir.path(), "o.db").await;
    let alias = dir.path().join("alias.db");
    std::fs::hard_link(&db, &alias).unwrap();
    let merged = open_merged(opts(&db), None).await.unwrap();
    PathFS(Arc::new(merged.fs.clone()))
        .write_file("/added", b"x")
        .await
        .unwrap();
    drop(merged);

    let mut child = spawn("read_added", &alias, &alias);
    wait_for(&mut child, "x");
    finish(child);
}

#[tokio::test]
async fn failed_new_synced_database_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("new.db");
    let remote = SyncOptions {
        remote_url: Some("http://127.0.0.1:9".to_string()),
        ..Default::default()
    };
    assert!(AgentFS::open(opts(&db).with_sync(remote)).await.is_err());
    assert!(!db.exists());
}

#[tokio::test]
async fn retained_handles_keep_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = overlay(dir.path(), "o.db").await;
    let merged = open_merged(opts(&db), None).await.unwrap();
    let kv = merged.kv.clone();
    let (_, file) = merged.fs.create_file(1, "n", 0o644, 0, 0).await.unwrap();
    drop(merged);
    assert_eq!(errno(open_merged(opts(&db), None).await), Some(libc::EBUSY));
    drop(kv);
    assert_eq!(errno(open_merged(opts(&db), None).await), Some(libc::EBUSY));
    drop(file);
    open_merged(opts(&db), None).await.unwrap();
}

#[tokio::test]
async fn legacy_opener_cannot_drop_the_lock_of_a_live_instance() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = overlay(dir.path(), "o.db").await;
    let merged = open_merged(opts(&db), None).await.unwrap();
    let busy = |e: Error| e.errno() == Some(libc::EBUSY);
    assert!(AgentFS::open(opts(&db)).await.err().is_some_and(busy));
    #[allow(deprecated)]
    let legacy = AgentFS::new(db.to_str().unwrap()).await;
    assert!(legacy.err().is_some_and(busy));
    assert!(agentfs_sdk::filesystem::AgentFS::new(db.to_str().unwrap())
        .await
        .is_err());
    assert!(agentfs_sdk::KvStore::new(db.to_str().unwrap())
        .await
        .is_err());
    assert!(agentfs_sdk::ToolCalls::new(db.to_str().unwrap())
        .await
        .is_err());
    assert!(!child_can_lock(&db));
    // A synced database goes through the same reservation
    std::fs::write(format!("{}-info", db.display()), "{}").unwrap();
    assert!(AgentFS::open(opts(&db)).await.err().is_some_and(busy));
    let remote = SyncOptions {
        remote_url: Some("http://127.0.0.1:9".to_string()),
        ..Default::default()
    };
    let synced = AgentFS::open(opts(&db).with_sync(remote)).await;
    assert!(synced.err().is_some_and(busy));
    assert!(!child_can_lock(&db));
    drop(merged);
    assert!(child_can_lock(&db));
}

#[tokio::test]
async fn close_and_reopen_release_the_lock_each_time() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = overlay(dir.path(), "o.db").await;
    for _ in 0..5 {
        let merged = open_merged(opts(&db), None).await.unwrap();
        assert!(!child_can_lock(&db));
        merged.close();
        assert!(child_can_lock(&db));
    }
}

#[tokio::test]
async fn child_holds_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let (db, base) = overlay(dir.path(), "o.db").await;
    let mut child = spawn("hold", &db, Path::new(&base.path));
    wait_for(&mut child, "ready");
    assert_eq!(errno(open_merged(opts(&db), None).await), Some(libc::EBUSY));
    assert_eq!(errno(delete_overlay(opts(&db)).await), Some(libc::EBUSY));
    assert_eq!(
        errno(rebind_overlay(opts(&db), &base).await),
        Some(libc::EBUSY)
    );
    assert!(list_overlays(dir.path(), None).await.unwrap().is_empty());
    finish(child);
    open_merged(opts(&db), None).await.unwrap();
}

#[tokio::test]
async fn paused_delete_in_child() {
    let dir = tempfile::tempdir().unwrap();
    let (db, base) = overlay(dir.path(), "o.db").await;
    let mut child = spawn("pause_delete", &db, Path::new(&base.path));
    wait_for(&mut child, "ready");
    assert_eq!(errno(open_merged(opts(&db), None).await), Some(libc::EBUSY));
    assert_eq!(errno(delete_overlay(opts(&db)).await), Some(libc::EBUSY));
    assert_eq!(
        errno(rebind_overlay(opts(&db), &base).await),
        Some(libc::EBUSY)
    );
    assert_eq!(
        errno(create_overlay(opts(&db), &base).await),
        Some(libc::EEXIST)
    );
    writeln!(stdin(&mut child), "go").unwrap();
    wait_for(&mut child, "removed");
    assert_eq!(
        errno(open_merged(opts(&db), None).await),
        Some(libc::ENOENT)
    );
    assert!(!db.exists());
    finish(child);
}

#[tokio::test]
async fn create_races_child_opener_with_other_base() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (make_base(dir.path(), "a"), make_base(dir.path(), "b"));
    let db = dir.path().join("o.db");
    let mut child = spawn("open_loop", &db, Path::new(&b.path));
    let mut out = BufReader::new(child.stdout.take().unwrap());
    for _ in 0..50 {
        let info = loop {
            if let Some(info) = retried(create_overlay(opts(&db), &a).await) {
                break info.unwrap();
            }
        };
        assert_eq!(info.base_identity, a.identity);
        while retried(delete_overlay(opts(&db)).await)
            .map(Result::unwrap)
            .is_none()
        {}
    }
    child.kill().unwrap();
    let mut seen = String::new();
    std::io::Read::read_to_string(&mut out, &mut seen).unwrap();
    assert!(!seen.contains("opened"));
    child.wait().unwrap();
}
