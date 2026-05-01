"""Thin client for proposing tasks from Claude instances in other repos."""

import os
import subprocess

from cli.api_client import CMClient


def _detect_repo_url() -> str:
    """Get the repo URL from git remote origin."""
    result = subprocess.run(
        ["git", "remote", "get-url", "origin"],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        raise RuntimeError("Could not detect repo URL from git remote")
    return result.stdout.strip()


class PlanningClient:
    """Client for Claude instances to propose tasks to the backlog."""

    def __init__(self, api_url: str | None = None, api_token: str | None = None):
        url = api_url or os.environ["CM_API_URL"]
        token = api_token or os.environ["CM_API_TOKEN"]
        self._client = CMClient(url, token)

    def propose_task(
        self,
        project: str,
        name: str,
        description: str = "",
        prompt: str = "",
        repo_url: str | None = None,
        difficulty: int | None = None,
        depends: list[str] | None = None,
    ) -> dict:
        """Create a task with source='claude' in draft status."""
        if not repo_url:
            repo_url = _detect_repo_url()

        body = {
            "repo_url": repo_url,
            "repo_branch": "main",
            "name": name,
            "project": project,
            "description": description,
            "prompt": prompt or name,
            "source": "claude",
            "is_cloud": False,
            "priority": 0,
        }
        if difficulty is not None:
            body["difficulty"] = difficulty
        if depends:
            body["depends"] = depends

        r = self._client.client.post("/tasks", json=body)
        r.raise_for_status()
        return r.json()

    def list_projects(self) -> list[dict]:
        """Return list of {name, repo_url} dicts."""
        r = self._client.client.get("/projects")
        r.raise_for_status()
        return r.json()

    def list_tasks(self, project: str | None = None,
                   status: str | None = None) -> list[dict]:
        params: dict = {}
        if project:
            params["project"] = project
        if status:
            params["status"] = status
        r = self._client.client.get("/tasks", params=params)
        r.raise_for_status()
        return r.json()

    def get_task(self, task_id: str) -> dict:
        r = self._client.client.get(f"/tasks/{task_id}")
        r.raise_for_status()
        return r.json()

    def update_task(self, task_id: str, **fields) -> dict:
        r = self._client.client.patch(f"/tasks/{task_id}", json=fields)
        r.raise_for_status()
        return r.json()
