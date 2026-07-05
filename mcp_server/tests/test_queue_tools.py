"""`enqueue` / `queue_depth` MCP tools (Continuous Tasks Phase 4).

Both are thin daemon-routed wrappers: `enqueue` -> daemon `enqueue`,
`queue_depth` -> daemon `queue.stats`. These tests pin the method names and
the param shapes (optional fields omitted when empty, so the daemon's
serde defaults — not empty strings — apply).
"""

from __future__ import annotations

import unittest
from unittest import mock


class QueueToolsRoutingTests(unittest.TestCase):
    def test_enqueue_routes_with_minimal_params(self):
        from mcp_server import server, control_client

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            captured["params"] = params
            return {"enqueued": True, "deduped": False, "id": "x", "depth": 1}

        with mock.patch.object(control_client, "call", side_effect=fake_call):
            result = server.enqueue(
                queue="scraper-creation-proposals",
                payload={"url": "https://a"},
            )

        self.assertEqual(captured["method"], "enqueue")
        self.assertEqual(
            captured["params"],
            {
                "queue": "scraper-creation-proposals",
                "payload": {"url": "https://a"},
            },
            "empty dedupe_key/source must be OMITTED, not sent as empty strings",
        )
        self.assertTrue(result["enqueued"])

    def test_enqueue_forwards_dedupe_key_and_source(self):
        from mcp_server import server, control_client

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["params"] = params
            return {"enqueued": False, "deduped": True, "id": None, "depth": 4}

        with mock.patch.object(control_client, "call", side_effect=fake_call):
            result = server.enqueue(
                queue="q",
                payload={"a": 1},
                dedupe_key="a.com",
                source="signal-source-investigation",
            )

        self.assertEqual(captured["params"]["dedupe_key"], "a.com")
        self.assertEqual(
            captured["params"]["source"], "signal-source-investigation"
        )
        self.assertTrue(result["deduped"])

    def test_queue_depth_routes_to_queue_stats(self):
        from mcp_server import server, control_client

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            captured["params"] = params
            return {"queue": "q", "pending": 3, "claimed": 0, "oldest_pending_at": None}

        with mock.patch.object(control_client, "call", side_effect=fake_call):
            result = server.queue_depth(queue="q")

        self.assertEqual(captured["method"], "queue.stats")
        self.assertEqual(captured["params"], {"queue": "q"})
        self.assertEqual(result["pending"], 3)


if __name__ == "__main__":
    unittest.main()
