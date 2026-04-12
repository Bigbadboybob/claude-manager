from datetime import datetime
from pydantic import BaseModel


class TaskCreate(BaseModel):
    repo_url: str
    repo_branch: str = "main"
    name: str | None = None
    prompt: str | None = None
    priority: int = 0
    # Planning fields
    project: str | None = None
    slug: str | None = None
    description: str | None = None
    difficulty: int | None = None
    depends: list[str] | None = None
    source: str = "user"
    is_cloud: bool = False


class TaskUpdate(BaseModel):
    status: str | None = None
    priority: int | None = None
    name: str | None = None
    prompt: str | None = None
    repo_branch: str | None = None
    worker_vm: str | None = None
    worker_zone: str | None = None
    ttyd_url: str | None = None
    blocked_at: datetime | None = None
    session_id: str | None = None
    wip_branch: str | None = None
    # Planning fields
    project: str | None = None
    slug: str | None = None
    description: str | None = None
    difficulty: int | None = None
    depends: list[str] | None = None
    source: str | None = None
    is_cloud: bool | None = None


class TaskResponse(BaseModel):
    id: str
    created_at: datetime
    updated_at: datetime
    repo_url: str
    repo_branch: str
    name: str | None
    prompt: str | None
    status: str
    priority: int
    worker_vm: str | None
    worker_zone: str | None
    ttyd_url: str | None
    blocked_at: datetime | None
    session_id: str | None
    wip_branch: str | None
    resume_metadata: dict | None
    # Planning fields
    project: str | None = None
    slug: str | None = None
    description: str | None = None
    difficulty: int | None = None
    depends: list[str] | None = None
    source: str = "user"
    is_cloud: bool = False

    class Config:
        from_attributes = True
