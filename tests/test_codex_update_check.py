"""Update alerts must be accurate, deduplicated, retryable, and install-free."""

import contextlib
import fcntl
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/codex-update-check"
loader = importlib.machinery.SourceFileLoader("codex_update_check", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
checker = importlib.util.module_from_spec(spec)
loader.exec_module(checker)


class UpdateCheckTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.config = self.root / "config.json"
        self.config.write_text(json.dumps({"hold_version": "0.153.4"}))
        self.state = {}
        self.current = self.enterContext(mock.patch.object(checker, "installed_version", return_value="0.153.4"))
        self.latest = self.enterContext(mock.patch.object(checker, "latest_version", return_value="0.154.0"))
        self.notify = self.enterContext(mock.patch.object(checker, "notify"))

    def run_check(self, now=1788730000):
        return checker.check(self.config, self.state, "/usr/bin/codex", "/fake/cm-notify", now)

    def test_new_release_notified_once_across_saved_state_and_registry_reversion(self):
        self.assertEqual(self.run_check(), 0)
        path = self.root / "state.json"
        checker.save_state(path, self.state)
        self.state = checker.load_json(path)
        self.latest.return_value = "0.153.4"
        self.run_check()
        self.latest.return_value = "0.154.0"
        self.run_check()
        self.assertEqual(self.notify.call_count, 1)
        text = self.notify.call_args.args[1]
        self.assertIn("Migration hold: 0.153.4", text)
        self.assertIn("@openai/codex@0.154.0", text)
        self.assertIn(checker.RELEASE_NOTES, text)

    def test_current_or_ahead_version_does_not_alert_or_downgrade(self):
        self.latest.return_value = "0.153.4"
        self.run_check()
        self.latest.return_value = "0.99.0"
        self.run_check()
        self.notify.assert_not_called()
        self.assertEqual(self.state["status"], "current")

    def test_numeric_version_order(self):
        self.config.write_text('{}')
        self.current.return_value = "0.9.0"
        self.latest.return_value = "0.10.0"
        self.run_check()
        self.assertEqual(self.state["status"], "update_available")
        self.assertEqual(self.notify.call_count, 1)

    def test_hold_mismatch_alerts_even_when_installed_is_latest(self):
        self.current.return_value = self.latest.return_value = "0.154.0"
        self.run_check()
        self.run_check()
        self.assertEqual(self.state["status"], "hold_mismatch")
        self.assertEqual(self.notify.call_count, 1)
        self.assertIn("expected 0.153.4, installed 0.154.0", self.notify.call_args.args[1])

    def test_failed_delivery_is_retried_and_never_marked_notified(self):
        self.notify.side_effect = checker.CheckError("notification unavailable")
        self.assertEqual(self.run_check(), 1)
        self.assertEqual(self.state["notified"], {})
        self.notify.side_effect = None
        self.assertEqual(self.run_check(), 0)
        self.assertIn("release:0.154.0", self.state["notified"])
        self.assertEqual(self.notify.call_count, 2)

    def test_repeated_failures_alert_then_deduplicate_and_report_recovery(self):
        self.latest.side_effect = checker.CheckError("registry unavailable")
        self.assertEqual(self.run_check(), 1)
        self.notify.assert_not_called()
        self.assertEqual(self.run_check(1788730100), 1)
        self.assertEqual(self.notify.call_count, 1)
        self.run_check(1788730200)
        self.assertEqual(self.notify.call_count, 1)
        self.latest.side_effect = None
        self.latest.return_value = "0.153.4"
        self.assertEqual(self.run_check(1788730300), 0)
        self.assertIn("recovered", self.notify.call_args.args[1])
        self.assertEqual(self.state["consecutive_failures"], 0)
        self.assertEqual(self.state["failure_alerted_at"], 0)

    def test_missing_or_invalid_hold_config_is_a_failure_not_a_removed_hold(self):
        for content in [None, '{"hold_version": "0.154.0-rc.1"}', '{"typo": true}']:
            if content is None:
                self.config.unlink(missing_ok=True)
            else:
                self.config.write_text(content)
            self.assertEqual(self.run_check(), 1)
        self.current.assert_not_called()

    def test_atomic_write_preserves_old_state_on_replace_failure(self):
        path = self.root / "state.json"
        checker.save_state(path, {"original": True})
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        with mock.patch.object(checker.os, "replace", side_effect=OSError("disk error")):
            with self.assertRaises(OSError):
                checker.save_state(path, {"original": False})
        self.assertEqual(json.loads(path.read_text()), {"original": True})
        self.assertEqual(list(self.root.glob("state.json.*")), [])

    def test_busy_lock_skips_without_fetch_or_notification(self):
        path = self.root / "state.json"
        with open(str(path) + ".lock", "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with contextlib.redirect_stdout(io.StringIO()):
                result = checker.main(["--state", str(path), "--config", str(self.config)])
        self.assertEqual(result, 0)
        self.current.assert_not_called()
        self.assertFalse(path.exists())

    def test_corrupt_state_is_preserved_and_alerted(self):
        path = self.root / "state.json"
        path.write_text('{broken')
        with contextlib.redirect_stderr(io.StringIO()):
            result = checker.main(["--state", str(path), "--config", str(self.config)])
        self.assertEqual(result, 1)
        self.assertEqual(path.read_text(), '{broken')
        self.current.assert_not_called()
        self.notify.assert_called_once()


class BoundaryTests(unittest.TestCase):
    def test_registry_rejects_prerelease_wrong_package_and_oversized_response(self):
        for payload in [
            json.dumps({"name": "@openai/codex", "version": "0.154.0-rc.1"}).encode(),
            json.dumps({"name": "different-package", "version": "0.154.0"}).encode(),
            b"x" * (checker.MAX_RESPONSE_BYTES + 1),
        ]:
            with self.subTest(payload=payload[:100]):
                with mock.patch.object(checker.urllib.request, "urlopen") as urlopen:
                    urlopen.return_value.__enter__.return_value.read.return_value = payload
                    with self.assertRaises(checker.CheckError):
                        checker.latest_version()

    def test_registry_uses_public_package_and_bounded_request(self):
        with mock.patch.object(checker.urllib.request, "urlopen") as urlopen:
            urlopen.return_value.__enter__.return_value.read.return_value = b'{"name":"@openai/codex","version":"0.154.0"}'
            self.assertEqual(checker.latest_version(), "0.154.0")
            self.assertEqual(urlopen.call_args.args[0].full_url, checker.REGISTRY_URL)
            self.assertEqual(urlopen.call_args.kwargs["timeout"], 20)

    def test_notifier_failure_does_not_expose_its_output(self):
        with mock.patch.object(checker.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, b"secret", b"token")) as run:
            with self.assertRaises(checker.CheckError) as error:
                checker.notify("/fake/notifier", "test message")
            self.assertNotIn("secret", str(error.exception))
            self.assertNotIn("token", str(error.exception))
            self.assertEqual(run.call_args.args[0], ["/fake/notifier", "test message"])
            self.assertEqual(run.call_args.kwargs["env"]["CM_NOTIFY_TAG"], "codex-updates")

    def test_cli_probe_only_requests_version(self):
        with mock.patch.object(checker.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "codex-cli 0.153.4\n", "")) as run:
            self.assertEqual(checker.installed_version("/usr/bin/codex"), "0.153.4")
            self.assertEqual(run.call_args.args[0], ["/usr/bin/codex", "--version"])


if __name__ == "__main__":
    unittest.main()
