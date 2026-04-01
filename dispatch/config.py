GCP_PROJECT = "prediction-market-scalper"
GCP_ZONE = "us-east4-a"
VM_MACHINE_TYPE = "e2-medium"
VM_IMAGE_FAMILY = "cm-worker-base"
VM_IMAGE_PROJECT = "prediction-market-scalper"

# Local postgres (Phase 1)
DB_DSN = "postgresql://predictionuser:oracle123@localhost/claude_manager"

# Repo shortnames -> full clone URLs
REPOS = {
    "predictionTrading": "https://github.com/Bigbadboybob/predictionTrading.git",
}
