"""Shared types for transcript parsers. Mirrors `agent::{Cursor, Message,
Role}` in the Rust crate."""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import Optional


class Role(str, Enum):
    USER = "user"
    ASSISTANT = "assistant"
    TOOL = "tool"


@dataclass
class Message:
    role: Role
    content: str
    ts: float = 0.0

    def to_dict(self) -> dict:
        return {"role": self.role.value, "content": self.content, "ts": self.ts}


@dataclass
class Cursor:
    """Opaque cursor. Wire format: `v1:<generation>:<offset>`. Parsed
    back via `parse_cursor`."""

    generation: int
    offset: int

    @property
    def raw(self) -> str:
        return f"v1:{self.generation}:{self.offset}"


def parse_cursor(raw: str) -> Optional[tuple[int, int]]:
    """Parse a cursor's wire form. Returns (generation, offset) or None
    on malformed input. Callers treat None as "start from offset 0" so
    a stale cursor self-corrects on the next read.

    Matches the Rust parser: rejects negative components. Without this,
    a malformed cursor with a negative offset would produce a Python
    list-from-end slice (or IndexError on read) instead of the intended
    "treat as malformed → restart at 0" behavior.
    """
    parts = raw.split(":", 2)
    if len(parts) != 3 or parts[0] != "v1":
        return None
    try:
        gen = int(parts[1])
        off = int(parts[2])
    except ValueError:
        return None
    if gen < 0 or off < 0:
        return None
    return gen, off


def format_cursor(generation: int, offset: int) -> str:
    return f"v1:{generation}:{offset}"


def resolve_offset(
    current_generation: int, since_raw: Optional[str]
) -> tuple[int, int]:
    """Resolve a `since` cursor against the session's current generation.
    Returns (effective_offset, current_generation). On generation
    mismatch (or missing/malformed cursor), restart at offset 0."""
    if not since_raw:
        return 0, current_generation
    parsed = parse_cursor(since_raw)
    if parsed is None:
        return 0, current_generation
    gen, off = parsed
    if gen != current_generation:
        return 0, current_generation
    return off, current_generation
