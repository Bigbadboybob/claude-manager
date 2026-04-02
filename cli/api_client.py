import httpx


class CMClient:
    def __init__(self, base_url: str, token: str):
        self.client = httpx.Client(
            base_url=base_url,
            headers={"Authorization": f"Bearer {token}"},
            timeout=30.0,
        )

    def add_task(self, repo_url: str, branch: str, prompt: str, priority: int = 0) -> dict:
        r = self.client.post("/tasks", json={
            "repo_url": repo_url, "repo_branch": branch,
            "prompt": prompt, "priority": priority,
        })
        r.raise_for_status()
        return r.json()

    def list_tasks(self, status: str | None = None) -> list[dict]:
        params = {"status": status} if status else {}
        r = self.client.get("/tasks", params=params)
        r.raise_for_status()
        return r.json()

    def get_task(self, task_id: str) -> dict:
        r = self.client.get(f"/tasks/{task_id}")
        r.raise_for_status()
        return r.json()

    def update_task(self, task_id: str, **fields) -> dict:
        r = self.client.patch(f"/tasks/{task_id}", json=fields)
        r.raise_for_status()
        return r.json()

    def delete_task(self, task_id: str):
        r = self.client.delete(f"/tasks/{task_id}")
        r.raise_for_status()
        return r.json()

    def list_workers(self) -> list[dict]:
        r = self.client.get("/workers")
        r.raise_for_status()
        return r.json()

    def health(self) -> dict:
        r = self.client.get("/health")
        r.raise_for_status()
        return r.json()
