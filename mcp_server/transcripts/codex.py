"""Codex JSONL parser. Mirrors `agent::codex::parse_lines` in the Rust
crate, including the empirically-verified dedup of `event_msg/agent_message`
records (they mirror later `response_item` assistant messages 1:1)."""

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
    messages: list[Message] = []
    # splitlines() to match Rust `str::lines()` semantics — empty
    # content yields [], not [""].
    lines = contents.splitlines()
    next_offset = start_offset
    for i in range(start_offset, len(lines)):
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
    start_offset, gen = resolve_offset(generation, since)
    try:
        with open(path, "r", encoding="utf-8") as f:
            contents = f.read()
    except (OSError, FileNotFoundError):
        return [], format_cursor(gen, start_offset)
    return parse_lines(contents, start_offset, limit, gen)


def render_message(v: dict) -> Optional[Message]:
    top_type = v.get("type", "")
    payload = v.get("payload") or {}
    payload_type = payload.get("type", "") if isinstance(payload, dict) else ""

    # function_call / function_call_output render as Tool one-liners.
    if top_type == "response_item" and payload_type in (
        "function_call",
        "function_call_output",
    ):
        name = payload.get("name") if isinstance(payload, dict) else None
        return Message(
            role=Role.TOOL,
            content=f"[tool_use: {name or '?'}]",
            ts=_extract_ts(v),
        )

    # event_msg/agent_message is intentionally SKIPPED — see the Rust
    # impl for the empirical dedup justification (mirrors response_item
    # assistant 1:1, surfacing both would double-count turns).
    if top_type == "event_msg" and payload_type == "agent_message":
        return None

    role_str = None
    if isinstance(payload, dict):
        role_str = payload.get("role")
    if role_str is None:
        role_str = v.get("role")
    if role_str == "user":
        role = Role.USER
    elif role_str == "assistant":
        role = Role.ASSISTANT
    else:
        return None

    content_val = None
    if isinstance(payload, dict):
        content_val = payload.get("content")
    if content_val is None:
        content_val = v.get("content")
    rendered = _render_content(content_val)
    if rendered is None:
        return None
    return Message(role=role, content=rendered, ts=_extract_ts(v))


def _render_content(content) -> Optional[str]:
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
        if t in ("text", "output_text", "input_text"):
            text = item.get("text")
            if isinstance(text, str):
                parts.append(text)
    if not parts:
        return None
    return "\n".join(parts)


def _extract_ts(v: dict) -> float:
    t = v.get("timestamp")
    if isinstance(t, (int, float)):
        return float(t)
    return 0.0
