#![cfg(any(target_os = "linux", target_os = "macos"))]

use agentfs_sdk::error::{Error, Result};
use agentfs_sdk::{AgentFS, AgentFSOptions, FileSystem, HostFS, OverlayFS, Stats};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    std::fs::create_dir_all(base.join("d/sub")).unwrap();
    std::fs::create_dir(base.join("e")).unwrap();
    std::fs::write(base.join("d/sub/y"), "y").unwrap();
    std::fs::write(base.join("d/x"), "x").unwrap();
    std::fs::write(base.join("f"), "base").unwrap();
    let db = dir.path().join("o.db");
    (dir, base, db)
}

async fn open(db: &Path, base: &Path) -> OverlayFS {
    let options = AgentFSOptions::with_path(db.to_str().unwrap()).with_base(base);
    let agent = AgentFS::open(options).await.unwrap();
    let fs = OverlayFS::new(Arc::new(HostFS::new(base).unwrap()), agent.fs);
    fs.load().await.unwrap();
    fs
}

async fn at(fs: &OverlayFS, path: &str) -> Stats {
    let mut stats = fs.getattr(1).await.unwrap().unwrap();
    for name in path.split('/').filter(|s| !s.is_empty()) {
        stats = fs.lookup(stats.ino, name).await.unwrap().unwrap();
    }
    stats
}

#[tokio::test]
async fn saved_lower_directory_inode_follows_the_move() {
    let (_dir, base, db) = fixture();
    let fs = open(&db, &base).await;
    let d = at(&fs, "/d").await;
    fs.rename(1, "d", 1, "m").await.unwrap();
    fs.create_file(d.ino, "new", 0o644, 0, 0).await.unwrap();
    assert!(fs.lookup(1, "d").await.unwrap().is_none());
    assert!(fs
        .lookup(at(&fs, "/m").await.ino, "new")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn saved_descendant_directory_inode_follows_the_move() {
    let (_dir, base, db) = fixture();
    let fs = open(&db, &base).await;
    let sub = at(&fs, "/d/sub").await;
    fs.rename(1, "d", 1, "m").await.unwrap();
    fs.create_file(sub.ino, "new", 0o644, 0, 0).await.unwrap();
    assert!(fs.lookup(1, "d").await.unwrap().is_none());
    assert!(fs
        .lookup(at(&fs, "/m/sub").await.ino, "new")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn delta_directory_inode_follows_the_move() {
    let (_dir, base, db) = fixture();
    let fs = open(&db, &base).await;
    let n = fs.mkdir(1, "n", 0o755, 0, 0).await.unwrap();
    fs.rename(1, "n", 1, "m").await.unwrap();
    fs.create_file(n.ino, "new", 0o644, 0, 0).await.unwrap();
    assert!(fs.lookup(1, "n").await.unwrap().is_none());
    assert!(fs
        .lookup(at(&fs, "/m").await.ino, "new")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn renamed_directory_stays_moved_after_reopen_and_second_rename() {
    let (_dir, base, db) = fixture();
    let fs = open(&db, &base).await;
    fs.rename(1, "d", 1, "m").await.unwrap();
    drop(fs);
    let fs = open(&db, &base).await;
    fs.rename(1, "m", 1, "n").await.unwrap();
    let n = at(&fs, "/n").await;
    fs.create_file(n.ino, "new", 0o644, 0, 0).await.unwrap();
    assert!(fs.lookup(1, "m").await.unwrap().is_none());
    assert!(fs.lookup(1, "d").await.unwrap().is_none());
}

fn errno(result: Result<()>) -> i32 {
    match result {
        Err(Error::Fs(e)) => e.to_errno(),
        Err(Error::Io(e)) => e.raw_os_error().unwrap(),
        other => panic!("expected an errno, got {other:?}"),
    }
}

#[tokio::test]
async fn rename_errors_do_not_depend_on_an_unreadable_unrelated_directory() {
    if unsafe { libc::geteuid() } == 0 {
        return; // root reads mode 000 directories
    }
    let (_dir, base, db) = fixture();
    let locked = base.join("d/zzz");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0)).unwrap();
    let fs = open(&db, &base).await;
    let sub = at(&fs, "/d/sub").await;

    let into_descendant = fs.rename(1, "d", sub.ino, "z").await;
    let onto_file = fs.rename(1, "d", 1, "f").await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(errno(into_descendant), libc::EINVAL);
    assert_eq!(errno(onto_file), libc::ENOTDIR);
    assert!(FileSystem::lookup(fs.delta(), 1, "d")
        .await
        .unwrap()
        .is_none());
}
