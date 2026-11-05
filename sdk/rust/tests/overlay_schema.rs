#![cfg(any(target_os = "linux", target_os = "macos"))]

use agentfs_sdk::{error::Error, AgentFS, AgentFSOptions, FileSystem, HostFS, OverlayFS};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use turso::{Builder, Connection};

const LEGACY_RUST_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/legacy_rust_overlay.db"
);

fn make_base(dir: &Path) -> PathBuf {
    let base = dir.join("base");
    std::fs::create_dir_all(base.join("d")).unwrap();
    std::fs::write(base.join("f"), "base").unwrap();
    std::fs::write(base.join("d/x"), "x").unwrap();
    base.canonicalize().unwrap()
}

fn opts(db: &Path) -> AgentFSOptions {
    AgentFSOptions::with_path(db.to_str().unwrap().to_string())
}

async fn raw(db: &Path) -> Connection {
    Builder::new_local(db.to_str().unwrap())
        .build()
        .await
        .unwrap()
        .connect()
        .unwrap()
}

async fn rows(conn: &Connection, sql: &str) -> Vec<Vec<String>> {
    let mut rows = conn.query(sql, ()).await.unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        let cols = (0..row.column_count())
            .map(|i| format!("{:?}", row.get_value(i).unwrap()))
            .collect();
        out.push(cols);
    }
    out
}

async fn columns(conn: &Connection, table: &str) -> Vec<(String, String, bool)> {
    let sql = format!("SELECT name, type, \"notnull\" FROM pragma_table_info('{table}')");
    let mut rows = conn.query(&sql, ()).await.unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        let text = |i| match row.get_value(i).unwrap() {
            turso::Value::Text(s) => s,
            v => panic!("unexpected {v:?}"),
        };
        let notnull = *row.get_value(2).unwrap().as_integer().unwrap() == 1;
        out.push((text(0), text(1), notnull));
    }
    out
}

fn col(name: &str, ty: &str, notnull: bool) -> (String, String, bool) {
    (name.to_string(), ty.to_string(), notnull)
}

async fn assert_v1_schema(conn: &Connection) {
    assert_eq!(
        columns(conn, "fs_whiteout").await,
        [
            col("path", "TEXT", false),
            col("created_at", "INTEGER", true)
        ]
    );
    assert_eq!(
        columns(conn, "fs_origin").await,
        [
            col("delta_ino", "INTEGER", false),
            col("lower_path", "TEXT", false)
        ]
    );
    assert_eq!(
        columns(conn, "fs_overlay_config").await,
        [col("key", "TEXT", false), col("value", "TEXT", true)]
    );
    assert!(rows(
        conn,
        "SELECT 1 FROM sqlite_master WHERE name = 'idx_fs_whiteout_parent'"
    )
    .await
    .is_empty());
}

#[tokio::test]
async fn new_overlay_has_v1_schema_and_copy_up_records_lower_path() {
    let dir = tempfile::tempdir().unwrap();
    let base = make_base(dir.path());
    let db = dir.path().join("o.db");

    let agent = AgentFS::open(opts(&db).with_base(&base)).await.unwrap();
    {
        let conn = agent.get_connection().await.unwrap();
        assert_v1_schema(&conn).await;
        let meta = std::fs::metadata(&base).unwrap();
        let config = rows(
            &conn,
            "SELECT key, value FROM fs_overlay_config ORDER BY key",
        )
        .await;
        assert_eq!(
            config,
            [
                vec![
                    "Text(\"base_identity\")".to_string(),
                    format!("Text(\"host:{}:{}\")", meta.dev(), meta.ino())
                ],
                vec![
                    "Text(\"base_path\")".to_string(),
                    format!("Text({:?})", base.to_str().unwrap())
                ],
                vec!["Text(\"version\")".to_string(), "Text(\"1\")".to_string()],
            ]
        );
    }

    let overlay = OverlayFS::new(Arc::new(HostFS::new(&base).unwrap()), agent.fs);
    overlay.load().await.unwrap();
    let d = overlay.lookup(1, "d").await.unwrap().unwrap();
    let x = overlay.lookup(d.ino, "x").await.unwrap().unwrap();
    let file = overlay.open(x.ino, libc::O_RDWR).await.unwrap();
    file.pwrite(0, b"y").await.unwrap();
    let conn = overlay.delta().get_connection().await.unwrap();
    let origins = rows(
        &conn,
        "SELECT lower_path FROM fs_origin ORDER BY lower_path",
    )
    .await;
    assert_eq!(origins, [["Text(\"/d\")"], ["Text(\"/d/x\")"]]);
}

#[tokio::test]
async fn legacy_rust_overlay_upgrades_to_the_v1_schema() {
    let dir = tempfile::tempdir().unwrap();
    let base = make_base(dir.path());
    let db = dir.path().join("legacy.db");
    std::fs::copy(LEGACY_RUST_FIXTURE, &db).unwrap();
    let conn = raw(&db).await;
    conn.execute(
        "UPDATE fs_overlay_config SET value = ?1 WHERE key = 'base_path'",
        [base.to_str().unwrap().to_string()],
    )
    .await
    .unwrap();
    drop(conn);

    let agent = AgentFS::open(opts(&db)).await.unwrap();
    assert_v1_schema(&agent.get_connection().await.unwrap()).await;
}

#[tokio::test]
async fn legacy_overlay_with_missing_base_opens_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("legacy.db");
    std::fs::copy(LEGACY_RUST_FIXTURE, &db).unwrap();
    let missing = "/tmp/agentfs-legacy-fixture-base";
    assert!(!Path::new(missing).exists());
    let snapshot = async || {
        let conn = raw(&db).await;
        let mut all = Vec::new();
        for table in ["fs_overlay_config", "fs_whiteout", "fs_origin"] {
            all.push(rows(&conn, &format!("SELECT * FROM {table} ORDER BY 1")).await);
        }
        all
    };
    let before = snapshot().await;
    assert_eq!(before[0].len(), 1);

    let agent = AgentFS::open(opts(&db)).await.unwrap();
    agent.kv.set("k", &"v").await.unwrap();
    assert_eq!(agent.kv.get::<String>("k").await.unwrap().unwrap(), "v");
    assert!(agent.fs.readdir(1).await.unwrap().is_some());
    let conn = agent.get_connection().await.unwrap();
    let result = OverlayFS::init_schema(&conn, missing).await;
    assert!(
        matches!(&result, Err(Error::BaseDirectoryNotFound(p)) if p == missing),
        "{:?}",
        result.err()
    );
    drop(conn);
    drop(agent);
    assert_eq!(snapshot().await, before);
}

#[tokio::test]
async fn legacy_go_overlay_migrates_keeping_whiteouts() {
    let dir = tempfile::tempdir().unwrap();
    let base = make_base(dir.path());
    let db = dir.path().join("go.db");
    drop(AgentFS::open(opts(&db)).await.unwrap());
    let conn = raw(&db).await;
    for sql in [
        "CREATE TABLE fs_whiteout (path TEXT PRIMARY KEY, parent_path TEXT NOT NULL, created_at INTEGER NOT NULL)",
        "CREATE INDEX idx_fs_whiteout_parent ON fs_whiteout(parent_path)",
        "CREATE TABLE fs_origin (delta_ino INTEGER PRIMARY KEY, base_ino INTEGER NOT NULL)",
        "INSERT INTO fs_whiteout VALUES ('/f', '/', 7)",
        "INSERT INTO fs_origin VALUES (5, 9)",
    ] {
        conn.execute(sql, ()).await.unwrap();
    }
    drop(conn);

    let agent = AgentFS::open(opts(&db).with_base(&base)).await.unwrap();
    let conn = agent.get_connection().await.unwrap();
    assert_v1_schema(&conn).await;
    assert_eq!(
        rows(&conn, "SELECT * FROM fs_origin").await,
        [["Integer(5)", "Null"]]
    );
}

#[tokio::test]
async fn unknown_overlay_version_is_rejected_without_writes() {
    let invalid_versions = [
        vec!["UPDATE fs_overlay_config SET value = X'31' WHERE key = 'version'"],
        vec![
            "UPDATE fs_overlay_config SET value = '2' WHERE key = 'version'",
            "DELETE FROM fs_overlay_config WHERE key = 'base_path'",
        ],
    ];
    for setup in invalid_versions {
        let dir = tempfile::tempdir().unwrap();
        let base = make_base(dir.path());
        let db = dir.path().join("o.db");
        drop(AgentFS::open(opts(&db).with_base(&base)).await.unwrap());

        let conn = raw(&db).await;
        for sql in setup
            .into_iter()
            .chain(["INSERT INTO fs_whiteout VALUES ('/f', 1)"])
        {
            conn.execute(sql, ()).await.unwrap();
        }
        let snapshot = async |conn: &Connection| {
            let mut all = Vec::new();
            for table in ["fs_overlay_config", "fs_whiteout", "fs_origin"] {
                all.push(rows(conn, &format!("SELECT * FROM {table} ORDER BY 1")).await);
            }
            all
        };
        let before = snapshot(&conn).await;
        drop(conn);

        for options in [opts(&db), opts(&db).with_base(&base)] {
            let result = AgentFS::open(options).await;
            assert!(
                matches!(result, Err(Error::UnsupportedOverlayVersion(_))),
                "{:?}",
                result.err()
            );
        }
        assert_eq!(snapshot(&raw(&db).await).await, before);
    }
}
