import importlib.machinery
import importlib.util
import shlex
from types import SimpleNamespace

import pytest

loader = importlib.machinery.SourceFileLoader('cm_compute_executor', 'scripts/cm-compute-executor')
spec = importlib.util.spec_from_loader(loader.name, loader)
executor = importlib.util.module_from_spec(spec)
loader.exec_module(executor)


def test_node_command_keeps_owner_as_single_argument():
    action = {'kind': 'grant', 'lease_id': 'id', 'payload': {'owner': 'agent:d:s;echo bad',
              'slots': 2, 'mem_gb': 32, 'expires_epoch': 123, 'pin': '0,8,1,9'}}
    parts = shlex.split(executor.command_for(action))
    assert parts[parts.index('--owner') + 1] == 'agent:d:s;echo bad'
    assert parts[parts.index('--pin') + 1] == '0,8,1,9'


def test_cutover_is_fail_closed_and_enable_requires_both_drains(monkeypatch, tmp_path):
    calls = []
    cfg = {'cutover_ready': False, 'lq_directory': str(tmp_path),
           'routes': {'box': {'argv': ['gcloud', 'compute', 'ssh', 'vm']}}}
    monkeypatch.setattr(executor, 'local_drained', lambda *_: calls.append('local') or True)
    monkeypatch.setattr(executor.subprocess, 'run', lambda *a, **kw: SimpleNamespace(stdout='{"drained":true,"max_slots":7}'))
    def api(path, body):
        calls.append((path, body))
        if path == '/compute/executor/pre-enable':
            return {'queued_count': 0, 'queued': [], 'enabled_boxes': [], 'has_ever_enabled': False, 'quarantined_boxes': []}
        return {'enabled': True}
    monkeypatch.setattr(executor, 'api', api)
    enabled = set()
    executor.enable_drained_boxes(cfg, enabled)
    assert not calls and not enabled
    cfg['cutover_ready'] = True
    executor.enable_drained_boxes(cfg, enabled)
    assert enabled == {'box'}
    assert calls[-1][1] == {'box': 'box', 'local_drained': True, 'node_drained': True, 'node_max_slots': 7}
    executor.enable_drained_boxes(cfg, enabled)
    assert calls.count('local') == 1


def test_pre_enable_lists_old_queue_and_blocks_first_enable_until_review(monkeypatch, tmp_path, caplog):
    calls = []
    queued = [{'lease_id': 'old', 'missing_justification': True}]
    report = {'queued_count': 1, 'queued': queued, 'enabled_boxes': [], 'has_ever_enabled': False, 'quarantined_boxes': []}
    cfg = {'cutover_ready': True, 'lq_directory': str(tmp_path),
           'routes': {'box': {'argv': ['gcloud', 'compute', 'ssh', 'vm']}}}
    monkeypatch.setattr(executor, 'local_drained', lambda *_: calls.append('local') or True)
    monkeypatch.setattr(executor.subprocess, 'run', lambda *a, **kw: SimpleNamespace(stdout='{"drained":true,"max_slots":7}'))
    def api(path, body):
        calls.append(path)
        if path == '/compute/executor/pre-enable':
            return report
        return {'enabled': True}
    monkeypatch.setattr(executor, 'api', api)
    enabled = set()
    with caplog.at_level('INFO', logger='cm-compute-executor'):
        executor.enable_drained_boxes(cfg, enabled)
    assert calls == ['/compute/executor/pre-enable'] and not enabled
    assert 'old' in caplog.text and 'missing_justification' in caplog.text
    cfg['allow_queued_at_cutover'] = True
    executor.enable_drained_boxes(cfg, enabled)
    assert enabled == {'box'}
    # Once another box is enabled, ordinary queued work must not prevent
    # later boxes from joining the pool.
    cfg['allow_queued_at_cutover'] = False
    report['enabled_boxes'] = []  # all live boxes may now be quarantined
    report['has_ever_enabled'] = True
    enabled.clear()
    executor.enable_drained_boxes(cfg, enabled)
    assert enabled == {'box'}


def test_pre_enable_check_failure_keeps_boxes_disabled(monkeypatch, tmp_path):
    cfg = {'cutover_ready': True, 'lq_directory': str(tmp_path),
           'routes': {'box': {'argv': ['gcloud', 'compute', 'ssh', 'vm']}}}
    monkeypatch.setattr(executor, 'api', lambda *_: {'queued_count': 0, 'queued': ['inconsistent'], 'enabled_boxes': [], 'has_ever_enabled': False, 'quarantined_boxes': []})
    monkeypatch.setattr(executor, 'local_drained', lambda *_: pytest.fail('must not start drain check'))
    enabled = set()
    executor.enable_drained_boxes(cfg, enabled)
    assert not enabled


def test_node_grant_refusal_is_reported_but_transport_failure_is_not(monkeypatch):
    import subprocess
    import pytest
    action = {'kind': 'grant', 'lease_id': 'lease-id', 'payload': {'box': 'box', 'owner': 'agent:d:s',
              'slots': 1, 'mem_gb': 16, 'expires_epoch': 123}}
    routes = {'box': {'argv': ['gcloud', 'compute', 'ssh', 'vm']}}
    def fail(returncode, stderr):
        def run(argv, **kwargs):
            raise subprocess.CalledProcessError(returncode, argv, stderr=stderr)
        return run
    monkeypatch.setattr(executor.subprocess, 'run', fail(3, 'banner\n{"refused":true,"error":"box capacity exceeded"}\n'))
    assert executor.execute(action, routes) == {'lease_id': 'lease-id', 'refused': True, 'error': 'box capacity exceeded'}
    monkeypatch.setattr(executor.subprocess, 'run', fail(3, '{"error":"unknown user","refused":false}'))
    assert executor.execute(action, routes) == {'lease_id': 'lease-id', 'retryable': True, 'error': 'unknown user'}
    for code, stderr in [(255, '{"refused":true,"error":"connection lost"}'),
                         (3, 'transport failed')]:
        monkeypatch.setattr(executor.subprocess, 'run', fail(code, stderr))
        with pytest.raises(subprocess.CalledProcessError):
            executor.execute(action, routes)


def test_failed_extension_requires_explicit_node_absence(monkeypatch):
    import subprocess
    import pytest
    action = {'kind': 'extend', 'lease_id': 'lease-id',
              'payload': {'box': 'box', 'expires_epoch': 123}}
    routes = {'box': {'argv': ['gcloud', 'compute', 'ssh', 'vm']}}
    calls = []
    def run(argv, **kwargs):
        calls.append(argv)
        if len(calls) == 1:
            raise subprocess.CalledProcessError(3, argv)
        return SimpleNamespace(stdout='{"lease_id":"lease-id","exists":false,"unit_inactive":true}')
    monkeypatch.setattr(executor.subprocess, 'run', run)
    assert executor.execute(action, routes) == {'lease_id': 'lease-id', 'absent': True, 'unit_inactive': True}
    assert ' probe ' in calls[1][-1]
    calls.clear()
    def transport_timeout(argv, **kwargs):
        calls.append(argv)
        raise subprocess.TimeoutExpired(argv, 90)
    monkeypatch.setattr(executor.subprocess, 'run', transport_timeout)
    with pytest.raises(subprocess.TimeoutExpired):
        executor.execute(action, routes)
    assert len(calls) == 1


def test_failed_extension_does_not_infer_absence_from_uncertain_probe(monkeypatch):
    import subprocess
    import pytest
    action = {'kind': 'extend', 'lease_id': 'lease-id',
              'payload': {'box': 'box', 'expires_epoch': 123}}
    routes = {'box': {'argv': ['gcloud', 'compute', 'ssh', 'vm']}}
    replies = iter([subprocess.CalledProcessError(255, 'ssh'),
                    SimpleNamespace(stdout='{"lease_id":"lease-id","exists":true}')])
    def run(*args, **kwargs):
        result = next(replies)
        if isinstance(result, Exception):
            raise result
        return result
    monkeypatch.setattr(executor.subprocess, 'run', run)
    with pytest.raises(ValueError, match='did not confirm'):
        executor.execute(action, routes)
    replies = iter([subprocess.CalledProcessError(3, 'extend'),
                    subprocess.TimeoutExpired('probe', 90)])
    with pytest.raises(subprocess.TimeoutExpired):
        executor.execute(action, routes)


def test_quarantine_skips_drain_and_inventory_logs_only_on_change(monkeypatch, tmp_path, caplog):
    report = {'queued_count': 1, 'queued': [{'lease_id': 'old'}], 'enabled_boxes': [],
              'has_ever_enabled': False, 'quarantined_boxes': ['box']}
    cfg = {'cutover_ready': True, 'allow_queued_at_cutover': True, 'lq_directory': str(tmp_path),
           'routes': {'box': {'argv': ['ssh', 'box']}}}
    monkeypatch.setattr(executor, '_last_queue_inventory', None)
    monkeypatch.setattr(executor, 'api', lambda *_: report)
    monkeypatch.setattr(executor, 'local_drained', lambda *_: pytest.fail('quarantined box was checked'))
    with caplog.at_level('INFO', logger='cm-compute-executor'):
        executor.enable_drained_boxes(cfg, set())
        executor.enable_drained_boxes(cfg, set())
        report['queued'][0]['lease_id'] = 'new'
        executor.enable_drained_boxes(cfg, set())
        report['has_ever_enabled'] = True
        report['queued'][0]['lease_id'] = 'later'
        executor.enable_drained_boxes(cfg, set())
    records = [r.message for r in caplog.records if 'pre-enable queue count' in r.message]
    assert len(records) == 2 and 'old' in records[0] and 'new' in records[1]
