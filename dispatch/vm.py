"""Launch and manage GCP spot VMs for worker tasks."""
import json
from google.cloud import compute_v1
from google.cloud import secretmanager_v1
from dispatch.config import GCP_PROJECT, GCP_ZONE, VM_MACHINE_TYPE, VM_IMAGE_FAMILY, VM_IMAGE_PROJECT
from pathlib import Path


def get_startup_script(task_id: str, repo_url: str, repo_branch: str,
                       prompt: str, manager_callback_url: str) -> str:
    """Build the worker VM startup script with task-specific metadata."""
    template = (Path(__file__).parent.parent / "worker" / "startup.sh").read_text()
    # We pass task info via instance metadata, not in the script itself
    return template


def launch_worker(task_id: str, repo_url: str, repo_branch: str,
                  prompt: str, manager_callback_url: str) -> str:
    """Create a spot VM for a task. Returns the instance name."""
    client = compute_v1.InstancesClient()

    instance_name = f"cm-worker-{task_id[:8]}"

    startup_script = get_startup_script(
        task_id, repo_url, repo_branch, prompt, manager_callback_url,
    )

    instance = compute_v1.Instance(
        name=instance_name,
        machine_type=f"zones/{GCP_ZONE}/machineTypes/{VM_MACHINE_TYPE}",
        scheduling=compute_v1.Scheduling(
            provisioning_model="SPOT",
            instance_termination_action="STOP",
            on_host_maintenance="TERMINATE",
        ),
        disks=[
            compute_v1.AttachedDisk(
                auto_delete=True,
                boot=True,
                initialize_params=compute_v1.AttachedDiskInitializeParams(
                    source_image=f"projects/{VM_IMAGE_PROJECT}/global/images/family/{VM_IMAGE_FAMILY}",
                    disk_size_gb=50,
                    disk_type=f"zones/{GCP_ZONE}/diskTypes/pd-balanced",
                ),
            ),
        ],
        network_interfaces=[
            compute_v1.NetworkInterface(
                access_configs=[
                    compute_v1.AccessConfig(name="External NAT"),
                ],
            ),
        ],
        metadata=compute_v1.Metadata(
            items=[
                compute_v1.Items(key="startup-script", value=startup_script),
                compute_v1.Items(key="task-id", value=task_id),
                compute_v1.Items(key="repo-url", value=repo_url),
                compute_v1.Items(key="repo-branch", value=repo_branch),
                compute_v1.Items(key="task-prompt", value=prompt),
                compute_v1.Items(key="manager-callback-url", value=manager_callback_url),
            ],
        ),
        service_accounts=[
            compute_v1.ServiceAccount(
                email="default",
                scopes=["https://www.googleapis.com/auth/cloud-platform"],
            ),
        ],
        tags=compute_v1.Tags(items=["cm-worker", "allow-ttyd"]),
    )

    op = client.insert(project=GCP_PROJECT, zone=GCP_ZONE, instance_resource=instance)
    op.result()  # Wait for creation

    # Get the external IP
    inst = client.get(project=GCP_PROJECT, zone=GCP_ZONE, instance=instance_name)
    external_ip = inst.network_interfaces[0].access_configs[0].nat_i_p

    return instance_name, external_ip


def delete_worker(instance_name: str):
    """Delete a worker VM."""
    client = compute_v1.InstancesClient()
    try:
        op = client.delete(project=GCP_PROJECT, zone=GCP_ZONE, instance=instance_name)
        op.result()
    except Exception:
        pass  # Already deleted


def get_worker_ip(instance_name: str) -> str | None:
    """Get the external IP of a worker VM."""
    client = compute_v1.InstancesClient()
    try:
        inst = client.get(project=GCP_PROJECT, zone=GCP_ZONE, instance=instance_name)
        return inst.network_interfaces[0].access_configs[0].nat_i_p
    except Exception:
        return None
