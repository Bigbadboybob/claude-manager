#!/usr/bin/env python3
"""Move each continuous task's long prompt into standing instructions.

Before: every fire pasted the whole orchestrator prompt (16–40 KB) into the
PTY. After: the prompt becomes `standing_instructions` (materialized by the
daemon as `AGENTS.override.md` / `CLAUDE.local.md` in the task worktree, loaded
by the engine as project instructions and re-injected after compaction), and
`default_prompt` becomes a one-paragraph dispatch. See
doc/continuous-standing-instructions.md.

Idempotent: a task whose default_prompt already carries the dispatch marker is
skipped. Every prompt is backed up under the audit dir before the update.
Requires the daemon on the host to accept `standing_instructions` (2026-09-14).

    scripts/migrate_standing_instructions.py --host cm-manager --dry-run
    scripts/migrate_standing_instructions.py --host cm-manager
    scripts/migrate_standing_instructions.py --host cm-manager --tasks bug-triage
"""
import argparse
import datetime as dt
import hashlib
import json
import pathlib
import subprocess

HERE = pathlib.Path(__file__).resolve().parent
CM_OP = HERE / "cm-op"
MARKER = "<!-- cm-dispatch v1 -->"
FILES = {"codex": "AGENTS.override.md", "claude": "CLAUDE.local.md"}


def dispatch(task_id, engine, memory_dir):
    f = FILES.get(engine, "the project instructions")
    return (
        f"{MARKER}\n"
        f"Run ONE cycle of the `{task_id}` orchestrator procedure now. Your standing instructions "
        f"(lane, GATE, lifecycle, the step-by-step cycle procedure and the task-channel contract) are the "
        f"\"CM continuous task standing instructions\" section of `{f}` at this worktree's root, already "
        f"loaded as project instructions (if that section is not in your context yet, read the file first) — follow them exactly; do not ask for them to be repeated. "
        f"If this is a fresh thread, read `HANDOVER_CODEX.md` (when present) and your `{memory_dir}/` memory "
        f"before Step 0. Close this run with `report_done` when its admitted work and worker obligations settle."
    )


def rpc(host, method, params):
    cmd = [str(CM_OP)]
    if host and host != "local":
        cmd += ["--ssh", host]
    cmd += [method, json.dumps(params)]
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
    if out.returncode != 0:
        raise SystemExit(f"{method} failed rc={out.returncode}: {out.stderr.strip()[:500]}")
    resp = json.loads(out.stdout)
    if not resp.get("ok", True) or "error" in resp:
        raise SystemExit(f"{method} error: {json.dumps(resp.get('error'))[:500]}")
    return resp.get("result", resp)


def read_state(host, task_id):
    code = "import json,sys;d=json.load(open(sys.argv[1]));print(json.dumps({k:d.get(k) for k in ('default_prompt','standing_instructions','engine','worktree_path')}))"
    path = f"/home/lucas/.cm/continuous-tasks/{task_id}/state.json"
    cmd = ["python3", "-c", code, path]
    if host and host != "local":
        cmd = ["ssh", "-o", "ConnectTimeout=15", host, "python3 -c " + json.dumps(code) + " " + path]
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    if out.returncode != 0:
        raise SystemExit(f"read state {task_id}: {out.stderr.strip()[:300]}")
    return json.loads(out.stdout)


def memory_dir_of(prompt, task_id):
    # The live prompts name their gitignored memory dir as `./.<slug>/`.
    import re
    m = re.search(r"\./\.([A-Za-z0-9_-]+)/", prompt)
    return f".{m.group(1)}" if m else f".{task_id}"


def sha(s):
    return hashlib.sha256(s.encode()).hexdigest()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default="cm-manager")
    ap.add_argument("--tasks")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--audit-dir")
    args = ap.parse_args()
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%d-%H%M%S")
    audit = pathlib.Path(args.audit_dir or pathlib.Path.home() / ".cm/audits" / f"standing-instructions-{stamp}")
    audit.mkdir(parents=True, exist_ok=True)
    (audit / "migrate_standing_instructions.py").write_text(pathlib.Path(__file__).read_text())
    wanted = set(args.tasks.split(",")) if args.tasks else None
    report = {"at": stamp, "host": args.host, "dry_run": args.dry_run, "tasks": {}}
    for t in rpc(args.host, "continuous.list", {})["tasks"]:
        tid = t["task_id"]
        if wanted and tid not in wanted:
            continue
        st = read_state(args.host, tid)
        prompt = st["default_prompt"] or ""
        engine = (st.get("engine") or "codex").lower()
        entry = {"engine": engine}
        (audit / f"{tid}.default_prompt.before.md").write_text(prompt)
        if MARKER in prompt:
            entry["action"] = "already"
        elif engine == "bash":
            entry["action"] = "skipped_bash"
        else:
            standing = prompt
            new_prompt = dispatch(tid, engine, memory_dir_of(prompt, tid))
            (audit / f"{tid}.standing.md").write_text(standing)
            (audit / f"{tid}.default_prompt.after.md").write_text(new_prompt)
            entry.update({"action": "migrated", "standing_bytes": len(standing), "dispatch_bytes": len(new_prompt),
                          "standing_sha256": sha(standing)})
            if not args.dry_run:
                rpc(args.host, "continuous.update", {"task_id": tid, "standing_instructions": standing, "default_prompt": new_prompt})
                check = read_state(args.host, tid)
                entry["verified"] = check["standing_instructions"] == standing and check["default_prompt"] == new_prompt
                if not entry["verified"]:
                    raise SystemExit(f"{tid}: readback mismatch")
        report["tasks"][tid] = entry
        print(f"{tid:40} {entry['action']:14} standing={entry.get('standing_bytes','-')} dispatch={entry.get('dispatch_bytes','-')}")
    (audit / "report.json").write_text(json.dumps(report, indent=1))
    print(f"audit: {audit}")


if __name__ == "__main__":
    main()
