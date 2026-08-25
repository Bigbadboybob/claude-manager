import os
from pathlib import Path

# Load .env from ~/.config/claude-manager/.env
_env_file = Path.home() / ".config" / "claude-manager" / ".env"
if _env_file.exists():
    for line in _env_file.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            key, _, value = line.partition("=")
            os.environ.setdefault(key.strip(), value.strip())

GCP_PROJECT = os.getenv("CM_GCP_PROJECT", "claude-manager-prod")
GCP_ZONE = os.getenv("CM_GCP_ZONE", "us-east4-a")
VM_MACHINE_TYPE = os.getenv("CM_VM_MACHINE_TYPE", "e2-medium")
VM_IMAGE_FAMILY = "cm-worker-base"
VM_IMAGE_PROJECT = GCP_PROJECT

# Database
DB_DSN = os.getenv("CM_DB_DSN")

# API
MANAGER_URL = os.getenv("CM_API_URL", "http://localhost:8000")
API_TOKEN = os.getenv("CM_API_TOKEN")
MAX_WORKERS = int(os.getenv("CM_MAX_WORKERS", "3"))

# --- Backtest lane (cloud auto-backtest) -----------------------------------
# Backtests run in the SAME project/zone as the prediction-trading data
# (db-replica-east4 + the results bucket) so window downloads are intra-zone
# over internal IP. A wrong-project/zone worker silently loses that advantage,
# so the defaults are pinned here and per-task metadata.vm may override them.
MAX_BACKTEST_WORKERS = int(os.getenv("CM_MAX_BACKTEST_WORKERS", "3"))
BACKTEST_GCP_PROJECT = os.getenv("CM_BACKTEST_GCP_PROJECT", "prediction-market-scalper")
BACKTEST_GCP_ZONE = os.getenv("CM_BACKTEST_GCP_ZONE", "us-east4-a")
BACKTEST_MACHINE_TYPE = os.getenv("CM_BACKTEST_MACHINE_TYPE", "n2-standard-4")  # 2026-08-24: n2 18 % faster than c2 on identical content; 200-vCPU quota
BACKTEST_IMAGE_FAMILY = os.getenv("CM_BACKTEST_IMAGE_FAMILY", "cm-backtest-worker")
BACKTEST_IMAGE_PROJECT = os.getenv("CM_BACKTEST_IMAGE_PROJECT", BACKTEST_GCP_PROJECT)
BACKTEST_SECRETS_PROJECT = os.getenv("CM_BACKTEST_SECRETS_PROJECT", BACKTEST_GCP_PROJECT)
BACKTEST_RESULTS_BUCKET = os.getenv(
    "CM_BACKTEST_RESULTS_BUCKET", "gs://prediction-market-scalper-datasets"
)
BACKTEST_MAX_RUNTIME_SECS = int(os.getenv("CM_BACKTEST_MAX_RUNTIME_SECS", "14400"))  # 4h

# How long a FAILED backtest keeps its VM after the blocked transition
# (ttyd/ssh debugging window). Anchored on blocked_at, NOT launch time:
# max_runtime_secs must stay long for long benches, but a run that failed
# two minutes in must not idle a c2-standard-4 for the rest of that limit
# (observed 2026-08-19: 9 idle workers hand-deleted). The failure evidence
# now rides the artifact (exit_code + log_tail) and GCS (pipeline.log), so
# the live-VM window can be short.
BACKTEST_BLOCKED_VM_TTL_SECS = int(os.getenv("CM_BACKTEST_BLOCKED_VM_TTL_SECS", "1800"))  # 30m

# Board hygiene: terminal backtest rows auto-archive once their result
# artifact is persisted (archived rows are hidden from default listings but
# stay fully readable by id — get_backtest_result / GCS keep working).
# Failures get a longer grace than successes: they carry an operator signal.
# <= 0 disables the respective sweep.
BACKTEST_ARCHIVE_DONE_SECS = int(os.getenv("CM_BACKTEST_ARCHIVE_DONE_SECS", "2700"))  # 45m
BACKTEST_ARCHIVE_BLOCKED_SECS = int(os.getenv("CM_BACKTEST_ARCHIVE_BLOCKED_SECS", "21600"))  # 6h

BACKTEST_VM_DEFAULTS = {
    "project": BACKTEST_GCP_PROJECT,
    "zone": BACKTEST_GCP_ZONE,
    "machine_type": BACKTEST_MACHINE_TYPE,
    "image_family": BACKTEST_IMAGE_FAMILY,
    "image_project": BACKTEST_IMAGE_PROJECT,
    "max_runtime_secs": BACKTEST_MAX_RUNTIME_SECS,
    # pd-standard, not pd-balanced: the PMS region's SSD_TOTAL_GB quota is
    # nearly exhausted by the trader/db disks (1970/2000 as of 2026-07-07),
    # while DISKS_TOTAL_GB has ~39TB free. pd-standard throughput scales
    # with size, hence 200GB (~24MB/s sustained — the one-shot ~2GB window
    # load takes ~90s and then lives in the 16GB page cache).
    "disk_type": os.getenv("CM_BACKTEST_DISK_TYPE", "pd-standard"),
    "disk_size_gb": int(os.getenv("CM_BACKTEST_DISK_SIZE_GB", "200")),
}

# --- Backtest worker ttyd exposure -----------------------------------------
# ttyd on a backtest worker is a ROOT web terminal (tmux attach) on a VM that
# carries a predictionTrading clone and the replica read DSN. It must never be
# publicly reachable: internet scanners were observed probing a worker's :8080
# (/.env.backup, /.git/config, /terraform.tfstate) within hours of boot. The
# workers keep their external IP — the project has no Cloud NAT, and they need
# egress for the GitHub clone + Secret Manager + gsutil — so the port is closed
# at the firewall instead: a dedicated network tag (below, replacing the old
# 0.0.0.0/0 `allow-ttyd` rule/tag on this lane) with an ingress allow scoped to
# the operator's networks plus the cm-manager host, ensured at launch by
# dispatch/vm.py::ensure_ttyd_firewall.
BACKTEST_TTYD_TAG = os.getenv("CM_BACKTEST_TTYD_TAG", "cm-backtest-ttyd")
# Comma-separated CIDRs allowed to reach :8080 on backtest workers (set in the
# claude-manager.service drop-in ttyd-firewall.conf). The dispatcher appends
# its own external IP when it runs on GCE, so this only needs the operator's
# networks. Empty AND off-GCE means no rule is ensured — ingress stays denied
# (GCP default-deny, fail closed); the ssh tunnel documented in
# worker/backtest_startup.sh remains the access path.
BACKTEST_TTYD_SOURCE_RANGES = [
    r.strip()
    for r in os.getenv("CM_BACKTEST_TTYD_SOURCE_RANGES", "").split(",")
    if r.strip()
]

_missing = [name for name, value in (("CM_DB_DSN", DB_DSN), ("CM_API_TOKEN", API_TOKEN)) if not value]
if _missing:
    raise RuntimeError(
        f"Required environment variable(s) not set: {', '.join(_missing)}. "
        f"Set them in the systemd unit (production) or ~/.config/claude-manager/.env (local)."
    )

# Repo shortnames -> full clone URLs (discovered from ~/.cm/projects/*/repo_url)
def _discover_repos():
    repos = {}
    projects_dir = Path.home() / ".cm" / "projects"
    if projects_dir.is_dir():
        for entry in projects_dir.iterdir():
            url_file = entry / "repo_url"
            if entry.is_dir() and url_file.exists():
                url = url_file.read_text().strip()
                if url:
                    repos[entry.name] = url
    return repos

REPOS = _discover_repos()
