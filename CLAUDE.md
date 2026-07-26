# Claude Manager

Task orchestration system for planning and running Claude coding sessions. Primarily used **locally with git worktrees** for day-to-day work; cloud dispatch to ephemeral GCP VMs is still supported for cases where it's useful (long-running tasks, isolation, running things away from the local machine).

> **Note:** This project started out cloud-first, but in practice local + worktrees turned out to be much smoother and is now the default mode. Cloud support is retained but secondary. When working on this project, assume local usage unless the user explicitly mentions cloud.

## Project overview

- **`tui/`** — Rust TUI client. The user-facing entry point. Workflow orchestration, planning board rendering, API communication, and the attach-stream side of session I/O. Build with `cargo build --workspace` (the TUI binary lives in `tui/` and depends on `daemon/`).
- **`daemon/`** — `cm-daemon`, the persistent host daemon. Owns session PTY lifecycle, manifest, manifest.watch broadcasts, workflow state, and the control-socket dispatcher. The TUI launches it automatically at startup if it isn't already running; mandatory since slice 10f (Phase 1's default flip).
- **`workflows/`** — TOML definitions for multi-agent workflows (e.g. `feedback.toml`). Loaded at TUI startup; each defines roles, transitions, and activation prompts. See the Workflows section below.
- **`mcp_server/`** — MCP server exposing tools that agents running inside the TUI can call:
  - `propose_task(...)` — push a task to the planning backlog.
  - `workflow_transition(to, prompt)` / `workflow_done(reason)` — workflow participants use these to hand off control or end a run. Events land in `~/.cm/workflow-runs/<id>/events.jsonl` and the TUI tails that file as its workflow control plane.
  - `ping`, `list_sessions`, `list_sessions_grouped`, `mcp_start_session`, `send_input`, `read_session_output`, `kill_session` — agent orchestration. Routed per-method by the resolver: session/PTY-touching methods go to `~/.cm/daemon.sock` (the daemon owns the registry); workflow-touching methods stay on `~/.cm/tui.sock`. Auth keys off `CM_TUI_SESSION_ID` injected when the TUI spawns the agent; descendant-only scope by default. `ping` now reports the caller's own `global_perms` / `task_id` / `workspace_id`; `list_sessions` entries carry `task_id` / `workspace_id` / `global_perms` and `list_sessions_grouped` returns them as a workspace→task tree.
  - `monitor_sessions`, `list_monitors`, `cancel_monitor` — **async waits** (the non-blocking replacement for parking in `wait_*` calls). A monitor watches sessions in the background (MCP-server-resident) and, on completion, delivers a `[cm-monitor <id>]` message into the CALLER's own session — waking it like a user message. `start_session` (wait=false + prompt) and `send_input` auto-register one per worker (`notify_on_done=false` opts out), so orchestrators should spawn/prompt, END THEIR TURN, and let the fire wake them. Delivery: mid-turn callers get turn-boundary delivery via the cm Stop hook's inbox (`~/.cm/inbox/<uid>/`, injected into claude-code spawns via `--settings`); at-prompt callers get gated PTY injection (deferred while the operator is typing), verified against the caller's LIVE transcript (re-resolved each check, so a resumed caller doesn't defeat verification) with at most one redelivery — and a redelivery only after a fresh marker re-check misses AND the caller is back at its prompt; result always retained in `list_monitors`. Monitors are **edge-triggered by default**: they arm against the watched session's transcript at registration and fire only on a NEW completed turn (or exit), so re-arming on an already-idle session doesn't instant-fire its stale reply — registration returns `already_idle` for sessions that are at their prompt right now (`edge=false` restores level-triggering). `cancel_monitor` is terminal from any state (aborts in-flight delivery, purges pending inbox files) and accepts `monitor_id="all"` as the one-call off switch. The hook also self-reports `session.turn_ended` → `semantic_idle` in `resolve_authorized_session`, and the wait/monitor loops treat a transcript ending in a completed assistant turn as idle even when a background task's spinner keeps the PTY noisy. See NOTES.md on branch `cm/proper-cm-subagent-wait`.
  - **Global permissions**: a session may carry a `global_perms` flag that short-circuits the descendant-only scope — it can list/prompt/read/kill/spawn against ANY session. Granted by the operator (A-e session-settings toggle → `session.set_global_perms` RPC) or propagated by an already-global agent via `start_session(global_perms=true)` (escalation-guarded in `mcp_start_session`: honored only if the caller is itself global). The flag lives on `DaemonSession`/`TerminalSession`/`ManifestEntry`; auth short-circuits in `daemon/src/control/auth.rs::check_session_caller` and its TUI mirror `caller_authorized_for`. See AGENT_ORCHESTRATION.md → "Global permissions".
  - `start_workflow`, `stop_workflow`, `get_workflow_state`, `list_workflows` — orchestrator-side workflow control. Caller is the orchestrator, NOT a participant.
  - `create_subtask`, `list_subtasks`, `mark_subtask_done` — subtasks fork off a parent task with `parent_task_id` set. `worktree_mode="branch"` creates a new worktree under `cm-sub/<slug-chain>-<short_id>`.
  - `submit_backtest`, `get_backtest_result` — cloud auto-backtest: submit a predictionTrading backtest (branch + script + config) as a `kind='backtest'` task; the dispatch daemon's backtest lane runs it on an ephemeral GCP **spot** worker in `prediction-market-scalper`/`us-east4-a` (same VPC/zone as the replica + results bucket), the worker publishes bulk results to `gs://prediction-market-scalper-datasets/backtests/<run_key>/` and POSTs a compact summary to the new `task_artifacts` table (`POST/GET /tasks/{id}/artifacts`), and `get_backtest_result(task_id, wait=...)` reads it back. Headless-capable via daemon proxy methods `backtest.submit`/`backtest.result`. `script="__cm_stub__"` runs an infra smoke test with fabricated results. Caveat: the `project`→repo_url default resolves from planning rows and can be wrong — pass `repo_url` explicitly (the PT-side twin server defaults to its own git origin).

## Permission convention for agents (Phase 7)

You have tools that can spawn subtasks, start sessions, send input to other sessions, kill sessions, start workflows, and create subtasks. **Before using any of them, state your intent in plain language and ask the user to confirm.** The tools will not refuse you, but the user expects to stay in the loop. Apply the same convention to anything destructive (killing a session, marking a subtask done, stopping a workflow).

Read-only tools (`ping`, `list_sessions`, `list_sessions_grouped`, `read_session_output`, `get_workflow_state`, `list_workflows`, `list_subtasks`) don't need pre-approval — call them as needed.

If you hold **global permissions** (`ping().global_perms == true`), you can reach sessions outside your own task tree. The convention matters *more*, not less, there: state your intent and confirm before driving unrelated work, and never pass `global_perms=true` to `start_session` without explicit user say-so.

## On `signal 9` from a Bash tool

If a Bash tool call dies with `signal 9` (SIGKILL) and you didn't initiate it, the session memory cap may have killed the process. The TUI logs structured kill records to `~/.cm/memory_kills/$CM_TUI_SESSION_ID.jsonl` (one JSON line per kill). Read that file when puzzled by an unexplained SIGKILL — each entry has `comm`, `argc`, `argv_sha256_prefix` (4-byte SHA-256 prefix you can use to correlate against your own argv), `rss_kb`, and the configured `soft_cap_bytes` / `hard_cap_bytes`. The file is per-session; `$CM_TUI_SESSION_ID` is set in your env. Memory caps are user-configured and gated on a startup preflight; an empty file means no kills have happened (or the cap is disabled). See `DESIGN_MEMORY_CAP.md` for the full mechanism.
- **`api/`** — FastAPI server (cloud mode only). Task CRUD, dispatch daemon for GCP workers, warm pool management. Runs on the `cm-manager` VM.
- **`dispatch/`** — Cloud-only. DB access (`db.py`), VM lifecycle (`vm.py`), config (`config.py`).
- **`cli/`** — CLI client + planning client library used by `mcp_server`.
- **`worker/`** — Startup scripts for cloud worker VMs.
- **`sql/`** — Database migrations (cloud-only, auto-run on API startup).

## Two TUI views

The TUI has two top-level views. **`A-t`** toggles between them.

### Sessions view

Each task has one or more sessions (Claude Code, Codex, or bash) running in a local PTY. The sidebar has two sub-views (toggle with `A-v`):

- **Status**: flat list of sessions, running ones first.
- **Task**: hierarchical — tasks as headers with their sessions indented underneath. Workflow-participant sessions form a sub-group under a workflow header with a vertical line down the left.

Local session state lives on disk:
- `~/.cm/tui-sessions.json` — the session manifest (label, type, session_id, hidden, workflow tags, etc.).
- `~/.claude/projects/<encoded>/*.jsonl` — Claude Code transcripts.
- `~/.codex/sessions/YYYY/MM/DD/<id>.jsonl` — Codex transcripts.
- `~/.cm/workflow-runs/<run-id>/` — per-run workflow state (`state.json` + `events.jsonl`).

### Planning view

A task board for triaging a backlog before you work on it. Tasks have statuses (`draft` → `backlog` → `running` → `done` / `blocked`). Launch a task from here with `A-f` (cloud) or `A-l` (linear-mode launch).

## Workflows (multi-agent framework)

A workflow is a TOML-defined state machine of agent roles running as sibling sessions on the same task.

- **Roles** have an engine (`claude-code` or `codex`), a context policy (`persistent` or `fresh`), and an optional activation prompt that's delivered to the PTY each time the role becomes active.
- **Transitions** are either static (`on_idle: to = "<role>"` in TOML) or dynamic (an agent calls `workflow_transition` or `workflow_done` via MCP).
- **Static `on_idle` transitions only fire after the outgoing role produces a new assistant message** — PTY startup noise doesn't trigger cascades.
- **`fresh` context** means the agent process is killed and respawned on activation; the session slot in the sidebar survives and its `session_id` swaps in place.
- **Templating** in activation prompts: `{{ roles.<role>.user[N] }}`, `{{ roles.<role>.assistant[N] }}` (negative indices work), plus aliases `last_message` and `initial_prompt`. Indices are relative to launch time — prior session history is invisible.

### Built-in: feedback mode

`workflows/feedback.toml` — worker (persistent) → reviewer (fresh) → manager (persistent). Reviewer audits `git diff`; manager decides whether to iterate or finish by calling `workflow_transition` / `workflow_done`. The manager template surfaces the worker's original prompt as `{{ roles.worker.initial_prompt }}` so decisions are anchored to the original goal.

### Workflow keybindings (Sessions view)

- `A-f` — launch a workflow on the focused session (prefills feedback mode)
- `A-u` — resume a paused workflow (runs auto-pause when you type into a participant session)
- `A-o` — stop the workflow (sessions stay open; their transcripts persist)
- `A-y` — show the workflow's history

## Other TUI keybindings

Global:
- `A-t` — toggle Sessions / Planning
- `A-q` — quit
- `A-j/k` — navigate
- `A-d` — mark task done
- `A-x` — delete
- `A-,` — toggle the activity feed (5-line strip above the status bar showing recent agent-initiated mutations: start_session, send_input, kill_session, start/stop_workflow, create_subtask, mark_subtask_done). Off by default.

Sessions view:
- `A-n` — new local session (creates a worktree)
- `A-s` — add a session to the focused task
- `A-a` — attach
- `A-w` — close session
- `A-H` — hide session's status indicator (also used to un-hide workflow participants, which default to hidden). Moved from `A-h`; the old `A-H` active-host switcher is retired (global host is being removed — new sessions use the `local` default).
- `A-h` / `A-l` — move the sidebar cursor LEFT / RIGHT between the main column and the **continuous column** (when the continuous column is shown; see `A-c`). `A-j`/`A-k` stay vertical within the focused column. See `DESIGN_CONTINUOUS_PANEL.md`.
- `A-c` — toggle the dedicated **continuous column** (orchestrators with their spawned subtasks nested) on/off. This is the single continuous control: ON = a third pane splits off the right (terminal | main | continuous) showing the continuous tree; OFF = continuous tasks are hidden entirely. Continuous tasks (an orchestrator + its subtasks, matched by `managed_by_uid` **or** task-tree `parent_task_id` so they group correctly across orchestrator respawns) render **only** in this column — never in the main sidebar. Persisted. (The old `A-C` column toggle + the separate `A-c` master-hide were merged into this one key.)
- `A-e` — settings for the focused row (Tab cycles fields; Space toggles checkboxes; Space/←/→ cycles color pickers). On a **session**: label, idle/burst timers, hidden, notify-on-idle, **global perms**, accent color. On a **workspace**: name, accent color (cascades to its sessions), **pinned** (pinned workspaces sort to the top of the sidebar with a 📌 marker). On a **task** subheader: name, accent color (stored TUI-side in the manifest's `task_colors` sidecar — tasks live in the planning API). Colors come from the named `USER_COLORS` palette and tint the row in the sidebar; selection highlight still overrides.
- `A-v` — toggle Status / Task sub-view
- `A-g` — jump to the next session needing attention (pending `notify_user` alerts first, then idle sessions; wraps, crosses into the continuous column)
- `A-;` — MRU quick-switch: alt-tab through recently focused sessions (first press ping-pongs A↔B; repeated presses walk deeper; any other key resets the walk)
- `A-p` — fuzzy-find palette: type-to-filter across all workspaces/sessions (case-insensitive substring, prefix matches ranked first; Up/Down/Tab/C-j/C-k select, Enter jumps). Sessions view only — planning keeps `A-p` as project picker.
- `A-i` — detail peek: read-only overlay with the focused row's bound task (name, status, full prompt — "what was this agent asked to do"), or workspace/session info when unbound. j/k / PgUp/PgDn scroll.
- `A-'` — yank the focused session's last assistant message to the clipboard (OSC 52, works over SSH; ~100KB cap with truncation notice)
- `A-9` — push (cloud) · `A-0` — pull (cloud)  *(moved off `A-p`/`A-l`)*
- `A-r` — refresh
- `A-R` — **revive / restart** the focused session in place: same uid, label, and task binding, with the conversation resumed (claude `--resume` / codex `resume`; bash respawns fresh). On a **dead** session it's a revive; on a **live** session it's a forced restart — the TUI kills the daemon child, waits (bounded ~2s) for the reaper to clear the uid, then runs the same revive flow (use case: pick up an updated agent binary/model without losing the conversation). Local sessions re-run the startup-restore primitive for one slot (re-attach if the daemon still holds the uid live, else respawn-resumed); remote sessions go through the daemon's `session.revive` RPC (argv/env composed daemon-side) and then auto-reattach via the deferred-reattach flow. Workflow participants are refused (the workflow engine owns their lifecycle — `A-u` resumes the run), as are continuous sessions (scheduler-owned).

Sidebar/pane state cues: idle sessions age through three visual buckets — just-finished "afterglow" (bright `●`, <2 min), settled (white `●`), and stale (>30 min, glyph + label dimmed). The status bar carries a session rollup (`⠹2 ●1 ⚑1` = running / idle / pending alerts) next to the task counts. The terminal pane's border tints by the active session's state (green running, yellow reconnecting, afterglow just-idle) and shows a right-aligned `▲ scrollback` tag whenever the view isn't at the live tail.

Planning view:
- `/` — incremental search: live-jumps to the first match as you type (Esc restores cursor + fold state, Enter commits). `n`/`N` cycle through matches afterwards with a `match i/N` echo; matching titles highlight; jumping to a folded subtask auto-unfolds its ancestors. The old always-on `[debug]` grid line is gone (re-enable with `CM_PLANNING_DEBUG=1`).
- `A-e` edit, `A-n` new, `A-N` new subtask of focused task (persists `parent_task_id` on the API row; same input form as `A-n` with the parent name shown for confirmation; worktree mode defaults to `inherit`), `A-a` accept (claim claude-proposed task), `A-i` insert header (bold-text section label), `A-A` bulk-archive done tasks in current project (with confirm), `A-V` toggle archived task visibility, `A-s/S` cycle status, `A-f` launch (cloud), `A-g` grid/linear toggle, `Space` toggle subtask fold on focused parent
- `A-w` — **watch a cloud backtest** (only on a `kind=backtest` task): spawns a local, READ-ONLY terminal view of the worker VM's tmux (the pipeline runs in a root-owned tmux named `backtest`), rendered like any other session and switched to in the Sessions view. Runs `gcloud compute ssh <worker_vm> --project <metadata.vm.project> --zone <metadata.vm.zone> -- -t "TERM=xterm-256color sudo sh -c '…wait for the backtest session then exec tmux attach -r -t backtest…'"` — the `-r` makes the attach read-only so watching can never disturb the run. It waits (bounded ~120s) for the session because `worker_vm`/`ttyd_url` are stamped at VM-CREATE time, before the in-VM startup has created the tmux — so hitting `A-w` the instant a VM appears would otherwise race the session and get an immediate "no sessions" exit. **The VM's project comes from `metadata.vm.project`, NOT the CM default project** (backtest VMs run in `prediction-market-scalper`). Direct SSH by default (the backtest project's firewall already allows tcp:22); set `CM_BACKTEST_SSH_IAP=1` to route through `--tunnel-through-iap` if your network blocks outbound port 22. On a non-backtest task, or a backtest not yet dispatched (no `worker_vm`), `A-w` shows a status-line hint instead of spawning; if the VM is already gone the ssh fails and the session exits cleanly.

The grid renders subtasks as an indented tree under each parent in
the parent's column. Subtasks are hidden from their own raw column
position when their parent is in the same project — `cursor.row`
indexes into the tree-aware visible-rows projection (not raw
`GridLayout`), and `Space` toggles a parent's fold. The cursor stays
on the same task across fold toggles. Default state is fully
collapsed; fold state lives in memory only (resets on restart).
Raw-layout mutations (`A-H/L` move-column, separator/empty/header
inserts, status cycling on layout slots) no-op when the cursor is
on a synthetic subtask row — subtasks have no raw position to
operate on. `A-J/K` reorder is the exception: on a subtask it
swaps with the adjacent sibling under the same parent (clamped
to the parent's slot range, so subtasks never leak out from
under their parent). Task-level ops (`A-d` done, `A-s/S` status,
`A-e` edit, `A-x` delete, `A-f` launch) work on subtasks normally
since they go through the slug, not the raw layout.

## Cloud mode (optional, secondary)

The GCP path is fully functional but used less. All infra is in GCP project **`claude-manager-prod`**, zone **`us-east4-a`**.

### VMs

| VM | Role | IP | Notes |
|----|------|----|-------|
| `cm-manager` | API server + remote daemon host | `35.186.186.160` (static) | Runs uvicorn on port 8000 and `cm-daemon` (see Multi-host) |
| `cm-db` | PostgreSQL | `10.150.0.2` (internal) | Database: `claude_manager`, user: `cmuser` |
| `cm-worker-*` | Ephemeral workers | Dynamic | Launched by dispatch daemon from `cm-worker-base` image family |

Workers run Claude in tmux, accessible via ttyd on port 8080. The dispatch daemon auto-claims `backlog` tasks with `is_cloud=true AND project IS NULL`.

### Deploying API changes

The API runs from `/opt/claude-manager/` on `cm-manager` as a systemd service (`claude-manager.service`, `Restart=always`). The unit file at `/etc/systemd/system/claude-manager.service` sets `CM_DB_DSN`, `CM_API_TOKEN`, etc. as `Environment=` directives — no inline env vars or `nohup` needed at restart time. Logs go to `/var/log/claude-manager.log`.

```bash
gcloud compute scp <local-file> cm-manager:/tmp/<file> --zone=us-east4-a --project=claude-manager-prod
gcloud compute ssh cm-manager --zone=us-east4-a --project=claude-manager-prod \
  --command="sudo cp /tmp/<file> /opt/claude-manager/<path>"

gcloud compute ssh cm-manager --zone=us-east4-a --project=claude-manager-prod \
  --command="sudo systemctl restart claude-manager"
```

Changes to Python files under `api/`, `dispatch/`, or `cli/` need a redeploy + restart. The MCP server is installed both on user machines (for local sessions) and at `/opt/cm-daemon/mcp_server/` on cm-manager (for sessions running against the remote daemon). Local edits take effect on next local MCP spawn; remote edits need an scp + `systemctl restart cm-daemon` (see Multi-host). The TUI and local `workflows/` are built and run locally — no deploy needed.

**Don't `pkill -f uvicorn`** — the systemd unit auto-respawns immediately, so a manual nohup launch fights the systemd-spawned one for port 8000. Also, `pkill -f uvicorn` over `gcloud ssh` self-matches on the SSH command line (which contains "uvicorn") and kills its own shell, returning exit 255. Use `systemctl restart` instead.

### Database

- Migrations in `sql/` run on every API startup (`db.init_db()`). They must be idempotent (`IF NOT EXISTS`, etc.).
- Avoid row-level UPDATEs in migrations — they re-run on every restart.
- Connection uses the internal IP (`10.150.0.2`), only reachable from inside the GCP VPC.

### GCS

- `gs://cm-sessions` — cloud session JSONL files for push/pull and preemption recovery.

## Multi-host (`hosts.toml`)

The TUI can drive sessions on multiple host daemons declared in `~/.cm/hosts.toml`. `local` is always present (synthesized when the file is missing or doesn't declare it). Each entry has `name`, `transport` (`unix` or `ssh-unix`), and transport-specific fields. The sidebar groups sessions by host.

**Host is a per-workspace attribute, not a global mode** (`DESIGN_REMOVE_GLOBAL_HOST.md`). There is no global "active host" switcher — the retired `A-H` host-cycler is gone (`A-H` now toggles session-hidden). A session's host comes from the workspace it was created in: the A-n form carries a host field (←/→ to pick a configured host; defaults to `local`), and every other create path (A-s add-session, workflow respawn, MCP spawn) inherits the workspace's / caller's host. New sessions default to `local`; non-local hosts are a per-task pick.

Example `~/.cm/hosts.toml`:

```toml
[[host]]
name = "local"
transport = "unix"
socket = "~/.cm/daemon.sock"
default = true

[[host]]
name = "manager"
transport = "ssh-unix"
ssh_host = "cm-manager"
ssh_user = "lucas"
remote_socket = "/home/lucas/.cm/daemon.sock"
```

`ssh_host` resolves through `~/.ssh/config`, so the operator needs a `Host cm-manager` alias pointing at the VM (use `IdentityFile ~/.ssh/google_compute_engine` + `IdentitiesOnly yes` for the gcloud-managed key). On first use the TUI spawns an `ssh -fN -L <local-sock>:<remote-sock>` tunnel under a private 0o700 dir with an unguessable per-spawn suffix; readiness is detected via `UnixStream::connect()` (not stat). The tunnel is respawned on death.

**Attached remote sessions auto-reconnect.** If the laptop loses connectivity and the tunnel dies while you're attached to a remote session, the session's PTY I/O stream (which reads daemon-side PTY bytes over the tunnel socket) hits EOF. Because the daemon owns the PTY + workflow, that work keeps running — only the TUI's client-side attach stream is dead. The TUI detects this via the attach reader's ground-truth signal (a socket EOF with **no** structured `End` frame, latched as a `transport_eof` flag and surfaced through the same `Arc<AtomicBool>` side channel as `memory_cap_kill`), distinguishing it from a genuine daemon-side child exit (which always sends an `End` frame). On detection the session is **not** torn down: its sidebar indicator flips to `⟳` (yellow), the slot is preserved, and the entry is requeued into the existing `pending_remote_reattach` flow. The per-host `manifest.watch` consumer re-warms the tunnel off-thread (exponential backoff, re-resolving the socket path each attempt); once it's connectable, `drain_deferred_remote_reattach` rebinds the PTY **in place** to the still-alive daemon session — no lost work, no TUI restart. A genuine daemon-session-gone (tunnel up but reattach fails) marks the slot exited. Local (non-remote) sessions have no detachable transport and are unaffected.

### `cm-manager` as a remote daemon host

`cm-manager` runs `cm-daemon.service` alongside `claude-manager.service`. Layout on the VM:

- `/opt/cm-daemon/cm-daemon` — daemon binary (Linux x86_64 release build)
- `/opt/cm-daemon/mcp_server/` — MCP server source + `.venv/` (deps installed via `uv pip install -r requirements.txt`)
- `/opt/cm-daemon/workflows/` — workflow TOMLs
- `/home/lucas/.cm/daemon.sock` — control socket
- `/home/lucas/.cm/daemon.toml` — daemon config (mode 0600). Sets `mcp_server_path`, `api_url = "http://localhost:8000"`, `api_token`, `log_path`, `workflows_dir`, and `[auth] mode = "ssh-trust"` (the SSH session IS the auth — no separate operator token over SSH-unix).
- `/etc/systemd/system/cm-daemon.service` — `Restart=always`, runs as user `lucas`, `Environment=PATH=/opt/cm-daemon/mcp_server/.venv/bin:...`. Being a **system** unit run as `lucas`, it has no user-session bus, so `systemd-run --user --scope` (memory caps) can't create scopes: the daemon's capability probe degrades every fire to **uncapped** by default. To enable per-session memory caps, install `deploy/cm-daemon.service.d/user-scope-cap.conf` (adds `XDG_RUNTIME_DIR=/run/user/%U`) + `loginctl enable-linger lucas`. See `DESIGN_MEMORY_CAP.md` → "Daemon-side (headless) capping".

Deploying daemon-side changes:

```bash
# Binary
cargo build --release -p cm-daemon  # locally
gcloud compute scp target/release/cm-daemon cm-manager:/tmp/cm-daemon --zone=us-east4-a --project=claude-manager-prod
ssh cm-manager 'sudo cp /tmp/cm-daemon /opt/cm-daemon/cm-daemon && sudo systemctl restart cm-daemon'

# MCP server / workflows
gcloud compute scp --recurse mcp_server/ cm-manager:/tmp/ --zone=us-east4-a --project=claude-manager-prod
ssh cm-manager 'sudo cp -r /tmp/mcp_server/* /opt/cm-daemon/mcp_server/ && sudo systemctl restart cm-daemon'
```

`claude` (npm `@anthropic-ai/claude-code`) and `codex` (npm `@openai/codex`) are installed system-wide so the daemon can spawn them from any session.

### `cm-manager` backtest-replay DB (predictionTrading)

`cm-manager` also hosts a **local PostgreSQL 17 + TimescaleDB** for predictionTrading backtest replay (the download-from-prod-once, replay-from-local paradigm).

- **Tunnel to prod** — `cm-db-tunnel.service` (systemd, runs as `lucas`, `Restart=always`): `ssh -i ~/.ssh/db-east4-tunnel -N -L 5433:localhost:5432 lucas@34.181.167.141`. `db-east4` lives in a **different** GCP project (`prediction-market-scalper`), so rather than grant the cm-manager SA cross-project IAM (OS Login is off there), a dedicated `~/.ssh/db-east4-tunnel` key is in db-east4's *instance* `ssh-keys` metadata (`default-allow-ssh` already opens `:22`). Mirrors the local `gcloud compute ssh db-east4 -- -N -L 5433:localhost:5432`.
- **Live trader log access (`ssh trader`)** — cm-manager reaches the **live trader** box (`trader-east4-c2`, also in `prediction-market-scalper`) via an `ssh trader` alias in `~/.ssh/config` (`HostName 34.86.189.249`, `User lucas`, dedicated `IdentityFile`, `IdentitiesOnly yes`) — same cross-project trick as the db tunnel: a dedicated key in the trader instance's metadata, so no gcloud/IAM. The bug-triage orchestrator (and its bug-fix subtask agents) use it to scan `~/predictionTrading/logs/traderApp.log.<UTC-date>` (readable directly as `lucas`, no sudo). **From cm-manager use `ssh trader '<cmd>'`, NOT `gcloud compute ssh`** — gcloud there is the `claude-manager-prod` SA with no IAM on `prediction-market-scalper`, so it fails with `resource not found` / `compute.instances.get` permission errors. Documented agent-side in predictionTrading's `gcp-instances` skill.
- **Local DB** — `predictiondb` / `predictionuser`, `timescaledb-tune`'d for the VM. Schema from `prediction/common/application/data/events/setup.sql` (run with `psql -f`; it uses `\ir` includes → 22 tables / 9 hypertables / the `all_events` view). Seed a window with `cd ~/.cm/repos/predictionTrading && uv run python -m analysis.backtests.scripts.download_events --from-config <run>.yaml` over the tunnel (needs `uv sync` + a uv-managed py3.12 first).
- **`.env`** at `~/.cm/repos/predictionTrading/.env` (the CLONE — worktrees source it via git-common-dir): `POSTGRES_CONNECTION_STRING` (`:5433` → prod via tunnel) + `LOCAL_POSTGRES_CONNECTION_STRING` (`:5432` → local replay). Minimal — just the two DSNs.
- **MCPs** — `postgres-remote` rides the repo's committed `.mcp.json` (universal — prod is the right target anywhere); `postgres-local` is **host-global** in cm-manager's `~/.claude.json` `mcpServers` with an *absolute* path to the clone's `scripts/mcp/postgres-local.sh` (per-machine opt-in: not every host has a local replay DB — e.g. the trader instance — so it is intentionally NOT committed to `.mcp.json`). `claude mcp list` from the clone shows all three connected.
