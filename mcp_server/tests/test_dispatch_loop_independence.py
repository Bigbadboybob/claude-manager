"""Regression test for dispatch / warm-pool loop independence.

Background: ``dispatch_loop`` used to run ``_dispatch_tasks`` and (every 3rd
tick) ``_maintain_warm_pools`` inside the same coroutine body. The "every
30s" interpretation only held if the body was instant — but warm-pool
maintenance does serial gcloud roundtrips per VM and ``gcloud instances
create`` calls, so under load it stretched past 30s and dragged the
dispatch cadence with it. The fix splits the two into their own asyncio
tasks; this test pins that they really are independent — a hung
``_maintain_warm_pools`` must not block ``_dispatch_tasks`` from running.
"""

from __future__ import annotations

import asyncio
import os
import unittest

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

from api import dispatch_daemon  # noqa: E402


class DispatchLoopIndependenceTest(unittest.IsolatedAsyncioTestCase):
    async def test_warm_pool_hang_does_not_block_dispatch(self):
        dispatch_calls = 0
        warm_started = asyncio.Event()
        warm_release = asyncio.Event()

        async def fake_dispatch(_pool):
            nonlocal dispatch_calls
            dispatch_calls += 1

        async def fake_warm(_pool):
            warm_started.set()
            # Hang until the test releases us — simulates a slow gcloud
            # ssh / instances create stretching a maintenance pass.
            await warm_release.wait()

        original_dispatch = dispatch_daemon._dispatch_tasks
        original_warm = dispatch_daemon._maintain_warm_pools
        dispatch_daemon._dispatch_tasks = fake_dispatch
        dispatch_daemon._maintain_warm_pools = fake_warm
        try:
            # Sub-second intervals so the test doesn't wait full 10s/30s.
            dispatch_task = asyncio.create_task(
                dispatch_daemon.dispatch_loop(pool=None, interval=0.02)
            )
            warm_task = asyncio.create_task(
                dispatch_daemon.warm_pool_loop(pool=None, interval=0.02)
            )

            try:
                # Wait until the warm loop is provably stuck.
                await asyncio.wait_for(warm_started.wait(), timeout=1.0)

                # Give dispatch time to spin many times while warm hangs.
                await asyncio.sleep(0.3)
                self.assertGreaterEqual(
                    dispatch_calls, 5,
                    f"dispatch_loop should run independently of a hung "
                    f"warm-pool pass; only saw {dispatch_calls} calls",
                )
            finally:
                warm_release.set()
                for t in (dispatch_task, warm_task):
                    t.cancel()
                for t in (dispatch_task, warm_task):
                    try:
                        await t
                    except asyncio.CancelledError:
                        pass
        finally:
            dispatch_daemon._dispatch_tasks = original_dispatch
            dispatch_daemon._maintain_warm_pools = original_warm


if __name__ == "__main__":
    unittest.main()
