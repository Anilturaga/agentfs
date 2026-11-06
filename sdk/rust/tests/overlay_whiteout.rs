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
    drop(overlay);

    let overlay = open(&db, &base).await;
    assert_eq!(read(&overlay, "n").await.as_deref(), Some("one"));
    assert_eq!(overlay.readdir(1).await.unwrap().unwrap(), ["d", "f", "n"]);
}

#[tokio::test]
async fn readdir_plus_inode_of_renamed_directory_is_usable() {
    let dir = tempfile::tempdir().unwrap();
    let base = make_base(dir.path());
    let overlay = open(&dir.path().join("o.db"), &base).await;

    let d = overlay.lookup(1, "d").await.unwrap().unwrap();
    overlay.create_file(d.ino, "n", 0o644, 0, 0).await.unwrap();
    overlay.rename(1, "d", 1, "m").await.unwrap();

    let entries = overlay.readdir_plus(1).await.unwrap().unwrap();
    let m = entries.iter().find(|e| e.name == "m").unwrap();
    assert!(overlay.getattr(m.stats.ino).await.unwrap().is_some());
    assert!(overlay.lookup(m.stats.ino, "n").await.unwrap().is_some());
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

#[tokio::test]
async fn delta_directory_over_a_base_non_directory_stays_usable() {
    let dir = tempfile::tempdir().unwrap();
    let base = make_base(dir.path());
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret"), "s").unwrap();

    for (i, base_is_symlink) in [false, true].into_iter().enumerate() {
        let db = dir.path().join(format!("o{i}.db"));
        let overlay = open(&db, &base).await;
        let k = overlay.mkdir(1, "k", 0o755, 0, 0).await.unwrap();
        overlay.create_file(k.ino, "a", 0o644, 0, 0).await.unwrap();
        drop(overlay);

        let path = base.join("k");
        if base_is_symlink {
            std::os::unix::fs::symlink(&outside, &path).unwrap();
        } else {
            std::fs::write(&path, "now a file").unwrap();
        }
        let overlay = open(&db, &base).await;
        let k = overlay.lookup(1, "k").await.unwrap().unwrap();
        assert!(k.is_directory());
        assert_eq!(overlay.readdir(k.ino).await.unwrap().unwrap(), ["a"]);
        assert_eq!(overlay.readdir_plus(k.ino).await.unwrap().unwrap().len(), 1);
        assert!(overlay.lookup(k.ino, "secret").await.unwrap().is_none());
        overlay.create_file(k.ino, "b", 0o644, 0, 0).await.unwrap();
        assert_eq!(overlay.readdir(k.ino).await.unwrap().unwrap(), ["a", "b"]);
        std::fs::remove_file(&path).unwrap();
    }
}

#[tokio::test]
async fn opaque_directory_ignores_a_changed_base() {
    let dir = tempfile::tempdir().unwrap();
    let base = make_base(dir.path());
    let db = dir.path().join("o.db");

    let overlay = open(&db, &base).await;
    let d = overlay.lookup(1, "d").await.unwrap().unwrap();
    overlay.unlink(d.ino, "x").await.unwrap();
    overlay.rmdir(1, "d").await.unwrap();
    let d = overlay.mkdir(1, "d", 0o755, 0, 0).await.unwrap();
    overlay.create_file(d.ino, "n", 0o644, 0, 0).await.unwrap();
    drop(overlay);

    std::fs::remove_dir_all(base.join("d")).unwrap();
    std::fs::write(base.join("d"), "now a file").unwrap();
    let overlay = open(&db, &base).await;
    let d = overlay.lookup(1, "d").await.unwrap().unwrap();
    assert_eq!(overlay.readdir(d.ino).await.unwrap().unwrap(), ["n"]);
    let entries = overlay.readdir_plus(d.ino).await.unwrap().unwrap();
    assert_eq!(entries.len(), 1);
}
