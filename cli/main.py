import asyncio
import subprocess
import sys
import webbrowser
from datetime import datetime, timezone

import click

from dispatch.config import REPOS, DB_DSN
from dispatch import db as db_mod


def run(coro):
    """Run an async function from sync click commands."""
    return asyncio.get_event_loop().run_until_complete(coro)


async def _get_pool():
    return await db_mod.get_pool()


def short_id(task_id) -> str:
    return str(task_id)[:8]


def time_ago(dt) -> str:
    if dt is None:
        return ""
    delta = datetime.now(timezone.utc) - dt
    minutes = int(delta.total_seconds() / 60)
    if minutes < 1:
        return "just now"
    if minutes < 60:
        return f"{minutes}m"
    hours = minutes // 60
    if hours < 24:
        return f"{hours}h"
    return f"{hours // 24}d"


def resolve_repo(name: str) -> str:
    """Resolve a repo shortname to a full URL."""
    if name in REPOS:
        return REPOS[name]
    if name.startswith("http") or name.startswith("git@"):
        return name
    # Try as GitHub owner/repo
    if "/" in name:
        return f"https://github.com/{name}.git"
    raise click.BadParameter(
        f"Unknown repo '{name}'. Known repos: {', '.join(REPOS.keys())}",
        param_hint="--repo",
    )


@click.group()
def cli():
    """Claude Manager — dispatch Claude Code tasks to cloud VMs."""
    pass


@cli.command()
@click.option("--repo", "-r", required=True, help="Repo name or URL")
@click.option("--prompt", "-p", required=True, help="Task prompt")
@click.option("--branch", "-b", default="main", help="Branch to clone")
@click.option("--priority", default=0, help="Priority (lower = higher)")
@click.option("--prompt-file", type=click.Path(exists=True), help="Read prompt from file")
def add(repo, prompt, branch, priority, prompt_file):
    """Add a task to the backlog."""
    if prompt_file:
        prompt = open(prompt_file).read()

    repo_url = resolve_repo(repo)

    async def _add():
        pool = await _get_pool()
        try:
            task = await db_mod.add_task(pool, repo_url, branch, prompt, priority)
            click.echo(f"Added task {short_id(task['id'])} to backlog")
            click.echo(f"  Repo:   {repo_url}")
            click.echo(f"  Prompt: {prompt[:80]}{'...' if len(prompt) > 80 else ''}")
        finally:
            await pool.close()

    run(_add())


@cli.command()
def backlog():
    """List backlog tasks."""
    async def _list():
        pool = await _get_pool()
        try:
            tasks = await db_mod.list_tasks(pool, status="backlog")
            if not tasks:
                click.echo("Backlog is empty.")
                return
            click.echo(f"  {'#':<4} {'Pri':<5} {'Task':<55} {'Repo':<25} {'Added'}")
            click.echo(f"  {'─'*4} {'─'*5} {'─'*55} {'─'*25} {'─'*8}")
            for t in tasks:
                prompt_short = t["prompt"][:52] + "..." if len(t["prompt"]) > 55 else t["prompt"]
                repo_short = t["repo_url"].split("/")[-1].replace(".git", "")
                click.echo(
                    f"  {short_id(t['id']):<4} {t['priority']:<5} "
                    f"{prompt_short:<55} {repo_short:<25} {time_ago(t['created_at'])}"
                )
        finally:
            await pool.close()

    run(_list())


@cli.command()
def queue():
    """List blocked tasks (your work queue)."""
    async def _list():
        pool = await _get_pool()
        try:
            tasks = await db_mod.list_tasks(pool, status="blocked")
            if not tasks:
                click.echo("Queue is empty. No blocked tasks.")
                return
            click.echo(f"  {'#':<10} {'Task':<55} {'Repo':<20} {'Waiting'}")
            click.echo(f"  {'─'*10} {'─'*55} {'─'*20} {'─'*8}")
            for t in tasks:
                prompt_short = t["prompt"][:52] + "..." if len(t["prompt"]) > 55 else t["prompt"]
                repo_short = t["repo_url"].split("/")[-1].replace(".git", "")
                click.echo(
                    f"  {short_id(t['id']):<10} {prompt_short:<55} "
                    f"{repo_short:<20} {time_ago(t['blocked_at'])}"
                )
        finally:
            await pool.close()

    run(_list())


@cli.command()
@click.argument("task_id")
@click.option("--browser", is_flag=True, help="Open in browser via ttyd instead of SSH")
def open(task_id, browser):
    """Open a task's Claude session (SSH into tmux by default)."""
    from dispatch.config import GCP_PROJECT, GCP_ZONE

    async def _open():
        pool = await _get_pool()
        try:
            tasks = await db_mod.list_tasks(pool)
            task = next((t for t in tasks if str(t["id"]).startswith(task_id)), None)
            if not task:
                click.echo(f"No task found matching '{task_id}'")
                return
            if not task["worker_vm"]:
                click.echo(f"Task {short_id(task['id'])} has no worker VM")
                return

            if browser:
                if not task["ttyd_url"]:
                    click.echo(f"Task {short_id(task['id'])} has no ttyd URL")
                    return
                click.echo(f"Opening {task['ttyd_url']}")
                webbrowser.open(task["ttyd_url"])
            else:
                vm = task["worker_vm"]
                click.echo(f"Attaching to {vm}...")
                # Replace this process with SSH + tmux attach
                import os
                os.execvp("gcloud", [
                    "gcloud", "compute", "ssh", vm,
                    f"--zone={GCP_ZONE}", f"--project={GCP_PROJECT}",
                    "--", "-t",
                    "TERM=xterm-256color sudo su - worker -c 'tmux attach -t claude'",
                ])
        finally:
            await pool.close()

    run(_open())


@cli.command()
@click.argument("task_id")
def done(task_id):
    """Mark a task as done and shut down its worker VM."""
    from dispatch.vm import delete_worker

    async def _done():
        pool = await _get_pool()
        try:
            tasks = await db_mod.list_tasks(pool)
            task = next((t for t in tasks if str(t["id"]).startswith(task_id)), None)
            if not task:
                click.echo(f"No task found matching '{task_id}'")
                return
            await db_mod.update_task(pool, str(task["id"]), status="done")
            click.echo(f"Task {short_id(task['id'])} marked done")
            if task["worker_vm"]:
                click.echo(f"Deleting VM {task['worker_vm']}...")
                delete_worker(task["worker_vm"])
                click.echo("VM deleted")
        finally:
            await pool.close()

    run(_done())


@cli.command()
@click.argument("task_id")
def cancel(task_id):
    """Cancel a task and shut down its worker VM."""
    from dispatch.vm import delete_worker

    async def _cancel():
        pool = await _get_pool()
        try:
            tasks = await db_mod.list_tasks(pool)
            task = next((t for t in tasks if str(t["id"]).startswith(task_id)), None)
            if not task:
                click.echo(f"No task found matching '{task_id}'")
                return
            await db_mod.update_task(pool, str(task["id"]), status="done")
            click.echo(f"Task {short_id(task['id'])} cancelled")
            if task["worker_vm"]:
                click.echo(f"Deleting VM {task['worker_vm']}...")
                delete_worker(task["worker_vm"])
                click.echo("VM deleted")
        finally:
            await pool.close()

    run(_cancel())


@cli.command()
def status():
    """Show summary of all tasks."""
    async def _status():
        pool = await _get_pool()
        try:
            tasks = await db_mod.list_tasks(pool)
            counts = {"backlog": 0, "running": 0, "blocked": 0, "done": 0}
            for t in tasks:
                counts[t["status"]] += 1
            click.echo(f"  Backlog: {counts['backlog']}")
            click.echo(f"  Running: {counts['running']}")
            click.echo(f"  Blocked: {counts['blocked']}")
            click.echo(f"  Done:    {counts['done']}")
        finally:
            await pool.close()

    run(_status())


@cli.command()
def workers():
    """List active worker VMs."""
    async def _workers():
        pool = await _get_pool()
        try:
            tasks = await db_mod.list_tasks(pool)
            active = [t for t in tasks if t["status"] in ("running", "blocked") and t["worker_vm"]]
            if not active:
                click.echo("No active workers.")
                return
            click.echo(f"  {'VM':<25} {'Status':<10} {'Task':<40} {'ttyd'}")
            click.echo(f"  {'─'*25} {'─'*10} {'─'*40} {'─'*30}")
            for t in active:
                prompt_short = t["prompt"][:37] + "..." if len(t["prompt"]) > 40 else t["prompt"]
                click.echo(
                    f"  {t['worker_vm']:<25} {t['status']:<10} "
                    f"{prompt_short:<40} {t['ttyd_url'] or 'pending'}"
                )
        finally:
            await pool.close()

    run(_workers())


@cli.command()
@click.argument("task_id")
def launch(task_id):
    """Manually launch a worker VM for a backlog task."""
    from dispatch.vm import launch_worker

    async def _launch():
        pool = await _get_pool()
        try:
            tasks = await db_mod.list_tasks(pool)
            task = next((t for t in tasks if str(t["id"]).startswith(task_id)), None)
            if not task:
                click.echo(f"No task found matching '{task_id}'")
                return
            if task["status"] != "backlog":
                click.echo(f"Task {short_id(task['id'])} is '{task['status']}', not 'backlog'")
                return

            click.echo(f"Launching VM for task {short_id(task['id'])}...")

            # For Phase 1, callback URL is empty (watcher posts directly)
            vm_name, external_ip = launch_worker(
                task_id=str(task["id"]),
                repo_url=task["repo_url"],
                repo_branch=task["repo_branch"],
                prompt=task["prompt"],
                manager_callback_url="",
            )

            ttyd_url = f"http://{external_ip}:8080"
            await db_mod.update_task(
                pool, str(task["id"]),
                status="running",
                worker_vm=vm_name,
                worker_zone="us-east4-a",
                ttyd_url=ttyd_url,
            )

            click.echo(f"VM: {vm_name}")
            click.echo(f"IP: {external_ip}")
            click.echo(f"ttyd: {ttyd_url}")
            click.echo(f"Task status -> running")
        finally:
            await pool.close()

    run(_launch())


@cli.command()
def sync():
    """Poll worker VMs and update task states."""
    from dispatch.config import GCP_PROJECT, GCP_ZONE

    async def _sync():
        pool = await _get_pool()
        try:
            tasks = await db_mod.list_tasks(pool)
            active = [t for t in tasks if t["status"] in ("running", "blocked") and t["worker_vm"]]
            if not active:
                click.echo("No active workers to sync.")
                return
            for t in active:
                vm = t["worker_vm"]
                try:
                    result = subprocess.run(
                        ["gcloud", "compute", "ssh", vm,
                         f"--zone={GCP_ZONE}", f"--project={GCP_PROJECT}",
                         "--command", "cat /var/log/cm-worker-state 2>/dev/null || echo unknown"],
                        capture_output=True, text=True, timeout=15,
                    )
                    state = result.stdout.strip()
                    if state in ("running", "blocked", "done") and state != t["status"]:
                        updates = {"status": state}
                        if state == "blocked":
                            from datetime import datetime, timezone
                            updates["blocked_at"] = datetime.now(timezone.utc)
                        await db_mod.update_task(pool, str(t["id"]), **updates)
                        click.echo(f"  {short_id(t['id'])} {t['status']} -> {state}")
                    else:
                        click.echo(f"  {short_id(t['id'])} {state} (no change)")
                except Exception as e:
                    click.echo(f"  {short_id(t['id'])} error: {e}")
        finally:
            await pool.close()

    run(_sync())


@cli.command()
def init_db():
    """Initialize the database schema."""
    async def _init():
        pool = await _get_pool()
        try:
            await db_mod.init_db(pool)
            click.echo("Database initialized")
        finally:
            await pool.close()

    run(_init())


if __name__ == "__main__":
    cli()
