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
# Live pipeline-phase marker. The PT pipeline (phase_report.py + the coarse stage markers
# below) writes it; the completion-watcher poll loop relays it to the CM phase endpoint so the
# /backtests panel can show "Setup - download 40%" during the pre-replay download instead of a
# blank Progress/ETA. Exported so the tmux pipeline + its python see the same path.
export CM_BT_PHASE_FILE=/var/run/cm-bt-phase.json

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

# Relay a phase marker (the JSON body written by phase_report.py / phase_marker below) to the
# dedicated merge endpoint. Merges into metadata.backtest server-side (no clobber) and stamps
# phase_updated_at. Best-effort — a failed heartbeat must never affect the run.
api_phase() {
    if [ -n "$MANAGER_URL" ] && [ -n "$API_TOKEN" ] && [ -n "${1//[[:space:]]/}" ]; then
        curl -sf -X POST "$MANAGER_URL/tasks/$TASK_ID/backtest-phase" \
            -H "Content-Type: application/json" \
            -H "Authorization: Bearer $API_TOKEN" \
            -d "$1" >/dev/null 2>&1 || echo "[cm-backtest] WARNING: phase callback failed"
    fi
}

# Build a coarse phase marker JSON on stdout (stdlib-only python3, so it works before uv sync).
# Used for stage boundaries the PT python doesn't emit itself (uv_sync, finalize); the finer
# download/replay markers come from phase_report.py inside the pipeline.
phase_marker() {   # $1=phase  $2=step  $3=detail
    CM_PHASE="$1" CM_STEP="$2" CM_DETAIL="$3" python3 - <<'PY' 2>/dev/null || true
import datetime, json, os
now = datetime.datetime.now(datetime.timezone.utc).isoformat()
print(json.dumps({
    "phase": os.environ.get("CM_PHASE"),
    "phase_step": os.environ.get("CM_STEP") or None,
    "phase_progress": None,
    "phase_started_at": now,
    "phase_detail": os.environ.get("CM_DETAIL") or None,
    "emitted_at": now,
}))
PY
}

# Backoff schedule between artifact POST attempts: 10 attempts (9 sleeps)
# spanning ~11 minutes, so a planning-API restart / transient 5xx window is
# ridden out instead of stranding a finished run at `blocked`.
ARTIFACT_POST_BACKOFF="5 10 20 40 60 90 120 150 180"

# POST an artifact row. Returns 1 on exhaustion — the caller must then PATCH
# blocked, never done (done implies results are retrievable via the API).
#
# Three hard-won properties, all from the 2026-07/08 "blocked with ZERO
# artifacts" incidents (5 runs, 25 POSTs, every one a 422):
#   1. An EMPTY body is refused outright. curl -sf -d "" POSTs a zero-length
#      body, which FastAPI rejects as 422 {"type":"missing","loc":["body"]} —
#      indistinguishable, under -f, from a server fault. A body-builder that
#      silently produced nothing is a worker bug; say so loudly.
#   2. The exact body is parked in GCS as backtest_artifact.json BEFORE the
#      first attempt, so recovery never has to rebuild it
#      (scripts/repost_backtest_artifact.py).
#   3. -f is NOT used: the HTTP status and response body of every failed
#      attempt are logged. -f collapses "422 your body is malformed" and
#      "503 API is down" into the same silent exit 22.
post_artifact() {
    local body="$1"
    if [ -z "${body//[[:space:]]/}" ]; then
        echo "[cm-backtest] FATAL: artifact body is EMPTY — refusing to POST"
        echo "[cm-backtest]        (an empty POST body is a 422 at the API; the bug is in the body builder, not the API)"
        return 1
    fi

    printf '%s' "$body" > /tmp/cm-backtest-artifact.json
    if [ -n "${GCS_PREFIX:-}" ]; then
        if gsutil cp /tmp/cm-backtest-artifact.json "$GCS_PREFIX/backtest_artifact.json" >&2; then
            echo "[cm-backtest] Artifact body parked at $GCS_PREFIX/backtest_artifact.json (${#body} bytes)"
        else
            echo "[cm-backtest] WARNING: could not park artifact body in GCS"
        fi
    fi

    local attempt=0 code sleep_s
    for sleep_s in $ARTIFACT_POST_BACKOFF ""; do
        attempt=$((attempt + 1))
        code=$(curl -sS -o /tmp/cm-artifact-resp.txt -w '%{http_code}' \
            -X POST "$MANAGER_URL/tasks/$TASK_ID/artifacts" \
            -H "Content-Type: application/json" \
            -H "Authorization: Bearer $API_TOKEN" \
            -d "$body" 2>/tmp/cm-artifact-curl.err)
        case "$code" in
            2*)
                echo "[cm-backtest] Artifact posted (HTTP $code, attempt $attempt)"
                return 0
                ;;
        esac
        echo "[cm-backtest] artifact POST failed (attempt $attempt/10): HTTP ${code:-000} bytes=${#body}"
        echo "[cm-backtest]   response: $(head -c 1000 /tmp/cm-artifact-resp.txt 2>/dev/null | tr '\n' ' ')"
        echo "[cm-backtest]   curl:     $(head -c 400 /tmp/cm-artifact-curl.err 2>/dev/null | tr '\n' ' ')"
        [ -n "$sleep_s" ] && sleep "$sleep_s"
    done

    echo "[cm-backtest] !!! ARTIFACT POST EXHAUSTED after $attempt attempts — RECOVER WITH: python3 scripts/repost_backtest_artifact.py $TASK_ID --gcs-prefix $GCS_PREFIX   (body parked at $GCS_PREFIX/backtest_artifact.json)"
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
    # So the tmux pipeline + its python (phase_report.py) write phase markers to the same
    # path the startup script relays. Sourced explicitly here in case the tmux server's env
    # predates the outer export.
    "CM_BT_PHASE_FILE": os.environ.get("CM_BT_PHASE_FILE", ""),
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

# Publish + build the artifact POST body in one step; echoes the body on
# stdout (all logging goes to stderr — the whole script's stderr lands in
# /var/log/cm-worker.log anyway).
#
# Real runs delegate to PT's publisher, which uploads the results dir AND
# emits the contract-shaped summary ({total_pnl, realized_pnl, fill_count,
# taker_pct, partial, grid_rows[], gcs_pointer}; {error:"no-summary",
# partial:true} if the run died before writing) — one reshaping
# implementation, owned repo-side. The stub path, and any run where the
# venv doesn't exist yet (preempt before/mid `uv sync` — `uv run` would
# auto-sync for minutes inside a ~30s trap), fall back to the bash
# rsync + summary.json wrap below.
emit_artifact() {   # $1 = partial ("true"/"false")
    local partial="$1"
    if [ "$BT_SCRIPT" != "__cm_stub__" ] && [ -d /workspace/.venv ]; then
        local flag=""
        [ "$partial" = "true" ] && flag="--partial"
        local out body rc
        if out=$(cd /workspace && timeout 240 uv run python -m \
                analysis.backtests.scripts.publish_results \
                --output-dir "$RESULTS_DIR" --gcs-prefix "$GCS_PREFIX" \
                --upload $flag 2>/tmp/cm-publish.err); then
            # The publisher's stdout is handed on through a FILE, never a pipe.
            # `python3 - <<'PYEOF'` takes its *script* from stdin, so the heredoc
            # WINS the stdin redirection and a piped payload is unreachable:
            # json.load(sys.stdin) sees EOF, the wrap dies, and the old code's
            # unconditional `return 0` reported success with an EMPTY body. That
            # is the artifact-POST bug — every run whose PT publisher succeeded
            # POSTed nothing and got 422'd 5/5 (runs cafef203, 61c1ffdf, 2c8ad55e,
            # 4074659d, 218091b7). Reuse build_artifact_json so there is exactly
            # ONE NaN-safe wrap implementation.
            printf '%s' "$out" > /tmp/cm-publisher-summary.json
            body=$(build_artifact_json "$partial" /tmp/cm-publisher-summary.json)
            rc=$?
            if [ "$rc" -eq 0 ] && [ -n "$body" ]; then
                echo "[cm-backtest] PT publisher succeeded (${#body} byte artifact body)" >&2
                printf '%s' "$body"
                return 0
            fi
            echo "[cm-backtest] PT publisher summary unusable (wrap rc=$rc, ${#body} bytes); falling back to bash publish" >&2
        else
            echo "[cm-backtest] PT publisher failed; falling back to bash publish" >&2
        fi
        tail -3 /tmp/cm-publish.err >&2 2>/dev/null || true
    fi
    publish_results >&2 || true
    build_artifact_json "$partial"
}

# $1 = partial ("true"/"false"), $2 = OPTIONAL explicit summary json path.
# Wraps the run's summary into the artifact body: {kind, partial, gcs_prefix,
# summary:{...summary, partial, gcs_pointer, run_key}}. With no $2 it prefers
# the PT publisher's compact backtest_summary.json (the designed contract
# shape) and falls back to the grid runner's summary.json; with $2 it uses
# exactly that file and FAILS (exit 1, empty stdout) if it is unusable, so
# emit_artifact can fall through to the bash publish path instead of shipping
# a no-summary stub over a perfectly good results dir.
# Non-finite floats (NaN/Infinity — common when a run has 0 trades) are coerced
# to null: they are INVALID JSON and Postgres JSONB rejects them, which 500s the
# artifact POST and leaves a successful run stuck "blocked".
build_artifact_json() {
    PARTIAL="$1" SUMMARY_FILE="${2:-}" RESULTS_DIR="$RESULTS_DIR" GCS_PREFIX="$GCS_PREFIX" RUN_KEY="$RUN_KEY" \
    python3 - <<'PYEOF'
import json, math, os, sys

partial = os.environ["PARTIAL"] == "true"
gcs = os.environ["GCS_PREFIX"]
results_dir = os.environ["RESULTS_DIR"]
explicit = os.environ.get("SUMMARY_FILE") or ""

if explicit:
    candidates = [explicit]
else:
    candidates = [os.path.join(results_dir, n)
                  for n in ("backtest_summary.json", "summary.json")]

summary = None
for path in candidates:
    try:
        with open(path) as f:
            loaded = json.load(f)
    except (OSError, ValueError):
        continue
    if isinstance(loaded, dict):
        summary = loaded
        break
if summary is None:
    if explicit:
        # Caller named a specific summary and it is missing / unparseable / not
        # an object. Fail loudly and empty-handed; the caller has a fallback.
        sys.exit("[cm-backtest] explicit summary file unusable: %s" % explicit)
    summary = {"error": "no-summary", "detail": "no readable summary json in results dir"}


def _finite(o):
    """Coerce NaN/Infinity -> None so the body is valid JSON (JSONB-storable)."""
    if isinstance(o, float):
        return o if math.isfinite(o) else None
    if isinstance(o, dict):
        return {k: _finite(v) for k, v in o.items()}
    if isinstance(o, list):
        return [_finite(v) for v in o]
    return o


summary = _finite(summary)
summary["partial"] = partial
summary["gcs_pointer"] = gcs
summary["run_key"] = os.environ["RUN_KEY"]

# Failure evidence (exported by the pipeline-failed branch): the exit code
# and the pipeline log's tail ride the artifact, so the reason a run died
# is readable from get_backtest_result — not only from a VM that the
# reaper will have deleted by the time anyone looks.
fail_exit = os.environ.get("CM_FAIL_EXIT_CODE") or ""
if fail_exit:
    summary.setdefault("error", "pipeline-failed")
    summary["exit_code"] = int(fail_exit) if fail_exit.isdigit() else fail_exit
    log_path = os.environ.get("CM_PIPELINE_LOG") or "/var/log/cm-pipeline.log"
    try:
        with open(log_path, "rb") as f:
            f.seek(0, 2)
            f.seek(max(0, f.tell() - 4000))
            summary["log_tail"] = f.read().decode("utf-8", "replace")
    except OSError:
        pass
print(json.dumps({
    "kind": "backtest-result",
    "partial": partial,
    "gcs_prefix": gcs,
    "summary": summary,
}, allow_nan=False))
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
    post_artifact "$(emit_artifact true)" || true

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
# Trading-environment telemetry (PT observability design Lane 3, §5 "cloud spot workers"): an
# INSERT-only `telemetry_writer` role on the PROD db, reachable from this VPC on db-east4's internal
# IP. Deliberately a SECOND, narrower credential than the replica DSN above — a spot worker may
# append env_* telemetry and nothing else. Absent/failed fetch is NOT fatal: the run then emits into
# the counting sink and its manifest records `dsn_source: none`, which is the honest outcome.
TELEMETRY_DSN=$(gcloud secrets versions access latest \
    --secret=telemetry-writer-dsn --project="$SECRETS_PROJECT" 2>/dev/null || echo "")
# ⚠ PRESENCE ONLY, never the value. This log stream is /var/log/cm-worker.log AND the tmux session
# ttyd serves on a public IP, so a DSN echoed anywhere here is a DSN published.
echo "[cm-backtest] Credentials loaded (deploy-key: $([ -s /root/.ssh/bt_deploy ] && echo yes || echo NO), replica: $([ -n "$REPLICA_DSN" ] && echo yes || echo NO), telemetry-writer: $([ -n "$TELEMETRY_DSN" ] && echo yes || echo NO))"

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
# Normalize an https github URL to the git@ SSH form (deploy-key auth); a git@
# origin passes through unchanged. Swap ONLY the host prefix: GNU sed has no lazy
# quantifier, so the old "([^/]+?)(\.git)?$" pattern doubled the suffix into
# repo.git.git and clone-failed on any .git-suffixed https URL.
SSH_URL=$(echo "$REPO_URL" | sed -E "s|^https://github.com/|git@github.com:|")
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
#
# ENV_TELEMETRY_CONNECTION_STRING is the env-telemetry writer DSN, consumed by
# EnvTelemetryConfig.resolved_dsn() (config > this var > counting sink). It goes HERE, in the file
# the runner already loads via load_dotenv(override=False), rather than into the submitted runner
# config: the config is hashed into manifest.json, re-dumped as resolved_config.yaml and rsync'd to
# GCS, so a credential placed there would be published with the run. It also never rides argv —
# run_submission.py echoes its full exec line into the public ttyd stream.
# Empty when the secret fetch failed; PT reads blank as unset and falls back to the counting sink.
cat > /workspace/.env <<ENVEOF
POSTGRES_CONNECTION_STRING=$REPLICA_DSN
LOCAL_POSTGRES_CONNECTION_STRING=$LOCAL_DSN
ENV_TELEMETRY_CONNECTION_STRING=$TELEMETRY_DSN
ENVEOF
chmod 600 /workspace/.env
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

# SETUP phase begins (portal /backtests Progress). download_events.py + run_submission.py emit
# the finer download/replay markers via phase_report.py; this coarse one covers `uv sync`, which
# runs before the repo's python is usable. Atomic write (tmp + mv), best-effort.
if [ -n "${CM_BT_PHASE_FILE:-}" ]; then
    _NOW=$(date -u +%Y-%m-%dT%H:%M:%S+00:00)
    printf '{"phase":"setup","phase_step":"uv_sync","phase_progress":null,"phase_started_at":"%s","phase_detail":"installing dependencies","emitted_at":"%s"}' "$_NOW" "$_NOW" \
        > "$CM_BT_PHASE_FILE.tmp" 2>/dev/null && mv "$CM_BT_PHASE_FILE.tmp" "$CM_BT_PHASE_FILE" 2>/dev/null || true
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
# Pipeline output ALSO lands in /var/log/cm-pipeline.log (tee passes it
# through to the tmux pane, so the ttyd live view is unchanged): a failure's
# last lines must survive the VM — pre-fix, recovering the failure reason
# took SSH + tmux capture-pane before the reaper deleted the instance.
# PIPESTATUS[0] (bash-only, hence the explicit bash -c) keeps the recorded
# exit code the PIPELINE's, not tee's.
tmux new-session -d -s backtest -x 200 -y 50 \
    "bash -c 'bash /root/cm_pipeline.sh 2>&1 | tee /var/log/cm-pipeline.log; echo \${PIPESTATUS[0]} > /var/run/cm-pipeline-exit; sleep 3600'"

# ---------------------------------------------------------------------------
# ttyd (observability). NOT publicly reachable: :8080 ingress is scoped by the
# cm-backtest-ttyd firewall rule (operator + cm-manager IPs only, ensured by
# the dispatcher — dispatch/vm.py::ensure_ttyd_firewall; the old allow-ttyd
# rule exposed this ROOT terminal to 0.0.0.0/0 and scanners probed it within
# hours of boot). The ttyd_url on the task keeps working from allowlisted
# networks; from anywhere else, tunnel instead:
#   gcloud compute ssh <vm> --project=<project> --zone=<zone> -- -L 8080:localhost:8080
# then open http://localhost:8080 (exact command logged below).
# ---------------------------------------------------------------------------
ttyd -i 0.0.0.0 -p 8080 --writable tmux attach -t backtest &

EXTERNAL_IP=$(curl -sf "http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip" \
    -H "Metadata-Flavor: Google" || echo "")
VM_NAME=$(curl -sf "http://metadata.google.internal/computeMetadata/v1/instance/name" -H "$META_HEADER" || echo "")
VM_ZONE=$(curl -sf "http://metadata.google.internal/computeMetadata/v1/instance/zone" -H "$META_HEADER" | awk -F/ '{print $NF}' || echo "")
VM_PROJECT=$(curl -sf "http://metadata.google.internal/computeMetadata/v1/project/project-id" -H "$META_HEADER" || echo "")
if [ -n "$EXTERNAL_IP" ]; then
    api_update "{\"ttyd_url\": \"http://${EXTERNAL_IP}:8080\"}"
fi
echo "[cm-backtest] ttyd: http://${EXTERNAL_IP}:8080 (allowlisted IPs only; from elsewhere: gcloud compute ssh $VM_NAME --project=$VM_PROJECT --zone=$VM_ZONE -- -L 8080:localhost:8080 then open http://localhost:8080)"
echo "running" > /var/log/cm-worker-state

# ---------------------------------------------------------------------------
# Completion watcher (SIGTERM-interruptible poll) — also relays the live phase
# marker the pipeline writes ($CM_BT_PHASE_FILE) to the CM phase endpoint whenever
# it changes, so the /backtests panel tracks SETUP -> REPLAY in flight. Relaying
# only on change keeps a truly stalled download visible: no new marker -> no PATCH
# -> phase_updated_at freezes -> the panel's "stalled?" hint fires.
# ---------------------------------------------------------------------------
LAST_PHASE_MARKER=""
while [ ! -f /var/run/cm-pipeline-exit ]; do
    if [ -f "$CM_BT_PHASE_FILE" ]; then
        CUR_PHASE_MARKER=$(cat "$CM_BT_PHASE_FILE" 2>/dev/null || echo "")
        if [ -n "$CUR_PHASE_MARKER" ] && [ "$CUR_PHASE_MARKER" != "$LAST_PHASE_MARKER" ]; then
            api_phase "$CUR_PHASE_MARKER" && LAST_PHASE_MARKER="$CUR_PHASE_MARKER"
        fi
    fi
    sleep 5
done
EXIT_CODE=$(cat /var/run/cm-pipeline-exit)
echo "[cm-backtest] Pipeline finished (exit $EXIT_CODE)"

# FINALIZE phase: publishing results + POSTing the artifact happens below (outside the
# tmux pipeline, so phase_report.py can't emit it). The run goes terminal moments later.
api_phase "$(phase_marker finalize '' 'publishing results')"

if [ "$EXIT_CODE" = "0" ]; then
    if post_artifact "$(emit_artifact false)"; then
        api_update '{"status": "done"}'   # PATCH handler deletes this VM
    else
        echo "[cm-backtest] Artifact POST exhausted retries — blocking for operator"
        api_update '{"status": "blocked"}'   # VM kept for debug; reaper sweeps later
    fi
else
    echo "[cm-backtest] Pipeline FAILED (exit $EXIT_CODE)"
    # Park the full pipeline log next to the results, then let the artifact
    # carry the exit code + log tail (build_artifact_json merges them off
    # CM_FAIL_EXIT_CODE). The VM itself is expendable after this: the
    # dispatcher's blocked-VM TTL deletes it ~30 min after the PATCH.
    if gsutil cp /var/log/cm-pipeline.log "$GCS_PREFIX/pipeline.log"; then
        echo "[cm-backtest] Pipeline log parked at $GCS_PREFIX/pipeline.log"
    else
        echo "[cm-backtest] WARNING: could not park pipeline log in GCS"
    fi
    export CM_FAIL_EXIT_CODE="$EXIT_CODE"
    post_artifact "$(emit_artifact true)" || true
    api_update '{"status": "blocked"}'
fi

echo "done" > /var/log/cm-worker-state
echo "[cm-backtest] Worker script complete"
