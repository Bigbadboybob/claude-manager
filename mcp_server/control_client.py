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

**Phase-1 transitional note** (doc/persistent-host-daemon.md): the
default is *still* `~/.cm/tui.sock` because the daemon scaffold
currently closes incoming connections — it doesn't yet dispatch MCP
RPCs. The default flips to `~/.cm/daemon.sock` once daemon-side
dispatch lands (the deferred half of slice 4). Set
`CM_USE_DAEMON_SOCKET=1` to opt into the daemon-socket-preferred
resolution during integration work; this lets developers exercise the
new path without breaking everyone else's MCP calls.
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


def resolve_socket_route() -> SocketRoute:
    """Resolve the control-socket path AND whether the daemon was
    selected as the target.

    Resolution order (matches `default_socket_path()`'s legacy
    sequence, with `chose_daemon` set as appropriate at each
    branch):

      1. `$CM_DAEMON_SOCKET` env var. Explicit daemon pin.
         Injected by the TUI when it spawned this agent with
         `CM_USE_DAEMON_SOCKET=1`. `chose_daemon=True`.
      2. `$CM_TUI_SOCKET` env var. Explicit TUI pin.
         `chose_daemon=False`.
      3. `CM_USE_DAEMON_SOCKET=1` opt-in: if `~/.cm/daemon.sock`
         exists, route to the daemon. `chose_daemon=True`.
      4. Fallback to `~/.cm/tui.sock`. `chose_daemon=False`.

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
    if os.environ.get("CM_USE_DAEMON_SOCKET", "").strip() == "1":
        daemon_sock = home / ".cm" / "daemon.sock"
        if daemon_sock.exists():
            return SocketRoute(daemon_sock, chose_daemon=True)
    return SocketRoute(home / ".cm" / "tui.sock", chose_daemon=False)


def default_socket_path() -> Path:
    """Backward-compatible path-only helper. New code should call
    `resolve_socket_route()` directly so the routing decision is
    visible alongside the path — see `SocketRoute` for the
    rationale."""
    return resolve_socket_route().path


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

    Sub-2b-3 review-8 #2: `socket_path` lets the caller pass a
    pre-resolved path so the method-string selection (made via
    `resolve_socket_route().chose_daemon`) and the actual socket
    dial bind to the SAME resolution. Pre-fix, `server.py`
    resolved the route once to pick `mcp_start_session` vs
    `start_session`, then `call()` independently re-resolved
    the path — a daemon socket appearing or disappearing
    between the two resolutions would route the wrong method
    to the wrong server. Two-step callers should always pass
    `socket_path` from the same `resolve_socket_route()` they
    consulted for method selection.

    Callers that don't care about routing (single-method
    helpers, ad-hoc CLI usage) can omit `socket_path` and the
    function resolves itself.

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

    path = socket_path if socket_path is not None else default_socket_path()
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
