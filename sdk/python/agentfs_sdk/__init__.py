"""AgentFS Python SDK

A filesystem and key-value store for AI agents, powered by SQLite.
"""

from .agentfs import AgentFS, AgentFSOptions
from .errors import ErrnoException, FsErrorCode, FsSyscall
from .filesystem import S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, Filesystem, Stats
from .kvstore import KvStore
from .overlay import (
    Base,
    Change,
    DbPath,
    DirEntry,
    HostAdapter,
    LowerAdapter,
    OverlayFile,
    OverlayFS,
    OverlayInfo,
    create_overlay,
    delete_overlay,
    list_overlays,
    open_merged,
    rebind_overlay,
)
from .toolcalls import ToolCall, ToolCalls, ToolCallStats

__version__ = "0.6.4"

__all__ = [
    "AgentFS",
    "AgentFSOptions",
    "KvStore",
    "Filesystem",
    "Stats",
    "S_IFDIR",
    "S_IFLNK",
    "S_IFMT",
    "S_IFREG",
    "ToolCalls",
    "ToolCall",
    "ToolCallStats",
    "ErrnoException",
    "FsErrorCode",
    "FsSyscall",
    "Base",
    "Change",
    "DbPath",
    "DirEntry",
    "HostAdapter",
    "LowerAdapter",
    "OverlayFile",
    "OverlayFS",
    "OverlayInfo",
    "create_overlay",
    "delete_overlay",
    "list_overlays",
    "open_merged",
    "rebind_overlay",
]
