"""Org model S2 endpoints (doc/org-model.md §4, §6, §10, §14).

Agents reach these only through their host daemon, which stamps the `actor`
(Owner is the operator caller, pid `owner`); the API trusts it under the
shared bearer token, as the items API does. Roster approval, adoption and
team writes are Owner-only in S2; a member's manager chain may also move it.
"""

from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException, Path, Query, Request
from pydantic import BaseModel, ConfigDict, Field

from api.auth import verify_token
from api.items_models import Actor
from dispatch import org_db
from dispatch.items_rules import ItemsError

router = APIRouter(prefix="/org", dependencies=[Depends(verify_token)])

Ref = Annotated[str, Path(min_length=1, max_length=200)]
Short = Annotated[str, Field(max_length=200)]
RequestId = Annotated[str, Field(min_length=1, max_length=200)]


class _Strict(BaseModel):
    model_config = ConfigDict(extra="forbid")


class Workspace(_Strict):
    daemon_id: Short
    workspace_id: Short
    path: Annotated[str, Field(max_length=1000)] | None = None


class RosterRow(_Strict):
    proposal_id: Short
    approve: bool = True
    handle: Short | None = None
    kind: Short | None = None
    title: Short | None = None
    team_id: Short | None = None
    manager: Short | None = None
    home_workspace: Workspace | None = None
    reason: Annotated[str, Field(max_length=500)] | None = None


class RosterApplyBody(_Strict):
    actor: Actor
    rows: list[RosterRow] = Field(min_length=1, max_length=org_db.MAX_ROWS)
    request_id: RequestId | None = None


class AdoptBody(_Strict):
    actor: Actor
    pid: Annotated[str, Field(min_length=1, max_length=300)]
    handle: Short
    title: Short | None = None
    kind: str = Field(default="agent", pattern=r"^(agent|continuous)$")
    team_id: Short | None = None
    manager: Short | None = None
    home_workspace: Workspace | None = None
    request_id: RequestId | None = None


class CloseAssistantBody(_Strict):
    actor: Actor
    pid: Annotated[str, Field(min_length=1, max_length=300)]
    member_id: Short
    reason: Annotated[str, Field(min_length=1, max_length=500)]
    # "kill_session": an agent already killed it under kill_session's own
    # scope; record it and hand its holds back without the close check.
    ended_by: str | None = Field(default=None, pattern=r"^kill_session$")
    request_id: RequestId | None = None


class PromoteBody(_Strict):
    actor: Actor
    pid: Annotated[str, Field(min_length=1, max_length=300)]
    handle: Short
    title: Short | None = None
    team: Short | None = None
    manager: Short | None = None
    reason: Annotated[str, Field(min_length=1, max_length=500)]
    # The owning daemon's verdict on the checkout (e.g. "shared_checkout"):
    # when set, the new member gets no home.
    home_refused: Short | None = None
    request_id: RequestId | None = None


class MoveBody(_Strict):
    actor: Actor
    manager: Short | None = None
    primary_team_id: Short | None = None
    reason: Annotated[str, Field(min_length=1, max_length=500)]
    expected_revision: int | None = None
    request_id: RequestId | None = None


class RetireBody(_Strict):
    actor: Actor
    reason: Annotated[str, Field(min_length=1, max_length=500)]
    request_id: RequestId | None = None


class AuthorityBody(_Strict):
    actor: Actor
    # Also the member's own main session (its assistant or worker is the
    # target, not the member's main itself).
    self_ok: bool = False
    # The session to control (a pid): it must be one of the member's.
    session: str | None = None


class ReinstateBody(_Strict):
    actor: Actor
    reason: Annotated[str, Field(min_length=1, max_length=500)] | None = None
    request_id: RequestId | None = None


class TeamCreateBody(_Strict):
    actor: Actor
    name: Short
    slug: Short | None = None
    kind: str = Field(default="team", pattern=r"^(team|initiative|division)$")
    parent_team_id: Short | None = None
    # A member reference: id, handle, `main/<handle>` or `owner`.
    lead: Short | None = None
    lead_member_id: Short | None = None
    charter: Annotated[str, Field(max_length=20000)] = ""
    standing_rules: list = Field(default_factory=list, max_length=100)
    coordinator_task_id: Short | None = None
    request_id: RequestId | None = None


class TeamPatchBody(_Strict):
    actor: Actor
    name: Short | None = None
    parent_team_id: Short | None = None
    lead: Short | None = None
    charter: Annotated[str, Field(max_length=20000)] | None = None
    standing_rules: list | None = Field(default=None, max_length=100)
    status: Short | None = None
    reason: Annotated[str, Field(max_length=500)] | None = None
    expected_revision: int | None = None
    request_id: RequestId | None = None


def _pool(request: Request):
    return request.app.state.pool


def _continuous_states(request: Request) -> list[dict]:
    reader = getattr(request.app.state, "read_continuous_states", None)
    return reader() if reader else []


async def _call(coro):
    try:
        return await coro
    except ItemsError as exc:
        raise HTTPException(status_code=exc.status, detail=exc.detail()) from None


@router.get("/snapshot")
async def snapshot(request: Request,
                   since_version: int | None = Query(None, ge=0, le=2**63 - 1)):
    return await _call(org_db.snapshot(_pool(request), since_version))


@router.get("/changes")
async def changes(request: Request, since: int = Query(0, ge=0, le=2**63 - 1),
                  limit: int = Query(500, ge=1, le=2000)):
    return await _call(org_db.changes(_pool(request), since, limit))


@router.get("/members/by-pid")
async def member_by_pid(request: Request,
                        pid: str = Query(..., min_length=1, max_length=300)):
    return await _call(org_db.member_for_pid(_pool(request), pid))


@router.get("/members/{ref:path}")
async def get_member(ref: Ref, request: Request):
    return await _call(org_db.get_member(_pool(request), ref))


@router.post("/members/{ref:path}/move")
async def move_member(ref: Ref, body: MoveBody, request: Request):
    fields = body.model_fields_set & {"manager", "primary_team_id"}
    return await _call(org_db.move(
        _pool(request), body.actor.model_dump(), ref, reason=body.reason,
        manager=body.manager, primary_team_id=body.primary_team_id, fields=fields,
        expected_revision=body.expected_revision, request_id=body.request_id))


@router.post("/members/{ref:path}/retire")
async def retire_member(ref: Ref, body: RetireBody, request: Request):
    return await _call(org_db.retire(
        _pool(request), body.actor.model_dump(), ref, reason=body.reason,
        request_id=body.request_id))


@router.post("/members/{ref:path}/authority")
async def member_authority(ref: Ref, body: AuthorityBody, request: Request):
    """Whether the actor may control this member's sessions: the check move
    and retire use (Owner, or a member up its manager chain; with `self_ok`
    also the member's own main session). 403 `not_member`/`not_manager`."""
    return await _call(org_db.authority(
        _pool(request), body.actor.model_dump(), ref, self_ok=body.self_ok, session=body.session))


@router.post("/members/{ref:path}/reinstate")
async def reinstate_member(ref: Ref, body: ReinstateBody, request: Request):
    return await _call(org_db.reinstate(
        _pool(request), body.actor.model_dump(), ref, reason=body.reason,
        request_id=body.request_id))


@router.post("/assistants/promote")
async def promote_assistant(body: PromoteBody, request: Request):
    """A member's assistant becomes a new member managed by it (§4.2)."""
    return await _call(org_db.promote(
        _pool(request), body.actor.model_dump(), pid=body.pid, handle=body.handle,
        title=body.title, team=body.team, manager=body.manager, reason=body.reason,
        home_refused=body.home_refused, request_id=body.request_id))


@router.post("/assistants/close")
async def close_assistant(body: CloseAssistantBody, request: Request):
    """Deliberately close a member's assistant (O5): authority and the
    holds hand-back; the session's daemon then closes it."""
    return await _call(org_db.close_assistant(
        _pool(request), body.actor.model_dump(), pid=body.pid, member_id=body.member_id,
        reason=body.reason, ended_by=body.ended_by, request_id=body.request_id))


@router.get("/teams/{ref}")
async def get_team(ref: Ref, request: Request):
    return await _call(org_db.get_team(_pool(request), ref))


@router.post("/teams")
async def create_team(body: TeamCreateBody, request: Request):
    return await _call(org_db.team_create(
        _pool(request), body.actor.model_dump(), name=body.name, slug=body.slug,
        kind=body.kind, parent_team_id=body.parent_team_id,
        lead=body.lead or body.lead_member_id, charter=body.charter,
        standing_rules=body.standing_rules, coordinator_task_id=body.coordinator_task_id,
        request_id=body.request_id))


@router.patch("/teams/{ref}")
async def patch_team(ref: Ref, body: TeamPatchBody, request: Request):
    changes = body.model_dump(exclude_unset=True)
    actor = changes.pop("actor")
    reason = changes.pop("reason", None)
    expected = changes.pop("expected_revision", None)
    request_id = changes.pop("request_id", None)
    return await _call(org_db.team_update(
        _pool(request), actor, ref, changes, reason=reason, expected_revision=expected,
        request_id=request_id))


@router.get("/roster")
async def roster(request: Request):
    return await _call(org_db.roster(_pool(request), _continuous_states(request)))


@router.post("/roster/apply")
async def roster_apply(body: RosterApplyBody, request: Request):
    rows = [row.model_dump(exclude_unset=True) for row in body.rows]
    for row in rows:
        if row.get("home_workspace") is not None:
            row["home_workspace"] = dict(row["home_workspace"])
    return await _call(org_db.roster_apply(
        _pool(request), body.actor.model_dump(), rows, body.request_id,
        _continuous_states(request)))


@router.post("/adopt")
async def adopt(body: AdoptBody, request: Request):
    return await _call(org_db.adopt(
        _pool(request), body.actor.model_dump(), pid=body.pid, handle=body.handle,
        title=body.title, kind=body.kind, team_id=body.team_id, manager=body.manager,
        home_workspace=body.home_workspace.model_dump() if body.home_workspace else None,
        request_id=body.request_id))


@router.get("/events")
async def events(request: Request, subject_type: str | None = Query(None, max_length=40),
                 subject_id: str | None = Query(None, max_length=200),
                 limit: int = Query(100, ge=1, le=1000)):
    return await _call(org_db.list_events(
        _pool(request), subject_type=subject_type, subject_id=subject_id, limit=limit))
