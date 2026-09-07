"""Opt-in real holder/daemon/Codex, isolated local model and disposable HOME.

Proves native notification ownership survives a brain SIGKILL with the same
session, backend, and transcript. Only this fixture's verified brain is killed.
"""

import asyncio
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import struct
import sys
import tempfile
import time
import uuid

REPO = Path(__file__).resolve().parents[2]
sys.path[:0] = [
    str(REPO),
    str(REPO / "doc/messaging/research/native-delivery-2026-09-07/codex"),
]
os.environ["CM_CODEX_RESEARCH_ROOT"] = tempfile.mkdtemp(prefix="cm-native-holder-")
os.environ.setdefault("CM_CODEX_BIN", shutil.which("codex") or "codex")
from harness import BIN, Mock, ROOT, config, make_env
from mcp_server.notifications import Queue
from mcp_server.tests.test_native_notifications import eventually


def rpc(path, token, method, params=None):
    body = json.dumps(
        {
            "id": uuid.uuid4().hex,
            "caller": {"token_id": token},
            "method": method,
            "params": params or {},
        }
    ).encode()
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(3)
        connection.connect(str(path))
        connection.sendall(struct.pack(">I", len(body)) + body)

        def exact(n):
            data = b""
            while len(data) < n:
                part = connection.recv(n - len(data))
                if not part:
                    raise ConnectionError("fixture control connection closed")
                data += part
            return data

        result = json.loads(exact(struct.unpack(">I", exact(4))[0]))
        assert result["ok"], result.get("error")
        return result["result"]


async def main():
    case = "holder"
    env, work = make_env(case)
    model = await Mock(case).start()
    config(env, work, model.port)
    directory = ROOT / case
    home = Path(env["HOME"])
    daemon_socket = directory / "daemon.sock"
    token = "isolated-native-holder-" + uuid.uuid4().hex
    env.update(
        CM_DAEMON_SOCKET=str(daemon_socket), CM_OPERATOR_TOKEN=token, CM_CODEX_BIN=BIN
    )
    binary_dir = Path(
        os.environ.get(
            "CM_NATIVE_BINARY_DIR", str(Path.home() / ".cm/shared-target/debug")
        )
    )
    log = (directory / "holder.log").open("wb")
    holder = await asyncio.create_subprocess_exec(
        str(binary_dir / "cm-holder"),
        "--brain",
        str(binary_dir / "cm-daemon"),
        env=env,
        cwd=work,
        stdin=asyncio.subprocess.DEVNULL,
        stdout=log,
        stderr=log,
    )
    uid = "ts-abcdef-99"
    queue = Queue(uid, home / ".cm")
    started = False

    async def call(method, params=None):
        return await asyncio.to_thread(rpc, daemon_socket, token, method, params)

    try:
        await eventually(lambda: daemon_socket.exists(), 15)
        health = await call("daemon.health")
        assert health["split"] is True
        cm_env = {
            "CM_DAEMON_SOCKET": str(daemon_socket),
            "CM_TUI_SOCKET": str(daemon_socket),
            "CM_TUI_SESSION_ID": uid,
        }
        await call(
            "start_session",
            {
                "uid": uid,
                "workspace_id": "ws-native-fixture",
                "worktree_path": str(work),
                "label": uid,
                "session_type": "codex",
                "working_dir": str(work),
                "cols": 120,
                "rows": 35,
                "argv": [
                    sys.executable,
                    str(REPO / "mcp_server/native_codex.py"),
                    "--session-uid",
                    uid,
                    "--cm-env",
                    json.dumps(cm_env),
                    "--",
                    "--dangerously-bypass-approvals-and-sandbox",
                ],
                "env": env,
            },
        )
        started = True
        await eventually(lambda: queue.snapshot()["transport"].get("connected"), 25)
        queue.publish(
            "before",
            "fixture",
            "[fixture before] native wake before brain restart",
            "[fixture before]",
        )
        await eventually(lambda: queue.get("before")["status"] == "observed", 20)
        before = queue.snapshot()["transport"]
        brain = (await call("daemon.health"))["brain_pid"]
        environ = Path(f"/proc/{brain}/environ").read_bytes().split(b"\0")
        assert ("HOME=" + str(home)).encode() in environ
        assert ("CM_DAEMON_SOCKET=" + str(daemon_socket)).encode() in environ
        os.kill(brain, signal.SIGKILL)
        deadline = time.monotonic() + 20
        while True:
            try:
                after_health = await call("daemon.health")
                if after_health["brain_pid"] != brain:
                    break
            except (OSError, ConnectionError, AssertionError):
                pass
            assert time.monotonic() < deadline, "isolated brain did not restart"
            await asyncio.sleep(0.1)
        queue.publish(
            "after",
            "fixture",
            "[fixture after] native wake after brain restart",
            "[fixture after]",
        )
        await eventually(lambda: queue.get("after")["status"] == "observed", 20)
        after = queue.snapshot()["transport"]
        assert before["pid"] == after["pid"]
        assert before["thread_id"] == after["thread_id"]
        resolved = await call("resolve_authorized_session", {"session_uid": uid})
        assert resolved["transcript_path"] == before["transcript_path"]
        owned = {before["pid"]}
        frontier = list(owned)
        while frontier:
            parent = frontier.pop()
            for task in Path(f"/proc/{parent}/task").glob("*/children"):
                try:
                    children = [int(pid) for pid in task.read_text().split()]
                except FileNotFoundError:
                    continue
                for child in children:
                    if child not in owned:
                        owned.add(child)
                        frontier.append(child)
        await call("kill_session", {"session_uid": uid})
        started = False
        await eventually(lambda: not Path(f"/proc/{before['pid']}").exists(), 10)

        def all_stopped():
            for pid in owned:
                try:
                    if (
                        Path(f"/proc/{pid}/stat")
                        .read_text()
                        .rsplit(") ", 1)[1]
                        .split()[0]
                        != "Z"
                    ):
                        return False
                except FileNotFoundError:
                    pass
            return True

        await eventually(all_stopped, 10)
        print(
            json.dumps(
                {
                    "brain_restarted": True,
                    "launcher_survived": True,
                    "same_thread_and_transcript": True,
                    "native_wakes_before_and_after": True,
                    "session_kill_terminated_launcher": True,
                    "owned_descendants_stopped": True,
                }
            ),
            flush=True,
        )
    finally:
        if started:
            try:
                await call("kill_session", {"session_uid": uid})
            except Exception:
                pass
        if holder.returncode is None:
            holder.terminate()
            try:
                await asyncio.wait_for(holder.wait(), 6)
            except asyncio.TimeoutError:
                holder.kill()
                await holder.wait()
        log.close()
        await model.close()


if __name__ == "__main__":
    asyncio.run(main())
