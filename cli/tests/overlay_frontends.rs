#![cfg(unix)]

use agentfs_sdk::{
    create_overlay, open_merged, AgentFSOptions, Base, FileSystem, MergedFS, PathFS,
};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

fn agentfs(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentfs"))
        .env("HOME", home)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

/// A new overlay at `db` over `base`, opened.
async fn overlay(db: &Path, base: &Path) -> MergedFS {
    let options = AgentFSOptions::with_path(db.to_str().unwrap());
    create_overlay(options.clone(), &Base::host(base).unwrap())
        .await
        .unwrap();
    open_merged(options, None).await.unwrap()
}

fn base_dir(dir: &Path) -> PathBuf {
    let base = dir.join("base");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("target.txt"), "from base").unwrap();
    base
}

#[tokio::test]
async fn diff_prints_quoted_names_and_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let base = base_dir(dir.path());
    std::fs::write(base.join("it's \"q\""), "old").unwrap();
    std::os::unix::fs::symlink("target.txt", base.join("link")).unwrap();
    let db = dir.path().join("o.db");

    let merged = overlay(&db, &base).await;
    let paths = PathFS(Arc::new(merged.fs.clone()));
    paths.write_file("/it's \"q\"", b"new").await.unwrap();
    paths.write_file("/new's.txt", b"x").await.unwrap();
    paths.remove("/link").await.unwrap();
    FileSystem::symlink(&merged.fs, 1, "made", "target.txt", 0, 0)
        .await
        .unwrap();
    drop((paths, merged));

    let out = agentfs(dir.path(), &["diff", db.to_str().unwrap()]);
    let expected = "M f /it's \"q\"\nD l /link\nA l /made\nA f /new's.txt\n";
    assert_eq!(stdout(&out), expected);
    let (db, base) = (db.display(), base.canonicalize().unwrap());
    let diagnostics = format!("Using agent: {db}\nBase: {}\n", base.display());
    assert_eq!(String::from_utf8_lossy(&out.stderr), diagnostics);
}

#[tokio::test]
async fn fs_cat_reads_base_only_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("o.db");
    drop(overlay(&db, &base_dir(dir.path())).await);

    let out = agentfs(
        dir.path(),
        &["fs", db.to_str().unwrap(), "cat", "/target.txt"],
    );
    assert_eq!(stdout(&out), "from base");
    assert!(out.stderr.is_empty());
}

#[tokio::test]
async fn diff_resolves_run_session_id() {
    let home = tempfile::tempdir().unwrap();
    let session = home.path().join(".agentfs/run/session-1");
    std::fs::create_dir_all(&session).unwrap();

    let merged = overlay(&session.join("delta.db"), &base_dir(home.path())).await;
    PathFS(Arc::new(merged.fs.clone()))
        .write_file("/added", b"x")
        .await
        .unwrap();
    drop(merged);

    let elsewhere = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentfs"))
        .env("HOME", home.path())
        .current_dir(elsewhere.path())
        .args(["diff", "session-1"])
        .output()
        .unwrap();
    assert_eq!(stdout(&out), "A f /added\n");
}

#[tokio::test]
async fn diff_of_overlay_open_in_other_process_is_busy() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("o.db");
    let merged = overlay(&db, &base_dir(dir.path())).await;

    let out = agentfs(dir.path(), &["diff", db.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("overlay database is in use"));
    drop(merged);
}
