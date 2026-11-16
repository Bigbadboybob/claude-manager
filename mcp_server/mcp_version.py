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


# Never descended into: a deployed payload carries its virtualenv
# (/opt/cm-daemon/mcp_server/.venv is ~1,200 files), and a venv rebuild must
# neither cost every MCP start a full hash nor change the payload's version.
_SKIP_DIRS = frozenset({"tests", "__pycache__", "site-packages", "node_modules", "venv"})


def payload_files(root: Path) -> list[Path]:
    files = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if not d.startswith(".") and d not in _SKIP_DIRS]
        files += [Path(dirpath) / f for f in filenames if f.endswith(".py")]
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
        "toolset_hash": toolset_hash(root),
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

def signature(schema, defs: dict | None = None, depth: int = 0):
    """A comparable shape for one JSON schema: type, items, properties,
    required and enum, with Optional[...] collapsed and $refs resolved."""
    if not isinstance(schema, dict) or depth > 12:
        return {"type": "any"}
    defs = defs or {}
    ref = schema.get("$ref")
    if isinstance(ref, str) and ref.startswith("#/$defs/"):
        return signature(defs.get(ref.rsplit("/", 1)[1], {}), defs, depth + 1)
    variants = schema.get("anyOf") or schema.get("oneOf")
    if isinstance(variants, list):
        kept = [signature(v, defs, depth + 1) for v in variants
                if not (isinstance(v, dict) and v.get("type") == "null")]
        unique = {json.dumps(k, sort_keys=True): k for k in kept}
        if len(unique) == 1:
            return next(iter(unique.values()))
        return {"anyOf": [unique[k] for k in sorted(unique)]}
    out = {"type": schema.get("type", "any")}
    if "enum" in schema:
        out["enum"] = sorted(json.dumps(v, sort_keys=True) for v in schema["enum"])
    if isinstance(schema.get("items"), dict):
        out["items"] = signature(schema["items"], defs, depth + 1)
    if isinstance(schema.get("properties"), dict):
        out["properties"] = {k: signature(v, defs, depth + 1)
                             for k, v in sorted(schema["properties"].items())}
        out["required"] = sorted(schema.get("required") or [])
    return out


def summarize(tools) -> dict:
    """{name: {"required": [...], "params": {param: signature}}} from MCP tools."""
    out = {}
    for tool in tools:
        schema = getattr(tool, "inputSchema", None) or tool.get("inputSchema") or {}
        name = getattr(tool, "name", None) or tool["name"]
        defs = schema.get("$defs") or {}
        props = schema.get("properties") or {}
        out[name] = {
            "required": sorted(schema.get("required") or []),
            "params": {key: signature(value, defs) for key, value in sorted(props.items())},
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


def toolset_hash(root: Path = HERE) -> str | None:
    """What a running Codex thread can notice: which tools exist and what they
    require. Additive optional parameters leave it unchanged."""
    tools = read_snapshot(root).get("tools")
    if not tools:
        return None
    shape = {name: sorted(t.get("required") or []) for name, t in tools.items()}
    return schema_hash(shape)


def _sig(value):
    return {"type": value} if isinstance(value, str) else value


def _type_breaks(where: str, old, new) -> list[str]:
    """Ways a value valid under OLD can be rejected under NEW."""
    if isinstance(old, (str, list)):
        # A pre-signature snapshot recorded only the outer type(s).
        return [] if _outer(new) == old else [f"{where} changed type {old!r} -> {_outer(new)!r}"]
    old, new = _sig(old), _sig(new)
    if not isinstance(old, dict) or not isinstance(new, dict):
        return [] if old == new else [f"{where} changed type {old!r} -> {new!r}"]
    if "anyOf" in old or "anyOf" in new or old.get("type") != new.get("type"):
        if old != new:
            return [f"{where} changed type {_brief(old)!r} -> {_brief(new)!r}"]
        return []
    problems = []
    if "enum" in new:
        dropped = sorted(set(old.get("enum") or ["<any>"]) - set(new["enum"]))
        if dropped:
            problems.append(f"{where} no longer accepts {', '.join(dropped)}")
    if "items" in old or "items" in new:
        problems += _type_breaks(f"{where}[]", old.get("items", {"type": "any"}),
                                 new.get("items", {"type": "any"}))
    if "properties" in old or "properties" in new:
        before, after = old.get("properties") or {}, new.get("properties") or {}
        for key in sorted(set(new.get("required") or []) - set(old.get("required") or [])):
            problems.append(f"{where}.{key} newly required")
        for key, kind in before.items():
            if key not in after:
                problems.append(f"{where}.{key} removed")
            else:
                problems += _type_breaks(f"{where}.{key}", kind, after[key])
    return problems


def _outer(sig):
    sig = _sig(sig)
    if isinstance(sig, dict) and "anyOf" in sig:
        return sorted(_outer(v) if not isinstance(_outer(v), list) else str(_outer(v))
                      for v in sig["anyOf"])
    return sig.get("type") if isinstance(sig, dict) else sig


def _brief(sig):
    return sig.get("type") if isinstance(sig, dict) and set(sig) == {"type"} else sig


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
            else:
                problems += [f"{name}: {p}" for p in
                             _type_breaks(f"parameter {param!r}", kind, after["params"][param])]
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
