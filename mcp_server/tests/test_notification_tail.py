"""Incremental receipt checks (notifications._Tail): same answers as the old
full 2 MiB rescan, while reading only appended bytes."""
import json
import os
import random
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from mcp_server import notifications
from mcp_server.notifications import Queue, ReceiptBackoff, transcript_observed


def reference_observed(path, engine, marker, binding=None, window=2 * 1024 * 1024):
    """The pre-2026-10-23 implementation, verbatim apart from `window`."""
    if not path or not marker:
        return False
    try:
        with open(path, "rb") as stream:
            stream.seek(0, 2)
            start = max(0, stream.tell() - window)
            stream.seek(start)
            if start:
                stream.readline()
            for line in stream:
                try:
                    item = json.loads(line)
                except (ValueError, UnicodeError):
                    continue
                if engine == "claude-code":
                    content = item.get("message", {})
                    origin = item.get("origin", {})
                    expected = binding or {}
                    channel_server = expected.get("channel_server")
                    if channel_server and expected.get("adapter") == "claude-mcp-channel-v1":
                        attachment = item.get("attachment", {})
                        if (
                            item.get("type") == "user"
                            and item.get("isMeta") is True
                            and content.get("role") == "user"
                            and origin == {"kind": "channel", "server": channel_server}
                            and marker in json.dumps(content.get("content"))
                        ) or (
                            item.get("type") == "attachment"
                            and attachment.get("type") == "queued_command"
                            and attachment.get("origin") == {"kind": "channel", "server": channel_server}
                            and marker in json.dumps(attachment.get("prompt"))
                        ):
                            return True
                        continue
                    if (
                        item.get("type") == "user"
                        and content.get("role") == "user"
                        and origin.get("kind") == "peer"
                        and origin.get("from") == "claude-manager"
                        and origin.get("selfSent") is True
                        and marker in json.dumps(content.get("content"))
                    ):
                        return True
                    attachment = item.get("attachment", {})
                    origin = attachment.get("origin", {})
                    expected = binding or {}
                    if (
                        item.get("type") == "attachment"
                        and attachment.get("type") == "queued_command"
                        and origin.get("kind") == "peer"
                        and origin.get("from") == "claude-manager"
                        and expected.get("peer_pid") is not None
                        and origin.get("verifiedPeerPid") == expected["peer_pid"]
                        and (
                            not expected.get("peer_start")
                            or str(origin.get("verifiedPeerProcStart"))
                            == expected["peer_start"]
                        )
                        and marker in json.dumps(attachment.get("prompt"))
                    ):
                        return True
                elif item.get("type") == "response_item":
                    payload = item.get("payload", {})
                    if (
                        payload.get("type") == "function_call_output"
                        and payload.get("name") == "cm_notification"
                        and marker in json.dumps(payload.get("output"))
                    ):
                        return True
    except OSError:
        pass
    return False


MARKERS = [f"[cm-n{i}]" for i in range(6)]
CHANNEL = {"adapter": "claude-mcp-channel-v1", "channel_server": "claude-manager"}
PEER = {"peer_pid": 42, "peer_start": "7"}


def record(rng):
    m = rng.choice(MARKERS)
    return rng.choice([
        {"type": "user", "origin": {"kind": "peer", "from": "claude-manager", "selfSent": True},
         "message": {"role": "user", "content": m}},
        {"type": "user", "isMeta": True, "origin": {"kind": "channel", "server": "claude-manager"},
         "message": {"role": "user", "content": f"<channel>{m}</channel>"}},
        {"type": "attachment", "attachment": {"type": "queued_command", "prompt": m,
         "origin": {"kind": "peer", "from": "claude-manager", "verifiedPeerPid": 42, "verifiedPeerProcStart": "7"}}},
        {"type": "attachment", "attachment": {"type": "queued_command", "prompt": m,
         "origin": {"kind": "channel", "server": "claude-manager"}}},
        {"type": "assistant", "message": {"role": "assistant", "content": f"quoting {m}"}},
        {"type": "user", "message": {"role": "user", "content": f"origin {m}"}},
        {"type": "queue-operation", "content": m},
        {"type": "response_item", "payload": {"type": "function_call_output", "name": "cm_notification", "output": m}},
        {"type": "response_item", "payload": {"type": "function_call_output", "name": "shell", "output": m}},
        {"type": "progress", "pad": "x" * rng.randrange(0, 120)},
    ])


class EquivalenceTests(unittest.TestCase):
    def setUp(self):
        notifications._tails.clear()

    def test_matches_the_full_rescan_through_appends_rewrites_and_window_slides(self):
        """Random appends (including partial lines finished later), in-place
        rewrites, truncations and replacements: every check agrees with the
        old implementation for every marker, engine and binding."""
        for seed in (20261023, 7, 99, 12345):
            notifications._tails.clear()
            with self.subTest(seed=seed):
                self._fuzz(random.Random(seed), steps=900)

    def _fuzz(self, rng, steps):
        window = 600
        clock = [1_700_000_000_000_000_000]
        with tempfile.TemporaryDirectory() as tmp, patch.object(notifications, "TAIL_WINDOW", window):
            path = Path(tmp) / "t.jsonl"
            path.write_bytes(b"")
            pending = b""
            for step in range(steps):
                op = rng.random()
                if op < 0.70:
                    line = json.dumps(record(rng)).encode() + b"\n"
                    if rng.random() < 0.2:
                        line += b'{"type":"user","origin":{"kind":"peer"'  # unfinished, finished below
                        pending = b',"from":"claude-manager","selfSent":true},"message":{"role":"user","content":"[cm-n0]"}}\n'
                    elif pending:
                        line = pending + line
                        pending = b""
                    with open(path, "ab") as f:
                        f.write(line)
                elif op < 0.75:
                    with open(path, "ab") as f:
                        f.write(b"not json\n")
                elif op < 0.80:  # rewrite in place (same inode), sometimes longer
                    body = b"".join(json.dumps(record(rng)).encode() + b"\n" for _ in range(rng.randrange(1, 12)))
                    path.write_bytes(body)
                    pending = b""
                elif op < 0.83:  # replace (new inode)
                    new = Path(tmp) / "new.jsonl"
                    new.write_bytes(json.dumps(record(rng)).encode() + b"\n")
                    os.replace(new, path)
                    pending = b""
                elif op < 0.85:
                    with open(path, "r+b") as f:
                        f.truncate(rng.randrange(0, max(1, path.stat().st_size)))
                    pending = b""
                # Real writers' mtimes move on; a coarse filesystem clock could
                # otherwise give two same-size rewrites one mtime in this loop.
                if path.exists():
                    clock[0] += 1_000_000
                    os.utime(path, ns=(clock[0], clock[0]))
                for marker in MARKERS:
                    for engine, binding in (("claude-code", None), ("claude-code", CHANNEL),
                                            ("claude-code", PEER), ("codex", None)):
                        self.assertEqual(
                            transcript_observed(str(path), engine, marker, binding),
                            reference_observed(str(path), engine, marker, binding, window=window),
                            f"step {step} {engine} {binding} {marker}",
                        )

    def test_missing_files_and_empty_markers_are_never_observed(self):
        self.assertFalse(transcript_observed("/nonexistent/x.jsonl", "codex", "[m]"))
        self.assertFalse(transcript_observed(None, "codex", "[m]"))
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "t.jsonl"
            path.write_text(json.dumps({"type": "response_item", "payload": {
                "type": "function_call_output", "name": "cm_notification", "output": "[m]"}}) + "\n")
            self.assertFalse(transcript_observed(str(path), "codex", ""))
            self.assertTrue(transcript_observed(str(path), "codex", "[m]"))
            path.unlink()
            self.assertFalse(transcript_observed(str(path), "codex", "[m]"))


class ReadVolumeTests(unittest.TestCase):
    """Microbenchmark: N checks of many markers on a 50 MiB transcript read
    the window once, then O(appended) bytes."""

    def test_checks_on_a_50_mib_transcript_read_only_appended_bytes(self):
        notifications._tails.clear()
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "big.jsonl"
            filler = (json.dumps({"type": "assistant", "message": {"role": "assistant",
                                  "content": "y" * 900}}) + "\n").encode()
            with open(path, "wb") as f:
                for _ in range(50 * 1024 * 1024 // len(filler) + 1):
                    f.write(filler)
            self.assertGreaterEqual(path.stat().st_size, 50 * 1024 * 1024)
            notifications.tail_stats.clear()
            markers = [f"[cm-x{i}]" for i in range(100)]  # 100 pending notices
            appended = 0
            for poll in range(50):
                line = (json.dumps({"type": "user", "origin": {"kind": "peer", "from": "claude-manager",
                        "selfSent": True}, "message": {"role": "user", "content": markers[poll]}}) + "\n").encode()
                with open(path, "ab") as f:
                    f.write(line)
                appended += len(line)
                for i, m in enumerate(markers):
                    self.assertEqual(transcript_observed(str(path), "claude-code", m), i <= poll)
            read = notifications.tail_stats["bytes_read"]
            # One window on the first check, then what was appended plus the
            # rewrite check (head and pre-offset bytes) once per change; the
            # 99 other markers each poll read nothing. The old code read
            # 2 MiB per check: 5000 checks, ~10 GiB.
            fp = 4 * notifications._FINGERPRINT
            self.assertLessEqual(read, notifications.TAIL_WINDOW + appended + 51 * fp)
            self.assertGreaterEqual(read, appended)


class BackoffTests(unittest.TestCase):
    def test_checks_space_out_after_the_eager_ones_and_cap(self):
        b = ReceiptBackoff()
        now = 100.0
        for _ in range(notifications.RECEIPT_EAGER_CHECKS):
            self.assertTrue(b.due("e", now))
            b.checked("e", now)
        self.assertFalse(b.due("e", now))
        self.assertTrue(b.due("e", now + 1.0))
        delays = []
        for _ in range(10):
            b.checked("e", now)
            delays.append(b.state["e"][1] - now)
        self.assertEqual(delays[:5], [2.0, 4.0, 8.0, 16.0, 30.0])
        self.assertEqual(max(delays), notifications.RECEIPT_MAX_INTERVAL_S)
        self.assertTrue(b.due("other", now), "a new notice is checked at once")
        b.retain(["other"])
        self.assertNotIn("e", b.state)


class EventCacheTests(unittest.TestCase):
    def test_snapshot_reparses_only_changed_event_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            q = Queue("uid-cache", Path(tmp))
            for i in range(20):
                q.publish(f"e{i}", "cm", f"[m{i}] hi", f"[m{i}]")
            q.snapshot()
            real = Path.read_text
            reads = []

            def counting(self, *a, **k):
                reads.append(self.name)
                return real(self, *a, **k)

            with patch.object(Path, "read_text", counting):
                snap = q.snapshot()
                self.assertEqual(len(snap["notifications"]), 20)
                self.assertEqual([r for r in reads if r != "transport.json"], [], "nothing changed: no parse")
                q.change("e3", {"pending"}, status="cancelled")
                reads.clear()
                snap = q.snapshot()
            parsed = [r for r in reads if r != "transport.json"]
            self.assertEqual(parsed, [q.event_path("e3").name], "snapshot re-parses only the changed file")
            self.assertEqual({e["id"]: e["status"] for e in snap["notifications"]}["e3"], "cancelled")
            # Returned events are copies: mutating one leaves the cache intact.
            snap["notifications"][0]["status"] = "bogus"
            self.assertNotIn("bogus", {e["status"] for e in q.snapshot()["notifications"]})
            # A removed file disappears.
            q.event_path("e5").unlink()
            self.assertEqual(len(q.snapshot()["notifications"]), 19)


if __name__ == "__main__":
    unittest.main()
