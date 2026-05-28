"""Client for the TUI control socket (`~/.cm/tui.sock`).

Wire format mirrors the Rust side: 4-byte big-endian length prefix, then
a UTF-8 JSON object. One request per connection in v1.

The MCP server uses this to drive the TUI — every tool that needs the TUI
to do something (start a session, kill a session, list sessions, resolve
a transcript path for authorization) goes through `call`.

Caller identity (`CM_TUI_SESSION_ID`) and the socket path
(`CM_TUI_SOCKET`, defaulting to `~/.cm/tui.sock`) are read from the
process environment — these are injected by the TUI when it spawns the
agent that hosts this MCP server.

**Routing model post slice 10f** (default-flip):

Each MCP method is statically classified into one of two routing
sets defined below — `DAEMON_METHODS` (session lifecycle, MCP RPC
surface) goes to `~/.cm/daemon.sock`; everything else (workflow
controller, planning, subtask CRUD) stays on `~/.cm/tui.sock`. The
per-method routing is independent of any single env var; see
`SocketRoute` and `resolve_socket_for_method` below.

The historical `CM_USE_DAEMON_SOCKET=1` opt-in flag is honored as
a forwards-compatible override for `default_socket_path()` (the
fallback path when no per-method routing applies), but no
production code path relies on it any more — the per-method
routing above is authoritative.
"""

from __future__ import annotations

import json
import os
import socket
import struct
import uuid
from pathlib import Path


class ControlError(Exception):
    """Raised when the TUI returns an error response. Carries `code` and
    `message` from the Response envelope so callers can distinguish e.g.
    `unauthorized` (re-raise to the agent) from `not_found` (return None
    in a list)."""

    def __init__(self, code: str, message: str):
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


class TransportError(Exception):
    """Raised on socket-level failures (connect refused, EOF, malformed
    response). Distinct from `ControlError` so callers can decide whether
    to retry."""


class SocketRoute:
    """Resolved socket path + the routing decision that picked it.

    Sub-2b-3 review-5 #3: separating "which path do I dial" from
    "did the resolver pick the daemon" lets MCP tool wrappers
    (`server.py`) choose method names that match the socket
    target. Pre-fix, `default_socket_path()` would route to the
    daemon under `CM_USE_DAEMON_SOCKET=1` but the per-tool
    method selection only looked at `CM_DAEMON_SOCKET` — so the
    opt-in flag would route `start_session` to the daemon's
    full-shape handler, which rejects the minimal MCP wire
    shape with InvalidParams. Routing both decisions through
    this struct ensures the two signals can't drift.
    """

    def __init__(self, path: Path, chose_daemon: bool) -> None:
        self.path = path
        self.chose_daemon = chose_daemon


# Sub-2c: methods the daemon dispatches. The Python resolver
# routes these to `CM_DAEMON_SOCKET` (when set); everything
# else routes to `CM_TUI_SOCKET`. Drift between this set and
# the daemon's actual dispatch arms is caught by
# `tests/test_socket_route_selection.py::DaemonMethodsAlignmentTests`
# which reads `daemon/src/control/dispatch.rs` and asserts the
# match-arm method names match this set (modulo
# `DAEMON_DISPATCHED_BUT_TUI_ROUTED`).
#
# Internal methods (TUI-pushed, not agent-called) are still
# included for correctness — the resolver gives the right
# answer regardless of who's calling.
DAEMON_METHODS: frozenset[str] = frozenset({
    "ping",
    "start_session",
    "session.attach",
    "attach.open",
    "kill_session",
    "read_session_output",
    "list_sessions",
    "task.update_tree",
    "resolve_authorized_session",
    "session.set_transcript_path",
    # 10d-2c-1 review round-5 (F1): TUI pushes workflow context
    # onto an already-spawned daemon session via this RPC after
    # `launch_workflow` binds an Existing-slot participant.
    # Operator-only on the daemon side — agents can't call it.
    "session.set_workflow_context",
    "propose_task",
    "mcp_start_session",
    # 10d-2b: workflow_transition / workflow_done flip from
    # MCP-server-side `_append_event` (direct events.jsonl
    # write) to daemon-side writers via 10d-2a's
    # `WorkflowEventsWriter`. Session-callable; both daemon-
    # spawned and TUI-spawned participants can call.
    "workflow_transition",
    "workflow_done",
    # 11e: workflow_reject_finding flips from Python `_append_event`
    # direct file write to daemon-routed. Lets Option B's
    # broadcaster-in-WorkflowEventsWriter cover this event type
    # post-Phase-2 file-tail deletion.
    "workflow_reject_finding",
    # Operator-only push from TUI; included for dispatch-surface
    # alignment, not Python-routable.
    "tui.update_sessions_snapshot",
    # 10d-2c-2-1: operator-only push from TUI of the
    # workflow-definitions map (loaded from `workflows/*.toml`).
    # Sets up `DaemonState.workflow_definitions` for the upcoming
    # 2c-2-2 daemon-resident workflow driver. Included here for
    # dispatch-surface alignment; not Python-routable.
    "workflow.update_definitions",
    # 10d-2c-3a: read-only workflow query methods, relocated
    # from TUI socket to daemon dispatch. Both Operator and
    # Session-callable; daemon reads disk directly (no in-memory
    # cache) — simpler than the TUI's reactive-cache shape.
    "get_workflow_state",
    "list_workflows",
    # 10d-3: stop_workflow daemon-routed. Mutates run.status to
    # Detached via the shared `apply_stop_workflow_status`
    # helper (terminal-state guard preserves Done). TUI A-o flow
    # writes state.json directly via the same canonical helper;
    # both paths produce byte-identical mutations (fire-output
    # parity test pins this).
    "stop_workflow",
    # 10e-b: manifest.watch — TUI subscribes to daemon-side
    # manifest diffs (ManifestSnapshot frame followed by
    # ManifestDiff stream frames). Operator-only at the dispatch
    # boundary (no Session-caller use case). Included here for
    # dispatch-surface alignment; not Python-routable — the MCP
    # server has no manifest-stream tool.
    "manifest.watch",
    # 11b: events.subscribe — TUI subscribes to daemon-side
    # workflow-event broadcasts (one WorkflowEventStateSnapshot
    # per active run, followed by WorkflowEvent stream frames).
    # Operator-only at the dispatch boundary; included here for
    # dispatch-surface alignment, not Python-routable.
    "events.subscribe",
    # 11c: workflow.get_state — Operator-only cold-read companion
    # to events.subscribe. Returns the full WorkflowRun JSON.
    # Used by the TUI for one-shot reads (e.g. workflow history
    # view); the Session-callable `get_workflow_state` covers
    # agent-side reads. Not Python-routable.
    "workflow.get_state",
})


# Methods that exist as daemon dispatch arms but are deliberately
# routed to the TUI by Python. The daemon's `send_input` writes
# `text + b'\n'` raw to the PTY, which is the wrong Enter
# encoding for any agent that has pushed kitty keyboard mode
# (kitty Enter = `\x1b[13u`, not `\n`/`\r`). The body lands in
# the input box but the agent never submits — the recurring
# "Enter didn't work for some reason" symptom that breaks
# orchestrator-driven `send_input` against Claude Code workers.
# Routing back to the TUI's `send_input` puts the call through
# the encoding-aware drainer (`enter_bytes_for_mode` +
# `format_body_for_delivery` + the ENTER_GAP separator that
# stops codex classifying the trailing keystroke as paste tail).
# The daemon-side dispatch arm is retained as an
# Operator-callable surface for direct daemon clients (no
# kitty-mode concern for those use cases); MCP just doesn't
# route there.
DAEMON_DISPATCHED_BUT_TUI_ROUTED: frozenset[str] = frozenset({
    "send_input",
})


def resolve_socket_route() -> SocketRoute:
    """Resolve the control-socket path AND whether the daemon was
    selected as the target.

    Resolution order:

      1. `$CM_DAEMON_SOCKET` env var. Explicit daemon pin.
         Injected by the TUI when it spawned this agent.
         `chose_daemon=True`.
      2. `$CM_TUI_SOCKET` env var. Explicit TUI pin.
         `chose_daemon=False`.
      3. Fallback to `~/.cm/tui.sock`. `chose_daemon=False`.

    10f default-flip: the historical `CM_USE_DAEMON_SOCKET=1`
    opt-in branch is removed. The env var is now a silent
    no-op — routing depends entirely on the explicit socket
    env vars injected by `build_env` at TUI spawn time.

    Returns the resolved route unconditionally — connectivity
    is the caller's problem; this function just resolves where
    to dial and which dialect to speak.
    """
    daemon_env = os.environ.get("CM_DAEMON_SOCKET", "").strip()
    if daemon_env:
        return SocketRoute(Path(daemon_env), chose_daemon=True)
    tui_env = os.environ.get("CM_TUI_SOCKET", "").strip()
    if tui_env:
        return SocketRoute(Path(tui_env), chose_daemon=False)
    home = Path(os.environ.get("HOME", "/tmp"))
    return SocketRoute(home / ".cm" / "tui.sock", chose_daemon=False)


def default_socket_path() -> Path:
    """Backward-compatible path-only helper. New code should call
    `resolve_socket_route()` directly so the routing decision is
    visible alongside the path — see `SocketRoute` for the
    rationale."""
    return resolve_socket_route().path


def daemon_socket_pinned() -> bool:
    """True iff the resolver has chosen the daemon socket. The
    canonical signal for "am I running under a daemon-spawn
    agent (vs TUI-local)?". MCP tool wrappers use it to pick
    between daemon-shape and TUI-shape method names (e.g.
    `mcp_start_session` vs `start_session`).

    10f default-flip: daemon-pinned ≡ `$CM_DAEMON_SOCKET` env
    var was set (and non-empty) by the spawning TUI. The
    historical opt-in env var has no effect here.
    """
    return resolve_socket_route().chose_daemon


def resolve_tui_socket_path() -> Path:
    """Resolve the TUI control-socket path independently of the
    daemon route decision. `CM_TUI_SOCKET` wins if set (non-
    empty); otherwise fall back to `$HOME/.cm/tui.sock`. Empty
    / whitespace-only values count as unset (round-13
    invariant)."""
    tui_env = os.environ.get("CM_TUI_SOCKET", "").strip()
    if tui_env:
        return Path(tui_env)
    home = Path(os.environ.get("HOME", "/tmp"))
    return home / ".cm" / "tui.sock"


def resolve_socket_for_method(method: str) -> Path:
    """Sub-2c: route a method to the right socket.

    Methods in `DAEMON_METHODS` route to the daemon socket when
    the unified resolver chose daemon (any of:
    `CM_DAEMON_SOCKET` set, OR `CM_USE_DAEMON_SOCKET=1` with
    `~/.cm/daemon.sock` present — see `resolve_socket_route`
    for the full precedence list). Otherwise they fall back to
    `CM_TUI_SOCKET` (the TUI implements many of these methods
    too, so this is the natural fallback for TuiLocal-spawned
    agents).

    Methods NOT in `DAEMON_METHODS` always route to
    `CM_TUI_SOCKET` — they have no daemon implementation, so a
    daemon socket pin doesn't help. This is the load-bearing
    piece of the sub-2c fix: under daemon-spawn,
    `workflow_transition` / `workflow_done` route to the TUI
    socket (which `build_env` now populates alongside the
    daemon socket).

    Sub-2c review-1: socket-path resolution is delegated to
    `resolve_socket_route()` (daemon side) and
    `resolve_tui_socket_path()` (TUI side) so this function
    doesn't re-implement env-var precedence. The pre-fix
    direct-env-check missed `CM_USE_DAEMON_SOCKET=1` —
    regression of the round-13 opt-in path.

    No silent cross-routing: if a daemon method is called and
    the daemon socket is unreachable (file missing, connect
    refused), connect fails loudly (TransportError). The
    resolver does NOT fall back to TUI for daemon methods when
    daemon is chosen — that would route `mcp_start_session`
    to the TUI, which doesn't understand the daemon's minimal
    shape.
    """
    route = resolve_socket_route()
    if method in DAEMON_METHODS and route.chose_daemon:
        return route.path
    return resolve_tui_socket_path()


def caller_session_uid() -> str:
    """Pull `CM_TUI_SESSION_ID` from env. Empty if missing — callers
    should treat that as "no UID known" and let the server return
    `not_found` for any session-targeting request."""
    return os.environ.get("CM_TUI_SESSION_ID", "").strip()


def call(
    method: str,
    params: dict | None = None,
    *,
    timeout: float = 30.0,
    socket_path: Path | None = None,
):
    """Send a single request and block on the response. Returns the
    `result` value from the Response envelope on success — typically a
    dict, but `list_sessions` returns a list. The only normalization
    we do is `None` → `{}` so callers don't have to special-case methods
    whose result is intentionally absent.

    Sub-2c: the default resolver is `resolve_socket_for_method`
    — daemon-supported methods (`DAEMON_METHODS`) route to
    `CM_DAEMON_SOCKET`; everything else routes to
    `CM_TUI_SOCKET`. This lets `workflow_transition` /
    `workflow_done` reach the TUI from a daemon-spawned agent
    while `mcp_start_session` / `list_sessions` reach the
    daemon. See `resolve_socket_for_method` for the precedence
    rules and no-silent-fallback contract.

    Sub-2b-3 review-8 #2 (still load-bearing): `socket_path`
    lets the caller pre-resolve and pass through, ensuring the
    method-string choice and the actual dial bind to the same
    resolution. Useful for two-step callers in `server.py`
    that pick between daemon-shape and TUI-shape method
    names — the same `resolve_socket_route()` or
    `daemon_socket_pinned()` call drives both decisions.

    Raises:
        ControlError: the daemon/TUI returned `ok=false`.
        TransportError: socket connect/read/parse failure.
    """
    request = {
        "id": uuid.uuid4().hex,
        "caller": {"session_uid": caller_session_uid()},
        "method": method,
        "params": params or {},
    }
    body = json.dumps(request, ensure_ascii=False).encode("utf-8")
    if len(body) > 4 * 1024 * 1024:
        raise TransportError("request body exceeds 4 MiB cap")

    path = (
        socket_path
        if socket_path is not None
        else resolve_socket_for_method(method)
    )
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(timeout)
        sock.connect(str(path))
    except OSError as e:
        raise TransportError(f"connect {path}: {e}") from e

    try:
        sock.sendall(struct.pack(">I", len(body)) + body)

        # Read length prefix.
        len_bytes = _read_exact(sock, 4)
        (resp_len,) = struct.unpack(">I", len_bytes)
        if resp_len > 4 * 1024 * 1024:
            raise TransportError(f"response too large: {resp_len}")

        resp_bytes = _read_exact(sock, resp_len)
    finally:
        sock.close()

    try:
        response = json.loads(resp_bytes.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as e:
        raise TransportError(f"malformed response: {e}") from e

    if response.get("ok") is True:
        # Don't `or {}` — that collapses a legitimate empty list (e.g.
        # `list_sessions` with no sessions in scope) into a dict. Only
        # normalize a missing `result` key.
        result = response.get("result")
        return result if result is not None else {}

    err = response.get("error") or {}
    raise ControlError(
        err.get("code", "internal"),
        err.get("message", "unknown error"),
    )


def _read_exact(sock: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise TransportError(f"unexpected EOF at byte {len(buf)} of {n}")
        buf.extend(chunk)
    return bytes(buf)
