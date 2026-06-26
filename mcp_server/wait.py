#!/usr/bin/env python3
"""cm-wait — background multi-session monitor CLI.

The non-blocking counterpart to the `wait_for_any_session_idle` MCP tool.
Run it under `Bash(run_in_background=true)` from an orchestrator session: it
blocks until the watched sessions finish, prints the result as JSON to
stdout, and exits — at which point the Claude Code harness wakes the
orchestrator with the output. That lets the orchestrator stay free to handle
the user or launch other work while N workers run, instead of parking inside
a blocking MCP tool call. (See the orchestrator skills for the pattern.)

Usage:
    python3 mcp_server/wait.py <uid> [<uid> ...] [options]

Options:
    --all              Wait until ALL listed sessions finish (default: the
                       first one to finish wins).
    --timeout SECS     Max seconds to wait (default 1800).
    --poll-interval S  Seconds between polls (default 2).
    --grace SECS       Transcript-less pending grace (default 8).
    --no-last-message  Don't read each finished session's final message.

Output: a single JSON object on stdout (nothing before it — there is no
intermediate output to poll; just wait for the completion notification):
    {"completed": [{session_uid, status, state, idle, last_message}, ...],
     "still_running": [...], "timed_out": bool, "mode": "any"|"all"}
A human-readable one-liner goes to stderr; stdout stays pure JSON.

Exit codes: 0 = stop condition met, 1 = timed out, 2 = usage error,
            3 = transport error (daemon unreachable).

Auth/env: reuses the same env the MCP server is spawned with —
CM_TUI_SESSION_ID (caller identity) and CM_DAEMON_SOCKET (the read-only
`resolve_authorized_session` is a daemon method). It never sends input, so
CM_TUI_SOCKET isn't needed. Depends only on the stdlib plus this package's
`control_client` / `transcripts` — no `mcp`/FastMCP package required, so any
`python3` can run it.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys

# Make `mcp_server` importable when invoked as a bare script
# (`python3 /abs/path/mcp_server/wait.py`), independent of cwd — mirrors
# server.py. Harmless when run via `python -m mcp_server.wait` from the
# repo root.
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from mcp_server import control_client  # noqa: E402
from mcp_server.monitor import _monitor_sessions  # noqa: E402


def _parse_args(argv):
    p = argparse.ArgumentParser(
        prog="cm-wait",
        description="Background multi-session monitor (non-blocking "
                    "counterpart to wait_for_any_session_idle).",
    )
    p.add_argument("uids", nargs="+",
                   help="Session UIDs to watch (from list_sessions).")
    p.add_argument("--all", action="store_true",
                   help="Wait until ALL listed sessions finish "
                        "(default: return on the first to finish).")
    p.add_argument("--timeout", type=float, default=1800.0,
                   help="Max seconds to wait (default 1800).")
    p.add_argument("--poll-interval", type=float, default=2.0,
                   help="Seconds between polls (default 2).")
    p.add_argument("--grace", type=float, default=8.0,
                   help="Transcript-less pending grace seconds (default 8).")
    p.add_argument("--no-last-message", action="store_true",
                   help="Skip reading each finished session's final message.")
    return p.parse_args(argv)


def main(argv=None) -> int:
    args = _parse_args(sys.argv[1:] if argv is None else argv)
    mode = "all" if args.all else "any"
    try:
        result = asyncio.run(_monitor_sessions(
            args.uids,
            mode=mode,
            timeout_s=args.timeout,
            poll_interval_s=args.poll_interval,
            pending_idle_grace_s=args.grace,
            return_last_message=not args.no_last_message,
        ))
    except control_client.TransportError as e:
        print(json.dumps({"error": "transport", "message": str(e)}))
        print(f"cm-wait: cannot reach daemon: {e}", file=sys.stderr)
        return 3

    result["mode"] = mode
    print(json.dumps(result))
    done = [c["session_uid"] for c in result["completed"]]
    print(
        f"cm-wait: {len(done)} finished {done}, "
        f"{len(result['still_running'])} still running"
        + (" (timed out)" if result["timed_out"] else ""),
        file=sys.stderr,
    )
    return 1 if result["timed_out"] else 0


if __name__ == "__main__":
    sys.exit(main())
