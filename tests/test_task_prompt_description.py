"""Org model S1: a task has one brief, its description (doc/org-model.md §13.2).

The pure folding rules run without a database; the API table and the
marker-gated backfill (sql/027) run against a throwaway PostgreSQL 17, like
test_task_changes.py.
"""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import uuid

ROOT = Path(__file__).resolve().parents[1]
PG_BIN = Path("/usr/lib/postgresql/17/bin")
PG_PORT = 55471

os.environ.setdefault("CM_DB_DSN", f"postgresql://postgres@localhost:{PG_PORT}/postgres")
os.environ.setdefault("CM_API_TOKEN", "test-token")

from dispatch import task_text  # noqa: E402


class FoldingTests(unittest.TestCase):
    def test_a_redundant_prompt_leaves_the_description(self):
        for prompt in ("", "  ", None, "Title", " Title ", "do it"):
            with self.subTest(prompt=prompt):
                self.assertEqual(task_text.fold_prompt("Why. do it", prompt, "Title"), "Why. do it")

    def test_a_real_prompt_is_appended_under_instructions(self):
        self.assertEqual(task_text.fold_prompt("Why.\n", " Step 1. ", "T"),
                         "Why.\n\n## Instructions\n\nStep 1.")
        self.assertEqual(task_text.fold_prompt("", "Step 1.", "T"), "## Instructions\n\nStep 1.")
        self.assertEqual(task_text.fold_prompt(None, "Step 1.", "T"), "## Instructions\n\nStep 1.")

    def test_the_brief_never_repeats_a_contained_prompt(self):
        folded = task_text.fold_prompt("Why.", "Step 1.", "T")
        self.assertEqual(task_text.compose_brief(folded, "Step 1.", "T"), folded)
        self.assertEqual(task_text.compose_brief("Why.", "Step 1.", "T"), "Why.\n\n---\n\nStep 1.")
        self.assertEqual(task_text.compose_brief("", "Step 1.", "T"), "Step 1.")
        self.assertEqual(task_text.compose_brief(None, None, "T"), "")


@unittest.skipUnless((PG_BIN / "initdb").exists(), "PostgreSQL 17 test binaries required")
class PromptDescriptionApiTests(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="cm-prompt-desc-")
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
        import asyncpg
        import httpx
        from api import auth as api_auth
        from api import main as api_main
        from dispatch import db

        self.db = db
        self.schema = "test_" + uuid.uuid4().hex
        admin = await asyncpg.connect(host="127.0.0.1", port=PG_PORT, user="postgres",
                                      database="postgres")
        await admin.execute(f'CREATE SCHEMA "{self.schema}"')
        await admin.close()
        self.pool = await asyncpg.create_pool(host="127.0.0.1", port=PG_PORT, user="postgres",
                                             database="postgres", min_size=1, max_size=4,
                                             init=db._init_connection,
                                             server_settings={"search_path": self.schema})
        await db.init_db(self.pool)
        api_main.app.state.pool = self.pool
        api_main.app.state.change_broker = None
        self.client = httpx.AsyncClient(
            transport=httpx.ASGITransport(app=api_main.app), base_url="http://cm",
            headers={"Authorization": f"Bearer {api_auth.API_TOKEN}"})

    async def asyncTearDown(self):
        await self.client.aclose()
        async with self.pool.acquire() as conn:
            await conn.execute(f'DROP SCHEMA "{self.schema}" CASCADE')
        await self.pool.close()

    async def create(self, **body) -> dict:
        r = await self.client.post("/tasks", json={"repo_url": "git@example:repo.git",
                                                   "project": "proj", **body})
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def patch(self, task_id: str, **body):
        return await self.client.patch(f"/tasks/{task_id}", json=body)

    async def stored(self, task_id: str) -> dict:
        return dict(await self.pool.fetchrow(
            "SELECT name, description, prompt FROM tasks WHERE id = $1", uuid.UUID(task_id)))

    # -- CREATE --------------------------------------------------------------

    async def test_create_with_description_only_is_stored_as_given(self):
        task = await self.create(name="T", description="Why and how.")
        self.assertEqual(task["description"], "Why and how.")
        self.assertEqual(task["prompt"], "")

    async def test_create_with_a_real_prompt_folds_it_into_the_description(self):
        task = await self.create(name="T", description="Why.", prompt="Never push to main.")
        self.assertEqual(task["description"], "Why.\n\n## Instructions\n\nNever push to main.")
        self.assertEqual(task["prompt"], "", "responses never echo the prompt")
        self.assertEqual((await self.stored(task["id"]))["prompt"], "Never push to main.",
                         "the column still records what was sent")

    async def test_create_ignores_a_prompt_that_is_the_name_or_already_there(self):
        named = await self.create(name="Fix it", description="", prompt="Fix it")
        self.assertEqual(named["description"], "")
        inside = await self.create(name="T", description="Why. Step 1.", prompt="Step 1.")
        self.assertEqual(inside["description"], "Why. Step 1.")

    async def test_create_leaves_continuous_and_backtest_prompts_alone(self):
        continuous = await self.create(name="orch", kind="continuous", prompt="contract")
        self.assertIsNone(continuous["description"])
        backtest = await self.create(name="bt", kind="backtest", prompt="Cloud backtest x",
                                     metadata={"backtest": {"branch": "b", "config": "c"}})
        self.assertIsNone(backtest["description"])
        self.assertEqual(continuous["prompt"], "", "and the stale copy is never echoed")

    # -- PATCH ---------------------------------------------------------------

    async def test_patch_with_description_only(self):
        task = await self.create(name="T", description="old")
        r = await self.patch(task["id"], description="new")
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(r.json()["description"], "new")

    async def test_patch_ignores_a_redundant_prompt(self):
        task = await self.create(name="T", description="Why.", prompt="Step 1.")
        stored = task["description"]
        for prompt in ("", None, "T", "Step 1."):
            with self.subTest(prompt=prompt):
                r = await self.patch(task["id"], prompt=prompt)
                self.assertEqual(r.status_code, 200, r.text)
                self.assertEqual(r.json()["description"], stored)
                self.assertEqual(r.json()["prompt"], "")

    async def test_patch_checks_the_description_that_will_be_stored(self):
        task = await self.create(name="T", description="Why. Step 1.")
        # The old editor's round trip: both fields, the prompt inside the new description.
        r = await self.patch(task["id"], description="Why. Step 2.", prompt="Step 2.")
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(r.json()["description"], "Why. Step 2.")
        # An instruction deleted from the description must not survive in the
        # prompt: the stored description no longer has it, so it is refused.
        r = await self.patch(task["id"], description="Why.", prompt="Step 2.")
        self.assertEqual(r.status_code, 409, r.text)
        self.assertEqual(r.json()["detail"]["code"], "prompt_deprecated")
        self.assertEqual((await self.stored(task["id"]))["description"], "Why. Step 2.",
                         "a refused PATCH writes nothing")

    async def test_patch_with_a_different_prompt_is_refused(self):
        task = await self.create(name="T", description="Why.")
        r = await self.patch(task["id"], prompt="Do something else.")
        self.assertEqual(r.status_code, 409, r.text)
        self.assertIn("description", r.json()["detail"]["message"])

    async def test_patch_compares_against_a_renamed_title(self):
        task = await self.create(name="Old", description="Why.")
        r = await self.patch(task["id"], name="New", prompt="New")
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(r.json()["name"], "New")

    # -- responses -----------------------------------------------------------

    async def test_every_task_response_has_an_empty_prompt(self):
        task = await self.create(name="T", description="", prompt="Step 1.")
        get = (await self.client.get(f"/tasks/{task['id']}")).json()
        listed = (await self.client.get("/tasks")).json()
        feed = (await self.client.get("/tasks/changes")).json()
        self.assertEqual(get["prompt"], "")
        self.assertEqual({t["prompt"] for t in listed}, {""})
        self.assertEqual({t["prompt"] for t in feed["tasks"]}, {""})
        self.assertEqual(get["description"], "## Instructions\n\nStep 1.")

    async def test_workers_preview_survives_a_null_prompt(self):
        task = await self.create(name="T", description="Why.")
        await self.pool.execute(
            "UPDATE tasks SET prompt = NULL, status = 'running', worker_vm = 'vm-1' WHERE id = $1",
            uuid.UUID(task["id"]))
        r = await self.client.get("/workers")
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(r.json()[0]["prompt"], "Why.")

    # -- backfill ------------------------------------------------------------

    async def raw(self, name, description, prompt, kind="oneshot") -> uuid.UUID:
        return await self.pool.fetchval(
            "INSERT INTO tasks (repo_url, repo_branch, name, description, prompt, kind) "
            "VALUES ('r', 'main', $1, $2, $3, $4) RETURNING id", name, description, prompt, kind)

    async def rerun_backfill(self):
        sql = (ROOT / "sql" / "027_prompt_into_description.sql").read_text()
        async with self.pool.acquire() as conn:
            await conn.execute(sql)

    async def test_backfill_runs_once_and_skips_continuous_and_backtest(self):
        await self.pool.execute(
            "DELETE FROM migrations_applied WHERE name = '027_prompt_into_description'")
        real = await self.raw("T", "Why.  \n", "  Never push.\n")
        empty_desc = await self.raw("T", None, "Step 1.")
        same = await self.raw("Fix it", "", "Fix it")
        inside = await self.raw("T", "Why. Step 1.", "Step 1.")
        nothing = await self.raw("T", "", "")
        null_prompt = await self.raw("T", "Why.", None)
        continuous = await self.raw("orch", "", "stale contract", kind="continuous")
        backtest = await self.raw("bt", "", "synthesized", kind="backtest")
        await self.rerun_backfill()

        async def desc(task_id):
            return await self.pool.fetchval("SELECT description FROM tasks WHERE id = $1", task_id)

        self.assertEqual(await desc(real), "Why.\n\n## Instructions\n\nNever push.")
        self.assertEqual(await desc(empty_desc), "## Instructions\n\nStep 1.")
        self.assertEqual(await desc(same), "")
        self.assertEqual(await desc(inside), "Why. Step 1.")
        self.assertEqual(await desc(nothing), "")
        self.assertEqual(await desc(null_prompt), "Why.")
        # Continuous contracts live in the continuous record's
        # standing_instructions; the task copy must never become deliverable.
        self.assertEqual(await desc(continuous), "")
        self.assertEqual(await desc(backtest), "")
        self.assertEqual(await self.pool.fetchval("SELECT prompt FROM tasks WHERE id = $1", real),
                         "  Never push.\n", "the prompt column is kept")

        # The marker makes later starts a no-op, even for a new matching row.
        late = await self.raw("T", "", "Late prompt.")
        await self.rerun_backfill()
        self.assertEqual(await desc(late), "")
        self.assertEqual(await desc(real), "Why.\n\n## Instructions\n\nNever push.")

    async def test_old_client_create_after_the_backfill_delivers_its_prohibition(self):
        # An old client sends both fields; the worker-only prohibition must
        # reach the brief every composer delivers.
        task = await self.create(name="T", description="Background.",
                                 prompt="Do NOT touch production.")
        row = await self.stored(task["id"])
        brief = task_text.compose_brief(row["description"], row["prompt"], row["name"])
        self.assertIn("Do NOT touch production.", brief)
        self.assertEqual(brief.count("Do NOT touch production."), 1)
        # Through the API the prompt is "", so an old composer joining both
        # fields delivers it once too.
        self.assertIn("Do NOT touch production.", task["description"])


if __name__ == "__main__":
    unittest.main()
