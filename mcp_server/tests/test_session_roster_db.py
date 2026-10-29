"""Roster contracts against a disposable scratch Postgres only."""

import os
import unittest
import uuid

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")
from dispatch import db, session_roster  # noqa: E402
from dispatch.items_rules import ItemsError  # noqa: E402

DSN = os.environ.get("CM_ITEMS_TEST_DSN")


def filters(**overrides):
    return {"host": None, "member": None, "manager": None, "state": None,
            "task": None, "include_exited": False, "limit": 100, "cursor": None,
            **overrides}


def caller(**overrides):
    return {"pid": "agent:d1:lead", "daemon_id": "d1", "session_uid": "lead",
            "workspace_id": "w", "workspace_uids": [], "task_id": None,
            "generation": 1, "process_identity": 101, "global": False, "owner": False,
            **overrides}


@unittest.skipUnless(DSN, "set CM_ITEMS_TEST_DSN to a disposable scratch database")
class RosterDb(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        import asyncpg
        self.pool = await asyncpg.create_pool(DSN, min_size=1, max_size=4, init=db._init_connection)
        async with self.pool.acquire() as conn:
            await conn.execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public")
        await db.init_db(self.pool)
        async with self.pool.acquire() as conn:
            await conn.execute("""INSERT INTO members(id,handle,kind,manager_member_id,revision)
                VALUES ('mem_lead','lead','agent','mem_owner',3),
                       ('mem_child','child','agent','mem_lead',1),
                       ('mem_other','other','agent','mem_owner',1)""")
            await conn.execute("""INSERT INTO incarnations(member_id,role,pid,daemon_id,session_uid,occupancy_generation)
                VALUES ('mem_lead','main','agent:d1:lead','d1','lead',2),
                       ('mem_child','main','agent:d2:child','d2','child',1),
                       ('mem_other','main','agent:d2:other','d2','other',1)""")
            self.root = await conn.fetchval("INSERT INTO tasks(repo_url,name) VALUES ('r','root') RETURNING id")
            self.direct = await conn.fetchval("INSERT INTO tasks(repo_url,name,parent_task_id) VALUES ('r','direct',$1) RETURNING id", self.root)
            self.filed = await conn.fetchval("INSERT INTO tasks(repo_url,name,metadata) VALUES ('r','filed',$1) RETURNING id", {"filer": {"task_id": str(self.direct)}})
            self.filed2 = await conn.fetchval("INSERT INTO tasks(repo_url,name,metadata) VALUES ('r','filed2',$1) RETURNING id", {"filer": {"task_id": str(self.filed)}})
            await conn.execute("""INSERT INTO session_states(pid,daemon_id,session_uid,host_label,task_id,name,engine,state,reported_at)
                VALUES ('agent:d1:lead','d1','lead','same',$1,'lead','codex','working',now()),
                       ('agent:d2:child','d2','child','same',NULL,'child','claude-code','idle',now()),
                       ('agent:d2:other','d2','other','same',NULL,'other','codex','idle',now()),
                       ('agent:d2:tasked','d2','tasked','same',$2,'tasked','codex','idle',now()),
                       ('agent:d1:unknown','d1','unknown','same',$1,'unknown',NULL,'unknown',now()),
                       ('agent:d1:shell','d1','shell','same',$1,'shell','bash','unknown',now())""", self.root, self.filed2)
            await conn.execute("""INSERT INTO org_session_facts(pid,daemon_id,session_uid,member_id,acting_as,reported_at)
                VALUES ('agent:d2:assistant','d2','assistant','mem_child','assistant',now())""")
            await conn.execute("""INSERT INTO session_states
                (pid,daemon_id,session_uid,host_label,name,engine,state,reported_at,roster_stamp_member,roster_stamp_role)
                VALUES ('agent:d2:assistant','d2','assistant','same','assistant','codex','working',now(),'mem_child','assistant')""")

    async def asyncTearDown(self):
        await self.pool.close()

    async def query(self, c=None, f=None):
        return await session_roster.read(self.pool, c or caller(task_id=str(self.root)), f or filters(), b"fixture-key")

    async def test_union_scope_filter_before_count_and_ambiguous_host(self):
        out = await self.query()
        self.assertEqual({r["session"]["uid"] for r in out["rows"]}, {"lead", "child", "assistant", "tasked"})
        self.assertEqual(out["coverage"]["unknown_kind_omitted"], 1)
        self.assertEqual(next(r for r in out["rows"] if r["session"]["uid"] == "assistant")["member"]["acting_as"], "assistant")
        self.assertEqual(next(r for r in out["rows"] if r["session"]["uid"] == "tasked")["member"]["source"], "unknown")
        with self.assertRaises(ItemsError) as exc:
            await self.query(f=filters(host="same"))
        self.assertEqual(exc.exception.code, "ambiguous_host")
        remote = await self.query(f=filters(host="d2", member="child"))
        self.assertEqual({r["session"]["uid"] for r in remote["rows"]}, {"child", "assistant"})

    async def test_cursor_scope_changes_and_global_scope(self):
        first = await self.query(f=filters(limit=1))
        self.assertTrue(first["more"])
        second = await self.query(f=filters(limit=1, cursor=first["next_cursor"]))
        self.assertNotEqual(first["rows"][0]["session"]["pid"], second["rows"][0]["session"]["pid"])
        with self.assertRaises(ItemsError):
            await self.query(f=filters(limit=1, cursor=first["next_cursor"], state="idle"))
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE members SET revision=revision+1 WHERE id='mem_lead'")
        with self.assertRaises(ItemsError):
            await self.query(f=filters(limit=1, cursor=first["next_cursor"]))
        full = await self.query(c=caller(**{"global": True}))
        self.assertIn("other", {r["session"]["uid"] for r in full["rows"]})

    async def test_taskless_local_workspace_and_stale_attribution(self):
        c = caller(pid="agent:d1:unbound", session_uid="unbound", task_id=None,
                   workspace_uids=["local"])
        async with self.pool.acquire() as conn:
            await conn.execute("""INSERT INTO session_states(pid,daemon_id,session_uid,host_label,name,engine,state,reported_at)
                VALUES ('agent:d1:local','d1','local','same','local','codex','idle',now()),
                       ('agent:d2:local','d2','local','same','remote','codex','idle',now())""")
            await conn.execute("UPDATE org_session_facts SET reported_at=now()-interval '91 seconds' WHERE pid='agent:d2:assistant'")
        local = await self.query(c=c)
        self.assertEqual([r["session"]["pid"] for r in local["rows"]], ["agent:d1:local"])
        lead = await self.query()
        self.assertNotIn("assistant", {r["session"]["uid"] for r in lead["rows"]})

    async def test_retained_org_stamp_is_not_fresh_proof_when_session_beat_omits_it(self):
        async with self.pool.acquire() as conn:
            await conn.execute("""UPDATE session_states SET task_id=$1,
                roster_stamp_member=NULL, roster_stamp_role=NULL, reported_at=now()
                WHERE pid='agent:d2:assistant'""", self.root)
            await conn.execute("UPDATE org_session_facts SET reported_at=now() WHERE pid='agent:d2:assistant'")
        out = await self.query()
        row = next(r for r in out["rows"] if r["session"]["uid"] == "assistant")
        self.assertIsNone(row["member"]["id"])
        self.assertEqual(row["member"]["source"], "unknown")

    async def test_rebinding_exposes_only_the_open_main_seat(self):
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE incarnations SET ended_at=now() WHERE member_id='mem_child' AND ended_at IS NULL")
            await conn.execute("""INSERT INTO incarnations(member_id,role,pid,daemon_id,session_uid,occupancy_generation)
                VALUES ('mem_child','main','agent:d1:new-main','d1','new-main',2)""")
            await conn.execute("""INSERT INTO session_states(pid,daemon_id,session_uid,host_label,name,engine,state,reported_at)
                VALUES ('agent:d1:new-main','d1','new-main','same','new-main','codex','idle',now())""")
        out = await self.query()
        found = {r["session"]["uid"]: r for r in out["rows"]}
        self.assertNotIn("child", found)
        self.assertEqual(found["new-main"]["member"]["acting_as"], "main")
        self.assertEqual(found["new-main"]["member"]["occupancy_generation"], 2)

    async def test_holds_and_stale_state_keep_sources_distinct(self):
        async with self.pool.acquire() as conn:
            board = await conn.fetchval("INSERT INTO boards(slug,name) VALUES ('fixture-board','fixture') RETURNING id")
            item = await conn.fetchval("""INSERT INTO items(board_id,number,title,status)
                VALUES ($1,1,'held','active') RETURNING id""", board)
            await conn.execute("INSERT INTO item_holders(item_id,pid,role) VALUES ($1,'agent:d2:child','reviewer')", item)
            await conn.execute("""UPDATE session_states SET reported_at=now()-interval '91 seconds',
                agent_state=$1 WHERE pid='agent:d2:child'""", {
                    "source": "relay", "since": 2.0, "observed_at": 3.0,
                    "detail": {"error_kind": "previous_response_not_found"}})
        out = await self.query(f=filters(state="stale"))
        row = out["rows"][0]
        self.assertEqual(row["activity"]["state"], "stale")
        self.assertEqual(row["activity"]["last_known_state"], "idle")
        self.assertEqual(row["activity"]["observed_at_host_clock"], 3.0)
        self.assertEqual(row["limit"]["recovery"], "not_exported")
        self.assertEqual(row["limit"]["error_kind"], "previous_response_not_found")
        self.assertEqual(row["items"], [{"board": "fixture-board", "board_id": str(board),
                         "board_slug_omitted": False, "n": 1, "title": "held",
                         "title_truncated": False, "status": "active", "role": "reviewer"}])

    async def test_seventy_five_rows_fit_one_page_and_unknown_scope_is_not_counted(self):
        async with self.pool.acquire() as conn:
            await conn.executemany("""INSERT INTO session_states
                (pid,daemon_id,session_uid,host_label,task_id,name,engine,state,reported_at)
                VALUES ($1,'d3',$2,'batch',$3,$2,'codex','idle',now())""",
                [(f"agent:d3:u{i:03d}", f"u{i:03d}", self.root) for i in range(75)])
            await conn.execute("""INSERT INTO session_states(pid,daemon_id,session_uid,host_label,engine,state,reported_at)
                VALUES ('agent:d4:hidden','d4','hidden','hidden',NULL,'unknown',now())""")
        out = await self.query(f=filters(host="d3"))
        self.assertEqual(len(out["rows"]), 75)
        self.assertFalse(out["more"])
        self.assertEqual(out["coverage"]["unknown_kind_omitted"], 0)

    async def test_reachable_cycle_refuses_instead_of_granting_partial_reachability(self):
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE tasks SET metadata=$1 WHERE id=$2",
                               {"filer": {"task_id": str(self.filed2)}}, self.root)
        with self.assertRaises(ItemsError) as exc:
            await self.query()
        self.assertEqual(exc.exception.code, "scope_cycle")
