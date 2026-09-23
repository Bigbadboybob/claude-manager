#!/usr/bin/env python3
"""Reap the checkouts of tasks that have been closed, one command per batch.

The `worktree.cleanup` RPC is a three-step dance — preview with a
caller-generated UUID, inspect the candidates, then apply under the same id —
and getting it wrong silently retains work instead of reaping it. `/triage-review`
closes tasks in batches of thirty, so it needs this as one call, not thirty
hand-rolled dances.

Reaping is the DEFAULT for a task this tool is given (Owner ruling 2026-09-18:
"for triage-review I would by default delete unless I say otherwise"); pass only
the tasks you actually closed, and leave out any you want to keep a checkout for.

The job is durable and detached daemon-side: it closes the task's own remaining
sessions, waits out their exit, then removes the checkouts, preserving branches
and archiving artifacts. This tool does not need to stay alive for that, so
`--wait` is only about how long you want to watch. Anything still settling is
reported as such and finishes on its own.

    scripts/reap_tasks.py --host cm-manager <task-id> [<task-id> ...]
    scripts/reap_tasks.py --host cm-manager --dry-run <task-id>   # preview only
"""
from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import time
import uuid

HERE = pathlib.Path(__file__).resolve().parent
CM_OP = HERE / "cm-op"
#: Deployed helper, for when this runs ON the host rather than across ssh.
DEPLOYED_CM_OP = pathlib.Path.home() / ".cm/docs/continuous-tasks/scripts/cm-op"
PREVIEW_TIMEOUT = 180
#: Terminal phases. `complete` can still mean RETAINED — read every result's
#: `removed` flag and message rather than trusting the phase.
DONE_PHASES = {"complete", "failed", "error"}


def helper() -> str:
    return str(CM_OP if CM_OP.exists() else DEPLOYED_CM_OP)


def rpc(host: str | None, params: dict) -> dict:
    cmd = [helper()]
    if host and host != "local":
        cmd += ["--ssh", host]
    cmd += ["worktree.cleanup", json.dumps(params)]
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=300)
    try:
        payload = json.loads(out.stdout)
    except json.JSONDecodeError:
        raise SystemExit(f"worktree.cleanup returned non-JSON: {out.stdout[:200]} {out.stderr[:200]}")
    if payload.get("error"):
        raise SystemExit(f"worktree.cleanup error: {json.dumps(payload['error'])[:300]}")
    return payload.get("result", payload)


def poll(host: str | None, job_id: str, want: set[str], timeout: float) -> dict:
    deadline = time.monotonic() + timeout
    state: dict = {}
    while True:
        state = rpc(host, {"action": "status", "id": job_id})
        if state.get("phase") in want or time.monotonic() >= deadline:
            return state
        time.sleep(3)


def reap(host: str | None, task_id: str, dry_run: bool, wait: float) -> dict:
    job_id = str(uuid.uuid4())
    rpc(host, {"action": "preview", "id": job_id, "task_id": task_id})
    state = poll(host, job_id, {"preview"} | DONE_PHASES, PREVIEW_TIMEOUT)
    candidates = state.get("candidates") or []
    summary = {"task_id": task_id, "job_id": job_id,
               "preview": [{"path": c["path"], "reason": c.get("reason")} for c in candidates]}
    if state.get("phase") != "preview":
        summary["error"] = f"preview did not settle (phase={state.get('phase')})"
        return summary
    if not candidates:
        summary["note"] = "no tracked checkout"
        return summary
    if dry_run:
        summary["note"] = "dry run; not applied"
        return summary
    rpc(host, {"action": "apply", "id": job_id})
    state = poll(host, job_id, DONE_PHASES, wait)
    summary["phase"] = state.get("phase")
    summary["closed_sessions"] = state.get("closed_sessions") or []
    summary["results"] = [{"path": r["path"], "removed": bool(r.get("removed")),
                           "message": r.get("message", "")} for r in (state.get("results") or [])]
    if state.get("phase") not in DONE_PHASES:
        # Not a failure: the daemon-side job keeps going without us.
        summary["note"] = f"still settling after {wait:.0f}s; job continues detached (status id {job_id})"
    return summary


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("task_ids", nargs="+")
    ap.add_argument("--host", default="cm-manager", help="owning host; 'local' for this one")
    ap.add_argument("--dry-run", action="store_true", help="preview only, never remove")
    ap.add_argument("--wait", type=float, default=90.0, help="seconds to watch each job before moving on")
    args = ap.parse_args()

    reaped = retained = 0
    report = []
    for task_id in args.task_ids:
        try:
            summary = reap(args.host, task_id, args.dry_run, args.wait)
        except SystemExit as exc:  # keep going; one bad task must not strand the batch
            summary = {"task_id": task_id, "error": str(exc)}
        report.append(summary)
        head = f"{task_id[:8]} "
        if summary.get("error"):
            print(head + f"ERROR {summary['error']}")
            continue
        if summary.get("note") and not summary.get("results"):
            print(head + f"{summary['note']} " + ", ".join(
                f"{c['reason']}" for c in summary.get("preview", [])))
            continue
        for result in summary.get("results", []):
            name = result["path"].rsplit("/", 1)[-1][:52]
            if result["removed"]:
                reaped += 1
                print(head + f"reaped {name}")
            else:
                retained += 1
                print(head + f"RETAINED {name}: {result['message'][:90]}")
        if summary.get("note"):
            print(head + summary["note"])
    print(f"\n{reaped} checkout(s) reaped, {retained} retained, {len(args.task_ids)} task(s) requested.")
    print("Branches and archived artifacts are preserved in every case.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
