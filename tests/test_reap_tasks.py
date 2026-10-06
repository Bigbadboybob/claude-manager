"""Batch pipeline of scripts/reap_tasks.py against a fake `worktree.cleanup` host.

The RPC layer (`call`), clock and sleep are injected, so these run instantly and
never touch ssh or a daemon."""
from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts import reap_tasks
from scripts import worktree_lineage as lineage


class FakeClock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now

    def sleep(self, secs):
        self.now += secs


class FakeHost:
    """Host-side cleanup jobs: a preview settles after `scan_rounds` status
    reads, an apply completes after `apply_rounds` (None = never)."""

    def __init__(self, clock, *, scan_rounds=1, apply_rounds=1, candidates=lambda tid: [], refuse_apply=(),
                 fail_rounds=0, round_secs=0.0):
        self.clock = clock
        self.scan_rounds = scan_rounds
        self.apply_rounds = apply_rounds
        self.candidates = candidates
        self.refuse_apply = set(refuse_apply)
        self.fail_rounds = fail_rounds
        self.round_secs = round_secs
        self.jobs: dict[str, dict] = {}
        self.rounds = 0
        self.max_scanning = 0
        self.applied: list[str] = []

    def __call__(self, host, calls):
        self.rounds += 1
        self.clock.now += self.round_secs
        if self.rounds <= self.fail_rounds:
            raise reap_tasks.RemoteError('remote call to cm-manager timed out after 60s')
        answers = []
        for params in calls:
            ident, action = params['id'], params['action']
            job = self.jobs.get(ident)
            if action == 'preview':
                if job is None:
                    job = self.jobs[ident] = {'id': ident, 'task_id': params['task_id'], 'phase': 'scanning', 'reads': 0}
                answers.append(dict(job))
            elif action == 'apply':
                if job['task_id'] in self.refuse_apply:
                    answers.append({'error': 'preview is stale; re-run preview', 'transient': False})
                    continue
                self.applied.append(job['task_id'])
                job.update(phase='queued', reads=0)
                answers.append(dict(job))
            else:
                job['reads'] += 1
                if job['phase'] == 'scanning' and job['reads'] >= self.scan_rounds:
                    job.update(phase='preview', candidates=[{'path': p, 'reason': 'terminal_and_inactive'}
                                                            for p in self.candidates(job['task_id'])])
                elif job['phase'] in ('queued', 'running') and self.apply_rounds is not None and job['reads'] >= self.apply_rounds:
                    job.update(phase='complete', results=[{'path': c['path'], 'removed': True, 'message': 'reaped'}
                                                          for c in job['candidates']])
                elif job['phase'] == 'queued':
                    job['phase'] = 'running'
                answers.append(dict(job))
        self.max_scanning = max(self.max_scanning, sum(j['phase'] == 'scanning' for j in self.jobs.values()))
        return answers


def ids(n):
    return [f'{i:08x}-0000-4000-8000-000000000000' for i in range(n)]


class ReapBatchTests(unittest.TestCase):
    def drive(self, host, task_ids, *, dry_run=False, wait=90.0, concurrency=6, preview_timeout=180.0):
        emitted = []
        jobs = reap_tasks.run('cm-manager', task_ids, dry_run, wait, call=host, clock=host.clock,
                              sleep=host.clock.sleep, emit=emitted.append, concurrency=concurrency,
                              preview_timeout=preview_timeout)
        return jobs, emitted

    def test_large_batch_pipelines_instead_of_one_task_at_a_time(self):
        clock = FakeClock()
        tasks = ids(45)
        host = FakeHost(clock, scan_rounds=2, apply_rounds=2,
                        candidates=lambda tid: [f'/wt/{tid[:8]}'] if int(tid[:8], 16) % 3 else [])
        jobs, emitted = self.drive(host, tasks)
        self.assertTrue(all(job.done for job in jobs))
        # Every task answered exactly once, in a bounded number of round trips.
        self.assertEqual(sorted(j.task_id for j in emitted), sorted(tasks))
        self.assertLessEqual(host.max_scanning, 6)
        self.assertLess(host.rounds, 40)
        reaped = [j for j in jobs if j.summary.get('results')]
        self.assertEqual(len(reaped), 30)
        self.assertTrue(all(r['removed'] for j in reaped for r in j.summary['results']))
        self.assertEqual({j.summary.get('note') for j in jobs if not j.summary.get('results')}, {'no tracked checkout'})
        self.assertEqual(sorted(host.applied), sorted(j.task_id for j in reaped))

    def test_wait_bounds_the_whole_batch_not_each_task(self):
        clock = FakeClock()
        tasks = ids(30)
        host = FakeHost(clock, apply_rounds=None, candidates=lambda tid: [f'/wt/{tid[:8]}'])
        jobs, emitted = self.drive(host, tasks, wait=30.0)
        self.assertEqual(len(emitted), 30)
        # Previews (5 waves of 6, 2 rounds each at 3 s) then ONE 30 s watch window.
        self.assertLess(clock.now, 30 * 3 + 60)
        for job in jobs:
            self.assertIn('still settling', job.summary['note'])
            self.assertIn(job.job_id, job.summary['note'])
            lines, _, _ = reap_tasks.render(job)
            self.assertIn(f'status id {job.job_id}', lines[-1])

    def test_dry_run_never_applies(self):
        clock = FakeClock()
        host = FakeHost(clock, candidates=lambda tid: ['/wt/x'])
        jobs, _ = self.drive(host, ids(8), dry_run=True)
        self.assertEqual(host.applied, [])
        self.assertEqual({j.summary['note'] for j in jobs}, {'dry run; not applied'})
        self.assertEqual(reap_tasks.render(jobs[0])[0], [f'{jobs[0].task_id[:8]} dry run; not applied terminal_and_inactive'])

    def test_failed_round_trips_are_retried(self):
        clock = FakeClock()
        host = FakeHost(clock, fail_rounds=3, candidates=lambda tid: ['/wt/x'])
        progress = []
        jobs = reap_tasks.run('cm-manager', ids(10), False, 60.0, call=host, clock=clock, sleep=clock.sleep,
                              progress=progress.append)
        self.assertTrue(all(j.summary.get('results') for j in jobs))
        self.assertTrue(any('timed out' in line for line in progress))

    def test_hung_host_times_out_with_a_clear_error_per_task(self):
        clock = FakeClock()
        host = FakeHost(clock, fail_rounds=10**6, round_secs=60.0)
        jobs, emitted = self.drive(host, ids(12), wait=10.0, preview_timeout=180.0)
        self.assertEqual(len(emitted), 12)
        for job in jobs:
            self.assertIn('preview did not settle within 180s', job.summary['error'])
            self.assertIn('timed out after 60s', job.summary['error'])
        # Bounded: two preview waves, each expiring on its own window.
        self.assertLess(clock.now, 2 * (180 + 60) + 60)

    def test_apply_refusal_is_reported_and_the_batch_continues(self):
        clock = FakeClock()
        tasks = ids(5)
        host = FakeHost(clock, candidates=lambda tid: ['/wt/x'], refuse_apply={tasks[2]})
        jobs, emitted = self.drive(host, tasks)
        refused = next(j for j in jobs if j.task_id == tasks[2])
        self.assertIn('apply refused: preview is stale', refused.summary['error'])
        self.assertEqual(sum(bool(j.summary.get('results')) for j in jobs), 4)
        self.assertEqual(len(emitted), 5)

    def test_duplicate_ids_are_reaped_once(self):
        clock = FakeClock()
        task = ids(1)[0]
        host = FakeHost(clock, candidates=lambda tid: ['/wt/x'])
        jobs, _ = self.drive(host, [task, task, task])
        self.assertEqual(len(jobs), 1)
        self.assertEqual(host.applied, [task])

    def test_retained_result_renders_its_reason(self):
        job = reap_tasks.Job('abcdef01-0000-4000-8000-000000000000')
        job.summary = {'results': [{'path': '/wt/keep', 'removed': False, 'message': 'retained: pinned'}]}
        self.assertEqual(reap_tasks.render(job), (['abcdef01 RETAINED keep: retained: pinned'], 0, 1))


class RpcBatchTests(unittest.TestCase):
    def test_round_trip_timeout_raises_a_clear_error(self):
        with patch.object(reap_tasks.subprocess, 'run', side_effect=subprocess.TimeoutExpired('ssh', 60)):
            with self.assertRaisesRegex(reap_tasks.RemoteError, 'cm-manager timed out after 60s'):
                reap_tasks.rpc_batch('cm-manager', [{'action': 'status', 'id': 'x'}], timeout=60)

    def test_per_call_answers_keep_order_and_mark_transport_errors_transient(self):
        payload = [{'ok': True, 'result': {'phase': 'preview'}},
                   {'ok': False, 'error': {'code': 'invalid_params', 'message': 'preview is not ready'}},
                   {'error': {'message': 'worktree.cleanup: TimeoutError: no response within 30s', 'transport': True}}]
        done = subprocess.CompletedProcess([], 0, stdout=json.dumps(payload), stderr='')
        with patch.object(reap_tasks.subprocess, 'run', return_value=done) as run:
            answers = reap_tasks.rpc_batch('cm-manager', [{}, {}, {}])
        cmd = run.call_args.args[0]
        self.assertEqual(cmd[1:3], ['--ssh', 'cm-manager'])
        self.assertIn('--batch', cmd)
        self.assertEqual(answers[0], {'phase': 'preview'})
        self.assertEqual(answers[1], {'error': 'preview is not ready', 'transient': False})
        self.assertTrue(answers[2]['transient'])

    def test_non_json_or_short_output_raises(self):
        for stdout in ('ssh: connect to host cm-manager: Connection timed out', '[]'):
            done = subprocess.CompletedProcess([], 255, stdout=stdout, stderr='')
            with patch.object(reap_tasks.subprocess, 'run', return_value=done):
                with self.assertRaises(reap_tasks.RemoteError):
                    reap_tasks.rpc_batch('cm-manager', [{}])

    def test_local_host_skips_ssh(self):
        done = subprocess.CompletedProcess([], 0, stdout='[{"ok": true, "result": {}}]', stderr='')
        with patch.object(reap_tasks.subprocess, 'run', return_value=done) as run:
            reap_tasks.rpc_batch('local', [{}])
        self.assertNotIn('--ssh', run.call_args.args[0])


class CmOpBatchTests(unittest.TestCase):
    def test_batch_answers_every_call_and_flags_dead_sockets(self):
        import importlib.machinery
        import importlib.util
        path = Path(__file__).resolve().parents[1] / 'scripts' / 'cm-op'
        loader = importlib.machinery.SourceFileLoader('cm_op', str(path))
        spec = importlib.util.spec_from_loader('cm_op', loader)
        cm_op = importlib.util.module_from_spec(spec)
        loader.exec_module(cm_op)
        with tempfile.TemporaryDirectory() as home:
            Path(home, '.cm').mkdir()
            Path(home, '.cm', 'operator-token').write_text('tok')
            env = {'HOME': home, 'CM_DAEMON_SOCKET': str(Path(home, 'missing.sock'))}
            with patch.dict(os.environ, env), patch('builtins.print') as printed:
                self.assertEqual(cm_op.main(['--timeout', '1', '--batch', json.dumps([['a', {}], ['b', {}]])]), 0)
            answers = json.loads(printed.call_args.args[0])
        self.assertEqual(len(answers), 2)
        self.assertTrue(all(a['error']['transport'] for a in answers))
        self.assertTrue(answers[0]['error']['message'].startswith('a: '))


class LineageRegisterTests(unittest.TestCase):
    def test_unchanged_record_is_not_rewritten(self):
        with tempfile.TemporaryDirectory() as base:
            repo = Path(base, 'repo')
            repo.mkdir()
            git = lambda *a: subprocess.check_output(['git', '-C', str(repo), *a], stderr=subprocess.DEVNULL)
            git('init', '-q', '-b', 'main')
            git('-c', 'user.name=t', '-c', 'user.email=t@example.invalid', 'commit', '-q', '--allow-empty', '-m', 'base')
            with patch.dict(os.environ, {'CM_LINEAGE_HOME': str(Path(base, 'cm')), 'CM_TUI_SESSION_ID': ''}):
                first = lineage.register(repo, task_ids=['t1'], inherit_session=False)
                with patch.object(lineage, 'atomic_json', wraps=lineage.atomic_json) as write:
                    self.assertEqual(lineage.register(repo, task_ids=['t1'], inherit_session=False), first)
                    write.assert_not_called()
                    lineage.register(repo, task_ids=['t2'], inherit_session=False)
                    write.assert_called_once()


if __name__ == '__main__':
    unittest.main()
