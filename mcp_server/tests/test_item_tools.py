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
        _, params = self.call(server.item_set, [14, 15], "done", each=True)
        self.assertEqual(params, {"n": [14, 15], "status": "done", "each": True})

    def test_gate_tools_route_to_the_daemon(self):
        method, params = self.call(server.gate_report, 14, [], "a" * 40, klass="py")
        self.assertEqual(method, "gate.report")
        self.assertRegex(params.pop("request_id"), r"^[0-9a-f]{32}$")
        self.assertEqual(params, {"n": 14, "steps": [], "head": "a" * 40, "klass": "py",
                                  "gate": "gate"})
        method, params = self.call(server.gate_waive, 14, "a" * 40, "mypy", "r", "k", "m-1")
        self.assertEqual((method, params["kind"], params["message_id"]), ("gate.waive", "waiver", "m-1"))
        method, params = self.call(server.gate_profile, klass="py")
        self.assertEqual((method, params), ("gate.profile", {"klass": "py", "default": False}))
        method, params = self.call(server.gate_receipts, 14)
        self.assertEqual((method, params), ("gate.receipts", {"n": 14, "history": False}))
        _, params = self.call(server.item_set, 14, mf="c" * 40, ruling="")
        self.assertEqual(params, {"n": 14, "mf": "c" * 40, "ruling": ""})

    def test_board_defaults(self):
        method, params = self.call(server.board)
        self.assertEqual(method, "board.read")
        self.assertEqual(params, {"view": "slim", "mine": False, "include_closed": True,
                                  "archived": False})

    def test_place_unplace_and_roles_route_to_the_daemon(self):
        method, params = self.call(server.place, "t-1", group="Latent", holder="me")
        self.assertEqual(method, "item.place")
        self.assertRegex(params.pop("request_id"), r"^[0-9a-f]{32}$")
        self.assertEqual(params, {"task_id": "t-1", "group": "Latent", "holder": "me"})
        method, params = self.call(server.unplace, "t-1", reason="later")
        self.assertEqual((method, params), ("item.unplace", {"task_id": "t-1", "reason": "later"}))
        _, params = self.call(server.item_set, 14, owner="rl", add_holder="ep", role="reviewer")
        self.assertEqual(params, {"n": 14, "owner": "rl", "add_holder": "ep", "role": "reviewer"})
        for method in ("item.place", "item.unplace", "group.create", "group.set",
                       "boards.on_boards"):
            self.assertIn(method, control_client.DAEMON_METHODS)

    def test_item_set_reopen_and_expect_status(self):
        _, params = self.call(server.item_set, 14, "blocked", reopen=True, expect_status="done")
        self.assertEqual(params, {"n": 14, "status": "blocked", "reopen": True,
                                  "expect_status": "done"})
        _, params = self.call(server.item_set, 14, "done")
        self.assertEqual(params, {"n": 14, "status": "done"})

    def test_job_and_board_settings(self):
        _, params = self.call(server.item_set, 14, eta="40m", job="pins L3")
        self.assertEqual(params, {"n": 14, "eta": "40m", "job": "pins L3"})
        with mock.patch.object(control_client, "call", return_value={"ok": 1}) as call:
            server.board(board="sejd", settings={"idle_s": 1800})
        calls = [c.args for c in call.call_args_list]
        self.assertEqual(calls[0], ("board.settings", {"board": "sejd", "settings": {"idle_s": 1800}}))
        self.assertEqual(calls[1][0], "board.read")
        self.assertNotIn("settings", calls[1][1])

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
        for method in ("item.create", "item.set", "item.resolve", "board.read", "board.settings",
                       "gate.report",
                       "gate.receipts", "gate.waive", "gate.profile"):
            self.assertIn(method, control_client.DAEMON_METHODS)


if __name__ == "__main__":
    unittest.main()
