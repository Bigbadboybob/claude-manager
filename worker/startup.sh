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

echo "[cm-worker] Task: $TASK_ID"
echo "[cm-worker] Repo: $REPO_URL (branch: $REPO_BRANCH)"

# ---------------------------------------------------------------------------
# Credentials (base image has Claude Code pre-authed, just need git token)
# ---------------------------------------------------------------------------
GCP_PROJECT="prediction-market-scalper"

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
# Start Claude Code in tmux
# ---------------------------------------------------------------------------
su - worker -c "tmux new-session -d -s claude -x 200 -y 50"
su - worker -c "tmux send-keys -t claude \
    'cd /workspace && claude --dangerously-skip-permissions --permission-mode bypassPermissions' Enter"

# Wait for Claude to be ready (base image has first-run done, just trust dialog)
for attempt in $(seq 1 30); do
    sleep 3
    PANE=$(su - worker -c "tmux capture-pane -t claude -p" 2>/dev/null || echo "")

    if echo "$PANE" | grep -q 'bypass permissions on'; then
        echo "[cm-worker] Claude ready (attempt $attempt)"
        break
    fi

    # Dismiss any dialog (trust dialog, etc.)
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
echo "[cm-worker] ttyd at http://${EXTERNAL_IP}:8080"

# Write state file (polled by cm CLI via SSH)
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
        LAST_STATE="$CURRENT_STATE"
    fi
done

echo "[cm-worker] Watcher exited"
wait
