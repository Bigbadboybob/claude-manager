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
                "approvalPolicy": "on-request", "sandbox": "read-only",
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
            assert last["approval_policy"] == ("never" if options.fresh else "on-request"), last
            assert last["sandbox_policy"]["type"] == ("danger-full-access" if options.fresh else "read-only"), last
            from mcp_server.notifications import Queue
            status = Queue("ts-resume-fixture", Path(env["HOME"]) / ".cm").snapshot()
            if thread:
                assert status["transport"]["thread_id"] == thread, status
                result.update(thread_id_preserved=True, history_preserved=True, resumed_turn_completed=True)
            else:
                result["fresh_yolo_session_completed"] = True
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
    asyncio.run(main(parser.parse_args()))
