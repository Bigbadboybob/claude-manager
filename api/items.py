"""Work items and boards (doc/items-board.md §6).

Agents reach these endpoints only through their host daemon, which stamps the
`actor`; the API trusts it under the shared bearer token.
"""

from fastapi import APIRouter, Depends, HTTPException, Path, Query, Request

from api.auth import verify_token
from api.items_models import (
    BoardPatchBody,
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
from dispatch import gate_db, items_db
from dispatch.items_rules import ItemsError

router = APIRouter(dependencies=[Depends(verify_token)])


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
        remove_holders=body.remove_holders, reason=body.reason))


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
    return await _call(items_db.heartbeat(
        _pool(request), daemon_id, host_label=body.host_label,
        sessions=body.sessions, exited=body.exited,
        acked_push_ids=body.acked_push_ids))
