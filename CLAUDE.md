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
  - `ping`, `list_sessions`, `mcp_start_session`, `send_input`, `read_session_output`, `kill_session` — agent orchestration. Routed per-method by the resolver: session/PTY-touching methods go to `~/.cm/daemon.sock` (the daemon owns the registry); workflow-touching methods stay on `~/.cm/tui.sock`. Auth keys off `CM_TUI_SESSION_ID` injected when the TUI spawns the agent; descendant-only scope.
  - `start_workflow`, `stop_workflow`, `get_workflow_state`, `list_workflows` — orchestrator-side workflow control. Caller is the orchestrator, NOT a participant.
  - `create_subtask`, `list_subtasks`, `mark_subtask_done` — subtasks fork off a parent task with `parent_task_id` set. `worktree_mode="branch"` creates a new worktree under `cm-sub/<slug-chain>-<short_id>`.

## Permission convention for agents (Phase 7)

You have tools that can spawn subtasks, start sessions, send input to other sessions, kill sessions, start workflows, and create subtasks. **Before using any of them, state your intent in plain language and ask the user to confirm.** The tools will not refuse you, but the user expects to stay in the loop. Apply the same convention to anything destructive (killing a session, marking a subtask done, stopping a workflow).

Read-only tools (`ping`, `list_sessions`, `read_session_output`, `get_workflow_state`, `list_workflows`, `list_subtasks`) don't need pre-approval — call them as needed.

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
- `A-h` — hide session's status indicator (also used to un-hide workflow participants, which default to hidden)
- `A-e` — session settings
- `A-v` — toggle Status / Task sub-view
- `A-p` — push (cloud)
- `A-l` — pull (cloud)
- `A-r` — refresh

Planning view:
- `A-e` edit, `A-n` new, `A-N` new subtask of focused task (persists `parent_task_id` on the API row; same input form as `A-n` with the parent name shown for confirmation; worktree mode defaults to `inherit`), `A-a` accept (claim claude-proposed task), `A-i` insert header (bold-text section label), `A-A` bulk-archive done tasks in current project (with confirm), `A-V` toggle archived task visibility, `A-s/S` cycle status, `A-f` launch (cloud), `A-g` grid/linear toggle

## Cloud mode (optional, secondary)

The GCP path is fully functional but used less. All infra is in GCP project **`claude-manager-prod`**, zone **`us-east4-a`**.

### VMs

| VM | Role | IP | Notes |
|----|------|----|-------|
| `cm-manager` | API server | `34.11.80.141` | Runs uvicorn on port 8000 |
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

Changes to Python files under `api/`, `dispatch/`, or `cli/` need a redeploy + restart. The MCP server runs locally on user machines (cm-manager has no `mcp_server/` directory), so changes there take effect on next local MCP spawn. The TUI and local `workflows/` are built and run locally — no deploy needed.

**Don't `pkill -f uvicorn`** — the systemd unit auto-respawns immediately, so a manual nohup launch fights the systemd-spawned one for port 8000. Also, `pkill -f uvicorn` over `gcloud ssh` self-matches on the SSH command line (which contains "uvicorn") and kills its own shell, returning exit 255. Use `systemctl restart` instead.

### Database

- Migrations in `sql/` run on every API startup (`db.init_db()`). They must be idempotent (`IF NOT EXISTS`, etc.).
- Avoid row-level UPDATEs in migrations — they re-run on every restart.
- Connection uses the internal IP (`10.150.0.2`), only reachable from inside the GCP VPC.

### GCS

- `gs://cm-sessions` — cloud session JSONL files for push/pull and preemption recovery.
