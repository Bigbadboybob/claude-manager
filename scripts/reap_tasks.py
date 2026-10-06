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

The whole batch runs as one pipeline (2026-10-04: the old one-task-at-a-time
loop paid a ~25 s host-wide preview scan plus the full `--wait` for EVERY task,
so 33 tasks needed ~30 min and printed nothing before the caller's timeout
killed it). Previews run a few at a time on the host, applies are queued the
moment their preview lands, and every round of submits and status reads is
ONE ssh round trip (`cm-op --batch`) with a per-call timeout. Each task's
line is printed (unbuffered) as soon as its answer is known; every task gets
exactly one outcome: reaped / RETAINED + reason / still settling + status id /
ERROR.

    scripts/reap_tasks.py --host cm-manager <task-id> [<task-id> ...]
    scripts/reap_tasks.py --host cm-manager --dry-run <task-id> ...   # preview only
"""
from __future__ import annotations

import argparse
import dataclasses
import json
import pathlib
import subprocess
import sys
import time
import uuid
from collections.abc import Callable

HERE = pathlib.Path(__file__).resolve().parent
CM_OP = HERE / "cm-op"
#: Deployed helper, for when this runs ON the host rather than across ssh.
DEPLOYED_CM_OP = pathlib.Path.home() / ".cm/docs/continuous-tasks/scripts/cm-op"
#: Per-task bound on a preview scan, counted from its submission.
PREVIEW_TIMEOUT = 180.0
#: Previews in flight at once. Each one is a host-wide inventory scan (git over
#: every checkout); more at once only contend for the same disk and CPU.
PREVIEW_CONCURRENCY = 2  # 6 overloaded cm-manager task-state lookups (2026-10-05: 11/11 previews timed out at 6, all clean at 2)
#: Bound on one remote round trip (ssh + every RPC it carries). A hung call
#: fails that round with a clear error; the next round retries it.
CALL_TIMEOUT = 90.0
#: Bound on each daemon RPC inside a round.
RPC_TIMEOUT = 20.0
POLL_SECS = 3.0
PROGRESS_SECS = 30.0
#: Terminal phases. `complete` can still mean RETAINED — read every result's
#: `removed` flag and message rather than trusting the phase.
DONE_PHASES = {"complete", "failed", "error"}


class RemoteError(Exception):
    """A whole round trip failed (ssh, helper, timeout); nothing in it answered."""


def helper() -> str:
    return str(CM_OP if CM_OP.exists() else DEPLOYED_CM_OP)


def rpc_batch(host: str | None, calls: list[dict], timeout: float = CALL_TIMEOUT) -> list[dict]:
    """Run many `worktree.cleanup` calls in one helper invocation.

    Returns one entry per call, in order: the RPC result dict, or
    ``{"error": "<message>"}`` for a call the daemon refused, with
    ``"transient": True`` when the call never got an answer (timeout, dead
    socket) and is worth retrying.
    Raises RemoteError when the round trip as a whole failed."""
    if not calls:
        return []
    cmd = [helper()]
    if host and host != "local":
        cmd += ["--ssh", host]
    cmd += ["--timeout", str(RPC_TIMEOUT), "--batch", json.dumps([["worktree.cleanup", c] for c in calls])]
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, check=False)
    except subprocess.TimeoutExpired:
        raise RemoteError(f"remote call to {host or 'local'} timed out after {timeout:g}s") from None
    try:
        payloads = json.loads(out.stdout)
    except json.JSONDecodeError:
        raise RemoteError(f"helper returned non-JSON (exit {out.returncode}): "
                          f"{out.stdout[:200]!r} {out.stderr.strip()[:200]!r}") from None
    if not isinstance(payloads, list) or len(payloads) != len(calls):
        raise RemoteError(f"helper returned {type(payloads).__name__} for {len(calls)} call(s): {out.stdout[:200]!r}")
    results = []
    for payload in payloads:
        error = payload.get("error") if isinstance(payload, dict) else "malformed response"
        if error:
            message = error.get("message") if isinstance(error, dict) else None
            transient = isinstance(error, dict) and bool(error.get("transport"))
            results.append({"error": str(message or json.dumps(error))[:300], "transient": transient})
        else:
            result = payload.get("result", payload)
            results.append(result if isinstance(result, dict) else {"error": f"unexpected result {result!r}"[:300]})
    return results


@dataclasses.dataclass
class Job:
    task_id: str
    job_id: str = dataclasses.field(default_factory=lambda: str(uuid.uuid4()))
    #: pending → scanning → (applying →) done
    stage: str = "pending"
    preview_started: float | None = None
    apply_submitted: bool = False
    state: dict = dataclasses.field(default_factory=dict)
    last_error: str | None = None
    summary: dict | None = None

    @property
    def done(self) -> bool:
        return self.summary is not None


def preview_rows(state: dict) -> list[dict]:
    return [{"path": c.get("path", "?"), "reason": c.get("reason")} for c in state.get("candidates") or []]


def finish(job: Job, **summary) -> None:
    job.stage = "done"
    job.summary = {"task_id": job.task_id, "job_id": job.job_id, "preview": preview_rows(job.state), **summary}


def apply_summary(job: Job, note: str | None = None) -> None:
    state = job.state
    extra: dict = {"phase": state.get("phase"), "closed_sessions": state.get("closed_sessions") or [],
                   "results": [{"path": r.get("path", "?"), "removed": bool(r.get("removed")),
                                "message": r.get("message", "")} for r in state.get("results") or []]}
    if state.get("phase") in ("failed", "error"):
        extra["error"] = f"cleanup job {state.get('phase')}: {state.get('message', '')} (status id {job.job_id})"
    if note:
        extra["note"] = note
    finish(job, **extra)


def on_preview_state(job: Job, state: dict, dry_run: bool) -> None:
    job.state = state
    phase = state.get("phase")
    if phase in DONE_PHASES:
        finish(job, error=f"preview ended in phase={phase}: {state.get('message', '')} (status id {job.job_id})")
    elif phase == "preview":
        if not state.get("candidates"):
            finish(job, note="no tracked checkout")
        elif dry_run:
            finish(job, note="dry run; not applied")
        else:
            job.stage = "applying"


def run(host: str | None, task_ids: list[str], dry_run: bool, wait: float, *,
        call: Callable[[str | None, list[dict]], list[dict]] = rpc_batch,
        clock: Callable[[], float] = time.monotonic, sleep: Callable[[float], None] = time.sleep,
        emit: Callable[[Job], None] = lambda job: None, progress: Callable[[str], None] = lambda line: None,
        concurrency: int = PREVIEW_CONCURRENCY, preview_timeout: float = PREVIEW_TIMEOUT) -> list[Job]:
    """Drive every task through preview → apply → watch as one pipeline.

    ``wait`` is ONE bound for the whole batch: once the last preview has
    settled, applied jobs are watched for at most ``wait`` more seconds (they
    have been watched all along before that). Every remote round is one
    ``call``; a failed round is retried next round, and a task whose window
    closes with its last call failing reports that error."""
    jobs = [Job(tid) for tid in dict.fromkeys(task_ids)]
    watch_deadline: float | None = None
    next_progress = clock() + PROGRESS_SECS

    def close(job: Job, **summary) -> None:
        finish(job, **summary)
        emit(job)

    while True:
        now = clock()
        # Expire previews that never settled within their own window.
        for job in jobs:
            if job.stage == "scanning" and job.preview_started is not None and now - job.preview_started >= preview_timeout:
                close(job, error=f"preview did not settle within {preview_timeout:g}s "
                                 f"(phase={job.state.get('phase')}; {job.last_error or 'no error'}; status id {job.job_id})")
        if watch_deadline is None and all(j.stage in ("applying", "done") for j in jobs):
            watch_deadline = now + wait
        if watch_deadline is not None and now >= watch_deadline:
            for job in jobs:
                if job.stage == "applying":
                    if not job.apply_submitted:
                        close(job, error=f"apply was never accepted: {job.last_error or 'no answer'} (status id {job.job_id})")
                    else:
                        seen = f"; last status read failed: {job.last_error}" if job.last_error else ""
                        apply_summary(job, note=f"still settling (phase={job.state.get('phase')}) after --wait "
                                                f"{wait:g}s{seen}; job continues detached (status id {job.job_id})")
                        emit(job)
        if all(j.done for j in jobs):
            return jobs

        calls: list[tuple[Job, str, dict]] = []
        scanning = sum(j.stage == "scanning" for j in jobs)
        for job in jobs:
            if job.stage == "pending" and scanning < concurrency:
                job.stage, job.preview_started = "scanning", now
                scanning += 1
                calls.append((job, "preview", {"action": "preview", "id": job.job_id, "task_id": job.task_id}))
            elif job.stage == "scanning" and job.state.get("phase") is not None:
                calls.append((job, "status", {"action": "status", "id": job.job_id}))
            elif job.stage == "scanning":
                # Submit (again): preview under the same id is idempotent.
                calls.append((job, "preview", {"action": "preview", "id": job.job_id, "task_id": job.task_id}))
            elif job.stage == "applying" and not job.apply_submitted:
                calls.append((job, "apply", {"action": "apply", "id": job.job_id}))
            elif job.stage == "applying":
                calls.append((job, "status", {"action": "status", "id": job.job_id}))

        try:
            answers = call(host, [params for _, _, params in calls])
        except RemoteError as exc:
            progress(f"remote round failed ({len(calls)} call(s)); retrying: {exc}")
            answers = [{"error": str(exc), "transient": True} for _ in calls]
        for (job, kind, _), answer in zip(calls, answers):
            if "error" in answer:
                job.last_error = answer["error"]
                if not answer.get("transient") and kind in ("preview", "apply"):
                    # The daemon refused the request: that is an answer, so report
                    # it now. Transport failures (timeouts, ssh) are retried.
                    close(job, error=f"{kind} refused: {job.last_error} (status id {job.job_id})")
                continue
            job.last_error = None
            if job.stage == "scanning":
                on_preview_state(job, answer, dry_run)
                if job.done:
                    emit(job)
            elif job.stage == "applying":
                job.state = answer
                if kind == "apply":
                    job.apply_submitted = True
                if answer.get("phase") in DONE_PHASES:
                    apply_summary(job)
                    emit(job)

        now = clock()
        if now >= next_progress and not all(j.done for j in jobs):
            next_progress = now + PROGRESS_SECS
            counts = {stage: sum(j.stage == stage for j in jobs) for stage in ("pending", "scanning", "applying", "done")}
            progress(f"… {counts['done']}/{len(jobs)} answered; {counts['scanning']} previewing, "
                     f"{counts['pending']} queued, {counts['applying']} applying")
        sleep(POLL_SECS)


def render(job: Job) -> tuple[list[str], int, int]:
    """The printed line(s) for one finished task, plus its reaped/retained counts."""
    summary = job.summary or {}
    head = f"{job.task_id[:8]} "
    if summary.get("error"):
        return [head + f"ERROR {summary['error']}"], 0, 0
    lines: list[str] = []
    reaped = retained = 0
    if summary.get("note") and not summary.get("results"):
        reasons = ", ".join(f"{c['reason']}" for c in summary.get("preview", []))
        return [head + f"{summary['note']} {reasons}".rstrip()], 0, 0
    for result in summary.get("results", []):
        name = result["path"].rsplit("/", 1)[-1][:52]
        if result["removed"]:
            reaped += 1
            lines.append(head + f"reaped {name}")
        else:
            retained += 1
            lines.append(head + f"RETAINED {name}: {result['message'][:90]}")
    if summary.get("note"):
        lines.append(head + summary["note"])
    if not lines:
        lines.append(head + f"complete; no per-checkout results ({job.state.get('message', '')})")
    return lines, reaped, retained


def main() -> int:
    # Progress must survive a caller's `timeout`/pipe: never sit in a block buffer.
    sys.stdout.reconfigure(line_buffering=True)  # type: ignore[union-attr]
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("task_ids", nargs="+")
    ap.add_argument("--host", default="cm-manager", help="owning host; 'local' for this one")
    ap.add_argument("--dry-run", action="store_true", help="preview only, never remove")
    ap.add_argument("--wait", type=float, default=90.0,
                    help="seconds to keep watching applied jobs once every preview has settled "
                         "(one bound for the whole batch, not per task)")
    ap.add_argument("--concurrency", type=int, default=PREVIEW_CONCURRENCY,
                    help="previews scanning on the host at once")
    args = ap.parse_args()

    totals = {"reaped": 0, "retained": 0}

    def emit(job: Job) -> None:
        lines, reaped, retained = render(job)
        totals["reaped"] += reaped
        totals["retained"] += retained
        for line in lines:
            print(line, flush=True)

    unique = list(dict.fromkeys(args.task_ids))
    print(f"{'previewing' if args.dry_run else 'reaping'} {len(unique)} task(s) on {args.host} "
          f"({args.concurrency} preview(s) at a time)…", flush=True)
    run(args.host, unique, args.dry_run, args.wait, emit=emit,
        progress=lambda line: print(line, flush=True), concurrency=max(1, args.concurrency))
    print(f"\n{totals['reaped']} checkout(s) reaped, {totals['retained']} retained, {len(unique)} task(s) requested.")
    print("Branches and archived artifacts are preserved in every case.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
