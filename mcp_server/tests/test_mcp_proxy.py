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
    here = Path(sys.argv[1]).parent
    with open(here / "pids", "a") as f:
        f.write(f"{os.getpid()}\\n")
    version = Path(sys.argv[1]).read_text().strip()
    if version == "dead":
        sys.exit(7)
    startup = here / "startup"
    if startup.exists():
        time.sleep(float(startup.read_text()))
    tools = ["probe_version", "slow", "long", "ask", "ask_idle", "askclient", "huge", "crash"]
    if version == "2":
        tools.append("probe_new_tool")
    lock = threading.Lock()
    asking = []

    def out(m):
        with lock:
            print(json.dumps({"jsonrpc": "2.0", **m}), flush=True)

    def text(t):
        return {"content": [{"type": "text", "text": t}]}

    me = (f"version={version} pid={os.getpid()} gen={os.environ.get('CM_MCP_PROXY_GENERATION')} "
          f"pred={os.environ.get('CM_MCP_PREDECESSOR_PID')} "
          f"predstart={os.environ.get('CM_MCP_PREDECESSOR_START')}")
    with open(here / "starts", "a") as f:
        f.write(f"{os.getpid()} {open(f'/proc/{os.getpid()}/stat').read().rsplit(') ', 1)[1].split()[19]}\\n")

    def call(msg):
        # Each call runs on its own thread, as calls do in server.py.
        name = msg["params"]["name"]
        if name == "crash":
            os._exit(3)
        if name == "slow":
            time.sleep(1.5)
        if name == "long":
            time.sleep(float(msg["params"]["arguments"].get("s", 8)))
        if name == "huge":
            with lock:
                print("x" * (int(os.environ["CM_MCP_PROXY_LINE_LIMIT"]) * 2), flush=True)
            return
        if name == "askclient":
            asking.append(msg["id"])
            out({"id": f"srv-{os.getpid()}", "method": "roots/list"})
            return
        if name == "ask":
            # Proxy-only traffic: must never reach the client.
            out({"method": "notifications/cm/status", "params": {"x": 1}})
            out({"method": "notifications/cm/activate", "params": {"mode": "ack"}})
        if name == "ask_idle":
            # A background policy decision, sent after this reply.
            threading.Timer(0.3, out, [{"method": "notifications/cm/activate",
                                        "params": {"mode": "idle"}}]).start()
        out({"id": msg["id"], "result": text(me)})

    for line in sys.stdin:
        msg = json.loads(line)
        method = msg.get("method")
        if method is None:
            if asking and str(msg.get("id", "")).startswith("srv-"):
                out({"id": asking.pop(), "result": text("answered " + me)})
            continue
        if method == "notifications/initialized" and (here / "ready").exists():
            out({"method": "notifications/cm/ready"})
        if "id" not in msg:
            continue
        if method == "initialize":
            if version == "dieinit":
                sys.exit(5)
            out({"id": msg["id"], "result": {
                "protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                "serverInfo": {"name": "probe", "version": version}}})
        elif method == "tools/list":
            out({"id": msg["id"], "result": {"tools": [
                {"name": n, "inputSchema": {"type": "object"}} for n in tools]}})
        else:
            threading.Thread(target=call, args=(msg,), daemon=True).start()
""")


class ProxyTests(unittest.TestCase):
    env_extra: dict = {}

    initialize = True

    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        (self.dir / "backend.py").write_text(BACKEND)
        self.version = self.dir / "version"
        self.version.write_text("1\n")
        self.home = self.dir / "home"
        self.home.mkdir()
        env = {**os.environ, "HOME": str(self.home), "CM_TUI_SESSION_ID": "ts-test-1",
               "CM_MCP_PROXY_IDLE_S": "0", "CM_MCP_PROXY_HOLD_RETRY_S": "1",
               "CM_MCP_PROXY_HOLD_MAX_S": "0.5", "CM_MCP_PROXY_QUIET_WAIT_S": "5",
               **self.env_extra}
        self.proc = subprocess.Popen(
            [sys.executable, str(PROXY), "--", sys.executable,
             str(self.dir / "backend.py"), str(self.version)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0,
            stderr=None if os.environ.get("PROXY_TEST_STDERR") else subprocess.DEVNULL, env=env)
        self.seen = []
        if not self.initialize:
            return
        self.send({"id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "t", "version": "0"}}})
        self.init = self.recv()
        self.send({"method": "notifications/initialized"})
        self.send({"id": 2, "method": "tools/list"})
        self.recv()

    def tearDown(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        self.proc.wait(30)
        try:
            self.proc.stdout.close()
        except OSError:
            pass
        deadline = time.time() + 10
        while any(self.alive(p) for p in self.pids()) and time.time() < deadline:
            time.sleep(0.1)
        self.assertEqual([p for p in self.pids() if self.alive(p)], [],
                         "the proxy left backend processes behind")
        leaked = [m for m in self.seen if str(m.get("method", "")).startswith("notifications/cm/")]
        self.assertEqual(leaked, [], "the proxy forwarded notifications/cm/* to the client")

    def send(self, message):
        self.proc.stdin.write((json.dumps({"jsonrpc": "2.0", **message}) + "\n").encode())
        self.proc.stdin.flush()

    def recv(self, timeout=15):
        # stdout is unbuffered (bufsize=0), so select() sees every pending
        # line; a buffered reader could hide a second line from select().
        ready, _, _ = select.select([self.proc.stdout], [], [], timeout)
        self.assertTrue(ready, "no message from the proxy")
        message = json.loads(self.proc.stdout.readline())
        self.seen.append(message)
        return message

    def call(self, rid, name="probe_version", **arguments):
        self.send({"id": rid, "method": "tools/call", "params": {"name": name, "arguments": arguments}})

    def replies(self, ids, timeout=20):
        """Collect replies for `ids` (in any order); other messages are kept in self.seen."""
        got = {}
        deadline = time.time() + timeout
        while set(ids) - set(got):
            if time.time() > deadline:
                self.fail(f"no reply for {sorted(set(ids) - set(got))}; got {got}; "
                          f"status {self.status()}")
            message = self.recv(max(0.1, deadline - time.time()))
            if message.get("id") in ids and "method" not in message:
                got[message["id"]] = message
        return got

    def pids(self):
        try:
            return [int(p) for p in (self.dir / "pids").read_text().split()]
        except OSError:
            return []

    @staticmethod
    def alive(pid):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        try:
            return Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1][0] != "Z"
        except OSError:
            return False

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
        self.assertEqual((status["activations"], status["proxy_version"]), (1, "2"))

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

    def test_a_crash_restart_names_the_dead_child_by_pid_and_start(self):
        # The dead child is reaped before the restart; its start time must
        # still reach the successor or monitor adoption is skipped.
        self.call(3)
        first = self.text(self.recv())
        dead = first.split("pid=")[1].split()[0]
        self.call(4, "crash")
        self.recv()
        self.wait_status("running", min_generation=2)
        self.call(5)
        second = self.text(self.recv())
        starts = dict(line.split() for line in (self.dir / "starts").read_text().splitlines())
        self.assertIn(f"pred={dead} ", second)
        self.assertIn(f"predstart={starts[dead]}", second)

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


class ReviewTests(ProxyTests):
    env_extra = {"CM_MCP_PROXY_LINE_LIMIT": "65536"}

    def test_requests_during_preflight_are_all_answered(self):
        # Review repro: ask -> activation; slow and probe arrive while the
        # candidate is still starting (1 s). Every one must be answered.
        (self.dir / "startup").write_text("1")
        self.version.write_text("2\n")
        self.call(3, "ask")
        self.call(4, "slow")
        self.call(5)
        got = self.replies([3, 4, 5])
        self.assertTrue(self.text(got[3]).startswith("version=1"))
        self.assertTrue(self.text(got[4]).startswith("version=1"))
        self.wait_status("running", min_generation=2)
        self.call(6)
        self.assertTrue(self.text(self.replies([6])[6]).startswith("version=2"))

    def test_a_long_call_during_preflight_aborts_without_holding_input(self):
        (self.dir / "startup").write_text("1")
        self.version.write_text("2\n")
        os.kill(self.proc.pid, signal.SIGUSR1)
        time.sleep(0.2)
        self.call(3, "long", s=9)  # reaches the old child while the candidate starts
        time.sleep(0.5)
        t0 = time.time()
        self.call(4)
        self.assertTrue(self.text(self.replies([4], timeout=3)[4]).startswith("version=1"))
        self.assertLess(time.time() - t0, 2, "input was held behind the long call")
        deadline = time.time() + 15
        while time.time() < deadline and not (self.status().get("last_activation") or {}).get("error"):
            time.sleep(0.1)
        self.assertIn("busy", self.status()["last_activation"]["error"])
        self.assertEqual(self.status()["activations"], 0)
        candidates = self.pids()[1:]
        self.assertTrue(candidates and not any(self.alive(p) for p in candidates),
                        "the abandoned candidate must be killed")
        self.assertTrue(self.text(self.replies([3], timeout=15)[3]).startswith("version=1"))

    def test_eof_during_an_activation_leaves_no_process(self):
        (self.dir / "startup").write_text("3")
        os.kill(self.proc.pid, signal.SIGUSR1)
        time.sleep(0.5)
        self.proc.stdin.close()
        self.proc.wait(20)  # tearDown asserts every backend is gone

    def test_an_old_child_crash_while_waiting_recovers(self):
        self.call(3, "long", s=3)
        time.sleep(0.2)
        os.kill(self.proc.pid, signal.SIGUSR1)  # waits for call 3
        time.sleep(0.2)
        self.call(4, "crash")
        got = self.replies([3, 4])
        self.assertIn("restarted", got[4]["error"]["message"])
        self.wait_status("running", min_generation=2)
        self.call(5)
        self.assertTrue(self.text(self.replies([5])[5]).startswith("version=1"))

    def test_a_client_answer_goes_to_the_backend_that_asked(self):
        self.call(3, "askclient")
        request = self.recv()
        self.assertEqual(request["method"], "roots/list")
        self.version.write_text("2\n")
        os.kill(self.proc.pid, signal.SIGUSR1)  # must wait for this exchange
        time.sleep(0.5)
        self.send({"id": request["id"], "result": {"roots": []}})
        self.assertTrue(self.text(self.replies([3])[3]).startswith("answered version=1"))
        self.wait_status("running", min_generation=2)

    def test_an_oversized_backend_line_fails_loudly_and_recovers(self):
        self.call(3, "huge")
        reply = self.replies([3])[3]
        self.assertIn("restarted", reply["error"]["message"])
        status = self.wait_status("running", min_generation=2)
        self.assertIn("over 65536 bytes", status["last_error"] or "x over 65536 bytes")
        self.call(4)
        self.assertTrue(self.text(self.replies([4])[4]).startswith("version=1"))

    def test_a_broken_engine_pipe_shuts_the_proxy_down(self):
        self.proc.stdout.close()
        self.call(3)
        self.proc.wait(20)

    def test_ready_ends_the_hold_early(self):
        (self.dir / "ready").write_text("")
        self.version.write_text("2\n")
        os.kill(self.proc.pid, signal.SIGUSR1)
        self.assertEqual(self.recv()["method"], "notifications/tools/list_changed")
        last = self.wait_status("running", min_generation=2)["last_activation"]
        self.assertTrue(last["ok"])
        self.assertLess(last["hold_ms"], 400)


class InitializeTests(ProxyTests):
    initialize = False

    def test_initialize_pending_when_the_backend_dies_is_answered(self):
        self.version.write_text("dieinit\n")
        self.send({"id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "t", "version": "0"}}})
        reply = self.recv()
        self.assertEqual(reply["id"], 1)
        self.assertIn("initialize", reply["error"]["message"])


if __name__ == "__main__":
    unittest.main()
