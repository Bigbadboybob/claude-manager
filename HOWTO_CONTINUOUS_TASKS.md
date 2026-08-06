# How-To: Create & Operate a Continuous Task

Operator runbook for standing up a new **continuous task** (a persistent orchestrator that fires on a schedule, scans some surface, and spawns + drives subtasks). This is the *practical* companion to `DESIGN_CONTINUOUS_TASKS.md` (which covers the architecture: data model, scheduler, funnel, run-mode executors). Read this when you want to **create one**, not understand the internals.

The five live orchestrators are your working templates — copy their `default_prompt` as a starting point:

| task_id | repo | host | cadence | run_mode | review_kind |
|---|---|---|---|---|---|
| `bug-triage` | predictionTrading | cm-manager | 3h | persistent | fix_first |
| `perf-triage` | predictionTrading | cm-manager | 6h | persistent | fix_first |
| `scraper-triage` | predictionTrading | cm-manager | 6h | persistent | fix_first |
| `behavior-triage` | predictionTrading | cm-manager | daily | persistent | investigate_first |
| `api-update` | predictionTrading | cm-manager | daily | persistent | investigate_first |

---

## 0. Decide the shape first

Before writing anything, pin these down — they determine everything else:

- **Target repo + host.** The task runs on a **daemon**, in a git worktree of the target repo, on the host you pick. Whatever the orchestrator needs at runtime must exist *on that host*: the repo checkout, any **project skills** it invokes (project skills live in `<repo>/.claude/skills/` — they're NOT available on a host without that checkout), the language **toolchain** (e.g. `cargo`/`clippy` for a Rust hunt, `uv` for Python), and any **credentials** (DB DSNs, tokens).
  - `cm-manager` is always-on and already hosts the predictionTrading clone + prod-DB tunnel — the default for predictionTrading automations.
  - `local` (the laptop daemon) is right when the target repo + skills + toolchain only live locally (e.g. claude-manager's own Rust code + its `.claude/skills/` project skills). Caveat: it only fires while the laptop is up — the scheduler catches up (once, no backfill) when it's next up, which is fine for multi-hour/day cadences.
- **run_mode.** `persistent` (the orchestrator keeps its session + context across fires, `/compact`-managed by `compact_every`; its disk memory survives) — use this for anything that drives subtasks across cycles. `fresh` respawns the session each fire (stateless-ish); rarely what you want for an orchestrator.
- **Cadence.** `schedule: {kind: "periodic", every_secs: N}`. Match the work: triage 3–6h, digest/audit daily (86400), heavier sweeps every few days.
- **What it scans → what it spawns.** The deterministic input-gather (a log sample, a DB snapshot, a changelog diff, a git-diff-since-last-run) and the lifecycle it drives subtasks through.
- **`review_kind`.** How `/triage-review` should treat it: `"fix_first"` (queue is mostly mergeable code fixes), `"investigate_first"` (queue is mostly investigate-only proposals awaiting you), or omit (not triage-reviewable). Set it at create time so discovery is config-driven (`continuous.list` surfaces it; the skill reads it — no skill edit per new task).

---

## 1. The operator RPC helper

All continuous CRUD goes over the daemon's operator socket (`~/.cm/daemon.sock`) **on the host that will own the task**. Run this on that host (e.g. `ssh cm-manager 'python3 -'`, or locally):

```python
import json, os, socket, struct
def rpc(method, params, sock=os.path.expanduser("~/.cm/daemon.sock")):
    req = {"id": os.urandom(6).hex(), "caller": {"token_id": "op"}, "method": method, "params": params}
    b = json.dumps(req).encode()
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(60); s.connect(sock)
    s.sendall(struct.pack(">I", len(b)) + b)
    n = struct.unpack(">I", s.recv(4))[0]; buf = b""
    while len(buf) < n: buf += s.recv(n - len(buf))
    s.close(); return json.loads(buf)
```

Methods you'll use: `continuous.create`, `continuous.list`, `continuous.update`, `continuous.pause`, `continuous.run_now`, `continuous.delete`, plus session methods (`read_session_output`, `send_input`, `kill_session`, `list_sessions`). All are **operator-only**.

---

## 2. Create the planning task

The orchestrator needs a real planning-task UUID as its parent so its subtasks nest under it on the board. Create it first and keep the `id`. Read API creds from the host's `~/.cm/daemon.toml` (`api_url`, `api_token`):

```python
import json, urllib.request, re
cfg = open(os.path.expanduser("~/.cm/daemon.toml")).read()
url = re.search(r'api_url\s*=\s*"([^"]+)"', cfg).group(1)
tok = re.search(r'api_token\s*=\s*"([^"]+)"', cfg).group(1)
body = json.dumps({"repo_url": "<repo>", "repo_branch": "main",
                   "name": "<Label> Orchestrator", "status": "running", "kind": "continuous"}).encode()
req = urllib.request.Request(url.rstrip("/") + "/tasks", data=body, method="POST",
                            headers={"Content-Type": "application/json", "Authorization": "Bearer " + tok})
planning_task_id = json.load(urllib.request.urlopen(req, timeout=30))["id"]
```

(`kind:"continuous"` may come back as `null` — the API doesn't persist it; harmless. `planning_task_id` is what matters.)

---

## 3. Write the `default_prompt` (the heart of it)

The prompt IS the orchestrator. Copy a live task's prompt as scaffolding:

```bash
python3 -c "import json;print(json.load(open('/home/lucas/.cm/continuous-tasks/scraper-triage/state.json'))['default_prompt'])"
```

Keep these **proven idioms** (every live orchestrator uses them):

- **You ARE the parent task; you do NOT do the work yourself** — you scan, spawn subtasks (`create_subtask` + `mcp_start_session`), and drive each along a lifecycle. Your memory is a gitignored `./.<task>/` dir (index.yaml + cycle-log.md) in your worktree that persists across cycles. First cycle: `mkdir -p .<task> && echo ".<task>/" >> "$(git rev-parse --git-path info/exclude)"`.
- **Your lane — first paragraph.** State exactly what you own and, explicitly, what you DON'T (hand off to the other triages). Scope discipline is what keeps findings honest.
- **Mandate-first.** "Your PRIMARY job is to FIND real issues and drive them to a fix; the GATE keeps findings honest, it is NOT a reason to file nothing." A big/expensive signal is a *dig-harder* signal, not a dismiss signal. (For investigation-first domains, add: "most findings become investigation tasks, not fixes — and a clean zero-finding cycle on a quiet window is also success; don't manufacture findings.")
- **The GATE** — a short numbered checklist a candidate must clear before you spawn it: quantifiable (name the number), real (not noise / a documented quirk), category fits a fixed enum, actionable.
- **Lifecycle** — `investigate → propose → implement → review → merge → monitor` (steps can collapse). Each cycle, push every open item to its next step.
- **Step 0: sync to main** (`git fetch origin && git merge --ff-only origin/main`) — you never commit in your own worktree, so this is always a clean ff; your `.<task>/` memory is ignored and survives.
- **Step 1: the deterministic gather** — a script/heredoc that produces the cycle's inputs. Reproduce whatever the old cron/harness did (log sample, DB snapshot, changelog diff). Sample, don't full-scan, when the source is huge (the trader daily log is ~1.8GB — a full scan times out; use `sample_logs`).
- **Step 2: drive open subtasks** — `list_subtasks`; derive each one's stage from **artifacts** (git working tree + committed diff + NOTES.md), NOT its self-report; record a structured block in index.yaml **every cycle**; act by stage.
- **Planning-status convention (load-bearing for the TUI).** Set a subtask `blocked` **only** when the ball is with the operator (a committed fix awaiting review, or an explicit human decision); everything the orchestrator advances itself stays `running`; reconcile each cycle. This drives the continuous column's **⚪ needs-you vs ◇ orchestrator-has-it** indicator — breaking the convention breaks the indicator. (Full glyph legend: §"Continuous-column glyph legend" below.)
- **Operator-directive ACK (load-bearing for the TUI).** When the operator unblocks an index issue out-of-band — clears its `blocked_reason` and leaves a dated `# OPERATOR <YYYY-MM-DD> …` comment in the entry (the `/triage-review` convention) — the TUI renders that issue as **○ dispatch pending** under you until you act. At **cycle start**, when you process such a directive, write `operator_ack: <YYYY-MM-DD>` (today, ≥ the directive date) into that issue's index entry — whether you dispatch a subtask, defer, or decide the directive needs no dispatch. The ack (or a live spawned subtask) is what clears the ○; without it the operator can't tell seen-and-handled from never-seen.
- **Re-spawn exited agents into the SAME worktree** — `mcp_start_session(task_id=<existing subtask id from list_subtasks>, …)`, never `create_subtask` for an existing finding. An exited agent is a reason to restart work, not punt to the user.
- **Adopt any pre-existing backlog** (e.g. the trader's own `index.yaml` `active` entries) with a clear dedup rule.
- **Step N: summary → `./.<task>/cycle-log.md`** — one paragraph per cycle; this is your continuity.

See `AGENT_ORCHESTRATION.md` for the MCP tools an orchestrator has, and the "Permission convention for agents" in `CLAUDE.md`.

---

## 4. `continuous.create`

The daemon creates the worktree **once** (reused every fire), registers the workspace, writes `~/.cm/continuous-tasks/<task_id>/state.json`, and **auto-fires once** (see §5). Params:

```python
rpc("continuous.create", {
    "task_id": "<slug>",                 # durable id; keys the worktree + workspace + state dir
    "planning_task_id": planning_task_id, # from §2 — the subtask parent
    "label": "<Label> Orchestrator",
    "engine": "claude",                  # claude | codex | bash
    "run_mode": "persistent",            # persistent | fresh
    "schedule": {"kind": "periodic", "every_secs": 86400},
    "default_prompt": prompt,            # from §3
    "repo_url": "<repo>",                # shortname/URL resolved on the host
    "compact_every": 16,                 # persistent: /compact the session every Nth fire (>=2; 0 disables)
    "supervise": True,                   # respawn a dead persistent session; watchdog
    "mem_cap_bytes": 0,                  # 0 = uncapped; None = daemon default
    "host": "local",                     # "local" = this daemon's host
    "review_kind": "fix_first",          # fix_first | investigate_first | omit
})
# -> {created, task_id, workspace_id: "ws-<slug>", worktree_path: ".../<repo>-<slug>"}
```

Other accepted params (see `ContinuousCreateParams` in `daemon/src/control/methods.rs`): `slug`, `workspace_id`, `project`, `start_branch`, `skill`, `modes`, `max_runtime_secs`, `downstream`, `enqueue_to`, `retention`. To change any of these later on a **live** task, use `continuous.update` (preserves `run_count` + history) — see §8.

---

## 5. Handle the auto-fire (GOTCHAs)

`continuous.create` **fires once immediately**. Two things bite here:

- **Seed any detector state FIRST.** If Step 1 runs a script that needs a config/state file (e.g. `api-update`'s `state.yaml` holds both the source list and baselines), the first cycle will fail on a missing file. Seed it into the worktree **right after create**, before the fire reaches Step 1. (The worktree is at the returned `worktree_path`; gitignored files persist there across cycles.)
- **The create-fire can RACE Claude Code's boot and be lost** — the paste lands in a still-booting session and vanishes (session sits idle at an empty prompt box, no transcript). And on a **shared-account 5-hour rate limit**, the fire lands the session at the interactive `/rate-limit-options` modal, which `send_input`'s kitty-Enter can't dismiss. **Always verify the fire actually delivered** (`read_session_output` shows the prompt + it's processing, or a transcript exists). Recovery:
  - Lost-to-boot-race → `continuous.run_now {task_id}` once the session is booted + idle.
  - Rate-limited → wait for the window reset, then `kill_session` the stuck session; a supervised persistent task auto-respawns a fresh session and re-fires cleanly (no `run_now` needed).

---

## 6. Smoke-test one cycle

Watch one full cycle end-to-end before trusting it. Read `read_session_output` as it works, then inspect the memory it wrote:

```bash
WT=/home/lucas/.cm/worktrees/<repo>-<slug>
cat "$WT/.<slug>/cycle-log.md"     # did it produce a sane summary?
cat "$WT/.<slug>/index.yaml"       # findings tracked correctly?
```

Confirm it: gathered inputs correctly, applied the GATE (didn't over-file on a quiet window, didn't miss a real issue), didn't over-spawn subtasks, and set planning statuses per the convention. **A clean "0 findings" cycle on a quiet window is a valid pass** for investigation-first tasks.

---

## 7. If you're MIGRATING a cron: disable it (only after the smoke test)

Never disable the old job until the new task has fired a real cycle successfully — no coverage gap. Then, reversibly: back up the crontab, comment the line with a dated prefix, reinstall.

```
# disabled <YYYY-MM-DD> (migrated to <host> <task> orchestrator): <original cron line>
```

(predictionTrading's triage crons live in **claude-triage's** crontab on the **trader**; the nightly non-triage jobs — signal-review, api-changelog — were in **lucas's** crontab on **aux-east4**. Back up first; guard the edit to touch exactly the intended line.)

---

## 8. Operate: verify · update · review · pause

- **Health read:** `continuous.list` → per-task `run_count`, `schedule`, `next_fire_at`, `current_session_uid`, `last_outcome`, `in_flight`, `review_kind`. Or read `~/.cm/continuous-tasks/<task>/state.json` directly.
- **Change a live task in place:** `continuous.update {task_id, <field>}` — **preserves `run_count` + run history** (no delete+recreate). Common: steer the `default_prompt`, set `compact_every`, change `schedule`, backfill `review_kind`. Applied fields take effect next fire; the live session keeps running.
- **Review its output:** `/triage-review <task>` (or no-arg to enumerate reviewable tasks via `review_kind`). Fix-first tasks → walk the merge queue; investigate-first → read the proposals + decide. The orchestrator NEVER merges triage fixes itself — you do — **unless** the task is explicitly designed to auto-merge high-confidence fixes (then it gates on build+test green and pushes only `main`).
- **Pause / manual fire / delete:** `continuous.pause {task_id, paused:true}`, `continuous.run_now {task_id}`, `continuous.delete {task_id, gc?}`.
- **Break-glass a run wedged `Running`:** `continuous.force_done {task_id, seq, reason?}` — operator-only; flips `last_run` Running → Done iff `seq` matches (read the seq off `state.json` first). Use when a run's end signal was lost and the run-active gate is starving fires (a Consumer's queue backing up is the tell). Before this existed the only recovery was `send_input`-puppeting the session into re-calling `report_done`.
- **Dispatch-pending read:** `continuous.dispatch_pending` — per reviewable task (`review_kind` set), the index issues whose `blocked_reason` an operator cleared with a dated `# OPERATOR <date>` comment and no `operator_ack` yet. This is what feeds the TUI's ○ indicator (legend below); the TUI additionally drops issues whose `subtask_task_id` maps to a live planning task.

---

## 8b. Push alerts + the auth/wedge watchdog (2026-08-03 incident)

Backstory: cm-manager's `~/.claude/.credentials.json` got truncated at 04:18; the momentum-detective and scraper-creation consumer runs wedged `Running` for **3.5 days** with every failure surfaced (journal + runs.jsonl) and nothing pushed. Two daemon guards now exist, plus a push channel:

- **Push channel** — set in `~/.cm/daemon.toml` (top level, absolute path; the systemd PATH is minimal):

  ```toml
  notify_command = "/home/lucas/.cm/bin/cm-notify"
  ```

  Unset (the default) = alerts land on stderr/journal only. The command gets the message as its one argument and the source as `CM_NOTIFY_TAG`. It now fires on: auth expiry, consumer wedges, `escalate_stuck`, the consecutive-failure circuit breaker, and persistent stalls.

- **Auth-expiry detection** (always on, claude-engine tasks) — the scheduler tail-reads the active run's transcript and matches the synthetic `authentication_failed` record ("Login expired · Please run /login"). One push per task per 6 h while it persists (`runs.jsonl` event `auth_expired`). The run is **deliberately left `Running`**: the run-active gate is what stops further fires from claiming+acking queue batches into a dead session. Recovery: `/login` on the daemon host, then `continuous.force_done {task_id, seq}` (the alert names both). Note the Stop hook does NOT run on auth-error turns, which is why this reads transcripts instead.
- **Credentials preflight** (always on when any claude continuous task is enabled) — `~/.claude/.credentials.json` existing but truncated/unparseable/token-less alerts within ~60 s of the file breaking, before any session proves it. A missing file is fine (keychain/API-key setups).
- **Consumer-wedge watchdog** (`[scheduler] consumer_wedge_grace_secs`, default 3600, `0` = off) — a Consumer run still `Running` whose live session's transcript ends in a *completed turn* (or a delivered prompt with no response) and has been quiet past the grace = the agent finished without `report_done`. The scheduler auto-closes it (`Running → Failed`, event `wedge_closed`, push) so the due-gate refires — up to `[scheduler] wedge_close_limit` (default 3) consecutive times; past that it escalates once (`wedge_escalated`, run left `Running` to stop close→refire from burning queue items) and waits for you. `report_done` / a clean exit / `force_done` reset the streak. Long-running cycles are safe: a mid-turn transcript (e.g. a blocking `wait_for_session_idle`) is never judged wedged, and the 1 h grace clears monitor-wake gaps (workers run ≤25 min).

---

## Continuous-column glyph legend

What each session row (and sub-line) in the TUI's Continuous column (`A-c`) means:

| Glyph | Meaning |
|---|---|
| spinner (green) | Session actively producing output — orchestrator mid-cycle, or a subtask agent working. |
| `●` (white) | **Operator action needed** — the row's planning task is raw-`blocked` (a committed fix awaiting `/triage-review`, or an explicit human decision). Load-bearing: `blocked` means *operator-action-needed*, nothing else (a merged-but-monitoring subtask goes back to `running`). |
| `◉` (cyan) + `↳ text` line | **Operator question parked** (`metadata.operator_question`) — the orchestrator needs an answer; the question renders inline under the row. |
| `○` (yellow) sub-line | **Dispatch pending** — the operator unblocked an index issue (cleared `blocked_reason` + dated `OPERATOR` directive) and the orchestrator hasn't acknowledged (`operator_ack`) or spawned a live subtask for it yet. One line per issue, e.g. `○ PERF-083 · dispatch pending (2026-07-18)`. Clears on ack or dispatch (polled ~30s). |
| `◇` (dim) | Idle, **orchestrator has it** — nothing needs you; the next fire advances it. |
| `⟳` (yellow) | Remote attach stream lost; auto-reconnecting (daemon-side work keeps running). |

---

## Gotchas, consolidated

- **Auto-fire race + rate-limit modal** — §5. Verify delivery; never assume `run_count=1` means it ran.
- **Seed detector state files before cycle-1** — §5.
- **`uv run` in a worktree** — continuous worktrees get their own `.venv`, so `uv run` works in them. But a repo's `.env` (DB DSNs) lives in the **clone**, not the worktree — source it via `REPO_DIR="$(dirname "$(git rev-parse --git-common-dir)")"; source "$REPO_DIR/.env"`.
- **DB access stays read-only** for prod — use the read-only role/DSN (e.g. the trader's `POSTGRES_READONLY_CONNECTION_STRING` via `ssh trader` as claude-triage), or a trusted SELECT-only script. Do NOT hand an agent a write DSN for ad-hoc `psql`. The repo's committed `postgres-remote` MCP is `--access-mode=unrestricted` — don't route agent drilldowns through it.
- **`next_fire_at` drifts from any "old cron hour"** — periodic schedules fire every `every_secs` from *creation* time, not at a fixed wall-clock hour. Fine for cadence-based work; if you need a specific hour, that's a `cron`-kind schedule (see the design doc).
- **`compact_every` boundary fires are compact-only and self-closing** — every Nth *scheduler* fire on a persistent task delivers `/compact` instead of the prompt (the cycle resumes next fire), claims **no** queue batch, and records its run already-`Done` at fire time (nothing exists to `report_done` it). The scheduler then spaces the next fire ≥10 min out so the batch/prompt paste can't land mid-summarization. (Pre-2026-07 the boundary run was recorded `Running` with no possible closer — on a persistent *Consumer* that wedged the run-active gate forever and silently consumed the boundary's batch; the scraper-opt incident.)
- **`review_kind` on existing tasks** — backfill via `continuous.update`. New tasks: set it at create. `continuous.list` + `state.json` carry it; `/triage-review` (no-arg) discovers from it.
- **Deploying a daemon change** (if you touch daemon code): can't `cp` over the running `/opt/cm-daemon/cm-daemon` ("Text file busy") — `sudo mv` it aside, then `cp` + `systemctl restart cm-daemon`. The restart cleanly reattaches live orchestrator PTYs (same session_uids survive).

---

## See also

- `DESIGN_CONTINUOUS_TASKS.md` — architecture, scheduler, run-mode executors, funnel, roadmap.
- `AGENT_ORCHESTRATION.md` — the MCP tool surface an orchestrator drives (start_session, create_subtask, list_sessions, global perms).
- `DESIGN_CONTINUOUS_PANEL.md` — the Sessions-view continuous column; authoritative glyph legend (implementation view of the legend above).
- The `/triage-review` skill — the human review/merge counterpart.
- Live templates: `~/.cm/continuous-tasks/{bug,perf,scraper,behavior}-triage/state.json`, `api-update/state.json` — copy a `default_prompt`.
