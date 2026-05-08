"""Contract tests for the Python transcript parsers. Run with:
    python -m unittest mcp_server.tests.test_transcripts

These tests exercise the same fixture transcripts the Rust agent unit
tests cover, asserting identical normalized output. Schema changes that
break parity here are exactly the kind of bug the contract test exists
to catch.
"""

from __future__ import annotations

import unittest

from mcp_server.transcripts import claude_code, codex
from mcp_server.transcripts.types import Role, parse_cursor, format_cursor, resolve_offset


class CursorRoundTripTest(unittest.TestCase):
    def test_parse_round_trips(self):
        self.assertEqual(parse_cursor(format_cursor(7, 42)), (7, 42))

    def test_zero(self):
        self.assertEqual(parse_cursor("v1:0:0"), (0, 0))

    def test_rejects_wrong_version(self):
        self.assertIsNone(parse_cursor("v2:1:1"))

    def test_rejects_malformed(self):
        for bad in ("", "v1", "v1:", "v1:abc:1", "v1:1:abc", "garbage"):
            self.assertIsNone(parse_cursor(bad), f"should reject {bad!r}")

    def test_rejects_negative_components(self):
        # Without rejection, Python's `int(...)` happily parses "-1" and
        # the parser would index from the end of the line list (or
        # IndexError on read). The Rust parser refuses negative values
        # at parse time via u64/usize; mirror that.
        for bad in ("v1:-1:0", "v1:0:-1", "v1:-5:-3"):
            self.assertIsNone(
                parse_cursor(bad),
                f"should reject negative-component cursor {bad!r}",
            )


class ResolveOffsetTest(unittest.TestCase):
    def test_none_starts_at_zero(self):
        self.assertEqual(resolve_offset(5, None), (0, 5))

    def test_matching_generation_keeps_offset(self):
        self.assertEqual(resolve_offset(5, "v1:5:17"), (17, 5))

    def test_generation_mismatch_restarts(self):
        # Cursor was issued under gen=3 (pre-/clear); current is gen=5.
        self.assertEqual(resolve_offset(5, "v1:3:17"), (0, 5))

    def test_malformed_cursor_restarts(self):
        self.assertEqual(resolve_offset(2, "garbage"), (0, 2))


class ClaudeCodeParserTest(unittest.TestCase):
    def test_empty_yields_no_messages(self):
        msgs, cur = claude_code.parse_lines("", 0, 100, 0)
        self.assertEqual(msgs, [])
        self.assertEqual(parse_cursor(cur), (0, 0))

    def test_single_user_turn(self):
        line = '{"type":"user","message":{"role":"user","content":"hi"}}'
        msgs, _ = claude_code.parse_lines(line, 0, 100, 0)
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0].role, Role.USER)
        self.assertEqual(msgs[0].content, "hi")

    def test_single_assistant_turn(self):
        line = (
            '{"type":"assistant","message":{"role":"assistant",'
            '"content":[{"type":"text","text":"hello"}]}}'
        )
        msgs, _ = claude_code.parse_lines(line, 0, 100, 0)
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0].role, Role.ASSISTANT)
        self.assertEqual(msgs[0].content, "hello")

    def test_tool_use_renders_one_liner(self):
        line = (
            '{"type":"assistant","message":{"role":"assistant",'
            '"content":[{"type":"tool_use","name":"Bash",'
            '"input":{"command":"ls -la"}}]}}'
        )
        msgs, _ = claude_code.parse_lines(line, 0, 100, 0)
        self.assertEqual(len(msgs), 1)
        self.assertTrue(msgs[0].content.startswith("[tool_use: Bash"))
        self.assertIn("command: ls -la", msgs[0].content)

    def test_thinking_only_drops_silently(self):
        line = (
            '{"type":"assistant","message":{"role":"assistant",'
            '"content":[{"type":"thinking","thinking":"..."}]}}'
        )
        msgs, cur = claude_code.parse_lines(line, 0, 100, 0)
        self.assertEqual(msgs, [])
        # Line was still consumed.
        self.assertEqual(parse_cursor(cur), (0, 1))

    def test_multi_turn_preserves_order(self):
        lines = [
            '{"type":"user","message":{"role":"user","content":"a"}}',
            '{"type":"assistant","message":{"role":"assistant",'
            '"content":[{"type":"text","text":"b"}]}}',
            '{"type":"user","message":{"role":"user","content":"c"}}',
        ]
        msgs, _ = claude_code.parse_lines("\n".join(lines), 0, 100, 0)
        self.assertEqual([m.content for m in msgs], ["a", "b", "c"])

    def test_meta_records_skipped(self):
        lines = [
            '{"type":"user","isMeta":true,"message":'
            '{"role":"user","content":"<local-command-caveat>..."}}',
            '{"type":"user","message":{"role":"user","content":"real"}}',
        ]
        msgs, _ = claude_code.parse_lines("\n".join(lines), 0, 100, 0)
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0].content, "real")

    def test_slash_command_record_skipped(self):
        lines = [
            '{"type":"user","message":{"role":"user","content":'
            '"<command-name>/clear</command-name>"}}',
            '{"type":"user","message":{"role":"user","content":"real prompt"}}',
        ]
        msgs, _ = claude_code.parse_lines("\n".join(lines), 0, 100, 0)
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0].content, "real prompt")

    def test_pure_tool_result_user_skipped(self):
        line = (
            '{"type":"user","message":{"role":"user","content":'
            '[{"type":"tool_result","tool_use_id":"x","content":"ok"}]}}'
        )
        msgs, _ = claude_code.parse_lines(line, 0, 100, 0)
        self.assertEqual(msgs, [])

    def test_malformed_line_skipped_offset_advances(self):
        lines = [
            "{not valid json",
            '{"type":"user","message":{"role":"user","content":"first"}}',
            "another garbage line",
            '{"type":"user","message":{"role":"user","content":"second"}}',
        ]
        msgs, cur = claude_code.parse_lines("\n".join(lines), 0, 100, 0)
        self.assertEqual([m.content for m in msgs], ["first", "second"])
        self.assertEqual(parse_cursor(cur), (0, 4))

    def test_cursor_advances(self):
        lines = [
            '{"type":"assistant","message":{"role":"assistant",'
            '"content":[{"type":"text","text":"a"}]}}',
            '{"type":"assistant","message":{"role":"assistant",'
            '"content":[{"type":"text","text":"b"}]}}',
            '{"type":"assistant","message":{"role":"assistant",'
            '"content":[{"type":"text","text":"c"}]}}',
        ]
        content = "\n".join(lines)
        m1, c1 = claude_code.parse_lines(content, 0, 2, 0)
        self.assertEqual(len(m1), 2)
        _, off = parse_cursor(c1)
        m2, _ = claude_code.parse_lines(content, off, 100, 0)
        self.assertEqual(len(m2), 1)
        self.assertEqual(m2[0].content, "c")

    def test_limit_zero_returns_no_messages(self):
        # Same regression case the Rust suite covers.
        line = '{"type":"user","message":{"role":"user","content":"hi"}}'
        msgs, cur = claude_code.parse_lines(line, 0, 0, 0)
        self.assertEqual(msgs, [])
        self.assertEqual(parse_cursor(cur), (0, 0))


class CodexParserTest(unittest.TestCase):
    def test_empty_yields_no_messages(self):
        msgs, cur = codex.parse_lines("", 0, 100, 0)
        self.assertEqual(msgs, [])
        self.assertEqual(parse_cursor(cur), (0, 0))

    def test_response_item_assistant(self):
        line = (
            '{"type":"response_item","payload":{"type":"message",'
            '"role":"assistant","content":[{"type":"output_text","text":"hi"}]}}'
        )
        msgs, _ = codex.parse_lines(line, 0, 100, 0)
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0].role, Role.ASSISTANT)
        self.assertEqual(msgs[0].content, "hi")

    def test_user_input_text(self):
        line = (
            '{"type":"response_item","payload":{"role":"user",'
            '"content":[{"type":"input_text","text":"do it"}]}}'
        )
        msgs, _ = codex.parse_lines(line, 0, 100, 0)
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0].role, Role.USER)
        self.assertEqual(msgs[0].content, "do it")

    def test_function_call_renders_as_tool(self):
        line = '{"type":"response_item","payload":{"type":"function_call","name":"shell"}}'
        msgs, _ = codex.parse_lines(line, 0, 100, 0)
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0].role, Role.TOOL)
        self.assertIn("tool_use: shell", msgs[0].content)

    def test_event_msg_lifecycle_filtered(self):
        line = '{"type":"event_msg","payload":{"type":"task_complete"}}'
        msgs, _ = codex.parse_lines(line, 0, 100, 0)
        self.assertEqual(msgs, [])

    def test_event_msg_agent_message_dropped_as_mirror(self):
        # The empirical dedup case: agent_message mirrors response_item
        # 1:1, so the parser must drop it.
        line = (
            '{"type":"event_msg","payload":{"type":"agent_message",'
            '"message":"all done"}}'
        )
        msgs, _ = codex.parse_lines(line, 0, 100, 0)
        self.assertEqual(msgs, [])

    def test_paired_records_emit_one_message(self):
        lines = [
            '{"type":"event_msg","payload":{"type":"agent_message",'
            '"message":"final answer"}}',
            '{"type":"response_item","payload":{"type":"message",'
            '"role":"assistant","content":[{"type":"output_text",'
            '"text":"final answer"}]}}',
        ]
        msgs, _ = codex.parse_lines("\n".join(lines), 0, 100, 0)
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0].content, "final answer")

    def test_limit_zero(self):
        line = (
            '{"type":"response_item","payload":{"role":"user",'
            '"content":[{"type":"input_text","text":"hi"}]}}'
        )
        msgs, cur = codex.parse_lines(line, 0, 0, 0)
        self.assertEqual(msgs, [])
        self.assertEqual(parse_cursor(cur), (0, 0))


if __name__ == "__main__":
    unittest.main()
