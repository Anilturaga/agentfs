# Python overlay API (#13)

## Summary
Add a native async overlay to `sdk/python` that implements SPEC.md "Overlay Filesystem" (format v1 with errata). SPEC.md defines the semantics. This doc defines the Python surface, the Python-specific rules, the split and the checks. If this doc and SPEC.md disagree, SPEC.md wins. The concepts match the TypeScript spec (PR #37, `docs/overlay-typescript.md`); see "TypeScript alignment".

## API
Add one module, `agentfs_sdk/overlay.py`. Export every public name below from `agentfs_sdk`. `Stats` is the existing `agentfs_sdk.Stats`.

```python
class LowerAdapter(Protocol):            # read-only base; each path is a SPEC "Path"
    locator: str                         # written to fs_overlay_config.base_path
    async def identity(self) -> str: ...
    async def lstat(self, path: str) -> Stats | None: ...   # None: the lower does not resolve path
    async def readdir(self, path: str) -> list[str]: ...    # names, any order
    async def readlink(self, path: str) -> str: ...
    async def read(self, path: str, offset: int, length: int) -> bytes: ...  # short only at EOF

class HostAdapter:                       # implements LowerAdapter
    def __init__(self, root: str | os.PathLike[str]) -> None: ...

DbPath = str | os.PathLike[str]
Base = DbPath | LowerAdapter             # a path means HostAdapter(path)

async def create_overlay(db_path: DbPath, base: Base) -> OverlayInfo: ...
async def open_merged(db_path: DbPath, base: Base | None = None) -> OverlayFS: ...
async def list_overlays(dir: DbPath, base: Base | None = None) -> list[OverlayInfo]: ...
async def delete_overlay(db_path: DbPath) -> None: ...
async def rebind_overlay(db_path: DbPath, base: Base) -> None: ...

@dataclass(frozen=True)
class OverlayInfo: path: str; version: int; base_path: str; base_identity: str
@dataclass(frozen=True)                  # str(change) is the SPEC text form, e.g. "M f /f"
class Change: change: Literal["A", "M", "D"]; type: Literal["f", "d", "l", "?"]; path: str
@dataclass(frozen=True)
class DirEntry: name: str; stats: Stats

class OverlayFS:                         # SPEC MergedFS; `async with` calls close() on exit
    # Same signatures as Filesystem: write_file, read_file, readdir, mkdir, rmdir,
    # unlink, rm, rename, copy_file, stat, access. Added:
    async def lstat(self, path: str) -> Stats: ...
    async def readlink(self, path: str) -> str: ...
    async def symlink(self, target: str, path: str) -> None: ...
    async def link(self, src: str, dst: str) -> None: ...
    async def chmod(self, path: str, mode: int) -> None: ...
    async def readdir_plus(self, path: str) -> list[DirEntry]: ...
    async def open(self, path: str) -> OverlayFile: ...
    async def diff(self) -> list[Change]: ...
    async def close(self) -> None: ...   # a second call does nothing; releases the writer lock

class OverlayFile:
    async def pread(self, offset: int, size: int) -> bytes: ...
    async def pwrite(self, offset: int, data: bytes) -> None: ...
    async def truncate(self, size: int) -> None: ...
    async def fstat(self) -> Stats: ...
```

## Rules
Lower adapter
1. The overlay calls an adapter method on P only after `lstat` returned a directory for each proper ancestor of P in the same walk. So no call passes through a base symlink, and an adapter resolves P with one lookup. The overlay ignores the adapter's `Stats.ino` and never calls `read` on a special file. Other adapter failures raise `OSError` and pass through unchanged.
2. `HostAdapter`: `locator` is `os.path.realpath(root)`. The constructor raises `ENOENT` if `root` is missing and `ENOTDIR` if it is not a directory. `identity()` is `f"host:{st.st_dev}:{st.st_ino}"` of `os.stat(locator)`. Every other call acts on `locator + path`, never on `path` alone: `lstat` is `os.lstat` (`FileNotFoundError` and `NotADirectoryError` give `None`), `readdir` is `os.listdir`, `readlink` is `os.readlink`, and `read` opens with `os.open(locator + path, O_RDONLY | O_NOFOLLOW)`, reads with `os.pread` and closes the descriptor in `finally`. All calls are blocking `os` calls.

Merged view
3. Each method resolves its path argument with the SPEC "Symlink Resolution" walk (`.`, `..` and empty components; `ELOOP` after 40 symlinks; `ENOTDIR`; `ENOENT`). The result is a symlink-free path P. The delta entry, the coverage and the lower entry of P are looked up by P.
4. These methods follow a symlink at the final component: `stat`, `read_file`, `write_file`, `open`, `chmod`, `readdir`, `readdir_plus`, `access`, `copy_file`. These do not: `lstat`, `readlink`, `mkdir`, `symlink`, `unlink`, `rmdir`, `rm`, `rename`, `link` (both paths).
5. Lookup, copy-up, Hide, the mutations and rename follow SPEC.md exactly. `rename` checks errors in the PR #35 order: `ENOENT`, the no-op cases, `EINVAL`, the destination errors, then `ENOTSUP` from the subtree scan.
6. Coverage uses `whiteouts: set[str]`, which `open_merged` loads and Hide extends. One instance opens the database, so the set needs no invalidation.
7. The delta is the existing `Filesystem`, by composition. Use its `_resolve_path`, `_create_inode`, `_create_dentry`, `_remove_dentry_and_maybe_inode` and `_update_file_content`. Extract `Filesystem._read_inode(ino) -> bytes` from `read_file` (the #11 chunk placement). Use direct SQL for `fs_symlink`, `fs_whiteout`, `fs_origin`, a delta move (`UPDATE fs_dentry`) and one `UPDATE fs_inode` that gives a copy-up the base mode, uid, gid and timestamps. The public behavior of `Filesystem` does not change. Reads never write to the delta (no `atime` update).
8. `ino`: the root is 1. Other values come from a counter that starts at 2, through `{delta_ino: ino}` and `{lower path: ino}`. Copy-up moves the path key to the new delta inode. Delta hard links share one value. No value is reused.
9. Convenience operations decompose. `write_file` creates missing parents with the overlay `mkdir` (as `Filesystem.write_file` does), then creates the file, or copies up without content and replaces the content. `rm` is `unlink`/`rmdir` over the merged subtree, bottom-up. `copy_file` is `read_file`, `write_file`, then `chmod` with the source permission bits.
10. `OverlayFile` keeps the merged `ino` and the path at `open`. Each call uses the delta inode of that `ino` if it exists. Else, if the merged entry at the path is still lower-backed with that `ino`, reads come from the lower, and `pwrite`/`truncate` copy up first. Else `pwrite`/`truncate` raise `ENOENT` (errata). `pwrite`/`truncate` rewrite the whole content.
11. `diff()` visits only (a) each delta entry P other than `/`: `M` if the lower resolves P, else `A`, with the delta entry's type; (b) the lower subtree rooted at each whiteout path, and the lower subtree strictly below each delta non-directory: each lower path there that is not delta-backed is `D`, with the base entry's type. It removes duplicates, sorts by UTF-8 bytes and never writes. This is the SPEC set comparison without a full base walk (the #9 rule for Rust).

Management and writer lock
12. `create_overlay`: `EEXIST` if `os.path.lexists(db_path)`. Resolve the base (rule 2). Create the AgentFS schema with `AgentFS.open_with`, then the v1 tables and the three config keys in one transaction.
13. `open_merged`: `ENOENT` if `db_path` is missing. Take the lock. Classify with the SPEC table before any write; `Filesystem.from_database` writes, so call it after classification. Initialize or migrate with the statements of `OverlayFS::migrate` (`sdk/rust/src/filesystem/overlayfs.rs:213`) between an explicit `BEGIN IMMEDIATE` and `COMMIT`, with `ROLLBACK` on error; without an explicit `BEGIN`, pyturso autocommits DDL. Without `base`, use `HostAdapter(base_path)` (`EINVAL` if there is no `base_path`). For v1, raise `ESTALE` if `await lower.identity() != base_identity`. Release the lock on every error.
14. `list_overlays`: names from `os.listdir(dir)` that end in `.db` and are regular files by `os.lstat`. `path` is `os.path.join(dir, name)`; sort by `path`. Read `fs_overlay_config` through a short-lived connection. Skip the file on any error or if the table is absent. Legacy: `version` 0, `base_identity` `""`. Filter as SPEC says. Do not check the lock registry.
15. `delete_overlay`: `ENOENT`; take the lock; `EINVAL` without `fs_overlay_config` (release, then raise). While the connection and the registry key still hold the lock, remove `db_path` and each sidecar that exists (`-wal -shm -log -wal-revert -info -changes`) with `os.remove`. Release only after the last removal, so no other opener reaches the database before it is gone (measured: removal under the open connection works, and the release recreates no file).
16. `rebind_overlay`: `ENOENT`; take the lock; `EINVAL` if `version` is absent, `ENOTSUP` if it is not `1`; overwrite `base_path` and `base_identity` in one transaction; release.
17. Take the lock: the key is `(st_dev, st_ino)` of `db_path`. Raise `EBUSY` if a module-level set holds the key; else add it, with no `await` between the check and the add, and connect. A `turso.DatabaseError` whose message contains `Locking error` (another process holds turso's file lock) becomes `EBUSY`.
18. Release the lock: close the connection, drop every reference to it, call `gc.collect()`, discard the key. pyturso 0.4.4 holds the cross-process file lock until the connection object is collected (measured: after `close()`, a child process gets "File is locked by another process" until `gc.collect()`). `close()` and every management operation release before they return.
19. `ErrnoException` subclasses `OSError` with `errno = getattr(errno, code)`. `.code`, `.syscall`, `.path` and the message stay. Add `ELOOP EXDEV ENOTSUP EBUSY ESTALE` to `FsErrorCode`. Add `lstat readlink symlink link chmod` and the five operation names to `FsSyscall`.

## Conformance runner
`sdk/python/tests/test_overlay_conformance.py` has one pytest item per case.
- The item runs the same file as a worker (`sys.executable <file> <index>`, `start_new_session=True`) with the case `timeout` (default 30 s). It kills the process group on timeout and after exit. Exit 0 passes. Exit 1 fails with the worker's stderr. Any other exit is a runner error.
- The worker follows `conformance/overlay/README.md`: the grammar, `$var`, `!x`, the outputs, and `ENOSYS` for an op that it cannot serve. The error name is `e.code`, else `errno.errorcode[e.errno]`. Boot is `create_overlay(db, base)`, then `open_merged(db, base)`, because `open_merged` does not create a database. `hread` is `pread(0, fstat().size)`. Database paths, and the paths that `listOverlays` prints, are relative to the case directory.
- Expected failures come from a new optional key `xfail_py` (issue numbers) in `cases.json`, as TS adds `xfail_ts`. The Python runner reads only `xfail_py`, as `pytest.mark.xfail(strict=True)`: a listed case that passes fails the run. Document both keys in the README.

## Decisions
| Decision | Chosen | Rejected, and why |
|---|---|---|
| Error type | `ErrnoException(OSError)` | builtin `FileNotFoundError` etc.: no `.code`, which SPEC requires; plain `Exception`: `except OSError` misses it |
| Delta access | compose `Filesystem` internals | subclass: inherits delta-only methods with wrong semantics; public `symlink`/`chmod`/`link`/`open` on `Filesystem` first: about 150 more lines, separate parity work |
| Ancestor check | the overlay walk (rule 1) | each adapter walks components: the same code in every adapter |
| Coverage | in-memory set, as Rust and TS | one query per lookup: a query for each path component |
| Diff | pruned walk (rule 11) | walk both full trees: similar size, but the cost grows with the base |
| Handle writes | whole-file rewrite | chunk-level writes: more code; v1 has no partial copy-up |
| Host I/O | blocking `os` calls | `asyncio.to_thread`: more code; no call blocks for long, because FIFOs are never opened |
| Entry points | module functions | `AgentFS` static methods: `open_merged` returns a filesystem, not an `AgentFS` |
| Case isolation | one subprocess per case | `asyncio.wait_for`: cannot stop a blocked FIFO open, which the README requires |
| Expected failures | `xfail_py` key | shared `xfail`: Python would fail when it passes a case that only Rust fails |

## Assumptions (made without a user answer)
- A1: `list_overlays` skips a database that another process holds open: pyturso has no read-only open, so the file "cannot be read" (SPEC: skip). TS chose the same (its Q2).
- A2: `open_merged` takes only a local path. Python databases always have a path, so the `ENOTSUP` row for lockless storage never applies.
- A3: `AgentFS.open` does not check the lock registry. Only the overlay operations do (case 32).
- A4: `write_file` through a dangling final symlink raises `ENOENT`. `open` and `read_file` of a directory raise `EISDIR`. `link` of a directory raises `EPERM`.

## Out of scope (won't build)
- `chown`, `utimens`, `statfs`, `fsync`, special-file creation, the deprecated `delete_file` alias.
- New public `Filesystem` methods for non-overlay use (symlinks, `chmod`, handles).
- Adapters other than `HostAdapter`; database handles instead of paths; encryption, sync or remote databases; KV and tool calls through `open_merged`; host names that are not UTF-8; Windows hosts (`O_NOFOLLOW` and removing an open file are POSIX behavior).
- What v1 leaves unspecified or #14 defers: crash atomicity beyond the transactions above, partial copy-up, chunk-level writes, persisted `ino`, concurrent readers.

## Sub-issues
The estimate is about 630 net source lines, above the 600 budget. Split (stack `py`), in the same stages as TS 12a–12c:

| Sub-issue | Branch / Base | Scope | Budget |
|---|---|---|---|
| py #1 read path | `py/<n>-overlay-read` / `overlay/main` | rules 1–4, 6–8, 11, 12, 13 without migration, 17–19, reads of rule 10; runner; `xfail_py` on every case except 1, 2, 33, 34; a database that needs migration raises `ENOTSUP` | ≤ 350 |
| py #2 mutations | `py/<n>-overlay-write` / py #1's branch | rules 5, 9, writes of rule 10; `xfail_py` left only on 32, 35–38, 41 | ≤ 200 |
| py #3 management | `py/<n>-overlay-manage` / py #2's branch | migration, rules 14–16, interop tests; no `xfail_py` left | ≤ 80 |

py #1 and py #2 have no code dependency on #9, so they can start after approval. py #3 needs #9, because the interop tests use the CLI routed through the SDK. Every stage keeps the full API above, including `rm` and `copy_file`. Approving this spec approves the 630 total.

## TypeScript alignment (#12, PR #37 at 7f2c38b)
Matched, modulo casing: the adapter methods, `locator`, "not found" as `None`/`null`, the overlay-side ancestor check, the host identity; the five operation names; `OverlayInfo` and `Change`; `OverlayFS` with `chmod`, `link`, `diff()`, `close()`; the `ino` maps and the whiteout set; the reuse of delta helpers; the handle rules; the convenience decompositions; the pruned diff; classification before schema writes; the Rust migration statements; the sidecars; skipping locked databases; `Locking error` → `EBUSY`; per-SDK `xfail` keys; the three stages.
Python-only, on purpose: `lstat`, `readlink`, `symlink`, `readdir_plus` and `open` are new on `OverlayFS` (the TS `FileSystem` has them); the adapter returns `Stats` (TS: `LowerStats`); `HostAdapter(root)` is a class (TS: `hostLower(dir)`); paths only (A2); the registry key is `(st_dev, st_ino)` (TS: `realpath`); `list_overlays` reads an overlay that this process holds through a second connection (TS: through the instance); `gc.collect()` on release (rule 18); a subprocess per case; Rust-written v1 whiteouts in the interop test come from the MCP `unlink` tool (TS relies on case 35).

## Validation criteria
Each PR: `cd sdk/python && uv sync && uv run ruff check . && uv run ruff format --check . && uv run pytest` passes. `## Proof` gives the conformance counts and `git diff --numstat` (source and tests separately). Every row of the SPEC "Unsupported in v1" table has a check: cases 30–34, 38 and 41, and `test_handle_write_after_unlink_is_enoent`. The lockless-storage row does not apply (A2). In `sdk/python`:

py #1:
- `uv run pytest tests/test_overlay_conformance.py -v`: 4 passed (cases 1, 2, 33, 34), 38 xfailed, 0 failed.
- `tests/test_overlay.py`: `test_errno_exception_is_oserror` (missing path: `OSError`, `errno.ENOENT`, `code == "ENOENT"`); `test_reads_never_write` (after reads over the conformance base, the row counts of `fs_inode`, `fs_dentry`, `fs_whiteout` and `fs_origin` do not change; `stat("/d/../f").ino == stat("/f").ino`; `open("/p")` raises `ENOTSUP`); `test_diff_does_not_walk_base` (10,000 base files in `/big`, read through a counting adapter around `HostAdapter`: `diff() == []` and `readdir` returned fewer than 100 names); `test_base_symlink_never_followed` (base `esc` is a symlink to the absolute path of an outside directory that holds `s`: `read_file("/esc/s")` raises `ENOENT`); `test_host_adapter_is_rooted` (an outside file at absolute path P holds `OUTSIDE`, and the base holds `LOWER` at base + P: `HostAdapter(base)` `lstat`, `read` and `readdir` on P and its parent answer from the base, and `read_file(P)` returns `LOWER`); `test_other_process_open_is_ebusy` (a child process holds `open_merged`); `test_close_releases_lock` (after `close()` and after `create_overlay()`, a child process opens the database); `test_open_errors` (the `ENOENT`, `ENOTDIR`, `EEXIST`, `EINVAL` and `ESTALE` of rules 2, 12 and 13).

py #2:
- Conformance: 36 passed, 6 xfailed (32, 35–38, 41). `test_handle_write_after_unlink_is_enoent`. `test_diff_does_not_walk_base` adds: after `write_file("/big/f1", "x")`, the diff text is `M d /big; M f /big/f1`, still fewer than 100 names. `test_base_symlink_never_followed` adds: copy up `/k`, close, replace base `k` with a symlink to the outside directory, reopen: `read_file("/k/s")` raises `ENOENT`.

py #3:
- Conformance: 42 passed. `test_failed_migration_changes_nothing` (legacy Go database plus an index on `fs_whiteout(parent_path)`: `open_merged` raises, the column stays, no `fs_overlay_config`); `test_management_errors` (each error of rules 15–16; `delete_overlay` removes each sidecar; a child holding the database makes `delete_overlay` raise `EBUSY`); `test_delete_holds_lock_until_removed` (wrap `os.remove`: before the first removal, a child `open_merged(db)` raises `EBUSY`; afterwards no database file exists and a child `open_merged(db)` raises `ENOENT`); `test_list_keeps_open_overlay_locked`; `test_management_releases_lock` (after `list_overlays` and `rebind_overlay`).
- Interop, 0 skipped (skips only without `AGENTFS_BIN`): `(cd ../../cli && RUSTUP_TOOLCHAIN=nightly-2026-03-01 cargo build) && AGENTFS_BIN=$PWD/../../cli/target/debug/agentfs uv run pytest tests/test_overlay_interop.py -v -rs`.
- `test_python_db_in_cli`: Python runs `unlink /f`, `write_file /d/x x2`, `rename /d/sub /m`, `symlink /d/x /a`, then closes. The `agentfs diff <db>` stdout lines equal `str()` of the Python changes. `agentfs fs <db> cat /d/x` prints `x2`, `cat /m/y` prints `y`, and `cat /f` fails.
- `test_cli_db_in_python`: `agentfs init r --base <base>`, `agentfs fs .agentfs/r.db write /d/x x2`, and the MCP `unlink` tool on `/f` (`agentfs serve mcp`). Python reads `x2` at `/d/x`, `stat /f` raises `ENOENT`, `fs_whiteout` has `/f`, and `diff()` equals the `agentfs diff` output.
