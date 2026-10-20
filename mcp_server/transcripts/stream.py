"""Bounded-memory transcript reading.

Transcripts grow to hundreds of MB (long Codex sessions). Reading one whole
(`f.read()` + `splitlines()` + a Message per line) peaks at several times the
file size, and the MCP server does that work in executor threads whose glibc
arenas keep the high-water mark: one MCP server reached 7.3 GB RSS after a
day of monitor completions (2026-10-13). These helpers stream line by line
or scan backwards in bounded chunks instead.

Lines split on b"\\n" with a trailing b"\\r" stripped, which is Rust's
`str::lines()` (the cursor format's reference); `str.splitlines()` also split
on rare separators such as U+2028 inside message text.
"""

from __future__ import annotations

import json
from collections import deque
from typing import Callable, Iterator, Optional

from .types import Message, Role, format_cursor, resolve_offset

Render = Callable[[dict], Optional[Message]]

_FIRST_CHUNK = 256 * 1024
_MAX_CHUNK = 8 * 1024 * 1024


def _render_line(raw: bytes, render: Render) -> Optional[Message]:
    line = raw.strip()
    if not line:
        return None
    try:
        value = json.loads(line)
    except ValueError:  # includes UnicodeDecodeError
        return None
    if not isinstance(value, dict):
        return None
    return render(value)


def read_messages(
    path: str,
    generation: int,
    since: Optional[str],
    limit: int,
    render: Render,
) -> tuple[list[Message], str]:
    """Up to `limit` messages after the `since` cursor, streaming: memory is
    one line plus the returned messages, never the whole file. The cursor
    advances past every consumed line (empty, malformed and meta included)."""
    start, gen = resolve_offset(generation, since)
    messages: list[Message] = []
    next_offset = start
    try:
        with open(path, "rb") as f:
            for index, raw in enumerate(f):
                if index < start:
                    continue
                if len(messages) >= limit:
                    break
                next_offset = index + 1
                message = _render_line(raw, render)
                if message is not None:
                    messages.append(message)
    except OSError:
        return [], format_cursor(gen, start)
    return messages, format_cursor(gen, next_offset)


def _lines_backward(path: str) -> Iterator[bytes]:
    """Lines from the end of the file towards the start, read in chunks that
    grow from 256 KiB to 8 MiB. Memory is one chunk plus one partial line."""
    with open(path, "rb") as f:
        end = f.seek(0, 2)
        chunk = _FIRST_CHUNK
        carry = b""
        while end > 0:
            start = max(0, end - chunk)
            f.seek(start)
            data = f.read(end - start) + carry
            lines = data.split(b"\n")
            # The first piece may continue in the previous chunk.
            carry = lines.pop(0) if start > 0 else b""
            for raw in reversed(lines):
                yield raw
            end = start
            chunk = min(chunk * 2, _MAX_CHUNK)
        if carry:
            yield carry


def last_assistant(path: Optional[str], render: Render) -> Optional[dict]:
    """The last assistant message with visible text, as a dict, found by
    scanning backwards from the end of the file. None when there is none or
    the file cannot be read."""
    if not path:
        return None
    try:
        for raw in _lines_backward(path):
            message = _render_line(raw, render)
            if message is not None and message.role == Role.ASSISTANT and message.content.strip():
                return message.to_dict()
    except OSError:
        return None
    return None


def line_count(path: str) -> int:
    """Number of lines as the cursor counts them (a final line without a
    newline counts), streaming."""
    count = 0
    last = b"\n"
    with open(path, "rb") as f:
        while True:
            block = f.read(1024 * 1024)
            if not block:
                break
            count += block.count(b"\n")
            last = block[-1:]
    return count + (0 if last == b"\n" else 1)


def end_cursor(path: Optional[str], generation: int) -> str:
    """A cursor positioned after the last line (what a full read returns)."""
    try:
        return format_cursor(generation, line_count(path) if path else 0)
    except OSError:
        return format_cursor(generation, 0)


def tail(path: str, generation: int, n: int, render: Render) -> tuple[list[Message], Optional[dict], str]:
    """(the last `n` rendered messages, the last assistant message, the end
    cursor) with bounded memory: what `read_messages(... everything)` then
    `messages[-n:]` gave, without materialising the whole transcript."""
    recent: deque[Message] = deque()
    assistant: Optional[dict] = None
    try:
        for raw in _lines_backward(path):
            if len(recent) >= n and assistant is not None:
                break
            message = _render_line(raw, render)
            if message is None:
                continue
            if len(recent) < n:
                recent.appendleft(message)
            if assistant is None and message.role == Role.ASSISTANT and message.content.strip():
                assistant = message.to_dict()
    except OSError:
        return [], None, format_cursor(generation, 0)
    return list(recent), assistant, end_cursor(path, generation)
