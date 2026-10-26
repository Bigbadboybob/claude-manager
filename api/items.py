"""Work items and boards (doc/items-board.md §6).

Agents reach these endpoints only through their host daemon, which stamps the
`actor`; the API trusts it under the shared bearer token.
"""

from fastapi import APIRouter, Depends, HTTPException, Path, Query, Request

from api.auth import verify_token
from api.items_models import (
    BoardPatchBody,
    EpicCloseApplyBody,
    EpicClosePlanBody,
    BoardJoinBody,
    BoardLeaveBody,
    EpicPocBody,
    EpicStaffBody,
    GroupCreateBody,
    GroupPatchBody,
    PlaceBody,
    UnplaceBody,
    GrantBody,
    GrantUpdateBody,
    ProfileBody,
    ReceiptBody,
    RefreshBody,
    ItemsChatTouchBody,
    HeartbeatBody,
    BoardResolveBody,
    ItemResolveBody,
    ItemsCreateBody,
    ItemsPatchBody,
    ItemsTouchBody,
)
import logging

from dispatch import epics, gate_db, items_db, org_db, placement
from dispatch.items_rules import ItemsError

router = APIRouter(dependencies=[Depends(verify_token)])
logger = logging.getLogger("cm.api.items")


def _pool(request: Request):
    return request.app.state.pool


async def _call(coro):
    try:
        return await coro
    except ItemsError as exc:
        raise HTTPException(status_code=exc.status, detail=exc.detail()) from None


@router.post("/boards/resolve")
async def resolve_board(body: BoardResolveBody, request: Request):
    if bool(body.task_id) == bool(body.ref):
        raise HTTPException(status_code=422, detail={
            "code": "invalid_field", "message": "pass exactly one of task_id or ref",
        })
    return await _call(items_db.resolve_board(
        _pool(request), task_id=body.task_id, ref=body.ref))


@router.get("/boards")
async def list_boards(request: Request, open_only: bool = Query(False)):
    return await _call(items_db.list_boards(_pool(request), open_only=open_only))


@router.get("/boards/{ref}")
async def read_board(
    ref: str, request: Request,
    since_version: int | None = Query(None, ge=0, le=2**63 - 1),
    archived: bool = Query(False),
    q: str | None = Query(None, max_length=200),
    history: int = Query(0, ge=0, le=items_db.MAX_HISTORY),
):
    return await _call(items_db.read_board(
        _pool(request), ref, since_version=since_version, archived=archived,
        q=q, history=history))


@router.patch("/boards/{ref}")
async def patch_board(ref: str, body: BoardPatchBody, request: Request):
    changes = body.model_dump(exclude_unset=True)
    actor = changes.pop("actor")
    return await _call(items_db.patch_board(_pool(request), ref, actor, changes))


@router.delete("/boards/{ref}")
async def delete_board(ref: str, request: Request):
    return await _call(items_db.delete_board(_pool(request), ref))


@router.post("/boards/{ref}/items")
async def create_items(ref: str, body: ItemsCreateBody, request: Request):
    specs = [spec.model_dump(exclude_unset=True) for spec in body.items]
    return await _call(items_db.create_items(
        _pool(request), ref, body.actor.model_dump(), specs, request_id=body.request_id))


@router.patch("/boards/{ref}/items")
async def update_items(ref: str, body: ItemsPatchBody, request: Request):
    fields = body.set.model_dump(exclude_unset=True)
    write = items_db.update_items_each if body.each else items_db.update_items
    return await _call(write(
        _pool(request), ref, body.actor.model_dump(), body.ns, fields,
        add_holders=[h.model_dump() for h in body.add_holders or []],
        remove_holders=body.remove_holders, reason=body.reason,
        reopen=body.reopen, expect_status=body.expect_status,
        owner=body.owner.model_dump() if body.owner else None, role=body.role))


@router.post("/boards/{ref}/items/touch")
async def touch_items(ref: str, body: ItemsTouchBody, request: Request):
    return await _call(items_db.touch_items(
        _pool(request), ref, body.actor.model_dump(), body.ns, source=body.source,
        message_id=body.message_id, excerpt=body.excerpt))


@router.post("/items/chat-touch")
async def chat_touch(body: ItemsChatTouchBody, request: Request):
    return await _call(items_db.chat_touch(
        _pool(request), body.actor.model_dump(), body.ns, channel=body.channel,
        source=body.source, message_id=body.message_id, excerpt=body.excerpt,
        suggest=body.suggest.model_dump() if body.suggest else None))


@router.post("/boards/{ref}/items/{n}/resolve")
async def resolve_item(ref: str, body: ItemResolveBody, request: Request,
                       n: int = Path(ge=1, le=2**31 - 1)):
    return await _call(items_db.resolve_item(
        _pool(request), ref, body.actor.model_dump(), n, body.action,
        kind=body.kind,
        holders=[h.model_dump() for h in body.holders] if body.holders is not None else None,
        blocked_by=body.blocked_by, blocked_on=body.blocked_on,
        check_back=body.check_back, reason=body.reason, message=body.message, to=body.to))


# ---- gate receipts (doc/gate-receipts.md §9) ---------------------------------

ItemN = Path(ge=1, le=2**31 - 1)


@router.post("/boards/{ref}/items/{n}/receipts")
async def post_receipt(ref: str, body: ReceiptBody, request: Request, n: int = ItemN):
    return await _call(gate_db.post_receipt(
        _pool(request), ref, body.actor.model_dump(), n, body.receipt,
        request_id=body.request_id))


@router.get("/boards/{ref}/items/{n}/receipts")
async def list_receipts(ref: str, request: Request, n: int = ItemN,
                        gate: str | None = Query(None, max_length=40),
                        history: bool = Query(False)):
    return await _call(gate_db.list_receipts(_pool(request), ref, n, gate=gate, history=history))


@router.post("/boards/{ref}/items/{n}/grants")
async def attach_grant(ref: str, body: GrantBody, request: Request, n: int = ItemN):
    fields = body.model_dump()
    actor = fields.pop("actor")
    return await _call(gate_db.attach_grant(_pool(request), ref, actor, n, fields))


@router.patch("/boards/{ref}/items/{n}/grants/{label}")
async def update_grant(ref: str, body: GrantUpdateBody, request: Request, n: int = ItemN,
                       label: str = Path(min_length=1, max_length=120)):
    return await _call(gate_db.update_grant(
        _pool(request), ref, body.actor.model_dump(), n, label, revoke=body.revoke,
        message_check=body.message_check))


@router.get("/boards/{ref}/gate-profiles")
async def get_gate_profile(ref: str, request: Request,
                           klass: str | None = Query(None, alias="class", max_length=40),
                           n: int | None = Query(None, ge=1, le=2**31 - 1)):
    return await _call(gate_db.get_profile(_pool(request), ref, klass=klass, n=n))


@router.put("/boards/{ref}/gate-profiles/{klass}")
async def put_gate_profile(ref: str, body: ProfileBody, request: Request,
                           klass: str = Path(min_length=1, max_length=40)):
    return await _call(gate_db.set_profile(
        _pool(request), ref, body.actor.model_dump(), klass, body.profile, reason=body.reason,
        default=body.default))


@router.post("/receipts/refresh")
async def refresh_receipts(body: RefreshBody, request: Request):
    return await _call(gate_db.refresh(_pool(request), body.daemon_id, body.checks))


@router.get("/items/owner-blocked")
async def owner_blocked(request: Request):
    return await _call(items_db.owner_blocked(_pool(request)))


@router.get("/items")
async def held_items(request: Request, holder_pid: str = Query(..., min_length=1, max_length=200),
                     open: bool = Query(True)):
    return await _call(items_db.held_items(_pool(request), holder_pid, open_only=open))


@router.post("/hosts/{daemon_id}/heartbeat")
async def heartbeat(body: HeartbeatBody, request: Request,
                    daemon_id: str = Path(min_length=1, max_length=200)):
    reply = await _call(items_db.heartbeat(
        _pool(request), daemon_id, host_label=body.host_label,
        sessions=body.sessions, exited=body.exited,
        acked_push_ids=body.acked_push_ids))
    # The org half (session facts, capabilities, home marks, the member
    # cache for ping). Best-effort: it never fails the items beat.
    try:
        reply["org"] = await org_db.heartbeat_extras(
            _pool(request), daemon_id, body.sessions, capabilities=body.capabilities,
            acked_mark_ids=body.acked_mark_ids)
    except Exception:  # noqa: BLE001
        logger.exception("org heartbeat extras failed for %s", daemon_id)
    return reply


# ---- org model S3: placement and groups (doc/org-model.md §7) ------------------

@router.post("/tasks/{task_id}/place")
async def place_task(task_id: str, body: PlaceBody, request: Request):
    return await _call(placement.place_task(
        _pool(request), task_id, body.board, body.actor.model_dump(), group=body.group,
        group_id=body.group_id, holder=body.holder.model_dump() if body.holder else None,
        role=body.role, replace=body.replace))


@router.post("/tasks/{task_id}/unplace")
async def unplace_task(task_id: str, body: UnplaceBody, request: Request):
    return await _call(placement.unplace_task(
        _pool(request), task_id, body.actor.model_dump(), reason=body.reason))


@router.post("/boards/{ref}/groups")
async def create_group(ref: str, body: GroupCreateBody, request: Request):
    return await _call(placement.create_group(
        _pool(request), ref, body.actor.model_dump(), name=body.name, kind=body.kind,
        parent_group_id=body.parent_group_id, description=body.description, goal=body.goal,
        ord=body.ord))


@router.patch("/boards/{ref}/groups/{group_id}")
async def set_group(ref: str, group_id: str, body: GroupPatchBody, request: Request):
    changes = body.model_dump(exclude_unset=True)
    actor = changes.pop("actor")
    expected = changes.pop("expected_revision", None)
    return await _call(placement.set_group(
        _pool(request), ref, actor, group_id, changes, expected_revision=expected))


@router.post("/boards/{ref}/members")
async def board_join(ref: str, body: BoardJoinBody, request: Request):
    """Add a session to the board (#28): spawn-time setup and set_poc."""
    from dispatch import board_members
    return await _call(board_members.join(
        _pool(request), ref, body.actor.model_dump(), pid=body.pid, name=body.name,
        group_id=body.group_id, group=body.group, reason=body.reason))


@router.post("/boards/{ref}/members/remove")
async def board_leave(ref: str, body: BoardLeaveBody, request: Request):
    """End a board membership: `board.leave` (self) or `board.remove_member`."""
    from dispatch import board_members
    return await _call(board_members.remove(
        _pool(request), ref, body.actor.model_dump(), pid=body.pid, editor=body.editor,
        reason=body.reason))


@router.get("/boards/{ref}/members")
async def board_member_list(ref: str, request: Request,
                            pid: str | None = Query(None, max_length=300)):
    from dispatch import board_members
    return await _call(board_members.members(_pool(request), ref, pid=pid))


@router.post("/boards/{ref}/epics/{group_id}/poc")
async def epic_set_poc(ref: str, group_id: str, body: EpicPocBody, request: Request):
    return await _call(epics.set_poc(
        _pool(request), ref, body.actor.model_dump(), group_id, body.member,
        team_id=body.team_id, request_id=body.request_id))


@router.post("/boards/{ref}/epics/{group_id}/staff")
async def epic_staff(ref: str, group_id: str, body: EpicStaffBody, request: Request):
    return await _call(epics.staff(
        _pool(request), ref, body.actor.model_dump(), group_id, body.member, body.as_,
        reason=body.reason, request_id=body.request_id))


@router.post("/boards/{ref}/epics/{group_id}/close-plan")
async def epic_close_plan(ref: str, group_id: str, body: EpicClosePlanBody, request: Request):
    return await _call(epics.close_plan(
        _pool(request), ref, body.actor.model_dump(), group_id, body.outcome))


@router.post("/boards/{ref}/close-plans/{plan_id}/apply")
async def epic_close_apply(ref: str, body: EpicCloseApplyBody, request: Request,
                           plan_id: int = Path(ge=1, le=2**62)):
    return await _call(epics.close_apply(
        _pool(request), ref, body.actor.model_dump(), plan_id, body.revision, body.choices))


@router.get("/items/on-boards")
async def on_boards(request: Request,
                    since_version: int | None = Query(None, ge=0, le=2**63 - 1)):
    return await _call(placement.on_boards(_pool(request), since_version=since_version))
