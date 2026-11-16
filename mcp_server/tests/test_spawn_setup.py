"""#28: a creator sets a session up at launch. The API side: board membership
by an explicit join, and set_poc naming a PoC that has not touched the board
yet (it joins in the same call) when the caller has authority over the board.

Database tests run when CM_ITEMS_TEST_DSN names a scratch Postgres database
(see test_items_api.py); the pure checks run without it.
"""

from __future__ import annotations

import os
import re
import unittest
from pathlib import Path

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

from mcp_server.tests import test_org_s3 as s3  # noqa: E402
from mcp_server.tests.test_org_s3 import DSN, LANE, ORCH, OWNER, agent  # noqa: E402

ROOT = Path(__file__).parents[2]
NEW = agent("new-lead", daemon="d2")      # a member's main that never touched the board
BOSS = agent("boss")                       # NEW's manager
STEWARD = agent("steward")
STRANGER = agent("stranger")
ASSISTANT = agent("rev1")                  # nobody's main


class Pure(unittest.TestCase):
    def test_migration_is_rerunnable_and_numbered_032(self):
        self.assertEqual([p.name for p in (ROOT / "sql").glob("032_*.sql")], ["032_board_members.sql"])
        body = "\n".join(l for l in (ROOT / "sql" / "032_board_members.sql").read_text().splitlines()
                         if not l.lstrip().startswith("--"))
        self.assertNotRegex(body, r"\bUPDATE\b")
        self.assertNotRegex(body, r"CREATE TABLE (?!IF NOT EXISTS)")
        self.assertNotRegex(body, r"CREATE (UNIQUE )?INDEX (?!IF NOT EXISTS)")
        self.assertTrue(re.search(r"PRIMARY KEY \(board_id, pid\)", body))


@unittest.skipUnless(DSN, "set CM_ITEMS_TEST_DSN to a scratch Postgres database")
class SpawnSetupDb(unittest.IsolatedAsyncioTestCase):
    asyncSetUp = s3.OrgS3Db.asyncSetUp
    task = s3.OrgS3Db.task
    beat = s3.OrgS3Db.beat
    epic = s3.OrgS3Db.epic
    read = s3.OrgS3Db.read

    async def asyncTearDown(self):
        await s3.OrgS3Db.asyncTearDown(self)

    async def members(self):
        """lfl (no session), the lane (main LANE, on the board), new-lead (main
        NEW, managed by boss, never on the board)."""
        try:
            import dispatch.org_db  # noqa: F401
        except ImportError:
            self.skipTest("needs the org core (S2)")
        async with self.pool.acquire() as conn:
            await conn.execute(
                """INSERT INTO members (id, handle, kind, manager_member_id) VALUES
                       ('mem_boss', 'boss', 'agent', 'mem_owner'),
                       ('mem_lane', 'lane', 'agent', 'mem_owner'),
                       ('mem_new', 'new-lead', 'agent', 'mem_boss')
                   ON CONFLICT DO NOTHING""")
            await conn.execute(
                """INSERT INTO member_aliases (org_id, alias, member_id) VALUES
                       ('main', 'boss', 'mem_boss'), ('main', 'lane', 'mem_lane'),
                       ('main', 'new-lead', 'mem_new')
                   ON CONFLICT DO NOTHING""")
            for member, who in (("mem_boss", BOSS), ("mem_lane", LANE), ("mem_new", NEW)):
                await conn.execute(
                    """INSERT INTO incarnations (member_id, role, pid, daemon_id, session_uid)
                       VALUES ($1, 'main', $2, $3, $4)""",
                    member, who["pid"], who["daemon_id"], who["session_uid"])
        await self.beat(NEW, task=await self.task("elsewhere"), daemon="d2", caps=s3.GUARDED, state="idle")
        self.e = await self.epic("Latent", goal="ship it")
        self.poc = f"/boards/{self.ref}/epics/{self.e['id']}/poc"

    async def joined(self, pid):
        return (await self.http.get(f"/boards/{self.ref}/members", params={"pid": pid})).json()

    async def test_set_poc_on_a_non_member_joins_it_with_authority_and_refuses_without(self):
        await self.members()
        self.assertFalse((await self.joined(NEW["pid"]))["on_board"])
        # No authority over the board: refused, and nothing joins.
        for actor in (STRANGER, LANE):
            r = await self.http.post(self.poc, json={"actor": actor, "member": "new-lead"})
            self.assertEqual(r.status_code, 403, r.text)
        self.assertEqual((await self.joined(NEW["pid"]))["members"], [])
        # The board's orchestrator: the PoC joins the board in the same call.
        r = await self.http.post(self.poc, json={"actor": ORCH, "member": NEW["pid"]})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual((r.json()["poc_member_id"], r.json()["joined_board"]), ("mem_new", [NEW["pid"]]))
        member = (await self.joined(NEW["pid"]))["members"][0]
        self.assertEqual((member["group_id"], member["added_by"]), (self.e["id"], ORCH["pid"]))
        self.assertTrue((await self.joined(NEW["pid"]))["on_board"])
        # A second set reports nothing new to join.
        r = await self.http.post(self.poc, json={"actor": ORCH, "member": "new-lead"})
        self.assertEqual(r.json()["joined_board"], [])

    async def test_the_steward_and_the_pocs_chain_may_name_the_poc(self):
        await self.members()
        r = await self.http.patch(f"/boards/{self.ref}", json={"actor": OWNER, "steward_pid": STEWARD["pid"]})
        self.assertEqual(r.status_code, 200, r.text)
        r = await self.http.post(self.poc, json={"actor": STEWARD, "member": "new-lead"})
        self.assertEqual(r.status_code, 200, r.text)
        # new-lead is the PoC now: its manager (boss) may hand the epic on.
        r = await self.http.post(self.poc, json={"actor": BOSS, "member": "lane"})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(r.json()["poc_member_id"], "mem_lane")
        # ...but closing an epic is still the board lead's or Owner's.
        r = await self.http.post(f"/boards/{self.ref}/epics/{self.e['id']}/close-plan",
                                 json={"actor": STEWARD, "outcome": "done"})
        self.assertEqual(r.status_code, 403, r.text)

    async def test_a_session_that_is_no_member_says_how_to_make_it_one(self):
        await self.members()
        r = await self.http.post(self.poc, json={"actor": ORCH, "member": ASSISTANT["pid"]})
        self.assertEqual(r.status_code, 422, r.text)
        detail = r.json()["detail"]
        self.assertEqual(detail["code"], "unknown_member")
        self.assertIn("promote", detail["message"])
        self.assertIn("hire=", detail["message"])

    async def test_a_joined_session_is_on_the_board_and_free_capacity_when_idle(self):
        await self.members()
        r = await self.http.post(f"/boards/{self.ref}/members", json={
            "actor": ORCH, "pid": NEW["pid"], "name": "new-lead", "group": "Latent"})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual((r.json()["joined"], r.json()["group_id"]), (True, self.e["id"]))
        again = (await self.http.post(f"/boards/{self.ref}/members", json={
            "actor": ORCH, "pid": NEW["pid"]})).json()
        self.assertFalse(again["joined"], "already on the board")
        free = {row["pid"] for row in (await self.read())["free_capacity"]}
        self.assertIn(NEW["pid"], free, "an idle joined session is capacity")
        r = await self.http.post(f"/boards/{self.ref}/members", json={
            "actor": ORCH, "pid": NEW["pid"], "group": "No such group"})
        self.assertEqual(r.status_code, 422, r.text)


@unittest.skipUnless(DSN, "set CM_ITEMS_TEST_DSN to a scratch Postgres database")
class MembershipEnds(SpawnSetupDb):
    """#28 review: a board membership can end: the session leaves, Owner, the
    orchestrator, the steward or its adder (while it can edit) removes it, or
    it exits for good. Never psql."""

    # Only this class's tests (the base class's run on their own).
    test_set_poc_on_a_non_member_joins_it_with_authority_and_refuses_without = None
    test_the_steward_and_the_pocs_chain_may_name_the_poc = None
    test_a_session_that_is_no_member_says_how_to_make_it_one = None
    test_a_joined_session_is_on_the_board_and_free_capacity_when_idle = None

    async def join(self, pid, actor=ORCH):
        r = await self.http.post(f"/boards/{self.ref}/members", json={"actor": actor, "pid": pid})
        self.assertEqual(r.status_code, 200, r.text)

    async def remove(self, pid, actor, editor=False, status=200):
        r = await self.http.post(f"/boards/{self.ref}/members/remove",
                                 json={"actor": actor, "pid": pid, "editor": editor})
        self.assertEqual(r.status_code, status, r.text)
        return r.json()

    async def test_who_may_end_a_membership(self):
        await self.members()
        await self.join(NEW["pid"], actor=LANE)        # LANE added it
        # A stranger, and the adder once it can no longer edit: refused.
        self.assertEqual((await self.remove(NEW["pid"], STRANGER, status=403))["detail"]["code"], "forbidden")
        await self.remove(NEW["pid"], LANE, editor=False, status=403)
        # The adder while it can edit: removed.
        self.assertTrue((await self.remove(NEW["pid"], LANE, editor=True))["removed"])
        self.assertEqual((await self.joined(NEW["pid"]))["members"], [])
        # The session itself leaves; Owner and the orchestrator remove anyone.
        for actor in (NEW, OWNER, ORCH):
            await self.join(NEW["pid"])
            self.assertTrue((await self.remove(NEW["pid"], actor))["removed"], actor)
        # Removing what is not joined is a no-op, not an error.
        self.assertFalse((await self.remove(NEW["pid"], OWNER))["removed"])

    async def test_a_session_gone_for_good_leaves_and_a_restart_keeps_it(self):
        await self.members()
        await self.join(NEW["pid"])
        await self.join(BOSS["pid"])
        from dispatch import board_members
        async with self.pool.acquire() as conn:
            # NEW exited 31 minutes ago; BOSS exited a minute ago (a restart).
            for who, minutes in ((NEW, 31), (BOSS, 1)):
                await conn.execute(
                    """INSERT INTO session_states (pid, daemon_id, session_uid, state, reported_at,
                                                   exited_at)
                       VALUES ($1, $2, $3, 'exited', now(), now() - make_interval(mins => $4))
                       ON CONFLICT (pid) DO UPDATE SET exited_at = EXCLUDED.exited_at,
                                                       state = 'exited'""",
                    who["pid"], who["daemon_id"], who["session_uid"], minutes)
        # The heartbeat that reports the daemon's snapshot ends the stale joins.
        await self.beat(ORCH, task=self.root, caps=s3.GUARDED)
        await self.beat(LANE, daemon="d2", caps=s3.GUARDED)
        self.assertEqual((await self.joined(NEW["pid"]))["members"], [], "gone for good: left")
        self.assertEqual(len((await self.joined(BOSS["pid"]))["members"]), 1, "a restart keeps it")
        async with self.pool.acquire() as conn:
            self.assertEqual(await board_members.end_exited(conn), 0)
        self.assertEqual(board_members.EXIT_GRACE_S, 1800)


class StartSessionSetup(unittest.IsolatedAsyncioTestCase):
    """The MCP layer: start_session passes the setup keys to session.setup
    right after the spawn and returns its per-step result; a setup error never
    fails the spawn. org(action="setup") is the same call later."""

    async def spawn(self, setup_reply, **kwargs):
        from pathlib import Path as P
        from unittest.mock import patch

        from mcp_server import control_client, server

        calls = []

        def call(method, params, **_):
            calls.append((method, params))
            if method == "session.setup":
                if isinstance(setup_reply, Exception):
                    raise setup_reply
                return setup_reply
            return {"session_uid": "child-1", "prompt_source": "none"}

        with (
            patch.object(control_client, "resolve_socket_route",
                         return_value=control_client.SocketRoute(path=P("/d.sock"), chose_daemon=True)),
            patch.object(control_client, "call", side_effect=call),
        ):
            return await server.start_session("claude-code", "lead", **kwargs), calls

    async def test_each_setup_param_reaches_session_setup_and_the_steps_come_back(self):
        steps = {"name": "ok", "channels": [{"channel": "ct/team", "result": "ok"}], "board": "ok",
                 "items": [{"n": 4, "result": "ok"}], "poc": "refused: forbidden: not the board lead",
                 "hire": "failed: this daemon cannot promote yet (org #27)"}
        result, calls = await self.spawn({"setup": steps}, name="Lead-A", channels=["ct/team"],
                                         board="team", epic="g-1", group="Latent", items=[4],
                                         poc_of="g-1", hire={"handle": "lead-a"})
        self.assertEqual(result["session_uid"], "child-1")
        self.assertEqual(result["setup"], steps)
        method, params = calls[-1]
        self.assertEqual(method, "session.setup")
        self.assertEqual(params, {"session_uid": "child-1", "name": "Lead-A", "channels": ["ct/team"],
                                  "board": "team", "group": "Latent", "epic": "g-1", "items": [4],
                                  "poc_of": "g-1", "hire": {"handle": "lead-a"}})

    async def test_no_setup_params_means_no_setup_call_and_a_failed_setup_keeps_the_spawn(self):
        from mcp_server import control_client

        result, calls = await self.spawn({"setup": {}})
        self.assertNotIn("setup", result)
        self.assertEqual([m for m, _ in calls], ["mcp_start_session"])
        result, _ = await self.spawn(control_client.ControlError("unauthorized", "not_spawner: x"),
                                     name="Lead-A")
        self.assertEqual(result["session_uid"], "child-1")
        self.assertEqual(result["setup"], {"error": "unauthorized: not_spawner: x"})

    def test_org_setup_runs_the_same_call_for_a_running_child(self):
        from unittest.mock import patch

        from mcp_server import control_client, server

        with patch.object(control_client, "call", return_value={"setup": {"board": "ok"}}) as call:
            out = server.org(action="setup", session_uid="child-1", board="team", items=[2])
        self.assertEqual(out, {"session_uid": "child-1", "setup": {"board": "ok"}})
        self.assertEqual(call.call_args.args, ("session.setup", {"session_uid": "child-1", "board": "team",
                                                                 "items": [2]}))


class NameBeforePrompt(unittest.IsolatedAsyncioTestCase):
    """msgfix's race: the name rides the spawn call itself, so the daemon claims
    it before the first prompt; setup then runs the remaining steps."""

    async def test_the_name_goes_with_the_spawn_and_comes_back_in_setup(self):
        from pathlib import Path as P
        from unittest.mock import patch

        from mcp_server import control_client, server

        calls = []

        def call(method, params, **_):
            calls.append((method, dict(params)))
            if method == "session.setup":
                return {"setup": {"board": "ok"}}
            return {"session_uid": "child-1", "prompt_source": "caller", "name_setup": "ok"}

        with (
            patch.object(control_client, "resolve_socket_route",
                         return_value=control_client.SocketRoute(path=P("/d.sock"), chose_daemon=True)),
            patch.object(control_client, "call", side_effect=call),
            patch.object(server.async_monitor, "register_monitor", return_value={"monitor_id": "m"}),
        ):
            result = await server.start_session("claude-code", "lead", prompt="go", name="Lead-A",
                                                 board="team")
        self.assertEqual(calls[0][0], "mcp_start_session")
        self.assertEqual(calls[0][1]["name"], "Lead-A")
        self.assertEqual(calls[1], ("session.setup", {"session_uid": "child-1", "board": "team"}))
        self.assertEqual(result["setup"], {"name": "ok", "board": "ok"})
        self.assertNotIn("name_setup", result)
