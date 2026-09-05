"""Thin client for proposing tasks from Claude instances in other repos."""

import os
import subprocess
from typing import Any

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


def _response_json(response) -> Any:
    """Decode a planning response and retain the API's structured error text."""
    if not response.is_success:
        detail = ""
        try:
            payload = response.json()
            if isinstance(payload, dict):
                detail = payload.get("detail") or payload.get("message") or ""
                if not detail and isinstance(payload.get("error"), str):
                    detail = payload["error"]
        except (TypeError, ValueError):
            detail = response.text.strip()
        if not isinstance(detail, str):
            detail = str(detail)
        if not detail:
            detail = response.reason_phrase or "response body was empty"
        raise RuntimeError(
            f"planning API returned {response.status_code}: {detail}"
        )
    return response.json()


class PlanningClient:
    """Client for Claude instances to propose tasks to the backlog."""

    def __init__(self, api_url: str | None = None, api_token: str | None = None):
        url = api_url or os.environ.get("CM_API_URL")
        if not url:
            raise RuntimeError(
                "CM_API_URL is not set — the MCP planning client needs the "
                "claude-manager API URL. The TUI normally injects this when "
                "it spawns MCP servers. To run the MCP server outside the "
                "TUI, set CM_API_URL=http://localhost:8000 (or the manager "
                "VM's IP) and CM_API_TOKEN=<your-token>."
            )
        token = api_token or os.environ.get("CM_API_TOKEN")
        if not token:
            raise RuntimeError(
                "CM_API_TOKEN is not set — the MCP planning client needs an "
                "auth token for the claude-manager API. The TUI normally "
                "injects this when it spawns MCP servers. To run the MCP "
                "server outside the TUI, set CM_API_TOKEN=<your-token> "
                "(and CM_API_URL=http://localhost:8000 or the manager VM's IP)."
            )
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
        metadata: dict | None = None,
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
        if metadata:
            body["metadata"] = metadata

        r = self._client.client.post("/tasks", json=body)
        return _response_json(r)

    def list_projects(self) -> list[dict]:
        """Return list of {name, repo_url} dicts."""
        r = self._client.client.get("/projects")
        return _response_json(r)

    def list_tasks(self, project: str | None = None,
                   status: str | None = None) -> list[dict]:
        params: dict = {}
        if project:
            params["project"] = project
        if status:
            params["status"] = status
        r = self._client.client.get("/tasks", params=params)
        return _response_json(r)

    def get_task(self, task_id: str) -> dict:
        r = self._client.client.get(f"/tasks/{task_id}")
        return _response_json(r)

    def update_task(self, task_id: str, **fields) -> dict:
        r = self._client.client.patch(f"/tasks/{task_id}", json=fields)
        return _response_json(r)

    def create_task(self, body: dict) -> dict:
        """Generic POST /tasks with a caller-assembled body.

        Used by `submit_backtest`, which needs fields propose_task's fixed
        body doesn't carry (kind, metadata, parent_task_id, status)."""
        r = self._client.client.post("/tasks", json=body)
        return _response_json(r)

    def get_task_artifacts(self, task_id: str) -> list[dict]:
        """Artifact rows for a task, newest first (cloud auto-backtest)."""
        r = self._client.client.get(f"/tasks/{task_id}/artifacts")
        return _response_json(r)
