"""Claude Code JSONL parser. Mirrors `agent::claude_code::parse_lines`
in the Rust crate. The contract test in `tests/` runs the same fixture
through both parsers and asserts identical normalized output.

Schema notes (matching the Rust impl):
  - top-level `type`: "user" or "assistant"
  - `message.content`: array of items, each with a `type` of "text",
    "thinking", "tool_use", or "tool_result"
  - meta records carry `isMeta: true` and are skipped
  - user records whose `content` is a string starting with `<command-name>`
    are slash-command invocations and are skipped
  - user records whose `content` is an array of pure tool_result items
    are not real user turns and are skipped
"""

from __future__ import annotations

import json
from typing import Optional

from .types import Cursor, Message, Role, format_cursor, resolve_offset


def parse_lines(
    contents: str,
    start_offset: int,
    limit: int,
    generation: int,
) -> tuple[list[Message], str]:
    """Parse a slice of the transcript starting at `start_offset` (0-based
    line index). Advances past every line — including malformed/meta/empty
    ones — so the cursor offset is monotonic and stable across appends.
    Returns (messages, next_cursor_raw)."""
    messages: list[Message] = []
    # splitlines() (not split("\n")) so empty content yields [] rather
    # than [""] — matches Rust's `str::lines()` semantics, including
    # not yielding a trailing empty entry from a final newline.
    lines = contents.splitlines()
    next_offset = start_offset
    for i in range(start_offset, len(lines)):
        # Stop BEFORE consuming this line if we already have `limit`
        # messages — cursor points at first unprocessed line.
        if len(messages) >= limit:
            break
        next_offset = i + 1
        line = lines[i].strip()
        if not line:
            continue
        try:
            v = json.loads(line)
        except json.JSONDecodeError:
            continue
        msg = render_message(v)
        if msg is not None:
            messages.append(msg)
    return messages, format_cursor(generation, next_offset)


def read_messages(
    path: str,
    generation: int,
    since: Optional[str],
    limit: int = 20,
) -> tuple[list[Message], str]:
    """Read up to `limit` messages from the transcript file at `path`,
    starting after the cursor `since` (or from the beginning if None).
    Returns (messages, next_cursor_raw)."""
    start_offset, gen = resolve_offset(generation, since)
    try:
        with open(path, "r", encoding="utf-8") as f:
            contents = f.read()
    except (OSError, FileNotFoundError):
        return [], format_cursor(gen, start_offset)
    return parse_lines(contents, start_offset, limit, gen)


def render_message(v: dict) -> Optional[Message]:
    ty = v.get("type")
    if ty == "user":
        role = Role.USER
    elif ty == "assistant":
        role = Role.ASSISTANT
    else:
        return None

    if v.get("isMeta") is True:
        return None

    content_val = v.get("message", {}).get("content")

    if role == Role.USER:
        if isinstance(content_val, str) and content_val.lstrip().startswith(
            "<command-name>"
        ):
            return None
        if _is_pure_tool_result(content_val):
            return None

    rendered = _render_content(content_val)
    if rendered is None:
        return None
    return Message(role=role, content=rendered, ts=_extract_ts(v))


def _is_pure_tool_result(content) -> bool:
    if not isinstance(content, list) or not content:
        return False
    return all(
        isinstance(item, dict) and item.get("type") == "tool_result"
        for item in content
    )


def _render_content(content) -> Optional[str]:
    """Stringify a Claude `message.content` field. Tool-use becomes a
    one-line summary; thinking is dropped."""
    if content is None:
        return None
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return None
    parts: list[str] = []
    for item in content:
        if not isinstance(item, dict):
            continue
        t = item.get("type", "")
        if t in ("text", "output_text"):
            text = item.get("text")
            if isinstance(text, str):
                parts.append(text)
        elif t == "tool_use":
            name = item.get("name") or "?"
            summary = _tool_use_summary(item)
            line = (
                f"[tool_use: {name} ({summary})]" if summary else f"[tool_use: {name}]"
            )
            parts.append(line)
        elif t == "tool_result":
            body = item.get("content")
            if isinstance(body, str):
                parts.append(f"[tool_result: {_truncate(body, 80)}]")
            else:
                parts.append("[tool_result: <binary>]")
        # thinking → skip
    if not parts:
        return None
    return "\n".join(parts)


def _tool_use_summary(item: dict) -> Optional[str]:
    inp = item.get("input")
    if not isinstance(inp, dict):
        return None
    for k in ("command", "description", "file_path", "url", "query", "pattern"):
        v = inp.get(k)
        if isinstance(v, str):
            return f"{k}: {_truncate(v, 60)}"
    return None


def _truncate(s: str, max_len: int) -> str:
    if len(s) <= max_len:
        return s
    return s[:max_len] + "…"


def _extract_ts(v: dict) -> float:
    t = v.get("timestamp")
    if isinstance(t, (int, float)):
        return float(t)
    # Claude's ISO timestamps are not parsed — Rust impl also returns 0.0.
    return 0.0
