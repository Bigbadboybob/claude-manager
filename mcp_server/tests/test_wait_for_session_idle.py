"""Tests for `wait_for_session_idle`'s bounded pending-idle return.

Regression for the hang where the tool waited indefinitely (up to
`timeout_s`, default 600s) on a session that was genuinely idle but
whose daemon `state` stayed `"pending"` — i.e. the daemon never bound
a transcript path. That happens for bash sessions (no transcript ever)
and for claude/codex sessions on a daemon with no transcript detector
watching them (the remote-session-execution case). Pre-fix the tool
gated its return on `state == "ready"`, so a quiet-but-pending session
never satisfied it and the caller blocked to the full timeout.

The fix keeps waiting through `pending` for a grace window (so a
freshly-spawned agent's transcript can self-heal into `ready`), then
returns idle with `state="pending"` rather than blocking forever. Any
busy poll resets the grace clock.

These drive the REAL `wait_for_session_idle` with a stubbed
`control_client.call` returning scripted `resolve_authorized_session`
responses, so the actual control flow is exercised.
"""

from __future__ import annotations

import time
import unittest

from mcp_server import control_client
from mcp_server.server import wait_for_session_idle

READY_IDLE = {"state": "ready", "idle": True}
READY_BUSY = {"state": "ready", "idle": False}
PENDING_IDLE = {"state": "pending", "idle": True}
PENDING_BUSY = {"state": "pending", "idle": False}
EXITED = {"state": "exited", "idle": True}


def _scripted(sequence):
    """A `control_client.call` replacement that yields each scripted
    resolve response in turn; the last item repeats forever."""
    box = {"i": 0}

    def _call(method, params, *a, **k):
        assert method == "resolve_authorized_session", method
        i = min(box["i"], len(sequence) - 1)
        box["i"] += 1
        return sequence[i]

    return _call


class WaitForSessionIdleTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self._orig_call = control_client.call

    def tearDown(self):
        control_client.call = self._orig_call

    async def _run(self, sequence, **kw):
        control_client.call = _scripted(sequence)
        kw.setdefault("poll_interval_s", 0.05)
        t0 = time.monotonic()
        res = await wait_for_session_idle("ts-x-0", **kw)
        return res, time.monotonic() - t0

    async def test_ready_idle_returns_immediately(self):
        res, dt = await self._run([READY_IDLE], pending_idle_grace_s=0.5)
        self.assertEqual(res, {
            "idle": True, "timed_out": False, "state": "ready",
            "status": "awaiting_input",
        })
        self.assertLess(dt, 0.2)

    async def test_exited_returns_idle_exited(self):
        res, _ = await self._run([EXITED], pending_idle_grace_s=0.5)
        self.assertEqual(res, {
            "idle": True, "timed_out": False, "state": "exited",
            "status": "exited",
        })

    async def test_pending_idle_returns_after_grace_not_timeout(self):
        # The core regression: a quiet pending session must return within
        # the grace window, NOT block to `timeout_s`.
        res, dt = await self._run(
            [PENDING_IDLE], pending_idle_grace_s=0.4, timeout_s=60
        )
        self.assertEqual(res, {
            "idle": True, "timed_out": False, "state": "pending",
            "status": "starting",
        })
        self.assertGreaterEqual(dt, 0.3)  # waited roughly the grace
        self.assertLess(dt, 5.0)  # nowhere near timeout_s=60

    async def test_pending_then_ready_returns_ready(self):
        # A freshly-spawned agent: pending+busy while spinning up, then its
        # transcript binds and it goes quiet → ready/idle. Must return ready
        # (the transcript bound), never the best-effort pending result.
        res, _ = await self._run(
            [PENDING_BUSY, PENDING_BUSY, READY_BUSY, READY_IDLE],
            pending_idle_grace_s=0.4,
        )
        self.assertEqual(res, {
            "idle": True, "timed_out": False, "state": "ready",
            "status": "awaiting_input",
        })

    async def test_busy_poll_resets_pending_idle_streak(self):
        # A busy poll partway through the pending-idle streak must restart
        # the grace clock, so the tool can't return before a fresh full
        # grace window of uninterrupted quiet has elapsed.
        seq = [PENDING_IDLE, PENDING_IDLE, PENDING_BUSY] + [PENDING_IDLE] * 50
        res, dt = await self._run(seq, pending_idle_grace_s=0.4, timeout_s=60)
        self.assertEqual(res["state"], "pending")
        self.assertTrue(res["idle"])
        # 3 polls (~0.15s) burned before the reset, then a fresh ~0.4s
        # streak — so it must take longer than the grace alone.
        self.assertGreater(dt, 0.45)

    async def test_busy_session_still_times_out(self):
        # A session that never goes quiet must still surface as timed_out.
        res, _ = await self._run(
            [READY_BUSY], pending_idle_grace_s=5, timeout_s=0.3
        )
        self.assertEqual(res, {
            "idle": False, "timed_out": True, "state": "ready",
            "status": "working",
        })


if __name__ == "__main__":
    unittest.main()
