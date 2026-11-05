"""Overlay filesystem: a writable AgentFS delta over a read-only base."""

import gc
import itertools
import os
import sys
import traceback
from dataclasses import dataclass, replace
from typing import Literal, NoReturn, Protocol

import turso
from turso.aio import Connection, connect

from .agentfs import AgentFS
from .constants import S_IFDIR, S_IFLNK, S_IFMT, S_IFREG
from .errors import ErrnoException, FsErrorCode, FsSyscall
from .filesystem import Filesystem, Stats

DbPath = str | os.PathLike[str]

_COLS = "i.ino, i.mode, i.nlink, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime"
_TYPES = {S_IFREG: "f", S_IFDIR: "d", S_IFLNK: "l"}
_SCHEMA = (
    "CREATE TABLE IF NOT EXISTS fs_overlay_config (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS fs_whiteout (path TEXT PRIMARY KEY, created_at INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS fs_origin (delta_ino INTEGER PRIMARY KEY, lower_path TEXT)",
)
_LEGACY = (
    "legacy overlay database: open it once with the agentfs CLI or Rust SDK to upgrade to format v1"
)
_held: set[tuple[int, int]] = set()


def _fail(code: FsErrorCode, syscall: FsSyscall, path: str | None = None) -> NoReturn:
    raise ErrnoException(code, syscall, path)


def _join(directory: str, name: str) -> str:
    return ("" if directory == "/" else directory) + "/" + name


def _is_dir(stats: Stats | None) -> bool:
    return stats is not None and stats.is_directory()


async def _q(db: Connection, sql: str, *args) -> list:
    return await (await db.execute(sql, args)).fetchall()


class LowerAdapter(Protocol):
    """Read-only base; each path is a SPEC "Path"."""

    locator: str

    async def identity(self) -> str: ...
    async def lstat(self, path: str) -> Stats | None: ...
    async def readdir(self, path: str) -> list[str]: ...
    async def readlink(self, path: str) -> str: ...
    async def read(self, path: str, offset: int, length: int) -> bytes: ...


class HostAdapter:
    """A host directory as a base. Every call acts on `locator + path`."""

    def __init__(self, root: str | os.PathLike[str]) -> None:
        self.locator = os.path.realpath(root)
        try:
            mode = os.stat(self.locator).st_mode
        except FileNotFoundError:
            _fail("ENOENT", "stat", self.locator)
        if mode & S_IFMT != S_IFDIR:
            _fail("ENOTDIR", "stat", self.locator)

    async def identity(self) -> str:
        st = os.stat(self.locator)
        return f"host:{st.st_dev}:{st.st_ino}"

    async def lstat(self, path: str) -> Stats | None:
        try:
            st = os.lstat(self.locator + path)
        except (FileNotFoundError, NotADirectoryError):
            return None
        times = (int(st.st_atime), int(st.st_mtime), int(st.st_ctime))
        return Stats(st.st_ino, st.st_mode, st.st_nlink, st.st_uid, st.st_gid, st.st_size, *times)

    async def readdir(self, path: str) -> list[str]:
        return os.listdir(self.locator + path)

    async def readlink(self, path: str) -> str:
        return os.readlink(self.locator + path)

    async def read(self, path: str, offset: int, length: int) -> bytes:
        fd = os.open(self.locator + path, os.O_RDONLY | os.O_NOFOLLOW)
        try:
            return os.pread(fd, length, offset)
        finally:
            os.close(fd)


Base = DbPath | LowerAdapter


def _lower(base: Base) -> LowerAdapter:
    return HostAdapter(base) if isinstance(base, (str, os.PathLike)) else base


@dataclass(frozen=True)
class OverlayInfo:
    path: str
    version: int
    base_path: str
    base_identity: str


@dataclass(frozen=True)
class Change:
    """One difference between the merged tree and the base; `str()` is the SPEC text form."""

    change: Literal["A", "M", "D"]
    type: Literal["f", "d", "l", "?"]
    path: str

    def __str__(self) -> str:
        return f"{self.change} {self.type} {self.path}"


@dataclass(frozen=True)
class DirEntry:
    name: str
    stats: Stats


@dataclass(frozen=True)
class _Node:
    """A merged entry. `lower` is the base entry at `path` if the base resolves it uncovered."""

    ino: int
    path: str
    stats: Stats
    delta: int | None
    lower: Stats | None


class _Lock:
    """The in-process claim on a database file and the connection that holds its file lock."""

    db: Connection | None = None
    key: tuple[int, int] | None = None

    async def take(self, path: str) -> None:
        if os.path.exists(path):
            st = os.stat(path)
            if (st.st_dev, st.st_ino) in _held:
                _fail("EBUSY", "open", path)
            self.key = (st.st_dev, st.st_ino)
            _held.add(self.key)
        try:
            self.db = await connect(path)
        except turso.DatabaseError as e:
            if "Locking error" not in str(e):
                raise
            raise ErrnoException("EBUSY", "open", path) from None

    async def release(self) -> None:
        """Closes the connection, frees all references; pyturso locks until collected."""
        db, self.db = self.db, None
        try:
            if db is not None:
                await db.close()
        finally:
            del db
            traceback.clear_frames(sys.exc_info()[2])
            gc.collect()
            _held.discard(self.key)
            self.key = None


class OverlayFile:
    def __init__(self, fs: "OverlayFS", node: _Node) -> None:
        self._fs = fs
        self._node = node

    async def pread(self, offset: int, size: int) -> bytes:
        return await self._fs._pread(self._node, offset, size)

    async def fstat(self) -> Stats:
        return await self._fs._stats(self._node.ino, self._node.delta, self._node.path)


class OverlayFS:
    """A writable AgentFS delta over a read-only base."""

    def __init__(self, lock: _Lock, delta: Filesystem, lower: LowerAdapter) -> None:
        self._lock = lock
        self._db = lock.db
        self._delta: Filesystem | None = delta
        self._lower = lower
        self._whiteouts: set[str] = set()
        self._inos = itertools.count(2)
        self._by_delta = {1: 1}
        self._by_lower: dict[str, int] = {}

    async def close(self) -> None:
        """Releases the writer lock. A second call does nothing."""
        self._db = self._delta = None
        await self._lock.release()

    async def __aenter__(self) -> "OverlayFS":
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.close()

    async def _stats(self, ino: int, delta: int | None, path: str) -> Stats:
        if delta is None:
            stats = await self._lower.lstat(path)
        else:
            rows = await _q(self._db, f"SELECT {_COLS} FROM fs_inode i WHERE i.ino = ?", delta)
            stats = Stats(*rows[0]) if rows else None
        return replace(stats, ino=ino) if stats else _fail("ENOENT", "stat", path)

    async def _pread(self, node: _Node, offset: int, size: int) -> bytes:
        if node.delta is None:
            return await self._lower.read(node.path, offset, size)
        return (await self._delta._read_inode(node.delta))[offset : offset + size]

    async def _child(self, parent: _Node, name: str) -> _Node | None:
        path = _join(parent.path, name)
        visible = _is_dir(parent.lower) and path not in self._whiteouts
        if parent.delta is not None:
            sql = f"SELECT {_COLS} FROM fs_dentry d JOIN fs_inode i ON i.ino = d.ino"
            rows = await _q(
                self._db, f"{sql} WHERE d.parent_ino = ? AND d.name = ?", parent.delta, name
            )
            if rows:
                stats = Stats(*rows[0])
                lower = await self._lower.lstat(path) if visible and stats.is_directory() else None
                ino = self._by_delta.setdefault(stats.ino, next(self._inos))
                return _Node(ino, path, replace(stats, ino=ino), stats.ino, lower)
        lower = await self._lower.lstat(path) if visible else None
        if lower is None:
            return None
        ino = self._by_lower.setdefault(path, next(self._inos))
        return _Node(ino, path, replace(lower, ino=ino), None, lower)

    async def _target(self, node: _Node) -> str:
        if node.delta is None:
            return await self._lower.readlink(node.path)
        rows = await _q(self._db, "SELECT target FROM fs_symlink WHERE ino = ?", node.delta)
        return rows[0][0] if rows else _fail("ENOENT", "readlink", node.path)

    async def _find(self, path: str, follow_last: bool, syscall: FsSyscall) -> _Node:
        stack = [self._root]
        parts = path.split("/")[::-1]
        node: _Node | None = stack[0]
        links = 0
        while parts:
            name = parts.pop()
            if name in ("", ".", ".."):
                if name == ".." and len(stack) > 1:
                    stack.pop()
                node = stack[-1]
                continue
            node = await self._child(stack[-1], name)
            if node is None:
                break
            if node.stats.is_symbolic_link() and (parts or follow_last):
                links += 1
                if links > 40:
                    _fail("ELOOP", syscall, path)
                target = await self._target(node)
                if target.startswith("/"):
                    del stack[1:]
                parts.extend(target.split("/")[::-1])
                node = stack[-1]
            elif parts:
                if not node.stats.is_directory():
                    _fail("ENOTDIR", syscall, path)
                stack.append(node)
        return node or _fail("ENOENT", syscall, path)

    async def _file(self, path: str) -> _Node:
        node = await self._find(path, True, "open")
        if not node.stats.is_file():
            _fail("EISDIR" if node.stats.is_directory() else "ENOTSUP", "open", path)
        return node

    async def _dir(self, path: str) -> _Node:
        node = await self._find(path, True, "scandir")
        return node if node.stats.is_directory() else _fail("ENOTDIR", "scandir", path)

    async def _names(self, directory: _Node) -> list[str]:
        sql = "SELECT name FROM fs_dentry WHERE parent_ino = ?"
        names = (
            {r[0] for r in await _q(self._db, sql, directory.delta)} if directory.delta else set()
        )
        if _is_dir(directory.lower):
            lower = await self._lower.readdir(directory.path)
            names.update(n for n in lower if _join(directory.path, n) not in self._whiteouts)
        return sorted(names, key=str.encode)

    async def stat(self, path: str) -> Stats:
        return (await self._find(path, True, "stat")).stats

    async def lstat(self, path: str) -> Stats:
        return (await self._find(path, False, "lstat")).stats

    async def read_file(self, path: str, encoding: str | None = "utf-8") -> bytes | str:
        node = await self._file(path)
        data = await self._pread(node, 0, node.stats.size)
        return data.decode(encoding) if encoding else data

    async def readdir(self, path: str) -> list[str]:
        return await self._names(await self._dir(path))

    async def readdir_plus(self, path: str) -> list[DirEntry]:
        directory = await self._dir(path)
        children = [(n, await self._child(directory, n)) for n in await self._names(directory)]
        return [DirEntry(name, node.stats) for name, node in children if node]

    async def readlink(self, path: str) -> str:
        node = await self._find(path, False, "readlink")
        if not node.stats.is_symbolic_link():
            _fail("EINVAL", "readlink", path)
        return await self._target(node)

    async def access(self, path: str) -> None:
        await self._find(path, True, "access")

    async def open(self, path: str) -> OverlayFile:
        return OverlayFile(self, await self._file(path))

    async def diff(self) -> list[Change]:
        """Merged tree vs base, walking only what a delta entry or whiteout hides."""
        changes: dict[str, Change] = {}
        hidden = list(self._whiteouts)
        memo = {"/": self._root.lower}

        async def base(path: str) -> Stats | None:
            chain = [path]
            while chain[-1] not in memo:
                chain.append(chain[-1].rsplit("/", 1)[0] or "/")
            for p in reversed(chain[:-1]):
                parent = memo[p.rsplit("/", 1)[0] or "/"]
                memo[p] = await self._lower.lstat(p) if _is_dir(parent) else None
            return memo[path]

        sql = "SELECT d.name, i.ino, i.mode FROM fs_dentry d JOIN fs_inode i ON i.ino = d.ino"
        queue = [(1, "/")]
        while queue:
            ino, directory = queue.pop()
            for name, child, mode in await _q(self._db, f"{sql} WHERE d.parent_ino = ?", ino):
                path = _join(directory, name)
                lower = await base(path)
                changes[path] = Change("M" if lower else "A", _TYPES.get(mode & S_IFMT, "?"), path)
                if mode & S_IFMT == S_IFDIR:
                    queue.append((child, path))
                elif _is_dir(lower):
                    hidden.append(path)
        while hidden:
            path = hidden.pop()
            if stats := await base(path):
                changes.setdefault(path, Change("D", _TYPES.get(stats.mode & S_IFMT, "?"), path))
                if stats.is_directory():
                    hidden.extend(_join(path, n) for n in await self._lower.readdir(path))
        return sorted(changes.values(), key=lambda c: c.path.encode())


async def _init_v1(db: Connection, lower: LowerAdapter, identity: str) -> None:
    config = [("base_path", lower.locator), ("base_identity", identity), ("version", "1")]
    await db.execute("BEGIN IMMEDIATE")
    try:
        for statement in _SCHEMA:
            await db.execute(statement)
        await db.executemany("INSERT INTO fs_overlay_config (key, value) VALUES (?, ?)", config)
        await db.execute("COMMIT")
    except BaseException:
        await db.execute("ROLLBACK")
        raise


async def _mount(lock: _Lock, path: str, base: Base | None) -> OverlayFS:
    db = lock.db
    tables = {r[0] for r in await _q(db, "SELECT name FROM sqlite_master WHERE type = 'table'")}
    has_config = "fs_overlay_config" in tables
    config = dict(await _q(db, "SELECT key, value FROM fs_overlay_config")) if has_config else {}
    version = config.get("version")
    if version not in (None, "1"):
        _fail("ENOTSUP", "open_merged", path)
    base = config.get("base_path") if base is None else base
    if base is None:
        _fail("EINVAL", "open_merged", path)
    go = "SELECT 1 FROM pragma_table_info('fs_whiteout') WHERE name = 'parent_path'"
    if version is None and (has_config or await _q(db, go)):
        raise ErrnoException("ENOTSUP", "open_merged", path, _LEGACY)
    lower = _lower(base)
    identity = await lower.identity()
    if version is None:
        await _init_v1(db, lower, identity)
    elif identity != config["base_identity"]:
        _fail("ESTALE", "open_merged", path)
    fs = OverlayFS(lock, await Filesystem.from_database(db), lower)
    fs._whiteouts = {r[0] for r in await _q(db, "SELECT path FROM fs_whiteout")}
    fs._root = _Node(1, "/", await fs._stats(1, 1, "/"), 1, await lower.lstat("/"))
    return fs


async def create_overlay(db_path: DbPath, base: Base) -> OverlayInfo:
    """Creates a database at `db_path` with an initialized v1 overlay over `base`."""
    path = os.fspath(db_path)
    if os.path.lexists(path):
        _fail("EEXIST", "create_overlay", path)
    lower = _lower(base)
    identity = await lower.identity()
    lock = _Lock()
    try:
        await lock.take(path)
        await AgentFS.open_with(lock.db)
        await _init_v1(lock.db, lower, identity)
    finally:
        await lock.release()
    return OverlayInfo(path, 1, lower.locator, identity)


async def open_merged(db_path: DbPath, base: Base | None = None) -> OverlayFS:
    """Opens the overlay in `db_path` and holds its writer lock until `close()`."""
    path = os.fspath(db_path)
    if not os.path.exists(path):
        _fail("ENOENT", "open_merged", path)
    lock = _Lock()
    try:
        await lock.take(path)
        return await _mount(lock, path, base)
    except BaseException:
        await lock.release()
        raise
