# TypeScript overlay API (spec v1)
Refs #12. Spec only: implementation waits for #9. `SPEC.md` "Overlay Filesystem" is normative; this doc maps it onto `sdk/typescript` and does not restate it. The work adds `OverlayFS` (a read-only lower layer under a writable AgentFS delta), the spec v1 management operations, a Node host adapter, injected lowers, and a TS runner for `conformance/overlay/cases.json`. The estimate (≈710 net source lines) exceeds the 600 budget, so it splits into three stacked sub-issues whose budgets total 750; approving this spec approves that total.

## Facts this design rests on (`overlay/main` af42485)
- `FileSystem` (`src/filesystem/interface.ts:136`) has no `chmod` or `link`. Cases 5, 16, 29, 30 and 31 need them.
- The delta class `AgentFS` (`src/filesystem/agentfs.ts`) never follows symlinks (`lstat` calls `stat`, :649), throws `ENOSYS` from `unlink` and `rename` on a symlink (`src/guards.ts:64`), creates missing parents with mode 0755 in `writeFile` (:461), and writes `atime` in `readFile` (:552).
- Turso JS (`@tursodatabase/database` 0.4.0-pre.18, probed in this run): a second open of one file **in the same process succeeds**. From another process, read-write and `{ readonly: true }` opens both fail with `GenericFailure` "Locking error: Failed locking file. File is locked by another process".
- `@tursodatabase/database-wasm` keeps files in OPFS through `createSyncAccessHandle()`, which is exclusive per file.
- The Cloudflare `AgentFS` is a separate synchronous implementation over Durable Object `SqlStorage`. It is not a `DatabasePromise`.

## Lower adapter (`src/overlay/overlayfs.ts`, browser-safe)
```ts
export interface LowerStats { mode: number; uid: number; gid: number; size: number; nlink: number; atime: number; mtime: number; ctime: number } // mode has S_IFMT bits; times in s
export interface LowerAdapter {
  readonly locator: string;                         // written to base_path
  identity(): Promise<string>;                      // written to base_identity; non-empty
  lstat(path: string): Promise<LowerStats | null>;  // null = not found
  readdir(path: string): Promise<string[]>;         // names, any order
  readlink(path: string): Promise<string>;
  read(path: string, offset: number, length: number): Promise<Uint8Array>; // short at EOF
}
```
- Every `path` is a spec Path (`/`, `/d/x`). OverlayFS calls a method on P only after `lstat` of each proper ancestor of P returned a directory in the same walk. So no call passes through a base symlink, and an adapter resolves P with one lookup.
- OverlayFS checks the `lstat` type first and never calls `read` on a special file. An adapter error other than "not found" propagates unchanged.

## Node host adapter (`src/overlay/node.ts`, Node entry only)
- `hostLower(dir: string): Promise<LowerAdapter>`. `locator` = `fs.promises.realpath(dir)`; a missing `dir` fails with `ENOENT`.
- `identity()` = `host:${dev}:${ino}` from `fs.promises.stat(locator, { bigint: true })`. BigInt keeps both numbers exact, so the string equals Rust's (`sdk/rust/src/filesystem/overlayfs.rs:201`).
- `lstat` = `fs.promises.lstat(locator + P)`: `ENOENT` and `ENOTDIR` give `null`; times are `Math.floor(ms / 1000)`. `readdir`, `readlink` and `read` use `fs.promises`; `read` closes its `FileHandle` in `finally`.

## Injected lowers (browser, Cloudflare)
- The caller passes any object that implements `LowerAdapter` as `base`. The SDK ships no browser or Cloudflare adapter.
- Without a file path, the database argument is a `DatabasePromise` that the caller opened (spec: "SDK-native database handle"), for example `@tursodatabase/database-wasm`. `index_browser.ts` exports `OverlayFS` and the handle forms of `createOverlay`, `openMerged` and `rebindOverlay`. There `base` is required (no host adapter can serve `base_path`), the entry passes the `buffer` package's `Buffer` as `AgentFS.openWith` does (`index_browser.ts:12`), and nothing imports `node.ts`.
- Cloudflare injects the lower the same way, but the MVP has no Durable Object delta: OverlayFS issues `BEGIN`/`COMMIT`, and DO SQL rejects them (it requires `transactionSync`). See Q1.

## `OverlayFS` (`src/overlay/overlayfs.ts`)
`export class OverlayFS implements FileSystem` (the existing interface, unchanged), plus:
```ts
chmod(path: string, mode: number): Promise<void>;           // follows the final symlink
link(existingPath: string, newPath: string): Promise<void>; // does not follow
diff(): Promise<Change[]>; // Change = { change: 'A' | 'M' | 'D'; type: 'f' | 'd' | 'l' | '?'; path: string }
close(): Promise<void>;    // releases the lock; closes the database only if openMerged opened it; a second call does nothing
```
- Only `openMerged` constructs it. State: `db`, `lower`, `chunkSize`, `whiteouts: Set<string>` and the entry table. The set loads at open (like Rust `load_whiteouts`) and needs no invalidation, because one instance opens the database.
- **Walk.** One primitive resolves every path argument one component at a time: `walk(path, followLast) → { parent, name, node | null, path }`. A node holds `deltaIno` (from `fs_dentry`), `covered`, and `lower` (base stats, set only if the lower resolves P and P is not covered). Precedence and symlink rules are the spec's: more than 40 links → `ELOOP`; `..` goes to the merged parent; an absolute target starts at the merged root. A missing or non-directory intermediate component fails with `ENOENT` or `ENOTDIR`.
- **Entries and `ino`.** `Entry = { ino, deltaIno?, lowerPath? }`, indexed by `byDelta: Map<number, Entry>` and `byLower: Map<string, Entry>`. The root is 1; a counter from 2 never reuses a value. Copy-up moves the entry from `byLower` to `byDelta` and keeps its `ino`. Removal or replacement deletes its keys. Delta hard links share one entry.
- **Delta writes.** Own SQL writes `fs_dentry` moves, `fs_symlink`, `fs_whiteout`, `fs_origin` and `fs_inode.mode`. Reuse `AgentFSFile`, `createInode`, `createDentry` and `removeDentryAndMaybeInode` from `filesystem/agentfs.ts`: export the class, drop `private` from the three methods, and mark all four `@internal`. Never call the delta's public path methods.
- **Reads** (`stat`, `lstat`, `readdir`, `readdirPlus`, `readFile`, `readlink`, `access`, `open`) never write to the delta. `readdir` merges the delta children with the base children minus whiteouts, sorted by UTF-8 bytes (not JS string order). `statfs` returns the delta's `statfs()`.
- **Handles.** `open` binds a handle to the `Entry`, so after a copy-up every handle uses the delta copy. Before `pwrite` or `truncate` on a lower-backed entry, the handle re-walks `lowerPath`. If the merged entry there is not this entry, it fails with `ENOENT`. Otherwise it copies up (`truncate(0)` skips the content). `open` of a lower special file fails with `ENOTSUP`.
- **Mutations** follow spec Copy-Up and Mutations step by step. Convenience operations decompose. `writeFile` = create missing parents (the current `FileSystem` contract) + create, or copy up without content + truncate + write. `rm` = `unlink`/`rmdir` over the merged subtree, bottom-up. `copyFile` = `readFile` + `writeFile` + `chmod` to the source permission bits.
- **`diff()`** returns exactly the spec's set comparison without walking the full base (the rule #9 adds for Rust). It walks the delta tree (A or M). It also walks the base subtree below each whiteout, and below each delta path whose base entry is a directory while the delta entry is a non-directory or is covered: D for each base path there that is not delta-backed. Then it dedupes and sorts by UTF-8 bytes.

## Management operations
| Spec | Node entry (`node.ts`) | Browser entry (`overlayfs.ts`) |
|---|---|---|
| `createOverlay` | `createOverlay(db: string \| DatabasePromise, base: string \| LowerAdapter): Promise<OverlayInfo>` | `createOverlay(db: DatabasePromise, base: LowerAdapter)` |
| `openMerged` | `openMerged(db: string \| DatabasePromise, base?: string \| LowerAdapter): Promise<OverlayFS>` | `openMerged(db, base: LowerAdapter)` |
| `listOverlays` | `listOverlays(dir: string, base?: string \| LowerAdapter): Promise<OverlayInfo[]>` | not exported |
| `deleteOverlay` | `deleteOverlay(dbPath: string): Promise<void>` | not exported |
| `rebindOverlay` | `rebindOverlay(db: string \| DatabasePromise, base: string \| LowerAdapter): Promise<void>` | `rebindOverlay(db, base: LowerAdapter)` |

- `OverlayInfo = { path: string; version: number; basePath: string; baseIdentity: string }`; a legacy overlay has `version` 0 and `baseIdentity` `''`. A string `base` means `hostLower(base)`. A handle `db` uses the browser form, so it needs `base`. On a handle, "`dbPath` exists" (`EEXIST`) means the database has a table.
- Checks run in the spec's order and throw `createFsError`, so `err.code` is the POSIX name. Add `EBUSY`, `EXDEV`, `ENOTSUP`, `ESTALE` and `ELOOP` to `FsErrorCode`, and `chmod` and `link` to `FsSyscall` (`src/errors.ts`).
- `openMerged` classifies the database **before** `AgentFS.fromDatabase`, because `initialize()` writes (case 33: "do not write"). Migration runs in one `BEGIN`…`COMMIT` with the statements of Rust `OverlayFS::migrate` (`overlayfs.rs:213`).
- `deleteOverlay` deletes `dbPath` and the sidecars `-wal`, `-shm`, `-log`, `-wal-revert`, `-info` and `-changes`; a missing sidecar is not an error.
- `listOverlays` opens each `*.db` with `{ readonly: true }` and skips a file it cannot open. It reads an overlay that this process holds through the registered instance. See Q2.

## Writer lock per target
A module registry `Map<string | DatabasePromise, OverlayFS>` holds each open overlay. `openMerged`, `deleteOverlay` and `rebindOverlay` check it (`EBUSY`) and register in one synchronous step as soon as the key is known, and unregister on failure and in `close()`.

| Target | Registry key | Other processes or contexts | Result |
|---|---|---|---|
| Node file | `realpath(dbPath)` | Turso file lock; an error whose message contains `Locking error` maps to `EBUSY` | rule holds |
| Browser (wasm) | the handle | The OPFS access handle is exclusive; a second tab fails in its own `connect()` | rule holds (A3) |
| Cloudflare DO | the handle | The DO serializes access by construction | rule holds (no DO delta in MVP) |
| Serverless | — | lockless | `createOverlay`, `openMerged`, `rebindOverlay` fail with `ENOTSUP`; `createServerlessAdapter` marks its result `lockless: true` |

## Conformance runner (`sdk/typescript/tests/overlay-conformance.test.ts`)
- Reads `conformance/overlay/cases.json`; one vitest test per case, with a timeout of `timeout` seconds (default 30). `npm test` runs it. New script: `"test:conformance": "vitest run tests/overlay-conformance.test.ts"`.
- Each case: a temp root with `base/`, `other/` and `dir/`; `sh -ec <base script>` in `base/`; `createOverlay(root/db, root/base)`, then `openMerged(root/db, root/base)`.
- It serves every op of the README grammar. Path ops call the current `OverlayFS`; `hread` = `fstat().size` + `pread`; `reopen` = `close()` + `openMerged(db)`; `sql` uses a separate raw `Database` (`a|b; c|d`, `NULL`); `plainDb` = `AgentFS.open({ path })` + `close()`; `legacyRust` copies `sdk/rust/tests/fixtures/legacy_rust_overlay.db`; `host*` act on the case's base. A relative path argument is relative to the root, an absolute one (`/dev/shm`) stays, and `listOverlays` prints root-relative paths. A failed step yields `err.code`; an unknown op yields `ENOSYS`.
- It adds an optional key `xfail_ts` (issue numbers) to `cases.json` and its README. The TS runner reads only `xfail_ts`; the Rust runner keeps reading `xfail`. A case with `xfail_ts` runs as `test.fails`, so a passing xfail case fails the run, as in Rust. See Q3.

## Decisions
| Chosen | Rejected (why) |
|---|---|
| Own SQL for rows, plus reused `AgentFSFile` and inode helpers | Delta path methods (symlink `ENOSYS` guards, no modes, auto-parents); all-new SQL (repeats ≈160 lines of chunk I/O) |
| OverlayFS walks the merged view itself | Delta path resolution (never follows symlinks) |
| Lower calls only below verified base directories | Each adapter walks components (repeated in every adapter) |
| `chmod`/`link` only on `OverlayFS` | Add them to `FileSystem` (forces `AgentFS` and Cloudflare changes) |
| Pruned `diff()` | Full base walk (≈10 fewer lines; O(base) reads, slow for remote injected lowers) |
| In-process registry plus engine lock | Engine lock only (Turso permits a second in-process open) |
| BigInt stats for identity | Number stats (inexact above 2^53) |

## Assumptions
- A1: #9 lands with `SPEC.md` semantics; the CLI `fs` and `diff` go through `openMerged`; #8 and #9 remove their `xfail` markers.
- A2: Turso lock behavior is as probed above; implementation re-checks it on the pinned version.
- A3: OPFS exclusivity gives cross-tab exclusion. Not run here (no browser in this environment).
- A4: `writeFile` keeps creating missing parents (`interface.ts:159`). Copy-up copies the base timestamps.

## Won't build
- A Durable Object delta adapter, or any built-in browser or Cloudflare lower adapter (Q1).
- `listOverlays` and `deleteOverlay` in the browser entry (a handle has no directory and no file to delete).
- `chmod`/`link` on `FileSystem`; `chown`, `utimens`, `mknod`, path `truncate` (`FileSystem` has none of them).
- OverlayFS in `just-bash` or in the Cloudflare `AgentFS`; encryption options; Go; host names that are not UTF-8.
- What v1 leaves unspecified or #14 defers: crash atomicity, partial or streaming copy-up, persisted `ino`, timestamp fidelity, concurrent readers.

## LOC estimate (net source; tests reported separately)
| Component | Lines |
|---|---|
| Types, errors, exports, serverless flag, `agentfs.ts` visibility | 45 |
| Walk, entries and `ino`, whiteout set, stats | 110 |
| Reads, read-only handle | 70 |
| `diff()` | 45 |
| Copy-up, Hide, writable handle | 75 |
| Mutations: create, `unlink`/`rmdir`/`rm`, `link`, `chmod`, `rename`, `copyFile` | 175 |
| Management core: registry, classify, v1 init, identity, migration, rebind, handle forms | 95 |
| Node: `hostLower`, path forms, `EBUSY` mapping, sidecars | 95 |
| **Total** (tests: runner ≈170, unit and interop ≈150) | **≈710 (range 680–760)** |

## Proposed split (stack `ts`; file the sub-issues after `SPEC APPROVED`)
| Sub-issue | Scope | Base | Budget |
|---|---|---|---|
| 12a read path | Types, `hostLower`, walk, entries, reads, read-only handle, `diff()`, registry, classify, v1 init, identity, `createOverlay` and `openMerged` (both forms), errors, exports, runner, `xfail_ts` | `overlay/main` after #9 | 400 |
| 12b mutations | Copy-up, Hide, writable handles, every mutation | 12a's branch | 260 |
| 12c management | Migration, `listOverlays`, `deleteOverlay`, `rebindOverlay`, serverless `ENOTSUP`, interop test | 12b's branch | 90 |

- Each verifies with `cd sdk/typescript && npm ci && npm run build && npm test && npm run test:conformance`, plus V7 (12a) or V8 (12c).
- 12a AC: V1 with cases 1, 2, 33 and 34 passing and every other case marked `xfail_ts` with 12b or 12c; V2, V3, V4 (first half), V5, V7.
- 12b AC: V1 with every case passing except 32, 35, 36, 37, 38 and 41, whose `xfail_ts` names only 12c; V4 (second half); V6.
- 12c AC: V1 with all 42 cases passing and no `xfail_ts` left; V8; V9.

## Validation criteria
- V1: `npm run test:conformance` prints `fail 0` and the pass and xfail counts that the sub-issue states.
- V2 (`tests/overlay.test.ts`): reads over the conformance base. `readdir('/')` = `d e f l p`; `stat('/l')` is a file; `lstat('/l')` is a symlink; `readFile('/l')` = `x`; `stat('/d')` bits = 0750; `stat('/f/x')` → `ENOTDIR`; `stat('/d/../f').ino` = `stat('/f').ino`; `open('/p')` → `ENOTSUP` within 1 s.
- V3: after V2, the row counts of `fs_inode`, `fs_dentry`, `fs_whiteout` and `fs_origin` are unchanged; `stat('/').ino` = 1; each `readdirPlus('/')` `ino` equals the `stat` `ino`.
- V4: base with 10,000 files in `/big`, read through a counting wrapper around `hostLower`. `diff()` = `[]` and `readdir` returned < 100 names in total. In 12b: after `writeFile('/big/f1', 'x')`, the `diff()` text is `M d /big; M f /big/f1`, still < 100 names.
- V5: while the test holds `openMerged(db)`, a child `node` process that imports `dist/index_node.js` and calls `openMerged(db)` prints `EBUSY`.
- V6: a handle test: `h = open('/f')`; `unlink('/f')`; `h.pwrite` → `ENOENT`. With cases 30–34 and 38, V5 and V9, every row of the spec's "Unsupported in v1" table has a test.
- V7: `cd sdk/typescript && npm run build && ! grep -lE "from ['\"](node:)?(fs|fs/promises|path|os|child_process)['\"]" dist/overlay/overlayfs.js && ! grep -q overlay/node dist/index_browser.js`.
- V8: interop, `tests/overlay-interop.test.ts` (`npm test` skips it; `npm run test:interop` fails without `AGENTFS_BIN`). Command: `cd cli && RUSTUP_TOOLCHAIN=nightly-2026-03-01 cargo build && cd ../sdk/typescript && AGENTFS_BIN=$PWD/../../cli/target/debug/agentfs npm run test:interop`.
  - Rust → TS: `agentfs init --base <base> r`, `agentfs fs r write /d/x x2`, `agentfs fs r write /n 1`. TS `openMerged('.agentfs/r.db')` reads `/d/x` = `x2`, `/n` = `1`, `/f` = `base`, and its `diff()` lines equal the `agentfs diff r` stdout lines. Case 35 covers Rust-written whiteouts and origins.
  - TS → Rust: TS `createOverlay`, `openMerged`, `unlink('/f')`, `writeFile('/d/x', 'x2')`, `rename('/d/sub', '/d/s2')`, `symlink('d/x', '/a')`, `close()`. Then `agentfs fs <db> cat /d/x` = `x2`, `agentfs fs <db> cat /d/s2/y` = `y`, `agentfs fs <db> ls /` has no `f`, and the `agentfs diff <db>` lines equal the TS `diff()` lines.
- V9: `openMerged(createServerlessAdapter(conn), lower)` fails with `ENOTSUP` before any query.

## Open questions (recommended default in bold)
- Q1 Cloudflare delta: **defer a DO `DatabasePromise` adapter to #14**; the MVP meets the issue's Cloudflare goal as a contract only (lower injection and lock rule). Alternative: a sub-issue 12d (≈60 lines that map `BEGIN`/`COMMIT` onto DO transactions, plus a DO test harness that the repo lacks).
- Q2 Turso also locks read-only opens, so `listOverlays` cannot read an overlay that another process holds: **skip it** (spec: "skip files that cannot be read"). Alternatives: fail the call with `EBUSY`; list it with `version` -1.
- Q3 **Add `xfail_ts`** for TS-only expected failures (12a and 12b need them). Alternative: share `xfail` across SDKs, which breaks when TS passes a case that only Rust fails.
