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

    def test_item_create_sends_only_given_fields_and_a_request_id(self):
        method, params = self.call(server.item, ["a", "b"], holder="rl", eta="40m")
        self.assertEqual(method, "item.create")
        request_id = params.pop("request_id")
        self.assertRegex(request_id, r"^[0-9a-f]{32}$")
        self.assertEqual(params, {"title": ["a", "b"], "holder": "rl", "eta": "40m"})
        _, params = self.call(server.item, "a", request_id="mine")
        self.assertEqual(params["request_id"], "mine")

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

    def test_item_resolve_has_no_engine_default_and_launch_gets_a_request_id(self):
        method, params = self.call(server.item_resolve, 14, "launch")
        self.assertEqual(method, "item.resolve")
        self.assertEqual(set(params), {"n", "action", "request_id"})
        _, params = self.call(server.item_resolve, 14, "nudge")
        self.assertEqual(params, {"n": 14, "action": "nudge"})

    def test_timeouts_exceed_the_worst_case_chain(self):
        with mock.patch.object(control_client, "call", return_value={}) as call:
            server.item_resolve(14, "launch")
            self.assertGreaterEqual(call.call_args.kwargs["timeout"], 4 * 8 + 60)
            server.item("a")
            self.assertGreaterEqual(call.call_args.kwargs["timeout"], 4 * 8)

    def test_transport_errors_name_the_request_id(self):
        boom = control_client.TransportError("read timed out")
        with mock.patch.object(control_client, "call", side_effect=boom):
            with self.assertRaises(control_client.TransportError) as cm:
                server.item("a", request_id="rq-7")
        self.assertIn("request_id='rq-7'", str(cm.exception))

    def test_methods_are_daemon_routed(self):
        for method in ("item.create", "item.set", "item.resolve", "board.read"):
            self.assertIn(method, control_client.DAEMON_METHODS)


if __name__ == "__main__":
    unittest.main()
