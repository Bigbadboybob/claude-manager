"""Continuous registration contracts against a disposable PostgreSQL over a private socket."""
import asyncio
import getpass
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import uuid

import asyncpg
from fastapi import FastAPI
import httpx

os.environ.setdefault("CM_DB_DSN", "postgresql://unused/test")
os.environ.setdefault("CM_API_TOKEN", "staffing-test-token")
from api.continuous_registration import router

PG = Path("/usr/lib/postgresql/17/bin")
SQL = Path(__file__).resolve().parents[1] / "sql/034_continuous_registration.sql"


class ContinuousRegistrationTests(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls):
        if not (PG / "initdb").exists():
            raise unittest.SkipTest("PostgreSQL 17 test binaries unavailable")
        cls.tmp = tempfile.TemporaryDirectory(prefix="cm-adopt40-pg-")
        cls.root = Path(cls.tmp.name)
        (cls.root / "OWNER.md").write_text("owner: state-core #40 test\npurpose: disposable isolated postgres\nkeep: none\ndelete-after: test\n")
        cls.data = cls.root / "data"
        try:
            subprocess.run([str(PG / "initdb"), "-D", str(cls.data), "-A", "trust", "--no-locale"],
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            subprocess.run([str(PG / "pg_ctl"), "-D", str(cls.data), "-l", str(cls.root / "pg.log"),
                            "-o", f"-F -k {cls.root} -c listen_addresses=''", "-w", "start"],
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        except BaseException:
            cls.tmp.cleanup()
            raise

    @classmethod
    def tearDownClass(cls):
        subprocess.run([str(PG / "pg_ctl"), "-D", str(cls.data), "-m", "immediate", "-w", "stop"],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        cls.tmp.cleanup()

    async def asyncSetUp(self):
        self.schema = "adopt40_" + uuid.uuid4().hex
        async def init(conn):
            await conn.set_type_codec("jsonb", encoder=json.dumps, decoder=json.loads,
                                      schema="pg_catalog", format="text")
        self.pool = await asyncpg.create_pool(host=str(self.root), user=getpass.getuser(),
                                            database="postgres", min_size=1, max_size=3, init=init,
                                            server_settings={"search_path": self.schema})
        await self.pool.execute(f'CREATE SCHEMA "{self.schema}"')
        await self.pool.execute("CREATE TABLE tasks (id uuid PRIMARY KEY, kind text, status text, metadata jsonb, updated_at timestamptz DEFAULT now())")
        await self.pool.execute(SQL.read_text())
        app = FastAPI()
        app.include_router(router)
        app.state.pool = self.pool
        self.inventory = self.root / (self.schema + "-inventory")
        app.state.continuous_registration_dir = self.inventory
        app.state.continuous_registration_host = "cm-manager"
        self.token = patch("api.auth.API_TOKEN", "staffing-test-token")
        self.token.start()
        self.client = httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://test",
                                       headers={"Authorization": "Bearer staffing-test-token"})

    async def asyncTearDown(self):
        await self.client.aclose()
        self.token.stop()
        await self.pool.execute(f'DROP SCHEMA "{self.schema}" CASCADE')
        await self.pool.close()

    async def task(self, kind="oneshot", continuous=None):
        tid = uuid.uuid4()
        metadata = {"history": ["prior turn"], "models": {"codex": "gpt-6.1-terra"}, "lead": "original"}
        if continuous is not None:
            metadata["continuous"] = continuous
        await self.pool.execute("INSERT INTO tasks(id,kind,status,metadata) VALUES($1,$2,'running',$3)", tid, kind, metadata)
        path = f"/tasks/{tid}/continuous-registration"
        snap = (await self.client.get(path)).json()
        payload = {"actor": "owner", "request_id": "adopt-1", "expected_kind": snap["kind"],
                   "expected_continuous": snap["continuous"], "expected_binding": snap["binding"],
                   "binding": {"daemon_id": str(uuid.uuid4()), "host": "cm-sessions", "continuous_task_id": "planning",
                               "session_uid": "terra", "workspace_id": "ws", "worktree_path": "/existing", "engine": "codex"}}
        return tid, path, payload, metadata

    async def test_oneshot_registration_preserves_history_and_idempotent_receipt(self):
        tid, path, body, metadata = await self.task()
        response = await self.client.put(path, json=body)
        self.assertEqual(response.status_code, 200, response.text)
        saved = await self.pool.fetchrow("SELECT * FROM tasks WHERE id=$1", tid)
        self.assertEqual(saved["kind"], "continuous")
        self.assertEqual(saved["status"], "running")
        for k, v in metadata.items():
            self.assertEqual(saved["metadata"][k], v)
        before = saved["updated_at"]
        replay = await self.client.put(path, json=body)
        self.assertTrue(replay.json()["already_registered"])
        self.assertEqual(await self.pool.fetchval("SELECT updated_at FROM tasks WHERE id=$1", tid), before)
        self.assertEqual((await self.client.put(path, json=body | {"binding": body["binding"] | {"session_uid": "other"}})).status_code, 409)
        await self.pool.execute(SQL.read_text())
        self.assertEqual(await self.pool.fetchval("SELECT count(*) FROM continuous_registration_requests"), 1)

    async def test_concurrent_hosts_only_one_claims_and_old_replay_never_rebinds(self):
        tid, path, body, _ = await self.task()
        competitor = body | {"request_id": "competitor", "binding": body["binding"] | {"daemon_id": str(uuid.uuid4()), "host": "cm-manager"}}
        replies = await asyncio.gather(self.client.put(path, json=body), self.client.put(path, json=competitor))
        self.assertEqual(sorted(r.status_code for r in replies), [200, 409])
        winner = body if replies[0].status_code == 200 else competitor
        snapshot = (await self.client.get(path)).json()
        replacement = winner | {"request_id": "rebind", "expected_kind": "continuous", "expected_continuous": snapshot["continuous"], "expected_binding": snapshot["binding"], "binding": winner["binding"] | {"session_uid": "replacement"}}
        self.assertEqual((await self.client.put(path, json=replacement)).status_code, 200)
        replay = (await self.client.put(path, json=winner)).json()
        self.assertEqual(replay["receipt"]["session_uid"], winner["binding"]["session_uid"])
        self.assertEqual(replay["current"]["binding"]["session_uid"], "replacement")
        self.assertEqual((await self.client.get(path)).json()["binding"]["session_uid"], "replacement")

    async def test_stale_cas_and_legacy_host_owner_refused_without_mutation(self):
        tid, path, body, _ = await self.task("continuous", {"task_id": "planning", "host": "cm-manager", "history": 7})
        self.assertEqual((await self.client.put(path, json=body)).status_code, 409)
        await self.pool.execute("UPDATE tasks SET metadata=metadata || '{\"continuous\":{\"task_id\":\"planning\",\"host\":\"cm-sessions\"}}'::jsonb WHERE id=$1", tid)
        self.assertEqual((await self.client.put(path, json=body)).status_code, 409)
        self.assertEqual(await self.pool.fetchval("SELECT count(*) FROM continuous_registrations"), 0)

    async def test_terminal_auth_bad_actor_and_native_engine_rejected(self):
        tid, path, body, _ = await self.task()
        self.assertEqual((await self.client.get(path, headers={"Authorization": "bad"})).status_code, 401)
        self.assertEqual((await self.client.put(path, json=body | {"actor": "agent"})).status_code, 422)
        self.assertEqual((await self.client.put(path, json=body | {"binding": body["binding"] | {"engine": "bash"}})).status_code, 422)
        await self.pool.execute("UPDATE tasks SET status='done' WHERE id=$1", tid)
        self.assertEqual((await self.client.put(path, json=body)).status_code, 409)
        self.assertEqual(await self.pool.fetchval("SELECT count(*) FROM continuous_registrations"), 0)

    def legacy_record(self, tid):
        folder = self.inventory / "planning"
        folder.mkdir(parents=True, exist_ok=True)
        path = folder / "state.json"
        path.write_text(json.dumps({"task_id": "planning", "planning_task_id": str(tid),
                                    "workspace_id": "ws", "worktree_path": "/existing", "engine": "codex"}))
        return path

    async def test_legacy_manager_owner_blocks_oneshot_with_null_metadata(self):
        tid, path, body, _ = await self.task()
        await self.pool.execute("UPDATE tasks SET metadata=NULL WHERE id=$1", tid)
        self.legacy_record(tid)
        snap = (await self.client.get(path)).json()
        self.assertEqual(snap["kind"], "oneshot")
        self.assertIsNone(snap["continuous"])
        self.assertEqual(snap["api_host_owners"][0]["host"], "cm-manager")
        response = await self.client.put(path, json=body)
        self.assertEqual(response.status_code, 409, response.text)
        self.assertIn("legacy_owner_conflict", response.text)
        self.assertEqual(await self.pool.fetchval("SELECT count(*) FROM continuous_registrations"), 0)
        # A matching host/local record can register without rewriting history.
        body["binding"]["host"] = "cm-manager"
        self.assertEqual((await self.client.put(path, json=body)).status_code, 200)

    async def test_corrupt_inventory_fails_closed_but_missing_state_is_absent(self):
        tid, path, body, _ = await self.task()
        file = self.legacy_record(tid)
        for contents in ("{broken", "[]", '{"task_id":"planning","planning_task_id":12}'):
            file.write_text(contents)
            response = await self.client.get(path)
            self.assertEqual(response.status_code, 409, response.text)
            self.assertIn("legacy_inventory_unavailable", response.text)
            self.assertEqual((await self.client.put(path, json=body)).status_code, 409)
        self.legacy_record(tid)
        file.chmod(0)
        try:
            self.assertEqual((await self.client.get(path)).status_code, 409)
            self.assertEqual((await self.client.put(path, json=body)).status_code, 409)
        finally:
            file.chmod(0o600)
        file.unlink()  # a lock/run-log-only directory establishes no owner
        response = await self.client.get(path)
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json()["api_host_owners"], [])
        self.assertEqual(await self.pool.fetchval("SELECT count(*) FROM continuous_registrations"), 0)

    async def test_lock_only_directory_is_absent_but_suspicious_state_entries_fail_closed(self):
        tid, path, body, _ = await self.task()
        lock_only = self.inventory / "cm-f1-cap-demo"
        lock_only.mkdir(parents=True)
        (lock_only / "state.json.lock").write_bytes(b"")
        snapshot = await self.client.get(path)
        self.assertEqual(snapshot.status_code, 200, snapshot.text)
        self.assertEqual(snapshot.json()["api_host_owners"], [])
        registered = await self.client.put(path, json=body)
        self.assertEqual(registered.status_code, 200, registered.text)

        # A dangling state symlink must not be mistaken for an absent file.
        await self.pool.execute("DELETE FROM continuous_registrations")
        await self.pool.execute("DELETE FROM continuous_registration_requests")
        await self.pool.execute("UPDATE tasks SET kind='oneshot', metadata=NULL WHERE id=$1", tid)
        dangling = self.inventory / "dangling"
        dangling.mkdir()
        (dangling / "state.json").symlink_to("gone.json")
        response = await self.client.get(path)
        self.assertEqual(response.status_code, 409, response.text)
        self.assertIn("legacy_inventory_unavailable", response.text)

        # A present symlink to a valid-looking record is still suspicious.
        target = self.inventory / "target"
        target.mkdir()
        (target / "state.json").write_text(json.dumps({
            "task_id": "target", "planning_task_id": str(tid), "workspace_id": "ws",
            "worktree_path": "/existing", "engine": "codex",
        }))
        linked = self.inventory / "linked"
        linked.symlink_to(target, target_is_directory=True)
        response = await self.client.get(path)
        self.assertEqual(response.status_code, 409, response.text)
        self.assertIn("legacy_inventory_unavailable", response.text)
        self.assertEqual(await self.pool.fetchval("SELECT count(*) FROM continuous_registrations"), 0)

    async def test_receipt_replay_exposes_current_closure_without_reopening_task(self):
        tid, path, body, _ = await self.task()
        self.assertEqual((await self.client.put(path, json=body)).status_code, 200)
        await self.pool.execute("UPDATE tasks SET status='done', metadata=jsonb_set(metadata,'{continuous,engine}','\"claude\"') WHERE id=$1", tid)
        reply = (await self.client.put(path, json=body)).json()
        self.assertTrue(reply["already_registered"])
        self.assertEqual(reply["current"]["status"], "done")
        self.assertEqual(reply["current"]["continuous"]["engine"], "claude")
        self.assertEqual(reply["receipt"]["engine"], "codex")
