"""Overlay filesystem: a writable AgentFS delta over a read-only base."""

import asyncio
import contextlib
import errno
import fcntl
import functools
import gc
import itertools
import os
import stat
import sys
import time
import traceback
from dataclasses import dataclass, replace
from typing import Literal, NoReturn, Protocol

import turso
from turso.aio import Connection, connect

from .agentfs import AgentFS
from .constants import DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG
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
_SIDECARS = ("-wal", "-shm", "-log", "-wal-revert", "-info", "-changes")
_held: dict[tuple[int, int], "OverlayInfo | None"] = {}


def _fail(code: FsErrorCode, syscall: FsSyscall, path: str | None = None) -> NoReturn:
    raise ErrnoException(code, syscall, path)


def _join(directory: str, name: str) -> str:
    return ("" if directory == "/" else directory) + "/" + name


def _is_dir(stats: Stats | None) -> bool:
    return stats is not None and stats.is_directory()


def _locked(op):
    """Runs a mutation alone, so concurrent copy-ups and creates cannot interleave."""

    @functools.wraps(op)
    async def run(self, *args, **kwargs):
        async with self._mutex:
            return await op(self, *args, **kwargs)

    return run


async def _q(db: Connection, sql: str, *args) -> list:
    return await (await db.execute(sql, args)).fetchall()


async def _run(db: Connection, sql: str, *args) -> None:
    await db.execute(sql, args)
    await db.commit()


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
    fd: int | None = None

    def _pin(self, path: str, syscall: FsSyscall) -> None:
        """Opens and locks the existing file, so a delete cannot run before the engine opens it."""
        try:
            self.fd = os.open(path, os.O_RDWR)
            fcntl.lockf(self.fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            same = [(s.st_dev, s.st_ino) for s in (os.fstat(self.fd), os.stat(path))]
        except FileNotFoundError:
            _fail("ENOENT", syscall, path)
        except OSError as e:
            if e.errno not in (errno.EACCES, errno.EAGAIN):
                raise
            _fail("EBUSY", syscall, path)
        if same != [self.key] * 2:
            _fail("ENOENT", syscall, path)

    async def take(self, path: str, syscall: FsSyscall, create: bool = False) -> None:
        try:
            st = os.stat(path)
        except FileNotFoundError:
            if not create:
                _fail("ENOENT", syscall, path)
        else:
            key = (st.st_dev, st.st_ino)
            if key in _held:
                _fail("EBUSY", syscall, path)
            if st.st_nlink > 1:
                _fail("ENOTSUP", syscall, path)
            self.key = key
            _held[key] = None
            self._pin(path, syscall)
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
            _held.pop(self.key, None)
            if self.fd is not None:
                os.close(self.fd)
            self.key = self.fd = None


class OverlayFile:
    def __init__(self, fs: "OverlayFS", node: _Node) -> None:
        self._fs = fs
        self._node = node

    async def pread(self, offset: int, size: int) -> bytes:
        return await self._fs._pread(self._node, offset, size)

    async def pwrite(self, offset: int, data: bytes) -> None:
        await self._fs._splice(self._node, offset, data, True)

    async def truncate(self, size: int) -> None:
        await self._fs._splice(self._node, size, b"", False)

    async def fstat(self) -> Stats:
        node, fs = self._node, self._fs
        return await fs._stats(node.ino, fs._delta_ino(node), node.path)


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
        self._copied: dict[int, int] = {}
        self._mutex = asyncio.Lock()

    async def close(self) -> None:
        """Finishes the running mutation, then releases the writer lock. Safe to repeat."""
        async with self._mutex:
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

    def _delta_ino(self, node: _Node) -> int | None:
        """The delta inode of `node`, which may have been copied up after `node` was resolved."""
        return node.delta if node.delta is not None else self._copied.get(node.ino)

    async def _pread(self, node: _Node, offset: int, size: int) -> bytes:
        if (delta := self._delta_ino(node)) is None:
            return await self._lower.read(node.path, offset, size)
        await self._stats(node.ino, delta, node.path)
        return (await self._delta._read_inode(delta))[offset : offset + size]

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

    async def _walk(self, path: str, follow_last: bool, syscall: FsSyscall):
        """Resolves `path` to `(parent, name, node, dir_only, via_link)`.

        `parent` and `name` locate the dentry of `node`, which is None when only the last
        component is missing; `name` is empty after a final `.` or `..`. `dir_only` is set when
        the path ends in `/`, `.` or `..`. `via_link` is set when a walk that does not follow
        the last component crossed a final symlink only because of that suffix.
        """
        stack = [self._root]
        parts = path.split("/")[::-1]
        parent, name, node = self._root, "", self._root
        dir_only = via_link = False
        links = 0
        while parts:
            part = parts.pop()
            if part in ("", ".", ".."):
                if part == ".." and len(stack) > 1:
                    stack.pop()
                node, dir_only = stack[-1], True
                name = "" if part else name
                continue
            parent, name, dir_only = stack[-1], part, False
            node = await self._child(parent, name)
            if node is None:
                if any(parts):
                    _fail("ENOENT", syscall, path)
                dir_only = bool(parts)
                break
            if node.stats.is_symbolic_link() and (parts or follow_last):
                links += 1
                if links > 40:
                    _fail("ELOOP", syscall, path)
                via_link = via_link or (not follow_last and all(p in ("", ".") for p in parts))
                target = await self._target(node)
                if target.startswith("/"):
                    del stack[1:]
                parts.extend(target.split("/")[::-1])
                node = stack[-1]
            elif parts:
                if not node.stats.is_directory():
                    _fail("ENOTDIR", syscall, path)
                stack.append(node)
        return parent, name, node, dir_only, via_link

    async def _find(self, path: str, follow_last: bool, syscall: FsSyscall) -> _Node:
        node = (await self._walk(path, follow_last, syscall))[2]
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

    async def _copy_up(self, node: _Node, content: bool = True) -> int:
        """Copies a lower-backed entry and its lower-backed ancestors, top-down, into the delta."""
        if (ino := self._delta_ino(node)) is not None:
            return ino
        low, path = node.lower, node.path
        kind = low.mode & S_IFMT
        if kind not in _TYPES:
            _fail("ENOTSUP", "open", path)
        parent_path, name = path.rsplit("/", 1)
        parent = await self._copy_up(await self._find(parent_path or "/", True, "open"))
        ino = await self._delta._create_inode(low.mode, low.uid, low.gid)
        if kind == S_IFLNK:
            await self._write_link(ino, await self._lower.readlink(path))
        elif kind == S_IFREG and content and low.size:
            await self._delta._update_file_content(ino, await self._lower.read(path, 0, low.size))
        db, times = self._db, (low.atime, low.mtime, low.ctime)
        await _run(
            db, "UPDATE fs_inode SET atime = ?, mtime = ?, ctime = ? WHERE ino = ?", *times, ino
        )
        await _run(db, "INSERT INTO fs_origin (delta_ino, lower_path) VALUES (?, ?)", ino, path)
        await self._delta._create_dentry(parent, name, ino)
        self._by_delta[ino] = node.ino
        self._copied[node.ino] = ino
        del self._by_lower[path]
        return ino

    async def _hide(self, parent: _Node, name: str) -> None:
        """Inserts a whiteout if the base resolves the entry and no whiteout covers it."""
        path = _join(parent.path, name)
        if _is_dir(parent.lower) and path not in self._whiteouts and await self._lower.lstat(path):
            await _run(self._db, "INSERT INTO fs_whiteout VALUES (?, ?)", path, int(time.time()))
            self._whiteouts.add(path)

    async def _write_link(self, ino: int, target: str) -> None:
        db = self._db
        await _run(db, "INSERT INTO fs_symlink (ino, target) VALUES (?, ?)", ino, target)
        await _run(db, "UPDATE fs_inode SET size = ? WHERE ino = ?", len(target.encode()), ino)

    async def _add(self, parent: _Node, name: str, mode: int) -> int:
        parent_ino = await self._copy_up(parent)
        ino = await self._delta._create_inode(mode)
        await self._delta._create_dentry(parent_ino, name, ino)
        return ino

    async def _locate(self, path: str, syscall: FsSyscall) -> tuple[_Node, str, _Node]:
        """Resolves an existing entry that is not the root, without following a final symlink."""
        parent, name, node, _, via_link = await self._walk(path, False, syscall)
        if via_link:
            _fail("ENOTDIR", syscall, path)
        node = node or _fail("ENOENT", syscall, path)
        if node.path == "/":
            _fail("EPERM", syscall, path)
        if not name:
            _fail("EINVAL", syscall, path)
        return parent, name, node

    async def _remove(self, parent: _Node, name: str, node: _Node) -> None:
        if (ino := self._delta_ino(node)) is not None:
            await self._delta._remove_dentry_and_maybe_inode(self._delta_ino(parent), name, ino)
        await self._hide(parent, name)

    async def _subtree(self, parent: _Node, name: str, node: _Node) -> list[tuple]:
        """The merged entries at and under `node` as (parent, name, node), parents first."""
        tree = [(parent, name, node)]
        for _, _, n in tree:
            if n.stats.is_directory():
                tree += [(n, c, await self._child(n, c)) for c in await self._names(n)]
        return tree

    async def _splice(self, node: _Node, offset: int, data: bytes, keep_tail: bool) -> None:
        """Rewrites a handle's content: old bytes before `offset`, `data`, then the old tail."""
        if offset < 0:
            _fail("EINVAL", "open")
        async with self._mutex:
            ino = self._delta_ino(node)
            if ino is None:
                try:
                    found = (await self._walk(node.path, False, "open"))[2]
                except ErrnoException as e:
                    if e.code not in ("ENOTDIR", "ELOOP"):
                        raise
                    found = None
                if found is None or found.ino != node.ino:
                    _fail("ENOENT", "open", node.path)
                ino = await self._copy_up(found, keep_tail or offset > 0)
            await self._stats(node.ino, ino, node.path)
            if keep_tail and not data:
                return
            old = await self._delta._read_inode(ino)
            tail = old[offset + len(data) :] if keep_tail else b""
            head = old[:offset].ljust(offset, b"\0")
            await self._delta._update_file_content(ino, head + data + tail)

    async def _write_file(self, path: str, content: str | bytes, encoding: str = "utf-8") -> None:
        data = content.encode(encoding) if isinstance(content, str) else content
        names = [n for n in path.split("/") if n][:-1]
        for i in range(1, len(names) + 1):
            directory = "/" + "/".join(names[:i])
            if (await self._walk(directory, True, "open"))[2] is None:
                await self._mkdir(directory)
        parent, name, node, dir_only, _ = await self._walk(path, True, "open")
        if dir_only or _is_dir(node and node.stats):
            _fail("EISDIR", "open", path)
        if node is None and (await self._walk(path, False, "open"))[2]:
            _fail("ENOENT", "open", path)
        ino = await (
            self._copy_up(node, False) if node else self._add(parent, name, DEFAULT_FILE_MODE)
        )
        await self._delta._update_file_content(ino, data)

    async def _mkdir(self, path: str) -> None:
        parent, name, node, _, via_link = await self._walk(path, False, "mkdir")
        if node or via_link:
            _fail("EEXIST", "mkdir", path)
        await self._add(parent, name, DEFAULT_DIR_MODE)

    async def _chmod(self, path: str, mode: int) -> None:
        ino = await self._copy_up(await self._find(path, True, "chmod"))
        sql = "UPDATE fs_inode SET mode = (mode & ?) | ?, ctime = ? WHERE ino = ?"
        await _run(self._db, sql, S_IFMT, mode & 0o7777, int(time.time()), ino)
        if ino == 1:
            self._root = replace(self._root, stats=await self._stats(1, 1, "/"))

    write_file = _locked(_write_file)
    mkdir = _locked(_mkdir)
    chmod = _locked(_chmod)

    @_locked
    async def symlink(self, target: str, path: str) -> None:
        parent, name, node, dir_only, via_link = await self._walk(path, False, "symlink")
        if node or via_link:
            _fail("EEXIST", "symlink", path)
        if dir_only:
            _fail("ENOENT", "symlink", path)
        await self._write_link(await self._add(parent, name, S_IFLNK | 0o777), target)

    @_locked
    async def unlink(self, path: str) -> None:
        parent, name, node = await self._locate(path, "unlink")
        if node.stats.is_directory():
            _fail("EISDIR", "unlink", path)
        await self._remove(parent, name, node)

    @_locked
    async def rmdir(self, path: str) -> None:
        parent, name, node = await self._locate(path, "rmdir")
        if not node.stats.is_directory():
            _fail("ENOTDIR", "rmdir", path)
        if await self._names(node):
            _fail("ENOTEMPTY", "rmdir", path)
        await self._remove(parent, name, node)

    @_locked
    async def rm(self, path: str, force: bool = False, recursive: bool = False) -> None:
        try:
            parent, name, node = await self._locate(path, "rm")
        except ErrnoException as e:
            if force and e.code == "ENOENT":
                return
            raise
        if node.stats.is_directory() and not recursive:
            _fail("EISDIR", "rm", path)
        for entry in reversed(await self._subtree(parent, name, node)):
            await self._remove(*entry)

    @_locked
    async def link(self, src: str, dst: str) -> None:
        node = (await self._locate(src, "link"))[2]
        if node.stats.is_directory():
            _fail("EPERM", "link", src)
        if (ino := self._delta_ino(node)) is None:
            _fail("EXDEV", "link", src)
        parent, name, found, dir_only, via_link = await self._walk(dst, False, "link")
        if found or via_link:
            _fail("EEXIST", "link", dst)
        if dir_only:
            _fail("ENOENT", "link", dst)
        await self._delta._create_dentry(await self._copy_up(parent), name, ino)

    @_locked
    async def rename(self, old_path: str, new_path: str) -> None:
        old_parent, old_name, src = await self._locate(old_path, "rename")
        parent, name, dst, dir_only, via_link = await self._walk(new_path, False, "rename")
        if via_link:
            _fail("ENOTDIR", "rename", new_path)
        dst_path = dst.path if dst else _join(parent.path, name)
        src_delta, dst_delta = self._delta_ino(src), dst and self._delta_ino(dst)
        if src.path == dst_path or (src_delta is not None and src_delta == dst_delta):
            return
        is_dir = src.stats.is_directory()
        if (is_dir and dst_path.startswith(src.path + "/")) or not name:
            _fail("EINVAL", "rename", new_path)
        if dst:
            if dst.stats.is_directory() != is_dir:
                _fail("ENOTDIR" if is_dir else "EISDIR", "rename", new_path)
            if is_dir and await self._names(dst):
                _fail("ENOTEMPTY", "rename", new_path)
        elif dir_only and not is_dir:
            _fail("ENOTDIR", "rename", new_path)
        tree = [n for _, _, n in await self._subtree(old_parent, old_name, src)]
        for n in tree:
            if n.delta is None and (n.lower.mode & S_IFMT) not in _TYPES:
                _fail("ENOTSUP", "rename", n.path)
        for n in tree:
            await self._copy_up(n)
        parent_ino = await self._copy_up(parent)
        if dst_delta is not None:
            await self._delta._remove_dentry_and_maybe_inode(parent_ino, name, dst_delta)
        move = "UPDATE fs_dentry SET parent_ino = ?, name = ? WHERE parent_ino = ? AND name = ?"
        await _run(self._db, move, parent_ino, name, self._delta_ino(old_parent), old_name)
        await self._hide(old_parent, old_name)
        await self._hide(parent, name)

    @_locked
    async def copy_file(self, src: str, dest: str) -> None:
        await self._write_file(dest, await self.read_file(src, None))
        await self._chmod(dest, (await self.stat(src)).mode)

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
    _held[lock.key] = OverlayInfo(path, 1, config.get("base_path", lower.locator), identity)
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
        await lock.take(path, "create_overlay", create=True)
        await AgentFS.open_with(lock.db)
        await _init_v1(lock.db, lower, identity)
    finally:
        await lock.release()
    return OverlayInfo(path, 1, lower.locator, identity)


async def open_merged(db_path: DbPath, base: Base | None = None) -> OverlayFS:
    """Opens the overlay in `db_path` and holds its writer lock until `close()`."""
    path = os.fspath(db_path)
    lock = _Lock()
    try:
        await lock.take(path, "open_merged")
        return await _mount(lock, path, base)
    except BaseException:
        await lock.release()
        raise


async def _config(db: Connection) -> dict[str, str] | None:
    """The `fs_overlay_config` rows, or None without that table."""
    if await _q(db, "SELECT 1 FROM sqlite_master WHERE name = 'fs_overlay_config'"):
        return dict(await _q(db, "SELECT key, value FROM fs_overlay_config"))


async def list_overlays(dir: DbPath, base: Base | None = None) -> list[OverlayInfo]:
    """The overlay databases directly in `dir`, sorted by path; with `base`, only those over it.

    A file that cannot be read, or that another process holds open, is left out.
    """
    lower = _lower(base) if base is not None else None
    identity = await lower.identity() if lower else None
    found = []
    for name in sorted(n for n in os.listdir(dir) if n.endswith(".db")):
        path = os.path.join(dir, name)
        lock = _Lock()
        try:
            st = os.lstat(path)
            if not stat.S_ISREG(st.st_mode):
                continue
            if (st.st_dev, st.st_ino) in _held:
                held = _held[st.st_dev, st.st_ino]
                info = held and replace(held, path=path)
            else:
                await lock.take(path, "list_overlays")
                config = await _config(lock.db)
                info = OverlayInfo(
                    path,
                    int(config.get("version", 0)),
                    config["base_path"],
                    config.get("base_identity", ""),
                )
        except Exception:
            continue
        finally:
            await lock.release()
        legacy = info and not info.base_identity and lower and info.base_path == lower.locator
        if info and (lower is None or info.base_identity == identity or legacy):
            found.append(info)
    return found


async def delete_overlay(db_path: DbPath) -> None:
    """Deletes the overlay database and its sidecars, holding its lock until the last is gone."""
    path = os.fspath(db_path)
    lock = _Lock()
    try:
        await lock.take(path, "delete_overlay")
        if await _config(lock.db) is None:
            _fail("EINVAL", "delete_overlay", path)
        for suffix in (*_SIDECARS, ""):
            with contextlib.suppress(FileNotFoundError):
                os.remove(path + suffix)
    finally:
        await lock.release()


async def rebind_overlay(db_path: DbPath, base: Base) -> None:
    """Records `base` as the base of the v1 overlay in `db_path`."""
    path, lower = os.fspath(db_path), _lower(base)
    rows = [("base_path", lower.locator), ("base_identity", await lower.identity())]
    lock = _Lock()
    try:
        await lock.take(path, "rebind_overlay")
        version = (await _config(lock.db) or {}).get("version")
        if version is None:
            _fail("EINVAL", "rebind_overlay", path)
        if version != "1":
            _fail("ENOTSUP", "rebind_overlay", path)
        sql = "INSERT OR REPLACE INTO fs_overlay_config (key, value) VALUES (?, ?)"
        await lock.db.executemany(sql, rows)
        await lock.db.commit()
    finally:
        await lock.release()
