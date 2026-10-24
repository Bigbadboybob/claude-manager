"""The real server.py behind mcp_proxy.py, without a daemon
(doc/mcp-managed-activation.md): the successor stays a passive candidate
until its predecessor exits, then announces `ready`, which ends the hold."""

import json
import os
import select
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
UID = "ts-proxyint-1"

try:
    import mcp  # noqa: F401
    HAVE_MCP = True
except ImportError:
    HAVE_MCP = False


@unittest.skipUnless(HAVE_MCP, "needs the MCP SDK (run with the MCP venv)")
class RealServerTests(unittest.TestCase):
    def setUp(self):
        self.home = Path(tempfile.mkdtemp())
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("CM_", "CLAUDE_CODE_"))}
        env.update({"HOME": str(self.home), "CM_TUI_SESSION_ID": UID,
                    "CM_AGENT_ENGINE": "codex",
                    "CM_DAEMON_SOCKET": str(self.home / "no-daemon.sock"),
                    "CM_TUI_SOCKET": str(self.home / "no-tui.sock")})
        self.proc = subprocess.Popen(
            [sys.executable, str(HERE / "mcp_proxy.py"), "--", sys.executable, str(HERE / "server.py")],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            bufsize=0, env=env)
        self.send({"id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "t", "version": "0"}}})
        self.assertIn("result", self.reply(1))
        self.send({"method": "notifications/initialized"})
        self.send({"id": 2, "method": "tools/list"})
        self.assertIn("ack_reconnect", [t["name"] for t in self.reply(2)["result"]["tools"]])

    def tearDown(self):
        self.proc.stdin.close()
        self.proc.wait(30)
        self.proc.stdout.close()

    def send(self, message):
        self.proc.stdin.write((json.dumps({"jsonrpc": "2.0", **message}) + "\n").encode())

    def reply(self, rid, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            ready, _, _ = select.select([self.proc.stdout], [], [], max(0.1, deadline - time.time()))
            if ready:
                message = json.loads(self.proc.stdout.readline())
                if message.get("id") == rid and "method" not in message:
                    return message
        self.fail(f"no reply {rid}")

    def read(self, name):
        try:
            return json.loads((self.home / ".cm/mcp" / UID / name).read_text())
        except (OSError, ValueError):
            return None

    def wait_for(self, predicate, timeout=40):
        deadline = time.time() + timeout
        while time.time() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(0.1)
        self.fail("timed out")

    def test_activation_promotes_the_successor_and_ends_the_hold_on_ready(self):
        first = self.wait_for(lambda: self.read("status.json"))
        proxy = self.read("proxy.json")
        self.assertEqual(first["pid"], proxy["backend_pid"])
        self.assertEqual(first["proxy"], {"version": "2", "generation": 1})
        os.kill(self.proc.pid, signal.SIGUSR1)
        done = self.wait_for(lambda: (self.read("proxy.json") or {}).get("activations") == 1
                             and self.read("proxy.json"))
        last = done["last_activation"]
        self.assertTrue(last["ok"], last)
        self.assertLess(last["hold_ms"], 3000, "ready should end the hold well before its 5 s cap")
        second = self.wait_for(lambda: (self.read("status.json") or {}).get("pid") == last["to_pid"]
                               and self.read("status.json"))
        self.assertEqual(second["proxy"]["generation"], 2)
        self.assertGreaterEqual(second["updated_at"], last["at"])
        # The connection still works, served by the new backend.
        self.send({"id": 3, "method": "tools/call",
                   "params": {"name": "ack_reconnect", "arguments": {}}})
        self.assertIn("result", self.reply(3))


class StartupCostTests(unittest.TestCase):
    def test_a_session_without_the_proxy_does_not_hash_at_import(self):
        env = {k: v for k, v in os.environ.items() if not k.startswith("CM_MCP_")}
        code = ("import sys; sys.path.insert(0, %r); from mcp_server import activation;"
                "print(activation._loaded is None)" % str(HERE.parent))
        out = subprocess.run([sys.executable, "-c", code], env=env, capture_output=True, text=True)
        self.assertEqual(out.stdout.strip(), "True", out.stderr)
        env["CM_MCP_PROXY_CHILD"] = "1"
        out = subprocess.run([sys.executable, "-c", code], env=env, capture_output=True, text=True)
        self.assertEqual(out.stdout.strip(), "False", "behind the proxy the version is pinned at import")


if __name__ == "__main__":
    unittest.main()
