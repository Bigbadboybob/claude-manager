"""Item MCP tools route to the daemon with only the arguments given."""

import unittest
from unittest import mock

from mcp_server import control_client, server


class ItemToolsTests(unittest.TestCase):
    def call(self, tool, *args, **kwargs):
        with mock.patch.object(control_client, "call", return_value={"ok": 1}) as call:
            self.assertEqual(tool(*args, **kwargs), {"ok": 1})
        (method, params), opts = call.call_args
        self.assertNotIn("socket_path", opts)  # routed like other daemon methods
        return method, params

    def test_item_create_sends_only_given_fields(self):
        method, params = self.call(server.item, ["a", "b"], holder="rl", eta="40m")
        self.assertEqual(method, "item.create")
        self.assertEqual(params, {"title": ["a", "b"], "holder": "rl", "eta": "40m"})

    def test_item_set_status_shorthand_and_clears(self):
        method, params = self.call(server.item_set, 14, "done", note="")
        self.assertEqual(method, "item.set")
        self.assertEqual(params, {"n": 14, "status": "done", "note": ""})
        _, params = self.call(server.item_set, [14, 15], blocked_by=[])
        self.assertEqual(params, {"n": [14, 15], "blocked_by": []})

    def test_board_defaults(self):
        method, params = self.call(server.board)
        self.assertEqual(method, "board.read")
        self.assertEqual(params, {"view": "slim", "mine": False, "include_closed": True,
                                  "archived": False})

    def test_item_resolve_has_no_engine_default(self):
        method, params = self.call(server.item_resolve, 14, "launch")
        self.assertEqual(method, "item.resolve")
        self.assertEqual(params, {"n": 14, "action": "launch"})

    def test_methods_are_daemon_routed(self):
        for method in ("item.create", "item.set", "item.resolve", "board.read"):
            self.assertIn(method, control_client.DAEMON_METHODS)


if __name__ == "__main__":
    unittest.main()
