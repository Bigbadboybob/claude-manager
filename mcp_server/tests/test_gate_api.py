"""Gate receipts API against a real Postgres (doc/gate-receipts.md §9).

Runs when CM_ITEMS_TEST_DSN names a scratch database (see test_items_api.py).
"""

from __future__ import annotations

import copy
import os
import unittest
import uuid
from pathlib import Path

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

from dispatch import db  # noqa: E402

ROOT = Path(__file__).parents[2]
DSN = os.environ.get("CM_ITEMS_TEST_DSN")
TOKEN = {"Authorization": f"Bearer {os.environ['CM_API_TOKEN']}"}

HEAD, HEAD2, MF, MF2 = "a" * 40, "b" * 40, "c" * 40, "d" * 40
BASE, BASE2, TREE, RUNNER = "e" * 40, "f" * 40, "1" * 40, "2" * 40


def agent(name, daemon="d1"):
    return {"pid": f"agent:{daemon}:{name}-uid", "name": name,
            "session_uid": f"{name}-uid", "daemon_id": daemon}


OWNER = {"pid": "owner", "name": "Owner"}
LANE = agent("lane")
EP = agent("ep")
STEWARD = agent("steward")

PROFILE = {
    "steps": [
        {"name": "pytest", "kind": "pytest", "argv": ["pytest", "tests"],
         "nodes": ["tests/test_a.py::test_one", "tests/test_a.py::test_two"]},
        {"name": "mypy", "kind": "mypy"},
        {"name": "fetch", "kind": "rc"},
    ],
    "waivable": ["mypy"],
    "require": {"runner": True},
    "base": {"ref": "origin/main", "scope": "paths"},
    "inputs": [{"name": "harvest", "kind": "dir"}],
}


def wire(head=HEAD, mf=None, **over) -> dict:
    nodes = [{"id": "tests/test_a.py::test_one", "outcome": "passed"},
             {"id": "tests/test_a.py::test_two", "outcome": "passed"}]
    r = {
        "head": head, "mf": mf, "attestation": "cm-gate", "branch": "lane",
        "runner": {"ref": "origin/main:scripts/gate.sh", "sha": RUNNER},
        "base": {"ref": "origin/main", "sha_before": BASE, "sha_after": BASE,
                 "merge_tree_before": TREE, "merge_tree_after": TREE},
        "closure": {"paths": ["src/fuse.py", "tests/test_a.py"]},
        "worktree": {"head_before": head, "head_after": head, "dirty_before": False,
                     "dirty_after": False, "path": "/w/lane"},
        # As the reporting daemon stamps it after rehashing the local path.
        "inputs": [{"name": "harvest", "kind": "dir", "resolved_path": "/data/harvest",
                    "daemon_hash": {"content_id": "tree:" + "9" * 64}}],
        "steps": [
            {"name": "pytest", "state": "ran", "rc": 0, "at_sha": head,
             "effective_cmd": {"argv": ["pytest", "tests"]},
             "summary": "== 2 passed in 1.00s ==", "nodes": nodes,
             "granted_nodes": [n["id"] for n in nodes], "skips": [], "deselected": []},
            {"name": "mypy", "state": "ran", "rc": 0,
             "summary": "Success: no issues found in 3 source files"},
            {"name": "fetch", "state": "ran", "rc": 0},
        ],
    }
    r.update(over)
    return r


def with_step(r: dict, name: str, **fields) -> dict:
    r = copy.deepcopy(r)
    next(s for s in r["steps"] if s["name"] == name).update(fields)
    return r


class MigrationText(unittest.TestCase):
    def test_migration_023_is_rerunnable_ddl_only(self):
        files = list((ROOT / "sql").glob("023_*.sql"))
        self.assertEqual([f.name for f in files], ["023_item_receipts.sql"])
        text = files[0].read_text()
        body = "\n".join(l for l in text.splitlines() if not l.lstrip().startswith("--"))
        for table in ("item_receipts", "item_receipt_inputs", "item_grants"):
            self.assertIn(f"CREATE TABLE IF NOT EXISTS {table} ", body)
        self.assertNotIn("UPDATE ", body)
        self.assertNotIn("INSERT ", body)
        self.assertNotRegex(body, r"CREATE (UNIQUE )?INDEX (?!IF NOT EXISTS)")
        self.assertNotRegex(body, r"ADD COLUMN (?!IF NOT EXISTS)")


@unittest.skipUnless(DSN, "set CM_ITEMS_TEST_DSN to a scratch Postgres database")
class GateApiDb(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        import asyncpg
        import httpx
        from fastapi import FastAPI

        from api.items import router

        self.pool = await asyncpg.create_pool(DSN, min_size=1, max_size=4,
                                              init=db._init_connection)
        async with self.pool.acquire() as conn:
            await conn.execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        await db.init_db(self.pool)
        await db.init_db(self.pool)
        app = FastAPI()
        app.state.pool = self.pool
        app.include_router(router)
        self.http = httpx.AsyncClient(transport=httpx.ASGITransport(app=app),
                                      base_url="http://t", headers=TOKEN)
        async with self.pool.acquire() as conn:
            root = str(await conn.fetchval(
                "INSERT INTO tasks (repo_url, prompt, name) VALUES ('r', 'p', 'root') RETURNING id"))
        self.ref = (await self.http.post("/boards/resolve", json={"task_id": root})).json()["slug"]
        r = await self.http.post(f"/boards/{self.ref}/items", json={
            "actor": LANE, "items": [{"title": "fuse SEJD"}, {"title": "consumer"}]})
        self.assertEqual(r.status_code, 200, r.text)
        await self.patch_board(OWNER, gate_editors=[EP["pid"]], waiver_grantors=[EP["pid"]],
                               steward_pid=STEWARD["pid"])
        await self.put_profile("py", PROFILE, actor=EP, default=True)

    async def asyncTearDown(self):
        await self.http.aclose()
        await self.pool.close()

    async def patch_board(self, actor, ok=True, **settings):
        r = await self.http.patch(f"/boards/{self.ref}", json={"actor": actor, **settings})
        if ok:
            self.assertEqual(r.status_code, 200, r.text)
        return r

    async def put_profile(self, klass, profile, actor=EP, ok=True, default=False, reason="setup"):
        r = await self.http.put(f"/boards/{self.ref}/gate-profiles/{klass}", json={
            "actor": actor, "profile": profile, "reason": reason, "default": default})
        if ok:
            self.assertEqual(r.status_code, 200, r.text)
        return r

    async def post(self, receipt, n=1, actor=LANE, ok=True, request_id=None):
        body = {"actor": actor, "receipt": receipt}
        if request_id:
            body["request_id"] = request_id
        r = await self.http.post(f"/boards/{self.ref}/items/{n}/receipts", json=body)
        if ok:
            self.assertEqual(r.status_code, 200, r.text)
            return r.json()["receipt"]
        return r

    async def receipts(self, n=1, history=False):
        r = await self.http.get(f"/boards/{self.ref}/items/{n}/receipts",
                                params={"history": str(history).lower()})
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def grant(self, actor=EP, ok=True, n=1, **body):
        payload = {"actor": actor, "head": HEAD, "step": "mypy", "reason": "stubs broken",
                   "risk": "type errors may slip", "message_id": "m-1",
                   "message_check": {"found": True, "sender": actor["pid"], "contains_head": True,
                                     "contains_step": True, "contains_kind": True}}
        payload.update(body)
        r = await self.http.post(f"/boards/{self.ref}/items/{n}/grants", json=payload)
        if ok:
            self.assertEqual(r.status_code, 200, r.text)
            return r.json()["grant"]
        return r

    async def item_set(self, n=1, actor=LANE, **fields):
        r = await self.http.patch(f"/boards/{self.ref}/items",
                                  json={"actor": actor, "ns": [n], "set": fields})
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def board(self):
        r = await self.http.get(f"/boards/{self.ref}")
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def flags(self, n=1):
        b = await self.board()
        return {f["kind"]: f for f in b["flags"] if f["n"] == n}

    async def pushes(self, kind):
        async with self.pool.acquire() as conn:
            return await conn.fetch("SELECT * FROM item_pushes WHERE kind = $1", kind)

    # ---- profiles ---------------------------------------------------------
    async def test_profile_editors_exclude_the_steward_and_log_every_edit(self):
        r = await self.put_profile("py", PROFILE, actor=STEWARD, ok=False)
        self.assertEqual(r.status_code, 403)
        r = await self.put_profile("py", PROFILE, actor=LANE, ok=False)
        self.assertEqual(r.status_code, 403)
        r = await self.patch_board(EP, ok=False, gate_editors=[LANE["pid"]])
        self.assertEqual(r.status_code, 403, "only Owner names gate editors")
        r = await self.http.put(f"/boards/{self.ref}/gate-profiles/py",
                                json={"actor": EP, "profile": PROFILE, "reason": " "})
        self.assertEqual(r.status_code, 422, "an edit needs a reason")
        await self.put_profile("py", PROFILE, actor=OWNER, reason="tighten")
        async with self.pool.acquire() as conn:
            ev = await conn.fetch("SELECT * FROM item_events WHERE type = 'gate_profile_changed' "
                                  "ORDER BY id")
        self.assertEqual([e["actor_pid"] for e in ev], [EP["pid"], "owner"])
        self.assertEqual(ev[-1]["reason"], "tighten")
        self.assertEqual(ev[-1]["prev"]["profile"]["version"], 1)
        self.assertEqual(ev[-1]["new"]["profile"]["version"], 2)

    async def test_any_profile_edit_restales_every_receipt_of_the_class(self):
        rec = await self.post(wire())
        self.assertEqual(rec["verdict"], "PASS", rec["headline"])
        out = (await self.put_profile("py", PROFILE, reason="same content, still an edit")).json()
        self.assertEqual(out["restaled"], [rec["id"]])
        latest = (await self.receipts())["latest"]["gate"]
        self.assertEqual((latest["verdict"], latest["was"]), ("STALE", "PASS"))
        self.assertEqual(latest["stale"]["reason"], "profile_changed")
        self.assertIn("receipt_stale", await self.flags())

    # ---- posting and reading ------------------------------------------------
    async def test_a_receipt_renders_on_the_board_and_moves_the_version(self):
        before = (await self.board())["board"]["version"]
        rec = await self.post(wire())
        self.assertEqual(rec["id"], f"r-{self.ref}-1-1")
        self.assertEqual([i["verification"] for i in rec["inputs"]], ["hashed_at_run"])
        self.assertEqual(rec["attestation"], "self")
        b = await self.board()
        self.assertGreater(b["board"]["version"], before)
        item = next(i for i in b["items"] if i["n"] == 1)
        self.assertEqual(item["gate"]["gate"]["verdict"], "PASS")
        self.assertEqual(item["gate"]["gate"]["id"], rec["id"])

    async def test_request_id_replays_the_first_receipt(self):
        a = await self.post(wire(), request_id="rq-1")
        b = await self.post(wire(head=HEAD2), request_id="rq-1")
        self.assertEqual(a["id"], b["id"])
        self.assertEqual(len((await self.receipts(history=True))["history"]), 1)

    async def test_the_poster_cannot_pick_a_laxer_class(self):
        await self.put_profile("lax", {"steps": [{"name": "fetch", "kind": "rc"}]})
        r = await self.post(dict(wire(), **{"class": "lax"}), ok=False)
        self.assertEqual(r.status_code, 422)
        self.assertEqual(r.json()["detail"]["code"], "class_mismatch")

    async def test_rerun_on_a_new_head_stales_the_old_receipt(self):
        old = await self.post(wire())
        new = await self.post(wire(head=HEAD2))
        hist = {h["id"]: h for h in (await self.receipts(history=True))["history"]}
        self.assertEqual(hist[old["id"]]["verdict"], "STALE")
        self.assertEqual(hist[old["id"]]["stale"]["reason"], "head_moved")
        self.assertEqual(hist[new["id"]]["verdict"], "PASS")
        self.assertNotIn("receipt_stale", await self.flags(), "a superseded receipt is not latest")

    # ---- EP cases -------------------------------------------------------------
    async def test_stale_mf_via_item_set_flags_and_pushes_the_holder_once(self):
        await self.put_profile("py", dict(PROFILE, require={"runner": True, "mf_sha": True}))
        rec = await self.post(wire(mf=MF))
        self.assertEqual(rec["verdict"], "PASS", rec["headline"])
        await self.item_set(mf=MF2)
        latest = (await self.receipts())["latest"]["gate"]
        self.assertEqual(latest["stale"], {"reason": "mf_moved", "prev": MF[:7], "new": MF2[:7]})
        self.assertIn("receipt_stale", await self.flags())
        self.assertEqual(len(await self.pushes("receipt_stale")), 1)
        # A fresh receipt against the old MF is stale on arrival.
        again = await self.post(wire(mf=MF))
        self.assertEqual(again["verdict"], "STALE")
        # One on the new MF clears the flag.
        fresh = await self.post(wire(mf=MF2))
        self.assertEqual(fresh["verdict"], "PASS")
        self.assertNotIn("receipt_stale", await self.flags())

    async def test_red_and_void_reruns_are_counted_and_repeat_void_flags(self):
        red = with_step(wire(), "pytest", rc=1, summary="== 1 failed, 1 passed in 1.00s ==",
                        nodes=[{"id": "tests/test_a.py::test_one", "outcome": "failed",
                                "exc_type": "AssertionError"},
                               {"id": "tests/test_a.py::test_two", "outcome": "passed"}])
        self.assertEqual((await self.post(red))["verdict"], "FAIL")
        void = with_step(wire(), "fetch", state="void", rc=None, reason="FETCH_FAIL")
        v1 = await self.post(void)
        self.assertEqual(v1["verdict"], "VOID")
        self.assertNotIn("runner_void", await self.flags())
        v2 = await self.post(void)
        self.assertEqual(v2["runs"], {"FAIL": 1, "VOID": 2})
        flag = (await self.flags())["runner_void"]
        self.assertEqual(flag["detail"]["count"], 2)
        self.assertEqual(len(await self.pushes("runner_void")), 1)
        await self.post(wire())
        self.assertNotIn("runner_void", await self.flags())

    async def test_fetch_fail_after_pytest_pass_is_void_on_the_board(self):
        rec = await self.post(with_step(wire(), "fetch", state="void", rc=None, reason="FETCH_FAIL"))
        self.assertEqual(rec["verdict"], "VOID")
        self.assertIn("fetch: void (FETCH_FAIL)", rec["headline"])

    async def test_missing_harvest_is_void_and_repeat_raises_input_void(self):
        missing = wire(inputs=[{"name": "harvest", "kind": "dir", "resolved_path": "/data/harvest",
                                "content_id": "tree:" + "0" * 64, "daemon_hash": {"missing": True}}])
        self.assertEqual((await self.post(missing))["verdict"], "VOID")
        await self.post(missing)
        self.assertEqual((await self.flags())["input_void"]["detail"]["input"], "harvest")

    async def test_waivers_need_the_grantor_and_the_exact_head(self):
        failing = with_step(wire(waivers=[f"g-{self.ref}-1-1"]), "mypy", rc=1,
                            summary="Found 2 errors in 1 file (checked 3 source files)")
        rec = await self.post(failing)
        self.assertEqual(rec["verdict"], "FAIL")
        r = await self.grant(actor=LANE, ok=False)
        self.assertEqual(r.status_code, 403, "a runner cannot attach a waiver")
        r = await self.grant(step="the gate", ok=False)
        self.assertEqual(r.status_code, 422, "a generic waiver is refused")
        wrong = await self.grant(head=HEAD2)
        self.assertTrue(wrong["verified"], "verified, but for another head")
        latest = (await self.receipts())["latest"]["gate"]
        self.assertEqual(latest["verdict"], "FAIL")
        self.assertIn("unverified (head", " ".join(x["text"] for x in latest["reasons"]))
        right = await self.grant()
        self.assertEqual(right["id"], f"g-{self.ref}-1-2")
        cited = await self.post(with_step(wire(waivers=[right["id"]]), "mypy", rc=1,
                                          summary="Found 2 errors in 1 file"))
        self.assertEqual(cited["verdict"], "PASS", cited["headline"])
        self.assertIn(f"waived by {right['id']}", cited["headline"])
        r = await self.http.patch(f"/boards/{self.ref}/items/1/grants/{right['id']}",
                                  json={"actor": EP, "revoke": True})
        self.assertEqual(r.status_code, 200, r.text)
        latest = (await self.receipts())["latest"]["gate"]
        self.assertEqual(latest["stale"]["reason"], "grant_revoked")

    async def test_unverified_message_blocks_until_rechecked(self):
        await self.grant(message_check={"found": False})
        rec = await self.post(with_step(wire(waivers=[f"g-{self.ref}-1-1"]), "mypy", rc=1,
                                        summary="Found 2 errors in 1 file"))
        self.assertEqual(rec["verdict"], "FAIL")
        r = await self.http.patch(f"/boards/{self.ref}/items/1/grants/g-{self.ref}-1-1", json={
            "actor": EP, "message_check": {"found": True, "sender": EP["pid"], "contains_head": True,
                                           "contains_step": True, "contains_kind": True}})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertTrue(r.json()["grant"]["verified"])
        self.assertEqual((await self.receipts())["latest"]["gate"]["verdict"], "PASS")

    async def test_shrunk_node_set_needs_a_node_waiver_not_just_a_variance(self):
        from dispatch.gate_rules import cmd_digest

        await self.put_profile("py", dict(PROFILE, waivable=["mypy", "pytest"]))
        argv = ["pytest", "tests", "-k", "one"]
        shrunk = with_step(wire(), "pytest", summary="== 1 passed in 1.00s ==",
                           nodes=[{"id": "tests/test_a.py::test_one", "outcome": "passed"}],
                           effective_cmd={"argv": argv})
        rec = await self.post(shrunk)
        self.assertEqual(rec["verdict"], "FAIL")
        self.assertIn("weakened", rec["headline"])
        g = await self.grant(kind="variance", step="pytest", cmd_digest=cmd_digest({"argv": argv}))
        self.assertTrue(g["verified"], g["problems"])
        latest = (await self.receipts())["latest"]["gate"]
        self.assertEqual(latest["verdict"], "FAIL", "the variance covers the -k, not test_two")
        w = await self.grant(step="pytest", nodes=["tests/test_a.py::test_two"])
        cited = await self.post(dict(shrunk, waivers=[w["id"]]))
        self.assertEqual(cited["verdict"], "PASS", cited["headline"])

    async def test_a_posted_attestation_or_typed_hash_never_binds(self):
        forged = wire(attestation="cm-gate", inputs=[
            {"name": "harvest", "kind": "dir", "locator": "/data/harvest",
             "content_id": "tree:" + "0" * 64, "hashed_by": "cm-gate",
             "verification": "hashed_at_run"}])
        rec = await self.post(forged)
        self.assertEqual(rec["attestation"], "self")
        self.assertEqual(rec["inputs"][0]["verification"], "self_supplied_unverified")
        self.assertEqual(rec["verdict"], "INCOMPLETE", rec["headline"])

    async def test_input_keys_are_scoped_so_unrelated_items_never_collide(self):
        await self.put_profile("py", dict(PROFILE, inputs=[{"name": "ticks", "kind": "db"}],
                                          accept_unverified=["db"]))
        db = lambda cid: wire(inputs=[{"name": "ticks", "kind": "db", "locator": "SELECT 1",
                                       "content_id": cid}])
        first = await self.post(db("snapshot:a"), n=1)
        await self.post(db("snapshot:b"), n=2)  # another item, same query text
        self.assertEqual((await self.receipts(n=1))["latest"]["gate"]["verdict"], "PASS",
                         "a query is scoped to its own item")
        async with self.pool.acquire() as conn:
            keys = sorted(r["locator"] for r in await conn.fetch("SELECT locator FROM item_receipt_inputs"))
        self.assertTrue(all(k.startswith("item:") and k.endswith(":ticks") for k in keys), keys)
        self.assertEqual(first["verdict"], "PASS")
        # Missing local paths count per resolved path across the board.
        gone = lambda: wire(inputs=[{"name": "harvest", "kind": "dir", "resolved_path": "/data/h1",
                                     "daemon_hash": {"missing": True}}])
        await self.put_profile("py", PROFILE)
        await self.post(gone(), n=1)
        other = wire(inputs=[{"name": "harvest", "kind": "dir", "resolved_path": "/data/h2",
                              "daemon_hash": {"missing": True}}])
        await self.post(other, n=1)
        self.assertNotIn("input_void", await self.flags(), "two different paths are not a repeat")
        await self.post(gone(), n=1)
        self.assertEqual((await self.flags())["input_void"]["detail"]["locator"], "/data/h1")

    async def test_changing_the_default_class_restales_inheriting_items(self):
        inherits = await self.post(wire(), n=1)
        await self.put_profile("other", PROFILE)
        await self.item_set(n=2, gate_class="other")
        pinned = await self.post(wire(), n=2)
        await self.patch_board(EP, gate_default_class="other")
        self.assertEqual((await self.receipts(n=1))["latest"]["gate"]["stale"]["reason"], "class_changed")
        self.assertEqual((await self.receipts(n=2))["latest"]["gate"]["verdict"], "PASS",
                         "an item with its own class keeps its receipt")
        again = await self.post(wire(), n=1)
        out = (await self.put_profile("py", PROFILE, default=True)).json()
        self.assertIn(again["id"], out["restaled"])
        self.assertNotIn(pinned["id"], out["restaled"])
        self.assertNotEqual(inherits["id"], again["id"])

    async def test_list_receipts_reads_only_the_latest_rows(self):
        for _ in range(3):
            await self.post(wire())
        out = await self.receipts()
        self.assertEqual(list(out["latest"]), ["gate"])
        self.assertEqual(out["latest"]["gate"]["id"], f"r-{self.ref}-1-3")
        hist = (await self.receipts(history=True))["history"]
        self.assertEqual([h["id"] for h in hist], [f"r-{self.ref}-1-{i}" for i in (3, 2, 1)])
        self.assertTrue(all("inputs" not in h and "closure" not in h for h in hist))
        item = next(i for i in (await self.board())["items"] if i["n"] == 1)
        self.assertEqual([st["name"] for st in item["gate"]["gate"]["steps"]], ["pytest", "mypy", "fetch"])

    async def test_selection_grants_supply_the_node_set_a_profile_lacks(self):
        bare = dict(PROFILE, steps=[{"name": "pytest", "kind": "pytest", "argv": ["pytest", "tests"]},
                                    *PROFILE["steps"][1:]])
        await self.put_profile("py", bare)
        rec = await self.post(wire())
        self.assertEqual(rec["verdict"], "INCOMPLETE")
        self.assertIn("no granted node set", rec["headline"])
        r = await self.grant(kind="selection", step="pytest", ok=False)
        self.assertEqual(r.status_code, 422, "a selection grant names its nodes")
        g = await self.grant(kind="selection", step="pytest",
                             nodes=["tests/test_a.py::test_one", "tests/test_a.py::test_two"])
        self.assertTrue(g["verified"], g["problems"])
        self.assertEqual((await self.receipts())["latest"]["gate"]["verdict"], "PASS")

    async def test_a_posted_resolved_path_without_the_daemons_hash_is_ignored(self):
        rec = await self.post(wire(inputs=[{"name": "harvest", "kind": "dir",
                                            "resolved_path": "/tmp/empty",
                                            "content_id": "tree:" + "1" * 64}]))
        self.assertEqual(rec["verdict"], "INCOMPLETE")
        self.assertEqual(rec["inputs"][0]["verification"], "self_supplied_unverified")
        async with self.pool.acquire() as conn:
            key = await conn.fetchval("SELECT locator FROM item_receipt_inputs")
        self.assertTrue(key.startswith("item:"), "a poster path never becomes a shared key")

    async def test_unbound_db_window_is_incomplete(self):
        await self.put_profile("py", dict(PROFILE, inputs=[{"name": "ticks", "kind": "db"}]))
        rec = await self.post(wire(inputs=[{"name": "ticks", "kind": "db",
                                            "locator": "SELECT * FROM ticks WHERE t > now() - '1d'"}]))
        self.assertEqual(rec["verdict"], "INCOMPLETE")
        self.assertEqual(rec["inputs"][0]["verification"], "self_supplied_unverified")

    async def test_ruling_binding_makes_older_rulings_stale_on_arrival(self):
        await self.item_set(ruling="EP-A1022")
        rec = await self.post(wire(ruling="EP-A1021"))
        self.assertEqual(rec["stale"]["reason"], "ruling_superseded")
        ok = await self.post(wire(ruling="EP-A1022"))
        self.assertEqual(ok["verdict"], "PASS")

    async def test_derived_artifacts_cascade_and_changed_inputs_restale(self):
        producer = await self.post(wire())
        consumer = await self.post(wire(inputs=[
            {"name": "harvest", "kind": "dir", "resolved_path": "/data/harvest",
             "daemon_hash": {"content_id": "tree:" + "9" * 64}},
            {"name": "harvest_json", "kind": "artifact", "resolved_path": "/l4/harvest.json",
             "daemon_hash": {"content_id": "sha256:" + "7" * 64}, "produced_by": producer["id"]}]), n=2)
        self.assertEqual(consumer["verdict"], "PASS", consumer["headline"])
        await self.post(wire(head=HEAD2))  # the producer's head moved
        latest = (await self.receipts(n=2))["latest"]["gate"]
        self.assertEqual(latest["stale"]["reason"], "input_receipt_invalid")
        self.assertEqual(latest["stale"]["prev"], producer["id"])
        # Rebinding a locator with new content restales its earlier readers.
        before = await self.post(wire(head=HEAD2))
        await self.post(wire(head=HEAD2, inputs=[{"name": "harvest", "kind": "dir",
                                                  "resolved_path": "/data/harvest",
                                                  "daemon_hash": {"content_id": "tree:" + "8" * 64}}]), n=2)
        hist = {h["id"]: h for h in (await self.receipts(history=True))["history"]}
        self.assertEqual(hist[before["id"]]["stale"]["reason"], "input_changed")

    async def test_refresh_watches_only_the_posters_receipts_and_restales_on_closure_hits(self):
        rec = await self.post(wire())
        row_id = await self._row_id(rec["id"])
        other = (await self.http.post("/receipts/refresh", json={"daemon_id": "d2", "checks": [
            {"receipt_id": row_id, "branch_head": HEAD2}]})).json()
        self.assertEqual((other["watch"], other["stale"]), ([], []))
        out = (await self.http.post("/receipts/refresh", json={"daemon_id": "d1", "checks": []})).json()
        self.assertEqual([w["receipt_id"] for w in out["watch"]], [row_id])
        self.assertEqual(out["watch"][0]["scope"], "paths")
        unrelated = {"receipt_id": row_id, "base_sha": BASE2, "changed_paths": ["docs/x.md"],
                     "runner_sha": RUNNER, "branch_head": HEAD}
        out = (await self.http.post("/receipts/refresh", json={"daemon_id": "d1",
                                                                "checks": [unrelated]})).json()
        self.assertEqual(out["stale"], [])
        hit = dict(unrelated, changed_paths=["src/fuse.py"])
        out = (await self.http.post("/receipts/refresh", json={"daemon_id": "d1",
                                                                "checks": [hit]})).json()
        self.assertEqual(out["stale"], [rec["id"]])
        self.assertEqual((await self.receipts())["latest"]["gate"]["stale"]["reason"], "base_moved")

    async def test_unchecked_base_is_shown_not_hidden(self):
        rec = await self.post(wire())
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE item_receipts SET posted_at = now() - interval '40 minutes'")
        latest = (await self.receipts())["latest"]["gate"]
        self.assertEqual(latest["verdict"], "PASS")
        self.assertGreaterEqual(latest["unchecked_s"], 2400)
        self.assertEqual(latest["id"], rec["id"])

    async def _row_id(self, label):
        seq = int(label.rsplit("-", 1)[1])
        async with self.pool.acquire() as conn:
            return await conn.fetchval("SELECT id FROM item_receipts WHERE seq = $1 ORDER BY id LIMIT 1",
                                       seq)

    async def test_engine_never_sends_receipt_stale_to_the_orchestrator(self):
        from api import board_engine

        await self.post(wire())
        await self.item_set(head=HEAD2)
        orch = agent("orch")
        await self.patch_board(OWNER, orchestrator_pid=orch["pid"])
        async with self.pool.acquire() as conn:
            await conn.execute(
                """INSERT INTO session_states (pid, daemon_id, session_uid, state, reported_at)
                   VALUES ($1, 'd1', 'orch-uid', 'idle', now())""", orch["pid"])
            board_id = await conn.fetchval("SELECT id FROM boards")
        await board_engine.tick_board(self.pool, str(board_id))
        texts = [p["text"] for p in await self.pushes("board")]
        self.assertFalse(any("receipt_stale" in t for t in texts), texts)


if __name__ == "__main__":
    unittest.main()
