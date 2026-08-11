"""Self-notifying async session monitors (S2 of the proper-async-wait
branch — the push counterpart to the blocking `wait_*` family).

An agent that spawns or prompts a worker shouldn't park inside a
blocking MCP call. `register_monitor` starts a background asyncio task
IN THE MCP SERVER PROCESS — which lives exactly as long as the calling
session, so monitor lifetime matches caller lifetime — that watches the
target sessions via `_monitor_sessions`, and on completion delivers a
`[cm-monitor <id>]` message back into the CALLER's own session via
daemon `send_input` (self-target is always auth-allowed:
`check_session_caller` short-circuits self to Allow).

Delivery discipline:
  - gated on the caller being at its prompt (PTY-idle or
    transcript-shape idle — a message injected into an idle composer
    starts a new turn, exactly like the user typing);
  - the daemon side additionally defers while the OPERATOR is typing
    (S1a's typing-quiet gate);
  - verified: the caller's transcript must gain the `[cm-monitor <id>]`
    marker within a bounded window, else ONE redelivery is attempted;
  - never lost: the result is retained in `_MONITORS` either way and
    readable via the `list_monitors` tool.

FastMCP-free (like `monitor.py`) so it stays importable from the
`cm-wait` CLI's dependency footprint and trivially unit-testable.
"""

from __future__ import annotations

import asyncio
import json
import os
import secrets
import sys
import time

from mcp_server import control_client
from mcp_server.monitor import (
    _monitor_sessions,
    last_completed_turn_fingerprint,
    transcript_turn_complete,
)

# ── Tunables ──────────────────────────────────────────────────────────

# Default watch budget. Generous — a worker turn can legitimately run
# very long; the monitor delivers a timed_out notification either way.
DEFAULT_TIMEOUT_S = 1800.0

# How long to wait for the CALLER to reach its prompt before delivering
# anyway (a queued steering message is acceptable as a last resort; the
# daemon's typing-quiet gate still protects against operator mangling).
CALLER_IDLE_MAX_WAIT_S = 1800.0

# Poll cadence for the caller-idle gate and delivery verification.
POLL_S = 2.0

# How long after a send_input the marker must appear in the caller's
# transcript before we call the delivery lost. The daemon's delivery
# thread alone takes ~4s (settle + gap) and may defer on the
# typing-quiet gate (up to 180s) — keep this comfortably above the
# common path but far below the gate's worst case; the retry covers
# the rest.
VERIFY_WINDOW_S = 45.0

# Redeliveries after a failed verification (1 = two sends total).
MAX_REDELIVERIES = 1

# How long a REdelivery waits for the caller to be back at its prompt.
# A duplicate injected mid-turn (or mid-typing) is worse than no
# redelivery — the result is retained in list_monitors regardless — so
# unlike the first attempt this gate does NOT fall through to "deliver
# anyway" on timeout.
REDELIVERY_GATE_S = 300.0

# Backstop against runaway registration loops.
MAX_ACTIVE_MONITORS = 32

# How much of the caller's transcript tail to scan for the marker.
_MARKER_SCAN_BYTES = 512 * 1024

# Truncation for each watched session's last message in the fire text.
_LAST_MESSAGE_CHARS = 700

# Per-session monitor inboxes, drained by the cm Stop hook
# (mcp_server/hooks/cm_stop_hook.py) at turn boundaries. Host-local by
# construction: the MCP server runs as a child of the caller session on
# the same host as the hook.
INBOX_ROOT = os.path.expanduser("~/.cm/inbox")

# monitor_id -> record. Records outlive task completion (they hold the
# result for `list_monitors` pickup); bounded because registration
# rejects past MAX_ACTIVE_MONITORS and completed records are pruned
# FIFO past twice that.
_MONITORS: dict[str, dict] = {}


class RegistrationError(Exception):
    """Monitor could not be registered; .code is machine-readable."""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


def _self_uid() -> str:
    return os.environ.get("CM_TUI_SESSION_ID", "").strip()


def _log(msg: str) -> None:
    print(f"cm-monitor: {msg}", file=sys.stderr, flush=True)


def _active_monitors() -> list[dict]:
    return [m for m in _MONITORS.values() if m["state"] == "watching"]


def _prune_finished() -> None:
    done = [k for k, m in _MONITORS.items() if m["state"] != "watching"]
    excess = len(done) - MAX_ACTIVE_MONITORS * 2
    for k in done[:max(0, excess)]:
        _MONITORS.pop(k, None)


def _capture_baseline(uid: str) -> tuple[dict | None, bool]:
    """One resolve of `uid` at ARM time. Returns (baseline-or-None,
    currently_at_prompt).

    Runs synchronously on the event loop by design: the baseline must
    be captured BEFORE a just-sent prompt can start its turn (the
    daemon's PTY delivery gives ~4s of runway; registration follows the
    send within milliseconds), or a fast worker could complete the new
    turn before an async capture ran and the edge would never fire. One
    socket round-trip + one bounded tail read; any failure degrades to
    a level trigger (None) rather than blocking registration."""
    try:
        resolved = control_client.call(
            "resolve_authorized_session", {"session_uid": uid}
        )
    except (control_client.ControlError, control_client.TransportError):
        return None, False
    state = resolved.get("state", "pending")
    idle = bool(resolved.get("idle", False))
    engine = resolved.get("engine", "claude-code")
    tpath = resolved.get("transcript_path")
    if state != "ready" or not tpath:
        # Starting / transcript-less (bash): nothing to anchor an edge
        # on — level behavior, which is right for a fresh spawn (its
        # first completed turn should fire).
        return None, False
    if engine == "claude-code":
        fp = last_completed_turn_fingerprint(engine, tpath)
        at_prompt = idle or transcript_turn_complete(engine, tpath)
        if fp is None:
            return None, at_prompt
        return {"kind": "turn", "value": fp}, at_prompt
    try:
        size = os.path.getsize(tpath)
    except OSError:
        return None, idle
    return {"kind": "size", "value": size}, idle


def register_monitor(
    session_uids: list[str],
    *,
    mode: str = "any",
    note: str = "",
    timeout_s: float = DEFAULT_TIMEOUT_S,
    source: str = "explicit",
    edge: bool = True,
) -> dict:
    """Start a background watch on `session_uids`; returns immediately.

    `edge=True` (default) arms against the sessions' CURRENT transcript
    state: the monitor fires only when a watched session completes a
    NEW turn (or exits) after registration. An already-idle session
    therefore does not instant-fire its stale last message — it is
    reported in the returned `already_idle` list instead so the caller
    can `read_last_turn` it directly. `edge=False` restores the
    level-triggered behavior (fire on current idleness).

    Raises RegistrationError when the environment can't support a
    monitor (no caller identity / no event loop / too many monitors).
    Auto-registration callers (`source="auto"`) replace any active
    monitor watching the SAME uid set for a fresh anchor instead of
    stacking duplicates.
    """
    uids = list(dict.fromkeys(session_uids or []))
    if not uids:
        raise RegistrationError("invalid_params", "session_uids is empty")
    if mode not in ("any", "all"):
        raise RegistrationError("invalid_params", f"bad mode {mode!r}")
    caller = _self_uid()
    if not caller:
        raise RegistrationError(
            "no_caller",
            "CM_TUI_SESSION_ID is not set — cannot deliver a monitor "
            "notification back to this session",
        )
    try:
        loop = asyncio.get_running_loop()
    except RuntimeError:
        raise RegistrationError(
            "no_event_loop", "monitor registration requires a running loop"
        )

    # Auto-registrations dedupe per target set: prompting the same
    # worker again re-anchors the watch rather than stacking a second
    # (stale) monitor that would fire twice.
    if source == "auto":
        for m in _active_monitors():
            if set(m["watching"]) == set(uids) and m["mode"] == mode:
                m["task"].cancel()
                m["state"] = "replaced"

    if len(_active_monitors()) >= MAX_ACTIVE_MONITORS:
        raise RegistrationError(
            "too_many_monitors",
            f"{MAX_ACTIVE_MONITORS} monitors already active — cancel or "
            "let some fire first",
        )

    baselines: dict[str, dict] = {}
    already_idle: list[str] = []
    if edge:
        for uid in uids:
            baseline, at_prompt = _capture_baseline(uid)
            if baseline is not None:
                baselines[uid] = baseline
            if at_prompt:
                already_idle.append(uid)
    if source == "auto":
        # A just-prompted worker is EXPECTED to still be at its prompt
        # (the PTY delivery has seconds of runway) — reporting it as
        # already-idle on every send_input would be misleading noise.
        # The edge baseline is what actually matters, and it's captured.
        already_idle = []

    monitor_id = f"mon-{secrets.token_hex(3)}"
    record = {
        "monitor_id": monitor_id,
        "watching": uids,
        "mode": mode,
        "note": note,
        "source": source,
        "caller": caller,
        "state": "watching",
        "created_at": time.time(),
        "result": None,
        "delivered": None,
        "baselines": baselines,
        "task": None,
    }
    _MONITORS[monitor_id] = record
    record["task"] = loop.create_task(
        _run_monitor(record, timeout_s=timeout_s),
        name=f"cm-monitor-{monitor_id}",
    )
    _prune_finished()
    async_note = (
        f"Async monitor {monitor_id} registered. When the watched "
        f"session{'s finish' if len(uids) > 1 else ' finishes'} the "
        f"turn, a '[cm-monitor {monitor_id}]' message will be "
        "delivered into YOUR session and wake you. END YOUR TURN "
        "instead of polling — do not sit in wait_* calls or "
        "read_session_output loops."
    )
    if already_idle:
        async_note += (
            f" NOTE: {', '.join(already_idle)} "
            f"{'are' if len(already_idle) > 1 else 'is'} ALREADY at the "
            "prompt — the monitor is edge-triggered and fires only when "
            "a NEW turn completes, so it will stay quiet until then. If "
            "you wanted the current output, call read_last_turn instead "
            "of re-arming monitors."
        )
    return {
        "monitor_id": monitor_id,
        "watching": uids,
        "mode": mode,
        "already_idle": already_idle,
        "async_note": async_note,
    }


async def _run_monitor(record: dict, *, timeout_s: float) -> None:
    """Watch → format → deliver → verify. Never raises (logs instead);
    always leaves the record with a terminal state + retained result."""
    monitor_id = record["monitor_id"]
    deadline = time.monotonic() + max(1.0, min(timeout_s, 86400.0))
    try:
        result = None
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                result = {
                    "completed": [], "still_running": record["watching"],
                    "timed_out": True,
                }
                break
            try:
                result = await _monitor_sessions(
                    record["watching"],
                    mode=record["mode"],
                    timeout_s=remaining,
                    baselines=record.get("baselines") or None,
                )
                break
            except control_client.TransportError as e:
                # Daemon briefly unreachable (restart) — the sessions
                # survive it (daemon-side persistence); retry.
                _log(f"{monitor_id}: daemon unreachable ({e}); retrying")
                await asyncio.sleep(5.0)
        record["result"] = result
        record["state"] = "fired"
        message = _format_fire_message(record, result)
        delivered = await _deliver_to_caller(record, message)
        record["delivered"] = delivered
        record["state"] = "delivered" if delivered else "undelivered"
        if not delivered:
            _log(
                f"{monitor_id}: could not verify delivery to "
                f"{record['caller']} — result retained, readable via "
                "list_monitors"
            )
    except asyncio.CancelledError:
        # Cancellation is terminal from ANY in-flight state — a monitor
        # cancelled mid-delivery ("fired") must not resurrect as
        # "undelivered" and keep sending. Only the auto-replace path's
        # "replaced" label survives.
        if record["state"] != "replaced":
            record["state"] = "cancelled"
        raise
    except Exception as e:  # noqa: BLE001 — background task, never raise
        record["state"] = "error"
        record["error"] = str(e)
        _log(f"{monitor_id}: monitor loop crashed: {e!r}")


def _fmt_exit_ts(value) -> str | None:
    """Render a unix timestamp the way the daemon does
    (`YYYY-MM-DDTHH:MM:SSZ`), or None when there's nothing usable."""
    try:
        ts = float(value)
    except (TypeError, ValueError):
        return None
    if ts <= 0:
        return None
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))


def _entry_lines(entry: dict) -> list[str]:
    """Render ONE completed session as fire-message lines.

    Two UX rules live here:

    3b — a KILLED session is announced as killed. Its transcript tail is
    whatever it happened to be saying when the SIGKILL landed, so it is
    printed on its own explicitly-labelled line, never in the
    `- <uid> (<status>): <content>` slot where a reader (or a parsing
    orchestrator) would take it for the worker's final report.

    3c — a fire is flagged when it is not a confirmed completion:
    transcript-derived idle (the PTY is still live, so background work
    may be running), and idle-but-alive sessions generally (an agent
    pausing between turns looks identical to one that finished).

    Everything is APPEND-ONLY relative to the historical
    `- <uid> (<status>): <content>` line — annotations are indented
    continuation lines — so existing consumers keep parsing.
    """
    uid = entry.get("session_uid", "?")
    status = entry.get("status", "?")
    lm = entry.get("last_message") or {}
    content = (lm.get("content") or "").strip()
    if len(content) > _LAST_MESSAGE_CHARS:
        content = (
            content[:_LAST_MESSAGE_CHARS]
            + f"… [truncated — read_last_turn('{uid}') for the rest]"
        )

    lines: list[str] = []
    killed = bool(entry.get("killed"))
    if killed:
        killer = entry.get("killed_by")
        when = _fmt_exit_ts(entry.get("exited_at"))
        head = f"- {uid} (killed"
        if killer:
            head += f" by {killer}"
        if when:
            head += f" at {when}"
        lines.append(head + ")")
        if content:
            lines.append(
                f"  last transcript fragment before kill: {content}"
            )
    elif content:
        lines.append(f"- {uid} ({status}): {content}")
    else:
        lines.append(f"- {uid} ({status})")

    if entry.get("idle_source") == "transcript":
        lines.append(
            "  turn ended per transcript; PTY still active — background "
            "work may still be running"
        )
    if not killed and entry.get("state") != "exited":
        lines.append(
            "  (no explicit done-report — this may be an interim turn, "
            "not completion)"
        )
    return lines


def _format_fire_message(record: dict, result: dict) -> str:
    """The text injected into the caller's session. Starts with the
    marker (also used for delivery verification)."""
    lines: list[str] = []
    head = f"[cm-monitor {record['monitor_id']} fired]"
    if record["note"]:
        head += f" {record['note']}"
    lines.append(head)
    if result.get("timed_out"):
        lines.append(
            "The watch TIMED OUT before every watched session finished."
        )
    for entry in result.get("completed", []):
        lines.extend(_entry_lines(entry))
    still = result.get("still_running") or []
    if still:
        lines.append(f"Still running: {', '.join(still)}")
    # Only claim the workers are at their prompt when at least one
    # actually is: a batch of exited/killed sessions has nothing to send
    # a follow-up to, and saying otherwise sends the orchestrator
    # prompting a corpse. `still_running` is part of that judgement — a
    # mode="any" fire whose first completer exited must NOT tell the
    # orchestrator that the live siblings listed one line above are gone.
    done = result.get("completed") or []
    all_exited = bool(done) and all(e.get("state") == "exited" for e in done)
    if all_exited and still:
        lines.append(
            "(Async monitor notification. The completed session(s) above "
            "EXITED — there is no prompt left to send them follow-ups; "
            "read what they left with read_last_turn. The still-running "
            "session(s) are live but NO LONGER WATCHED — re-arm with "
            "monitor_sessions to be woken when they finish.)"
        )
    elif all_exited:
        lines.append(
            "(Async monitor notification. The watched session(s) have "
            "EXITED — there is no prompt left to send follow-ups to. Read "
            "what they left with read_last_turn, or start a fresh session.)"
        )
    else:
        lines.append(
            "(Async monitor notification. The completed worker(s) are now "
            "awaiting input — continue orchestrating: read full output with "
            "read_last_turn, send follow-ups with send_input, or finish up. "
            "Prompting a worker again auto-registers a fresh monitor.)"
        )
    return "\n".join(lines)


async def _caller_at_prompt(caller: str) -> tuple[bool, dict | None]:
    """One resolve of the caller: (at_prompt, resolved-or-None)."""
    try:
        resolved = await asyncio.to_thread(
            control_client.call,
            "resolve_authorized_session",
            {"session_uid": caller},
        )
    except control_client.ControlError:
        return False, None
    except control_client.TransportError:
        return False, None
    state = resolved.get("state", "pending")
    idle = bool(resolved.get("idle", False))
    if state == "ready" and idle:
        return True, resolved
    if state == "ready":
        sem = await asyncio.to_thread(
            transcript_turn_complete,
            resolved.get("engine", "claude-code"),
            resolved.get("transcript_path"),
        )
        return sem, resolved
    # pending (no transcript yet) — treat quiet as at-prompt.
    return idle, resolved


def _transcript_contains_marker(path: str | None, marker: str) -> bool:
    if not path:
        return False
    try:
        with open(path, "rb") as f:
            f.seek(0, 2)
            size = f.tell()
            f.seek(max(0, size - _MARKER_SCAN_BYTES))
            tail = f.read().decode("utf-8", errors="replace")
    except OSError:
        return False
    return marker in tail


def _write_inbox(caller: str, monitor_id: str, message: str) -> str | None:
    """Atomically drop `message` into the caller's inbox. Returns the
    final path, or None when the write failed (delivery falls back to
    PTY injection)."""
    inbox = os.path.join(INBOX_ROOT, caller)
    path = os.path.join(inbox, f"{int(time.time() * 1000)}-{monitor_id}.json")
    tmp = path + ".tmp"
    try:
        os.makedirs(inbox, exist_ok=True)
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(json.dumps({"text": message}))
        os.rename(tmp, path)
        return path
    except OSError as e:
        _log(f"{monitor_id}: inbox write failed ({e}); using PTY delivery")
        try:
            os.remove(tmp)
        except OSError:
            pass
        return None


async def _try_inbox_delivery(record: dict, message: str) -> bool:
    """Turn-boundary delivery for a mid-turn caller: drop the message
    in the caller's inbox; the cm Stop hook drains it when the turn
    ends (block+reason — atomic, no PTY typing). Returns True once the
    hook consumed it. Returns False — after TAKING THE MESSAGE BACK —
    when the caller reaches its prompt with the file unconsumed (a
    hook-less caller: pre-hook spawn, codex) or the wait cap passes;
    the caller then gets the PTY path instead."""
    caller = record["caller"]
    monitor_id = record["monitor_id"]
    path = _write_inbox(caller, monitor_id, message)
    if path is None:
        return False
    try:
        deadline = time.monotonic() + CALLER_IDLE_MAX_WAIT_S
        while time.monotonic() < deadline:
            if not os.path.exists(path):
                _log(f"{monitor_id}: delivered via Stop-hook inbox")
                return True
            at_prompt, _ = await _caller_at_prompt(caller)
            if at_prompt:
                # Caller is at its prompt but nothing consumed the
                # message — no Stop hook fired for it. Take it back; PTY
                # injection is both available and safe now.
                try:
                    os.remove(path)
                except FileNotFoundError:
                    return True  # consumed in the race after all
                except OSError:
                    pass
                return False
            await asyncio.sleep(POLL_S)
    except asyncio.CancelledError:
        # cancel_monitor's purge can race a just-written file — take the
        # pending message back ourselves so nothing delivers post-cancel.
        try:
            os.remove(path)
        except OSError:
            pass
        raise
    try:
        os.remove(path)
    except FileNotFoundError:
        return True
    except OSError:
        pass
    return False


async def _marker_landed(
    caller: str, marker: str, fallback_tpath: str | None
) -> bool:
    """Check the caller's CURRENT transcript for `marker`. Re-resolves
    the transcript path each call — a caller that was resumed/rotated
    mid-delivery moves to a new file, and verifying against the
    arm-time snapshot would miss a copy that demonstrably landed (the
    spurious-redelivery bug)."""
    tpath = fallback_tpath
    try:
        resolved = await asyncio.to_thread(
            control_client.call,
            "resolve_authorized_session",
            {"session_uid": caller},
        )
        tpath = resolved.get("transcript_path") or tpath
    except (control_client.ControlError, control_client.TransportError):
        pass
    if not tpath:
        return False
    return await asyncio.to_thread(_transcript_contains_marker, tpath, marker)


async def _deliver_to_caller(record: dict, message: str) -> bool:
    """Deliver the fire message into the caller's session.

    Mid-turn caller → Stop-hook inbox (atomic turn-boundary delivery,
    zero PTY typing). At-prompt caller (or inbox fallback) → gated PTY
    injection, verified against the caller's live transcript with at
    most one redelivery — and the redelivery only happens after a fresh
    marker re-check misses AND the caller is back at its prompt, so a
    late-landing first copy can't earn a duplicate. True = delivered
    (verified / hook-consumed)."""
    caller = record["caller"]
    monitor_id = record["monitor_id"]
    marker = f"[cm-monitor {monitor_id}"

    at_prompt, resolved = await _caller_at_prompt(caller)
    if not at_prompt:
        if await _try_inbox_delivery(record, message):
            return True
        # Fell back: re-gate on the prompt before injecting.
        gate_deadline = time.monotonic() + CALLER_IDLE_MAX_WAIT_S
        while time.monotonic() < gate_deadline:
            at_prompt, resolved = await _caller_at_prompt(caller)
            if at_prompt:
                break
            await asyncio.sleep(POLL_S)
        else:
            _log(
                f"{monitor_id}: caller {caller} busy past the gate window — "
                "delivering anyway (will queue as a steering message)"
            )
    last_tpath = (resolved or {}).get("transcript_path")

    for attempt in range(1 + MAX_REDELIVERIES):
        if attempt > 0:
            # The first copy may have landed late (queued composer
            # text) or verified against a stale path — re-check before
            # spamming a duplicate, and only resend into an at-prompt
            # caller. This gate does NOT deliver-anyway on timeout: an
            # unverified-but-probably-arrived notification beats a
            # guaranteed duplicate; the result stays in list_monitors.
            regate_deadline = time.monotonic() + REDELIVERY_GATE_S
            cleared = False
            while time.monotonic() < regate_deadline:
                if await _marker_landed(caller, marker, last_tpath):
                    return True
                at_prompt, resolved = await _caller_at_prompt(caller)
                if resolved and resolved.get("transcript_path"):
                    last_tpath = resolved["transcript_path"]
                if at_prompt:
                    cleared = True
                    break
                await asyncio.sleep(POLL_S)
            if not cleared:
                _log(
                    f"{monitor_id}: redelivery gate never cleared — "
                    "skipping resend (result retained in list_monitors)"
                )
                break
        try:
            await asyncio.to_thread(
                control_client.call,
                "send_input",
                {"session_uid": caller, "text": message, "submit": True},
            )
        except (control_client.ControlError, control_client.TransportError) as e:
            _log(f"{monitor_id}: delivery send failed: {e}")
            await asyncio.sleep(POLL_S)
            continue
        if last_tpath is None:
            # Nothing to verify against (caller transcript unknown) —
            # trust the successful send.
            return True
        verify_deadline = time.monotonic() + VERIFY_WINDOW_S
        while time.monotonic() < verify_deadline:
            if await _marker_landed(caller, marker, last_tpath):
                return True
            await asyncio.sleep(POLL_S)
        _log(
            f"{monitor_id}: delivery attempt {attempt + 1} not observed "
            f"in caller transcript"
        )
    return False


def _purge_inbox(caller: str, monitor_id: str) -> None:
    """Remove any not-yet-consumed inbox files this monitor wrote, so a
    cancelled monitor can't ghost-deliver at the caller's next turn
    boundary."""
    inbox = os.path.join(INBOX_ROOT, caller)
    try:
        names = os.listdir(inbox)
    except OSError:
        return
    for name in names:
        if name.endswith(f"-{monitor_id}.json"):
            try:
                os.remove(os.path.join(inbox, name))
            except OSError:
                pass


# States with anything left to do. Everything else ("delivered",
# "undelivered", "replaced", "cancelled", "error") is inert: the task is
# finished and only the retained result remains.
_LIVE_STATES = ("watching", "fired")


def _cancel_record(m: dict) -> None:
    """Terminal cancel from ANY state: stop the watch task (also aborts
    an in-flight delivery — a "fired" monitor can pend in the caller-
    idle gate for many minutes) and purge pending inbox messages.
    Retained results stay readable via list_monitors."""
    task = m.get("task")
    if task is not None and not task.done():
        task.cancel()
    if m["state"] in _LIVE_STATES:
        m["state"] = "cancelled"
    _purge_inbox(m["caller"], m["monitor_id"])


def cancel_monitor(monitor_id: str) -> dict:
    if monitor_id in ("all", "*"):
        live = [m for m in _MONITORS.values() if m["state"] in _LIVE_STATES]
        for m in live:
            _cancel_record(m)
        return {
            "cancelled": [m["monitor_id"] for m in live],
            "count": len(live),
        }
    m = _MONITORS.get(monitor_id)
    if m is None:
        return {"error": "not_found", "monitor_id": monitor_id}
    _cancel_record(m)
    return {"monitor_id": monitor_id, "state": m["state"]}


def list_monitors() -> dict:
    """Snapshot for the read-only tool. Strips the asyncio task."""
    out = []
    for m in _MONITORS.values():
        out.append({
            k: v for k, v in m.items() if k != "task"
        })
    return {"monitors": out}
