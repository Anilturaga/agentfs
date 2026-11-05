#![cfg(unix)]

use agentfs_sdk::{filesystem, HostFS, OverlayFS};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use turso::{Builder, Connection};

const LEGACY_RUST_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../sdk/rust/tests/fixtures/legacy_rust_overlay.db"
);

fn diff(db: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_agentfs"))
        .arg("diff")
        .arg(db)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

async fn connect(db: &Path) -> Connection {
    let db = Builder::new_local(db.to_str().unwrap()).build().await;
    db.unwrap().connect().unwrap()
}

async fn count(db: &Path, sql: &str) -> i64 {
    let conn = connect(db).await;
    let mut rows = conn.query(sql, ()).await.unwrap();
    let row = rows.next().await.unwrap().unwrap();
    *row.get_value(0).unwrap().as_integer().unwrap()
}

#[test]
fn commands_over_legacy_overlay_with_missing_base_name_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("legacy.db");
    std::fs::copy(LEGACY_RUST_FIXTURE, &db).unwrap();
    for args in [&["diff"][..], &["nfs"], &["serve", "nfs"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_agentfs"))
            .args(args)
            .arg(&db)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?}: {stderr}");
        assert!(
            stderr.contains("base directory does not exist: /tmp/agentfs-legacy-fixture-base"),
            "{args:?}: {stderr}"
        );
    }
}

#[tokio::test]
async fn diff_is_identical_before_and_after_legacy_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    std::fs::create_dir_all(base.join("d")).unwrap();
    std::fs::write(base.join("f"), "base").unwrap();
    std::fs::write(base.join("d/x"), "x").unwrap();
    let db = dir.path().join("legacy.db");
    std::fs::copy(LEGACY_RUST_FIXTURE, &db).unwrap();

    let conn = connect(&db).await;
    conn.execute(
        "UPDATE fs_overlay_config SET value = ?1 WHERE key = 'base_path'",
        [base.canonicalize().unwrap().to_str().unwrap().to_string()],
    )
    .await
    .unwrap();
    drop(conn);

    // The diff of the legacy copy, read without touching its overlay tables
    let baseline = {
        let delta = filesystem::AgentFS::new(db.to_str().unwrap())
            .await
            .unwrap();
        let overlay = OverlayFS::new(Arc::new(HostFS::new(&base).unwrap()), delta);
        overlay.load().await.unwrap();
        let changes = overlay.diff().await.unwrap();
        changes.iter().map(|c| format!("{c}\n")).collect::<String>()
    };
    assert!(!baseline.is_empty());
    let version = "SELECT count(*) FROM fs_overlay_config WHERE key = 'version'";
    let legacy = "SELECT count(*) FROM pragma_table_info('fs_origin') WHERE name = 'base_ino'";
    assert_eq!(count(&db, version).await, 0);
    assert_eq!(count(&db, legacy).await, 1);

    // The first run upgrades the database on open; the second reads the upgraded one.
    assert_eq!(diff(&db), baseline);
    assert_eq!(diff(&db), baseline);
    assert_eq!(count(&db, version).await, 1);
}
