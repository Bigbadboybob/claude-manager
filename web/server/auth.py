"""Validate Cloudflare Access signatures and claims even at the origin."""
import asyncio
import time

import httpx
import jwt
from starlette.responses import JSONResponse


class AccessVerifier:
    def __init__(self, settings, client=None):
        self.settings = settings
        self.client = client or httpx.AsyncClient(timeout=10, follow_redirects=False)
        self.keys = {}
        self.expires = 0.0
        self.last_refresh = -float("inf")
        self.lock = asyncio.Lock()

    async def close(self):
        await self.client.aclose()

    async def verify(self, token):
        if not token or len(token) > 16384:
            raise ValueError("Missing Access assertion")
        header = jwt.get_unverified_header(token)
        kid = header.get("kid")
        if header.get("alg") != "RS256" or not isinstance(kid, str) or len(kid) > 256:
            raise ValueError("Unsupported Access signing key")
        now = time.monotonic()
        if now >= self.expires or kid not in self.keys:
            async with self.lock:
                now = time.monotonic()
                if (now >= self.expires or kid not in self.keys) and now - self.last_refresh >= 60:
                    self.last_refresh = now
                    response = await self.client.get(
                        f"https://{self.settings.team_domain}/cdn-cgi/access/certs")
                    response.raise_for_status()
                    if len(response.content) > 256 * 1024:
                        raise ValueError("Oversized Access key set")
                    keys = {}
                    for key in response.json()["keys"]:
                        if key.get("kty") == "RSA" and key.get("use", "sig") == "sig":
                            keys[key["kid"]] = jwt.PyJWK.from_dict(key, algorithm="RS256").key
                    if not keys:
                        raise ValueError("Empty Access key set")
                    self.keys, self.expires = keys, now + 3600
        if kid not in self.keys or time.monotonic() >= self.expires:
            raise ValueError("Access key unavailable")
        claims = jwt.decode(token, self.keys[kid], algorithms=["RS256"],
                            audience=self.settings.audience,
                            issuer=f"https://{self.settings.team_domain}", leeway=30,
                            options={"require": ["exp", "iat", "iss", "aud", "sub", "email"]})
        if not isinstance(claims["email"], str) or claims["email"].lower() != self.settings.owner_email.lower():
            raise ValueError("Access identity is not Owner")
        return claims


class AccessMiddleware:
    def __init__(self, app, verifier, origin):
        self.app, self.verifier, self.origin = app, verifier, origin

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            return await self.app(scope, receive, send)
        if scope["path"] == "/healthz" and scope["method"] == "GET":
            return await self.app(scope, receive, send)
        headers = {k.decode().lower(): v.decode() for k, v in scope["headers"]}
        try:
            claims = await self.verifier.verify(headers.get("cf-access-jwt-assertion"))
        except (ValueError, KeyError, TypeError, jwt.PyJWTError, httpx.HTTPError):
            return await JSONResponse({"error": {"code": "access_denied", "message": "Sign in through Cloudflare Access."}},
                                      status_code=403)(scope, receive, send)
        if headers.get("origin") not in (None, self.origin):
            return await JSONResponse({"error": {"code": "origin_denied", "message": "Foreign origin refused."}},
                                      status_code=403)(scope, receive, send)
        if scope["method"] not in ("GET", "HEAD", "OPTIONS"):
            if (headers.get("x-cm-board") != "1"
                    or headers.get("content-type", "").split(";", 1)[0].strip() != "application/json"):
                return await JSONResponse({"error": {"code": "csrf_denied", "message": "Use an authenticated JSON board request."}},
                                          status_code=403)(scope, receive, send)
        scope.setdefault("state", {})["identity"] = claims
        async def secure_send(message):
            if message["type"] == "http.response.start":
                message["headers"] += [
                    (b"cache-control", b"no-cache" if scope["path"] == "/sw.js" else b"no-store"), (b"x-content-type-options", b"nosniff"),
                    (b"referrer-policy", b"same-origin"),
                    (b"content-security-policy", b"default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; worker-src 'self'; frame-ancestors 'none'; base-uri 'self'; object-src 'none'"),
                ]
            await send(message)

        # Bound the complete body before routing. An exception raised inside
        # receive() can otherwise be swallowed by downstream JSON parsers.
        body = bytearray()
        while True:
            message = await receive()
            if message["type"] == "http.disconnect":
                return
            body.extend(message.get("body", b""))
            if len(body) > 65536:
                return await JSONResponse({"error": {"code": "body_too_large", "message": "Request exceeds 64 KiB."}},
                                          status_code=413)(scope, receive, secure_send)
            if not message.get("more_body", False):
                break
        consumed = False

        async def bounded_receive():
            nonlocal consumed
            if consumed:
                return await receive()
            consumed = True
            return {"type": "http.request", "body": bytes(body), "more_body": False}

        await self.app(scope, bounded_receive, secure_send)
