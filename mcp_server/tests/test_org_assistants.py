"""Durable assistants (doc/org-model.md §4.2, §4.3, Owner decision O5).

Daemons report which member each session is an assistant of; the API uses
it for `org.member`, the deliberate close (authority + holds hand-back),
board free capacity and the status-mirror gate. Database tests run when
CM_ORG_TEST_DSN or CM_ITEMS_TEST_DSN names a scratch database (see
test_items_api.py); the pure checks run without it.
"""

from __future__ import annotations

import os
import re
import unittest
from pathlib import Path

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

from mcp_server.tests import test_org_api as base  # noqa: E402
from mcp_server.tests.test_org_api import D1, DSN, OWNER, pid, uid_at  # noqa: E402

ROOT = Path(__file__).parents[2]


class Pure(unittest.TestCase):
    def test_migration_is_rerunnable_and_takes_number_031(self):
        self.assertEqual([p.name for p in (ROOT / "sql").glob("031_*.sql")], ["031_org_assistants.sql"])
        body = "\n".join(l for l in (ROOT / "sql" / "031_org_assistants.sql").read_text().splitlines()
                         if not l.lstrip().startswith("--"))
        self.assertNotRegex(body, r"\bUPDATE\b")
        for stmt in re.findall(r"ALTER TABLE [^;]+;", body):
            self.assertIn("IF NOT EXISTS", stmt)

    def test_the_mirror_needs_both_guards(self):
        from dispatch import placement
        self.assertEqual(placement.MIRROR_CAPS, ("lifecycle_guard_v1", "assistant_guard_v1"))

    # The reaper's protections are tested in tests/test_worktree_reaper.py.


@unittest.skipUnless(DSN, "set CM_ORG_TEST_DSN to a scratch Postgres database")
class AssistantsDb(unittest.IsolatedAsyncioTestCase):
    asyncSetUp = base.OrgApiDb.asyncSetUp
    asyncTearDown = base.OrgApiDb.asyncTearDown
    task = base.OrgApiDb.task
    initiative = base.OrgApiDb.initiative

    async def beat(self, daemon, sessions, **extra):
        """test_org_api's beat, plus the member facts an O5 daemon reports."""
        rows = []
        for s in sessions:
            row = {"pid": pid(s["uid"], daemon), "session_uid": s["uid"], "state": s.get("state", "idle"),
                   "name": s.get("name"), "engine": s.get("engine", "claude-code"),
                   "task_id": s.get("task_id"), "workspace_id": s.get("ws"),
                   "worktree_path": s.get("path"), "continuous_task_id": s.get("continuous"),
                   "managed_by_uid": s.get("managed_by"), "workflow_run_id": s.get("workflow"),
                   "auto_place_owner": s.get("auto_place_owner", False),
                   "auto_place_unresolved": s.get("auto_place_unresolved", False),
                   "spawned_age_s": s.get("age_s", 3 * 86400)}
            for key in ("member_id", "acting_as"):
                if s.get(key):
                    row[key] = s[key]
            rows.append(row)
        r = await self.http.post(f"/hosts/{daemon}/heartbeat", json={"sessions": rows, **extra})
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()
    board_with_holder = base.OrgApiDb.board_with_holder
    roster = base.OrgApiDb.roster
    apply = base.OrgApiDb.apply
    snapshot = base.OrgApiDb.snapshot
    swarm = base.OrgApiDb.swarm

    async def members(self):
        """The swarm roster applied: swarm-coord leads, `org` is a lane
        under it, bug-triage is a continuous member. `helper` (young, holds
        an item) is no member: it becomes org's assistant."""
        self.board = await self.swarm()
        roster = await self.roster()
        await self.apply([{"proposal_id": p["proposal_id"]} for p in roster["proposals"]])
        snap = await self.snapshot()
        self.ids = {m["handle"]: m["id"] for m in snap["members"]}
        self.assertIn("org", self.ids)
        self.assertNotIn("helper", self.ids)
        self.elsewhere = await self.task("unrelated")
        self.rev = uid_at(0.5, 9)
        await self.report()

    async def report(self, extra=()):
        """The daemon's beat: the helper and a second assistant (on a task
        off the board) stamped as org's assistants."""
        await self.beat(D1, [
            {"uid": self.coord, "name": "Swarm-Coord", "task_id": self.root, "ws": "ws-coord", "path": "/w/coord"},
            {"uid": self.lane, "name": "org", "task_id": self.lane_task, "ws": "ws-org", "path": "/w/org",
             "managed_by": self.coord},
            {"uid": self.helper, "name": "helper", "task_id": self.lane_task, "managed_by": self.lane,
             "member_id": self.ids["org"], "acting_as": "assistant"},
            {"uid": self.rev, "name": "rev1", "task_id": self.elsewhere, "managed_by": self.lane,
             "member_id": self.ids["org"], "acting_as": "assistant"},
            {"uid": self.orch, "name": "bug-triage-orchestrator", "continuous": "bug-triage"},
            *extra,
        ])

    async def close(self, actor_pid, target, status=200, **body):
        r = await self.http.post("/org/assistants/close", json={
            "actor": {"pid": actor_pid}, "pid": target, "member_id": self.ids["org"],
            "reason": "finished", **body})
        self.assertEqual(r.status_code, status, r.text)
        return r.json()

    async def test_the_daemon_reports_assistants_and_org_member_answers_for_them(self):
        await self.members()
        r = await self.http.get("/org/members/by-pid", params={"pid": pid(self.helper)})
        body = r.json()
        self.assertEqual((body["acting_as"], body["member"]["handle"]), ("assistant", "org"))
        # A main is still a main.
        r = await self.http.get("/org/members/by-pid", params={"pid": pid(self.lane)})
        self.assertEqual(r.json()["acting_as"], "main")
        # A session nobody claims is nobody's.
        r = await self.http.get("/org/members/by-pid", params={"pid": pid(self.orch) + "-x"})
        self.assertIsNone(r.json()["member"])

    async def test_snapshot_lists_live_assistant_and_worker_facts_by_member(self):
        await self.members()
        worker = uid_at(0.1, 19)
        await self.report([{"uid": worker, "name": "worker", "ws": "ws-worker",
                            "managed_by": self.lane, "member_id": self.ids["org"],
                            "acting_as": "worker"}])
        snapshot = await self.snapshot()
        member = next(m for m in snapshot["members"] if m["id"] == self.ids["org"])
        rows = {a["session_uid"]: a for a in member["assistants"]}
        self.assertIsInstance(rows[worker].pop("spawned_at"), str)
        self.assertEqual(rows[worker], {"pid": pid(worker), "daemon_id": D1,
                         "session_uid": worker, "acting_as": "worker",
                         "spawned_by": {"daemon_id": D1, "session_uid": self.lane},
                         "workspace_id": "ws-worker"})
        self.assertEqual(rows[self.lane]["acting_as"], "main")
        self.assertEqual(rows[self.lane]["pid"], pid(self.lane))
        legacy = (await self.http.get("/org/snapshot", params={"legacy_placement": "true"})).json()
        self.assertEqual(legacy["legacy_main_tasks"][self.ids["org"]], self.lane_task)
        self.assertNotIn("legacy_main_tasks", snapshot)
        self.assertEqual(rows[self.helper]["acting_as"], "assistant")
        self.assertTrue(all("assistants" in m for m in snapshot["members"]))
        # UIDs are scoped to hosts, and missing spawners remain null.
        await self.beat("d2", [{"uid": worker, "name": "other-host", "member_id": self.ids["org"],
                                "acting_as": "worker"}])
        snapshot = await self.snapshot()
        member = next(m for m in snapshot["members"] if m["id"] == self.ids["org"])
        duplicates = [a for a in member["assistants"] if a["session_uid"] == worker]
        self.assertEqual({a["daemon_id"] for a in duplicates}, {D1, "d2"})
        self.assertIsNone(next(a for a in duplicates if a["daemon_id"] == "d2")["spawned_by"])
        await self.close(OWNER["pid"], pid(self.helper))
        snapshot = await self.snapshot()
        member = next(m for m in snapshot["members"] if m["id"] == self.ids["org"])
        self.assertNotIn(self.helper, [a["session_uid"] for a in member["assistants"]])

    async def test_explicit_unresolved_placement_seats_a_spawned_agent_once(self):
        uid = uid_at(0.1, 55)
        session = {"uid": uid, "name": "orphan", "managed_by": "gone",
                   "auto_place_owner": True, "auto_place_unresolved": True}
        await self.beat(D1, [session])
        first = await self.snapshot()
        member = next(m for m in first["members"] if m["main"] and m["main"]["pid"] == pid(uid))
        self.assertEqual((member["kind"], member["manager_member_id"]), ("agent", "mem_owner"))
        await self.beat(D1, [session])
        again = await self.snapshot()
        self.assertEqual(sum(bool(m["main"] and m["main"]["pid"] == pid(uid))
                             for m in again["members"]), 1)

    async def test_owner_placement_joins_live_assistant_or_worker_and_retries(self):
        await self.members()
        for n, role in enumerate(("assistant", "worker"), 1):
            existing, fresh = uid_at(4, 70 + n), uid_at(0.01, 80 + n)
            rows = [{"uid": existing, "ws": f"shared-{n}", "path": f"/w/shared-{n}",
                     "member_id": self.ids["org"], "acting_as": role},
                    {"uid": fresh, "ws": f"shared-{n}", "path": f"/w/shared-{n}",
                     "name": f"owner-new-{n}", "auto_place_owner": True}]
            reply = await self.beat(D1, rows)
            self.assertEqual(reply["org"]["assignments"][fresh], self.ids["org"])
            self.assertEqual(reply["org"]["auto_placed"][0]["acting_as"], "assistant")
            again = await self.beat(D1, rows)
            self.assertEqual(again["org"]["auto_placed"], [])
            self.assertEqual(again["org"]["assignments"][fresh], self.ids["org"])
            async with self.pool.acquire() as conn:
                fact = await conn.fetchrow("SELECT * FROM org_session_facts WHERE pid = $1", pid(fresh))
                self.assertFalse(fact["auto_place_owner"])
                self.assertTrue(fact["assignment_authoritative"])
                self.assertEqual(await conn.fetchval(
                    "SELECT count(*) FROM incarnations WHERE pid = $1 AND ended_at IS NULL", pid(fresh)), 0)

    async def test_owner_placement_uses_oldest_member_and_central_stream_mark(self):
        await self.members()
        older, newer, fresh = uid_at(8, 71), uid_at(1, 72), uid_at(0.01, 73)
        reply = await self.beat(D1, [
            {"uid": newer, "ws": "shared", "member_id": self.ids["org"], "acting_as": "assistant"},
            {"uid": older, "ws": "shared", "member_id": self.ids["swarm-coord"], "acting_as": "worker"},
            {"uid": fresh, "ws": "shared", "name": "join-oldest", "auto_place_owner": True}])
        self.assertEqual(reply["org"]["assignments"][fresh], self.ids["swarm-coord"])
        mark_body = {"actor": OWNER, "member": self.ids["org"], "daemon_id": D1,
                     "workspace_id": "mark-only", "path": "/w/mark-only", "kind": "stream",
                     "reason": "test", "request_id": "mark-only"}
        marked = await self.http.post("/org/repair/workspace-mark", json=mark_body)
        self.assertEqual(marked.status_code, 200, marked.text)
        marked_fresh = uid_at(0.01, 74)
        reply = await self.beat(D1, [{"uid": marked_fresh, "ws": "mark-only", "name": "join-mark",
                                      "auto_place_owner": True}])
        self.assertEqual(reply["org"]["assignments"][marked_fresh], self.ids["org"])

    async def test_owner_placement_never_gives_home_with_unattributed_live_session(self):
        shell, fresh = uid_at(2, 81), uid_at(0.01, 82)
        await self.beat(D1, [{"uid": shell, "engine": "bash", "ws": "shared"},
                             {"uid": fresh, "name": "new-home-test", "ws": "shared",
                              "auto_place_owner": True}])
        member = (await self.http.get("/org/members/main/new-home-test")).json()["member"]
        self.assertIsNone(member["home_workspace"])

    async def test_owner_placement_joins_a_vacant_members_live_assistant_by_path(self):
        await self.members()
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE members SET status = 'vacant' WHERE id = $1", self.ids["org"])
            await conn.execute("UPDATE incarnations SET ended_at = now() WHERE member_id = $1", self.ids["org"])
        assistant, fresh = uid_at(2, 83), uid_at(0.01, 84)
        reply = await self.beat(D1, [
            {"uid": assistant, "ws": "ws-a", "path": "/w/vacant", "member_id": self.ids["org"],
             "acting_as": "assistant"},
            {"uid": fresh, "ws": "ws-b", "path": "/w/vacant", "name": "vacant-join",
             "auto_place_owner": True}])
        self.assertEqual(reply["org"]["assignments"][fresh], self.ids["org"])

    async def test_owner_repair_reassigns_and_pushes_stream_idempotently(self):
        await self.members()
        fresh = uid_at(0.01, 91)
        row = {"uid": fresh, "name": "accidental-member", "ws": "bad-home", "path": "/w/bad-home",
               "auto_place_owner": True}
        reply = await self.beat(D1, [row])
        source = reply["org"]["auto_placed"][0]["member_id"]
        repair = {"actor": OWNER, "member": source, "destination": self.ids["org"],
                  "reason": "repair placement", "request_id": "repair-source"}
        refused = await self.http.post("/org/repair/auto-placed-assistant", json={
            **repair, "actor": {"pid": pid(self.lane)}})
        self.assertEqual(refused.status_code, 403, refused.text)
        first = await self.http.post("/org/repair/auto-placed-assistant", json=repair)
        self.assertEqual(first.status_code, 200, first.text)
        again = await self.http.post("/org/repair/auto-placed-assistant", json=repair)
        self.assertEqual(first.json(), again.json())
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval("SELECT status FROM members WHERE id = $1", source), "retired")
            self.assertEqual(await conn.fetchval("SELECT end_reason FROM incarnations WHERE member_id = $1", source),
                             "repair placement")
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM org_events WHERE request_id = $1", "repair-source"), 2)
            old_mark_id = await conn.fetchval("SELECT id FROM org_workspace_marks WHERE member_id = $1", source)
        mark = {"actor": OWNER, "member": self.ids["org"], "daemon_id": D1, "workspace_id": "bad-home",
                "kind": "stream", "reason": "restore stream", "request_id": "repair-mark"}
        refused = await self.http.post("/org/repair/workspace-mark", json={
            **mark, "actor": {"pid": pid(self.lane)}})
        self.assertEqual(refused.status_code, 403, refused.text)
        marked = await self.http.post("/org/repair/workspace-mark", json=mark)
        self.assertEqual(marked.status_code, 200, marked.text)
        replay = await self.http.post("/org/repair/workspace-mark", json=mark)
        self.assertEqual(marked.json(), replay.json())
        self.assertNotEqual(marked.json()["id"], old_mark_id)
        reply = await self.beat(D1, [row], capabilities=["org_marks_v1"], acked_mark_ids=[old_mark_id])
        self.assertEqual(reply["org"]["assignments"][fresh], self.ids["org"])
        pushed = next(m for m in reply["org"]["workspace_marks"] if m["workspace_id"] == "bad-home")
        self.assertEqual((pushed["kind"], pushed["member_id"]), ("stream", self.ids["org"]))
        self.assertEqual(pushed["path"], "/w/bad-home")

    async def test_repair_refuses_non_auto_placed_and_member_with_holds(self):
        await self.members()
        repair = {"actor": OWNER, "member": self.ids["org"], "destination": self.ids["swarm-coord"],
                  "reason": "test", "request_id": "not-auto"}
        response = await self.http.post("/org/repair/auto-placed-assistant", json=repair)
        self.assertEqual(response.status_code, 409, response.text)
        self.assertEqual(response.json()["detail"]["code"], "not_auto_placed")
        uid = uid_at(0.01, 92)
        result = await self.beat(D1, [{"uid": uid, "name": "held-auto", "ws": "held",
                                       "auto_place_owner": True}])
        source = result["org"]["auto_placed"][0]["member_id"]
        await self.board_with_holder(await self.task("held task"), pid(uid))
        response = await self.http.post("/org/repair/auto-placed-assistant", json={
            **repair, "member": source, "request_id": "has-holds"})
        self.assertEqual(response.status_code, 409, response.text)
        self.assertEqual(response.json()["detail"]["code"], "has_holds")

    async def test_repair_refuses_reports_team_lead_and_other_live_assistants(self):
        await self.members()
        uid = uid_at(0.01, 93)
        result = await self.beat(D1, [{"uid": uid, "name": "guarded-auto", "ws": "guarded",
                                       "auto_place_owner": True}])
        source = result["org"]["auto_placed"][0]["member_id"]
        body = {"actor": OWNER, "member": source, "destination": self.ids["swarm-coord"],
                "reason": "test guard", "request_id": "guarded-repair"}
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE members SET manager_member_id = $1 WHERE id = $2", source, self.ids["org"])
        response = await self.http.post("/org/repair/auto-placed-assistant", json=body)
        self.assertEqual(response.json()["detail"]["code"], "has_reports")
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE members SET manager_member_id = $1 WHERE id = $2", self.ids["swarm-coord"], self.ids["org"])
            team = await conn.fetchval("SELECT id FROM initiatives WHERE lead_member_id = $1 LIMIT 1", self.ids["swarm-coord"])
            await conn.execute("UPDATE initiatives SET lead_member_id = $1 WHERE id = $2", source, team)
        response = await self.http.post("/org/repair/auto-placed-assistant", json=body)
        self.assertEqual(response.json()["detail"]["code"], "leads_team")
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE initiatives SET lead_member_id = $1 WHERE id = $2", self.ids["swarm-coord"], team)
        helper = uid_at(1, 94)
        await self.beat(D1, [{"uid": uid, "name": "guarded-auto", "ws": "guarded", "auto_place_owner": True},
                             {"uid": helper, "member_id": source, "acting_as": "assistant"}])
        response = await self.http.post("/org/repair/auto-placed-assistant", json=body)
        self.assertEqual(response.json()["detail"]["code"], "has_sessions")

    async def test_close_authority_main_chain_and_owner_allowed_others_refused(self):
        await self.members()
        # Refused: an unrelated member (the continuous one), the assistant
        # itself, a session that belongs to no member.
        for actor, code in ((pid(self.orch), "not_manager"), (pid(self.helper), "not_member"),
                            (pid("nobody"), "not_member")):
            body = await self.close(actor, pid(self.rev), status=403)
            self.assertEqual(body["detail"]["code"], code, actor)
        # A member's main session is not an assistant.
        body = await self.close(OWNER["pid"], pid(self.lane), status=409)
        self.assertEqual(body["detail"]["code"], "not_an_assistant")
        # Allowed: the member's own main session...
        out = await self.close(pid(self.lane), pid(self.helper), request_id="c1")
        self.assertEqual(out["main_pid"], pid(self.lane))
        # ... a member up its manager chain ...
        out = await self.close(pid(self.coord), pid(self.rev))
        self.assertEqual(out["holds"], {"handed_back": [], "kept": []})
        # ... and Owner.
        third = uid_at(0.2, 11)
        await self.report([{"uid": third, "name": "rev2", "task_id": self.elsewhere,
                            "member_id": self.ids["org"], "acting_as": "assistant"}])
        await self.close(OWNER["pid"], pid(third))
        # A retry with the same request id replays.
        again = await self.close(pid(self.lane), pid(self.helper), request_id="c1")
        self.assertEqual(again["main_pid"], pid(self.lane))
        events = (await self.http.get("/org/events", params={"subject_type": "member"})).json()
        self.assertEqual(sum(e["type"] == "close_assistant" for e in events), 3)

    async def test_closing_hands_the_assistants_holds_back_to_its_member(self):
        await self.members()
        items = (await self.http.get(f"/boards/{self.board['slug']}")).json()["items"]
        helper_item = next(i for i in items if i["title"] == "helper work")
        self.assertEqual([(h["pid"], h["role"]) for h in helper_item["holders"]],
                         [(pid(self.helper), "owner")])
        out = await self.close(pid(self.lane), pid(self.helper))
        self.assertEqual([(h["n"], h["role"], h["to"]) for h in out["holds"]["handed_back"]],
                         [(helper_item["n"], "owner", pid(self.lane))])
        items = (await self.http.get(f"/boards/{self.board['slug']}")).json()["items"]
        moved = next(i for i in items if i["n"] == helper_item["n"])
        self.assertEqual([(h["pid"], h["role"]) for h in moved["holders"]], [(pid(self.lane), "owner")])
        self.assertEqual(moved["status"], "active")
        # The daemon still reporting the old stamp does not reopen it.
        await self.report()
        async with self.pool.acquire() as conn:
            acting = await conn.fetchval("SELECT acting_as FROM org_session_facts WHERE pid = $1",
                                         pid(self.helper))
        self.assertEqual(acting, "closed")

    async def test_a_vacant_members_assistant_keeps_its_holds(self):
        await self.members()
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE incarnations SET ended_at = now() WHERE pid = $1", pid(self.lane))
        out = await self.close(OWNER["pid"], pid(self.helper))
        self.assertIsNone(out["main_pid"])
        self.assertEqual(out["holds"]["handed_back"], [])
        self.assertEqual([k["role"] for k in out["holds"]["kept"]], ["owner"])

    async def test_free_capacity_lists_idle_assistants_of_the_boards_members(self):
        await self.members()
        read = (await self.http.get(f"/boards/{self.board['slug']}")).json()
        rows = {r["name"]: r for r in read["free_capacity"]}
        # rev1 works on a task off the board, but it is the lane's assistant.
        self.assertEqual(rows["rev1"]["assistant_of"], "org")
        self.assertIn("rev1 (for org)", read["launch"]["free"])
        # The lane itself holds an active item: not free.
        self.assertNotIn("org", rows)
        # A member-less session off the board is not capacity.
        stray = uid_at(0.1, 12)
        await self.report([{"uid": stray, "name": "stray", "task_id": self.elsewhere}])
        read = (await self.http.get(f"/boards/{self.board['slug']}")).json()
        self.assertNotIn("stray", {r["name"] for r in read["free_capacity"]})

    async def test_retire_hands_the_retired_members_holds_to_its_manager(self):
        await self.members()
        # The lane `org` holds "work" as owner; Swarm-Coord manages it.
        r = await self.http.post("/org/members/org/retire", json={
            "actor": {"pid": pid(self.coord)}, "reason": "lane wound down"})
        self.assertEqual(r.status_code, 200, r.text)
        handed = r.json()["holds"]["handed_back"]
        self.assertEqual([(h["role"], h["to"]) for h in handed], [("owner", pid(self.coord))])
        items = (await self.http.get(f"/boards/{self.board['slug']}")).json()["items"]
        work = next(i for i in items if i["title"] == "work")
        self.assertEqual([(h["pid"], h["role"]) for h in work["holders"]], [(pid(self.coord), "owner")])

    async def test_an_agents_kill_session_hands_holds_back_without_the_close_check(self):
        await self.members()
        # The daemon already killed it under kill_session's scope (its own
        # spawner, which is not in org's chain): the API hands holds back.
        r = await self.http.post("/org/assistants/close", json={
            "actor": {"pid": pid("someone-in-scope")}, "pid": pid(self.helper),
            "member_id": self.ids["org"], "reason": "killed with kill_session",
            "ended_by": "kill_session"})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual([h["to"] for h in r.json()["holds"]["handed_back"]], [pid(self.lane)])
        events = (await self.http.get("/org/events", params={"subject_type": "member"})).json()
        self.assertIn("assistant_killed", [e["type"] for e in events])
        # Any other ended_by is refused.
        r = await self.http.post("/org/assistants/close", json={
            "actor": {"pid": pid("x")}, "pid": pid(self.rev), "member_id": self.ids["org"],
            "reason": "r", "ended_by": "anything"})
        self.assertEqual(r.status_code, 422, r.text)

    async def test_workers_are_attributed_but_never_free_capacity_assistants(self):
        await self.members()
        worker = uid_at(0.3, 13)
        await self.report([{"uid": worker, "name": "bt-fix", "task_id": self.elsewhere,
                            "member_id": self.ids["org"], "acting_as": "worker"}])
        async with self.pool.acquire() as conn:
            acting = await conn.fetchval("SELECT acting_as FROM org_session_facts WHERE pid = $1", pid(worker))
        self.assertEqual(acting, "worker")
        read = (await self.http.get(f"/boards/{self.board['slug']}")).json()
        self.assertNotIn("bt-fix", {r["name"] for r in read["free_capacity"]})
        # Not an assistant: closing it is refused (kill_session it instead).
        body = await self.close(OWNER["pid"], pid(worker), status=409)
        self.assertEqual(body["detail"]["code"], "not_an_assistant")

    async def test_a_close_must_name_the_member_the_daemon_reported(self):
        await self.members()
        # The coordinator's main names itself as the member: it would take the
        # lane's assistant's holds. Refused, with and without ended_by.
        for extra in ({}, {"ended_by": "kill_session"}):
            r = await self.http.post("/org/assistants/close", json={
                "actor": OWNER, "pid": pid(self.helper), "member_id": self.ids["swarm-coord"],
                "reason": "take over", **extra})
            self.assertEqual(r.status_code, 409, r.text)
            self.assertEqual(r.json()["detail"]["code"], "not_an_assistant")
        # A session its daemon never reported as anyone's assistant.
        r = await self.http.post("/org/assistants/close", json={
            "actor": OWNER, "pid": pid("never-reported"), "member_id": self.ids["org"], "reason": "r"})
        self.assertEqual(r.status_code, 409, r.text)
        items = (await self.http.get(f"/boards/{self.board['slug']}")).json()["items"]
        helper_item = next(i for i in items if i["title"] == "helper work")
        self.assertEqual([h["pid"] for h in helper_item["holders"]], [pid(self.helper)], "holds untouched")
