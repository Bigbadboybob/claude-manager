# Incremental task feed (`GET /tasks/changes`)

## Why

Until 2026-09-10 the laptop TUI refreshed the whole task list every five seconds: two `GET /tasks` calls per tick (`backend_loop` fetched once for the Sessions view and once more for Planning), each 5,006,310 uncompressed bytes for 1,424 non-archived tasks (2.44 MB of prompts, 1.32 MB of descriptions), and the API ignored `Accept-Encoding: gzip`. One always-on viewer at that cadence is 80.57 GiB/day, about $278/month at the observed ~$0.115/GiB (30 days). Gzip alone would leave ~$91/month; sending only what changed removes nearly all of it. The full cost audit is in the predictionTrading repo, `agent_docs/cost-audit-2026-09-10.md` (branch `cm/lower-cost`); this is an illustrative per-viewer figure, not an attribution of the whole CM network bill ($15.33/day baseline, $18.66/day Sep 3–7 across all consumers).

## Design

**Change log.** `sql/016_task_changes.sql` adds `task_changes (seq BIGSERIAL, task_id, op, changed_at)` and `task_change_meta (epoch, pruned_through)`. Row-level `AFTER` triggers on `tasks` append one row per insert/update/delete and `pg_notify('cm_task_changes', seq)`; an `AFTER UPDATE OR DELETE` trigger on `initiatives` fans out to member tasks when slug/name/status/color/coordinator change (those feed the `initiative` object embedded in every task row). Because the log is written by triggers, every write path is covered: the HTTP API, the in-process dispatch daemon (`db.update_task`, `claim_next_task`, the archive sweep), and raw SQL from psql or scripts.

**Commit order.** A `BIGSERIAL` alone is not commit-ordered: a transaction can draw seq 100, stall, and commit after seq 101 is already visible, so a reader that advanced its cursor to 101 would never see 100 (the same hole a naked `updated_at > cursor` filter has). `BEFORE ... FOR EACH STATEMENT` triggers on both tables take `pg_advisory_xact_lock(hashtext('cm_task_changes'))` before any row lock or sequence draw; PostgreSQL releases it at commit/abort, so task-writing transactions serialise through the log and seq order equals commit order. Taking it before the statement's row locks on both tables keeps the lock order uniform (advisory first, rows second), so it cannot deadlock with ordinary row locking. Writes here are single-statement autocommits, so the serialisation costs microseconds. `tests/test_task_changes.py::test_seq_order_is_commit_order` pins this with two connections.

**Endpoint.** `GET /tasks/changes?since=<seq>&epoch=<epoch>&wait=<0..25>&limit=<1..2000>[&project=][&include_archived=]`, same bearer token as every other route.

* No `since`/`epoch`, an `epoch` from another log lineage, a cursor below `pruned_through` (retention) or above the sequence (restored DB): reply `{"reset": true, "epoch", "cursor", "tasks": [...full list...]}`. The snapshot reads its cursor **before** the rows in one transaction, so every change with seq ≤ cursor is reflected (rows may be newer, which later change rows re-deliver idempotently).
* Otherwise: `{"reset": false, "epoch", "cursor", "changes": [{"seq", "task_id", "op", "task"}], "more"}`. Log rows after `since` (at most `limit`) are collapsed to one entry per task, in seq order, each carrying the task's **current** row: `op = "upsert"` with the row, or `op = "remove"` when the row is gone (hard delete) or no longer matches the subscription (archived, or moved out of the `project` filter). Rows are read after the log page, so a delivered row is at least as new as `cursor`.
* `wait > 0` and nothing to deliver: the request parks on an `asyncio.Event` fed by a single `LISTEN cm_task_changes` connection (`api/task_changes.py::ChangeBroker`) until a change commits or the budget elapses, then replies with an empty `changes` list and the same cursor. Waiters hold no DB connection and no per-client queue; a slow or vanished client costs nothing beyond its own bounded request. If the listen connection drops, waiters time out and re-query (25 s latency), never miss changes; the broker reconnects with backoff.
* Retention: `change_log_maintenance_loop` prunes rows older than `CM_TASK_CHANGES_RETENTION_SECS` (7 days) every `CM_TASK_CHANGES_PRUNE_INTERVAL_SECS` (15 min) and records `pruned_through`, so an older cursor resnapshots instead of silently missing rows.

**Compression.** `GZipMiddleware(minimum_size=1024, compresslevel=5)` on the app: `/tasks`, `/tasks/changes` snapshots and every other large JSON reply are gzipped for clients that ask (ureq, httpx and requests all do). A full snapshot is ~1.6 MB on the wire instead of 5.0 MB.

**Compatibility.** `GET /tasks` and every other route are unchanged; old clients keep working and just receive gzip. The TUI detects a 404 from `/tasks/changes` (API predating the feed, or a rollback) and falls back to the legacy 5 s full poll, re-probing the feed every 60 s.

## TUI client (`tui/src/backend.rs`)

Two threads share a `TaskCache` (`HashMap<id, Task>` + `ChangeCursor {epoch, seq}` + generation):

* **feed thread** — one snapshot (`wait=0`), then long polls with `wait=25` (under the client's 30 s request timeout). A page is applied only if the cache still sits at the cursor and generation the request was issued with (compare-and-swap); otherwise it is discarded and the next poll resumes from the newer cursor. That rules out stale overwrites across reconnects, forced snapshots and duplicate replies. On errors the cursor is retained and the poll retries with backoff (1 s → 10 s); the next success catches up from the cursor or resnapshots if the server no longer has it. Connected/Disconnected status events behave as before.
* **command thread** — executes writes (PATCH/DELETE/POST) and re-emits the cached lists every 5 s **from memory**, so the app's `reconcile_tasks` / workspace sweeps keep their cadence with zero network. After a write the feed delivers the committed row (the trigger's NOTIFY releases the long poll immediately); only legacy mode refetches. `r` / planning `r` force a fresh snapshot; `A-t` answers Planning from the cache.

The app above the backend is unchanged: it still receives `TasksUpdated(Vec<Task>)` / `PlanTasksUpdated(Vec<Task>)` in the API's list order, so selection, sorting, detail views and control actions are untouched. Prompts and descriptions still ride in the snapshot (once per TUI start) and in change entries (only for changed tasks).

## Other consumers

* **trading portal** (`portal/backend/services/backtest_sync.py` in predictionTrading): every 20 s it downloads `GET /tasks?project=predictionTrading&include_archived=true` and then `GET /tasks/{id}/artifacts` for every backtest task in running/done/blocked/archived (≈ 390 requests per 30 s observed). This traffic is VPC-internal (cm-manager → trading-portal over the `.internal` name), so it is not laptop egress; it gains gzip automatically. It should move to `/tasks/changes?project=predictionTrading&include_archived=true` and fetch artifacts only for tasks whose row changed — a predictionTrading-side change, not done here.
* **daemon / MCP / CLI** `api_list_tasks` callers run on demand (rehydrate on boot, `list_tasks`, `list_subtasks` tool calls), not on a timer.

## Measuring

`scripts/measure-task-feed.py` boots a throwaway PostgreSQL 17 + the real API, seeds a production-shaped task list and reports wire bytes, requests, API CPU seconds and DB transactions per scenario (legacy identity/gzip polling, cold snapshot, idle long poll, single change, 50-change burst, reconnect catch-up, delete, expired cursor). Run on cm-sessions on 2026-09-10 (synthetic prompts compress better than real ones: the real prod list was 5,106,471 B raw / 1,649,095 B gzipped that day, the synthetic one 4.63 MB / 0.70 MB; `db_xacts` / `db_tup_returned` include the dispatch loops, see the no-client baseline):

| scenario | seconds | requests | wire_bytes | api_cpu_s | db_xacts | db_tup_returned | notes |
|---|---:|---:|---:|---:|---:|---:|---|
| baseline_no_client | 30.03 | 0 | 0 | 0.09 | 82 | 37360 |  |
| legacy_identity | 30.03 | 6 | 27765216 | 0.51 | 84 | 41372 | rows_per_response=1424, bytes_per_day_at_5s=79963822080 |
| legacy_gzip | 30.03 | 6 | 4218624 | 0.98 | 74 | 28700 | rows_per_response=1424, bytes_per_day_at_5s=12149637120 |
| feed_cold_snapshot_gzip | 0.21 | 1 | 703190 | 0.14 | 14 | 7262 | rows=1424 |
| feed_idle | 60.05 | 3 | 345 | 0.21 | 132 | 38329 | bytes_per_day=496386 |
| feed_single_change | 0.06 | 1 | 1102 | 0.01 | 2 | 123 | entries=1, patch_to_delivery_ms=31 |
| feed_burst_50 | 0.29 | 2 | 26014 | 0.15 | 2 | 123 | pages=2 |
| feed_reconnect_30_changes | 0.04 | 1 | 15976 | 0.01 | 2 | 123 | entries=30 |
| feed_delete | 0.04 | 1 | 202 | 0.01 | 2 | 123 |  |
| feed_expired_cursor_resnapshot | 0.22 | 1 | 702819 | 0.14 | 12 | 4393 |  |

Read: legacy polling moves 4.6 MB per request; the feed's idle cost is 3 requests and 345 bytes per minute (one ~112-byte reply per 25 s hold) with no DB work beyond the dispatcher's baseline, a status change is delivered ~30 ms after its commit as a ~1 KB page, and a reconnect after 30 edits catches up with one 16 KB page instead of a resnapshot.

Live on cm-manager right after the deploy (2026-09-10 04:50 UTC, 1,444 non-archived tasks): `/tasks` 5,106,471 B identity vs 1,649,095 B gzip; feed snapshot 1,649,152 B; idle poll reply 112 B; a benign PATCH on one task was delivered to a waiting poll in 44 ms as a 3,298 B page.

## Operations

* API deploy: copy `api/main.py`, `api/models.py`, `api/task_changes.py`, `dispatch/db.py`, `sql/016_task_changes.sql` to `/opt/claude-manager/` on cm-manager and `sudo systemctl restart claude-manager`; the migration runs at startup. Rollback is the previous files + restart; the triggers and log table are harmless to an old API (the log just grows until pruned, and nothing reads it).
* The log serialises task writes: a long-running transaction that touched `tasks` or `initiatives` blocks every other task write until it commits. Keep task-writing transactions short (they all are today).
* `SELECT count(*) FROM task_changes` and `task_change_meta` show the log's state; `pg_stat_activity` shows the broker's `LISTEN` connection (application_name is the default asyncpg one).
