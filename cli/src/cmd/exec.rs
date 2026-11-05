//! Execute a command with an AgentFS filesystem mounted.
//!
//! This module provides the `agentfs exec` command which mounts an AgentFS
//! filesystem to a temporary directory, runs a command with that as the
//! working directory, and automatically unmounts when done.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

use crate::cmd::init::{open_fs, resolve_db};
use crate::mount::{mount_fs, MountBackend, MountOpts};

/// Handle the exec command.
///
/// Mounts the specified AgentFS, runs the command, and unmounts on completion.
pub async fn handle_exec_command(
    id_or_path: String,
    command: PathBuf,
    args: Vec<String>,
    backend: MountBackend,
    encryption: Option<(String, String)>,
) -> Result<()> {
    let opened = open_fs(resolve_db(&id_or_path, encryption.as_ref())?, None).await?;
    if let Some(base) = opened.base() {
        eprintln!("Using overlay filesystem with base: {}", base);
    }
    let fs = opened.mutex();

    // Create a temporary directory for the mount
    let exec_id = uuid::Uuid::new_v4().to_string();
    let mountpoint = std::env::temp_dir().join(format!("agentfs-exec-{}", exec_id));
    std::fs::create_dir_all(&mountpoint).context("Failed to create mount directory")?;

    let fsname = format!(
        "agentfs:{}",
        std::fs::canonicalize(&id_or_path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| id_or_path.clone())
    );

    let mount_opts = MountOpts {
        mountpoint: mountpoint.clone(),
        backend,
        fsname,
        uid: None,
        gid: None,
        allow_other: false,
        allow_root: false,
        auto_unmount: false,
        lazy_unmount: true,
        timeout: std::time::Duration::from_secs(10),
    };

    // Mount the filesystem
    let _mount_handle = mount_fs(fs, mount_opts).await?;

    // Run the command with the mountpoint as working directory
    let status = Command::new(&command)
        .args(&args)
        .current_dir(&mountpoint)
        .status()
        .with_context(|| format!("Failed to execute: {}", command.display()))?;

    // Drop the mount handle to unmount
    drop(_mount_handle);

    // Clean up the temporary directory
    let _ = std::fs::remove_dir_all(&mountpoint);

    // Exit with the command's exit code
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}
