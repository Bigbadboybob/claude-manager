"""Org model S3 (doc/org-model.md §7): groups and epics, link-first placement,
the status mirror, translated legacy task writes, item roles, description
revisions, launch grants and possible_duplicate.

Runs against Postgres when CM_ITEMS_TEST_DSN names a scratch database (see
test_items_api.py); the pure checks run without it.
"""

from __future__ import annotations

import os
import unittest
import uuid

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

from dispatch import db, items_db  # noqa: E402
from dispatch.items_rules import Item, duplicate_hints  # noqa: E402

DSN = os.environ.get("CM_ITEMS_TEST_DSN")
TOKEN = {"Authorization": f"Bearer {os.environ['CM_API_TOKEN']}"}


def agent(name, daemon="d1"):
    return {"pid": f"agent:{daemon}:{name}-uid", "name": name,
            "session_uid": f"{name}-uid", "daemon_id": daemon}


ORCH = agent("orch")
LANE = agent("lane")
OTHER = agent("other")
OWNER = {"pid": "owner", "name": "Owner"}


class Pure(unittest.TestCase):
    def test_the_lock_is_the_task_feeds(self):
        self.assertEqual(items_db.TASK_WRITE_LOCK_SQL, db.TASK_WRITE_LOCK_SQL)

    def test_duplicate_hints_need_two_shared_words_or_a_shared_path(self):
        a = Item(n=1, title="fuse SEJD hot operators", status="active", links=["daemon/src/x.rs"])
        b = Item(n=2, title="SEJD hot operator fusion", status="active")
        c = Item(n=3, title="fuse SEJD hot operators again", status="active")
        d = Item(n=4, title="unrelated", status="active", links=["daemon/src/x.rs"])
        e = Item(n=5, title="fuse SEJD hot operators", status="done")
        hints = {h["n"]: h["why"] for h in duplicate_hints(a, [a, b, c, d, e])}
        self.assertEqual(set(hints), {3, 4})
        self.assertIn("title", hints[3])
        self.assertIn("links: daemon/src/x.rs", hints[4])


@unittest.skipUnless(DSN, "set CM_ITEMS_TEST_DSN to a scratch Postgres database")
class OrgS3Db(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        import asyncpg
        import httpx
        from fastapi import FastAPI

        from api.items import router
        from api import main as api_main

        self.pool = await asyncpg.create_pool(DSN, min_size=1, max_size=4,
                                              init=db._init_connection)
        async with self.pool.acquire() as conn:
            await conn.execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        await db.init_db(self.pool)
        app = FastAPI()
        app.state.pool = self.pool
        app.include_router(router)
        # The legacy task PATCH route, with the pool the test owns.
        app.add_api_route("/tasks/{task_id}", api_main.update_task, methods=["PATCH"],
                          response_model=api_main.TaskResponse)
        app.dependency_overrides[api_main.get_pool] = lambda: self.pool
        app.dependency_overrides[api_main.verify_token] = lambda: None
        self.http = httpx.AsyncClient(transport=httpx.ASGITransport(app=app),
                                      base_url="http://t", headers=TOKEN)
        self.root = await self.task("root")
        self.board = (await self.http.post("/boards/resolve", json={"task_id": self.root})).json()
        self.ref = self.board["slug"]
        await self.beat(ORCH, task=self.root, caps=["lifecycle_guard_v1"])

    async def asyncTearDown(self):
        await self.http.aclose()
        await self.pool.close()

    # ---- helpers -------------------------------------------------------
    async def task(self, name, *, description="do the thing; done when tested", kind="oneshot",
                   is_cloud=False, project="p", status="backlog") -> str:
        async with self.pool.acquire() as conn:
            return str(await conn.fetchval(
                """INSERT INTO tasks (repo_url, prompt, name, description, kind, is_cloud,
                                      project, status)
                   VALUES ('r', 'p', $1, $2, $3, $4, $5, $6) RETURNING id""",
                name, description, kind, is_cloud, project, status))

    async def beat(self, *who, task=None, caps=None, daemon="d1", state="working"):
        body = {"host_label": "h", "sessions": [
            {"pid": w["pid"], "session_uid": w["session_uid"], "name": w["name"], "state": state,
             "task_id": task} for w in who]}
        if caps is not None:
            body["capabilities"] = caps
        r = await self.http.post(f"/hosts/{daemon}/heartbeat", json=body)
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def place(self, task_id, ok=True, **kw):
        body = {"actor": kw.pop("actor", ORCH), "board": kw.pop("board", self.ref), **kw}
        r = await self.http.post(f"/tasks/{task_id}/place", json=body)
        if ok:
            self.assertEqual(r.status_code, 200, r.text)
        return r

    async def patch_items(self, ns, ok=True, actor=ORCH, **body):
        r = await self.http.patch(f"/boards/{self.ref}/items", json={
            "actor": actor, "ns": ns if isinstance(ns, list) else [ns], **body})
        if ok:
            self.assertEqual(r.status_code, 200, r.text)
        return r

    async def task_row(self, task_id):
        async with self.pool.acquire() as conn:
            return dict(await conn.fetchrow("SELECT * FROM tasks WHERE id = $1", uuid.UUID(task_id)))

    async def read(self, **params):
        r = await self.http.get(f"/boards/{self.ref}", params=params)
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def item(self, n):
        return {i["n"]: i for i in (await self.read())["items"]}[n]

    # ---- groups --------------------------------------------------------
    async def test_groups_headings_epics_and_nesting(self):
        r = await self.http.post(f"/boards/{self.ref}/items", json={
            "actor": LANE, "items": [{"title": "a", "group": "Batching"}]})
        self.assertEqual(r.status_code, 200, r.text)
        board = await self.read()
        (g,) = board["groups"]
        self.assertEqual((g["name"], g["kind"], g["open_items"]), ("Batching", "heading", 1))
        self.assertEqual(board["items"][0]["group_id"], g["id"])
        # An epic is Owner's or the board lead's (the orchestrator) to make.
        r = await self.http.post(f"/boards/{self.ref}/groups", json={
            "actor": LANE, "name": "Latent", "kind": "epic"})
        self.assertEqual(r.status_code, 403)
        r = await self.http.post(f"/boards/{self.ref}/groups", json={
            "actor": ORCH, "name": "Latent", "kind": "epic", "goal": "ship the forecaster"})
        self.assertEqual(r.status_code, 200, r.text)
        epic = r.json()
        self.assertEqual((epic["kind"], epic["epic_status"], epic["poc_member_id"]),
                         ("epic", "active", None))
        # Headings nest inside epics; a group cannot be inside itself.
        r = await self.http.post(f"/boards/{self.ref}/groups", json={
            "actor": LANE, "name": "Law review", "parent_group_id": epic["id"]})
        sub = r.json()
        r = await self.http.patch(f"/boards/{self.ref}/groups/{epic['id']}", json={
            "actor": ORCH, "parent_group_id": sub["id"]})
        self.assertEqual((r.status_code, r.json()["detail"]["code"]), (409, "cycle"))
        # Promote the heading (revision-checked); renaming keeps items.grp for old readers.
        r = await self.http.patch(f"/boards/{self.ref}/groups/{g['id']}", json={
            "actor": ORCH, "kind": "epic", "expected_revision": 5})
        self.assertEqual((r.status_code, r.json()["detail"]["code"]), (409, "revision_changed"))
        r = await self.http.patch(f"/boards/{self.ref}/groups/{g['id']}", json={
            "actor": ORCH, "kind": "epic", "name": "Batching & capacity", "expected_revision": 0})
        self.assertEqual(r.json()["kind"], "epic")
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval("SELECT grp FROM items WHERE number = 1"),
                             "Batching & capacity")
        # An item joins a group by id.
        await self.patch_items(1, actor=LANE, set={"group_id": sub["id"]})
        self.assertEqual((await self.item(1))["group"], "Law review")

    async def test_grp_backfill_and_holder_roles_migration(self):
        # Items written the pre-S3 way: free-text grp, holders without roles.
        r = await self.http.post(f"/boards/{self.ref}/items", json={
            "actor": ORCH, "items": [{"title": "solo", "holders": [LANE]},
                                     {"title": "pair", "holders": [LANE, OTHER]}]})
        self.assertEqual(r.status_code, 200)
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE items SET grp = 'Old heading', group_id = NULL")
            await conn.execute("UPDATE item_holders SET role = NULL")
            await conn.execute("DELETE FROM groups")
            await conn.execute("DELETE FROM migrations_applied WHERE name LIKE '029b_%'")
        await db.init_db(self.pool)
        board = await self.read()
        self.assertEqual([g["name"] for g in board["groups"]], ["Old heading"])
        items = {i["n"]: i for i in board["items"]}
        self.assertEqual([h["role"] for h in items[1]["holders"]], ["owner"])
        self.assertEqual([h["role"] for h in items[2]["holders"]], ["holder", "holder"])
        self.assertIn("pick_owner", items[2]["flags"])
        # Naming the coordinator settles the steward flag.
        await self.patch_items(2, owner=LANE, set={})
        items = {i["n"]: i for i in (await self.read())["items"]}
        self.assertNotIn("pick_owner", items[2]["flags"])
        # A second init_db run does not redo either backfill.
        await db.init_db(self.pool)
        self.assertEqual(len((await self.read())["groups"]), 1)

    # ---- roles ---------------------------------------------------------
    async def test_one_owner_contributors_and_transfer(self):
        await self.http.post(f"/boards/{self.ref}/items", json={
            "actor": LANE, "items": [{"title": "x"}]})
        third = agent("third")
        await self.patch_items(1, actor=LANE, add_holders=[OTHER, third], role="reviewer", set={})
        roles = {h["name"]: h["role"] for h in (await self.item(1))["holders"]}
        self.assertEqual(roles, {"lane": "owner", "other": "reviewer", "third": "reviewer"})
        # Removing the owner while several others hold it needs a new owner.
        r = await self.patch_items(1, ok=False, actor=LANE, remove_holders=[LANE["pid"]], set={})
        self.assertEqual(r.json()["detail"]["code"], "owner_required")
        await self.patch_items(1, actor=LANE, owner=OTHER, set={})
        roles = {h["name"]: h["role"] for h in (await self.item(1))["holders"]}
        self.assertEqual(roles, {"lane": "contributor", "other": "owner", "third": "reviewer"})
        # The owner leaving with one holder left: that holder takes over.
        await self.patch_items(1, actor=OTHER, remove_holders=[OTHER["pid"], third["pid"]], set={})
        self.assertEqual([(h["name"], h["role"]) for h in (await self.item(1))["holders"]],
                         [("lane", "owner")])
        # Hand-back keeps today's rules.
        await self.patch_items(1, set={"holders": []})
        self.assertEqual((await self.item(1))["status"], "open")

    # ---- placement and the mirror -------------------------------------
    async def test_place_mirror_close_unplace_and_replace(self):
        t = await self.task("ship it")
        r = await self.place(t, holder=LANE)
        reply = r.json()
        self.assertEqual((reply["item_status"], reply["task_status"]), ("active", "running"))
        n = reply["n"]
        row = await self.task_row(t)
        self.assertEqual((row["placement"]["n"], row["placement"]["status"]), (n, "active"))
        item = await self.item(n)
        self.assertEqual((item["task_id"], item["title"], item["description"]),
                         (t, "ship it", "do the thing; done when tested"))
        self.assertEqual(item["holders"][0]["role"], "owner")
        # Placing again on the same board answers the existing link.
        self.assertTrue((await self.place(t)).json()["already_placed"])
        # Blocked and done mirror; a board close marks terminal_via=item.
        await self.patch_items(n, actor=LANE, set={"status": "blocked", "blocked_on": "EP GO",
                                                     "check_back": "2h"})
        self.assertEqual((await self.task_row(t))["status"], "blocked")
        await self.patch_items(n, actor=LANE, set={"status": "done"})
        row = await self.task_row(t)
        self.assertEqual((row["status"], row["metadata"]["terminal_via"]), ("done", "item"))
        await self.patch_items(n, actor=LANE, reopen=True, set={"status": "dropped"})
        row = await self.task_row(t)
        self.assertEqual((row["status"], row["metadata"]["dropped"]), ("done", True))
        await self.patch_items(n, actor=LANE, reopen=True, set={"status": "active"})
        row = await self.task_row(t)
        self.assertEqual(row["status"], "running")
        self.assertNotIn("terminal_via", row["metadata"])
        # Unplace: link archived, task back to backlog, writes refused.
        r = await self.http.post(f"/tasks/{t}/unplace", json={"actor": ORCH, "reason": "later"})
        self.assertEqual(r.status_code, 200, r.text)
        row = await self.task_row(t)
        self.assertEqual((row["status"], row["placement"]), ("backlog", None))
        r = await self.patch_items(n, ok=False, actor=LANE, set={"note": "stale"})
        self.assertEqual((r.status_code, r.json()["detail"]["code"]), (409, "link_archived"))
        self.assertNotIn(n, {i["n"] for i in (await self.read())["items"]})
        # Re-placing on the same board reactivates the same item.
        self.assertEqual((await self.place(t, holder=LANE)).json()["n"], n)
        # Another board: refused unless replace (unplace there, place here).
        other = (await self.http.post("/boards/resolve",
                                      json={"task_id": await self.task("other root")})).json()
        r = await self.place(t, ok=False, board=other["slug"])
        self.assertEqual((r.status_code, r.json()["detail"]["code"]), (409, "placed_elsewhere"))
        r = await self.place(t, board=other["slug"], replace=True, actor=OWNER)
        self.assertEqual((await self.task_row(t))["placement"]["board"], other["slug"])

    async def test_place_refusals(self):
        for kw, code in ((dict(kind="backtest"), "not_placeable"),
                         (dict(kind="continuous"), "not_placeable"),
                         (dict(is_cloud=True, project=None), "not_placeable"),
                         (dict(description=""), "description_required"),
                         (dict(status="done"), "task_closed")):
            t = await self.task("x", **kw)
            r = await self.place(t, ok=False)
            self.assertEqual(r.json()["detail"]["code"], code, kw)
        # A draft is accepted by placing it.
        t = await self.task("draft", status="draft")
        await self.place(t)
        self.assertEqual((await self.task_row(t))["status"], "backlog")
        # Unplacing refuses live blocked_by edges.
        a, b = await self.task("a"), await self.task("b")
        na = (await self.place(a, holder=LANE)).json()["n"]
        nb = (await self.place(b, holder=LANE)).json()["n"]
        await self.patch_items(nb, actor=LANE, set={"blocked_by": [na]})
        for t in (a, b):
            r = await self.http.post(f"/tasks/{t}/unplace", json={"actor": ORCH})
            self.assertEqual(r.json()["detail"]["code"], "has_dependencies")

    async def test_mirror_waits_for_every_daemon_holding_the_tasks_sessions(self):
        t = await self.task("guarded")
        old = agent("lane", daemon="d9")
        await self.beat(old, task=t, daemon="d9", caps=[])  # an old daemon holds a session
        n = (await self.place(t, holder=old)).json()["n"]
        row = await self.task_row(t)
        self.assertEqual(row["status"], "backlog", "no mirror while d9 lacks the guard")
        self.assertEqual((row["placement"]["mirror"], row["placement"]["mirror_waiting_on"]),
                         ("off", ["d9"]))
        await self.beat(old, task=t, daemon="d9", caps=["lifecycle_guard_v1"])
        await self.patch_items(n, actor=ORCH, set={"note": "go"})
        self.assertEqual((await self.task_row(t))["status"], "running")

    # ---- legacy writes -------------------------------------------------
    async def legacy(self, t, ok=True, **body):
        r = await self.http.patch(f"/tasks/{t}", json=body)
        if ok:
            self.assertEqual(r.status_code, 200, r.text)
        return r

    async def test_legacy_status_writes_are_translated(self):
        t = await self.task("legacy")
        n = (await self.place(t, holder=LANE)).json()["n"]
        # Unchanged status (the old editor always sends one) is ignored.
        await self.legacy(t, status="running", name="legacy renamed")
        item = await self.item(n)
        self.assertEqual((item["status"], item["title"]), ("active", "legacy renamed"))
        # blocked alone: a labelled default trigger and a flag; nothing invented.
        await self.legacy(t, status="blocked", actor=LANE)
        item = await self.item(n)
        self.assertEqual((item["status"], item["blocked_on"]),
                         ("blocked", "legacy status write: no reason given"))
        self.assertIsNotNone(item["check_back_at"])
        self.assertIn("legacy_blocked", item["flags"])
        async with self.pool.acquire() as conn:
            actor = await conn.fetchval(
                "SELECT actor_pid FROM item_events WHERE type = 'updated' ORDER BY id DESC LIMIT 1")
        self.assertEqual(actor, LANE["pid"], "attributed to the session, not the API token")
        # blocked with a reason; done; a write with no item equivalent.
        await self.legacy(t, status="blocked", reason="waiting on EP", actor=LANE)
        self.assertEqual((await self.item(n))["blocked_on"], "waiting on EP")
        await self.legacy(t, status="done")
        self.assertEqual((await self.task_row(t))["status"], "done")
        r = await self.legacy(t, ok=False, status="draft")
        self.assertEqual((r.status_code, r.json()["detail"]["code"]), (409, "placed_on_board"))
        # backlog unplaces (subject to the unplace rules); running from a spawn
        # makes the spawned session the owner of an unheld item.
        t2 = await self.task("spawned")
        n2 = (await self.place(t2, actor=OWNER)).json()["n"]
        await self.legacy(t2, status="running", actor=OTHER)
        item = await self.item(n2)
        self.assertEqual((item["status"], [(h["name"], h["role"]) for h in item["holders"]]),
                         ("active", [("other", "owner")]))
        await self.legacy(t2, status="backlog")
        self.assertIsNone((await self.task_row(t2))["placement"])

    async def test_description_revisions_notify_holders(self):
        t = await self.task("described")
        n = (await self.place(t, holder=LANE)).json()["n"]
        await self.legacy(t, description="new contract: also benchmark it", actor=ORCH)
        row = await self.task_row(t)
        self.assertEqual((row["description"], row["description_rev"]),
                         ("new contract: also benchmark it", 1))
        item = await self.item(n)
        self.assertEqual((item["description"], item["description_rev"]),
                         ("new contract: also benchmark it", 1))
        async with self.pool.acquire() as conn:
            pushes = await conn.fetch(
                "SELECT session_uid, text FROM item_pushes WHERE kind = 'description_changed'")
        self.assertEqual([p["session_uid"] for p in pushes], ["lane-uid"])
        self.assertIn("changed by orch (revision 1)", pushes[0]["text"])
        # A task that isn't placed just bumps its revision.
        t2 = await self.task("loose")
        await self.legacy(t2, description="v2")
        self.assertEqual((await self.task_row(t2))["description_rev"], 1)

    # ---- launch grants, duplicates ------------------------------------
    async def test_launch_grants_follow_the_owner(self):
        t = await self.task("granted")
        n = (await self.place(t, holder=LANE)).json()["n"]
        reply = await self.beat(ORCH, task=self.root, caps=["lifecycle_guard_v1"])
        self.assertEqual([(g["task_id"], g["session_uid"]) for g in reply["launch_grants"]],
                         [(t, "lane-uid")])
        await self.patch_items(n, actor=LANE, owner=OTHER, set={})
        reply = await self.beat(ORCH, task=self.root, caps=["lifecycle_guard_v1"])
        self.assertEqual([g["session_uid"] for g in reply["launch_grants"]], ["other-uid"])
        await self.patch_items(n, actor=OTHER, set={"status": "done"})
        reply = await self.beat(ORCH, task=self.root, caps=["lifecycle_guard_v1"])
        self.assertEqual(reply["launch_grants"], [])

    async def test_create_with_a_request_id_reuses_existing_groups_and_hints_duplicates(self):
        await self.http.post(f"/boards/{self.ref}/items", json={
            "actor": LANE, "items": [{"title": "fuse SEJD hot operators", "group": "RL"}]})
        r = await self.http.post(f"/boards/{self.ref}/items", json={
            "actor": LANE, "request_id": "rq-1",
            "items": [{"title": "fuse SEJD hot operators again", "group": "rl"}]})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(r.json()["possible_duplicate"]["2"][0]["n"], 1)
        board = await self.read()
        self.assertEqual([g["name"] for g in board["groups"]], ["RL"])
        self.assertEqual({i["group"] for i in board["items"]}, {"RL"})

    async def test_possible_duplicate_on_place_and_on_boards(self):
        await self.http.post(f"/boards/{self.ref}/items", json={
            "actor": LANE, "items": [{"title": "fuse SEJD hot operators"}]})
        t = await self.task("fuse SEJD hot operators properly")
        reply = (await self.place(t, holder=LANE)).json()
        self.assertEqual([d["n"] for d in reply["possible_duplicate"]], [1])
        await self.http.post(f"/boards/{self.ref}/groups", json={
            "actor": ORCH, "name": "Latent", "kind": "epic"})
        r = await self.http.get("/items/on-boards")
        body = r.json()
        self.assertEqual([e["name"] for e in body["epics"]], ["Latent"])
        self.assertEqual([i["n"] for i in body["items"]], [1], "board-native items only")
        again = (await self.http.get("/items/on-boards",
                                     params={"since_version": body["version"]})).json()
        self.assertEqual(again, {"unchanged": True, "version": body["version"]})

    # ---- epics -----------------------------------------------------------
    async def epic(self, name, actor=ORCH, **kw):
        r = await self.http.post(f"/boards/{self.ref}/groups", json={
            "actor": actor, "name": name, "kind": "epic", **kw})
        self.assertEqual(r.status_code, 200, r.text)
        return r.json()

    async def test_epic_close_plan_applies_items_and_resumes(self):
        e = await self.epic("Latent")
        heading = (await self.http.post(f"/boards/{self.ref}/groups", json={
            "actor": ORCH, "name": "Follow-ups"})).json()
        await self.http.post(f"/boards/{self.ref}/items", json={
            "actor": LANE, "items": [{"title": "native a", "group_id": e["id"]},
                                     {"title": "native b", "group_id": e["id"]}]})
        t = await self.task("placed c")
        nc = (await self.place(t, holder=LANE, group_id=e["id"])).json()["n"]
        url = f"/boards/{self.ref}/epics/{e['id']}/close-plan"
        r = await self.http.post(url, json={"actor": LANE, "outcome": "done"})
        self.assertEqual(r.status_code, 403, "closing is Owner's or the board lead's")
        plan = (await self.http.post(url, json={"actor": ORCH, "outcome": "done"})).json()
        rows = {i["n"]: i for i in plan["items"]}
        self.assertEqual(sorted(rows), [1, 2, nc])
        self.assertEqual(rows[nc]["choice_options"], ["move", "unplace", "drop"])
        self.assertEqual(rows[1]["choice_options"], ["move", "drop"])
        self.assertEqual([t["name"] for t in plan["move_targets"]], ["Follow-ups"])
        self.assertIsNone(plan["team"])
        apply = f"/boards/{self.ref}/close-plans/{plan['plan_id']}/apply"
        # Every open item needs a choice; nothing is applied until it has one.
        r = await self.http.post(apply, json={"actor": ORCH, "revision": plan["revision"],
                                              "choices": {"items": {"1": "drop"}}})
        self.assertEqual(r.json()["detail"]["code"], "choice_required")
        r = await self.http.post(apply, json={"actor": ORCH, "revision": plan["revision"] + 1,
                                              "choices": {}})
        self.assertEqual(r.json()["detail"]["code"], "revision_changed")
        # A partial apply (crash after #1 moved) resumes without redoing #1.
        choices = {"items": {"1": f"move:{heading['id']}", "2": "drop", str(nc): "unplace"}}
        await self.patch_items(1, set={"group_id": heading["id"]})
        async with self.pool.acquire() as conn:
            await conn.execute("""UPDATE epic_close_plans SET status = 'applying',
                                         progress = '["item:1"]'::jsonb WHERE id = $1""",
                               plan["plan_id"])
        # Rebuilding while it is being applied is refused: resume instead.
        r = await self.http.post(url, json={"actor": ORCH, "outcome": "done"})
        self.assertEqual(r.json()["detail"]["code"], "close_in_progress")
        r = await self.http.post(apply, json={"actor": ORCH, "revision": plan["revision"],
                                              "choices": choices})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual((r.json()["applied"], r.json()["pending"]), (True, []))
        items = {i["n"]: i for i in (await self.read(archived="true"))["items"]}
        self.assertEqual(items[1]["group"], "Follow-ups")
        self.assertNotIn(2, items, "dropped items leave the live board")
        self.assertEqual((await self.task_row(t))["status"], "backlog")
        group = {g["id"]: g for g in (await self.read())["groups"]}[e["id"]]
        self.assertEqual((group["epic_status"], group["closed_by"]), ("done", "orch"))
        # A closed epic takes no new items; a second apply is a replay.
        r = await self.patch_items(1, ok=False, set={"group_id": e["id"]})
        self.assertEqual(r.json()["detail"]["code"], "epic_closed")
        r = await self.http.post(apply, json={"actor": ORCH, "revision": plan["revision"],
                                              "choices": choices})
        self.assertTrue(r.json()["replayed"])

    async def test_close_needs_sub_epics_closed_first(self):
        parent = await self.epic("Program")
        await self.epic("Phase 1", parent_group_id=parent["id"])
        r = await self.http.post(f"/boards/{self.ref}/epics/{parent['id']}/close-plan",
                                 json={"actor": OWNER, "outcome": "cancelled"})
        self.assertEqual((r.status_code, r.json()["detail"]["code"]), (409, "active_sub_epics"))

    async def test_set_poc_without_the_org_core_says_so(self):
        try:
            import dispatch.org_db  # noqa: F401
            self.skipTest("org core present: covered by the PoC tests")
        except ImportError:
            pass
        e = await self.epic("Latent")
        r = await self.http.post(f"/boards/{self.ref}/epics/{e['id']}/poc",
                                 json={"actor": OWNER, "member": "lfl"})
        self.assertEqual((r.status_code, r.json()["detail"]["code"]), (409, "org_core_missing"))

    async def test_poc_team_staffing_and_close_with_the_org_core(self):
        try:
            import dispatch.org_db  # noqa: F401
        except ImportError:
            self.skipTest("needs the org core (S2)")
        async with self.pool.acquire() as conn:
            await conn.execute(
                """INSERT INTO members (id, handle, kind, manager_member_id)
                   VALUES ('mem_lfl', 'lfl', 'agent', 'mem_owner'),
                          ('mem_lane', 'lane', 'agent', 'mem_owner')
                   ON CONFLICT DO NOTHING""")
            await conn.execute(
                """INSERT INTO member_aliases (org_id, alias, member_id)
                   VALUES ('main', 'lfl', 'mem_lfl'), ('main', 'lane', 'mem_lane')
                   ON CONFLICT DO NOTHING""")
            await conn.execute(
                """INSERT INTO incarnations (member_id, role, pid, daemon_id, session_uid)
                   VALUES ('mem_lane', 'main', $1, 'd1', 'lane-uid')""", LANE["pid"])
        e = await self.epic("Latent", goal="ship the forecaster")
        await self.http.post(f"/boards/{self.ref}/items", json={
            "actor": LANE, "items": [{"title": "fit the model", "group_id": e["id"]}]})
        poc = f"/boards/{self.ref}/epics/{e['id']}/poc"
        r = await self.http.post(poc, json={"actor": LANE, "member": "lfl"})
        self.assertEqual(r.status_code, 403)
        r = await self.http.post(poc, json={"actor": ORCH, "member": "lfl"})
        self.assertEqual(r.status_code, 200, r.text)
        body = r.json()
        self.assertEqual((body["poc_member_id"], body["group"]["poc_member_id"]),
                         ("mem_lfl", "mem_lfl"))
        self.assertEqual([(s["member_id"], s["holds"], s["offer"]) for s in body["staffing"]],
                         [("mem_lane", [1], "guest")])
        async with self.pool.acquire() as conn:
            team = await conn.fetchrow("SELECT kind, lead_member_id, name FROM initiatives WHERE id = $1",
                                       uuid.UUID(body["team_id"]))
        self.assertEqual(tuple(team), ("team", "mem_lfl", "Latent"))
        # The PoC is read from the team's lead, never stored on the group.
        group = {g["id"]: g for g in (await self.read())["groups"]}[e["id"]]
        self.assertEqual(group["poc_member_id"], "mem_lfl")
        r = await self.http.post(f"/boards/{self.ref}/epics/{e['id']}/staff",
                                 json={"actor": ORCH, "member": "lane", "as": "guest"})
        self.assertEqual(r.status_code, 200, r.text)
        plan = (await self.http.post(f"/boards/{self.ref}/epics/{e['id']}/close-plan",
                                     json={"actor": ORCH, "outcome": "done"})).json()
        self.assertEqual((plan["team"]["options"], plan["team"]["default"]),
                         (["keep", "standby", "archive"], "keep"))
        guest = {m["member_id"]: m for m in plan["members"]}["mem_lane"]
        self.assertEqual((guest["role"], guest["default"]), ("guest", "end"))
        r = await self.http.post(f"/boards/{self.ref}/close-plans/{plan['plan_id']}/apply", json={
            "actor": ORCH, "revision": plan["revision"],
            "choices": {"items": {"1": "drop"}, "team": "standby"}})
        self.assertEqual(r.status_code, 200, r.text)
        # Standby is Owner's here: pending, and the epic still closes.
        self.assertEqual(r.json()["pending"],
                         [{"row": "team", "choice": "standby", "who_must_decide": "owner"}])
        async with self.pool.acquire() as conn:
            until = await conn.fetchval(
                "SELECT until FROM team_memberships WHERE member_id = 'mem_lane' AND role = 'guest'")
        self.assertIsNotNone(until, "the guest membership ended")

    # ---- review of 6522623 ---------------------------------------------
    async def test_the_mirror_gate_fails_closed_and_legacy_writes_still_land(self):
        async with self.pool.acquire() as conn:
            await conn.execute("DELETE FROM org_daemons")
        t = await self.task("ungated")
        n = (await self.place(t, holder=LANE)).json()["n"]
        row = await self.task_row(t)
        self.assertEqual((row["status"], row["placement"]["mirror"],
                          row["placement"]["mirror_waiting_on"]),
                         ("backlog", "off", ["no_fresh_daemons"]))
        # A board close is not mirrored: no terminal_via for an old sweep.
        await self.patch_items(n, actor=LANE, set={"status": "done"})
        row = await self.task_row(t)
        self.assertEqual(row["status"], "backlog")
        self.assertNotIn("terminal_via", row["metadata"] or {})
        # A daemon that heartbeats without the guard keeps it closed.
        await self.beat(agent("x", daemon="d7"), daemon="d7", caps=[])
        await self.beat(ORCH, task=self.root, caps=["lifecycle_guard_v1"])
        t2 = await self.task("legacy while closed")
        await self.place(t2, holder=LANE)
        self.assertEqual((await self.task_row(t2))["placement"]["mirror_waiting_on"], ["d7"])
        # Legacy status writes still update the task row as today.
        await self.legacy(t2, status="blocked", reason="EP GO", actor=LANE)
        self.assertEqual((await self.task_row(t2))["status"], "blocked")
        # Once every fresh daemon advertises the guard, the engine's pass
        # writes the waiting tasks through.
        await self.beat(agent("x", daemon="d7"), daemon="d7", caps=["lifecycle_guard_v1"])
        from dispatch import placement
        self.assertGreaterEqual(await placement.remirror_pending(self.pool), 2)
        row = await self.task_row(t)
        self.assertEqual((row["status"], row["metadata"]["terminal_via"]), ("done", "item"))
        self.assertNotIn("mirror", row["placement"])

    async def test_naming_yourself_owner_grants_no_launch(self):
        t = await self.task("someone else's")
        n = (await self.place(t, holder=LANE)).json()["n"]   # placed by the orchestrator
        await self.patch_items(n, actor=OTHER, owner=OTHER, set={})
        roles = {h["name"]: h["role"] for h in (await self.item(n))["holders"]}
        self.assertEqual(roles["other"], "owner")
        reply = await self.beat(ORCH, task=self.root, caps=["lifecycle_guard_v1"])
        self.assertEqual(reply["launch_grants"], [], "the role, but no grant")
        async with self.pool.acquire() as conn:
            reason = await conn.fetchval("SELECT reason FROM launch_grants WHERE pid = $1",
                                         LANE["pid"])
        self.assertEqual(reason, "owner changed")
        # Handing back revokes with its own label.
        t2 = await self.task("handed back")
        n2 = (await self.place(t2, holder=OTHER)).json()["n"]
        await self.patch_items(n2, actor=OTHER, set={"holders": []})
        async with self.pool.acquire() as conn:
            reason = await conn.fetchval(
                "SELECT reason FROM launch_grants WHERE task_id = $1", uuid.UUID(t2))
        self.assertEqual(reason, "handed back")

    async def test_a_close_plan_applies_only_on_its_board_and_skips_stale_items(self):
        e = await self.epic("Latent")
        await self.http.post(f"/boards/{self.ref}/items", json={
            "actor": LANE, "items": [{"title": "a", "group_id": e["id"]},
                                     {"title": "b", "group_id": e["id"]}]})
        plan = (await self.http.post(f"/boards/{self.ref}/epics/{e['id']}/close-plan",
                                     json={"actor": ORCH, "outcome": "done"})).json()
        other = (await self.http.post("/boards/resolve",
                                      json={"task_id": await self.task("other root")})).json()
        body = {"actor": OWNER, "revision": plan["revision"],
                "choices": {"items": {"1": "drop", "2": "drop"}}}
        r = await self.http.post(f"/boards/{other['slug']}/close-plans/{plan['plan_id']}/apply",
                                 json=body)
        self.assertEqual((r.status_code, r.json()["detail"]["code"]), (404, "not_found"))
        # #2 closed since the plan was built: skipped and recorded.
        await self.patch_items(2, actor=LANE, set={"status": "done"})
        r = await self.http.post(f"/boards/{self.ref}/close-plans/{plan['plan_id']}/apply",
                                 json={**body, "actor": ORCH})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(r.json()["skipped"], [{"n": 2, "reason": "no longer open"}])

    async def test_archiving_a_placed_task(self):
        t = await self.task("archived")
        n = (await self.place(t, holder=LANE)).json()["n"]
        r = await self.legacy(t, ok=False, status="archived")
        self.assertEqual(r.json()["detail"]["code"], "placed_on_board")
        await self.patch_items(n, actor=LANE, set={"status": "done"})
        await self.legacy(t, status="archived")
        row = await self.task_row(t)
        self.assertEqual((row["status"], row["placement"]), ("archived", None))
