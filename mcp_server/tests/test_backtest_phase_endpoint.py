"""POST /tasks/{id}/backtest-phase merges a live phase heartbeat into metadata.backtest.

The backtest worker relays a phase marker (SETUP/REPLAY/FINALIZE + download fraction) here; the
endpoint must (a) accept the phase body, (b) hand merge_task_metadata_backtest exactly the phase*
fields plus a server-stamped phase_updated_at (never trusting the worker clock for freshness), and
(c) 404 an unknown task. The merge itself (server-side jsonb `||`, no clobber) is a thin DB method
mocked here — the acceptance + field mapping is what this pins.
"""

from __future__ import annotations

import os
import unittest
from datetime import datetime, timezone
from unittest import mock

import pytest

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

# Cloud-only deps — skip cleanly when the FastAPI stack isn't installed.
pytest.importorskip("fastapi")
pytest.importorskip("asyncpg")

from fastapi import HTTPException  # noqa: E402

from api import main as api_main  # noqa: E402
from api.models import BacktestPhaseUpdate  # noqa: E402


class BacktestPhaseEndpointTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.captured: dict = {}
        self.existing = {
            "id": "t-1",
            "status": "running",
            "metadata": {
                "backtest": {"run_key": "rk-1", "launched_at": "2026-08-25T00:00:00Z"}
            },
        }

        async def fake_get(_pool, _task_id):
            return self.existing

        async def fake_merge(_pool, _task_id, fields):
            self.captured = fields
            return {
                **self.existing,
                "metadata": {"backtest": {"run_key": "rk-1", **fields}},
            }

        self._patches = [
            mock.patch.object(api_main.db, "get_task", fake_get),
            mock.patch.object(api_main.db, "merge_task_metadata_backtest", fake_merge),
        ]
        for p in self._patches:
            p.start()

    async def asyncTearDown(self):
        for p in self._patches:
            p.stop()

    async def test_setup_phase_heartbeat_is_merged_with_server_stamp(self):
        body = BacktestPhaseUpdate.model_validate(
            {
                "phase": "setup",
                "phase_step": "download_events",
                "phase_progress": 0.4,
                "phase_detail": "orderbooks: 10 snaps",
                "phase_started_at": "2026-08-25T00:01:00+00:00",
                "emitted_at": "2026-08-25T00:04:00+00:00",
            }
        )
        before = datetime.now(timezone.utc)
        result = await api_main.update_backtest_phase("t-1", body, pool=None)

        self.assertEqual(self.captured["phase"], "setup")
        self.assertEqual(self.captured["phase_step"], "download_events")
        self.assertEqual(self.captured["phase_progress"], 0.4)
        self.assertEqual(self.captured["phase_detail"], "orderbooks: 10 snaps")
        self.assertEqual(self.captured["phase_started_at"], "2026-08-25T00:01:00+00:00")
        # Worker clock preserved separately, server clock is authoritative for freshness.
        self.assertEqual(self.captured["phase_emitted_at"], "2026-08-25T00:04:00+00:00")
        stamped = datetime.fromisoformat(self.captured["phase_updated_at"])
        self.assertGreaterEqual(stamped, before)
        # The merged row surfaces the phase back (run_key preserved by the merge).
        self.assertEqual(result["metadata"]["backtest"]["phase"], "setup")
        self.assertEqual(result["metadata"]["backtest"]["run_key"], "rk-1")

    async def test_minimal_phase_only_body_leaves_optional_fields_none(self):
        body = BacktestPhaseUpdate.model_validate({"phase": "replay"})
        await api_main.update_backtest_phase("t-1", body, pool=None)
        self.assertEqual(self.captured["phase"], "replay")
        self.assertIsNone(self.captured["phase_step"])
        self.assertIsNone(self.captured["phase_progress"])
        self.assertIsNone(self.captured["phase_started_at"])
        self.assertIsNone(self.captured["phase_emitted_at"])
        self.assertIsNotNone(self.captured["phase_updated_at"])

    async def test_unknown_task_is_404_and_no_merge(self):
        async def fake_get_none(_pool, _task_id):
            return None

        with mock.patch.object(api_main.db, "get_task", fake_get_none):
            body = BacktestPhaseUpdate.model_validate({"phase": "setup"})
            with self.assertRaises(HTTPException) as ctx:
                await api_main.update_backtest_phase("nope", body, pool=None)
        self.assertEqual(ctx.exception.status_code, 404)
        self.assertEqual(self.captured, {})


if __name__ == "__main__":
    unittest.main()
