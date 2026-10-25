"""Staffing API contracts against a disposable PostgreSQL over a private socket."""
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
from api.staffing import router

PG = Path("/usr/lib/postgresql/17/bin")
SQL = Path(__file__).resolve().parents[1] / "sql/026_staffing_policy.sql"


class StaffingApiTests(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls):
        if not (PG / "initdb").exists():
            raise unittest.SkipTest("PostgreSQL 17 test binaries unavailable")
        cls.tmp = tempfile.TemporaryDirectory(prefix="cm-staffing-pg-")
        cls.root = Path(cls.tmp.name)
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
        self.schema = "staffing_" + uuid.uuid4().hex
        async def init(conn):
            await conn.set_type_codec("jsonb", encoder=json.dumps, decoder=json.loads,
                                      schema="pg_catalog", format="text")
        self.pool = await asyncpg.create_pool(host=str(self.root), user=getpass.getuser(),
                                            database="postgres", min_size=1, max_size=3, init=init,
                                            server_settings={"search_path": self.schema})
        await self.pool.execute(f'CREATE SCHEMA "{self.schema}"')
        await self.pool.execute(SQL.read_text())
        app = FastAPI()
        app.include_router(router)
        app.state.pool = self.pool
        self.token = patch("api.auth.API_TOKEN", "staffing-test-token")
        self.token.start()
        self.client = httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://test",
                                       headers={"Authorization": "Bearer staffing-test-token"})

    async def asyncTearDown(self):
        await self.client.aclose()
        self.token.stop()
        await self.pool.execute(f'DROP SCHEMA "{self.schema}" CASCADE')
        await self.pool.close()

    async def test_cas_concurrent_edits_and_migration_preserve_policy(self):
        original = (await self.client.get("/staffing")).json()
        self.assertEqual(original["version"], 1)
        self.assertEqual(original["policy"]["rules"]["fanout"], "claude-code")
        policy = original["policy"] | {"default_engine": "claude-code"}
        body = {"expected_version": 1, "policy": policy, "actor": "owner"}
        replies = await asyncio.gather(self.client.put("/staffing", json=body),
                                       self.client.put("/staffing", json=body))
        self.assertEqual(sorted(r.status_code for r in replies), [200, 409])
        await self.pool.execute(SQL.read_text())
        saved = (await self.client.get("/staffing")).json()
        self.assertEqual(saved["version"], 2)
        self.assertEqual(saved["policy"], policy)

    async def test_auth_actor_and_policy_validation(self):
        self.assertEqual((await self.client.get("/staffing", headers={"Authorization": "bad"})).status_code, 401)
        original = (await self.client.get("/staffing")).json()["policy"]
        for change in ({"actor": "agent"},
                       {"policy": original | {"default_engine": "bash"}},
                       {"policy": original | {"rules": {"mystery": "codex"}}},
                       {"policy": original | {"preference": {"engine": "codex", "strength": "strong", "reason": "limits", "expires_at": "2026-11-08T00:00:00"}}}):
            body = {"expected_version": 1, "actor": "owner", "policy": original} | change
            self.assertEqual((await self.client.put("/staffing", json=body)).status_code, 422, change)
        self.assertEqual((await self.client.get("/staffing")).json()["version"], 1)

    async def test_preference_round_trip_and_clear(self):
        snapshot = (await self.client.get("/staffing")).json()
        policy = snapshot["policy"]
        policy["preference"] = {"engine": "codex", "strength": "strong", "reason": "higher limits", "expires_at": "2026-11-08T00:00:00Z"}
        response = await self.client.put("/staffing", json={"expected_version": 1, "actor": "owner", "policy": policy})
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json()["policy"]["preference"]["reason"], "higher limits")
        policy["preference"] = None
        response = await self.client.put("/staffing", json={"expected_version": 2, "actor": "owner", "policy": policy})
        self.assertEqual(response.status_code, 200, response.text)
        self.assertIsNone(response.json()["policy"]["preference"])
