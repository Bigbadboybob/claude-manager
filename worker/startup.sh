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

# ---------------------------------------------------------------------------
# Credentials
# ---------------------------------------------------------------------------
GCP_PROJECT="prediction-market-scalper"

# Long-lived OAuth token (1 year, from `claude setup-token`)
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

# Set onboarding complete flag (skips all first-run dialogs)
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

# Write the OAuth token to worker's bashrc so it's available in tmux
echo "export CLAUDE_CODE_OAUTH_TOKEN=$CLAUDE_OAUTH_TOKEN" >> "$WORKER_HOME/.bashrc"

# ---------------------------------------------------------------------------
# Start Claude Code in tmux
# ---------------------------------------------------------------------------
su - worker -c "tmux new-session -d -s claude -x 200 -y 50"
su - worker -c "tmux send-keys -t claude \
    'cd /workspace && claude --dangerously-skip-permissions --permission-mode bypassPermissions' Enter"

# Wait for Claude to be ready
for attempt in $(seq 1 30); do
    sleep 3
    PANE=$(su - worker -c "tmux capture-pane -t claude -p" 2>/dev/null || echo "")

    if echo "$PANE" | grep -q 'bypass permissions on'; then
        echo "[cm-worker] Claude ready (attempt $attempt)"
        break
    fi

    # Dismiss any remaining dialog
    su - worker -c "tmux send-keys -t claude Enter"
    echo "[cm-worker] Sent Enter (attempt $attempt)"
done

# Send the task prompt
PROMPT_FILE=$(mktemp)
echo "$TASK_PROMPT" > "$PROMPT_FILE"
chmod 644 "$PROMPT_FILE"
su - worker -c "tmux send-keys -t claude \"\$(cat $PROMPT_FILE)\" Enter"
rm -f "$PROMPT_FILE"

echo "[cm-worker] Prompt sent"

# ---------------------------------------------------------------------------
# Start ttyd
# ---------------------------------------------------------------------------
ttyd -i 0.0.0.0 -p 8080 --writable su - worker -c "tmux attach -t claude" &

EXTERNAL_IP=$(curl -sf "http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip" \
    -H "Metadata-Flavor: Google")
TTYD_URL="http://${EXTERNAL_IP}:8080"
echo "[cm-worker] ttyd at $TTYD_URL"

# Report ttyd URL to manager
api_update "{\"ttyd_url\": \"$TTYD_URL\"}"

# Also write state file for debugging
echo "running" > /var/log/cm-worker-state

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
