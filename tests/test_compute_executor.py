import importlib.machinery
import importlib.util
import shlex
from types import SimpleNamespace

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
    monkeypatch.setattr(executor.subprocess, 'run', lambda *a, **kw: SimpleNamespace(stdout='{"drained":true}'))
    monkeypatch.setattr(executor, 'api', lambda path, body: calls.append((path, body)))
    enabled = set()
    executor.enable_drained_boxes(cfg, enabled)
    assert not calls and not enabled
    cfg['cutover_ready'] = True
    executor.enable_drained_boxes(cfg, enabled)
    assert enabled == {'box'}
    assert calls[-1][1] == {'box': 'box', 'local_drained': True, 'node_drained': True}
    executor.enable_drained_boxes(cfg, enabled)
    assert calls.count('local') == 1
