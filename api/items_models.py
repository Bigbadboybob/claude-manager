"""Request bodies for the items API (doc/items-board.md §6).

Fields left out of a body are left unchanged; an explicit null clears.  The
rules in `dispatch.items_rules` do the semantic validation, so these models
only fix the wire shape.
"""

from pydantic import BaseModel, ConfigDict, Field


class _Strict(BaseModel):
    model_config = ConfigDict(extra="forbid")


class Actor(_Strict):
    pid: str = Field(min_length=1, max_length=200)
    name: str | None = None
    session_uid: str | None = None
    daemon_id: str | None = None
    task_id: str | None = None


class Holder(_Strict):
    pid: str = Field(min_length=1, max_length=200)
    name: str | None = None
    session_uid: str | None = None
    daemon_id: str | None = None


class ItemFields(_Strict):
    title: str | None = None
    status: str | None = None
    note: str | None = None
    group: str | None = None
    holders: list[Holder] | None = None
    blocked_by: list[int] | None = None
    blocked_on: str | None = None
    check_back: str | None = None
    eta: str | None = None
    links: list[str] | None = None


class ItemSpec(ItemFields):
    title: str


class BoardResolveBody(_Strict):
    task_id: str | None = None
    ref: str | None = None


class BoardPatchBody(_Strict):
    actor: Actor
    name: str | None = None
    orchestrator_pid: str | None = None
    idle_s: int | None = None
    stale_s: int | None = None
    unassigned_s: int | None = None
    repush_s: int | None = None
    escalate_s: int | None = None
    digest_s: int | None = None
    holder_idle_enabled: bool | None = None


class ItemsCreateBody(_Strict):
    actor: Actor
    items: list[ItemSpec] = Field(min_length=1, max_length=50)


class ItemsPatchBody(_Strict):
    actor: Actor
    ns: list[int] = Field(min_length=1, max_length=50)
    set: ItemFields = Field(default_factory=ItemFields)
    add_holders: list[Holder] | None = None
    remove_holders: list[str] | None = None
    reason: str | None = None


class ItemResolveBody(_Strict):
    actor: Actor
    action: str
    kind: str | None = None
    holders: list[Holder] | None = None
    blocked_by: list[int] | None = None
    blocked_on: str | None = None
    check_back: str | None = None
    reason: str | None = None
    message: str | None = None
