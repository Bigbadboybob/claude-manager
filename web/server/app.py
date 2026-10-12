"""A small, authenticated Owner RPC adapter. Run one uvicorn worker."""
from contextlib import asynccontextmanager
import json
from typing import Annotated, Literal

from fastapi import FastAPI, HTTPException, Query, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel, ConfigDict, Field, StringConstraints
from starlette.exceptions import HTTPException as StarletteHTTPException

from .auth import AccessMiddleware, AccessVerifier
from .config import Settings
from .push import PushService
from .rpc import DaemonClient, DaemonError

Short = Annotated[str, StringConstraints(min_length=1, max_length=512)]
RequestID = Annotated[str, StringConstraints(min_length=1, max_length=128)]


class Model(BaseModel):
    model_config = ConfigDict(extra="forbid")


class Target(Model):
    conversation: Short | None = None
    channel: Short | None = None
    dm: Short | None = None


class Read(Target):
    inbox: bool = False
    dms: bool = False
    cursor: dict | None = None
    thread: Short | None = None
    mentions_only: bool = False
    unread_only: bool = False
    newest_first: bool = True
    mark_read: bool = True
    limit: int = Field(default=50, ge=1, le=200)


class Send(Target):
    body: str = Field(min_length=1, max_length=3000)
    request_id: RequestID
    reply_to: Short | None = None
    mentions: list[Short] = Field(default_factory=list, max_length=100)
    mention_here: bool = False


class Mark(Target):
    through: Short | None = None
    all: bool = False
    mentions: bool = False


class React(Model):
    message_id: Short
    emoji: Literal["✅", "👀", "👍", "❌", "🎉"]
    remove: bool = False
    request_id: RequestID


class Join(Model):
    conversation: Short
    request_id: RequestID


class Seen(Model):
    key: Short
    alert_id: Short


class Availability(Model):
    level: Literal["away", "around", "focused", "on-call", "unset"]
    request_id: RequestID
    note: str = Field(default="", max_length=200)


class SubscriptionKeys(Model):
    p256dh: str = Field(max_length=256)
    auth: str = Field(max_length=256)


class Subscription(Model):
    endpoint: str = Field(max_length=2048)
    keys: SubscriptionKeys
    expirationTime: float | None = None


class Endpoint(Model):
    endpoint: str = Field(max_length=2048)


class TestPush(Model):
    endpoint: str | None = Field(default=None, max_length=2048)


def params(model):
    return model.model_dump(exclude_none=True)


def target_count(data):
    return sum(bool(data.get(key)) for key in ("conversation", "channel", "dm", "inbox", "dms"))


def directory_params(query, cursor, limit):
    result = {"limit": limit}
    if query:
        result["query"] = query
    if cursor:
        try:
            parsed = json.loads(cursor)
            if not isinstance(parsed, dict):
                raise ValueError()
        except ValueError:
            raise HTTPException(422, "Invalid directory cursor") from None
        result["cursor"] = parsed
    return result


def create_app(settings=None, *, daemon=None, verifier=None, push=None, workers=True):
    settings = settings or Settings.from_env()
    daemon = daemon or DaemonClient(settings)
    verifier = verifier or AccessVerifier(settings)

    @asynccontextmanager
    async def lifespan(app):
        if app.state.push is None:
            app.state.push = PushService(settings, daemon)
        if workers:
            await app.state.push.start()
        try:
            yield
        finally:
            if workers:
                await app.state.push.close()
            await verifier.close()

    app = FastAPI(lifespan=lifespan, docs_url=None, redoc_url=None, openapi_url=None)
    app.state.push = push
    app.add_middleware(AccessMiddleware, verifier=verifier, origin=settings.origin)

    @app.exception_handler(DaemonError)
    async def daemon_error(_request, exc):
        status = (403 if exc.code == "unauthorized" else 409 if "conflict" in exc.code
                  or exc.code in {"join_required", "name_required"} else 422
                  if exc.code in {"invalid_params", "invalid_target", "invalid_cursor", "invalid_reaction", "not_found"}
                  else 503)
        return JSONResponse({"error": {"code": exc.code, "message": exc.message}}, status_code=status)

    @app.exception_handler(RequestValidationError)
    async def invalid_input(_request, _exc):
        # Do not echo arbitrary request bodies or push credentials in error data.
        return JSONResponse({"error": {"code": "invalid_params", "message": "Invalid board request fields."}}, status_code=422)

    @app.exception_handler(OSError)
    async def storage_error(_request, _exc):
        return JSONResponse({"error": {"code": "storage_unavailable", "message": "Board storage is unavailable."}}, status_code=503)

    @app.exception_handler(StarletteHTTPException)
    async def http_error(_request, exc):
        return JSONResponse({"error": {"code": "invalid_params", "message": str(exc.detail)}}, status_code=exc.status_code)

    @app.get("/healthz")
    async def health():
        return {"status": "ok"}

    @app.get("/api/bootstrap")
    async def bootstrap(request: Request):
        availability = await daemon.call("messaging.availability", {"action": "get"})
        return {"actor": "owner", "user": {"email": request.state.identity["email"]},
                "features": {"alerts_seen": True, "push": True}, **availability}

    @app.get("/api/channels")
    async def channels(query: str = Query(default="", max_length=200),
                       cursor: str | None = Query(default=None, max_length=8192),
                       limit: int = Query(default=100, ge=1, le=200)):
        return await daemon.call("messaging.channels", {"action": "list", **directory_params(query, cursor, limit)})

    @app.get("/api/dms")
    async def dms(cursor: str | None = Query(default=None, max_length=8192),
                  limit: int = Query(default=100, ge=1, le=200)):
        return await daemon.call("messaging.dms", directory_params(None, cursor, limit))

    @app.get("/api/people")
    async def people(query: str = Query(default="", max_length=200),
                     cursor: str | None = Query(default=None, max_length=8192),
                     limit: int = Query(default=100, ge=1, le=200)):
        return await daemon.call("messaging.people", directory_params(query, cursor, limit))

    @app.get("/api/counts")
    async def counts():
        return await daemon.call("messaging.counts", {})

    @app.post("/api/read")
    async def read(body: Read):
        data = params(body)
        if target_count(data) != 1:
            raise HTTPException(422, "Choose one conversation or inbox target")
        return await daemon.call("messaging.read", data)

    @app.post("/api/send")
    async def send(body: Send):
        data = params(body)
        if target_count(data) != 1:
            raise HTTPException(422, "Choose one conversation target")
        return await daemon.call("messaging.send", data)

    @app.post("/api/channels/join")
    async def join(body: Join):
        return await daemon.call("messaging.channels", {"action": "join", **params(body)})

    @app.post("/api/mark-read")
    async def mark(body: Mark):
        data = params(body)
        n = target_count(data)
        if n > 1 or body.all and (n or body.mentions or body.through) or not (n or body.all or body.mentions):
            raise HTTPException(422, "Choose one conversation, all, or mentions")
        return await daemon.call("messaging.mark_read", data)

    @app.post("/api/react")
    async def react(body: React):
        return await daemon.call("messaging.react", params(body))

    @app.get("/api/alerts")
    async def alerts():
        return await daemon.call("owner_attention.list", {})

    @app.post("/api/alerts/seen")
    async def seen(body: Seen):
        return await daemon.call("owner_attention.ack", {"session_uid": body.key, "alert_id": body.alert_id})

    @app.get("/api/availability")
    async def availability_get():
        return await daemon.call("messaging.availability", {"action": "get"})

    @app.post("/api/availability")
    async def availability_set(body: Availability):
        return await daemon.call("messaging.availability", {"action": "set", "source": "cli", **params(body)})

    @app.get("/api/push/vapid-public-key")
    async def public_key():
        return {"public_key": app.state.push.store.public_key, "enabled": True}

    @app.post("/api/push/subscribe")
    async def subscribe(body: Subscription):
        try:
            app.state.push.store.subscribe(params(body))
        except ValueError as exc:
            raise HTTPException(422, str(exc)) from exc
        return {"subscribed": True}

    @app.delete("/api/push/subscribe")
    async def unsubscribe(body: Endpoint):
        return {"removed": app.state.push.store.unsubscribe(body.endpoint)}

    @app.post("/api/push/test")
    async def test_push(body: TestPush):
        try:
            await app.state.push.test(body.endpoint)
        except ValueError as exc:
            raise HTTPException(422, str(exc)) from exc
        return {"accepted": True}

    app.mount("/", StaticFiles(directory=settings.static_path, html=True, check_dir=False), name="static")
    return app
