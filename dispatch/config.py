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
BACKTEST_MACHINE_TYPE = os.getenv("CM_BACKTEST_MACHINE_TYPE", "c2-standard-4")
BACKTEST_IMAGE_FAMILY = os.getenv("CM_BACKTEST_IMAGE_FAMILY", "cm-backtest-worker")
BACKTEST_IMAGE_PROJECT = os.getenv("CM_BACKTEST_IMAGE_PROJECT", BACKTEST_GCP_PROJECT)
BACKTEST_SECRETS_PROJECT = os.getenv("CM_BACKTEST_SECRETS_PROJECT", BACKTEST_GCP_PROJECT)
BACKTEST_RESULTS_BUCKET = os.getenv(
    "CM_BACKTEST_RESULTS_BUCKET", "gs://prediction-market-scalper-datasets"
)
BACKTEST_MAX_RUNTIME_SECS = int(os.getenv("CM_BACKTEST_MAX_RUNTIME_SECS", "14400"))  # 4h

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
