#!/usr/bin/env bash
set -euo pipefail

export HOME=/root
export PATH="/root/.local/bin:/usr/local/bin:$PATH"

exec > /var/log/cm-worker.log 2>&1
echo "[cm-warm] Starting warm VM at $(date)"

# ---------------------------------------------------------------------------
# Metadata
# ---------------------------------------------------------------------------
META_URL="http://metadata.google.internal/computeMetadata/v1/instance/attributes"
META_HEADER="Metadata-Flavor: Google"

REPO_URL=$(curl -sf "$META_URL/repo-url" -H "$META_HEADER")
REPO_BRANCH=$(curl -sf "$META_URL/repo-branch" -H "$META_HEADER")
MANAGER_URL=$(curl -sf "$META_URL/manager-callback-url" -H "$META_HEADER" || echo "")
API_TOKEN=$(curl -sf "$META_URL/api-token" -H "$META_HEADER" || echo "")
POOL_ID=$(curl -sf "$META_URL/pool-id" -H "$META_HEADER" || echo "")

echo "[cm-warm] Repo: $REPO_URL (branch: $REPO_BRANCH)"
echo "[cm-warm] Pool: $POOL_ID"

# ---------------------------------------------------------------------------
# Credentials
# ---------------------------------------------------------------------------
GCP_PROJECT="prediction-market-scalper"

CLAUDE_OAUTH_TOKEN=$(gcloud secrets versions access latest \
    --secret=claude-setup-token --project="$GCP_PROJECT")

GITHUB_TOKEN=$(gcloud secrets versions access latest \
    --secret=github-pat --project="$GCP_PROJECT")

echo "[cm-warm] Credentials loaded"

# ---------------------------------------------------------------------------
# Clone and setup
# ---------------------------------------------------------------------------
AUTHED_URL=$(echo "$REPO_URL" | sed "s|https://github.com/|https://x-access-token:${GITHUB_TOKEN}@github.com/|")
git clone -b "$REPO_BRANCH" "$AUTHED_URL" /workspace
cd /workspace
git config url."https://x-access-token:${GITHUB_TOKEN}@github.com/".insteadOf "https://github.com/"

if [ -f setup.sh ]; then
    echo "[cm-warm] Running setup.sh..."
    chmod +x setup.sh
    bash setup.sh || echo "[cm-warm] WARNING: setup.sh failed"
fi

chown -R worker:worker /workspace
echo "[cm-warm] Repo ready"

# ---------------------------------------------------------------------------
# Configure Claude Code auth
# ---------------------------------------------------------------------------
WORKER_HOME=$(eval echo ~worker)

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
echo "export CLAUDE_CODE_OAUTH_TOKEN=$CLAUDE_OAUTH_TOKEN" >> "$WORKER_HOME/.bashrc"

# ---------------------------------------------------------------------------
# Start Claude Code in tmux (no prompt — waiting for task assignment)
# ---------------------------------------------------------------------------
su - worker -c "tmux new-session -d -s claude -x 200 -y 50"
su - worker -c "tmux send-keys -t claude \
    'cd /workspace && claude --dangerously-skip-permissions --permission-mode bypassPermissions' Enter"

# Wait for Claude to be ready
for attempt in $(seq 1 30); do
    sleep 3
    PANE=$(su - worker -c "tmux capture-pane -t claude -p" 2>/dev/null || echo "")
    if echo "$PANE" | grep -q 'bypass permissions on'; then
        echo "[cm-warm] Claude ready (attempt $attempt)"
        break
    fi
    su - worker -c "tmux send-keys -t claude Enter"
    echo "[cm-warm] Sent Enter (attempt $attempt)"
done

# ---------------------------------------------------------------------------
# Start ttyd
# ---------------------------------------------------------------------------
ttyd -i 0.0.0.0 -p 8080 --writable su - worker -c "tmux attach -t claude" &

EXTERNAL_IP=$(curl -sf "http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip" \
    -H "Metadata-Flavor: Google")
echo "[cm-warm] ttyd at http://${EXTERNAL_IP}:8080"

echo "[cm-warm] VM is ready and waiting for tasks"

# ---------------------------------------------------------------------------
# Watcher: same as regular worker but also detects task completion
# and resets to "ready" state
# ---------------------------------------------------------------------------
LAST_STATE="ready"
CURRENT_TASK=""

while true; do
    sleep 5

    if ! su - worker -c "tmux has-session -t claude" 2>/dev/null; then
        echo "[cm-warm] Claude session ended unexpectedly"
        break
    fi

    STATUS_LINE=$(su - worker -c "tmux capture-pane -t claude -p" 2>/dev/null | grep -v '^$' | tail -1)

    if echo "$STATUS_LINE" | grep -q 'esc to interrupt'; then
        CURRENT_STATE="busy"
    else
        # Claude is idle — either ready for a task or done with one
        if [ "$LAST_STATE" = "busy" ]; then
            CURRENT_STATE="task_done"
        else
            CURRENT_STATE="ready"
        fi
    fi

    if [ "$CURRENT_STATE" != "$LAST_STATE" ]; then
        echo "[cm-warm] $LAST_STATE -> $CURRENT_STATE"

        if [ "$CURRENT_STATE" = "task_done" ]; then
            # Task finished — notify manager, go back to ready
            # The dispatch daemon will detect this and update the task + warm VM status
            echo "[cm-warm] Task completed, returning to ready state"
            CURRENT_STATE="ready"
        fi

        LAST_STATE="$CURRENT_STATE"
    fi
done

echo "[cm-warm] Watcher exited"
wait
