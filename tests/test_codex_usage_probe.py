import contextlib
import fcntl
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / 'scripts/codex-usage-probe'
loader = importlib.machinery.SourceFileLoader('codex_usage_probe', str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
probe = importlib.util.module_from_spec(spec)
loader.exec_module(probe)

FAKE = r'''#!/usr/bin/env python3
import os, sys, json, datetime, time, tomllib
from pathlib import Path
if '--version' in sys.argv:
 print('codex-cli 0.153.4'); raise SystemExit(0)
mode=os.environ.get('PROBE_TEST_MODE','success')
if mode=='timeout':
 import subprocess
 child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'])
 Path(os.environ['PROBE_CHILD_PID']).write_text(str(child.pid))
 time.sleep(60)
home=Path(os.environ['CODEX_HOME'])
thread='01a07929-a98a-7a10-a476-b673a60024e3'
model='wrong-model' if mode=='wrong_model' else sys.argv[sys.argv.index('--model')+1]
reply='CM_CODEX_PROBE_OK'
overrides={}
for i, arg in enumerate(sys.argv[:-1]):
 if arg=='-c': overrides.update(tomllib.loads(sys.argv[i+1]))
provider=overrides.get('model_provider','openai')
if provider=='cm_pool':
 assert sys.argv.index('exec') < sys.argv.index('-c')
 assert overrides['model_providers']['cm_pool']['auth']['timeout_ms']==5000
meta={'type':'session_meta','payload':{'id':thread,'cli_version':'0.153.4','model_provider':provider}}
context={'type':'turn_context','payload':{'model':model}}
final={'type':'event_msg','payload':{'type':'task_complete','last_agent_message':reply}}
records=[meta,context,final]
events=[{'type':'thread.started','thread_id':thread},{'type':'turn.started'}, {'type':'item.completed','item':{'type':'agent_message','text':reply}}, {'type':'turn.completed'}]
if mode=='missing_model': records.remove(context)
if mode=='missing_completion': events.pop()
if mode=='tool': events.insert(2,{'type':'item.completed','item':{'type':'command_execution'}})
if mode in ['auth','usage','model','unknown']:
 errors={'auth':{'message':'unexpected status 401 Unauthorized: Bearer SECRET','codex_error_info':'other'},'usage':{'message':"You've hit your usage limit. Try again later.",'codex_error_info':'usage_limit_exceeded'},'model':{'message':'unexpected status 404 Not Found: model not found; SECRET','codex_error_info':'other'},'unknown':{'message':'SECRET miscellaneous failure','codex_error_info':'other'}}
 error=errors[mode]
 final['payload']['error']=error
 events=[events[0],{'type':'turn.failed','error':error}]
path=home/'sessions'/datetime.datetime.now(datetime.timezone.utc).strftime('%Y/%m/%d')/('rollout-'+thread+'.jsonl')
path.parent.mkdir(parents=True,exist_ok=True)
path.write_text(''.join(json.dumps(r)+'\n' for r in records))
for event in events: print(json.dumps(event))
print('SECRET diagnostic',file=sys.stderr)
raise SystemExit(1 if mode in ['auth','usage','model','unknown'] else 0)
'''


class CodexUsageProbeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name)
        self.codex_home = self.home / '.codex'
        self.binary = self.home / 'codex'
        self.binary.write_text(FAKE)
        self.binary.chmod(0o700)
        self.env = mock.patch.dict(os.environ, {'HOME': str(self.home), 'CODEX_HOME': str(self.codex_home), 'PROBE_TEST_MODE': 'success'})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.config = probe.configure(str(self.binary), 'gpt-5.6-sol', self.codex_home)

    def test_success_requires_rollout_model_completion_and_no_tools(self):
        result = probe.probe(self.config, self.codex_home)
        self.assertEqual(result['status'], 'OK')
        self.assertEqual(result['observed_model'], 'gpt-5.6-sol')
        self.assertGreaterEqual(result['checked_at'], result['started_at'])
        for mode in ['wrong_model', 'missing_model', 'missing_completion', 'tool']:
            with self.subTest(mode=mode), mock.patch.dict(os.environ, {'PROBE_TEST_MODE': mode}):
                self.assertEqual(probe.probe(self.config, self.codex_home)['status'], 'ERROR')

    def test_structured_failures_are_classified_without_leaking_raw_details(self):
        for mode, status in [('auth','AUTH_EXPIRED'),('usage','USAGE_LIMITED'),('model','MODEL_UNAVAILABLE'),('unknown','ERROR')]:
            with self.subTest(mode=mode), mock.patch.dict(os.environ, {'PROBE_TEST_MODE': mode}):
                result = probe.probe(self.config, self.codex_home)
                self.assertEqual(result['status'], status)
                self.assertNotIn('SECRET', json.dumps(result))
        self.assertEqual(probe.error_status([{'type':'item.completed','item':{'type':'agent_message','text':"You've hit your usage limit. Try again later."}}], []), 'ERROR')

    def test_pinned_runtime_or_provider_change_cannot_certify_recovery(self):
        self.binary.write_text(FAKE + '\n# changed\n')
        self.assertEqual(probe.probe(self.config, self.codex_home)['status'], 'ERROR')
        self.binary.write_text(FAKE)
        self.codex_home.mkdir(exist_ok=True)
        (self.codex_home/'config.toml').write_text('model_provider = "different"\n')
        self.assertEqual(probe.probe(self.config, self.codex_home)['status'], 'ERROR')

    def test_changed_host_model_invalidates_pinned_configuration(self):
        self.codex_home.mkdir(exist_ok=True)
        (self.codex_home/'config.toml').write_text('model = "different-model"\n')
        self.assertEqual(probe.probe(self.config, self.codex_home)['status'], 'ERROR')

    def test_pool_probe_uses_pinned_route_and_detects_helper_or_endpoint_drift(self):
        self.codex_home.mkdir(exist_ok=True)
        helper = self.home/'token-helper'
        helper.write_text('#!/bin/sh\nprintf PRIVATE_TEST_KEY\n')
        helper.chmod(0o700)
        definition = {'name':'openai', 'base_url':'http://127.0.0.1:2455/backend-api/codex',
                      'wire_api':'responses', 'supports_websockets':True,
                      'auth':{'command':str(helper),'timeout_ms':5000,'refresh_interval_ms':300000}}
        path = self.codex_home/'config.toml'
        text = 'model="gpt-5.6-sol"\nmodel_provider="cm_pool"\nmodel_providers.cm_pool=' + probe.toml_literal(definition) + '\n'
        path.write_text(text)
        config = probe.configure(str(self.binary), 'gpt-5.6-sol', self.codex_home)
        result = probe.probe(config, self.codex_home)
        self.assertEqual(result['status'], 'OK')
        self.assertEqual(result['model_provider'], 'cm_pool')
        self.assertEqual(result['provider_config'], definition)
        self.assertNotIn('PRIVATE_TEST_KEY', json.dumps(result))
        helper.write_text('#!/bin/sh\nprintf CHANGED_TEST_KEY\n')
        self.assertEqual(probe.probe(config, self.codex_home)['status'], 'ERROR')
        path.write_text(text.replace('127.0.0.1:2455','127.0.0.1:9999'))
        with self.assertRaises(ValueError): probe.configure(str(self.binary), 'gpt-5.6-sol', self.codex_home)
        path.write_text(text)
        definition['env_key'] = 'AMBIGUOUS_AUTH'
        path.write_text('model_provider="cm_pool"\nmodel_providers.cm_pool=' + probe.toml_literal(definition))
        with self.assertRaises(ValueError): probe.configure(str(self.binary), 'gpt-5.6-sol', self.codex_home)

    def test_pool_exhaustion_is_distinct_from_individual_account_usage(self):
        message = 'unexpected status 503 Service Unavailable: No available accounts. Service is operating in degraded mode: all upstream accounts are unavailable'
        self.assertEqual(probe.error_status([{'type':'turn.failed','error':{'message':message}}], []), 'POOL_UNAVAILABLE')
        self.assertEqual(probe.error_status([{'type':'item.completed','item':{'type':'agent_message','text':message}}], []), 'ERROR')
        self.assertEqual(probe.error_status([{'type':'turn.failed','error':{'message':'unexpected status 503 Service Unavailable: owner unavailable'}}], []), 'ERROR')

    def test_timeout_kills_disposable_child_group(self):
        pidfile = self.home/'child.pid'
        with mock.patch.dict(os.environ, {'PROBE_TEST_MODE':'timeout','PROBE_CHILD_PID':str(pidfile)}):
            result = probe.probe(self.config, self.codex_home, timeout=0.3)
        self.assertEqual(result['status'], 'TIMEOUT')
        pid = int(pidfile.read_text())
        # A zombie can await init's reap, but no child may remain executing.
        stat = Path('/proc')/str(pid)/'stat'
        self.assertTrue(not stat.exists() or stat.read_text().split()[2] == 'Z')

    def test_failure_replaces_success_and_lock_contention_preserves_last_result(self):
        config, state, lock = [self.home/name for name in ['config.json','state.json','probe.lock']]
        probe.atomic_write(config, self.config)
        args = ['--config',str(config),'--state',str(state),'--lock',str(lock)]
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(probe.main(args), 0)
            self.assertEqual(json.loads(state.read_text())['status'], 'OK')
            previous = state.read_bytes()
            with lock.open('a') as held:
                fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
                self.assertEqual(probe.main(args), 0)
                self.assertEqual(state.read_bytes(), previous)
            with mock.patch.dict(os.environ, {'PROBE_TEST_MODE':'auth'}):
                self.assertEqual(probe.main(args), 1)
        self.assertEqual(json.loads(state.read_text())['status'], 'AUTH_EXPIRED')
        self.assertEqual(state.stat().st_mode & 0o777, 0o600)

    def test_failed_atomic_replace_preserves_previous_complete_state(self):
        state = self.home/'state.json'
        probe.atomic_write(state, {'status':'OK'})
        before = state.read_bytes()
        with mock.patch.object(os, 'replace', side_effect=OSError('disk full')):
            with self.assertRaises(OSError): probe.atomic_write(state, {'status':'ERROR'})
        self.assertEqual(state.read_bytes(), before)
        self.assertEqual(list(self.home.glob('*.tmp')), [])

    def test_actual_pinned_cli_error_fixtures(self):
        root = SCRIPT.parents[1]/'daemon/tests/fixtures/codex-0.153.4'
        for name, expected in [('invalid_api_key','AUTH_EXPIRED'),('usage_limit_reached','USAGE_LIMITED'),('model_not_found','ERROR'),('pool_unavailable','POOL_UNAVAILABLE')]:
            events = [json.loads(s) for s in (root/(name+'-exec.jsonl')).read_text().splitlines()]
            records = [json.loads(s) for s in (root/(name+'.jsonl')).read_text().splitlines()]
            self.assertEqual(probe.error_status(events,records), expected)


if __name__ == '__main__': unittest.main()
