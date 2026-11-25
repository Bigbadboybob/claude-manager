"""Transactional lease reservations on a disposable PostgreSQL instance."""
import asyncio
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from types import SimpleNamespace

os.environ.setdefault('CM_DB_DSN', 'postgresql://unused')
os.environ.setdefault('CM_API_TOKEN', 'unused')
import asyncpg
from api import compute_leases as api
from dispatch import db

ROOT = Path(__file__).resolve().parents[1]
PG = Path('/usr/lib/postgresql/17/bin')


@unittest.skipUnless((PG / 'initdb').exists(), 'PostgreSQL 17 required')
class ComputeApiTests(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix='cm-compute-test-')
        cls.root = Path(cls.tmp.name)
        (cls.root / 'OWNER.md').write_text('owner: lease service test\ncreated: 2026-11-25\npurpose: disposable postgres\nkeep: none\ndelete-after: test\n')
        subprocess.run([str(PG / 'initdb'), '-D', str(cls.root / 'data'), '--no-locale', '--auth=trust'],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        subprocess.run([str(PG / 'pg_ctl'), '-D', str(cls.root / 'data'), '-l', str(cls.root / 'pg.log'),
                        '-o', f"-F -k {cls.root} -h '' -p 55483", '-w', 'start'],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        cfg = json.loads((ROOT / 'dispatch/compute-pool.example.json').read_text())
        cfg['boxes'] = [dict(cfg['boxes'][0], enabled=True, slots=['0,8'])]
        (cls.root / 'pool.json').write_text(json.dumps(cfg))
        cls.prev = os.environ.get('CM_COMPUTE_POOL_CONFIG')
        os.environ['CM_COMPUTE_POOL_CONFIG'] = str(cls.root / 'pool.json')

    @classmethod
    def tearDownClass(cls):
        subprocess.run([str(PG / 'pg_ctl'), '-D', str(cls.root / 'data'), '-m', 'fast', '-w', 'stop'],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        if cls.prev is None:
            os.environ.pop('CM_COMPUTE_POOL_CONFIG', None)
        else:
            os.environ['CM_COMPUTE_POOL_CONFIG'] = cls.prev
        cls.tmp.cleanup()

    async def asyncSetUp(self):
        self.pool = await asyncpg.create_pool(host=str(self.root), port=55483, database='postgres',
                                               min_size=1, max_size=4, init=db._init_connection)
        async with self.pool.acquire() as conn:
            await conn.execute('DROP TABLE IF EXISTS compute_lease_notices, compute_lease_samples, compute_lease_actions, compute_leases, compute_boxes, item_pushes, incarnations, members CASCADE')
            await conn.execute('CREATE TABLE members (id text PRIMARY KEY, primary_team_id uuid)')
            await conn.execute('CREATE TABLE incarnations (member_id text, pid text, ended_at timestamptz)')
            await conn.execute('''CREATE TABLE item_pushes (id bigserial PRIMARY KEY, daemon_id text, session_uid text,
                pid text, kind text, text text, dedupe text UNIQUE, delivered_at timestamptz, dropped_reason text)''')
            sql = (ROOT / 'sql/040_compute_leases.sql').read_text()
            await conn.execute(sql)
            await conn.execute(sql)
            await conn.execute("INSERT INTO compute_boxes (name, enabled, checked_at, evidence) VALUES ('d6', true, now(), '{}'::jsonb)")
        self.request = SimpleNamespace(app=SimpleNamespace(state=SimpleNamespace(pool=self.pool)))

    async def asyncTearDown(self):
        await self.pool.close()

    def body(self, uid):
        return api.LeaseRequest(actor=api.Actor(pid=f'agent:d:{uid}', daemon_id='d', session_uid=uid),
                                cpus=2, mem_gb=16, duration=300, priority='normal',
                                purpose='test a job', **{'class': 'burst'})

    async def test_concurrent_requests_reserve_once_and_release_before_next_grant(self):
        first, second = await asyncio.gather(api.request_lease(self.body('one'), self.request),
                                              api.request_lease(self.body('two'), self.request))
        self.assertEqual(sorted([first['status'], second['status']]), ['granting', 'queued'])
        action = (await api.executor_poll(self.request))['action']
        result = {'lease_id': str(action['lease_id']), 'configured': True, 'workdir': '/work/id'}
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result=result), self.request)
        owner = next(x for x in (self.body('one'), self.body('two'))
                     if str(x.actor.pid).endswith(str('one' if first['status'] == 'granting' else 'two')))
        active = first if first['status'] == 'granting' else second
        await api.release(api.LeaseChange(actor=owner.actor, lease_id=active['lease_id']), self.request)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM compute_leases WHERE status='queued'"), 1)
        action = (await api.executor_poll(self.request))['action']
        self.assertEqual(action['kind'], 'release')
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'],
            result={'lease_id': str(action['lease_id']), 'released': True}), self.request)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM compute_leases WHERE status='granting'"), 1)

    async def test_configured_box_stays_off_until_confirmed_drained(self):
        async with self.pool.acquire() as conn:
            await conn.execute('DELETE FROM compute_boxes')
        result = await api.request_lease(self.body('one'), self.request)
        self.assertEqual(result['status'], 'queued')
        status = await api.status(api.StatusRequest(actor=self.body('one').actor), self.request)
        self.assertFalse(status['pool'][0]['enabled'])
        self.assertEqual(status['queue'][0]['purpose'], 'test a job')
        self.assertEqual(status['queue'][0]['owner'], 'agent:d:one')
        with self.assertRaises(api.HTTPException):
            await api.executor_enable(api.BoxEnable(box='d6', local_drained=True, node_drained=False), self.request)
        await api.executor_enable(api.BoxEnable(box='d6', local_drained=True, node_drained=True), self.request)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', result['lease_id']), 'granting')

    async def test_refused_extension_still_acknowledges_idle_notice(self):
        owner = self.body('one')
        result = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(action['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        await api.request_lease(self.body('two'), self.request)
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE compute_leases SET last_activity_at=now()-interval '3 hours' WHERE id=$1", result['lease_id'])
        with self.assertRaises(api.HTTPException) as exc:
            await api.extend(api.LeaseChange(actor=owner.actor, lease_id=result['lease_id'], duration=60), self.request)
        self.assertEqual(exc.exception.status_code, 409)
        async with self.pool.acquire() as conn:
            self.assertTrue(await conn.fetchval("SELECT last_activity_at > now()-interval '1 minute' FROM compute_leases WHERE id=$1", result['lease_id']))

    async def test_cancel_during_grant_keeps_slot_until_node_teardown(self):
        owner = self.body('one')
        result = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        cancelled = await api.release(api.LeaseChange(actor=owner.actor, lease_id=result['lease_id']), self.request)
        self.assertEqual(cancelled['status'], 'release_requested')
        waiting = await api.request_lease(self.body('two'), self.request)
        self.assertEqual(waiting['status'], 'queued')
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(action['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', result['lease_id']), 'releasing')
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', waiting['lease_id']), 'queued')

    async def test_reclaim_waits_for_observed_final_notice(self):
        from datetime import datetime, timedelta, timezone
        owner = self.body('one')
        granted = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(action['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        await api.request_lease(self.body('two'), self.request)
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['policy'].update(idle_threshold_seconds=60, idle_answer_seconds=60,
                             sample_max_gap_seconds=15, notice_expiry_guard_seconds=30)
        cfg_path.write_text(json.dumps(cfg))
        try:
            now = datetime.now(timezone.utc)
            async with self.pool.acquire() as conn, conn.transaction():
                await conn.execute("UPDATE compute_leases SET last_activity_at=$2, expires_at=$3 WHERE id=$1",
                    granted['lease_id'], now-timedelta(minutes=3), now+timedelta(hours=1))
                row = await conn.fetchrow('SELECT * FROM compute_leases WHERE id=$1', granted['lease_id'])
                for age in (130, 65):
                    await api._notice_action(conn, row, 'idle_notice')
                    notice = await conn.fetchrow('SELECT action_id,push_id FROM compute_lease_notices WHERE sent_at IS NULL ORDER BY push_id DESC LIMIT 1')
                    await conn.execute("UPDATE compute_lease_actions SET status='done' WHERE id=$1", notice['action_id'])
                    await conn.execute('UPDATE compute_lease_notices SET sent_at=$2 WHERE action_id=$1', notice['action_id'], now-timedelta(seconds=age))
                    await conn.execute('UPDATE item_pushes SET delivered_at=$2 WHERE id=$1', notice['push_id'], now-timedelta(seconds=age))
                for i in range(7):
                    await conn.execute('INSERT INTO compute_lease_samples (lease_id,sampled_at,usage_usec) VALUES ($1,$2,$3)',
                        granted['lease_id'], now-timedelta(seconds=60-i*10), i*1_000_000)
            await api.sample(api.SampleReceipt(lease_id=granted['lease_id'], cpu_usage_usec=6_000_000), self.request)
            await api.executor_poll(self.request)
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', granted['lease_id']), 'active')
                push = await conn.fetchval("SELECT n.push_id FROM compute_lease_notices n JOIN compute_lease_actions a ON a.id=n.action_id WHERE a.kind='reclaim_notice'")
                self.assertIsNotNone(push)
                await conn.execute('UPDATE item_pushes SET delivered_at=now() WHERE id=$1', push)
            release = (await api.executor_poll(self.request))['action']
            self.assertEqual(release['kind'], 'release')
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', granted['lease_id']), 'releasing')
        finally:
            cfg_path.write_text(original)

    async def test_dropped_notice_never_counts_as_delivered(self):
        result = await api.request_lease(self.body('one'), self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(action['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        async with self.pool.acquire() as conn, conn.transaction():
            row = await conn.fetchrow('SELECT * FROM compute_leases WHERE id=$1', result['lease_id'])
            await api._notice_action(conn, row, 'idle_notice')
            push = await conn.fetchval("SELECT push_id FROM compute_lease_notices WHERE lease_id=$1", result['lease_id'])
            await conn.execute("UPDATE item_pushes SET delivered_at=now(), dropped_reason='not observed' WHERE id=$1", push)
        await api.executor_poll(self.request)
        async with self.pool.acquire() as conn:
            notice = await conn.fetchrow("SELECT n.sent_at,a.status,a.result FROM compute_lease_notices n JOIN compute_lease_actions a ON a.id=n.action_id WHERE n.push_id=$1", push)
            self.assertIsNone(notice['sent_at'])
            self.assertEqual(notice['status'], 'done')
            self.assertEqual(json.loads(notice['result']), {'delivered': False})

    async def test_gate_priority_is_visible_and_audited(self):
        body = self.body('gate')
        body.priority = 'v221-gate'
        body.purpose = 'EP ruling 123'
        with self.assertLogs('cm.api.compute', level='INFO') as logs:
            await api.request_lease(body, self.request)
        self.assertIn('agent:d:gate', logs.output[0])
        self.assertIn('EP ruling 123', logs.output[0])
        self.assertIn('v221-gate', logs.output[0])

    async def test_old_heartbeat_ack_cannot_confirm_lease_delivery(self):
        from datetime import datetime, timezone
        from dispatch import items_db
        async with self.pool.acquire() as conn:
            ident = await conn.fetchval("""INSERT INTO item_pushes
                (daemon_id,session_uid,pid,kind,text,dedupe)
                VALUES ('d','one','agent:d:one','compute_lease','notice','notice-1') RETURNING id""")
            now = datetime.now(timezone.utc)
            await items_db._ack_pushes(conn, 'd', [ident], [], now)
            self.assertIsNone(await conn.fetchval('SELECT delivered_at FROM item_pushes WHERE id=$1', ident))
            await items_db._ack_pushes(conn, 'other', [], [ident], now)
            self.assertIsNone(await conn.fetchval('SELECT delivered_at FROM item_pushes WHERE id=$1', ident))
            await items_db._ack_pushes(conn, 'd', [], [ident], now)
            self.assertIsNotNone(await conn.fetchval('SELECT delivered_at FROM item_pushes WHERE id=$1', ident))
