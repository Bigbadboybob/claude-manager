"""Fake CM Board backend for frontend development and the smoke test.

Implements web/API.md over in-memory data with the standard library only:

    python3 web/dev/fake_server.py [--port 8011]

then open http://127.0.0.1:8011/. No Access check, no daemon, no push
delivery (subscriptions are accepted and remembered). Writes require the
same `Content-Type: application/json` + `X-CM-Board: 1` headers as the real
server, so the frontend is exercised against that rule too.
"""
from __future__ import annotations

import argparse
import itertools
import json
import mimetypes
import threading
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

STATIC = Path(__file__).resolve().parent.parent / "static"
ALLOWED = ["✅", "👀", "👍", "❌", "🎉"]
LEVELS = ["away", "around", "focused", "on-call", "unset"]


def now() -> datetime:
    return datetime.now(timezone.utc)


def iso(t: datetime) -> str:
    return t.strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


class Board:
    """In-memory conversations with per-conversation read cursors."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.seq = itertools.count(1)
        self.people = [
            {"id": "owner", "name": "Owner"},
            {"id": "agent:scout", "name": "Scout"},
            {"id": "agent:swarm-coord", "name": "Swarm-Coord"},
            {"id": "agent:state-core", "name": "state-core"},
        ]
        self.names = {p["id"]: p["name"] for p in self.people}
        self.channels = [
            {"id": "c-general", "path": "general", "name": "general", "joined": True},
            {"id": "c-swarm", "path": "initiative/swarm-focused-design/claude-manager",
             "name": "Swarm Focused Design · claude-manager", "joined": True},
            {"id": "c-gpu", "path": "gpu-utilization", "name": "gpu-utilization", "joined": True},
        ]
        self.dms = [{"id": "d-scout", "members": ["owner", "agent:scout"]}]
        self.messages: dict[str, list[dict]] = {c["id"]: [] for c in self.channels}
        self.messages["d-scout"] = []
        self.read_through: dict[str, int] = {}
        self.requests: dict[str, dict] = {}
        self.subscriptions: dict[str, dict] = {}
        self.availability = {"level": "focused", "set": True, "changed_at": iso(now()), "age_s": 0}
        self.alerts = [{
            "key": "q-1", "id": "alert-1", "session_uid": "ts-scout", "label": "Scout",
            "message": "Need a decision: deploy R1.1 tonight or tomorrow?", "task_id": None,
            "continuous_task_id": None, "urgency": "decision", "released_at": iso(now()),
        }, {
            "key": "q-2", "id": "alert-2", "session_uid": "ts-perf", "label": "Native-Rollout-Perf",
            "host": "sessions", "message": "Bench finished: 18% faster. Merge?\nDetails in #gpu-utilization.",
            "task_id": "fb838659", "continuous_task_id": None, "urgency": "decision",
            "released_at": iso(now() - timedelta(hours=3)),
        }, {
            "key": "q-2", "id": "alert-3", "session_uid": "ts-perf", "label": "Native-Rollout-Perf",
            "host": "sessions", "message": "Still waiting on the merge decision.", "task_id": "fb838659",
            "continuous_task_id": None, "urgency": "decision", "released_at": iso(now() - timedelta(minutes=20)),
        }]
        start = now() - timedelta(hours=30)
        for i in range(160):
            self._post("c-swarm", ["agent:swarm-coord", "agent:state-core", "agent:scout"][i % 3],
                       f"Status update {i}: slice progress, tests green. See `web/API.md`."
                       + (" Long detail. " * (i % 4)), start + timedelta(minutes=10 * i),
                       mention=(i in (150, 157)))
        self._post("c-general", "agent:scout", "Morning all.", now() - timedelta(hours=2))
        self._post("c-gpu", "agent:scout", "GPU utilisation 92% on the RL box.", now() - timedelta(minutes=50))
        self._post("d-scout", "agent:scout", "Can you look at the bench results? https://example.com/bench",
                   now() - timedelta(minutes=5))
        # Owner has read everything except the newest few.
        for cid, items in self.messages.items():
            self.read_through[cid] = items[-1]["_seq"] - (3 if cid == "c-swarm" else 1) if items else 0

    def _post(self, cid: str, actor: str, body: str, at: datetime, mention: bool = False,
              reply_to: str | None = None) -> dict:
        seq = next(self.seq)
        msg = {
            "id": f"origin:{seq:06d}", "type": "message.create", "conversation_id": cid,
            "created_at": iso(at), "actor": {"id": actor, "name": self.names.get(actor, actor),
                                             "kind": "owner" if actor == "owner" else "agent"},
            "body": body,
            "data": {"reply_to": reply_to, "thread_root": reply_to,
                     "mention_recipients": ["owner"] if mention else [], "mention_here": False},
            "reactions": {}, "_seq": seq, "_reactors": {},
        }
        self.messages[cid].append(msg)
        return msg

    def _view(self, cid: str, m: dict) -> dict:
        out = {k: v for k, v in m.items() if not k.startswith("_")}
        out["read"] = m["actor"]["id"] == "owner" or m["_seq"] <= self.read_through.get(cid, 0)
        out["reactions"] = {
            e: {"count": len(who), "names": [self.names.get(p, p) for p in who], "mine": "owner" in who}
            for e, who in m["_reactors"].items() if who
        }
        return out

    def counts(self) -> dict:
        conversations, dms, mentions = {}, 0, 0
        is_dm = {d["id"] for d in self.dms}
        for cid, items in self.messages.items():
            cursor = self.read_through.get(cid, 0)
            unread = [m for m in items if m["_seq"] > cursor and m["actor"]["id"] != "owner"]
            ment = [m for m in unread if "owner" in m["data"]["mention_recipients"]]
            conversations[cid] = {"unread": len(unread), "mentions": len(ment)}
            if cid in is_dm:
                dms += len(unread)
            else:
                mentions += len(ment)
        return {"conversations": conversations, "unread": dms + mentions, "dms": dms, "mentions": mentions}

    def target(self, p: dict) -> str:
        if p.get("conversation") in self.messages:
            return p["conversation"]
        for c in self.channels:
            if p.get("channel") == c["path"]:
                return c["id"]
        for d in self.dms:
            if p.get("dm") in d["members"]:
                return d["id"]
        raise ApiError(422, "invalid_target", "Choose one conversation, channel or dm")

    def read(self, p: dict) -> dict:
        if p.get("inbox"):
            items = [(cid, m) for cid, ms in self.messages.items() for m in ms
                     if m["actor"]["id"] != "owner"]
            if p.get("mentions_only"):
                items = [(c, m) for c, m in items if "owner" in m["data"]["mention_recipients"]]
            items.sort(key=lambda x: x[1]["_seq"], reverse=True)
            return {"items": [self._view(c, m) for c, m in items[: int(p.get("limit") or 50)]],
                    "next_cursor": None, "context": []}
        cid = self.target(p)
        items = self.messages[cid]
        if p.get("thread"):
            items = [m for m in items if m["id"] == p["thread"] or m["data"]["thread_root"] == p["thread"]]
        newest = list(reversed(items))
        offset = int((p.get("cursor") or {}).get("offset", 0))
        limit = max(1, min(int(p.get("limit") or 50), 200))
        page = newest[offset: offset + limit]
        views = [self._view(cid, m) for m in page]  # read flags before advance
        nxt = {"offset": offset + limit, "filter": "fake"} if offset + limit < len(newest) else None
        direct = not p.get("thread") and not p.get("mentions_only") and not p.get("unread_only")
        if direct and p.get("mark_read", True) and offset == 0 and items:
            self.read_through[cid] = max(self.read_through.get(cid, 0), items[-1]["_seq"])
        return {"items": views, "next_cursor": nxt, "context": [],
                "read_cursor": {"logical_time": str(self.read_through.get(cid, 0)), "event_id": None},
                "receipt": {"actor": "owner", "space_id": "fake", "ids": [v["id"] for v in views]}}

    def send(self, p: dict) -> dict:
        rid = p.get("request_id")
        if not rid:
            raise ApiError(422, "invalid_params", "request_id is required")
        if rid in self.requests:
            return self.requests[rid]
        body = p.get("body") or ""
        if not 1 <= len(body) <= 3000:
            raise ApiError(422, "message_too_long", "Body must be 1–3000 characters")
        cid = self.target(p)
        msg = self._post(cid, "owner", body, now(), reply_to=p.get("reply_to"))
        self.read_through[cid] = msg["_seq"]
        result = {"event": self._view(cid, msg), "event_id": msg["id"], "replication": "replicated"}
        self.requests[rid] = result
        return result

    def find(self, message_id: str) -> tuple[str, dict]:
        for cid, items in self.messages.items():
            for m in items:
                if m["id"] == message_id:
                    return cid, m
        raise ApiError(422, "not_found", "Message not found")

    def react(self, p: dict) -> dict:
        if p.get("emoji") not in ALLOWED:
            raise ApiError(422, "invalid_emoji", "Allowed: " + " ".join(ALLOWED))
        if not p.get("request_id"):
            raise ApiError(422, "invalid_params", "request_id is required")
        cid, m = self.find(p.get("message_id", ""))
        who = m["_reactors"].setdefault(p["emoji"], [])
        if p.get("remove"):
            if "owner" in who:
                who.remove("owner")
        elif "owner" not in who:
            who.append("owner")
            if "owner" in m["data"]["mention_recipients"]:
                self.read_through[cid] = max(self.read_through.get(cid, 0), m["_seq"])
        return {"event_id": f"react:{next(self.seq)}", "message_id": m["id"],
                "reactions": self._view(cid, m)["reactions"], "counts": self.counts()}

    def mark_read(self, p: dict) -> dict:
        if p.get("all"):
            for cid, items in self.messages.items():
                if items:
                    self.read_through[cid] = items[-1]["_seq"]
            return {"changed": True, "counts": self.counts()}
        if p.get("mentions") and not any(p.get(k) for k in ("conversation", "channel", "dm")):
            for cid, items in self.messages.items():
                ment = [m for m in items if "owner" in m["data"]["mention_recipients"]]
                if ment:
                    self.read_through[cid] = max(self.read_through.get(cid, 0), ment[-1]["_seq"])
            return {"changed": True, "counts": self.counts()}
        cid = self.target(p)
        items = self.messages[cid]
        through = items[-1]["_seq"] if items else 0
        if p.get("mentions"):
            ment = [m for m in items if "owner" in m["data"]["mention_recipients"]]
            through = ment[-1]["_seq"] if ment else self.read_through.get(cid, 0)
        if p.get("through"):
            through = self.find(p["through"])[1]["_seq"]
        before = self.read_through.get(cid, 0)
        self.read_through[cid] = max(before, through)
        return {"changed": self.read_through[cid] != before, "conversation_id": cid, "counts": self.counts()}


class ApiError(Exception):
    def __init__(self, status: int, code: str, message: str) -> None:
        super().__init__(message)
        self.status, self.code, self.message = status, code, message


BOARD = Board()


class Handler(BaseHTTPRequestHandler):
    server_version = "cm-board-fake/1"

    def log_message(self, fmt: str, *args) -> None:  # quiet
        pass

    def _json(self, status: int, data) -> None:
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _body(self) -> dict:
        if self.headers.get("X-CM-Board") != "1" or "application/json" not in (self.headers.get("Content-Type") or ""):
            raise ApiError(403, "origin", "Writes need Content-Type: application/json and X-CM-Board: 1")
        n = int(self.headers.get("Content-Length") or 0)
        try:
            return json.loads(self.rfile.read(n) or b"{}")
        except json.JSONDecodeError:
            raise ApiError(422, "invalid_json", "Body is not JSON")

    def _query(self) -> dict:
        q = {k: v[0] for k, v in parse_qs(urlparse(self.path).query).items()}
        if "cursor" in q:
            q["cursor"] = json.loads(q["cursor"])
        return q

    def _route(self, method: str) -> None:
        path = urlparse(self.path).path
        try:
            with BOARD.lock:
                if not path.startswith("/api/") and path != "/healthz":
                    return self._static(path)
                result = self._api(method, path)
            self._json(200, result)
        except ApiError as e:
            self._json(e.status, {"error": {"code": e.code, "message": e.message}})

    def _api(self, method: str, path: str):
        b = BOARD
        if (method, path) == ("GET", "/healthz"):
            return {"ok": True}
        if (method, path) == ("GET", "/api/bootstrap"):
            return {"actor": "owner", "user": {"email": "owner@example.com"},
                    "features": {"alerts_seen": True, "push": True}, "owner_availability": b.availability}
        if (method, path) == ("GET", "/api/channels"):
            return {"items": [dict(c, **b.counts()["conversations"][c["id"]]) for c in b.channels], "next_cursor": None}
        if (method, path) == ("GET", "/api/dms"):
            return {"items": [dict(d, **b.counts()["conversations"][d["id"]]) for d in b.dms], "next_cursor": None}
        if (method, path) == ("GET", "/api/people"):
            return {"items": b.people, "next_cursor": None}
        if (method, path) == ("GET", "/api/counts"):
            return b.counts()
        if (method, path) == ("POST", "/api/read"):
            return b.read(self._body())
        if (method, path) == ("POST", "/api/send"):
            return b.send(self._body())
        if (method, path) == ("POST", "/api/channels/join"):
            self._body()
            return {"joined": True}
        if (method, path) == ("POST", "/api/mark-read"):
            return b.mark_read(self._body())
        if (method, path) == ("POST", "/api/react"):
            return b.react(self._body())
        if (method, path) == ("GET", "/api/alerts"):
            return {"items": b.alerts, "seen_supported": True}
        if (method, path) == ("POST", "/api/alerts/seen"):
            p = self._body()
            before = len(b.alerts)
            b.alerts = [a for a in b.alerts if not (a["key"] == p.get("key") and a["id"] == p.get("alert_id"))]
            return {"ok": True, "acknowledged": len(b.alerts) != before}
        if path == "/api/availability":
            if method == "POST":
                p = self._body()
                if p.get("level") not in LEVELS or not p.get("request_id"):
                    raise ApiError(422, "invalid_level", "Unknown level")
                b.availability = {"level": p["level"], "set": p["level"] != "unset",
                                  "changed_at": iso(now()), "age_s": 0}
                return {"owner_availability": b.availability, "event_id": f"avail:{next(b.seq)}"}
            return {"owner_availability": b.availability}
        if (method, path) == ("GET", "/api/push/vapid-public-key"):
            return {"public_key": "BOrFakeVapidKeyForLocalDevelopmentOnly-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                    "enabled": True}
        if path == "/api/push/subscribe":
            p = self._body()
            if method == "DELETE":
                return {"removed": b.subscriptions.pop(p.get("endpoint"), None) is not None}
            if not p.get("endpoint") or not (p.get("keys") or {}).get("p256dh"):
                raise ApiError(422, "invalid_subscription", "endpoint and keys required")
            b.subscriptions[p["endpoint"]] = p
            return {"subscribed": True}
        if (method, path) == ("POST", "/api/push/test"):
            self._body()
            return {"accepted": True}
        raise ApiError(404, "not_found", f"No route {method} {path}")

    def _static(self, path: str) -> None:
        rel = "index.html" if path in ("", "/") else path.lstrip("/")
        target = (STATIC / rel).resolve()
        if STATIC not in target.parents and target != STATIC / "index.html" or not target.is_file():
            return self._json(404, {"error": {"code": "not_found", "message": path}})
        data = target.read_bytes()
        kind = "application/manifest+json" if target.suffix == ".webmanifest" else (
            mimetypes.guess_type(target.name)[0] or "application/octet-stream")
        if target.suffix == ".js":
            kind = "text/javascript"
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-cache")
        if target.name == "sw.js":
            self.send_header("Service-Worker-Allowed", "/")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self) -> None:
        self._route("GET")

    def do_POST(self) -> None:
        self._route("POST")

    def do_DELETE(self) -> None:
        self._route("DELETE")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8011)
    args = ap.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"fake CM Board on http://127.0.0.1:{args.port}/", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
