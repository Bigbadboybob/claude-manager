#!/usr/bin/env python3
"""Installer regression checks; never touch the live user service or network."""
from pathlib import Path
from unittest.mock import patch, Mock
import contextlib
import importlib.util
import io
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("installer", Path(__file__).with_name("install-codex-oauth-tunnel.py"))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)
verify_target = installer.verify_target
ensure_port_free = installer.ensure_port_free


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.path = self.home / ".config/systemd/user" / installer.UNIT_NAME
        self.enterContext(patch.object(Path, "home", return_value=self.home))
        self.enterContext(patch.object(installer.socket, "gethostname", return_value="pop-os"))
        self.control = self.enterContext(patch.object(installer, "systemctl"))
        self.port = self.enterContext(patch.object(installer, "ensure_port_free"))
        self.target = self.enterContext(patch.object(installer, "verify_target"))
        self.enterContext(patch.object(installer, "listening", return_value=True))
        self.enterContext(contextlib.redirect_stdout(io.StringIO()))

    def test_installs_callback_only_and_repeated_install_is_safe(self):
        with patch.object(installer, "active", side_effect=[False, True, True, True]):
            installer.install()
            installer.install()
        self.assertEqual(self.path.read_text(), installer.UNIT)
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o644)
        self.port.assert_called_once()
        self.assertIn("-L localhost:1455:127.0.0.1:1455 cm-sessions", installer.UNIT)
        self.assertIn("ExitOnForwardFailure=yes", installer.UNIT)
        self.assertIn("Restart=on-failure", installer.UNIT)
        self.assertNotIn("-L 2455", installer.UNIT)

    def test_cloud_host_refused_before_any_change(self):
        for hostname in ("cm-sessions", "cm-manager.internal"):
            with patch.object(installer.socket, "gethostname", return_value=hostname):
                with self.assertRaisesRegex(SystemExit, "local laptop terminal"):
                    installer.install()
        self.control.assert_not_called()
        self.assertFalse(self.path.exists())

    def test_unrelated_existing_unit_is_preserved(self):
        self.path.parent.mkdir(parents=True)
        self.path.write_text("another owner's unit\n")
        with self.assertRaisesRegex(SystemExit, "Refusing to replace"):
            installer.install()
        self.assertEqual(self.path.read_text(), "another owner's unit\n")
        self.control.assert_not_called()

    def test_port_conflict_and_unreachable_host_write_nothing(self):
        for mock, error in ((self.port, SystemExit("Port busy")),
                            (self.target, subprocess.CalledProcessError(255, "ssh"))):
            with self.subTest(mock=mock), patch.object(installer, "active", return_value=False):
                mock.side_effect = error
                with self.assertRaises(type(error)):
                    installer.install()
                mock.side_effect = None
                self.assertFalse(self.path.exists())
                self.control.assert_not_called()

    def test_failed_activation_rolls_back_only_new_unit(self):
        def control(*args, **kwargs):
            if args[0] == "enable":
                raise subprocess.CalledProcessError(1, "systemctl")
            return Mock(returncode=0)
        self.control.side_effect = control
        with patch.object(installer, "active", return_value=False):
            with self.assertRaises(subprocess.CalledProcessError):
                installer.install()
        self.assertFalse(self.path.exists())
        self.control.assert_any_call("disable", "--now", installer.UNIT_NAME, check=False)

    def test_readiness_accepts_only_expected_status(self):
        for status in ("ok", "ready", "not_ready"):
            with self.subTest(status=status), patch.object(installer.subprocess, "run", return_value=Mock(stdout='{"status":"' + status + '"}')):
                if status == "not_ready":
                    with self.assertRaises(SystemExit):
                        verify_target()
                else:
                    verify_target()

    def test_startup_timeout_removes_new_service(self):
        with patch.object(installer, "active", return_value=False), patch.object(installer.time, "monotonic", side_effect=[0, 20]):
            with self.assertRaisesRegex(RuntimeError, "did not become ready"):
                installer.install()
        self.assertFalse(self.path.exists())
        self.control.assert_any_call("disable", "--now", installer.UNIT_NAME, check=False)

    def test_busy_ipv4_or_ipv6_port_refuses_without_killing_owner(self):
        import errno
        for calls in ([OSError(errno.EADDRINUSE, "busy")], [None, OSError(errno.EADDRINUSE, "busy")]):
            with self.subTest(calls=calls), patch.object(installer.socket, "socket") as sock, patch.object(installer.socket, "has_ipv6", True):
                sock.return_value.__enter__.return_value.bind.side_effect = calls
                with self.assertRaisesRegex(SystemExit, "already in use"):
                    ensure_port_free()
        self.control.assert_not_called()

    def test_failed_repeat_preserves_preexisting_unit(self):
        self.path.parent.mkdir(parents=True)
        self.path.write_text(installer.UNIT)
        with patch.object(installer, "active", return_value=False):
            self.control.side_effect = subprocess.CalledProcessError(1, "systemctl")
            with self.assertRaises(subprocess.CalledProcessError):
                installer.install()
        self.assertEqual(self.path.read_text(), installer.UNIT)


if __name__ == "__main__":
    unittest.main()
