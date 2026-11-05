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
- `Reservation` (private RAII guard) is an entry in `static Mutex<HashMap<(u64, u64), Option<OverlayInfo>>>`. The key is the file's `(st_dev, st_ino)` from `std::fs::metadata` (a `stat`, no fd), so hard links and symlinks of one file share one key. Each of the five operations takes it before any turso open and drops it only after its last turso handle closes. A taken key fails with `EBUSY`. Reason: turso keeps one `Database` per canonical path (separate state per alias), and the close of any duplicate fd drops the process's `fcntl` lock.
- `open_merged` moves its reservation into `fs`: `OverlayFS` gets a private type-erased keep-alive field (`Option<Arc<dyn Any + Send + Sync>>`, `None` from `OverlayFS::new`). The reservation ends when the last clone of `fs` drops, for example after a mount. A clone of `kv` must not outlive `fs`.
- `open_merged`: (1) `ENOENT` if `metadata` fails. (2) Take the reservation. (3) `AgentFS::open(db)` with `db.base = base.path`; it checks the version and initializes or migrates (`lib.rs:358-376`). Then `ENOENT` if the path's `(st_dev, st_ino)` is no longer the key: a concurrent delete won, and turso's builder created a new file there. (4) Without `base`: `Base::host(base_path)`; `EINVAL` if `base_path` is absent. (5) `ESTALE` if `base_identity != base.identity`. (6) `OverlayFS::new(base.fs, agent.fs)`, then `load()`. (7) Store the info in the reservation.
- `create_overlay`: create the file with `OpenOptions::create_new` (atomic `EEXIST`; that fd closes before any turso open). Take the reservation. `AgentFS::open(db)` with `db.base = base.path` writes the schema and the v1 config. Close, release, return the info.
- `list_overlays`: for each regular `*.db` file directly in `dir`: a reservation with info gives that info (this process holds it open); a reservation without info (a management call in progress) skips the file; else take the reservation and read `fs_overlay_config` through a raw open in its own `tokio::spawn` task, then release. A failed or panicking task skips the file; this includes a file that another process holds open (amendment 1). Keep `base_identity == base.identity` (legacy: empty identity and `base_path == base.path`). Sort by path.
- `delete_overlay`: `ENOENT`; reservation; raw open (turso lock); `EINVAL` without `fs_overlay_config`. Then, with the raw connection and its lock still open, remove the file and each `-wal -shm -log -wal-revert -info -changes` sidecar that exists. Then close and release. Another opener gets `EBUSY` before the removal and `ENOENT` after it.
- `rebind_overlay`: `ENOENT`; reservation; raw open; no version `EINVAL`, version not `1` `ENOTSUP`; overwrite `base_path` and `base_identity` in one transaction; close; release.
- Raw open: move `lib.rs:338-348` into `pub(crate) async fn open_local(path: &str, enc: Option<&EncryptionConfig>) -> Result<turso::Database>`; `AgentFS::open` uses it too. Move the identity format (`overlayfs.rs:201`) into `pub(crate) fn host_identity(path: &str) -> Result<String>` for `init_schema` and `Base::host`.

## Errors
`error.rs` adds `#[error("{msg}")] Overlay { errno: i32, msg: String }` and `pub fn errno(&self) -> Option<i32>`: `Fs` → `to_errno()`, `Io` → `raw_os_error()`, `Overlay` → `errno`, `UnsupportedOverlayVersion` → `ENOTSUP`, `BaseDirectoryNotFound` → `ENOENT`, `NotADirectory` → `ENOTDIR`, else `None`. The conformance runner (`errno_name`) and `fuse.rs:33` use it.

| Condition | Operations | Value and message | errno |
|---|---|---|---|
| Database file missing | open, delete, rebind | `Overlay`: "overlay database not found: {db}" | ENOENT |
| Database file exists | create | `Overlay`: "overlay database already exists: {db}" | EEXIST |
| Reservation taken, or turso `Error::Error` contains "File is locked" | open, delete, rebind | `Overlay`: "overlay database is in use: {db}" | EBUSY |
| Open without base: no `base_path`. Delete: no `fs_overlay_config`. Rebind: no `version` | open, delete, rebind | `Overlay`: "not an overlay database: {db}" | EINVAL |
| Version other than `1` | open, rebind | `UnsupportedOverlayVersion` (exists) | ENOTSUP |
| Host base missing / not a directory | `Base::host`, open | `BaseDirectoryNotFound` / `NotADirectory` (exist) | ENOENT / ENOTDIR |
| `base_identity` differs | open | `Overlay`: "base of {db} changed: recorded {base_path}, given {path}" | ESTALE |

## Diff without a full base walk
`pub async fn OverlayFS::diff()` in `overlayfs.rs` reads the delta, the lower adapter and the in-memory whiteout set. It never reads `fs_origin` or `fs_overlay_config`, never writes, and never changes overlay inode maps.
1. Delta walk from the delta root, with base directory ino 1. For each delta entry `e` at `P` (`readdir_plus`): `b = base.lookup(base_dir, name)` if the base directory is known. Emit `M` if `b` exists, else `A`, typed from `e`. If `e` is a directory, recurse; the base directory is `b` if `b` is a directory. Else, if `b` is a directory, emit `D` for each entry of its base subtree (shadowed).
2. Whiteouts. For each whiteout `W` where no whiteout covers parent(`W`): find the base entry at `W` by lookups; skip `W` if there is none. Let `d = delta.lstat(W)`. No `d`: emit `D` for `W` and for its base subtree. `d` and the base entry are directories: for each base child, no delta child → `D` for it and its subtree; both directories → recurse; else skip. `d` is not a directory: skip (step 1 did it).
3. Sort by path (byte order). Drop exact duplicates.
- Why exact: a lower-backed entry is in L and M and is not delta-backed, so it is never a change. Each delta entry is in M. A base path is outside M only if it is not delta-backed and is covered or below a delta non-directory. Steps 1-2 visit exactly those paths.
- Cost: base `readdir` runs only below whiteouts and below delta non-directories that shadow base directories. Base `lookup` runs once per delta entry and once per whiteout path component.

## CLI routing
New helpers in `cli/src/cmd/init.rs` (next to `open_agentfs`):
- `resolve_db(id_or_path, encryption) -> anyhow::Result<AgentFSOptions>`: `AgentFSOptions::resolve`, then `~/.agentfs/run/<id>/delta.db` (upstream #321).
- `enum Opened { Merged(MergedFS), Plain(AgentFS) }` with `arc() -> Arc<dyn FileSystem>`, `mutex() -> Arc<Mutex<dyn FileSystem + Send>>`, `kv() -> &KvStore`.
- `open_fs(options, base: Option<Base>) -> Result<Opened, SdkError>`: `open_merged`. On `Overlay{EINVAL | ENOENT}` with no base, `open_agentfs` (plain; it creates a missing file, as today).
- `open_session(options, base: Base) -> Result<MergedFS, SdkError>`: `create_overlay` if the file is missing, then `open_merged(Some(base))`. A resume from another cwd fails with `ESTALE`.

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
- Estimate: SDK +340/−190. CLI ≈ −300 net (fs.rs −190, mount −55, exec −37, linux −24, nfs −19, darwin −17, mcp −8, init.rs +50). Total ≈ −150 net; the budget is ≤ +300.

## Decisions
| Chosen | Rejected |
|---|---|
| One `Overlay { errno, msg }` variant and `errno()`: messages name paths (a CLI test checks them); one POSIX map for runner, FUSE, CLI | New `FsError` variants (fixed text, no path); `io::Error::from_raw_os_error` (no path) |
| `db: AgentFSOptions`: carries the encryption and sync auth that `run --key` and the CLI use | `&Path` (loses encryption) |
| One reservation keyed by `(st_dev, st_ino)` for all five operations, plus turso's `fcntl` lock | Canonical-path key (misses hard links); reservation in `open_merged` only (duplicate-fd races); an extra `flock` fd |
| Two-phase diff | Full merged walk (rejected in #18); SQL over `fs_dentry` (still needs base lookups) |
| `PathFS` over `dyn FileSystem`: one code path for overlay and plain databases | Methods on `MergedFS` (plain databases need a second path); `FileSystem` default methods (clash with `mkdir`/`rename`) |
| `char` fields in `Change`; `open_merged` reuses `AgentFS::open` to classify and migrate | Enums (more lines); a second copy of the version table |

## Approval items (`SPEC APPROVED` approves both; else the spec is revised)
1. SPEC amendment, Writer Policy (`SPEC.md:637`). New text: "`listOverlays` reads `fs_overlay_config` without the lock. Where the storage engine has no lock-free read (the Rust SDK on turso 0.4.4), `listOverlays` takes the lock for each read instead, and MAY omit a database that another instance holds open. `deleteOverlay` takes the lock first." The implementation PR applies it and adds an errata bullet. Reason: turso 0.4.4's `Builder` always opens read-write and locks. Rejected: a direct `turso_core` dependency for a read-only open (new dependency, ≈ +30 lines, a second query API). Same-process open overlays stay listed (from the reservation).
2. Source-breaking removal of four public methods: `AgentFS::{get_delta_paths, get_file_mode, get_whiteouts, is_overlay_enabled}`. They exist only for the old diff, and `get_file_mode` builds SQL with `format!` (the quote bug). Rejected: deprecation (keeps the bug public).

## Assumptions (not confirmed by the coordinator)
- A1 (#8): #8 keeps `OverlayFS::new(base, delta)` and `load()`, and reports `EXDEV`/`ENOTSUP` as `Error::Io` or `Error::Fs`; `errno()` maps both. Cases 6, 30, 31 and 42 pass only with #8. The implementation PR targets #8's branch while #8 is open, else `overlay/main`.
- A2: each Rust base is a host directory and each database is a locked local file, so the `ENOTSUP` storage row never applies.
- A3: turso 0.4.4 reports a cross-process lock as `Error::Error("Locking error: Failed locking file. File is locked by another process")`. A test pins it.
- A4: `init --base` and `init -c` keep `AgentFS::open(with_base)`; they only create new overlays.

## Out of scope
- An `agentfs rebind` command (open question: default is to defer; after `ESTALE`, the user runs the session from the recorded directory). `init`, `ps`, `timeline`, `sync`, `migrate`; the TypeScript and Python SDKs.
- Joining a mounted Linux session: it reads the `base_path` file, because the owner holds the lock.

## Validation criteria
All commands run with `export RUSTUP_TOOLCHAIN=nightly-2026-03-01`.
1. `cd sdk/rust && cargo test --test overlay_conformance -- --nocapture` prints `pass 42, xfail 0, fail 0` (with #8, A1). No case in `cases.json` lists `9` in `xfail`.
2. `sdk/rust/tests/overlay_management.rs`: `diff_reads_under_100_base_entries` (10 000 base files; write one, unlink one; diff is `M d /files; M f /files/0; D f /files/1`; a counting lower adapter returns < 100 `readdir`/`readdir_plus` entries during `diff()`); `list_overlays_skips_unreadable_lists_open` (garbage and encrypted `*.db` skipped; one held by `open_merged` in this process listed); `hard_link_alias_is_busy` (hold `open_merged(a.db)`; `open_merged`, `delete_overlay`, `rebind_overlay` of a hard link of `a.db` → `EBUSY`); `ops_are_busy_while_other_process_holds_it` (the test binary re-runs itself to hold the overlay in a child; `open_merged`, `delete_overlay`, `rebind_overlay` → `EBUSY`; `list_overlays` omits it); `path_fs_resolves_dot_dot_and_symlinks`.
3. Unit test `overlay::held_reservation_blocks_every_operation`: hold a `Reservation` as an in-progress delete or rebind does; `open_merged`, `delete_overlay`, `rebind_overlay` → `EBUSY`, `list_overlays` skips the file; after the drop, all succeed.
4. `cli/tests/overlay_frontends.rs`: `diff_prints_quoted_names_and_symlinks`, `fs_cat_reads_base_only_file`, `diff_resolves_run_session_id` (temp `HOME`), `diff_of_overlay_open_in_other_process_is_busy`. Unit tests: `init::open_session_from_another_cwd_is_estale`, `mcp_server::read_file_sees_base_only_file`.
5. `cli/tests/overlay_upgrade.rs` has no `LEGACY_DIFF`. Baseline: on the untouched legacy copy, `OverlayFS::new(HostFS(base), filesystem::AgentFS::new(db))`, `load()`, `diff()` (none of these touch the overlay tables), then drop it; assert that the baseline is not empty and that the copy is still legacy (no `version` row, `fs_origin.base_ino` present). Then the first and the second `agentfs diff` (it migrates) each print exactly the baseline, and `version` is `1`. `overlay_schema.rs` keeps only the `assert_v1_schema` check of its legacy-upgrade test; case 35 covers the rest.
6. The full `AGENTS.md` checks for `sdk/rust` and `cli` pass. No FUSE here: `mount`, `exec` and Linux `run` get compile, clippy and review; `run_darwin.rs` gets review only.
7. `grep -rnE "fs_overlay_config|is_overlay_enabled|get_delta_paths|get_whiteouts|get_file_mode" cli/src sdk/rust/src/lib.rs` prints nothing. `OverlayFS::new` in `cli/src` stays only in `cmd/init.rs` (A4). `git diff --numstat overlay/main -- sdk/rust/src cli/src` sums to ≤ +300 net, the `cli/src` part is negative, and the PR reports test lines apart.
