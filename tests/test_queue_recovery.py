"""Real PostgreSQL recovery transactions in a disposable local cluster."""
import asyncio
import os
from pathlib import Path
import shutil
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
class QueueRecoveryTests(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="cm-queue-recovery-")
        cls.dir = Path(cls.temp.name)
        subprocess.run([str(PG_BIN / "initdb"), "-D", str(cls.dir / "data"),
                        "--no-locale", "--encoding=UTF8", "--auth=trust"],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        subprocess.run([str(PG_BIN / "pg_ctl"), "-D", str(cls.dir / "data"),
                        "-l", str(cls.dir / "server.log"), "-o",
                        f"-F -k {cls.dir} -h '' -p 55467", "-w", "start"],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

    @classmethod
    def tearDownClass(cls):
        subprocess.run([str(PG_BIN / "pg_ctl"), "-D", str(cls.dir / "data"),
                        "-m", "fast", "-w", "stop"], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        cls.temp.cleanup()

    async def asyncSetUp(self):
        self.schema = "test_" + uuid.uuid4().hex
        self.pool = await asyncpg.create_pool(host=str(self.dir), port=55467,
                                             database="postgres", min_size=1, max_size=4,
                                             init=db._init_connection,
                                             server_settings={"search_path": self.schema})
        async with self.pool.acquire() as conn:
            await conn.execute(f'CREATE SCHEMA "{self.schema}"')
            await conn.execute((ROOT / "sql/012_queue_items.sql").read_text())
            await conn.execute((ROOT / "sql/014_queue_recovery_receipts.sql").read_text())

    async def asyncTearDown(self):
        async with self.pool.acquire() as conn:
            await conn.execute(f'DROP SCHEMA "{self.schema}" CASCADE')
        await self.pool.close()

    async def consumed_batch(self):
        for key in ("completed", "unfinished"):
            await db.enqueue_queue_item(self.pool, "canary", {"kind": key}, dedupe_key=key)
        items = await db.claim_queue_items(self.pool, "canary", 2, "task#7")
        await db.ack_queue_items(self.pool, "canary", [i["id"] for i in items])
        return {i["payload"]["kind"]: i for i in items}

    async def test_partial_batch_keeps_ids_keys_and_completed_outcomes(self):
        batch = await self.consumed_batch()
        original = batch["unfinished"]
        recovered = await db.recover_queue_item(self.pool, "canary", original["id"], "task#7", "retry-7")
        self.assertEqual(recovered["id"], original["id"])
        self.assertFalse(recovered["already_recovered"])
        next_batch = await db.claim_queue_items(self.pool, "canary", 10, "task#8")
        self.assertEqual(next_batch, [original])
        await db.ack_queue_items(self.pool, "canary", [original["id"]])
        # Simulate the original recovery response being lost. Its retry after
        # the next batch was consumed must not reopen either item.
        retry = await db.recover_queue_item(self.pool, "canary", original["id"], "task#7", "retry-7")
        self.assertTrue(retry["already_recovered"])
        self.assertEqual((await db.queue_stats(self.pool, "canary"))["pending"], 0)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM queue_items WHERE state='consumed'"), 2)

    async def test_concurrent_retries_make_one_receipt_and_one_pending_item(self):
        batch = await self.consumed_batch()
        item = batch["unfinished"]["id"]
        results = await asyncio.gather(*[
            db.recover_queue_item(self.pool, "canary", item, "task#7", "same-retry") for _ in range(8)])
        self.assertEqual(sum(not r["already_recovered"] for r in results), 1)
        self.assertEqual((await db.queue_stats(self.pool, "canary"))["pending"], 1)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM queue_recovery_receipts"), 1)

    async def test_stale_claim_wrong_queue_and_reused_key_cannot_change_work(self):
        batch = await self.consumed_batch()
        item = batch["unfinished"]["id"]
        for queue, claim in [("wrong", "task#7"), ("canary", "task#6")]:
            with self.assertRaises(db.QueueRecoveryConflict):
                await db.recover_queue_item(self.pool, queue, item, claim, "bad")
        await db.recover_queue_item(self.pool, "canary", item, "task#7", "good")
        with self.assertRaises(db.QueueRecoveryConflict):
            await db.recover_queue_item(self.pool, "canary", batch["completed"]["id"], "task#7", "good")
        self.assertEqual((await db.queue_stats(self.pool, "canary"))["pending"], 1)

    async def test_dedupe_collision_rolls_back_without_a_false_receipt(self):
        batch = await self.consumed_batch()
        await db.enqueue_queue_item(self.pool, "canary", {"new": True}, dedupe_key="unfinished")
        with self.assertRaises(db.QueueRecoveryConflict):
            await db.recover_queue_item(self.pool, "canary", batch["unfinished"]["id"], "task#7", "collision")
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM queue_recovery_receipts"), 0)
            self.assertEqual(await conn.fetchval("SELECT state FROM queue_items WHERE id=$1",uuid.UUID(batch["unfinished"]["id"])), "consumed")


if __name__ == "__main__":
    unittest.main()
