"""Opt-in Resume identity regression: private daemon, real Codex, mock model.

Usage: python integration_session_resume_identity.py --daemon-binary <cm-daemon>
Uses only a temporary HOME and test sockets. Never connects to a live CM daemon.
Requires the same aiohttp/pyte/websockets environment as integration_codex_resume.
"""
import argparse
import asyncio
import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[2]
ROOT = Path(tempfile.mkdtemp(prefix="cm-resume-identity-"))
os.environ["CM_CODEX_RESEARCH_ROOT"] = str(ROOT)
sys.path.insert(0, str(REPO / "doc/messaging/research/native-delivery-2026-09-07/codex"))
from harness import BIN, Mock, RPC, config, make_env

async def main(binary, mcp_server):
    env, work = make_env("identity")
    model = await Mock("identity").start()
    config(env, work, model.port)
    env.update(CM_CODEX_BIN=BIN, CM_OPERATOR_TOKEN="isolated-identity-token",
               CM_DAEMON_SOCKET=str(ROOT / "daemon.sock"))
    cm = Path(env["HOME"]) / ".cm"
    cm.mkdir()
    (cm / "daemon.toml").write_text("mcp_server_path = " + json.dumps(str(mcp_server)) + "\n")
    uid = "ts-aabbccdd-0"
    log = (ROOT / "daemon.log").open("wb")
    daemon = rpc = None

    def call(method, params=None, expect_ok=True, session_caller=False):
        body = json.dumps({"id": "fixture", "caller": {"session_uid": uid} if session_caller else {"token_id": "isolated-identity-token"},
                           "method": method, "params": params or {}}).encode()
        def exact(conn, n):
            data = b""
            while len(data) < n:
                part = conn.recv(n - len(data))
                if not part:
                    raise EOFError()
                data += part
            return data
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(10)
            conn.connect(env["CM_DAEMON_SOCKET"])
            conn.sendall(struct.pack(">I", len(body)) + body)
            result = json.loads(exact(conn, struct.unpack(">I", exact(conn, 4))[0]))
        if expect_ok:
            assert result.get("ok"), result
            return result["result"]
        return result

    async def start_daemon():
        proc = await asyncio.create_subprocess_exec(binary, env=env, cwd=work,
                                                   stdout=log, stderr=log, start_new_session=True)
        for _ in range(100):
            try:
                await asyncio.to_thread(call, "daemon.health")
                return proc
            except (FileNotFoundError, ConnectionRefusedError):
                assert proc.returncode is None
                await asyncio.sleep(.1)
        raise TimeoutError("private daemon failed to start")

    async def output_until(text):
        end = time.monotonic() + 45
        while time.monotonic() < end:
            result = await asyncio.to_thread(call, "read_session_output", {"session_uid": uid})
            if text in base64.b64decode(result["bytes"]).decode(errors="replace"):
                return
            await asyncio.sleep(.1)
        raise AssertionError(f"Missing {text}; inspect {ROOT}")

    async def close_session():
        await asyncio.to_thread(call, "kill_session", {"session_uid": uid})
        for _ in range(100):
            rows = await asyncio.to_thread(call, "list_sessions")
            if not any(r["session_uid"] == uid for r in rows):
                return
            await asyncio.sleep(.1)
        raise TimeoutError("private session did not exit")

    try:
        rpc = await RPC("identity", env, work).start()
        thread = await rpc.call("thread/start", {"cwd": str(work), "approvalPolicy": "never", "sandbox": "danger-full-access"})
        tid = thread["thread"]["id"]
        await rpc.call("turn/start", {"threadId": tid, "input": [{"type": "text", "text": "PRESERVE_HISTORY"}]})
        await rpc.wait_event("turn/completed")
        await rpc.close()
        rpc = None
        daemon = await start_daemon()
        await asyncio.to_thread(call, "session.revive", {
            "uid": uid, "workspace_id": "original-workspace", "worktree_path": str(work),
            "session_type": "codex", "label": "Winners", "transcript_id": tid,
            "task_id": "original-task", "managed_by_uid": "ts-parent-0", "global_perms": True,
        })
        await output_until("MOCK_RESPONSE_1")
        person_before = await asyncio.to_thread(call, "messaging.open", {}, True, True)
        preferences = {"uid": uid, "hidden": True, "color": "green", "idle_timeout_secs": 17,
                       "burst_threshold": 19, "notify_on_idle": True, "seeded_from_snapshot": "seed"}
        await asyncio.to_thread(call, "tui.update_sessions_snapshot", {"sessions": [], "preferences": [preferences]})
        resume = {"uid": "ts-deadbeef-0", "workspace_id": "wrong-workspace", "label": "wrong-name",
                  "engine": "codex", "task_id": "wrong-task", "resume_id": tid}
        conflict = await asyncio.to_thread(call, "session.resume", resume, False)
        assert not conflict["ok"] and conflict["error"]["code"] == "conflict", conflict
        assert len(await asyncio.to_thread(call, "list_sessions")) == 1
        await close_session()
        daemon.terminate()
        await asyncio.wait_for(daemon.wait(), 5)
        # Force resolution exclusively from the durable identity archive, as
        # after row closure / bounded tombstone eviction / a fresh viewer.
        for file in ["daemon-tombstones.json", "tui-sessions.json"]:
            (cm / file).unlink(missing_ok=True)
        daemon = await start_daemon()
        result = await asyncio.to_thread(call, "session.resume", resume)
        assert result["identity_preserved"] and result["session_uid"] == uid, result
        assert result["workspace_id"] == "original-workspace", result
        person_after = await asyncio.to_thread(call, "messaging.open", {}, True, True)
        assert person_after["actor_id"] == person_before["actor_id"]
        entry = result["entry"]
        for field, value in preferences.items():
            assert entry[field] == value, (field, entry)
        assert entry["label"] == "Winners" and entry["task_id"] == "original-task"
        assert entry["managed_by_uid"] == "ts-parent-0" and entry["global_perms"]
        await output_until("MOCK_RESPONSE_1")
        await asyncio.sleep(2)
        await asyncio.to_thread(call, "send_input", {"session_uid": uid, "text": "AFTER_RESUME", "submit": True})
        await output_until("MOCK_RESPONSE_2")
        transport = json.loads((cm / "notifications" / hashlib.sha256(uid.encode()).hexdigest() / "transport.json").read_text())
        assert transport["thread_id"] == tid
        print(json.dumps({"ok": True, "same_uid": uid, "same_thread": tid, "same_chat_identity": person_after["actor_id"], "running_rejected": True,
                          "history_and_next_turn": True, "closed_row_and_daemon_restart": True,
                          "task_parent_permissions_and_preferences_preserved": True, "fixture": str(ROOT)}, indent=2))
    finally:
        if rpc:
            await rpc.close()
        if daemon and daemon.returncode is None:
            try:
                await close_session()
            except Exception:
                pass
            daemon.terminate()
            try:
                await asyncio.wait_for(daemon.wait(), 5)
            except asyncio.TimeoutError:
                daemon.kill()
                await daemon.wait()
        log.close()
        await model.close()

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--daemon-binary", required=True, type=lambda p: str(Path(p).resolve()))
    parser.add_argument("--mcp-server", type=Path, default=REPO / "mcp_server/server.py")
    options = parser.parse_args()
    asyncio.run(main(options.daemon_binary, options.mcp_server))
