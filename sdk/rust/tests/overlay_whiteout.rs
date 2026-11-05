#![cfg(any(target_os = "linux", target_os = "macos"))]

use agentfs_sdk::{AgentFS, AgentFSOptions, FileSystem, HostFS, OverlayFS};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn make_base(dir: &Path) -> PathBuf {
    let base = dir.join("base");
    std::fs::create_dir_all(base.join("d")).unwrap();
    std::fs::write(base.join("f"), "base").unwrap();
    std::fs::write(base.join("d/x"), "x").unwrap();
    base.canonicalize().unwrap()
}

async fn open(db: &Path, base: &Path) -> OverlayFS {
    let options = AgentFSOptions::with_path(db.to_str().unwrap().to_string()).with_base(base);
    let agent = AgentFS::open(options).await.unwrap();
    let overlay = OverlayFS::new(Arc::new(HostFS::new(base).unwrap()), agent.fs);
    overlay.load().await.unwrap();
    overlay
}

async fn whiteouts(overlay: &OverlayFS) -> Vec<String> {
    let conn = overlay.delta().get_connection().await.unwrap();
    let mut rows = conn
        .query("SELECT path FROM fs_whiteout ORDER BY path", ())
        .await
        .unwrap();
    let mut paths = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        paths.push(row.get_value(0).unwrap().as_text().unwrap().clone());
    }
    paths
}

async fn read(overlay: &OverlayFS, name: &str) -> Option<String> {
    let stats = overlay.lookup(1, name).await.unwrap()?;
    let file = overlay.open(stats.ino, libc::O_RDONLY).await.unwrap();
    Some(String::from_utf8(file.pread(0, 64).await.unwrap()).unwrap())
}

async fn create(overlay: &OverlayFS, name: &str, content: &str) {
    let (_, file) = overlay.create_file(1, name, 0o644, 0, 0).await.unwrap();
    file.pwrite(0, content.as_bytes()).await.unwrap();
}

#[tokio::test]
async fn recreating_a_deleted_base_file_keeps_its_whiteout() {
    let dir = tempfile::tempdir().unwrap();
    let base = make_base(dir.path());
    let db = dir.path().join("o.db");

    let overlay = open(&db, &base).await;
    overlay.unlink(1, "f").await.unwrap();
    create(&overlay, "f", "again").await;
    assert_eq!(whiteouts(&overlay).await, ["/f"]);
    assert_eq!(read(&overlay, "f").await.as_deref(), Some("again"));
    drop(overlay);

    let overlay = open(&db, &base).await;
    assert_eq!(whiteouts(&overlay).await, ["/f"]);
    assert_eq!(read(&overlay, "f").await.as_deref(), Some("again"));
    assert!(overlay
        .readdir(1)
        .await
        .unwrap()
        .unwrap()
        .contains(&"f".to_string()));
}

#[tokio::test]
async fn persisted_whiteout_does_not_hide_a_delta_entry() {
    let dir = tempfile::tempdir().unwrap();
    let base = make_base(dir.path());
    let db = dir.path().join("o.db");

    let overlay = open(&db, &base).await;
    create(&overlay, "n", "one").await;
    drop(overlay);

    let overlay = open(&db, &base).await;
    overlay
        .delta()
        .get_connection()
        .await
        .unwrap()
        .execute(
            "INSERT INTO fs_whiteout (path, created_at) VALUES ('/n', 0)",
            (),
        )
        .await
        .unwrap();
    overlay.load().await.unwrap();
    assert_eq!(read(&overlay, "n").await.as_deref(), Some("one"));
    assert_eq!(overlay.readdir(1).await.unwrap().unwrap(), ["d", "f", "n"]);
}

#[tokio::test]
async fn recreated_directory_stays_opaque_over_base() {
    let dir = tempfile::tempdir().unwrap();
    let base = make_base(dir.path());
    let db = dir.path().join("o.db");

    let overlay = open(&db, &base).await;
    let d = overlay.lookup(1, "d").await.unwrap().unwrap();
    overlay.unlink(d.ino, "x").await.unwrap();
    overlay.rmdir(1, "d").await.unwrap();
    std::fs::write(base.join("d/y"), "y").unwrap();
    let d = overlay.mkdir(1, "d", 0o755, 0, 0).await.unwrap();
    assert!(overlay.lookup(d.ino, "y").await.unwrap().is_none());
    assert!(overlay.readdir(d.ino).await.unwrap().unwrap().is_empty());
}
