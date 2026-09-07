"""Opt-in real-client test with a local mock model. Never uses live sessions."""

import sys, os, json, time, socket, struct, threading
from pathlib import Path

repo = Path(__file__).resolve().parents[2]
import tempfile

os.environ["CM_RESEARCH_OUTPUT"] = tempfile.mkdtemp(prefix="cm-native-claude-test-")
sys.path[:0] = [
    str(repo),
    str(repo / "doc/messaging/research/native-delivery-2026-09-07/claude"),
]
from claude_harness import Mock, Client, wait, ROOT
from mcp_server.notifications import Queue

mode = sys.argv[1] if len(sys.argv) > 1 else "idle"
case = "claude-production-mcp-" + mode
root = ROOT / case
root.mkdir(parents=True, exist_ok=True)
uid = "ts-native-claude-fixture"
control = str(root / "control.sock")
Path(control).unlink(missing_ok=True)
sock = socket.socket(socket.AF_UNIX)
sock.bind(control)
sock.listen()
sock.settimeout(0.2)
stop = threading.Event()
client = None
calls = []


def serve():
    while not stop.is_set():
        try:
            conn, _ = sock.accept()
        except socket.timeout:
            continue
        except OSError:
            return
        with conn:
            try:
                n = struct.unpack(">I", conn.recv(4))[0]
                data = b""
                while len(data) < n:
                    data += conn.recv(n - len(data))
                req = json.loads(data)
                calls.append(req["method"])
                path = (
                    next(
                        (root / "config" / "projects").rglob(client.session + ".jsonl"),
                        None,
                    )
                    if client
                    else None
                )
                result = {
                    "state": "ready",
                    "engine": "claude-code",
                    "transcript_path": str(path) if path else None,
                    "idle": True,
                }
                payload = json.dumps(
                    {"id": req["id"], "ok": True, "result": result}
                ).encode()
                conn.sendall(struct.pack(">I", len(payload)) + payload)
            except Exception:
                pass


thread = threading.Thread(target=serve, daemon=True)
thread.start()
cm_env = {
    "CM_TUI_SESSION_ID": uid,
    "CM_DAEMON_SOCKET": control,
    "CM_TUI_SOCKET": control,
    "HOME": str(root / "home"),
}
mcp = {
    "mcpServers": {
        "claude-manager": {
            "command": str(repo / ".venv/bin/python"),
            "args": [str(repo / "mcp_server/server.py")],
            "env": cm_env,
        }
    }
}
from claude_cases import tool_reply, main_requests

release = root / "release"
settings = {}
reply = None
if mode == "active":
    reply = tool_reply(
        "Bash",
        {
            "command": f"while test ! -f {release}; do sleep 0.1; done; echo ACTIVE_TOOL_DONE",
            "description": "Isolated controlled command",
        },
    )
elif mode == "approval":
    reply = tool_reply(
        "Bash",
        {
            "command": "printf CM_APPROVAL_TEST",
            "description": "Harmless isolated approval test",
        },
    )
    settings["permissions"] = {"defaultMode": "default", "ask": ["Bash"]}
mock = Mock(case, reply)
client = Client(case, mock, settings=settings, extra=["--mcp-config", json.dumps(mcp)])
q = Queue(uid, root / "home" / ".cm")


def contains(text):
    return any(text in json.dumps(r["body"].get("messages", [])) for r in mock.requests)


try:
    assert wait(lambda: q.snapshot()["transport"].get("connected"), 20), q.snapshot()
    client.send("BASELINE\r")
    assert wait(lambda: contains("BASELINE"), 15)
    time.sleep(1)
    if mode == "idle":
        client.send("UNSENT_NATIVE_CLAUDE_DRAFT")
        time.sleep(0.2)
    start = time.time()
    q.publish(
        "native-one",
        "fixture",
        "[cm-test native-one] Automated CM fixture notification",
        "[cm-test native-one]",
    )
    if mode in ("active", "approval"):
        assert wait(lambda: q.get("native-one")["status"] == "submitted", 5), (
            q.snapshot()
        )
        time.sleep(0.3)
        assert not contains("[cm-test native-one]"), (
            "notification interrupted the foreground command or approval"
        )
        if mode == "active":
            release.write_text("go")
        else:
            import re

            plain = re.sub(
                r"\x1b\[[0-9;?]*[A-Za-z]", "", client.buf.decode(errors="replace")
            )
            assert "Doyouwanttoproceed?" in "".join(plain.split()), (
                "approval prompt disappeared"
            )
            client.send("\r")
    assert wait(lambda: contains("[cm-test native-one]"), 8), q.snapshot()
    latency = time.time() - start
    assert wait(lambda: q.get("native-one")["status"] == "observed", 5), q.snapshot()
    result = {
        "mode": mode,
        "latency_s": latency,
        "status": q.get("native-one")["status"],
        "control_methods": sorted(set(calls)),
    }
    if mode == "idle":
        draft_sent = contains("UNSENT_NATIVE_CLAUDE_DRAFT")
        client.send("\r")
        assert wait(lambda: contains("UNSENT_NATIVE_CLAUDE_DRAFT"), 8)
        result.update(draft_not_submitted=not draft_sent, draft_preserved=True)
    else:
        result["checkpoint_or_approval_preserved"] = True
    print(json.dumps(result), flush=True)

finally:
    client.close()
    mock.close()
    stop.set()
    sock.close()
    thread.join(1)
