"""Claude telemetry normalization and detached, silent, fail-open transport."""

from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import threading
from unittest.mock import patch

import pytest

from mcp_server import control_client
from mcp_server.hooks import cm_state_hook as hook

FIXTURES = Path(__file__).parent / "fixtures" / "claude-hooks"
SCRIPT = Path(hook.__file__)


def fixture(event):
    return json.loads((FIXTURES / f"{event}.json").read_text())


def test_stop_fixture_is_shared_with_rust():
    report = hook.normalize(fixture("Stop"), observed_at=1000.0)
    assert report == fixture("Stop.normalized")
    assert "PRIVATE" not in json.dumps(report)


@pytest.mark.parametrize(
    "event,expected",
    [
        ("UserPromptSubmit", {"source": "user", "prompt_id": "prompt-1"}),
        (
            "StopFailure",
            {
                "error_kind": "rate_limit",
                "error_details": "Usage window exhausted.",
                "last_assistant_message": "Retry later.",
            },
        ),
        ("PermissionRequest", {"waiting_for": "permission", "tool_name": "Bash"}),
        ("Notification", {"notification_type": "permission_prompt"}),
    ],
)
def test_event_metadata_without_raw_inputs(event, expected):
    report = hook.normalize(fixture(event), observed_at=1000.0)
    assert report["event"] == event
    assert report["kind"] == "hook"
    assert report["payload"]["observed_at"] == 1000.0
    assert report["payload"]["transcript_path"] == fixture(event)["transcript_path"]
    for key, value in expected.items():
        assert report["payload"][key] == value
    assert "PRIVATE" not in json.dumps(report)


@pytest.mark.parametrize("event", sorted(hook.EVENTS))
def test_subagents_cannot_report_main_turn_edges(event):
    payload = {
        **fixture(event),
        "agent_id": "child-agent",
        "agent_type": "general-purpose",
    }
    assert hook.normalize(payload, observed_at=1000.0) is None
    with patch.object(control_client, "call") as call:
        hook.report("ts-test", payload, observed_at=1000.0)
    call.assert_not_called()


@pytest.mark.parametrize("event", ["PreToolUse", None, [], {}])
def test_unsupported_events_are_ignored(event):
    assert hook.normalize({"hook_event_name": event}, observed_at=1000.0) is None


def test_unicode_text_is_bounded_by_utf8_bytes():
    payload = fixture("StopFailure")
    for key in (
        "error",
        "error_details",
        "last_assistant_message",
        "transcript_path",
        "prompt_id",
        "source",
    ):
        payload[key] = "🐈" * 5000
    report = hook.normalize(payload, observed_at=1000.0)["payload"]
    for key, value in report.items():
        if isinstance(value, str):
            assert len(value.encode()) <= (
                256 if key in {"prompt_id", "source"} else 4096
            )
    assert "\ufffd" not in json.dumps(report, ensure_ascii=False)


@pytest.mark.parametrize(
    "change",
    [
        {"background_tasks": None},
        {"session_crons": None},
        {"background_tasks": [None]},
        {"background_tasks": [{"type": ["shell"], "id": "bad"}]},
        {"background_tasks": [{"type": "new-engine-kind", "id": "bad"}]},
        {"background_tasks": [{"type": "shell", "id": "x" * 257}]},
        {"session_crons": [{"id": "x", "schedule": "* * * * *", "recurring": "yes"}]},
        {
            "session_crons": [
                {"id": "x" * 257, "schedule": "* * * * *", "recurring": True}
            ]
        },
    ],
)
def test_unavailable_or_unrepresentable_jobs_never_claim_complete(change):
    bg = hook.normalize({**fixture("Stop"), **change}, observed_at=1000.0)["payload"][
        "background"
    ]
    assert bg["complete"] is False


def test_duplicates_terminal_entries_and_empty_complete_enumeration():
    payload = fixture("Stop")
    payload["background_tasks"].append(copy.deepcopy(payload["background_tasks"][0]))
    payload["session_crons"].append(copy.deepcopy(payload["session_crons"][0]))
    bg = hook.background(payload, 1000.0)
    assert not bg["complete"]
    assert len(bg["jobs"]) == 4
    assert len(bg["crons"]) == 1
    payload["background_tasks"] = [
        dict(job, status="completed") for job in payload["background_tasks"]
    ]
    payload["session_crons"] = []
    assert hook.background(payload, 1000.0) == {
        "complete": True,
        "observed_at": 1000.0,
        "jobs": [],
        "crons": [],
    }


def test_large_escaped_payload_stays_below_rpc_cap_and_marks_incomplete():
    payload = fixture("Stop")
    payload["background_tasks"] = [
        {"type": "shell", "id": str(i), "description": "\x00" * 600} for i in range(300)
    ]
    payload["session_crons"] = [
        {"id": str(i), "schedule": "\x00" * 300, "recurring": True} for i in range(300)
    ]
    report = hook.normalize(payload, observed_at=1000.0)
    assert len(json.dumps(report, ensure_ascii=False).encode()) < 256 * 1024
    bg = report["payload"]["background"]
    assert not bg["complete"]
    assert len(bg["jobs"]) <= 256 and len(bg["crons"]) <= 256


@pytest.mark.parametrize(
    "event", ["UserPromptSubmit", "StopFailure", "PermissionRequest", "Notification"]
)
def test_non_stop_never_falls_back_to_turn_ended(event):
    with patch.object(
        control_client,
        "call",
        side_effect=control_client.ControlError("unknown_method", "old"),
    ) as call:
        hook.report("ts-test", fixture(event), observed_at=1000.0)
    assert call.call_count == 1
    assert call.call_args.args[0] == "session.agent_report"


def environment(home, uid="ts-test"):
    env = dict(
        os.environ,
        HOME=str(home),
        CM_DAEMON_SOCKET=str(home / "daemon.sock"),
        CM_TUI_SOCKET=str(home / "absent-tui.sock"),
        PYTHONDONTWRITEBYTECODE="1",
    )
    env.pop("CM_TUI_SESSION_ID", None)
    if uid is not None:
        env["CM_TUI_SESSION_ID"] = uid
    return env


@pytest.mark.parametrize(
    "raw,uid",
    [
        ("{bad", "ts-test"),
        ("null", "ts-test"),
        ("[]", "ts-test"),
        ('{"hook_event_name": []}', "ts-test"),
        ('{"hook_event_name": "UserPromptSubmit"}', None),
        ('{"hook_event_name": "PermissionRequest", "agent_id": "child"}', "ts-test"),
        ('{"hook_event_name": "PreToolUse"}', "ts-test"),
        ('{"hook_event_name": "UserPromptSubmit"}', "ts-test"),  # absent socket
        ("x" * (hook.MAX_INPUT + 1), "ts-test"),
    ],
    ids=[
        "malformed",
        "null",
        "array",
        "bad-event",
        "no-uid",
        "subagent",
        "unknown-event",
        "no-daemon",
        "oversized",
    ],
)
def test_executable_fails_open_without_output(tmp_path, raw, uid):
    result = subprocess.run(
        [sys.executable, str(SCRIPT)],
        input=raw,
        text=True,
        capture_output=True,
        env=environment(tmp_path, uid),
        timeout=5,
    )
    assert (result.returncode, result.stdout, result.stderr) == (0, "", "")


def test_double_fork_returns_before_daemon_reply_and_closes_output_pipes(tmp_path):
    """A real blocked RPC cannot retain the hook caller or its captured pipes."""
    release, received = threading.Event(), threading.Event()
    requests, errors = [], []
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
        listener.bind(str(tmp_path / "daemon.sock"))
        listener.listen(1)
        listener.settimeout(5)

        def serve():
            try:
                with listener.accept()[0] as conn:
                    conn.settimeout(5)
                    with conn.makefile("rb") as stream:
                        size = struct.unpack(">I", stream.read(4))[0]
                        requests.append(json.loads(stream.read(size)))
                    received.set()
                    assert release.wait(5)
                    response = json.dumps(
                        {"id": requests[0]["id"], "ok": True, "result": {"ok": True}}
                    ).encode()
                    conn.sendall(struct.pack(">I", len(response)) + response)
                    # The grandchild must finish and close the socket, too.
                    assert conn.recv(1) == b""
            except Exception as exc:
                errors.append(exc)

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        try:
            # 2s is a generous CI bound, below the 3s report timeout. Timing the
            # usual <50ms return is a benchmark; this checks the IPC dependency.
            result = subprocess.run(
                [sys.executable, str(SCRIPT)],
                input=json.dumps(fixture("PermissionRequest")),
                text=True,
                capture_output=True,
                env=environment(tmp_path),
                timeout=2,
            )
            assert (result.returncode, result.stdout, result.stderr) == (0, "", "")
            assert received.wait(2)
            assert requests[0]["method"] == "session.agent_report"
            assert requests[0]["params"]["session_uid"] == "ts-test"
            assert requests[0]["params"]["payload"]["waiting_for"] == "permission"
        finally:
            release.set()
            worker.join(timeout=6)
        assert not worker.is_alive()
        assert not errors
