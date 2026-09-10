#!/usr/bin/env python3
"""Durable checkout identities and creation links; stdlib-only Git hook payload."""
from __future__ import annotations

import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time
import uuid

MARKER = '# cm-worktree-lineage v1'


def cm_home() -> Path:
    return Path(os.environ.get('CM_LINEAGE_HOME', str(Path.home() / '.cm')))


def atomic_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, name = tempfile.mkstemp(prefix='.' + path.name, dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as stream:
            json.dump(value, stream)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


@contextlib.contextmanager
def locked(path: Path):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with path.open('a') as stream:
        os.chmod(path, 0o600)
        fcntl.flock(stream, fcntl.LOCK_EX)
        yield


def git(path: Path, *args: str) -> str:
    env = {key: value for key, value in os.environ.items() if key not in {'GIT_DIR', 'GIT_COMMON_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE'}}
    result = subprocess.run(['git', '-C', str(path), *args], capture_output=True, text=True, timeout=15, check=True, env=env)
    return result.stdout.strip()


def checkout(path: Path, *, create: bool = True) -> dict:
    """A UUID in Git's per-worktree admin dir detects deletion + path reuse."""
    if path.is_symlink():
        raise ValueError('symlink checkout')
    path = path.resolve(strict=True)
    top = Path(git(path, 'rev-parse', '--show-toplevel')).resolve()
    if top != path:
        raise ValueError('path is not the checkout root')
    admin = Path(git(path, 'rev-parse', '--absolute-git-dir')).resolve()
    common = Path(git(path, 'rev-parse', '--path-format=absolute', '--git-common-dir')).resolve()
    stamp = admin / 'cm-checkout-id'
    # Existing Git directory identity alone can be recycled, so create a new UUID
    # after a worktree has been removed/re-added even at the exact same pathname.
    if create:
        try:
            fd = os.open(stamp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        except FileExistsError:
            pass
        else:
            with os.fdopen(fd, 'w') as stream:
                stream.write(str(uuid.uuid4()) + '\n')
    ident = str(uuid.UUID(stamp.read_text().strip()))
    return {'id': ident, 'path': str(path), 'git_dir': str(admin), 'common_dir': str(common), 'primary': admin == common}


def stored_records() -> dict[str, dict]:
    result = {}
    for p in (cm_home() / 'worktree-lineage').glob('*.json'):
        try:
            value = json.loads(p.read_text())
            if p.stem == str(uuid.UUID(value['id'])):
                result[value['id']] = value
        except (OSError, ValueError, KeyError):
            continue
    return result


def records() -> dict[str, dict]:
    """Validate inventory identities without spawning Git for every stored row."""
    result = {}
    for ident, value in stored_records().items():
        try:
            path = Path(value['path'])
            if path.is_symlink() or not path.is_dir() or str(path.resolve()) != value['path']:
                continue
            dotgit = path / '.git'
            if dotgit.is_symlink():
                continue
            if dotgit.is_dir():
                admin = dotgit.resolve()
            else:
                link = dotgit.read_text().strip()
                if not link.startswith('gitdir: '):
                    continue
                admin = (path / link[8:]).resolve()
            if str(admin) == value['git_dir'] and (admin / 'cm-checkout-id').read_text().strip() == ident:
                result[ident] = value
        except (OSError, ValueError, KeyError):
            continue
    return result


def session_owner(uid: str) -> tuple[set[str], Path | None]:
    """Resolve inherited CM identity from the host's own durable manifest."""
    tasks: set[str] = set()
    parent = None
    try:
        data = json.loads((cm_home() / 'daemon-sessions.json').read_text())
        workspaces = data.get('workspaces', {})
        bindings = data.get('bindings', {})
        for ws in workspaces.values() if isinstance(workspaces, dict) else workspaces:
            for session in ws.get('sessions', []) + ws.get('tombstones', []):
                if session.get('session_uid', session.get('uid')) == uid:
                    if session.get('task_id'):
                        tasks.add(session['task_id'])
                    else:
                        tasks.update(tid for tid, wid in bindings.items() if wid == ws.get('id'))
                    if ws.get('worktree_path'):
                        parent = Path(ws['worktree_path'])
    except (OSError, ValueError, TypeError):
        pass
    return tasks, parent


def register(path: Path, parent: Path | None = None, task_ids=(), source='git-hook', session_uid: str | None = None, inherit_session: bool = True) -> dict:
    child = checkout(path)
    uid = (session_uid or os.environ.get('CM_TUI_SESSION_ID', '')) if inherit_session else ''
    inherited, session_path = session_owner(uid) if uid else (set(), None)
    task_ids = set(task_ids) | inherited
    parent_ids = set()
    for parent_path in {p for p in (parent, session_path) if p is not None}:
        if parent_path.resolve() == path.resolve():
            continue
        try:
            parent_ids.add(register(parent_path, source='parent', inherit_session=False)['id'])
        except (OSError, ValueError, subprocess.SubprocessError):
            pass
    target = cm_home() / 'worktree-lineage' / (child['id'] + '.json')
    with locked(target.with_suffix('.lock')):
        try:
            old = json.loads(target.read_text())
        except FileNotFoundError:
            old = {}
        parents = set(old.get('parents', []))
        parents.update(parent_ids)
        value = {**child, 'parents': sorted(parents), 'task_ids': sorted(set(old.get('task_ids', [])) | task_ids),
                 'session_uids': sorted(set(old.get('session_uids', [])) | ({uid} if uid else set())),
                 'sources': sorted(set(old.get('sources', [])) | {source}), 'created_at': old.get('created_at', time.time())}
        atomic_json(target, value)
    return value


def install_hook(repo: Path) -> dict:
    """Chain the existing hook in place; never change core.hooksPath or bootstrap."""
    hook = Path(git(repo, 'rev-parse', '--path-format=absolute', '--git-path', 'hooks/post-checkout'))
    hook.parent.mkdir(parents=True, exist_ok=True)
    with locked(cm_home() / 'worktree-lineage' / ('hook-' + hashlib.sha256(str(hook).encode()).hexdigest() + '.lock')):
        if hook.is_symlink():
            raise ValueError(f'leaving symlink hook unchanged: {hook}')
        original = hook.read_text() if hook.exists() else ''
        if MARKER in original:
            return {'hook': str(hook), 'installed': True}
        previous = hook.with_name('post-checkout.before-cm-lineage')
        if previous.exists():
            raise ValueError(f'hook was replaced since installation; inspect {hook}')
        executable = hook.exists() and os.access(hook, os.X_OK)
        if hook.exists():
            os.replace(hook, previous)
        script = shlex.quote(str(Path(__file__).resolve()))
        # Run tracking before the pre-existing hook: that hook can exit, exec,
        # change cwd, or create further worktrees. Its exit status is preserved.
        body = f'#!/bin/sh\n{MARKER}\n/usr/bin/python3 {script} hook "$@" >/dev/null 2>&1 || :\n'
        if executable:
            body += f'exec {shlex.quote(str(previous))} "$@"\n'
        else:
            body += 'exit 0\n'
        fd, name = tempfile.mkstemp(prefix='.post-checkout-', dir=hook.parent)
        with os.fdopen(fd, 'w') as stream:
            stream.write(body)
        os.chmod(name, 0o755)
        os.replace(name, hook)
    return {'hook': str(hook), 'installed': True}


def hook(args: list[str]) -> None:
    # post-checkout also fires on branch switches. Record only checkout creation;
    # otherwise an innocent later visit would silently reparent an old checkout.
    if len(args) < 3 or set(args[0]) != {'0'} or args[2] != '1':
        return
    path = Path.cwd()
    parent = None
    pid = os.getppid()
    for _ in range(6):
        try:
            proc = Path('/proc') / str(pid)
            candidate = (proc / 'cwd').resolve(strict=True)
            if candidate != path:
                try:
                    parent = Path(git(candidate, 'rev-parse', '--show-toplevel'))
                    break
                except subprocess.SubprocessError:
                    pass
            stat = (proc / 'stat').read_text().rsplit(')', 1)[1].split()
            pid = int(stat[1])
        except (OSError, ValueError):
            break
    register(path, parent)


if __name__ == '__main__':
    if sys.argv[1:2] == ['hook']:
        hook(sys.argv[2:])
    elif sys.argv[1:2] == ['install']:
        for arg in sys.argv[2:]:
            print(json.dumps(install_hook(Path(arg))))
    elif sys.argv[1:2] == ['register']:
        print(json.dumps(register(Path(sys.argv[2]), Path(sys.argv[3]) if len(sys.argv) > 3 else None, source='cm')))
