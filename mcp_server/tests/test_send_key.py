"""`send_key` MCP tool (Owner 2026-10-22): one key, no body, no Enter, to one
or more sessions; per-session results; send_input's routing; an optional
monitor only on the sessions that got the key. Byte encoding is tested in
Rust (daemon/src/keys.rs, daemon/src/control/send_key.rs)."""

from __future__ import annotations

import asyncio
import inspect
import unittest
from tempfile import TemporaryDirectory
from unittest import mock

from mcp_server import async_monitor, control_client


def _reply(statuses: dict[str, str]) -> dict:
    results = [{"session_uid": u, "status": s} for u, s in statuses.items()]
    return {"key": "enter", "sent": sum(s == "ok" for s in statuses.values()), "results": results}


class SendKeyToolTests(unittest.TestCase):
    def call(self, *args, **kwargs):
        from mcp_server.server import send_key

        return asyncio.run(send_key(*args, **kwargs))

    def test_sends_one_key_with_a_uid_list_and_returns_per_session_results(self):
        calls = []

        def fake(method, params=None, **_):
            calls.append((method, params))
            return _reply({"ts-a": "ok", "ts-b": "exited", "ts-c": "unauthorized", "ts-d": "not_found"})

        with mock.patch.object(control_client, "call", side_effect=fake), \
                mock.patch.object(async_monitor, "register_monitor") as reg:
            res = self.call(["ts-a", "ts-b", "ts-c", "ts-d"], "enter")
            one = self.call("ts-a", "y")
        self.assertEqual(calls[0], ("send_key", {"session_uids": ["ts-a", "ts-b", "ts-c", "ts-d"], "key": "enter"}))
        self.assertEqual(calls[1], ("send_key", {"session_uids": ["ts-a"], "key": "y"}), "a single uid becomes a list")
        self.assertEqual([r["status"] for r in res["results"]], ["ok", "exited", "unauthorized", "not_found"])
        self.assertEqual(res["sent"], 1)
        reg.assert_not_called()  # notify_on_done defaults to false
        self.assertNotIn("monitor", one)

    def test_notify_on_done_watches_only_the_sessions_that_got_the_key(self):
        with mock.patch.object(control_client, "call", return_value=_reply({"ts-a": "ok", "ts-b": "exited", "ts-c": "ok"})), \
                mock.patch.object(async_monitor, "register_monitor", return_value={"monitor_id": "m1"}) as reg:
            res = self.call(["ts-a", "ts-b", "ts-c"], "enter", notify_on_done=True)
        reg.assert_called_once()
        self.assertEqual(reg.call_args.args[0], ["ts-a", "ts-c"])
        self.assertEqual(reg.call_args.kwargs["mode"], "all")
        self.assertEqual(res["monitor"], {"monitor_id": "m1"})
        # Nothing reached: no monitor.
        with mock.patch.object(control_client, "call", return_value=_reply({"ts-b": "exited"})), \
                mock.patch.object(async_monitor, "register_monitor") as reg:
            self.call(["ts-b"], "enter", notify_on_done=True)
        reg.assert_not_called()

    def test_more_than_fifty_sessions_is_refused_before_any_call(self):
        with mock.patch.object(control_client, "call") as call:
            res = self.call([f"ts-{i}-0" for i in range(51)], "enter")
        call.assert_not_called()
        self.assertEqual(res["error"], "invalid_params")

    def test_description_says_one_key_and_no_trailing_enter(self):
        from mcp_server.server import send_key

        doc = inspect.getdoc(send_key)
        self.assertIn("exactly ONE key", doc)
        self.assertIn("no Enter after it", doc)
        for name in ["enter", "escape", "tab", "shift-tab", "up", "down", "left", "right", "home", "end",
                     "pageup", "pagedown", "backspace", "delete", "space", "ctrl-c", "ctrl-d"]:
            self.assertIn(name, doc)

    def test_routes_like_send_input(self):
        env = {"CM_DAEMON_SOCKET": "/tmp/d.sock", "CM_TUI_SOCKET": "/tmp/t.sock"}
        with TemporaryDirectory() as tmp, mock.patch.dict("os.environ", {"HOME": tmp, **env}, clear=True):
            self.assertEqual(str(control_client.resolve_socket_for_method("send_key")), "/tmp/t.sock")
            self.assertEqual(str(control_client.resolve_socket_for_method("send_input")), "/tmp/t.sock")
            self.assertEqual(str(control_client.resolve_socket_for_method("session.send_key")), "/tmp/d.sock")


if __name__ == "__main__":
    unittest.main()
