# Claude Manager — Design Document

## Overview

Run Claude Code tasks in the cloud. Claudes work autonomously on GCP spot VMs until they're done or blocked. Blocked Claudes show up in a queue. You work through the queue, interacting with each Claude directly via its terminal session, then move on to the next one.

Two things matter:
1. **Task backlog** — a list of tasks to kick off easily
2. **Blocked queue** — which Claudes need you right now

## Architecture

```
    You (CLI / Portal)
         |
         v
    Manager Instance (persistent GCP VM)
    ├── API Server (FastAPI)
    ├── Dispatch Daemon (polls backlog, launches workers)
    └── connects to Cloud SQL (Postgres)
         |
         | launches
         v
    Worker VMs (spot instances)
    ├── tmux session running Claude Code (interactive)
    └── ttyd exposing the session over HTTPS
```

## How It Works

**Kick off a task:**
```
cm add --repo predictionTrading --prompt "Fix the flaky test in test_calibration.py"
```

Task goes into the backlog. Dispatch daemon picks it up, spins up a spot VM, VM clones the repo, runs `setup.sh`, starts Claude Code with the prompt in a tmux session. ttyd exposes the session.

**Claude works autonomously.** It has `--permission-mode bypassPermissions` so it can do whatever it needs.

**Claude stops.** Either it's done or it's stuck and waiting for input. A watcher process detects that Claude is idle (waiting at the input prompt) and marks the task as `blocked` in the database.

**You see it in your queue:**
```
cm queue

  #  Task                                          Repo                Waiting
  1  Fix flaky test in test_calibration.py          predictionTrading   3m
  2  Add retry logic to scraper pipeline            predictionTrading   12m
```

**You open the session:**
```
cm open 1
```

This opens the ttyd URL in your browser — you're looking at the actual Claude Code session. You can see what Claude did, what it's asking, and type your response directly. When you're done, close the tab and move on to the next one.

**You mark it done when you're satisfied:**
```
cm done 1
```

VM shuts down and self-deletes.

## Database

Cloud SQL (managed Postgres), separate from the manager VM. Manager is stateless and disposable.

```sql
CREATE TABLE tasks (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- What to do
    repo_url        TEXT NOT NULL,
    repo_branch     TEXT NOT NULL DEFAULT 'main',
    prompt          TEXT NOT NULL,

    -- State
    status          TEXT NOT NULL DEFAULT 'backlog'
                    CHECK (status IN ('backlog', 'running', 'blocked', 'done')),
    priority        INTEGER NOT NULL DEFAULT 0,     -- lower = higher priority

    -- Worker
    worker_vm       TEXT,                            -- GCP instance name
    worker_zone     TEXT,
    ttyd_url        TEXT,                            -- URL to attach to the session
    blocked_at      TIMESTAMPTZ,                     -- when Claude stopped

    -- Preemption recovery
    session_id      TEXT,                            -- Claude Code session UUID for resume
    wip_branch      TEXT,                            -- branch with WIP commit
    resume_metadata JSONB
);

CREATE INDEX idx_tasks_backlog ON tasks (priority, created_at) WHERE status = 'backlog';
CREATE INDEX idx_tasks_blocked ON tasks (blocked_at) WHERE status = 'blocked';
```

Four statuses. That's it.

- **backlog**: Waiting to be picked up by the dispatch daemon.
- **running**: A worker VM is executing. Claude is working.
- **blocked**: Claude stopped and is waiting for you. Shows up in your queue.
- **done**: You marked it done. Worker VM self-deletes.

## CLI

```
cm add --repo <name> --prompt "..."          # add to backlog
cm add --repo <name> --prompt-file spec.md

cm backlog                                    # list backlog
cm queue                                      # list blocked tasks (your work queue)
cm open <id>                                  # open ttyd session in browser
cm done <id>                                  # mark done, VM shuts down
cm cancel <id>                                # kill task, VM shuts down
cm reorder <id> --priority <n>                # reorder backlog

cm workers                                    # list active VMs
cm status                                     # summary: backlog depth, running, blocked
```

## Worker VM

### Startup

```bash
#!/usr/bin/env bash
set -euo pipefail

TASK_ID=$(curl -s "http://metadata.google.internal/computeMetadata/v1/instance/attributes/task-id" \
    -H "Metadata-Flavor: Google")
MANAGER_URL=$(curl -s "http://metadata.google.internal/computeMetadata/v1/instance/attributes/manager-url" \
    -H "Metadata-Flavor: Google")
REPO_URL=$(curl -s "http://metadata.google.internal/computeMetadata/v1/instance/attributes/repo-url" \
    -H "Metadata-Flavor: Google")
REPO_BRANCH=$(curl -s "http://metadata.google.internal/computeMetadata/v1/instance/attributes/repo-branch" \
    -H "Metadata-Flavor: Google")

# Credentials
export ANTHROPIC_API_KEY=$(gcloud secrets versions access latest --secret=claude-api-key)
GITHUB_TOKEN=$(gcloud secrets versions access latest --secret=github-app-token)
git config --global url."https://x-access-token:${GITHUB_TOKEN}@github.com/".insteadOf "https://github.com/"

# Clone and setup
git clone -b "$REPO_BRANCH" "$REPO_URL" /workspace
cd /workspace
./setup.sh

# Check if resuming a preempted session
TASK_JSON=$(curl -s "$MANAGER_URL/tasks/$TASK_ID")
PROMPT=$(echo "$TASK_JSON" | jq -r .prompt)
SESSION_ID=$(echo "$TASK_JSON" | jq -r '.session_id // empty')
WIP_BRANCH=$(echo "$TASK_JSON" | jq -r '.wip_branch // empty')

# Checkout WIP branch if resuming
if [ -n "$WIP_BRANCH" ]; then
    git checkout "$WIP_BRANCH" 2>/dev/null || true
fi

# Restore session file if resuming
CLAUDE_ARGS="--permission-mode bypassPermissions --dangerously-skip-permissions"
if [ -n "$SESSION_ID" ]; then
    mkdir -p ~/.claude/projects/-workspace/
    gsutil cp "gs://cm-sessions/${TASK_ID}/${SESSION_ID}.jsonl" \
        ~/.claude/projects/-workspace/
    # Restore subagent files if they exist
    gsutil -m cp -r "gs://cm-sessions/${TASK_ID}/${SESSION_ID}/" \
        ~/.claude/projects/-workspace/ 2>/dev/null || true
    CLAUDE_ARGS="$CLAUDE_ARGS --resume $SESSION_ID"
fi

# Start Claude in tmux
tmux new-session -d -s claude -x 200 -y 50
tmux send-keys -t claude "claude $CLAUDE_ARGS" Enter
sleep 3

# Only send prompt if this is a fresh session (not a resume)
if [ -z "$SESSION_ID" ]; then
    tmux send-keys -t claude "$PROMPT" Enter
fi

# Expose via ttyd
TTYD_TOKEN=$(curl -s "$MANAGER_URL/workers/$(hostname)/token" | jq -r .token)
ttyd -p 7681 -c "dispatch:${TTYD_TOKEN}" tmux attach -t claude &
curl -s -X PATCH "$MANAGER_URL/tasks/$TASK_ID" \
    -H "Content-Type: application/json" \
    -d "{\"ttyd_url\": \"https://$(hostname):7681\"}"

# Watcher: detect when Claude is idle
python3 /opt/dispatch/watcher.py \
    --task-id "$TASK_ID" \
    --manager-url "$MANAGER_URL" \
    --tmux-session claude &

# Preemption handler
trap 'on_preempt' SIGTERM
on_preempt() {
    cd /workspace
    BRANCH="dispatch/wip-${TASK_ID}"
    git checkout -b "$BRANCH" 2>/dev/null || git checkout "$BRANCH"
    git add -A && git commit -m "WIP: preempted task $TASK_ID" || true
    git push origin "$BRANCH" || true

    # Save Claude session for resume on new worker
    SESSION_FILE=$(ls ~/.claude/projects/-workspace/*.jsonl 2>/dev/null | head -1)
    SESSION_ID=$(basename "$SESSION_FILE" .jsonl)
    if [ -n "$SESSION_FILE" ]; then
        gsutil cp "$SESSION_FILE" "gs://cm-sessions/${TASK_ID}/${SESSION_ID}.jsonl"
        # Copy subagent files if they exist
        SESSION_DIR="${SESSION_FILE%.jsonl}"
        [ -d "$SESSION_DIR" ] && gsutil -m cp -r "$SESSION_DIR" "gs://cm-sessions/${TASK_ID}/"
    fi

    curl -s -X PATCH "$MANAGER_URL/tasks/$TASK_ID" \
        -H "Content-Type: application/json" \
        -d "{\"status\": \"backlog\", \"wip_branch\": \"$BRANCH\", \"session_id\": \"$SESSION_ID\"}"
    exit 0
}

wait
```

### Watcher

A lightweight process on the worker VM that monitors the tmux session. When Claude is waiting at the input prompt (idle), it hits the manager API:

```
PATCH /tasks/{id}  { "status": "blocked", "blocked_at": "..." }
```

When the user interacts via ttyd and Claude starts working again, the watcher detects activity and sets it back to `running`.

Implementation: poll `tmux capture-pane` output every few seconds, look for the Claude Code input prompt pattern. Simple.

### Base Image

Pre-baked GCP image with:
- Ubuntu 24.04, Python 3.12 + uv, Node.js 22, Claude Code CLI
- gcloud CLI, git, tmux, ttyd, jq, curl
- Build deps (gcc, libffi, etc.)
- `/opt/dispatch/watcher.py`

Keeps boot time ~30s instead of ~5min.

## Secrets

Two GCP projects:

| Project | Secrets |
|---|---|
| `prediction-market-scalper` | Trading repo secrets (API keys, DB creds, etc.) |
| `claude-manager` | Infra secrets (Claude API key, GitHub App key, Cloud SQL password) |

Workers get a service account with `secretmanager.secretAccessor` on both projects. Manager gets accessor on `claude-manager` plus `compute.instanceAdmin` for launching workers and `cloudsql.client` for DB access.

## Git Auth

GitHub App installed on all repos dispatch targets. Manager generates short-lived installation tokens (1hr). Workers use them for git operations — no SSH keys, no long-lived PATs.

## Repo Contract

Every target repo must have `setup.sh` in the root. `git clone && ./setup.sh` must work on a fresh VM with gcloud auth. The dispatch system doesn't care what's in it.

## Implementation Phases

### Phase 0: Validate Assumptions — DONE
All validated locally:
- **Session resume across machines**: Works. Copy `~/.claude/projects/{path}/{sessionId}.jsonl` to the new machine, run `claude --resume <sessionId>` — full conversation history restored. Store session files in GCS bucket between preemptions.
- **tmux + Claude Code**: Works. `tmux send-keys` sends prompts, Claude executes them.
- **Idle detection**: Works. Poll `tmux capture-pane`, check last non-blank line for `esc to interrupt` — present means busy, absent means idle (Claude waiting for input).
- **Sending input via tmux**: Works. `tmux send-keys -t <session> "<text>" Enter`.
- **ttyd**: Not yet tested (needs install). Not a blocker for Phase 1.
- **Workspace trust dialog**: Claude shows a trust prompt on first run in a new directory. Need to pre-accept on base image or find a bypass.

### Phase 1: Single Worker MVP
- Local Postgres, Python script on laptop
- Launch one spot VM, run one task end-to-end
- Watcher detects blocked, CLI shows it, `open` attaches via ttyd
- Manual `done` to mark complete

### Phase 2: Cloud Infrastructure
- Create `claude-manager` GCP project
- Cloud SQL, manager VM, GitHub App
- API server, dispatch daemon
- CLI talks to manager over HTTPS

### Phase 3: Preemption Handling
- SIGTERM → git commit/push WIP → upload session JSONL to GCS → set task back to backlog
- New worker: download session from GCS → checkout WIP branch → `claude --resume <sessionId>`
- Full conversation context restored, Claude continues where it left off

### Phase 4: Multiple Workers
- Dispatch daemon manages concurrent workers
- Configurable max workers
- Zombie detection

### Phase 5: Web Portal
- Task list + blocked queue UI (design TBD)
- Embedded ttyd for each worker
- Mobile-friendly

### Phase 6: Meta-Agent
- Claude managing the task board
- Task decomposition, follow-up creation, prioritization

## Session Resume — How It Works

Claude Code stores conversation history in JSONL files:

```
~/.claude/projects/{project-path}/{sessionId}.jsonl     # conversation messages (required)
~/.claude/projects/{project-path}/{sessionId}/          # subagents + tool results (optional)
```

The project path is derived from cwd: `/workspace` → `-workspace`.

**To resume on a new machine:**
1. Copy the `.jsonl` file (and optional subdirectory) to `~/.claude/projects/-workspace/`
2. Run `claude --resume <sessionId>` from `/workspace`

Full conversation context is restored. Claude picks up exactly where it left off.

**For preemption:** Session files are uploaded to GCS (`gs://cm-sessions/{taskId}/`) on SIGTERM and downloaded on the replacement worker's boot.

## Open Questions

- **Workspace trust dialog**: Claude prompts "Is this a project you trust?" on first run in a new directory. Need to find a way to pre-accept this on the base image or via a CLI flag. May need to pre-create a trust entry in `~/.claude/`.
- **Worker idle cost**: While blocked, the VM is running but idle. Spot VMs are ~$0.01/hr so this is cheap, but if you don't get to the queue for hours, consider VM suspend/resume.
- **Preemption while blocked**: If a spot VM gets preempted while in `blocked` state (waiting for you), the preemption handler fires — same as during `running`. Session saved, WIP committed, task goes back to backlog.
- **Session file size**: Long conversations could produce large JSONL files. Need to monitor. For most tasks this should be manageable (our test conversation was ~13KB for 3 turns).
