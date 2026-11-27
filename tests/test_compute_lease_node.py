import importlib.machinery
import importlib.util
import json
import subprocess
from types import SimpleNamespace

import pytest

loader = importlib.machinery.SourceFileLoader('cm_lease_node', 'scripts/cm-lease')
spec = importlib.util.spec_from_loader(loader.name, loader)
node = importlib.util.module_from_spec(spec)
loader.exec_module(node)
REAL_CREATE_WORKDIR = node.create_workdir
REAL_CLEANUP_WORKDIR = node.workdir_size_and_remove_if_empty
REAL_REFRESH_OWNER = node.refresh_retained_owner

LEASE = '21da6f57-0d54-4e75-a19d-4a9faab79aaa'


def test_slice_unit_has_no_uuid_hierarchy():
    name = node.unit(LEASE)
    assert name == 'cmlease-21da6f570d544e75a19d4a9faab79aaa.slice'
    assert '-' not in name.removeprefix('cmlease-')


@pytest.fixture
def context(tmp_path, monkeypatch):
    state = tmp_path / 'state'
    state.mkdir()
    work = tmp_path / 'work'
    work.mkdir()
    monkeypatch.setattr(node, 'STATE', state)
    monkeypatch.setattr(node, 'LOG', tmp_path / 'audit.log')
    monkeypatch.setattr(node.time, 'time', lambda: 1000)
    monkeypatch.setattr(node.os, 'chown', lambda *args: None)
    monkeypatch.setattr(node.shutil, 'disk_usage', lambda path: SimpleNamespace(free=100 * 1024 ** 3))
    monkeypatch.setattr(node.pwd, 'getpwnam', lambda _: SimpleNamespace(pw_uid=1, pw_gid=1))
    def make_workdir(path, owner, ident, expires, user):
        path.mkdir()
        (path / 'OWNER.md').write_text(owner)
        return {'device': 1, 'inode': 2, 'sha256': 'test'}
    monkeypatch.setattr(node, 'create_workdir', make_workdir)
    monkeypatch.setattr(node, 'workdir_size_and_remove_if_empty', lambda path, user, marker: 1)
    monkeypatch.setattr(node, 'refresh_retained_owner', lambda *args: None)
    calls = []
    monkeypatch.setattr(node, 'run', lambda *args: calls.append(args))
    cfg = dict(user='lucas', max_slots=4, cpus_per_slot=2, mem_gb_per_slot=16,
               max_duration_seconds=3600, workdir_root=str(work))
    args = SimpleNamespace(lease_id=LEASE, owner='agent:daemon:session', slots=1,
                           mem_gb=16, pin=None, expires_epoch=2000)
    return cfg, args, calls


@pytest.mark.parametrize('key,bad', [('min_free_gb', 0), ('min_free_gb', True),
                                      ('owner_dir_cap_gb', '20'), ('owner_dir_cap_gb', -1)])
def test_invalid_disk_policy_refuses_grant_but_not_release(context, key, bad):
    cfg, args, _ = context
    node.grant(args, cfg)
    cfg[key] = bad
    other = SimpleNamespace(**vars(args))
    other.lease_id = '00000000-0000-4000-8000-000000000001'
    with pytest.raises(node.Refused, match=key):
        node.grant(other, cfg)
    assert node.release(LEASE, cfg)['released']


def test_grant_refuses_low_free_disk(context, monkeypatch):
    cfg, args, _ = context
    monkeypatch.setattr(node.shutil, 'disk_usage', lambda path: SimpleNamespace(free=14 * 1024 ** 3))
    with pytest.raises(node.Refused, match='insufficient free space'):
        node.grant(args, cfg)
    assert node.load(LEASE) is None


def test_grant_is_idempotent_and_loose_limits_memory_only(context):
    cfg, args, calls = context
    first = node.grant(args, cfg)
    second = node.grant(args, cfg)
    assert first == second
    properties = [call for call in calls if 'set-property' in call]
    assert len(properties) == 2
    assert all(call[4:] == ('MemoryMax=16G',) for call in properties)


def test_interrupted_grant_remains_reserved_and_is_reconfigured(context, monkeypatch):
    cfg, args, calls = context
    def fail(*args):
        raise OSError('lost reply')
    monkeypatch.setattr(node, 'run', fail)
    with pytest.raises(OSError):
        node.grant(args, cfg)
    assert not node.load(LEASE).get('configured')
    monkeypatch.setattr(node, 'run', lambda *args: calls.append(args))
    assert node.grant(args, cfg)['configured']
    assert any('set-property' in call for call in calls)


def test_pinned_grant_and_release_address_only_lease_slice(context):
    cfg, args, calls = context
    args.pin = '0,8'
    node.grant(args, cfg)
    properties = next(call for call in calls if 'set-property' in call)
    assert properties[4:] == ('MemoryMax=16G', 'AllowedCPUs=0,8')
    result = node.release(LEASE, cfg)
    assert result['released']
    assert calls[-1] == ('/usr/bin/systemctl', 'stop', node.unit(LEASE))
    assert (node.Path(cfg['workdir_root']) / LEASE / 'OWNER.md').exists()
    assert node.release(LEASE, cfg)['already_absent']


def test_grant_rejects_conflicting_duplicate_and_invalid_cpu_count(context):
    cfg, args, calls = context
    args.pin = '0'
    with pytest.raises(node.Refused):
        node.grant(args, cfg)
    args.pin = '0,8'
    node.grant(args, cfg)
    args.mem_gb = 8
    with pytest.raises(node.Refused):
        node.grant(args, cfg)


def test_expired_grant_retry_releases_the_node_reservation(context, monkeypatch):
    cfg, args, calls = context
    node.grant(args, cfg)
    monkeypatch.setattr(node.time, 'time', lambda: 2001)
    result = node.grant(args, cfg)
    assert result['lease_id'] == LEASE and result['expired'] and result['released']
    assert node.load(LEASE) is None
    assert calls[-1] == ('/usr/bin/systemctl', 'stop', node.unit(LEASE))


def test_workdir_creation_drops_privileges_before_touching_user_path(monkeypatch, tmp_path):
    calls = []
    monkeypatch.setattr(node, 'run', lambda *args: calls.append(args) or '{"device": 1, "inode": 2, "sha256": "x"}')
    marker = node.create_workdir(tmp_path / LEASE, 'agent:daemon:owner', LEASE, 2000, 'lucas')
    assert calls[0][:5] == ('/usr/sbin/runuser', '-u', 'lucas', '--', '/usr/bin/python3')
    assert marker['inode'] == 2
    assert not (tmp_path / LEASE).exists()


def _use_local_workdir_commands(monkeypatch):
    """Execute the runuser payload as this test user, within pytest's tmpdir."""
    calls = []
    def run(*args):
        calls.append(args)
        if args[0] == '/usr/sbin/runuser':
            completed = subprocess.run(args[4:], check=True, text=True, capture_output=True)
            return completed.stdout.strip()
        assert args[:2] in (('/usr/bin/systemctl', 'start'), ('/usr/bin/systemctl', 'stop'),
                            ('/usr/bin/systemctl', 'set-property'))
        return ''
    monkeypatch.setattr(node, 'create_workdir', REAL_CREATE_WORKDIR)
    monkeypatch.setattr(node, 'workdir_size_and_remove_if_empty', REAL_CLEANUP_WORKDIR)
    monkeypatch.setattr(node, 'refresh_retained_owner', REAL_REFRESH_OWNER)
    monkeypatch.setattr(node, 'run', run)
    return calls


def test_release_removes_unchanged_generated_owner_file_and_empty_directory(context, monkeypatch):
    cfg, args, _ = context
    calls = _use_local_workdir_commands(monkeypatch)
    row = node.grant(args, cfg)
    path = node.Path(row['workdir'])
    assert (path / 'OWNER.md').is_file()
    assert row['owner_marker']['inode'] == (path / 'OWNER.md').stat().st_ino
    result = node.release(LEASE, cfg)
    assert result['released'] and result['workdir_bytes'] > 0
    assert not path.exists()
    assert ('/usr/bin/systemctl', 'stop', node.unit(LEASE)) in calls


def test_release_preserves_user_files_and_generated_marker(context, monkeypatch):
    cfg, args, _ = context
    _use_local_workdir_commands(monkeypatch)
    row = node.grant(args, cfg)
    path = node.Path(row['workdir'])
    (path / 'results.txt').write_text('keep this')
    result = node.release(LEASE, cfg)
    assert result['released'] and result['workdir_bytes'] > 0
    assert (path / 'results.txt').read_text() == 'keep this'
    assert (path / 'OWNER.md').is_file()


def test_release_recreates_deleted_owner_marker_for_retained_files(context, monkeypatch):
    cfg, args, _ = context
    _use_local_workdir_commands(monkeypatch)
    row = node.grant(args, cfg)
    path = node.Path(row['workdir'])
    (path / 'OWNER.md').unlink()
    (path / 'result').write_text('keep')
    result = node.release(LEASE, cfg)
    assert result['released'] and 'cleanup_skipped' not in result
    marker = (path / 'OWNER.md').read_text()
    assert args.owner in marker and LEASE in marker and 'delete-after:' in marker
    assert (path / 'result').read_text() == 'keep'


@pytest.mark.parametrize('change', ['edited', 'replaced', 'symlink', 'no_marker'])
def test_release_preserves_unrecognized_owner_file(context, monkeypatch, tmp_path, change):
    cfg, args, _ = context
    _use_local_workdir_commands(monkeypatch)
    row = node.grant(args, cfg)
    path = node.Path(row['workdir'])
    owner = path / 'OWNER.md'
    original = owner.read_bytes()
    if change == 'edited':
        owner.write_text('my own notes')
    elif change == 'replaced':
        replacement = path / 'replacement'
        replacement.write_bytes(original)
        replacement.replace(owner)
    elif change == 'symlink':
        external = tmp_path / 'external'
        external.write_text('external data')
        owner.unlink()
        owner.symlink_to(external)
    else:
        saved = node.load(LEASE)
        saved.pop('owner_marker')
        node.save(LEASE, saved)
    result = node.release(LEASE, cfg)
    assert result['released']
    assert owner.exists()
    if change == 'symlink':
        assert external.read_text() == 'external data'


@pytest.mark.parametrize('exit_code', [0, 137])
def test_scope_resets_failed_after_job_exit_without_changing_result(monkeypatch, exit_code):
    calls = []
    class Process:
        code = None
        def poll(self): return self.code
        def wait(self):
            self.code = exit_code
            return self.code
    def run(args, **kwargs):
        calls.append((args, kwargs))
        return SimpleNamespace(returncode=0)
    monkeypatch.setattr(node.subprocess, 'run', run)
    monkeypatch.setattr(node.fcntl, 'flock', lambda *args: None)
    scope = 'cmlease-job-123.scope'
    assert node.wait_for_scope(Process(), scope, object()) == exit_code
    assert calls[-1][0] == ['/usr/bin/systemctl', 'reset-failed', scope]
    assert calls[-1][1]['check'] is False


def test_scope_reset_failure_does_not_replace_job_exit_code(monkeypatch):
    class Process:
        code = None
        def poll(self): return self.code
        def wait(self):
            self.code = 137
            return self.code
    def run(args, **kwargs):
        if args[1] == 'reset-failed':
            raise OSError('systemctl unavailable')
        return SimpleNamespace(returncode=0)
    monkeypatch.setattr(node.subprocess, 'run', run)
    monkeypatch.setattr(node.fcntl, 'flock', lambda *args: None)
    assert node.wait_for_scope(Process(), 'cmlease-job-123.scope', object()) == 137


def test_scope_timeout_stops_and_resets_failed_unit(monkeypatch):
    calls = []
    class Process:
        code = None
        def poll(self): return self.code
        def terminate(self): pass
        def wait(self):
            self.code = -15
            return self.code
    def run(args, **kwargs):
        calls.append(args)
        return SimpleNamespace(returncode=1)
    times = iter([0, 31])
    monkeypatch.setattr(node, 'time', SimpleNamespace(monotonic=lambda: next(times)))
    monkeypatch.setattr(node.subprocess, 'run', run)
    with pytest.raises(node.Refused, match='scope did not become active'):
        node.wait_for_scope(Process(), 'cmlease-job-123.scope', object())
    assert calls[-2:] == [['/usr/bin/systemctl', 'stop', 'cmlease-job-123.scope'],
                          ['/usr/bin/systemctl', 'reset-failed', 'cmlease-job-123.scope']]


@pytest.mark.parametrize('pin', [None, '0,8'])
def test_run_scope_requests_collection_on_success_or_failure(context, monkeypatch, pin):
    cfg, args, _ = context
    args.pin = pin
    row = node.grant(args, cfg)
    state = node.STATE
    launched = []

    class RootOwnedState:
        def mkdir(self, **kwargs): return state.mkdir(**kwargs)
        def lstat(self): return SimpleNamespace(st_uid=0, st_mode=0o700)
        def is_symlink(self): return False
        def __truediv__(self, name): return state / name

    process = object()
    monkeypatch.setattr(node, 'STATE', RootOwnedState())
    monkeypatch.setattr(node, 'config', lambda: cfg)
    monkeypatch.setattr(node.os, 'geteuid', lambda: 0)
    monkeypatch.setattr(node.sys, 'argv', ['cm-lease', 'run', '--lease-id', LEASE, '--', '/bin/true'])
    monkeypatch.setattr(node.subprocess, 'Popen', lambda argv: launched.append(argv) or process)
    monkeypatch.setattr(node, 'wait_for_scope', lambda p, scope, lock: 247 if p is process else -1)
    with pytest.raises(SystemExit) as exc:
        node.main()
    assert exc.value.code == 247
    argv = launched[0]
    assert argv[:3] == ['/usr/bin/systemd-run', '--quiet', '--scope']
    assert '--slice=' + row['slice'] in argv
    assert '--property=TimeoutStopSec=30s' in argv
    assert '--property=CollectMode=inactive-or-failed' in argv[:argv.index('--')]
    assert argv[-1] == '/bin/true'
    assert '--chdir=' + row['workdir'] in argv
    assert argv.index('/usr/sbin/runuser') < argv.index('--chdir=' + row['workdir'])
    assert ('/usr/bin/taskset' in argv) is bool(pin)


def test_probe_requires_missing_record_and_inactive_empty_slice(context, monkeypatch):
    cfg, args, calls = context
    monkeypatch.setattr(node, 'run', lambda *a: calls.append(a) or 'LoadState=loaded\nActiveState=inactive\nControlGroup=')
    assert node.probe(LEASE) == {'lease_id': LEASE, 'exists': False, 'unit_inactive': True}
    assert node.unit(LEASE) in calls[-1]
    monkeypatch.setattr(node, 'run', lambda *a: 'LoadState=loaded\nActiveState=active\nControlGroup=/active')
    with pytest.raises(node.Refused):
        node.probe(LEASE)
    node.save(LEASE, {'lease_id': LEASE})
    assert node.probe(LEASE)['exists'] is True
    node.save(LEASE, {})
    assert node.probe(LEASE)['exists'] is True


def test_reap_continues_after_one_lease_fails(context, monkeypatch):
    node.save(LEASE, {'expires': 1})
    second = 'a4c4e6cf-bfad-4e66-8a63-d1fce23ba171'
    node.save(second, {'expires': 1})
    def release(ident, cfg):
        if ident == LEASE:
            raise OSError('stop failed')
        return {'lease_id': ident, 'released': True}
    monkeypatch.setattr(node, 'release', release)
    result = node.reap(context[0])
    assert result['released'] == [{'lease_id': second, 'released': True}]
    assert result['failed'][0]['lease_id'] == LEASE
    assert 'stop failed' in result['failed'][0]['error']
    assert 'stop failed' in node.LOG.read_text()


def test_config_resolves_root_but_defers_invalid_root_to_grant_or_cleanup(tmp_path, monkeypatch):
    real = tmp_path / 'real'
    real.mkdir()
    work = real / 'work'
    work.mkdir()
    link = tmp_path / 'link'
    link.symlink_to(real, target_is_directory=True)
    cfg = dict(user='lucas', max_slots=4, cpus_per_slot=2, mem_gb_per_slot=16,
               max_duration_seconds=3600, workdir_root=str(work / '..' / 'work'))

    class RootOwnedConfig:
        def lstat(self):
            return SimpleNamespace(st_uid=0, st_mode=0o600)
        def is_file(self):
            return True
        def is_symlink(self):
            return False
        def read_text(self):
            return json.dumps(cfg)

    monkeypatch.setattr(node, 'CONFIG', RootOwnedConfig())
    assert node.config()['workdir_root'] == str(work.resolve())
    cfg['workdir_root'] = str(link / 'work')
    loaded = node.config()  # invalid root does not prevent status or release setup
    with pytest.raises(node.Refused, match='symlink ancestors'):
        node.workdir_root(loaded)
    cfg['workdir_root'] = str(link)
    loaded = node.config()
    with pytest.raises(node.Refused, match='symlink ancestors'):
        node.workdir_root(loaded)


def test_release_stops_slice_and_skips_workdir_resolving_outside_root(context, tmp_path):
    cfg, args, calls = context
    node.grant(args, cfg)
    path = node.Path(cfg['workdir_root']) / LEASE
    (path / 'OWNER.md').unlink()
    path.rmdir()
    outside = tmp_path / 'outside'
    outside.mkdir()
    path.symlink_to(outside, target_is_directory=True)
    calls.clear()
    result = node.release(LEASE, cfg)
    assert result['released'] and 'workdir realpath' in result['cleanup_skipped']
    assert node.load(LEASE) is None
    assert calls == [('/usr/bin/systemctl', 'stop', node.unit(LEASE))]
    assert outside.exists()


def test_release_skips_symlink_to_another_lease_directory(context):
    cfg, args, calls = context
    node.grant(args, cfg)
    root = node.Path(cfg['workdir_root'])
    path = root / LEASE
    (path / 'OWNER.md').unlink()
    path.rmdir()
    sibling = root / 'another'
    sibling.mkdir()
    path.symlink_to(sibling, target_is_directory=True)
    result = node.release(LEASE, cfg)
    assert result['released'] and 'workdir realpath' in result['cleanup_skipped']
    assert node.load(LEASE) is None
    assert sibling.exists()


@pytest.mark.parametrize('non_raising_resolve', [False, True])
def test_release_skips_unresolvable_workdir(context, monkeypatch, non_raising_resolve):
    cfg, args, calls = context
    node.grant(args, cfg)
    path = node.Path(cfg['workdir_root']) / LEASE
    (path / 'OWNER.md').unlink()
    path.rmdir()
    path.symlink_to(path)
    # Python <=3.12 raises on a loop; 3.13 may return it as an unresolved
    # path. In the latter case the real cleanup sees exists() is false.
    if non_raising_resolve:
        original_resolve = type(path).resolve
        def resolve(self, *args, **kwargs):
            return self if self == path else original_resolve(self, *args, **kwargs)
        monkeypatch.setattr(type(path), 'resolve', resolve)
    monkeypatch.setattr(node, 'workdir_size_and_remove_if_empty', REAL_CLEANUP_WORKDIR)
    result = node.release(LEASE, cfg)
    assert result['released']
    assert result.get('cleanup_skipped') or result.get('workdir_bytes') == 0
    if non_raising_resolve:
        assert result['workdir_bytes'] == 0
    assert node.load(LEASE) is None
    assert path.is_symlink()


def test_reap_stops_slice_and_skips_workdir_resolving_outside_root(context, tmp_path, monkeypatch):
    cfg, args, calls = context
    node.grant(args, cfg)
    path = node.Path(cfg['workdir_root']) / LEASE
    (path / 'OWNER.md').unlink()
    path.rmdir()
    outside = tmp_path / 'outside'
    outside.mkdir()
    path.symlink_to(outside, target_is_directory=True)
    monkeypatch.setattr(node.time, 'time', lambda: 2001)
    calls.clear()
    result = node.reap(cfg)
    assert result['failed'] == []
    assert result['released'][0]['released']
    assert 'workdir realpath' in result['released'][0]['cleanup_skipped']
    assert node.load(LEASE) is None
    assert calls == [('/usr/bin/systemctl', 'stop', node.unit(LEASE))]
    assert outside.exists()


@pytest.mark.parametrize('bad_root', ['missing', 'symlink', 'absent_key', 'invalid_type'])
def test_bad_root_cannot_prevent_release(context, tmp_path, bad_root):
    cfg, args, calls = context
    node.grant(args, cfg)
    if bad_root == 'missing':
        cfg['workdir_root'] = str(tmp_path / 'missing')
    elif bad_root == 'symlink':
        link = tmp_path / 'link'
        link.symlink_to(cfg['workdir_root'], target_is_directory=True)
        cfg['workdir_root'] = str(link)
    elif bad_root == 'absent_key':
        del cfg['workdir_root']
    else:
        cfg['workdir_root'] = None
    with pytest.raises(node.Refused):
        node.grant(args, cfg)
    calls.clear()
    result = node.release(LEASE, cfg)
    assert result['released'] and result['cleanup_skipped']
    assert node.load(LEASE) is None
    assert calls == [('/usr/bin/systemctl', 'stop', node.unit(LEASE))]


def test_bad_root_cannot_prevent_reap(context, tmp_path, monkeypatch):
    cfg, args, calls = context
    node.grant(args, cfg)
    cfg['workdir_root'] = str(tmp_path / 'missing')
    monkeypatch.setattr(node.time, 'time', lambda: 2001)
    calls.clear()
    result = node.reap(cfg)
    assert result['failed'] == [] and result['released'][0]['cleanup_skipped']
    assert node.load(LEASE) is None
    assert calls == [('/usr/bin/systemctl', 'stop', node.unit(LEASE))]


def test_failed_slice_stop_keeps_record_even_when_root_is_invalid(context, tmp_path, monkeypatch):
    cfg, args, calls = context
    node.grant(args, cfg)
    cfg['workdir_root'] = str(tmp_path / 'missing')
    def fail(*args):
        raise OSError('systemctl failed')
    monkeypatch.setattr(node, 'run', fail)
    with pytest.raises(OSError, match='systemctl failed'):
        node.release(LEASE, cfg)
    assert node.load(LEASE) is not None


def test_cleanup_failure_does_not_reverse_confirmed_release(context, monkeypatch):
    cfg, args, calls = context
    node.grant(args, cfg)
    def fail(*args):
        raise OSError('du failed')
    monkeypatch.setattr(node, 'workdir_size_and_remove_if_empty', fail)
    result = node.release(LEASE, cfg)
    assert result['released'] and 'du failed' in result['cleanup_skipped']
    assert node.load(LEASE) is None
    assert calls[-1] == ('/usr/bin/systemctl', 'stop', node.unit(LEASE))


def test_status_works_with_bad_workdir_root(context, tmp_path, monkeypatch, capsys):
    cfg, args, calls = context
    node.grant(args, cfg)
    cfg['workdir_root'] = str(tmp_path / 'missing')
    state = node.STATE
    cgroup = tmp_path / 'cgroup' / 'lease'
    cgroup.mkdir(parents=True)
    (cgroup / 'cpu.stat').write_text('usage_usec 123\n')
    (cgroup / 'memory.current').write_text('4')
    (cgroup / 'memory.peak').write_text('5')

    class RootOwnedConfig:
        def lstat(self): return SimpleNamespace(st_uid=0, st_mode=0o600)
        def is_file(self): return True
        def is_symlink(self): return False
        def read_text(self): return json.dumps(cfg)

    class RootOwnedState:
        def mkdir(self, **kwargs): return state.mkdir(**kwargs)
        def lstat(self): return SimpleNamespace(st_uid=0, st_mode=0o700)
        def is_symlink(self): return False
        def __truediv__(self, name): return state / name

    real_path = node.Path
    monkeypatch.setattr(node, 'CONFIG', RootOwnedConfig())
    monkeypatch.setattr(node, 'STATE', RootOwnedState())
    monkeypatch.setattr(node, 'Path', lambda value: tmp_path / 'cgroup' if value == '/sys/fs/cgroup' else real_path(value))
    monkeypatch.setattr(node.os, 'geteuid', lambda: 0)
    monkeypatch.setattr(node, 'run', lambda *args: calls.append(args) or '/lease')
    monkeypatch.setattr(node.sys, 'argv', ['cm-lease', 'status', '--lease-id', LEASE])
    node.main()
    result = json.loads(capsys.readouterr().out)
    assert result['lease_id'] == LEASE and result['cpu_usage_usec'] == 123
    assert node.unit(LEASE) in calls[-1]


def test_invalid_drain_config_does_not_prevent_release_or_reap(context, monkeypatch):
    cfg, args, _ = context
    node.grant(args, cfg)
    cfg['drain_ignore_datadirs'] = 'invalid'
    class RootOwnedConfig:
        def lstat(self): return SimpleNamespace(st_uid=0, st_mode=0o600)
        def is_file(self): return True
        def is_symlink(self): return False
        def read_text(self): return json.dumps(cfg)
    monkeypatch.setattr(node, 'CONFIG', RootOwnedConfig())
    assert node.release(LEASE, node.config())['released']
    node.save(LEASE, {'lease_id': LEASE, 'workdir': str(node.Path(cfg['workdir_root']) / LEASE),
                      'user': cfg['user'], 'owner': args.owner, 'expires': 2000})
    monkeypatch.setattr(node.time, 'time', lambda: 2001)
    result = node.reap(node.config())
    assert not result['failed'] and result['released'][0]['released']


def test_drain_check_reports_node_capacity(context, tmp_path, monkeypatch, capsys):
    cfg, _, _ = context
    state = node.STATE
    proc = tmp_path / 'empty-proc'
    proc.mkdir()
    class RootOwnedState:
        def mkdir(self, **kwargs): return state.mkdir(**kwargs)
        def lstat(self): return SimpleNamespace(st_uid=0, st_mode=0o700)
        def is_symlink(self): return False
        def __truediv__(self, name): return state / name
    real_path = node.Path
    monkeypatch.setattr(node, 'STATE', RootOwnedState())
    monkeypatch.setattr(node, 'config', lambda: cfg)
    monkeypatch.setattr(node, 'Path', lambda value: proc if value == '/proc' else real_path(value))
    monkeypatch.setattr(node.os, 'geteuid', lambda: 0)
    monkeypatch.setattr(node.sys, 'argv', ['cm-lease', 'drain-check'])
    node.main()
    result = json.loads(capsys.readouterr().out)
    assert result == {'drained': True, 'matching_pids': [], 'ignored_pids': [], 'max_slots': cfg['max_slots'], 'max_duration_seconds': cfg['max_duration_seconds']}


@pytest.mark.parametrize('bad', ['bad', None, [123], ['relative/path'], ['/ok', None], ['/bad\0path']])
def test_config_refuses_invalid_drain_ignore_datadirs(context, monkeypatch, bad):
    cfg, _, _ = context
    cfg['drain_ignore_datadirs'] = bad
    class RootOwnedConfig:
        def lstat(self): return SimpleNamespace(st_uid=0, st_mode=0o600)
        def is_file(self): return True
        def is_symlink(self): return False
        def read_text(self): return json.dumps(cfg)
    monkeypatch.setattr(node, 'CONFIG', RootOwnedConfig())
    loaded = node.config()
    with pytest.raises(node.Refused, match='drain_ignore_datadirs'):
        node.drain_ignore_datadirs(loaded)


def test_drain_check_ignores_only_configured_database_processes(context, tmp_path, monkeypatch, capsys):
    cfg, _, _ = context
    state = node.STATE
    proc_root = tmp_path / 'proc'
    proc_root.mkdir()
    datadir = '/home/lucas/ep-lanes/pgtest/data'
    class RootOwnedState:
        def mkdir(self, **kwargs): return state.mkdir(**kwargs)
        def lstat(self): return SimpleNamespace(st_uid=0, st_mode=0o700)
        def is_symlink(self): return False
        def __truediv__(self, name): return state / name
    def process(pid, argv, cwd, exe, parent=1):
        path = proc_root / str(pid)
        path.mkdir()
        (path / 'cmdline').write_bytes(b'\0'.join(x.encode() for x in argv) + b'\0')
        (path / 'stat').write_text(f'{pid} (process name) S {parent} 0')
        (path / 'cwd').symlink_to(cwd)
        (path / 'exe').symlink_to(exe)
    process(101, ['/usr/lib/postgresql/16/bin/postgres', '-D', datadir], '/tmp', '/usr/lib/postgresql/16/bin/postgres')
    process(102, ['postgres: writer'], datadir + '/pg_wal', '/usr/lib/postgresql/16/bin/postgres', parent=101)
    process(103, ['bash', '/home/lucas/ep-lanes/driver.sh'], '/home/lucas/ep-lanes/pgtest/other', '/usr/bin/bash')
    process(104, ['bash', '/home/lucas/ep-lanes/job.sh'], '/home/lucas/ep-lanes/pgtest/other', '/usr/bin/bash')
    process(105, ['postgres', '-D', datadir + '-other'], '/home/lucas/ep-lanes/pgtest/other', '/usr/lib/postgresql/16/bin/postgres')
    process(106, ['postgres: backend'], datadir, '/usr/lib/postgresql/16/bin/postgres')
    process(107, ['postgres: other'], datadir + '/pg_wal', '/usr/lib/postgresql/16/bin/postgres')
    process(108, ['bash', '-D', datadir], '/home/lucas/ep-lanes/pgtest/other', '/usr/bin/bash')
    process(109, ['bash', '/home/lucas/ep-lanes/job.sh'], datadir, '/usr/bin/bash')
    real_path = node.Path
    monkeypatch.setattr(node, 'STATE', RootOwnedState())
    monkeypatch.setattr(node, 'config', lambda: cfg)
    monkeypatch.setattr(node, 'Path', lambda value: proc_root if value == '/proc' else real_path(value))
    monkeypatch.setattr(node.os, 'geteuid', lambda: 0)
    monkeypatch.setattr(node.sys, 'argv', ['cm-lease', 'drain-check'])
    node.main()
    baseline = json.loads(capsys.readouterr().out)
    assert set(baseline['matching_pids']) == {101, 102, 103, 104, 105, 106, 107, 108, 109}
    assert baseline['ignored_pids'] == [] and not baseline['drained']
    cfg['drain_ignore_datadirs'] = []
    node.main()
    assert json.loads(capsys.readouterr().out) == baseline
    cfg['drain_ignore_datadirs'] = [datadir]
    node.main()
    result = json.loads(capsys.readouterr().out)
    assert {row['pid']: row['datadir'] for row in result['ignored_pids']} == {101: datadir, 102: datadir, 106: datadir}
    assert set(result['matching_pids']) == {103, 104, 105, 107, 108, 109}
    assert not result['drained']
    for pid in (103, 104, 105, 107, 108, 109):
        for entry in (proc_root / str(pid)).iterdir():
            entry.unlink()
        (proc_root / str(pid)).rmdir()
    node.main()
    drained = json.loads(capsys.readouterr().out)
    assert drained['drained'] and {row['pid'] for row in drained['ignored_pids']} == {101, 102, 106}
