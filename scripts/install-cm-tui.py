#!/usr/bin/env python3
"""Laptop installer template; package it using doc/TUI_RELEASES.md."""
from pathlib import Path
import hashlib
import os
import shutil
import socket
import subprocess
import tempfile


RELEASE = None


def install(release):
    if socket.gethostname().split('.')[0] in {'cm-sessions', 'cm-manager'}:
        raise SystemExit('Run this in a local laptop terminal, outside a CM cloud Bash session.')
    target = Path.home() / '.cm/shared-target/release/claude-manager-tui'
    if not target.is_file():
        raise SystemExit(f'Expected laptop TUI is missing: {target}')
    fd, name = tempfile.mkstemp(prefix='.cm-tui-update-', dir=target.parent)
    os.close(fd)
    new = Path(name)
    try:
        subprocess.run([
            'scp', '-q', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10',
            release['source'], str(new),
        ], check=True, timeout=120)
        if hashlib.sha256(new.read_bytes()).hexdigest() != release['sha256']:
            raise SystemExit('Downloaded binary checksum mismatch; current TUI kept unchanged.')
        new.chmod(0o755)
        short = release['commit'][:7]
        backup = target.with_name(f'claude-manager-tui.before-{short}')
        if not backup.exists():
            shutil.copy2(target, backup)
        os.replace(new, target)
        print(f'Installed CM TUI {short}.')
        print('Close and reopen the TUI to activate it; cloud daemons and sessions were not restarted.')
        print('Rollback binary:', backup)
    finally:
        new.unlink(missing_ok=True)


if __name__ == '__main__':
    if RELEASE is None:
        raise SystemExit('Package this installer with doc/TUI_RELEASES.md before distributing it.')
    install(RELEASE)
