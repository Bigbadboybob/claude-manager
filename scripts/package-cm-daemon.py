#!/usr/bin/env python3
"""Build a committed laptop daemon release on cm-sessions; never deploy it."""
import argparse
import ast
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess


REPO = Path(__file__).resolve().parent.parent


def git(*args):
    return subprocess.check_output(['git', '-C', str(REPO), *args], text=True).strip()


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def embed(template, target, release):
    text = template.read_text()
    assert text.count('RELEASE = None') == 1
    text = text.replace('RELEASE = None', 'RELEASE = ' + repr(release))
    ast.parse(text)
    target.write_text(text)
    target.chmod(0o755)


def stage(revision, target, release, with_tui):
    # Exclusive creation preserves every previous release. A failed stage remains
    # labelled incomplete for inspection; it never advertises an install command.
    release.mkdir(exist_ok=False)
    (release / 'OWNER.md').write_text(
        f'session: {os.environ.get("CM_TUI_SESSION_ID", "manual release packager")}\n'
        f'created: {datetime.now(timezone.utc).date()}\n'
        'purpose: Staged laptop daemon release (not installed)\n'
        'keep: binaries, installers, SHA256SUMS and release.json\n'
        'delete-after: release retirement by Owner/coordinator\n')
    receipt = {'commit': revision, 'created_at': datetime.now(timezone.utc).isoformat(),
               'status': 'incomplete staging', 'components': ['cm-daemon', 'cm-holder'],
               'activation': 'brain only; holder is packaged, not upgraded'}
    receipt_path = release / 'release.json'
    receipt_path.write_text(json.dumps(receipt, indent=2) + '\n')
    files = ['cm-daemon', 'cm-holder'] + (['claude-manager-tui'] if with_tui else [])
    for name in files:
        shutil.copy2(target / 'release' / name, release / name)
    artifact = lambda name: {'source': 'cm-sessions:' + str(release / name),
                             'sha256': sha(release / name)}
    data = {'commit': revision, 'daemon': artifact('cm-daemon'),
            'holder': artifact('cm-holder'),
            'installer_source': 'cm-sessions:' + str(release / 'install-laptop-daemon.py')}
    if with_tui:
        embed(REPO / 'scripts/install-cm-tui.py', release / 'install-laptop-tui.py',
              {'commit': revision, **artifact('claude-manager-tui')})
        data['tui_installer'] = artifact('install-laptop-tui.py')
        receipt['components'].append('claude-manager-tui')
    embed(REPO / 'scripts/install-cm-daemon.py', release / 'install-laptop-daemon.py', data)
    receipt.update(status='ready for laptop installation', artifacts=data,
                   build='cargo build --locked --release (private target)',
                   checks='Record review/test evidence separately; packaging does not run tests.')
    receipt_path.write_text(json.dumps(receipt, indent=2) + '\n')
    (release / 'SHA256SUMS').write_text(''.join(
        sha(path) + '  ' + path.name + '\n' for path in sorted(release.iterdir())
        if path.is_file() and path.name != 'SHA256SUMS'))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target-dir', type=Path, required=True, help='Private disposable Cargo target')
    parser.add_argument('--with-tui', action='store_true')
    parser.add_argument('--offline', action='store_true')
    args = parser.parse_args()
    if socket.gethostname().split('.')[0] != 'cm-sessions':
        parser.error('Build and stage on cm-sessions (the installer uses that SSH alias).')
    if git('status', '--porcelain'):
        parser.error('Commit changes and use a clean checkout before packaging.')
    revision = git('rev-parse', 'HEAD')
    target = args.target_dir.expanduser().resolve()
    shared = (Path.home() / '.cm/shared-target').resolve()
    if target == shared or shared in target.parents or str(target).startswith('/tmp/'):
        parser.error('Use a private target on the large /home filesystem, never shared-target or /tmp.')
    release = Path.home() / '.cm/releases' / ('daemon-' + revision[:12])
    if release.exists():
        parser.error(f'Release already exists; keep it unchanged: {release}')
    target.mkdir(parents=True, exist_ok=True)
    if not (target / 'OWNER.md').exists():
        (target / 'OWNER.md').write_text(
            f'session: {os.environ.get("CM_TUI_SESSION_ID", "manual release packager")}\n'
            f'created: {datetime.now(timezone.utc).date()}\n'
            'purpose: Private laptop daemon release build\nkeep: none\n'
            'delete-after: packaging and checks finish\n')
    command = ['cargo', 'build', '--locked', '--release']
    command += ['--workspace'] if args.with_tui else ['-p', 'cm-daemon', '-p', 'cm-holder']
    if args.offline:
        command.append('--offline')
    subprocess.run(command, cwd=REPO, env={**os.environ, 'CARGO_TARGET_DIR': str(target),
                                         'CARGO_BUILD_JOBS': '2'}, check=True)
    if git('rev-parse', 'HEAD') != revision or git('status', '--porcelain'):
        raise SystemExit('Sources changed during build; release not staged. Rebuild the committed revision.')
    stage(revision, target, release, args.with_tui)
    subprocess.run(['sha256sum', '-c', 'SHA256SUMS'], cwd=release, check=True)
    print(f'Ready for laptop installation (not deployed): {release}')
    suffix = ' - --with-tui' if args.with_tui else ''
    print(f"ssh cm-sessions 'cat {release}/install-laptop-daemon.py' | python3{suffix}")


if __name__ == '__main__':
    main()
