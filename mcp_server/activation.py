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
_loaded: dict | None = None


def loaded() -> dict:
    """The payload this process runs: {payload_hash, tools_hash, toolset_hash,
    version}. Computed once (a pruned walk of ~25 files, a few ms)."""
    global _loaded
    if _loaded is None:
        digest = mcp_version.payload_hash(HERE)
        stamp = mcp_version.read_stamp(HERE)
        _loaded = {
            "payload_hash": digest,
            "tools_hash": mcp_version.snapshot_hash(HERE),
            "toolset_hash": mcp_version.toolset_hash(HERE),
            # The stamp names what was loaded only if it matches the files.
            "version": (stamp["version"] if stamp and stamp.get("payload_hash") == digest
                        else digest),
        }
    return _loaded


# Behind the proxy the loaded version must be pinned before a deploy can
# change the files, so hash at import there; elsewhere only when asked.
if os.environ.get("CM_MCP_PROXY_CHILD") == "1":
    loaded()

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
    if not stamp or stamp["payload_hash"] == loaded()["payload_hash"]:
        return None
    if not consistent:
        return None  # files changed after stamping, or a copy is in progress
    # Only a change Codex threads can notice (tools added/removed, required
    # parameters) counts; an additive optional parameter does not.
    toolset = stamp.get("toolset_hash")
    return {"version": stamp["version"], "payload_hash": stamp["payload_hash"],
            "toolset_hash": toolset,
            "tools_change": bool(toolset) and toolset != loaded()["toolset_hash"]}


def _previous() -> dict:
    path = status_path()
    try:
        return json.loads(path.read_text()) if path else {}
    except (OSError, ValueError):
        return {}


def _session_toolset(previous: dict) -> str | None:
    """The tool set the engine saw first. A Codex thread keeps that list."""
    gen = (proxy() or {}).get("generation", 0)
    if gen > 1 and previous.get("session_toolset_hash"):
        return previous["session_toolset_hash"]
    return loaded()["toolset_hash"]


_session_tools = None


def status(extra: dict | None = None) -> dict:
    global _session_tools
    if _session_tools is None:
        _session_tools = _session_toolset(_previous())
    engine = os.environ.get("CM_AGENT_ENGINE")
    mine = loaded()
    value = {
        "version": mine["version"],
        "payload_hash": mine["payload_hash"],
        "tools_hash": mine["tools_hash"],
        "toolset_hash": mine["toolset_hash"],
        "session_toolset_hash": _session_tools,
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
        engine == "codex" and _session_tools and mine["toolset_hash"] != _session_tools)
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


# ---- activation policy -----------------------------------------------------
# EP's rule: activate only at an agent-approved boundary, i.e. an explicit
# ack_reconnect() or idle for IDLE_S with nothing unread, no wake pending and
# no tool call in flight (the proxy checks the last part itself).

IDLE_S = float(os.environ.get("CM_MCP_ACTIVATION_IDLE_S", 60))
POLL_S = float(os.environ.get("CM_MCP_ACTIVATION_POLL_S", 10))
NOTICE_SOURCE = "mcp_update"


def notice_text(version: str, monitors: int, codex_tools_unseen: bool, can_ack: bool) -> str:
    """One line. Idle activation is the default; acking is only a shortcut."""
    watched = (f"your {monitors} monitor{'s' if monitors != 1 else ''} and chat watches carry over"
               if monitors else "your chat watches carry over")
    text = (f"[cm-mcp-update {version}] A CM MCP update ({version}) is pending. It activates "
            f"on its own after you have been idle for a minute"
            + (" (ack_reconnect() applies it sooner)" if can_ack else "")
            + f"; {watched} and nothing is re-sent.")
    if codex_tools_unseen:
        text += " New tools appear after a restart."
    return text


async def blocked_reason(uid: str, call, queue) -> str | None:
    """Why this session is not at an idle boundary right now (None = it is)."""
    import asyncio
    try:
        resolved = await asyncio.to_thread(
            call, "resolve_authorized_session", {"session_uid": uid}, timeout=5)
    except Exception:  # noqa: BLE001 - no evidence of idleness is not idle
        return "daemon_unreachable"
    if not isinstance(resolved, dict):
        return "state_unknown"
    if resolved.get("exited"):
        return "exited"
    agent = resolved.get("agent_state")
    if not isinstance(agent, dict):
        return "state_unknown"
    if agent.get("state") != "idle":
        return str(agent.get("state") or "state_unknown")
    since = agent.get("since")
    if not isinstance(since, (int, float)) or time.time() - since < IDLE_S:
        return "idle_too_recently"
    try:
        inbox = await asyncio.to_thread(
            call, "messaging.read",
            {"inbox": True, "unread_only": True, "limit": 1, "view": "slim"}, timeout=10)
        if inbox.get("items"):
            return "unread_inbox"
    except Exception:  # noqa: BLE001
        return "inbox_unknown"
    snapshot = await asyncio.to_thread(queue.snapshot)
    # Wakes held for a rotation/handoff wait for the new process anyway; they
    # must not stall an MCP activation (a capped hold, but minutes long).
    held = bool(snapshot.get("hold"))
    if any((e["status"] == "submitting" or (e["status"] == "pending" and not held))
           and e.get("source") != NOTICE_SOURCE
           for e in snapshot["notifications"]):
        return "notice_pending"
    return None


class Policy:
    """Polls for a pending update; tells a working agent once, then asks the
    proxy to activate at the first idle boundary."""

    def __init__(self, send_activate, call, queue_factory, monitors, can_ack=lambda: True):
        self.send_activate = send_activate
        self.call = call
        self.queue_factory = queue_factory
        self.monitors = monitors
        self.can_ack = can_ack
        self.noticed: set[str] = set()
        self.requested_at = 0.0

    async def tick(self) -> dict:
        update = pending()
        uid = _uid()
        if not update or not uid:
            return write_status({"blocked": None})
        if not proxy():
            return write_status({"blocked": "no_proxy"})
        queue = self.queue_factory()
        blocked = await blocked_reason(uid, self.call, queue)
        # Only an agent mid-turn hears about it: a notice would wake an idle
        # one, and an idle one is simply activated within about a minute.
        if update["version"] not in self.noticed and blocked in {"working", "working-background"}:
            unseen = (os.environ.get("CM_AGENT_ENGINE") == "codex"
                      and update.get("tools_change", False))
            text = notice_text(update["version"], self.monitors(), unseen, self.can_ack())
            try:
                queue.publish(f"mcp-update:{update['version']}", NOTICE_SOURCE, text,
                              f"[cm-mcp-update {update['version']}]")
                self.noticed.add(update["version"])
            except (OSError, ValueError, RuntimeError):
                pass  # the notice is a courtesy; activation does not depend on it
        if blocked is None and time.monotonic() - self.requested_at >= IDLE_S:
            # The proxy also requires IDLE_S without client requests, so a
            # refused request is simply repeated on a later tick.
            self.requested_at = time.monotonic()
            await self.send_activate("idle")
        return write_status({"blocked": blocked})

    async def run(self):
        import asyncio
        while True:
            await asyncio.sleep(POLL_S)
            try:
                await self.tick()
            except asyncio.CancelledError:
                raise
            except Exception:  # noqa: BLE001 - policy failure must never touch tools
                pass


def ack(monitors: int) -> dict:
    """ack_reconnect's answer. The caller sends the activation request."""
    update = pending()
    if not update:
        return {"activating": False, "pending": False,
                "version": loaded()["version"], "note": "No CM MCP update is pending."}
    if not proxy():
        return {"activating": False, "pending": update["version"],
                "note": "This session's MCP server has no proxy; a manual /mcp reconnect is needed once."}
    out = {"activating": True, "after": "this reply", "version": update["version"],
           "from_version": loaded()["version"], "monitors_carried": monitors}
    if os.environ.get("CM_AGENT_ENGINE") == "codex" and update.get("tools_change"):
        out["note"] = "New tools appear after a restart."
    return out


# ---- predecessor and promotion ---------------------------------------------
# A backend the proxy starts while another serves is a CANDIDATE: it answers
# the preflight but runs no notification consumer, policy or monitor
# adoption until its predecessor has exited (it may yet be abandoned).


def start_ticks(pid: int) -> str | None:
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()[19]
    except (OSError, IndexError):
        return None


def predecessor() -> tuple[int, str] | None:
    try:
        pid = int(os.environ.get("CM_MCP_PREDECESSOR_PID") or 0)
    except ValueError:
        return None
    if not pid or pid == os.getpid():
        return None
    return pid, os.environ.get("CM_MCP_PREDECESSOR_START", "")


def process_alive(pid: int, start: str = "") -> bool:
    """Whether this exact process (pid plus start time) still runs."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return False  # another user's process: not our predecessor
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
    except (OSError, IndexError):
        return False
    if fields[0] == "Z":
        return False  # exited; only waiting to be reaped
    return not start or fields[19] == start  # a reused pid is not our predecessor


# Longer than the proxy ever keeps an undecided candidate alive (a slow
# preflight plus the old child's stop sequence, which ends in SIGKILL), so
# reaching it means the predecessor survived its own replacement.
PROMOTION_DEADLINE_S = float(os.environ.get("CM_MCP_PROMOTION_DEADLINE_S", 45))


async def wait_promoted(poll_s: float = 0.2, deadline_s: float | None = None) -> None:
    """Return once the predecessor has exited. Past the deadline this FAILS
    LOUDLY: it logs and exits the process instead of starting the consumer
    beside a live predecessor. Nothing has been started yet, so exiting is
    clean; the proxy treats it as a crash and restarts (or, for a candidate
    not yet switched to, keeps the old child serving)."""
    import asyncio
    import sys
    before = predecessor()
    budget = PROMOTION_DEADLINE_S if deadline_s is None else deadline_s
    limit = time.monotonic() + budget
    while before and process_alive(*before):
        if time.monotonic() > limit:
            print(f"cm-mcp: predecessor {before[0]} still alive after "
                  f"{budget:g} s; exiting rather than running two consumers",
                  file=sys.stderr, flush=True)
            os._exit(3)
        await asyncio.sleep(poll_s)
