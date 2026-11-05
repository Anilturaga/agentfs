pub mod connection_pool;
pub mod error;
pub mod filesystem;
pub mod kvstore;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod overlay;
pub mod schema;
pub mod toolcalls;

use error::{Error, Result};
use std::{
    any::Any,
    path::{Path, PathBuf},
    sync::Arc,
};
use turso::{Builder, EncryptionOpts};

// Re-export turso sync types for CLI usage
pub use turso::sync::{DatabaseSyncStats, PartialBootstrapStrategy, PartialSyncOpts};

// Re-export filesystem types
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use filesystem::HostFS;
pub use filesystem::{
    BoxedFile, Change, DirEntry, File, FileSystem, FilesystemStats, FsError, OverlayFS, PathFS,
    Stats, TimeChange, DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO,
    S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK,
};
pub use kvstore::KvStore;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use overlay::{
    create_overlay, delete_overlay, list_overlays, open_merged, rebind_overlay, Base, MergedFS,
    OverlayInfo,
};
pub use schema::{SchemaVersion, AGENTFS_SCHEMA_VERSION};
pub use toolcalls::{ToolCall, ToolCallStats, ToolCallStatus, ToolCalls};

pub(crate) async fn open_local(
    path: &str,
    enc: Option<&EncryptionConfig>,
) -> Result<turso::Database> {
    Ok(match enc {
        Some(enc) => {
            Builder::new_local(path)
                .experimental_encryption(true)
                .with_encryption(EncryptionOpts {
                    cipher: enc.cipher.clone(),
                    hexkey: enc.hex_key.clone(),
                })
                .build()
                .await?
        }
        None => Builder::new_local(path).build().await?,
    })
}

/// Directory containing agentfs databases
pub fn agentfs_dir() -> &'static std::path::Path {
    std::path::Path::new(".agentfs")
}

/// Information about a mounted agentfs filesystem
#[derive(Debug, Clone)]
pub struct Mount {
    /// The ID (from the mount source, e.g., "agentfs:my-agent" -> "my-agent")
    pub id: String,
    /// The mountpoint path
    pub mountpoint: PathBuf,
}

/// Get all currently mounted agentfs filesystems by parsing /proc/mounts
///
/// This is the authoritative source for mount information - if it's in /proc/mounts,
/// it's mounted. If not, it's not. No stale state possible.
#[cfg(target_os = "linux")]
pub fn get_mounts() -> Vec<Mount> {
    let Ok(contents) = std::fs::read_to_string("/proc/mounts") else {
        return vec![];
    };
    contents
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[0].starts_with("agentfs:") {
                let agent_id = parts[0].strip_prefix("agentfs:")?.to_string();
                // Skip the internal "fuse" mount used by the daemon
                if agent_id == "fuse" {
                    return None;
                }
                Some(Mount {
                    id: agent_id,
                    mountpoint: PathBuf::from(parts[1]),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Get all currently mounted agentfs filesystems (non-Linux stub)
#[cfg(not(target_os = "linux"))]
pub fn get_mounts() -> Vec<Mount> {
    // On macOS, we could parse the output of `mount` command
    // For now, return empty - can be implemented later
    vec![]
}

/// Configuration options for sync
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    /// Remote URL for syncing
    pub remote_url: Option<String>,
    /// Auth token for remote sync
    pub auth_token: Option<String>,
    /// Partial sync options
    pub partial_sync: Option<PartialSyncOpts>,
}

/// Configuration options for local encryption
#[derive(Debug, Clone)]
pub struct EncryptionConfig {
    /// Hex-encoded encryption key
    pub hex_key: String,
    /// Cipher algorithm (e.g., "aegis256", "aegis128l", "aes256gcm" etc)
    pub cipher: String,
}

/// Configuration options for opening an AgentFS instance
#[derive(Debug, Clone, Default)]
pub struct AgentFSOptions {
    /// Optional unique identifier for the agent.
    /// - If Some(id): Creates persistent storage at `.agentfs/{id}.db`
    /// - If None: Uses ephemeral in-memory database
    pub id: Option<String>,
    /// Optional custom path to the database file.
    /// Takes precedence over `id` if both are set.
    pub path: Option<String>,
    /// Optional base directory for overlay filesystem (copy-on-write).
    /// When set, the filesystem operates as an overlay on top of this directory.
    pub base: Option<PathBuf>,
    /// Sync options for remote database synchronization
    pub sync: SyncOptions,
    /// Encryption configuration for database at rest
    pub encryption: Option<EncryptionConfig>,
}

impl AgentFSOptions {
    /// Validates an agent ID to prevent path traversal and ensure safe filesystem operations.
    /// Returns true if the ID contains only alphanumeric characters, hyphens, and underscores.
    pub fn validate_agent_id(id: &str) -> bool {
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    }
    pub fn db_path(&self) -> Result<String> {
        // Determine database path: path takes precedence over id
        if let Some(path) = &self.path {
            // Custom path provided directly
            Ok(path.to_string())
        } else if let Some(id) = &self.id {
            // Validate agent ID to prevent path traversal attacks
            if !Self::validate_agent_id(id) {
                return Err(Error::InvalidAgentId(id.clone()));
            }

            // Ensure .agentfs directory exists
            let agentfs_dir = agentfs_dir();
            if !agentfs_dir.exists() {
                std::fs::create_dir_all(agentfs_dir)?;
            }
            Ok(format!("{}/{}.db", agentfs_dir.display(), id))
        } else {
            // No id or path = ephemeral in-memory database
            Ok(":memory:".to_string())
        }
    }
    /// Create options for a persistent agent with the given ID
    pub fn with_id(id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            path: None,
            base: None,
            sync: SyncOptions::default(),
            encryption: None,
        }
    }

    /// Create options for an ephemeral in-memory agent
    pub fn ephemeral() -> Self {
        Self {
            id: None,
            path: None,
            base: None,
            sync: SyncOptions::default(),
            encryption: None,
        }
    }

    /// Create options with a custom database path
    pub fn with_path(path: impl Into<String>) -> Self {
        Self {
            id: None,
            path: Some(path.into()),
            base: None,
            sync: SyncOptions::default(),
            encryption: None,
        }
    }

    /// Set sync options
    pub fn with_sync(mut self, sync: SyncOptions) -> Self {
        self.sync = sync;
        self
    }

    /// Set the base directory for overlay filesystem (copy-on-write)
    pub fn with_base(mut self, base: impl Into<PathBuf>) -> Self {
        self.base = Some(base.into());
        self
    }

    /// Enable local encryption with a hex-encoded key and cipher
    ///
    /// # Arguments
    /// * `hex_key` - Hex-encoded encryption key (64 chars for 256-bit ciphers, 32 for 128-bit)
    /// * `cipher` - Cipher algorithm (e.g., "aegis256", "aes256gcm" etc)
    pub fn with_encryption_key(mut self, hex_key: &str, cipher: &str) -> Self {
        self.encryption = Some(EncryptionConfig {
            hex_key: hex_key.to_string(),
            cipher: cipher.to_string(),
        });
        self
    }

    /// Set encryption configuration directly
    pub fn with_encryption(mut self, encryption: EncryptionConfig) -> Self {
        self.encryption = Some(encryption);
        self
    }

    /// Resolve an id-or-path string to AgentFSOptions
    ///
    /// Resolution order (first match wins):
    /// 1. `:memory:` -> ephemeral in-memory database
    /// 2. Valid agent ID with existing `.agentfs/{id}.db` -> uses that agent
    /// 3. Existing file path -> uses that path directly
    ///
    /// Returns an error if neither an agent nor a file exists.
    pub fn resolve(id_or_path: impl Into<String>) -> Result<Self> {
        let id_or_path = id_or_path.into();

        if id_or_path == ":memory:" {
            return Ok(Self::ephemeral());
        }

        // First, check if it's a valid agent ID with an existing database in .agentfs/
        if AgentFSOptions::validate_agent_id(&id_or_path) {
            let db_path = agentfs_dir().join(format!("{}.db", id_or_path));
            if db_path.exists() {
                return Ok(Self::with_path(db_path.to_str().ok_or_else(|| {
                    Error::InvalidUtf8Path(db_path.display().to_string())
                })?));
            }
        }

        // Fall back to treating as a direct file path
        let path = Path::new(&id_or_path);
        if path.is_file() {
            Ok(Self::with_path(id_or_path))
        } else if AgentFSOptions::validate_agent_id(&id_or_path) {
            // Not a valid agent and not an existing file
            Err(Error::AgentNotFound {
                id: id_or_path.clone(),
                path: agentfs_dir()
                    .join(format!("{}.db", id_or_path))
                    .display()
                    .to_string(),
            })
        } else {
            Err(Error::InvalidAgentId(id_or_path))
        }
    }
}

/// The main AgentFS SDK struct
///
/// This provides a unified interface to the filesystem, key-value store,
/// and tool calls tracking backed by a SQLite database.
pub struct AgentFS {
    pool: connection_pool::ConnectionPool,
    sync_db: Option<turso::sync::Database>,
    pub kv: KvStore,
    pub fs: filesystem::AgentFS,
    pub tools: ToolCalls,
}

impl AgentFS {
    /// Open an AgentFS instance
    ///
    /// # Arguments
    /// * `options` - Configuration options (use Default::default() for ephemeral)
    ///
    /// # Examples
    /// ```no_run
    /// use agentfs_sdk::{AgentFS, AgentFSOptions};
    ///
    /// # async fn example() -> agentfs_sdk::error::Result<()> {
    /// // Persistent storage
    /// let agent = AgentFS::open(AgentFSOptions::with_id("my-agent")).await?;
    ///
    /// // Ephemeral in-memory
    /// let agent = AgentFS::open(AgentFSOptions::ephemeral()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn open(options: AgentFSOptions) -> Result<Self> {
        Self::open_held(options, None).await
    }

    pub(crate) async fn open_held(
        options: AgentFSOptions,
        held: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Result<Self> {
        // Validate base directory if provided
        if let Some(ref path) = options.base {
            if !path.exists() {
                return Err(Error::BaseDirectoryNotFound(path.display().to_string()));
            }
            if !path.is_dir() {
                return Err(Error::NotADirectory(path.display().to_string()));
            }
        }

        // Encryption is not supported with sync
        if options.encryption.is_some() && options.sync.remote_url.is_some() {
            return Err(Error::EncryptionNotSupported(
                "Local encryption is not supported with cloud sync".to_string(),
            ));
        }

        let db_path = options.db_path()?;
        let meta_path = format!("{db_path}-info");

        let (held, created) = match held {
            None => connection_pool::ConnectionPool::reserve(&db_path)?,
            held => (held, false),
        };

        // Determine if this is a synced database:
        // 1. If sync.remote_url is set, create a new synced database
        // 2. If {path}-info file exists, open as existing synced database
        // 3. Otherwise, open as local database (with optional encryption via URI)
        let (sync_db, pool) = if let Some(remote_url) = options.sync.remote_url {
            // Creating a new synced database
            let mut builder =
                turso::sync::Builder::new_remote(&db_path).with_remote_url(remote_url);
            if let Some(auth_token) = options.sync.auth_token {
                builder = builder.with_auth_token(auth_token);
            }
            if let Some(partial_sync) = options.sync.partial_sync {
                builder = builder.with_partial_sync_opts_experimental(partial_sync);
            }
            let db = builder.build().await.inspect_err(|_| {
                if created {
                    let _ = std::fs::remove_file(&db_path);
                }
            })?;
            let pool = connection_pool::ConnectionPool::new_sync(db.clone());
            held.into_iter().for_each(|held| pool.hold(held));
            (Some(db), pool)
        } else if std::fs::exists(&meta_path).unwrap_or(false) {
            let mut builder = turso::sync::Builder::new_remote(&db_path);
            if let Some(auth_token) = options.sync.auth_token {
                builder = builder.with_auth_token(auth_token);
            }
            let db = builder.build().await?;
            let pool = connection_pool::ConnectionPool::new_sync(db.clone());
            held.into_iter().for_each(|held| pool.hold(held));
            (Some(db), pool)
        } else {
            let enc = options.encryption.as_ref();
            (
                None,
                connection_pool::ConnectionPool::open_held(&db_path, enc, held).await?,
            )
        };

        // Check schema version for existing databases
        let conn = pool.get_connection().await?;
        schema::check_schema_version(&conn).await?;

        // An overlay with an unsupported version is never opened, whatever its base
        OverlayFS::check_version(&conn).await?;

        // Open the overlay schema at v1 (migrating legacy databases) for the given
        // base, or for the base recorded in an existing overlay database if it still
        // exists. A database whose recorded base is gone stays as it is.
        let base_path = match options.base {
            Some(base_path) => Some(
                std::fs::canonicalize(base_path)?
                    .to_string_lossy()
                    .to_string(),
            ),
            None => OverlayFS::config_value(&conn, "base_path")
                .await?
                .filter(|p| std::path::Path::new(p).exists()),
        };
        if let Some(base_path) = base_path {
            OverlayFS::init_schema(&conn, &base_path).await?;
        }
        drop(conn);

        Self::open_with_pool(pool, sync_db).await
    }

    /// Open an AgentFS instance from a connection pool
    pub async fn open_with_pool(
        pool: connection_pool::ConnectionPool,
        sync_db: Option<turso::sync::Database>,
    ) -> Result<Self> {
        let kv = KvStore::from_pool(pool.clone()).await?;
        let fs = filesystem::AgentFS::from_pool(pool.clone()).await?;
        let tools = ToolCalls::from_pool(pool.clone()).await?;

        Ok(Self {
            pool,
            sync_db,
            kv,
            fs,
            tools,
        })
    }

    /// Open an AgentFS instance from a sync database
    pub async fn open_with_sync_db(db: turso::sync::Database) -> Result<Self> {
        let pool = connection_pool::ConnectionPool::new_sync(db.clone());
        Self::open_with_pool(pool, Some(db)).await
    }

    /// Create a new AgentFS instance (deprecated, use `open` instead)
    ///
    /// # Arguments
    /// * `db_path` - Path to the SQLite database file (use ":memory:" for in-memory database)
    #[deprecated(
        since = "0.2.0",
        note = "Use AgentFS::open with AgentFSOptions instead"
    )]
    pub async fn new(db_path: &str) -> Result<Self> {
        let pool = connection_pool::ConnectionPool::open_local(db_path, None).await?;
        Self::open_with_pool(pool, None).await
    }

    /// Get a connection from the pool
    pub async fn get_connection(&self) -> Result<connection_pool::PooledConnection> {
        self.pool.get_connection().await
    }

    /// Get the connection pool
    pub fn get_pool(&self) -> connection_pool::ConnectionPool {
        self.pool.clone()
    }

    /// Check if sync is enabled for this database
    pub fn is_synced(&self) -> bool {
        self.sync_db.is_some()
    }

    /// Get the underlying sync database (if sync is enabled)
    pub fn get_sync_db(&self) -> Option<&turso::sync::Database> {
        self.sync_db.as_ref()
    }

    /// Pull changes from remote database
    pub async fn pull(&self) -> Result<()> {
        let db = self.sync_db.as_ref().ok_or(Error::SyncNotEnabled)?;
        db.pull().await?;
        Ok(())
    }

    /// Push local changes to remote database
    pub async fn push(&self) -> Result<()> {
        let db = self.sync_db.as_ref().ok_or(Error::SyncNotEnabled)?;
        db.push().await?;
        Ok(())
    }

    /// Checkpoint the local database
    pub async fn checkpoint(&self) -> Result<()> {
        let db = self.sync_db.as_ref().ok_or(Error::SyncNotEnabled)?;
        db.checkpoint().await?;
        Ok(())
    }

    /// Get sync statistics
    pub async fn sync_stats(&self) -> Result<DatabaseSyncStats> {
        let db = self.sync_db.as_ref().ok_or(Error::SyncNotEnabled)?;
        let stats = db.stats().await?;
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_agentfs_creation() {
        let agentfs = AgentFS::open(AgentFSOptions::ephemeral()).await.unwrap();
        // Just verify we can get the connection
        let _conn = agentfs.get_connection().await.unwrap();
    }

    #[tokio::test]
    async fn test_agentfs_with_id() {
        let agentfs = AgentFS::open(AgentFSOptions::with_id("test-agent"))
            .await
            .unwrap();
        // Just verify we can get the connection
        let _conn = agentfs.get_connection().await.unwrap();

        // Cleanup
        let agentfs_dir = agentfs_dir();
        let file_names = ["test-agent.db", "test-agent.db-shm", "test-agent.db-wal"];
        for file_name in file_names {
            let _ = std::fs::remove_file(agentfs_dir.join(file_name));
        }
    }

    #[tokio::test]
    async fn test_kv_operations() {
        let agentfs = AgentFS::open(AgentFSOptions::ephemeral()).await.unwrap();

        // Set a value
        agentfs.kv.set("test_key", &"test_value").await.unwrap();

        // Get the value
        let value: Option<String> = agentfs.kv.get("test_key").await.unwrap();
        assert_eq!(value, Some("test_value".to_string()));

        // Delete the value
        agentfs.kv.delete("test_key").await.unwrap();

        // Verify deletion
        let value: Option<String> = agentfs.kv.get("test_key").await.unwrap();
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn test_filesystem_operations() {
        let agentfs = AgentFS::open(AgentFSOptions::ephemeral()).await.unwrap();

        // Create a directory
        agentfs.fs.mkdir("/test_dir", 0, 0).await.unwrap();

        // Check directory exists
        let stats = agentfs.fs.stat("/test_dir").await.unwrap();
        assert!(stats.is_some());
        let dir_stats = stats.unwrap();
        assert!(dir_stats.is_directory());

        // Write a file
        let data = b"Hello, AgentFS!";
        let (_, file) = agentfs
            .fs
            .create_file("/test_dir/test.txt", DEFAULT_FILE_MODE, 0, 0)
            .await
            .unwrap();
        file.pwrite(0, data).await.unwrap();

        // Read the file
        let read_data = agentfs
            .fs
            .read_file("/test_dir/test.txt")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read_data, data);

        // List directory
        let entries = agentfs.fs.readdir(dir_stats.ino).await.unwrap().unwrap();
        assert_eq!(entries, vec!["test.txt"]);
    }

    #[tokio::test]
    async fn test_tool_calls() {
        let agentfs = AgentFS::open(AgentFSOptions::ephemeral()).await.unwrap();

        // Start a tool call
        let id = agentfs
            .tools
            .start("test_tool", Some(serde_json::json!({"param": "value"})))
            .await
            .unwrap();

        // Mark it as successful
        agentfs
            .tools
            .success(id, Some(serde_json::json!({"result": "success"})))
            .await
            .unwrap();

        // Get the tool call
        let call = agentfs.tools.get(id).await.unwrap().unwrap();
        assert_eq!(call.name, "test_tool");
        assert_eq!(call.status, ToolCallStatus::Success);

        // Get stats
        let stats = agentfs.tools.stats_for("test_tool").await.unwrap().unwrap();
        assert_eq!(stats.total_calls, 1);
        assert_eq!(stats.successful, 1);
    }

    #[test]
    fn test_resolve_memory() {
        let opts = AgentFSOptions::resolve(":memory:").unwrap();
        assert!(opts.id.is_none());
        assert!(opts.path.is_none());
    }

    #[test]
    fn test_resolve_existing_file_path() {
        // Create a temporary file to test with
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join("test_resolve_existing.db");
        std::fs::write(&temp_file, b"test").unwrap();

        let opts = AgentFSOptions::resolve(temp_file.to_str().unwrap()).unwrap();
        assert!(opts.id.is_none());
        assert_eq!(opts.path, Some(temp_file.to_str().unwrap().to_string()));

        // Cleanup
        let _ = std::fs::remove_file(&temp_file);
    }

    #[test]
    fn test_resolve_valid_agent_id_with_existing_db() {
        // Setup: create .agentfs directory and a test database
        let agentfs_dir = agentfs_dir();
        let _ = std::fs::create_dir_all(agentfs_dir);
        let db_path = agentfs_dir.join("test-resolve-agent.db");
        std::fs::write(&db_path, b"test").unwrap();

        let opts = AgentFSOptions::resolve("test-resolve-agent").unwrap();
        assert!(opts.id.is_none());
        assert_eq!(opts.path, Some(db_path.to_string_lossy().to_string()));

        // Cleanup
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_resolve_invalid_agent_id() {
        // Agent IDs with path traversal should be rejected
        let result = AgentFSOptions::resolve("../evil");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid agent ID"));

        // Agent IDs with spaces should be rejected
        let result = AgentFSOptions::resolve("invalid agent");
        assert!(result.is_err());

        // Agent IDs with special characters should be rejected
        let result = AgentFSOptions::resolve("agent@test");
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_nonexistent_agent() {
        let result = AgentFSOptions::resolve("nonexistent-agent-12345");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_resolve_valid_agent_id_formats() {
        // Setup: create .agentfs directory and test databases
        let agentfs_dir = agentfs_dir();
        let _ = std::fs::create_dir_all(agentfs_dir);

        // Test various valid ID formats
        let valid_ids = ["my-agent", "my_agent", "MyAgent123", "agent-123_test"];

        for id in valid_ids {
            let db_path = agentfs_dir.join(format!("{}.db", id));
            std::fs::write(&db_path, b"test").unwrap();

            let opts = AgentFSOptions::resolve(id).unwrap();
            assert!(opts.id.is_none());
            assert_eq!(opts.path, Some(db_path.to_string_lossy().to_string()));

            // Cleanup
            let _ = std::fs::remove_file(&db_path);
        }
    }

    #[tokio::test]
    async fn test_encrypted_database_creation() {
        let hex_key = "b1bbfda4f589dc9daaf004fe21111e00dc00c98237102f5c7002a5669fc76327";
        let db_path = agentfs_dir().join("test-encrypted-agent.db");

        let file_names = [
            "test-encrypted-agent.db",
            "test-encrypted-agent.db-shm",
            "test-encrypted-agent.db-wal",
        ];
        for file_name in file_names {
            let _ = std::fs::remove_file(agentfs_dir().join(file_name));
        }

        // create encrypted database and write data
        {
            let agentfs = AgentFS::open(
                AgentFSOptions::with_id("test-encrypted-agent")
                    .with_encryption_key(hex_key, "aegis256"),
            )
            .await
            .unwrap();

            agentfs
                .kv
                .set("test_key", &"encrypted_value")
                .await
                .unwrap();
        }

        // verify database file exists
        assert!(db_path.exists(), "Database file should exist");

        // reopen with correct key - data should be readable
        {
            let agentfs = AgentFS::open(
                AgentFSOptions::with_path(db_path.to_str().unwrap())
                    .with_encryption_key(hex_key, "aegis256"),
            )
            .await
            .unwrap();

            let value: Option<String> = agentfs.kv.get("test_key").await.unwrap();
            assert_eq!(value, Some("encrypted_value".to_string()));
        }

        // opening with wrong key should panic (turso panics on decryption failure)
        let wrong_key = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let path_clone = db_path.clone();
        let result = std::panic::catch_unwind(|| {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let agentfs = AgentFS::open(
                    AgentFSOptions::with_path(path_clone.to_str().unwrap())
                        .with_encryption_key(wrong_key, "aegis256"),
                )
                .await
                .unwrap();
                let _: Option<String> = agentfs.kv.get("test_key").await.unwrap();
            })
        });
        assert!(result.is_err(), "Opening with wrong key should panic");

        // opening without key should panic (encrypted db read as plaintext)
        let path_clone = db_path.clone();
        let result = std::panic::catch_unwind(|| {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let agentfs =
                    AgentFS::open(AgentFSOptions::with_path(path_clone.to_str().unwrap()))
                        .await
                        .unwrap();
                let _: Option<String> = agentfs.kv.get("test_key").await.unwrap();
            })
        });
        assert!(result.is_err(), "Opening without key should panic");

        // cleanup
        for file_name in file_names {
            let _ = std::fs::remove_file(agentfs_dir().join(file_name));
        }
    }
}
