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
DB_DSN = os.getenv("CM_DB_DSN", "postgresql://predictionuser:oracle123@localhost/claude_manager")

# API
MANAGER_URL = os.getenv("CM_API_URL", "http://localhost:8000")
API_TOKEN = os.getenv("CM_API_TOKEN", "dev-token")
MAX_WORKERS = int(os.getenv("CM_MAX_WORKERS", "3"))

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
