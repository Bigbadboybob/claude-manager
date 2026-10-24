"""Tool schemas stay compatible with running Codex threads.

A Codex thread keeps the tool list it started with, even after CM activates
new MCP code underneath it (doc/mcp-managed-activation.md). So against the
checked-in snapshot a change may only ADD (tools, optional parameters);
removing a tool or parameter, adding a required parameter or changing a type
must be marked with `mcp_version.py snapshot --write --allow-breaking`."""

import shutil
import tempfile
import unittest
from pathlib import Path

from mcp_server import mcp_version


class SnapshotTests(unittest.TestCase):
    def test_current_tools_match_the_checked_in_snapshot(self):
        snapshot = mcp_version.read_snapshot()
        current = mcp_version.current_summary()
        marked = {entry["change"] for entry in snapshot.get("breaking", [])
                  if entry.get("tools_hash") == mcp_version.schema_hash(current)}
        unmarked = [p for p in mcp_version.breaking_changes(snapshot["tools"], current)
                    if p not in marked]
        self.assertEqual(unmarked, [], "breaking tool-schema change; see mcp_version.py")
        self.assertEqual(
            mcp_version.schema_hash(current), mcp_version.schema_hash(snapshot["tools"]),
            "tool_schemas.json is outdated: run "
            "`python3 mcp_server/mcp_version.py snapshot --write`")


class BreakingChangeTests(unittest.TestCase):
    OLD = {
        "a": {"required": ["x"], "params": {"x": "string", "y": "integer"}},
        "b": {"required": [], "params": {}},
    }

    def test_additions_are_compatible(self):
        new = {
            "a": {"required": ["x"], "params": {"x": "string", "y": "integer", "z": "boolean"}},
            "b": {"required": [], "params": {}},
            "c": {"required": ["q"], "params": {"q": "string"}},
        }
        self.assertEqual(mcp_version.breaking_changes(self.OLD, new), [])

    def test_each_breaking_shape_is_reported(self):
        new = {"a": {"required": ["x", "y"], "params": {"x": "array", "y": "integer"}}}
        self.assertEqual(mcp_version.breaking_changes(self.OLD, new), [
            "a: new required parameter 'y'",
            "a: parameter 'x' changed type 'string' -> 'array'",
            "b: tool removed",
        ])
        dropped = {"a": {"required": ["x"], "params": {"x": "string"}}, "b": self.OLD["b"]}
        self.assertEqual(mcp_version.breaking_changes(self.OLD, dropped),
                         ["a: parameter 'y' removed"])

    def test_optional_wrappers_compare_by_inner_type(self):
        self.assertEqual(mcp_version._param_type(
            {"anyOf": [{"type": "string"}, {"type": "null"}]}), "string")


class StampTests(unittest.TestCase):
    def test_stamp_tracks_the_payload(self):
        root = Path(tempfile.mkdtemp())
        try:
            (root / "server.py").write_text("print(1)\n")
            (root / "tests").mkdir()
            (root / "tests" / "test_x.py").write_text("x\n")
            first = mcp_version.stamp(root, git="0123456789abcdef")
            self.assertEqual(mcp_version.read_stamp(root)["payload_hash"], first["payload_hash"])
            self.assertEqual(first["version"], "0123456789ab")
            (root / "tests" / "test_x.py").write_text("changed\n")  # tests are not payload
            self.assertEqual(mcp_version.payload_hash(root), first["payload_hash"])
            (root / "server.py").write_text("print(2)\n")
            self.assertNotEqual(mcp_version.payload_hash(root), first["payload_hash"])
        finally:
            shutil.rmtree(root)

    def test_unstamped_payload_reads_none(self):
        self.assertIsNone(mcp_version.read_stamp(Path(tempfile.mkdtemp())))


if __name__ == "__main__":
    unittest.main()
