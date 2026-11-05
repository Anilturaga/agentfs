"""Runs conformance/overlay/cases.json: one worker subprocess per case.

Run as a script (`python test_overlay_conformance.py <index>`), the file is the worker.
Worker exit codes: 0 every step matched, 1 a step result did not match, 2 harness error.
"""

import asyncio
import errno
import gc
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest
import turso.aio

import agentfs_sdk as sdk
from agentfs_sdk import ErrnoException

REPO = Path(__file__).resolve().parents[3]
CASES = REPO / "conformance" / "overlay" / "cases.json"
LEGACY_RUST_DB = REPO / "sdk" / "rust" / "tests" / "fixtures" / "legacy_rust_overlay.db"


def enosys() -> ErrnoException:
    return ErrnoException("ENOSYS", "open")


def field(stats, name: str = "type") -> str:
    if name == "type":
        if stats.is_file():
            return "f"
        if stats.is_directory():
            return "d"
        return "l" if stats.is_symbolic_link() else "?"
    return {"mode": f"{stats.mode & 0o7777:o}", "size": str(stats.size), "ino": str(stats.ino)}[
        name
    ]


class Worker:
    """The ops of conformance/overlay/README.md; an op that the SDK lacks answers ENOSYS."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.fs = None
        self.handle = None
        self.current = "db"

    def at(self, p: str) -> str:
        return str(self.root / p)

    def host(self, p: str) -> Path:
        return Path(str(self.root / "base") + p)

    def call(self, owner, name: str, *args):
        method = getattr(owner, name, None)
        if method is None:
            raise enosys()
        return method(*args)

    async def op_readdir(self, p):
        return " ".join(await self.fs.readdir(p))

    async def op_stat(self, p, f="type"):
        return field(await self.fs.stat(p), f)

    async def op_lstat(self, p, f="type"):
        return field(await self.fs.lstat(p), f)

    async def op_plus(self, directory, name, f="type"):
        for entry in await self.fs.readdir_plus(directory):
            if entry.name == name:
                return field(entry.stats, f)
        raise ErrnoException("ENOENT", "stat")

    async def op_read(self, p):
        return await self.fs.read_file(p)

    async def op_write(self, p, *s):
        await self.call(self.fs, "write_file", p, " ".join(s))

    async def op_readlink(self, p):
        return await self.fs.readlink(p)

    async def op_mkdir(self, p):
        await self.call(self.fs, "mkdir", p)

    async def op_rmdir(self, p):
        await self.call(self.fs, "rmdir", p)

    async def op_unlink(self, p):
        await self.call(self.fs, "unlink", p)

    async def op_chmod(self, p, mode):
        await self.call(self.fs, "chmod", p, int(mode, 8))

    async def op_symlink(self, target, p):
        await self.call(self.fs, "symlink", target, p)

    async def op_link(self, src, dst):
        await self.call(self.fs, "link", src, dst)

    async def op_rename(self, src, dst):
        await self.call(self.fs, "rename", src, dst)

    async def op_open(self, p):
        self.handle = await self.fs.open(p)

    async def op_hread(self):
        return (await self.handle.pread(0, (await self.handle.fstat()).size)).decode()

    async def op_diff(self):
        return "; ".join(map(str, await self.fs.diff()))

    async def op_openMerged(self, db, base=None):
        self.fs = await sdk.open_merged(self.at(db), base and self.at(base))
        self.current = db

    async def op_close(self):
        closing, self.fs, self.handle = self.fs, None, None
        await closing.close()

    async def op_reopen(self):
        await self.op_close()
        await self.op_openMerged(self.current)

    async def op_createOverlay(self, db, base):
        await sdk.create_overlay(self.at(db), self.at(base))

    async def op_deleteOverlay(self, db):
        await self.call(sdk, "delete_overlay", self.at(db))

    async def op_rebindOverlay(self, db, base):
        await self.call(sdk, "rebind_overlay", self.at(db), self.at(base))

    async def op_listOverlays(self, directory, base=None):
        found = await self.call(sdk, "list_overlays", self.at(directory), base and self.at(base))
        return " ".join(o.path.replace(f"{self.root}/", "") for o in found)

    async def op_plainDb(self, db):
        await (await sdk.AgentFS.open(sdk.AgentFSOptions(path=self.at(db)))).close()
        gc.collect()

    async def op_legacyRust(self, db, base):
        shutil.copyfile(LEGACY_RUST_DB, self.at(db))
        update = f"UPDATE fs_overlay_config SET value = '{self.at(base)}' WHERE key = 'base_path'"
        await self.op_sql(db, update)

    async def op_sql(self, db, *statement):
        conn = await turso.aio.connect(self.at(db))
        try:
            rows = await (await conn.execute(" ".join(statement))).fetchall()
            await conn.commit()
        finally:
            await conn.close()
            del conn
            gc.collect()
        return "; ".join("|".join("NULL" if v is None else str(v) for v in r) for r in rows)

    async def op_hostRead(self, p):
        return self.host(p).read_text()

    async def op_hostWrite(self, p, *s):
        self.host(p).write_text(" ".join(s))

    async def op_hostRm(self, p):
        self.host(p).unlink()

    async def op_hostMkdir(self, p):
        self.host(p).mkdir()

    async def step(self, text: str):
        """Returns the bound name, the result or error name, success, and the `=>` text."""
        command, arrow, want = text.partition("=>")
        bound = re.fullmatch(r"(\w+) = (.*)", command.strip())
        words = (bound.group(2) if bound else command).split()
        try:
            op = getattr(self, f"op_{words[0]}", None)
            if op is None:
                raise enosys()
            out, ok = await op(*words[1:]), True
        except OSError as e:
            out, ok = getattr(e, "code", None) or errno.errorcode[e.errno], False
        name = bound.group(1) if bound else None
        return name, "" if out is None else str(out), ok, (want.strip() if arrow else None)


async def run_case(index: int) -> int:
    doc = json.loads(CASES.read_text())
    root = Path(tempfile.mkdtemp(prefix="overlay-case-"))
    for name in ("base", "other", "dir"):
        (root / name).mkdir()
    worker = Worker(root)
    values: dict[str, str] = {}
    try:
        subprocess.run(["sh", "-ec", doc["base"]], cwd=root / "base", check=True)
        await worker.op_createOverlay("db", "base")
        await worker.op_openMerged("db", "base")
        for text in doc["cases"][index]["steps"]:
            text = re.sub(r"\$(\w+)", lambda m: values.get(m[1], m[0]), text)
            name, out, ok, want = await worker.step(text)
            if want is None:
                good = ok
            elif want.startswith("!"):
                good = ok and out != want[1:]
            else:
                good = out == want
            if not good:
                print(f"`{text}` gave `{out}`", file=sys.stderr)
                return 1
            if name:
                values[name] = out
        return 0
    except BaseException as e:
        print(repr(e), file=sys.stderr)
        return 2
    finally:
        shutil.rmtree(root, ignore_errors=True)


class CaseFailure(AssertionError):
    """A step result did not match."""


def run_group(cmd: list[str], seconds: int) -> tuple[int, str]:
    """Runs `cmd` in its own session and kills the process group as soon as it exits."""
    with tempfile.TemporaryFile("w+") as log:
        proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=log, start_new_session=True)
        try:
            returncode = proc.wait(timeout=seconds)
        except subprocess.TimeoutExpired:
            raise RuntimeError(f"timed out after {seconds}s") from None
        finally:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait()
        log.seek(0)
        return returncode, log.read()


def param(index: int, case: dict):
    marks = []
    if "xfail_py" in case:
        reason = f"issues {case['xfail_py']}"
        marks.append(pytest.mark.xfail(strict=True, raises=CaseFailure, reason=reason))
    return pytest.param(index, case.get("timeout", 30), marks=marks, id=f"case-{case['id']}")


@pytest.mark.parametrize(
    "index,seconds", [param(i, c) for i, c in enumerate(json.loads(CASES.read_text())["cases"])]
)
def test_case(index: int, seconds: int) -> None:
    code, stderr = run_group([sys.executable, __file__, str(index)], seconds)
    if code == 1:
        raise CaseFailure(stderr)
    if code != 0:
        raise RuntimeError(f"runner error (exit {code}): {stderr}")


async def test_worker_names_os_errors_and_raises_harness_errors(tmp_path):
    worker = Worker(tmp_path)
    (tmp_path / "base").mkdir()
    assert await worker.step("hostRead /missing => ENOENT") == (None, "ENOENT", False, "ENOENT")
    with pytest.raises(TypeError):
        await worker.step("hostRead")


def test_run_group_kills_descendants_that_hold_stderr_open():
    start = time.monotonic()
    assert run_group(["sh", "-c", "sleep 30 & exit 0"], 10) == (0, "")
    assert time.monotonic() - start < 5
    with pytest.raises(RuntimeError, match="timed out"):
        run_group(["sleep", "30"], 1)


if __name__ == "__main__":
    status = asyncio.run(run_case(int(sys.argv[1])))
    sys.stdout.flush()
    os._exit(status)
