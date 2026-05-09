"""Tests for dispatch_daemon health-check exception differentiation.

Pins that ``_check_vm_alive`` and ``_probe_warm_vm`` log distinct
messages for permanent (NotFound), transient (timeout / 5xx), and
unknown failure modes so an operator reading
``/var/log/claude-manager.log`` can tell why VMs keep getting flagged
dead. The dispatch loop's recovery behavior (create-a-replacement on
False) is intentionally unchanged; only the log granularity matters.
"""

from __future__ import annotations

import logging
import os
import subprocess
import sys
import types
import unittest

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")


def _install_google_stubs():
    """Stand in for google-cloud-compute / google.api_core in the test env.

    Real google libs aren't installed where these tests run, but
    ``_check_vm_alive`` lazy-imports them, so registering fake modules
    in ``sys.modules`` is enough for the function to exercise its
    real except-clause structure against our fake exception classes.
    """
    class _NotFound(Exception):
        pass

    class _ServiceUnavailable(Exception):
        pass

    class _DeadlineExceeded(Exception):
        pass

    class _InternalServerError(Exception):
        pass

    class _TooManyRequests(Exception):
        pass

    class _RetryError(Exception):
        pass

    class _FakeClient:
        next_exc: Exception | None = None
        next_status: str = "RUNNING"

        def get(self, **_kwargs):
            if _FakeClient.next_exc is not None:
                raise _FakeClient.next_exc
            return types.SimpleNamespace(status=_FakeClient.next_status)

    google_mod = types.ModuleType("google")
    google_cloud_mod = types.ModuleType("google.cloud")
    compute_v1_mod = types.ModuleType("google.cloud.compute_v1")
    compute_v1_mod.InstancesClient = _FakeClient
    api_core_mod = types.ModuleType("google.api_core")
    exceptions_mod = types.ModuleType("google.api_core.exceptions")
    exceptions_mod.NotFound = _NotFound
    exceptions_mod.ServiceUnavailable = _ServiceUnavailable
    exceptions_mod.DeadlineExceeded = _DeadlineExceeded
    exceptions_mod.InternalServerError = _InternalServerError
    exceptions_mod.TooManyRequests = _TooManyRequests
    exceptions_mod.RetryError = _RetryError

    google_cloud_mod.compute_v1 = compute_v1_mod
    api_core_mod.exceptions = exceptions_mod
    google_mod.cloud = google_cloud_mod
    google_mod.api_core = api_core_mod

    sys.modules["google"] = google_mod
    sys.modules["google.cloud"] = google_cloud_mod
    sys.modules["google.cloud.compute_v1"] = compute_v1_mod
    sys.modules["google.api_core"] = api_core_mod
    sys.modules["google.api_core.exceptions"] = exceptions_mod

    return _FakeClient, _NotFound, _ServiceUnavailable


_FakeClient, _NotFound, _ServiceUnavailable = _install_google_stubs()

from api import dispatch_daemon  # noqa: E402


class CheckVmAliveLoggingTest(unittest.TestCase):
    def setUp(self):
        _FakeClient.next_exc = None
        _FakeClient.next_status = "RUNNING"

    def test_running_returns_true(self):
        self.assertTrue(dispatch_daemon._check_vm_alive("vm-x"))

    def test_not_found_logs_info_returns_false(self):
        _FakeClient.next_exc = _NotFound("vm gone")
        with self.assertLogs("cm.dispatch", level=logging.INFO) as cap:
            self.assertFalse(dispatch_daemon._check_vm_alive("vm-x"))
        info_msgs = [
            r.getMessage() for r in cap.records if r.levelno == logging.INFO
        ]
        self.assertTrue(
            any("gone (404)" in m for m in info_msgs),
            f"expected '404' info log, got: {cap.output}",
        )
        self.assertFalse(
            any(r.levelno >= logging.WARNING for r in cap.records),
            f"NotFound should not trigger WARNING: {cap.output}",
        )

    def test_transient_logs_debug_returns_false(self):
        _FakeClient.next_exc = _ServiceUnavailable("503")
        with self.assertLogs("cm.dispatch", level=logging.DEBUG) as cap:
            self.assertFalse(dispatch_daemon._check_vm_alive("vm-x"))
        debug_msgs = [
            r.getMessage() for r in cap.records if r.levelno == logging.DEBUG
        ]
        self.assertTrue(
            any(
                "transient" in m and "_ServiceUnavailable" in m
                for m in debug_msgs
            ),
            f"expected transient debug log naming the exception class: {cap.output}",
        )
        self.assertFalse(
            any(r.levelno >= logging.WARNING for r in cap.records),
            f"transient errors should not WARNING: {cap.output}",
        )

    def test_unknown_logs_warning_returns_false(self):
        _FakeClient.next_exc = ValueError("weird")
        with self.assertLogs("cm.dispatch", level=logging.WARNING) as cap:
            self.assertFalse(dispatch_daemon._check_vm_alive("vm-x"))
        warn_msgs = [
            r.getMessage() for r in cap.records if r.levelno == logging.WARNING
        ]
        self.assertTrue(
            any("ValueError" in m and "weird" in m for m in warn_msgs),
            f"expected WARNING naming class+message: {cap.output}",
        )

    def test_missing_google_libs_logs_warning_returns_false(self):
        # Regression: imports must stay inside the try so a missing /
        # broken google-cloud-compute install doesn't propagate
        # ModuleNotFoundError up into the dispatch loop. Old catch-all
        # silently returned False; we preserve that, just observably.
        saved = {
            k: sys.modules[k] for k in (
                "google", "google.cloud", "google.cloud.compute_v1",
                "google.api_core", "google.api_core.exceptions",
            ) if k in sys.modules
        }
        for k in saved:
            del sys.modules[k]
        try:
            with self.assertLogs("cm.dispatch", level=logging.WARNING) as cap:
                self.assertFalse(dispatch_daemon._check_vm_alive("vm-x"))
            warn_msgs = [
                r.getMessage() for r in cap.records
                if r.levelno == logging.WARNING
            ]
            self.assertTrue(
                any(
                    "unavailable" in m and (
                        "ModuleNotFoundError" in m or "ImportError" in m
                    )
                    for m in warn_msgs
                ),
                f"expected WARNING about missing gcloud libs: {cap.output}",
            )
        finally:
            sys.modules.update(saved)


class ProbeWarmVmLoggingTest(unittest.IsolatedAsyncioTestCase):
    """The ssh ready-grep path inside ``_probe_warm_vm``.

    We shortcut ``_check_vm_alive`` to True so the test only exercises
    the second catch-all (the ssh roundtrip).
    """

    def setUp(self):
        self._orig_alive = dispatch_daemon._check_vm_alive
        self._orig_ssh = dispatch_daemon._ssh_command
        dispatch_daemon._check_vm_alive = lambda _name: True

    def tearDown(self):
        dispatch_daemon._check_vm_alive = self._orig_alive
        dispatch_daemon._ssh_command = self._orig_ssh

    async def test_ssh_timeout_logs_debug_marks_not_ready(self):
        def boom(*_a, **_k):
            raise subprocess.TimeoutExpired(cmd="gcloud ssh ...", timeout=15)
        dispatch_daemon._ssh_command = boom
        vm = {"vm_name": "warm-1", "status": "booting"}
        with self.assertLogs("cm.dispatch", level=logging.DEBUG) as cap:
            result = await dispatch_daemon._probe_warm_vm(vm)
        self.assertEqual(result, (vm, True, False))
        self.assertTrue(
            any(
                "transient" in r.getMessage() and "TimeoutExpired" in r.getMessage()
                and r.levelno == logging.DEBUG
                for r in cap.records
            ),
            f"expected DEBUG transient log: {cap.output}",
        )
        self.assertFalse(
            any(r.levelno >= logging.WARNING for r in cap.records),
            f"timeout should not WARNING: {cap.output}",
        )

    async def test_ssh_unknown_logs_warning_marks_not_ready(self):
        def boom(*_a, **_k):
            raise ValueError("wat")
        dispatch_daemon._ssh_command = boom
        vm = {"vm_name": "warm-1", "status": "booting"}
        with self.assertLogs("cm.dispatch", level=logging.WARNING) as cap:
            result = await dispatch_daemon._probe_warm_vm(vm)
        self.assertEqual(result, (vm, True, False))
        self.assertTrue(
            any(
                "ValueError" in r.getMessage() and "wat" in r.getMessage()
                and r.levelno == logging.WARNING
                for r in cap.records
            ),
            f"expected WARNING naming class+message: {cap.output}",
        )


if __name__ == "__main__":
    unittest.main()
