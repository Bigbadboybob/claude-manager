"""Backtest board hygiene: terminal rows auto-archive; failed VMs get a
short post-blocked TTL.

Background: every submit_backtest mints a kind='backtest' planning row
("backtest: <label> @ <branch>") and nothing ever moved terminal rows to
'archived' — a K-repeats x N-arms perf bench permanently occupied K*N board
rows. `_archive_finished_backtests` sweeps done/blocked rows off the default
board once their result artifact is persisted (results stay readable by id —
get_backtest_result already treats 'archived' as terminal). Failures keep a
longer grace than successes, and rows with no artifact are never swept.

The reaper companion: blocked rows holding a VM are now torn down on a short
blocked_at-anchored TTL (BACKTEST_BLOCKED_VM_TTL_SECS) instead of the
launch-anchored max_runtime — a run that failed 2 minutes into a 12h-limit
bench must not idle its c2-standard-4 for the remaining hours (observed
2026-08-19: 9 idle workers hand-deleted).
"""

from __future__ import annotations

import asyncio
import os
import unittest
from datetime import datetime, timedelta, timezone
from unittest import mock

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

from api import dispatch_daemon  # noqa: E402


def _row(status, *, task_id="11111111-2222-3333-4444-555555555555",
         updated_ago_s=0.0, blocked_ago_s=None, worker_vm=None,
         metadata=None):
    now = datetime.now(timezone.utc)
    return {
        "id": task_id,
        "status": status,
        "worker_vm": worker_vm,
        "ttyd_url": None,
        "updated_at": now - timedelta(seconds=updated_ago_s),
        "blocked_at": (
            now - timedelta(seconds=blocked_ago_s)
            if blocked_ago_s is not None else None
        ),
        "metadata": metadata or {},
    }


class _Recorder:
    """Async-callable that records every call's (args, kwargs)."""

    def __init__(self, result=None):
        self.calls: list[tuple[tuple, dict]] = []
        self._result = result

    async def __call__(self, *args, **kwargs):
        self.calls.append((args, kwargs))
        return self._result


class ArchiveFinishedBacktestsTest(unittest.IsolatedAsyncioTestCase):
    """_archive_finished_backtests: grace anchors, VM guard, disable knob."""

    async def _run(self, rows, *, done_grace=100, blocked_grace=1000):
        update = _Recorder()
        with mock.patch.object(dispatch_daemon, "BACKTEST_ARCHIVE_DONE_SECS", done_grace), \
             mock.patch.object(dispatch_daemon, "BACKTEST_ARCHIVE_BLOCKED_SECS", blocked_grace), \
             mock.patch.object(dispatch_daemon.db, "list_terminal_backtests_with_artifacts",
                               _Recorder(result=rows)), \
             mock.patch.object(dispatch_daemon.db, "update_task", update):
            await dispatch_daemon._archive_finished_backtests(pool=None)
        return update.calls

    async def test_done_past_grace_is_archived(self):
        calls = await self._run([_row("done", updated_ago_s=101)])
        self.assertEqual(len(calls), 1)
        (_, task_id), kwargs = calls[0]
        self.assertEqual(kwargs, {"status": "archived"})

    async def test_done_within_grace_stays(self):
        calls = await self._run([_row("done", updated_ago_s=50)])
        self.assertEqual(calls, [])

    async def test_blocked_gets_longer_grace_than_done(self):
        # Past the done grace but within the blocked grace: a failure keeps
        # its board row longer than a success would — it carries a signal.
        calls = await self._run([_row("blocked", blocked_ago_s=500)])
        self.assertEqual(calls, [])

    async def test_blocked_past_blocked_grace_is_archived(self):
        calls = await self._run([_row("blocked", blocked_ago_s=1001)])
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][1], {"status": "archived"})

    async def test_blocked_anchor_is_blocked_at_not_updated_at(self):
        # updated_at bumped recently (e.g. the reaper cleared worker_vm) must
        # not restart a failure's clock — blocked_at is the anchor.
        calls = await self._run(
            [_row("blocked", updated_ago_s=10, blocked_ago_s=1001)]
        )
        self.assertEqual(len(calls), 1)

    async def test_blocked_without_blocked_at_falls_back_to_updated_at(self):
        calls = await self._run([_row("blocked", updated_ago_s=1001)])
        self.assertEqual(len(calls), 1)

    async def test_row_still_holding_a_vm_is_left_to_the_reaper(self):
        # An archived row would fall outside list_active_backtests and
        # strand its VM forever — teardown must come first.
        calls = await self._run(
            [_row("blocked", blocked_ago_s=99999, worker_vm="cm-worker-x")]
        )
        self.assertEqual(calls, [])

    async def test_nonpositive_grace_disables_that_sweep(self):
        calls = await self._run(
            [_row("done", updated_ago_s=99999),
             _row("blocked", blocked_ago_s=99999)],
            done_grace=0,
        )
        # done sweep disabled; blocked sweep still runs.
        self.assertEqual(len(calls), 1)


class ReapBlockedVmTtlTest(unittest.IsolatedAsyncioTestCase):
    """_reap_stuck_backtests: blocked-VM teardown is blocked_at-anchored."""

    async def _run(self, rows, *, ttl=600):
        update = _Recorder()
        artifact = _Recorder()
        delete = mock.MagicMock()
        with mock.patch.object(dispatch_daemon, "BACKTEST_BLOCKED_VM_TTL_SECS", ttl), \
             mock.patch.object(dispatch_daemon.db, "list_active_backtests",
                               _Recorder(result=rows)), \
             mock.patch.object(dispatch_daemon.db, "update_task", update), \
             mock.patch.object(dispatch_daemon.db, "add_task_artifact", artifact), \
             mock.patch.object(dispatch_daemon, "_delete_worker_sync", delete):
            await dispatch_daemon._reap_stuck_backtests(pool=None)
        return update.calls, artifact.calls, delete

    async def test_blocked_within_ttl_keeps_its_vm(self):
        # The debugging window: a fresh failure's VM survives the pass even
        # when the run launched far beyond max_runtime ago.
        row = _row("blocked", blocked_ago_s=60, worker_vm="cm-worker-x",
                   metadata={"backtest": {"launched_at": "2020-01-01T00:00:00+00:00"}})
        updates, artifacts, delete = await self._run([row])
        self.assertEqual(updates, [])
        self.assertEqual(artifacts, [])
        delete.assert_not_called()

    async def test_blocked_past_ttl_tears_down_without_status_change(self):
        row = _row("blocked", blocked_ago_s=601, worker_vm="cm-worker-x",
                   metadata={"vm": {"project": "pms", "zone": "us-east4-a"}})
        updates, artifacts, delete = await self._run([row])
        self.assertEqual(len(updates), 1)
        _, kwargs = updates[0]
        self.assertEqual(kwargs, {"worker_vm": None, "ttyd_url": None})
        self.assertEqual(artifacts, [])
        delete.assert_called_once_with("cm-worker-x", "pms", "us-east4-a")

    async def test_blocked_without_blocked_at_uses_updated_at(self):
        # Legacy rows (blocked before blocked_at stamping existed).
        row = _row("blocked", updated_ago_s=601, worker_vm="cm-worker-x")
        updates, _, delete = await self._run([row])
        self.assertEqual(len(updates), 1)
        delete.assert_called_once()

    async def test_running_within_limit_is_untouched(self):
        launched = (datetime.now(timezone.utc) - timedelta(seconds=60)).isoformat()
        row = _row("running", worker_vm="cm-worker-x",
                   metadata={"backtest": {"launched_at": launched},
                             "vm": {"max_runtime_secs": 3600}})
        updates, artifacts, delete = await self._run([row])
        self.assertEqual(updates, [])
        self.assertEqual(artifacts, [])
        delete.assert_not_called()

    async def test_running_past_limit_is_reaped_with_blocked_at_stamp(self):
        # The timeout path must land a terminal row the archive sweep can
        # clean: blocked WITH blocked_at, worker_vm cleared, partial
        # artifact attached — not a third zombie state.
        launched = (datetime.now(timezone.utc) - timedelta(seconds=7200)).isoformat()
        row = _row("running", worker_vm="cm-worker-x",
                   metadata={"backtest": {"launched_at": launched, "run_key": "rk"},
                             "vm": {"max_runtime_secs": 3600}})
        updates, artifacts, delete = await self._run([row])
        self.assertEqual(len(updates), 1)
        _, kwargs = updates[0]
        self.assertEqual(kwargs["status"], "blocked")
        self.assertIsNotNone(kwargs["blocked_at"])
        self.assertIsNone(kwargs["worker_vm"])
        self.assertEqual(len(artifacts), 1)
        self.assertTrue(artifacts[0][1]["partial"])
        self.assertEqual(artifacts[0][1]["summary"]["error"], "timeout")
        delete.assert_called_once()

    async def test_claimed_but_never_launched_requeues(self):
        row = _row("running", updated_ago_s=600)
        updates, artifacts, delete = await self._run([row])
        self.assertEqual(len(updates), 1)
        self.assertEqual(updates[0][1], {"status": "backlog"})
        self.assertEqual(artifacts, [])
        delete.assert_not_called()


class ArchiveSweepIsolationTest(unittest.IsolatedAsyncioTestCase):
    """A crashing reap pass must not starve the archive pass (and vice
    versa) — they share the throttled tick in dispatch_loop."""

    async def test_reaper_crash_does_not_skip_archive(self):
        archive_ran = asyncio.Event()

        async def boom(_pool):
            raise RuntimeError("reaper boom")

        async def fake_archive(_pool):
            archive_ran.set()

        async def fake_dispatch(_pool):
            pass

        with mock.patch.object(dispatch_daemon, "_reap_stuck_backtests", boom), \
             mock.patch.object(dispatch_daemon, "_archive_finished_backtests", fake_archive), \
             mock.patch.object(dispatch_daemon, "_dispatch_tasks", fake_dispatch), \
             mock.patch.object(dispatch_daemon, "_dispatch_backtests", fake_dispatch):
            loop_task = asyncio.create_task(
                dispatch_daemon.dispatch_loop(pool=None, interval=0.01)
            )
            try:
                await asyncio.wait_for(archive_ran.wait(), timeout=2.0)
            finally:
                loop_task.cancel()
                try:
                    await loop_task
                except asyncio.CancelledError:
                    pass


if __name__ == "__main__":
    unittest.main()
