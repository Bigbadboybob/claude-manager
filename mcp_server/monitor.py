"""FastMCP-free monitor core: session status + transcript-tail helpers and
the multi-session wait loop shared by the `wait_for_any_session_idle` MCP
tool (`server.py`) and the `cm-wait` CLI (`wait.py`).

Kept deliberately free of any `mcp`/FastMCP import (and of
`cli.planning_client`) so the CLI can run under a bare `python3` with only
the stdlib plus this package's `control_client` and `transcripts` modules
on the path — the background monitor shouldn't need the full MCP server's
dependencies installed.
"""

from __future__ import annotations

import asyncio
import json
import time

from mcp_server import control_client
from mcp_server.transcripts import claude_code as transcripts_claude
from mcp_server.transcripts import codex as transcripts_codex
from mcp_server.transcripts.types import Role


def _session_status(state: str, idle: bool) -> str:
    """Collapse the daemon's (state, idle) pair into ONE unambiguous
    word. Agents routinely misread the raw signal because "idle" means
    three different things depending on `state`; this is the legible
    summary they should branch on:

      - "starting"        spawned but transcript not bound yet — the
                          agent is still coming up, OR it's a
                          transcript-less bash session. `idle` is not
                          meaningful here, so a quiet pending session is
                          still "starting", not "done".
      - "working"         transcript bound, PTY active in the last ~2s
                          (mid-turn).
      - "awaiting_input"  transcript bound, PTY quiet — the agent
                          finished its turn and is back at the prompt,
                          waiting on you.
      - "exited"          process gone.

    `state` and `idle` are still returned alongside `status` everywhere,
    so callers that want the raw signal keep it.
    """
    if state == "exited":
        return "exited"
    if state == "pending":
        return "starting"
    return "awaiting_input" if idle else "working"


# Large enough to mean "read the whole transcript": parse_lines stops
# once it has this many rendered messages, and no real session approaches
# it, so this reads everything.
_READ_ALL_LIMIT = 100_000_000

# ── Transcript-shape semantic idle ────────────────────────────────────
#
# The daemon's `idle` is a PTY-output quietness heuristic (~2s). A
# session running a BACKGROUND task (subagent, background bash) keeps
# its PTY noisy forever — spinner + status-line updates — while the
# main agent is at the prompt, so quietness-idle reports "working"
# indefinitely (the false-busy bug). The transcript is the semantic
# signal: spinner noise never touches it, and a claude-code turn is
# over exactly when the last main-chain message line is an assistant
# entry whose `stop_reason` is terminal. Verified shape (2026-07-18):
# a quiet session's tail is `assistant(stop_reason=end_turn)` followed
# only by non-message lines (`system`, `attachment`, `ai-title`,
# `mode`, `permission-mode`, `last-prompt`); mid-turn tails end in
# `assistant(stop_reason=tool_use)` or a `user` tool-result line.
# Sidechain (subagent) entries carry `isSidechain: true` and must be
# skipped — a foreground subagent's own end_turn is not the main
# agent's.

# stop_reasons that mean the assistant's turn is COMPLETE. `tool_use`
# (and None, mid-stream) mean more work is coming. Conservative: an
# unknown stop_reason reads as busy, which degrades to the existing
# PTY heuristic rather than a premature fire.
_TERMINAL_STOP_REASONS = {"end_turn", "stop_sequence", "refusal"}

# How much transcript tail to scan backwards for the last message line.
# Message lines are at most a few hundred KB apart in practice.
_SEMANTIC_TAIL_BYTES = 256 * 1024

# How long the transcript must CONTINUOUSLY read turn-complete before a
# busy-PTY session is reported idle. Debounces the moment between an
# end_turn landing and a queued follow-up user message starting the
# next turn.
SEMANTIC_IDLE_GRACE_S = 3.0


def transcript_turn_complete(engine: str, path: str | None) -> bool:
    """True iff `path`'s last main-chain message line is a completed
    assistant turn. claude-code transcripts only — codex encodes turns
    differently and keeps the PTY heuristic. Sync (file IO): callers
    offload via asyncio.to_thread. Any read/parse failure returns
    False (falls back to the PTY heuristic)."""
    if engine != "claude-code" or not path:
        return False
    try:
        with open(path, "rb") as f:
            f.seek(0, 2)
            size = f.tell()
            f.seek(max(0, size - _SEMANTIC_TAIL_BYTES))
            tail = f.read().decode("utf-8", errors="replace")
    except OSError:
        return False
    for line in reversed(tail.splitlines()):
        line = line.strip()
        if not line:
            continue
        try:
            entry = json.loads(line)
        except ValueError:
            # The first line of a mid-file tail read is usually a
            # partial line; anything else unparseable is skipped the
            # same way.
            continue
        if not isinstance(entry, dict):
            continue
        etype = entry.get("type")
        if etype not in ("assistant", "user"):
            continue  # system / attachment / ai-title / mode / ...
        if entry.get("isSidechain"):
            continue  # a subagent's messages, not the main chain
        if etype != "assistant":
            return False  # user line last: a turn is starting/running
        stop = (entry.get("message") or {}).get("stop_reason")
        return stop in _TERMINAL_STOP_REASONS
    return False


def _parser_for(engine: str):
    return transcripts_codex if engine == "codex" else transcripts_claude


def _read_all_messages(engine: str, path: str, generation: int):
    """Parse the entire current-generation transcript. Returns
    (messages, end_cursor)."""
    return _parser_for(engine).read_messages(
        path, generation, None, _READ_ALL_LIMIT
    )


def _last_assistant(messages) -> dict | None:
    """The last assistant message in `messages` as a dict, or None."""
    for m in reversed(messages):
        if m.role == Role.ASSISTANT:
            return m.to_dict()
    return None


def _monitor_completed_entry(
    uid: str,
    status: str,
    state: str,
    idle: bool,
    engine: str,
    transcript_path: str | None,
    generation: int,
    include_message: bool,
) -> dict:
    """Build a monitor completed-entry, reading the final assistant
    message off disk when asked. Sync (does file IO) — callers offload it
    via asyncio.to_thread."""
    last_message = None
    if include_message and transcript_path is not None:
        try:
            msgs, _ = _read_all_messages(engine, transcript_path, generation)
            last_message = _last_assistant(msgs)
        except OSError:
            last_message = None
    return {
        "session_uid": uid,
        "status": status,
        "state": state,
        "idle": idle,
        "last_message": last_message,
    }


async def _monitor_sessions(
    session_uids: list[str],
    *,
    mode: str = "any",
    timeout_s: float = 1800.0,
    poll_interval_s: float = 2.0,
    pending_idle_grace_s: float = 8.0,
    return_last_message: bool = True,
) -> dict:
    """Watch `session_uids` until the stop condition is met or `timeout_s`.

    A session "completes" by the same rule as `wait_for_session_idle`:
    it finishes its turn (ready + quiet -> `awaiting_input`), or it
    `exited`; a transcript-less pending+quiet session reports after
    `pending_idle_grace_s`. A bad/unauthorized uid is reported once with
    status="error" so a typo can't block the whole monitor.

    mode="any" (default): return as soon as >=1 session completes. The
        `completed` batch may hold more than one if several finished in
        the same poll.
    mode="all": keep going, accumulating completions across polls, until
        EVERY session has completed (or `timeout_s`).

    Returns {completed, still_running, timed_out}. Exactly the wire shape
    of `wait_for_any_session_idle`. On timeout, `completed` holds whatever
    finished so far (always empty for mode="any", since a non-empty batch
    would have returned earlier) and `still_running` the rest.
    """
    # Dedupe, preserve order.
    uids = list(dict.fromkeys(session_uids or []))
    if not uids:
        return {"completed": [], "still_running": [], "timed_out": False}

    deadline = time.monotonic() + max(1.0, min(timeout_s, 86400.0))
    interval = max(0.5, min(poll_interval_s, 30.0))
    grace = max(1.0, min(pending_idle_grace_s, 60.0))

    seen: dict[str, bool] = {u: False for u in uids}
    pending_idle_since: dict[str, float | None] = {u: None for u in uids}
    # Monotonic time each session's transcript FIRST read turn-complete
    # in an unbroken streak while its PTY stayed busy; None otherwise.
    semantic_idle_since: dict[str, float | None] = {u: None for u in uids}
    # Last successful (engine, transcript_path, generation) per uid, so we
    # can still read the final reply after a session is evicted on exit.
    last_meta: dict[str, tuple[str, str | None, int]] = {}
    remaining = list(uids)
    completed: list[dict] = []

    while True:
        now = time.monotonic()
        for uid in list(remaining):
            try:
                resolved = await asyncio.to_thread(
                    control_client.call,
                    "resolve_authorized_session",
                    {"session_uid": uid},
                )
            except control_client.ControlError as e:
                code = getattr(e, "code", "error")
                if seen[uid] and code == "not_found":
                    # Evicted after being seen == exited.
                    eng, tpath, gen = last_meta.get(
                        uid, ("claude-code", None, 0)
                    )
                    completed.append(await asyncio.to_thread(
                        _monitor_completed_entry, uid, "exited", "exited",
                        True, eng, tpath, gen, return_last_message,
                    ))
                else:
                    # Never resolved == bad / unauthorized uid. Report once
                    # rather than blocking the whole monitor on a typo.
                    completed.append({
                        "session_uid": uid, "status": "error",
                        "state": "exited", "idle": True,
                        "last_message": None, "error": code,
                    })
                remaining.remove(uid)
                continue

            seen[uid] = True
            state = resolved.get("state", "pending")
            idle = bool(resolved.get("idle", False))
            engine = resolved.get("engine", "claude-code")
            tpath = resolved.get("transcript_path")
            gen = int(resolved.get("generation", 0))
            last_meta[uid] = (engine, tpath, gen)

            done = False
            status_override = None
            if state == "exited":
                done = True
            elif state == "ready" and idle:
                done = True
            elif state == "ready":
                # PTY busy — but a background task's spinner keeps the
                # PTY noisy while the agent is at the prompt. Consult
                # the transcript shape (debounced) so those sessions
                # still complete. When the daemon ALSO reports
                # hook-derived `semantic_idle` (the cm Stop hook fired
                # after the last delivered input), skip the debounce —
                # the turn boundary is confirmed, not inferred.
                if await asyncio.to_thread(
                    transcript_turn_complete, engine, tpath
                ):
                    hook_idle = resolved.get("semantic_idle") is True
                    if semantic_idle_since[uid] is None:
                        semantic_idle_since[uid] = now
                    if hook_idle or (
                        now - semantic_idle_since[uid] >= SEMANTIC_IDLE_GRACE_S
                    ):
                        done = True
                        status_override = "awaiting_input"
                else:
                    semantic_idle_since[uid] = None
            elif state == "pending" and idle:
                if pending_idle_since[uid] is None:
                    pending_idle_since[uid] = now
                elif now - pending_idle_since[uid] >= grace:
                    done = True
            else:
                pending_idle_since[uid] = None

            if done:
                entry = await asyncio.to_thread(
                    _monitor_completed_entry, uid,
                    status_override or _session_status(state, idle),
                    state, idle,
                    engine, tpath, gen, return_last_message,
                )
                if status_override is not None:
                    # Surfaced for callers/debugging: the PTY was busy
                    # (spinner) but the transcript says the turn ended.
                    entry["idle_source"] = "transcript"
                completed.append(entry)
                remaining.remove(uid)

        # mode="any": first poll with any completion wins (completed holds
        # only that poll's batch, since earlier polls were empty).
        # mode="all": wait until nothing is left running.
        if mode == "any" and completed:
            return {
                "completed": completed,
                "still_running": remaining,
                "timed_out": False,
            }
        if mode == "all" and not remaining:
            return {
                "completed": completed,
                "still_running": remaining,
                "timed_out": False,
            }
        if time.monotonic() >= deadline:
            return {
                "completed": completed,
                "still_running": remaining,
                "timed_out": True,
            }
        await asyncio.sleep(interval)
