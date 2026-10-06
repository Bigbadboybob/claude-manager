"""Items API against a real Postgres (doc/items-board.md §6).

The database tests run when CM_ITEMS_TEST_DSN names a scratch database they may
wipe, e.g. a private cluster:

    initdb -D ~/.local/share/<lane>/pg -A trust -U postgres
    pg_ctl -D ~/.local/share/<lane>/pg -o "-k <dir> -c listen_addresses='' -p 55432" start
    createdb -h <dir> -p 55432 -U postgres items_test
    CM_ITEMS_TEST_DSN=postgresql://postgres@/items_test?host=<dir>&port=55432 pytest …

Without it only the migration text checks run.
"""

from __future__ import annotations

import asyncio
import os
import unittest
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

from dispatch import db, items_db  # noqa: E402

ROOT = Path(__file__).parents[2]
DSN = os.environ.get("CM_ITEMS_TEST_DSN")
TOKEN = {"Authorization": f"Bearer {os.environ['CM_API_TOKEN']}"}


def agent(name, daemon="d1"):
    return {"pid": f"agent:{daemon}:{name}-uid", "name": name,
            "session_uid": f"{name}-uid", "daemon_id": daemon}


ORCH = agent("orch")
LANE = agent("lane")


class MigrationText(unittest.TestCase):
    def test_migration_is_rerunnable_ddl_only(self):
        text = (ROOT / "sql" / "017_items.sql").read_text()
        for table in ("boards", "items", "item_holders", "item_deps", "item_events",
                      "item_flags", "session_states", "item_pushes"):
            self.assertIn(f"CREATE TABLE IF NOT EXISTS {table} ", text)
        body = "\n".join(l for l in text.splitlines() if not l.lstrip().startswith("--"))
        self.assertNotIn("UPDATE ", body)
        self.assertNotIn("INSERT ", body)
        self.assertNotIn("task_changes", body)
        self.assertNotRegex(body, r"CREATE (UNIQUE )?INDEX (?!IF NOT EXISTS)")

    def test_migration_number_is_unique(self):
        self.assertEqual(len(list((ROOT / "sql").glob("017_*.sql"))), 1)


@unittest.skipUnless(DSN, "set CM_ITEMS_TEST_DSN to a scratch Postgres database")
class ItemsApiDb(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        import httpx
        from fastapi import FastAPI

        from api.items import router

        self.pool = await __import__("asyncpg").create_pool(
            DSN, min_size=1, max_size=4, init=db._init_connection)
        async with self.pool.acquire() as conn:
            await conn.execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        await db.init_db(self.pool)
        await db.init_db(self.pool)  # migrations must re-run cleanly
        app = FastAPI()
        app.state.pool = self.pool
        app.include_router(router)
        self.http = httpx.AsyncClient(transport=httpx.ASGITransport(app=app),
                                      base_url="http://t", headers=TOKEN)
        self.root = await self._task("root")
        self.board = (await self.post("/boards/resolve", {"task_id": self.root})).json()
        self.ref = self.board["slug"]

    async def asyncTearDown(self):
        await self.http.aclose()
        await self.pool.close()

    async def _task(self, name, parent=None, initiative=None) -> str:
        async with self.pool.acquire() as conn:
            return str(await conn.fetchval(
                """INSERT INTO tasks (repo_url, prompt, name, parent_task_id, initiative_id)
                   VALUES ('r', 'p', $1, $2, $3) RETURNING id""",
                name, uuid.UUID(parent) if parent else None,
                uuid.UUID(initiative) if initiative else None,
            ))

    async def post(self, path, body):
        return await self.http.post(path, json=body)

    async def create(self, *specs, actor=ORCH, ok=True):
        r = await self.post(f"/boards/{self.ref}/items", {
            "actor": actor,
            "items": [s if isinstance(s, dict) else {"title": s} for s in specs],
        })
        if ok:
            self.assertEqual(r.status_code, 200, r.text)
        return r

    async def patch(self, ns, actor=ORCH, ok=True, **kw):
        body = {"actor": actor, "ns": ns if isinstance(ns, list) else [ns]}
        for key in ("add_holders", "remove_holders", "reason"):
            if key in kw:
                body[key] = kw.pop(key)
        body["set"] = kw
        r = await self.http.patch(f"/boards/{self.ref}/items", json=body)
        if ok:
            self.assertEqual(r.status_code, 200, r.text)
        return r

    async def read(self, **params):
        r = await self.http.get(f"/boards/{self.ref}", params=params)
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def fetch(self, sql, *args):
        async with self.pool.acquire() as conn:
            return await conn.fetch(sql, *args)

    # ---- boards ---------------------------------------------------------
    async def test_task_board_is_shared_by_the_subtree(self):
        self.assertEqual(self.ref, f"task-{self.root[:8]}")
        self.assertEqual(self.board["name"], "root")
        child = await self._task("child", parent=self.root)
        grandchild = await self._task("gc", parent=child)
        again = (await self.post("/boards/resolve", {"task_id": grandchild})).json()
        self.assertEqual(again["id"], self.board["id"])
        self.assertEqual(len(await self.fetch("SELECT 1 FROM boards")), 1)

    async def test_initiative_board(self):
        coord = await self._task("coord")
        async with self.pool.acquire() as conn:
            ini = await conn.fetchval(
                """INSERT INTO initiatives (slug, name, coordinator_task_id)
                   VALUES ('sfd', 'Swarm Focused Design', $1) RETURNING id""",
                uuid.UUID(coord))
        lane = await self._task("lane", initiative=str(ini))
        sub = await self._task("sub", parent=lane)
        b = (await self.post("/boards/resolve", {"task_id": sub})).json()
        self.assertEqual((b["slug"], b["name"]), ("sfd", "Swarm Focused Design"))
        self.assertEqual(b["initiative_id"], str(ini))
        by_ref = (await self.post("/boards/resolve", {"ref": "sfd"})).json()
        self.assertEqual(by_ref["id"], b["id"])

    async def test_unknown_task_and_board_are_404(self):
        r = await self.post("/boards/resolve", {"task_id": str(uuid.uuid4())})
        self.assertEqual(r.status_code, 404)
        self.assertEqual(r.json()["detail"]["code"], "not_found")
        self.assertEqual((await self.http.get("/boards/nope")).status_code, 404)

    async def test_auth_required(self):
        r = await self.http.get(f"/boards/{self.ref}", headers={"Authorization": "Bearer x"})
        self.assertEqual(r.status_code, 401)

    async def test_patch_board_settings(self):
        r = await self.http.patch(f"/boards/{self.ref}", json={
            "actor": {"pid": "owner"}, "idle_s": 600, "holder_idle_enabled": True})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(r.json()["settings"]["idle_s"], 600)
        self.assertTrue(r.json()["settings"]["holder_idle_enabled"])
        bad = await self.http.patch(f"/boards/{self.ref}", json={
            "actor": {"pid": "owner"}, "stale_s": 0})
        self.assertEqual(bad.status_code, 422)
        events = await self.fetch("SELECT type, new FROM item_events")
        self.assertEqual(events[0]["type"], "board_updated")

    # ---- writes ---------------------------------------------------------
    async def test_numbering_persists_and_concurrent_creates_never_collide(self):
        await self.create("a", "b")
        results = await asyncio.gather(*(self.create(f"c{i}") for i in range(6)))
        ns = sorted(r.json()["items"][0]["n"] for r in results)
        self.assertEqual(ns, [3, 4, 5, 6, 7, 8])
        rows = await self.fetch("SELECT next_number FROM boards")
        self.assertEqual(rows[0]["next_number"], 9)

    async def test_failed_batch_writes_nothing(self):
        r = await self.create("ok", {"title": "bad", "blocked_by": [42]}, ok=False)
        self.assertEqual(r.status_code, 422)
        self.assertEqual(r.json()["detail"]["code"], "invalid_blocker")
        self.assertEqual(await self.fetch("SELECT 1 FROM items"), [])
        self.assertEqual((await self.fetch("SELECT next_number FROM boards"))[0][0], 1)

    async def test_cycle_refused_over_http(self):
        await self.create("a", "b")
        await self.patch(2, blocked_by=[1])
        r = await self.patch(1, blocked_by=[2], ok=False)
        self.assertEqual(r.status_code, 409)
        self.assertEqual(r.json()["detail"], {"code": "cycle", "message": "cycle: 1→2→1",
                                              "cycle": [1, 2, 1]})
        deps = await self.fetch("SELECT * FROM item_deps")
        self.assertEqual(len(deps), 1)

    async def test_waiting_requires_eta(self):
        await self.create("job")
        r = await self.patch(1, status="waiting", ok=False)
        self.assertEqual(r.json()["detail"]["code"], "eta_required")
        r = await self.patch(1, status="waiting", eta="40m")
        self.assertIsNotNone(r.json()["items"][0]["eta_at"])

    async def test_done_cascade_persists_unblock_push_and_history(self):
        await self.create("a", {"title": "b", "holders": [LANE]})
        await self.patch(2, blocked_by=[1])
        r = await self.patch(1, status="done", note="abc123")
        self.assertEqual(r.json()["unblocked"], [2])
        self.assertEqual(await self.fetch("SELECT * FROM item_deps"), [])
        status = await self.fetch("SELECT number, status, clock_reset_at, closed_at FROM items ORDER BY number")
        self.assertEqual([(s["number"], s["status"]) for s in status], [(1, "done"), (2, "active")])
        self.assertIsNotNone(status[1]["clock_reset_at"])
        self.assertIsNotNone(status[0]["closed_at"])
        pushes = await self.fetch("SELECT kind, daemon_id, session_uid FROM item_pushes ORDER BY id")
        self.assertEqual([p["kind"] for p in pushes], ["assigned", "unblocked"])
        self.assertEqual(pushes[1]["session_uid"], "lane-uid")
        types = [e["type"] for e in await self.fetch(
            "SELECT type FROM item_events WHERE item_number = 2 ORDER BY id")]
        self.assertEqual(types, ["created", "updated", "unblocked"])

    async def test_drop_flags_dependent_and_resolve_repoints(self):
        await self.create("a", "b", "c")
        await self.patch(2, blocked_by=[1])
        await self.patch(1, status="dropped", reason="superseded by 3")
        board = await self.read()
        self.assertEqual([(f["n"], f["kind"]) for f in board["flags"]], [(2, "blocker_dropped")])
        self.assertEqual(board["board"]["health"]["unresolved"], 1)
        r = await self.post(f"/boards/{self.ref}/items/2/resolve", {
            "actor": ORCH, "action": "block", "blocked_by": [3]})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(r.json()["flags_resolved"], ["blocker_dropped"])
        self.assertEqual(r.json()["item"]["blocked_by"], [3])
        board = await self.read()
        self.assertEqual(board["flags"], [])
        flag = (await self.fetch("SELECT resolution, resolved_by FROM item_flags"))[0]
        self.assertEqual((flag["resolution"], flag["resolved_by"]), ("block", ORCH["pid"]))

    async def test_holders_add_remove_and_reopen(self):
        await self.create("a")
        await self.patch(1, add_holders=[LANE])
        await self.patch(1, remove_holders=[ORCH["pid"]])
        holders = await self.fetch("SELECT pid FROM item_holders")
        self.assertEqual([h["pid"] for h in holders], [LANE["pid"]])
        await self.patch(1, status="done")
        await self.patch(1, status="active")
        row = (await self.fetch("SELECT closed_at, status FROM items"))[0]
        self.assertIsNone(row["closed_at"])
        self.assertEqual(row["status"], "active")

    async def test_nudge_resolve_snoozes_and_pushes(self):
        await self.create({"title": "a", "holders": [LANE]})
        async with self.pool.acquire() as conn:
            await conn.execute("INSERT INTO item_flags (item_id, kind) SELECT id, 'stale' FROM items")
        r = await self.post(f"/boards/{self.ref}/items/1/resolve", {
            "actor": ORCH, "action": "nudge"})
        self.assertEqual(r.json()["flags_resolved"], ["stale"])
        flag = (await self.fetch("SELECT snooze_until, raised_at FROM item_flags"))[0]
        self.assertGreater(flag["snooze_until"] - flag["raised_at"], timedelta(seconds=7000))
        kinds = [p["kind"] for p in await self.fetch("SELECT kind FROM item_pushes ORDER BY id")]
        self.assertEqual(kinds, ["assigned", "nudge"])

    # ---- reads ----------------------------------------------------------
    async def test_read_version_holder_state_and_closed_strip(self):
        await self.create({"title": "a", "group": "RL", "holders": [LANE]}, "b")
        board = await self.read()
        version = board["board"]["version"]
        self.assertGreater(version, 0)
        self.assertEqual(await self.read(since_version=version),
                         {"unchanged": True, "version": version})
        lane_state = board["items"][1]["holders"][0]["state"]
        self.assertEqual(lane_state["state"], "unknown")  # no heartbeat yet
        now = datetime.now(timezone.utc)
        async with self.pool.acquire() as conn:
            await conn.execute(
                """INSERT INTO session_states (pid, daemon_id, session_uid, task_id, name,
                                               state, state_since, reported_at)
                   VALUES ($1, 'd1', 'lane-uid', $2, 'lane', 'idle', $3, $4)""",
                LANE["pid"], uuid.UUID(self.root), now - timedelta(minutes=24), now)
        await self.patch(2, status="done")
        board = await self.read(history=5)
        self.assertGreater(board["board"]["version"], version)
        live = board["items"]
        self.assertEqual([i["n"] for i in live], [1])  # grouped, closed item left
        st = live[0]["holders"][0]["state"]
        self.assertEqual(st["state"], "idle")
        self.assertGreaterEqual(st["for_s"], 24 * 60)
        self.assertEqual([i["n"] for i in board["recently_closed"]], [2])
        self.assertEqual(live[0]["history"][-1]["type"], "created")
        orch = board["board"]["orchestrator"]
        self.assertEqual(orch["pid"], LANE["pid"])  # only live session on the root task
        self.assertEqual(board["free_capacity"], [])  # lane holds an active item
        await self.patch(1, status="done")
        board = await self.read()
        self.assertEqual([s["pid"] for s in board["free_capacity"]], [LANE["pid"]])

    async def test_archived_items_leave_the_board_but_stay_searchable(self):
        await self.create({"title": "fuse SEJD", "note": "PR-ready"}, "other")
        await self.patch(1, status="done")
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE items SET archived_at = now() WHERE number = 1")
        board = await self.read()
        self.assertEqual(board["recently_closed"], [])
        self.assertNotIn("archived", board)
        board = await self.read(archived="true", q="sejd")
        self.assertEqual([i["n"] for i in board["archived"]], [1])
        self.assertEqual(board["items"], [])

    async def test_held_items_and_board_list(self):
        await self.create({"title": "a", "holders": [LANE]}, {"title": "b", "holders": [LANE]})
        await self.patch(2, status="done")
        r = await self.http.get("/items", params={"holder_pid": LANE["pid"]})
        self.assertEqual(r.json(), [{"board": self.ref, "n": 1, "title": "a", "status": "active"}])
        boards = (await self.http.get("/boards", params={"open_only": "true"})).json()
        self.assertEqual([b["slug"] for b in boards], [self.ref])


if __name__ == "__main__":
    unittest.main()
