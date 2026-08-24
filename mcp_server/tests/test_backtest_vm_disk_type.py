"""Backtest-lane boot-disk compatibility + deterministic-launch-failure handling.

Two coupled behaviours of `api.dispatch_daemon._launch_backtest_worker`,
both first forced by the c3-standard-16 perf arms (cm bug 24e3d6ff):

1. BOOT-DISK AUTO-COMPAT. The lane defaults to a pd-standard boot disk
   (SSD_TOTAL_GB quota pressure in prediction-market-scalper), but the
   c3/c4/n4/h3-class families cannot boot pd-standard at all — GCE rejects
   the insert. A submission that named only `machine_type` therefore gets
   pd-balanced; an EXPLICIT `metadata.vm.disk_type` is never rewritten.

2. DETERMINISTIC FAILURES BLOCK, capacity failures retry. The old failure
   branch always requeued to `backlog`, and `claim_next_backtest_task`
   orders by (priority, created_at) — so a submission whose VM spec can
   never be accepted came straight back to the head of its priority band
   and starved every backtest behind it. Spec errors now land the row in
   `blocked` with the evidence; quota/stockout still requeue.
"""

from __future__ import annotations

import os
import unittest
from datetime import datetime
from unittest import mock

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

from google.api_core import exceptions as gax  # noqa: E402

from api import dispatch_daemon  # noqa: E402
from dispatch import vm as dispatch_vm  # noqa: E402

_VM_DEFAULTS = {
    "project": "prediction-market-scalper",
    "zone": "us-east4-a",
    "machine_type": "c2-standard-4",
    "image_family": "cm-backtest-worker",
    "image_project": "prediction-market-scalper",
    "max_runtime_secs": 14400,
    "disk_type": "pd-standard",
    "disk_size_gb": 200,
}


class _FakeDb:
    """Records the writes `_launch_backtest_worker` makes."""

    def __init__(self):
        self.updates: list[tuple[str, dict]] = []
        self.artifacts: list[dict] = []

    async def update_task(self, _pool, task_id, **fields):
        self.updates.append((task_id, fields))
        return None

    async def add_task_artifact(self, _pool, task_id, *, summary,
                                kind="backtest-result", gcs_prefix=None,
                                partial=False):
        self.artifacts.append({"task_id": task_id, "summary": summary,
                               "kind": kind, "partial": partial})
        return {}

    def statuses(self) -> list[str]:
        return [f["status"] for _, f in self.updates if "status" in f]


def _task(vm_meta: dict | None = None) -> dict:
    return {
        "id": "abcd1234-0000-0000-0000-000000000000",
        "repo_url": "https://github.com/x/pt",
        "repo_branch": "main",
        "metadata": {
            "backtest": {"branch": "cm/perf", "config": "c.yaml"},
            **({"vm": vm_meta} if vm_meta is not None else {}),
        },
    }


class _LaunchHarness(unittest.IsolatedAsyncioTestCase):
    """Runs the real `_launch_backtest_worker` against a fake DB + launcher."""

    async def _run(self, task, *, launch=None):
        captured: dict = {}

        def fake_launch(_task, _branch, _bt, vm_over, _run_key):
            captured["vm_over"] = dict(vm_over)
            if launch is not None:
                return launch()
            return ("cm-worker-abcd1234-aaaaaa", "10.0.0.1")

        fake_db = _FakeDb()
        with mock.patch.object(dispatch_daemon, "db", fake_db), \
             mock.patch.object(dispatch_daemon, "BACKTEST_VM_DEFAULTS",
                               dict(_VM_DEFAULTS)), \
             mock.patch.object(dispatch_daemon, "_launch_backtest_vm_sync",
                               fake_launch):
            await dispatch_daemon._launch_backtest_worker(None, task)
        return captured, fake_db


class DiskTypeResolutionTests(unittest.TestCase):
    """`dispatch.vm.resolve_disk_type` — the pure decision."""

    def test_pd_standard_kept_for_supporting_families(self):
        for mt in ("c2-standard-4", "e2-medium", "n2-highmem-8", "n1-standard-1"):
            with self.subTest(machine_type=mt):
                self.assertEqual(
                    dispatch_vm.resolve_disk_type(mt, "pd-standard"),
                    "pd-standard")

    def test_pd_standard_downgraded_for_incompatible_families(self):
        for mt in ("c3-standard-16", "c3d-highmem-8", "C4-standard-4",
                   "n4-standard-2", "h3-standard-88"):
            with self.subTest(machine_type=mt):
                self.assertEqual(
                    dispatch_vm.resolve_disk_type(mt, "pd-standard"),
                    "pd-balanced")

    def test_explicit_value_is_never_rewritten(self):
        self.assertEqual(
            dispatch_vm.resolve_disk_type("c3-standard-16", "pd-standard",
                                          explicit=True),
            "pd-standard")

    def test_non_pd_standard_disks_pass_through(self):
        for dt in ("pd-ssd", "pd-balanced", "hyperdisk-balanced"):
            with self.subTest(disk_type=dt):
                self.assertEqual(
                    dispatch_vm.resolve_disk_type("c3-standard-16", dt), dt)

    def test_empty_disk_type_falls_back_to_the_universal_default(self):
        self.assertEqual(dispatch_vm.resolve_disk_type("c3-standard-16", None),
                         "pd-balanced")
        self.assertEqual(dispatch_vm.resolve_disk_type("", ""), "pd-balanced")

    def test_unknown_family_is_assumed_compatible(self):
        # A stale allow-list must never silently rewrite a working config.
        self.assertEqual(
            dispatch_vm.resolve_disk_type("zz9-standard-1", "pd-standard"),
            "pd-standard")


class LaunchErrorClassificationTests(unittest.TestCase):
    """`dispatch.vm.classify_launch_error` — retry vs block."""

    def test_unsupported_disk_type_is_a_spec_error(self):
        exc = gax.BadRequest(
            "Invalid value for field 'resource.disks[0].initializeParams."
            "diskType': 'zones/us-east4-a/diskTypes/pd-standard'. Disk type "
            "pd-standard is not supported for machine type c3-standard-16."
        )
        self.assertEqual(dispatch_vm.classify_launch_error(exc), "spec")

    def test_unknown_machine_type_is_a_spec_error(self):
        exc = gax.NotFound(
            "The resource 'projects/p/zones/us-east4-a/machineTypes/"
            "c3-standard-17' was not found"
        )
        self.assertEqual(dispatch_vm.classify_launch_error(exc), "spec")

    def test_local_payload_errors_are_spec_errors(self):
        # e.g. dispatch/vm.py's metadata-size guard — a retry re-raises it.
        self.assertEqual(
            dispatch_vm.classify_launch_error(ValueError("too big")), "spec")

    def test_quota_and_stockout_stay_retryable(self):
        for exc in (
            gax.Forbidden("Quota 'SSD_TOTAL_GB' exceeded. Limit: 2000.0 in "
                          "region us-east4."),
            gax.ServiceUnavailable(
                "The zone 'projects/p/zones/us-east4-a' does not have enough "
                "resources available to fulfill the request."),
            gax.TooManyRequests("Rate Limit Exceeded"),
            gax.InternalServerError("Internal error. Please try again."),
            ConnectionResetError("connection reset by peer"),
        ):
            with self.subTest(exc=type(exc).__name__):
                self.assertEqual(dispatch_vm.classify_launch_error(exc),
                                 "capacity")

    def test_unrecognised_errors_default_to_retry(self):
        # Conservative default: a misfiled capacity error costs one retry, a
        # misfiled spec error would kill a runnable submission.
        self.assertEqual(
            dispatch_vm.classify_launch_error(RuntimeError("¯\\_(ツ)_/¯")),
            "capacity")


class DispatchDiskTypeTests(_LaunchHarness):
    """The merge the dispatcher hands to `launch_worker`."""

    async def test_bare_c3_machine_type_gets_a_bootable_disk(self):
        captured, fake_db = await self._run(
            _task({"machine_type": "c3-standard-16"}))
        self.assertEqual(captured["vm_over"]["disk_type"], "pd-balanced")
        # ...and the corrected value is what gets persisted back on the row.
        meta = [f for _, f in fake_db.updates if "metadata" in f][0]["metadata"]
        self.assertEqual(meta["vm"]["disk_type"], "pd-balanced")

    async def test_explicit_disk_type_reaches_the_boot_spec_unchanged(self):
        captured, _ = await self._run(
            _task({"machine_type": "c3-standard-16", "disk_type": "pd-ssd"}))
        self.assertEqual(captured["vm_over"]["disk_type"], "pd-ssd")

    async def test_explicit_pd_standard_is_honoured_even_when_doomed(self):
        # An operator override is never silently rewritten; the resulting
        # rejection is a spec error, which blocks rather than looping.
        captured, _ = await self._run(
            _task({"machine_type": "c3-standard-16",
                   "disk_type": "pd-standard"}))
        self.assertEqual(captured["vm_over"]["disk_type"], "pd-standard")

    async def test_default_machine_type_keeps_the_lane_default_disk(self):
        captured, _ = await self._run(_task({}))
        self.assertEqual(captured["vm_over"]["disk_type"], "pd-standard")
        self.assertEqual(captured["vm_over"]["machine_type"], "c2-standard-4")

    async def test_missing_vm_metadata_still_launches(self):
        captured, _ = await self._run(_task(None))
        self.assertEqual(captured["vm_over"]["disk_type"], "pd-standard")


class SpecErrorBlocksTests(_LaunchHarness):
    """Head-of-line fix: deterministic launch failures must not requeue."""

    _SPEC_EXC = gax.BadRequest(
        "Invalid value for field 'resource.disks[0].initializeParams."
        "diskType': 'pd-standard'. Disk type pd-standard is not supported "
        "for machine type c3-standard-16."
    )

    async def test_spec_error_blocks_with_evidence(self):
        def boom():
            raise self._SPEC_EXC

        _, fake_db = await self._run(
            _task({"machine_type": "c3-standard-16",
                   "disk_type": "pd-standard"}),
            launch=boom,
        )

        self.assertEqual(fake_db.statuses(), ["blocked"])
        self.assertNotIn(
            "backlog", fake_db.statuses(),
            "a spec-rejected submission must not return to the queue head",
        )
        _, fields = fake_db.updates[0]
        self.assertIsInstance(fields["blocked_at"], datetime)
        err = fields["metadata"]["launch_error"]
        self.assertEqual(err["class"], "spec")
        self.assertEqual(err["machine_type"], "c3-standard-16")
        self.assertEqual(err["disk_type"], "pd-standard")
        self.assertIn("not supported", err["message"])

    async def test_spec_error_records_a_result_artifact(self):
        def boom():
            raise self._SPEC_EXC

        _, fake_db = await self._run(
            _task({"machine_type": "c3-standard-16"}), launch=boom)

        self.assertEqual(len(fake_db.artifacts), 1)
        summary = fake_db.artifacts[0]["summary"]
        self.assertEqual(summary["error"], "vm-spec-rejected")
        self.assertTrue(fake_db.artifacts[0]["partial"])
        self.assertTrue(summary["run_key"])

    async def test_artifact_failure_does_not_unblock_the_row(self):
        def boom():
            raise self._SPEC_EXC

        fake_db = _FakeDb()

        async def exploding_artifact(*_a, **_k):
            raise RuntimeError("artifacts table unreachable")

        fake_db.add_task_artifact = exploding_artifact  # type: ignore[method-assign]

        def fake_launch(*_a, **_k):
            boom()

        with mock.patch.object(dispatch_daemon, "db", fake_db), \
             mock.patch.object(dispatch_daemon, "BACKTEST_VM_DEFAULTS",
                               dict(_VM_DEFAULTS)), \
             mock.patch.object(dispatch_daemon, "_launch_backtest_vm_sync",
                               fake_launch):
            await dispatch_daemon._launch_backtest_worker(
                None, _task({"machine_type": "c3-standard-16"}))

        self.assertEqual(fake_db.statuses(), ["blocked"])

    async def test_capacity_error_still_requeues(self):
        def boom():
            raise gax.Forbidden(
                "Quota 'SSD_TOTAL_GB' exceeded. Limit: 2000.0 in region "
                "us-east4.")

        _, fake_db = await self._run(
            _task({"machine_type": "c3-standard-16"}), launch=boom)

        self.assertEqual(fake_db.statuses(), ["backlog"])
        self.assertEqual(fake_db.artifacts, [])


if __name__ == "__main__":
    unittest.main()
