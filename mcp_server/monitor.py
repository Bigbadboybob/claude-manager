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
import hashlib
import json
import os
import time

from mcp_server import control_client
from mcp_server.transcripts import claude_code as transcripts_claude
from mcp_server.transcripts import codex as transcripts_codex
from mcp_server.transcripts.types import Role


def _session_status(state: str, idle: bool, reported_done: bool = False) -> str:
    """Collapse the daemon's (state, idle, reported_done) triple into ONE
    unambiguous word. Agents routinely misread the raw signal because
    "idle" means three different things depending on `state`; this is the
    legible summary they should branch on:

      - "starting"        spawned but transcript not bound yet — the
                          agent is still coming up, OR it's a
                          transcript-less bash session. `idle` is not
                          meaningful here, so a quiet pending session is
                          still "starting", not "done".
      - "working"         transcript bound, PTY active in the last ~2s
                          (mid-turn).
      - "awaiting_input"  transcript bound, PTY quiet — the agent
                          finished its turn and is back at the prompt,
                          waiting on you. NOT a claim that it finished
                          its WORK; see "reported".
      - "reported"        the agent called `report_done` and has not been
                          given new work since — it says it is finished.
      - "exited"          process gone.

    UX item 4a: `awaiting_input` used to carry three unrelated meanings —
    a worker delivering its final verdict, a worker whose background
    fan-out is still running, and a worker paused mid-task all looked
    identical, so orchestrators pattern-matched the prose for "VERDICT:"
    to tell them apart. `reported` is the agent's own answer, and it
    outranks the PTY-derived words: a session that has declared itself
    done reads "reported" whether or not it is still flushing its final
    message (`idle` still tells you whether the turn has ended).

    `exited` still wins over everything — a dead session's own opinion of
    its state stopped being actionable when the process went away, and
    "did it report before it died" is a separate field (see
    `_monitor_completed_entry`).

    `state` and `idle` are still returned alongside `status` everywhere,
    so callers that want the raw signal keep it.
    """
    if state == "exited":
        return "exited"
    if reported_done:
        return "reported"
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


def last_completed_turn_fingerprint(engine: str, path: str | None) -> str | None:
    """Identity of the last COMPLETED main-chain assistant turn in
    `path`, or None when there isn't one (no transcript, unreadable, a
    non-claude engine, or no terminal assistant entry in the tail
    window).

    This is the edge-trigger baseline for async monitors: a monitor
    armed while the watched session is already at its prompt fires only
    once this identity CHANGES — i.e. a NEW turn completed after
    arming — instead of instant-firing on the stale last message.

    Unlike `transcript_turn_complete` this skips over trailing user /
    tool-result lines: mid-turn, it identifies the PREVIOUS completed
    turn, which is exactly the right baseline (the in-flight turn's
    completion changes it). Sync (file IO): callers offload via
    asyncio.to_thread where it matters."""
    if engine != "claude-code" or not path:
        return None
    try:
        with open(path, "rb") as f:
            f.seek(0, 2)
            size = f.tell()
            f.seek(max(0, size - _SEMANTIC_TAIL_BYTES))
            tail = f.read().decode("utf-8", errors="replace")
    except OSError:
        return None
    for line in reversed(tail.splitlines()):
        line = line.strip()
        if not line:
            continue
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        if not isinstance(entry, dict) or entry.get("type") != "assistant":
            continue
        if entry.get("isSidechain"):
            continue
        stop = (entry.get("message") or {}).get("stop_reason")
        if stop in _TERMINAL_STOP_REASONS:
            uid = entry.get("uuid")
            if uid:
                return str(uid)
            return hashlib.sha1(line.encode("utf-8")).hexdigest()
    return None


def baseline_for(engine: str, tpath: str | None) -> dict | None:
    """A baseline pinned to the session's CURRENT completed work, or None
    when there is nothing to anchor on (no transcript, unreadable, no
    completed turn yet) — in which case the watch degrades to level
    behavior for that session.

    Two uses, same shape: `async_monitor._capture_baseline` arms a fresh
    monitor with it, and `_monitor_sessions` RE-arms with it every time a
    `until="final"` watch skips an interim turn end. Sync (file IO):
    callers offload via asyncio.to_thread where it matters."""
    if engine == "claude-code":
        fp = last_completed_turn_fingerprint(engine, tpath)
        return {"kind": "turn", "value": fp} if fp is not None else None
    if not tpath:
        return None
    try:
        return {"kind": "size", "value": os.path.getsize(tpath)}
    except OSError:
        return None


def report_anchor_for(resolved: dict) -> float | None:
    """The `report_done` timestamp a watch should treat as ALREADY SEEN,
    or None when the session has not reported.

    The done-report equivalent of an edge baseline: a `until="final"`
    watch armed on a session that reported ten minutes ago must not fire
    on that stale report, and comparing timestamps is exact — the daemon
    clears the flag on any new input, so a fresh report always carries a
    later stamp. Sessions reporting without a timestamp (a daemon older
    than this field) anchor at 0.0, which reads as "seen"."""
    if not resolved.get("reported_done"):
        return None
    try:
        return float(resolved.get("reported_done_at") or 0.0)
    except (TypeError, ValueError):
        return 0.0


def _report_is_new(resolved: dict, anchor: float | None) -> bool:
    """True when `resolved` carries a done report the watch has not
    already accounted for at arm time."""
    current = report_anchor_for(resolved)
    if current is None:
        return False
    return anchor is None or current != anchor


def _edge_passed(baseline: dict, engine: str, tpath: str | None) -> bool:
    """True when the watched session has produced NEW completed work
    since `baseline` was captured at arm time. "turn" baselines compare
    the last-completed-turn fingerprint (claude-code); "size" baselines
    (codex — no turn parse) accept any transcript growth. Unknown
    baseline kinds fail open (level behavior)."""
    kind = baseline.get("kind")
    if kind == "turn":
        fp = last_completed_turn_fingerprint(engine, tpath)
        return fp is not None and fp != baseline.get("value")
    if kind == "size":
        if not tpath:
            return False
        try:
            return os.path.getsize(tpath) > int(baseline.get("value", 0))
        except OSError:
            return False
    return True


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


# Outcome keys the daemon puts on a session's
# `resolve_authorized_session` / `list_sessions(include_exited)` payload.
# Copied onto the completed entry so a consumer can tell "the agent
# finished and stopped" apart from "someone killed it mid-turn" — the
# difference between a final report and a truncated fragment. See
# `async_monitor._format_fire_message`.
#
#   killed / killed_by / exited_at   how it ended (UX item 3b; the
#       daemon tombstone. `killed_by` is a who-or-what — a session uid,
#       "operator", "memory-cap", or a uid annotated with the sweep that
#       killed it — rendered verbatim, never parsed).
#   reported_done / reported_done_at / report_reason
#       whether it said it was FINISHED (UX item 4a). Present on live
#       rows and carried onto the tombstone, so "exited after reporting
#       done" is distinguishable from "exited mid-task".
_EXIT_PROVENANCE_KEYS = (
    "killed",
    "killed_by",
    "exited_at",
    "reported_done",
    "reported_done_at",
    "report_reason",
)


def _monitor_completed_entry(
    uid: str,
    status: str,
    state: str,
    idle: bool,
    engine: str,
    transcript_path: str | None,
    generation: int,
    include_message: bool,
    exit_meta: dict | None = None,
) -> dict:
    """Build a monitor completed-entry, reading the final assistant
    message off disk when asked. Sync (does file IO) — callers offload it
    via asyncio.to_thread.

    `exit_meta` is the daemon's resolve payload for the session (or None
    when it could no longer be resolved); its exit-provenance keys —
    `killed` / `killed_by` / `exited_at` — are carried onto the entry when
    present so the fire message can label a killed session as killed
    instead of presenting its last transcript line as a final report."""
    last_message = None
    if include_message and transcript_path is not None:
        try:
            msgs, _ = _read_all_messages(engine, transcript_path, generation)
            last_message = _last_assistant(msgs)
        except OSError:
            last_message = None
    entry = {
        "session_uid": uid,
        "status": status,
        "state": state,
        "idle": idle,
        "last_message": last_message,
    }
    for key in _EXIT_PROVENANCE_KEYS:
        if exit_meta and exit_meta.get(key) is not None:
            entry[key] = exit_meta[key]
    return entry


async def _monitor_sessions(
    session_uids: list[str],
    *,
    mode: str = "any",
    until: str = "turn_end",
    timeout_s: float = 1800.0,
    poll_interval_s: float = 2.0,
    pending_idle_grace_s: float = 8.0,
    return_last_message: bool = True,
    baselines: dict[str, dict] | None = None,
    report_anchors: dict[str, float] | None = None,
) -> dict:
    """Watch `session_uids` until the stop condition is met or `timeout_s`.

    `baselines` (uid -> baseline captured at arm time, see
    `_edge_passed`) makes the watch EDGE-triggered for those uids: an
    idle observation only completes once the session has produced a new
    completed turn (or grown its transcript) past the baseline. Exits
    and evictions always complete regardless. Absent/None baselines
    keep the level-triggered behavior the blocking `wait_*` tools want
    (already-idle returns immediately).

    A session "completes" by the same rule as `wait_for_session_idle`:
    it finishes its turn (ready + quiet -> `awaiting_input`), or it
    `exited`; a transcript-less pending+quiet session reports after
    `pending_idle_grace_s`. A bad/unauthorized uid is reported once with
    status="error" so a typo can't block the whole monitor.

    `until` selects WHAT counts as finished for each session:

    until="turn_end" (default): the rule above — the next completed turn.
    until="final": a turn ending is not enough. The session completes
        only when it EXITS or when its agent has called `report_done`
        (`reported_done` on the daemon payload, cleared by any new input
        — see `DaemonSession::reported_done`). Every interim turn end
        RE-ARMS the watch against the turn that just finished, so the
        same idle state can't be re-examined every poll and the skipped
        turns are counted onto the eventual entry as `interim_turn_ends`.
        This is the mode for "wake me when the worker is DONE, not each
        time it pauses" — the fan-out case where a worker ends its
        launch turn with background subagents still running, and an
        orchestrator reading that as completion is a live incident.
        Note a session that can't report (bash, or any agent that never
        calls the tool) completes only on exit, so pair final mode with
        a `timeout_s` you're willing to wait.

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

    final_mode = until == "final"
    # Re-arm bookkeeping lives here rather than on the caller's record so a
    # blocking caller gets the same behavior; `baselines` is mutated in
    # place for uids the final watch skips past.
    if final_mode and baselines is None:
        baselines = {}
    # uid -> the done report already present when the watch armed (see
    # `report_anchor_for`). Absent uids have no anchor, so ANY report on
    # them fires — the level-triggered reading, which is what a caller
    # that passed no baselines asked for.
    report_anchors = report_anchors or {}
    interim_turn_ends: dict[str, int] = {u: 0 for u in uids}

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

            # Edge gate: an idle observation on the SAME turn that existed
            # when the watch was armed is not a completion.
            edge_ok = True
            if done and state != "exited" and baselines:
                b = baselines.get(uid)
                if b is not None and not await asyncio.to_thread(
                    _edge_passed, b, engine, tpath
                ):
                    edge_ok = False

            reported = bool(resolved.get("reported_done"))
            if final_mode and state != "exited":
                if _report_is_new(resolved, report_anchors.get(uid)):
                    # The report IS the edge. Deciding this off the
                    # transcript instead would be fragile in exactly the
                    # case that matters: this watch RE-ARMS its baseline
                    # past every interim turn, so a report arriving with
                    # no further transcript movement would sit behind a
                    # baseline the agent can never pass.
                    edge_ok = True
                elif done and edge_ok:
                    # A turn ended and the agent has NOT said it is
                    # finished. Re-arm past it and keep watching — this
                    # is the whole point of final mode. The re-arm also
                    # stops the same idle state from being re-tested (and
                    # its transcript re-read) on every poll.
                    new_baseline = await asyncio.to_thread(
                        baseline_for, engine, tpath
                    )
                    # Count only turns we can actually pin. A
                    # transcript-less session — bash, or an agent still
                    # coming up — has no turn boundary to re-arm against,
                    # and incrementing per poll would make the count a
                    # poll counter.
                    if (
                        new_baseline is not None
                        and new_baseline != baselines.get(uid)
                    ):
                        baselines[uid] = new_baseline
                        interim_turn_ends[uid] += 1
                    done = False

            if not edge_ok:
                # Idle on the pre-arm turn: not a completion, keep waiting
                # for the next edge.
                done = False
            if not done:
                status_override = None

            if done:
                # `status_override` only exists to force "awaiting_input"
                # when the PTY looks busy but the transcript says the turn
                # ended — a strictly weaker claim than the agent's own
                # done report, so `reported` wins over it.
                status_word = (
                    _session_status(state, idle, True)
                    if reported
                    else status_override or _session_status(state, idle)
                )
                entry = await asyncio.to_thread(
                    _monitor_completed_entry, uid,
                    status_word,
                    state, idle,
                    engine, tpath, gen, return_last_message,
                    resolved,
                )
                if status_override is not None:
                    # Surfaced for callers/debugging: the PTY was busy
                    # (spinner) but the transcript says the turn ended.
                    entry["idle_source"] = "transcript"
                if interim_turn_ends[uid]:
                    entry["interim_turn_ends"] = interim_turn_ends[uid]
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
