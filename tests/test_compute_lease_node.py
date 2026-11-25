import importlib.machinery
import importlib.util
import json
from types import SimpleNamespace

import pytest

loader = importlib.machinery.SourceFileLoader('cm_lease_node', 'scripts/cm-lease')
spec = importlib.util.spec_from_loader(loader.name, loader)
node = importlib.util.module_from_spec(spec)
loader.exec_module(node)

LEASE = '21da6f57-0d54-4e75-a19d-4a9faab79aaa'


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
    monkeypatch.setattr(node.pwd, 'getpwnam', lambda _: SimpleNamespace(pw_uid=1, pw_gid=1))
    def make_workdir(path, owner, ident, expires, user):
        path.mkdir()
        (path / 'OWNER.md').write_text(owner)
    monkeypatch.setattr(node, 'create_workdir', make_workdir)
    monkeypatch.setattr(node, 'workdir_size_and_remove_if_empty', lambda path, user: 1)
    calls = []
    monkeypatch.setattr(node, 'run', lambda *args: calls.append(args))
    cfg = dict(user='lucas', max_slots=4, cpus_per_slot=2, mem_gb_per_slot=16,
               max_duration_seconds=3600, workdir_root=str(work))
    args = SimpleNamespace(lease_id=LEASE, owner='agent:daemon:session', slots=1,
                           mem_gb=16, pin=None, expires_epoch=2000)
    return cfg, args, calls


def test_grant_is_idempotent_and_loose_limits_memory_and_cpu(context):
    cfg, args, calls = context
    first = node.grant(args, cfg)
    second = node.grant(args, cfg)
    assert first == second
    properties = [call for call in calls if 'set-property' in call]
    assert len(properties) == 2
    assert all('MemoryMax=16G' in call and 'CPUQuota=200%' in call for call in properties)
    assert all(not any('AllowedCPUs' in value for value in call) for call in properties)


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
    assert any('AllowedCPUs=0,8' in call for call in calls)
    result = node.release(LEASE, cfg)
    assert result['released']
    assert calls[-1] == ('/usr/bin/systemctl', 'stop', 'cmlease-' + LEASE + '.slice')
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
    assert calls[-1] == ('/usr/bin/systemctl', 'stop', 'cmlease-' + LEASE + '.slice')


def test_workdir_creation_drops_privileges_before_touching_user_path(monkeypatch, tmp_path):
    calls = []
    monkeypatch.setattr(node, 'run', lambda *args: calls.append(args))
    node.create_workdir(tmp_path / LEASE, 'agent:daemon:owner', LEASE, 2000, 'lucas')
    assert calls[0][:5] == ('/usr/sbin/runuser', '-u', 'lucas', '--', '/usr/bin/python3')
    assert not (tmp_path / LEASE).exists()


def test_probe_requires_missing_record_and_inactive_empty_slice(context, monkeypatch):
    cfg, args, calls = context
    monkeypatch.setattr(node, 'run', lambda *a: 'LoadState=loaded\nActiveState=inactive\nControlGroup=')
    assert node.probe(LEASE) == {'lease_id': LEASE, 'exists': False, 'unit_inactive': True}
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
    assert calls == [('/usr/bin/systemctl', 'stop', 'cmlease-' + LEASE + '.slice')]
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


def test_release_skips_unresolvable_workdir(context):
    cfg, args, calls = context
    node.grant(args, cfg)
    path = node.Path(cfg['workdir_root']) / LEASE
    (path / 'OWNER.md').unlink()
    path.rmdir()
    path.symlink_to(path)
    result = node.release(LEASE, cfg)
    assert result['released'] and result['cleanup_skipped']
    assert node.load(LEASE) is None


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
    assert calls == [('/usr/bin/systemctl', 'stop', 'cmlease-' + LEASE + '.slice')]
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
    assert calls == [('/usr/bin/systemctl', 'stop', 'cmlease-' + LEASE + '.slice')]


def test_bad_root_cannot_prevent_reap(context, tmp_path, monkeypatch):
    cfg, args, calls = context
    node.grant(args, cfg)
    cfg['workdir_root'] = str(tmp_path / 'missing')
    monkeypatch.setattr(node.time, 'time', lambda: 2001)
    calls.clear()
    result = node.reap(cfg)
    assert result['failed'] == [] and result['released'][0]['cleanup_skipped']
    assert node.load(LEASE) is None
    assert calls == [('/usr/bin/systemctl', 'stop', 'cmlease-' + LEASE + '.slice')]


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
    assert calls[-1] == ('/usr/bin/systemctl', 'stop', 'cmlease-' + LEASE + '.slice')


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
    monkeypatch.setattr(node, 'run', lambda *args: '/lease')
    monkeypatch.setattr(node.sys, 'argv', ['cm-lease', 'status', '--lease-id', LEASE])
    node.main()
    result = json.loads(capsys.readouterr().out)
    assert result['lease_id'] == LEASE and result['cpu_usage_usec'] == 123
