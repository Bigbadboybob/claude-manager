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
