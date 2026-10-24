"""Prototype checks for mcp_proxy.py (doc/mcp-managed-activation.md).

Uses a stdlib JSON-RPC backend, so it needs no MCP SDK."""

import json
import os
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from pathlib import Path

PROXY = Path(__file__).resolve().parent.parent / "mcp_proxy.py"

BACKEND = textwrap.dedent("""
    import json, os, sys, time
    from pathlib import Path
    version = Path(sys.argv[1]).read_text().strip()
    tools = ["probe_version"] + (["probe_new_tool"] if version == "2" else [])
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
            if msg["params"]["name"] == "slow":
                time.sleep(1.5)
            result = {"content": [{"type": "text",
                                   "text": f"version={version} pid={os.getpid()}"}]}
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
""")


class ProxySwapTests(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        (self.dir / "backend.py").write_text(BACKEND)
        self.version = self.dir / "version"
        self.version.write_text("1\n")
        self.proc = subprocess.Popen(
            [sys.executable, str(PROXY), "--", sys.executable,
             str(self.dir / "backend.py"), str(self.version)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.send({"id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "t", "version": "0"}}})
        self.init = self.recv()
        self.send({"method": "notifications/initialized"})

    def tearDown(self):
        self.proc.stdin.close()
        self.proc.wait(10)
        self.proc.stdout.close()

    def send(self, message):
        self.proc.stdin.write((json.dumps({"jsonrpc": "2.0", **message}) + "\n").encode())
        self.proc.stdin.flush()

    def recv(self):
        return json.loads(self.proc.stdout.readline())

    def call(self, rid, name="probe_version"):
        self.send({"id": rid, "method": "tools/call", "params": {"name": name, "arguments": {}}})

    def text(self, reply):
        return reply["result"]["content"][0]["text"]

    def test_swap_keeps_the_connection_and_announces_new_tools(self):
        self.assertTrue(self.init["result"]["capabilities"]["tools"]["listChanged"])
        self.send({"id": 2, "method": "tools/list"})
        self.assertEqual([t["name"] for t in self.recv()["result"]["tools"]], ["probe_version"])
        self.call(3)
        first = self.text(self.recv())
        self.version.write_text("2\n")
        os.kill(self.proc.pid, signal.SIGUSR1)
        self.assertEqual(self.recv()["method"], "notifications/tools/list_changed")
        self.call(4)
        second = self.text(self.recv())
        self.assertTrue(first.startswith("version=1") and second.startswith("version=2"))
        self.assertNotEqual(first.split("pid=")[1], second.split("pid=")[1])

    def test_a_call_in_flight_finishes_on_the_old_backend(self):
        self.send({"id": 2, "method": "tools/list"})
        self.recv()
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
        (self.dir / "backend.py").write_text("import sys; sys.exit(3)\n")
        os.kill(self.proc.pid, signal.SIGUSR1)
        time.sleep(1.0)
        self.call(2)
        self.assertTrue(self.text(self.recv()).startswith("version=1"))


if __name__ == "__main__":
    unittest.main()
