#!/usr/bin/env python3
"""Laptop installer checks: disposable files, fake RPCs, no live daemon calls."""
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import struct
import subprocess
import tempfile
import unittest
from unittest.mock import patch


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


installer = module('installer', 'install-cm-daemon.py')
packager = module('packager', 'package-cm-daemon.py')


class InstallerTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.home = Path(temp.name)
        self.target = self.home / '.cm/shared-target/release/cm-daemon'
        self.target.parent.mkdir(parents=True)
        self.target.write_bytes(b'old on-disk executable')
        self.old = self.home / 'running-brain'
        self.old.write_bytes(b'actual pinned old executable')
        self.holder = self.home / 'holder'
        self.holder.write_bytes(b'unchanged holder')
        self.new = b'new brain'
        self.release = {'commit': '123456789abcdef',
                        'daemon': {'source': 'cm-sessions:/release/cm-daemon',
                                   'sha256': hashlib.sha256(self.new).hexdigest()},
                        'installer_source': 'cm-sessions:/release/install-laptop-daemon.py'}
        self.health = dict(split=True, strong_operator_auth=True, breaker_state='running',
                           holder_epoch=12, brain_pid=100, holder_sessions=3, sessions=3, mcp_ok=True)
        self.calls = []
        self.commands = []
        self.enterContext(patch.object(Path, 'home', return_value=self.home))
        self.enterContext(patch.object(installer.socket, 'gethostname', return_value='owner-laptop'))
        self.enterContext(patch.object(installer.platform, 'system', return_value='Linux'))
        self.enterContext(patch.object(installer.platform, 'machine', return_value='x86_64'))
        self.enterContext(patch.object(installer.os, 'geteuid', return_value=1000))
        self.enterContext(patch.object(installer, 'process', side_effect=self.process))
        self.rpc = self.enterContext(patch.object(installer, 'rpc', side_effect=self.call))
        self.run = self.enterContext(patch.object(installer.subprocess, 'run', side_effect=self.command))
        self.output = io.StringIO()
        self.enterContext(contextlib.redirect_stdout(self.output))
        self.enterContext(contextlib.redirect_stderr(self.output))

    def process(self, pid):
        if pid == 90:
            return {'pid': 90, 'start': '222', 'image': self.holder, 'path': self.holder, 'parent': 1}
        return {'pid': pid, 'start': str(pid), 'image': self.old if pid == 100 else self.target,
                'path': self.target, 'parent': 90}

    def call(self, method, params, **kwargs):
        self.calls.append((method, params))
        if method == 'daemon.restart':
            self.assertEqual(params, {'binary_path': str(self.target)})
            self.health['holder_epoch'] += 1
            self.health['brain_pid'] += 1
            raise ConnectionResetError('exec EOF')
        return self.health.copy()

    def command(self, args, **kwargs):
        self.commands.append(args)
        if args[0] == 'scp':
            Path(args[-1]).write_bytes(b'fake TUI installer' if args[-2].endswith('tui.py') else self.new)
        return subprocess.CompletedProcess(args, 0)

    def install(self, **kwargs):
        installer.install(self.release, soak=0, **kwargs)

    def backup(self):
        return self.target.with_name('cm-daemon.before-123456789abc')

    def test_activate_repeat_and_rollback_preserve_original_pinned_image(self):
        self.install()
        self.assertEqual(self.target.read_bytes(), self.new)
        self.assertEqual(self.backup().read_bytes(), self.old.read_bytes())
        self.install()
        self.assertEqual(self.backup().read_bytes(), self.old.read_bytes())
        self.install(rollback=True)
        self.assertEqual(self.target.read_bytes(), self.old.read_bytes())
        self.assertEqual(self.health['holder_epoch'], 15)
        self.assertEqual(self.target.stat().st_mode & 0o777, 0o755)
        self.assertFalse(list(self.target.parent.glob('.cm-daemon-update-*')))
        self.assertIn('--rollback --binary-path', self.output.getvalue())

    def test_checksum_transfer_and_preflight_fail_before_replacement(self):
        def preflight_failure(args, **kwargs):
            if args[-1] == '--daemon-preflight':
                raise subprocess.CalledProcessError(1, args)
            return self.command(args, **kwargs)
        for failure in ['checksum', 'transfer', 'preflight']:
            with self.subTest(failure=failure):
                if failure == 'checksum':
                    self.new = b'corrupt'
                    self.run.side_effect = self.command
                elif failure == 'transfer':
                    self.run.side_effect = subprocess.TimeoutExpired('scp', 120)
                else:
                    self.new = b'new brain'
                    self.run.side_effect = preflight_failure
                with self.assertRaises((installer.InstallError, subprocess.SubprocessError)):
                    self.install()
                self.assertEqual(self.target.read_bytes(), b'old on-disk executable')
                self.assertFalse(self.backup().exists())
                self.assertFalse(any(m == 'daemon.restart' for m, _ in self.calls))

    def test_cloud_monolith_unauthorized_and_inflight_restart_refused(self):
        for host in ['cm-sessions', 'cm-manager.internal']:
            with self.subTest(host=host), patch.object(installer.socket, 'gethostname', return_value=host):
                with self.assertRaisesRegex(installer.InstallError, 'local laptop terminal'):
                    self.install()
        for key, value in [('split', False), ('strong_operator_auth', False), ('restarting', True),
                           ('breaker_state', 'held_down'), ('mcp_ok', False)]:
            with self.subTest(key=key), patch.dict(self.health, {key: value}):
                with self.assertRaises(installer.InstallError):
                    self.install()
        self.run.assert_not_called()

    def test_epoch_only_is_not_proof_wrong_running_executable_fails(self):
        original = self.process
        def wrong(pid):
            result = original(pid)
            if pid == 101:
                result['image'] = self.old
            return result
        with patch.object(installer, 'process', side_effect=wrong):
            with self.assertRaisesRegex(installer.InstallError, 'checksum does not match'):
                self.install()
        self.assertNotIn('Laptop brain activated:', self.output.getvalue())
        self.assertIn('Activation was not verified', self.output.getvalue())

    def test_restart_refusal_retains_backup_and_never_claims_activation(self):
        def refuse(method, params, **kwargs):
            if method == 'daemon.restart':
                raise installer.RpcError('restart_busy')
            return self.call(method, params, **kwargs)
        self.rpc.side_effect = refuse
        with self.assertRaisesRegex(installer.RpcError, 'restart_busy'):
            self.install()
        self.assertEqual(self.backup().read_bytes(), self.old.read_bytes())
        self.assertNotIn('Laptop brain activated:', self.output.getvalue())

    def test_concurrent_generation_change_prevents_replacement(self):
        def race(args, **kwargs):
            if args[-1] == '--daemon-preflight':
                self.health['holder_epoch'] += 1
            return self.command(args, **kwargs)
        self.run.side_effect = race
        with self.assertRaisesRegex(installer.InstallError, 'Brain changed'):
            self.install()
        self.assertEqual(self.target.read_bytes(), b'old on-disk executable')

    def test_combined_tui_runs_only_after_verified_brain(self):
        self.release['tui_installer'] = {'source': 'cm-sessions:/release/tui.py',
            'sha256': hashlib.sha256(b'fake TUI installer').hexdigest()}
        def command(args, **kwargs):
            if args[0] == installer.sys.executable:
                self.assertEqual(self.health['holder_epoch'], 13)
                self.assertIn('Laptop brain activated:', self.output.getvalue())
            return self.command(args, **kwargs)
        self.run.side_effect = command
        self.install(with_tui=True)
        self.assertEqual(self.commands[-1][0], installer.sys.executable)

    def test_unwritable_pin_requires_explicit_sudo(self):
        with patch.object(installer.os, 'access', return_value=False):
            with self.assertRaisesRegex(installer.InstallError, 'sudo -v'):
                self.install()
        self.run.assert_not_called()

    def test_opt_pin_and_override_do_not_assume_shared_target(self):
        opt = self.home / 'opt/cm-daemon/cm-daemon'
        opt.parent.mkdir(parents=True)
        self.target = opt
        self.target.write_bytes(b'old opt binary')
        self.install()
        self.assertEqual(opt.read_bytes(), self.new)
        self.assertEqual(self.calls[-2], ('daemon.restart', {'binary_path': str(opt)}))

    def test_atomic_sudo_install_uses_sibling_rename(self):
        source = self.old
        target = self.home / 'opt/cm-daemon'
        installer.atomic_copy(source, target, sudo=True)
        install, move, cleanup = self.commands
        self.assertEqual(install[:4], ['sudo', '-n', 'install', '-m0755'])
        self.assertEqual(Path(install[-1]).parent, target.parent)
        self.assertEqual(move[:4], ['sudo', '-n', 'mv', '-T'])
        self.assertEqual(move[-2:], [install[-1], str(target)])
        self.assertEqual(cleanup[-1], install[-1])

    def test_tampered_backup_is_refused(self):
        self.install()
        self.backup().write_bytes(b'changed backup')
        with self.assertRaisesRegex(installer.InstallError, 'checksum mismatch'):
            self.install(rollback=True)
        self.assertEqual(self.health['holder_epoch'], 13)

    def test_verifier_detects_extra_epoch_and_holder_replacement(self):
        before = self.health.copy()
        holder = self.process(90)
        holder['sha256'] = installer.digest(self.holder)
        self.health['holder_epoch'] += 2
        with self.assertRaisesRegex(installer.InstallError, 'more than once'):
            installer.verify(before, holder, self.release['daemon']['sha256'], soak=0)
        self.health['holder_epoch'] -= 1
        holder['start'] = 'wrong holder'
        with self.assertRaisesRegex(installer.InstallError, 'Holder identity changed'):
            installer.verify(before, holder, self.release['daemon']['sha256'], soak=0)

    def test_verifier_times_out_without_restart(self):
        before = self.health.copy()
        with patch.object(installer.time, 'monotonic', side_effect=[0, 121]):
            with self.assertRaisesRegex(installer.InstallError, 'timed out'):
                installer.verify(before, {}, 'unused', soak=0)


class TransportAndPackageTests(unittest.TestCase):
    def test_fragmented_rpc_uses_laptop_token_and_socket(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / '.cm').mkdir()
            (home / '.cm/operator-token').write_text('test-only-token')
            result = json.dumps({'ok': True, 'result': {'holder_epoch': 1}}).encode()
            frame = struct.pack('>I', len(result)) + result
            with patch.object(Path, 'home', return_value=home), \
                    patch.object(installer.socket, 'socket') as sock:
                connection = sock.return_value.__enter__.return_value
                connection.recv.side_effect = [bytes([b]) for b in frame]
                self.assertEqual(installer.rpc('daemon.health', {}), {'holder_epoch': 1})
                connection.connect.assert_called_once_with(str(home / '.cm/daemon.sock'))
                wire = connection.sendall.call_args.args[0]
                self.assertEqual(json.loads(wire[4:])['caller'], {'token_id': 'test-only-token'})

    def test_stage_embeds_checked_artifacts_and_refuses_existing_release(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            target = root / 'target/release'
            target.mkdir(parents=True)
            for name in ['cm-daemon', 'cm-holder', 'claude-manager-tui']:
                (target / name).write_bytes(name.encode())
            release = root / 'daemon-123'
            packager.stage('123456789abcdef', target.parent, release, True)
            for line in (release / 'SHA256SUMS').read_text().splitlines():
                checksum, filename = line.split('  ')
                self.assertEqual(packager.sha(release / filename), checksum)
            namespace = {'__name__': 'test'}
            exec(compile((release / 'install-laptop-daemon.py').read_text(), '<installer>', 'exec'), namespace)
            data = namespace['RELEASE']
            self.assertEqual(data['daemon']['sha256'], packager.sha(release / 'cm-daemon'))
            self.assertEqual(data['tui_installer']['sha256'], packager.sha(release / 'install-laptop-tui.py'))
            self.assertEqual(json.loads((release / 'release.json').read_text())['status'], 'ready for laptop installation')
            with self.assertRaises(FileExistsError):
                packager.stage('123456789abcdef', target.parent, release, True)


if __name__ == '__main__':
    unittest.main()
