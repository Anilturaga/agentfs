# Rust SDK: overlay management API and CLI routing (#9)

## Summary
Add the SPEC v1 management operations to `agentfs-sdk`. `diff()` reads only the delta and the base parts that can change its result. Route each CLI front-end through the SDK. Delete the CLI's own overlay assembly, its `fs_overlay_config` SQL and its host-probing diff. CLI text output keeps its format. SPEC.md "Overlay Filesystem" stays normative, except for the amendment in "Approval items". Implementation starts after #8 merges and the coordinator posts `SPEC APPROVED`.

## API
New file `sdk/rust/src/overlay.rs`, compiled where `HostFS` is (`linux`, `macos`), re-exported from the crate root.
```rust
pub struct Base { pub fs: Arc<dyn FileSystem>, pub path: String, pub identity: String }
impl Base { pub fn host(dir: impl AsRef<Path>) -> Result<Base>; } // HostFS, canonical path, "host:<st_dev>:<st_ino>"
#[derive(Debug, Clone, PartialEq, Eq)] pub struct OverlayInfo { pub path: PathBuf, pub version: u32, pub base_path: String, pub base_identity: String }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct Change { pub change: char, pub file_type: char, pub path: String }
// change: 'A'|'M'|'D'; file_type: 'f'|'d'|'l'|'?'; Display gives the text form "M f /src/main.rs"
pub struct MergedFS { pub fs: OverlayFS, pub kv: KvStore, info: OverlayInfo }
impl MergedFS { pub fn info(&self) -> &OverlayInfo; pub async fn diff(&self) -> Result<Vec<Change>>; pub fn close(self); }
pub async fn create_overlay(db: AgentFSOptions, base: &Base) -> Result<OverlayInfo>;
pub async fn open_merged(db: AgentFSOptions, base: Option<Base>) -> Result<MergedFS>;
pub async fn list_overlays(dir: impl AsRef<Path>, base: Option<&Base>) -> Result<Vec<OverlayInfo>>;
pub async fn delete_overlay(db: AgentFSOptions) -> Result<()>;
pub async fn rebind_overlay(db: AgentFSOptions, base: &Base) -> Result<()>;
// sdk/rust/src/filesystem/mod.rs: SDK path API ("Symlink Resolution") over any FileSystem
pub struct PathFS(pub Arc<dyn FileSystem>);
impl PathFS { // `.` skipped, `..` to the parent, 40 links max; mkdir/remove/rename act on the last name itself
    pub async fn resolve(&self, path: &str, follow_last: bool) -> Result<Stats>; // FsError::NotFound if absent
    pub async fn stat(&self, path: &str) -> Result<Option<Stats>>;
    pub async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>>;
    pub async fn write_file(&self, path: &str, data: &[u8]) -> Result<()>; // truncate, or create 0644
    pub async fn mkdir(&self, path: &str) -> Result<()>;                   // DEFAULT_DIR_MODE, uid/gid 0
    pub async fn remove(&self, path: &str) -> Result<()>;                  // rmdir a directory, else unlink
    pub async fn rename(&self, from: &str, to: &str) -> Result<()>;
}
```
- `db` names the database as for `AgentFS::open` (id or path, encryption, sync auth). The `base` argument replaces `db.base`. `close()` drops the handle.
- `Base.fs` is the lower adapter. A caller can set another adapter over the same directory (`HostFS::with_fuse_mountpoint`, `/proc/self/fd/N`). `path` and `identity` stay those of the directory. The Rust SDK has host adapters only.
### Exclusion
POSIX facts that drive this design: turso 0.4.4 locks with whole-file `F_SETLK` write locks; they belong to the process, so a second lock in the same process succeeds, and the close of any fd on the inode drops all of the process's locks on it. Turso also creates the path if it is missing and shares one `Database` per canonical path in a process.
- Registry: `static Mutex<HashMap<(u64, u64), Entry>>`, `Entry { info: Option<OverlayInfo>, parked: Vec<File> }`, keyed by the file's `(st_dev, st_ino)`. Hard links and symlinks of one file share one key.
- `Reservation::acquire(path)`, a private RAII guard: (1) `stat`; missing → `ENOENT`; key in the registry → `EBUSY`; no fd opens. (2) Insert the key. Open `F` read-write without `O_CREAT`. If `fstat(F)` differs → release, `ENOENT`; when `F`'s inode has its own entry, park `F` there, so that it closes after that holder. (3) Take the `F_SETLK` write lock on `F`; failure → release, `EBUSY`. (4) Re-`stat`; a different key or no file → release, `ENOENT`. After (4), cooperating processes cannot change what the path names: delete needs the lock, and create publishes only with `link`, which never replaces. No turso open happens before (4), so no write either. A `#[cfg(test)]` hook runs between (2) and (3). The guard owns `F`; its drop removes the entry, closes the parked fds, then closes `F`.
- Each turso database that this module opens sits in a `ConnectionPool` that holds its reservation. `ConnectionPoolInner` gets a last field `keep: OnceLock<Box<dyn Any + Send + Sync>>` and `pub(crate) fn hold(&self, ..)`. Struct fields drop in order, so turso's fds close before `F` closes and before the entry goes. `fs`, `kv`, delta file handles and pooled connections all hold the pool, so the reservation ends with the last of them.
### Operations
- `open_merged`: (1) acquire. (2) `AgentFS::open(db)` with `db.base = base.path`, then `hold`. `AgentFS::open` checks the version and initializes or migrates (`lib.rs:358-376`). (3) Without `base`: `Base::host(base_path)`; `EINVAL` if `base_path` is absent. (4) `ESTALE` if `base_identity != base.identity`. (5) `OverlayFS::new(base.fs, agent.fs)`, `load()`, store the info in the entry.
- `create_overlay`: `db.sync` is ignored, the file is always local. Create `<dir>/.<name>.<pid>-<nanos>.tmp` with `create_new` (a new name on `EEXIST`) and acquire it. `AgentFS::open` on it with `db.base = base.path` writes the schema and the v1 config. Run `PRAGMA wal_checkpoint(TRUNCATE)`, close, remove the temp file's sidecars. `link(tmp, path)` publishes the complete file and never overwrites; if `path` exists → `EEXIST`. Unlink tmp, release. A crash leaves only a `.tmp` file, and listing never shows it.
- `list_overlays`: for each regular `*.db` file `p` directly in `dir`: `stat` key in the registry with info → that info with `path = p`; key without info (a call in progress) → skip; else acquire (`ENOENT`/`EBUSY` → skip: amendment 1), raw read of `fs_overlay_config` in its own `tokio::spawn` task, release. A failed or panicking task skips `p`. Keep `base_identity == base.identity` (legacy: empty identity and `base_path == base.path`). Sort by path.
- `delete_overlay`: acquire; raw open; `EINVAL` without `fs_overlay_config`; close turso, which drops `F`'s lock. `F_SETLK` again (`EBUSY` → stop, nothing removed) and re-`stat` (`ENOENT` if the path names another inode). Remove the existing `-wal -shm -log -wal-revert -info -changes` sidecars first, the main file last (a second `#[cfg(test)]` hook runs just before it), then release. During the removals no turso handle is open and the lock is held. After the main path is gone, delete touches no path, so a replacement keeps its sidecars.
- `rebind_overlay`: acquire; raw open; no version `EINVAL`, version not `1` `ENOTSUP`; overwrite `base_path` and `base_identity` in one transaction; close; release.
- Raw open: move `lib.rs:338-348` into `pub(crate) async fn open_local(path: &str, enc: Option<&EncryptionConfig>) -> Result<turso::Database>`; `AgentFS::open` uses it too, and this module wraps each result in a `ConnectionPool` with `hold`. Move the identity format (`overlayfs.rs:201`) into `pub(crate) fn host_identity(path: &str) -> Result<String>` for `init_schema` and `Base::host`.

## Errors
`error.rs` adds `#[error("{msg}")] Overlay { errno: i32, msg: String }` and `pub fn errno(&self) -> Option<i32>`: `Fs` → `to_errno()`, `Io` → `raw_os_error()`, `Overlay` → `errno`, `UnsupportedOverlayVersion` → `ENOTSUP`, `BaseDirectoryNotFound` → `ENOENT`, `NotADirectory` → `ENOTDIR`, else `None`. The conformance runner (`errno_name`) and `fuse.rs:33` use it.

| Condition | Operations | Value and message | errno |
|---|---|---|---|
| Database file missing, or the path changed during acquire | open, delete, rebind | `Overlay`: "overlay database not found: {db}" | ENOENT |
| Database file exists | create | `Overlay`: "overlay database already exists: {db}" | EEXIST |
| Key in the registry, or `F_SETLK` fails (another process holds the lock) | open, delete, rebind | `Overlay`: "overlay database is in use: {db}" | EBUSY |
| Open without base: no `base_path`. Delete: no `fs_overlay_config`. Rebind: no `version` | open, delete, rebind | `Overlay`: "not an overlay database: {db}" | EINVAL |
| Version other than `1` | open, rebind | `UnsupportedOverlayVersion` (exists) | ENOTSUP |
| Host base missing / not a directory | `Base::host`, open | `BaseDirectoryNotFound` / `NotADirectory` (exist) | ENOENT / ENOTDIR |
| `base_identity` differs | open | `Overlay`: "base of {db} changed: recorded {base_path}, given {path}" | ESTALE |

## Diff without a full base walk
`pub async fn OverlayFS::diff()` in `overlayfs.rs` reads the delta, the lower adapter and the in-memory whiteout set. It never reads `fs_origin` or `fs_overlay_config`, never writes, and never changes overlay inode maps.
1. Delta walk from the delta root, with base directory ino 1. For each delta entry `e` at `P` (`readdir_plus`): `b = base.lookup(base_dir, name)` if the base directory is known. Emit `M` if `b` exists, else `A`, typed from `e`. If `e` is a directory, recurse; the base directory is `b` if `b` is a directory. Else, if `b` is a directory, emit `D` for each entry of its base subtree (shadowed).
2. Whiteouts. For each whiteout `W` where no whiteout covers parent(`W`): find the base entry at `W` by lookups; skip `W` if there is none. Let `d = delta.lstat(W)`. No `d`: emit `D` for `W` and for its base subtree. `d` and the base entry are directories: for each base child, no delta child → `D` for it and its subtree; both directories → recurse; else skip. `d` is not a directory: skip (step 1 did it).
3. Sort by path (byte order). Drop exact duplicates.
- Why exact: a lower-backed entry is in L and M and is not delta-backed, so it is never a change. Each delta entry is in M. A base path is outside M only if it is not delta-backed and is covered or below a delta non-directory. Steps 1-2 visit exactly those paths. Cost: base `readdir` runs only below whiteouts and below delta non-directories that shadow base directories; base `lookup` runs once per delta entry and once per whiteout path component.

## CLI routing
New helpers in `cli/src/cmd/init.rs` (next to `open_agentfs`):
- `resolve_db(id_or_path, encryption) -> anyhow::Result<AgentFSOptions>`: `AgentFSOptions::resolve`, then `~/.agentfs/run/<id>/delta.db` (upstream #321).
- `enum Opened { Merged(MergedFS), Plain(AgentFS) }` with `arc() -> Arc<dyn FileSystem>`, `mutex() -> Arc<Mutex<dyn FileSystem + Send>>`, `kv() -> &KvStore`.
- `open_fs(options, base: Option<Base>) -> Result<Opened, SdkError>`: with no base and no file, `open_agentfs` (plain; it creates the file, as `serve nfs` does today). Else `open_merged`; on `Overlay{EINVAL}` with no base, `open_agentfs`.
- `open_session(options, base: Base) -> Result<MergedFS, SdkError>`: `create_overlay` if the file is missing (`EEXIST` from a racing peer is ignored), then `open_merged(Some(base))`. A resume from another cwd fails with `ESTALE`.

| Front-end | Deleted CLI code | Replacement |
|---|---|---|
| `diff` | `fs.rs:165-273`: SDK `get_*` helpers, `format!` SQL (quote bug), host `exists()` probes | `open_fs`; `Merged`: print `m.diff()`; `Plain`: "No diff (non-overlay filesystem)" |
| `fs ls/cat/write` | `fs.rs:15-163`: raw `fs_dentry` SQL, delta-only path API | `PathFS(opened.arc())`; `ls` walks `readdir_plus` breadth-first, same lines |
| MCP | delta-only `agentfs.fs` in `mcp_server.rs` | `McpServer { opened, fs: PathFS }`; KV through `opened.kv()` |
| `mount` FUSE | `mount.rs:146-182` | `open_fs`; an overlay reopens with `Base { fs: HostFS::with_fuse_mountpoint, .. }` |
| `mount` NFS, `exec` | `mount.rs:226-259`, `exec.rs:28-72` | `open_fs(..)`, then `mutex()` |
| `serve nfs`, `nfs` | `nfs.rs:30-47` | `open_fs` (keeps `resolve_db_path`) |
| `run` Linux | `sandbox/linux.rs:186-219` | `open_session`: `Base::host(cwd)` with `fs = HostFS("/proc/self/fd/N").with_fuse_mountpoint` |
| `run` macOS | `run_darwin.rs:61-88` | `open_session(Base::host(cwd))` |
- The SDK deletes `AgentFS::{get_delta_paths, get_file_mode, get_whiteouts, is_overlay_enabled}` (`lib.rs:468-657`; approval item 2).
- Estimate: SDK ≈ +390/−190 (`overlay.rs` ≈ 220, `diff` ≈ 55, `PathFS` ≈ 65, errors ≈ 17, pool `hold` ≈ 6, `lib.rs` ≈ 20). CLI ≈ −300 net (fs.rs −190, mount −55, exec −37, linux −24, nfs −19, darwin −17, mcp −8, init.rs +50). Total ≈ −100 net; the budget is ≤ +300.

## Decisions
| Chosen | Rejected |
|---|---|
| One `Overlay { errno, msg }` variant and `errno()`: messages name paths; one POSIX map for runner, FUSE, CLI | New `FsError` variants (fixed text, no path); `io::Error::from_raw_os_error` (no path) |
| `db: AgentFSOptions`: carries the encryption and sync auth that `run --key` and the CLI use | `&Path` (loses encryption) |
| Inode-keyed reservation with our own `F_SETLK` before any engine access, held by the pool | Canonical-path key (misses hard links); turso's lock alone (taken after the path may have changed); `flock` (BSD and macOS may tie it to turso's `fcntl` lock) |
| Private temp file published with `link` | `create_new` placeholder (another opener can initialize it); `renameat2(NOREPLACE)` (Linux only; macOS needs `renamex_np`) |
| Two-phase diff; `PathFS` over `dyn FileSystem` (one code path for overlay and plain databases) | Full merged walk (rejected in #18); path methods on `MergedFS`; `FileSystem` default methods (clash with `mkdir`/`rename`) |
| `char` fields in `Change`; `open_merged` reuses `AgentFS::open` to classify and migrate | Enums (more lines); a second copy of the version table |

## Approval items (`SPEC APPROVED` approves all three; else the spec is revised)
1. SPEC amendment, Writer Policy (`SPEC.md:637`). New text: "`listOverlays` reads `fs_overlay_config` without the lock. Where the storage engine has no lock-free read (the Rust SDK on turso 0.4.4), `listOverlays` takes the lock for each read instead, and MAY omit a database that another instance holds open. `deleteOverlay` takes the lock first." The implementation PR applies it and adds an errata bullet. Rejected: a direct `turso_core` dependency for a read-only open (new dependency, ≈ +30 lines, a second query API).
2. Source-breaking removal of four public methods: `AgentFS::{get_delta_paths, get_file_mode, get_whiteouts, is_overlay_enabled}`. They exist only for the old diff, and `get_file_mode` builds SQL with `format!` (the quote bug). Rejected: deprecation (keeps the bug public).
3. Bounded limitations. L1: a raw `turso::Database` or `turso::Connection` that a caller clones out of `get_pool()` and keeps after every SDK handle dropped keeps the file open past the reservation. A same-process reopen then shares turso state, and the close of either fd drops the other's lock. L2: a process that unlinks or renames the database without the lock (`rm`, `agentfs init --force`) is outside the protocol: the path can then name another inode, and sidecars that it leaves behind can attach to a later database at that path.

## Assumptions (not confirmed by the coordinator)
- A1 (#8): #8 keeps `OverlayFS::new(base, delta)` and `load()`, and reports `EXDEV`/`ENOTSUP` as `Error::Io` or `Error::Fs`; `errno()` maps both. Cases 6, 30, 31 and 42 pass only with #8. The implementation PR targets #8's branch while #8 is open, else `overlay/main`.
- A2: each Rust base is a host directory and each database is a local file, so the `ENOTSUP` storage row never applies. A3: `init --base` and `init -c` keep `AgentFS::open(with_base)`.

## Out of scope
An `agentfs rebind` command (default: defer; after `ESTALE`, the user runs the session from the recorded directory). Joining a mounted Linux session (it reads the `base_path` file). `init`, `ps`, `timeline`, `sync`, `migrate`; the TypeScript and Python SDKs.

## Validation criteria
All commands run with `export RUSTUP_TOOLCHAIN=nightly-2026-03-01`.
1. `cd sdk/rust && cargo test --test overlay_conformance -- --nocapture` prints `pass 42, xfail 0, fail 0` (with #8, A1). No case in `cases.json` lists `9` in `xfail`.
2. `sdk/rust/tests/overlay_management.rs`: `diff_reads_under_100_base_entries` (10 000 base files; write one, unlink one; the diff is `M d /files; M f /files/0; D f /files/1`; a counting lower adapter returns < 100 `readdir`/`readdir_plus` entries during `diff()`); `listing` (garbage and encrypted `*.db` are skipped; a database held in this process is listed, and its hard link in another directory is listed under the scanned path); `hard_link_alias_is_busy` (`open_merged`, `delete_overlay`, `rebind_overlay` of a hard link of a held database → `EBUSY`); `path_fs_resolves_dot_dot_and_symlinks`.
3. Same file, where "child" is the test binary re-run as a helper process: `retained_handles_keep_reservation` (keep a `kv` clone and a delta file handle and drop the `MergedFS`: `open_merged` → `EBUSY`; drop `kv`: still `EBUSY`; drop the handle: success); `child_holds_overlay` (`open_merged`, `delete_overlay`, `rebind_overlay` → `EBUSY`; listing omits it); `paused_delete_in_child` (the child takes a raw `F_SETLK` on the database and pauses: the three calls → `EBUSY`, `create_overlay` → `EEXIST`; the child removes the sidecars and the main file and pauses: `open_merged` → `ENOENT`, and the path stays absent); `create_races_child_opener_with_other_base` (the child loops `open_merged(path, B)` while the parent runs `create_overlay(path, A)` and `delete_overlay`, retried on `EBUSY`, 50 times: each create reports A's identity, and the child never succeeds).
4. Unit tests in `overlay.rs` with the test hook: `held_reservation_blocks_every_operation` (hold a `Reservation`: the three calls → `EBUSY`, listing skips the file); `path_replaced_during_acquire` (the hook unlinks the path and creates a replacement with a `-wal`: `ENOENT`, the replacement and its `-wal` are intact, no turso open happened); `delete_holds_lock_until_unlink` (a hook point before the main-file unlink sees the sidecars gone and the main file present; a child's `open_merged` → `EBUSY`).
5. `cli/tests/overlay_frontends.rs`: `diff_prints_quoted_names_and_symlinks`, `fs_cat_reads_base_only_file`, `diff_resolves_run_session_id` (temp `HOME`), `diff_of_overlay_open_in_other_process_is_busy`. Unit tests: `init::open_session_from_another_cwd_is_estale`, `mcp_server::read_file_sees_base_only_file`.
6. `cli/tests/overlay_upgrade.rs` has no `LEGACY_DIFF`. Baseline: on the untouched legacy copy, `OverlayFS::new(HostFS(base), filesystem::AgentFS::new(db))`, `load()`, `diff()` (none of them touch the overlay tables), then drop it; assert that the baseline is not empty and that the copy is still legacy (no `version` row; `fs_origin.base_ino` present). Then the first and the second `agentfs diff` (the first migrates) each print exactly the baseline, and `version` is `1`.
7. `overlay_schema.rs` keeps only the `assert_v1_schema` check of its legacy-upgrade test; case 35 covers the rest. The full `AGENTS.md` checks for `sdk/rust` and `cli` pass. No FUSE here: `mount`, `exec` and Linux `run` get compile, clippy and review; `run_darwin.rs` gets review only.
8. `grep -rnE "fs_overlay_config|is_overlay_enabled|get_delta_paths|get_whiteouts|get_file_mode" cli/src sdk/rust/src/lib.rs` prints nothing, and `OverlayFS::new` in `cli/src` stays only in `cmd/init.rs` (A3). `git diff --numstat overlay/main -- sdk/rust/src cli/src` sums to ≤ +300 net, the `cli/src` part is negative, and the PR reports test lines apart.
