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
        self.assertEqual(mcp_version.signature(
            {"anyOf": [{"type": "string"}, {"type": "null"}]}), {"type": "string"})


def tool(param_schema, required=()):
    return mcp_version.summarize([{"name": "t", "inputSchema": {
        "type": "object", "properties": {"p": param_schema}, "required": list(required),
        "$defs": {"Item": {"type": "object", "properties": {"id": {"type": "string"}},
                           "required": ["id"]}}}}])


class NestedSchemaTests(unittest.TestCase):
    def breaks(self, old, new):
        return mcp_version.breaking_changes(tool(old), tool(new))

    def test_nested_breaking_shapes(self):
        cases = [
            ({"type": "array", "items": {"type": "string"}},
             {"type": "array", "items": {"type": "integer"}}, "parameter 'p'[] changed type"),
            ({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "string"}}},
             {"type": "object", "properties": {"a": {"type": "string"}}}, "parameter 'p'.b removed"),
            ({"type": "object", "properties": {"a": {"type": "string"}}},
             {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]},
             "parameter 'p'.a newly required"),
            ({"type": "string", "enum": ["x", "y"]}, {"type": "string", "enum": ["x"]},
             "no longer accepts \"y\""),
            ({"type": "string"}, {"type": "string", "enum": ["x"]}, "no longer accepts"),
            ({"type": "array", "items": {"$ref": "#/$defs/Item"}},
             {"type": "array", "items": {"type": "object", "properties": {"id": {"type": "integer"}}}},
             "parameter 'p'[].id changed type"),
        ]
        for old, new, expected in cases:
            with self.subTest(expected=expected):
                problems = self.breaks(old, new)
                self.assertTrue(any(expected in p for p in problems), problems)

    def test_nested_additions_are_compatible(self):
        old = {"type": "object", "properties": {"a": {"type": "string", "enum": ["x"]}},
               "required": ["a"]}
        new = {"type": "object", "properties": {"a": {"type": "string", "enum": ["x", "z"]},
                                                "b": {"type": "integer"}}, "required": ["a"]}
        self.assertEqual(self.breaks(old, new), [])
        self.assertEqual(self.breaks({"type": "array", "items": {"$ref": "#/$defs/Item"}},
                                     {"type": "array", "items": {"$ref": "#/$defs/Item"}}), [])

    def test_the_toolset_hash_ignores_optional_parameters(self):
        root = Path(tempfile.mkdtemp())
        try:
            def write(tools):
                (root / mcp_version.SNAPSHOT).write_text(__import__("json").dumps({"tools": tools}))
                return mcp_version.toolset_hash(root)
            base = write({"t": {"required": ["a"], "params": {"a": {"type": "string"}}}})
            optional = write({"t": {"required": ["a"], "params": {"a": {"type": "string"},
                                                                   "b": {"type": "string"}}}})
            self.assertEqual(base, optional)
            self.assertNotEqual(base, write({"t": {"required": ["a"], "params": {}},
                                             "u": {"required": [], "params": {}}}))
            self.assertNotEqual(base, write({"t": {"required": ["a", "b"], "params": {}}}))
        finally:
            shutil.rmtree(root)


class PayloadWalkTests(unittest.TestCase):
    def test_virtualenvs_and_dot_dirs_are_not_payload(self):
        root = Path(tempfile.mkdtemp())
        try:
            (root / "server.py").write_text("x = 1\n")
            for junk in (".venv/lib/python3.12/site-packages/m.py", "venv/m.py",
                         "lib/site-packages/m.py", ".git/hooks/h.py", "__pycache__/c.py"):
                (root / junk).parent.mkdir(parents=True, exist_ok=True)
                (root / junk).write_text("y = 2\n")
            (root / "hooks").mkdir()
            (root / "hooks" / "cm_stop_hook.py").write_text("z = 3\n")
            names = [str(p.relative_to(root)) for p in mcp_version.payload_files(root)]
            self.assertEqual(names, ["hooks/cm_stop_hook.py", "server.py"])
            before = mcp_version.payload_hash(root)
            (root / ".venv/lib/python3.12/site-packages/m.py").write_text("rebuilt\n")
            self.assertEqual(mcp_version.payload_hash(root), before, "a venv rebuild is not a new version")
        finally:
            shutil.rmtree(root)


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
