"""Monitor watches survive an MCP activation and fire exactly once
(doc/mcp-managed-activation.md §5).

The predecessor is simulated by a journal producer whose pid has exited;
the successor is this process. Exactly-once rests on two facts tested here:
adoption moves a record in ONE journal write (predecessor copy `replaced`,
successor copy `watching`), and a monitor's notice ID is its monitor ID, so a
notice the predecessor already published is verified, never re-sent."""

import asyncio
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from mcp_server import async_monitor, monitor_state
from mcp_server.notifications import Queue

CALLER = "ts-caller-1"
WORKER = "ts-worker-2"
BASELINE = {"kind": "turn", "value": "fingerprint-at-arm-time"}


def dead_pid() -> int:
    proc = subprocess.Popen([sys.executable, "-c", "pass"])
    proc.wait()
    return proc.pid


class AdoptionTests(unittest.TestCase):
    def setUp(self):
        self.home = Path(tempfile.mkdtemp())
        self.env = mock.patch.dict(os.environ, {"HOME": str(self.home),
                                                "CM_TUI_SESSION_ID": CALLER})
        self.env.start()
        async_monitor._MONITORS.clear()
        self.verify = mock.patch.object(async_monitor, "VERIFY_WINDOW_S", 0.3)
        self.verify.start()
        self.watch_calls = []

    def tearDown(self):
        async_monitor._MONITORS.clear()
        self.verify.stop()
        self.env.stop()
        shutil.rmtree(self.home)

    def journal_path(self):
        return self.home / ".cm" / "monitor-state" / f"{CALLER}.json"

    def write_predecessor(self, pid, state="watching", producer="pred", extra=None, **record):
        mid = record.pop("monitor_id", "mon-aaaaaa")
        value = {
            "monitor_id": mid, "watching": [WORKER], "mode": "any", "until": "turn_end",
            "note": "", "source": "explicit", "state": state, "caller": CALLER,
            "created_at": time.time() - 30, "deadline_at": time.time() + 600,
            "baselines": {WORKER: BASELINE}, "report_anchors": {}, "result": None,
            **record,
        }
        path = self.journal_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        journal = json.loads(path.read_text()) if path.exists() else {
            "schema_version": 1, "coverage_version": 0, "session_uid": CALLER,
            "revision": 1, "producers": {}}
        journal["producers"].setdefault(producer, {"started_at": time.time(), "pid": pid,
                                                   "records": {}})["records"][mid] = value
        journal["revision"] += 1
        path.write_text(json.dumps(journal))
        return mid

    def journal(self):
        return json.loads(self.journal_path().read_text())

    async def finished_turn(self, uids, **kwargs):
        """The watched worker finished a turn while no MCP process watched."""
        self.watch_calls.append(kwargs)
        return {"completed": [{"session_uid": WORKER, "status": "awaiting_input",
                               "last_message": {"content": f"done at {time.time()}"}}],
                "still_running": [], "timed_out": False}

    def adopt(self, pid):
        async def run():
            with mock.patch.object(async_monitor, "_monitor_sessions", self.finished_turn):
                adopted = await async_monitor.adopt_predecessor(pid)
                tasks = [async_monitor._MONITORS[m]["task"] for m in adopted]
                await asyncio.gather(*tasks)
                return adopted
        return asyncio.run(run())

    def monitor_events(self):
        return [e for e in Queue(CALLER).snapshot()["notifications"]
                if e["source"] == "session_monitor"]

    def test_a_turn_that_ends_during_the_swap_fires_exactly_once(self):
        pid = dead_pid()
        mid = self.write_predecessor(pid)
        self.assertEqual(self.adopt(pid), [mid])
        self.assertEqual(self.watch_calls[0]["baselines"], {WORKER: BASELINE},
                         "the successor resumes the ORIGINAL edge, not a fresh one")
        events = self.monitor_events()
        self.assertEqual([e["id"] for e in events], [f"monitor:{mid}"])
        predecessor = self.journal()["producers"]["pred"]["records"][mid]
        self.assertEqual(predecessor["state"], "replaced")
        self.assertEqual(predecessor["adopted_by"], monitor_state.PRODUCER_ID)
        mine = self.journal()["producers"][monitor_state.PRODUCER_ID]["records"][mid]
        self.assertEqual(mine["adopted_from"], "pred")
        # Adopting again finds nothing: the predecessor's copy is settled.
        self.assertEqual(self.adopt(pid), [])
        self.assertEqual(len(self.monitor_events()), 1)

    def test_a_predecessor_that_published_then_died_is_not_doubled(self):
        pid = dead_pid()
        mid = self.write_predecessor(pid, state="fired",
                                     notification_id="monitor:mon-aaaaaa")
        # It published (different text: its own timestamps) and died before
        # recording the delivery.
        Queue(CALLER).publish(f"monitor:{mid}", "session_monitor",
                              f"[cm-monitor {mid}] predecessor text", f"[cm-monitor {mid}")
        self.adopt(pid)
        events = self.monitor_events()
        self.assertEqual(len(events), 1)
        self.assertIn("predecessor text", events[0]["text"])

    def test_a_successor_that_died_after_adopting_hands_over_again(self):
        first, second = dead_pid(), dead_pid()
        mid = self.write_predecessor(first, state="interrupted")
        # Successor 1 adopted (one write) and was killed before its watch ran.
        with mock.patch.object(monitor_state, "PRODUCER_ID", "succ1"), \
                mock.patch.object(os, "getpid", return_value=second):
            self.assertEqual(list(monitor_state.adopt(first)), [mid])
        self.assertEqual(self.journal()["producers"]["succ1"]["records"][mid]["state"],
                         "watching")
        self.assertEqual(self.adopt(first), [], "the first predecessor no longer owns it")
        self.assertEqual(self.adopt(second), [mid])
        self.assertEqual(len(self.monitor_events()), 1)

    def test_a_failed_adoption_write_changes_nothing(self):
        # A crash in the middle of the handover write: the rename never
        # happens, so the predecessor still owns the record and nobody else
        # does. A later adoption then fires it once.
        pid = dead_pid()
        mid = self.write_predecessor(pid)
        real_replace = os.replace

        def crash(src, dst):
            if str(dst).endswith(f"{CALLER}.json"):
                raise OSError("simulated crash before rename")
            return real_replace(src, dst)

        with mock.patch.object(monitor_state.os, "replace", crash), \
                mock.patch.object(async_monitor, "ADOPT_RETRY_S", 0.01):
            self.assertEqual(self.adopt(pid), [])
        journal = self.journal()
        self.assertEqual(journal["producers"]["pred"]["records"][mid]["state"], "watching")
        self.assertNotIn(monitor_state.PRODUCER_ID, journal["producers"])
        self.assertEqual(self.monitor_events(), [])
        self.assertEqual(self.adopt(pid), [mid])
        self.assertEqual(len(self.monitor_events()), 1)

    def test_a_transient_write_failure_is_retried(self):
        pid = dead_pid()
        mid = self.write_predecessor(pid)
        real_replace, failures = os.replace, []

        def flaky(src, dst):
            if str(dst).endswith(f"{CALLER}.json") and not failures:
                failures.append(dst)
                raise OSError("transient")
            return real_replace(src, dst)

        with mock.patch.object(monitor_state.os, "replace", flaky), \
                mock.patch.object(async_monitor, "ADOPT_RETRY_S", 0.01):
            self.assertEqual(self.adopt(pid), [mid])
        self.assertEqual(len(self.monitor_events()), 1)

    def test_settled_and_pre_baseline_records_are_left_alone(self):
        pid = dead_pid()
        self.write_predecessor(pid, state="delivered", monitor_id="mon-dddddd")
        self.write_predecessor(pid, state="cancelled", monitor_id="mon-cccccc")
        old = self.write_predecessor(pid, monitor_id="mon-oooooo")
        journal = self.journal()
        del journal["producers"]["pred"]["records"][old]["baselines"]
        self.journal_path().write_text(json.dumps(journal))
        self.assertEqual(self.adopt(pid), [])
        self.assertEqual(self.journal()["producers"]["pred"]["records"][old]["state"], "watching")
        self.assertEqual(self.monitor_events(), [])

    def test_a_live_predecessor_is_never_adopted_from(self):
        mid = self.write_predecessor(os.getppid())
        with mock.patch.object(async_monitor, "PREDECESSOR_EXIT_WAIT_S", 0.5):
            self.assertEqual(self.adopt(os.getppid()), [])
        self.assertEqual(self.journal()["producers"]["pred"]["records"][mid]["state"], "watching")

    def test_the_deadline_carries_over(self):
        pid = dead_pid()
        self.write_predecessor(pid, deadline_at=time.time() - 5)

        async def never(uids, **kwargs):
            raise AssertionError("an expired watch must not start watching")

        async def run():
            with mock.patch.object(async_monitor, "_monitor_sessions", never):
                adopted = await async_monitor.adopt_predecessor(pid)
                await asyncio.gather(*(async_monitor._MONITORS[m]["task"] for m in adopted))
                return async_monitor._MONITORS[adopted[0]]
        record = asyncio.run(run())
        self.assertTrue(record["result"]["timed_out"])
        self.assertEqual(len(self.monitor_events()), 1)

    def test_registration_persists_what_a_successor_needs(self):
        async def forever(*args, **kwargs):
            await asyncio.sleep(3600)

        async def run():
            with mock.patch.object(async_monitor, "_capture_baseline",
                                   return_value=(BASELINE, False, None)), \
                    mock.patch.object(async_monitor, "_monitor_sessions", forever):
                out = async_monitor.register_monitor([WORKER], timeout_s=300)
                await asyncio.sleep(0)
                async_monitor._MONITORS[out["monitor_id"]]["task"].cancel()
                return out["monitor_id"]
        mid = asyncio.run(run())
        saved = self.journal()["producers"][monitor_state.PRODUCER_ID]["records"][mid]
        self.assertEqual(saved["baselines"], {WORKER: BASELINE})
        self.assertEqual(saved["caller"], CALLER)
        self.assertGreater(saved["deadline_at"], time.time() + 200)
        self.assertEqual(self.journal()["producers"][monitor_state.PRODUCER_ID]["pid"], os.getpid())


if __name__ == "__main__":
    unittest.main()
