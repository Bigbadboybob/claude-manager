#!/usr/bin/env python3
"""Check the laptop installer with disposable files and a mocked SSH transfer."""
from pathlib import Path
from unittest.mock import patch
import contextlib
import hashlib
import importlib.util
import io
import subprocess
import tempfile
import unittest


spec = importlib.util.spec_from_file_location('installer', Path(__file__).with_name('install-cm-tui.py'))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.target = self.home / '.cm/shared-target/release/claude-manager-tui'
        self.target.parent.mkdir(parents=True)
        self.target.write_bytes(b'previous executable')
        self.payload = b'new executable'
        self.release = {'commit': '123456789abcdef', 'source': 'cm-sessions:/release/claude-manager-tui',
                        'sha256': hashlib.sha256(self.payload).hexdigest()}
        self.enterContext(patch.object(Path, 'home', return_value=self.home))
        self.enterContext(patch('socket.gethostname', return_value='owner-laptop'))
        self.enterContext(contextlib.redirect_stdout(io.StringIO()))

    def transfer(self, args, **kwargs):
        Path(args[-1]).write_bytes(self.payload)

    def test_install_and_repeat_preserve_original_backup(self):
        with patch('subprocess.run', side_effect=self.transfer):
            installer.install(self.release)
            installer.install(self.release)
        self.assertEqual(self.target.read_bytes(), self.payload)
        self.assertEqual(self.target.stat().st_mode & 0o777, 0o755)
        self.assertEqual(self.target.with_name('claude-manager-tui.before-1234567').read_bytes(),
                         b'previous executable')
        self.assertFalse(list(self.target.parent.glob('.cm-tui-update-*')))

    def test_corrupt_or_failed_transfer_keeps_existing_binary(self):
        def corrupt(args, **kwargs):
            Path(args[-1]).write_bytes(b'incomplete')
        for failure in [corrupt, subprocess.TimeoutExpired('scp', 120),
                        subprocess.CalledProcessError(1, 'scp')]:
            with self.subTest(failure=str(failure)), patch('subprocess.run', side_effect=failure):
                with self.assertRaises((SystemExit, subprocess.SubprocessError)):
                    installer.install(self.release)
            self.assertEqual(self.target.read_bytes(), b'previous executable')
            self.assertFalse(list(self.target.parent.glob('.cm-tui-update-*')))
            self.assertFalse(self.target.with_name('claude-manager-tui.before-1234567').exists())

    def test_cloud_hosts_are_refused_before_download(self):
        for host in ['cm-sessions', 'cm-manager.internal']:
            with self.subTest(host=host), patch('socket.gethostname', return_value=host), \
                    patch('subprocess.run') as transfer:
                with self.assertRaisesRegex(SystemExit, 'local laptop terminal'):
                    installer.install(self.release)
                transfer.assert_not_called()
        self.assertEqual(self.target.read_bytes(), b'previous executable')


if __name__ == '__main__':
    unittest.main()
