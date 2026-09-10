"""Change broker for the incremental task feed (``GET /tasks/changes``).

The database is the source of truth (``task_changes`` log, see
``sql/016_task_changes.sql``); this module only provides *wake-ups*. One
dedicated connection ``LISTEN``s on ``cm_task_changes`` — the trigger sends the
committed seq as payload — and every long-poll request parks on an
``asyncio.Event`` until a seq above its cursor is announced or its wait budget
runs out. Waiters hold no DB connection and no per-client queue: a slow or
vanished client costs nothing beyond its own HTTP request, which is bounded by
the request's ``wait`` parameter.

If the listen connection drops, ``latest_seq`` stops advancing until it is
re-established (exponential backoff, capped); waiters then simply time out
and re-query, so a broken notification path degrades to ``wait``-second
latency, never to missed changes.
"""

from __future__ import annotations

import asyncio
import logging
import os

import asyncpg

from dispatch import db

logger = logging.getLogger("cm.api.task_changes")

CHANNEL = "cm_task_changes"

# Change-log retention. A client whose cursor predates it resnapshots (one
# gzipped full list) instead of catching up; a week comfortably covers a
# laptop that was closed over a long weekend.
RETENTION_SECS = float(os.environ.get("CM_TASK_CHANGES_RETENTION_SECS", 7 * 86400))
PRUNE_INTERVAL_SECS = float(os.environ.get("CM_TASK_CHANGES_PRUNE_INTERVAL_SECS", 900))


async def change_log_maintenance_loop(pool, interval: float = PRUNE_INTERVAL_SECS,
                                      retention: float = RETENTION_SECS) -> None:
    """Periodically prune ``task_changes`` past ``retention`` seconds."""
    while True:
        try:
            deleted = await db.prune_task_changes(pool, retention)
            if deleted:
                logger.info("pruned %d task_changes rows older than %.0fs", deleted, retention)
        except asyncio.CancelledError:
            raise
        except Exception:  # noqa: BLE001 — keep the loop alive
            logger.exception("task_changes prune failed")
        await asyncio.sleep(interval)


class ChangeBroker:
    def __init__(self, dsn: str):
        self._dsn = dsn
        self.latest_seq: int = 0
        self._event = asyncio.Event()
        self._task: asyncio.Task | None = None
        self._conn: asyncpg.Connection | None = None
        self.listening: bool = False

    # -- lifecycle ---------------------------------------------------------

    def start(self) -> None:
        if self._task is None:
            self._task = asyncio.create_task(self._run(), name="task-change-broker")

    async def stop(self) -> None:
        if self._task is not None:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
            self._task = None
        await self._close_conn()
        self.listening = False
        self._wake()

    async def _close_conn(self) -> None:
        conn, self._conn = self._conn, None
        if conn is not None:
            try:
                await conn.close(timeout=2)
            except Exception:  # noqa: BLE001 — best-effort close on teardown
                pass

    async def _run(self) -> None:
        backoff = 1.0
        while True:
            try:
                conn = await asyncpg.connect(self._dsn)
                await conn.add_listener(CHANNEL, self._on_notify)
                self._conn = conn
                self.listening = True
                backoff = 1.0
                logger.info("task-change broker listening on %s", CHANNEL)
                # A cheap liveness probe: asyncpg surfaces a dead connection
                # on the next query, so a dropped socket is noticed within
                # one probe interval instead of never.
                while True:
                    await asyncio.sleep(30)
                    await conn.execute("SELECT 1")
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001 — reconnect on any failure
                self.listening = False
                logger.warning("task-change broker connection lost (%s); retrying in %.0fs",
                               exc, backoff)
                await self._close_conn()
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30.0)

    # -- notifications -----------------------------------------------------

    def _on_notify(self, _conn, _pid, _channel, payload) -> None:
        try:
            seq = int(payload)
        except (TypeError, ValueError):
            seq = self.latest_seq + 1
        self.announce(seq)

    def announce(self, seq: int) -> None:
        """Record that changes up to ``seq`` are committed and wake waiters."""
        if seq > self.latest_seq:
            self.latest_seq = seq
        self._wake()

    def _wake(self) -> None:
        event, self._event = self._event, asyncio.Event()
        event.set()

    # -- waiting -----------------------------------------------------------

    async def wait_beyond(self, since: int, timeout: float) -> bool:
        """Block until a seq above ``since`` has been announced or ``timeout``
        elapses. Returns True when woken by an announcement."""
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while self.latest_seq <= since:
            remaining = deadline - loop.time()
            if remaining <= 0:
                return False
            event = self._event
            try:
                await asyncio.wait_for(event.wait(), timeout=remaining)
            except asyncio.TimeoutError:
                return False
        return True
