#![cfg(unix)]

use agentfs_sdk::{AgentFS, AgentFSOptions};
use std::path::Path;

#[tokio::test]
async fn ephemeral_database_ignores_a_file_named_like_the_sentinel() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    std::fs::write(":memory:", b"").unwrap();
    drop(AgentFS::open(AgentFSOptions::ephemeral()).await.unwrap());
    assert_eq!(std::fs::metadata(":memory:").unwrap().len(), 0);
    assert!(!Path::new(":memory:-wal").exists());
}
