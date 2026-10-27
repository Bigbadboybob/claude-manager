"""Write rules for work items (doc/items-board.md §2), free of any I/O.

`dispatch.items_db` loads one board's live items under the board-row lock,
runs a `BoardTx` over them, and writes back exactly what the transaction
recorded.  Keeping the rules here lets them be tested without Postgres.
"""

from __future__ import annotations

import re
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone

STATUSES = ("open", "active", "waiting", "blocked", "blocked_on_owner", "done", "dropped")
OWNER_PID = "owner"
# Waiting on a decision only Owner can make: set by Owner, or by a holder who
# names the decision in `blocked_on`; cleared by Owner, or by recording
# Owner's answer with item_resolve(n, "answer", message=...).
OWNER_BLOCKED = "blocked_on_owner"
CLOSED = frozenset({"done", "dropped"})
RESOLVE_ACTIONS = ("nudge", "reassign", "launch", "block", "drop", "answer", "apply", "dismiss")
# A keyword in a holder's or the steward's progress post suggests a status;
# the holder or steward confirms it (item_resolve "apply") or dismisses it.
# Never applied automatically (Board-Steward, 2026-10-20).
SUGGESTED = "suggested_transition"
SUGGEST_STATUSES = frozenset({"done", "blocked"})

MAX_TITLE = 200
MAX_NOTE = 500
MAX_GROUP = 80
MAX_BLOCKED_ON = 200
MAX_JOB = 200
MAX_DESCRIPTION = 20000
# Holder roles (doc/org-model.md §7.2). `holder` is the legacy co-owner role
# multi-holder items migrated with; the single-owner rule skips such items.
ROLES = ("owner", "contributor", "reviewer", "approver", "holder")
# While an item is one of these and held, exactly one holder is its owner.
OWNED_STATUSES = frozenset({"active", "waiting", "blocked"})
GROUP_KINDS = ("heading", "epic")
EPIC_CLOSED = frozenset({"done", "cancelled"})
MAX_LINKS = 20
MAX_LINK = 500
MAX_ITEMS_PER_CALL = 50
MAX_N = 2**31 - 1

# Board settings a nudge snoozes each flag kind for; anything else uses repush_s.
SNOOZE_SETTING = {
    "holder_idle": "idle_s",
    "stale": "stale_s",
    "unassigned": "unassigned_s",
    "unlaunched": "launch_s",
}

# Strict boards (`backlog_allowed: false`, doc/strict-boards.md): what each
# non-conforming shape is called on a needs_triage flag, and the call that
# fixes it.
TRIAGE_FIX = {
    "waiting": 'item_set(n, "active", eta="40m", job="...") for a job in flight, '
               'or block it with its trigger',
    "open": 'give it a holder (item_set(n, holder=...)) or drop it and file a planning task',
    "no_trigger": 'item_set(n, blocked_by=[m]) or item_set(n, blocked_on="...", check_back="4h")',
    "no_check_back": 'item_set(n, check_back="4h")',
    "check_back_too_far": 'item_set(n, check_back="24h") (a closer check-back)',
    "job_without_eta": 'item_set(n, eta="40m")',
}


def strict(board: dict) -> bool:
    """A board that holds only running or blocked work."""
    return board.get("backlog_allowed") is False


def check_back_max(board: dict) -> timedelta:
    return timedelta(seconds=board.get("check_back_max_s") or 259200)


def strict_violation(it: "Item", board: dict, now: datetime) -> str | None:
    """Why an open item does not fit a strict board (a TRIAGE_FIX key), or None."""
    if it.status in CLOSED:
        return None
    if it.status == "waiting":
        return "waiting"
    if it.status == "open":
        return "open"
    if it.status == "blocked" and not it.blocked_by:
        if not it.blocked_on:
            return "no_trigger"
        if it.check_back_at is None:
            return "no_check_back"
        if it.check_back_at > now + check_back_max(board):
            return "check_back_too_far"
    if it.status == "active" and it.job and it.eta_at is None:
        return "job_without_eta"
    return None


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
    job: str | None = None                            # the job in flight (active only)
    group_id: str | None = None                       # its heading or epic (groups.id)
    team_id: str | None = None                        # the owning team (org S2)
    task_id: str | None = None                        # the planning task it was placed from
    link_archived_at: datetime | None = None          # unplaced: read-only from then on
    description: str | None = None                    # the task's, for a placed item
    description_rev: int = 0
    actionable_at: datetime | None = None             # unlaunched clock start
    launched_at: datetime | None = None               # a holder's own last write / #N post
    links: list = field(default_factory=list)
    touched_at: datetime | None = None
    clock_reset_at: datetime | None = None
    closed_at: datetime | None = None
    closed_by: str | None = None                      # name (else pid) of who closed it
    archived_at: datetime | None = None
    created_by: str | None = None
    created_at: datetime | None = None
    stale_nudged_at: datetime | None = None
    flag_pushed: dict = field(default_factory=dict)   # kind -> fingerprint at last push
    gate_head: str | None = None                      # gate_binding.head (read-only here)
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
            "job": self.job,
            "group_id": self.group_id,
            "links": list(self.links),
            "holders": [h["pid"] for h in self.holders],
            "owner": next((h["pid"] for h in self.holders if h.get("role") == "owner"), None),
        }

    def public(self) -> dict:
        return {
            "n": self.n,
            "title": self.title,
            "status": self.status,
            "holders": [
                {**{k: h.get(k) for k in ("pid", "name", "session_uid", "daemon_id", "role")},
                 **{k: h[k] for k in ("member_id", "acting_as") if h.get(k) is not None}}
                for h in self.holders
            ],
            "group_id": self.group_id,
            "team_id": self.team_id,
            "task_id": self.task_id,
            "link_archived_at": iso(self.link_archived_at),
            "description": self.description,
            "description_rev": self.description_rev,
            "note": self.note,
            "group": self.group,
            "blocked_by": sorted(self.blocked_by),
            "blocked_on": self.blocked_on,
            "check_back_at": iso(self.check_back_at),
            "eta_at": iso(self.eta_at),
            "waiting_set_at": iso(self.waiting_set_at),
            "job": self.job,
            "actionable_at": iso(self.actionable_at),
            "launched_at": iso(self.launched_at),
            "links": list(self.links),
            "touched_at": iso(self.touched_at),
            "clock_reset_at": iso(self.clock_reset_at),
            "closed_at": iso(self.closed_at),
            "closed_by": self.closed_by,
            "archived_at": iso(self.archived_at),
            "created_by": self.created_by,
            "created_at": iso(self.created_at),
            "flags": sorted(self.open_flags),
        }


@dataclass
class Group:
    """A board group (doc/org-model.md §7.3): a topic heading, or an epic with
    a goal, a status and (from org S2) a team whose lead is its PoC."""
    id: str
    name: str
    kind: str = "heading"
    parent_id: str | None = None
    ord: int = 0
    description: str | None = None
    goal: str | None = None
    epic_status: str | None = None
    team_id: str | None = None
    revision: int = 0
    closed_at: datetime | None = None
    closed_by: str | None = None

    @property
    def closed(self) -> bool:
        return self.kind == "epic" and self.epic_status in EPIC_CLOSED

    def public(self, poc_member_id: str | None = None) -> dict:
        return {
            "id": self.id, "name": self.name, "kind": self.kind,
            "parent_group_id": self.parent_id, "ord": self.ord,
            "description": self.description, "goal": self.goal,
            "epic_status": self.epic_status, "team_id": self.team_id,
            "poc_member_id": poc_member_id, "revision": self.revision,
            "closed_at": iso(self.closed_at), "closed_by": self.closed_by,
        }


_TOKEN = re.compile(r"[a-z0-9][a-z0-9_-]{3,}")
_STOP = frozenset({
    "with", "from", "that", "this", "into", "onto", "when", "then", "than", "them",
    "make", "makes", "should", "would", "could", "about", "after", "before", "over",
    "under", "more", "less", "also", "only", "just", "item", "task", "work", "done",
})


def title_tokens(title: str | None) -> set[str]:
    return {t for t in _TOKEN.findall((title or "").lower()) if t not in _STOP}


def duplicate_hints(it: "Item", others) -> list[dict]:
    """`possible_duplicate` (doc/org-model.md §7.4): open items whose titles
    share at least two significant words and 40% of them, or that link the
    same path. A hint only; nothing is refused."""
    mine = title_tokens(it.title)
    paths = {link for link in it.links if "/" in link}
    out = []
    for o in others:
        if o.n == it.n or o.status in CLOSED or o.archived_at is not None or o.link_archived_at:
            continue
        theirs = title_tokens(o.title)
        shared = mine & theirs
        why = []
        if len(shared) >= 2 and len(shared) / max(1, len(mine | theirs)) >= 0.4:
            why.append("title: " + ", ".join(sorted(shared)))
        same = paths & {link for link in o.links if "/" in link}
        if same:
            why.append("links: " + ", ".join(sorted(same)[:3]))
        if why:
            out.append({"n": o.n, "title": o.title, "status": o.status, "why": "; ".join(why)})
    return out[:5]


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
    role = value.get("role")
    if role is not None and role not in ROLES:
        raise invalid("invalid_field", f"role must be one of {', '.join(ROLES)}", field="role")
    out = {
        "pid": value["pid"],
        "name": value.get("name"),
        "session_uid": value.get("session_uid"),
        "daemon_id": value.get("daemon_id"),
    }
    for key in ("member_id", "acting_as"):
        if value.get(key) is not None:
            out[key] = value[key]
    if role is not None:
        out["role"] = role
    if value.get("added_at") is not None:
        out["added_at"] = value["added_at"]
    return out


def _dedupe_holders(holders) -> list:
    seen: dict[str, dict] = {}
    for h in holders:
        seen.setdefault(h["pid"], h)
    return list(seen.values())


class BoardTx:
    """One locked board's write transaction, recorded for `items_db` to apply."""

    def __init__(self, board: dict, items: dict[int, Item], actor: dict, now: datetime,
                 groups: dict[str, Group] | None = None):
        if not actor or not actor.get("pid"):
            raise invalid("invalid_field", "actor.pid is required", field="actor")
        self.board = board
        self.items = items
        self.groups: dict[str, Group] = groups if groups is not None else {}
        # Heading groups created by this transaction (item_set(group="new")).
        self.groups_created: list[str] = []
        # Placed items: task fields to write in the same transaction (the
        # task owns title and description, doc/org-model.md §7.1).
        self.task_writes: dict[int, dict] = {}
        # (n, previous owner pid | None, new owner holder | None): the
        # launch grants to move (§7.4).
        self.owner_changes: list[tuple[int, str | None, dict | None]] = []
        self.actor = actor
        self.now = now
        self.created: list[int] = []
        self.dirty: set[int] = set()
        self.holders_changed: set[int] = set()
        self.deps_changed: set[int] = set()
        self.events: list[dict] = []
        self.flags_raised: list[dict] = []
        self.flags_detail: list[dict] = []
        self.flags_resolved: list[dict] = []
        self.pushes: list[dict] = []
        self.unblocked: list[int] = []
        self.warnings: list[str] = []
        self.board_changed = False
        # (n, entered) for every move into or out of blocked_on_owner.
        self.owner_transitions: list[tuple[int, bool]] = []

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
            if detail is not None and it.open_flags[kind].get("detail") != detail:
                it.open_flags[kind]["detail"] = detail
                self.flags_detail.append({"n": it.n, "kind": kind, "detail": detail})
                self._event(it, "flag_updated", new={"kind": kind, "detail": detail})
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

    # ---- roles ---------------------------------------------------------
    @staticmethod
    def _assign_roles(holders: list[dict], prev_pids: set, *, replaced: bool) -> list[dict]:
        """Give each new holder a role: the first becomes owner when there is
        none, the rest contributors. Holders that predate roles (migrated
        co-owners) keep theirs."""
        out = [dict(h) for h in holders]
        has_owner = any(h.get("role") == "owner" for h in out)
        for h in out:
            if h.get("role") is not None or (h["pid"] in prev_pids and not replaced):
                continue
            h["role"] = "contributor" if has_owner else "owner"
            has_owner = True
        return out

    def _check_owner(self, it: Item, status: str, holders: list[dict]):
        """Exactly one owner while active, waiting or blocked and held (§7.2).
        Items still carrying migrated co-owners are exempt until one is named."""
        if status not in OWNED_STATUSES or not holders:
            return
        if any(h.get("role") in (None, "holder") for h in holders):
            return
        owners = [h["pid"] for h in holders if h.get("role") == "owner"]
        if not owners:
            raise invalid("owner_required",
                          f"#{it.n} needs an owner while it is {status}: name one with "
                          f"item_set({it.n}, owner=...), or hand it back (holder=\"none\")",
                          field="owner")
        if len(owners) > 1:
            raise invalid("one_owner",
                          f"#{it.n} can have one owner; transfer it with item_set({it.n}, owner=...)",
                          field="owner", owners=owners)

    def _notify_description(self, it: Item):
        who = self.actor.get("name") or self.actor["pid"]
        for h in it.holders:
            if h["pid"] == self.actor["pid"]:
                continue
            self._push(it, h, "description_changed",
                       f"{self._prefix()} the description of #{it.n} \"{it.title}\" was changed "
                       f"by {who} (revision {it.description_rev}). Re-read it before you go on: "
                       f"board(view=\"full\", query=\"{it.title[:40]}\").")

    # ---- groups --------------------------------------------------------
    def resolve_group(self, *, name: str | None = None, group_id: str | None = None,
                      create: bool = True) -> Group | None:
        """The group an item write names: by id, or by name (a unique match
        anywhere on the board, else the top-level one), creating a top-level
        heading for a new name, as free-text groups always worked."""
        if group_id:
            g = self.groups.get(str(group_id))
            if g is None:
                raise invalid("invalid_group", f"no group {group_id} on board {self.board['slug']}",
                              field="group_id")
            return g
        name = _text(name, MAX_GROUP, "group")
        if not name:
            return None
        matches = [g for g in self.groups.values() if g.name.lower() == name.lower()]
        if len(matches) == 1:
            return matches[0]
        top = [g for g in matches if g.parent_id is None]
        if top:
            return top[0]
        if matches:
            raise invalid("ambiguous_group",
                          f"{len(matches)} groups on board {self.board['slug']} are named "
                          f"{name!r}: pass group_id", field="group",
                          candidates=[{"id": g.id, "parent_group_id": g.parent_id} for g in matches])
        if not create:
            raise invalid("invalid_group", f"no group {name!r} on board {self.board['slug']}",
                          field="group")
        g = Group(id=str(uuid.uuid4()), name=name)
        self.groups[g.id] = g
        self.groups_created.append(g.id)
        return g

    # ---- strict boards -------------------------------------------------
    def _launch_min(self) -> int:
        return max(1, int((self.board.get("launch_s") or 900) // 60))

    def _check_strict(self, it: Item, status: str, explicit, prev_status, creating: bool,
                      holders: list, fields: dict, blockers_given: bool):
        """Refuse new non-conforming intent on a strict board, naming the
        call that conforms (doc/strict-boards.md §2.2). A write that leaves an
        already non-conforming item as it was still succeeds (needs_triage
        carries it); only entering such a state is refused."""
        n, slug = it.n, self.board["slug"]
        entering = creating or status != prev_status
        if status == "waiting" and (entering or explicit == "waiting"):
            raise invalid("waiting_not_allowed",
                          f"board {slug} holds only running or blocked work. A job in flight: "
                          f'item_set({n}, "active", eta="40m", job="..."). Waiting on '
                          f"something: item_set({n}, blocked_by=[m]), or "
                          f'item_set({n}, blocked_on="...", check_back="4h")', field="status")
        # Owner's own add may be unheld: the orchestrator assigns it.
        unheld = status == "open" or (creating and status == "active" and not holders
                                      and self.actor["pid"] != OWNER_PID)
        if unheld and (entering or explicit == "open"):
            raise invalid("holder_required",
                          f"board {slug} keeps no backlog: give #{n if not creating else 'it'} "
                          f"a holder (holder=...), launch it with item_resolve(n, \"launch\"), "
                          f"or file future work as a planning task (propose_task)",
                          field="holders")
        touches_block = blockers_given or "blocked_on" in fields or "check_back" in fields
        if status == "blocked" and (entering or touches_block) and not it.blocked_by:
            if not it.blocked_on:
                raise invalid("trigger_required",
                              f"on board {slug} a blocked item names its trigger: "
                              f"item_set({n}, blocked_by=[m]) for an item, or "
                              f'item_set({n}, blocked_on="...", check_back="4h")',
                              field="blocked_by")
            if it.check_back_at is None:
                raise invalid("check_back_required",
                              f"on board {slug} blocking on free text needs a check-back: "
                              f'item_set({n}, blocked_on="{it.blocked_on}", check_back="4h")',
                              field="check_back")

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
               remove_holders=None, reason: str | None = None, reopen: bool = False,
               expect_status: str | None = None, owner=None,
               role: str | None = None) -> list[int]:
        if not ns:
            raise invalid("invalid_field", "ns must not be empty", field="ns")
        if len(ns) > MAX_ITEMS_PER_CALL:
            raise invalid("invalid_field", f"at most {MAX_ITEMS_PER_CALL} items per call",
                          field="ns")
        fields = dict(fields)
        # A `holders` key replaces the list; an explicit null clears it.
        replace = None
        if "holders" in fields:
            replace = [_holder(h) for h in (fields.pop("holders") or [])]
        if role is not None and role not in ROLES:
            raise invalid("invalid_field", f"role must be one of {', '.join(ROLES)}", field="role")
        adds = [_holder({**h, "role": h.get("role") or role} if isinstance(h, dict) else h)
                for h in (add_holders or [])]
        owner_h = _holder(owner) if owner is not None else None
        removes = [h if isinstance(h, str) else _holder(h)["pid"] for h in (remove_holders or [])]
        if expect_status is not None and expect_status not in STATUSES:
            raise invalid("invalid_status",
                          f"expect_status must be one of {', '.join(STATUSES)}",
                          field="expect_status")
        out = []
        for n in dict.fromkeys(ns):
            it = self.item(n)
            # Compare-and-set: checked here, under the board-row lock the
            # write holds, so nothing can change the item in between.
            if expect_status is not None and it.status != expect_status:
                raise ItemsError(409, "status_changed",
                                 f"#{n} is {it.status} now, not {expect_status}: re-read it "
                                 f"(board(query=...)) before writing", n=n,
                                 current_status=it.status, expected=expect_status)
            self.apply(n, fields, holders=replace, add=adds, remove=removes, reason=reason,
                       reopen=reopen, owner=owner_h)
            out.append(n)
        return out

    def apply(self, n: int, fields: dict, *, holders=None, add=(), remove=(),
              reason=None, creating=False, answered=False, reopen=False, owner=None):
        """Apply one item's field changes and every rule that follows from them."""
        it = self.item(n)
        unknown = set(fields) - {
            "status", "title", "note", "group", "group_id", "blocked_by", "blocked_on",
            "check_back", "eta", "job", "links", "description", "team_id",
        }
        if unknown:
            raise invalid("invalid_field", f"unknown fields: {sorted(unknown)}",
                          field=sorted(unknown)[0])
        # An unplaced task's item is history: its number still resolves, but
        # writing through it could revive a stale assignment (§7.1).
        if it.link_archived_at is not None and not creating:
            raise ItemsError(409, "link_archived",
                             f"#{n} was unplaced at {iso(it.link_archived_at)} and is read-only. "
                             f"To work on the task again, place it: place(task=\"{it.task_id}\", "
                             f"board=\"{self.board['slug']}\")", n=n, task_id=it.task_id)
        prev = None if creating else it.snapshot()
        prev_status = None if creating else it.status
        prev_pids = [] if creating else [h["pid"] for h in it.holders]
        prev_owner = next((h["pid"] for h in it.holders if h.get("role") == "owner"), None)
        strict_board = strict(self.board)

        explicit = fields.get("status")
        if "status" in fields and (explicit is None or explicit not in STATUSES):
            raise invalid("invalid_status",
                          f"status must be one of {', '.join(STATUSES)}", field="status")
        if "title" in fields:
            it.title = _text(fields["title"], MAX_TITLE, "title", required=True)
            if it.task_id and not creating:
                # The task owns it: one copy, so get_task and deliveries agree.
                self.task_writes.setdefault(n, {})["name"] = it.title
        if "description" in fields:
            text = _text(fields["description"], MAX_DESCRIPTION, "description")
            if text != it.description:
                it.description = text
                it.description_rev += 1
                if it.task_id:
                    self.task_writes.setdefault(n, {})["description"] = text
                if not creating:
                    self._notify_description(it)
        if "note" in fields:
            it.note = _text(fields["note"], MAX_NOTE, "note")
        if "group" in fields or "group_id" in fields:
            g = (self.resolve_group(group_id=fields["group_id"]) if fields.get("group_id")
                 else self.resolve_group(name=fields.get("group")))
            if g is not None and g.id != it.group_id and g.closed:
                raise ItemsError(409, "epic_closed",
                                 f"epic {g.name!r} is {g.epic_status}; put #{n} in an open group",
                                 group_id=g.id)
            it.group_id = g.id if g else None
            it.group = g.name if g else None
        if "team_id" in fields:
            it.team_id = _text(fields["team_id"], 64, "team_id")
        if "links" in fields:
            it.links = _links(fields["links"])
        if "blocked_on" in fields:
            it.blocked_on = _text(fields["blocked_on"], MAX_BLOCKED_ON, "blocked_on")
        if "check_back" in fields:
            it.check_back_at = (None if fields["check_back"] is None
                                else parse_when(fields["check_back"], self.now, "check_back"))
            if (strict_board and it.check_back_at is not None
                    and it.check_back_at > self.now + check_back_max(self.board)):
                hours = int(check_back_max(self.board).total_seconds() // 3600)
                raise invalid("check_back_too_far",
                              f"#{n}: board {self.board['slug']} takes a check_back at most "
                              f"{hours}h out, so it stays a real next look: "
                              f'item_set({n}, check_back="24h")', field="check_back")
        job_given = "job" in fields and fields["job"] is not None
        if "job" in fields:
            it.job = _text(fields["job"], MAX_JOB, "job")
        eta_given = "eta" in fields and fields["eta"] is not None
        if "eta" in fields:
            it.eta_at = None if fields["eta"] is None else parse_when(fields["eta"], self.now, "eta")
            it.waiting_set_at = self.now if eta_given else None

        # Holders.
        # Copies: roles are adjusted below and compared with the old list.
        new_holders = [dict(h) for h in (it.holders if holders is None else holders)]
        for h in add:
            new_holders.append(dict(h))
        new_holders = [h for h in _dedupe_holders(new_holders) if h["pid"] not in set(remove)]
        if owner is not None:
            # Transferring ownership is one atomic write (§7.2): the named
            # holder becomes owner (added if new), any other owner contributes.
            new_holders = [h for h in new_holders if h["pid"] != owner["pid"]]
            for h in new_holders:
                # Naming the owner also settles a migrated item's co-owners.
                if h.get("role") in ("owner", "holder", None):
                    h["role"] = "contributor"
            existing = next((h for h in it.holders if h["pid"] == owner["pid"]), None)
            new_holders.insert(0, {**(existing or owner), "role": "owner"})
        new_holders = self._assign_roles(new_holders, set(prev_pids), replaced=holders is not None)
        # The owner left and one holder remains: the successor is not in
        # doubt (today's add-then-remove hand-off keeps working). With
        # several left, _check_owner asks for an explicit owner.
        if (prev_owner and len(new_holders) == 1
                and not any(h.get("role") == "owner" for h in new_holders)
                and new_holders[0].get("role") not in (None, "holder")):
            new_holders[0]["role"] = "owner"
        holders_changed = (creating or [h["pid"] for h in new_holders] != prev_pids
                           or [h.get("role") for h in new_holders]
                           != [h.get("role") for h in it.holders])
        if holders_changed:
            it.holders = new_holders
            self.holders_changed.add(n)

        # Blockers: real, open items on this board (rule 3).
        blockers_given = "blocked_by" in fields
        if blockers_given:
            wanted = fields["blocked_by"] or []
            if not isinstance(wanted, list) or not all(
                    isinstance(b, int) and not isinstance(b, bool) and 0 < b <= MAX_N
                    for b in wanted):
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
                if not new_holders and status == "active" and strict_board:
                    # A handback on a strict board is accepted: the item
                    # stays active, unheld, and goes unlaunched.
                    self.warnings.append(
                        f"#{n} has no holder now; it stays active and the orchestrator is "
                        f"told if nobody takes it within {self._launch_min()} min (unlaunched)")
                elif not new_holders and status == "active":
                    status = "open"
                elif not new_holders:
                    # Blocked/waiting keep their blockers or ETA; stale and
                    # overdue still reach the orchestrator.
                    self.warnings.append(
                        f"#{n} has no holder now; it stays {status}"
                    )
                elif status == "open":
                    status = "active"
            if status not in CLOSED:
                if blockers_given and it.blocked_by:
                    status = "blocked"
                elif "blocked_on" in fields and it.blocked_on and status != OWNER_BLOCKED:
                    status = "blocked"
                elif eta_given:
                    # A strict board has no waiting: an eta means a job in
                    # flight on an active item.
                    status = "active" if strict_board else "waiting"
                elif (blockers_given and not it.blocked_by and status == "blocked"
                      and not it.blocked_on):
                    status = "active" if new_holders or strict_board else "open"

        if (strict_board and creating and status == "open" and not new_holders
                and self.actor["pid"] == OWNER_PID):
            # Owner's add (Alt+B a) is not backlog: it is work for the
            # orchestrator to assign. Active and unheld, it goes unlaunched.
            status = "active"
            self.warnings.append(
                f"#{n} has no holder; the orchestrator is told if nobody takes it within "
                f"{self._launch_min()} min (unlaunched)")
        if strict_board:
            self._check_strict(it, status, explicit, prev_status, creating, new_holders,
                               fields, blockers_given)

        # Blocked on Owner: who may enter and leave it.
        actor_pid = self.actor["pid"]
        is_owner = actor_pid == OWNER_PID

        # A done item reopens only on purpose (rule 7): a write from a stale
        # read (sejd #1665, #1643, #1505, 2026-10-24) otherwise turns finished
        # work back into blocked work. Owner acts deliberately, so Owner's
        # writes (the TUI, Alt+B) reopen without the flag.
        reopened = prev_status == "done" and status != "done"
        if reopened and not (reopen or is_owner):
            who = it.closed_by or "unknown"
            when = iso(it.closed_at) or "an unknown time"
            raise ItemsError(409, "reopen_required",
                             f"#{n} is done (closed by {who} at {when}); this write would make "
                             f"it {status}. If that is a stale read, re-read the board. To "
                             f"reopen it on purpose: item_set({n}, \"{status}\", ..., reopen=True)",
                             n=n, current_status="done", closed_by=it.closed_by,
                             closed_at=iso(it.closed_at), would_be=status)
        if status == OWNER_BLOCKED and prev_status != OWNER_BLOCKED:
            if not is_owner:
                if actor_pid not in prev_pids and actor_pid not in [h["pid"] for h in it.holders]:
                    raise ItemsError(403, "owner_only",
                                     "only Owner or the item's holder can mark it blocked on Owner")
                if not it.blocked_on:
                    raise invalid("decision_required",
                                  "name the decision Owner must make in blocked_on "
                                  "(e.g. blocked_on=\"approve 8xH100 for SEJD?\")",
                                  field="blocked_on")
        if prev_status == OWNER_BLOCKED and status != OWNER_BLOCKED and not (is_owner or answered):
            raise ItemsError(403, "owner_only",
                             f"#{n} is blocked on Owner: only Owner clears it, or record Owner's "
                             f"answer with item_resolve({n}, \"answer\", message=...)")

        # Only a blocked item carries blockers (blocked_on also names the
        # decision of an item blocked on Owner).
        if status != "blocked":
            if blockers_given and it.blocked_by:
                raise invalid("invalid_field", "blocked_by requires status blocked",
                              field="blocked_by")
            keeps_text = status == OWNER_BLOCKED
            if "blocked_on" in fields and it.blocked_on and not keeps_text:
                raise invalid("invalid_field", "blocked_on requires status blocked",
                              field="blocked_on")
            if it.blocked_by:
                it.blocked_by = set()
                self.deps_changed.add(n)
            if not keeps_text:
                it.blocked_on = None
            it.check_back_at = None
        elif not it.blocked_by and not it.blocked_on:
            self.warnings.append(
                f"#{n} is blocked with neither blocked_by nor blocked_on; it is not exempt "
                "from idle/stale flags"
            )

        # Waiting needs an ETA (rule 4). An active item may carry one too
        # (explicit status active with eta): inside it the item is not
        # idle or stale (§5). Leaving both clears it.
        keeps_eta = status == "active" and it.eta_at is not None and (
            eta_given or prev_status == "active")
        if status == "waiting":
            if not eta_given and (prev_status != "waiting" or it.eta_at is None):
                raise invalid("eta_required", "waiting requires eta (e.g. 40m, 2h, or a time)",
                              field="eta")
        elif keeps_eta:
            pass
        elif it.eta_at is not None or it.waiting_set_at is not None:
            if eta_given:
                raise invalid("invalid_field", "eta requires status waiting or active",
                              field="eta")
            it.eta_at = None
            it.waiting_set_at = None

        # A job in flight rides on an active item; leaving active clears it.
        # On a strict board it needs an eta, so it always expires.
        if status != "active":
            if job_given:
                raise invalid("invalid_field",
                              f'job requires status active: item_set({n}, "active", eta="40m", '
                              f'job="...")', field="job")
            it.job = None
        elif strict_board and it.job and it.eta_at is None:
            raise invalid("eta_required",
                          f"#{n}: a job needs an eta on board {self.board['slug']}: "
                          f'item_set({n}, "active", eta="40m", job="{it.job}")', field="eta")

        # The unlaunched clock: created or entering active, or its holders
        # changed (an assignment or a handback) while active. A holder's own
        # write (claiming it, declaring a job) is that holder on it.
        if status == "active" and (creating or prev_status != "active" or holders_changed):
            it.actionable_at = self.now
        if self.actor["pid"] in [h["pid"] for h in it.holders]:
            it.launched_at = self.now

        self._check_owner(it, status, new_holders if holders_changed else it.holders)
        it.status = status
        owner_now = next((h for h in it.holders if h.get("role") == "owner"), None)
        if status in CLOSED and prev_owner and status != prev_status:
            # Closing unassigns: no launch grant outlives the work.
            self.owner_changes.append((n, prev_owner, None))
        elif (owner_now or {}).get("pid") != prev_owner and status not in CLOSED:
            self.owner_changes.append((n, prev_owner, owner_now))
        if (owner_now and "pick_owner" in it.open_flags
                and not any(h.get("role") in (None, "holder") for h in it.holders)):
            self.resolve_flag(it, "pick_owner", "cleared")
        if strict_board:
            reason_ = strict_violation(it, self.board, self.now)
            if reason_:
                self.warnings.append(f"#{n} does not fit board {self.board['slug']} "
                                     f"({reason_}): {TRIAGE_FIX[reason_]}")
        if (status == OWNER_BLOCKED) != (prev_status == OWNER_BLOCKED) and not creating:
            self.owner_transitions.append((n, status == OWNER_BLOCKED))
        elif creating and status == OWNER_BLOCKED:
            self.owner_transitions.append((n, True))
        if n in self.deps_changed:
            self._check_cycles(it)
            self._clear_blocker_dropped(it)

        # Any status change settles a pending suggestion: it was acted on.
        if status != prev_status and not creating:
            self.resolve_flag(it, SUGGESTED, "cleared")

        # Close and reopen (rules 5-7).
        was_closed = prev_status in CLOSED
        if status in CLOSED and not was_closed:
            it.closed_at = self.now
            it.closed_by = self.actor.get("name") or self.actor["pid"]
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

        # Any board member's write is a check-in: it resets the stale clock
        # (the engine never writes items through apply).
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
                after = {k: new[k] for k in changed}
                if reopened:
                    # Recorded: which writes reopened finished work, and how.
                    after["reopened"] = "owner" if is_owner and not reopen else "reopen=true"
                self._event(it, "updated", prev={k: prev[k] for k in changed},
                            new=after, reason=reason)
            else:
                self._event(it, "touched", new={"via": "item_set"}, reason=reason)

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
            dep.status = "active" if dep.holders or strict(self.board) else "open"
            dep.clock_reset_at = self.now
            if dep.status == "active":
                dep.actionable_at = self.now
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

    def touch(self, ns: list[int], *, source: str, message_id: str | None = None,
              excerpt: str | None = None, any_item: bool = False,
              suggest: dict | None = None) -> tuple[list[int], list[dict]]:
        """Observed activity on items (a holder's chat post naming `#N`):
        reset the stale clock and append history. Never changes status or
        note. Only open items the actor holds are touched. With `any_item`
        (the board's steward or orchestrator) a post about someone else's
        item is recorded in history but resets no clock: only a holder's
        touch counts as progress, so nagging never hides a stall."""
        touched, skipped = [], []
        for n in dict.fromkeys(ns):
            it = self.items.get(n)
            if it is None or it.archived_at is not None:
                skipped.append({"n": n, "reason": "not_found"})
            elif it.status in CLOSED:
                skipped.append({"n": n, "reason": "closed"})
            elif not any_item and self.actor["pid"] not in [h["pid"] for h in it.holders]:
                skipped.append({"n": n, "reason": "not_holder"})
            else:
                if self.actor["pid"] in [h["pid"] for h in it.holders]:
                    it.touched_at = self.now
                    it.launched_at = self.now
                    self.dirty.add(n)
                self._event(it, f"{source}_ref", new={
                    "message_id": message_id,
                    "excerpt": (excerpt or "")[:200] or None,
                })
                touched.append(n)
                if suggest:
                    self._suggest(it, suggest, message_id)
        return touched, skipped

    def _suggest(self, it: Item, suggest: dict, message_id: str | None):
        """Raise a suggested transition from a keyword in a progress post;
        only the holders and the steward hear about it. A suggestion with a
        new target settles the old one and is raised (and pushed) afresh, so
        `apply` can never execute a target nobody was shown. A dismissed
        target is not suggested again until the item's status or head moves."""
        to = suggest.get("status")
        keyword = str(suggest.get("keyword") or "")[:20]
        if to not in SUGGEST_STATUSES or it.status == to or not keyword:
            return
        if it.status == OWNER_BLOCKED:
            return  # waiting on Owner's decision: not the poster's to close
        dismissed = it.flag_pushed.get(f"{SUGGESTED}:dismissed:{to}")
        if dismissed and dismissed.get("status") == it.status and dismissed.get("head") == it.gate_head:
            return
        who = self.actor.get("name") or self.actor["pid"]
        current = it.open_flags.get(SUGGESTED)
        if current and (current.get("detail") or {}).get("to") != to:
            self.resolve_flag(it, SUGGESTED, "superseded")
            current = None
        detail = {"to": to, "keyword": keyword, "by": who, "message_id": message_id}
        self.raise_flag(it, SUGGESTED, detail)
        if current:
            return  # same target again: detail refreshed, no second push
        text = (f"{self._prefix()} #{it.n} \"{it.title}\": {who} posted {keyword}. Mark it {to}? "
                f"item_resolve({it.n}, \"apply\", to=\"{to}\") or item_resolve({it.n}, \"dismiss\").")
        audience = list(it.holders)
        steward = self.board.get("steward_pid")
        if steward and steward not in [h["pid"] for h in audience]:
            parts = steward.split(":")
            if len(parts) == 3 and parts[0] == "agent":
                audience.append({"pid": steward, "daemon_id": parts[1], "session_uid": parts[2]})
        for h in audience:
            if h["pid"] != self.actor["pid"]:
                self._push(it, h, SUGGESTED, text)

    def resolve(self, n: int, action: str, *, kind: str | None = None, holders=None,
                blocked_by=None, blocked_on=None, check_back=None, reason=None,
                message=None, to: str | None = None) -> list[str]:
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
        if action == "answer":
            if it.status != OWNER_BLOCKED:
                raise invalid("invalid_field", f"#{n} is not blocked on Owner", field="action")
            answer = _text(message, MAX_NOTE, "message", required=True)
            resolved = [k for k in sorted(it.open_flags) if self.resolve_flag(it, k, "answer")]
            self.apply(n, {"status": "active" if it.holders or strict(self.board) else "open",
                           "note": f"Owner: {answer}"},
                       reason=f"answer: {answer}", answered=True)
            self._event(it, "resolved", new={"action": action, "flags": resolved},
                        reason=f"answer: {answer}")
            return resolved
        if action in ("apply", "dismiss"):
            flag = it.open_flags.get(SUGGESTED)
            if not flag:
                raise invalid("invalid_field", f"#{n} has no suggested transition", field="action")
            target = (flag.get("detail") or {}).get("to")
            if action == "apply":
                # The caller confirms the target it was shown; a suggestion
                # that changed since is refused, never executed.
                if not to:
                    raise invalid("invalid_field", f"apply needs to=\"{target}\" (the suggestion "
                                  "you are confirming)", field="to")
                if to != target:
                    raise ItemsError(409, "suggestion_changed",
                                     f"#{n}'s suggestion is now {target}, not {to}", n=n, to=target)
            self.resolve_flag(it, SUGGESTED, action)
            if action == "dismiss":
                it.flag_pushed[f"{SUGGESTED}:dismissed:{target}"] = {
                    "status": it.status, "head": it.gate_head, "at": iso(self.now)}
                self.dirty.add(n)
            if action == "apply":
                self.apply(n, {"status": target}, reason=reason or
                           f"confirmed suggestion ({(flag.get('detail') or {}).get('keyword')})")
            self._event(it, "resolved", new={"action": action, "flags": [SUGGESTED]}, reason=reason)
            return [SUGGESTED]
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
            it.touched_at = self.now  # a member acted on it
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
