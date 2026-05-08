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
    """Resolve the socket path. Prefer `CM_TUI_SOCKET`; fall back to the
    `~/.cm/tui.sock` default the TUI uses."""
    env = os.environ.get("CM_TUI_SOCKET", "").strip()
    if env:
        return Path(env)
    home = os.environ.get("HOME", "/tmp")
    return Path(home) / ".cm" / "tui.sock"


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
