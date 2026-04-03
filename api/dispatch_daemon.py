import asyncio
import logging
from dispatch import db
from dispatch.config import GCP_ZONE, MAX_WORKERS, MANAGER_URL, API_TOKEN

logger = logging.getLogger("cm.dispatch")


async def dispatch_loop(pool):
    """Background loop: claim backlog tasks, launch VMs."""
    logger.info(f"Dispatch daemon started (max_workers={MAX_WORKERS})")

    while True:
        try:
            # Count active workers
            running = await db.list_tasks(pool, status="running")
            blocked = await db.list_tasks(pool, status="blocked")
            active_count = len(running) + len(blocked)

            if active_count < MAX_WORKERS:
                task = await db.claim_next_task(pool)
                if task:
                    logger.info(f"Dispatching task {task['id']}")
                    # Run VM launch in a thread to avoid blocking the event loop
                    vm_name, external_ip = await asyncio.to_thread(
                        _launch_worker, task
                    )
                    ttyd_url = f"http://{external_ip}:8080"
                    await db.update_task(
                        pool, str(task["id"]),
                        worker_vm=vm_name,
                        worker_zone=GCP_ZONE,
                        ttyd_url=ttyd_url,
                    )
                    logger.info(f"Task {task['id']} -> VM {vm_name} ({external_ip})")
        except asyncio.CancelledError:
            logger.info("Dispatch daemon shutting down")
            raise
        except Exception:
            logger.exception("Dispatch loop error")

        await asyncio.sleep(10)


def _launch_worker(task):
    """Synchronous VM launch (called via asyncio.to_thread)."""
    from dispatch.vm import launch_worker
    # Use wip_branch if task was preempted, otherwise use repo_branch
    branch = task.get("wip_branch") or task["repo_branch"]
    return launch_worker(
        task_id=str(task["id"]),
        repo_url=task["repo_url"],
        repo_branch=branch,
        prompt=task["prompt"],
        manager_callback_url=MANAGER_URL,
    )
