# Claude Manager

Task orchestration system for planning and running Claude coding sessions. Primarily used **locally with git worktrees** for day-to-day work; cloud dispatch to ephemeral GCP VMs is still supported for cases where it's useful (long-running tasks, isolation, running things away from the local machine).

> **Note:** This project started out cloud-first, but in practice local + worktrees turned out to be much smoother and is now the default mode. Cloud support is retained but secondary. When working on this project, assume local usage unless the user explicitly mentions cloud.

## Project overview

- **`api/`** — FastAPI server (runs on `cm-manager` VM). Task CRUD, dispatch daemon, warm pool management.
- **`dispatch/`** — DB access (`db.py`), VM lifecycle (`vm.py`), config (`config.py`).
- **`tui/`** — Rust TUI client. Two views: **Work** (active sessions) and **Planning** (task board).
- **`mcp_server/`** — MCP server so Claude instances can propose tasks back to the backlog.
- **`cli/`** — CLI client and planning client library.
- **`worker/`** — Startup scripts for worker VMs.
- **`sql/`** — Database migrations, auto-run on API startup via `db.init_db()`.

## Infrastructure

All infra is in GCP project **`claude-manager-prod`**, zone **`us-east4-a`**.

### VMs

| VM | Role | IP | Notes |
|----|------|----|-------|
| `cm-manager` | API server | `34.11.80.141` | Runs uvicorn on port 8000 |
| `cm-db` | PostgreSQL | `10.150.0.2` (internal) | Database: `claude_manager`, user: `cmuser` |
| `cm-worker-*` | Ephemeral workers | Dynamic | Launched by dispatch daemon from `cm-worker-base` image family |

### Deploying code changes

The API runs from `/opt/claude-manager/` on `cm-manager`. To deploy:

```bash
# Copy changed files
gcloud compute scp <local-file> cm-manager:/tmp/<file> --zone=us-east4-a --project=claude-manager-prod
gcloud compute ssh cm-manager --zone=us-east4-a --project=claude-manager-prod \
  --command="sudo cp /tmp/<file> /opt/claude-manager/<path>"

# Restart API (kill old, start new)
gcloud compute ssh cm-manager --zone=us-east4-a --project=claude-manager-prod \
  --command="sudo pkill -f uvicorn"
gcloud compute ssh cm-manager --zone=us-east4-a --project=claude-manager-prod \
  --command='sudo bash -c '"'"'cd /opt/claude-manager && CM_DB_DSN="postgresql://cmuser:D8tO2oHwlCU%2FNLhH8GkkjdLeS69xQqjR@10.150.0.2/claude_manager" CM_API_TOKEN="HfxQJ9mAdZ3LUeZQNjvCDrvgR/GhBETvWtMlSVxBj2w=" CM_API_URL="http://34.11.80.141:8000" CM_GCP_PROJECT="claude-manager-prod" CM_GCP_ZONE="us-east4-a" CM_MAX_WORKERS=3 nohup /opt/claude-manager/.venv/bin/uvicorn api.main:app --host 0.0.0.0 --port 8000 > /var/log/claude-manager.log 2>&1 &'"'"''
```

When changing Python files (api/, dispatch/, mcp_server/, cli/), you MUST deploy to the VM and restart the API. The TUI binary is built and run locally.

### Database

- Migrations in `sql/` are run on every API startup (`db.init_db()`). They must be idempotent (use `IF NOT EXISTS`, etc.).
- Avoid row-level UPDATE statements in migrations — they re-run on every restart.
- Connection string uses the internal IP (`10.150.0.2`), only accessible from within the GCP VPC.

### Worker VMs

- Base image: `cm-worker-base` family in `claude-manager-prod`
- Launched by `dispatch/vm.py`, managed by `api/dispatch_daemon.py`
- Workers run Claude in a tmux session, accessible via ttyd on port 8080
- The dispatch daemon auto-claims `backlog` tasks with `is_cloud=true` and `project IS NULL` (planning tasks are launched manually)

### GCS

- `gs://cm-sessions` — session JSONL files for push/pull and preemption recovery

## Development notes

- The TUI is Rust (`tui/`), build with `cargo build` from `tui/`. It connects to the API specified by `CM_API_URL`.
- Task status flow: `draft` → `backlog` → `running` → `done` (or `blocked`)
- Planning tasks (have `project` set) are NOT auto-dispatched. They're launched from the TUI with A-f.
- The dispatch daemon only auto-dispatches tasks with `is_cloud=true AND project IS NULL`.

## TUI keybindings

Both views use Alt+key. Consistent across views:
- **A-d** = done (mark task done)
- **A-x** = delete
- **A-q** = quit

Planning-specific: **A-e** edit, **A-n** new, **A-s/S** cycle status, **A-f** launch, **A-g** grid/linear toggle
Work-specific: **A-s** new session, **A-a** attach, **A-w** close, **A-p** push, **A-l** pull
