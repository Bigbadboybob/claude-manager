"""Opt-in real-client test with a local mock model. Never uses live sessions."""

import sys, os, json, asyncio, time
from pathlib import Path

repo = Path(__file__).resolve().parents[2]
sys.path[:0] = [
    str(repo),
    str(repo / "doc/messaging/research/native-delivery-2026-09-07/codex"),
]
import tempfile

os.environ["CM_CODEX_RESEARCH_ROOT"] = tempfile.mkdtemp(prefix="cm-native-codex-test-")
import shutil

os.environ.setdefault("CM_CODEX_BIN", shutil.which("codex") or "codex")
from harness import *
import tui_fixture as tf
from mcp_server.notifications import Queue


async def main():
    case = "codex-wrapper-2"
    env, work = make_env(case)
    mock = await Mock(case).start()
    config(env, work, mock.port)
    env["CM_CODEX_BIN"] = os.environ["CM_CODEX_BIN"]

    async def reply(req, data, n):
        return await mock.reply(
            req,
            "NATIVE_NOTIFICATION_OK"
            if "[cm-test probe-one]" in json.dumps(data)
            else "BASELINE_OK",
        )

    mock.handler = reply
    tf.BIN = sys.executable
    tui = await tf.TUI(
        case,
        env,
        work,
        args=(
            str(repo / "mcp_server/native_codex.py"),
            "--session-uid",
            "ts-native-fixture",
            "--cm-env",
            json.dumps({"CM_DAEMON_SOCKET": str(work / "absent.sock")}),
            "--",
            "--dangerously-bypass-approvals-and-sandbox",
        ),
    ).start()
    q = Queue("ts-native-fixture", Path(env["HOME"]) / ".cm")
    try:
        await asyncio.sleep(3)
        print("INITIAL", tui.snapshot("initial"), flush=True)
        await tui.send("BASELINE")
        await tui.send("\r")
        await tui.wait_text("BASELINE_OK", 20)
        await tui.send("UNSENT_DRAFT")
        before = tui.snapshot("draft-before")
        start = time.time()
        q.publish(
            "probe-one",
            "test",
            "[cm-test probe-one] Automated CM fixture wake",
            "[cm-test probe-one]",
        )
        await tui.wait_text("NATIVE_NOTIFICATION_OK", 15)
        await asyncio.sleep(0.4)
        print(
            "RESULT",
            json.dumps(
                {
                    "latency_s": time.time() - start,
                    "queue": q.snapshot(),
                    "draft_preserved": "UNSENT_DRAFT" in tui.snapshot("draft-after"),
                    "draft_in_model": any(
                        "UNSENT_DRAFT" in json.dumps(r.get("body"))
                        for r in mock.requests
                    ),
                }
            ),
            flush=True,
        )
    finally:
        await tui.close()
        await mock.close()


asyncio.run(main())
