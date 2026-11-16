"""Lead-bound queue semantics against a disposable PostgreSQL cluster."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import uuid

os.environ.setdefault("CM_DB_DSN", "postgresql://unused")
os.environ.setdefault("CM_API_TOKEN", "unused")
import asyncpg

from dispatch import db


ROOT = Path(__file__).resolve().parents[1]
PG_BIN = Path("/usr/lib/postgresql/17/bin")


@unittest.skipUnless((PG_BIN / "initdb").exists(), "PostgreSQL 17 test binaries required")
class QueueBindingTests(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="cm-queue-binding-")
        cls.dir = Path(cls.temp.name)
        subprocess.run(
            [str(PG_BIN / "initdb"), "-D", str(cls.dir / "data"), "--no-locale",
             "--encoding=UTF8", "--auth=trust"],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        subprocess.run(
            [str(PG_BIN / "pg_ctl"), "-D", str(cls.dir / "data"), "-l",
             str(cls.dir / "server.log"), "-o", f"-F -k {cls.dir} -h '' -p 55471",
             "-w", "start"], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )

    @classmethod
    def tearDownClass(cls):
        subprocess.run(
            [str(PG_BIN / "pg_ctl"), "-D", str(cls.dir / "data"), "-m", "fast", "-w", "stop"],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        cls.temp.cleanup()

    async def asyncSetUp(self):
        self.schema = "test_" + uuid.uuid4().hex
        self.pool = await asyncpg.create_pool(
            host=str(self.dir), port=55471, database="postgres", min_size=1, max_size=4,
            init=db._init_connection, server_settings={"search_path": self.schema},
        )
        async with self.pool.acquire() as conn:
            await conn.execute(f'CREATE SCHEMA "{self.schema}"')
            for name in ("012_queue_items.sql", "014_queue_recovery_receipts.sql", "037_queue_bindings.sql"):
                await conn.execute((ROOT / "sql" / name).read_text())

    async def asyncTearDown(self):
        async with self.pool.acquire() as conn:
            await conn.execute(f'DROP SCHEMA "{self.schema}" CASCADE')
        await self.pool.close()

    async def test_burst_wake_and_claim_clears_pending(self):
        await db.bind_queue(self.pool, "momentum", "ingest", "operator")
        first = await db.enqueue_queue_item(self.pool, "momentum", {"n": 1})
        second = await db.enqueue_queue_item(self.pool, "momentum", {"n": 2})
        self.assertTrue(first["wake_requested"])
        self.assertTrue(first["wake_id"])
        self.assertFalse(second["wake_requested"])
        stats = await db.queue_stats(self.pool, "momentum")
        self.assertTrue(stats["wake_pending"])
        items = await db.claim_queue_items(self.pool, "momentum", 10, "lead#1", consumer="ingest")
        self.assertEqual(len(items), 2)
        self.assertFalse((await db.queue_stats(self.pool, "momentum"))["wake_pending"])

    async def test_wrong_consumer_and_claim_token_are_refused(self):
        await db.bind_queue(self.pool, "q", "ingest", "operator")
        item = await db.enqueue_queue_item(self.pool, "q", {"x": 1})
        with self.assertRaisesRegex(db.QueueBindingRefusal, "ingest"):
            await db.claim_queue_items(self.pool, "q", 1, "bad", consumer="other")
        claimed = await db.claim_queue_items(self.pool, "q", 1, "lead#1", consumer="ingest")
        with self.assertRaisesRegex(db.QueueBindingRefusal, "claim_token"):
            await db.ack_queue_items(self.pool, "q", [item["id"]], consumer="ingest", claim_token=str(uuid.uuid4()))
        await db.ack_queue_items(self.pool, "q", [claimed[0]["id"]], consumer="ingest", claim_token=claimed[0]["claim_token"])

    async def test_bind_over_backlog_reports_depth(self):
        for i in range(3):
            await db.enqueue_queue_item(self.pool, "backlog-q", {"i": i})
        bound = await db.bind_queue(self.pool, "backlog-q", "ingest", "operator")
        self.assertTrue(bound["wake_requested"])
        self.assertEqual(bound["depth"], 3)

    async def test_failed_wake_can_be_retried_and_lease_expiry_requeues(self):
        await db.bind_queue(self.pool, "q", "ingest", "operator")
        await db.enqueue_queue_item(self.pool, "q", {"x": 1})
        await db.record_queue_wake(self.pool, "q", delivered=False, error="no live main")
        stats = await db.queue_stats(self.pool, "q")
        self.assertEqual(stats["wake_delivery"], "failed")
        retried = await db.enqueue_queue_item(self.pool, "q", {"x": 2})
        self.assertTrue(retried["wake_requested"])
        claimed = await db.claim_queue_items(self.pool, "q", 1, "lead#1", consumer="ingest", lease_seconds=30)
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE queue_items SET lease_until=now()-interval '1 second' WHERE id=$1", uuid.UUID(claimed[0]["id"]))
        stats = await db.queue_stats(self.pool, "q")
        self.assertEqual(stats["pending"], 2)

    async def test_unbound_queue_keeps_legacy_shape_and_flow(self):
        result = await db.enqueue_queue_item(self.pool, "legacy", {"ok": True})
        self.assertNotIn("wake_requested", result)
        items = await db.claim_queue_items(self.pool, "legacy", 1, "task#1")
        self.assertEqual(items[0]["payload"], {"ok": True})
        self.assertEqual(await db.ack_queue_items(self.pool, "legacy", [items[0]["id"]]), 1)

    async def test_api_handlers_return_binding_read_shape_and_enqueue_alias_payload(self):
        from api import main as api_main

        await api_main.bind_queue("api-q", {"consumer": "ingest", "bound_by": "operator"}, self.pool)
        result = await api_main.enqueue_queue_item("api-q", {"payload": {"source": "direct-api"}}, self.pool)
        self.assertTrue(result["wake_requested"])
        stats = await api_main.get_queue_stats("api-q", self.pool)
        self.assertEqual(stats["binding"]["consumer"], "ingest")
        self.assertIn("oldest_item_age_seconds", stats)


if __name__ == "__main__":
    unittest.main()
