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
- `A-H` — switch active host (cycles through entries in `~/.cm/hosts.toml`; see Multi-host below)
- `A-e` — session settings
- `A-v` — toggle Status / Task sub-view
- `A-p` — push (cloud)
- `A-l` — pull (cloud)
- `A-r` — refresh

Planning view:
- `A-e` edit, `A-n` new, `A-N` new subtask of focused task (persists `parent_task_id` on the API row; same input form as `A-n` with the parent name shown for confirmation; worktree mode defaults to `inherit`), `A-a` accept (claim claude-proposed task), `A-i` insert header (bold-text section label), `A-A` bulk-archive done tasks in current project (with confirm), `A-V` toggle archived task visibility, `A-s/S` cycle status, `A-f` launch (cloud), `A-g` grid/linear toggle, `Space` toggle subtask fold on focused parent

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

The TUI can drive sessions on multiple host daemons declared in `~/.cm/hosts.toml`. `local` is always present (synthesized when the file is missing or doesn't declare it). Each entry has `name`, `transport` (`unix` or `ssh-unix`), and transport-specific fields. Switch the active host with `A-H` in the Sessions view; the sidebar groups sessions by host.

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
- `/etc/systemd/system/cm-daemon.service` — `Restart=always`, runs as user `lucas`, `Environment=PATH=/opt/cm-daemon/mcp_server/.venv/bin:...`

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
