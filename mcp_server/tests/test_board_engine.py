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

    def test_closed_items_have_no_flags(self):
        self.assertEqual(flags(item(status="done", touched=ago(days=2))), {})


class Compose(unittest.TestCase):
    def test_unassigned_reports_item_age_not_flag_age(self):
        it = item(status="open", holders=(), touched=ago(minutes=7))
        flag = {"kind": "unassigned", "n": 1, "detail": None, "raised_at": NOW}
        self.assertEqual(board_engine.describe(flag, it, {}, NOW), "#1 unassigned (no holder 7m)")

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
            await conn.execute("UPDATE item_flags SET raised_at = raised_at - $1::interval, "
                               "last_pushed_at = last_pushed_at - $1::interval", d)
            await conn.execute("UPDATE boards SET last_digest_at = last_digest_at - $1::interval", d)
            await conn.execute("UPDATE item_holders SET added_at = added_at - $1::interval", d)

    async def pushes(self, kind=None):
        rows = await self.sql("SELECT kind, session_uid, text, owner_alert FROM item_pushes ORDER BY id")
        return [dict(r) for r in rows if kind is None or r["kind"] == kind]

    async def tick(self):
        return await board_engine.tick_board(self.pool, self.board["id"])

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
        # No re-push before repush_s.
        await self.tick()
        self.assertEqual(len(await self.pushes("board")), 1)
        await self.age(31)
        await self.tick()
        self.assertEqual(len(await self.pushes("board")), 2)
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

    async def test_flags_raised_at_different_times_share_one_repush(self):
        await self.beat(self.row(ORCH, "working", task=self.root), self.row(HOLDER, "working"))
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "a", "holders": [HOLDER]}])
        await self.age(150)
        await self.tick()                                   # #1 stale: push 1
        await items_db.create_items(self.pool, self.ref, ORCH, [{"title": "b", "holders": [HOLDER]}])
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE items SET touched_at = now() - interval '3 hours' WHERE number = 2")
        await self.age(10)
        await self.tick()                                   # #2 new: push 2 carries both
        second = (await self.pushes("board"))[-1]["text"]
        self.assertIn("2 flags", second)
        await self.age(25)
        await self.tick()                                   # #1 would be due alone: not yet
        self.assertEqual(len(await self.pushes("board")), 2)
        await self.age(6)
        await self.tick()
        boards = await self.pushes("board")
        self.assertEqual(len(boards), 3)
        self.assertIn("2 flags", boards[-1]["text"])

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
