"""Regression test: PlanningClient raises a helpful error when env is unset.

Before the fix, ``PlanningClient()`` did ``os.environ["CM_API_URL"]`` directly,
so MCP servers spawned outside the TUI's environment crashed with a bare
``KeyError: 'CM_API_URL'`` and zero context. The constructor now raises a
``RuntimeError`` that names the variable and explains how to set it.
"""

from __future__ import annotations

import unittest
from unittest import mock

from cli.planning_client import PlanningClient


class PlanningClientEnvTests(unittest.TestCase):
    def test_missing_api_url_raises_helpful_error(self):
        with mock.patch.dict("os.environ", {}, clear=True):
            with self.assertRaises(RuntimeError) as ctx:
                PlanningClient()
        msg = str(ctx.exception)
        self.assertIn("CM_API_URL", msg)
        self.assertIn("TUI", msg)
        self.assertIn("CM_API_TOKEN", msg)

    def test_missing_api_token_raises_helpful_error(self):
        with mock.patch.dict(
            "os.environ", {"CM_API_URL": "http://localhost:8000"}, clear=True
        ):
            with self.assertRaises(RuntimeError) as ctx:
                PlanningClient()
        msg = str(ctx.exception)
        self.assertIn("CM_API_TOKEN", msg)
        self.assertIn("TUI", msg)

    def test_explicit_args_bypass_env(self):
        with mock.patch.dict("os.environ", {}, clear=True):
            client = PlanningClient(api_url="http://x", api_token="t")
        self.assertIsNotNone(client)


if __name__ == "__main__":
    unittest.main()
