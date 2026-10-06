#!/usr/bin/env python3
"""Move every continuous task onto its own chat channel (DESIGN_TASK_CHANNELS.md).

For each task on the target host:
  1. `continuous.ensure_channel {task_id}` — create/attach `ct/<slug>`, bind the
     scheduler subscription, name the current session `<task_id>-orchestrator`,
     join every attributable worker. Idempotent.
  2. Replace the 2026-09-10 DM routing section at the top of `default_prompt`
     with the 2026-09-14 channel section (prepend when absent; skip when the
     new marker is already present), via `continuous.update`.

Schedules, pause state and every other field are untouched. Every prompt is
backed up before and after under the audit dir. Requires the daemon on the host
to run a build with `continuous.ensure_channel` (2026-09-14).

    scripts/migrate_task_channels.py --host cm-manager --dry-run
    scripts/migrate_task_channels.py --host cm-manager
    scripts/migrate_task_channels.py --host cm-manager --tasks bug-triage,perf-triage
    scripts/migrate_task_channels.py --host cm-manager --prompt-only   # skip ensure_channel
"""
import argparse
import datetime as dt
import hashlib
import json
import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
CM_OP = HERE / "cm-op"
OLD_MARKER = "## Owner policy 2026-09-10"
NEW_MARKER = "## Owner policy 2026-09-14"

NEW_SECTION = """## Owner policy 2026-09-14: task channel, mentions both ways, ready-for-approval
This task has its own chat channel, created and populated by CM. Read `chat_open()` and use its `continuous` block: `channel.path` / `channel.id`, your `role`, and for workers the CURRENT `orchestrator.participant_id`. CM names this session `<task_id>-orchestrator` itself; never claim, rename or spawn any session with an `-orchestrator` name (the suffix is reserved). Workers use a short codename, ideally their subtask id. All routine coordination happens in that channel: workers post each completed step, gate receipt, blocker, review request and handoff there with `mentions=[<orchestrator participant id>]`; you dispatch, return and promote work by posting there with `mentions=[<worker participant id>]` (resolve a worker with `chat_people(query=<subtask id or label>)`; `send_input` remains available for a worker that is not enrolled in chat yet). A mention wakes the addressed session natively and you wake on every post in the channel: answer on the wake, not on the next scheduled cycle. The schedule admits NEW work only. This overrides any DM-based routing, routine-notification or unfinished-session-cleanup instruction below. NEVER notify_user or DM/mention Owner for routine events. Read ~/.cm/policies/continuous-review-routing.md and include its worker contract (channel, mentions, ready-for-approval exit criteria) in every worker brief.
Drive each fix to ready-for-approval BEFORE promoting it to owner_review: rebased on current origin/main (re-verified whenever main moves); targeted tests plus the repository's full type-check gate and lint on touched files green on the rebased tree, with receipts posted in the channel; notes in the lane's own files, never a repo-root NOTES.md; operational gates done (registrations, build/table rows, config diffs, live probe receipts); an in-lane review verdict posted with risks and behaviour deltas; cross-lane collisions checked against the other task channels. Promote with ONE approval packet in the channel: task id, branch head sha, what it fixes, diff stat, gate receipts, behaviour change with defaults, dependencies/sequencing, deploy targets, rollback line, open Owner calls. Returned work goes back to `implementing` with the exact correction, by mention. The operator posts landing dispositions (landed sha, deploy time/target, terminal state) in the channel mentioning you: acknowledge in the channel, reconcile metadata and the index, and never re-dispatch a landed or done item. A system notice in the channel saying a run is HELD means: finish this cycle and call report_done.
Maintain metadata.continuous_stage at each transition (queued, investigating, implementing, review_queued, reviewing, owner_review, deploying, monitoring, waiting, done) with actual UTC stage_updated_at, next_action and reviewed_commit on approval; preserve unrelated metadata. Internal review stays planning `running`; `blocked` means an actual Owner decision. Idle never means approved. Unfinished tasks retain visible live sessions; worker report_done never authorizes teardown; your own report_done still closes each scheduled run. If the channel is unavailable, retain the pending handoff in task metadata and the index for periodic reconciliation, not an Owner notification.
"""


def rpc(host, method, params):
    cmd = [str(CM_OP)]
    if host and host != "local":
        cmd += ["--ssh", host]
    cmd += [method, json.dumps(params)]
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
    if out.returncode != 0:
        raise SystemExit(f"{method} failed rc={out.returncode}: {out.stderr.strip()[:500]}")
    try:
        resp = json.loads(out.stdout)
    except json.JSONDecodeError:
        raise SystemExit(f"{method}: non-JSON response: {out.stdout[:500]}")
    if not resp.get("ok", True) or "error" in resp:
        raise SystemExit(f"{method} error: {json.dumps(resp.get('error'))[:500]}")
    return resp.get("result", resp)


def read_prompt(host, task_id):
    code = (
        "import json,sys;print(json.load(open(sys.argv[1]))['default_prompt'])"
    )
    path = f"/home/lucas/.cm/continuous-tasks/{task_id}/state.json"
    cmd = ["python3", "-c", code, path]
    if host and host != "local":
        cmd = ["ssh", "-o", "ConnectTimeout=15", host, "python3 -c " + json.dumps(code) + " " + path]
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    if out.returncode != 0:
        raise SystemExit(f"read prompt {task_id}: {out.stderr.strip()[:300]}")
    return out.stdout[:-1] if out.stdout.endswith("\n") else out.stdout


def rewrite_prompt(prompt):
    """Return (new_prompt, action)."""
    if NEW_MARKER in prompt:
        return prompt, "already"
    lines = prompt.split("\n")
    if lines and lines[0].startswith(OLD_MARKER):
        # The 2026-09-10 section is the header plus its consecutive non-blank
        # lines; the task's own prompt starts after the first blank line
        # (verified on all 14 live prompts on 2026-09-14). Never cut at the
        # next markdown header: several prompts have none for many lines.
        end = 1
        while end < len(lines) and lines[end].strip() != "":
            end += 1
        rest = lines[end:]
        while rest and rest[0].strip() == "":
            rest.pop(0)
        return NEW_SECTION + "\n" + "\n".join(rest), "replaced"
    return NEW_SECTION + "\n" + prompt, "prepended"


def sha(s):
    return hashlib.sha256(s.encode()).hexdigest()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default="cm-manager")
    ap.add_argument("--tasks", help="comma-separated task ids (default: all)")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--prompt-only", action="store_true", help="skip continuous.ensure_channel")
    ap.add_argument("--channel-only", action="store_true", help="skip the prompt rewrite")
    ap.add_argument("--audit-dir", default=None)
    args = ap.parse_args()

    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%d-%H%M%S")
    audit = pathlib.Path(args.audit_dir or pathlib.Path.home() / ".cm/audits" / f"task-channels-{stamp}")
    audit.mkdir(parents=True, exist_ok=True)
    (audit / "migrate_task_channels.py").write_text(pathlib.Path(__file__).read_text())

    tasks = rpc(args.host, "continuous.list", {})["tasks"]
    wanted = set(args.tasks.split(",")) if args.tasks else None
    report = {"at": stamp, "host": args.host, "dry_run": args.dry_run, "tasks": {}}
    for t in tasks:
        tid = t["task_id"]
        if wanted and tid not in wanted:
            continue
        entry = {"paused": t.get("paused"), "current_session_uid": t.get("current_session_uid")}
        if not args.prompt_only:
            if args.dry_run:
                entry["channel"] = "dry-run"
            else:
                entry["channel"] = rpc(args.host, "continuous.ensure_channel", {"task_id": tid})
        if not args.channel_only:
            before = read_prompt(args.host, tid)
            (audit / f"{tid}.default_prompt.before.md").write_text(before)
            after, action = rewrite_prompt(before)
            entry["prompt"] = {"action": action, "before_sha256": sha(before), "after_sha256": sha(after),
                               "before_len": len(before), "after_len": len(after)}
            if action != "already":
                (audit / f"{tid}.default_prompt.after.md").write_text(after)
                if not args.dry_run:
                    rpc(args.host, "continuous.update", {"task_id": tid, "default_prompt": after})
                    check = read_prompt(args.host, tid)
                    entry["prompt"]["verified"] = sha(check) == sha(after)
                    if not entry["prompt"]["verified"]:
                        raise SystemExit(f"{tid}: prompt readback mismatch")
        report["tasks"][tid] = entry
        ch = entry.get("channel")
        chs = ch if isinstance(ch, str) else (ch or {}).get("path")
        print(f"{tid:40} channel={chs!s:26} prompt={entry.get('prompt', {}).get('action', '-')}")
    (audit / "report.json").write_text(json.dumps(report, indent=1))
    print(f"audit: {audit}")


if __name__ == "__main__":
    main()
