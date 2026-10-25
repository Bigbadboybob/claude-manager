"""Promote an assistant (doc/org-model.md §4.2): a member's assistant becomes
the main session of a new member managed by that member. Database tests run
when CM_ORG_TEST_DSN or CM_ITEMS_TEST_DSN names a scratch database (see
test_items_api.py).
"""

from __future__ import annotations

import os
import unittest

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

from mcp_server.tests import test_org_api as base  # noqa: E402
from mcp_server.tests.test_org_api import D1, DSN, OWNER, pid, uid_at  # noqa: E402
from mcp_server.tests import test_org_assistants as assistants  # noqa: E402


@unittest.skipUnless(DSN, "set CM_ORG_TEST_DSN to a scratch Postgres database")
class PromoteDb(unittest.IsolatedAsyncioTestCase):
    asyncSetUp = base.OrgApiDb.asyncSetUp
    asyncTearDown = base.OrgApiDb.asyncTearDown
    task = base.OrgApiDb.task
    initiative = base.OrgApiDb.initiative
    board_with_holder = base.OrgApiDb.board_with_holder
    roster = base.OrgApiDb.roster
    apply = base.OrgApiDb.apply
    snapshot = base.OrgApiDb.snapshot
    swarm = base.OrgApiDb.swarm
    beat = assistants.AssistantsDb.beat
    members = assistants.AssistantsDb.members

    async def report(self, extra=()):
        """AssistantsDb's beat, with the helper in its own stream checkout."""
        await self.beat(D1, [
            {"uid": self.coord, "name": "Swarm-Coord", "task_id": self.root, "ws": "ws-coord", "path": "/w/coord"},
            {"uid": self.lane, "name": "org", "task_id": self.lane_task, "ws": "ws-org", "path": "/w/org",
             "managed_by": self.coord},
            {"uid": self.helper, "name": "helper", "task_id": self.lane_task, "managed_by": self.lane,
             "ws": "ws-helper", "path": "/w/helper", "member_id": self.ids["org"], "acting_as": "assistant"},
            {"uid": self.rev, "name": "rev1", "task_id": self.elsewhere, "managed_by": self.lane,
             "member_id": self.ids["org"], "acting_as": "assistant"},
            {"uid": self.orch, "name": "bug-triage-orchestrator", "continuous": "bug-triage"},
            *extra,
        ])

    async def promote(self, actor_pid, target, status=200, **body):
        r = await self.http.post("/org/assistants/promote", json={
            "actor": {"pid": actor_pid}, "pid": target, "handle": body.pop("handle", "helper"),
            "reason": "owns its own area now", **body})
        self.assertEqual(r.status_code, status, r.text)
        return r.json()

    async def test_the_members_main_promotes_its_assistant(self):
        await self.members()
        before = (await self.snapshot())["version"]
        out = await self.promote(pid(self.lane), pid(self.helper), title="Helper", request_id="p1")
        member = out["member"]
        self.assertEqual((member["handle"], member["manager_member_id"], out["promoted_from"]),
                         ("helper", self.ids["org"], self.ids["org"]))
        self.assertEqual(member["main"]["pid"], pid(self.helper))
        self.assertEqual(member["home_workspace"]["workspace_id"], "ws-helper")
        self.assertGreater(out["version"], before)
        # It works as the new member now, and a stale beat that still
        # reports the old assistant stamp does not turn it back.
        await self.report()
        r = (await self.http.get("/org/members/by-pid", params={"pid": pid(self.helper)})).json()
        self.assertEqual((r["acting_as"], r["member"]["handle"]), ("main", "helper"))
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval(
                "SELECT acting_as FROM org_session_facts WHERE pid = $1", pid(self.helper)), "promoted")
        # Its holds stay with it.
        items = (await self.http.get(f"/boards/{self.board['slug']}")).json()["items"]
        held = next(i for i in items if i["title"] == "helper work")
        self.assertEqual([h["pid"] for h in held["holders"]], [pid(self.helper)])
        # The snapshot (F6, the roster) shows it under org.
        snap = await self.snapshot()
        self.assertEqual({m["handle"]: m["manager_member_id"] for m in snap["members"]}["helper"],
                         self.ids["org"])
        # A retry with the same request id replays; a fresh one is refused.
        again = await self.promote(pid(self.lane), pid(self.helper), title="Helper", request_id="p1")
        self.assertEqual(again["member"]["id"], member["id"])
        body = await self.promote(pid(self.lane), pid(self.helper), status=409, handle="helper-2")
        self.assertEqual(body["detail"]["code"], "already_member")
        events = (await self.http.get("/org/events", params={"subject_type": "member"})).json()
        self.assertEqual(sum(e["type"] == "assistant_promoted" for e in events), 1)

    async def test_the_manager_chain_and_owner_may_promote_too(self):
        await self.members()
        out = await self.promote(pid(self.coord), pid(self.rev), handle="rev")
        self.assertEqual(out["member"]["manager_member_id"], self.ids["org"])
        out = await self.promote(OWNER["pid"], pid(self.helper), handle="helper")
        self.assertEqual(out["member"]["manager_member_id"], self.ids["org"])

    async def test_refusals(self):
        await self.members()
        shell, cont, stray, worker = uid_at(0.3, 13), uid_at(0.3, 14), uid_at(0.3, 15), uid_at(0.3, 16)
        await self.report([
            {"uid": shell, "name": "bash", "engine": "bash", "member_id": self.ids["org"],
             "acting_as": "assistant"},
            {"uid": cont, "name": "x-orchestrator", "continuous": "x", "member_id": self.ids["org"],
             "acting_as": "assistant"},
            {"uid": stray, "name": "stray", "task_id": self.elsewhere},
            {"uid": worker, "name": "bug-triage-w1", "managed_by": self.orch, "member_id": self.ids["org"],
             "acting_as": "worker"},
        ])
        cases = [
            # Sideways: a member outside org's chain (the continuous member).
            (pid(self.orch), pid(self.rev), 403, "not_manager", {}),
            # The assistant itself, and a session that is no member's main.
            (pid(self.helper), pid(self.rev), 403, "not_member", {}),
            (pid("nobody"), pid(self.rev), 403, "not_member", {}),
            (OWNER["pid"], pid(shell), 409, "not_an_agent", {}),
            (OWNER["pid"], pid(self.lane), 409, "already_member", {}),
            (OWNER["pid"], pid(cont), 409, "continuous", {}),
            (OWNER["pid"], pid(stray), 409, "not_an_assistant", {}),
            # A non-durable (continuous) worker is no assistant.
            (OWNER["pid"], pid(worker), 409, "not_an_assistant", {}),
            (OWNER["pid"], pid(self.rev), 409, "handle_taken", {"handle": "org"}),
            # A lead places the new member only inside its own subtree, and
            # gives it only a team it leads.
            (pid(self.lane), pid(self.rev), 403, "needs_owner", {"manager": "swarm-coord", "handle": "rev"}),
            (pid(self.lane), pid(self.rev), 403, "needs_owner", {"team": "swarm", "handle": "rev"}),
        ]
        for actor, target, status, code, extra in cases:
            body = await self.promote(actor, target, status=status, **extra)
            self.assertEqual(body["detail"]["code"], code, (actor, target, extra))
        r = await self.http.post("/org/assistants/promote", json={
            "actor": OWNER, "pid": pid(self.rev), "handle": "rev"})
        self.assertEqual(r.status_code, 422, "a reason is required")
        snap = await self.snapshot()
        self.assertNotIn("rev", {m["handle"] for m in snap["members"]}, "nothing was created")

    async def test_a_lead_places_the_promoted_member_in_its_subtree_and_its_team(self):
        await self.members()
        out = await self.promote(pid(self.coord), pid(self.rev), handle="rev", manager="swarm-coord",
                                 team="swarm")
        self.assertEqual((out["member"]["manager_member_id"], out["member"]["primary_team_id"]),
                         (self.ids["swarm-coord"], self.init_id))
        out = await self.promote(pid(self.coord), pid(self.helper), manager="org")
        self.assertEqual(out["member"]["manager_member_id"], self.ids["org"])
