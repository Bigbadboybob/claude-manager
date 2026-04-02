from datetime import datetime
from pydantic import BaseModel


class TaskCreate(BaseModel):
    repo_url: str
    repo_branch: str = "main"
    prompt: str
    priority: int = 0


class TaskUpdate(BaseModel):
    status: str | None = None
    priority: int | None = None
    prompt: str | None = None
    repo_branch: str | None = None
    worker_vm: str | None = None
    worker_zone: str | None = None
    ttyd_url: str | None = None
    blocked_at: datetime | None = None
    session_id: str | None = None
    wip_branch: str | None = None


class TaskResponse(BaseModel):
    id: str
    created_at: datetime
    updated_at: datetime
    repo_url: str
    repo_branch: str
    prompt: str
    status: str
    priority: int
    worker_vm: str | None
    worker_zone: str | None
    ttyd_url: str | None
    blocked_at: datetime | None
    session_id: str | None
    wip_branch: str | None
    resume_metadata: dict | None

    class Config:
        from_attributes = True
