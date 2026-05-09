import asyncio
import logging
import shlex
import subprocess
import uuid
from datetime import datetime, timezone
from dispatch import db
from dispatch.config import (
    GCP_PROJECT, GCP_ZONE, MAX_WORKERS, MANAGER_URL, API_TOKEN,
)


def _warm_vm_name(pool_id) -> str:
    """Build a unique warm-VM instance name for a pool.

    Pool id alone collides when pool_size > 1 (gcloud rejects duplicate
    instance names with 409 Conflict), so we mix in a short uuid4 suffix.
    """
    return f"cm-warm-{str(pool_id)[:8]}-{uuid.uuid4().hex[:6]}"

logger = logging.getLogger("cm.dispatch")

DISPATCH_INTERVAL_SECS = 10
WARM_POOL_INTERVAL_SECS = 30


async def dispatch_loop(pool, interval: float = DISPATCH_INTERVAL_SECS):
    """Background loop: claim backlog tasks and assign workers.

    Runs independently of warm-pool maintenance — see ``warm_pool_loop``.
    A slow warm-pool pass (serial gcloud ssh probes per VM, gcloud
    instances create, etc.) must not delay task dispatch, which is why
    the two cadences live in separate asyncio tasks.
    """
    logger.info(f"Dispatch daemon started (max_workers={MAX_WORKERS})")

    while True:
        try:
            await _dispatch_tasks(pool)
        except asyncio.CancelledError:
            logger.info("Dispatch daemon shutting down")
            raise
        except Exception:
            logger.exception("Dispatch loop error")

        await asyncio.sleep(interval)


async def warm_pool_loop(pool, interval: float = WARM_POOL_INTERVAL_SECS):
    """Background loop: maintain warm-pool VM counts and health.

    Lives in its own asyncio task (see ``api.main.lifespan``) so a slow
    pass doesn't drift the dispatch cadence. Per-VM probes inside
    ``_maintain_warm_pools`` are themselves parallelized — one slow
    gcloud ssh shouldn't stall the whole pass either.
    """
    logger.info("Warm-pool maintenance loop started")

    while True:
        try:
            await _maintain_warm_pools(pool)
        except asyncio.CancelledError:
            logger.info("Warm-pool maintenance shutting down")
            raise
        except Exception:
            logger.exception("Warm-pool loop error")

        await asyncio.sleep(interval)


async def _dispatch_tasks(pool):
    """Claim backlog tasks and launch workers (or assign to warm VMs)."""
    active_count = await db.count_dispatchable(pool)

    if active_count >= MAX_WORKERS:
        return

    task = await db.claim_next_task(pool)
    if not task:
        return

    logger.info(f"Dispatching task {task['id']}")

    # Atomically claim a ready warm VM (status flips to busy in same statement)
    warm_vm = await db.find_ready_warm_vm(pool, task["repo_url"], str(task["id"]))
    if warm_vm:
        await _assign_to_warm_vm(pool, task, warm_vm)
    else:
        await _launch_new_worker(pool, task)


async def _assign_to_warm_vm(pool, task, warm_vm):
    """Assign a task to a warm VM that has already been claimed (status=busy)."""
    logger.info(f"Assigning task {task['id']} to warm VM {warm_vm['vm_name']}")

    branch = task.get("wip_branch") or task["repo_branch"]
    prompt = task.get("prompt") or ""

    try:
        if branch != "main":
            inner = (
                "cd /workspace && git fetch origin && "
                f"git checkout {shlex.quote(branch)}"
            )
            await asyncio.to_thread(
                _ssh_command, warm_vm["vm_name"],
                f"sudo -u worker bash -c {shlex.quote(inner)}",
            )

        # Deliver the prompt via tmux paste-buffer fed from stdin so no
        # user-controlled bytes ever land in a shell-interpolated string.
        if prompt:
            remote_cmd = (
                "sudo -u worker tmux load-buffer - && "
                "sudo -u worker tmux paste-buffer -t claude && "
                "sudo -u worker tmux send-keys -t claude Enter"
            )
            await asyncio.to_thread(
                _ssh_command, warm_vm["vm_name"], remote_cmd, prompt,
            )
    except Exception:
        logger.exception(
            f"Failed to assign task {task['id']} to warm VM "
            f"{warm_vm['vm_name']}; rolling back"
        )
        await db.update_warm_vm(
            pool, warm_vm["id"], status="ready", current_task_id=None,
        )
        await db.update_task(pool, str(task["id"]), status="backlog")
        return

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
            vm_name = _warm_vm_name(wp["id"])
            # Persist the row BEFORE gcloud creates the instance so concurrent
            # dispatchers can't pick the same generated name (the unique
            # constraint on warm_vms.vm_name will reject the second insert).
            try:
                vm_row = await db.add_warm_vm(
                    pool, wp["id"], vm_name, GCP_ZONE, external_ip=None,
                )
            except Exception:
                logger.exception(f"Failed to reserve warm VM row for pool {wp['id']}")
                continue
            try:
                external_ip = await asyncio.to_thread(
                    _launch_warm_vm_sync, wp, vm_name,
                )
                await db.update_warm_vm(pool, vm_row["id"], external_ip=external_ip)
                logger.info(f"Warm VM {vm_name} launched ({external_ip})")
            except Exception:
                logger.exception(f"Failed to launch warm VM for pool {wp['id']}")
                # Roll back the reserved row so the slot can be retried next tick.
                try:
                    await db.delete_warm_vm(pool, vm_row["id"])
                except Exception:
                    logger.exception(
                        f"Failed to clean up reserved warm VM row {vm_row['id']}"
                    )

        # Check health of existing VMs in parallel — a single slow gcloud
        # ssh would otherwise stall the whole pass behind it.
        if alive_vms:
            results = await asyncio.gather(
                *(_probe_warm_vm(vm) for vm in alive_vms)
            )
            for vm, is_alive, became_ready in results:
                if not is_alive:
                    logger.warning(f"Warm VM {vm['vm_name']} is dead, removing")
                    await db.delete_warm_vm(pool, vm["id"])
                    if vm.get("current_task_id"):
                        await db.update_task(pool, vm["current_task_id"], status="backlog")
                elif became_ready:
                    logger.info(f"Warm VM {vm['vm_name']} is now ready")
                    await db.update_warm_vm(pool, vm["id"], status="ready")


async def _probe_warm_vm(vm):
    """Probe a single warm VM concurrently with siblings.

    Returns ``(vm, is_alive, became_ready)``. DB writes stay in the caller
    so we don't fan out asyncpg connection use for what is mostly an
    IO-bound gcloud roundtrip.
    """
    is_alive = await asyncio.to_thread(_check_vm_alive, vm["vm_name"])
    if not is_alive:
        return (vm, False, False)
    if vm["status"] != "booting":
        return (vm, True, False)
    try:
        output = await asyncio.to_thread(
            _ssh_command, vm["vm_name"],
            "grep -c 'ready and waiting' /var/log/cm-worker.log 2>/dev/null || echo 0",
        )
        return (vm, True, output.strip() != "0")
    except Exception:
        return (vm, True, False)



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


def _launch_warm_vm_sync(wp, vm_name):
    """Launch a warm pool VM (no task, just repo setup).

    The caller pre-generates ``vm_name`` and persists the warm_vms row so
    concurrent dispatchers can't race on the same instance name.
    """
    from dispatch.vm import launch_worker
    from pathlib import Path

    warm_startup = (Path(__file__).parent.parent / "worker" / "warm_startup.sh").read_text()

    from google.cloud import compute_v1
    from dispatch.config import VM_IMAGE_FAMILY, VM_IMAGE_PROJECT

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
    return external_ip


def _check_vm_alive(vm_name: str) -> bool:
    """Check if a GCP VM instance exists and is running."""
    try:
        from google.cloud import compute_v1
        client = compute_v1.InstancesClient()
        inst = client.get(project=GCP_PROJECT, zone=GCP_ZONE, instance=vm_name)
        return inst.status == "RUNNING"
    except Exception:
        return False


def _ssh_command(vm_name: str, command: str, stdin: str | None = None) -> str:
    """Run a command on a VM via SSH.

    Raises subprocess.CalledProcessError on non-zero exit and
    subprocess.TimeoutExpired if the command runs past the timeout — callers
    that want to ignore failures must wrap the call in try/except.
    """
    result = subprocess.run(
        ["gcloud", "compute", "ssh", vm_name,
         f"--zone={GCP_ZONE}", f"--project={GCP_PROJECT}",
         "--command", command],
        input=stdin,
        capture_output=True, text=True, timeout=15, check=True,
    )
    return result.stdout
