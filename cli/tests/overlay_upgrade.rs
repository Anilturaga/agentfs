#![cfg(unix)]

use std::process::Command;
use turso::Builder;

const LEGACY_RUST_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../sdk/rust/tests/fixtures/legacy_rust_overlay.db"
);

/// Output of `agentfs diff` for the fixture, as produced by the code that created it.
const LEGACY_DIFF: &str = "M d /d\nD f /d/x\nA f /d/z\nD f /f\n";

fn diff(db: &std::path::Path) -> String {
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

#[tokio::test]
async fn diff_is_identical_before_and_after_legacy_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    std::fs::create_dir_all(base.join("d")).unwrap();
    std::fs::write(base.join("f"), "base").unwrap();
    std::fs::write(base.join("d/x"), "x").unwrap();
    let db = dir.path().join("legacy.db");
    std::fs::copy(LEGACY_RUST_FIXTURE, &db).unwrap();

    let conn = Builder::new_local(db.to_str().unwrap())
        .build()
        .await
        .unwrap()
        .connect()
        .unwrap();
    conn.execute(
        "UPDATE fs_overlay_config SET value = ?1 WHERE key = 'base_path'",
        [base.canonicalize().unwrap().to_str().unwrap().to_string()],
    )
    .await
    .unwrap();
    drop(conn);

    // The first run upgrades the database on open; the second reads the upgraded one.
    assert_eq!(diff(&db), LEGACY_DIFF);
    assert_eq!(diff(&db), LEGACY_DIFF);
}
