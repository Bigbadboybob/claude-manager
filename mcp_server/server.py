"""MCP server for Claude instances to propose tasks to the backlog."""

import json
import os
import sys
import time
import uuid
from pathlib import Path

# Add project root to path so cli.planning_client is importable
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from mcp.server.fastmcp import FastMCP

from cli.planning_client import PlanningClient

mcp = FastMCP("claude-manager")


def _workflow_run_dir() -> Path:
    """Return the directory for the current workflow run, creating it if needed.

    Raises RuntimeError if CM_WORKFLOW_RUN_ID is not set (tool called outside a workflow).
    """
    run_id = os.environ.get("CM_WORKFLOW_RUN_ID", "").strip()
    if not run_id:
        raise RuntimeError(
            "CM_WORKFLOW_RUN_ID is not set — workflow tools are only usable "
            "inside a workflow-participant session."
        )
    base = Path(os.path.expanduser("~/.cm/workflow-runs")) / run_id
    base.mkdir(parents=True, exist_ok=True)
    return base


def _append_event(tool: str, args: dict) -> dict:
    """Append an MCP tool-call event to the active workflow run's events.jsonl."""
    role = os.environ.get("CM_ROLE", "").strip() or "unknown"
    run_dir = _workflow_run_dir()
    event = {
        "id": uuid.uuid4().hex,
        "ts": time.time(),
        "run_id": os.environ["CM_WORKFLOW_RUN_ID"],
        "role": role,
        "tool": tool,
        "args": args,
    }
    # Append atomically: open with O_APPEND, write one line.
    events_path = run_dir / "events.jsonl"
    with events_path.open("a", encoding="utf-8") as f:
        f.write(json.dumps(event) + "\n")
        f.flush()
    return event


_BRIEF_FIELDS = (
    "id", "slug", "project", "name", "status", "source",
    "priority", "difficulty", "is_cloud",
)
_FULL_EXTRA_FIELDS = (
    "description", "prompt", "depends", "repo_url", "repo_branch",
    "wip_branch", "session_id", "ttyd_url", "worker_vm",
    "blocked_at", "created_at", "updated_at",
)


def _shape_task(task: dict, *, full: bool) -> dict:
    """Project an API task dict to a stable shape for MCP responses.

    `source` is always present — "user" for tasks created by the human owner
    in the TUI, "claude" for tasks proposed by an agent. Agents should check
    this to distinguish the two.
    """
    out = {k: task.get(k) for k in _BRIEF_FIELDS}
    if full:
        for k in _FULL_EXTRA_FIELDS:
            out[k] = task.get(k)
    return out


@mcp.tool()
def propose_task(
    project: str,
    name: str,
    description: str = "",
    prompt: str = "",
    difficulty: int | None = None,
    depends: list[str] | None = None,
) -> str:
    """Propose a new task to the project backlog.

    The task is created with source='claude' in draft status.
    The project owner will review and accept or reject it in the TUI.

    Args:
        project: Project name (use list_projects to see valid names)
        name: Short task title
        description: Detailed description of what needs to be done
        prompt: Instructions for the Claude instance that will work on this task
        difficulty: Optional difficulty rating (1-10)
        depends: Optional list of task slugs this depends on
    """
    client = PlanningClient()
    task = client.propose_task(
        project=project,
        name=name,
        description=description,
        prompt=prompt,
        difficulty=difficulty,
        depends=depends,
    )
    return f"Proposed task '{name}' in project '{project}' (id: {task['id']})"


@mcp.tool()
def list_projects() -> list[dict]:
    """List all available projects and their repo URLs.

    Use this to discover valid project names before calling propose_task.
    """
    client = PlanningClient()
    return client.list_projects()


@mcp.tool()
def list_tasks(
    project: str | None = None,
    status: str | None = None,
    source: str | None = None,
) -> list[dict]:
    """List tasks across the planning system.

    Returns BOTH human-created and agent-proposed tasks by default. Each
    entry exposes a `source` field:
      - "user"   — created by the human owner in the TUI
      - "claude" — proposed by a Claude agent via `propose_task`

    Inspect that field to tell them apart, and prefer leaving user tasks
    untouched unless the user explicitly asked you to modify them.

    Args:
        project: Optional project name filter.
        status: Optional status filter ("draft", "backlog", "running",
            "blocked", "done").
        source: Optional source filter ("user" or "claude"). Default returns both.
    """
    client = PlanningClient()
    tasks = client.list_tasks(project=project, status=status)
    if source:
        tasks = [t for t in tasks if t.get("source") == source]
    return [_shape_task(t, full=False) for t in tasks]


@mcp.tool()
def get_task(task_id: str) -> dict:
    """Get full details of a single task by its UUID.

    The returned object includes a `source` field — "user" for tasks
    created by the human owner, "claude" for agent-proposed tasks.

    Args:
        task_id: Task UUID (find one via `list_tasks`).
    """
    client = PlanningClient()
    return _shape_task(client.get_task(task_id), full=True)


@mcp.tool()
def update_task(
    task_id: str,
    name: str | None = None,
    description: str | None = None,
    prompt: str | None = None,
    status: str | None = None,
    priority: int | None = None,
    difficulty: int | None = None,
    project: str | None = None,
    depends: list[str] | None = None,
) -> dict:
    """Edit a task's planning fields.

    This tool can modify ANY task — including tasks created by the human
    owner (source="user"). That power cuts both ways: be conservative when
    touching user tasks. Don't rewrite their prompt, change scope, or move
    them between statuses unless the user explicitly asked you to. Agent-
    proposed tasks (source="claude") are fair game for self-revision.

    The returned object includes the `source` field so you can confirm
    what you just modified.

    Only the fields you pass (non-None) will be updated.

    Args:
        task_id: Task UUID.
        name: New title.
        description: New description.
        prompt: New launch prompt.
        status: "draft", "backlog", "running", "blocked", or "done".
        priority: Lower number = higher priority.
        difficulty: 1-10.
        project: Reassign to a different project.
        depends: Replace the dependency list (task slugs).
    """
    fields = {
        k: v for k, v in {
            "name": name,
            "description": description,
            "prompt": prompt,
            "status": status,
            "priority": priority,
            "difficulty": difficulty,
            "project": project,
            "depends": depends,
        }.items() if v is not None
    }
    if not fields:
        raise ValueError("No fields to update — pass at least one field.")
    client = PlanningClient()
    return _shape_task(client.update_task(task_id, **fields), full=True)


@mcp.tool()
def workflow_transition(to: str, prompt: str) -> str:
    """Hand control to another role in the current workflow.

    Use this to end your turn and activate a different role with a specific prompt.
    The TUI delivers `prompt` to that role's session when it next activates.

    Args:
        to: Name of the role to transition to (must be declared in the workflow).
        prompt: The message to send to that role when it activates.
    """
    event = _append_event("workflow_transition", {"to": to, "prompt": prompt})
    return f"Queued transition to '{to}' (event {event['id']})."


@mcp.tool()
def workflow_done(reason: str) -> str:
    """End the current workflow run.

    Use this when the workflow's goal is achieved and no further iteration is needed.
    All participant sessions remain open in the TUI but the workflow stops firing
    transitions.

    Args:
        reason: Short explanation of why the workflow is complete.
    """
    event = _append_event("workflow_done", {"reason": reason})
    return f"Workflow marked done (event {event['id']})."


if __name__ == "__main__":
    mcp.run()
