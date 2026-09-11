# How-To: Create & Operate a Continuous Task

Operator runbook for creating and operating a **continuous task**: a persistent orchestrator that admits new work on its configured schedule or queue and handles worker handoffs through messaging. Scheduled reconciliation recovers missed handoffs. This is the practical companion to `DESIGN_CONTINUOUS_TASKS.md`; the current review contract is [continuous-review-routing.md](doc/continuous-review-routing.md).

These are example task shapes, not a live fleet inventory. Read `continuous.list` for current tasks, schedules, pause state and `review_kind`; a retained definition may be paused. Copy a suitable current `default_prompt` and include the shared review policy:

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
- **Handoff transport.** Workers DM the current parent participant; a healthy native connection wakes that session without waiting for its next scheduled scan. Verify this independently of cadence. Existing embedded Codex sessions may lack native delivery even after their prompts are updated.
- **What it scans → what it spawns.** The deterministic input-gather (a log sample, a DB snapshot, a changelog diff, a git-diff-since-last-run) and the lifecycle it drives subtasks through.
- **`review_kind`.** How `/triage-review` should treat it: `"fix_first"` (queue is mostly mergeable code fixes), `"investigate_first"` (queue is mostly investigate-only proposals awaiting you), or omit (not triage-reviewable). Set it at create time so discovery is config-driven (`continuous.list` surfaces it; the skill reads it — no skill edit per new task).

---

## 1. The operator RPC helper

Continuous CRUD goes over the daemon's operator socket (`~/.cm/daemon.sock`) **on the host that owns the task**. Prefer the maintained helper from the CM checkout; it reads that host's token without printing it:

```bash
scripts/cm-op --ssh cm-manager continuous.list '{}'
```

For a structured Python workflow, use the real host-local token, not the historical literal `"op"`:

```python
import json, os, socket, struct
def rpc(method, params, sock=os.path.expanduser("~/.cm/daemon.sock")):
    with open(os.path.expanduser("~/.cm/operator-token")) as token_file:
        token = token_file.read().strip()
    req = {"id": os.urandom(6).hex(), "caller": {"token_id": token}, "method": method, "params": params}
    b = json.dumps(req).encode()
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(60); s.connect(sock)
    s.sendall(struct.pack(">I", len(b)) + b)
    n = struct.unpack(">I", s.recv(4))[0]; buf = b""
    while len(buf) < n: buf += s.recv(n - len(buf))
    s.close(); return json.loads(buf)
```

Continuous CRUD methods include `continuous.create`, `continuous.list`, `continuous.update`, `continuous.pause`, `continuous.run_now` and `continuous.delete`; these require Operator access. Agent session tools have their own authenticated task/workspace scope. Operator capability does not expand the user's authorized task.

An agent's session-control MCP tools address its local daemon even when chat spans multiple hosts. Use the same `scripts/cm-op --ssh <owning-host>` helper for already-authorized remote worker inspection or dispatch. A local `not_found` is not evidence the remote worker exited; verify it on its owning host before replacing it. See [cross-host session control](doc/AGENT_QUICKSTART.md#controlling-a-session-on-another-host).

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

- **Descriptive orchestrator identity.** Name the session `<task>-orchestrator`, e.g. `health-triage-orchestrator`; this overrides the general short-codename advice. Claim it on first `chat_send(name=...)`. Existing participants use `chat_open().name.revision` with `chat_rename`, preserving their UID, conversations, mentions and aliases. Include `~/.cm/policies/continuous-review-routing.md` in every prompt and worker brief and keep that shared policy deployed on the execution host.

- **Review routing and lifecycle (Owner, 2026-09-10).** Include [continuous-review-routing.md](doc/continuous-review-routing.md) in every new orchestrator and worker brief. Routine worker handoffs go by DM to the parent; healthy native delivery wakes it as messages arrive. Drain and acknowledge the inbox before cadence gates, review only the handed-off work, and deduplicate by task ID + artifact SHA + stage. A DM does not admit a new scan or queue batch. Keep schedules, queue admission, completion monitors and periodic reconciliation. Only the orchestrator escalates a reviewed decision that actually requires Owner. Maintain `metadata.continuous_stage`, actual UTC `stage_updated_at` and `next_action`; distinguish `review_queued`/`reviewing` from `owner_review`. Preserve visible live sessions while tasks remain unfinished. Activity/idle state is never a review decision.

- **You ARE the parent task; you do NOT do the work yourself** — you scan, spawn subtasks (`create_subtask` + `mcp_start_session`), and drive each along a lifecycle. Your memory is a gitignored `./.<task>/` dir (index.yaml + cycle-log.md) in your worktree that persists across cycles. First cycle: `mkdir -p .<task> && echo ".<task>/" >> "$(git rev-parse --git-path info/exclude)"`.
- **Your lane — first paragraph.** State exactly what you own and, explicitly, what you DON'T (hand off to the other triages). Scope discipline is what keeps findings honest.
- **Mandate-first.** "Your PRIMARY job is to FIND real issues and drive them to a fix; the GATE keeps findings honest, it is NOT a reason to file nothing." A big/expensive signal is a *dig-harder* signal, not a dismiss signal. (For investigation-first domains, add: "most findings become investigation tasks, not fixes — and a clean zero-finding cycle on a quiet window is also success; don't manufacture findings.")
- **The GATE** — a short numbered checklist a candidate must clear before you spawn it: quantifiable (name the number), real (not noise / a documented quirk), category fits a fixed enum, actionable.
- **Lifecycle** — `investigate → propose → implement → review → merge → monitor` (steps can collapse). Advance the relevant item on a worker handoff; reconcile all open items during scheduled cycles within their existing authority boundaries.
- **Before a scheduled scan: sync to main** — fetch and fast-forward only when the orchestrator worktree is clean and its branch permits it. Respect the repository's shared-checkout rules; do not assume a fast-forward or overwrite another session's work. A handoff-only wake does not require starting the scan procedure.
- **Step 1: the deterministic gather** — a script/heredoc that produces the cycle's inputs. Reproduce whatever the old cron/harness did (log sample, DB snapshot, changelog diff). Sample, don't full-scan, when the source is huge (the trader daily log is ~1.8GB — a full scan times out; use `sample_logs`).
- **Drive subtasks from evidence** — resolve the stable planning parent/child IDs and derive stage from git, NOTES and validation artifacts. Persist each reviewed handoff immediately to task metadata and the index. On scheduled reconciliation, join bulk task rows, live session summaries, retained delivery state and the parent index first; inspect individual transcripts only for exceptions. Include terminal tasks with live workers as cleanup candidates, and open tasks without live sessions as recovery candidates. Neither mismatch alone authorizes completion or termination.
- **Planning status and visible stage.** Internal review queues and orchestrator work stay `running`. Set `blocked` only for a concrete Owner decision after parent review. Record `review_queued`, `reviewing`, `owner_review`, `waiting` or the other shared stage values separately in metadata. The subtask label color and legend carry stage; focused text overrides that color. Idle means the session stopped working, not that review passed.
- **Operator-directive ACK (load-bearing for the TUI).** When the operator unblocks an index issue out-of-band — clears its `blocked_reason` and leaves a dated `# OPERATOR <YYYY-MM-DD> …` comment in the entry (the `/triage-review` convention) — the TUI renders that issue as **○ dispatch pending** under you until you act. Process it on its authorized message handoff or the next scheduled reconciliation. Write `operator_ack: <YYYY-MM-DD>` (today, ≥ the directive date) when handled, whether you dispatch, defer or determine that no dispatch is needed. The ack (or a live spawned subtask) clears the ○. A message receipt alone does not prove the directive was handled.
- **Recover unfinished tasks using the SAME task/worktree** — `mcp_start_session(task_id=<existing subtask id>, …)`, not another `create_subtask`. Resolve the current parent participant after replacement; old DMs do not transfer to a new UID. Preserve pause state and concrete infrastructure holds; record failed recovery rather than repeatedly launching a known-broken worker. Already terminal work is reviewed for cleanup, not relaunched merely because its session exited.
- **Separate slice completion, task completion and cleanup.** Worker `report_done` reports a completed slice; parent `report_done` closes its scheduled run. Neither proves the planning task is finished. Keep unfinished tasks live and visible. After an authorized terminal disposition and settled worker activity, verify closure and use the guarded cleanup procedure in [task-worktree-cleanup.md](doc/task-worktree-cleanup.md). A retained transcript tombstone is history, and a closed worker is not proof its worktree was reaped.
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
    "engine": "codex",                   # codex (new-task default) | claude | bash
    "run_mode": "persistent",            # persistent | fresh
    "schedule": {"kind": "periodic", "every_secs": 86400},
    "default_prompt": prompt,            # from §3
    "repo_url": "<repo>",                # shortname/URL resolved on the host
    "compact_every": 16,                 # persistent: /compact the session every Nth fire (>=2; 0 disables)
    "supervise": True,                   # respawn a dead persistent session; watchdog
    "mem_cap_bytes": 6442450944,         # bytes; 0 = uncapped; omit = [scheduler] default_cap.
                                         # Non-zero values below 64 MiB are REJECTED (the
                                         # "default_cap = 3 is a 3-byte cap" trap).
    "wedge_grace_secs": None,            # per-task run-wedge close grace (secs);
                                         # omit = [scheduler] consumer_wedge_grace_secs; 0 = closer off for this task
    "host": "local",                     # "local" = this daemon's host
    "review_kind": "fix_first",          # fix_first | investigate_first | omit
})
# -> {created, task_id, workspace_id: "ws-<slug>", worktree_path: ".../<repo>-<slug>"}
```

Other accepted params (see `ContinuousCreateParams` in `daemon/src/control/methods.rs`): `slug`, `workspace_id`, `project`, `start_branch`, `skill`, `modes`, `max_runtime_secs`, `downstream`, `enqueue_to`, `retention`. To change any of these later on a **live** task, use `continuous.update` (preserves `run_count` + history) — see §8.

The create-only default in this source revision is Codex; existing task records retain their explicit engine. Use an explicit engine when operating across daemon versions. CM adds no model override locally; cm-manager's Codex configuration selects `gpt-5.6-sol`. Fresh-run watchdog investigators use the task's agent engine: Codex for Codex, Claude for Claude. Explicit bash tasks retain a Claude investigator. These source changes await deployment and the [continuous Codex migration gates](doc/continuous-codex-migration.md), including actual model and lifecycle verification; they do not migrate existing sessions.

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

Also verify one actual worker-to-current-parent DM: accepted message, native wake observed, parent inbox read/acknowledgment, artifact review and persisted stage. No routine Owner notification should occur. A prompt update, idle parent or successful `chat_send` alone does not prove this path. Inspect `notification_status` for missing/uncertain delivery; preserve pending handoffs and periodic reconciliation until delivery is verified. Do not restart a busy parent just to pass the smoke test.

---

## 7. If you're MIGRATING a cron: disable it (only after the smoke test)

Never disable the old job until the new task has fired a real cycle successfully — no coverage gap. Then, reversibly: back up the crontab, comment the line with a dated prefix, reinstall.

```
# disabled <YYYY-MM-DD> (migrated to <host> <task> orchestrator): <original cron line>
```

(predictionTrading's triage crons live in **claude-triage's** crontab on the **trader**; the nightly non-triage jobs — signal-review, api-changelog — were in **lucas's** crontab on **aux-east4**. Back up first; guard the edit to touch exactly the intended line.)

---

## 8. Operate: verify · update · review · pause

- **Health read:** `continuous.list` → per-task `run_count`, `schedule`, `next_fire_at`, `current_session_uid`, `last_outcome`, `in_flight`, `account_blocked`, `review_kind`. Or read `~/.cm/continuous-tasks/<task>/state.json` directly.
- **Review delivery health:** inspect the current parent's native `notification_status`, pending receipts and recorded handoffs separately from scheduled-run health. A completed scheduler run does not prove worker messages were processed. Missing transport requires a supported migration/reconnect at an appropriate boundary; model/backend failures are a separate blocker. See [native notifications](doc/messaging/NATIVE_NOTIFICATIONS.md).
- **Change a live task in place:** `continuous.update {task_id, <field>}` — **preserves `run_count` + run history** (no delete+recreate). Common: steer the `default_prompt`, set `compact_every`, change `schedule`, backfill `review_kind`. Applied fields take effect next fire; the live session keeps running.
- **Review its output:** `/triage-review <task>` (or no-arg to enumerate reviewable tasks via `review_kind`). Fix-first tasks → walk the merge queue; investigate-first → read the proposals + decide. The orchestrator NEVER merges triage fixes itself — you do — **unless** the task is explicitly designed to auto-merge high-confidence fixes (then it gates on build+test green and pushes only `main`).
- **Pause / manual fire / delete:** `continuous.pause {task_id, paused:true}`, `continuous.run_now {task_id}`, `continuous.delete {task_id, gc?}`.
- **Break-glass a run wedged `Running`:** `continuous.force_done {task_id, seq, reason?}` — operator-only; flips `last_run` Running → Done iff `seq` matches (read the seq off `state.json` first). Use when a run's end signal was lost and the run-active gate is starving fires (a Consumer's queue backing up is the tell). Before this existed the only recovery was `send_input`-puppeting the session into re-calling `report_done`.
- **Dispatch-pending read:** `continuous.dispatch_pending` — per reviewable task (`review_kind` set), the index issues whose `blocked_reason` an operator cleared with a dated `# OPERATOR <date>` comment and no `operator_ack` yet. This is what feeds the TUI's ○ indicator (legend below); the TUI additionally drops issues whose `subtask_task_id` maps to a live planning task.

**Consumer cadence semantics (2026-09-02 — deploy the daemon for these).** A Consumer fires when `enabled && !paused && in_flight.is_none() && next_fire_at <= now`, its last run is NOT `Running`, the queue depth is known, and **either** arm trips:

- **`depth_threshold`** — the burst arm: fire as soon as `pending >= depth_threshold`, whatever the window. `0` DISABLES it (the live `scraper-creation` shape), leaving the window as the only trigger.
- **`window_secs`** — a latency bound **on a queued item**, measured from the queue's `oldest_pending_at` (the API has always reported it; the scheduler now reads it). "Nothing waits more than an hour" means exactly that: an item enqueued during a long run has already served its wait when the run ends, so the consumer fires on the next tick rather than starting a fresh hour. It was previously approximated as time since the last fire, which cost a full extra window after any run that outlived `window_secs` — with `depth_threshold 0` that halves throughput. A never-fired task still fires on its first item immediately. If the API sends no parseable timestamp, the arm falls back to the old `last_fired_at` measure.
- **`last_fired_at`** now means "a prompt was DELIVERED" — only real fires stamp it. A run-active skip, a benign skip, and a failed launch leave it alone (they also stopped holding the run-wedge closer's quiet clock open, which had made that closer unable to fire for a wedged consumer with a non-empty queue).
- **Depth freshness** — depths are polled at most once per 30 s per queue. A poll FAILURE no longer reads as depth 0 (which parked every consumer on that queue for the TTL): the last good reading keeps driving fires for up to 5 minutes, logging once per TTL, after which the depth is unknown and the consumer holds until the API answers again.
- **`next_fire_at` is a not-before gate, not a cadence** — after any fire or skip it moves out 60 s, so a deep queue drains at ≤1 batch/min.
- **Staged batches are pruned.** Each fire writes `<worktree>/.queue/batch-<seq>.json`; the newest `[scheduler] consumer_batch_keep` (default 50, `0` = keep all) survive, plus the batch of any run still `Running` or account-blocked — those files are the only replay source for a blocked batch, so they are never evicted.
- **Requeue is explicit.** `POST /queues/{q}/requeue` needs `{"ids": [...]}` or `{"all": true}`; a bare call is now a 400 instead of re-pending every claimed item (which, mid-fire, hands the running orchestrator's batch to the next one).

---

## 8b. Push alerts + the auth/wedge watchdog (2026-08-03 incident)

Backstory: cm-manager's `~/.claude/.credentials.json` got truncated at 04:18; the momentum-detective and scraper-creation consumer runs wedged `Running` for **3.5 days** with every failure surfaced (journal + runs.jsonl) and nothing pushed. Two daemon guards now exist, plus a push channel:

- **Push channel** — set in `~/.cm/daemon.toml` (top level, absolute path; the systemd PATH is minimal):

  ```toml
  notify_command = "/home/lucas/.cm/bin/cm-notify"
  ```

  Unset (the default) = alerts land on stderr/journal only. The command gets the message as its one argument and the source as `CM_NOTIFY_TAG`. It now fires on: Claude account blocks/recoveries, consumer wedges, `escalate_stuck`, the consecutive-failure circuit breaker, and persistent stalls.

- **Account-block detection + post-login recovery** (always on, claude-engine tasks) — the scheduler tail-reads the active run's transcript and recognizes both the synthetic `authentication_failed` record ("Login expired · Please run /login", event `auth_expired`) and Claude's otherwise-ordinary subscription banner ("You've hit your weekly limit …", event `usage_limited`). It persists `account_blocked` on the exact run and leaves that run **deliberately `Running`**: both scheduled fires and persistent supervision are held, which stops Consumers from claiming+acking more queue batches into a dead account. Recovery is automatic after `/login`: the host's end-to-end `claude-usage-probe` must write a newer `OK` check to `~/.cm/claude-probe-state.json`; for a Consumer, the scheduler first idempotently re-enqueues every item from the blocked run's staged `.queue/batch-<seq>.json` (delivery-time ack has already marked the originals consumed), then marks the poisoned run `Failed`, kills its old session, and lets persistent supervision respawn+refire (or a Fresh schedule fire after a 5 s reap-settle floor). A missing/corrupt batch defers recovery instead of silently dropping work. A valid-looking credentials file is intentionally insufficient because an exhausted account still has a valid token. `continuous.force_done {task_id, seq}` remains the break-glass fallback if the usage probe is absent/broken. One push per task per 6 h while blocked, plus an `account_recovered` push/audit line on recovery. Note the Stop hook does NOT run on auth-error turns, which is why detection reads transcripts instead.
- **Credentials preflight** (always on when any claude continuous task is enabled) — `~/.claude/.credentials.json` existing but truncated/unparseable/token-less alerts within ~60 s of the file breaking, before any session proves it. A missing file is fine (keychain/API-key setups).
- **Run-wedge watchdog — EVERY schedule** (`[scheduler] consumer_wedge_grace_secs`, default 3600, `0` = off fleet-wide; per-task override `wedge_grace_secs`, `0` = off for that task) — a run still `Running` whose live session's transcript ends in a *completed turn* (or a delivered prompt with no response) and has been quiet past the grace = the agent finished without `report_done`. The scheduler auto-closes it (`Running → Failed`, event `wedge_closed`, push) so the schedule refires — up to `[scheduler] wedge_close_limit` (default 3) consecutive times; past that it escalates once (`wedge_escalated`, run left `Running` to stop close→refire from burning queue items) and waits for you. `report_done` / a clean exit / `force_done` reset the streak. Long-running cycles are safe: a mid-turn transcript (e.g. a blocking `wait_for_session_idle`) is never judged wedged, and the 1 h grace clears monitor-wake gaps (workers run ≤25 min); a task with legitimately longer silent gaps sets its own `wedge_grace_secs` instead of the fleet losing the guard. *(Until the 2026-08 wedge campaign this closer was **Consumer-only** — six of nine live orchestrators, all Periodic, had NO daemon-side closer at all.)*
- **⚠ `persistent_max_stall_secs` is a DETECTOR, not a closer.** It only writes a daemon-log alert + a `"stalled"` runs.jsonl line, once per fire episode — it has **never auto-closed a run** and does not "force-close at 6 h". (The 2026-08-12 "guard verified firing at ~6h1m" observation was the task's own 6 h *periodic cadence* re-firing and replacing `last_run` — the wedge characterization confirmed seq 169 has no close event at all in runs.jsonl. Health budgets tuned to "CRIT = the guard failed" were bracketing a guard that does not exist; the run-wedge watchdog above is the actual closer.)
- **Restart-orphan sweep (startup)** — at daemon start, before any session restore, every run left `Running` by the previous daemon instance is closed as `Orphaned` (event `restart_orphaned`, trigger_source `startup-sweep`) unless its session process demonstrably survived (then `readopted`, run left open). A `systemctl restart cm-daemon` no longer needs the manual post-restart `force_done` sweep the runbook used to prescribe.

---

## Continuous-column stage and activity indicators

Subtask **text color** shows `metadata.continuous_stage`; the fixed legend explains Queue, Investigate, Build, Review Q, Reviewing, Needs you, Deploy, Verify, Waiting, Done and Unstaged. Focused text uses the selection highlight instead. See [the stage contract](doc/continuous-review-routing.md#visible-stages). An idle session can still be waiting for parent review, Owner review, evidence or runtime recovery.

What each session row (and sub-line) in the TUI's Continuous column (`A-c`) means:

| Glyph | Meaning |
|---|---|
| spinner (green) | Session actively producing output — orchestrator mid-cycle, or a subtask agent working. |
| `●` (white) | Legacy planning-status attention signal for `blocked`. Use the durable stage and concrete next action to distinguish reviewed Owner work from stale legacy metadata. A routine review request belongs with the parent and stays `running`. |
| `◉` (cyan) + `↳ text` line | **Operator question parked** (`metadata.operator_question`) — the orchestrator needs an answer; the question renders inline under the row. |
| `○` (yellow) sub-line | **Dispatch pending** — the operator unblocked an index issue (cleared `blocked_reason` + dated `OPERATOR` directive) and the orchestrator hasn't acknowledged (`operator_ack`) or spawned a live subtask for it yet. One line per issue, e.g. `○ PERF-083 · dispatch pending (2026-07-18)`. Clears on ack or dispatch (polled ~30s). |
| `◇` (dim) | Idle activity indicator. Read stage/next action for who owns the next step; a message wake may advance it before the next scheduled fire. |
| `⟳` (yellow) | Remote attach stream lost; auto-reconnecting (daemon-side work keeps running). |

---

## Gotchas, consolidated

- **Auto-fire race + rate-limit modal** — §5. Verify delivery; never assume `run_count=1` means it ran.
- **Seed detector state files before cycle-1** — §5.
- **Worktree environments** — use the target repository's bootstrap hooks/helper; do not assume each checkout gets a private `.venv` or hand-create links. predictionTrading provisions shared environment/configuration links through `scripts/python/bootstrap_worktree.py`. Inspect bindings without modifying or printing credentials.
- **Routine DB scans are reads** — use the configured database MCP or repository query helper and its query guidance. Queue/lifecycle writes and implementation changes follow the task's existing authority; tool availability alone does not expand the work scope.
- **`next_fire_at` drifts from any "old cron hour"** — periodic schedules fire every `every_secs` from *creation* time, not at a fixed wall-clock hour. Fine for cadence-based work; if you need a specific hour, that's a `cron`-kind schedule (see the design doc).
- **`compact_every` boundary fires are compact-only and self-closing** — every Nth *scheduler* fire on a persistent task delivers `/compact` instead of the prompt (the cycle resumes next fire), claims **no** queue batch, and records its run already-`Done` at fire time (nothing exists to `report_done` it). The scheduler then spaces the next fire ≥10 min out so the batch/prompt paste can't land mid-summarization. (Pre-2026-07 the boundary run was recorded `Running` with no possible closer — on a persistent *Consumer* that wedged the run-active gate forever and silently consumed the boundary's batch; the scraper-opt incident.)
- **`review_kind` on existing tasks** — backfill via `continuous.update`. New tasks: set it at create. `continuous.list` + `state.json` carry it; `/triage-review` (no-arg) discovers from it.
- **Deploying daemon changes:** follow [the holder/brain runbook](HOWTO_HOLDER_BRAIN_SPLIT.md). Routine brain deployments use `daemon.restart` through the supported deployment helper and preserve holder-owned sessions. `systemctl restart cm-daemon` is a hard service restart that can kill sessions; do not use it as a routine documentation, prompt or MCP-tool refresh. Updating guidance does not install a new TUI or migrate legacy session transports.

---

## See also

- `DESIGN_CONTINUOUS_TASKS.md` — architecture, scheduler, run-mode executors, funnel, roadmap.
- [Continuous review routing](doc/continuous-review-routing.md) — message handoffs, periodic reconciliation, quiet reviews, lifecycle stages and session retention.
- `AGENT_ORCHESTRATION.md` — the MCP tool surface an orchestrator drives (start_session, create_subtask, list_sessions, global perms).
- `DESIGN_CONTINUOUS_PANEL.md` — the Sessions-view continuous column; authoritative glyph legend (implementation view of the legend above).
- The `/triage-review` skill — the human review/merge counterpart.
- Live templates: `~/.cm/continuous-tasks/{bug,perf,scraper,behavior}-triage/state.json`, `api-update/state.json` — copy a `default_prompt`.
