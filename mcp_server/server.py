"""MCP server for Claude instances to propose tasks to the backlog."""

import sys
import os

# Add project root to path so cli.planning_client is importable
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from mcp.server.fastmcp import FastMCP

from cli.planning_client import PlanningClient

mcp = FastMCP("claude-manager")


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


if __name__ == "__main__":
    mcp.run()
