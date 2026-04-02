import os
import subprocess
import webbrowser
from datetime import datetime, timezone

import click

from dispatch.config import REPOS, GCP_PROJECT, GCP_ZONE, MANAGER_URL, API_TOKEN
from cli.api_client import CMClient


def get_client() -> CMClient:
    return CMClient(MANAGER_URL, API_TOKEN)


def short_id(task_id) -> str:
    return str(task_id)[:8]


def time_ago(dt_str) -> str:
    if not dt_str:
        return ""
    if isinstance(dt_str, str):
        dt = datetime.fromisoformat(dt_str.replace("Z", "+00:00"))
    else:
        dt = dt_str
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
    if name in REPOS:
        return REPOS[name]
    if name.startswith("http") or name.startswith("git@"):
        return name
    if "/" in name:
        return f"https://github.com/{name}.git"
    raise click.BadParameter(
        f"Unknown repo '{name}'. Known repos: {', '.join(REPOS.keys())}",
        param_hint="--repo",
    )


def find_task(client: CMClient, task_id: str) -> dict | None:
    """Find a task by ID prefix."""
    tasks = client.list_tasks()
    return next((t for t in tasks if t["id"].startswith(task_id)), None)


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
    client = get_client()
    task = client.add_task(repo_url, branch, prompt, priority)
    click.echo(f"Added task {short_id(task['id'])} to backlog")
    click.echo(f"  Repo:   {repo_url}")
    click.echo(f"  Prompt: {prompt[:80]}{'...' if len(prompt) > 80 else ''}")


@cli.command()
def backlog():
    """List backlog tasks."""
    client = get_client()
    tasks = client.list_tasks(status="backlog")
    if not tasks:
        click.echo("Backlog is empty.")
        return
    click.echo(f"  {'#':<10} {'Pri':<5} {'Task':<50} {'Repo':<20} {'Added'}")
    click.echo(f"  {'─'*10} {'─'*5} {'─'*50} {'─'*20} {'─'*8}")
    for t in tasks:
        prompt_short = t["prompt"][:47] + "..." if len(t["prompt"]) > 50 else t["prompt"]
        repo_short = t["repo_url"].split("/")[-1].replace(".git", "")
        click.echo(
            f"  {short_id(t['id']):<10} {t['priority']:<5} "
            f"{prompt_short:<50} {repo_short:<20} {time_ago(t['created_at'])}"
        )


@cli.command()
def queue():
    """List blocked tasks (your work queue)."""
    client = get_client()
    tasks = client.list_tasks(status="blocked")
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


@cli.command("open")
@click.argument("task_id")
@click.option("--browser", is_flag=True, help="Open in browser via ttyd instead of SSH")
def open_cmd(task_id, browser):
    """Open a task's Claude session (SSH into tmux by default)."""
    client = get_client()
    task = find_task(client, task_id)
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
        zone = task.get("worker_zone") or GCP_ZONE
        click.echo(f"Attaching to {vm}...")
        os.execvp("gcloud", [
            "gcloud", "compute", "ssh", vm,
            f"--zone={zone}", f"--project={GCP_PROJECT}",
            "--", "-t",
            "TERM=xterm-256color sudo su - worker -c 'tmux attach -t claude'",
        ])


@cli.command()
@click.argument("task_id")
def done(task_id):
    """Mark a task as done and shut down its worker VM."""
    client = get_client()
    task = find_task(client, task_id)
    if not task:
        click.echo(f"No task found matching '{task_id}'")
        return
    client.update_task(task["id"], status="done")
    click.echo(f"Task {short_id(task['id'])} marked done (VM will be deleted)")


@cli.command()
@click.argument("task_id")
def cancel(task_id):
    """Cancel a task and shut down its worker VM."""
    client = get_client()
    task = find_task(client, task_id)
    if not task:
        click.echo(f"No task found matching '{task_id}'")
        return
    client.delete_task(task["id"])
    click.echo(f"Task {short_id(task['id'])} cancelled")


@cli.command()
def status():
    """Show summary of all tasks."""
    client = get_client()
    tasks = client.list_tasks()
    counts = {"backlog": 0, "running": 0, "blocked": 0, "done": 0}
    for t in tasks:
        counts[t["status"]] = counts.get(t["status"], 0) + 1
    click.echo(f"  Backlog: {counts['backlog']}")
    click.echo(f"  Running: {counts['running']}")
    click.echo(f"  Blocked: {counts['blocked']}")
    click.echo(f"  Done:    {counts['done']}")


@cli.command()
def workers():
    """List active worker VMs."""
    client = get_client()
    ws = client.list_workers()
    if not ws:
        click.echo("No active workers.")
        return
    click.echo(f"  {'VM':<25} {'Status':<10} {'Task':<40} {'ttyd'}")
    click.echo(f"  {'─'*25} {'─'*10} {'─'*40} {'─'*30}")
    for w in ws:
        click.echo(
            f"  {w['worker_vm']:<25} {w['status']:<10} "
            f"{w['prompt']:<40} {w['ttyd_url'] or 'pending'}"
        )


@cli.command()
@click.argument("task_id")
@click.option("--lines", "-n", default=50, help="Number of lines to show")
def logs(task_id, lines):
    """Show worker VM startup/watcher logs."""
    client = get_client()
    task = find_task(client, task_id)
    if not task:
        click.echo(f"No task found matching '{task_id}'")
        return
    if not task["worker_vm"]:
        click.echo(f"Task {short_id(task['id'])} has no worker VM")
        return

    vm = task["worker_vm"]
    zone = task.get("worker_zone") or GCP_ZONE
    result = subprocess.run(
        ["gcloud", "compute", "ssh", vm,
         f"--zone={zone}", f"--project={GCP_PROJECT}",
         "--command", f"tail -n {lines} /var/log/cm-worker.log 2>/dev/null || echo 'No logs yet'"],
        capture_output=True, text=True, timeout=15,
    )
    click.echo(result.stdout.rstrip())


if __name__ == "__main__":
    cli()
