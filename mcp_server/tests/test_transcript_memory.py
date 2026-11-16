"""Transcript reads stay bounded in memory and match the old full parse.

2026-10-13: an MCP server reached 7.3 GB RSS after a day. Each monitor
completion (and read_last_turn / the wait tools) parsed the whole transcript
(`f.read()` + `splitlines()` + a Message per line) in an executor thread just
to find the last assistant message; long Codex transcripts are hundreds of MB
and every worker thread's glibc arena kept its high-water mark.
"""

from __future__ import annotations

import asyncio
import json
import tempfile
import tracemalloc
import unittest
from pathlib import Path

from mcp_server import monitor
from mcp_server.transcripts import claude_code, codex, stream


def codex_line(role: str, text: str) -> str:
    kind = "output_text" if role == "assistant" else "input_text"
    return json.dumps({"type": "response_item", "payload": {
        "type": "message", "role": role, "content": [{"type": kind, "text": text}]}})


def tool_line(size: int) -> str:
    return json.dumps({"type": "response_item", "payload": {
        "type": "function_call_output", "output": "o" * size}})


def claude_line(role: str, text: str) -> str:
    return json.dumps({"type": role, "message": {"role": role, "content": [{"type": "text", "text": text}]}})


def old_full_read(parser, path: str, generation: int, since=None, limit=100_000_000):
    """The pre-fix implementation: whole file, splitlines, parse."""
    with open(path, "r", encoding="utf-8") as f:
        contents = f.read()
    start, gen = parser.resolve_offset(generation, since)
    return parser.parse_lines(contents, start, limit, gen)


class Fixture:
    def __init__(self, lines: list[str], trailing_newline: bool = True):
        self.dir = tempfile.TemporaryDirectory()
        self.path = str(Path(self.dir.name) / "t.jsonl")
        text = "\n".join(lines) + ("\n" if trailing_newline else "")
        Path(self.path).write_text(text, encoding="utf-8")

    def close(self):
        self.dir.cleanup()


class StreamingEquivalenceTests(unittest.TestCase):
    def cases(self):
        big = "x" * (stream._FIRST_CHUNK + 1234)  # one line longer than a chunk
        codex_lines = [
            json.dumps({"type": "session_meta", "payload": {"id": "s"}}),
            codex_line("user", "first"),
            "",
            "{not json",
            codex_line("assistant", "reply one"),
            tool_line(50_000),
            codex_line("assistant", big),
            tool_line(300_000),
            codex_line("user", "again"),
            codex_line("assistant", "   "),  # whitespace-only never counts as the reply
        ]
        claude_lines = [
            claude_line("user", "hi"),
            claude_line("assistant", "hello there"),
            "",
            claude_line("user", "more"),
        ]
        return [(codex, codex_lines), (claude_code, claude_lines)]

    def test_read_messages_matches_the_old_full_parse_including_cursors(self):
        for parser, lines in self.cases():
            for trailing in (True, False):
                fx = Fixture(lines, trailing)
                try:
                    for since in (None, "v1:7:0", "v1:7:3", "v1:7:6", "v1:7:999", "v1:6:4"):
                        for limit in (1, 2, 100):
                            new = parser.read_messages(fx.path, 7, since, limit)
                            old = old_full_read(parser, fx.path, 7, since, limit)
                            self.assertEqual(
                                ([m.to_dict() for m in new[0]], new[1]),
                                ([m.to_dict() for m in old[0]], old[1]),
                                (parser.__name__, trailing, since, limit),
                            )
                finally:
                    fx.close()

    def test_last_assistant_tail_and_end_cursor_match_the_old_full_parse(self):
        for parser, lines in self.cases():
            fx = Fixture(lines)
            try:
                old_msgs, old_cursor = old_full_read(parser, fx.path, 3)
                old_last = monitor._last_assistant(old_msgs)
                self.assertEqual(stream.last_assistant(fx.path, parser.render_message), old_last)
                self.assertEqual(stream.end_cursor(fx.path, 3), old_cursor)
                for n in (0, 1, 3, 50):
                    tail, last, cursor = stream.tail(fx.path, 3, n, parser.render_message)
                    expected = old_msgs[-n:] if n else []
                    self.assertEqual([m.to_dict() for m in tail], [m.to_dict() for m in expected], n)
                    self.assertEqual((last, cursor), (old_last, old_cursor), n)
            finally:
                fx.close()

    def test_the_last_reply_is_found_across_many_chunks(self):
        # The only assistant message sits ~20 MB before the end.
        lines = [codex_line("assistant", "the answer")] + [tool_line(100_000) for _ in range(200)]
        fx = Fixture(lines)
        try:
            found = stream.last_assistant(fx.path, codex.render_message)
            self.assertEqual(found["content"], "the answer")
        finally:
            fx.close()

    def test_missing_files_degrade_quietly(self):
        self.assertIsNone(stream.last_assistant("/nonexistent/t.jsonl", codex.render_message))
        self.assertEqual(stream.end_cursor("/nonexistent/t.jsonl", 2), "v1:2:0")
        self.assertEqual(codex.read_messages("/nonexistent/t.jsonl", 2, None, 5), ([], "v1:2:0"))


class BoundedMemoryTests(unittest.TestCase):
    """Peak traced Python memory is independent of transcript size."""

    @classmethod
    def setUpClass(cls):
        # ~60 MB of tool output with replies sprinkled in, the shape of a long
        # Codex session.
        lines = []
        for i in range(600):
            lines.append(codex_line("user", f"step {i}"))
            lines.append(tool_line(100_000))
            lines.append(codex_line("assistant", f"done {i}"))
        cls.fx = Fixture(lines)
        cls.size = Path(cls.fx.path).stat().st_size

    @classmethod
    def tearDownClass(cls):
        cls.fx.close()

    def peak(self, fn) -> int:
        tracemalloc.start()
        try:
            fn()
            return tracemalloc.get_traced_memory()[1]
        finally:
            tracemalloc.stop()

    def test_each_reader_stays_far_below_the_file_size(self):
        path = self.fx.path
        limit = 24 * 1024 * 1024
        self.assertGreater(self.size, 50 * 1024 * 1024)
        checks = {
            "last_assistant": lambda: stream.last_assistant(path, codex.render_message),
            "tail": lambda: stream.tail(path, 0, 20, codex.render_message),
            "end_cursor": lambda: stream.end_cursor(path, 0),
            "read_messages near the end": lambda: codex.read_messages(path, 0, "v1:0:1790", 100),
            "monitor completion": lambda: monitor._monitor_completed_entry(
                "u", "awaiting_input", "ready", True, "codex", path, 0, True, None),
        }
        for name, fn in checks.items():
            self.assertLess(self.peak(fn), limit, f"{name} peaked near the file size")

    def test_old_full_parse_would_fail_this_bound(self):
        # Guards the test itself: the pre-fix path peaks above the bound.
        self.assertGreater(self.peak(lambda: old_full_read(codex, self.fx.path, 0)), 24 * 1024 * 1024)

    def test_repeated_completions_in_worker_threads_do_not_accumulate(self):
        path = self.fx.path

        async def run(rounds: int):
            for _ in range(rounds):
                entries = await asyncio.gather(*[
                    asyncio.to_thread(monitor._monitor_completed_entry, f"u{j}", "awaiting_input",
                                      "ready", True, "codex", path, 0, True, None)
                    for j in range(4)
                ])
                self.assertEqual(entries[0]["last_message"]["content"], "done 599")

        tracemalloc.start()
        try:
            asyncio.run(run(3))
            after_warmup = tracemalloc.get_traced_memory()[0]
            asyncio.run(run(20))
            current, peak = tracemalloc.get_traced_memory()
        finally:
            tracemalloc.stop()
        self.assertLess(current - after_warmup, 1024 * 1024, "nothing retained across completions")
        self.assertLess(peak, 4 * 24 * 1024 * 1024, "four concurrent completions stay bounded")


if __name__ == "__main__":
    unittest.main()
