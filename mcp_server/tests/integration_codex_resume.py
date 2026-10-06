"""Opt-in Codex CLI resume regression using a private HOME and local mock model.

Run with aiohttp, pyte and websockets installed. No real credentials, model
requests, CM daemons or user transcripts are used. --expect-rejection captures
the pre-fix failure; --legacy-argv checks compatibility with an older viewer.
"""

import argparse
import asyncio
import fcntl
import json
import os
import pty
import shutil
import shlex
import struct
import sys
import tempfile
import termios
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path[:0] = [
    str(REPO),
    str(REPO / "doc/messaging/research/native-delivery-2026-09-07/codex"),
]
os.environ.setdefault("CM_CODEX_RESEARCH_ROOT", tempfile.mkdtemp(prefix="cm-resume-"))
os.environ.setdefault("CM_CODEX_BIN", shutil.which("codex") or "codex")

from harness import BIN, ROOT, RPC, Mock, config, make_env
from tui_fixture import TUI


class LauncherTUI(TUI):
    async def start(self):
        # Unlike the research helper, preserve the exact launcher's argv:
        # CM's resume ID must remain its final positional argument.
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 120, 0, 0))

        def setup():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        self.p = await asyncio.create_subprocess_exec(
            sys.executable, *self.args, env=self.env, cwd=self.work,
            stdin=slave, stdout=slave, stderr=slave, preexec_fn=setup,
        )
        os.close(slave)
        os.set_blocking(self.master, False)
        self.reader = asyncio.create_task(self.read())
        return self


async def main(options):
    case = "resume"
    env, work = make_env(case)
    model = await Mock(case).start()
    config(env, work, model.port)
    config_path = Path(env["CODEX_HOME"]) / "config.toml"
    config_path.write_text(config_path.read_text().replace(
        'sandbox_mode = "danger-full-access"', 'sandbox_mode = "read-only"'
    ))
    if options.cm_policy:
        policy_path = Path(env["HOME"]) / ".cm/codex-permissions.json"
        policy_path.parent.mkdir(parents=True, exist_ok=True)
        policy_path.write_text(json.dumps({"mode": "full-access-auto-review"}))
    env["CM_CODEX_BIN"] = BIN
    rpc = tui = None
    result = {"binary": BIN, "legacy_argv": options.legacy_argv}
    try:
        # Approval policy comes from the saved thread. Codex 0.154 resolves
        # sandbox from the new backend's config, so keep that restrictive too.
        # An old CM argv must not widen either through a backend override.
        thread = None
        if not options.fresh:
            rpc = await RPC(case, env, work).start()
            seed = await rpc.call("thread/start", {
                "cwd": str(work), "modelProvider": "mock",
                "approvalPolicy": "never" if options.cm_policy or options.live_repair else "on-request",
                "sandbox": "read-only",
            })
            thread = seed["thread"]["id"]
            await rpc.call("turn/start", {
                "threadId": thread,
                "input": [{"type": "text", "text": "HISTORY_MUST_SURVIVE_RESTART"}],
            })
            await rpc.wait_event("turn/completed")
            await rpc.close()
            rpc = None

        engine_args = [] if options.fresh else ["resume"]
        if options.legacy_argv or options.fresh:
            engine_args.append("--dangerously-bypass-approvals-and-sandbox")
        engine_args.append("--no-alt-screen")
        if thread:
            engine_args.append(thread)
        absent = str(work / "absent.sock")
        tui = await LauncherTUI(case, env, work, args=(
            str(REPO / "mcp_server/native_codex.py"),
            "--session-uid", "ts-resume-fixture", "--cm-env",
            json.dumps({"CM_DAEMON_SOCKET": absent, "CM_TUI_SOCKET": absent}),
            "--", *engine_args,
        )).start()
        if options.expect_rejection:
            await asyncio.wait_for(tui.p.wait(), 20)
            assert b"Permission overrides are not supported when resuming a remote task" in tui.raw, tui.visible()
            result["reproduced_permission_rejection"] = True
        else:
            await tui.wait_text("gpt-5.6-sol" if options.fresh else "MOCK_RESPONSE_1", 25)
            await tui.send("FRESH_SESSION" if options.fresh else "CONTINUE_AFTER_RESTART")
            await tui.send("\r")
            await tui.wait_text("MOCK_RESPONSE_1" if options.fresh else "MOCK_RESPONSE_2", 25)
            records = []
            for path in Path(env["CODEX_HOME"]).glob("sessions/**/*.jsonl"):
                records.extend(json.loads(line) for line in path.read_text().splitlines())
            contexts = [r["payload"] for r in records if r.get("type") == "turn_context"]
            assert len(contexts) >= (1 if options.fresh else 2), contexts
            last = contexts[-1]
            result["resumed_permissions"] = {k: last.get(k) for k in ("approval_policy", "sandbox_policy", "permission_profile")}
            expected_approval = "on-request" if options.cm_policy or not (options.fresh or options.live_repair) else "never"
            assert last["approval_policy"] == expected_approval, last
            assert last["sandbox_policy"]["type"] == ("danger-full-access" if options.fresh or options.cm_policy else "read-only"), last
            if options.cm_policy:
                assert last["approvals_reviewer"] == "auto_review", last
            from mcp_server.notifications import Queue
            status = Queue("ts-resume-fixture", Path(env["HOME"]) / ".cm").snapshot()
            if thread:
                assert status["transport"]["thread_id"] == thread, status
                result.update(thread_id_preserved=True, history_preserved=True, resumed_turn_completed=True)
            else:
                result["fresh_yolo_session_completed"] = True
            if options.live_repair:
                from ws_appserver_tui import WSRPC
                # Discover only this fixture launcher's owning backend.
                def descendants(pid):
                    p = Path("/proc") / str(pid)
                    args = (p / "cmdline").read_bytes().decode().split("\0")
                    if Path(args[0]).name == "codex" and "--listen" in args:
                        yield args[args.index("--listen") + 1][7:]
                    for child in (p / "task" / str(pid) / "children").read_text().split():
                        yield from descendants(int(child))
                sockets = list(descendants(tui.p.pid))
                assert len(sockets) == 1, sockets
                operator = await WSRPC(case, env, work, sockets[0], "operator").start()
                try:
                    assert thread in (await operator.call("thread/loaded/list"))["data"]
                    await operator.call("thread/settings/update", {
                        "threadId": thread, "approvalPolicy": "on-request",
                        "approvalsReviewer": "auto_review", "permissions": ":danger-full-access",
                    })
                    io_target = Path(env["HOME"]) / "outside-workspace-canary.txt"
                    code = (
                        "import urllib.request,pathlib; "
                        f"assert urllib.request.urlopen('http://127.0.0.1:{model.port}/health',timeout=5).status == 200; "
                        f"pathlib.Path({str(io_target)!r}).write_text('network-and-write-ok'); "
                        "print('CM_PERMISSION_IO_OK')"
                    )
                    sent_tool = False
                    async def repair_reply(request, data, number):
                        nonlocal sent_tool
                        if not sent_tool:
                            sent_tool = True
                            return await model.reply(request, tool={
                                "name": "exec", "namespace": "functions",
                                "input": 'text(await tools.exec_command(' + json.dumps({
                                    "cmd": "python3 -c " + shlex.quote(code),
                                    "yield_time_ms": 1000, "login": False,
                                }) + '));',
                            })
                        return await model.reply(request, "MOCK_LIVE_REPAIR_DONE")
                    model.handler = repair_reply
                    # The ordinary remote TUI must adopt the broadcast settings,
                    # rather than overwrite them with its cached old policy.
                    await asyncio.sleep(.2)
                    await tui.send("AFTER_LIVE_PERMISSION_REPAIR")
                    await tui.send("\r")
                    await tui.wait_text("MOCK_LIVE_REPAIR_DONE", 25)
                    assert io_target.read_text() == "network-and-write-ok"
                    contexts = [r["payload"] for p in Path(env["CODEX_HOME"]).glob("sessions/**/*.jsonl")
                                for line in p.read_text().splitlines()
                                if (r := json.loads(line)).get("type") == "turn_context"]
                    assert contexts[-1]["approval_policy"] == "on-request", contexts[-1]
                    assert contexts[-1]["sandbox_policy"]["type"] == "danger-full-access", contexts[-1]
                    assert contexts[-1]["approvals_reviewer"] == "auto_review", contexts[-1]
                    assert tui.p.returncode is None
                    result["live_repair_adopted_by_remote_tui"] = True
                    result["live_repair_network_and_outside_workspace_write"] = True
                finally:
                    await operator.close()
            if options.cm_policy:
                from mcp_server.tests.test_native_notifications import eventually
                queue = Queue("ts-resume-fixture", Path(env["HOME"]) / ".cm")
                queue.publish("policy-wake", "fixture", "[policy-wake] verify continuous wake", "[policy-wake]")
                await eventually(lambda: queue.get("policy-wake")["status"] == "observed", 20)
                await tui.wait_text("MOCK_RESPONSE_2" if options.fresh else "MOCK_RESPONSE_3", 25)
                contexts = [r["payload"] for p in Path(env["CODEX_HOME"]).glob("sessions/**/*.jsonl")
                            for line in p.read_text().splitlines()
                            if (r := json.loads(line)).get("type") == "turn_context"]
                assert contexts[-1]["approval_policy"] == "on-request", contexts[-1]
                assert contexts[-1]["sandbox_policy"]["type"] == "danger-full-access", contexts[-1]
                assert contexts[-1]["approvals_reviewer"] == "auto_review", contexts[-1]
                result["native_wake_permissions_preserved"] = True
        print(json.dumps(result, indent=2), flush=True)
        (ROOT / case / "result.json").write_text(json.dumps(result, indent=2))
    finally:
        if tui:
            await tui.close()
        if rpc:
            await rpc.close()
        await model.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--legacy-argv", action="store_true")
    parser.add_argument("--expect-rejection", action="store_true")
    parser.add_argument("--fresh", action="store_true")
    parser.add_argument("--cm-policy", action="store_true")
    parser.add_argument("--live-repair", action="store_true")
    asyncio.run(main(parser.parse_args()))
