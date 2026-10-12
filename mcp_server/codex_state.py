"""Bounded, I/O-free app-server observations for session.agent_report.

Only explicit frontend selection changes the foreground. Ephemeral title jobs
and unrelated threads cannot make the selected conversation appear busy/idle.
Times supplied by the caller make notification sequences deterministic in tests.
"""

from dataclasses import dataclass, field
import math
import re


LIMIT = 256
REQUEST_KINDS = {
    "item/commandExecution/requestApproval": "approval",
    "item/fileChange/requestApproval": "approval",
    "item/permissions/requestApproval": "approval",
    "execCommandApproval": "approval",
    "applyPatchApproval": "approval",
    "item/tool/requestUserInput": "user_input",
    "mcpServer/elicitation/request": "elicitation",
    "item/tool/call": "tool_call",
    "account/chatgptAuthTokens/refresh": "auth_refresh",
}


def short(value, size=256):
    return str(value or "").encode("utf-8")[:size].decode("utf-8", "ignore")


def timestamp(value, now):
    return (
        value
        if isinstance(value, (float, int)) and math.isfinite(value) and 0 < value <= now
        else now
    )


def parent_id(thread):
    source = thread.get("source")
    subagent = source.get("subagent", {}) if isinstance(source, dict) else {}
    spawn = subagent.get("thread_spawn", {}) if isinstance(subagent, dict) else {}
    return thread.get("parentThreadId") or spawn.get("parent_thread_id")


def selectable(thread):
    return bool(
        thread.get("id") and not thread.get("ephemeral") and not parent_id(thread)
    )


@dataclass
class Thread:
    parent: str | None = None
    ephemeral: bool = False
    status: str | None = None
    flags: list = field(default_factory=list)
    status_seen: bool = False
    turn_id: str | None = None
    completed_id: str | None = None
    started_at: float | None = None
    ended_at: float | None = None
    last_status: str | None = None
    error_kind: str | None = None
    retrying: bool = False


class CodexState:
    def __init__(self):
        self.foreground = None
        self.connected = False
        self.version = None
        self.turn_seq = 0
        self.threads = {}
        self.requests = {}
        self.terminals = {}
        self.background_complete = False
        self.background_at = None
        self.ended = []
        self.thread_overflow = False
        self.request_overflow = False

    @property
    def overflow(self):
        if len(self.threads) < LIMIT:
            self.thread_overflow = False
        if len(self.requests) < LIMIT:
            self.request_overflow = False
        return self.thread_overflow or self.request_overflow

    def connection(self, connected):
        self.connected = connected
        if not connected:
            self.background_complete = False

    def thread(self, uid):
        if not isinstance(uid, str) or not uid or len(uid.encode()) > LIMIT:
            return None
        if uid not in self.threads:
            if len(self.threads) >= LIMIT:
                tree = self.tree()
                requested = {r[0] for r in self.requests.values()}
                victim = next(
                    (
                        key
                        for key, t in self.threads.items()
                        if key not in tree
                        and key not in requested
                        and (
                            t.ephemeral
                            or t.status in {"idle", "notLoaded", "systemError"}
                        )
                    ),
                    None,
                )
                if victim is not None:
                    del self.threads[victim]
                    self.thread_overflow = False
            if len(self.threads) >= LIMIT:
                # Do not silently forget an active child or human request.
                self.thread_overflow = True
                return None
            self.threads[uid] = Thread()
        return self.threads[uid]

    def tree(self):
        result = {self.foreground} if self.foreground else set()
        for _ in range(len(self.threads)):
            children = {
                uid
                for uid, t in self.threads.items()
                if t.parent in result and not t.ephemeral
            }
            if children <= result:
                break
            result |= children
        return result

    def status(self, thread, status, *, notification=False):
        if isinstance(status, dict):
            thread.status = status.get("type")
            thread.flags = [short(f) for f in status.get("activeFlags", [])[:16]]
            thread.status_seen |= notification

    def turn(self, uid, turn, now, *, completed=False):
        thread = self.thread(uid)
        if thread is None or not isinstance(turn, dict) or not turn.get("id"):
            return
        active = turn.get("status") == "inProgress"
        if active and thread.completed_id == turn["id"]:
            return
        if active and thread.turn_id != turn["id"]:
            thread.turn_id = turn["id"]
            thread.started_at = timestamp(turn.get("startedAt"), now)
            thread.retrying = False
            thread.error_kind = None
            if uid == self.foreground:
                self.turn_seq += 1
        if completed and turn.get("status") in {"completed", "interrupted", "failed"}:
            # A duplicate response must not move an ending forward to receipt time.
            if thread.completed_id != turn["id"]:
                thread.ended_at = timestamp(turn.get("completedAt"), now)
            thread.completed_id = turn["id"]
            thread.turn_id = turn["id"]
            thread.last_status = turn["status"]
            thread.retrying = False
            error = turn.get("error") or {}
            info = error.get("codexErrorInfo")
            thread.error_kind = (
                short(next(iter(info), "unknown") if isinstance(info, dict) else info)
                or None
            )
        if not thread.status_seen:
            if active:
                thread.status = "active"
            elif completed:
                thread.status = "idle"

    def load_thread(self, value, now, *, select=False):
        uid = value.get("id")
        thread = self.thread(uid)
        if thread is None:
            return
        thread.parent = parent_id(value)
        thread.ephemeral = bool(value.get("ephemeral"))
        if select and selectable(value) and uid != self.foreground:
            self.foreground = uid
            self.terminals = {}
            self.ended = []
            self.background_complete = False
            self.background_at = None
        turns = value.get("turns") or []
        if turns:
            latest = turns[-1]
            self.turn(uid, latest, now, completed=latest.get("status") != "inProgress")
        self.status(thread, value.get("status"))

    def observe(self, message, now, *, method=None, params=None, select=False):
        """Consume an upstream notification/request or a correlated RPC reply."""
        result = message.get("result")
        if isinstance(result, dict):
            if method == "initialize":
                # App-server prefixes its runtime version with the client's
                # chosen name (e.g. cm_fixture_frontend), not always codex.
                match = re.match(
                    r"[^ /]+/(\d+\.\d+\.\d+[^ ]*)", result.get("userAgent", "")
                )
                if match:
                    self.version = short(match[1])
            if method in {
                "thread/start",
                "thread/resume",
                "thread/fork",
                "thread/read",
            }:
                if isinstance(result.get("thread"), dict):
                    self.load_thread(result["thread"], now, select=select)
            if method == "turn/start":
                self.turn((params or {}).get("threadId"), result.get("turn"), now)
            return
        event = message.get("method")
        data = message.get("params") or {}
        uid = data.get("threadId") or data.get("conversationId")
        if event == "thread/started":
            self.load_thread(data.get("thread", {}), now)
        elif event == "thread/status/changed":
            if (thread := self.thread(uid)) is not None:
                self.status(thread, data.get("status"), notification=True)
        elif event in {"turn/started", "turn/completed"}:
            turn = dict(data.get("turn") or {})
            # Notifications mark the edge now; server history timestamps may
            # have only second precision, earlier than CM's input in that second.
            turn["completedAt" if event == "turn/completed" else "startedAt"] = now
            self.turn(uid, turn, now, completed=event == "turn/completed")
        elif event == "error":
            if (thread := self.thread(uid)) is not None:
                if data.get("turnId") == thread.turn_id:
                    thread.retrying = bool(data.get("willRetry"))
        elif event == "serverRequest/resolved":
            self.requests.pop(data.get("requestId"), None)
        elif event == "thread/closed":
            if uid != self.foreground and not any(
                t.parent == uid for t in self.threads.values()
            ):
                self.threads.pop(uid, None)
            elif uid in self.threads:
                self.status(self.threads[uid], {"type": "notLoaded"})
        if "id" in message and event in REQUEST_KINDS:
            if message["id"] not in self.requests:
                if len(self.requests) >= LIMIT:
                    self.request_overflow = True
                else:
                    self.requests[message["id"]] = (uid, REQUEST_KINDS[event], now)

    def background(self, uid, terminals, now, *, complete):
        if uid != self.foreground:
            return
        if not complete and not terminals:
            self.background_complete = False
            return
        # Partial pages still prove that the jobs they contain are running;
        # they cannot prove that a previously seen job ended.
        jobs = {} if complete else dict(self.terminals)
        for terminal in terminals[:LIMIT]:
            tid = short(terminal.get("processId"))
            if not tid:
                complete = False
                continue
            old = self.terminals.get(tid, {})
            job = {
                "id": tid,
                "kind": "terminal",
                "label": short(terminal.get("command")),
                "first_seen_at": old.get("first_seen_at", now),
                "wakes_agent": False,
            }
            pid, cpu = terminal.get("osPid"), terminal.get("cpuPercent")
            if isinstance(pid, int) and 0 < pid <= 0xFFFFFFFF:
                job["pid"] = pid
            if isinstance(cpu, (int, float)) and math.isfinite(cpu) and cpu >= 0:
                job["cpu"] = cpu
            jobs[tid] = job
        complete &= len(terminals) <= LIMIT and len(jobs) <= LIMIT
        jobs = dict(list(jobs.items())[:LIMIT])
        if complete and self.background_complete:
            for tid, old in self.terminals.items():
                if tid not in jobs:
                    self.ended.append(
                        {"id": tid, "label": old["label"], "ended_at": now}
                    )
            self.ended = self.ended[-10:]
        self.terminals = jobs
        self.background_complete = complete
        self.background_at = now

    def snapshot(self):
        thread = self.threads.get(self.foreground, Thread())
        tree = self.tree()
        flags = sorted(
            {f for uid in tree if uid in self.threads for f in self.threads[uid].flags}
        )[:16]
        return {
            "backend_connected": self.connected
            and not self.overflow
            and thread.status in {"active", "idle", "systemError"},
            "foreground": thread.status
            if thread.status in {"active", "idle", "systemError"}
            else "idle",
            "engine_version": self.version,
            "turn_seq": self.turn_seq,
            "turn_started_at": thread.started_at,
            "last_turn": {"ended_at": thread.ended_at, "status": thread.last_status},
            "active_flags": flags,
            "pending_requests": [
                {"kind": kind, "since": since}
                for uid, kind, since in self.requests.values()
                if uid in tree
            ],
            "child_active": any(
                self.threads[uid].status == "active"
                for uid in tree
                if uid != self.foreground
            ),
            "retrying": thread.retrying,
            "error_kind": thread.error_kind,
            "background": {
                "complete": self.background_complete,
                "observed_at": self.background_at,
                "jobs": list(self.terminals.values()),
                "crons": [],
                "ended": list(self.ended),
            },
        }
