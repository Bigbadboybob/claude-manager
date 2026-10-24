"""mcp_proxy.py against a stdlib JSON-RPC backend (no MCP SDK needed).

doc/mcp-managed-activation.md describes the mechanism these tests pin."""

import json
import os
import select
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from pathlib import Path

PROXY = Path(__file__).resolve().parent.parent / "mcp_proxy.py"

# Behavior comes from the `version` file read at startup: "1"/"2" serve,
# "dead" exits before answering anything.
BACKEND = textwrap.dedent("""
    import json, os, sys, threading, time
    from pathlib import Path
    version = Path(sys.argv[1]).read_text().strip()
    if version == "dead":
        sys.exit(7)
    tools = ["probe_version", "slow", "ask", "ask_idle", "crash"] + (["probe_new_tool"] if version == "2" else [])
    def out(m):
        print(json.dumps({"jsonrpc": "2.0", **m}), flush=True)
    for line in sys.stdin:
        msg = json.loads(line)
        if "id" not in msg:
            continue
        method, result = msg["method"], None
        if method == "initialize":
            result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                      "serverInfo": {"name": "probe", "version": version}}
        elif method == "tools/list":
            result = {"tools": [{"name": n, "inputSchema": {"type": "object"}} for n in tools]}
        elif method == "tools/call":
            name = msg["params"]["name"]
            if name == "crash":
                os._exit(3)
            if name == "slow":
                time.sleep(1.5)
            if name == "ask":
                # Proxy-only traffic: must never reach the client.
                out({"method": "notifications/cm/status", "params": {"x": 1}})
                out({"method": "notifications/cm/activate", "params": {"mode": "ack"}})
            if name == "ask_idle":
                # A background policy decision, sent after this reply.
                threading.Timer(0.3, out, [{"method": "notifications/cm/activate",
                                            "params": {"mode": "idle"}}]).start()
            result = {"content": [{"type": "text",
                                   "text": f"version={version} pid={os.getpid()} "
                                           f"gen={os.environ.get('CM_MCP_PROXY_GENERATION')} "
                                           f"pred={os.environ.get('CM_MCP_PREDECESSOR_PID')}"}]}
        out({"id": msg["id"], "result": result})
""")


class ProxyTests(unittest.TestCase):
    env_extra: dict = {}

    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        (self.dir / "backend.py").write_text(BACKEND)
        self.version = self.dir / "version"
        self.version.write_text("1\n")
        self.home = self.dir / "home"
        self.home.mkdir()
        env = {**os.environ, "HOME": str(self.home), "CM_TUI_SESSION_ID": "ts-test-1",
               "CM_MCP_PROXY_IDLE_S": "0", "CM_MCP_PROXY_HOLD_RETRY_S": "1",
               **self.env_extra}
        self.proc = subprocess.Popen(
            [sys.executable, str(PROXY), "--", sys.executable,
             str(self.dir / "backend.py"), str(self.version)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
        self.seen = []
        self.send({"id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "t", "version": "0"}}})
        self.init = self.recv()
        self.send({"method": "notifications/initialized"})
        self.send({"id": 2, "method": "tools/list"})
        self.recv()

    def tearDown(self):
        self.proc.stdin.close()
        self.proc.wait(20)
        self.proc.stdout.close()
        leaked = [m for m in self.seen if str(m.get("method", "")).startswith("notifications/cm/")]
        self.assertEqual(leaked, [], "the proxy forwarded notifications/cm/* to the client")

    def send(self, message):
        self.proc.stdin.write((json.dumps({"jsonrpc": "2.0", **message}) + "\n").encode())
        self.proc.stdin.flush()

    def recv(self, timeout=15):
        ready, _, _ = select.select([self.proc.stdout], [], [], timeout)
        self.assertTrue(ready, "no message from the proxy")
        message = json.loads(self.proc.stdout.readline())
        self.seen.append(message)
        return message

    def call(self, rid, name="probe_version"):
        self.send({"id": rid, "method": "tools/call", "params": {"name": name, "arguments": {}}})

    def text(self, reply):
        return reply["result"]["content"][0]["text"]

    def status(self):
        return json.loads((self.home / ".cm/mcp/ts-test-1/proxy.json").read_text())

    def wait_status(self, state, timeout=15, min_generation=0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                status = self.status()
                if status["state"] == state and status["generation"] >= min_generation:
                    return self.status()
            except (OSError, ValueError):
                pass
            time.sleep(0.05)
        self.fail(f"proxy never reached {state}: {self.status()}")


class SwapTests(ProxyTests):
    def test_swap_keeps_the_connection_and_announces_new_tools(self):
        self.assertTrue(self.init["result"]["capabilities"]["tools"]["listChanged"])
        self.call(3)
        first = self.text(self.recv())
        self.version.write_text("2\n")
        os.kill(self.proc.pid, signal.SIGUSR1)
        self.assertEqual(self.recv()["method"], "notifications/tools/list_changed")
        self.call(4)
        second = self.text(self.recv())
        self.assertTrue(first.startswith("version=1") and second.startswith("version=2"))
        old_pid = first.split("pid=")[1].split()[0]
        self.assertIn(f"pred={old_pid}", second)  # the successor knows whom it replaced
        status = self.wait_status("running")
        self.assertEqual((status["activations"], status["proxy_version"]), (1, "1"))

    def test_a_call_in_flight_finishes_on_the_old_backend(self):
        self.call(3, "slow")
        time.sleep(0.2)
        self.version.write_text("2\n")
        os.kill(self.proc.pid, signal.SIGUSR1)
        reply = self.recv()
        self.assertEqual(reply["id"], 3)
        self.assertTrue(self.text(reply).startswith("version=1"))
        self.assertEqual(self.recv()["method"], "notifications/tools/list_changed")
        self.call(4)
        self.assertTrue(self.text(self.recv()).startswith("version=2"))

    def test_a_failed_preflight_leaves_the_old_backend_serving(self):
        self.version.write_text("dead\n")
        os.kill(self.proc.pid, signal.SIGUSR1)
        time.sleep(1.0)
        self.call(3)
        self.assertTrue(self.text(self.recv()).startswith("version=1"))
        self.assertFalse(self.status()["last_activation"]["ok"])

    def test_backend_requested_activation_waits_for_its_own_reply(self):
        # `ask` emits notifications/cm/* (never forwarded) and asks to activate.
        self.version.write_text("2\n")
        self.call(3, "ask")
        reply = self.recv()
        self.assertEqual(reply["id"], 3)
        self.assertTrue(self.text(reply).startswith("version=1"))
        self.assertEqual(self.recv()["method"], "notifications/tools/list_changed")
        self.call(4)
        self.assertTrue(self.text(self.recv()).startswith("version=2"))


class CrashTests(ProxyTests):
    def test_a_crash_fails_the_open_call_and_restarts(self):
        self.call(3)
        first = self.text(self.recv())
        self.call(4, "crash")
        reply = self.recv()
        self.assertEqual(reply["id"], 4)
        self.assertIn("restarted", reply["error"]["message"])
        self.wait_status("running", min_generation=2)
        self.call(5)
        second = self.text(self.recv())
        self.assertNotEqual(first.split("pid=")[1].split()[0], second.split("pid=")[1].split()[0])

    def test_crash_loop_holds_down_and_a_fixed_payload_recovers(self):
        self.version.write_text("dead\n")
        self.call(3, "crash")
        self.recv()
        status = self.wait_status("held_down")
        self.assertGreaterEqual(status["recent_crashes"], 3)
        self.call(4)
        reply = self.recv()
        self.assertIn("unavailable", reply["error"]["message"])
        held_generation = status["generation"]
        self.version.write_text("1\n")  # the fixed deploy
        self.wait_status("running", min_generation=held_generation + 1)
        self.call(5)
        self.assertTrue(self.text(self.recv()).startswith("version=1"))


class IdleModeTests(ProxyTests):
    env_extra = {"CM_MCP_PROXY_IDLE_S": "30"}

    def test_idle_request_is_refused_after_recent_client_traffic(self):
        self.version.write_text("2\n")
        self.call(3, "ask_idle")
        self.recv()
        time.sleep(1.0)
        self.call(4)
        self.assertTrue(self.text(self.recv()).startswith("version=1"))
        self.assertEqual(self.status()["activations"], 0)


class IdleModeHonoredTests(ProxyTests):
    def test_idle_request_swaps_when_quiet(self):
        self.version.write_text("2\n")
        self.call(3, "ask_idle")
        self.recv()
        self.assertEqual(self.recv()["method"], "notifications/tools/list_changed")
        self.call(4)
        self.assertTrue(self.text(self.recv()).startswith("version=2"))


if __name__ == "__main__":
    unittest.main()
