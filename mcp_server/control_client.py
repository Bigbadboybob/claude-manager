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


def default_socket_path() -> Path:
    """Resolve the control socket path.

    Resolution order:
      1. `$CM_DAEMON_SOCKET` env var. Injected by the TUI when it
         spawned this agent with `CM_USE_DAEMON_SOCKET=1`. When
         present, this is the authoritative pin — the agent talks
         to the daemon socket, not the TUI's.
      2. `$CM_TUI_SOCKET` env var. The current default injection by
         the TUI's MCP-config builder. Daemon-spawned agents always
         get exactly one of these two pins, so the resolver never
         sees both.
      3. `~/.cm/tui.sock` — fallback for ad-hoc callers running
         outside any env injection. The daemon binds
         `~/.cm/daemon.sock` but doesn't yet dispatch MCP RPCs (it
         closes incoming connections), so defaulting to it would
         break every MCP call.

    Opt-in development override (for ad-hoc callers without env
    injection):
      `CM_USE_DAEMON_SOCKET=1` prefers `~/.cm/daemon.sock` when it
      exists, falling back to `tui.sock` if absent. Used to exercise
      the daemon path during integration work. Once daemon-side
      dispatch is wired (deferred half of slice 4 of
      doc/persistent-host-daemon.md), the default flips and this
      opt-in becomes a no-op.

    Returns the resolved path unconditionally — connectivity is the
    caller's problem; this function just resolves where to dial.
    """
    # An explicit daemon-socket pin trumps everything: the TUI
    # already decided this agent talks to the daemon, no
    # filesystem probes needed.
    daemon_env = os.environ.get("CM_DAEMON_SOCKET", "").strip()
    if daemon_env:
        return Path(daemon_env)
    tui_env = os.environ.get("CM_TUI_SOCKET", "").strip()
    if tui_env:
        return Path(tui_env)
    home = Path(os.environ.get("HOME", "/tmp"))
    if os.environ.get("CM_USE_DAEMON_SOCKET", "").strip() == "1":
        daemon_sock = home / ".cm" / "daemon.sock"
        if daemon_sock.exists():
            return daemon_sock
    return home / ".cm" / "tui.sock"


def caller_session_uid() -> str:
    """Pull `CM_TUI_SESSION_ID` from env. Empty if missing — callers
    should treat that as "no UID known" and let the server return
    `not_found` for any session-targeting request."""
    return os.environ.get("CM_TUI_SESSION_ID", "").strip()


def call(method: str, params: dict | None = None, *, timeout: float = 30.0):
    """Send a single request and block on the response. Returns the
    `result` value from the Response envelope on success — typically a
    dict, but `list_sessions` returns a list. The only normalization
    we do is `None` → `{}` so callers don't have to special-case methods
    whose result is intentionally absent.

    Raises:
        ControlError: the TUI returned `ok=false`.
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

    path = default_socket_path()
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
