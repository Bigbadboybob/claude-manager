#!/usr/bin/env python3
"""cm Stop hook — injected into every cm-spawned claude-code session via
`--settings` (see `mcp_config::claude_settings_hook_arg` in the daemon,
mirrored by the TUI's `claude_args`). Runs at every turn boundary and:

  1. Reports the turn-end to the daemon (`session.turn_ended`), which
     stamps `last_turn_end_at` → `resolve_authorized_session` derives
     `semantic_idle` from it. This is the event-driven idle signal that
     PTY-quietness heuristics can't provide (a background task's
     spinner keeps the PTY noisy while the agent sits at its prompt).

  2. Drains this session's monitor inbox (`~/.cm/inbox/<uid>/*.json`,
     written by `async_monitor` when a fired monitor found the session
     mid-turn). If messages are pending, emits Stop-hook
     `{"decision": "block", "reason": <messages>}` — Claude then
     CONTINUES the turn, processing the messages as its next
     instruction. This delivers monitor notifications at the exact turn
     boundary with zero PTY typing (no mangling, no kitty timing).
     Verified empirically on claude CLI 2.1.214 (see NOTES.md S0).

Design constraints:
  - FAIL OPEN. A broken daemon socket, malformed stdin, or any other
    surprise must never wedge the agent's stop — every failure path
    exits 0 with no output.
  - stdlib only (plus mcp_server.control_client, itself stdlib-only),
    so any python3 can run it.
  - Loop-safe: blocking is keyed solely on inbox content, and messages
    are consumed (renamed away) BEFORE the block is emitted, so a
    re-fired Stop (`stop_hook_active: true`) with an empty inbox always
    allows the stop.
"""

from __future__ import annotations

import json
import os
import sys

# Make `mcp_server` importable when invoked as a bare script — same
# bootstrap as wait.py (this file lives at mcp_server/hooks/).
sys.path.insert(
    0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..")
)

INBOX_ROOT = os.path.expanduser("~/.cm/inbox")

# Turn-end report timeout. The hook runs on every turn boundary — a
# hung daemon must not stall the agent, so keep this tight.
TURN_ENDED_TIMEOUT_S = 3.0


def _session_uid() -> str:
    return os.environ.get("CM_TUI_SESSION_ID", "").strip()


def _report_turn_ended(uid: str, transcript_path: str | None = None) -> None:
    """Best-effort `session.turn_ended` self-report.

    `transcript_path` is the file Claude Code names in this hook's stdin
    payload — the only exact, live answer to "which conversation is this
    session in". The daemon re-stamps it as the session's resume key, so a
    restart resumes the CURRENT conversation instead of whatever id was
    recorded at spawn (which goes stale on /clear, /compact, or a fork, and
    then silently resumes the wrong history). The daemon bounds the claim to
    this session's own transcript directory; sending it is safe and
    best-effort like everything else here.
    """
    try:
        from mcp_server import control_client

        params = {"session_uid": uid}
        if transcript_path:
            params["transcript_path"] = transcript_path
        control_client.call(
            "session.turn_ended",
            params,
            timeout=TURN_ENDED_TIMEOUT_S,
        )
    except Exception:  # noqa: BLE001 — fail open, always
        pass


def _drain_inbox(uid: str) -> list[str]:
    """Consume every pending inbox message for `uid`, oldest first.
    Files are renamed to `.consumed` before reading so a concurrent
    writer or a hook re-run can't double-deliver; unreadable files are
    dropped (their content is retained monitor-side either way)."""
    inbox = os.path.join(INBOX_ROOT, uid)
    try:
        names = sorted(
            n for n in os.listdir(inbox) if n.endswith(".json")
        )
    except OSError:
        return []
    messages: list[str] = []
    for name in names:
        path = os.path.join(inbox, name)
        consumed = path + ".consumed"
        try:
            os.rename(path, consumed)
        except OSError:
            continue  # raced with another consumer — theirs now
        try:
            with open(consumed, "r", encoding="utf-8") as f:
                payload = json.load(f)
            text = payload.get("text") if isinstance(payload, dict) else None
            if isinstance(text, str) and text.strip():
                messages.append(text)
        except (OSError, ValueError):
            pass
        finally:
            try:
                os.remove(consumed)
            except OSError:
                pass
    return messages


def main() -> int:
    try:
        # Consume stdin fully (an unread pipe can raise BrokenPipeError on
        # the writer). The uid still comes from the env, which is
        # authoritative, but the payload carries `transcript_path` — the
        # live resume key we forward below. Malformed or absent JSON just
        # means no path; the turn-end report goes out either way.
        transcript_path = None
        try:
            raw = sys.stdin.read()
        except OSError:
            raw = ""
        try:
            payload = json.loads(raw) if raw.strip() else None
            if isinstance(payload, dict):
                value = payload.get("transcript_path")
                if isinstance(value, str) and value.strip():
                    transcript_path = value.strip()
        except ValueError:
            pass

        uid = _session_uid()
        if not uid:
            return 0  # not a cm session — nothing to do

        _report_turn_ended(uid, transcript_path)

        messages = _drain_inbox(uid)
        if messages:
            reason = "\n\n".join(messages)
            print(json.dumps({"decision": "block", "reason": reason}))
        return 0
    except Exception:  # noqa: BLE001 — never break the agent's stop
        return 0


if __name__ == "__main__":
    sys.exit(main())
