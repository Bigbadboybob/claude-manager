import asyncio
import logging
import subprocess
from datetime import datetime, timezone
from dispatch import db
from dispatch.config import (
    GCP_PROJECT, GCP_ZONE, MAX_WORKERS, MANAGER_URL, API_TOKEN,
)

logger = logging.getLogger("cm.dispatch")


async def dispatch_loop(pool):
    """Background loop: dispatch tasks, maintain warm pools, detect zombies."""
    logger.info(f"Dispatch daemon started (max_workers={MAX_WORKERS})")

    tick = 0
    while True:
        try:
            await _dispatch_tasks(pool)

            # Warm pool maintenance every 30s
            if tick % 3 == 0:
                await _maintain_warm_pools(pool)


        except asyncio.CancelledError:
            logger.info("Dispatch daemon shutting down")
            raise
        except Exception:
            logger.exception("Dispatch loop error")

        tick += 1
        await asyncio.sleep(10)


async def _dispatch_tasks(pool):
    """Claim backlog tasks and launch workers (or assign to warm VMs)."""
    running = await db.list_tasks(pool, status="running")
    blocked = await db.list_tasks(pool, status="blocked")
    active_count = len(running) + len(blocked)

    if active_count >= MAX_WORKERS:
        return

    task = await db.claim_next_task(pool)
    if not task:
        return

    logger.info(f"Dispatching task {task['id']}")

    # Check if there's a warm VM ready for this repo
    warm_vm = await db.find_ready_warm_vm(pool, task["repo_url"])
    if warm_vm:
        await _assign_to_warm_vm(pool, task, warm_vm)
    else:
        await _launch_new_worker(pool, task)


async def _assign_to_warm_vm(pool, task, warm_vm):
    """Assign a task to an existing warm VM."""
    logger.info(f"Assigning task {task['id']} to warm VM {warm_vm['vm_name']}")

    # Mark warm VM as busy
    await db.update_warm_vm(pool, warm_vm["id"],
                            status="busy", current_task_id=task["id"])

    branch = task.get("wip_branch") or task["repo_branch"]

    # If the task has a different branch, checkout first
    if branch != "main":
        await asyncio.to_thread(
            _ssh_command, warm_vm["vm_name"],
            f"sudo su - worker -c 'cd /workspace && git fetch origin && git checkout {branch}'"
        )

    # Send the prompt via tmux (only if prompt exists — otherwise sync mode)
    prompt = task.get("prompt") or ""
    if prompt:
        await asyncio.to_thread(
            _ssh_command, warm_vm["vm_name"],
            f"sudo su - worker -c \"tmux send-keys -t claude '{prompt}' Enter\""
        )

    ttyd_url = f"http://{warm_vm['external_ip']}:8080"
    await db.update_task(pool, str(task["id"]),
                         worker_vm=warm_vm["vm_name"],
                         worker_zone=warm_vm["vm_zone"],
                         ttyd_url=ttyd_url)

    logger.info(f"Task {task['id']} assigned to warm VM {warm_vm['vm_name']}")


async def _launch_new_worker(pool, task):
    """Launch a new ephemeral worker VM for a task."""
    branch = task.get("wip_branch") or task["repo_branch"]
    try:
        vm_name, external_ip = await asyncio.to_thread(
            _launch_worker_sync, task, branch
        )
        ttyd_url = f"http://{external_ip}:8080"
        await db.update_task(
            pool, str(task["id"]),
            worker_vm=vm_name,
            worker_zone=GCP_ZONE,
            ttyd_url=ttyd_url,
        )
        logger.info(f"Task {task['id']} -> VM {vm_name} ({external_ip})")
    except Exception:
        logger.exception(f"Failed to launch VM for task {task['id']}")
        # Put back in backlog so it can be retried
        await db.update_task(pool, str(task["id"]), status="backlog")


async def _maintain_warm_pools(pool):
    """Ensure warm pools have the right number of VMs."""
    pools = await db.list_warm_pools(pool)
    for wp in pools:
        vms = await db.list_warm_vms(pool, pool_id=wp["id"])
        alive_vms = [v for v in vms if v["status"] != "dead"]

        # Launch missing VMs
        needed = wp["pool_size"] - len(alive_vms)
        for i in range(needed):
            logger.info(f"Launching warm VM for pool {wp['id']} ({wp['repo_url']})")
            try:
                vm_name, external_ip = await asyncio.to_thread(
                    _launch_warm_vm_sync, wp
                )
                await db.add_warm_vm(
                    pool, wp["id"], vm_name, GCP_ZONE, external_ip
                )
                logger.info(f"Warm VM {vm_name} launched ({external_ip})")
            except Exception:
                logger.exception(f"Failed to launch warm VM for pool {wp['id']}")

        # Check health of existing VMs
        for vm in alive_vms:
            is_alive = await asyncio.to_thread(_check_vm_alive, vm["vm_name"])
            if not is_alive:
                logger.warning(f"Warm VM {vm['vm_name']} is dead, removing")
                await db.delete_warm_vm(pool, vm["id"])
                if vm.get("current_task_id"):
                    await db.update_task(pool, vm["current_task_id"], status="backlog")
            elif vm["status"] == "booting":
                # Check if the VM is actually ready (Claude at the prompt)
                try:
                    output = await asyncio.to_thread(
                        _ssh_command, vm["vm_name"],
                        "grep -c 'ready and waiting' /var/log/cm-worker.log 2>/dev/null || echo 0"
                    )
                    if output.strip() != "0":
                        logger.info(f"Warm VM {vm['vm_name']} is now ready")
                        await db.update_warm_vm(pool, vm["id"], status="ready")
                except Exception:
                    pass



def _launch_worker_sync(task, branch):
    """Synchronous VM launch."""
    from dispatch.vm import launch_worker
    return launch_worker(
        task_id=str(task["id"]),
        repo_url=task["repo_url"],
        repo_branch=branch,
        prompt=task["prompt"],
        manager_callback_url=MANAGER_URL,
    )


def _launch_warm_vm_sync(wp):
    """Launch a warm pool VM (no task, just repo setup)."""
    from dispatch.vm import launch_worker
    from pathlib import Path

    warm_startup = (Path(__file__).parent.parent / "worker" / "warm_startup.sh").read_text()

    from google.cloud import compute_v1
    from dispatch.config import VM_IMAGE_FAMILY, VM_IMAGE_PROJECT

    vm_name = f"cm-warm-{str(wp['id'])[:8]}"
    client = compute_v1.InstancesClient()

    instance = compute_v1.Instance(
        name=vm_name,
        machine_type=f"zones/{GCP_ZONE}/machineTypes/{wp['vm_machine_type']}",
        scheduling=compute_v1.Scheduling(
            provisioning_model="SPOT",
            instance_termination_action="STOP",
            on_host_maintenance="TERMINATE",
        ),
        disks=[compute_v1.AttachedDisk(
            auto_delete=True, boot=True,
            initialize_params=compute_v1.AttachedDiskInitializeParams(
                source_image=f"projects/{VM_IMAGE_PROJECT}/global/images/family/{VM_IMAGE_FAMILY}",
                disk_size_gb=50,
                disk_type=f"zones/{GCP_ZONE}/diskTypes/pd-balanced",
            ),
        )],
        network_interfaces=[compute_v1.NetworkInterface(
            access_configs=[compute_v1.AccessConfig(name="External NAT")],
        )],
        metadata=compute_v1.Metadata(items=[
            compute_v1.Items(key="startup-script", value=warm_startup),
            compute_v1.Items(key="repo-url", value=wp["repo_url"]),
            compute_v1.Items(key="repo-branch", value=wp["repo_branch"]),
            compute_v1.Items(key="manager-callback-url", value=MANAGER_URL),
            compute_v1.Items(key="api-token", value=API_TOKEN),
            compute_v1.Items(key="pool-id", value=str(wp["id"])),
        ]),
        service_accounts=[compute_v1.ServiceAccount(
            email="default",
            scopes=["https://www.googleapis.com/auth/cloud-platform"],
        )],
        tags=compute_v1.Tags(items=["cm-worker", "allow-ttyd"]),
    )

    op = client.insert(project=GCP_PROJECT, zone=GCP_ZONE, instance_resource=instance)
    op.result()

    inst = client.get(project=GCP_PROJECT, zone=GCP_ZONE, instance=vm_name)
    external_ip = inst.network_interfaces[0].access_configs[0].nat_i_p
    return vm_name, external_ip


def _check_vm_alive(vm_name: str) -> bool:
    """Check if a GCP VM instance exists and is running."""
    try:
        from google.cloud import compute_v1
        client = compute_v1.InstancesClient()
        inst = client.get(project=GCP_PROJECT, zone=GCP_ZONE, instance=vm_name)
        return inst.status == "RUNNING"
    except Exception:
        return False


def _ssh_command(vm_name: str, command: str) -> str:
    """Run a command on a VM via SSH."""
    result = subprocess.run(
        ["gcloud", "compute", "ssh", vm_name,
         f"--zone={GCP_ZONE}", f"--project={GCP_PROJECT}",
         "--command", command],
        capture_output=True, text=True, timeout=15,
    )
    return result.stdout
