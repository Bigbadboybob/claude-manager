"""This MCP backend's version and activation state (doc/mcp-managed-activation.md).

The backend, not the proxy, owns policy: it knows which payload it loaded,
notices a newer stamped payload on disk, and writes the per-session
`~/.cm/mcp/<uid>/status.json` that `ping().mcp` and `scripts/cm-mcp-versions`
read. FastMCP-free so it stays unit-testable.
"""

from __future__ import annotations

import json
import os
import re
import tempfile
import time
from pathlib import Path

from mcp_server import mcp_version

HERE = Path(__file__).resolve().parent
STARTED_AT = time.time()
LOADED_HASH = mcp_version.payload_hash(HERE)
LOADED_TOOLS = mcp_version.snapshot_hash(HERE)
_loaded_stamp = mcp_version.read_stamp(HERE)
# The stamp describes what was loaded only if it matches the loaded files.
LOADED_VERSION = (_loaded_stamp["version"]
                  if _loaded_stamp and _loaded_stamp.get("payload_hash") == LOADED_HASH
                  else LOADED_HASH)

_disk = {"mtime": None, "stamp": None, "consistent": False}


def _uid() -> str | None:
    uid = os.environ.get("CM_TUI_SESSION_ID", "")
    return uid if re.fullmatch(r"[A-Za-z0-9_-]{1,160}", uid) else None


def status_path() -> Path | None:
    uid = _uid()
    return Path.home() / ".cm" / "mcp" / uid / "status.json" if uid else None


def proxy() -> dict | None:
    if os.environ.get("CM_MCP_PROXY_CHILD") != "1":
        return None
    return {"version": os.environ.get("CM_MCP_PROXY_VERSION"),
            "generation": int(os.environ.get("CM_MCP_PROXY_GENERATION") or 0)}


def disk_stamp(root: Path = HERE) -> tuple[dict | None, bool]:
    """(stamp on disk, whether it matches the files on disk). Rehashes only
    when the stamp file changes, so polling it is a stat."""
    try:
        mtime = (root / mcp_version.STAMP).stat().st_mtime_ns
    except OSError:
        _disk.update(mtime=None, stamp=None, consistent=False)
        return None, False
    if mtime != _disk["mtime"]:
        stamp = mcp_version.read_stamp(root)
        consistent = bool(stamp) and stamp["payload_hash"] == mcp_version.payload_hash(root)
        _disk.update(mtime=mtime, stamp=stamp, consistent=consistent)
    return _disk["stamp"], _disk["consistent"]


def pending(root: Path = HERE) -> dict | None:
    """A newer, consistently stamped payload than the one loaded."""
    stamp, consistent = disk_stamp(root)
    if not stamp or stamp["payload_hash"] == LOADED_HASH:
        return None
    if not consistent:
        return None  # files changed after stamping, or a copy is in progress
    return {"version": stamp["version"], "payload_hash": stamp["payload_hash"],
            "tools_hash": stamp.get("tools_hash"),
            "tools_change": stamp.get("tools_hash") != LOADED_TOOLS}


def _previous() -> dict:
    path = status_path()
    try:
        return json.loads(path.read_text()) if path else {}
    except (OSError, ValueError):
        return {}


def _session_tools_hash(previous: dict) -> str | None:
    """The tool set the engine saw first. A Codex thread keeps that list."""
    gen = (proxy() or {}).get("generation", 0)
    if gen > 1 and previous.get("session_tools_hash"):
        return previous["session_tools_hash"]
    return LOADED_TOOLS


_session_tools = None


def status(extra: dict | None = None) -> dict:
    global _session_tools
    if _session_tools is None:
        _session_tools = _session_tools_hash(_previous())
    engine = os.environ.get("CM_AGENT_ENGINE")
    value = {
        "version": LOADED_VERSION,
        "payload_hash": LOADED_HASH,
        "tools_hash": LOADED_TOOLS,
        "session_tools_hash": _session_tools,
        "engine": engine,
        "proxy": proxy(),
        "pid": os.getpid(),
        "started_at": STARTED_AT,
        "pending": pending(),
        "updated_at": time.time(),
    }
    # Codex threads keep their first tool list: say so instead of implying
    # the new tools are usable.
    value["tools_changed_unseen"] = bool(
        engine == "codex" and _session_tools and LOADED_TOOLS != _session_tools)
    value.update(extra or {})
    return value


def write_status(extra: dict | None = None) -> dict:
    value = status(extra)
    path = status_path()
    if path:
        try:
            path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=".status.")
            with os.fdopen(fd, "w") as stream:
                json.dump(value, stream)
            os.replace(tmp, path)
        except OSError:
            pass  # advisory; never fail a tool call over it
    return value


def brief() -> dict:
    """The `mcp` field in ping: what an agent needs, not the bookkeeping."""
    value = status()
    out = {"version": value["version"], "proxy": bool(value["proxy"])}
    if value["pending"]:
        out["update_pending"] = value["pending"]["version"]
    if value["tools_changed_unseen"]:
        out["note"] = "New CM tools appear in this session after a restart (A-R)."
    return out
