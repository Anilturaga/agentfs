//! Standalone NFS server command.
//!
//! This module provides a standalone NFS server that exports an AgentFS
//! filesystem over the network, allowing remote systems (like VMs) to mount
//! it as their root filesystem.

use agentfs_sdk::{agentfs_dir, AgentFSOptions};
use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::signal;

use crate::cmd::init::open_fs;
use crate::nfs::AgentNFS;

/// Handle the `nfs` command - start a standalone NFS server.
pub async fn handle_nfs_command(id_or_path: String, bind: String, port: u32) -> Result<()> {
    // Resolve database path
    let db_path = resolve_db_path(&id_or_path)?;

    // Open or create the AgentFS database
    let db_path_str = db_path
        .to_str()
        .context("Database path contains non-UTF8 characters")?;

    let opened = open_fs(AgentFSOptions::with_path(db_path_str), None).await?;
    match opened.base() {
        Some(base) => eprintln!("Mode: overlay (base: {})", base),
        None => eprintln!("Mode: direct AgentFS"),
    }
    let fs = opened.mutex();

    // Create NFS adapter
    let nfs = AgentNFS::new(fs);

    // Bind NFS server
    let bind_addr_str = format!("{}:{}", bind, port);
    let listener = crate::nfsserve::tcp::NFSTcpListener::bind(&bind_addr_str, nfs)
        .await
        .with_context(|| format!("Failed to bind NFS server to {}", bind_addr_str))?;

    // Print server info
    eprintln!();
    eprintln!("AgentFS NFS Server");
    eprintln!("  Database: {}", db_path.display());
    eprintln!("  Listening: {}", bind_addr_str);
    eprintln!("  Export: /");
    eprintln!();
    eprintln!("Mount from client:");
    eprintln!(
        "  mount -t nfs -o vers=3,tcp,port={},mountport={},nolock {}:/ /mnt",
        port, port, bind
    );
    eprintln!();
    eprintln!("Press Ctrl+C to stop.");
    eprintln!();

    // Spawn the NFS server task
    use crate::nfsserve::tcp::NFSTcp;
    let server_handle = tokio::spawn(async move {
        if let Err(e) = listener.handle_forever().await {
            eprintln!("NFS server error: {}", e);
        }
    });

    // Wait for Ctrl+C
    signal::ctrl_c()
        .await
        .context("Failed to listen for ctrl+c")?;

    eprintln!();
    eprintln!("Shutting down...");

    // Abort the server task (no graceful shutdown support)
    server_handle.abort();

    Ok(())
}

/// Resolve an agent ID or path to a database path.
fn resolve_db_path(id_or_path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(id_or_path);

    // If it looks like a path (contains / or ends with .db), use it directly
    if id_or_path.contains('/') || id_or_path.ends_with(".db") {
        return Ok(path);
    }

    // Otherwise, treat it as an agent ID and look in .agentfs/
    let agentfs_dir = agentfs_dir();
    let db_path = agentfs_dir.join(format!("{}.db", id_or_path));

    if db_path.exists() {
        Ok(db_path)
    } else {
        // If it doesn't exist, still return the path - AgentFS will create it
        Ok(db_path)
    }
}
