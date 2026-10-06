"""Item write rules (doc/items-board.md §2), exercised without Postgres."""

from __future__ import annotations

import unittest
from datetime import datetime, timedelta, timezone

from dispatch.items_rules import BoardTx, Item, ItemsError, parse_when

NOW = datetime(2026, 10, 6, 12, 0, tzinfo=timezone.utc)


def agent(name, daemon="d1"):
    return {"pid": f"agent:{daemon}:{name}-uid", "name": name,
            "session_uid": f"{name}-uid", "daemon_id": daemon}


ORCH = agent("orch")
LANE = agent("lane")
OTHER = agent("other", daemon="d2")
OWNER = {"pid": "owner", "name": "Owner"}


def board(**over):
    b = {"id": "b-1", "slug": "sfd", "next_number": 1, "idle_s": 1200, "stale_s": 7200,
         "unassigned_s": 300, "repush_s": 1800, "escalate_s": 3600, "digest_s": 300}
    b.update(over)
    return b


class Board:
    """Carries state across transactions the way items_db reloads it."""

    def __init__(self):
        self.board = board()
        self.items: dict[int, Item] = {}

    def tx(self, actor=ORCH, now=NOW) -> BoardTx:
        return BoardTx(self.board, self.items, actor, now)

    def create(self, *specs, actor=ORCH):
        tx = self.tx(actor)
        tx.create([s if isinstance(s, dict) else {"title": s} for s in specs])
        return tx

    def set(self, ns, actor=ORCH, now=NOW, **fields):
        tx = self.tx(actor, now)
        add = fields.pop("add_holders", None)
        remove = fields.pop("remove_holders", None)
        reason = fields.pop("reason", None)
        tx.update(ns if isinstance(ns, list) else [ns], fields,
                  add_holders=add, remove_holders=remove, reason=reason)
        return tx

    def __getitem__(self, n) -> Item:
        return self.items[n]


class NumberingAndDefaults(unittest.TestCase):
    def test_numbers_are_sequential_and_never_reused(self):
        b = Board()
        tx = b.create("a", "b", "c")
        self.assertEqual(tx.created, [1, 2, 3])
        self.assertTrue(tx.board_changed)
        self.assertEqual(b.create("d").created, [4])
        self.assertEqual(b.board["next_number"], 5)

    def test_default_holder_is_the_caller_and_status_active(self):
        b = Board()
        b.create("x", actor=LANE)
        self.assertEqual([h["pid"] for h in b[1].holders], [LANE["pid"]])
        self.assertEqual(b[1].status, "active")
        self.assertEqual(b[1].created_by, LANE["pid"])

    def test_owner_or_empty_holders_make_it_open(self):
        b = Board()
        b.create("x", actor=OWNER)
        b.create({"title": "y", "holders": []})
        self.assertEqual(b[1].status, "open")
        self.assertEqual(b[2].status, "open")
        self.assertEqual(b[2].holders, [])

    def test_holder_none_opens_and_adding_a_holder_activates(self):
        b = Board()
        b.create("x")
        b.set(1, holders=[])
        self.assertEqual(b[1].status, "open")
        b.set(1, add_holders=[LANE])
        self.assertEqual(b[1].status, "active")

    def test_explicit_null_holders_clears(self):
        b = Board()
        b.create("x")
        tx = b.tx()
        tx.update([1], {"holders": None})
        self.assertEqual(b[1].holders, [])
        self.assertEqual(b[1].status, "open")

    def test_last_holder_leaving_blocked_or_waiting_keeps_status(self):
        b = Board()
        b.create("a", "b", "c")
        b.set(2, blocked_by=[1])
        tx = b.set(2, holders=[])
        self.assertEqual((b[2].status, b[2].blocked_by), ("blocked", {1}))
        self.assertTrue(tx.warnings)
        b.set(3, eta="2h")
        b.set(3, remove_holders=[ORCH["pid"]])
        self.assertEqual(b[3].status, "waiting")
        self.assertIsNotNone(b[3].eta_at)

    def test_out_of_range_blocker_refused(self):
        b = Board()
        b.create("a", "b")
        for bad in ([2**31], [0], [True]):
            with self.assertRaises(ItemsError) as cm:
                b.set(2, blocked_by=bad)
            self.assertEqual(cm.exception.status, 422)

    def test_title_is_required_and_bounded(self):
        b = Board()
        with self.assertRaises(ItemsError) as cm:
            b.create({"title": "   "})
        self.assertEqual(cm.exception.code, "invalid_field")
        with self.assertRaises(ItemsError):
            b.create("x" * 201)

    def test_note_is_one_line(self):
        b = Board()
        b.create({"title": "x", "note": "line one\nline two"})
        self.assertEqual(b[1].note, "line one line two")

    def test_unknown_status_refused(self):
        b = Board()
        b.create("x")
        with self.assertRaises(ItemsError) as cm:
            b.set(1, status="finished")
        self.assertEqual(cm.exception.code, "invalid_status")
        self.assertEqual(cm.exception.status, 422)

    def test_missing_item_is_404(self):
        with self.assertRaises(ItemsError) as cm:
            Board().set(9, note="x")
        self.assertEqual(cm.exception.status, 404)


class Blockers(unittest.TestCase):
    def setUp(self):
        self.b = Board()
        self.b.create("a", "b", "c")

    def test_blocked_by_sets_blocked(self):
        tx = self.b.set(2, blocked_by=[1])
        self.assertEqual(self.b[2].status, "blocked")
        self.assertEqual(self.b[2].blocked_by, {1})
        self.assertIn(2, tx.deps_changed)

    def test_blocker_must_exist_and_be_open(self):
        for bad in ([9], [2]):
            with self.assertRaises(ItemsError) as cm:
                self.b.set(2, blocked_by=bad)
            self.assertEqual(cm.exception.code, "invalid_blocker")
        self.b.set(1, status="done")
        with self.assertRaises(ItemsError) as cm:
            self.b.set(2, blocked_by=[1])
        self.assertEqual(cm.exception.code, "invalid_blocker")

    def test_direct_cycle_refused_with_path(self):
        self.b.set(2, blocked_by=[1])
        with self.assertRaises(ItemsError) as cm:
            self.b.set(1, blocked_by=[2])
        self.assertEqual(cm.exception.status, 409)
        self.assertEqual(cm.exception.code, "cycle")
        self.assertEqual(cm.exception.message, "cycle: 1→2→1")

    def test_long_cycle_refused(self):
        self.b.set(2, blocked_by=[1])
        self.b.set(3, blocked_by=[2])
        with self.assertRaises(ItemsError) as cm:
            self.b.set(1, blocked_by=[3])
        self.assertEqual(cm.exception.message, "cycle: 1→3→2→1")

    def test_diamond_is_not_a_cycle(self):
        self.b.create("d")
        self.b.set(2, blocked_by=[1])
        self.b.set(3, blocked_by=[1])
        self.b.set(4, blocked_by=[2, 3])
        self.assertEqual(self.b[4].blocked_by, {2, 3})

    def test_blocked_by_with_a_non_blocked_status_refused(self):
        with self.assertRaises(ItemsError):
            self.b.set(2, blocked_by=[1], status="active")

    def test_leaving_blocked_clears_blockers(self):
        self.b.set(2, blocked_by=[1])
        tx = self.b.set(2, status="active")
        self.assertEqual(self.b[2].blocked_by, set())
        self.assertIn(2, tx.deps_changed)

    def test_clearing_blocked_by_returns_to_active(self):
        self.b.set(2, blocked_by=[1])
        self.b.set(2, blocked_by=[])
        self.assertEqual(self.b[2].status, "active")

    def test_blocked_on_text_needs_no_item_and_warns_when_bare(self):
        self.b.set(2, blocked_on="EP GO")
        self.assertEqual(self.b[2].status, "blocked")
        tx = self.b.set(3, status="blocked")
        self.assertTrue(tx.warnings)


class DoneAndDropped(unittest.TestCase):
    def setUp(self):
        self.b = Board()
        self.b.create("a")
        self.b.create({"title": "b", "holders": [LANE]})
        self.b.set(2, blocked_by=[1])

    def test_done_unblocks_dependent_and_pushes_its_holders(self):
        later = NOW + timedelta(minutes=5)
        tx = self.b.set(1, now=later, status="done")
        dep = self.b[2]
        self.assertEqual(dep.status, "active")
        self.assertEqual(dep.blocked_by, set())
        self.assertEqual(dep.clock_reset_at, later)
        self.assertEqual(self.b[1].closed_at, later)
        self.assertEqual(tx.unblocked, [2])
        self.assertIn(2, tx.deps_changed)
        self.assertEqual([(p["kind"], p["pid"]) for p in tx.pushes], [("unblocked", LANE["pid"])])
        self.assertIn("#2", tx.pushes[0]["text"])
        self.assertIn("unblocked", [e["type"] for e in tx.events])

    def test_done_with_another_blocker_left_stays_blocked(self):
        self.b.create("c")
        self.b.set(2, blocked_by=[1, 3])
        tx = self.b.set(1, status="done")
        self.assertEqual(self.b[2].status, "blocked")
        self.assertEqual(self.b[2].blocked_by, {3})
        self.assertEqual(tx.unblocked, [])
        self.assertEqual(tx.pushes, [])

    def test_done_does_not_unblock_a_free_text_block(self):
        self.b.set(2, blocked_by=[1], blocked_on="JP review")
        self.b.set(1, status="done")
        self.assertEqual(self.b[2].status, "blocked")
        self.assertEqual(self.b[2].blocked_by, set())

    def test_done_resolves_own_flags_as_closed(self):
        tx0 = self.b.tx()
        tx0.raise_flag(self.b[1], "stale")
        tx = self.b.set(1, status="done")
        self.assertEqual(self.b[1].open_flags, {})
        self.assertEqual(tx.flags_resolved[0]["resolution"], "closed")

    def test_dropped_flags_dependent_and_keeps_edge(self):
        tx = self.b.set(1, status="dropped", reason="superseded")
        self.assertEqual(self.b[2].blocked_by, {1})
        self.assertIn("blocker_dropped", self.b[2].open_flags)
        self.assertEqual(tx.flags_raised[0]["detail"], {"blockers": [1]})
        self.assertEqual(self.b[2].status, "blocked")

    def test_second_dropped_blocker_updates_flag_detail(self):
        self.b.create("c")
        self.b.set(2, blocked_by=[1, 3])
        self.b.set(1, status="dropped")
        tx = self.b.set(3, status="dropped")
        self.assertEqual(self.b[2].open_flags["blocker_dropped"]["detail"], {"blockers": [1, 3]})
        self.assertEqual(tx.flags_detail, [{"n": 2, "kind": "blocker_dropped",
                                            "detail": {"blockers": [1, 3]}}])
        self.assertEqual(tx.flags_raised, [])

    def test_repointing_clears_blocker_dropped(self):
        self.b.create("c")
        self.b.set(1, status="dropped")
        tx = self.b.set(2, blocked_by=[3])
        self.assertNotIn("blocker_dropped", self.b[2].open_flags)
        self.assertEqual(tx.flags_resolved[-1]["resolution"], "cleared")

    def test_reopening_dropped_blocker_clears_flag(self):
        self.b.set(1, status="dropped")
        self.b.set(1, status="active")
        self.assertNotIn("blocker_dropped", self.b[2].open_flags)

    def test_dropped_then_done_unblocks(self):
        self.b.set(1, status="dropped")
        self.b.set(1, status="done")
        self.assertEqual(self.b[2].status, "active")
        self.assertNotIn("blocker_dropped", self.b[2].open_flags)

    def test_reopen_clears_closed_and_archived(self):
        self.b.set(1, status="done")
        self.b[1].archived_at = NOW
        self.b.set(1, status="active")
        self.assertIsNone(self.b[1].closed_at)
        self.assertIsNone(self.b[1].archived_at)


class Waiting(unittest.TestCase):
    def setUp(self):
        self.b = Board()
        self.b.create("job")

    def test_waiting_requires_eta(self):
        with self.assertRaises(ItemsError) as cm:
            self.b.set(1, status="waiting")
        self.assertEqual(cm.exception.code, "eta_required")

    def test_eta_implies_waiting_and_stores_times(self):
        self.b.set(1, eta="40m")
        it = self.b[1]
        self.assertEqual(it.status, "waiting")
        self.assertEqual(it.eta_at, NOW + timedelta(minutes=40))
        self.assertEqual(it.waiting_set_at, NOW)

    def test_waiting_keeps_eta_on_later_edits_and_clears_on_leaving(self):
        self.b.set(1, status="waiting", eta="1h30m")
        self.b.set(1, note="still running")
        self.assertEqual(self.b[1].status, "waiting")
        self.b.set(1, status="active")
        self.assertIsNone(self.b[1].eta_at)
        self.assertIsNone(self.b[1].waiting_set_at)

    def test_eta_with_other_status_refused(self):
        with self.assertRaises(ItemsError):
            self.b.set(1, status="active", eta="40m")

    def test_parse_when_forms(self):
        self.assertEqual(parse_when("2h", NOW, "eta"), NOW + timedelta(hours=2))
        self.assertEqual(parse_when("1d", NOW, "eta"), NOW + timedelta(days=1))
        self.assertEqual(parse_when("2026-10-07T00:00:00Z", NOW, "eta"),
                         datetime(2026, 10, 7, tzinfo=timezone.utc))
        with self.assertRaises(ItemsError):
            parse_when("soon", NOW, "eta")


class TouchEventsAndPushes(unittest.TestCase):
    def test_touch_on_status_note_holders_only(self):
        b = Board()
        b.create("x")
        later = NOW + timedelta(hours=1)
        b.set(1, now=later, group="RL")
        self.assertEqual(b[1].touched_at, NOW)
        b.set(1, now=later, note="hi")
        self.assertEqual(b[1].touched_at, later)

    def test_update_event_records_only_changed_fields(self):
        b = Board()
        b.create("x")
        tx = b.set(1, note="n", group="g", reason="why")
        (event,) = tx.events
        self.assertEqual(event["type"], "updated")
        self.assertEqual(event["prev"], {"note": None, "group": None})
        self.assertEqual(event["new"], {"note": "n", "group": "g"})
        self.assertEqual(event["reason"], "why")

    def test_noop_update_writes_no_event(self):
        b = Board()
        b.create({"title": "x", "note": "n"})
        self.assertEqual(b.set(1, note="n").events, [])

    def test_assigned_push_only_when_someone_else_adds_you(self):
        b = Board()
        tx = b.create({"title": "x", "holders": [LANE, ORCH]})
        self.assertEqual([p["pid"] for p in tx.pushes], [LANE["pid"]])
        self.assertEqual(tx.pushes[0]["kind"], "assigned")
        self.assertEqual(tx.pushes[0]["daemon_id"], "d1")
        tx = b.set(1, actor=OTHER, add_holders=[OTHER])
        self.assertEqual(tx.pushes, [])

    def test_no_push_to_a_holder_without_a_session(self):
        b = Board()
        tx = b.create({"title": "x", "holders": [{"pid": "owner"}]})
        self.assertEqual(tx.pushes, [])

    def test_batch_update_applies_to_each(self):
        b = Board()
        b.create("a", "b")
        b.set([1, 2], status="done")
        self.assertEqual({b[1].status, b[2].status}, {"done"})


class Resolve(unittest.TestCase):
    def setUp(self):
        self.b = Board()
        self.b.create({"title": "x", "holders": [LANE]}, "y")
        tx = self.b.tx()
        tx.raise_flag(self.b[1], "holder_idle")
        tx.raise_flag(self.b[1], "stale")

    def test_nudge_pushes_holder_and_snoozes_by_threshold(self):
        tx = self.b.tx()
        resolved = tx.resolve(1, "nudge", kind="holder_idle", message="status?")
        self.assertEqual(resolved, ["holder_idle"])
        self.assertEqual(tx.flags_resolved[0]["snooze_until"], NOW + timedelta(seconds=1200))
        self.assertEqual(tx.pushes[0]["kind"], "nudge")
        self.assertIn("status?", tx.pushes[0]["text"])
        self.assertIn("stale", self.b[1].open_flags)

    def test_nudge_without_holder_refused(self):
        self.b.set(1, holders=[])
        with self.assertRaises(ItemsError) as cm:
            self.b.tx().resolve(1, "nudge")
        self.assertEqual(cm.exception.code, "no_holders")

    def test_reassign_replaces_holders_and_resolves_all(self):
        tx = self.b.tx()
        resolved = tx.resolve(1, "reassign", holders=[OTHER])
        self.assertEqual(sorted(resolved), ["holder_idle", "stale"])
        self.assertEqual([h["pid"] for h in self.b[1].holders], [OTHER["pid"]])
        self.assertEqual(tx.pushes[0]["kind"], "assigned")

    def test_block_on_text_needs_check_back(self):
        with self.assertRaises(ItemsError) as cm:
            self.b.tx().resolve(1, "block", blocked_on="EP GO")
        self.assertEqual(cm.exception.code, "check_back_required")
        self.b.tx().resolve(1, "block", blocked_on="EP GO", check_back="2h")
        self.assertEqual(self.b[1].status, "blocked")
        self.assertEqual(self.b[1].check_back_at, NOW + timedelta(hours=2))

    def test_block_by_item(self):
        self.b.tx().resolve(1, "block", blocked_by=[2])
        self.assertEqual(self.b[1].blocked_by, {2})

    def test_drop(self):
        tx = self.b.tx()
        tx.resolve(1, "drop", reason="Owner ruled no")
        self.assertEqual(self.b[1].status, "dropped")
        self.assertEqual(self.b[1].open_flags, {})
        self.assertEqual({f["resolution"] for f in tx.flags_resolved}, {"drop"})

    def test_launch_is_daemon_side(self):
        with self.assertRaises(ItemsError):
            self.b.tx().resolve(1, "launch")


if __name__ == "__main__":
    unittest.main()
