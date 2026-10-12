#!/usr/bin/env python3
"""Standalone laptop brain installer template; see doc/TUI_RELEASES.md."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid


RELEASE = None


class InstallError(RuntimeError):
    pass


class RpcError(InstallError):
    pass


def digest(path):
    with Path(path).open('rb') as stream:
        checksum = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            checksum.update(chunk)
        return checksum.hexdigest()


def recv_exact(sock, length):
    data = bytearray()
    while len(data) < length:
        part = sock.recv(length - len(data))
        if not part:
            raise ConnectionError('daemon closed the connection')
        data.extend(part)
    return data


def rpc(method, params, timeout=5):
    # Always the laptop's local socket and token, never a cloud endpoint inherited
    # through CM_DAEMON_SOCKET / CM_OPERATOR_TOKEN in a shell environment.
    home = Path.home() / '.cm'
    token = (home / 'operator-token').read_text().strip()
    if not token:
        raise InstallError('The laptop operator token is empty.')
    request = json.dumps({'id': 'laptop-daemon-install', 'caller': {'token_id': token},
                          'method': method, 'params': params}).encode()
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(timeout)
        sock.connect(str(home / 'daemon.sock'))
        sock.sendall(struct.pack('>I', len(request)) + request)
        length = struct.unpack('>I', recv_exact(sock, 4))[0]
        if length > 8 * 1024 * 1024:
            raise InstallError('Oversized daemon response.')
        reply = json.loads(recv_exact(sock, length))
    if not reply.get('ok') or reply.get('error'):
        raise RpcError(f'{method} refused: {reply.get("error")}')
    return reply['result']


def process(pid):
    """Identity and executable of a Linux process, including a deleted pin."""
    root = Path('/proc') / str(pid)
    fields = (root / 'stat').read_text().rsplit(')', 1)[1].split()
    exe = os.readlink(root / 'exe').removesuffix(' (deleted)')
    return {'pid': pid, 'parent': int(fields[1]), 'start': fields[19],
            'path': Path(exe), 'image': root / 'exe'}


def preflight_health():
    health = rpc('daemon.health', {})
    if (health.get('split') is not True or health.get('strong_operator_auth') is not True
            or health.get('breaker_state') != 'running' or health.get('holder_status_error')
            or health.get('restarting') or health.get('draining')
            or health.get('mcp_ok') is not True
            or not isinstance(health.get('holder_epoch'), int)
            or not isinstance(health.get('brain_pid'), int)
            or health.get('sessions') != health.get('holder_sessions')):
        raise InstallError('Laptop daemon is not a healthy, operator-authenticated holder/brain split. '
                           'Inspect daemon.health before installing; no hard restart is attempted.')
    brain = process(health['brain_pid'])
    holder = process(brain['parent'])
    holder['sha256'] = digest(holder['image'])
    return health, brain, holder


def download(artifact, target):
    subprocess.run(['scp', '-q', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10',
                    artifact['source'], str(target)], check=True, timeout=120)
    if digest(target) != artifact['sha256']:
        raise InstallError('Downloaded checksum mismatch; installed binaries kept unchanged.')
    target.chmod(0o755)


def atomic_copy(source, target, sudo=False):
    """Replace the inode, including /opt installs; never truncate a live image."""
    tmp = target.with_name('.cm-daemon-update-' + uuid.uuid4().hex)
    try:
        if sudo:
            subprocess.run(['sudo', '-n', 'install', '-m0755', '--', str(source), str(tmp)], check=True)
            subprocess.run(['sudo', '-n', 'mv', '-T', '--', str(tmp), str(target)], check=True)
        else:
            with tmp.open('xb') as dst, Path(source).open('rb') as src:
                shutil.copyfileobj(src, dst)
                dst.flush()
                os.fsync(dst.fileno())
            tmp.chmod(0o755)
            os.replace(tmp, target)
    finally:
        if sudo:
            subprocess.run(['sudo', '-n', 'rm', '-f', '--', str(tmp)], check=False)
        else:
            tmp.unlink(missing_ok=True)


def verify(before, holder, expected, timeout=120, soak=90):
    deadline = time.monotonic() + timeout
    stable_since = None
    while True:
        try:
            after = rpc('daemon.health', {})
        except (OSError, ConnectionError):
            after = None  # socket closure/refusal is never itself proof of success
        now = time.monotonic()
        if after and after.get('holder_epoch', 0) > before['holder_epoch']:
            if after['holder_epoch'] != before['holder_epoch'] + 1:
                raise InstallError('Holder epoch advanced more than once: crash/rollback suspected.')
            brain = process(after['brain_pid'])
            parent = process(brain['parent'])
            if ((parent['pid'], parent['start']) != (holder['pid'], holder['start'])
                    or digest(parent['image']) != holder['sha256']):
                raise InstallError('Holder identity changed during the brain restart.')
            if after['brain_pid'] == before['brain_pid'] or digest(brain['image']) != expected:
                raise InstallError('Running brain executable checksum does not match the release.')
            if (after.get('breaker_state') != 'running' or after.get('holder_status_error')
                    or after.get('split') is not True):
                raise InstallError('Holder is not healthy after restart.')
            ready = (after.get('mcp_ok') is True
                     and after.get('sessions') == before['sessions']
                     and after.get('holder_sessions') == before['holder_sessions'])
            if ready:
                if stable_since is None:
                    stable_since = now
                    print(f'Brain checksum verified; epoch {before["holder_epoch"]} → '
                          f'{after["holder_epoch"]}. Checking stability for {soak:g}s…', flush=True)
                if now - stable_since >= soak:
                    return after
            else:
                stable_since = None
        elif stable_since is not None:
            raise InstallError('Lost the verified brain during the stability check.')
        if now >= deadline + (soak if stable_since is not None else 0):
            raise InstallError('Restart verification timed out; activation is unconfirmed.')
        time.sleep(1)


def rollback_line(release, target, sudo):
    host, source = release['installer_source'].split(':', 1)
    args = ['python3', '-', '--rollback', '--binary-path', str(target)]
    if sudo:
        args.append('--sudo')
    return shlex.join(['ssh', host, 'cat ' + shlex.quote(source)]) + ' | ' + shlex.join(args)


def install(release, *, binary_path=None, sudo=False, rollback=False, with_tui=False, soak=90):
    if socket.gethostname().split('.')[0] in {'cm-sessions', 'cm-manager'}:
        raise InstallError('Run this in a local laptop terminal, outside a CM cloud Bash session.')
    if platform.system() != 'Linux' or platform.machine() not in {'x86_64', 'amd64'}:
        raise InstallError('This release requires Linux x86-64.')
    if with_tui and (rollback or not release.get('tui_installer')):
        raise InstallError('--with-tui requires a combined release and cannot accompany --rollback.')
    if os.geteuid() == 0:
        raise InstallError('Run as the laptop user; use --sudo for a root-owned /opt destination.')
    lock_path = Path.home() / '.cm/laptop-daemon-install.lock'
    with lock_path.open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise InstallError('Another laptop daemon install is running.') from None
        before, brain, holder = preflight_health()
        target = Path(binary_path).expanduser() if binary_path else brain['path']
        if not target.is_absolute() or not target.parent.is_dir() or target.is_symlink():
            raise InstallError(f'Expected an absolute binary path in an existing directory: {target}')
        if target.name != 'cm-daemon' or str(target).startswith(('/proc/', '/dev/')):
            raise InstallError('Cannot infer a durable cm-daemon path; specify --binary-path explicitly.')
        if sudo:
            subprocess.run(['sudo', '-n', 'true'], check=True)
        elif not os.access(target.parent, os.W_OK):
            raise InstallError(f'{target.parent} is not writable. Run sudo -v, then retry with --sudo '
                               '(do not run the whole installer as root).')
        backup = target.with_name('cm-daemon.before-' + release['commit'][:12])
        backup_sum = backup.with_name(backup.name + '.sha256')
        if backup.is_symlink() or backup_sum.is_symlink():
            raise InstallError('Refusing a symlink at the rollback backup path.')
        print(f'Laptop brain: {brain["path"]}; install destination: {target}', flush=True)
        print('Rollback command: ' + rollback_line(release, target, sudo), flush=True)
        with tempfile.TemporaryDirectory(prefix='cm-laptop-daemon-') as scratch:
            scratch = Path(scratch)
            candidate = scratch / 'cm-daemon'
            tui = scratch / 'install-tui.py'
            if rollback:
                if not backup.is_file() or not backup_sum.is_file():
                    raise InstallError(f'Rollback backup/checksum missing: {backup}')
                expected = backup_sum.read_text().strip()
                shutil.copyfile(backup, candidate)
                candidate.chmod(0o755)
                if digest(candidate) != expected:
                    raise InstallError('Rollback backup checksum mismatch.')
            else:
                download(release['daemon'], candidate)
                expected = release['daemon']['sha256']
                if with_tui:
                    download(release['tui_installer'], tui)
            subprocess.run([str(candidate), '--daemon-preflight'], check=True, timeout=60)
            # Refuse an intervening deployment; back up the actual old pinned image,
            # which can differ from the old pathname's contents after a failed deploy.
            check = rpc('daemon.health', {})
            if (check.get('holder_epoch'), check.get('brain_pid')) != (before['holder_epoch'], before['brain_pid']):
                raise InstallError('Brain changed during download/preflight; retry from fresh health.')
            if not rollback:
                if backup.exists() != backup_sum.exists():
                    raise InstallError('Incomplete rollback backup; inspect it before retrying.')
                if backup.exists():
                    if digest(backup) != backup_sum.read_text().strip():
                        raise InstallError('Existing rollback backup checksum mismatch.')
                else:
                    old = scratch / 'previous-brain'
                    shutil.copyfile(brain['image'], old)
                    checksum = scratch / 'backup.sha256'
                    checksum.write_text(digest(old) + '\n')
                    atomic_copy(old, backup, sudo)
                    atomic_copy(checksum, backup_sum, sudo)
            atomic_copy(candidate, target, sudo)
            print(f'Binary installed at {target}; requesting brain restart…', flush=True)
            try:
                try:
                    rpc('daemon.restart', {'binary_path': str(target)}, timeout=120)
                except (ConnectionError, TimeoutError, OSError) as exc:
                    print(f'Restart transport ended ({type(exc).__name__}); verifying activation.', flush=True)
                after = verify(before, holder, expected, soak=soak)
            except Exception:
                print('Activation was not verified. Installed binary and rollback backup remain.\n'
                      'Rollback command: ' + rollback_line(release, target, sudo), file=sys.stderr)
                raise
            print(f'Laptop brain activated: epoch {after["holder_epoch"]}, SHA256 {expected}; '
                  f'{after["sessions"]} sessions retained. Holder unchanged.', flush=True)
            print('Initial stability check passed. The holder breaker horizon is 10 minutes; '
                  'continue watching for another epoch change.', flush=True)
            if with_tui:
                # A separate component transaction. Its failure must not imply that
                # the already verified brain was rolled back.
                print('Brain activated; installing the bundled TUI next.', flush=True)
                subprocess.run([sys.executable, str(tui)], check=True, timeout=180)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary-path', help='Override the running brain image path')
    parser.add_argument('--sudo', action='store_true', help='Use pre-authorized sudo for /opt file installation only')
    parser.add_argument('--rollback', action='store_true', help='Restore this release\'s original brain backup and verify activation')
    parser.add_argument('--with-tui', action='store_true', help='Install the bundled TUI after verifying the brain restart')
    parser.add_argument('--soak-seconds', type=int, default=90)
    args = parser.parse_args()
    if RELEASE is None:
        parser.error('Package this template with scripts/package-cm-daemon.py first.')
    if args.soak_seconds < 0:
        parser.error('--soak-seconds must be nonnegative')
    try:
        install(RELEASE, binary_path=args.binary_path, sudo=args.sudo, rollback=args.rollback,
                with_tui=args.with_tui, soak=args.soak_seconds)
    except (InstallError, OSError, ValueError, subprocess.SubprocessError) as exc:
        raise SystemExit(str(exc)) from None


if __name__ == '__main__':
    main()
