"""Opt-in real Claude + disposable holder/brain channel smoke test.

CM_CHANNEL_LIVE_ACCOUNT=1 opts into two tiny real model turns. Credentials are
copied into a private disposable config and removed in finally. No live CM
session/config is touched. Supply CM_NATIVE_BINARY_DIR for freshly built bins.
"""
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))
from mcp_server.notifications import Queue


def rpc(path, token, method, params=None):
    body = json.dumps({"id": uuid.uuid4().hex, "caller": {"token_id": token},
                       "method": method, "params": params or {}}).encode()
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(5)
        s.connect(str(path))
        s.sendall(struct.pack(">I", len(body)) + body)
        def exact(n):
            data = b""
            while len(data) < n:
                part = s.recv(n-len(data))
                if not part:
                    raise ConnectionError("fixture daemon disconnected")
                data += part
            return data
        result = json.loads(exact(struct.unpack(">I", exact(4))[0]))
        assert result["ok"], result
        return result["result"]


def wait(predicate, seconds=30):
    deadline = time.monotonic()+seconds
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(.1)
    raise AssertionError("condition timed out")


def main():
    assert os.environ.get("CM_CHANNEL_LIVE_ACCOUNT") == "1", "explicit live-account opt-in required"
    actual_home = Path.home()
    root = Path(tempfile.mkdtemp(prefix="cm-channel-holder-test-"))
    root.chmod(0o700)
    home = root/"home"; home.mkdir()
    config = home/".claude"; config.mkdir(mode=0o700)
    work = root/"work"; work.mkdir()
    actual = json.loads((actual_home/".claude.json").read_text())
    copied = {k: v for k, v in actual.items() if k.startswith("cached") or k in ("oauthAccount", "userID")}
    copied.update(hasCompletedOnboarding=True, theme="dark", projects={str(work): {"hasTrustDialogAccepted": True}})
    (config/".claude.json").write_text(json.dumps(copied))
    credential = config/".credentials.json"
    shutil.copy2(actual_home/".claude/.credentials.json", credential)
    credential.chmod(0o600)
    (home/".cm").mkdir()
    (home/".cm/daemon.toml").write_text(f'mcp_server_path = "{REPO}/mcp_server/server.py"\n')
    sock = root/"daemon.sock"
    token = "fixture-"+uuid.uuid4().hex
    uid = "ts-abcdef-123"
    sid = str(uuid.uuid4())
    env = {k:v for k,v in os.environ.items() if not k.startswith(("CM_", "CLAUDE", "ANTHROPIC", "DISABLE_"))}
    env.update(HOME=str(home), CLAUDE_CONFIG_DIR=str(config), DISABLE_AUTOUPDATER="1",
               CM_DAEMON_SOCKET=str(sock), CM_OPERATOR_TOKEN=token, ENABLE_TOOL_SEARCH="false")
    binary_dir = Path(os.environ["CM_NATIVE_BINARY_DIR"])
    log = (root/"holder.log").open("wb")
    holder = subprocess.Popen([str(binary_dir/"cm-holder"), "--brain", str(binary_dir/"cm-daemon")],
        env=env, cwd=work, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
    started = False
    q = Queue(uid, home/".cm")
    print("fixture", root, flush=True)
    call = lambda method, params=None: rpc(sock, token, method, params)
    try:
        wait(sock.exists)
        assert call("daemon.health")["split"]
        cm_env = {"HOME": str(home), "CM_DAEMON_SOCKET": str(sock), "CM_TUI_SOCKET": str(sock),
                  "CM_TUI_SESSION_ID": uid, "CM_AGENT_ENGINE": "claude-code", "CM_CLAUDE_CHANNEL": "1"}
        mcp = {"mcpServers": {"claude-manager": {"command": sys.executable,
                "args": [str(REPO/"mcp_server/server.py")], "env": cm_env}}}
        args = [os.environ.get("CM_CHANNEL_TEST_CLAUDE_BIN") or shutil.which("claude"), "--model", "haiku", "--session-id", sid,
                "--setting-sources", "", "--settings", json.dumps({"permissions": {"defaultMode": "bypassPermissions"}, "skipDangerousModePermissionPrompt": True}),
                "--strict-mcp-config", "--mcp-config", json.dumps(mcp), "--tools", "",
                "--system-prompt", "You are a CM notification transport test. Reply CM_TEST_DONE to each test notification and do nothing else. Never call tools.",
                "--debug-file", str(root/"claude.log"), "--dangerously-load-development-channels", "server:claude-manager"]
        # Publish before startup to verify the durable queue waits for MCP init.
        q.publish("before", "fixture", "[cm-test before] Reply CM_TEST_DONE only. Do nothing else.", "[cm-test before]")
        call("start_session", {"uid": uid, "workspace_id": "fixture", "worktree_path": str(work),
              "working_dir": str(work), "label": "channel-fixture", "session_type": "claude-code",
              "cols": 120, "rows": 36, "argv": args, "env": env, "skip_task_status_promotion": True})
        started = True
        wait(lambda: "accepted own CM channel startup confirmation" in (root/"holder.log").read_text(), 30)
        wait(lambda: any((config/"projects").rglob(sid+".jsonl")), 20)
        transcript = next((config/"projects").rglob(sid+".jsonl"))
        call("session.set_transcript_path", {"session_uid": uid, "transcript_path": str(transcript)})
        wait(lambda: q.get("before")["status"] == "observed", 30)
        assert q.get("before")["receipt"]["kind"] == "channel_write_only"
        assert "Channel notifications registered" in (root/"claude.log").read_text()
        def responses():
            return [json.loads(line) for p in (config/"projects").rglob(sid+".jsonl")
                    for line in p.read_text().splitlines() if line.strip()]
        wait(lambda: any(x.get("type")=="assistant" and "CM_TEST_DONE" in json.dumps(x.get("message")) for x in responses()), 40)
        before = call("daemon.health")
        claude_pid = next(int(child) for child in Path(f"/proc/{holder.pid}/task/{holder.pid}/children").read_text().split()
                          if sid.encode() in Path(f"/proc/{child}/cmdline").read_bytes().split(b"\0"))
        claude_start = Path(f"/proc/{claude_pid}/stat").read_text().rsplit(") ",1)[1].split()[19]
        # Kill ONLY this verified disposable brain. Holder-owned Claude stays.
        brain = before["brain_pid"]
        assert ("HOME="+str(home)).encode() in Path(f"/proc/{brain}/environ").read_bytes().split(b"\0")
        os.kill(brain, signal.SIGKILL)
        def restarted():
            try:
                return call("daemon.health")["holder_epoch"] == before["holder_epoch"]+1
            except (OSError, ConnectionError):
                return False
        wait(restarted, 20)
        assert Path(f"/proc/{claude_pid}/stat").read_text().rsplit(") ",1)[1].split()[19] == claude_start
        q.publish("after", "fixture", "[cm-test after] Reply CM_TEST_DONE only. Do nothing else.", "[cm-test after]")
        wait(lambda: q.get("after")["status"] == "observed", 30)
        wait(lambda: sum(x.get("type")=="assistant" and "CM_TEST_DONE" in json.dumps(x.get("message")) for x in responses()) >= 2, 40)
        assert (root/"holder.log").read_text().count("accepted own CM channel startup confirmation") == 1
        print(json.dumps({"automatic_scoped_confirmation": True, "native_channel_receipts": True,
                          "actual_model_responses": 2, "same_claude_pid_after_brain_restart": True,
                          "confirmation_not_replayed_after_restart": True}), flush=True)
    finally:
        if started:
            try: call("kill_session", {"session_uid": uid})
            except Exception: pass
        if holder.poll() is None:
            holder.terminate()
            try: holder.wait(6)
            except subprocess.TimeoutExpired: holder.kill(); holder.wait()
        log.close()
        credential.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
