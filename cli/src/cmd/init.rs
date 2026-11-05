use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agentfs_sdk::{
    agentfs_dir, create_overlay, error::Error as SdkError, open_merged, AgentFS, AgentFSOptions,
    Base, EncryptionConfig, FileSystem, KvStore, MergedFS, OverlayFS, PartialBootstrapStrategy,
    PartialSyncOpts, SyncOptions,
};
use anyhow::{Context, Result as AnyhowResult};
use tokio::sync::Mutex;

use crate::opts::{MountBackend, SyncCommandOptions};

pub struct EncryptionOptions {
    /// Hex-encoded encryption key
    pub key: String,
    /// Cipher algorithm
    pub cipher: String,
}

fn with_env_token(mut options: AgentFSOptions) -> AgentFSOptions {
    if options.sync.auth_token.is_none() {
        options.sync.auth_token = std::env::var("TURSO_DB_AUTH_TOKEN").ok();
    }
    options
}

pub async fn open_agentfs(options: AgentFSOptions) -> Result<AgentFS, SdkError> {
    AgentFS::open(with_env_token(options)).await
}

/// The database named by an agent ID, a path, or the ID of an `agentfs run` session.
pub fn resolve_db(
    id_or_path: &str,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<AgentFSOptions> {
    let session = dirs::home_dir()
        .map(|home| home.join(".agentfs/run").join(id_or_path).join("delta.db"))
        .filter(|db| AgentFSOptions::validate_agent_id(id_or_path) && db.is_file());
    let mut options = match (AgentFSOptions::resolve(id_or_path), session) {
        (Ok(options), _) => options,
        (Err(_), Some(db)) => AgentFSOptions::with_path(db.to_string_lossy()),
        (Err(e), None) => return Err(e.into()),
    };
    if let Some((key, cipher)) = encryption {
        options = options.with_encryption(EncryptionConfig {
            hex_key: key.clone(),
            cipher: cipher.clone(),
        });
    }
    Ok(options)
}

/// A database opened as an overlay when it is one, else as a plain filesystem.
pub enum Opened {
    Merged(MergedFS),
    Plain(AgentFS),
}

impl Opened {
    pub fn arc(&self) -> Arc<dyn FileSystem> {
        match self {
            Opened::Merged(m) => Arc::new(m.fs.clone()),
            Opened::Plain(a) => Arc::new(a.fs.clone()),
        }
    }

    pub fn mutex(&self) -> Arc<Mutex<dyn FileSystem + Send>> {
        match self {
            Opened::Merged(m) => Arc::new(Mutex::new(m.fs.clone())),
            Opened::Plain(a) => Arc::new(Mutex::new(a.fs.clone())),
        }
    }

    pub fn base(&self) -> Option<&str> {
        match self {
            Opened::Merged(m) => Some(&m.info().base_path),
            Opened::Plain(_) => None,
        }
    }

    pub fn kv(&self) -> &KvStore {
        match self {
            Opened::Merged(m) => &m.kv,
            Opened::Plain(a) => &a.kv,
        }
    }
}

/// Open the database over `base`, or over the base it records. A database that is not an
/// overlay, or does not exist yet, opens as a plain filesystem.
pub async fn open_fs(options: AgentFSOptions, base: Option<Base>) -> Result<Opened, SdkError> {
    let exists = options.db_path().is_ok_and(|db| Path::new(&db).exists());
    if base.is_none() && !exists {
        return Ok(Opened::Plain(open_agentfs(options).await?));
    }
    let given = base.is_some();
    match open_merged(with_env_token(options.clone()), base).await {
        Ok(merged) => Ok(Opened::Merged(merged)),
        Err(SdkError::Overlay {
            errno: libc::EINVAL,
            ..
        }) if !given => Ok(Opened::Plain(open_agentfs(options).await?)),
        Err(e) => Err(e),
    }
}

/// Open the overlay of an `agentfs run` session over `base`, creating it on the first run.
/// A later run over another directory fails with `ESTALE`.
pub async fn open_session(options: AgentFSOptions, base: Base) -> Result<MergedFS, SdkError> {
    if !options.db_path().is_ok_and(|db| Path::new(&db).exists()) {
        match create_overlay(options.clone(), &base).await {
            Err(e) if e.errno() != Some(libc::EEXIST) => return Err(e),
            _ => {}
        }
    }
    open_merged(options, Some(base)).await
}

pub fn build_sync_options(sync_cmd_options: &SyncCommandOptions) -> SyncOptions {
    let mut sync = SyncOptions {
        remote_url: sync_cmd_options.sync_remote_url.clone(),
        auth_token: std::env::var("TURSO_DB_AUTH_TOKEN").ok(),
        partial_sync: None,
    };

    if sync_cmd_options.sync_remote_url.is_some() {
        let mut partial_sync = PartialSyncOpts {
            bootstrap_strategy: Some(PartialBootstrapStrategy::Prefix { length: 128 * 1024 }),
            prefetch: false,
            segment_size: 128 * 1024,
        };
        let mut has_partial_sync = false;

        if let Some(prefetch) = sync_cmd_options.sync_partial_prefetch {
            partial_sync.prefetch = prefetch;
            has_partial_sync = true;
        }
        if let Some(segment_size) = sync_cmd_options.sync_partial_segment_size {
            partial_sync.segment_size = segment_size;
            has_partial_sync = true;
        }
        if let Some(length) = sync_cmd_options.sync_partial_bootstrap_length {
            partial_sync.bootstrap_strategy = Some(PartialBootstrapStrategy::Prefix { length });
            has_partial_sync = true;
        }
        if let Some(ref query) = sync_cmd_options.sync_partial_bootstrap_query {
            partial_sync.bootstrap_strategy = Some(PartialBootstrapStrategy::Query {
                query: query.clone(),
            });
            has_partial_sync = true;
        }

        if has_partial_sync {
            sync.partial_sync = Some(partial_sync);
        }
    }

    sync
}

pub async fn init_database(
    id: Option<String>,
    sync_options: SyncCommandOptions,
    force: bool,
    base: Option<PathBuf>,
    encryption: Option<EncryptionOptions>,
    command: Option<String>,
    backend: MountBackend,
) -> AnyhowResult<()> {
    // Generate ID if not provided
    let id = id.unwrap_or_else(|| {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        format!("agent-{}", timestamp)
    });

    // Validate agent ID for safety
    if !AgentFSOptions::validate_agent_id(&id) {
        anyhow::bail!(
            "Invalid agent ID '{}'. Agent IDs must contain only alphanumeric characters, hyphens, and underscores.",
            id
        );
    }

    // Validate base directory if provided
    if let Some(ref base_path) = base {
        if !base_path.exists() {
            anyhow::bail!("Base directory does not exist: {}", base_path.display());
        }
        if !base_path.is_dir() {
            anyhow::bail!("Base path is not a directory: {}", base_path.display());
        }
    }

    // Check if agent already exists
    let db_path = agentfs_dir().join(format!("{}.db", id));
    if db_path.exists() {
        if force {
            for suffix in [
                "",
                "-wal",
                "-shm",
                "-log",
                "-wal-revert",
                "-info",
                "-changes",
            ] {
                let mut path = db_path.clone().into_os_string();
                path.push(suffix);
                match std::fs::remove_file(&path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        return Err(e).context("Failed to remove existing database file(s)");
                    }
                    _ => {}
                }
            }
        } else {
            anyhow::bail!(
                "Agent '{}' already exists at '{}'. Use --force to overwrite.",
                id,
                db_path.display()
            );
        }
    }

    let mut open_options =
        AgentFSOptions::with_id(&id).with_sync(build_sync_options(&sync_options));
    if let Some(base_path) = base.as_ref() {
        open_options = open_options.with_base(base_path);
    }

    let encrypted = if let Some(enc_opts) = encryption {
        if sync_options.sync_remote_url.is_some() {
            anyhow::bail!("Local encryption is not supported with cloud sync");
        }
        if !enc_opts.key.chars().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!("Encryption key must be a valid hex string");
        }
        open_options = open_options.with_encryption(EncryptionConfig {
            hex_key: enc_opts.key,
            cipher: enc_opts.cipher,
        });
        true
    } else {
        false
    };

    // Use the SDK to initialize the database - this ensures consistency
    // The SDK will create .agentfs directory and database file
    let agent = AgentFS::open(open_options)
        .await
        .context("Failed to initialize database")?;

    if let Some(ref base_path) = base {
        if agent.is_synced() {
            agent.push().await?;
        }

        eprintln!("Created overlay filesystem: {}", db_path.display());
        eprintln!("Agent ID: {}", id);
        eprintln!("Base: {}", base_path.display());
        if encrypted {
            eprintln!("Encryption: enabled");
        }
    } else {
        if agent.is_synced() {
            agent.push().await?;
        }

        eprintln!("Created agent filesystem: {}", db_path.display());
        eprintln!("Agent ID: {}", id);
        if encrypted {
            eprintln!("Encryption: enabled");
        }
    }

    // If a command was provided, mount the filesystem and execute it
    if let Some(cmd_str) = command {
        run_init_cmd(&id, cmd_str, backend, base, agent).await?;
    }

    Ok(())
}

#[cfg(unix)]
async fn run_init_cmd(
    id: &str,
    cmd_str: String,
    backend: MountBackend,
    base: Option<PathBuf>,
    agent: AgentFS,
) -> AnyhowResult<()> {
    use crate::mount::{mount_fs, MountOpts};
    use agentfs_sdk::{FileSystem, HostFS};
    use std::process::Command;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let fs: Arc<Mutex<dyn FileSystem + Send>> = if let Some(ref base_path) = base {
        let canonical = base_path
            .canonicalize()
            .context("Failed to canonicalize base path")?;
        let hostfs = HostFS::new(&canonical)?;
        let overlay = OverlayFS::new(Arc::new(hostfs), agent.fs);
        Arc::new(Mutex::new(overlay)) as Arc<Mutex<dyn FileSystem + Send>>
    } else {
        Arc::new(Mutex::new(agent.fs)) as Arc<Mutex<dyn FileSystem + Send>>
    };

    let exec_id = uuid::Uuid::new_v4().to_string();
    let mountpoint = std::env::temp_dir().join(format!("agentfs-init-{}", exec_id));
    std::fs::create_dir_all(&mountpoint).context("Failed to create mount directory")?;

    let mount_opts = MountOpts {
        mountpoint: mountpoint.clone(),
        backend,
        fsname: format!("agentfs:{}", id),
        uid: None,
        gid: None,
        allow_other: false,
        allow_root: false,
        auto_unmount: false,
        lazy_unmount: true,
        timeout: std::time::Duration::from_secs(10),
    };

    let mount_handle = mount_fs(fs, mount_opts).await?;

    let status = Command::new("sh")
        .arg("-c")
        .arg(&cmd_str)
        .current_dir(&mountpoint)
        .status()
        .with_context(|| format!("Failed to execute: {}", cmd_str))?;

    drop(mount_handle);

    let _ = std::fs::remove_dir_all(&mountpoint);

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

#[cfg(not(unix))]
async fn run_init_cmd(
    _id: &str,
    _cmd_str: String,
    _backend: MountBackend,
    _base: Option<PathBuf>,
    _agent: AgentFS,
) -> AnyhowResult<()> {
    anyhow::bail!("The -c option is not supported on Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_session_from_another_cwd_is_estale() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let options = AgentFSOptions::with_path(dir.path().join("delta.db").to_str().unwrap());

        for _ in 0..2 {
            let session = open_session(options.clone(), Base::host(&a).unwrap()).await;
            assert!(session.is_ok());
        }
        let other = open_session(options, Base::host(&b).unwrap()).await;
        assert_eq!(other.err().and_then(|e| e.errno()), Some(libc::ESTALE));
    }

    #[tokio::test]
    async fn force_init_removes_only_target_agent_files() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(".agentfs");
        std::fs::create_dir(&agents).unwrap();

        let target = [
            "force.db",
            "force.db-wal",
            "force.db-shm",
            "force.db-log",
            "force.db-wal-revert",
            "force.db-info",
            "force.db-changes",
        ];
        let others = ["force-victim.db", "force-victim.db-wal", "force.dbx"];
        for name in target.iter().chain(&others) {
            std::fs::write(agents.join(name), "stale").unwrap();
        }

        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = tokio::spawn(init_database(
            Some("force".to_string()),
            SyncCommandOptions {
                sync_remote_url: None,
                sync_partial_prefetch: None,
                sync_partial_segment_size: None,
                sync_partial_bootstrap_query: None,
                sync_partial_bootstrap_length: None,
            },
            true,
            None,
            None,
            None,
            MountBackend::default(),
        ))
        .await;
        std::env::set_current_dir(previous).unwrap();
        result.unwrap().unwrap();

        for name in others {
            assert!(agents.join(name).exists(), "{name} was deleted");
        }
        for name in &target[1..] {
            assert!(
                std::fs::read(agents.join(name)).map_or(true, |c| c != b"stale"),
                "{name} was not removed"
            );
        }
        assert_ne!(std::fs::read(agents.join("force.db")).unwrap(), b"stale");
    }
}
