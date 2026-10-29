"""Board flag engine (doc/items-board.md §4, §5).

`compute_flags` is tested as a pure function; the tick, pushes, escalation and
close-out run against Postgres when CM_ITEMS_TEST_DSN is set (see
test_items_api.py for how to start a scratch cluster).
"""

from __future__ import annotations

import os
import unittest
import uuid
from datetime import datetime, timedelta, timezone

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

from api import board_engine  # noqa: E402
from api.board_engine import compose, compute_flags, holder_status  # noqa: E402
from dispatch import db, items_db  # noqa: E402
from dispatch.items_rules import Item  # noqa: E402

NOW = datetime(2026, 10, 7, 12, 0, tzinfo=timezone.utc)
BOARD = {"idle_s": 1200, "stale_s": 7200, "unassigned_s": 300, "repush_s": 1800,
         "escalate_s": 3600, "digest_s": 300, "holder_idle_enabled": True}
LANE = {"pid": "agent:d1:lane", "name": "lane", "session_uid": "lane", "daemon_id": "d1"}
RL = {"pid": "agent:d1:rl", "name": "rl", "session_uid": "rl", "daemon_id": "d1"}
LIVE = {"d1"}


def ago(**kw):
    return NOW - timedelta(**kw)


def state(st="working", since=None, reported=None, exited=False):
    return {"state": st, "state_since": since or ago(minutes=1),
            "idle_since": since if st == "idle" else None,
            "reported_at": reported or ago(seconds=10),
            "exited_at": ago(minutes=5) if exited else None}


def item(n=1, status="active", holders=(LANE,), touched=None, **kw):
    return Item(n=n, title=f"t{n}", status=status, holders=list(holders),
                touched_at=touched or ago(minutes=1), created_at=ago(hours=5), **kw)


def flags(it, states=None, items=None, board=None, live=frozenset()):
    items = items or {it.n: it}
    live = LIVE if live is None else live
    return compute_flags(it, states or {}, items, {**BOARD, **(board or {})}, NOW, live)


class HolderStatus(unittest.TestCase):
    def test_states(self):
        self.assertEqual(holder_status(state("idle"), NOW, LIVE, "agent:d1:x"), "idle")
        self.assertEqual(holder_status(state(exited=True), NOW, LIVE, "agent:d1:x"), "gone")
        stale = state("idle", reported=ago(seconds=200))
        self.assertEqual(holder_status(stale, NOW, LIVE, "agent:d1:x"), "unknown")
        # Missing row: gone only when that holder's daemon is heartbeating.
        self.assertEqual(holder_status(None, NOW, LIVE, "agent:d1:x"), "gone")
        # Grace: a fresh exit or a holder added since the last beat is not gone yet.
        just_exited = {**state(exited=True), "exited_at": ago(seconds=30)}
        self.assertEqual(holder_status(just_exited, NOW, LIVE, "agent:d1:x"), "unknown")
        self.assertEqual(holder_status(None, NOW, LIVE, "agent:d1:x", ago(seconds=20)), "unknown")
        self.assertEqual(holder_status(None, NOW, LIVE, "agent:d1:x", ago(minutes=5)), "gone")
        self.assertEqual(flags(item(), live=LIVE), {"holder_gone": {"holders": ["lane"]}})
        self.assertEqual(holder_status(None, NOW, LIVE, "agent:d9:x"), "unknown")
        self.assertEqual(holder_status(None, NOW, LIVE, "owner"), "unknown")


class ComputeFlags(unittest.TestCase):
    def test_unassigned_after_threshold(self):
        self.assertEqual(flags(item(status="open", holders=(), touched=ago(minutes=6))),
                         {"unassigned": None})
        self.assertEqual(flags(item(status="open", holders=(), touched=ago(minutes=4))), {})

    def test_holder_gone_even_when_blocked_but_not_unknown(self):
        blocker = item(2)
        it = item(status="blocked", blocked_by={2})
        self.assertEqual(flags(it, {LANE["pid"]: state(exited=True)}, {1: it, 2: blocker}),
                         {"holder_gone": {"holders": ["lane"]}})
        stale = {LANE["pid"]: state("idle", reported=ago(minutes=5))}
        self.assertEqual(flags(item(), stale), {})

    def test_idle_needs_every_holder_idle_past_threshold(self):
        both = item(holders=(LANE, RL), touched=ago(hours=1))
        idle = {LANE["pid"]: state("idle", ago(minutes=30)), RL["pid"]: state("idle", ago(minutes=25))}
        self.assertEqual(flags(both, idle), {"holder_idle": {"holders": ["lane", "rl"]}})
        one_working = {**idle, RL["pid"]: state("working")}
        self.assertEqual(flags(both, one_working), {})
        recent = {**idle, RL["pid"]: state("idle", ago(minutes=5))}
        self.assertEqual(flags(both, recent), {})

    def test_idle_clock_restarts_on_touch_and_unblock(self):
        idle = {LANE["pid"]: state("idle", ago(hours=1))}
        self.assertEqual(flags(item(touched=ago(minutes=5)), idle), {})
        self.assertEqual(flags(item(touched=ago(hours=1), clock_reset_at=ago(minutes=5)), idle), {})

    def test_unknown_is_never_idle_and_idle_flag_is_off_by_default(self):
        unknown = {LANE["pid"]: state("idle", ago(hours=1), reported=ago(minutes=5))}
        self.assertEqual(flags(item(touched=ago(hours=1)), unknown), {})
        idle = {LANE["pid"]: state("idle", ago(hours=1))}
        self.assertEqual(flags(item(touched=ago(hours=1)), idle,
                               board={"holder_idle_enabled": False}), {})

    def test_waiting_before_overdue_is_not_idle_then_overdue(self):
        idle = {LANE["pid"]: state("idle", ago(hours=1))}
        w = item(status="waiting", touched=ago(hours=1), eta_at=ago(minutes=-10),
                 waiting_set_at=ago(minutes=30))
        self.assertEqual(flags(w, idle), {})
        # eta 40m set 60m ago: passed at -20m, grace 10m -> overdue.
        late = item(status="waiting", touched=ago(hours=1), eta_at=ago(minutes=20),
                    waiting_set_at=ago(minutes=60))
        self.assertEqual(set(flags(late, idle)), {"overdue"})
        within = item(status="waiting", touched=ago(hours=1), eta_at=ago(minutes=5),
                      waiting_set_at=ago(minutes=45))
        self.assertEqual(flags(within, idle), {})

    def test_an_active_item_inside_its_eta_is_not_idle_stale_or_background_stale(self):
        idle = {LANE["pid"]: state("idle", ago(hours=5))}
        busy = {LANE["pid"]: {**state("working-background"), "reported_at": ago(seconds=5)}}
        inside = item(touched=ago(hours=8), eta_at=ago(minutes=-30), waiting_set_at=ago(hours=1))
        self.assertEqual(flags(inside, idle), {})
        self.assertEqual(flags(inside, busy), {})
        # Errored or gone holders end the window at once; errored still flags.
        errored = {LANE["pid"]: state("errored")}
        self.assertEqual(set(flags(inside, errored)), {"holder_errored", "stale"})
        gone = {LANE["pid"]: state(exited=True)}
        self.assertIn("holder_gone", flags(inside, gone, live={"d1"}))
        self.assertIn("stale", flags(inside, gone, live={"d1"}))
        waiting_human = {LANE["pid"]: state("waiting-on-human")}
        self.assertEqual(set(flags(inside, waiting_human)), {"holder_waiting_on_human"})

    def test_a_passed_eta_flags_once_then_stale_after_the_fixed_grace(self):
        idle = {LANE["pid"]: state("idle", ago(hours=5))}
        just = item(touched=ago(hours=3), eta_at=ago(minutes=10))
        self.assertEqual(flags(just, idle), {"eta_passed": {"eta_at": just.eta_at.isoformat()}})
        later = item(touched=ago(hours=3), eta_at=ago(minutes=31))
        got = flags(later, idle)
        self.assertEqual(set(got), {"eta_passed", "stale"})
        self.assertEqual(got["stale"], {"eta_at": later.eta_at.isoformat()})
        # Activity since the ETA clears both; normal clocks resume.
        touched = item(touched=ago(minutes=5), eta_at=ago(minutes=31))
        self.assertEqual(flags(touched, idle), {})

    def test_an_active_eta_is_capped_at_stale_background_s(self):
        # Review of f24e170: eta="30d" must not silence an item for weeks.
        idle = {LANE["pid"]: state("idle", ago(hours=9))}
        far = item(touched=ago(hours=8), eta_at=ago(days=-30), waiting_set_at=ago(hours=8))
        got = flags(far, idle, board={"stale_background_s": 6 * 3600})
        self.assertEqual(set(got), {"eta_passed", "stale"})
        self.assertEqual(got["eta_passed"]["eta_at"], ago(hours=2).isoformat())

    def test_stale_and_its_exemptions(self):
        self.assertEqual(flags(item(touched=ago(hours=3))), {"stale": None})
        blocker = item(2)
        chain = item(status="blocked", blocked_by={2}, touched=ago(hours=3))
        self.assertEqual(flags(chain, items={1: chain, 2: blocker}), {})
        text = item(status="blocked", blocked_on="EP GO", touched=ago(hours=3))
        self.assertEqual(flags(text), {"stale": None})
        # Waiting uses the larger of stale_s and its eta window.
        long_job = item(status="waiting", touched=ago(hours=3), eta_at=ago(hours=-2),
                        waiting_set_at=ago(hours=3))
        self.assertEqual(flags(long_job), {})

    def test_chain_end_is_still_flagged(self):
        end = item(2, touched=ago(hours=3))
        waiter = item(1, status="blocked", blocked_by={2}, touched=ago(hours=3))
        items = {1: waiter, 2: end}
        self.assertEqual(flags(waiter, items=items), {})
        self.assertEqual(flags(end, items=items), {"stale": None})

    def test_check_back_and_dropped_blocker_and_human_states(self):
        cb = item(status="blocked", blocked_on="JP", check_back_at=ago(minutes=1))
        self.assertIn("check_back", flags(cb))
        dropped = item(2, status="dropped")
        w = item(status="blocked", blocked_by={2})
        self.assertEqual(flags(w, items={1: w, 2: dropped}),
                         {"blocker_dropped": {"blockers": [2]}})
        human = {LANE["pid"]: state("waiting-on-human")}
        self.assertEqual(flags(item(), human),
                         {"holder_waiting_on_human": {"holders": ["lane"]}})

    def test_open_with_holders_is_treated_as_held(self):
        held = item(status="open", touched=ago(hours=3))
        self.assertEqual(flags(held), {"stale": None})
        gone = {LANE["pid"]: state(exited=True)}
        self.assertIn("holder_gone", flags(item(status="open"), gone))
        # Something blocked behind it stays exempt, but the chain end surfaces.
        waiter = item(2, status="blocked", blocked_by={1}, touched=ago(hours=3))
        both = {1: held, 2: waiter}
        self.assertEqual(flags(waiter, items=both), {})
        self.assertEqual(flags(held, items=both), {"stale": None})

    def test_blocked_on_owner_is_never_idle_or_stale_but_gone_still_counts(self):
        idle = {LANE["pid"]: state("idle", ago(hours=3))}
        owner = item(status="blocked_on_owner", blocked_on="approve?", touched=ago(hours=5))
        self.assertEqual(flags(owner, idle), {})
        gone = {LANE["pid"]: state(exited=True)}
        self.assertEqual(flags(owner, gone), {"holder_gone": {"holders": ["lane"]}})

    def test_closed_items_have_no_flags(self):
        self.assertEqual(flags(item(status="done", touched=ago(days=2))), {})


class StrictFlags(unittest.TestCase):
    """needs_triage, unlaunched, holder_overloaded (doc/strict-boards.md)."""

    STRICT = {"backlog_allowed": False, "launch_s": 900, "max_active_per_holder": 2,
              "check_back_max_s": 259200}

    def strict(self, it, states=None, items=None, live=frozenset(), **board):
        return flags(it, states, items, {**self.STRICT, **board}, live)

    def test_non_conforming_items_need_triage_and_nothing_on_other_boards(self):
        waiting = item(status="waiting", eta_at=NOW + timedelta(hours=1), waiting_set_at=ago(minutes=5))
        self.assertEqual(self.strict(waiting, {LANE["pid"]: state()}).get("needs_triage"),
                         {"reason": "waiting"})
        self.assertNotIn("needs_triage", flags(waiting, {LANE["pid"]: state()}))
        bare = item(status="blocked")
        self.assertEqual(self.strict(bare, {LANE["pid"]: state()})["needs_triage"],
                         {"reason": "no_trigger"})
        far = item(status="blocked", blocked_on="EP", check_back_at=NOW + timedelta(days=5))
        self.assertEqual(self.strict(far, {LANE["pid"]: state()})["needs_triage"],
                         {"reason": "check_back_too_far"})
        fine = item(status="blocked", blocked_on="EP", check_back_at=NOW + timedelta(hours=5))
        self.assertNotIn("needs_triage", self.strict(fine, {LANE["pid"]: state()}))

    def test_unlaunched_after_launch_s_when_nobody_live_is_on_it(self):
        unheld = item(holders=(), actionable_at=ago(minutes=16), touched=ago(minutes=16))
        self.assertEqual(self.strict(unheld),
                         {"unlaunched": {"actionable_at": ago(minutes=16).isoformat()}})
        self.assertEqual(self.strict(item(holders=(), actionable_at=ago(minutes=14))), {})
        # An idle holder that never touched it since it became actionable.
        idle = {LANE["pid"]: state("idle", since=ago(minutes=20))}
        assigned = item(actionable_at=ago(minutes=16), touched=ago(minutes=16))
        self.assertIn("unlaunched", self.strict(assigned, idle))
        # A holder that wrote or posted about it itself, or is working, or
        # unknown, is on it. Somebody else's touch (an orchestrator's note)
        # is not.
        noted = item(actionable_at=ago(minutes=16), touched=ago(minutes=3))
        self.assertIn("unlaunched", self.strict(noted, idle))
        touched = item(actionable_at=ago(minutes=16), touched=ago(minutes=3),
                       launched_at=ago(minutes=3))
        self.assertNotIn("unlaunched", self.strict(touched, idle))
        # The holder's own declaring write is at the same instant as the
        # clock start: that is the holder on it.
        claimed = item(actionable_at=ago(minutes=40), touched=ago(minutes=40),
                       launched_at=ago(minutes=40))
        self.assertNotIn("unlaunched", self.strict(claimed, idle))
        # Inside a declared job's ETA it is being worked, whoever touched it.
        job = item(actionable_at=ago(minutes=40), touched=ago(minutes=40), job="bench",
                   eta_at=NOW + timedelta(hours=1))
        self.assertNotIn("unlaunched", self.strict(job, idle))
        self.assertNotIn("unlaunched", self.strict(assigned, {LANE["pid"]: state("working")}))
        self.assertNotIn("unlaunched", self.strict(assigned))  # no state row: unknown
        # Never on other boards; never while blocked (a passed check-back
        # is not an unblock); a gone holder is holder_gone's.
        self.assertNotIn("unlaunched", flags(unheld))
        parked = item(status="blocked", holders=(), blocked_on="EP", check_back_at=ago(minutes=30),
                      actionable_at=ago(hours=2))
        self.assertEqual(set(self.strict(parked)), {"check_back"})
        gone = self.strict(assigned, {LANE["pid"]: state(exited=True)}, live=None)
        self.assertNotIn("unlaunched", gone)

    def test_overloaded_flags_the_extras_beyond_the_holders_focus(self):
        its = {n: item(n, touched=ago(minutes=n)) for n in (1, 2, 3)}
        states = {LANE["pid"]: state()}
        got = {n: self.strict(its[n], states, its).get("holder_overloaded") for n in its}
        # #1 and #2 were touched most recently: the focus. #3 is the extra.
        self.assertEqual(got, {1: None, 2: None, 3: {"holders": ["lane"]}})
        # An item inside its job's eta does not count.
        its[1].eta_at = NOW + timedelta(minutes=30)
        self.assertIsNone(self.strict(its[3], states, its).get("holder_overloaded"))
        # The threshold is a board setting; the orchestrator is exempt.
        its[1].eta_at = None
        self.assertIsNone(self.strict(its[3], states, its, max_active_per_holder=3)
                          .get("holder_overloaded"))
        self.assertIsNone(self.strict(its[3], states, its, resolved_orchestrator_pid=LANE["pid"])
                          .get("holder_overloaded"))
        self.assertNotIn("holder_overloaded", flags(its[3], states, its))


class Compose(unittest.TestCase):
    def test_unassigned_reports_item_age_not_flag_age(self):
        it = item(status="open", holders=(), touched=ago(minutes=7))
        flag = {"kind": "unassigned", "n": 1, "detail": None, "raised_at": NOW}
        self.assertEqual(board_engine.describe(flag, it, {}, NOW), "#1 unassigned (no holder 7m)")

    def test_truncated_flag_push_keeps_complete_count_and_recovery_instruction(self):
        flags = [f"#{n} stale (" + "long detail " * 9 + ")" for n in range(16)]
        text = compose("sfd", flags, {"done": list(range(30))})
        self.assertLessEqual(len(text), 1000)
        shown = sum(part in text for part in flags)
        self.assertGreater(shown, 0)
        self.assertLess(shown, len(flags))
        self.assertIn(f"+{len(flags) - shown} more: board(board=\"sfd\", flags_only=true)", text)
        self.assertTrue(text.endswith('flags_only=true)'))

    def test_truncated_single_flag_and_tiny_limit_do_not_misdirect(self):
        text = compose("sfd", ["x" * 2000], {"done": [9]})
        self.assertIn("1 flag · 1 done", text)
        self.assertNotIn("flags: ·", text)
        self.assertTrue(text.endswith('+1 more: board(board="sfd", flags_only=true)'))
        tiny = compose("other-board", ["x" * 200], {}, limit=25)
        self.assertLessEqual(len(tiny), 25)
        self.assertNotIn("board(flags_only=true)", tiny)
        self.assertIn("+1 more", tiny)

    def test_shape_and_limit(self):
        text = compose("sfd", ["#14 holder_idle (rl idle 24m)"], {"done": [9, 11]})
        self.assertEqual(text, '[cm-board sfd] 1 flag: #14 holder_idle (rl idle 24m) · 2 done '
                               '(#9,#11). board(board="sfd") / item_resolve(n, action)')
        many = compose("sfd", [f"#{n} stale (untouched 3h)" for n in range(200)], {})
        self.assertLessEqual(len(many), 1000)
        self.assertIn("more", many)
        digest = compose("sfd", [], {"done": list(range(1, 15))})
        self.assertIn("14 done (#1,#2,#3,#4,#5,#6,#7,#8,#9,#10,+4)", digest)


DSN = os.environ.get("CM_ITEMS_TEST_DSN")
ORCH = {"pid": "agent:d1:orch-uid", "name": "orch", "session_uid": "orch-uid", "daemon_id": "d1"}
HOLDER = {"pid": "agent:d1:lane-uid", "name": "lane", "session_uid": "lane-uid", "daemon_id": "d1"}


@unittest.skipUnless(DSN, "set CM_ITEMS_TEST_DSN to a scratch Postgres database")
class EngineDb(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        import asyncpg
        self.pool = await asyncpg.create_pool(DSN, min_size=1, max_size=4,
                                              init=db._init_connection)
        async with self.pool.acquire() as conn:
            await conn.execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        await db.init_db(self.pool)
        async with self.pool.acquire() as conn:
            self.root = str(await conn.fetchval(
                "INSERT INTO tasks (repo_url, prompt, name) VALUES ('r','p','root') RETURNING id"))
        self.board = await items_db.resolve_board(self.pool, task_id=self.root)
        self.ref = self.board["slug"]
        # Most tests are about flags: with no grace, stale nudges the holder
        # and flags on the next pass (tick() runs two). Nudge tests set it.
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE boards SET nudge_grace_s = 0")

    async def asyncTearDown(self):
        await self.pool.close()

    async def sql(self, q, *args):
        async with self.pool.acquire() as conn:
            return await conn.fetch(q, *args)

    async def beat(self, *rows):
        await items_db.heartbeat(self.pool, "d1", host_label="h", sessions=list(rows),
                                 exited=[], acked_push_ids=[])

    def row(self, who, st, age_s=0, task=None):
        return {"pid": who["pid"], "session_uid": who["session_uid"], "name": who["name"],
                "task_id": task, "state": st, "state_age_s": age_s,
                "idle_for_s": age_s if st == "idle" else None}

    async def age(self, minutes):
        """Move the board's clocks into the past instead of waiting."""
        d = timedelta(minutes=minutes)
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE items SET touched_at = touched_at - $1::interval, created_at = created_at - $1::interval", d)
            await conn.execute("UPDATE items SET actionable_at = actionable_at - $1::interval", d)
            await conn.execute("UPDATE item_flags SET raised_at = raised_at - $1::interval, "
                               "last_pushed_at = last_pushed_at - $1::interval", d)
            await conn.execute("UPDATE boards SET last_digest_at = last_digest_at - $1::interval", d)
            await conn.execute("UPDATE item_holders SET added_at = added_at - $1::interval", d)
            await conn.execute("UPDATE items SET stale_nudged_at = stale_nudged_at - $1::interval, "
                               "check_back_at = check_back_at - $1::interval", d)
            # The engine's "pushed at" marks move with the other clocks.
            for r in await conn.fetch("SELECT id, flag_pushed FROM items"):
                marks = {k: ({**v, "at": (datetime.fromisoformat(v["at"]) - d).isoformat()}
                             if isinstance(v, dict) and v.get("at") else v)
                         for k, v in (r["flag_pushed"] or {}).items()}
                await conn.execute("UPDATE items SET flag_pushed = $2 WHERE id = $1", r["id"], marks)

    async def pushes(self, kind=None):
        rows = await self.sql("SELECT kind, session_uid, text, owner_alert FROM item_pushes ORDER BY id")
        return [dict(r) for r in rows if kind is None or r["kind"] == kind]

    async def tick(self, passes: int = 2):
        total = {"raised": [], "cleared": [], "nudged": [], "pushed": False, "escalated": False}
        for _ in range(passes):
            s = await board_engine.tick_board(self.pool, self.board["id"])
            for k in ("raised", "cleared", "nudged"):
                total[k] += s.get(k, [])
            total["pushed"] |= s["pushed"]
            total["escalated"] |= s["escalated"]
        return total

    async def test_raise_push_orchestrator_then_clear(self):
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.age(150)
        s = await self.tick()
        self.assertEqual(s["raised"], [(1, "stale")])
        board_push = await self.pushes("board")
        self.assertEqual(len(board_push), 1)
        self.assertEqual(board_push[0]["session_uid"], "orch-uid")
        self.assertIn("#1 stale (untouched 2h30m)", board_push[0]["text"])
        # Never re-pushed on a timer: the item has not changed.
        await self.tick()
        await self.age(31)
        await self.tick()
        self.assertEqual(len(await self.pushes("board")), 1)
        # Touching the item clears the flag.
        await items_db.update_items(self.pool, self.ref, HOLDER, [1], {"note": "still on it"})
        s = await self.tick()
        self.assertEqual(s["cleared"], [(1, "stale")])
        flag = (await self.sql("SELECT resolution, resolved_by FROM item_flags"))[0]
        self.assertEqual((flag["resolution"], flag["resolved_by"]), ("cleared", "system:board-engine"))
        types = [r["type"] for r in await self.sql("SELECT type FROM item_events ORDER BY id")]
        self.assertIn("flag_raised", types)
        self.assertIn("flag_resolved", types)

    async def test_nudge_snooze_blocks_reraise(self):
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.age(150)
        await self.tick()
        await items_db.resolve_item(self.pool, self.ref, ORCH, 1, "nudge")
        s = await self.tick()
        self.assertEqual(s["raised"], [])
        # The nudge also touched the item; once it is stale again, the snooze
        # still holds the flag back until it expires.
        await self.age(150)
        self.assertEqual((await self.tick())["raised"], [])
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE item_flags SET snooze_until = now() - interval '1 s'")
        s = await self.tick()
        self.assertEqual(s["raised"], [(1, "stale")])

    async def test_holder_gone_and_overdue_push_holder_first(self):
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [
            {"title": "job", "holders": [HOLDER], "status": "waiting", "eta": "10m"},
            {"title": "gone", "holders": [{"pid": "agent:d1:dead-uid", "name": "dead",
                                           "session_uid": "dead-uid", "daemon_id": "d1"}]},
        ])
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE items SET eta_at = now() - interval '10 min', "
                               "waiting_set_at = now() - interval '20 min' WHERE number = 1")
        s = await self.tick()
        self.assertEqual(s["raised"], [(1, "overdue")])  # dead-uid was only just added
        await self.age(2)
        s = await self.tick()
        self.assertEqual(s["raised"], [(2, "holder_gone")])
        overdue = await self.pushes("overdue")
        self.assertEqual([p["session_uid"] for p in overdue], ["lane-uid"])
        board_text = (await self.pushes("board"))[0]["text"]
        self.assertIn("#2 holder_gone (dead gone)", board_text)
        self.assertNotIn("overdue", board_text)  # orchestrator hears after repush_s

    async def test_digest_batches_done_and_dropped(self):
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH,
                                    [{"title": t, "holders": [HOLDER]} for t in "abc"])
        await self.tick()
        await items_db.update_items(self.pool, self.ref, HOLDER, [1, 2], {"status": "done"})
        await items_db.update_items(self.pool, self.ref, HOLDER, [3], {"status": "dropped"})
        await self.tick()  # digest_s has not passed since the first tick
        self.assertEqual(await self.pushes("board"), [])
        await self.age(6)
        await self.tick()
        (push,) = await self.pushes("board")
        self.assertIn("2 done (#1,#2) · 1 dropped (#3)", push["text"])
        await self.age(6)
        await self.tick()
        self.assertEqual(len(await self.pushes("board")), 1)

    async def test_escalates_once_when_orchestrator_is_idle(self):
        await self.beat(self.row(ORCH, "idle", 3600, task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.age(150)
        await self.tick()
        self.assertEqual(await self.pushes("escalation"), [])
        await self.age(61)
        s = await self.tick()
        self.assertTrue(s["escalated"])
        (alert,) = await self.pushes("escalation")
        self.assertTrue(alert["owner_alert"])
        self.assertEqual(alert["session_uid"], "orch-uid")
        self.assertIn("while orch is idle", alert["text"])
        await self.age(61)
        await self.tick()
        self.assertEqual(len(await self.pushes("escalation")), 1)

    async def test_a_push_carries_only_flags_whose_item_changed(self):
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.age(150)
        await self.tick()                                   # #1 stale: push 1
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "b", "holders": [HOLDER]}])
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE items SET touched_at = now() - interval '3 hours' WHERE number = 2")
        await self.age(10)
        await self.tick()                                   # #2 new: push 2 is only #2
        second = (await self.pushes("board"))[-1]["text"]
        self.assertIn("1 flag: #2 stale", second)
        await self.age(120)
        await self.tick()
        self.assertEqual(len(await self.pushes("board")), 2, "nothing changed, nothing pushed")

    async def test_a_flapping_background_flag_is_pushed_once_per_item_state(self):
        # EP, 2026-10-20: the same stale_background flags arrived over and
        # over. A holder flickering between background work and idle clears
        # and re-raises the flag; with no change to the item it stays quiet.
        busy = self.row(HOLDER, "working-background")
        await self.beat(self.row(ORCH, "working", task=self.root), busy)
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.age(7 * 60)
        s = await self.tick()
        self.assertEqual(s["raised"], [(1, "stale_background")])
        self.assertEqual(len(await self.pushes("board")), 1)
        for _ in range(3):
            await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
            await self.tick()
            await self.beat(self.row(ORCH, "working", task=self.root), busy)
            s = await self.tick()
        self.assertEqual(s["raised"], [(1, "stale_background")], "it really did come back")
        texts = [p["text"] for p in await self.pushes("board")]
        self.assertEqual(sum("stale_background" in t for t in texts), 1, texts)
        # A change to the item (a holder's update) makes it news again.
        await items_db.update_items(self.pool, self.ref, HOLDER, [1], {"note": "still running"})
        await self.age(8 * 60)  # (the outbox dedupes identical text within a minute)
        await self.tick()
        texts = [p["text"] for p in await self.pushes("board")]
        self.assertEqual(sum("stale_background" in t for t in texts), 2, texts)

    async def test_a_second_episode_of_an_episodic_flag_is_pushed_again(self):
        # Review of f24e170: a second permission prompt after recovery has
        # the same detail (holder names) and must still reach the orchestrator.
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        for _ in range(2):
            await self.beat(self.row(ORCH, "working", task=self.root),
                            self.row(HOLDER, "waiting-on-human"))
            s = await self.tick()
            self.assertIn((1, "holder_waiting_on_human"), s["raised"])
            await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
            s = await self.tick()
            self.assertIn((1, "holder_waiting_on_human"), s["cleared"])
        # Both episodes were pushed (the outbox itself folds identical text
        # inside one minute, which only a test reaches this fast).
        rows = await self.sql("SELECT push_count FROM item_flags "
                              "WHERE kind = 'holder_waiting_on_human' ORDER BY id")
        self.assertEqual([r["push_count"] for r in rows], [1, 1])

    async def test_an_active_eta_flags_eta_passed_once_then_stale(self):
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "idle", 3600))
        await items_db.create_items(self.pool, self.ref, ORCH, [
            {"title": "bench", "holders": [HOLDER], "status": "active", "eta": "2h"}])
        (row,) = await self.sql("SELECT status, eta_at FROM items")
        self.assertEqual(row["status"], "active")
        self.assertIsNotNone(row["eta_at"])
        await self.age(150)                                 # untouched 2.5h, inside the eta
        self.assertEqual((await self.tick())["raised"], [])
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE items SET eta_at = now() - interval '5 min'")
        s = await self.tick()
        self.assertEqual(s["raised"], [(1, "eta_passed")])
        self.assertEqual([p["session_uid"] for p in await self.pushes("eta_passed")], ["lane-uid"])
        await self.tick()
        self.assertEqual(len(await self.pushes("eta_passed")), 1, "once")
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE items SET eta_at = now() - interval '31 min'")
        s = await self.tick()
        self.assertEqual(s["raised"], [(1, "stale")], "no nudge round: the holder heard at the eta")
        # A new eta clears both.
        await items_db.update_items(self.pool, self.ref, HOLDER, [1], {"status": "active", "eta": "1h"})
        s = await self.tick()
        self.assertEqual(sorted(s["cleared"]), [(1, "eta_passed"), (1, "stale")])

    async def test_escalates_when_orchestrator_errored(self):
        await self.beat(self.row(ORCH, "errored", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.age(150)
        await self.tick()
        await self.age(61)
        await self.tick()
        (alert,) = await self.pushes("escalation")
        self.assertIn("while orch is errored", alert["text"])

    async def test_no_escalation_while_orchestrator_works(self):
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.age(150)
        await self.tick()
        await self.age(61)
        await self.tick()
        self.assertEqual(await self.pushes("escalation"), [])

    async def test_no_orchestrator_escalates_through_coordinators_last_session(self):
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.beat(self.row(HOLDER, "working"))  # orchestrator exited
        await self.age(150)
        await self.tick()
        self.assertEqual(await self.pushes("board"), [])  # nobody to push
        await self.age(61)
        await self.tick()
        (alert,) = await self.pushes("escalation")
        self.assertEqual(alert["session_uid"], "orch-uid")
        self.assertIn("with no orchestrator", alert["text"])

    async def test_engine_never_pushes_or_escalates_to_a_bash_pane(self):
        shell = {"pid": "agent:d1:shell-uid", "name": "shell", "session_uid": "shell-uid",
                 "daemon_id": "d1"}
        await self.beat({**self.row(shell, "working", task=self.root), "engine": "bash"},
                        {**self.row(ORCH, "working", task=self.root), "engine": "codex"},
                        self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.age(150)
        await self.tick()
        self.assertEqual([p["session_uid"] for p in await self.pushes("board")], ["orch-uid"])
        # With only the bash pane left on the coordinator task, escalation finds nobody.
        await self.beat({**self.row(shell, "working", task=self.root), "engine": "bash"},
                        self.row(HOLDER, "working"))
        async with self.pool.acquire() as conn:
            await conn.execute("DELETE FROM session_states WHERE pid = $1", ORCH["pid"])
        await self.age(61)
        await self.tick()
        self.assertEqual(await self.pushes("escalation"), [])

    async def test_stale_nudges_the_holder_first_then_flags_after_the_grace(self):
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE boards SET nudge_grace_s = 1800")
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.age(150)
        s = await self.tick(1)
        self.assertEqual((s["nudged"], s["raised"]), ([1], []))
        (nudge,) = await self.pushes("stale_nudge")
        self.assertEqual(nudge["session_uid"], "lane-uid")
        self.assertTrue(nudge["text"].startswith(f"[cm-board {self.ref}]"), nudge["text"])
        self.assertEqual(await self.pushes("board"), [])  # the orchestrator is not told yet
        s = await self.tick(1)
        self.assertEqual((s["nudged"], s["raised"]), ([], []))  # grace running
        await self.age(31)
        s = await self.tick(1)
        self.assertEqual(s["raised"], [(1, "stale")])
        self.assertEqual(len(await self.pushes("stale_nudge")), 1)

    async def test_a_touch_after_the_nudge_starts_a_new_episode(self):
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE boards SET nudge_grace_s = 1800")
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.age(150)
        await self.tick(1)
        await items_db.touch_items(self.pool, self.ref, HOLDER, [1], source="chat", message_id="m")
        self.assertEqual((await self.tick(1))["raised"], [])  # touched: not stale
        await self.age(150)
        s = await self.tick(1)
        self.assertEqual((s["nudged"], s["raised"]), ([1], []))  # nudged again, not flagged

    async def test_background_work_suppresses_stale_until_the_softer_cap(self):
        busy = {**self.row(HOLDER, "working-background")}
        await self.beat(self.row(ORCH, "working", task=self.root), busy)
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.age(150)
        s = await self.tick()
        self.assertEqual((s["raised"], s["nudged"]), ([], []))
        await self.age(6 * 60)
        s = await self.tick()
        self.assertEqual(s["raised"], [(1, "stale_background")])
        # A live background job that wakes the agent counts too.
        idle_with_job = {**self.row(HOLDER, "idle"),
                         "agent_state": {"background": {"jobs": [{"id": "j", "wakes_agent": True}]}}}
        now = datetime.now(timezone.utc)
        self.assertTrue(board_engine.background_busy(
            {**idle_with_job, "reported_at": now, "exited_at": None}, now))
        self.assertFalse(board_engine.background_busy(
            {**self.row(HOLDER, "idle"), "reported_at": now, "exited_at": None,
             "agent_state": {"background": {"jobs": [{"id": "t", "wakes_agent": False}]}}}, now))

    async def test_a_parked_item_is_not_stale_before_its_check_back(self):
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await items_db.update_items(self.pool, self.ref, ORCH, [1],
                                    {"status": "blocked", "blocked_on": "EP GO", "check_back": "4h"})
        await self.age(150)
        self.assertEqual((await self.tick())["raised"], [])
        await self.age(120)  # past the check-back
        raised = {k for _, k in (await self.tick())["raised"]}
        self.assertEqual(raised, {"check_back", "stale"})

    # ---- strict boards (doc/strict-boards.md) ----------------------------
    async def make_strict(self, **settings):
        await items_db.patch_board(self.pool, self.ref, {"pid": "owner", "name": "Owner"},
                                   {"backlog_allowed": False, **settings})

    async def test_needs_triage_reaches_the_orchestrator_once_and_never_owner(self):
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [
            {"title": "parked", "holders": [HOLDER], "status": "waiting", "eta": "3h"}])
        await self.make_strict()
        s = await self.tick()
        self.assertIn((1, "needs_triage"), s["raised"])
        (push,) = await self.pushes("board")
        self.assertIn("#1 needs_triage (waiting: not allowed on this board)", push["text"])
        self.assertEqual([p["kind"] for p in await self.pushes()], ["assigned", "board"],
                         "no needs_triage push to the holder")
        # Unchanged: never re-pushed, and never escalated, even with the
        # orchestrator idle past escalate_s.
        await self.beat(self.row(ORCH, "idle", 7200, task=self.root), self.row(HOLDER, "working"))
        await self.age(61)
        await self.tick()
        self.assertEqual(len(await self.pushes("board")), 1)
        self.assertEqual(await self.pushes("escalation"), [])
        # Making it conform clears it; so does flipping the board back.
        await items_db.update_items(self.pool, self.ref, HOLDER, [1],
                                    {"status": "active", "eta": "40m", "job": "bench"})
        s = await self.tick()
        self.assertIn((1, "needs_triage"), s["cleared"])
        job = await self.sql("SELECT job, status FROM items WHERE number = 1")
        self.assertEqual((job[0]["job"], job[0]["status"]), ("bench", "active"))

    async def test_unlaunched_names_free_sessions(self):
        free = {"pid": "agent:d1:free-uid", "name": "spare", "session_uid": "free-uid"}
        bash = {"pid": "agent:d1:sh-uid", "name": "bash", "session_uid": "sh-uid"}
        rows = [self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"),
                self.row(free, "idle", 600, task=self.root),
                {**self.row(bash, "idle", 600, task=self.root), "engine": "bash"}]
        await self.beat(*rows)
        await self.make_strict()
        await items_db.create_items(self.pool, self.ref, HOLDER, [{"title": "a"}])
        reply = await items_db.update_items(self.pool, self.ref, HOLDER, [1], {"holders": []})
        self.assertTrue(any("unlaunched" in w for w in reply["warnings"]), reply)
        await self.age(14)
        await self.tick()
        self.assertEqual(await self.pushes("board"), [])
        await self.age(2)
        s = await self.tick()
        self.assertIn((1, "unlaunched"), s["raised"])
        (push,) = await self.pushes("board")
        self.assertIn("#1 unlaunched (no live holder on it 16m; free: spare)", push["text"])
        board = await items_db.read_board(self.pool, self.ref)
        self.assertEqual(board["launch"]["free"], ["spare"])
        self.assertEqual([u["n"] for u in board["launch"]["unlaunched"]], [1])
        self.assertEqual([r["name"] for r in board["free_capacity"]], ["spare"])
        # A launch (a holder that touches it) clears it.
        await items_db.update_items(self.pool, self.ref, ORCH, [1], {"holders": [free]})
        await items_db.update_items(self.pool, self.ref, free, [1], {"note": "on it"})
        s = await self.tick()
        self.assertIn((1, "unlaunched"), s["cleared"])

    async def test_owners_add_on_a_strict_board_goes_unlaunched_to_the_orchestrator(self):
        await self.beat(self.row(ORCH, "working", task=self.root))
        await self.make_strict()
        reply = await items_db.create_items(self.pool, self.ref, {"pid": "owner", "name": "Owner"},
                                            [{"title": "from Owner", "status": "open"}])
        self.assertEqual([(i["status"], i["holders"]) for i in reply["items"]], [("active", [])])
        with self.assertRaises(items_db.ItemsError):
            await items_db.create_items(self.pool, self.ref, HOLDER,
                                        [{"title": "agent backlog", "holders": []}])
        await self.age(16)
        s = await self.tick()
        self.assertIn((1, "unlaunched"), s["raised"])
        (push,) = await self.pushes("board")
        self.assertIn("#1 unlaunched", push["text"])

    async def test_a_missed_overload_notice_is_sent_on_the_next_pass(self):
        await self.beat(self.row(HOLDER, "working"))  # no orchestrator on the board
        await self.make_strict()
        await items_db.create_items(self.pool, self.ref, HOLDER,
                                    [{"title": t} for t in ("a", "b", "c")])
        s = await self.tick()
        self.assertIn((1, "holder_overloaded"), s["raised"])
        self.assertEqual(await self.pushes("board"), [])
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await self.tick()
        (push,) = await self.pushes("board")
        self.assertIn("#1 holder_overloaded", push["text"])
        await self.age(6)
        await self.tick()
        self.assertEqual(len(await self.pushes("board")), 1, "once per episode")

    async def test_unlaunched_respects_a_declared_job_and_never_pages_owner(self):
        idle_lane = self.row(HOLDER, "idle", 3600)
        await self.beat(self.row(ORCH, "idle", 7200, task=self.root), idle_lane)
        await self.make_strict()
        # The holder's own item with a job and its eta: the declaring write
        # is the holder on it, and inside the eta it is being worked.
        await items_db.create_items(self.pool, self.ref, HOLDER, [
            {"title": "bench", "eta": "2h", "job": "C2 run"}])
        # Assigned by the orchestrator to an idle holder that never starts.
        await items_db.create_items(self.pool, self.ref, ORCH, [
            {"title": "assigned", "holders": [HOLDER]}])
        await self.age(20)
        s = await self.tick()
        self.assertEqual([n for n, k in s["raised"] if k == "unlaunched"], [2])
        await self.age(61)
        await self.tick()
        self.assertEqual(await self.pushes("escalation"), [],
                         "unlaunched is the orchestrator's, never an Owner page")
        # The holder posting #2 is it on the item.
        await items_db.touch_items(self.pool, self.ref, HOLDER, [2], source="chat")
        s = await self.tick()
        self.assertIn((2, "unlaunched"), s["cleared"])

    async def test_overloaded_holder_hears_once_per_episode(self):
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await self.make_strict()
        await items_db.create_items(self.pool, self.ref, HOLDER,
                                    [{"title": t} for t in ("a", "b", "c", "d")])
        # A fifth inside its job's eta is counted neither by the flag nor by
        # the holder's push.
        await items_db.create_items(self.pool, self.ref, HOLDER,
                                    [{"title": "e", "eta": "3h", "job": "bench"}])
        s = await self.tick()
        self.assertEqual(sorted(n for n, k in s["raised"] if k == "holder_overloaded"), [1, 2])
        (mine,) = await self.pushes("holder_overloaded")
        self.assertEqual(mine["session_uid"], "lane-uid")
        self.assertIn("you hold 4 active items (this board allows 2", mine["text"])
        (board_push,) = await self.pushes("board")
        self.assertIn("holder_overloaded", board_push["text"])
        await self.tick()
        self.assertEqual(len(await self.pushes("holder_overloaded")), 1)
        # The focus moving (the holder touches #1, so #1's flag moves to #3)
        # is the same episode: no new push to anyone.
        await self.age(6)
        await items_db.update_items(self.pool, self.ref, HOLDER, [1], {"note": "on #1 now"})
        s = await self.tick()
        self.assertIn((3, "holder_overloaded"), s["raised"])
        self.assertIn((1, "holder_overloaded"), s["cleared"])
        self.assertEqual(len(await self.pushes("holder_overloaded")), 1)
        self.assertFalse(any("holder_overloaded" in p["text"] for p in (await self.pushes("board"))[1:]))
        # An item inside its job's eta is not counted in the holder's push.
        await items_db.update_items(self.pool, self.ref, HOLDER, [3], {"eta": "1h", "job": "j"})
        await self.beat(self.row(ORCH, "idle", 7200, task=self.root), self.row(HOLDER, "working"))
        await self.age(61)
        s = await self.tick()
        self.assertIn((3, "holder_overloaded"), s["cleared"], "inside its eta it doesn't count")
        self.assertEqual(await self.pushes("escalation"), [])
        # Raising the board's threshold ends the episode.
        await items_db.patch_board(self.pool, self.ref, {"pid": "owner"}, {"max_active_per_holder": 4})
        s = await self.tick()
        self.assertEqual(sorted(n for n, k in s["cleared"] if k == "holder_overloaded"), [2])

    async def test_close_out_archives_and_prunes(self):
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a"}, {"title": "b"}])
        await items_db.update_items(self.pool, self.ref, ORCH, [1, 2], {"status": "done"})
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE items SET closed_at = now() - interval '25 hours' WHERE number = 1")
            await conn.execute("INSERT INTO item_requests (board_id, request_id, actor_pid, numbers, created_at) "
                               "VALUES ($1, 'old', 'x', '{}', now() - interval '8 days')",
                               uuid.UUID(self.board["id"]))
        await board_engine.close_out(self.pool)
        rows = {r["number"]: r["archived_at"] for r in await self.sql("SELECT number, archived_at FROM items")}
        self.assertIsNotNone(rows[1])
        self.assertIsNone(rows[2])
        self.assertEqual(await self.sql("SELECT 1 FROM item_requests"), [])
        self.assertEqual(await board_engine.active_boards(self.pool), [self.board["id"]])


if __name__ == "__main__":
    unittest.main()
