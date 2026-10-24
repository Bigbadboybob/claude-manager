"""The backend's activation policy: tell a working agent once, then ask the
proxy to activate only at an idle boundary (doc/mcp-managed-activation.md §4)."""

import asyncio
import os
import shutil
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from mcp_server import activation

UPDATE = {"version": "abc123", "payload_hash": "h2", "tools_hash": "t1", "tools_change": False}


class FakeQueue:
    def __init__(self, events=()):
        self.events = list(events)
        self.published = []

    def snapshot(self):
        return {"notifications": self.events}

    def publish(self, event_id, source, text, marker):
        self.published.append((event_id, source, text, marker))


class PolicyTests(unittest.TestCase):
    def setUp(self):
        self.home = Path(tempfile.mkdtemp())
        self.env = mock.patch.dict(os.environ, {
            "HOME": str(self.home), "CM_TUI_SESSION_ID": "ts-abc-1",
            "CM_AGENT_ENGINE": "claude-code", "CM_MCP_PROXY_CHILD": "1",
            "CM_MCP_PROXY_VERSION": "1", "CM_MCP_PROXY_GENERATION": "1"})
        self.env.start()
        self.pending = mock.patch.object(activation, "pending", return_value=dict(UPDATE))
        self.pending.start()
        activation._session_tools = None
        self.agent = {"state": "idle", "since": time.time() - 120}
        self.inbox = []
        self.queue = FakeQueue()
        self.sent = []

    def tearDown(self):
        self.pending.stop()
        self.env.stop()
        shutil.rmtree(self.home)

    def call(self, method, params, timeout=None):
        if method == "resolve_authorized_session":
            return {"agent_state": self.agent}
        if method == "messaging.read":
            return {"items": self.inbox}
        raise AssertionError(method)

    async def send(self, mode):
        self.sent.append(mode)

    def tick(self, policy=None):
        self.policy = policy or getattr(self, "policy", None) or activation.Policy(
            self.send, self.call, lambda: self.queue, lambda: 2)
        return asyncio.run(self.policy.tick())

    def test_a_recently_idle_agent_is_not_woken(self):
        self.agent = {"state": "idle", "since": time.time() - 10}
        self.assertEqual(self.tick()["blocked"], "idle_too_recently")
        self.assertEqual((self.queue.published, self.sent), ([], []))

    def test_idle_boundary_requests_activation_without_a_notice(self):
        status = self.tick()
        self.assertEqual(self.sent, ["idle"])
        self.assertIsNone(status["blocked"])
        self.assertEqual(self.queue.published, [], "an idle agent is never woken")

    def test_a_working_agent_is_told_once_and_not_activated(self):
        self.agent = {"state": "working", "since": time.time() - 5}
        self.tick()
        status = self.tick()
        self.assertEqual(self.sent, [])
        self.assertEqual(status["blocked"], "working")
        self.assertEqual(len(self.queue.published), 1)
        event_id, source, text, marker = self.queue.published[0]
        self.assertEqual((event_id, source), ("mcp-update:abc123", "mcp_update"))
        self.assertTrue(text.startswith(marker))
        self.assertIn("your 2 monitors and chat watches carry over", text)
        self.assertIn("on its own", text)
        self.assertNotIn("must", text)

    def test_each_blocker_holds_activation(self):
        cases = [
            ({"state": "idle", "since": time.time() - 10}, [], [], "idle_too_recently"),
            (dict(self.agent), [{"id": "m"}], [], "unread_inbox"),
            (dict(self.agent), [], [{"status": "pending", "source": "chat"}], "notice_pending"),
            (dict(self.agent), [], [{"status": "submitting", "source": "session_monitor"}],
             "notice_pending"),
            ({"state": "waiting-on-human", "since": 0}, [], [], "waiting-on-human"),
        ]
        for agent, inbox, events, reason in cases:
            with self.subTest(reason=reason):
                self.agent, self.inbox, self.queue = agent, inbox, FakeQueue(events)
                self.sent = []
                self.policy = None
                self.assertEqual(self.tick()["blocked"], reason)
                self.assertEqual(self.sent, [])

    def test_our_own_pending_notice_does_not_block(self):
        self.queue = FakeQueue([{"status": "pending", "source": "mcp_update"}])
        self.tick()
        self.assertEqual(self.sent, ["idle"])

    def test_without_a_proxy_nothing_is_sent(self):
        os.environ.pop("CM_MCP_PROXY_CHILD")
        status = self.tick()
        self.assertEqual((status["blocked"], self.sent, self.queue.published), ("no_proxy", [], []))

    def test_codex_notice_says_new_tools_need_a_restart(self):
        os.environ["CM_AGENT_ENGINE"] = "codex"
        self.pending.stop()
        self.pending = mock.patch.object(
            activation, "pending", return_value={**UPDATE, "tools_change": True})
        self.pending.start()
        self.agent = {"state": "working", "since": time.time()}
        self.tick()
        self.assertTrue(self.queue.published[0][2].endswith("New tools appear after a restart."))
        os.environ["CM_AGENT_ENGINE"] = "claude-code"
        self.assertNotIn("restart", activation.notice_text("v", 0, False, True))


class AckTests(unittest.TestCase):
    def test_nothing_pending(self):
        with mock.patch.object(activation, "pending", return_value=None):
            self.assertFalse(activation.ack(0)["activating"])

    def test_pending_behind_the_proxy(self):
        with mock.patch.object(activation, "pending", return_value=dict(UPDATE)), \
                mock.patch.dict(os.environ, {"CM_MCP_PROXY_CHILD": "1"}):
            result = activation.ack(3)
        self.assertEqual((result["activating"], result["version"], result["monitors_carried"]),
                         (True, "abc123", 3))

    def test_pending_without_a_proxy_explains(self):
        with mock.patch.object(activation, "pending", return_value=dict(UPDATE)), \
                mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("CM_MCP_PROXY_CHILD", None)
            self.assertIn("manual /mcp", activation.ack(0)["note"])


if __name__ == "__main__":
    unittest.main()
