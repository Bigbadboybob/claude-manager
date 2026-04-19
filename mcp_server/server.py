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
