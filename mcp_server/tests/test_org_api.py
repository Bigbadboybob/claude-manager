"""Org model S2 API against a real Postgres (doc/org-model.md §4, §6, §9, §14).

The database tests run when CM_ORG_TEST_DSN (or CM_ITEMS_TEST_DSN) names a
scratch database they may wipe; see test_items_api.py for a private cluster
recipe. Without it only the migration text checks run.
"""

from __future__ import annotations

import asyncio
import os
import re
import unittest
import uuid
from datetime import timedelta
from pathlib import Path

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

from dispatch import db, org_db  # noqa: E402

ROOT = Path(__file__).parents[2]
DSN = os.environ.get("CM_ORG_TEST_DSN") or os.environ.get("CM_ITEMS_TEST_DSN")
TOKEN = {"Authorization": f"Bearer {os.environ['CM_API_TOKEN']}"}
OWNER = {"pid": "owner", "name": "Owner"}
D1, D2 = "d1", "d2"


def pid(uid, daemon=D1):
    return f"agent:{daemon}:{uid}"


def uid_at(hours_ago: float, n: int = 1) -> str:
    """A daemon-shaped uid spawned `hours_ago` hours ago (spawn order)."""
    import time
    nanos = int((time.time() - hours_ago * 3600) * 1e9)
    return f"ts-{nanos:x}-{n:x}"


class MigrationText(unittest.TestCase):
    def test_migration_is_rerunnable(self):
        text = (ROOT / "sql" / "026_org_core.sql").read_text()
        body = "\n".join(l for l in text.splitlines() if not l.lstrip().startswith("--"))
        self.assertNotRegex(body, r"\bUPDATE\s+\w+\s+SET")
        for insert in re.findall(r"INSERT INTO \w+[^;]*;", body):
            # Seeds only, and only rerunnable ones (the function bodies' own
            # inserts are inside CREATE OR REPLACE FUNCTION).
            if "org_changes (subject_type" in insert:
                continue
            self.assertIn("ON CONFLICT DO NOTHING", insert, insert)
        self.assertNotRegex(body, r"CREATE TABLE (?!IF NOT EXISTS)")
        self.assertNotRegex(body, r"CREATE (UNIQUE )?INDEX (?!IF NOT EXISTS)")
        self.assertNotRegex(body, r"CREATE TRIGGER")

    def test_migration_number_is_unique(self):
        self.assertEqual(len(list((ROOT / "sql").glob("026_*.sql"))), 1)


class Helpers(unittest.TestCase):
    def test_handles(self):
        self.assertEqual(org_db.normalize_handle("Swarm-Coord"), "swarm-coord")
        self.assertEqual(org_db.normalize_handle("bug_triage  orchestrator!"), "bug-triage-orchestrator")
        self.assertIsNone(org_db.normalize_handle("-"))
        self.assertEqual(org_db._free_handle("ep", {"ep", "ep-2"}), "ep-3")
        self.assertEqual(org_db._free_handle("owner", set()), "owner-2")
        with self.assertRaises(org_db.OrgError):
            org_db.check_handle("Owner")
        self.assertTrue(org_db.new_member_id().startswith("mem_"))
        self.assertRegex(org_db.new_member_id(), r"^mem_[0-9a-z]{26}$")

    def test_lead_rule_matches_the_daemon(self):
        early, late = uid_at(5), uid_at(1)
        rows = [{"session_uid": late, "engine": "codex"}, {"session_uid": early, "engine": "claude"},
                {"session_uid": uid_at(9), "engine": "bash"}]
        self.assertEqual(org_db.lead_session(None, rows), early)
        self.assertEqual(org_db.lead_session(late, rows), late)
        self.assertEqual(org_db.lead_session("gone", rows), early)
        self.assertEqual(org_db.lead_session(" Owner ", rows), "owner")
        self.assertIsNone(org_db.lead_session(None, rows[2:]))


@unittest.skipUnless(DSN, "set CM_ORG_TEST_DSN to a scratch Postgres database")
class OrgApiDb(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        import asyncpg
        import httpx
        from fastapi import FastAPI

        from api.items import router as items_router
        from api.org import router as org_router

        self.pool = await asyncpg.create_pool(DSN, min_size=1, max_size=6, init=db._init_connection)
        async with self.pool.acquire() as conn:
            await conn.execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        await db.init_db(self.pool)
        await db.init_db(self.pool)  # migrations must re-run cleanly
        self.continuous: list[dict] = []
        app = FastAPI()
        app.state.pool = self.pool
        app.state.read_continuous_states = lambda: self.continuous
        app.include_router(items_router)
        app.include_router(org_router)
        self.http = httpx.AsyncClient(transport=httpx.ASGITransport(app=app),
                                      base_url="http://t", headers=TOKEN)

    async def asyncTearDown(self):
        await self.http.aclose()
        await self.pool.close()

    # ---- fixtures ----------------------------------------------------------

    async def task(self, name, *, metadata=None, initiative=None, parent=None) -> str:
        async with self.pool.acquire() as conn:
            return str(await conn.fetchval(
                """INSERT INTO tasks (repo_url, prompt, name, metadata, initiative_id, parent_task_id)
                   VALUES ('r', '', $1, $2, $3, $4) RETURNING id""",
                name, metadata, uuid.UUID(initiative) if initiative else None,
                uuid.UUID(parent) if parent else None))

    async def initiative(self, slug, coordinator, kind="initiative") -> str:
        init = await db.create_initiative(self.pool, slug=slug, name=slug.title(),
                                          coordinator_task_id=coordinator, kind=kind)
        return init["id"]

    async def beat(self, daemon, sessions, **extra):
        body = {"sessions": [
            {"pid": pid(s["uid"], daemon), "session_uid": s["uid"], "state": s.get("state", "idle"),
             "name": s.get("name"), "engine": s.get("engine", "claude-code"),
             "task_id": s.get("task_id"), "workspace_id": s.get("ws"),
             "worktree_path": s.get("path"), "continuous_task_id": s.get("continuous"),
             "managed_by_uid": s.get("managed_by"), "workflow_run_id": s.get("workflow"),
             "spawned_age_s": s.get("age_s", 3 * 86400)}
            for s in sessions], **extra}
        r = await self.http.post(f"/hosts/{daemon}/heartbeat", json=body)
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def board_with_holder(self, root_task, holder_pid, title="work"):
        from dispatch import items_db
        board = await items_db.resolve_board(self.pool, task_id=root_task, ref=None)
        r = await self.http.post(f"/boards/{board['slug']}/items", json={
            "actor": {"pid": "owner"},
            "items": [{"title": title, "status": "active", "holders": [{"pid": holder_pid}]}]})
        self.assertEqual(r.status_code, 200, r.text)
        return board

    async def roster(self):
        r = await self.http.get("/org/roster")
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def apply(self, rows, actor=OWNER, request_id=None, status=200):
        body = {"actor": actor, "rows": rows}
        if request_id:
            body["request_id"] = request_id
        r = await self.http.post("/org/roster/apply", json=body)
        self.assertEqual(r.status_code, status, r.text)
        return r.json()

    async def snapshot(self, since=None):
        r = await self.http.get("/org/snapshot", params={} if since is None else {"since_version": since})
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def swarm(self):
        """An initiative led by `coord`, a long-lived lane holding items on its
        board, a continuous orchestrator and its worker, and a young helper."""
        self.coord, self.lane, self.helper = uid_at(80, 1), uid_at(70, 2), uid_at(1, 3)
        self.orch, self.worker = uid_at(90, 4), uid_at(2, 5)
        self.root = await self.task("swarm root")
        self.lane_task = await self.task("org lane", parent=self.root)
        self.init_id = await self.initiative("swarm", self.root)
        self.continuous = [{"task_id": "bug-triage", "label": "Bug triage",
                            "planning_task_id": None, "current_session_uid": self.orch}]
        await self.beat(D1, [
            {"uid": self.coord, "name": "Swarm-Coord", "task_id": self.root, "ws": "ws-coord",
             "path": "/w/coord"},
            {"uid": self.lane, "name": "org", "task_id": self.lane_task, "ws": "ws-org",
             "path": "/w/org", "managed_by": self.coord},
            {"uid": self.helper, "name": "helper", "task_id": self.lane_task, "age_s": 3600},
            {"uid": self.orch, "name": "bug-triage-orchestrator", "continuous": "bug-triage",
             "ws": "ws-bt", "path": "/w/bt"},
            {"uid": self.worker, "name": "bt-007", "continuous": "bug-triage",
             "managed_by": self.orch},
        ])
        board = await self.board_with_holder(self.root, pid(self.lane))
        await self.board_with_holder(self.root, pid(self.helper), "helper work")
        return board

    # ---- schema --------------------------------------------------------------

    async def test_owner_seed_team_kind_and_nullable_coordinator(self):
        snap = await self.snapshot()
        owner = next(m for m in snap["members"] if m["id"] == "mem_owner")
        self.assertEqual((owner["handle"], owner["kind"], owner["manager_member_id"]),
                         ("owner", "owner", None))
        async with self.pool.acquire() as conn:
            await conn.execute("INSERT INTO initiatives (slug, name, kind) VALUES ('t1', 'T1', 'team')")
            with self.assertRaises(Exception):
                await conn.execute("INSERT INTO initiatives (slug, name, kind) VALUES ('t2', 'T2', 'squad')")
            with self.assertRaises(Exception):  # only Owner has no manager
                await conn.execute("INSERT INTO members (id, handle, kind) VALUES ('mem_x', 'xx', 'agent')")

    async def test_manager_cycles_are_refused_in_the_writing_transaction(self):
        await self.swarm()
        roster = await self.roster()
        await self.apply([{"proposal_id": p["proposal_id"]} for p in roster["proposals"]])
        snap = await self.snapshot()
        by = {m["handle"]: m for m in snap["members"]}
        coord, lane = by["swarm-coord"], by["org"]
        self.assertEqual(lane["manager_member_id"], coord["id"])
        r = await self.http.post("/org/members/swarm-coord/move", json={
            "actor": OWNER, "manager": "org", "reason": "test"})
        self.assertEqual(r.status_code, 409, r.text)
        self.assertEqual(r.json()["detail"]["code"], "cycle")
        async with self.pool.acquire() as conn:
            with self.assertRaisesRegex(Exception, "org_cycle"):
                await conn.execute("UPDATE members SET manager_member_id = $1 WHERE id = $2",
                                   coord["id"], coord["id"])
        # Two concurrent moves that would only together close a loop: the
        # advisory lock serialises them, so exactly one wins.
        bt = by["bug-triage"]
        results = await asyncio.gather(
            self.http.post("/org/members/bug-triage/move",
                           json={"actor": OWNER, "manager": "org", "reason": "a"}),
            self.http.post("/org/members/org/move",
                           json={"actor": OWNER, "manager": "bug-triage", "reason": "b"}))
        self.assertEqual(sorted(r.status_code for r in results), [200, 409], [r.text for r in results])
        self.assertTrue(bt)

    async def test_team_parent_cycles_are_refused(self):
        a = (await self.http.post("/org/teams", json={"actor": OWNER, "name": "Alpha"})).json()
        b = (await self.http.post("/org/teams", json={"actor": OWNER, "name": "Beta",
                                                      "parent_team_id": a["id"]})).json()
        self.assertEqual(b["parent_team_id"], a["id"])
        r = await self.http.patch(f"/org/teams/{a['id']}", json={"actor": OWNER, "parent_team_id": b["id"]})
        self.assertEqual(r.status_code, 409, r.text)
        self.assertEqual(r.json()["detail"]["code"], "cycle")
        r = await self.http.post("/org/teams", json={"actor": {"pid": pid("x")}, "name": "Gamma"})
        self.assertEqual(r.status_code, 403, r.text)
        r = await self.http.patch(f"/org/teams/{a['id']}", json={
            "actor": OWNER, "charter": "ship S2", "status": "standby", "expected_revision": 0})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual((r.json()["charter"], r.json()["status"], r.json()["revision"]),
                         ("ship S2", "standby", 1))
        r = await self.http.patch(f"/org/teams/{a['id']}", json={
            "actor": OWNER, "charter": "stale", "expected_revision": 0})
        self.assertEqual(r.json()["detail"]["code"], "revision_conflict")

    async def test_trusted_team_create_runs_in_the_callers_transaction(self):
        async with self.pool.acquire() as conn:
            async with conn.transaction():
                team_id = await org_db.create_team_conn(
                    conn, {"pid": pid("lead")}, name="Epic team", lead="owner",
                    authorized_by="epic.set_poc:lead_on_own_board")
        snap = await self.snapshot()
        team = next(t for t in snap["teams"] if t["id"] == team_id)
        self.assertEqual((team["kind"], team["lead_member_id"], team["coordinator_task_id"]),
                         ("team", "mem_owner", None))
        events = (await self.http.get("/org/events", params={"subject_id": team_id})).json()
        self.assertEqual(events[0]["new"]["authorized_by"], "epic.set_poc:lead_on_own_board")

    # ---- roster ----------------------------------------------------------------

    async def test_roster_proposes_continuous_leads_and_lanes_once_each(self):
        await self.swarm()
        roster = await self.roster()
        by_id = {p["proposal_id"]: p for p in roster["proposals"]}
        self.assertEqual(set(by_id), {f"continuous:{D1}:bug-triage", f"lead:{self.init_id}",
                                      f"lane:{pid(self.lane)}"}, by_id)
        cont = by_id[f"continuous:{D1}:bug-triage"]
        self.assertEqual((cont["handle"], cont["kind"], cont["pids"]),
                         ("bug-triage", "continuous", [pid(self.orch)]))
        self.assertEqual(cont["aliases"], ["bug-triage-orchestrator"])
        self.assertEqual(cont["home_workspace"], {"daemon_id": D1, "workspace_id": "ws-bt", "path": "/w/bt"})
        lead = by_id[f"lead:{self.init_id}"]
        self.assertEqual((lead["handle"], lead["team_id"], lead["pids"]),
                         ("swarm-coord", self.init_id, [pid(self.coord)]))
        lane = by_id[f"lane:{pid(self.lane)}"]
        self.assertEqual((lane["handle"], lane["team_id"], lane["manager_handle"], lane["title"]),
                         ("org", self.init_id, "swarm-coord", "org lane"))
        # The helper is too young, the worker is a continuous assistant.
        self.assertNotIn(f"lane:{pid(self.helper)}", by_id)

    async def test_roster_dedupes_by_pid_and_never_steals_an_alias(self):
        await self.swarm()
        # The orchestrator also coordinates a division: one member, two roles.
        div_root = await self.task("ops root", metadata={"lead_session_uid": self.orch})
        await self.beat(D1, [
            {"uid": self.coord, "name": "Swarm-Coord", "task_id": self.root},
            {"uid": self.lane, "name": "org", "task_id": self.lane_task},
            {"uid": self.orch, "name": "bug-triage-orchestrator", "continuous": "bug-triage",
             "task_id": div_root, "ws": "ws-bt"},
        ])
        div = await self.initiative("ops", div_root, kind="division")
        # Someone already answers to `org` and `bug-triage-orchestrator`.
        async with self.pool.acquire() as conn:
            await conn.execute(
                """INSERT INTO members (id, handle, kind, manager_member_id) VALUES
                   ('mem_old', 'org', 'agent', 'mem_owner')""")
            await conn.execute(
                """INSERT INTO member_aliases (org_id, alias, member_id) VALUES
                   ('main', 'org', 'mem_old'), ('main', 'bug-triage-orchestrator', 'mem_old')""")
        roster = await self.roster()
        by_id = {p["proposal_id"]: p for p in roster["proposals"]}
        self.assertNotIn(f"lead:{div}", by_id)
        cont = by_id[f"continuous:{D1}:bug-triage"]
        self.assertEqual([lo["team_id"] for lo in cont["lead_of"]], [div])
        self.assertEqual(cont["aliases"], [])
        self.assertIn("alias bug-triage-orchestrator belongs to another member; not added",
                      cont["conflicts"])
        lane = by_id[f"lane:{pid(self.lane)}"]
        self.assertEqual(lane["handle"], "org-2")
        self.assertIn("handle org taken → org-2", lane["conflicts"])

    async def test_roster_apply_is_owner_only_per_row_and_idempotent(self):
        await self.swarm()
        roster = await self.roster()
        ids = [p["proposal_id"] for p in roster["proposals"]]
        r = await self.apply([{"proposal_id": ids[0]}], actor={"pid": pid(self.coord)}, status=403)
        self.assertEqual(r["detail"]["code"], "owner_only")
        before = (await self.snapshot())["version"]
        rows = [
            {"proposal_id": f"lane:{pid(self.lane)}", "handle": "org-lane"},  # lead row sorts first
            {"proposal_id": f"lead:{self.init_id}"},
            {"proposal_id": f"continuous:{D1}:bug-triage", "approve": False, "reason": "later"},
            {"proposal_id": "lane:agent:d1:nobody"},
        ]
        result = await self.apply(rows, request_id="apply-1")
        self.assertEqual(sorted(c["member"]["handle"] for c in result["created"]), ["org-lane", "swarm-coord"])
        self.assertEqual(result["declined"], [f"continuous:{D1}:bug-triage"])
        self.assertEqual([(r["proposal_id"], r["code"]) for r in result["rejected"]],
                         [("lane:agent:d1:nobody", "stale")])
        replay = await self.apply(rows, request_id="apply-1")
        self.assertEqual({k: v for k, v in replay.items() if k != "version"},
                         {k: v for k, v in result.items() if k != "version"})
        snap = await self.snapshot()
        self.assertGreater(snap["version"], before)
        self.assertEqual(await self.snapshot(snap["version"]), {"unchanged": True, "version": snap["version"]})
        by = {m["handle"]: m for m in snap["members"]}
        coord, lane = by["swarm-coord"], by["org-lane"]
        self.assertEqual(lane["manager_member_id"], coord["id"])
        self.assertEqual(coord["manager_member_id"], "mem_owner")
        self.assertEqual(lane["main"]["pid"], pid(self.lane))
        self.assertEqual(lane["main"]["occupancy_generation"], 1)
        self.assertEqual(lane["home_workspace"], {"daemon_id": D1, "workspace_id": "ws-org",
                                                  "path": "/w/org", "state": "central"})
        team = next(t for t in snap["teams"] if t["id"] == self.init_id)
        self.assertEqual(team["lead_member_id"], coord["id"])
        self.assertEqual(team["board"], "swarm")
        self.assertIn({"member_id": lane["id"], "role": "primary"}, team["members"])
        roster = await self.roster()
        self.assertEqual(roster["proposals"], [])
        self.assertEqual(roster["declined"], [f"continuous:{D1}:bug-triage"])
        self.assertIsNotNone(roster["last_applied_at"])

    async def test_a_lane_whose_manager_is_not_approved_names_the_row(self):
        await self.swarm()
        result = await self.apply([{"proposal_id": f"lane:{pid(self.lane)}"}])
        self.assertEqual([r["code"] for r in result["rejected"]], ["unknown_manager"])
        result = await self.apply([{"proposal_id": f"lane:{pid(self.lane)}", "manager": "owner"}])
        self.assertEqual(result["created"][0]["member"]["manager_member_id"], "mem_owner")

    async def test_board_anchored_boards_get_a_team_from_the_roster(self):
        root = await self.task("standalone root")
        lead_uid, lane_uid = uid_at(40, 1), uid_at(30, 2)
        await self.beat(D2, [{"uid": lead_uid, "name": "Kestrel", "task_id": root},
                             {"uid": lane_uid, "name": "Wren", "task_id": await self.task("sub", parent=root)}])
        board = await self.board_with_holder(root, pid(lane_uid, D2))
        roster = await self.roster()
        by_id = {p["proposal_id"]: p for p in roster["proposals"]}
        lead = by_id[f"lead:board:{board['id']}"]
        self.assertEqual(lead["lead_of"][0]["create"], True)
        lane = by_id[f"lane:{pid(lane_uid, D2)}"]
        self.assertEqual(lane["manager_handle"], "kestrel")
        result = await self.apply([{"proposal_id": p} for p in by_id])
        self.assertEqual(result["rejected"], [])
        snap = await self.snapshot()
        team = next(t for t in snap["teams"] if t["coordinator_task_id"] == root)
        self.assertEqual((team["kind"], team["board"]), ("team", board["slug"]))
        wren = next(m for m in snap["members"] if m["handle"] == "wren")
        self.assertEqual(wren["primary_team_id"], team["id"])

    # ---- adoption, heartbeat, reads --------------------------------------------

    async def test_adopt_binds_a_live_session_and_the_heartbeat_carries_it(self):
        uid = uid_at(3)
        await self.beat(D1, [{"uid": uid, "name": "rev1", "ws": "ws-r", "path": "/w/r"}])
        body = {"actor": OWNER, "pid": pid(uid), "handle": "reviewer", "title": "Reviewer",
                "request_id": "adopt-1"}
        r = await self.http.post("/org/adopt", json={**body, "actor": {"pid": pid("other")}})
        self.assertEqual(r.status_code, 403, r.text)
        r = await self.http.post("/org/adopt", json=body)
        self.assertEqual(r.status_code, 200, r.text)
        adopted = r.json()
        self.assertEqual((adopted["member"]["handle"], adopted["member"]["title"],
                          adopted["home_mark"]), ("reviewer", "Reviewer", {"state": "central"}))
        r = await self.http.post("/org/adopt", json=body)  # same request: same answer
        self.assertEqual(r.json()["member"]["id"], adopted["member"]["id"])
        r = await self.http.post("/org/adopt", json={**body, "handle": "rev-2", "request_id": "adopt-2"})
        self.assertEqual(r.json()["detail"]["code"], "pid_already_member")
        r = await self.http.post("/org/adopt", json={**body, "pid": pid("gone"), "request_id": "adopt-3"})
        self.assertEqual(r.status_code, 404)
        # The member cache rides the heartbeat reply; marks wait for the
        # daemon to advertise org_preserve_v1, and its ack confirms them.
        reply = await self.beat(D1, [{"uid": uid, "name": "rev1", "ws": "ws-r"}])
        self.assertEqual(reply["org"]["members"][uid]["handle"], "reviewer")
        self.assertEqual(reply["org"]["members"][uid]["acting_as"], "main")
        self.assertEqual(reply["org"]["workspace_marks"], [])
        reply = await self.beat(D1, [{"uid": uid, "ws": "ws-r"}], capabilities=["org_preserve_v1"])
        marks = reply["org"]["workspace_marks"]
        self.assertEqual([(m["workspace_id"], m["handle"]) for m in marks], [("ws-r", "reviewer")])
        version = (await self.snapshot())["version"]
        reply = await self.beat(D1, [{"uid": uid, "ws": "ws-r"}], capabilities=["org_preserve_v1"])
        self.assertEqual(len(reply["org"]["workspace_marks"]), 1)  # re-sent until acked
        self.assertEqual((await self.snapshot())["version"], version)  # attempts don't feed
        reply = await self.beat(D1, [{"uid": uid, "ws": "ws-r"}], capabilities=["org_preserve_v1"],
                                acked_mark_ids=[marks[0]["id"]])
        self.assertEqual(reply["org"]["workspace_marks"], [])
        member = (await self.http.get("/org/members/main/reviewer")).json()["member"]
        self.assertEqual(member["home_workspace"]["state"], "confirmed")
        # A remote daemon's beat never carries this daemon's members.
        self.assertEqual((await self.beat(D2, []))["org"]["members"], {})

    async def test_reads_by_pid_handle_and_task(self):
        await self.swarm()
        roster = await self.roster()
        await self.apply([{"proposal_id": p["proposal_id"]} for p in roster["proposals"]])
        me = (await self.http.get("/org/members/by-pid", params={"pid": pid(self.lane)})).json()
        self.assertEqual((me["acting_as"], me["member"]["handle"]), ("main", "org"))
        self.assertEqual([c["handle"] for c in me["chain"]], ["swarm-coord", "owner"])
        self.assertEqual(me["teams"][0]["my_role"], "primary")
        boss = (await self.http.get("/org/members/swarm-coord")).json()
        self.assertEqual([r["handle"] for r in boss["reports"]], ["org"])
        self.assertEqual(boss["teams"][0]["my_role"], "lead")
        alias = (await self.http.get("/org/members/bug-triage-orchestrator")).json()
        self.assertEqual(alias["member"]["handle"], "bug-triage")
        none = (await self.http.get("/org/members/by-pid", params={"pid": pid(self.helper)})).json()
        self.assertEqual((none["member"], none["acting_as"]), (None, None))
        self.assertEqual((await self.http.get("/org/members/nobody")).status_code, 404)
        team = (await self.http.get("/org/teams/swarm")).json()
        self.assertEqual(team["lead"]["handle"], "swarm-coord")
        task = await db.get_task(self.pool, self.root)
        org = await org_db.task_org(self.pool, task)
        self.assertEqual(org["team"]["lead"]["handle"], "swarm-coord")
        self.assertEqual(org["lead_member"]["handle"], "swarm-coord")
        self.assertEqual([m["handle"] for m in org["members"]], ["swarm-coord"])
        changes = (await self.http.get("/org/changes", params={"since": 0})).json()
        self.assertTrue(any(c["subject_type"] == "team" and c["subject_id"] == self.init_id
                            for c in changes["changes"]))

    async def test_moves_by_the_manager_chain(self):
        await self.swarm()
        roster = await self.roster()
        await self.apply([{"proposal_id": p["proposal_id"]} for p in roster["proposals"]])
        r = await self.http.post("/org/members/org/move", json={
            "actor": {"pid": pid(self.orch)}, "manager": "bug-triage", "reason": "steal"})
        self.assertEqual(r.json()["detail"]["code"], "not_manager")
        r = await self.http.post("/org/members/org/move", json={
            "actor": {"pid": pid(self.coord)}, "manager": "owner", "reason": "promote",
            "expected_revision": 1})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(r.json()["member"]["manager_member_id"], "mem_owner")
        self.assertEqual(r.json()["member"]["revision"], 2)
        events = (await self.http.get("/org/events", params={"subject_type": "member"})).json()
        self.assertEqual((events[0]["type"], events[0]["reason"]), ("move", "promote"))


if __name__ == "__main__":
    unittest.main()
