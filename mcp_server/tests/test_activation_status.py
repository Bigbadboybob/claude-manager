"""activation.py: which payload is loaded, when an update is pending, and
the per-session status.json (doc/mcp-managed-activation.md §6)."""

import json
import os
import shutil
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from mcp_server import activation, mcp_version


class PendingTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        (self.root / "server.py").write_text("v = 'next'\n")
        activation._disk.update(mtime=None, stamp=None, consistent=False)

    def tearDown(self):
        shutil.rmtree(self.root)

    def test_unstamped_payload_is_never_pending(self):
        self.assertIsNone(activation.pending(self.root))

    def test_a_consistent_newer_stamp_is_pending(self):
        stamp = mcp_version.stamp(self.root, git="feedfacecafe0000")
        pending = activation.pending(self.root)
        self.assertEqual(pending["version"], "feedfacecafe")
        self.assertEqual(pending["payload_hash"], stamp["payload_hash"])

    def test_files_changed_after_stamping_are_not_pending(self):
        mcp_version.stamp(self.root)
        (self.root / "server.py").write_text("v = 'half copied'\n")
        os.utime(self.root / mcp_version.STAMP)  # a rehash only follows a stamp change
        self.assertIsNone(activation.pending(self.root))
        mcp_version.stamp(self.root)
        self.assertIsNotNone(activation.pending(self.root))

    def test_the_loaded_payload_is_not_pending(self):
        stamp = mcp_version.stamp(self.root)
        with mock.patch.object(activation, "_loaded", {**activation.loaded(),
                                                       "payload_hash": stamp["payload_hash"]}):
            self.assertIsNone(activation.pending(self.root))


class PendingToolsetTests(unittest.TestCase):
    def test_only_a_toolset_change_is_a_tools_change(self):
        root = Path(tempfile.mkdtemp())
        try:
            (root / "server.py").write_text("v = 3\n")
            activation._disk.update(mtime=None, stamp=None, consistent=False)
            mcp_version.stamp(root)
            stamp = json.loads((root / mcp_version.STAMP).read_text())
            same = {**activation.loaded(), "toolset_hash": stamp["toolset_hash"]}
            with mock.patch.object(activation, "_loaded", same):
                self.assertFalse(activation.pending(root)["tools_change"])
            other = {**activation.loaded(), "toolset_hash": "something-else"}
            with mock.patch.object(activation, "_loaded", other):
                self.assertEqual(activation.pending(root)["tools_change"], bool(stamp["toolset_hash"]))
        finally:
            shutil.rmtree(root)


class PredecessorTests(unittest.TestCase):
    def test_identity_is_pid_plus_start_time(self):
        me = os.getpid()
        start = activation.start_ticks(me)
        self.assertTrue(activation.process_alive(me, start))
        self.assertFalse(activation.process_alive(me, str(int(start) + 1)), "a reused pid")
        import subprocess, sys
        proc = subprocess.Popen([sys.executable, "-c", "pass"])
        proc.wait()
        self.assertFalse(activation.process_alive(proc.pid))

    def test_another_users_process_is_not_our_predecessor(self):
        with mock.patch.object(activation.os, "kill", side_effect=PermissionError):
            self.assertFalse(activation.process_alive(1))

    def test_promotion_waits_for_the_predecessor(self):
        import asyncio, subprocess, sys
        proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(0.6)"])
        env = {"CM_MCP_PREDECESSOR_PID": str(proc.pid),
               "CM_MCP_PREDECESSOR_START": activation.start_ticks(proc.pid) or ""}
        with mock.patch.dict(os.environ, env):
            t0 = time.time()
            asyncio.run(activation.wait_promoted(poll_s=0.05))
            proc.wait()
            self.assertGreater(time.time() - t0, 0.4)
        with mock.patch.dict(os.environ, {"CM_MCP_PREDECESSOR_PID": ""}):
            t0 = time.time()
            asyncio.run(activation.wait_promoted())
            self.assertLess(time.time() - t0, 0.1, "the first backend has nothing to wait for")


class StatusFileTests(unittest.TestCase):
    def setUp(self):
        self.home = Path(tempfile.mkdtemp())
        self.env = mock.patch.dict(os.environ, {
            "HOME": str(self.home), "CM_TUI_SESSION_ID": "ts-abc-1",
            "CM_AGENT_ENGINE": "codex", "CM_MCP_PROXY_CHILD": "1",
            "CM_MCP_PROXY_VERSION": "1", "CM_MCP_PROXY_GENERATION": "1"})
        self.env.start()
        activation._session_tools = None

    def tearDown(self):
        self.env.stop()
        activation._session_tools = None
        shutil.rmtree(self.home)

    def test_first_generation_records_the_session_tool_set(self):
        written = activation.write_status()
        on_disk = json.loads((self.home / ".cm/mcp/ts-abc-1/status.json").read_text())
        self.assertEqual(on_disk["session_toolset_hash"], activation.loaded()["toolset_hash"])
        self.assertEqual(on_disk["proxy"], {"version": "1", "generation": 1})
        self.assertFalse(written["tools_changed_unseen"])

    def test_a_codex_successor_with_other_tools_says_they_are_unseen(self):
        path = self.home / ".cm/mcp/ts-abc-1/status.json"
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps({"session_toolset_hash": "older-tool-set"}))
        os.environ["CM_MCP_PROXY_GENERATION"] = "2"
        value = activation.write_status()
        self.assertEqual(value["session_toolset_hash"], "older-tool-set")
        self.assertTrue(value["tools_changed_unseen"])
        self.assertIn("after a restart", activation.brief()["note"])
        os.environ["CM_AGENT_ENGINE"] = "claude-code"  # Claude refreshes its tools
        self.assertFalse(activation.status()["tools_changed_unseen"])


if __name__ == "__main__":
    unittest.main()
