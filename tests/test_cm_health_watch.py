import importlib.machinery
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest import mock

loader = importlib.machinery.SourceFileLoader('cm_health_watch', str(Path(__file__).resolve().parents[1] / 'scripts/cm-health-watch'))
spec = importlib.util.spec_from_loader(loader.name, loader)
watch = importlib.util.module_from_spec(spec)
loader.exec_module(watch)
NOW = 1788807000


def account(remaining, status='active', **extra):
    window = dict(window='primary', used_percent=100-remaining,
                  recorded_at=NOW-30, reset_at=NOW+3600, window_minutes=10080,
                  credits_has=False, credits_unlimited=False, credits_balance=0)
    window.update(extra)
    return {'status': status, 'windows': [window]}


class PoolTests(unittest.TestCase):
    def test_one_exhausted_account_does_not_alert_while_another_has_quota(self):
        p = watch.pool_status([account(0, 'rate_limited'), account(86)], NOW)
        self.assertEqual(p['status'], 'healthy')
        self.assertEqual(p['best_headroom_percent'], 86)
        self.assertEqual(watch.quota_alert(p, 10), {})

    def test_zero_purchased_credits_is_not_zero_subscription_quota(self):
        self.assertEqual(watch.pool_status([account(86, credits_balance=0)], NOW)['status'], 'healthy')

    def test_warning_requires_every_usable_account_low(self):
        self.assertEqual(watch.pool_status([account(9), account(60)], NOW)['status'], 'healthy')
        self.assertEqual(watch.pool_status([account(9), account(8)], NOW)['status'], 'low')

    def test_each_account_is_limited_by_its_most_used_window(self):
        a = account(80)
        a['windows'].append(account(3)['windows'][0] | {'window': 'secondary'})
        self.assertEqual(watch.pool_status([a, account(0, 'rate_limited')], NOW)['status'], 'low')

    def test_full_exhaustion_reset_waits_for_all_blocking_windows(self):
        a = account(0, 'rate_limited', reset_at=NOW+100)
        a['windows'].append(account(0, reset_at=NOW+500)['windows'][0] | {'window': 'secondary'})
        p = watch.pool_status([a, account(0, 'quota_exceeded', reset_at=NOW+800)], NOW)
        self.assertEqual(p['status'], 'exhausted')
        self.assertEqual(p['next_reset_at'], NOW+500)
        self.assertIn('across all enabled accounts', watch.quota_alert(p, 10)['quota:exhausted'])

    def test_auth_or_disabled_accounts_are_not_mislabeled_as_quota_exhaustion(self):
        for rows in [[], [account(90, 'paused')], [account(90, 'reauth_required')], [account(0), account(90, 'deactivated')]]:
            p = watch.pool_status(rows, NOW)
            self.assertEqual(p['status'], 'unavailable')
            self.assertIn('not proof', watch.quota_alert(p, 10)['quota:unavailable'])

    def test_paid_capacity_prevents_false_exhaustion(self):
        for extra in [{'credits_unlimited': True}, {'credits_has': True, 'credits_balance': 12}]:
            self.assertEqual(watch.pool_status([account(0, **extra)], NOW)['status'], 'healthy')

    def test_stale_missing_future_or_post_reset_usage_never_becomes_zero(self):
        for row in [account(0, recorded_at=NOW-1300), account(0, recorded_at=NOW+10), account(0, reset_at=NOW-10), account(0, reset_at=NOW-100), {'status': 'active', 'windows': []}]:
            p = watch.pool_status([row, account(0)], NOW)
            self.assertEqual(p['status'], 'unknown')
            with self.assertRaises(watch.WatchError):
                watch.quota_alert(p, 10)
        self.assertEqual(watch.pool_status([account(0, recorded_at=NOW-1300), account(80)], NOW)['status'], 'healthy')

    def test_reader_uses_latest_window_rows_and_does_not_expose_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'pool.db'
            c = sqlite3.connect(path)
            c.executescript('''CREATE TABLE accounts(id TEXT,status TEXT,delete_requested_at TEXT,access_token_encrypted TEXT);
            CREATE TABLE usage_history(id INTEGER,account_id TEXT,window TEXT,used_percent REAL,recorded_at REAL,reset_at REAL,window_minutes INTEGER,credits_has INTEGER,credits_unlimited INTEGER,credits_balance REAL);
            INSERT INTO accounts VALUES('one','active',NULL,'SECRET'),('deleted','active','now','SECRET');''')
            for ident, used in [(1, 100), (2, 14)]:
                c.execute('INSERT INTO usage_history VALUES(?,?,?,?,?,?,?,?,?,?)', (ident,'one','primary',used,NOW-100+ident,NOW+3600,10080,0,0,0))
            c.commit();c.close()
            rows = watch.pool_accounts(path)
            self.assertEqual(len(rows), 1)
            self.assertNotIn('SECRET', str(rows))
            self.assertEqual(rows[0]['windows'][0]['used_percent'], 14)


class NotificationTests(unittest.TestCase):
    def setUp(self):
        self.state = {}
        self.send = self.enterContext(mock.patch.object(watch, 'notify'))

    def test_alert_deduplication_survives_saved_state_and_recovers_once(self):
        watch.deliver(self.state, 'pool', {'quota:low':'low'}, '/notify', True)
        self.state = json.loads(json.dumps(self.state))
        watch.deliver(self.state, 'pool', {'quota:low':'low'}, '/notify', True)
        watch.deliver(self.state, 'pool', {}, '/notify', True)
        watch.deliver(self.state, 'pool', {}, '/notify', True)
        self.assertEqual(self.send.call_count, 2)
        self.assertIn('Recovered', self.send.call_args.args[2])

    def test_escalation_is_not_a_recovery_and_failed_delivery_retries(self):
        watch.deliver(self.state, 'pool', {'quota:low':'low'}, '/notify', True)
        self.send.side_effect = watch.WatchError('failed')
        with self.assertRaises(watch.WatchError):
            watch.deliver(self.state, 'pool', {'quota:exhausted':'empty'}, '/notify', True)
        self.assertNotIn('quota:exhausted', self.state['active_alerts']['pool'])
        self.send.side_effect = None
        watch.deliver(self.state, 'pool', {'quota:exhausted':'empty'}, '/notify', True)
        self.assertEqual(self.state['active_alerts']['pool'], {'quota:exhausted':'empty'})
        self.assertFalse(any('Recovered' in c.args[2] for c in self.send.call_args_list))

    def test_dry_run_does_not_claim_a_notification_was_delivered(self):
        watch.deliver(self.state, 'pool', {'quota:low':'low'}, '/notify', False)
        self.send.assert_not_called()
        self.assertEqual(self.state['active_alerts']['pool'], {})

    def test_component_read_failures_preserve_prior_incident_and_alert_after_three(self):
        config = {'pool': {'database':'unused'}, 'notify_command':'/notify'}
        self.state = {'active_alerts': {'pool': {'quota:low':'low'}}}
        with mock.patch.object(watch, 'pool_accounts', side_effect=RuntimeError('PRIVATE')):
            for i in range(3):
                watch.run(config, Path('/unused'), self.state, NOW+i, True)
        self.assertEqual(self.send.call_count, 1)
        self.assertIn('quota:low', self.state['active_alerts']['pool'])
        self.assertNotIn('PRIVATE', str(self.state))

    def test_low_warning_has_recovery_hysteresis(self):
        config = {'pool': {'database':'unused'}, 'notify_command':'/notify'}
        with mock.patch.object(watch, 'pool_accounts') as accounts:
            for remaining in [9, 11, 9, 19, 21, 22]:
                accounts.return_value = [account(remaining)]
                watch.run(config, Path('/unused'), self.state, NOW, True)
        self.assertEqual(self.send.call_count, 2)


class FleetTests(unittest.TestCase):
    def setUp(self):
        self.health = dict(ok=True, mcp_ok=True, split=True, sessions=2, holder_sessions=2, breaker_state='running')
        self.task = dict(task_id='consumer', enabled=True, paused=False, in_flight=False,
                         last_run={'seq':1,'status':'done'}, next_fire_at=NOW-10000,
                         schedule={'kind':'consumer','queue':'q','window_secs':3600,'depth_threshold':0})

    def test_queued_items_within_window_are_not_stalled(self):
        queue = {'q': {'pending':6,'oldest_pending_at':NOW-3000}}
        self.assertEqual(watch.fleet_alerts(self.health,[self.task],queue,NOW), {})
        queue['q']['oldest_pending_at'] = NOW-5000
        self.assertIn('task:consumer', watch.fleet_alerts(self.health,[self.task],queue,NOW))

    def test_running_or_paused_tasks_are_not_forced_due(self):
        for change in [{'paused':True}, {'in_flight':True}, {'last_run':{'seq':1,'status':'running'}}]:
            self.assertEqual(watch.fleet_alerts(self.health,[self.task|change],{},NOW), {})

    def test_old_queue_waits_for_scheduled_fire_and_overdue_grace(self):
        # Reproduce the Sep 10 alert after a compact-only Done run. The
        # remaining queue items are hours old, but the scheduler is waiting.
        queue = {'q': {'pending':4,'oldest_pending_at':NOW-7*3600}}
        scheduled = NOW + 420
        task = self.task | {'next_fire_at':scheduled,'last_run':{'seq':400,'status':'done'}}
        for now in (NOW, scheduled, scheduled+600):
            with self.subTest(now=now):
                self.assertEqual(watch.fleet_alerts(self.health,[task],queue,now), {})
        self.assertIn('task:consumer', watch.fleet_alerts(self.health,[task],queue,scheduled+601))

    def test_depth_eligibility_does_not_bypass_scheduler_delay(self):
        queue = {'q': {'pending':6,'oldest_pending_at':NOW-5000}}
        task = self.task | {'next_fire_at':NOW+60,'schedule':self.task['schedule']|{'depth_threshold':5}}
        self.assertEqual(watch.fleet_alerts(self.health,[task],queue,NOW), {})
        self.assertIn('task:consumer', watch.fleet_alerts(self.health,[task],queue,NOW+661))

    def test_consumer_without_scheduled_timestamp_still_detects_overdue_work(self):
        queue = {'q': {'pending':4,'oldest_pending_at':NOW-5000}}
        self.assertIn('task:consumer', watch.fleet_alerts(self.health,[self.task|{'next_fire_at':None}],queue,NOW))

    def test_daemon_registry_mismatch_and_task_hold_are_distinct(self):
        alerts = watch.fleet_alerts(self.health|{'holder_sessions':3},[self.task|{'recovery_hold':{'run_seq':1}}],{},NOW)
        self.assertEqual(set(alerts), {'daemon','task:consumer'})
        self.assertIn('Other accounts may still have capacity', alerts['task:consumer'])


class MigrationTests(unittest.TestCase):
    def test_counts_completed_nonempty_and_natural_cycles_not_handover_or_failed(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);task=root/'continuous-tasks'/'test';task.mkdir(parents=True)
            (task/'state.json').write_text(json.dumps({'schedule':{'kind':'consumer','queue':'q'},'worktree_path':str(root)}))
            events=[]
            for seq,token,count,done in [(1,'before',4,True),(2,'handover',5,True),(3,'ft_sched_three',0,True),(4,'ft_sched_four',2,True),(5,'ft_sched_five',2,False),(6,'ft_sched_six',3,True)]:
                record={'seq':seq,'fire_token':token,'session_uid':'s','ts':NOW-90000,'detail':{'batch_count':count}}
                events.append(record|{'event':'fired','status':'running'})
                events.append(record|{'event':'report_done','status':'done' if done else 'failed'})
            (task/'runs.jsonl').write_text('\n'.join(map(json.dumps, events)))
            cfg={'test':{'baseline_seq':1,'completed':2,'scheduled':1,'nonempty':2,'observe_seconds':86400}}
            p=watch.migration_progress(root,cfg,NOW)['test']
            self.assertEqual(p['completed_seqs'], [3,4,6])
            self.assertEqual(p['nonempty_seqs'], [4,6])
            self.assertTrue(p['runtime_ready'])
            self.assertTrue(p['artifact_review_required'])
            self.assertFalse(watch.migration_progress(root,cfg,NOW-10000)['test']['runtime_ready'])

    def test_success_from_another_session_does_not_close_an_admission(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);task=root/'continuous-tasks'/'test';task.mkdir(parents=True)
            (task/'state.json').write_text(json.dumps({'schedule':{'kind':'periodic'},'worktree_path':str(root)}))
            (task/'runs.jsonl').write_text('\n'.join(map(json.dumps,[
                {'seq':2,'event':'fired','fire_token':'f','session_uid':'old'},
                {'seq':2,'event':'report_done','status':'done','fire_token':'f','session_uid':'new','ts':NOW}])))
            p=watch.migration_progress(root,{'test':{'baseline_seq':1}},NOW)['test']
            self.assertEqual(p['completed_seqs'], [])


if __name__ == '__main__':
    unittest.main()
