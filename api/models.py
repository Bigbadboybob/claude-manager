from datetime import datetime
from pydantic import BaseModel, Field


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
    initiative_id: str | None = None
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
    initiative_id: str | None = None
    # Free-form JSONB bag. PATCH replaces the whole object — callers that
    # want to merge should read first and re-send the merged dict.
    metadata: dict | None = None


class BacktestPhaseUpdate(BaseModel):
    """Live pipeline-phase heartbeat POSTed by a backtest worker mid-run.

    The worker's ``backtest_startup.sh`` relays a marker file the PT pipeline writes
    (``phase_report.py``) into ``POST /tasks/{id}/backtest-phase``, which merges these
    fields into ``metadata.backtest`` server-side (no clobber of run_key/launched_at/…)
    and stamps ``phase_updated_at``. The portal mirrors them onto the backtest_runs row so
    the ``/backtests`` panel can show ``Setup - download 40%`` during the pre-replay download
    instead of a blank Progress/ETA.

    ``phase`` is setup|replay|finalize; ``phase_step`` is the sub-step (e.g. download_events);
    ``phase_progress`` is a 0..1 fraction for the active phase; ``emitted_at`` is the worker's
    own clock for the marker (stored as-is; the server-authoritative freshness stamp is
    ``phase_updated_at``, set on write).
    """
    phase: str
    phase_step: str | None = None
    phase_progress: float | None = None
    phase_started_at: datetime | None = None
    phase_detail: str | None = None
    emitted_at: datetime | None = None


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
    initiative_id: str | None = None
    initiative: dict | None = None
    # Free-form JSONB bag. Reads come back as a dict (or None).
    metadata: dict | None = None

    class Config:
        from_attributes = True


class TaskChange(BaseModel):
    """One entry of the incremental task feed: apply ``task`` on ``upsert``,
    drop the cached task on ``remove`` (deleted, archived, or filtered out)."""

    seq: int
    task_id: str
    op: str  # "upsert" | "remove"
    task: TaskResponse | None = None


class TaskChangesResponse(BaseModel):
    epoch: str
    cursor: int
    reset: bool
    # Full consistent snapshot; present only when ``reset`` is true.
    tasks: list[TaskResponse] | None = None
    changes: list[TaskChange] = []
    more: bool = False


class InitiativeCreate(BaseModel):
    """Create a draft initiative around an existing coordinator task."""

    slug: str | None = None
    name: str
    description: str = ""
    color: str | None = None
    coordinator_task_id: str
    coordinator_project: str | None = None
    docs_path: str = "cm-initiative"
    shared_channel: str | None = None
    metadata: dict | None = None


class InitiativeUpdate(BaseModel):
    name: str | None = None
    description: str | None = None
    color: str | None = None
    status: str | None = None
    coordinator_task_id: str | None = None
    coordinator_project: str | None = None
    docs_path: str | None = None
    shared_channel: str | None = None
    metadata: dict | None = None
    reason: str | None = None


class InitiativeProjectCreate(BaseModel):
    project: str
    role: str = ""
    project_channel: str | None = None


class InitiativeProjectUpdate(BaseModel):
    status: str | None = None
    role: str | None = None
    project_channel: str | None = None
    reason: str | None = None


class InitiativeResponse(BaseModel):
    id: str
    slug: str
    name: str
    description: str
    status: str
    color: str | None = None
    coordinator_task_id: str
    coordinator_project: str | None = None
    docs_path: str
    shared_channel: str | None = None
    approved_at: datetime | None = None
    approved_by: str | None = None
    metadata: dict | None = None
    created_at: datetime
    updated_at: datetime
    projects: list[dict] = Field(default_factory=list)
    task_counts: dict[str, int] = Field(default_factory=dict)
