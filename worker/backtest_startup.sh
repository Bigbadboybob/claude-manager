#!/usr/bin/env bash
# Backtest worker (cloud auto-backtest): no Claude session — a deterministic
# pipeline run in tmux (ttyd kept for live observability). Launched by the
# backtest lane in api/dispatch_daemon.py with per-task metadata; runs on the
# cm-backtest-worker image (postgres 17 + timescaledb + uv + pre-warmed uv
# cache baked in — see the ops runbook in the PR/plan).
#
# Deliberately NOT `set -e`: every failure must reach the fail-report path
# (publish partials, POST an artifact, PATCH blocked) instead of dying silently.
set -uo pipefail

export HOME=/root
export PATH="/root/.local/bin:/usr/local/bin:$PATH"

exec > /var/log/cm-worker.log 2>&1
echo "[cm-backtest] Starting at $(date)"

# ---------------------------------------------------------------------------
# Metadata
# ---------------------------------------------------------------------------
META_URL="http://metadata.google.internal/computeMetadata/v1/instance/attributes"
META_HEADER="Metadata-Flavor: Google"

TASK_ID=$(curl -sf "$META_URL/task-id" -H "$META_HEADER")
REPO_URL=$(curl -sf "$META_URL/repo-url" -H "$META_HEADER")
REPO_BRANCH=$(curl -sf "$META_URL/repo-branch" -H "$META_HEADER")
MANAGER_URL=$(curl -sf "$META_URL/manager-callback-url" -H "$META_HEADER" || echo "")
API_TOKEN=$(curl -sf "$META_URL/api-token" -H "$META_HEADER" || echo "")
RUN_KEY=$(curl -sf "$META_URL/run-key" -H "$META_HEADER")
SECRETS_PROJECT=$(curl -sf "$META_URL/secrets-project" -H "$META_HEADER" || echo "prediction-market-scalper")
RESULTS_BUCKET=$(curl -sf "$META_URL/results-bucket" -H "$META_HEADER" || echo "gs://prediction-market-scalper-datasets")
curl -sf "$META_URL/backtest-payload" -H "$META_HEADER" > /root/backtest-payload.json

GCS_PREFIX="$RESULTS_BUCKET/backtests/$RUN_KEY"
RESULTS_DIR=/workspace/results

echo "[cm-backtest] Task: $TASK_ID"
echo "[cm-backtest] Repo: $REPO_URL (branch: $REPO_BRANCH)"
echo "[cm-backtest] Run key: $RUN_KEY -> $GCS_PREFIX"

# ---------------------------------------------------------------------------
# API helpers
# ---------------------------------------------------------------------------
api_update() {
    if [ -n "$MANAGER_URL" ] && [ -n "$API_TOKEN" ]; then
        curl -sf -X PATCH "$MANAGER_URL/tasks/$TASK_ID" \
            -H "Content-Type: application/json" \
            -H "Authorization: Bearer $API_TOKEN" \
            -d "$1" || echo "[cm-backtest] WARNING: API callback failed"
    fi
}

# POST an artifact row; 5 attempts with backoff. Returns 1 on exhaustion —
# the caller must then PATCH blocked, never done (done implies results are
# retrievable via the API).
post_artifact() {
    for i in 1 2 3 4 5; do
        if curl -sf -X POST "$MANAGER_URL/tasks/$TASK_ID/artifacts" \
            -H "Content-Type: application/json" \
            -H "Authorization: Bearer $API_TOKEN" \
            -d "$1" > /dev/null; then
            echo "[cm-backtest] Artifact posted"
            return 0
        fi
        echo "[cm-backtest] artifact POST failed (attempt $i)"
        sleep $((i * 10))
    done
    return 1
}

# ---------------------------------------------------------------------------
# Parse the submission payload -> /root/cm-bt.env (sourced by the pipeline)
# ---------------------------------------------------------------------------
python3 - <<'PYEOF'
import json, shlex

with open("/root/backtest-payload.json") as f:
    bt = json.load(f)

config = bt.get("config") or ""
if "\n" in config:
    # Inline YAML — materialize into the workspace so repo-relative
    # includes/paths inside it resolve the same as a committed config.
    config_path = "/workspace/cm-run-config.yaml"
    with open("/root/cm-run-config.yaml", "w") as f:
        f.write(config)
else:
    config_path = config  # repo-relative path, resolved after clone

lines = {
    "BT_SCRIPT": bt.get("script") or "analysis.backtests.backtest_actrader_grid",
    "BT_BRANCH": bt.get("branch") or "",
    "BT_LABEL": bt.get("label") or "",
    "BT_BASELINE_REF": bt.get("baseline_ref") or "",
    "BT_REGRESSION": "1" if bt.get("regression") else "",
    "BT_CONFIG": config_path,
    "BT_CONFIG_INLINE": "1" if "\n" in config else "",
}
with open("/root/cm-bt.env", "w") as f:
    for k, v in lines.items():
        f.write(f"export {k}={shlex.quote(v)}\n")
print(f"[cm-backtest] payload parsed: script={lines['BT_SCRIPT']} config={config_path}")
PYEOF
# shellcheck disable=SC1091
source /root/cm-bt.env

# ---------------------------------------------------------------------------
# Publish + artifact builders (shared by happy path / failure / preempt)
# ---------------------------------------------------------------------------
publish_results() {
    [ -d "$RESULTS_DIR" ] || { echo "[cm-backtest] No results dir to publish"; return 1; }
    if gsutil -m rsync -r "$RESULTS_DIR" "$GCS_PREFIX/"; then
        echo "[cm-backtest] Published $GCS_PREFIX"
    else
        echo "[cm-backtest] WARNING: publish failed"
        return 1
    fi
}

# $1 = partial ("true"/"false"). Wraps results/summary.json into the artifact
# body: {kind, partial, gcs_prefix, summary:{...summary, partial, gcs_pointer,
# run_key}}. Missing summary.json -> {"error":"no-summary", ...}.
build_artifact_json() {
    PARTIAL="$1" RESULTS_DIR="$RESULTS_DIR" GCS_PREFIX="$GCS_PREFIX" RUN_KEY="$RUN_KEY" \
    python3 - <<'PYEOF'
import json, os

partial = os.environ["PARTIAL"] == "true"
gcs = os.environ["GCS_PREFIX"]
summary_path = os.path.join(os.environ["RESULTS_DIR"], "summary.json")
try:
    with open(summary_path) as f:
        summary = json.load(f)
    if not isinstance(summary, dict):
        summary = {"error": "summary-not-a-dict", "raw_type": type(summary).__name__}
except (OSError, ValueError) as e:
    summary = {"error": "no-summary", "detail": str(e)}

summary["partial"] = partial
summary["gcs_pointer"] = gcs
summary["run_key"] = os.environ["RUN_KEY"]
print(json.dumps({
    "kind": "backtest-result",
    "partial": partial,
    "gcs_prefix": gcs,
    "summary": summary,
}))
PYEOF
}

# ---------------------------------------------------------------------------
# Preemption handler (GCP sends SIGTERM ~30s before stopping spot instances)
# ---------------------------------------------------------------------------
# Land whatever partial results exist, then requeue. worker_vm is left set on
# the task — the dispatcher deletes this (STOPped) instance at relaunch.
# The metadata PATCH replaces the whole JSONB object, so resume hints are
# merged into a freshly-GET'd copy rather than sent as a fragment.
on_preempt() {
    echo "[cm-backtest] PREEMPTION DETECTED"
    publish_results || true
    post_artifact "$(build_artifact_json true)" || true

    MERGED_BODY=$(curl -sf "$MANAGER_URL/tasks/$TASK_ID" \
        -H "Authorization: Bearer $API_TOKEN" | python3 -c "
import json, sys
from datetime import datetime, timezone
task = json.load(sys.stdin)
meta = task.get('metadata') or {}
bt = meta.setdefault('backtest', {})
resume = bt.setdefault('resume', {})
resume['attempt'] = int(resume.get('attempt') or 0) + 1
resume['preempted_at'] = datetime.now(timezone.utc).isoformat()
print(json.dumps({'status': 'backlog', 'metadata': meta}))
" 2>/dev/null || echo '{"status": "backlog"}')
    api_update "$MERGED_BODY"
    echo "[cm-backtest] Task re-queued to backlog"
    exit 0
}

trap 'on_preempt' SIGTERM

# ---------------------------------------------------------------------------
# Secrets (project comes from instance metadata, NOT hardcoded)
# ---------------------------------------------------------------------------
# Git auth is a dedicated READ-ONLY deploy key ("cm-backtest-worker" on the
# predictionTrading repo), not the github-pat — the PAT expires; the key
# doesn't. Private half lives in Secret Manager as backtest-git-deploy-key.
mkdir -p /root/.ssh && chmod 700 /root/.ssh
gcloud secrets versions access latest \
    --secret=backtest-git-deploy-key --project="$SECRETS_PROJECT" \
    > /root/.ssh/bt_deploy 2>/dev/null || true
chmod 600 /root/.ssh/bt_deploy 2>/dev/null || true
export GIT_SSH_COMMAND="ssh -i /root/.ssh/bt_deploy -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new"
REPLICA_DSN=$(gcloud secrets versions access latest \
    --secret=replica-connection-string --project="$SECRETS_PROJECT" || echo "")
echo "[cm-backtest] Credentials loaded (deploy-key: $([ -s /root/.ssh/bt_deploy ] && echo yes || echo NO), replica: $([ -n "$REPLICA_DSN" ] && echo yes || echo NO))"

# ---------------------------------------------------------------------------
# Local Postgres (baked into the cm-backtest-worker image)
# ---------------------------------------------------------------------------
systemctl start postgresql 2>/dev/null || pg_ctlcluster 17 main start 2>/dev/null || true
for i in $(seq 1 30); do
    pg_isready -q 2>/dev/null && break
    sleep 2
done
pg_isready -q && echo "[cm-backtest] Postgres up" || echo "[cm-backtest] WARNING: Postgres not ready"

# Matches the role/db baked into the image (ops runbook step 6).
LOCAL_DSN="postgresql://predictionuser:predictionpass@localhost:5432/predictiondb"

# ---------------------------------------------------------------------------
# Clone the repo at the requested branch
# ---------------------------------------------------------------------------
BRANCH="${BT_BRANCH:-$REPO_BRANCH}"
# Deploy keys authenticate over SSH — normalize an https origin to the
# git@github.com form (a git@ origin passes through unchanged).
SSH_URL=$(echo "$REPO_URL" | sed -E "s|https://github.com/([^/]+)/([^/]+?)(\.git)?$|git@github.com:\1/\2.git|")
if ! git clone -b "$BRANCH" "$SSH_URL" /workspace; then
    echo "[cm-backtest] FATAL: clone failed ($REPO_URL @ $BRANCH)"
    post_artifact "$(python3 -c "
import json
print(json.dumps({'kind': 'backtest-result', 'partial': True, 'gcs_prefix': None,
                  'summary': {'error': 'clone-failed', 'branch': '$BRANCH',
                              'run_key': '$RUN_KEY', 'partial': True}}))
")" || true
    api_update '{"status": "blocked"}'
    exit 1
fi
cd /workspace
# Route any in-repo https github remotes through the deploy key too.
git config url."git@github.com:".insteadOf "https://github.com/"

# Inline config was staged in /root before /workspace existed — move it in.
[ "${BT_CONFIG_INLINE:-}" = "1" ] && cp /root/cm-run-config.yaml /workspace/cm-run-config.yaml

# The PT clients read DSNs from .env (worktrees source it via git-common-dir;
# here the clone IS the root). POSTGRES_CONNECTION_STRING points at the
# REPLICA — the download path treats it as the source; PT-side handles the
# standby's read-only-ness (pg_is_in_recovery() auto-detect).
cat > /workspace/.env <<ENVEOF
POSTGRES_CONNECTION_STRING=$REPLICA_DSN
LOCAL_POSTGRES_CONNECTION_STRING=$LOCAL_DSN
ENVEOF
echo "[cm-backtest] Repo ready"

# ---------------------------------------------------------------------------
# Pipeline (runs inside tmux; exit code lands in /var/run/cm-pipeline-exit so
# this script stays a trap-responsive poll loop)
# ---------------------------------------------------------------------------
cat > /root/cm_pipeline.sh <<'EOF'
#!/usr/bin/env bash
set -uo pipefail
source /root/cm-bt.env
cd /workspace
RESULTS_DIR=/workspace/results

if [ "$BT_SCRIPT" = "__cm_stub__" ]; then
    # CM-side e2e stub: exercises dispatch -> VM -> publish -> artifact ->
    # done with zero PT dependencies (PT's run_submission.py is being built
    # in parallel). Sleep long enough to observe 'running' + ttyd.
    echo "[cm-pipeline] STUB run"
    mkdir -p "$RESULTS_DIR"
    python3 - <<'PY'
import json
summary = {
    "total_pnl": 12.34, "realized_pnl": 10.0, "fill_count": 42,
    "taker_pct": 0.5, "grid_rows": [{"param": "stub", "pnl": 12.34}],
}
with open("/workspace/results/summary.json", "w") as f:
    json.dump(summary, f)
PY
    echo "stub,row" > "$RESULTS_DIR/fills.csv"
    sleep 15
    exit 0
fi

echo "[cm-pipeline] uv sync"
uv sync || exit 10

echo "[cm-pipeline] downloading window from replica"
uv run python -m analysis.backtests.scripts.download_events \
    --from-config "$BT_CONFIG" || exit 11

echo "[cm-pipeline] running submission: $BT_SCRIPT"
# CLI contract owned by PT's AUTO_BACKTEST_PLAN.md Phase 2 (run_submission.py)
# — adjust this one invocation if the resolver's flags change.
uv run python -m analysis.backtests.scripts.run_submission \
    --script "$BT_SCRIPT" \
    --config "$BT_CONFIG" \
    --out "$RESULTS_DIR" \
    ${BT_REGRESSION:+--regression} \
    ${BT_BASELINE_REF:+--baseline-ref "$BT_BASELINE_REF"} || exit 12

exit 0
EOF
chmod +x /root/cm_pipeline.sh

rm -f /var/run/cm-pipeline-exit
tmux new-session -d -s backtest -x 200 -y 50 \
    "bash /root/cm_pipeline.sh; echo \$? > /var/run/cm-pipeline-exit; sleep 3600"

# ---------------------------------------------------------------------------
# ttyd (observability)
# ---------------------------------------------------------------------------
ttyd -i 0.0.0.0 -p 8080 --writable tmux attach -t backtest &

EXTERNAL_IP=$(curl -sf "http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip" \
    -H "Metadata-Flavor: Google" || echo "")
if [ -n "$EXTERNAL_IP" ]; then
    api_update "{\"ttyd_url\": \"http://${EXTERNAL_IP}:8080\"}"
fi
echo "running" > /var/log/cm-worker-state

# ---------------------------------------------------------------------------
# Completion watcher (SIGTERM-interruptible poll)
# ---------------------------------------------------------------------------
while [ ! -f /var/run/cm-pipeline-exit ]; do
    sleep 5
done
EXIT_CODE=$(cat /var/run/cm-pipeline-exit)
echo "[cm-backtest] Pipeline finished (exit $EXIT_CODE)"

if [ "$EXIT_CODE" = "0" ]; then
    publish_results
    if post_artifact "$(build_artifact_json false)"; then
        api_update '{"status": "done"}'   # PATCH handler deletes this VM
    else
        echo "[cm-backtest] Artifact POST exhausted retries — blocking for operator"
        api_update '{"status": "blocked"}'   # VM kept for debug; reaper sweeps later
    fi
else
    echo "[cm-backtest] Pipeline FAILED (exit $EXIT_CODE)"
    publish_results || true
    post_artifact "$(build_artifact_json true)" || true
    api_update '{"status": "blocked"}'
fi

echo "done" > /var/log/cm-worker-state
echo "[cm-backtest] Worker script complete"
