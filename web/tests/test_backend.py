import asyncio
import base64
import json
from pathlib import Path
import struct
import tempfile
import time
import unittest
from unittest.mock import patch

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ec, rsa
import httpx
import jwt

from web.server.app import create_app
from web.server.auth import AccessVerifier
from web.server.config import Settings
from web.server.push import PushStore, PushService, PushSession, b64, subscription
from web.server.rpc import DaemonClient, DaemonError


def settings(root):
    static = root / "static"
    static.mkdir(exist_ok=True)
    (static / "index.html").write_text("<p>board shell</p>")
    (static / "sw.js").write_text("// service worker")
    return Settings(team_domain="test.cloudflareaccess.com", audience="board-aud",
                    owner_email="owner@example.com", socket_path=root / "daemon.sock",
                    token_path=root / "operator-token", push_path=root / "push", static_path=static)


def device(name="one"):
    key = ec.generate_private_key(ec.SECP256R1()).public_key().public_bytes(
        serialization.Encoding.X962, serialization.PublicFormat.UncompressedPoint)
    return {"endpoint": f"https://web.push.apple.com/{name}", "expirationTime": None,
            "keys": {"p256dh": b64(key), "auth": b64(b"test-auth-secret")}}


class AuthTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        temp = tempfile.TemporaryDirectory(); self.addCleanup(temp.cleanup)
        self.cfg = settings(Path(temp.name))
        self.key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        public = jwt.algorithms.RSAAlgorithm.to_jwk(self.key.public_key(), as_dict=True)
        public.update(kid="good", use="sig")
        self.fetches = []
        def fetch(req):
            self.fetches.append(str(req.url))
            return httpx.Response(200, json={"keys": [public]})
        self.verifier = AccessVerifier(self.cfg, httpx.AsyncClient(transport=httpx.MockTransport(fetch)))
        self.addAsyncCleanup(self.verifier.close)

    def token(self, **overrides):
        now = int(time.time())
        claims = {"iss": "https://test.cloudflareaccess.com", "aud": ["board-aud"],
                  "sub": "owner-id", "email": "owner@example.com", "iat": now, "exp": now + 600}
        claims.update(overrides)
        return jwt.encode(claims, self.key, algorithm="RS256", headers={"kid": "good"})

    async def test_signed_owner_and_cached_keys(self):
        for _ in range(3):
            self.assertEqual((await self.verifier.verify(self.token()))["email"], self.cfg.owner_email)
        self.assertEqual(self.fetches, ["https://test.cloudflareaccess.com/cdn-cgi/access/certs"])

    async def test_reject_wrong_claims_unsigned_bad_signature_and_expiry(self):
        for claims in [{"aud": "another-app"}, {"iss": "https://evil.example"},
                       {"email": "someone@example.com"}, {"email": 42},
                       {"exp": int(time.time()) - 120}, {"iat": int(time.time()) + 3600},
                       {"nbf": int(time.time()) + 3600}]:
            with self.subTest(claims=claims), self.assertRaises((ValueError, jwt.PyJWTError)):
                await self.verifier.verify(self.token(**claims))
        for token in [None, "junk", jwt.encode({"email": self.cfg.owner_email}, "", algorithm="none"),
                      self.token()[:-10] + "AAAAAAAAAA"]:
            with self.subTest(token_type=str(token)[:10]), self.assertRaises((ValueError, jwt.PyJWTError)):
                await self.verifier.verify(token)

    async def test_unknown_kid_refresh_is_throttled(self):
        await self.verifier.verify(self.token())
        unknown = jwt.encode({"x": 1}, self.key, algorithm="RS256", headers={"kid": "unknown"})
        for _ in range(5):
            with self.assertRaises(ValueError):
                await self.verifier.verify(unknown)
        self.assertEqual(len(self.fetches), 1)

    async def test_key_rotation_and_expired_cache_fail_closed(self):
        await self.verifier.verify(self.token())
        self.key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        public = jwt.algorithms.RSAAlgorithm.to_jwk(self.key.public_key(), as_dict=True)
        public.update(kid="new", use="sig")
        async def fetch(_):
            return httpx.Response(200, json={"keys": [public]})
        self.verifier.client = httpx.AsyncClient(transport=httpx.MockTransport(fetch))
        self.addAsyncCleanup(self.verifier.client.aclose)
        self.verifier.last_refresh -= 61
        claims = jwt.decode(self.token(), options={"verify_signature": False})
        rotated = jwt.encode(claims, self.key, algorithm="RS256", headers={"kid": "new"})
        self.assertEqual((await self.verifier.verify(rotated))["sub"], "owner-id")
        self.verifier.expires = 0
        with self.assertRaises(ValueError):
            await self.verifier.verify(rotated)


class FakeVerifier:
    async def verify(self, token):
        if token != "signed-owner":
            raise ValueError("bad token")
        return {"email": "owner@example.com"}

    async def close(self):
        pass


class FakeDaemon:
    def __init__(self):
        self.calls = []

    async def call(self, method, params, **kwargs):
        self.calls.append((method, params))
        if method == "messaging.availability":
            return {"owner_availability": {"level": params.get("level", "focused"), "set": True}}
        if method == "messaging.counts":
            return {"conversations": {}, "unread": 4, "mentions": 3, "dms": 1}
        return {"items": [], "method": method, "params": params}


class ApiTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        temp = tempfile.TemporaryDirectory(); self.addCleanup(temp.cleanup)
        self.cfg = settings(Path(temp.name))
        self.daemon = FakeDaemon()
        self.push = PushService(self.cfg, self.daemon)
        app = create_app(self.cfg, daemon=self.daemon, verifier=FakeVerifier(), push=self.push, workers=False)
        self.client = httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url=self.cfg.origin)
        self.addAsyncCleanup(self.client.aclose)
        self.headers = {"Cf-Access-Jwt-Assertion": "signed-owner", "X-CM-Board": "1"}

    async def test_everything_except_health_requires_access(self):
        self.assertEqual((await self.client.get("/healthz")).status_code, 200)
        for path in ["/", "/sw.js", "/manifest.webmanifest", "/api/counts", "/api/push/vapid-public-key"]:
            self.assertEqual((await self.client.get(path)).status_code, 403, path)
        self.assertEqual(self.daemon.calls, [])
        shell = await self.client.get("/", headers=self.headers)
        self.assertEqual(shell.status_code, 200)
        self.assertIn("frame-ancestors 'none'", shell.headers["content-security-policy"])
        sw = await self.client.get("/sw.js", headers=self.headers)
        self.assertEqual(sw.headers["cache-control"], "no-cache")
        self.assertIn("javascript", sw.headers["content-type"])

    async def test_csrf_and_forged_actor_or_method_are_refused(self):
        send = {"conversation": "cid", "body": "Hello", "request_id": "same-id"}
        for headers in [{"Cf-Access-Jwt-Assertion": "signed-owner"},
                        {**self.headers, "Origin": "https://other.example"}]:
            self.assertEqual((await self.client.post("/api/send", headers=headers, json=send)).status_code, 403)
        for field in ["from", "actor", "method", "caller", "token_id", "_authenticated_actor"]:
            response = await self.client.post("/api/send", headers=self.headers, json={**send, field: "forged"})
            self.assertEqual(response.status_code, 422)
            self.assertNotIn("forged", response.text)
        self.assertEqual(self.daemon.calls, [])

    async def test_mapping_read_send_react_scoped_marks_alerts_and_availability(self):
        calls = [
            ("/api/read", {"conversation": "cid", "cursor": {"last_id": "old"}, "mark_read": False}, "messaging.read"),
            ("/api/send", {"conversation": "cid", "body": "Hello", "reply_to": "id", "request_id": "same"}, "messaging.send"),
            ("/api/react", {"message_id": "id", "emoji": "✅", "request_id": "same", "remove": True}, "messaging.react"),
            ("/api/mark-read", {"conversation": "cid", "mentions": True}, "messaging.mark_read"),
            ("/api/channels/join", {"conversation": "cid", "request_id": "join"}, "messaging.channels"),
            ("/api/alerts/seen", {"key": "origin-key", "alert_id": "versioned-id"}, "owner_attention.ack"),
            ("/api/availability", {"level": "around", "request_id": "available"}, "messaging.availability"),
        ]
        for path, body, method in calls:
            response = await self.client.post(path, headers=self.headers, json=body)
            self.assertEqual(response.status_code, 200, response.text)
            self.assertEqual(self.daemon.calls[-1][0], method)
        self.assertEqual(self.daemon.calls[0][1]["mark_read"], False)
        self.assertEqual(self.daemon.calls[1][1]["request_id"], "same")
        self.assertEqual(self.daemon.calls[5][1], {"session_uid": "origin-key", "alert_id": "versioned-id"})
        for path, method in [("/api/channels", "messaging.channels"), ("/api/dms", "messaging.dms"),
                             ("/api/people", "messaging.people"), ("/api/counts", "messaging.counts"),
                             ("/api/alerts", "owner_attention.list")]:
            self.assertEqual((await self.client.get(path, headers=self.headers)).status_code, 200)
            self.assertEqual(self.daemon.calls[-1][0], method)
        bootstrap = (await self.client.get("/api/bootstrap", headers=self.headers)).json()
        self.assertEqual(bootstrap["actor"], "owner")
        self.assertTrue(bootstrap["features"]["alerts_seen"])

    async def test_invalid_targets_cursors_sizes_and_reactions(self):
        for path, data in [("/api/read", {"channel": "x", "inbox": True}),
                           ("/api/read", {"channel": "x", "limit": 1000}),
                           ("/api/mark-read", {"channel": "x", "all": True}),
                           ("/api/send", {"channel": "x", "body": "x" * 3001, "request_id": "id"}),
                           ("/api/react", {"message_id": "x", "emoji": "anything", "request_id": "id"})]:
            self.assertEqual((await self.client.post(path, headers=self.headers, json=data)).status_code, 422)
        self.assertEqual((await self.client.get("/api/channels?cursor=[]", headers=self.headers)).status_code, 422)
        self.assertFalse(self.daemon.calls)

    async def test_push_routes_and_manual_test(self):
        data = device()
        key = (await self.client.get("/api/push/vapid-public-key", headers=self.headers)).json()
        self.assertEqual(key["public_key"], self.push.store.public_key)
        self.assertEqual((await self.client.post("/api/push/subscribe", headers=self.headers, json=data)).json(), {"subscribed": True})
        result = await self.client.post("/api/push/test", headers=self.headers, json={"endpoint": data["endpoint"]})
        self.assertEqual(result.json(), {"accepted": True})
        self.assertEqual(len(self.push.store.state["jobs"]), 1)
        result = await self.client.request("DELETE", "/api/push/subscribe", headers=self.headers, json={"endpoint": data["endpoint"]})
        self.assertEqual(result.json(), {"removed": True})
        self.assertFalse(self.push.store.state["jobs"])

    async def test_streamed_body_limit_rejects_before_rpc_and_storage_errors_are_safe(self):
        async def chunks():
            for _ in range(20):
                yield b"x" * 4096
        response = await self.client.post("/api/send", headers={**self.headers, "Content-Type": "application/json"}, content=chunks())
        self.assertEqual(response.status_code, 413)
        self.assertEqual(response.json()["error"]["code"], "body_too_large")
        self.assertFalse(self.daemon.calls)
        with patch("web.server.push.atomic", side_effect=OSError("secret path")):
            response = await self.client.post("/api/push/subscribe", headers=self.headers, json=device())
        self.assertEqual(response.status_code, 503)
        self.assertNotIn("secret", response.text)

    async def test_directory_gets_map_to_fixed_owner_methods(self):
        for path, method in [("bootstrap", "messaging.availability"), ("channels", "messaging.channels"),
                             ("dms", "messaging.dms"), ("people", "messaging.people"),
                             ("counts", "messaging.counts"), ("alerts", "owner_attention.list"),
                             ("availability", "messaging.availability")]:
            self.assertEqual((await self.client.get("/api/" + path, headers=self.headers)).status_code, 200)
            self.assertEqual(self.daemon.calls[-1][0], method)


class PushTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        temp = tempfile.TemporaryDirectory(); self.addCleanup(temp.cleanup)
        self.cfg = settings(Path(temp.name))
        self.store = PushStore(self.cfg.push_path)
        self.device = device()
        self.store.subscribe(self.device)
        self.payload = {"title": "Agent", "body": "Needs attention", "url": "/#alerts", "tag": "one", "badge": 1}

    async def test_keys_and_subscription_persist_with_private_permissions(self):
        again = PushStore(self.cfg.push_path)
        self.assertEqual(again.public_key, self.store.public_key)
        self.assertEqual(len(again.state["subscriptions"]), 1)
        for path, mode in [(self.cfg.push_path, 0o700), (self.store.key_path, 0o600), (self.store.path, 0o600)]:
            self.assertEqual(path.stat().st_mode & 0o777, mode)
        self.store.subscribe(self.device)
        self.assertEqual(len(self.store.state["subscriptions"]), 1)

    async def test_retry_survives_restart_and_gone_devices_are_removed(self):
        self.store.enqueue([self.payload], cursor={"sequence": 1})
        service = PushService(self.cfg, FakeDaemon(), store=self.store, sender=lambda *a: 503)
        await service.deliver_due()
        job = next(iter(self.store.state["jobs"].values()))
        self.assertEqual(job["attempts"], 1)
        self.assertGreater(job["next_at"], time.time())
        self.store = PushStore(self.cfg.push_path)
        self.assertEqual(self.store.state["cursor"], {"sequence": 1})
        for status in (404, 410):
            self.store.subscribe(self.device)
            self.store.enqueue([self.payload])
            for job in self.store.state["jobs"].values(): job["next_at"] = 0
            await PushService(self.cfg, FakeDaemon(), store=self.store, sender=lambda *a: status).deliver_due()
            self.assertFalse(self.store.state["subscriptions"])
            self.assertFalse(self.store.state["jobs"])

    async def test_success_deduplicates_jobs_and_checkpoint_failure_rolls_back_memory(self):
        self.store.enqueue([self.payload, self.payload])
        self.assertEqual(len(self.store.state["jobs"]), 1)
        await PushService(self.cfg, FakeDaemon(), store=self.store, sender=lambda *a: 201).deliver_due()
        self.assertFalse(self.store.state["jobs"])
        with patch("web.server.push.atomic", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                self.store.enqueue([self.payload], cursor={"sequence": 99})
        self.assertFalse(self.store.state["jobs"])
        self.assertIsNone(self.store.state["cursor"])

    async def test_invalid_subscription_keys_and_ssrf_endpoints_are_rejected(self):
        for endpoint in ["http://localhost/a", "https://127.0.0.1/a", "https://fcm.googleapis.com.evil/a",
                         "https://user@web.push.apple.com/a", "https://web.push.apple.com:8443/a"]:
            with self.assertRaises(ValueError): subscription({**self.device, "endpoint": endpoint})
        with self.assertRaises(ValueError): subscription({**self.device, "keys": {"p256dh": "bad", "auth": "bad"}})

    async def test_push_redirects_are_refused_and_old_events_expire(self):
        with PushSession() as session, patch("requests.Session.request") as send:
            session.post(self.device["endpoint"], allow_redirects=True)
            self.assertFalse(send.call_args.kwargs["allow_redirects"])
            with self.assertRaises(ValueError): session.post("https://127.0.0.1/private")
        self.store.enqueue([{**self.payload, "expires_at": time.time() - 1}], cursor={"sequence": 99})
        self.assertFalse(self.store.state["jobs"])
        self.assertEqual(self.store.state["cursor"], {"sequence": 99})

    async def test_feed_enqueues_without_reading_message_history(self):
        class Daemon(FakeDaemon):
            async def call(self, method, params, **kwargs):
                if method == "owner.notifications.next":
                    if any(m == method for m, _ in self.calls): raise asyncio.CancelledError()
                    self.calls.append((method, params))
                    return {"events": [{"id": "cm-alert:a", "created_at": time.time(), "title": "A", "body": "hello", "url": "/#alerts"}],
                            "cursor": {"generation": "g", "sequence": 1}}
                return await super().call(method, params, **kwargs)
        daemon = Daemon()
        with self.assertRaises(asyncio.CancelledError):
            await PushService(self.cfg, daemon, store=self.store).feed_loop()
        self.assertEqual([m for m, _ in daemon.calls], ["owner.notifications.next", "messaging.counts"])
        self.assertEqual(next(iter(self.store.state["jobs"].values()))["payload"]["badge"], 4)


class RpcTests(unittest.IsolatedAsyncioTestCase):
    async def test_fragmented_local_operator_rpc_and_uncertain_timeout(self):
        with tempfile.TemporaryDirectory() as temp:
            cfg = settings(Path(temp)); cfg.token_path.write_text("test-local-token")
            seen = []
            async def handle(reader, writer):
                try:
                    size = struct.unpack(">I", await reader.readexactly(4))[0]
                    req = json.loads(await reader.readexactly(size)); seen.append(req)
                    if req["method"] == "delay":
                        await asyncio.sleep(0.1)
                    payload = json.dumps({"ok": True, "result": {"unread": 7}}).encode()
                    data = struct.pack(">I", len(payload)) + payload
                    for i in range(0, len(data), 3):
                        writer.write(data[i:i + 3]); await writer.drain()
                except (ConnectionError, asyncio.CancelledError):
                    pass
                finally:
                    writer.close()
            server = await asyncio.start_unix_server(handle, path=cfg.socket_path)
            async with server:
                client = DaemonClient(cfg)
                self.assertEqual(await client.call("messaging.counts", {}), {"unread": 7})
                self.assertEqual(seen[0]["caller"], {"token_id": "test-local-token"})
                with self.assertRaises(DaemonError) as error:
                    await client.call("delay", {"request_id": "retained"}, timeout=0.02)
                self.assertEqual(error.exception.code, "outcome_unknown")
                await asyncio.sleep(0.12)
