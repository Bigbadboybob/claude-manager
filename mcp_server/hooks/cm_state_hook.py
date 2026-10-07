#!/usr/bin/env python3
"""Fail-open Claude state telemetry. No stdout and no permission decisions.

The state hooks double-fork before IPC so even a synchronous/older hook runner
returns promptly. Stop imports report() directly and retains its inbox contract.
"""

from __future__ import annotations

import json
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))

EVENTS = {
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "PermissionRequest",
    "Notification",
}
MAX_INPUT = 2 * 1024 * 1024
REPORT_TIMEOUT = 3.0


def text(value, limit=4096):
    if not isinstance(value, str):
        return None
    return value.encode("utf-8", errors="replace")[:limit].decode(
        "utf-8", errors="ignore"
    )


def background(payload: dict, now: float) -> dict:
    tasks, crons = payload.get("background_tasks"), payload.get("session_crons")
    complete = isinstance(tasks, list) and isinstance(crons, list)
    jobs, schedules = [], []
    seen_jobs, seen_crons = set(), set()
    for task in tasks if isinstance(tasks, list) else []:
        if not isinstance(task, dict):
            complete = False
            continue
        if task.get("status") in ("completed", "failed", "killed", "stopped"):
            continue
        kind, ident = task.get("type"), text(task.get("id"), 256)
        if (
            kind not in ("shell", "subagent", "monitor", "workflow")
            or not ident
            or ident != task.get("id")
            or ident in seen_jobs
        ):
            complete = False
            continue
        seen_jobs.add(ident)
        if len(jobs) >= 256:
            complete = False
            continue
        jobs.append(
            {
                "id": ident,
                "kind": kind,
                "label": text(task.get("description") or task.get("command"), 512)
                or ident,
                "first_seen_at": now,
                "wakes_agent": True,
            }
        )
    for cron in crons if isinstance(crons, list) else []:
        if not isinstance(cron, dict):
            complete = False
            continue
        ident, schedule = text(cron.get("id"), 256), text(cron.get("schedule"), 256)
        recurring = cron.get("recurring")
        if (
            not ident
            or ident != cron.get("id")
            or not schedule
            or not isinstance(recurring, bool)
            or ident in seen_crons
        ):
            complete = False
            continue
        seen_crons.add(ident)
        if len(schedules) >= 256:
            complete = False
            continue
        schedules.append({"id": ident, "schedule": schedule, "recurring": recurring})
    return {"complete": complete, "observed_at": now, "jobs": jobs, "crons": schedules}


def normalize(
    payload: dict, *, observed_at: float, continuing: bool = False
) -> dict | None:
    event = payload.get("hook_event_name")
    # Child hooks share CM's environment, but cannot open/close the main turn.
    if not isinstance(event, str) or event not in EVENTS or payload.get("agent_id"):
        return None
    body = {"observed_at": observed_at}
    for key in (
        "transcript_path",
        "prompt_id",
        "source",
        "tool_name",
        "notification_type",
        "error_details",
        "last_assistant_message",
    ):
        value = text(payload.get(key), 256 if key in {"prompt_id", "source"} else 4096)
        if value:
            body[key] = value
    if event == "Stop":
        body["continuing"] = continuing
        body["stop_hook_active"] = payload.get("stop_hook_active") is True
        body["background"] = background(payload, observed_at)
    elif event == "StopFailure":
        body["error_kind"] = text(payload.get("error")) or "unknown"
    elif event == "PermissionRequest":
        body["waiting_for"] = "permission"
    # Keep even a worst-case Unicode/control-character payload under the
    # daemon's 256 KiB report cap. Dropped entries never imply complete data.
    bg = body.get("background")
    while bg and len(json.dumps(body, ensure_ascii=False).encode()) > 240 * 1024:
        bg["complete"] = False
        entries = bg["jobs"] or bg["crons"]
        if not entries:
            break
        entries.pop()
    return {"kind": "hook", "event": event, "payload": body}


def report(
    uid: str, payload: dict, *, observed_at: float, continuing: bool = False
) -> None:
    """Send a bounded self-report; only an unknown method enables Stop fallback."""
    try:
        from mcp_server import control_client

        params = normalize(payload, observed_at=observed_at, continuing=continuing)
        if not params:
            return
        params["session_uid"] = uid
        deadline = time.monotonic() + REPORT_TIMEOUT
        try:
            control_client.call("session.agent_report", params, timeout=REPORT_TIMEOUT)
        except control_client.ControlError as exc:
            if params["event"] != "Stop" or exc.code not in {
                "unknown_method",
                "method_not_found",
                -32601,
            }:
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            legacy = {"session_uid": uid, "continuing": continuing}
            if params["payload"].get("transcript_path"):
                legacy["transcript_path"] = params["payload"]["transcript_path"]
            control_client.call("session.turn_ended", legacy, timeout=remaining)
    except Exception:  # never wedge a prompt, stop, error or permission decision
        pass


def detach_report(uid: str, payload: dict, observed_at: float) -> None:
    child = os.fork()
    if child:
        os.waitpid(child, 0)  # reap the trampoline, never wait for daemon IPC
        return
    try:
        os.setsid()
        with open(os.devnull, "rb+", buffering=0) as sink:
            for fd in (0, 1, 2):
                os.dup2(sink.fileno(), fd)
        if os.fork():
            os._exit(0)
        report(uid, payload, observed_at=observed_at)
    except BaseException:
        pass
    finally:
        os._exit(0)


def main() -> int:
    try:
        observed_at = time.time()
        uid = os.environ.get("CM_TUI_SESSION_ID", "").strip()
        raw = sys.stdin.buffer.read(MAX_INPUT + 1)
        if not uid or len(raw) > MAX_INPUT:
            return 0
        payload = json.loads(raw)
        if (
            not isinstance(payload, dict)
            or not isinstance(payload.get("hook_event_name"), str)
            or payload["hook_event_name"] not in EVENTS
            or payload.get("agent_id")
        ):
            return 0
        detach_report(uid, payload, observed_at)
    except Exception:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
