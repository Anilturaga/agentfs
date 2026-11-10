"""Overlay read path tests"""

import asyncio
import errno
import fcntl
import gc
import json
import os
import shutil
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest
from turso.aio import connect

import agentfs_sdk.overlay as overlay_module
from agentfs_sdk import (
    AgentFS,
    AgentFSOptions,
    ErrnoException,
    Filesystem,
    HostAdapter,
    create_overlay,
    delete_overlay,
    list_overlays,
    open_merged,
    rebind_overlay,
)

REPO = Path(__file__).resolve().parents[3]
BASE_SCRIPT = json.loads((REPO / "conformance" / "overlay" / "cases.json").read_text())["base"]
LEGACY_RUST_DB = REPO / "sdk" / "rust" / "tests" / "fixtures" / "legacy_rust_overlay.db"
S_IFLNK = 0o120000


@pytest.fixture
def base(tmp_path):
    path = tmp_path / "base"
    path.mkdir()
    subprocess.run(["sh", "-ec", BASE_SCRIPT], cwd=path, check=True)
    return path


@pytest.fixture
async def overlay(tmp_path, base):
    await create_overlay(tmp_path / "o.db", base)
    return tmp_path / "o.db"


async def raw(db, work):
    """Runs `work(conn)` on a short-lived connection and releases the file lock."""
    conn = await connect(str(db))
    try:
        result = await work(conn)
        await conn.commit()
        return result
    finally:
        await conn.close()
        del conn
        gc.collect()


async def rows(db, sql):
    async def work(conn):
        return await (await conn.execute(sql)).fetchall()

    return await raw(db, work)


def child(code: str, *args: str, **kwargs) -> subprocess.Popen:
    prelude = "import asyncio, sys\nfrom agentfs_sdk import open_merged\n"
    return subprocess.Popen(
        [sys.executable, "-c", prelude + textwrap.dedent(code), *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        **kwargs,
    )


OPEN_AND_CLOSE = """
    async def main():
        await (await open_merged(sys.argv[1], sys.argv[2])).close()
    asyncio.run(main())
"""

HOLD = """
    async def main():
        fs = await open_merged(sys.argv[1], sys.argv[2])
        print("ready", flush=True)
        sys.stdin.readline()
        await fs.close()
    asyncio.run(main())
"""


async def code(coro) -> str:
    with pytest.raises(ErrnoException) as excinfo:
        await coro
    return excinfo.value.code


OPEN_CODE = """
    async def main():
        try:
            await open_merged(sys.argv[1], sys.argv[2])
        except OSError as e:
            print(e.code)
    asyncio.run(main())
"""


DELETE = """
    from agentfs_sdk import delete_overlay
    async def main():
        try:
            await delete_overlay(sys.argv[1])
        except OSError as e:
            print(e.code)
    asyncio.run(main())
"""


def child_delete_code(db) -> str:
    """The error code of `delete_overlay` in another process; empty if it deleted."""
    return child(DELETE, str(db)).communicate(timeout=30)[0].strip()


def child_open_code(db, base) -> str:
    """The error code of `open_merged` in another process; empty if it opened."""
    return child(OPEN_CODE, str(db), str(base)).communicate(timeout=30)[0].strip()


def child_opens(db, base) -> bool:
    proc = child(OPEN_AND_CLOSE, str(db), str(base))
    return proc.wait(timeout=30) == 0


async def test_errno_exception_is_oserror(overlay, base):
    async with await open_merged(overlay, base) as fs:
        with pytest.raises(OSError) as excinfo:
            await fs.read_file("/missing")
    assert isinstance(excinfo.value, ErrnoException)
    assert excinfo.value.errno == errno.ENOENT
    assert excinfo.value.code == "ENOENT"


async def test_reads_never_write(overlay, base):
    tables = "fs_inode", "fs_dentry", "fs_whiteout", "fs_origin"
    count = " UNION ALL ".join(f"SELECT count(*) FROM {t}" for t in tables)
    before = await rows(overlay, count)
    async with await open_merged(overlay, base) as fs:
        assert (await fs.stat("/d/../f")).ino == (await fs.stat("/f")).ino
        assert await fs.readdir("/") == ["d", "e", "f", "l", "p"]
        assert [e.name for e in await fs.readdir_plus("/d")] == ["sub", "x"]
        assert await fs.read_file("/l") == "x"
        assert await fs.readlink("/l") == "d/x"
        assert (await fs.lstat("/l")).is_symbolic_link()
        await fs.access("/d/sub/y")
        handle = await fs.open("/f")
        assert await handle.pread(1, 2) == b"as"
        assert (await handle.fstat()).size == 4
        assert await fs.diff() == []
        with pytest.raises(ErrnoException) as excinfo:
            await fs.open("/p")
        assert excinfo.value.code == "ENOTSUP"
    assert await rows(overlay, count) == before


async def test_delta_entries_and_whiteouts(overlay, base):
    async def seed(conn):
        fs = await Filesystem.from_database(conn)
        await fs.write_file("/d/new", "delta")
        await fs.write_file("/f", "over")
        await conn.execute("INSERT INTO fs_whiteout VALUES ('/d/x', 0), ('/e', 0)")
        await conn.execute(
            "INSERT INTO fs_inode (ino, mode, nlink, atime, mtime, ctime) "
            f"VALUES (99, {S_IFLNK}, 1, 0, 0, 0)"
        )
        await conn.execute("INSERT INTO fs_symlink VALUES (99, 'd/new')")
        await conn.execute("INSERT INTO fs_dentry (name, parent_ino, ino) VALUES ('s', 1, 99)")

    await raw(overlay, seed)
    async with await open_merged(overlay, base) as fs:
        assert await fs.readdir("/") == ["d", "f", "l", "p", "s"]
        assert await fs.readdir("/d") == ["new", "sub"]
        assert await fs.read_file("/f") == "over"
        assert await fs.read_file("/s") == "delta"
        assert await fs.read_file("/d/new") == "delta"
        for gone in ("/e", "/d/x", "/l"):
            with pytest.raises(ErrnoException) as excinfo:
                await fs.stat(gone)
            assert excinfo.value.code == "ENOENT"
        inos = {e.name: e.stats.ino for e in await fs.readdir_plus("/d")}
        assert inos == {"new": (await fs.stat("/d/new")).ino, "sub": (await fs.stat("/d/sub")).ino}
        assert len({(await fs.stat(p)).ino for p in ("/", "/d", "/d/new", "/d/sub", "/f")}) == 5
        assert [str(c) for c in await fs.diff()] == [
            "M d /d",
            "A f /d/new",
            "D f /d/x",
            "D d /e",
            "M f /f",
            "A l /s",
        ]


class CountingAdapter:
    """Wraps a HostAdapter and counts the names that `readdir` returns."""

    def __init__(self, inner: HostAdapter) -> None:
        self.inner = inner
        self.locator = inner.locator
        self.names = 0

    async def readdir(self, path):
        names = await self.inner.readdir(path)
        self.names += len(names)
        return names

    def __getattr__(self, name):
        return getattr(self.inner, name)


async def test_diff_does_not_walk_base(tmp_path):
    big = tmp_path / "base" / "big"
    big.mkdir(parents=True)
    for i in range(10_000):
        (big / f"f{i}").write_text("x")
    adapter = CountingAdapter(HostAdapter(tmp_path / "base"))
    await create_overlay(tmp_path / "o.db", adapter)
    async with await open_merged(tmp_path / "o.db", adapter) as fs:
        assert await fs.diff() == []
        await fs.write_file("/big/f1", "x")
        assert [str(c) for c in await fs.diff()] == ["M d /big", "M f /big/f1"]
    assert adapter.names < 100


async def test_base_symlink_never_followed(tmp_path):
    outside = tmp_path / "outside"
    outside.mkdir()
    (outside / "s").write_text("SECRET")
    base = tmp_path / "base"
    base.mkdir()
    (base / "esc").symlink_to(outside)
    (base / "k").mkdir()
    await create_overlay(tmp_path / "o.db", base)
    async with await open_merged(tmp_path / "o.db", base) as fs:
        assert await code(fs.read_file("/esc/s")) == "ENOENT"
        await fs.chmod("/k", 0o700)
    (base / "k").rmdir()
    (base / "k").symlink_to(outside)
    async with await open_merged(tmp_path / "o.db", base) as fs:
        assert await code(fs.read_file("/k/s")) == "ENOENT"


async def test_host_adapter_is_rooted(tmp_path):
    outside = tmp_path / "outside" / "f"
    outside.parent.mkdir()
    outside.write_text("OUTSIDE")
    base = tmp_path / "base"
    lower = base / str(outside).lstrip("/")
    lower.parent.mkdir(parents=True)
    lower.write_text("LOWER")
    parent = str(outside.parent)
    adapter = HostAdapter(base)
    assert (await adapter.lstat(str(outside))).size == len("LOWER")
    assert await adapter.read(str(outside), 0, 100) == b"LOWER"
    assert await adapter.readdir(parent) == ["f"]
    assert (await adapter.lstat(parent)).is_directory()
    await create_overlay(tmp_path / "o.db", base)
    async with await open_merged(tmp_path / "o.db", base) as fs:
        assert await fs.read_file(str(outside)) == "LOWER"


async def test_other_process_open_is_ebusy(overlay, base):
    holder = child(HOLD, str(overlay), str(base), stdin=subprocess.PIPE)
    try:
        assert holder.stdout.readline() == "ready\n"
        with pytest.raises(ErrnoException) as excinfo:
            await open_merged(overlay, base)
        assert excinfo.value.code == "EBUSY"
    finally:
        holder.communicate("\n", timeout=30)
    async with await open_merged(overlay, base):
        with pytest.raises(ErrnoException) as excinfo:
            await open_merged(overlay, base)
        assert excinfo.value.code == "EBUSY"


async def test_close_releases_lock(tmp_path, base, overlay):
    other = tmp_path / "other"
    other.mkdir()
    assert child_opens(overlay, base)
    fs = await open_merged(overlay, base)
    await fs.close()
    await fs.close()
    assert child_opens(overlay, base)
    assert await code(open_merged(overlay, other)) == "ESTALE"
    assert child_opens(overlay, base)


async def test_open_errors(tmp_path, base, overlay):
    other = tmp_path / "other"
    other.mkdir()

    assert await code(open_merged(tmp_path / "missing.db", base)) == "ENOENT"
    assert await code(create_overlay(tmp_path / "x.db", tmp_path / "missing")) == "ENOENT"
    assert await code(create_overlay(tmp_path / "y.db", base / "f")) == "ENOTDIR"
    assert await code(create_overlay(overlay, base)) == "EEXIST"
    await (await AgentFS.open(AgentFSOptions(path=str(tmp_path / "plain.db")))).close()
    gc.collect()
    assert await code(open_merged(tmp_path / "plain.db")) == "EINVAL"
    assert await code(open_merged(overlay, other)) == "ESTALE"
    assert not (tmp_path / "x.db").exists()


async def test_non_v1_databases_are_not_migrated(tmp_path, base, overlay):
    await raw(
        overlay,
        lambda c: c.execute("UPDATE fs_overlay_config SET value = '2' WHERE key = 'version'"),
    )
    assert await code(open_merged(overlay, base)) == "ENOTSUP"

    legacy = tmp_path / "legacy.db"
    shutil.copyfile(LEGACY_RUST_DB, legacy)
    with pytest.raises(ErrnoException) as excinfo:
        await open_merged(legacy, base)
    assert excinfo.value.code == "ENOTSUP"
    assert "upgrade to format v1" in str(excinfo.value)

    go = tmp_path / "go.db"
    await (await AgentFS.open(AgentFSOptions(path=str(go)))).close()
    gc.collect()
    ddl = "CREATE TABLE fs_whiteout (path TEXT PRIMARY KEY, parent_path TEXT, created_at INTEGER)"
    await raw(go, lambda c: c.execute(ddl))
    with pytest.raises(ErrnoException) as excinfo:
        await open_merged(go, base)
    assert excinfo.value.code == "ENOTSUP"
    tables = await rows(go, "SELECT name FROM sqlite_master WHERE name = 'fs_overlay_config'")
    assert tables == []


class DeniedAdapter:
    """Wraps a HostAdapter and refuses `lstat` of one path, like an unsearchable directory."""

    def __init__(self, inner: HostAdapter, denied: str) -> None:
        self.inner = inner
        self.locator = inner.locator
        self.denied = denied

    async def lstat(self, path):
        if path == self.denied:
            raise PermissionError(errno.EACCES, "denied", path)
        return await self.inner.lstat(path)

    def __getattr__(self, name):
        return getattr(self.inner, name)


async def test_delta_file_is_found_without_probing_the_base(overlay, base):
    async def seed(conn):
        await (await Filesystem.from_database(conn)).write_file("/d/x", "delta")

    await raw(overlay, seed)
    denied = DeniedAdapter(HostAdapter(base), "/d/x")
    async with await open_merged(overlay, denied) as fs:
        assert await fs.read_file("/d/x") == "delta"


async def test_diff_handles_deep_hidden_trees(tmp_path, overlay, base):
    path = base / "deep"
    for _ in range(1201):
        path.mkdir()
        path = path / "a"
    await raw(overlay, lambda c: c.execute("INSERT INTO fs_whiteout VALUES ('/deep', 0)"))
    async with await open_merged(overlay, base) as fs:
        changes = await fs.diff()
    assert len(changes) == 1201
    assert str(changes[0]) == "D d /deep"


async def test_host_adapter_reports_access_errors(tmp_path, monkeypatch):
    real_stat = os.stat

    def deny(path, *args, **kwargs):
        if str(path) == str(tmp_path.resolve()):
            raise PermissionError(errno.EACCES, "denied", str(path))
        return real_stat(path, *args, **kwargs)

    monkeypatch.setattr(os, "stat", deny)
    with pytest.raises(PermissionError):
        HostAdapter(tmp_path)


async def test_copy_up_keeps_identity_mode_times_and_origin(overlay, base):
    os.chmod(base / "f", 0o600)
    os.utime(base / "e", (1_000_000_000, 1_000_000_000))
    async with await open_merged(overlay, base) as fs:
        ino = (await fs.stat("/f")).ino
        handle = await fs.open("/f")
        await handle.pwrite(0, b"B")
        assert await handle.pread(0, 10) == b"Base"
        await handle.pwrite(6, b"!")
        assert await fs.read_file("/f", None) == b"Base\0\0!"
        await handle.truncate(2)
        assert await fs.read_file("/f", None) == b"Ba"
        after = await fs.stat("/f")
        assert (after.ino, after.mode & 0o7777) == (ino, 0o600)
        await fs.write_file("/e/n", "x")
        copied = await fs.stat("/e")
        assert (copied.mode & 0o7777, copied.mtime) == (0o755, 1_000_000_000)
        assert [str(c) for c in await fs.diff()] == ["M d /e", "A f /e/n", "M f /f"]
    assert (base / "f").read_text() == "base"
    origins = await rows(overlay, "SELECT lower_path FROM fs_origin ORDER BY lower_path")
    assert origins == [("/e",), ("/f",)]


async def test_handle_write_after_unlink_is_enoent(overlay, base):
    async with await open_merged(overlay, base) as fs:
        gone, replaced = await fs.open("/f"), await fs.open("/d/x")
        await fs.unlink("/f")
        assert await code(gone.pwrite(0, b"z")) == "ENOENT"
        await fs.unlink("/d/x")
        await fs.write_file("/d/x", "new")
        assert await code(replaced.truncate(0)) == "ENOENT"
        assert await code(gone.pwrite(-1, b"z")) == "EINVAL"
        assert await fs.read_file("/d/x") == "new"
    assert (base / "f").read_text() == "base"


async def test_parents_rm_and_copy_file(overlay, base):
    os.chmod(base / "f", 0o600)
    async with await open_merged(overlay, base) as fs:
        await fs.write_file("/n/a/b", "x")
        await fs.copy_file("/f", "/n/copy")
        assert await fs.read_file("/n/copy") == "base"
        assert (await fs.stat("/n/copy")).mode & 0o7777 == 0o600
        assert await code(fs.rm("/d")) == "EISDIR"
        await fs.rm("/d", recursive=True)
        await fs.rm("/nope", force=True)
        assert await code(fs.rm("/nope")) == "ENOENT"
        assert await fs.readdir("/") == ["e", "f", "l", "n", "p"]
        assert [str(c) for c in await fs.diff()] == [
            "D d /d",
            "D d /d/sub",
            "D f /d/sub/y",
            "D f /d/x",
            "A d /n",
            "A d /n/a",
            "A f /n/a/b",
            "A f /n/copy",
        ]


async def test_mutations_resolve_dentries_by_name(overlay, base):
    (base / "aliasd").symlink_to("d")
    await (await open_merged(overlay, base)).close()
    async with await open_merged(overlay, base) as fs:
        for attempt in (
            fs.rmdir("/aliasd/"),
            fs.rename("/aliasd/", "/moved"),
            fs.rm("/aliasd/", recursive=True),
        ):
            assert await code(attempt) == "ENOTDIR"
        assert await code(fs.mkdir("/aliasd/")) == "EEXIST"
        assert await code(fs.rm("/d/.", recursive=True)) == "EINVAL"
        assert await code(fs.rename("/f", "/zz/")) == "ENOTDIR"
        await fs.rename("/d", "/e/")
        assert await fs.readdir("/e") == ["sub", "x"]


async def test_concurrent_first_writes_are_serialized(overlay, base):
    async with await open_merged(overlay, base) as fs:
        a, b = await fs.open("/f"), await fs.open("/f")
        await asyncio.gather(a.pwrite(0, b"A"), b.pwrite(1, b"B"))
        assert await fs.read_file("/f") == "ABse"
        await asyncio.gather(fs.write_file("/d/a", "A"), fs.write_file("/d/b", "B"))
        assert await fs.readdir("/d") == ["a", "b", "sub", "x"]


async def test_rename_of_a_deep_base_tree_and_symlink_sizes(tmp_path, overlay, base):
    (base / "u").symlink_to("é")
    path = base / "deep"
    for _ in range(300):
        path.mkdir()
        path = path / "a"
    async with await open_merged(overlay, base) as fs:
        await fs.rename("/u", "/moved")
        await fs.symlink("é", "/made")
        assert [(await fs.lstat(p)).size for p in ("/moved", "/made")] == [2, 2]
        await fs.rename("/deep", "/deeper")
        await fs.rm("/deeper", recursive=True)
        assert await code(fs.stat("/deeper/a")) == "ENOENT"


async def test_root_chmod_is_visible_without_reopen(overlay, base):
    async with await open_merged(overlay, base) as fs:
        await fs.chmod("/", 0o701)
        assert (await fs.stat("/")).mode & 0o7777 == 0o701
        assert (await fs.stat("/d/..")).mode & 0o7777 == 0o701
    async with await open_merged(overlay, base) as fs:
        assert (await fs.stat("/")).mode & 0o7777 == 0o701


async def test_write_through_dangling_symlink_is_enoent(overlay, base):
    (base / "dangling").symlink_to("missing")
    async with await open_merged(overlay, base) as fs:
        await fs.symlink("nowhere", "/mine")
        for link in ("/dangling", "/mine"):
            assert await code(fs.write_file(link, "x")) == "ENOENT"
        await fs.write_file("/fresh", "x")
        assert await fs.readdir("/") == ["d", "dangling", "e", "f", "fresh", "l", "mine", "p"]


async def test_empty_pwrite_keeps_size_and_truncate_pads(overlay, base):
    async with await open_merged(overlay, base) as fs:
        await fs.write_file("/n", "abc")
        for path in ("/f", "/n"):
            handle = await fs.open(path)
            await handle.pwrite(10, b"")
            assert (await fs.stat(path)).size == (4 if path == "/f" else 3)
        await handle.truncate(5)
        assert await fs.read_file("/n", None) == b"abc\0\0"


async def test_handle_after_ancestor_replaced_by_file_is_enoent(overlay, base):
    async with await open_merged(overlay, base) as fs:
        handle = await fs.open("/d/x")
        await fs.rm("/d", recursive=True)
        await fs.write_file("/d", "file")
        assert await code(handle.pwrite(0, b"z")) == "ENOENT"
        assert await code(handle.truncate(0)) == "ENOENT"
        assert await fs.read_file("/d") == "file"


async def test_handle_on_unlinked_delta_file_is_enoent(overlay, base):
    async with await open_merged(overlay, base) as fs:
        await fs.write_file("/n", "abc")
        handle = await fs.open("/n")
        await fs.link("/n", "/m")
        await fs.unlink("/n")
        await handle.pwrite(0, b"X")
        assert await fs.read_file("/m") == "Xbc"
        await fs.unlink("/m")
        for attempt in (handle.pwrite(0, b"z"), handle.truncate(0), handle.pread(0, 1)):
            assert await code(attempt) == "ENOENT"


async def test_handle_after_ancestor_became_symlink_loop_is_enoent(overlay, base):
    async with await open_merged(overlay, base) as fs:
        handle = await fs.open("/d/x")
        await fs.rm("/d", recursive=True)
        await fs.symlink("d", "/d")
        assert await code(handle.pwrite(0, b"z")) == "ENOENT"
        assert await code(handle.truncate(0)) == "ENOENT"
        await fs.rm("/d")
        await fs.symlink("f", "/d")
        assert await code(handle.pwrite(0, b"z")) == "ENOENT"


SIDECARS = ("-wal", "-shm", "-log", "-wal-revert", "-info", "-changes")


async def plain_db(path):
    await (await AgentFS.open(AgentFSOptions(path=str(path)))).close()
    gc.collect()


async def test_management_errors(tmp_path, base, overlay):
    other = tmp_path / "other"
    other.mkdir()
    missing, plain, v2 = tmp_path / "missing.db", tmp_path / "plain.db", tmp_path / "v2.db"
    await plain_db(plain)
    await create_overlay(v2, base)
    await raw(
        v2, lambda c: c.execute("UPDATE fs_overlay_config SET value = '2' WHERE key = 'version'")
    )

    for db in (missing, plain, v2):
        want = {missing: "ENOENT", plain: "EINVAL", v2: "ENOTSUP"}[db]
        assert await code(rebind_overlay(db, base)) == want
    assert await code(delete_overlay(missing)) == "ENOENT"
    assert await code(delete_overlay(plain)) == "EINVAL"
    assert not missing.exists() and plain.exists()

    holder = child(HOLD, str(overlay), str(base), stdin=subprocess.PIPE)
    try:
        assert holder.stdout.readline() == "ready\n"
        assert await code(delete_overlay(overlay)) == "EBUSY"
        assert await code(rebind_overlay(overlay, other)) == "EBUSY"
    finally:
        holder.communicate("\n", timeout=30)
    assert await code(open_merged(overlay, other)) == "ESTALE"

    await delete_overlay(overlay)
    assert not overlay.exists()


async def test_hard_linked_database_is_refused(tmp_path, base, overlay):
    os.link(overlay, tmp_path / "alias")
    before = sorted(p.name for p in tmp_path.iterdir()), overlay.read_bytes()
    for attempt in (
        open_merged(overlay, base),
        delete_overlay(overlay),
        rebind_overlay(overlay, base),
    ):
        assert await code(attempt) == "ENOTSUP"
    assert await list_overlays(tmp_path) == []
    assert (sorted(p.name for p in tmp_path.iterdir()), overlay.read_bytes()) == before


async def test_list_overlays(tmp_path, base, overlay):
    other = tmp_path / "other"
    other.mkdir()
    shutil.copyfile(LEGACY_RUST_DB, tmp_path / "legacy.db")
    update = f"UPDATE fs_overlay_config SET value = '{base}' WHERE key = 'base_path'"
    await raw(tmp_path / "legacy.db", lambda c: c.execute(update))
    await plain_db(tmp_path / "plain.db")
    (tmp_path / "garbage.db").write_text("not a database")
    (tmp_path / "dir.db").mkdir()
    (tmp_path / "notes.txt").write_text("x")

    listed = await list_overlays(tmp_path)
    assert [(i.path, i.version) for i in listed] == [
        (str(tmp_path / "legacy.db"), 0),
        (str(overlay), 1),
    ]
    assert listed[0].base_identity == "" and listed[1].base_path == str(base.resolve())
    assert await list_overlays(tmp_path, base) == listed
    assert await list_overlays(tmp_path, other) == []


async def test_delete_holds_lock_until_removed(tmp_path, base, overlay, monkeypatch):
    checked, remove = [], os.remove

    def spy(path, *args, **kwargs):
        first = not checked
        if first or str(path) == str(overlay):
            checked.append(child_open_code(overlay, base))
        if first:
            for suffix in SIDECARS:
                Path(f"{overlay}{suffix}").touch()
        remove(path, *args, **kwargs)

    monkeypatch.setattr(os, "remove", spy)
    await delete_overlay(overlay)
    monkeypatch.undo()
    assert checked == ["EBUSY", "EBUSY"]
    assert [p.name for p in tmp_path.iterdir() if p.name.startswith("o.db")] == []
    assert child_open_code(overlay, base) == "ENOENT"


async def test_list_keeps_open_overlay_locked(tmp_path, base, overlay):
    async with await open_merged(overlay, base) as fs:
        assert [i.path for i in await list_overlays(tmp_path, base)] == [str(overlay)]
        assert child_open_code(overlay, base) == "EBUSY"
        await fs.write_file("/n", "x")
        assert await fs.read_file("/n") == "x"


async def test_management_releases_lock(tmp_path, base, overlay):
    other = tmp_path / "other"
    other.mkdir()
    await list_overlays(tmp_path)
    assert child_opens(overlay, base)
    await rebind_overlay(overlay, other)
    assert child_opens(overlay, other)
    assert await code(open_merged(overlay, base)) == "ESTALE"


async def test_listing_does_not_resurrect_deleted_database(tmp_path, base, overlay, monkeypatch):
    real_open, deleted = os.open, []

    def delete_before_open(path, *args, **kwargs):
        if str(path) == str(overlay) and not deleted:
            deleted.append(child_delete_code(overlay))
        return real_open(path, *args, **kwargs)

    monkeypatch.setattr(os, "open", delete_before_open)
    assert await list_overlays(tmp_path) == []
    monkeypatch.undo()
    assert deleted == [""]
    assert not overlay.exists()
    await create_overlay(overlay, base)


async def test_listing_pins_database_against_delete(tmp_path, base, overlay, monkeypatch):
    real_connect, deleted = overlay_module.connect, []

    def delete_before_connect(path):
        deleted.append(child_delete_code(overlay))
        return real_connect(path)

    monkeypatch.setattr(overlay_module, "connect", delete_before_connect)
    assert [i.path for i in await list_overlays(tmp_path)] == [str(overlay)]
    monkeypatch.undo()
    assert deleted == ["EBUSY"]
    assert overlay.exists()


async def test_listing_skips_entries_that_disappear(tmp_path, overlay, monkeypatch):
    gone, notes = tmp_path / "gone.db", tmp_path / "notes.txt"
    gone.write_text("x")
    notes.write_text("x")
    seen, real_lstat = [], os.lstat

    def vanish(path, *args, **kwargs):
        seen.append(Path(path).name)
        if Path(path) == gone:
            gone.unlink()
        return real_lstat(path, *args, **kwargs)

    monkeypatch.setattr(os, "lstat", vanish)
    assert [i.path for i in await list_overlays(tmp_path)] == [str(overlay)]
    monkeypatch.undo()
    assert "gone.db" in seen and "notes.txt" not in seen


async def test_failed_acquisition_keeps_the_lock_of_the_owner(tmp_path, base, overlay, monkeypatch):
    other = tmp_path / "other.db"
    await create_overlay(other, base)
    link = tmp_path / "link"
    link.symlink_to(other)
    real_open, real_connect, seen = os.open, overlay_module.connect, []

    def retarget(path, *args, **kwargs):
        if str(path) == str(link):
            link.unlink()
            link.symlink_to(overlay)
        return real_open(path, *args, **kwargs)

    async def acquire_meanwhile(path):
        if not seen:
            monkeypatch.setattr(os, "open", retarget)
            seen.append(await code(open_merged(link, base)))
            monkeypatch.setattr(os, "open", real_open)
            seen.append(child_delete_code(overlay))
        return await real_connect(path)

    monkeypatch.setattr(overlay_module, "connect", acquire_meanwhile)
    assert [i.path for i in await list_overlays(tmp_path)] == [str(overlay), str(other)]
    monkeypatch.undo()
    assert seen == ["ENOENT", "EBUSY"]
    assert overlay.exists()


@pytest.mark.parametrize("hook", ["open", "lockf"])
async def test_late_hard_link_is_refused(hook, tmp_path, base, overlay, monkeypatch):
    alias = tmp_path / "alias"
    owner = os if hook == "open" else fcntl
    real = getattr(owner, hook)

    def link_around(*args, **kwargs):
        if hook == "open" and str(args[0]) == str(overlay):
            os.link(overlay, alias)
        result = real(*args, **kwargs)
        if hook == "lockf":
            os.link(overlay, alias)
        return result

    for operation in (
        lambda: open_merged(overlay, base),
        lambda: delete_overlay(overlay),
        lambda: rebind_overlay(overlay, base),
    ):
        monkeypatch.setattr(owner, hook, link_around)
        assert await code(operation()) == "ENOTSUP"
        monkeypatch.undo()
        alias.unlink()
        assert overlay.exists()
    assert child_opens(overlay, base)
