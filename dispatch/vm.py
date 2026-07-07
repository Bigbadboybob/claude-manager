"""Launch and manage GCP spot VMs for worker tasks."""
import uuid
from google.cloud import compute_v1
from dispatch.config import GCP_PROJECT, GCP_ZONE, VM_MACHINE_TYPE, VM_IMAGE_FAMILY, VM_IMAGE_PROJECT, API_TOKEN
from pathlib import Path

# GCE caps instance metadata at 256KB/value and 512KB total. The backtest
# payload (inline YAML config + fields) plus the startup script sit far
# under that, but reject pathological values with a clear error instead of
# letting the insert fail opaquely.
_METADATA_VALUE_MAX_BYTES = 250_000


def read_worker_script(filename: str = "startup.sh") -> str:
    """Read a worker VM startup script from worker/ by filename."""
    return (Path(__file__).parent.parent / "worker" / filename).read_text()


def launch_worker(task_id: str, repo_url: str, repo_branch: str,
                  prompt: str, manager_callback_url: str, *,
                  overrides: dict | None = None,
                  startup_script: str | None = None,
                  extra_metadata: dict[str, str] | None = None) -> tuple[str, str]:
    """Create a spot VM for a task. Returns (instance_name, external_ip).

    `overrides` may carry per-task VM settings (metadata.vm on backtest
    tasks): project, zone, machine_type, image_family, image_project,
    service_account, disk_size_gb. Anything absent falls back to the
    process-global config values, so existing callers are unchanged.

    The instance name carries a random suffix: SPOT preemption STOPs the
    instance (instance_termination_action="STOP"), so a preempt-requeued
    task's relaunch would collide with its still-existing old instance if
    names were derived from the task id alone.
    """
    client = compute_v1.InstancesClient()

    o = overrides or {}
    project = o.get("project") or GCP_PROJECT
    zone = o.get("zone") or GCP_ZONE
    machine_type = o.get("machine_type") or VM_MACHINE_TYPE
    image_family = o.get("image_family") or VM_IMAGE_FAMILY
    image_project = o.get("image_project") or VM_IMAGE_PROJECT
    sa_email = o.get("service_account") or "default"
    disk_size_gb = int(o.get("disk_size_gb") or 50)
    disk_type = o.get("disk_type") or "pd-balanced"

    instance_name = f"cm-worker-{task_id[:8]}-{uuid.uuid4().hex[:6]}"

    if startup_script is None:
        startup_script = read_worker_script("startup.sh")

    items = [
        compute_v1.Items(key="startup-script", value=startup_script),
        compute_v1.Items(key="task-id", value=task_id),
        compute_v1.Items(key="repo-url", value=repo_url),
        compute_v1.Items(key="repo-branch", value=repo_branch),
        compute_v1.Items(key="task-prompt", value=prompt),
        compute_v1.Items(key="manager-callback-url", value=manager_callback_url),
        compute_v1.Items(key="api-token", value=API_TOKEN),
    ]
    for key, value in (extra_metadata or {}).items():
        if len(value.encode("utf-8")) > _METADATA_VALUE_MAX_BYTES:
            raise ValueError(
                f"instance metadata value for {key!r} exceeds "
                f"{_METADATA_VALUE_MAX_BYTES} bytes (GCE limit)"
            )
        items.append(compute_v1.Items(key=key, value=value))

    instance = compute_v1.Instance(
        name=instance_name,
        machine_type=f"zones/{zone}/machineTypes/{machine_type}",
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
                    source_image=f"projects/{image_project}/global/images/family/{image_family}",
                    disk_size_gb=disk_size_gb,
                    disk_type=f"zones/{zone}/diskTypes/{disk_type}",
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
        metadata=compute_v1.Metadata(items=items),
        service_accounts=[
            compute_v1.ServiceAccount(
                # "default" resolves to the default compute SA of the project
                # the instance is created IN — backtest workers automatically
                # get the prediction-market-scalper SA.
                email=sa_email,
                scopes=["https://www.googleapis.com/auth/cloud-platform"],
            ),
        ],
        tags=compute_v1.Tags(items=["cm-worker", "allow-ttyd"]),
    )

    op = client.insert(project=project, zone=zone, instance_resource=instance)
    op.result()  # Wait for creation

    # Get the external IP
    inst = client.get(project=project, zone=zone, instance=instance_name)
    external_ip = inst.network_interfaces[0].access_configs[0].nat_i_p

    return instance_name, external_ip


def delete_worker(instance_name: str, project: str = GCP_PROJECT,
                  zone: str = GCP_ZONE):
    """Delete a worker VM. Backtest workers live in another project/zone —
    callers with a task row should resolve the location from metadata.vm."""
    client = compute_v1.InstancesClient()
    try:
        op = client.delete(project=project, zone=zone, instance=instance_name)
        op.result()
    except Exception:
        pass  # Already deleted


def get_worker_ip(instance_name: str, project: str = GCP_PROJECT,
                  zone: str = GCP_ZONE) -> str | None:
    """Get the external IP of a worker VM."""
    client = compute_v1.InstancesClient()
    try:
        inst = client.get(project=project, zone=zone, instance=instance_name)
        return inst.network_interfaces[0].access_configs[0].nat_i_p
    except Exception:
        return None
