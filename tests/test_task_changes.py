"""Incremental task feed (``GET /tasks/changes``) against a real PostgreSQL.

Boots a throwaway PostgreSQL 17 cluster, runs every migration in ``sql/``
inside a private schema, mounts the real FastAPI app over httpx's ASGI
transport (real HTTP semantics: auth header, query params, gzip negotiation)
and drives the feed through the same code paths the TUI uses. Covers the
behaviours the design promises: consistent snapshot + cursor, every write
path (HTTP, in-process dispatcher, raw SQL), removals (delete / archive /
filter), initiative display metadata, expired / foreign / future cursors,
long-poll wake-ups, pagination, and the commit-order guarantee of the
advisory-lock serialisation.
"""

from __future__ import annotations

import asyncio
import gzip
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import uuid

ROOT = Path(__file__).resolve().parents[1]
PG_BIN = Path("/usr/lib/postgresql/17/bin")
PG_PORT = 55468

# dispatch.config reads these once at import; another test module in the same
# process may have imported it first with its own stubs, so use whatever the
# imported modules actually hold (the pool and broker below are wired by hand).
os.environ.setdefault("CM_DB_DSN", f"postgresql://postgres@localhost:{PG_PORT}/postgres")
os.environ.setdefault("CM_API_TOKEN", "test-token")

import asyncpg  # noqa: E402
import httpx  # noqa: E402

from api import auth as api_auth  # noqa: E402
from api import main as api_main  # noqa: E402
from api.task_changes import ChangeBroker  # noqa: E402
from dispatch import db  # noqa: E402

AUTH = {"Authorization": f"Bearer {api_auth.API_TOKEN}"}


@unittest.skipUnless((PG_BIN / "initdb").exists(), "PostgreSQL 17 test binaries required")
class TaskChangesTests(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="cm-task-changes-")
        cls.dir = Path(cls.temp.name)
        subprocess.run([str(PG_BIN / "initdb"), "-D", str(cls.dir / "data"),
                        "--no-locale", "--encoding=UTF8", "--auth=trust", "-U", "postgres"],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        subprocess.run([str(PG_BIN / "pg_ctl"), "-D", str(cls.dir / "data"),
                        "-l", str(cls.dir / "server.log"), "-o",
                        f"-F -k {cls.dir} -h 127.0.0.1 -p {PG_PORT}", "-w", "start"],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

    @classmethod
    def tearDownClass(cls):
        subprocess.run([str(PG_BIN / "pg_ctl"), "-D", str(cls.dir / "data"),
                        "-m", "fast", "-w", "stop"], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        cls.temp.cleanup()

    async def asyncSetUp(self):
        self.schema = "test_" + uuid.uuid4().hex
        self.dsn = f"postgresql://postgres@127.0.0.1:{PG_PORT}/postgres?options=-csearch_path%3D{self.schema}"
        admin = await asyncpg.connect(host="127.0.0.1", port=PG_PORT, user="postgres",
                                      database="postgres")
        await admin.execute(f'CREATE SCHEMA "{self.schema}"')
        await admin.close()
        self.pool = await asyncpg.create_pool(host="127.0.0.1", port=PG_PORT, user="postgres",
                                             database="postgres", min_size=1, max_size=6,
                                             init=db._init_connection,
                                             server_settings={"search_path": self.schema})
        await db.init_db(self.pool)
        self.broker = ChangeBroker(self.dsn)
        self.broker.start()
        api_main.app.state.pool = self.pool
        api_main.app.state.change_broker = self.broker
        self.transport = httpx.ASGITransport(app=api_main.app)
        self.client = httpx.AsyncClient(transport=self.transport, base_url="http://cm",
                                        headers=AUTH)
        # Wait for the LISTEN connection so wake-up tests are deterministic.
        for _ in range(100):
            if self.broker.listening:
                break
            await asyncio.sleep(0.05)
        self.assertTrue(self.broker.listening)

    async def asyncTearDown(self):
        await self.client.aclose()
        await self.broker.stop()
        async with self.pool.acquire() as conn:
            await conn.execute(f'DROP SCHEMA "{self.schema}" CASCADE')
        await self.pool.close()

    # -- helpers -----------------------------------------------------------

    async def create(self, name: str, **extra) -> dict:
        body = {"repo_url": "git@example:repo.git", "name": name, "prompt": f"prompt {name}",
                "project": "proj", **extra}
        r = await self.client.post("/tasks", json=body)
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def changes(self, since, epoch, **params) -> dict:
        q = {"since": since, "epoch": epoch, **params}
        r = await self.client.get("/tasks/changes", params=q)
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def snapshot(self, **params) -> dict:
        r = await self.client.get("/tasks/changes", params=params)
        self.assertEqual(r.status_code, 200, r.text)
        body = r.json()
        self.assertTrue(body["reset"])
        return body

    @staticmethod
    def apply(cache: dict, page: dict) -> None:
        """The client-side merge rule the TUI implements."""
        if page["reset"]:
            cache.clear()
            cache.update({t["id"]: t for t in page["tasks"]})
            return
        for ch in page["changes"]:
            if ch["op"] == "upsert":
                cache[ch["task_id"]] = ch["task"]
            else:
                cache.pop(ch["task_id"], None)

    # -- tests -------------------------------------------------------------

    async def test_requires_auth(self):
        r = await self.client.get("/tasks/changes", headers={"Authorization": "Bearer nope"})
        self.assertEqual(r.status_code, 401)
        r = await self.client.get("/tasks/changes", headers={"Authorization": ""})
        self.assertEqual(r.status_code, 401)

    async def test_snapshot_then_incremental_updates(self):
        a = await self.create("a")
        b = await self.create("b")
        snap = await self.snapshot()
        cache: dict = {}
        self.apply(cache, snap)
        self.assertEqual(set(cache), {a["id"], b["id"]})
        cursor, epoch = snap["cursor"], snap["epoch"]

        # Idle: nothing changed -> empty page, same cursor.
        idle = await self.changes(cursor, epoch)
        self.assertEqual(idle, {"epoch": epoch, "cursor": cursor, "reset": False,
                                "tasks": None, "changes": [], "more": False})

        # Edits through the HTTP API: status, priority, name.
        r = await self.client.patch(f"/tasks/{a['id']}", json={"status": "running", "priority": 7})
        self.assertEqual(r.status_code, 200, r.text)
        r = await self.client.patch(f"/tasks/{b['id']}", json={"name": "b2"})
        self.assertEqual(r.status_code, 200, r.text)
        page = await self.changes(cursor, epoch)
        self.assertFalse(page["reset"])
        self.assertEqual([c["op"] for c in page["changes"]], ["upsert", "upsert"])
        self.assertEqual([c["task_id"] for c in page["changes"]], [a["id"], b["id"]])
        self.assertGreater(page["cursor"], cursor)
        self.apply(cache, page)
        self.assertEqual(cache[a["id"]]["status"], "running")
        self.assertEqual(cache[a["id"]]["priority"], 7)
        self.assertEqual(cache[b["id"]]["name"], "b2")
        # Full row shape (what /tasks returns) travels in each change.
        self.assertEqual(cache[a["id"]]["prompt"], "prompt a")
        cursor = page["cursor"]

        # Several edits to one task collapse to one entry carrying the latest row.
        for i in range(3):
            await self.client.patch(f"/tasks/{a['id']}", json={"priority": i})
        page = await self.changes(cursor, epoch)
        self.assertEqual(len(page["changes"]), 1)
        self.assertEqual(page["changes"][0]["task"]["priority"], 2)
        self.apply(cache, page)
        cursor = page["cursor"]

        # Addition appears as an upsert.
        c = await self.create("c")
        page = await self.changes(cursor, epoch)
        self.assertEqual([(x["op"], x["task_id"]) for x in page["changes"]], [("upsert", c["id"])])
        self.apply(cache, page)
        cursor = page["cursor"]

        # The cache now equals the server's list exactly.
        listed = (await self.client.get("/tasks")).json()
        self.assertEqual({t["id"]: t for t in listed}, cache)

    async def test_archive_delete_and_unarchive(self):
        a = await self.create("a")
        b = await self.create("b")
        snap = await self.snapshot()
        cache: dict = {}
        self.apply(cache, snap)
        cursor, epoch = snap["cursor"], snap["epoch"]

        # Archive -> remove (archived rows are outside the default subscription).
        await self.client.patch(f"/tasks/{a['id']}", json={"status": "archived"})
        # Hard delete -> remove.
        r = await self.client.delete(f"/tasks/{b['id']}")
        self.assertEqual(r.status_code, 200)
        page = await self.changes(cursor, epoch)
        self.assertEqual([(x["op"], x["task_id"], x["task"]) for x in page["changes"]],
                         [("remove", a["id"], None), ("remove", b["id"], None)])
        self.apply(cache, page)
        self.assertEqual(cache, {})
        cursor = page["cursor"]

        # Unarchive -> upsert again.
        await self.client.patch(f"/tasks/{a['id']}", json={"status": "backlog"})
        page = await self.changes(cursor, epoch)
        self.assertEqual([(x["op"], x["task_id"]) for x in page["changes"]], [("upsert", a["id"])])
        self.apply(cache, page)
        self.assertEqual(set(cache), {a["id"]})

        # A subscription that includes archived rows sees archive as an upsert.
        snap2 = await self.snapshot(include_archived="true")
        await self.client.patch(f"/tasks/{a['id']}", json={"status": "archived"})
        page = await self.changes(snap2["cursor"], snap2["epoch"], include_archived="true")
        self.assertEqual(page["changes"][0]["op"], "upsert")
        self.assertEqual(page["changes"][0]["task"]["status"], "archived")

    async def test_project_filter_and_move_between_projects(self):
        a = await self.create("a", project="p1")
        await self.create("b", project="p2")
        snap = await self.snapshot(project="p1")
        self.assertEqual([t["id"] for t in snap["tasks"]], [a["id"]])
        await self.client.patch(f"/tasks/{a['id']}", json={"project": "p2"})
        page = await self.changes(snap["cursor"], snap["epoch"], project="p1")
        self.assertEqual([(x["op"], x["task_id"]) for x in page["changes"]], [("remove", a["id"])])

    async def test_writes_outside_the_http_endpoint_are_delivered(self):
        # The dispatcher only claims cloud tasks without a project.
        a = await self.create("a", is_cloud=True, project=None)
        snap = await self.snapshot()
        cursor, epoch = snap["cursor"], snap["epoch"]
        # In-process dispatcher paths: claim (SELECT ... FOR UPDATE SKIP LOCKED
        # inside an UPDATE) and a plain field update.
        claimed = await db.claim_next_task(self.pool)
        self.assertEqual(claimed["id"], a["id"])
        await db.update_task(self.pool, a["id"], worker_vm="vm-1")
        # Raw SQL from a foreign client (psql, a script): still logged by trigger.
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE tasks SET priority = 42 WHERE id = $1", a["id"])
        page = await self.changes(cursor, epoch)
        self.assertEqual(len(page["changes"]), 1)
        task = page["changes"][0]["task"]
        self.assertEqual((task["status"], task["worker_vm"], task["priority"]),
                         ("running", "vm-1", 42))
        # Raw DELETE too.
        async with self.pool.acquire() as conn:
            await conn.execute("DELETE FROM tasks WHERE id = $1", a["id"])
        page = await self.changes(page["cursor"], epoch)
        self.assertEqual([(x["op"], x["task_id"]) for x in page["changes"]], [("remove", a["id"])])

    async def test_initiative_display_metadata_redelivers_member_tasks(self):
        coord = await self.create("coordinator")
        member = await self.create("member")
        r = await self.client.post("/initiatives", json={
            "name": "Init One", "coordinator_task_id": coord["id"], "color": "red"})
        self.assertEqual(r.status_code, 200, r.text)
        init = r.json()
        # Members may only join through an approved initiative project.
        r = await self.client.post(f"/initiatives/{init['id']}/projects", json={"project": "proj"})
        self.assertEqual(r.status_code, 200, r.text)
        r = await self.client.patch(f"/initiatives/{init['id']}/projects/proj",
                                    json={"status": "approved"})
        self.assertEqual(r.status_code, 200, r.text)
        r = await self.client.patch(f"/tasks/{member['id']}", json={"initiative_id": init["id"]})
        self.assertEqual(r.status_code, 200, r.text)
        snap = await self.snapshot()
        cache: dict = {}
        self.apply(cache, snap)
        self.assertEqual(cache[member["id"]]["initiative"]["name"], "Init One")

        r = await self.client.patch(f"/initiatives/{init['id']}", json={"name": "Init Renamed",
                                                                          "color": "blue"})
        self.assertEqual(r.status_code, 200, r.text)
        page = await self.changes(snap["cursor"], snap["epoch"])
        ids = {x["task_id"] for x in page["changes"]}
        self.assertEqual(ids, {coord["id"], member["id"]})
        self.apply(cache, page)
        for tid in ids:
            self.assertEqual(cache[tid]["initiative"]["name"], "Init Renamed")
            self.assertEqual(cache[tid]["initiative"]["color"], "blue")

        # An initiative edit that touches no display field emits nothing.
        r = await self.client.patch(f"/initiatives/{init['id']}",
                                    json={"description": "longer text"})
        self.assertEqual(r.status_code, 200, r.text)
        page2 = await self.changes(page["cursor"], snap["epoch"])
        self.assertEqual(page2["changes"], [])

    async def test_expired_foreign_and_future_cursors_resnapshot(self):
        a = await self.create("a")
        snap = await self.snapshot()
        cursor, epoch = snap["cursor"], snap["epoch"]

        # Foreign epoch -> reset.
        page = await self.changes(cursor, "another-lineage")
        self.assertTrue(page["reset"])
        self.assertEqual([t["id"] for t in page["tasks"]], [a["id"]])

        # Future cursor (e.g. the server DB was restored) -> reset.
        page = await self.changes(cursor + 1000, epoch)
        self.assertTrue(page["reset"])

        # Expired cursor: prune everything logged so far, then a client that
        # held a cursor from before the prune must resnapshot, while a client
        # at the pruned boundary keeps catching up incrementally.
        await self.client.patch(f"/tasks/{a['id']}", json={"priority": 1})
        page = await self.changes(cursor, epoch)
        self.assertEqual(len(page["changes"]), 1)
        after = page["cursor"]
        deleted = await db.prune_task_changes(self.pool, keep_seconds=0)
        self.assertGreaterEqual(deleted, 1)
        page = await self.changes(cursor, epoch)
        self.assertTrue(page["reset"], "cursor below pruned_through must reset")
        self.assertEqual(page["cursor"], after)
        page = await self.changes(after, epoch)
        self.assertFalse(page["reset"])
        self.assertEqual(page["changes"], [])
        # And the log keeps working after a prune.
        await self.client.patch(f"/tasks/{a['id']}", json={"priority": 2})
        page = await self.changes(after, epoch)
        self.assertEqual(page["changes"][0]["task"]["priority"], 2)
        # A fresh snapshot after a full prune anchors its cursor at the pruned
        # boundary, not at 0.
        snap = await self.snapshot()
        self.assertGreaterEqual(snap["cursor"], after)

    async def test_long_poll_wakes_on_change_and_times_out_idle(self):
        a = await self.create("a")
        snap = await self.snapshot()
        cursor, epoch = snap["cursor"], snap["epoch"]
        loop = asyncio.get_running_loop()

        t0 = loop.time()
        idle = await self.changes(cursor, epoch, wait=0.3)
        self.assertEqual(idle["changes"], [])
        self.assertGreaterEqual(loop.time() - t0, 0.25)

        async def poke():
            await asyncio.sleep(0.2)
            await self.client.patch(f"/tasks/{a['id']}", json={"name": "woken"})

        t0 = loop.time()
        poker = asyncio.create_task(poke())
        page = await self.changes(cursor, epoch, wait=5)
        await poker
        elapsed = loop.time() - t0
        self.assertLess(elapsed, 2.0, "long poll must return promptly on NOTIFY")
        self.assertEqual(page["changes"][0]["task"]["name"], "woken")

    async def test_pagination_more_flag(self):
        tasks = [await self.create(f"t{i}") for i in range(5)]
        snap = await self.snapshot()
        cursor, epoch = snap["cursor"], snap["epoch"]
        for t in tasks:
            await self.client.patch(f"/tasks/{t['id']}", json={"priority": 1})
        seen: list[str] = []
        page = await self.changes(cursor, epoch, limit=2)
        self.assertTrue(page["more"])
        seen += [c["task_id"] for c in page["changes"]]
        while page["more"]:
            page = await self.changes(page["cursor"], epoch, limit=2)
            seen += [c["task_id"] for c in page["changes"]]
        self.assertEqual(seen, [t["id"] for t in tasks])

    async def test_seq_order_is_commit_order(self):
        """A transaction that drew a seq but has not committed blocks later
        writers (advisory lock), so a reader can never observe seq N+1 while
        seq N is still pending — the gap that a naked sequence/updated_at
        cursor would skip forever."""
        a = await self.create("a")
        b = await self.create("b")
        snap = await self.snapshot()
        cursor, epoch = snap["cursor"], snap["epoch"]

        t1 = await asyncpg.connect(self.dsn)
        t2 = await asyncpg.connect(self.dsn)
        try:
            tx1 = t1.transaction()
            await tx1.start()
            await t1.execute("UPDATE tasks SET priority = 10 WHERE id = $1", a["id"])
            done2 = asyncio.Event()

            async def writer2():
                await t2.execute("UPDATE tasks SET priority = 20 WHERE id = $1", b["id"])
                done2.set()

            w2 = asyncio.create_task(writer2())
            await asyncio.sleep(0.4)
            self.assertFalse(done2.is_set(), "second writer must wait for the first commit")
            # Nothing is visible yet either (t1 uncommitted, t2 blocked).
            page = await self.changes(cursor, epoch)
            self.assertEqual(page["changes"], [])
            await tx1.commit()
            await asyncio.wait_for(w2, 5)
            page = await self.changes(cursor, epoch)
            self.assertEqual([c["task_id"] for c in page["changes"]], [a["id"], b["id"]])
            self.assertLess(page["changes"][0]["seq"], page["changes"][1]["seq"])
        finally:
            await t1.close()
            await t2.close()

    async def _deadlock_shape(self, serialize_first: bool) -> tuple[bool, list[str]]:
        """T1: transaction that row-locks the initiative, then writes tasks.
        T2: a concurrent single-statement initiative UPDATE (trigger takes the
        advisory lock first, then waits on the row). Returns (deadlocked,
        change ops delivered). Deterministic: T2 is provably waiting before
        T1 issues its task write."""
        coord = await self.create("coordinator")
        r = await self.client.post("/initiatives", json={
            "name": "Init", "coordinator_task_id": coord["id"]})
        self.assertEqual(r.status_code, 200, r.text)
        init = r.json()
        snap = await self.snapshot()
        t1 = await asyncpg.connect(self.dsn)
        t2 = await asyncpg.connect(self.dsn)
        deadlocked = False
        try:
            tx1 = t1.transaction()
            await tx1.start()
            if serialize_first:
                await db._serialize_task_writes(t1)
            await t1.fetchrow("SELECT * FROM initiatives WHERE id = $1 FOR UPDATE", init["id"])

            async def writer2():
                await t2.execute("UPDATE initiatives SET color = 'red' WHERE id = $1", init["id"])

            w2 = asyncio.create_task(writer2())
            # Wait until T2 is genuinely blocked on a lock (not merely scheduled).
            for _ in range(100):
                waiting = await self.pool.fetchval(
                    "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock' "
                    "AND query LIKE 'UPDATE initiatives SET color%'")
                if waiting:
                    break
                await asyncio.sleep(0.02)
            self.assertTrue(waiting, "T2 must be waiting on a lock")
            try:
                await t1.execute(
                    "UPDATE tasks SET priority = 5, updated_at = now() WHERE id = $1", coord["id"])
                await tx1.commit()
            except asyncpg.DeadlockDetectedError:
                deadlocked = True
                await tx1.rollback()
            try:
                await asyncio.wait_for(w2, 10)
            except asyncpg.DeadlockDetectedError:
                deadlocked = True
        finally:
            await t1.close()
            await t2.close()
        page = await self.changes(snap["cursor"], snap["epoch"])
        return deadlocked, [c["op"] for c in page["changes"]]

    async def test_row_lock_before_task_write_deadlocks_without_serialize(self):
        """The hazard the application helper exists for: a transaction holding
        a row lock taken by an EARLIER statement, then writing tasks, deadlocks
        against a concurrent trigger-serialised writer."""
        deadlocked, _ops = await self._deadlock_shape(serialize_first=False)
        self.assertTrue(deadlocked, "expected PostgreSQL to detect the lock cycle")

    async def test_serialized_transaction_does_not_deadlock(self):
        """With the advisory lock taken first, the same interleaving serialises:
        T2 waits on the advisory lock (not the row), T1 completes, T2 follows,
        and the feed delivers both commits in order."""
        deadlocked, ops = await self._deadlock_shape(serialize_first=True)
        self.assertFalse(deadlocked)
        self.assertEqual(ops, ["upsert"], "coordinator task delivered once (collapsed)")

    async def test_helper_key_matches_trigger_key(self):
        trigger_src = await self.pool.fetchval(
            "SELECT prosrc FROM pg_proc WHERE proname = 'task_changes_serialize'")
        self.assertIn("pg_advisory_xact_lock(hashtext('cm_task_changes'))", trigger_src)
        self.assertIn("pg_advisory_xact_lock(hashtext('cm_task_changes'))", db.TASK_WRITE_LOCK_SQL)

    async def test_update_initiative_under_concurrent_initiative_writes(self):
        """Soak the real application paths that mix row locks and task writes:
        update_initiative (FOR UPDATE then UPDATE tasks/initiatives) racing
        single-statement initiative and task updates. Pre-fix this deadlocked
        intermittently; post-fix every iteration must succeed."""
        coord = await self.create("coordinator")
        r = await self.client.post("/initiatives", json={
            "name": "Init", "coordinator_task_id": coord["id"]})
        self.assertEqual(r.status_code, 200, r.text)
        init = r.json()
        other = await self.create("other")
        async with self.pool.acquire() as c:
            await c.execute("SET deadlock_timeout = '50ms'")
        for i in range(25):
            results = await asyncio.gather(
                db.update_initiative(self.pool, init["id"], name=f"n{i}", actor="t"),
                self.pool.execute("UPDATE initiatives SET color = $2 WHERE id = $1", init["id"], f"c{i}"),
                db.update_task(self.pool, coord["id"], priority=i),
                db.update_task(self.pool, other["id"], priority=i),
                return_exceptions=True,
            )
            errors = [x for x in results if isinstance(x, Exception)]
            self.assertEqual(errors, [], f"iteration {i}: {errors}")

    async def test_snapshot_cursor_is_consistent_with_rows(self):
        """Every change with seq <= snapshot cursor is reflected in the rows."""
        a = await self.create("a")
        for i in range(5):
            await self.client.patch(f"/tasks/{a['id']}", json={"priority": i})
        snap = await self.snapshot()
        self.assertEqual(snap["tasks"][0]["priority"], 4)
        page = await self.changes(snap["cursor"], snap["epoch"])
        self.assertEqual(page["changes"], [])

    async def test_gzip_negotiation_on_list_and_feed(self):
        big = "x" * 4000
        await self.create("a", prompt=big)
        r = await self.client.get("/tasks", headers={"Accept-Encoding": "gzip"})
        self.assertEqual(r.headers.get("content-encoding"), "gzip")
        self.assertEqual(r.json()[0]["prompt"], big)
        raw = await self.client.get("/tasks", headers={"Accept-Encoding": "identity"})
        self.assertIsNone(raw.headers.get("content-encoding"))

        async def wire_bytes(path: str, encoding: str) -> tuple[bytes, str | None]:
            # httpx decodes transparently; stream to see the bytes on the wire.
            async with httpx.AsyncClient(transport=self.transport, base_url="http://cm",
                                         headers=AUTH) as c:
                req = c.build_request("GET", path, headers={"Accept-Encoding": encoding})
                resp = await c.send(req, stream=True)
                body = b"".join([chunk async for chunk in resp.aiter_raw()])
                await resp.aclose()
                return body, resp.headers.get("content-encoding")

        raw_body, enc = await wire_bytes("/tasks", "identity")
        self.assertIsNone(enc)
        gz_body, enc = await wire_bytes("/tasks", "gzip")
        self.assertEqual(enc, "gzip")
        self.assertLess(len(gz_body), len(raw_body) // 4)
        self.assertEqual(gzip.decompress(gz_body), raw_body)
        feed_body, enc = await wire_bytes("/tasks/changes", "gzip")
        self.assertEqual(enc, "gzip")
        self.assertIn(b'"reset":true', gzip.decompress(feed_body).replace(b" ", b""))

    async def test_task_list_endpoint_unchanged_for_old_clients(self):
        a = await self.create("a")
        await self.client.patch(f"/tasks/{a['id']}", json={"status": "archived"})
        b = await self.create("b")
        listed = (await self.client.get("/tasks")).json()
        self.assertEqual([t["id"] for t in listed], [b["id"]])
        listed = (await self.client.get("/tasks", params={"include_archived": "true"})).json()
        self.assertEqual({t["id"] for t in listed}, {a["id"], b["id"]})
        single = (await self.client.get(f"/tasks/{a['id']}")).json()
        self.assertEqual(single["status"], "archived")


if __name__ == "__main__":
    unittest.main()
