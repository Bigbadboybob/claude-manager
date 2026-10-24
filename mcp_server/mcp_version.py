"""MCP payload versions and tool-schema compatibility (doc/mcp-managed-activation.md).

A deploy copies the `mcp_server/` payload, then runs `mcp_version.py stamp`
ON THE TARGET, which writes `VERSION` last (atomically). Backends compare
the payload they loaded with that stamp to decide that an update is pending.
An unstamped checkout (any dev worktree) never activates anything.

`tool_schemas.json` is the checked-in summary of every tool's input schema.
Running Codex threads keep the tool list they started with, so a release
must not remove a tool, add a required parameter, or change a parameter's
type unless the change is marked (`snapshot --write --allow-breaking`).
`tests/test_tool_schema_compat.py` enforces that, and the snapshot's hash
tells status.json whether a session's tool set changed.

Standard library only, so the stamp step needs no virtualenv.
Usage:
  mcp_version.py stamp [--git SHA] [--dir DIR]
  mcp_version.py snapshot [--write] [--allow-breaking "tool: reason" ...]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
STAMP = "VERSION"
SNAPSHOT = "tool_schemas.json"
_EXTRA_FILES = ("AGENT_GUIDE.md", SNAPSHOT, "requirements.txt")


def payload_files(root: Path) -> list[Path]:
    files = [p for p in root.rglob("*.py")
             if "tests" not in p.relative_to(root).parts
             and "__pycache__" not in p.parts]
    files += [root / name for name in _EXTRA_FILES if (root / name).is_file()]
    return sorted(set(files))


def payload_hash(root: Path = HERE) -> str:
    digest = hashlib.sha256()
    for path in payload_files(root):
        digest.update(str(path.relative_to(root)).encode() + b"\0")
        digest.update(path.read_bytes() + b"\0")
    return digest.hexdigest()[:16]


def _atomic_json(path: Path, value: dict) -> None:
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream, indent=1, sort_keys=True)
            stream.write("\n")
        os.chmod(tmp, 0o644)
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def stamp(root: Path = HERE, git: str | None = None) -> dict:
    value = {
        "payload_hash": payload_hash(root),
        "tools_hash": snapshot_hash(root),
        "git": git,
        "stamped_at": time.time(),
    }
    value["version"] = (git[:12] if git else value["payload_hash"])
    _atomic_json(root / STAMP, value)
    return value


def read_stamp(root: Path = HERE) -> dict | None:
    try:
        value = json.loads((root / STAMP).read_text())
    except (OSError, ValueError):
        return None
    return value if isinstance(value, dict) and value.get("payload_hash") else None


# ---- tool schemas ---------------------------------------------------------

def _param_type(schema: dict):
    """A comparable type for one parameter (anyOf/Optional collapsed)."""
    if "type" in schema:
        return schema["type"]
    variants = schema.get("anyOf") or schema.get("oneOf")
    if isinstance(variants, list):
        kinds = sorted({json.dumps(_param_type(v), sort_keys=True)
                        for v in variants if v.get("type") != "null"})
        return [json.loads(k) for k in kinds] if len(kinds) != 1 else json.loads(kinds[0])
    return "any"


def summarize(tools) -> dict:
    """{name: {"required": [...], "params": {param: type}}} from MCP tools."""
    out = {}
    for tool in tools:
        schema = getattr(tool, "inputSchema", None) or tool.get("inputSchema") or {}
        name = getattr(tool, "name", None) or tool["name"]
        props = schema.get("properties") or {}
        out[name] = {
            "required": sorted(schema.get("required") or []),
            "params": {key: _param_type(value) for key, value in sorted(props.items())},
        }
    return dict(sorted(out.items()))


def schema_hash(summary: dict) -> str:
    return hashlib.sha256(json.dumps(summary, sort_keys=True).encode()).hexdigest()[:16]


def read_snapshot(root: Path = HERE) -> dict:
    try:
        return json.loads((root / SNAPSHOT).read_text())
    except (OSError, ValueError):
        return {"tools": {}, "breaking": []}


def snapshot_hash(root: Path = HERE) -> str | None:
    tools = read_snapshot(root).get("tools")
    return schema_hash(tools) if tools else None


def breaking_changes(old: dict, new: dict) -> list[str]:
    """Changes that break a running Codex thread holding the OLD schema."""
    problems = []
    for name, before in old.items():
        after = new.get(name)
        if after is None:
            problems.append(f"{name}: tool removed")
            continue
        for param in sorted(set(after["required"]) - set(before["required"])):
            problems.append(f"{name}: new required parameter {param!r}")
        for param, kind in before["params"].items():
            if param not in after["params"]:
                problems.append(f"{name}: parameter {param!r} removed")
            elif after["params"][param] != kind:
                problems.append(f"{name}: parameter {param!r} changed type "
                                f"{kind!r} -> {after['params'][param]!r}")
    return problems


def current_summary() -> dict:
    sys.path.insert(0, str(HERE.parent))
    import asyncio
    from mcp_server import server
    return summarize(asyncio.run(server.mcp.list_tools()))


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    st = sub.add_parser("stamp", help="write VERSION for this payload (run on the target, last)")
    st.add_argument("--git")
    st.add_argument("--dir", type=Path, default=HERE)
    sn = sub.add_parser("snapshot", help="check or rewrite tool_schemas.json")
    sn.add_argument("--write", action="store_true")
    sn.add_argument("--allow-breaking", action="append", default=[],
                    help='"tool: reason" for each intended breaking change')
    args = parser.parse_args(argv)
    if args.cmd == "stamp":
        print(json.dumps(stamp(args.dir, args.git), sort_keys=True))
        return 0
    snapshot = read_snapshot()
    new = current_summary()
    problems = breaking_changes(snapshot.get("tools", {}), new)
    allowed = {entry.split(":", 1)[0].strip() for entry in args.allow_breaking}
    blocking = [p for p in problems if p.split(":", 1)[0] not in allowed]
    if blocking:
        print("breaking tool-schema changes (running Codex threads keep the old schema):",
              *blocking, sep="\n  ", file=sys.stderr)
        print('mark intended ones with --allow-breaking "<tool>: <reason>"', file=sys.stderr)
        return 1
    if not args.write:
        current = schema_hash(new) == schema_hash(snapshot.get("tools", {}))
        print("snapshot current" if current else "snapshot outdated; rerun with --write")
        return 0 if current else 1
    breaking = snapshot.get("breaking", [])
    breaking += [{"change": p, "reason": next(e for e in args.allow_breaking
                                               if e.split(":", 1)[0].strip() == p.split(":", 1)[0]),
                  "tools_hash": schema_hash(new)} for p in problems]
    _atomic_json(HERE / SNAPSHOT, {"tools": new, "breaking": breaking})
    print(f"wrote {SNAPSHOT}: {len(new)} tools, hash {schema_hash(new)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
