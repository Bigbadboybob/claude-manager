from datetime import datetime
from pydantic import BaseModel


class TaskCreate(BaseModel):
    repo_url: str
    repo_branch: str = "main"
    name: str | None = None
    prompt: str | None = None
    priority: int = 0
    status: str = "backlog"
    # Planning fields
    project: str | None = None
    slug: str | None = None
    description: str | None = None
    difficulty: int | None = None
    depends: list[str] | None = None
    source: str = "user"
    is_cloud: bool = False
    # Task kind — "oneshot" (default) or "continuous". Continuous rows are
    # excluded from the legacy GCP one-shot dispatcher.
    kind: str = "oneshot"
    # Subtask fields (Phase 5 of agent orchestration)
    parent_task_id: str | None = None
    worktree_mode: str = "inherit"
    # Optional initial wip_branch — set by `create_subtask` for both
    # modes so the API row matches the worktree on disk from the
    # first save (no UPDATE round-trip needed).
    wip_branch: str | None = None
    # Free-form JSONB bag for skill/agent attachments (e.g.
    # `metadata.resume.design_doc_path` for the design-doc bundle).
    metadata: dict | None = None


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
    kind: str | None = None
    # Subtask fields
    parent_task_id: str | None = None
    worktree_mode: str | None = None
    # Free-form JSONB bag. PATCH replaces the whole object — callers that
    # want to merge should read first and re-send the merged dict.
    metadata: dict | None = None


class ArtifactCreate(BaseModel):
    """Structured result artifact POSTed by a worker (cloud auto-backtest).

    `summary` carries the compact metrics dict — for backtests:
    {total_pnl, realized_pnl, fill_count, taker_pct, partial, grid_rows[],
    baseline_delta?, gcs_pointer} (shape enforced by the worker, not here).
    Bulk output lives in GCS under `gcs_prefix`.
    """
    kind: str = "backtest-result"
    summary: dict
    gcs_prefix: str | None = None
    partial: bool = False


class ArtifactResponse(BaseModel):
    id: str
    task_id: str
    kind: str
    summary: dict
    gcs_prefix: str | None
    partial: bool
    created_at: datetime


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
    kind: str = "oneshot"
    # Subtask fields (Phase 5)
    parent_task_id: str | None = None
    worktree_mode: str = "inherit"
    # Free-form JSONB bag. Reads come back as a dict (or None).
    metadata: dict | None = None

    class Config:
        from_attributes = True
