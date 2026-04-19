# Claude Manager

A local-first Rust TUI for orchestrating coding sessions across multiple agents (Claude Code, Codex). The TUI spawns and manages local agent processes, organizes them by task, runs multi-agent *workflows* (e.g. worker → reviewer → manager iteration loops), and optionally dispatches work to ephemeral GCP VMs. A planning board view lets you triage a backlog and launch tasks from it.

**Primary mode is local.** The GCP/VM path is a secondary feature — most development and day-to-day use happens locally.

## Project overview

- **`tui/`** — Rust TUI client. The main product. Owns local session lifecycle (PTY spawning via alacritty), workflow orchestration, planning board rendering, and API communication. Build with `cargo build` from `tui/`. Run the binary directly; no daemon needed for local use.
- **`workflows/`** — TOML definitions for multi-agent workflows (e.g. `feedback.toml`). Loaded at TUI startup; each defines roles, transitions, and activation prompts. See the Workflows section below.
- **`mcp_server/`** — MCP server exposing tools that agents running inside the TUI can call:
  - `propose_task(...)` — push a task to the planning backlog.
  - `workflow_transition(to, prompt)` / `workflow_done(reason)` — workflow participants use these to hand off control or end a run. Events land in `~/.cm/workflow-runs/<id>/events.jsonl` and the TUI tails that file as its workflow control plane.
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
- `A-e` edit, `A-n` new, `A-s/S` cycle status, `A-f` launch (cloud), `A-g` grid/linear toggle

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

The API runs from `/opt/claude-manager/` on `cm-manager`.

```bash
gcloud compute scp <local-file> cm-manager:/tmp/<file> --zone=us-east4-a --project=claude-manager-prod
gcloud compute ssh cm-manager --zone=us-east4-a --project=claude-manager-prod \
  --command="sudo cp /tmp/<file> /opt/claude-manager/<path>"

gcloud compute ssh cm-manager --zone=us-east4-a --project=claude-manager-prod \
  --command="sudo pkill -f uvicorn"
gcloud compute ssh cm-manager --zone=us-east4-a --project=claude-manager-prod \
  --command='sudo bash -c '"'"'cd /opt/claude-manager && CM_DB_DSN="postgresql://cmuser:D8tO2oHwlCU%2FNLhH8GkkjdLeS69xQqjR@10.150.0.2/claude_manager" CM_API_TOKEN="HfxQJ9mAdZ3LUeZQNjvCDrvgR/GhBETvWtMlSVxBj2w=" CM_API_URL="http://34.11.80.141:8000" CM_GCP_PROJECT="claude-manager-prod" CM_GCP_ZONE="us-east4-a" CM_MAX_WORKERS=3 nohup /opt/claude-manager/.venv/bin/uvicorn api.main:app --host 0.0.0.0 --port 8000 > /var/log/claude-manager.log 2>&1 &'"'"''
```

Changes to Python files under `api/`, `dispatch/`, `mcp_server/`, or `cli/` need a redeploy + restart. The TUI and local `workflows/` are built and run locally — no deploy needed.

### Database

- Migrations in `sql/` run on every API startup (`db.init_db()`). They must be idempotent (`IF NOT EXISTS`, etc.).
- Avoid row-level UPDATEs in migrations — they re-run on every restart.
- Connection uses the internal IP (`10.150.0.2`), only reachable from inside the GCP VPC.

### GCS

- `gs://cm-sessions` — cloud session JSONL files for push/pull and preemption recovery.
