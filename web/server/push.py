"""One Owner's devices and bounded, durable Web Push retries. No chat-store reads."""
import asyncio
import base64
import copy
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
import hashlib
import json
import logging
import math
import os
from pathlib import Path
import tempfile
import time
from urllib.parse import urlsplit
import uuid

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ec
from pywebpush import webpush, WebPushException
import requests

log = logging.getLogger(__name__)
MAX_DEVICES = 32
MAX_JOBS = 2048
TTL = 3600


class PushSession(requests.Session):
    def request(self, method, url, **kwargs):
        valid_endpoint(url)
        kwargs["allow_redirects"] = False
        return super().request(method, url, **kwargs)


def b64(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def unb64(value):
    if not isinstance(value, str) or len(value) > 256:
        raise ValueError("Invalid subscription key")
    return base64.b64decode(value + "=" * (-len(value) % 4), altchars=b"-_", validate=True)


def valid_endpoint(endpoint):
    if not isinstance(endpoint, str) or len(endpoint) > 2048:
        raise ValueError("Invalid push endpoint")
    url = urlsplit(endpoint)
    host = url.hostname or ""
    # Prevent the privileged sender from becoming a URL-fetch proxy. These are
    # the push services used by Safari/iOS, Chromium, Firefox and Windows.
    supported = (host == "web.push.apple.com" or host.endswith(".push.apple.com")
                 or host == "fcm.googleapis.com" or host.endswith(".push.services.mozilla.com")
                 or host == "updates.push.services.mozilla.com" or host.endswith(".notify.windows.com"))
    if (url.scheme != "https" or url.username or url.password or url.fragment
            or url.port not in (None, 443) or not url.path or not supported):
        raise ValueError("Unsupported push service; use Safari, Chromium, Firefox or Windows Web Push")
    return endpoint


def subscription(value):
    endpoint = valid_endpoint(value.get("endpoint"))
    keys = value.get("keys", {})
    public, auth = unb64(keys.get("p256dh")), unb64(keys.get("auth"))
    if len(public) != 65 or len(auth) != 16:
        raise ValueError("Invalid subscription keys")
    ec.EllipticCurvePublicKey.from_encoded_point(ec.SECP256R1(), public)
    return {"endpoint": endpoint, "keys": {"p256dh": b64(public), "auth": b64(auth)}}


def atomic(path, data):
    fd, name = tempfile.mkstemp(prefix=".board-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(name, 0o600)
        os.replace(name, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        Path(name).unlink(missing_ok=True)


def retry_after(value):
    if not value:
        return None
    try:
        seconds = float(value)
    except (ValueError, TypeError):
        try:
            seconds = parsedate_to_datetime(value).timestamp() - time.time()
        except (ValueError, TypeError, OverflowError):
            return None
    return max(0, seconds) if math.isfinite(seconds) else None


def event_payload(event, badge):
    if not isinstance(event, dict):
        raise ValueError("Invalid event")
    for field, limit in (("id", 1024), ("title", 400), ("body", 560), ("url", 4096)):
        if not isinstance(event[field], str) or len(event[field]) > limit:
            raise ValueError("Invalid event field")
    if (not event["id"] or not event["url"].startswith("/") or event["url"].startswith("//")
            or "\\" in event["url"] or any(ord(c) < 32 or ord(c) == 127 for c in event["url"])):
        raise ValueError("Invalid notification link/identity")
    created = event["created_at"]
    if isinstance(created, bool) or not isinstance(created, (int, float)) or not math.isfinite(created):
        raise ValueError("Invalid event time")
    return {"title": event["title"][:100], "body": event["body"][:140], "url": event["url"],
            "tag": event["id"], "badge": badge, "expires_at": created + TTL}


class PushStore:
    def __init__(self, root):
        self.root = root
        root.mkdir(mode=0o700, parents=True, exist_ok=True)
        root.chmod(0o700)
        owner = root / "OWNER.md"
        if not owner.exists():
            owner.write_text(f"session: cm-board-web service\ncreated: {datetime.now(timezone.utc).date()}\n"
                             "purpose: Owner Web Push keys, subscriptions and retry state\n"
                             "keep: vapid.pem and state.json (private, persistent)\n"
                             "delete-after: explicit service retirement; never rotate keys during upgrades\n")
            owner.chmod(0o600)
        self.key_path = root / "vapid.pem"
        if not self.key_path.exists():
            key = ec.generate_private_key(ec.SECP256R1())
            atomic(self.key_path, key.private_bytes(serialization.Encoding.PEM,
                   serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
        self.key_path.chmod(0o600)
        key = serialization.load_pem_private_key(self.key_path.read_bytes(), password=None)
        if not isinstance(key, ec.EllipticCurvePrivateKey) or not isinstance(key.curve, ec.SECP256R1):
            raise ValueError("Existing VAPID key is not P-256; refusing to rotate it")
        self.public_key = b64(key.public_key().public_bytes(serialization.Encoding.X962,
                                                         serialization.PublicFormat.UncompressedPoint))
        self.path = root / "state.json"
        if self.path.exists():
            self.state = json.loads(self.path.read_text())
            if self.state.get("version") != 1:
                raise ValueError("Unknown Web Push state version")
            self.path.chmod(0o600)
        else:
            self.state = {"version": 1, "subscriptions": {}, "jobs": {}, "cursor": None, "badge": 0}
        self.state.setdefault("backoffs", {})
        self.durable = copy.deepcopy(self.state)
        if not self.path.exists():
            self.save()

    def save(self):
        try:
            atomic(self.path, json.dumps(self.state, separators=(",", ":")).encode())
        except OSError:
            self.state = copy.deepcopy(self.durable)
            raise
        self.durable = copy.deepcopy(self.state)

    def subscribe(self, data):
        data = subscription(data)
        devices = self.state["subscriptions"]
        if data["endpoint"] not in devices and len(devices) >= MAX_DEVICES:
            raise ValueError("Too many registered devices; remove a previous subscription")
        devices[data["endpoint"]] = data
        self.save()

    def unsubscribe(self, endpoint):
        removed = self.state["subscriptions"].pop(endpoint, None) is not None
        self.state["backoffs"].pop(endpoint, None)
        self.state["jobs"] = {k: j for k, j in self.state["jobs"].items() if j["endpoint"] != endpoint}
        if removed:
            self.save()
        return removed

    def enqueue(self, payloads, *, cursor=None, endpoint=None):
        now = time.time()
        jobs = {k: j for k, j in self.state["jobs"].items() if j["expires"] > now}
        devices = [endpoint] if endpoint else list(self.state["subscriptions"])
        if endpoint and endpoint not in self.state["subscriptions"]:
            raise ValueError("Subscribe this device before testing push")
        for payload in payloads:
            payload = dict(payload)
            expires = min(payload.pop("expires_at", now + TTL), now + TTL)
            if expires <= now:
                continue
            if not payload["url"].startswith("/") or payload["url"].startswith("//"):
                raise ValueError("Push links must remain on the board origin")
            for device in devices:
                key = hashlib.sha256((device + "\0" + payload["tag"]).encode()).hexdigest()
                jobs.setdefault(key, {"endpoint": device, "payload": payload, "attempts": 0,
                                      "next_at": now, "expires": expires})
        if len(jobs) > MAX_JOBS:
            raise ValueError("Web Push retry queue is full; cursor was not advanced")
        self.state["jobs"] = jobs
        if cursor is not None:
            self.state["cursor"] = cursor
        self.save()


class PushService:
    def __init__(self, settings, daemon, store=None, sender=None):
        self.settings, self.daemon = settings, daemon
        self.store = store or PushStore(settings.push_path)
        self.sender = sender or self._send
        self.ready = asyncio.Event()
        self.tasks = []
        self.last_test = -float("inf")

    async def start(self):
        self.tasks = [asyncio.create_task(self.feed_loop()), asyncio.create_task(self.send_loop())]

    async def close(self):
        for task in self.tasks:
            task.cancel()
        await asyncio.gather(*self.tasks, return_exceptions=True)

    async def test(self, endpoint=None):
        if time.monotonic() - self.last_test < 30:
            raise ValueError("Wait 30 seconds before another test notification")
        if endpoint:
            valid_endpoint(endpoint)
        self.store.enqueue([{"title": "CM board", "body": "Web Push is connected to this board.",
                             "url": "/", "tag": "cm-test:" + str(uuid.uuid4()),
                             "badge": self.store.state["badge"]}], endpoint=endpoint)
        self.last_test = time.monotonic()
        self.ready.set()

    async def feed_loop(self):
        delay = 2
        while True:
            try:
                result = await self.daemon.call("owner.notifications.next",
                    {"cursor": self.store.state["cursor"], "wait_ms": 25000}, timeout=35, feed=True)
                events = result["events"]
                if result.get("lost"):
                    log.warning("Owner notification replay window was exceeded; historical pushes were lost")
                valid = []
                for event in events:
                    try:
                        valid.append(event_payload(event, self.store.state["badge"]))
                    except (KeyError, ValueError, TypeError, OverflowError):
                        log.warning("Skipped malformed Owner notification event")
                if valid:
                    try:
                        counts = await self.daemon.call("messaging.counts", {})
                        self.store.state["badge"] = counts["unread"]
                    except Exception:
                        log.warning("Unread badge refresh failed; retaining the previous badge count")
                    for payload in valid:
                        payload["badge"] = self.store.state["badge"]
                    self.store.enqueue(valid, cursor=result["cursor"])
                    self.ready.set()
                elif result["cursor"] != self.store.state["cursor"]:
                    self.store.state["cursor"] = result["cursor"]
                    self.store.save()
                delay = 2
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                # Endpoint tokens, message text and operator credentials never enter logs.
                log.warning("Owner notification feed unavailable (%s); retry in %ss", type(exc).__name__, delay)
                await asyncio.sleep(delay)
                delay = min(60, delay * 2)

    def _send(self, data, payload):
        try:
            with PushSession() as session:
                session.trust_env = False
                response = webpush(subscription_info=data, data=json.dumps(payload),
                                   vapid_private_key=str(self.store.key_path),
                                   vapid_claims={"sub": self.settings.origin},
                                   ttl=TTL, timeout=10, requests_session=session)
            return response.status_code, retry_after(response.headers.get("Retry-After"))
        except WebPushException as exc:
            if exc.response is None:
                return 503, None
            return exc.response.status_code, retry_after(exc.response.headers.get("Retry-After"))

    async def deliver_due(self):
        now = time.time()
        selected = {}
        changed = False
        for key, job in list(self.store.state["jobs"].items()):
            endpoint = job["endpoint"]
            if endpoint not in self.store.state["subscriptions"] or job["expires"] <= now:
                self.store.state["jobs"].pop(key, None)
                changed = True
            elif (job["next_at"] <= now and self.store.state["backoffs"].get(endpoint, {}).get("next_at", 0) <= now):
                selected.setdefault(endpoint, (key, job))
        slots = asyncio.Semaphore(4)

        async def send_one(endpoint, key, job):
            async with slots:
                device = self.store.state["subscriptions"].get(endpoint)
                if not device:
                    return key, 410, None
                try:
                    result = await asyncio.to_thread(self.sender, device, job["payload"])
                    status, after = result if isinstance(result, tuple) else (result, None)
                    return key, status, after
                except Exception:
                    return key, 503, None

        results = await asyncio.gather(*(send_one(endpoint, *job) for endpoint, job in selected.items()))
        for key, status, after in results:
            # Subscribe/unsubscribe/feed activity may run during network awaits.
            job = self.store.state["jobs"].get(key)
            if job is None:
                continue
            changed = True
            endpoint = job["endpoint"]
            if status in (404, 410):
                self.store.state["subscriptions"].pop(endpoint, None)
                self.store.state["backoffs"].pop(endpoint, None)
                self.store.state["jobs"] = {k: j for k, j in self.store.state["jobs"].items() if j["endpoint"] != endpoint}
            elif 200 <= status < 300 or status in (400, 403, 413):
                self.store.state["jobs"].pop(key, None)
                self.store.state["backoffs"].pop(endpoint, None)
                if status >= 400:
                    log.warning("Push delivery returned permanent failure %s; discarded notification", status)
            else:
                attempts = self.store.state["backoffs"].get(endpoint, {}).get("attempts", 0) + 1
                delay = min(900, 10 * (2 ** min(attempts - 1, 7)))
                if after is not None:
                    delay = max(delay, after)
                next_at = time.time() + delay
                self.store.state["backoffs"][endpoint] = {"attempts": attempts, "next_at": next_at}
                job["attempts"], job["next_at"] = attempts, next_at
                log.warning("Push delivery returned %s; endpoint queued for retry", status)
        if changed:
            self.store.save()

    async def send_loop(self):
        while True:
            try:
                self.ready.clear()
                await self.deliver_due()
                due = min((min(j["expires"], max(j["next_at"], self.store.state["backoffs"].get(j["endpoint"], {}).get("next_at", 0)))
                           for j in self.store.state["jobs"].values()), default=time.time() + 3600)
                try:
                    await asyncio.wait_for(self.ready.wait(), timeout=max(1, due - time.time()))
                except TimeoutError:
                    pass
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                log.error("Push retry state unavailable (%s)", type(exc).__name__)
                await asyncio.sleep(30)
