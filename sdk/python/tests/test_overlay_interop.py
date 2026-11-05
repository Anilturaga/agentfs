"""An overlay database written by this SDK is read by the agentfs CLI, and the other way round."""

import json
import os
import subprocess

import pytest

from agentfs_sdk import open_merged

from .test_overlay import base, code, overlay, rows  # noqa: F401

BIN = os.environ.get("AGENTFS_BIN")
pytestmark = pytest.mark.skipif(not BIN, reason="AGENTFS_BIN is not set")


def agentfs(home, *args, **kwargs) -> subprocess.CompletedProcess:
    env = {**os.environ, "HOME": str(home)}
    return subprocess.run(
        [BIN, *map(str, args)], env=env, capture_output=True, text=True, timeout=60, **kwargs
    )


async def test_python_db_in_cli(tmp_path, base, overlay):  # noqa: F811
    async with await open_merged(overlay, base) as fs:
        await fs.unlink("/f")
        await fs.write_file("/d/x", "x2")
        await fs.rename("/d/sub", "/m")
        await fs.symlink("/d/x", "/a")
        changes = [str(c) for c in await fs.diff()]
    assert agentfs(tmp_path, "diff", overlay).stdout.splitlines() == changes
    assert agentfs(tmp_path, "fs", overlay, "cat", "/d/x").stdout == "x2"
    assert agentfs(tmp_path, "fs", overlay, "cat", "/m/y").stdout == "y"
    assert agentfs(tmp_path, "fs", overlay, "cat", "/f").returncode != 0


async def test_cli_db_in_python(tmp_path, base):  # noqa: F811
    db = tmp_path / ".agentfs" / "r.db"
    assert agentfs(tmp_path, "init", "r", "--base", base, cwd=tmp_path).returncode == 0
    assert agentfs(tmp_path, "fs", db, "write", "/d/x", "x2").returncode == 0
    remove = {"name": "remove", "arguments": {"path": "/f"}}
    call = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": remove}
    assert "error" not in json.loads(
        agentfs(tmp_path, "serve", "mcp", db, input=json.dumps(call)).stdout
    )

    async with await open_merged(db) as fs:
        assert await fs.read_file("/d/x") == "x2"
        assert await code(fs.stat("/f")) == "ENOENT"
        changes = [str(c) for c in await fs.diff()]
    assert await rows(db, "SELECT path FROM fs_whiteout") == [("/f",)]
    assert agentfs(tmp_path, "diff", db).stdout.splitlines() == changes
