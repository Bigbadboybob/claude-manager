"""Request bodies for the items API (doc/items-board.md §6).

Fields left out of a body are left unchanged; an explicit null clears.  The
rules in `dispatch.items_rules` do the semantic validation, so these models
only fix the wire shape.
"""

from typing import Annotated

from pydantic import BaseModel, ConfigDict, Field

# Bounds keep oversized values a 422 rather than a driver error (int4 columns).
ItemNumber = Annotated[int, Field(ge=1, le=2**31 - 1)]
Short = Annotated[str, Field(max_length=200)]
Reason = Annotated[str, Field(max_length=500)]
When = Annotated[str, Field(max_length=64)]
Seconds = Annotated[int, Field(ge=1, le=30 * 24 * 3600)]


class _Strict(BaseModel):
    model_config = ConfigDict(extra="forbid")


class Actor(_Strict):
    pid: str = Field(min_length=1, max_length=200)
    name: Short | None = None
    session_uid: Short | None = None
    daemon_id: Short | None = None
    task_id: Short | None = None


class Holder(_Strict):
    pid: str = Field(min_length=1, max_length=200)
    name: Short | None = None
    session_uid: Short | None = None
    daemon_id: Short | None = None


class ItemFields(_Strict):
    title: str | None = None
    status: str | None = None
    note: str | None = None
    group: str | None = None
    holders: list[Holder] | None = Field(default=None, max_length=50)
    blocked_by: list[ItemNumber] | None = Field(default=None, max_length=50)
    blocked_on: str | None = None
    check_back: When | None = None
    eta: When | None = None
    links: list[str] | None = None


class ItemSpec(ItemFields):
    title: str


class BoardResolveBody(_Strict):
    task_id: Short | None = None
    ref: Short | None = None


class BoardPatchBody(_Strict):
    actor: Actor
    name: Short | None = None
    orchestrator_pid: Short | None = None
    idle_s: Seconds | None = None
    stale_s: Seconds | None = None
    unassigned_s: Seconds | None = None
    repush_s: Seconds | None = None
    escalate_s: Seconds | None = None
    digest_s: Seconds | None = None
    holder_idle_enabled: bool | None = None


class ItemsCreateBody(_Strict):
    actor: Actor
    items: list[ItemSpec] = Field(min_length=1, max_length=50)
    request_id: Short | None = None


class ItemsPatchBody(_Strict):
    actor: Actor
    ns: list[ItemNumber] = Field(min_length=1, max_length=50)
    set: ItemFields = Field(default_factory=ItemFields)
    add_holders: list[Holder] | None = Field(default=None, max_length=50)
    remove_holders: list[Short] | None = Field(default=None, max_length=50)
    reason: Reason | None = None


class ItemResolveBody(_Strict):
    actor: Actor
    action: str = Field(max_length=32)
    kind: str | None = Field(default=None, max_length=64)
    holders: list[Holder] | None = Field(default=None, max_length=50)
    blocked_by: list[ItemNumber] | None = Field(default=None, max_length=50)
    blocked_on: str | None = None
    check_back: When | None = None
    reason: Reason | None = None
    message: str | None = Field(default=None, max_length=1000)


class HeartbeatBody(_Strict):
    # Rows are validated one by one in dispatch.items_db.heartbeat: one bad
    # row is clipped or dropped, never the whole host's beat.
    host_label: str | None = None
    sessions: list[dict] = Field(default_factory=list, max_length=2000)
    exited: list[str] = Field(default_factory=list, max_length=2000)
    acked_push_ids: list[Annotated[int, Field(ge=1)]] = Field(default_factory=list, max_length=1000)
