"""Python transcript parsers mirroring the Rust `agent::{claude_code,codex}`
implementations. Used by the `read_session_output` MCP tool: after
`resolve_authorized_session` returns a path, the MCP server reads the
JSONL file directly and parses with these.

Cursor format matches Rust exactly: `v1:<generation>:<offset>`. The
agent (Rust) and reader (Python) share the wire encoding so a cursor
issued by one side resolves correctly on the other.
"""

from .types import Cursor, Message, Role, parse_cursor, format_cursor
from . import claude_code, codex

__all__ = [
    "Cursor",
    "Message",
    "Role",
    "claude_code",
    "codex",
    "parse_cursor",
    "format_cursor",
]
