//! Overlay management: create, open, list, rebind and delete overlay databases.
//! Turso's `F_SETLK` lock belongs to the process, and closing any descriptor of the file drops
//! it. Every open of a database file therefore goes through a [`Reservation`]: a registry entry
//! keyed by the file's inode and a lock of our own, held until the last handle drops.

use crate::{
    connection_pool::ConnectionPool,
    error::{Error, Result},
    filesystem::{overlayfs::host_identity, Change},
    AgentFS, AgentFSOptions, FileSystem, HostFS, KvStore, OverlayFS, SyncOptions,
};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::ErrorKind,
    os::unix::{fs::MetadataExt, io::AsRawFd},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

const SIDECARS: [&str; 6] = ["-wal", "-shm", "-log", "-wal-revert", "-info", "-changes"];

pub struct Base {
    pub fs: Arc<dyn FileSystem>,
    pub path: String,
    pub identity: String,
}

impl Base {
    pub fn host(dir: impl AsRef<Path>) -> Result<Base> {
        let fs = Arc::new(HostFS::new(dir.as_ref())?);
        let path = std::fs::canonicalize(dir)?.to_string_lossy().into_owned();
        Ok(Base {
            fs,
            identity: host_identity(&path)?,
            path,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayInfo {
    pub path: PathBuf,
    /// 1, or 0 for a legacy overlay
    pub version: u32,
    pub base_path: String,
    /// Empty for a legacy overlay
    pub base_identity: String,
}

/// An open overlay. The database stays reserved until this and its cloned handles drop.
pub struct MergedFS {
    pub fs: OverlayFS,
    pub kv: KvStore,
    info: OverlayInfo,
}

impl MergedFS {
    pub fn info(&self) -> &OverlayInfo {
        &self.info
    }

    pub async fn diff(&self) -> Result<Vec<Change>> {
        self.fs.diff().await
    }

    pub fn close(self) {}
}

type Key = (u64, u64);

#[derive(Default)]
struct Entry {
    info: Option<OverlayInfo>,
    parked: Vec<File>,
}

static REGISTRY: Mutex<BTreeMap<Key, Entry>> = Mutex::new(BTreeMap::new());

fn registry() -> MutexGuard<'static, BTreeMap<Key, Entry>> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

fn fail(errno: i32, msg: String) -> Error {
    Error::Overlay { errno, msg }
}

fn not_found(db: &str) -> Error {
    fail(libc::ENOENT, format!("overlay database not found: {db}"))
}

fn busy(db: &str) -> Error {
    fail(libc::EBUSY, format!("overlay database is in use: {db}"))
}

fn not_overlay(db: &str) -> Error {
    fail(libc::EINVAL, format!("not an overlay database: {db}"))
}

fn key_of(path: &Path) -> std::io::Result<Key> {
    std::fs::metadata(path).map(|m| (m.dev(), m.ino()))
}

fn try_lock(file: &File) -> std::io::Result<()> {
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    match unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

#[cfg(test)]
type Hook = Box<dyn Fn()>;

#[cfg(test)]
thread_local! {
    /// Runs between opening the file and locking it (0), before delete unlinks it (1), and between
    /// creating a file and reserving it (2).
    static HOOKS: std::cell::RefCell<[Option<Hook>; 3]> = Default::default();
}

#[cfg(test)]
fn hook(stage: usize) {
    HOOKS.with(|hooks| hooks.borrow()[stage].iter().for_each(|run| run()));
}

pub(crate) struct Reservation {
    key: Key,
    file: Option<File>,
    db: String,
}

impl Reservation {
    /// Like `acquire`, after creating the file if it is absent. The descriptor of a new file
    /// becomes the reservation's, or is parked if another opener wins.
    pub(crate) fn acquire_or_create(path: &str) -> Result<Self> {
        let created = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path);
        #[cfg(test)]
        hook(2);
        match created {
            Ok(file) => Self::reserve(Path::new(path), Some(file)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Self::acquire(Path::new(path)),
            Err(e) => Err(e.into()),
        }
    }

    /// Reserves the existing file at `path`: `ENOENT` if it is absent or the path changes
    /// meanwhile, `EBUSY` if this process or another one holds it.
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        Self::reserve(path, None)
    }

    fn reserve(path: &Path, created: Option<File>) -> Result<Self> {
        let db = path.display().to_string();
        let meta = std::fs::metadata(path).map_err(|_| not_found(&db))?;
        let key = (meta.dev(), meta.ino());
        {
            let mut registry = registry();
            if let Some(entry) = registry.get_mut(&key) {
                entry.parked.extend(created);
                return Err(busy(&db));
            }
            if meta.nlink() > 1 {
                let msg = format!("hard-linked overlay database is not supported: {db}");
                return Err(fail(libc::ENOTSUP, msg));
            }
            registry.insert(key, Entry::default());
        }
        let mut held = Self {
            key,
            file: None,
            db,
        };
        let file = match created {
            Some(file) => file,
            None => OpenOptions::new().read(true).write(true).open(path)?,
        };
        let opened = file.metadata().map(|m| (m.dev(), m.ino()))?;
        if opened != key {
            // The path now names another file. Closing a descriptor of a held file would
            // drop that holder's lock, so it closes after the holder instead.
            match registry().get_mut(&opened) {
                Some(entry) => entry.parked.push(file),
                None => drop(file),
            }
            return Err(not_found(&held.db));
        }
        held.file = Some(file);
        #[cfg(test)]
        hook(0);
        held.lock(path)?;
        Ok(held)
    }

    fn lock(&self, path: &Path) -> Result<()> {
        match try_lock(self.file.as_ref().unwrap()) {
            Ok(()) => {}
            Err(e) if matches!(e.raw_os_error(), Some(libc::EACCES | libc::EAGAIN)) => {
                return Err(busy(&self.db));
            }
            Err(e) => return Err(e.into()),
        }
        match key_of(path) {
            Ok(key) if key == self.key => Ok(()),
            _ => Err(not_found(&self.db)),
        }
    }
}

impl Drop for Reservation {
    /// Holds the registry through the closes, so no one reserves a still-open file.
    fn drop(&mut self) {
        let mut registry = registry();
        if let Some(entry) = registry.get_mut(&self.key) {
            entry.parked.clear();
        }
        drop(self.file.take());
        registry.remove(&self.key);
    }
}

async fn read_info(conn: &turso::Connection, path: &Path) -> Result<Option<OverlayInfo>> {
    let Some(base_path) = OverlayFS::config_value(conn, "base_path").await? else {
        return Ok(None);
    };
    let version = OverlayFS::config_value(conn, "version").await?;
    Ok(Some(OverlayInfo {
        path: path.to_path_buf(),
        version: version.map_or(0, |v| v.parse().unwrap_or(0)),
        base_path,
        base_identity: OverlayFS::config_value(conn, "base_identity")
            .await?
            .unwrap_or_default(),
    }))
}

/// Creates the overlay database `db` over `base`: `EEXIST` if the file exists. It is built in a
/// private file and published with `link`, so no one opens a half-written overlay.
pub async fn create_overlay(db: AgentFSOptions, base: &Base) -> Result<OverlayInfo> {
    let path = PathBuf::from(db.db_path()?);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (tmp, held) = loop {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let tmp = path.with_file_name(format!(".{name}.{}-{nanos}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(_) => break (tmp.clone(), Reservation::acquire(&tmp).map(Arc::new)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    let created = match held {
        Ok(held) => build(&db, base, &path, &tmp, &held).await,
        Err(e) => Err(e),
    };
    for suffix in std::iter::once("").chain(SIDECARS) {
        let _ = std::fs::remove_file(format!("{}{suffix}", tmp.display()));
    }
    created
}

async fn build(
    db: &AgentFSOptions,
    base: &Base,
    path: &Path,
    tmp: &Path,
    held: &Arc<Reservation>,
) -> Result<OverlayInfo> {
    let options = AgentFSOptions {
        id: None,
        path: Some(tmp.to_string_lossy().into_owned()),
        base: Some(base.path.clone().into()),
        sync: SyncOptions::default(),
        ..db.clone()
    };
    let agent = AgentFS::open_held(options, Some(held.clone())).await?;
    let info = {
        let conn = agent.get_connection().await?;
        let mut rows = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await?;
        while rows.next().await?.is_some() {}
        read_info(&conn, path).await?
    };
    drop(agent);
    std::fs::hard_link(tmp, path).map_err(|e| match e.kind() {
        ErrorKind::AlreadyExists => {
            let msg = format!("overlay database already exists: {}", path.display());
            fail(libc::EEXIST, msg)
        }
        _ => e.into(),
    })?;
    info.ok_or_else(|| not_overlay(&path.display().to_string()))
}

/// Opens the overlay `db`, migrating a legacy one. Without `base`, the recorded base is used.
/// `ENOENT` if the file is absent, `ESTALE` if the base is not the recorded one, `EINVAL` if
/// it is not an overlay and `EBUSY` if it is in use.
pub async fn open_merged(db: AgentFSOptions, base: Option<Base>) -> Result<MergedFS> {
    let path = db.db_path()?;
    let held = Arc::new(Reservation::acquire(Path::new(&path))?);
    let options = AgentFSOptions {
        base: base.as_ref().map(|b| b.path.clone().into()),
        ..db
    };
    let agent = AgentFS::open_held(options, Some(held.clone())).await?;
    let info = {
        let conn = agent.get_connection().await?;
        read_info(&conn, Path::new(&path)).await?
    };
    let info = info.ok_or_else(|| not_overlay(&path))?;
    let base = match base {
        Some(base) => base,
        None => Base::host(&info.base_path)?,
    };
    if info.base_identity != base.identity {
        let msg = format!(
            "base of {path} changed: recorded {}, given {}",
            info.base_path, base.path
        );
        return Err(fail(libc::ESTALE, msg));
    }
    let fs = OverlayFS::new(base.fs, agent.fs);
    fs.load().await?;
    if let Some(entry) = registry().get_mut(&held.key) {
        entry.info = Some(info.clone());
    }
    Ok(MergedFS {
        fs,
        kv: agent.kv,
        info,
    })
}

/// The overlay databases directly in `dir`, sorted by path; with `base`, only those over it.
/// A file that cannot be read, or that another instance is opening, is left out.
pub async fn list_overlays(dir: impl AsRef<Path>, base: Option<&Base>) -> Result<Vec<OverlayInfo>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let named = path.extension().is_some_and(|e| e == "db") && entry.file_type()?.is_file();
        let (Some(file), true) = (path.to_str().map(str::to_string), named) else {
            continue;
        };
        let Ok(key) = key_of(&path) else { continue };
        match registry().get(&key) {
            Some(Entry {
                info: Some(info), ..
            }) => {
                found.push(OverlayInfo {
                    path,
                    ..info.clone()
                });
                continue;
            }
            Some(_) => continue,
            None => {}
        }
        let Ok(held) = Reservation::acquire(&path) else {
            continue;
        };
        // A panic of the storage engine on a file it cannot read ends only this task.
        let read = tokio::spawn(async move {
            let pool = ConnectionPool::open_held(&file, None, Some(Arc::new(held)))
                .await
                .ok()?;
            let conn = pool.get_connection().await.ok()?;
            read_info(&conn, &path).await.ok().flatten()
        });
        found.extend(read.await.ok().flatten());
    }
    found.retain(|info| {
        base.is_none_or(|b| {
            info.base_identity == b.identity
                || (info.base_identity.is_empty() && info.base_path == b.path)
        })
    });
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(found)
}

/// Deletes the overlay database `db` and its sidecar files. `ENOENT`, `EINVAL` and `EBUSY` as
/// for `open_merged`.
pub async fn delete_overlay(db: AgentFSOptions) -> Result<()> {
    let path = db.db_path()?;
    let held = Arc::new(Reservation::acquire(Path::new(&path))?);
    {
        let pool =
            ConnectionPool::open_held(&path, db.encryption.as_ref(), Some(held.clone())).await?;
        let conn = pool.get_connection().await?;
        if !OverlayFS::has_column(&conn, "fs_overlay_config", "key").await? {
            return Err(not_overlay(&path));
        }
    }
    // Closing the engine's descriptors dropped the lock; take it again before removing files.
    held.lock(Path::new(&path))?;
    for suffix in SIDECARS {
        match std::fs::remove_file(format!("{path}{suffix}")) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    #[cfg(test)]
    hook(1);
    Ok(std::fs::remove_file(&path)?)
}

/// Binds the overlay `db` to `base`. `ENOTSUP` if its version is not 1.
pub async fn rebind_overlay(db: AgentFSOptions, base: &Base) -> Result<()> {
    let path = db.db_path()?;
    let held = Arc::new(Reservation::acquire(Path::new(&path))?);
    let pool = ConnectionPool::open_held(&path, db.encryption.as_ref(), Some(held.clone())).await?;
    let conn = pool.get_connection().await?;
    if !OverlayFS::check_version(&conn).await? {
        return Err(not_overlay(&path));
    }
    conn.execute("BEGIN IMMEDIATE", ()).await?;
    let rows = [
        ("base_path", &*base.path),
        ("base_identity", &*base.identity),
    ];
    match OverlayFS::set_config(&conn, &rows).await {
        Ok(()) => Ok(conn.execute("COMMIT", ()).await.map(drop)?),
        Err(e) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        process::Command,
        rc::Rc,
    };

    fn opts(path: &Path) -> AgentFSOptions {
        AgentFSOptions::with_path(path.to_str().unwrap())
    }

    async fn overlay(dir: &Path) -> (PathBuf, Base) {
        let (db, base) = (dir.join("o.db"), Base::host(dir).unwrap());
        create_overlay(opts(&db), &base).await.unwrap();
        (db, base)
    }

    fn errno<T>(result: Result<T>) -> Option<i32> {
        result.err().and_then(|e| e.errno())
    }

    fn set_hook(stage: usize, run: impl Fn() + 'static) {
        HOOKS.with(|hooks| hooks.borrow_mut()[stage] = Some(Box::new(run)));
    }

    #[tokio::test]
    async fn held_reservation_blocks_every_operation() {
        let dir = tempfile::tempdir().unwrap();
        let (db, base) = overlay(dir.path()).await;
        let held = Reservation::acquire(&db).unwrap();
        assert_eq!(errno(open_merged(opts(&db), None).await), Some(libc::EBUSY));
        assert_eq!(errno(delete_overlay(opts(&db)).await), Some(libc::EBUSY));
        assert_eq!(
            errno(rebind_overlay(opts(&db), &base).await),
            Some(libc::EBUSY)
        );
        assert!(list_overlays(dir.path(), None).await.unwrap().is_empty());
        drop(held);
        assert_eq!(list_overlays(dir.path(), None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn path_replaced_during_acquire() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _) = overlay(dir.path()).await;
        let path = db.clone();
        set_hook(0, move || {
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, "replacement").unwrap();
            std::fs::write(format!("{}-wal", path.display()), "wal").unwrap();
        });
        assert_eq!(
            errno(open_merged(opts(&db), None).await),
            Some(libc::ENOENT)
        );
        HOOKS.with(|hooks| hooks.borrow_mut()[0] = None);
        assert_eq!(std::fs::read(&db).unwrap(), b"replacement");
        assert_eq!(
            std::fs::read(format!("{}-wal", db.display())).unwrap(),
            b"wal"
        );
        assert!(!registry().contains_key(&key_of(&db).unwrap()));
    }

    /// Run again as a child process: exits 0 when the lock on `PROBE_DB` is refused.
    #[test]
    fn probe() {
        let Ok(db) = std::env::var("PROBE_DB") else {
            return;
        };
        let file = OpenOptions::new().read(true).write(true).open(db).unwrap();
        std::process::exit(if try_lock(&file).is_err() { 0 } else { 3 });
    }

    #[test]
    fn creating_opener_cannot_drop_the_lock_of_a_racing_holder() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("new.db");
        let winner = Rc::new(RefCell::new(None));
        let (path, slot) = (db.clone(), winner.clone());
        set_hook(2, move || {
            *slot.borrow_mut() = Some(Reservation::acquire(&path).unwrap());
        });
        let created = Reservation::acquire_or_create(db.to_str().unwrap());
        HOOKS.with(|hooks| hooks.borrow_mut()[2] = None);
        assert_eq!(errno(created), Some(libc::EBUSY));
        let probe = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "overlay::tests::probe"])
            .env("PROBE_DB", &db)
            .status();
        assert_eq!(probe.unwrap().code(), Some(0));
        assert!(winner.borrow().is_some());
    }

    #[tokio::test]
    async fn failed_remote_open_keeps_what_another_opener_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("new.db");
        let path = db.clone();
        set_hook(2, move || std::fs::write(&path, b"populated").unwrap());
        let remote = SyncOptions {
            remote_url: Some("http://127.0.0.1:9".to_string()),
            ..Default::default()
        };
        let opened = AgentFS::open(opts(&db).with_sync(remote)).await;
        HOOKS.with(|hooks| hooks.borrow_mut()[2] = None);
        assert!(opened.is_err());
        assert_eq!(std::fs::read(&db).unwrap(), b"populated");
    }

    #[tokio::test]
    async fn delete_holds_lock_until_unlink() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _) = overlay(dir.path()).await;
        let checked = Rc::new(Cell::new(false));
        let (path, seen) = (db.clone(), checked.clone());
        set_hook(1, move || {
            let sidecars = SIDECARS.map(|s| format!("{}{s}", path.display()));
            assert!(sidecars.iter().all(|s| !Path::new(s).exists()));
            assert!(path.exists());
            let probe = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "overlay::tests::probe"])
                .env("PROBE_DB", &path)
                .status();
            assert_eq!(probe.unwrap().code(), Some(0));
            seen.set(true);
        });
        delete_overlay(opts(&db)).await.unwrap();
        HOOKS.with(|hooks| hooks.borrow_mut()[1] = None);
        assert!(checked.get() && !db.exists());
    }
}
