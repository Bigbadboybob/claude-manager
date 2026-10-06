"""Write rules for work items (doc/items-board.md §2), free of any I/O.

`dispatch.items_db` loads one board's live items under the board-row lock,
runs a `BoardTx` over them, and writes back exactly what the transaction
recorded.  Keeping the rules here lets them be tested without Postgres.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone

STATUSES = ("open", "active", "waiting", "blocked", "done", "dropped")
CLOSED = frozenset({"done", "dropped"})
RESOLVE_ACTIONS = ("nudge", "reassign", "launch", "block", "drop")

MAX_TITLE = 200
MAX_NOTE = 500
MAX_GROUP = 80
MAX_BLOCKED_ON = 200
MAX_LINKS = 20
MAX_LINK = 500
MAX_ITEMS_PER_CALL = 50

# Board settings a nudge snoozes each flag kind for; anything else uses repush_s.
SNOOZE_SETTING = {
    "holder_idle": "idle_s",
    "stale": "stale_s",
    "unassigned": "unassigned_s",
}

_DURATION = re.compile(r"^(?:(\d+)d)?(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$")


class ItemsError(Exception):
    """A refused write.  `status` is the HTTP status the API answers with."""

    def __init__(self, status: int, code: str, message: str, **extra):
        super().__init__(message)
        self.status = status
        self.code = code
        self.message = message
        self.extra = extra

    def detail(self) -> dict:
        return {"code": self.code, "message": self.message, **self.extra}


def invalid(code: str, message: str, **extra) -> ItemsError:
    return ItemsError(422, code, message, **extra)


def parse_when(value: str, now: datetime, field_name: str) -> datetime:
    """A duration (`40m`, `2h`, `1h30m`, `1d`) from now, or an RFC 3339 time."""
    text = str(value).strip()
    match = _DURATION.match(text.lower())
    if text and match and any(match.groups()):
        days, hours, minutes, seconds = (int(g or 0) for g in match.groups())
        return now + timedelta(days=days, hours=hours, minutes=minutes, seconds=seconds)
    try:
        parsed = datetime.fromisoformat(text.replace("Z", "+00:00"))
    except ValueError:
        raise invalid(
            "invalid_field",
            f"{field_name} must be a duration like 40m, 2h, 1h30m or an RFC 3339 time",
            field=field_name,
        ) from None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed


def iso(value):
    return value.isoformat() if isinstance(value, datetime) else value


@dataclass
class Item:
    n: int
    title: str
    status: str
    id: int | None = None
    note: str | None = None
    group: str | None = None
    blocked_on: str | None = None
    check_back_at: datetime | None = None
    eta_at: datetime | None = None
    waiting_set_at: datetime | None = None
    links: list = field(default_factory=list)
    touched_at: datetime | None = None
    clock_reset_at: datetime | None = None
    closed_at: datetime | None = None
    archived_at: datetime | None = None
    created_by: str | None = None
    created_at: datetime | None = None
    holders: list = field(default_factory=list)       # [{pid, name, session_uid, daemon_id}]
    blocked_by: set = field(default_factory=set)      # item numbers
    open_flags: dict = field(default_factory=dict)    # kind -> {raised_at, detail}

    def snapshot(self) -> dict:
        """The fields history records, JSON-ready."""
        return {
            "title": self.title,
            "status": self.status,
            "note": self.note,
            "group": self.group,
            "blocked_on": self.blocked_on,
            "blocked_by": sorted(self.blocked_by),
            "check_back_at": iso(self.check_back_at),
            "eta_at": iso(self.eta_at),
            "links": list(self.links),
            "holders": [h["pid"] for h in self.holders],
        }

    def public(self) -> dict:
        return {
            "n": self.n,
            "title": self.title,
            "status": self.status,
            "holders": [
                {k: h.get(k) for k in ("pid", "name", "session_uid", "daemon_id")}
                for h in self.holders
            ],
            "note": self.note,
            "group": self.group,
            "blocked_by": sorted(self.blocked_by),
            "blocked_on": self.blocked_on,
            "check_back_at": iso(self.check_back_at),
            "eta_at": iso(self.eta_at),
            "waiting_set_at": iso(self.waiting_set_at),
            "links": list(self.links),
            "touched_at": iso(self.touched_at),
            "clock_reset_at": iso(self.clock_reset_at),
            "closed_at": iso(self.closed_at),
            "archived_at": iso(self.archived_at),
            "created_by": self.created_by,
            "created_at": iso(self.created_at),
            "flags": sorted(self.open_flags),
        }


def _text(value, limit: int, name: str, *, required: bool = False) -> str | None:
    if value is None:
        if required:
            raise invalid("invalid_field", f"{name} is required", field=name)
        return None
    if not isinstance(value, str):
        raise invalid("invalid_field", f"{name} must be a string", field=name)
    text = " ".join(value.split())  # one line
    if not text:
        if required:
            raise invalid("invalid_field", f"{name} must not be empty", field=name)
        return None
    if len(text) > limit:
        raise invalid("invalid_field", f"{name} is longer than {limit} characters", field=name)
    return text


def _links(value) -> list:
    if value is None:
        return []
    if not isinstance(value, list) or not all(isinstance(v, str) for v in value):
        raise invalid("invalid_field", "links must be a list of strings", field="links")
    links = [v.strip() for v in value if v.strip()]
    if len(links) > MAX_LINKS or any(len(v) > MAX_LINK for v in links):
        raise invalid(
            "invalid_field",
            f"at most {MAX_LINKS} links of at most {MAX_LINK} characters",
            field="links",
        )
    return links


def _holder(value) -> dict:
    if not isinstance(value, dict) or not isinstance(value.get("pid"), str) or not value["pid"]:
        raise invalid("invalid_field", "each holder needs a pid", field="holders")
    return {
        "pid": value["pid"],
        "name": value.get("name"),
        "session_uid": value.get("session_uid"),
        "daemon_id": value.get("daemon_id"),
    }


def _dedupe_holders(holders) -> list:
    seen: dict[str, dict] = {}
    for h in holders:
        seen.setdefault(h["pid"], h)
    return list(seen.values())


class BoardTx:
    """One locked board's write transaction, recorded for `items_db` to apply."""

    def __init__(self, board: dict, items: dict[int, Item], actor: dict, now: datetime):
        if not actor or not actor.get("pid"):
            raise invalid("invalid_field", "actor.pid is required", field="actor")
        self.board = board
        self.items = items
        self.actor = actor
        self.now = now
        self.created: list[int] = []
        self.dirty: set[int] = set()
        self.holders_changed: set[int] = set()
        self.deps_changed: set[int] = set()
        self.events: list[dict] = []
        self.flags_raised: list[dict] = []
        self.flags_resolved: list[dict] = []
        self.pushes: list[dict] = []
        self.unblocked: list[int] = []
        self.warnings: list[str] = []
        self.board_changed = False

    # ---- lookups -------------------------------------------------------
    def item(self, n: int) -> Item:
        it = self.items.get(n)
        if it is None:
            raise ItemsError(404, "not_found", f"no item #{n} on board {self.board['slug']}", n=n)
        return it

    def _dependents(self, n: int) -> list[Item]:
        return sorted(
            (it for it in self.items.values() if n in it.blocked_by), key=lambda it: it.n
        )

    def _actor_holder(self) -> list[dict]:
        a = self.actor
        if a.get("session_uid") and a.get("daemon_id"):
            return [_holder(a)]
        return []

    # ---- recording helpers --------------------------------------------
    def _event(self, it: Item, type_: str, prev=None, new=None, reason=None):
        self.events.append({
            "n": it.n, "type": type_, "prev": prev, "new": new, "reason": reason,
        })

    def _push(self, it: Item, holder: dict, kind: str, text: str):
        if not holder.get("daemon_id") or not holder.get("session_uid"):
            return  # Owner, or a holder recorded without a session
        self.pushes.append({
            "n": it.n,
            "pid": holder["pid"],
            "daemon_id": holder["daemon_id"],
            "session_uid": holder["session_uid"],
            "kind": kind,
            "text": text,
            "dedupe": f"{kind}:{self.board['id']}:{it.n}:{holder['pid']}:{self.now.isoformat()}",
        })

    def _prefix(self) -> str:
        return f"[cm-board {self.board['slug']}]"

    def raise_flag(self, it: Item, kind: str, detail: dict | None = None):
        if kind in it.open_flags:
            return
        it.open_flags[kind] = {"raised_at": self.now, "detail": detail}
        self.flags_raised.append({"n": it.n, "kind": kind, "detail": detail})
        self._event(it, "flag_raised", new={"kind": kind, "detail": detail})

    def resolve_flag(self, it: Item, kind: str, resolution: str,
                     snooze_until: datetime | None = None):
        if kind not in it.open_flags:
            return False
        del it.open_flags[kind]
        self.flags_resolved.append({
            "n": it.n, "kind": kind, "resolution": resolution,
            "snooze_until": snooze_until,
        })
        self._event(it, "flag_resolved", new={
            "kind": kind, "resolution": resolution, "snooze_until": iso(snooze_until),
        })
        return True

    def _clear_blocker_dropped(self, it: Item):
        if "blocker_dropped" not in it.open_flags:
            return
        if not any(self.items.get(b) and self.items[b].status == "dropped"
                   for b in it.blocked_by):
            self.resolve_flag(it, "blocker_dropped", "cleared")

    # ---- cycle check ---------------------------------------------------
    def _check_cycles(self, it: Item):
        """Refuse when some blocker of `it` (transitively) waits on `it`."""
        for start in sorted(it.blocked_by):
            path = self._path(start, it.n, {it.n})
            if path is not None:
                chain = "→".join(str(x) for x in [it.n, *path])
                raise ItemsError(409, "cycle", f"cycle: {chain}", cycle=[it.n, *path])

    def _path(self, frm: int, target: int, seen: set) -> list[int] | None:
        if frm == target:
            return [frm]
        if frm in seen:
            return None
        seen.add(frm)
        node = self.items.get(frm)
        if node is None:
            return None
        for nxt in sorted(node.blocked_by):
            rest = self._path(nxt, target, seen)
            if rest is not None:
                return [frm, *rest]
        return None

    # ---- operations ----------------------------------------------------
    def create(self, specs: list[dict]) -> list[int]:
        if not specs:
            raise invalid("invalid_field", "items must not be empty", field="items")
        if len(specs) > MAX_ITEMS_PER_CALL:
            raise invalid("invalid_field", f"at most {MAX_ITEMS_PER_CALL} items per call",
                          field="items")
        created = []
        for spec in specs:
            spec = dict(spec)
            title = _text(spec.pop("title", None), MAX_TITLE, "title", required=True)
            n = int(self.board["next_number"])
            self.board["next_number"] = n + 1
            self.board_changed = True
            it = Item(n=n, title=title, status="open", touched_at=self.now,
                      created_at=self.now, created_by=self.actor["pid"])
            self.items[n] = it
            self.created.append(n)
            holders = spec.pop("holders", None)
            holders = (self._actor_holder() if holders is None
                       else [_holder(h) for h in holders])
            self.apply(n, spec, holders=holders, creating=True)
            created.append(n)
        return created

    def update(self, ns: list[int], fields: dict, *, add_holders=None,
               remove_holders=None, reason: str | None = None) -> list[int]:
        if not ns:
            raise invalid("invalid_field", "ns must not be empty", field="ns")
        if len(ns) > MAX_ITEMS_PER_CALL:
            raise invalid("invalid_field", f"at most {MAX_ITEMS_PER_CALL} items per call",
                          field="ns")
        fields = dict(fields)
        holders = fields.pop("holders", None) if "holders" in fields else None
        replace = None if holders is None else [_holder(h) for h in holders]
        adds = [_holder(h) for h in (add_holders or [])]
        removes = [h if isinstance(h, str) else _holder(h)["pid"] for h in (remove_holders or [])]
        out = []
        for n in dict.fromkeys(ns):
            self.item(n)
            self.apply(n, fields, holders=replace, add=adds, remove=removes, reason=reason)
            out.append(n)
        return out

    def apply(self, n: int, fields: dict, *, holders=None, add=(), remove=(),
              reason=None, creating=False):
        """Apply one item's field changes and every rule that follows from them."""
        it = self.item(n)
        unknown = set(fields) - {
            "status", "title", "note", "group", "blocked_by", "blocked_on",
            "check_back", "eta", "links",
        }
        if unknown:
            raise invalid("invalid_field", f"unknown fields: {sorted(unknown)}",
                          field=sorted(unknown)[0])
        prev = None if creating else it.snapshot()
        prev_status = None if creating else it.status
        prev_pids = [] if creating else [h["pid"] for h in it.holders]

        explicit = fields.get("status")
        if "status" in fields and (explicit is None or explicit not in STATUSES):
            raise invalid("invalid_status",
                          f"status must be one of {', '.join(STATUSES)}", field="status")
        if "title" in fields:
            it.title = _text(fields["title"], MAX_TITLE, "title", required=True)
        if "note" in fields:
            it.note = _text(fields["note"], MAX_NOTE, "note")
        if "group" in fields:
            it.group = _text(fields["group"], MAX_GROUP, "group")
        if "links" in fields:
            it.links = _links(fields["links"])
        if "blocked_on" in fields:
            it.blocked_on = _text(fields["blocked_on"], MAX_BLOCKED_ON, "blocked_on")
        if "check_back" in fields:
            it.check_back_at = (None if fields["check_back"] is None
                                else parse_when(fields["check_back"], self.now, "check_back"))
        eta_given = "eta" in fields and fields["eta"] is not None
        if "eta" in fields:
            it.eta_at = None if fields["eta"] is None else parse_when(fields["eta"], self.now, "eta")
            it.waiting_set_at = self.now if eta_given else None

        # Holders.
        new_holders = list(it.holders) if holders is None else holders
        for h in add:
            new_holders.append(h)
        new_holders = [h for h in _dedupe_holders(new_holders) if h["pid"] not in set(remove)]
        holders_changed = creating or [h["pid"] for h in new_holders] != prev_pids
        if holders_changed:
            it.holders = new_holders
            self.holders_changed.add(n)

        # Blockers: real, open items on this board (rule 3).
        blockers_given = "blocked_by" in fields
        if blockers_given:
            wanted = fields["blocked_by"] or []
            if not isinstance(wanted, list) or not all(isinstance(b, int) for b in wanted):
                raise invalid("invalid_field", "blocked_by must be a list of item numbers",
                              field="blocked_by")
            for b in wanted:
                blocker = self.items.get(b)
                if b == n:
                    raise invalid("invalid_blocker", f"#{n} cannot block itself", blocker=b)
                if blocker is None or blocker.archived_at is not None:
                    raise invalid("invalid_blocker", f"no open item #{b} on this board",
                                  blocker=b)
                if blocker.status in CLOSED:
                    raise invalid("invalid_blocker", f"#{b} is {blocker.status}", blocker=b)
            if set(wanted) != it.blocked_by:
                it.blocked_by = set(wanted)
                self.deps_changed.add(n)

        # Status, explicit or derived (rule 1).
        status = "open" if creating else it.status
        if explicit:
            status = explicit
        else:
            if creating:
                status = "active" if new_holders else "open"
            elif holders_changed and status not in CLOSED:
                if not new_holders:
                    status = "open"
                elif status == "open":
                    status = "active"
            if status not in CLOSED:
                if blockers_given and it.blocked_by:
                    status = "blocked"
                elif "blocked_on" in fields and it.blocked_on:
                    status = "blocked"
                elif eta_given:
                    status = "waiting"
                elif (blockers_given and not it.blocked_by and status == "blocked"
                      and not it.blocked_on):
                    status = "active" if new_holders else "open"

        # Only a blocked item carries blockers.
        if status != "blocked":
            if blockers_given and it.blocked_by:
                raise invalid("invalid_field", "blocked_by requires status blocked",
                              field="blocked_by")
            if "blocked_on" in fields and it.blocked_on:
                raise invalid("invalid_field", "blocked_on requires status blocked",
                              field="blocked_on")
            if it.blocked_by:
                it.blocked_by = set()
                self.deps_changed.add(n)
            it.blocked_on = None
            it.check_back_at = None
        elif not it.blocked_by and not it.blocked_on:
            self.warnings.append(
                f"#{n} is blocked with neither blocked_by nor blocked_on; it is not exempt "
                "from idle/stale flags"
            )

        # Waiting needs an ETA (rule 4); leaving waiting clears it.
        if status == "waiting":
            if not eta_given and (prev_status != "waiting" or it.eta_at is None):
                raise invalid("eta_required", "waiting requires eta (e.g. 40m, 2h, or a time)",
                              field="eta")
        elif it.eta_at is not None or it.waiting_set_at is not None:
            if eta_given:
                raise invalid("invalid_field", "eta requires status waiting", field="eta")
            it.eta_at = None
            it.waiting_set_at = None

        it.status = status
        if n in self.deps_changed:
            self._check_cycles(it)
            self._clear_blocker_dropped(it)

        # Close and reopen (rules 5-7).
        was_closed = prev_status in CLOSED
        if status in CLOSED and not was_closed:
            it.closed_at = self.now
            for kind in sorted(it.open_flags):
                self.resolve_flag(it, kind, "closed")
        elif status not in CLOSED and was_closed:
            it.closed_at = None
            it.archived_at = None
            for dep in self._dependents(n):
                self._clear_blocker_dropped(dep)
        if status == "done" and prev_status != "done":
            self._cascade_done(it)
        elif status == "dropped" and prev_status not in CLOSED:
            self._flag_dependents_dropped(it)

        if status != prev_status or "note" in fields or holders_changed:
            it.touched_at = self.now

        # Assigned pushes (rule 8).
        for h in it.holders:
            if h["pid"] not in prev_pids and h["pid"] != self.actor["pid"]:
                self._push(it, h, "assigned",
                           f"{self._prefix()} you were assigned #{n} \"{it.title}\" by "
                           f"{self.actor.get('name') or self.actor['pid']}. Update it with "
                           f"item_set({n}, ...); hand it back with item_set({n}, holder=\"none\").")

        self.dirty.add(n)
        new = it.snapshot()
        if creating:
            self._event(it, "created", new=new, reason=reason)
        else:
            changed = [k for k in new if new[k] != prev[k]]
            if changed:
                self._event(it, "updated", prev={k: prev[k] for k in changed},
                            new={k: new[k] for k in changed}, reason=reason)

    def _cascade_done(self, it: Item):
        for dep in self._dependents(it.n):
            dep.blocked_by.discard(it.n)
            self.deps_changed.add(dep.n)
            self.dirty.add(dep.n)
            self._clear_blocker_dropped(dep)
            if dep.blocked_by or dep.status != "blocked" or dep.blocked_on:
                self._event(dep, "updated", prev={"blocked_by": sorted(dep.blocked_by | {it.n})},
                            new={"blocked_by": sorted(dep.blocked_by)},
                            reason=f"#{it.n} done")
                continue
            dep.status = "active" if dep.holders else "open"
            dep.clock_reset_at = self.now
            self.unblocked.append(dep.n)
            self._event(dep, "unblocked", prev={"status": "blocked", "blocked_by": [it.n]},
                        new={"status": dep.status, "blocked_by": []}, reason=f"#{it.n} done")
            for h in dep.holders:
                self._push(dep, h, "unblocked",
                           f"{self._prefix()} #{dep.n} \"{dep.title}\" is unblocked: "
                           f"#{it.n} \"{it.title}\" is done. It is active again.")

    def _flag_dependents_dropped(self, it: Item):
        for dep in self._dependents(it.n):
            if dep.status in CLOSED:
                continue
            dropped = sorted(b for b in dep.blocked_by
                             if self.items.get(b) and self.items[b].status == "dropped")
            self.raise_flag(dep, "blocker_dropped", {"blockers": dropped})
            self.dirty.add(dep.n)

    def resolve(self, n: int, action: str, *, kind: str | None = None, holders=None,
                blocked_by=None, blocked_on=None, check_back=None, reason=None,
                message=None) -> list[str]:
        it = self.item(n)
        if action not in RESOLVE_ACTIONS:
            raise invalid("invalid_field", f"action must be one of {', '.join(RESOLVE_ACTIONS)}",
                          field="action")
        if action == "launch":
            raise invalid("invalid_field",
                          "launch is completed by the daemon: spawn, then reassign",
                          field="action")
        if it.status in CLOSED and action != "drop":
            raise invalid("item_closed", f"#{n} is {it.status}")
        kinds = [kind] if kind else sorted(it.open_flags)
        resolved: list[str] = []

        if action == "nudge":
            if not it.holders:
                raise invalid("no_holders", f"#{n} has no holder to nudge; reassign it instead")
            for k in kinds:
                setting = SNOOZE_SETTING.get(k, "repush_s")
                until = self.now + timedelta(seconds=int(self.board.get(setting) or 0))
                if self.resolve_flag(it, k, "nudge", snooze_until=until):
                    resolved.append(k)
            what = ", ".join(kinds) if kinds else "in need of an update"
            text = (f"{self._prefix()} #{n} \"{it.title}\" looks {what}: update it, close it "
                    f"(item_set({n}, \"done\")) or hand it back (item_set({n}, holder=\"none\")).")
            extra = message or reason
            if extra:
                text += f" {self.actor.get('name') or self.actor['pid']}: {extra}"
            for h in it.holders:
                self._push(it, h, "nudge", text)
            self._event(it, "resolved", new={"action": action, "flags": resolved}, reason=reason)
            self.dirty.add(n)
            return resolved

        for k in kinds:
            if self.resolve_flag(it, k, action):
                resolved.append(k)
        if action == "reassign":
            if not holders:
                raise invalid("invalid_field", "reassign needs holders", field="holders")
            self.apply(n, {}, holders=[_holder(h) for h in holders], reason=reason)
        elif action == "block":
            fields: dict = {"status": "blocked"}
            if blocked_by:
                fields["blocked_by"] = blocked_by
            if blocked_on:
                if not check_back:
                    raise invalid("check_back_required",
                                  "blocking on free text needs a check_back time",
                                  field="check_back")
                fields["blocked_on"] = blocked_on
                fields["check_back"] = check_back
            if not blocked_by and not blocked_on:
                raise invalid("invalid_field", "block needs blocked_by or blocked_on",
                              field="blocked_by")
            self.apply(n, fields, reason=reason)
        elif action == "drop":
            self.apply(n, {"status": "dropped"}, reason=reason)
        self._event(it, "resolved", new={"action": action, "flags": resolved}, reason=reason)
        return resolved
