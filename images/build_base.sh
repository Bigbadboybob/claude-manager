#!/usr/bin/env bash
# Creates the cm-worker base image VM.
# After running this:
#   1. SSH in and run `sudo su - worker -c "claude setup-token"` to auth
#   2. Navigate the OAuth flow (visit URL, paste code)
#   3. Run: gcloud compute instances stop cm-base-image --zone=us-east4-a --project=prediction-market-scalper
#   4. Run: gcloud compute images create cm-worker-base-v1 --source-disk=cm-base-image --source-disk-zone=us-east4-a --project=prediction-market-scalper --family=cm-worker-base
#   5. Delete the VM: gcloud compute instances delete cm-base-image --zone=us-east4-a --project=prediction-market-scalper --quiet

set -euo pipefail

PROJECT="prediction-market-scalper"
ZONE="us-east4-a"
VM_NAME="cm-base-image"

gcloud compute instances create "$VM_NAME" \
    --project="$PROJECT" \
    --zone="$ZONE" \
    --machine-type=e2-medium \
    --image-family=ubuntu-2404-lts-amd64 \
    --image-project=ubuntu-os-cloud \
    --boot-disk-size=50GB \
    --boot-disk-type=pd-balanced \
    --metadata=startup-script='#!/bin/bash
set -euo pipefail
export HOME=/root
export DEBIAN_FRONTEND=noninteractive
exec > /var/log/cm-base-setup.log 2>&1

echo "[base] Starting base image setup at $(date)"

# System packages
apt-get update -qq
apt-get install -y -qq git tmux ttyd jq curl python3 python3-pip nodejs npm > /dev/null 2>&1
echo "[base] System packages installed"

# uv
curl -LsSf https://astral.sh/uv/install.sh | sh
export PATH="/root/.local/bin:$PATH"
echo "[base] uv installed"

# Claude Code
npm install -g @anthropic-ai/claude-code > /dev/null 2>&1
echo "[base] Claude Code installed at $(which claude)"

# Create worker user
useradd -m -s /bin/bash worker 2>/dev/null || true
WORKER_HOME=/home/worker

# uv for worker user too
su - worker -c "curl -LsSf https://astral.sh/uv/install.sh | sh" > /dev/null 2>&1

# Pre-configure Claude settings
mkdir -p "$WORKER_HOME/.claude"
cat > "$WORKER_HOME/.claude/settings.json" << SETTINGS
{"skipDangerousModePermissionPrompt": true}
SETTINGS
chown -R worker:worker "$WORKER_HOME/.claude"

echo "[base] Base image setup complete at $(date)"
echo "[base] Next: SSH in and run claude setup-token as worker user"
' \
    --tags=cm-worker,allow-ttyd \
    --service-account=default \
    --scopes=cloud-platform

echo "VM created: $VM_NAME"
echo "Wait ~5 min for setup, then check:"
echo "  gcloud compute ssh $VM_NAME --zone=$ZONE --project=$PROJECT -- 'cat /var/log/cm-base-setup.log'"
