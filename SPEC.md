# Agent Filesystem Specification

**Version:** 0.4

## Introduction

The Agent Filesystem Specification defines a SQLite schema for representing agent filesystem state. The specification consists of three main components:

1. **Tool Call Audit Trail**: Captures tool invocations, parameters, and results for debugging, auditing, and performance analysis
2. **Virtual Filesystem**: Stores agent artifacts (files, documents, outputs) using a Unix-like inode design with support for hard links, proper metadata, and efficient file operations
3. **Key-Value Store**: Provides simple get/set operations for agent context, preferences, and structured state that doesn't fit into the filesystem model

All timestamps in this specification use Unix epoch format (seconds since 1970-01-01 00:00:00 UTC) with optional nanosecond precision via separate `_nsec` columns.

## Tool Calls

The tool call tracking schema captures tool invocations for debugging, auditing, and analysis.

### Schema

#### Table: `tool_calls`

Stores individual tool invocations with parameters and results. This is an insert-only audit log.

```sql
CREATE TABLE tool_calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  parameters TEXT,
  result TEXT,
  error TEXT,
  started_at INTEGER NOT NULL,
  completed_at INTEGER NOT NULL,
  duration_ms INTEGER NOT NULL
)

CREATE INDEX idx_tool_calls_name ON tool_calls(name)
CREATE INDEX idx_tool_calls_started_at ON tool_calls(started_at)
```

**Fields:**

- `id` - Unique tool call identifier
- `name` - Tool name (e.g., 'read_file', 'web_search', 'execute_code')
- `parameters` - JSON-serialized input parameters (NULL if no parameters)
- `result` - JSON-serialized result (NULL if error)
- `error` - Error message (NULL if success)
- `started_at` - Invocation timestamp (Unix timestamp, seconds)
- `completed_at` - Completion timestamp (Unix timestamp, seconds)
- `duration_ms` - Execution duration in milliseconds

### Operations

#### Record Tool Call

```sql
INSERT INTO tool_calls (name, parameters, result, error, started_at, completed_at, duration_ms)
VALUES (?, ?, ?, ?, ?, ?, ?)
```

**Note:** Insert once when the tool call completes. Either `result` or `error` should be set, not both.

#### Query Tool Calls by Name

```sql
SELECT * FROM tool_calls
WHERE name = ?
ORDER BY started_at DESC
```

#### Query Recent Tool Calls

```sql
SELECT * FROM tool_calls
WHERE started_at > ?
ORDER BY started_at DESC
```

#### Analyze Tool Performance

```sql
SELECT
  name,
  COUNT(*) as total_calls,
  SUM(CASE WHEN error IS NULL THEN 1 ELSE 0 END) as successful,
  SUM(CASE WHEN error IS NOT NULL THEN 1 ELSE 0 END) as failed,
  AVG(duration_ms) as avg_duration_ms
FROM tool_calls
GROUP BY name
ORDER BY total_calls DESC
```

### Consistency Rules

1. Exactly one of `result` or `error` SHOULD be non-NULL (mutual exclusion)
2. `completed_at` MUST always be set (no NULL values)
3. `duration_ms` MUST always be set and equal to `(completed_at - started_at) * 1000`
4. Parameters and results MUST be valid JSON strings when present
5. Records MUST NOT be updated or deleted (insert-only audit log)

### Implementation Notes

- This is an insert-only audit log - no updates or deletes
- Insert the record once when the tool call completes
- Set either `result` (on success) or `error` (on failure), but not both
- `parameters`, `result`, and `error` are stored as JSON-serialized strings
- `duration_ms` should be computed as `(completed_at - started_at) * 1000`
- Use indexes for efficient queries by name or time
- Consider periodic archival of old tool call records to a separate table

### Extension Points

Implementations MAY extend the tool call schema with additional functionality:

- Session/conversation grouping (add `session_id` field)
- User attribution (add `user_id` field)
- Cost tracking (add `cost` field for API calls)
- Parent/child relationships for nested tool calls
- Token usage tracking
- Input/output size metrics

Such extensions SHOULD use separate tables to maintain referential integrity.

## Virtual Filesystem

The virtual filesystem provides POSIX-like file operations for agent artifacts. The filesystem separates namespace (paths and names) from data (file content and metadata) using a Unix-like inode design. This enables hard links (multiple paths to the same file), efficient file operations, proper file metadata (permissions, timestamps), and chunked content storage.

### Schema

#### Table: `fs_config`

Stores filesystem-level configuration. This table is initialized once when the filesystem is created and MUST NOT be modified afterward.

```sql
CREATE TABLE fs_config (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
)
```

**Fields:**

- `key` - Configuration key
- `value` - Configuration value (stored as text)

**Required Configuration:**

| Key | Description | Default |
|-----|-------------|---------|
| `chunk_size` | Size of data chunks in bytes | `4096` |

**Notes:**

- `chunk_size` determines the fixed size of data chunks in `fs_data`
- All chunks except the last chunk of a file are exactly `chunk_size` bytes
- Configuration is immutable after filesystem initialization
- Implementations MAY define additional configuration keys

#### Table: `fs_inode`

Stores file and directory metadata.

```sql
CREATE TABLE fs_inode (
  ino INTEGER PRIMARY KEY AUTOINCREMENT,
  mode INTEGER NOT NULL,
  nlink INTEGER NOT NULL DEFAULT 0,
  uid INTEGER NOT NULL DEFAULT 0,
  gid INTEGER NOT NULL DEFAULT 0,
  size INTEGER NOT NULL DEFAULT 0,
  atime INTEGER NOT NULL,
  mtime INTEGER NOT NULL,
  ctime INTEGER NOT NULL,
  rdev INTEGER NOT NULL DEFAULT 0,
  atime_nsec INTEGER NOT NULL DEFAULT 0,
  mtime_nsec INTEGER NOT NULL DEFAULT 0,
  ctime_nsec INTEGER NOT NULL DEFAULT 0
)
```

**Fields:**

- `ino` - Inode number (unique identifier)
- `mode` - File type and permissions (Unix mode bits)
- `nlink` - Number of hard links pointing to this inode
- `uid` - Owner user ID
- `gid` - Owner group ID
- `size` - Total file size in bytes
- `atime` - Last access time (Unix timestamp, seconds)
- `mtime` - Last modification time (Unix timestamp, seconds)
- `ctime` - Creation/change time (Unix timestamp, seconds)
- `rdev` - Device number for character and block devices (major/minor encoded)
- `atime_nsec` - Nanosecond component of last access time (0–999999999)
- `mtime_nsec` - Nanosecond component of last modification time (0–999999999)
- `ctime_nsec` - Nanosecond component of creation/change time (0–999999999)

**Mode Encoding:**

The `mode` field combines file type and permissions:

```
File type (upper bits):
  0o170000 - File type mask (S_IFMT)
  0o100000 - Regular file (S_IFREG)
  0o040000 - Directory (S_IFDIR)
  0o120000 - Symbolic link (S_IFLNK)
  0o010000 - FIFO/named pipe (S_IFIFO)
  0o020000 - Character device (S_IFCHR)
  0o060000 - Block device (S_IFBLK)
  0o140000 - Socket (S_IFSOCK)

Permissions (lower 12 bits):
  0o000777 - Permission bits (rwxrwxrwx)

Example:
  0o100644 - Regular file, rw-r--r--
  0o040755 - Directory, rwxr-xr-x
```

**Special Inodes:**

- Inode 1 MUST be the root directory

#### Table: `fs_dentry`

Maps names to inodes (directory entries).

```sql
CREATE TABLE fs_dentry (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  parent_ino INTEGER NOT NULL,
  ino INTEGER NOT NULL,
  UNIQUE(parent_ino, name)
)

CREATE INDEX idx_fs_dentry_parent ON fs_dentry(parent_ino, name)
```

**Fields:**

- `id` - Internal entry ID
- `name` - Basename (filename or directory name)
- `parent_ino` - Parent directory inode number
- `ino` - Inode this entry points to

**Constraints:**

- `UNIQUE(parent_ino, name)` - No duplicate names in a directory

**Notes:**

- Root directory (ino=1) has no dentry (no parent)
- Multiple dentries MAY point to the same inode (hard links)
- Link count is stored in `fs_inode.nlink` and must be incremented/decremented when dentries are added/removed

#### Table: `fs_data`

Stores file content in fixed-size chunks. Chunk size is configured at filesystem level via `fs_config`.

```sql
CREATE TABLE fs_data (
  ino INTEGER NOT NULL,
  chunk_index INTEGER NOT NULL,
  data BLOB NOT NULL,
  PRIMARY KEY (ino, chunk_index)
)
```

**Fields:**

- `ino` - Inode number
- `chunk_index` - Zero-based chunk index (chunk 0 contains bytes 0 to chunk_size-1)
- `data` - Binary content (BLOB), exactly `chunk_size` bytes except for the last chunk

**Notes:**

- Directories MUST NOT have data chunks
- Chunk size is determined by the `chunk_size` value in `fs_config`
- All chunks except the last chunk of a file MUST be exactly `chunk_size` bytes
- The last chunk MAY be smaller than `chunk_size`
- Byte offset for a chunk = `chunk_index * chunk_size`
- To read at byte offset `N`: `chunk_index = N / chunk_size`, `offset_in_chunk = N % chunk_size`

#### Table: `fs_symlink`

Stores symbolic link targets.

```sql
CREATE TABLE fs_symlink (
  ino INTEGER PRIMARY KEY,
  target TEXT NOT NULL
)
```

**Fields:**

- `ino` - Inode number of the symlink
- `target` - Target path (may be absolute or relative)

### Operations

#### Path Resolution

To resolve a path to an inode:

1. Start at root inode (ino=1)
2. Split path by `/` and filter empty components
3. For each component:
   ```sql
   SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?
   ```
4. Return final inode or NULL if any component not found

#### Creating a File

1. Resolve parent directory path to inode
2. Get chunk size from config:
   ```sql
   SELECT value FROM fs_config WHERE key = 'chunk_size'
   ```
3. Insert inode:
   ```sql
   INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
   VALUES (?, ?, ?, 0, ?, ?, ?)
   RETURNING ino
   ```
4. Insert directory entry:
   ```sql
   INSERT INTO fs_dentry (name, parent_ino, ino)
   VALUES (?, ?, ?)
   ```
5. Increment link count:
   ```sql
   UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?
   ```
6. Split data into chunks and insert each:
   ```sql
   INSERT INTO fs_data (ino, chunk_index, data)
   VALUES (?, ?, ?)
   ```
   Where `chunk_index` starts at 0 and increments for each chunk.
7. Update inode size:
   ```sql
   UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?
   ```

#### Reading a File

1. Resolve path to inode
2. Fetch all chunks in order:
   ```sql
   SELECT data FROM fs_data WHERE ino = ? ORDER BY chunk_index ASC
   ```
3. Concatenate chunks in order
4. Update access time:
   ```sql
   UPDATE fs_inode SET atime = ? WHERE ino = ?
   ```

#### Reading a File at Offset

To read `length` bytes starting at byte offset `offset`:

1. Resolve path to inode
2. Get chunk size from config:
   ```sql
   SELECT value FROM fs_config WHERE key = 'chunk_size'
   ```
3. Calculate chunk range:
   - `start_chunk = offset / chunk_size`
   - `end_chunk = (offset + length - 1) / chunk_size`
4. Fetch required chunks:
   ```sql
   SELECT chunk_index, data FROM fs_data
   WHERE ino = ? AND chunk_index >= ? AND chunk_index <= ?
   ORDER BY chunk_index ASC
   ```
5. Extract the requested byte range from the chunks:
   - `offset_in_first_chunk = offset % chunk_size`
   - Skip first `offset_in_first_chunk` bytes of first chunk
   - Take `length` total bytes across chunks

#### Listing a Directory

1. Resolve directory path to inode
2. Query entries:
   ```sql
   SELECT name FROM fs_dentry WHERE parent_ino = ? ORDER BY name ASC
   ```

#### Deleting a File

1. Resolve path to get inode and parent
2. Delete directory entry:
   ```sql
   DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?
   ```
3. Decrement link count:
   ```sql
   UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?
   ```
4. Check if last link:
   ```sql
   SELECT nlink FROM fs_inode WHERE ino = ?
   ```
5. If nlink = 0, delete inode and data:
   ```sql
   DELETE FROM fs_inode WHERE ino = ?
   DELETE FROM fs_data WHERE ino = ?
   ```

#### Creating a Hard Link

1. Resolve source path to get inode
2. Resolve destination parent to get parent_ino
3. Insert new directory entry:
   ```sql
   INSERT INTO fs_dentry (name, parent_ino, ino)
   VALUES (?, ?, ?)
   ```
4. Increment link count:
   ```sql
   UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?
   ```

#### Reading File Metadata (stat)

1. Resolve path to inode
2. Query inode (includes link count):
   ```sql
   SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
          atime_nsec, mtime_nsec, ctime_nsec
   FROM fs_inode WHERE ino = ?
   ```

### Initialization

When creating a new agent database, initialize the filesystem configuration and root directory:

```sql
-- Initialize filesystem configuration
INSERT INTO fs_config (key, value) VALUES ('chunk_size', '4096');

-- Initialize root directory
INSERT INTO fs_inode (ino, mode, nlink, uid, gid, size, atime, mtime, ctime)
VALUES (1, 16877, 1, 0, 0, 0, unixepoch(), unixepoch(), unixepoch());
```

Where `16877` = `0o040755` (directory with rwxr-xr-x permissions)

**Note:** The `chunk_size` value can be customized at filesystem creation time but MUST NOT be changed afterward. The root directory has `nlink=1` as it has no parent directory entry.

### Consistency Rules

1. Root inode (ino=1) MUST always exist
2. Every dentry MUST reference a valid inode
3. Every dentry MUST reference a valid parent inode
4. No directory MAY contain duplicate names
5. Directories MUST have mode with S_IFDIR bit set
6. Regular files MUST have mode with S_IFREG bit set
7. File size MUST match total size of all data chunks
8. Every inode MUST have at least one dentry (except root)

### Implementation Notes

- Use `RETURNING` clause to safely get auto-generated inode numbers
- Parent directories are created implicitly as needed
- Empty files have an inode but no data chunks
- Symlink resolution is implementation-defined (not part of schema)
- Use transactions for multi-step operations to maintain consistency

### Extension Points

Implementations MAY extend the filesystem schema with additional functionality:

- Extended attributes table
- File ACLs and advanced permissions
- Quota tracking per user/group
- Version history and snapshots
- Content deduplication
- Compression metadata
- File checksums/hashes

Such extensions SHOULD use separate tables to maintain referential integrity.

## Overlay Filesystem

The overlay filesystem puts a writable **delta** (an AgentFS database) over a read-only **base** (the lower layer). All changes go to the delta. The overlay never writes to the base. This section defines **overlay format version 1** (v1). Every SDK that implements overlays MUST implement v1.

### Terms

- **Path**: an absolute, `/`-separated path with no empty, `.` or `..` components and no trailing `/`. The root is `/`. Paths compare as byte strings.
- **parent(P)**: P without its last component.
- **The lower resolves P**: every proper ancestor of P is a directory in the base, and the base has an entry at P. The base is read one component at a time and never follows symlinks.
- **P is delta-backed**: the delta has an entry at P, resolved through `fs_dentry` from inode 1.
- **P is covered**: `fs_whiteout` has a row for P or for a proper ancestor of P.
- **Merged entry**: an entry of the merged view. It is delta-backed or lower-backed.

### Schema

#### Table: `fs_overlay_config`

```sql
CREATE TABLE fs_overlay_config (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
)
```

| Key | Value |
|-----|-------|
| `version` | Overlay format version: `1` |
| `base_path` | Base locator. Host directory: its canonical absolute path. Other lower adapters: an adapter-defined string. |
| `base_identity` | Base identity at initialization (see Lower Layer) |

Initialization writes the three keys in one transaction. After that, only migration and `rebindOverlay` change these rows.

#### Table: `fs_whiteout`

```sql
CREATE TABLE fs_whiteout (
  path TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL
)
```

- `path` - Path whose base entry, and the base subtree below it, is hidden. Never `/`.
- `created_at` - Insertion time (Unix timestamp, seconds)
- A whiteout hides base entries only. It never hides a delta entry.
- v1 never deletes a whiteout. A whiteout at a delta-backed directory makes that directory opaque: its base children stay hidden.

#### Table: `fs_origin`

```sql
CREATE TABLE fs_origin (
  delta_ino INTEGER PRIMARY KEY,
  lower_path TEXT
)
```

- `delta_ino` - Delta inode that a copy-up created
- `lower_path` - Path of the copied base entry at copy-up time, in this overlay's base layer (the only lower layer in v1). NULL means the path is unknown: the origin was migrated from a legacy database.
- Origins record provenance only. Lookup, stat, readdir and diff MUST NOT read `fs_origin`.

### Lower Layer

An SDK reads the base through a read-only lower adapter. The adapter provides:

- `identity()` - a non-empty string that identifies the base
- `lstat(path)` - stats, or "not found"; it never follows symlinks at any component
- `readdir(path)` - entry names
- `readlink(path)` - symlink target
- `read(path, offset, length)` - bytes of a regular file

The **host adapter** serves a host directory. Its locator is the canonical absolute path of the directory. Its identity is `host:<st_dev>:<st_ino>` of that directory in decimal, for example `host:2049:1234567`. A remount can change `st_dev` (for example after a container restart). The identity then changes and `openMerged` fails with `ESTALE` until the user calls `rebindOverlay`. Browser and Cloudflare targets inject their own adapter.

The base MUST NOT change while an overlay over it is open. The base MAY change while no overlay over it is open. v1 does not detect such changes: after reopen, lower-backed entries show the current base, and delta entries and whiteouts keep their meaning by path.

### Lookup Precedence

The merged entry at P (P is not `/`) can exist only if the merged entry at parent(P) is a directory. Then:

1. If P is delta-backed, return the delta entry.
2. Else, if P is covered, return "not found".
3. Else, if the lower resolves P, return the base entry.
4. Else, return "not found".

The merged root is the delta root.

### Reads and Handles

- `stat`, `lstat`, `readlink`, `open`, `read`, `readdir` and `access` MUST NOT write to the delta for a lower-backed entry.
- `readdir(P)` returns the name of each merged entry whose parent is P, once, sorted by byte order, without `.` and `..`.
- `readdir-plus(P)` returns, for each name, the same stats as `lstat` of that path.
- A handle refers to the merged entry, not to a layer. After a copy-up of the entry, every handle of the entry reads and writes the delta copy, including handles opened before the copy-up.

### Copy-Up

Copy-up of the lower-backed entry at P:

1. Copy up each lower-backed proper ancestor of P, top-down.
2. Create the delta entry at P with the same type, permission bits, uid and gid. A regular file gets its full content. A symlink gets the same target. A directory gets no children; its base children stay visible through lookup.
3. Insert `(delta_ino, P)` into `fs_origin`.

- Copy-up does not create or delete whiteouts, and does not change the merged `ino`.
- Copy-up of a special file (FIFO, character device, block device, socket) fails with `ENOTSUP`.
- v1 does not specify timestamps after copy-up.
- If the change that causes the copy-up truncates the file to 0 bytes, copy-up MAY skip the content.

These changes copy up a lower-backed entry first: a write or truncate through any handle, `chmod`, `chown`, `utimens`, and `rename`. Creating an entry in a lower-backed directory copies up that directory. `open` never copies up, whatever its flags; an open that truncates is a truncate.

### Mutations

Each mutation applies the POSIX error rules to the merged view (for example `ENOENT`, `EEXIST`, `EISDIR`, `ENOTDIR`, `ENOTEMPTY`). **Hide(P)** means: if the lower resolves P and P is not covered, insert a whiteout for P.

- **Create** (file, directory, symlink, special file) at P: copy up parent(P) if it is lower-backed, then create the entry in the delta. A whiteout at P stays, so a new directory at P is empty.
- **unlink(P)**, **rmdir(P)**: delete the delta entry at P, if there is one. Then Hide(P). Whiteouts below P stay.
- **link(S, D)**: if S is lower-backed, fail with `EXDEV`. Else copy up parent(D) if it is lower-backed, then link in the delta.
- **rename(S, D)**:
  1. If S does not exist, fail with `ENOENT`.
  2. If S and D are the same path, or the same delta inode, return success and change nothing.
  3. If D is below S, fail with `EINVAL`. Apply the POSIX rules for an existing D.
  4. Copy up S. If S is a directory, copy up each lower-backed entry of its merged subtree, top-down. Check the full subtree first: if it contains a lower special file, fail with `ENOTSUP` and change nothing.
  5. Copy up parent(D) if it is lower-backed. Delete the delta entry at D, if there is one.
  6. Move the delta entry from S to D. Moved entries keep their origins.
  7. Hide(S). Hide(D). Hide(D) makes a moved directory opaque over a replaced base directory.

SDK convenience operations (for example `writeFile`, `rm`, `copyFile`) behave as their decomposition into these operations.

### Symlink Resolution

SDK path resolution walks the merged view one component at a time.

- A symlink at an intermediate component is always followed. Which operations follow a symlink at the final component is as in the base filesystem.
- The target comes from the merged entry. A relative target resolves from the directory that contains the symlink. An absolute target resolves from the merged root, never from the host root.
- `..` goes to the merged parent. At the root, `..` stays at the root.
- A resolution that follows more than 40 symlinks fails with `ELOOP`.
- A kernel mount (FUSE, NFS) resolves symlinks itself. These rules apply to SDK path APIs.

### Stat Identity

- The stats of a merged entry come from its backing layer: the delta entry if it is delta-backed, else the base entry.
- The open instance gives each merged entry an `ino`. The root has `ino` 1.
- In one instance, an entry keeps its `ino` through `lookup`, `stat`, `lstat`, `readdir-plus`, `fstat`, copy-up and rename. Distinct entries have distinct values, except delta hard links of one delta inode, which share one. An instance never reuses an `ino`.
- An entry starts at its first lookup. It ends when it is removed or replaced.
- After close and reopen, `ino` values MAY differ. The type, permission bits, uid, gid, size, nlink and mtime of each delta-backed entry MUST NOT change. The same applies to a lower-backed entry whose base entry did not change.

### Writer Policy

- One instance at a time opens an overlay database. v1 has no separate reader mode.
- The instance holds an exclusive lock on the database from open to close. A second open, from any process or from the same process, fails with `EBUSY`.
- For a local database file, the engine's exclusive file lock (turso takes it on each read-write open) enforces the rule across processes. The SDK MUST also refuse a second open of the same file in its own process.
- Storage that serializes access by construction (for example a Cloudflare Durable Object) satisfies the rule.
- An overlay on storage that allows concurrent clients without a lock (for example a remote serverless database) fails to open with `ENOTSUP`.
- `listOverlays` reads `fs_overlay_config` without the lock. `deleteOverlay` takes the lock first.

### Versioning and Migration

`openMerged` classifies the database before any other access:

| Database state | Action |
|----------------|--------|
| `version` is `1` | Open as v1. |
| `version` has another value | Fail with `ENOTSUP`. Do not write. |
| `fs_overlay_config` exists and has no `version` (legacy Rust) | Migrate. |
| No `fs_overlay_config`, base given (plain AgentFS or legacy Go) | Initialize v1, migrating legacy Go tables. |
| No `fs_overlay_config`, no base given | Fail with `EINVAL`. |

Migration runs in one transaction. A failed migration leaves the database unchanged.

1. `fs_whiteout`: create it if absent. If it has a `parent_path` column (legacy Go), rebuild it as v1 and keep every row. Drop `idx_fs_whiteout_parent`.
2. `fs_origin`: create it if absent. If it has a `base_ino` column, rebuild it as v1: keep every row and set `lower_path` to NULL.
3. `fs_overlay_config`: create it if absent. Write `base_path` and `base_identity` from the base that is being opened. Write `version` = `1` last.

The AgentFS schema version (`0.4`) is independent of the overlay format version.

### Management Operations

Every SDK exposes these operations in its own naming style. `base` is a host directory path or a lower adapter.

- `createOverlay(dbPath, base) → OverlayInfo` - Create a database at `dbPath` with the AgentFS schema and an initialized v1 overlay. If `dbPath` exists, fail with `EEXIST`. If a host base does not exist, fail with `ENOENT`.
- `openMerged(dbPath, base?) → MergedFS` - If `dbPath` does not exist, fail with `ENOENT`. Take the lock (Writer Policy). Classify the database (Versioning and Migration). Without `base`, use the host adapter at `base_path`: fail with `EINVAL` if there is no `base_path`, and with `ENOENT` if the directory does not exist. For a v1 database, if the base identity is not `base_identity`, fail with `ESTALE`. Where storage has no path, `dbPath` is an SDK-native database handle.
- `MergedFS` - The SDK's filesystem interface with the semantics of this section, plus `diff()` and `close()`. `close()` releases the lock.
- `MergedFS.diff() → Change[]` - See Diff.
- `listOverlays(dir, base?) → OverlayInfo[]` - Read each regular file named `*.db` directly in `dir` that has `fs_overlay_config`. Skip files that cannot be read (for example encrypted databases). With `base`, keep only overlays whose `base_identity` is the identity of `base` (legacy: whose `base_path` is its canonical path). Sort by path.
- `deleteOverlay(dbPath)` - If `dbPath` does not exist, fail with `ENOENT`. Take the lock. If the database has no `fs_overlay_config`, fail with `EINVAL`. Delete `dbPath` and its engine sidecar files (`-wal`, `-shm`, `-info`).
- `rebindOverlay(dbPath, base)` - If `dbPath` does not exist, fail with `ENOENT`. Take the lock. If `version` is absent, fail with `EINVAL`; if it is not `1`, fail with `ENOTSUP`. In one transaction, overwrite `base_path` and `base_identity` with the locator and identity of `base`. Only this explicit call rebinds; `openMerged` never rebinds.
- `OverlayInfo` - `{ path, version, basePath, baseIdentity }`. A legacy overlay has `version` 0 and an empty `baseIdentity`.
- `Change` - `{ change: A | M | D, type: f | d | l | ?, path }`.

### Diff

`diff()` compares the merged tree with the base tree. It does not write to the delta.

- L is the set of base paths and M is the set of merged paths, both without `/`. Both walks enter directories only and never follow symlinks.
- `A`: P is in M and not in L. `M`: P is in L and in M, and P is delta-backed. `D`: P is in L and not in M.
- The type is `f` (regular file), `d` (directory), `l` (symlink) or `?` (other). `A` and `M` use the merged entry. `D` uses the base entry.
- Changes are sorted by path in byte order. The text form is one line per change, `<change> <type> <path>`, for example `M f /src/main.rs`.

### Unsupported in v1

| Operation | Required error |
|-----------|----------------|
| A second open of an open overlay database | `EBUSY` |
| `link` with a lower-backed source (hard link across layers) | `EXDEV` |
| Copy-up or `open` of a lower special file | `ENOTSUP` |
| An overlay format version other than 1 | `ENOTSUP` |
| An overlay on storage that allows concurrent clients without a lock | `ENOTSUP` |
| A base whose identity is not `base_identity` (recover with `rebindOverlay`) | `ESTALE` |

- SDKs report each error with its POSIX name (TypeScript and Python: `ErrnoException.code`).
- Base hard links are independent merged entries. A copy-up of one path does not change the others.
- Copy-up copies the full file. v1 has no partial copy-up.
- Special files that are created in the delta follow the Virtual Filesystem section.
- v1 does not specify: crash atomicity of multi-step operations, timestamp fidelity on copy-up, `ino` stability across reopen, reads through a handle after its entry is removed, directory redirects, hard-link identity across layers, concurrent readers, and mount session lifecycle.

### Decisions

| Decision | Rationale | Rejected alternative |
|----------|-----------|----------------------|
| A delta entry wins over a whiteout | A whiteout then also marks an opaque directory, so v1 needs no opaque flag. | Whiteout checked first (hides recreated entries) |
| Whiteouts are never deleted | Create and delete need no whiteout bookkeeping. | Delete on create (base children come back) |
| No `parent_path` column | No v1 query lists child whiteouts; coverage uses point lookups on `path`. | Keep `parent_path` and its index with no reader |
| Origin is a base path, never read by semantics | Stable and portable to all adapters; stale origins cannot change a stat. | Base inode numbers (unstable, not portable) |
| Directory rename copies up the subtree | No new schema; lookup stays a pure path merge. | Redirects (new table); `EXDEV` (breaks SDK callers) |
| Copy-up on first change, never on open | TypeScript and Python `open` has no flags; one rule fits every SDK. | Copy-up on a write-intent open |
| Instance-scoped `ino` | Browser and Cloudflare adapters have no stable inode numbers. | A persisted `ino` map |
| One opener, enforced by a lock | Turso already takes the lock; caches need no invalidation. | Concurrent readers with generation checks |
| Host identity is `st_dev` and `st_ino` of the base root | `st_ino` repeats across filesystems (mount roots are often inode 1). A false match merges an unrelated tree; a false mismatch is loud and loses no data. | `st_ino` only (false matches); filesystem UUID (no portable API in Node); content hash (slow) |
| Only an explicit `rebindOverlay` changes the base binding | Identity changes on remount are expected; an explicit rebind keeps false matches impossible without stranding data. | Automatic rebind in `openMerged` (false matches); no recovery (stranded data) |
| Legacy origins migrate with `lower_path` NULL | A legacy row stores a process-local counter, and the current dentry path is wrong after a rename. | Derive the path from the current dentry; keep `base_ino` |
| The base may change while no overlay is open | The path merge gives a defined result for any base state, and users edit project bases between sessions. | Base immutable for the overlay's lifetime (unenforceable; breaks that workflow) |
| Hard link across layers fails with `EXDEV` | A copy-up would split the base link group; tools fall back on `EXDEV`. | Copy up, then link |
| Lower special files fail with `ENOTSUP` | Copying a FIFO or device by content blocks or corrupts. | Copy-up as a new special file |
| Diff is a set comparison of two trees | Independent of storage; no host probes; correct for symlinks. | Probe host paths with `exists()` |
| Management operations take a database path | Matches `.agentfs/<id>.db`; no new layout. | An overlay store with generated ids |
| Unknown version fails, migration is one transaction | Old SDKs never rewrite newer formats; failures leave legacy data intact. | Best-effort upgrade |
| Legacy migration trusts the base given at the first v1 open | Legacy databases store no identity, and Rust already rewrites `base_path` on each open. | Reject legacy databases |

### Conformance Cases

Each case starts with a new overlay over this base. `write P "s"` creates P if it is absent and replaces its content. `read P` returns the full content. `open` uses read-write flags where the SDK has flags. `reopen` closes the overlay and calls `openMerged` again. `diff →` lists changes in text form, separated by `;`. Modes are octal.

```
/f        regular file "base", mode 0644
/d/       directory, mode 0750
/d/x      regular file "x"
/d/sub/   directory
/d/sub/y  regular file "y"
/e/       empty directory
/l        symlink "d/x"
/p        FIFO
```

1. `readdir /` → `d e f l p`; `diff` → empty.
2. `h = open /f`; `h.read` → `base`; `reopen`; `diff` → empty.
3. `write /f "new"`; `read /f` → `new`; base `/f` → `base`; `diff` → `M f /f`.
4. `h = open /f`; `write /f "new"` by path; `h.read` → `new`.
5. `chmod /f 0600`; `reopen`; `stat /f` → permission bits `0600`; `diff` → `M f /f`.
6. `write /d/x "x2"`; `stat /d` → permission bits `0750`; `diff` → `M d /d; M f /d/x`.
7. `write /d/n "n"`; `readdir /d` → `n sub x`; `diff` → `M d /d; A f /d/n`.
8. `unlink /f`; `stat /f` → `ENOENT`; `readdir /` → `d e l p`; `diff` → `D f /f`.
9. `unlink /f`; `write /f "again"`; `reopen`; `read /f` → `again`; `diff` → `M f /f`.
10. `rmdir /d` → `ENOTEMPTY`; `unlink /d` → `EISDIR`; `rmdir /f` → `ENOTDIR`; `mkdir /d` → `EEXIST`.
11. `unlink /d/sub/y`; `rmdir /d/sub`; `unlink /d/x`; `rmdir /d`; `readdir /` → `e f l p`; `diff` → `D d /d; D d /d/sub; D f /d/sub/y; D f /d/x`.
12. Case 11, then `mkdir /d`; `reopen`; `readdir /d` → empty; `diff` → `M d /d; D d /d/sub; D f /d/sub/y; D f /d/x`.
13. Case 11, then `write /d "z"`; `stat /d/x` → `ENOTDIR`; `diff` → `M f /d; D d /d/sub; D f /d/sub/y; D f /d/x`.
14. `rename /f /f`; `read /f` → `base`; `diff` → empty.
15. `rename /d /d`; `readdir /d` → `sub x`; `diff` → empty.
16. `write /n "1"`; `link /n /n2`; `rename /n /n2`; `stat /n` and `stat /n2` succeed.
17. `rename /f /g`; `stat /f` → `ENOENT`; `read /g` → `base`; `diff` → `D f /f; A f /g`.
18. `rename /d /m`; `reopen`; `readdir /m` → `sub x`; `read /m/sub/y` → `y`; `stat /d` → `ENOENT`; `diff` → `D d /d; D d /d/sub; D f /d/sub/y; D f /d/x; A d /m; A d /m/sub; A f /m/sub/y; A f /m/x`.
19. `rename /d /m`; `mkdir /d`; `readdir /d` → empty.
20. `rename /d /e`; `readdir /e` → `sub x`; `stat /d` → `ENOENT`; `diff` → `D d /d; D d /d/sub; D f /d/sub/y; D f /d/x; M d /e; A d /e/sub; A f /e/sub/y; A f /e/x`.
21. `rename /f /d/x`; `read /d/x` → `base`; `stat /f` → `ENOENT`; `diff` → `M d /d; M f /d/x; D f /f`.
22. `rename /d /d/sub/z` → `EINVAL`; `rename /e /d` → `ENOTEMPTY`; `rename /f /e` → `EISDIR`; `rename /e /f` → `ENOTDIR`.
23. `mkdir /k`; `write /k/a "1"`; `rename /k /k2`; `readdir /k2` → `a`; `diff` → `A d /k2; A f /k2/a`.
24. `read /l` → `x`; `write /d/x "x2"`; `read /l` → `x2`; `readlink /l` → `d/x`; `lstat /l` → symlink.
25. `unlink /d/x`; `read /l` → `ENOENT`; `lstat /l` → symlink.
26. `symlink "/d/x" /a`; `read /a` → `x`; `diff` → `A l /a`.
27. `symlink "b" /a`; `symlink "a" /b`; `stat /a` → `ELOOP`.
28. `i = stat(/f).ino`; `write /f "new"`; `rename /f /g`; `stat(/g).ino` → `i`; `readdir-plus /` entry `g` → `ino` `i`; `stat(/).ino` → `1`; `stat(/d).ino` ≠ `i`.
29. `chmod /d 0700`; `write /d/x "z"`; `reopen`; `readdir-plus /` entry `d` and `stat /d` → permission bits `0700`; `readdir-plus /d` entry `x` and `stat /d/x` → size `1`.
30. `link /f /f2` → `EXDEV`; `write /n "1"`; `link /n /n2`; `stat(/n).ino` = `stat(/n2).ino`.
31. `chmod /p 0600` → `ENOTSUP`; `open /p` → `ENOTSUP`; `rename /p /q` → `ENOTSUP`; `unlink /p`; `diff` → `D ? /p`.
32. Keep overlay A open; `openMerged` of its database → `EBUSY`; `deleteOverlay` → `EBUSY`; close A; `openMerged` succeeds.
33. `close`; set `version` to `2` through a separate raw database connection, then close that connection; `openMerged` → `ENOTSUP`; the rows of `fs_overlay_config`, `fs_whiteout` and `fs_origin` are unchanged.
34. `createOverlay(db, base)`; `openMerged(db, other directory)` → `ESTALE`; `openMerged(plain AgentFS database)` without base → `EINVAL`.
35. Legacy Rust database (`unlink /f`, `write /d/x "x2"`, `rename /d/x /d/z`); `openMerged` with base → `version` `1`; the `fs_whiteout` rows are unchanged; every `fs_origin` row is kept with `lower_path` NULL; `read /d/z` → `x2`; `diff` → `M d /d; D f /d/x; A f /d/z; D f /f`.
36. Legacy Go database with a whiteout row for `/f`: `openMerged` without base → `EINVAL`; with base → `version` `1`; `fs_whiteout` has row `/f` and no `parent_path` column; `stat /f` → `ENOENT`.
37. `createOverlay(dir/a.db, base)`; again → `EEXIST`; `listOverlays(dir)` and `listOverlays(dir, base)` → `dir/a.db`; `listOverlays(dir, other directory)` → empty; `deleteOverlay(dir/a.db)`; `listOverlays(dir)` → empty.
38. Directories A and B have the same root `st_ino` on different filesystems (on Linux: `/dev/shm` and `/dev`). `createOverlay(dir/a.db, A)`; `openMerged(dir/a.db, B)` → `ESTALE`; `listOverlays(dir, B)` → empty.
39. On a private copy of the base: `write /d/x "x2"`; `close`; set base `/f` to `edited` and base `/d/x` to `b2`; `openMerged` without base; `read /f` → `edited`; `read /d/x` → `x2`; `diff` → `M d /d; M f /d/x`.
40. On a private copy of the base: `write /f "new"`; `close`; replace base `/f` with a directory that contains regular file `x`; `openMerged` without base; `diff` → `M f /f; D f /f/x`.
41. A and B are different directories. `createOverlay(dir/a.db, A)`; `openMerged(dir/a.db, A)`; `write /n "1"`; `close`; `openMerged(dir/a.db, B)` → `ESTALE`; `rebindOverlay(dir/a.db, B)`; `openMerged(dir/a.db)` succeeds; `read /n` → `1`.

## Key-Value Data

The key-value store provides simple get/set operations for agent context and state.

### Schema

#### Table: `kv_store`

Stores arbitrary key-value pairs with automatic timestamping.

```sql
CREATE TABLE kv_store (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL,
  created_at INTEGER DEFAULT (unixepoch()),
  updated_at INTEGER DEFAULT (unixepoch())
)

CREATE INDEX idx_kv_store_created_at ON kv_store(created_at)
```

**Fields:**

- `key` - Unique key identifier
- `value` - JSON-serialized value
- `created_at` - Creation timestamp (Unix timestamp, seconds)
- `updated_at` - Last update timestamp (Unix timestamp, seconds)

### Operations

#### Set a Value

```sql
INSERT INTO kv_store (key, value, updated_at)
VALUES (?, ?, unixepoch())
ON CONFLICT(key) DO UPDATE SET
  value = excluded.value,
  updated_at = unixepoch()
```

#### Get a Value

```sql
SELECT value FROM kv_store WHERE key = ?
```

#### Delete a Value

```sql
DELETE FROM kv_store WHERE key = ?
```

#### List All Keys

```sql
SELECT key, created_at, updated_at FROM kv_store ORDER BY key ASC
```

### Consistency Rules

1. Keys MUST be unique (enforced by PRIMARY KEY)
2. Values MUST be valid JSON strings
3. Timestamps MUST use Unix epoch format (seconds)

### Implementation Notes

- Values are stored as JSON strings; serialize before storing, deserialize after retrieving
- Use `ON CONFLICT` clause for upsert operations
- Indexes on `created_at` support temporal queries
- Updates automatically refresh the `updated_at` timestamp
- Keys can use any naming convention (e.g., namespaced: `user:preferences`, `session:state`)

### Extension Points

Implementations MAY extend the key-value store schema with additional functionality:

- Namespaced keys with hierarchy support
- Value versioning/history
- TTL (time-to-live) for automatic expiration
- Value size limits and quotas

Such extensions SHOULD use separate tables to maintain referential integrity.

## Revision History

### Overlay Format v1

- Replaced the Overlay Filesystem section with overlay format v1: `fs_overlay_config` (`version`, `base_path`, `base_identity`), `fs_whiteout` without `parent_path`, and `fs_origin` with `lower_path` in place of `base_ino`
- Defined lookup, copy-up, mutations, rename, symlink resolution, stat identity, the writer policy, migration, management operations, diff, unsupported operations and conformance cases
- The AgentFS schema version stays 0.4

### Version 0.4

- Added nanosecond timestamp precision for `atime`, `mtime`, and `ctime`
- Added `atime_nsec`, `mtime_nsec`, `ctime_nsec` columns to `fs_inode` table (DEFAULT 0 for backward compatibility)
- Nanosecond precision enables correct NFS `wcc_data` cache invalidation when multiple operations occur within the same second
- Added POSIX special file support (FIFOs, character devices, block devices, sockets)
- Added `rdev` column to `fs_inode` table for device major/minor numbers
- Added `S_IFIFO`, `S_IFCHR`, `S_IFBLK`, `S_IFSOCK` file type constants to Mode Encoding
- Updated stat query to include `rdev` field

### Version 0.3

- Added `fs_origin` table to Overlay Filesystem for tracking copy-up origin inodes
- Origin tracking ensures consistent inode numbers after copy-up (similar to Linux overlayfs `trusted.overlay.origin`)

### Version 0.2

- Added Overlay Filesystem section with `fs_whiteout` table for copy-on-write semantics
- Whiteout table includes `parent_path` column with index for efficient O(1) child lookups
- Added `nlink` column to `fs_inode` table to store link count directly
- Link count is now maintained in the inode rather than computed via COUNT(*) on `fs_dentry`

### Version 0.1

- Added `fs_config` table for filesystem-level configuration
- Changed `fs_data` table to use fixed-size chunks with `chunk_index` instead of variable-size chunks with `offset` and `size`
- Added `chunk_size` configuration option (default: 4096 bytes)
- Added "Reading a File at Offset" operation for efficient partial reads
- Chunk-based storage enables efficient random access reads without loading entire files

### Version 0.0

- Initial specification
- Tool call audit trail (`tool_calls` table)
- Virtual filesystem (`fs_inode`, `fs_dentry`, `fs_data`, `fs_symlink` tables)
- Key-value store (`kv_store` table)
