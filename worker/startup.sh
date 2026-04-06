#!/usr/bin/env bash
set -euo pipefail

export HOME=/root
export PATH="/root/.local/bin:/usr/local/bin:$PATH"

exec > /var/log/cm-worker.log 2>&1
echo "[cm-worker] Starting at $(date)"

# ---------------------------------------------------------------------------
# Metadata
# ---------------------------------------------------------------------------
META_URL="http://metadata.google.internal/computeMetadata/v1/instance/attributes"
META_HEADER="Metadata-Flavor: Google"

TASK_ID=$(curl -sf "$META_URL/task-id" -H "$META_HEADER")
REPO_URL=$(curl -sf "$META_URL/repo-url" -H "$META_HEADER")
REPO_BRANCH=$(curl -sf "$META_URL/repo-branch" -H "$META_HEADER")
TASK_PROMPT=$(curl -sf "$META_URL/task-prompt" -H "$META_HEADER")
MANAGER_URL=$(curl -sf "$META_URL/manager-callback-url" -H "$META_HEADER" || echo "")
API_TOKEN=$(curl -sf "$META_URL/api-token" -H "$META_HEADER" || echo "")

echo "[cm-worker] Task: $TASK_ID"
echo "[cm-worker] Repo: $REPO_URL (branch: $REPO_BRANCH)"
echo "[cm-worker] Manager: $MANAGER_URL"

# Helper: update task via API
api_update() {
    if [ -n "$MANAGER_URL" ] && [ -n "$API_TOKEN" ]; then
        curl -sf -X PATCH "$MANAGER_URL/tasks/$TASK_ID" \
            -H "Content-Type: application/json" \
            -H "Authorization: Bearer $API_TOKEN" \
            -d "$1" || echo "[cm-worker] WARNING: API callback failed"
    fi
}

# Fetch full task details from API (for session_id)
SESSION_ID=""
if [ -n "$MANAGER_URL" ] && [ -n "$API_TOKEN" ]; then
    TASK_JSON=$(curl -sf "$MANAGER_URL/tasks/$TASK_ID" \
        -H "Authorization: Bearer $API_TOKEN" || echo "{}")
    SESSION_ID=$(echo "$TASK_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin).get('session_id') or '')" 2>/dev/null || echo "")
fi
echo "[cm-worker] Session ID: ${SESSION_ID:-none}"

# ---------------------------------------------------------------------------
# Credentials
# ---------------------------------------------------------------------------
GCP_PROJECT="claude-manager-prod"
GCS_BUCKET="gs://cm-sessions-claude-manager"

CLAUDE_OAUTH_TOKEN=$(gcloud secrets versions access latest \
    --secret=claude-setup-token --project="$GCP_PROJECT")

GITHUB_TOKEN=$(gcloud secrets versions access latest \
    --secret=github-pat --project="$GCP_PROJECT")

echo "[cm-worker] Credentials loaded"

# ---------------------------------------------------------------------------
# Clone and setup
# ---------------------------------------------------------------------------
AUTHED_URL=$(echo "$REPO_URL" | sed "s|https://github.com/|https://x-access-token:${GITHUB_TOKEN}@github.com/|")
git clone -b "$REPO_BRANCH" "$AUTHED_URL" /workspace
cd /workspace
git config url."https://x-access-token:${GITHUB_TOKEN}@github.com/".insteadOf "https://github.com/"

if [ -f setup.sh ]; then
    echo "[cm-worker] Running setup.sh..."
    chmod +x setup.sh
    bash setup.sh || echo "[cm-worker] WARNING: setup.sh failed, continuing anyway"
fi

chown -R worker:worker /workspace
echo "[cm-worker] Repo ready"

# ---------------------------------------------------------------------------
# Configure Claude Code auth for worker user
# ---------------------------------------------------------------------------
WORKER_HOME=$(eval echo ~worker)

# Set onboarding complete flag
if [ -f "$WORKER_HOME/.claude.json" ]; then
    python3 -c "
import json
with open('$WORKER_HOME/.claude.json') as f: d = json.load(f)
d['hasCompletedOnboarding'] = True
with open('$WORKER_HOME/.claude.json', 'w') as f: json.dump(d, f)
"
else
    echo '{"hasCompletedOnboarding":true}' > "$WORKER_HOME/.claude.json"
fi
chown worker:worker "$WORKER_HOME/.claude.json"

# Write the OAuth token to worker's bashrc
echo "export CLAUDE_CODE_OAUTH_TOKEN=$CLAUDE_OAUTH_TOKEN" >> "$WORKER_HOME/.bashrc"

# ---------------------------------------------------------------------------
# Restore session if resuming (cm push -> cloud)
# ---------------------------------------------------------------------------
CLAUDE_ARGS="--dangerously-skip-permissions --permission-mode bypassPermissions"

if [ -n "$SESSION_ID" ]; then
    echo "[cm-worker] Restoring session $SESSION_ID from GCS..."
    mkdir -p "$WORKER_HOME/.claude/projects/-workspace"
    gsutil cp "$GCS_BUCKET/$SESSION_ID/$SESSION_ID.jsonl" \
        "$WORKER_HOME/.claude/projects/-workspace/" 2>/dev/null || echo "[cm-worker] WARNING: No session file in GCS"
    # Restore subagent files
    gsutil -m cp -r "$GCS_BUCKET/$SESSION_ID/$SESSION_ID/" \
        "$WORKER_HOME/.claude/projects/-workspace/" 2>/dev/null || true
    chown -R worker:worker "$WORKER_HOME/.claude/projects"
    CLAUDE_ARGS="$CLAUDE_ARGS --resume $SESSION_ID"
    echo "[cm-worker] Will resume session $SESSION_ID"
fi

# ---------------------------------------------------------------------------
# Start Claude Code in tmux
# ---------------------------------------------------------------------------
su - worker -c "tmux new-session -d -s claude -x 200 -y 50"
su - worker -c "tmux send-keys -t claude \
    'cd /workspace && claude $CLAUDE_ARGS' Enter"

# Wait for Claude to be ready
for attempt in $(seq 1 30); do
    sleep 3
    PANE=$(su - worker -c "tmux capture-pane -t claude -p" 2>/dev/null || echo "")

    if echo "$PANE" | grep -q 'bypass permissions on'; then
        echo "[cm-worker] Claude ready (attempt $attempt)"
        break
    fi

    su - worker -c "tmux send-keys -t claude Enter"
    echo "[cm-worker] Sent Enter (attempt $attempt)"
done

# Send prompt if this is a fresh task (not resuming) and prompt is non-empty
if [ -z "$SESSION_ID" ] && [ -n "$TASK_PROMPT" ]; then
    PROMPT_FILE=$(mktemp)
    echo "$TASK_PROMPT" > "$PROMPT_FILE"
    chmod 644 "$PROMPT_FILE"
    su - worker -c "tmux send-keys -t claude \"\$(cat $PROMPT_FILE)\" Enter"
    rm -f "$PROMPT_FILE"
    echo "[cm-worker] Prompt sent (async)"
elif [ -z "$SESSION_ID" ]; then
    echo "[cm-worker] No prompt — waiting for user (sync)"
else
    echo "[cm-worker] Resumed session (no prompt sent)"
fi

# ---------------------------------------------------------------------------
# Start ttyd
# ---------------------------------------------------------------------------
ttyd -i 0.0.0.0 -p 8080 --writable su - worker -c "tmux attach -t claude" &

EXTERNAL_IP=$(curl -sf "http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip" \
    -H "Metadata-Flavor: Google")
TTYD_URL="http://${EXTERNAL_IP}:8080"
echo "[cm-worker] ttyd at $TTYD_URL"

api_update "{\"ttyd_url\": \"$TTYD_URL\"}"
echo "running" > /var/log/cm-worker-state

# ---------------------------------------------------------------------------
# Preemption handler (GCP sends SIGTERM 30s before killing spot instances)
# ---------------------------------------------------------------------------
on_preempt() {
    echo "[cm-worker] PREEMPTION DETECTED — saving state..."

    # 1. Save Claude session file to GCS
    SESSION_FILE=$(ls -t /home/worker/.claude/projects/-workspace/*.jsonl 2>/dev/null | head -1)
    FOUND_SESSION_ID=""
    if [ -n "$SESSION_FILE" ]; then
        FOUND_SESSION_ID=$(basename "$SESSION_FILE" .jsonl)
        echo "[cm-worker] Saving session $FOUND_SESSION_ID to GCS..."
        gsutil cp "$SESSION_FILE" "$GCS_BUCKET/$FOUND_SESSION_ID/$FOUND_SESSION_ID.jsonl" 2>/dev/null || true
        # Save subagent files if they exist
        SESSION_DIR="${SESSION_FILE%.jsonl}"
        [ -d "$SESSION_DIR" ] && gsutil -m cp -r "$SESSION_DIR" "$GCS_BUCKET/$FOUND_SESSION_ID/" 2>/dev/null || true
        echo "[cm-worker] Session saved"
    else
        echo "[cm-worker] WARNING: No session file found"
    fi

    # 2. Git commit and push WIP
    cd /workspace
    WIP_BRANCH="cm/preempt-${TASK_ID:0:8}"
    su - worker -c "cd /workspace && git checkout -b $WIP_BRANCH 2>/dev/null || git checkout $WIP_BRANCH"
    su - worker -c "cd /workspace && git add -A && git commit -m 'WIP: preempted task ${TASK_ID:0:8}'" 2>/dev/null || true
    su - worker -c "cd /workspace && git push -u origin $WIP_BRANCH" 2>/dev/null || true
    echo "[cm-worker] WIP pushed to $WIP_BRANCH"

    # 3. Re-queue the task with session info
    REQUEUE_BODY="{\"status\": \"backlog\", \"wip_branch\": \"$WIP_BRANCH\""
    if [ -n "$FOUND_SESSION_ID" ]; then
        REQUEUE_BODY="$REQUEUE_BODY, \"session_id\": \"$FOUND_SESSION_ID\""
    fi
    REQUEUE_BODY="$REQUEUE_BODY}"
    api_update "$REQUEUE_BODY"
    echo "[cm-worker] Task re-queued to backlog"

    echo "[cm-worker] Preemption handling complete"
    exit 0
}

trap 'on_preempt' SIGTERM

# ---------------------------------------------------------------------------
# Watcher: detect idle (blocked) vs working (running)
# ---------------------------------------------------------------------------
sleep 30  # grace period

LAST_STATE="running"
echo "[cm-worker] Watcher started"

while true; do
    sleep 5

    if ! su - worker -c "tmux has-session -t claude" 2>/dev/null; then
        echo "[cm-worker] Claude session ended"
        echo "done" > /var/log/cm-worker-state
        api_update '{"status": "done"}'
        break
    fi

    STATUS_LINE=$(su - worker -c "tmux capture-pane -t claude -p" 2>/dev/null | grep -v '^$' | tail -1)

    if echo "$STATUS_LINE" | grep -q 'esc to interrupt'; then
        CURRENT_STATE="running"
    else
        CURRENT_STATE="blocked"
    fi

    if [ "$CURRENT_STATE" != "$LAST_STATE" ]; then
        echo "[cm-worker] $LAST_STATE -> $CURRENT_STATE"
        echo "$CURRENT_STATE" > /var/log/cm-worker-state

        if [ "$CURRENT_STATE" = "blocked" ]; then
            api_update "{\"status\": \"blocked\", \"blocked_at\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\"}"
        else
            api_update '{"status": "running"}'
        fi

        LAST_STATE="$CURRENT_STATE"
    fi
done

echo "[cm-worker] Watcher exited"
wait
