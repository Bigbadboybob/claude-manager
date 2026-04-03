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


def _get_project_path(cwd: str) -> str:
    """Convert a directory path to Claude's project path format."""
    return cwd.replace("/", "-").lstrip("-")


def _find_latest_session(cwd: str) -> tuple[str, str] | None:
    """Find the most recent Claude session for a directory. Returns (session_id, jsonl_path)."""
    from pathlib import Path
    project_path = _get_project_path(cwd)
    project_dir = Path.home() / ".claude" / "projects" / project_path
    if not project_dir.exists():
        return None
    jsonl_files = sorted(project_dir.glob("*.jsonl"), key=lambda p: p.stat().st_mtime, reverse=True)
    if not jsonl_files:
        return None
    session_id = jsonl_files[0].stem
    return session_id, str(jsonl_files[0])


GCS_BUCKET = "gs://cm-sessions-prediction-market-scalper"


@cli.command()
@click.option("--name", "-n", help="Task name/description")
@click.option("--repo", "-r", help="Repo name (auto-detected from git remote if omitted)")
@click.option("--session", "-s", help="Session ID (auto-detected from most recent if omitted)")
@click.option("--cwd", help="Working directory (defaults to current)")
def push(name, repo, session, cwd):
    """Push a local Claude session to the cloud."""
    cwd = cwd or os.getcwd()

    # Auto-detect repo from git remote
    if not repo:
        result = subprocess.run(
            ["git", "-C", cwd, "remote", "get-url", "origin"],
            capture_output=True, text=True,
        )
        if result.returncode != 0:
            click.echo("Could not detect git remote. Use --repo.")
            return
        repo_url = result.stdout.strip()
        # Normalize SSH URLs to HTTPS
        if repo_url.startswith("git@github.com:"):
            repo_url = repo_url.replace("git@github.com:", "https://github.com/")
    else:
        repo_url = resolve_repo(repo)

    # Auto-detect session
    if not session:
        found = _find_latest_session(cwd)
        if not found:
            click.echo(f"No Claude sessions found for {cwd}")
            return
        session_id, jsonl_path = found
    else:
        session_id = session
        project_path = _get_project_path(cwd)
        from pathlib import Path
        jsonl_path = str(Path.home() / ".claude" / "projects" / project_path / f"{session_id}.jsonl")

    # Auto-generate name from last user message if not provided
    if not name:
        name = f"Session {session_id[:8]} (pushed from local)"

    click.echo(f"Session: {session_id[:8]}")
    click.echo(f"Repo:    {repo_url}")

    # 1. Commit and push WIP
    click.echo("Committing WIP...")
    branch = f"cm/push-{session_id[:8]}"
    subprocess.run(["git", "-C", cwd, "checkout", "-b", branch], capture_output=True)
    subprocess.run(["git", "-C", cwd, "add", "-A"], capture_output=True)
    result = subprocess.run(
        ["git", "-C", cwd, "commit", "-m", f"WIP: {name}"],
        capture_output=True, text=True,
    )
    if "nothing to commit" in result.stdout:
        click.echo("  No uncommitted changes")
    else:
        click.echo(f"  Committed to {branch}")
    subprocess.run(
        ["git", "-C", cwd, "push", "-u", "origin", branch],
        capture_output=True, text=True,
    )
    click.echo(f"  Pushed branch {branch}")

    # 2. Upload session file to GCS
    click.echo("Uploading session...")
    gcs_path = f"{GCS_BUCKET}/{session_id}/{session_id}.jsonl"
    subprocess.run(
        ["gcloud", "storage", "cp", jsonl_path, gcs_path],
        capture_output=True,
    )
    # Upload subagent files if they exist
    from pathlib import Path
    subdir = Path(jsonl_path).parent / session_id
    if subdir.exists():
        subprocess.run(
            ["gcloud", "storage", "cp", "-r", str(subdir), f"{GCS_BUCKET}/{session_id}/"],
            capture_output=True,
        )
    click.echo(f"  Uploaded to {gcs_path}")

    # 3. Create task via API
    client = get_client()
    task = client.add_task(repo_url, branch, name, priority=0)
    # Update with session info
    client.update_task(task["id"], session_id=session_id, wip_branch=branch)

    click.echo(f"\nTask {short_id(task['id'])} created. Dispatch daemon will pick it up.")
    click.echo(f"  cm queue    — check when it's blocked")
    click.echo(f"  cm open {short_id(task['id'])}  — attach to the session")
    click.echo(f"  cm pull {short_id(task['id'])}  — pull it back locally")


@cli.command()
@click.argument("task_id")
@click.option("--cwd", help="Local repo directory (auto-detected from repo URL)")
def pull(task_id, cwd):
    """Pull a cloud Claude session back to local."""
    client = get_client()
    task = find_task(client, task_id)
    if not task:
        click.echo(f"No task found matching '{task_id}'")
        return

    session_id = task.get("session_id")
    if not session_id:
        click.echo(f"Task {short_id(task['id'])} has no session to pull")
        return

    # Auto-detect local repo dir from repo URL
    if not cwd:
        repo_name = task["repo_url"].split("/")[-1].replace(".git", "")
        # Check common locations
        for base in [os.path.expanduser("~/code/projects"), os.getcwd()]:
            candidate = os.path.join(base, repo_name)
            if os.path.isdir(candidate):
                cwd = candidate
                break
        if not cwd:
            click.echo(f"Could not find local repo for {repo_name}. Use --cwd.")
            return

    click.echo(f"Session: {session_id[:8]}")
    click.echo(f"Local:   {cwd}")

    # 1. Fetch and checkout the branch
    branch = task.get("wip_branch")
    if branch:
        click.echo(f"Checking out {branch}...")
        subprocess.run(["git", "-C", cwd, "fetch", "origin", branch], capture_output=True)
        subprocess.run(["git", "-C", cwd, "checkout", branch], capture_output=True)
        click.echo(f"  On branch {branch}")

    # 2. Download session from GCS (or from VM if still running)
    from pathlib import Path
    project_path = _get_project_path(cwd)
    local_project_dir = Path.home() / ".claude" / "projects" / project_path
    local_project_dir.mkdir(parents=True, exist_ok=True)
    local_jsonl = local_project_dir / f"{session_id}.jsonl"

    # Try GCS first
    click.echo("Downloading session...")
    gcs_path = f"{GCS_BUCKET}/{session_id}/{session_id}.jsonl"
    result = subprocess.run(
        ["gcloud", "storage", "cp", gcs_path, str(local_jsonl)],
        capture_output=True, text=True,
    )

    if result.returncode != 0:
        # Try downloading from the running VM via SSH
        if task.get("worker_vm"):
            click.echo("  Not in GCS, pulling from worker VM...")
            vm = task["worker_vm"]
            zone = task.get("worker_zone") or GCP_ZONE
            result = subprocess.run(
                ["gcloud", "compute", "ssh", vm,
                 f"--zone={zone}", f"--project={GCP_PROJECT}",
                 "--command",
                 f"sudo cat /home/worker/.claude/projects/-workspace/{session_id}.jsonl"],
                capture_output=True, text=True, timeout=15,
            )
            if result.returncode == 0:
                # Filter out SSH noise
                content = "\n".join(
                    l for l in result.stdout.splitlines()
                    if not l.startswith("Pseudo-terminal")
                )
                local_jsonl.write_text(content)
            else:
                click.echo(f"  Could not download session: {result.stderr}")
                return
    click.echo(f"  Saved to {local_jsonl}")

    # Download subagent files
    subprocess.run(
        ["gcloud", "storage", "cp", "-r",
         f"{GCS_BUCKET}/{session_id}/{session_id}/",
         str(local_project_dir / session_id) + "/"],
        capture_output=True,
    )

    # 3. Mark the cloud task as done if it's still running
    if task["status"] in ("running", "blocked"):
        click.echo("Marking cloud task as done...")
        client.update_task(task["id"], status="done")

    click.echo(f"\nSession pulled. Resume with:")
    click.echo(f"  cd {cwd}")
    click.echo(f"  claude --resume {session_id}")


@cli.group()
def warm():
    """Manage warm pool VMs."""
    pass


@warm.command("add")
@click.option("--repo", "-r", required=True, help="Repo name or URL")
@click.option("--branch", "-b", default="main", help="Branch")
@click.option("--size", "-s", default=1, help="Number of warm VMs to maintain")
def warm_add(repo, branch, size):
    """Add a warm pool for a repo."""
    repo_url = resolve_repo(repo)
    client = get_client()
    wp = client.create_warm_pool(repo_url, branch, size)
    click.echo(f"Warm pool created: {short_id(wp['id'])}")
    click.echo(f"  Repo: {repo_url}")
    click.echo(f"  Size: {size} VMs")
    click.echo(f"  Dispatch daemon will launch VMs within 30s")


@warm.command("list")
def warm_list():
    """List warm pools and their VMs."""
    client = get_client()
    pools = client.list_warm_pools()
    if not pools:
        click.echo("No warm pools configured.")
        return
    for wp in pools:
        repo_short = wp["repo_url"].split("/")[-1].replace(".git", "")
        click.echo(f"  Pool {short_id(wp['id'])} — {repo_short} (size={wp['pool_size']})")
        vms = wp.get("vms", [])
        if vms:
            for vm in vms:
                click.echo(f"    {vm['vm_name']:<25} {vm['status']:<8} {vm.get('external_ip', '')}")
        else:
            click.echo(f"    (no VMs yet — daemon will launch shortly)")


@warm.command("remove")
@click.argument("pool_id")
def warm_remove(pool_id):
    """Remove a warm pool and delete its VMs."""
    client = get_client()
    pools = client.list_warm_pools()
    pool = next((p for p in pools if p["id"].startswith(pool_id)), None)
    if not pool:
        click.echo(f"No warm pool matching '{pool_id}'")
        return
    client.delete_warm_pool(pool["id"])
    click.echo(f"Warm pool {short_id(pool['id'])} removed (VMs being deleted)")


@cli.command()
def config():
    """Show current configuration."""
    client = get_client()
    cfg = client.get_config()
    click.echo(f"  Max workers:     {cfg['max_workers']}")
    click.echo(f"  Zombie timeout:  {cfg['zombie_timeout_minutes']}m")


if __name__ == "__main__":
    cli()
