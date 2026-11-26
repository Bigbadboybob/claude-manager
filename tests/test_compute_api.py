"""Transactional lease reservations on a disposable PostgreSQL instance."""
import asyncio
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock, patch

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
            await conn.execute('DROP TABLE IF EXISTS compute_lease_sampling_epochs, compute_lease_notices, compute_lease_samples, compute_lease_actions, compute_leases, compute_boxes, item_pushes, incarnations, members CASCADE')
            await conn.execute('CREATE TABLE members (id text PRIMARY KEY, primary_team_id uuid)')
            await conn.execute('CREATE TABLE incarnations (member_id text, pid text, ended_at timestamptz)')
            await conn.execute('''CREATE TABLE item_pushes (id bigserial PRIMARY KEY, daemon_id text, session_uid text,
                pid text, kind text, text text, dedupe text UNIQUE, delivered_at timestamptz, dropped_reason text)''')
            sql = (ROOT / 'sql/040_compute_leases.sql').read_text()
            await conn.execute(sql)
            await conn.execute(sql)
            answer_sql = (ROOT / 'sql/041_compute_notice_answers.sql').read_text()
            await conn.execute(answer_sql)
            await conn.execute(answer_sql)
            justification_sql = (ROOT / 'sql/042_compute_lease_justification.sql').read_text()
            await conn.execute(justification_sql)
            await conn.execute(justification_sql)
            history_sql = (ROOT / 'sql/043_compute_box_ever_enabled.sql').read_text()
            await conn.execute(history_sql)
            await conn.execute(history_sql)
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

    async def test_session_capped_waiter_does_not_block_other_grants_or_extensions(self):
        import uuid
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'][0]['slots'] = [f'{i},{i+12}' for i in range(12)]
        cfg_path.write_text(json.dumps(cfg))
        try:
            async with self.pool.acquire() as conn:
                for i in range(8):
                    await conn.execute("""INSERT INTO compute_leases
                        (id,owner,purpose,priority,class,cpus,mem_gb,duration_seconds,status,box,slot_ids,granted_at,expires_at)
                        VALUES ($1,'agent:d:a','running','normal','burst',2,16,300,'active','d6',$2,now(),now()+interval '5 minutes')""",
                        uuid.uuid4(), [cfg['boxes'][0]['slots'][i]])
            a = await api.request_lease(self.body('a'), self.request)
            self.assertEqual(a['status'], 'queued')
            b_body = self.body('b')
            b = await api.request_lease(b_body, self.request)
            self.assertEqual(b['status'], 'granting')
            action = (await api.executor_poll(self.request))['action']
            self.assertEqual(action['lease_id'], b['lease_id'])
            await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id': str(b['lease_id']), 'configured': True, 'workdir': '/work/b'}), self.request)
            result = await api.extend(api.LeaseChange(actor=b_body.actor, lease_id=b['lease_id'], duration=60), self.request)
            self.assertEqual(result['status'], 'extending')
        finally:
            cfg_path.write_text(original)

    async def test_changed_box_config_skips_mismatched_reservation_without_freeing_capacity(self):
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'][0]['slots'] = ['0,8', '1,9']
        cfg_path.write_text(json.dumps(cfg))
        try:
            owner = self.body('owner')
            owner.cpus, owner.mem_gb = 4, 32
            granted = await api.request_lease(owner, self.request)
            action = (await api.executor_poll(self.request))['action']
            await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id': str(granted['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
            cfg['boxes'][0]['slots'] = ['0,8']
            cfg_path.write_text(json.dumps(cfg))
            with self.assertLogs('cm.compute.scheduler', level='ERROR') as logs:
                waiting = await api.request_lease(self.body('waiting'), self.request)
                self.assertEqual(waiting['status'], 'queued')
                self.assertIsNone((await api.executor_poll(self.request))['action'])
            self.assertIn('skipping box d6', '\n'.join(logs.output))
            self.assertEqual(sum('skipping box d6' in line for line in logs.output), 1)
            status = await api.status(api.StatusRequest(actor=owner.actor), self.request)
            self.assertEqual(status['pool'][0]['occupied_slots'], 2)
            self.assertTrue(status['pool'][0]['quarantined'])
            self.assertIn('does not match configured slots', status['pool'][0]['quarantine_reason'])
        finally:
            cfg_path.write_text(original)

    async def test_release_cleanup_results_are_visible_only_to_owner(self):
        async def finish(owner, cleanup):
            body = self.body(owner)
            lease = await api.request_lease(body, self.request)
            action = (await api.executor_poll(self.request))['action']
            await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id': str(lease['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
            await api.release(api.LeaseChange(actor=body.actor, lease_id=lease['lease_id']), self.request)
            action = (await api.executor_poll(self.request))['action']
            await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id': str(lease['lease_id']), 'released': True, **cleanup}), self.request)
            return body, lease['lease_id']

        owner, measured = await finish('owner', {'workdir_bytes': 123})
        _, skipped = await finish('owner', {'cleanup_skipped': 'unsafe workdir'})
        _, private = await finish('other', {'workdir_bytes': 456})
        # Accept both JSONB objects and the JSON-encoded strings written by
        # earlier executor receipts.
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE compute_lease_actions SET result=$2::jsonb WHERE lease_id=$1 AND kind='release'",
                               measured, {'lease_id': str(measured), 'released': True, 'workdir_bytes': 123})
        result = await api.status(api.StatusRequest(actor=owner.actor), self.request)
        recent = {lease['id']: lease for lease in result['leases']}
        self.assertEqual(set(recent), {measured, skipped})
        self.assertIsNotNone(recent[measured]['released_at'])
        self.assertEqual(recent[measured]['workdir_bytes'], 123)
        self.assertIsNone(recent[measured]['cleanup_skipped'])
        self.assertIsNotNone(recent[skipped]['released_at'])
        self.assertEqual(recent[skipped]['cleanup_skipped'], 'unsafe workdir')
        self.assertIsNone(recent[skipped]['workdir_bytes'])
        selected = await api.status(api.StatusRequest(actor=owner.actor, lease_id=skipped), self.request)
        self.assertEqual(next(x for x in selected['leases'] if x['id'] == skipped)['cleanup_skipped'], 'unsafe workdir')
        with self.assertRaises(api.HTTPException) as exc:
            await api.status(api.StatusRequest(actor=owner.actor, lease_id=private), self.request)
        self.assertEqual(exc.exception.status_code, 404)

    async def test_expired_grant_release_cleanup_is_visible(self):
        owner = self.body('owner')
        lease = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'expired': True, 'released': True,
            'cleanup_skipped': 'invalid workdir root'}), self.request)
        status = await api.status(api.StatusRequest(actor=owner.actor, lease_id=lease['lease_id']), self.request)
        result = next(x for x in status['leases'] if x['id'] == lease['lease_id'])
        self.assertIsNotNone(result['released_at'])
        self.assertEqual(result['status'], 'failed')
        self.assertEqual(result['error'], 'grant expired before activation')
        self.assertEqual(result['cleanup_skipped'], 'invalid workdir root')
        self.assertIsNone(result['workdir_bytes'])

    async def test_retryable_node_error_is_recorded_and_expired_grant_fails_and_notifies(self):
        owner = self.body('owner')
        lease = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        result = await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'retryable': True, 'error': 'getpwnam: unknown user'}), self.request)
        self.assertTrue(result['retryable'])
        async with self.pool.acquire() as conn:
            row = await conn.fetchrow('SELECT status,error FROM compute_leases WHERE id=$1', lease['lease_id'])
            self.assertEqual(row['status'], 'granting')
            self.assertIn('getpwnam: unknown user', row['error'])
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_lease_actions WHERE id=$1', action['action_id']), 'claimed')
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'expired': True, 'released': True}), self.request)
        status = await api.status(api.StatusRequest(actor=owner.actor, lease_id=lease['lease_id']), self.request)
        failed = next(x for x in status['leases'] if x['id'] == lease['lease_id'])
        self.assertEqual(failed['status'], 'failed')
        self.assertIn('getpwnam: unknown user', failed['error'])
        async with self.pool.acquire() as conn:
            push = await conn.fetchrow("SELECT pid,text FROM item_pushes WHERE kind='compute_lease'")
            self.assertEqual(push['pid'], owner.actor.pid)
            self.assertIn('getpwnam: unknown user', push['text'])

    async def test_release_requested_during_retryable_grant_releases_before_freeing(self):
        owner = self.body('owner')
        lease = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.release(api.LeaseChange(actor=owner.actor, lease_id=lease['lease_id']), self.request)
        receipt = api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'retryable': True, 'error': 'config error'})
        self.assertTrue((await api.executor_complete(receipt, self.request))['releasing'])
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', lease['lease_id']), 'releasing')
            self.assertTrue(await conn.fetchval('SELECT slot_ids FROM compute_leases WHERE id=$1', lease['lease_id']))
        release = (await api.executor_poll(self.request))['action']
        self.assertEqual(release['kind'], 'release')
        await api.executor_complete(api.ActionReceipt(action_id=release['action_id'], result={
            'lease_id': str(lease['lease_id']), 'released': True}), self.request)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', lease['lease_id']), 'failed')

    async def test_expired_unconfirmed_release_quarantines_without_freeing(self):
        owner = self.body('owner')
        lease = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        await api.release(api.LeaseChange(actor=owner.actor, lease_id=lease['lease_id']), self.request)
        release = (await api.executor_poll(self.request))['action']
        receipt = api.ActionReceipt(action_id=release['action_id'], result={
            'lease_id': str(lease['lease_id']), 'retryable': True, 'error': 'broken config'})
        await api.executor_complete(receipt, self.request)
        async with self.pool.acquire() as conn:
            self.assertTrue(await conn.fetchval("SELECT enabled FROM compute_boxes WHERE name='d6'"))
            await conn.execute("UPDATE compute_leases SET expires_at=now()-interval '1 second' WHERE id=$1", lease['lease_id'])
        with self.assertLogs('cm.api.compute', level='WARNING') as logs:
            await api.executor_complete(receipt, self.request)
            await api.executor_complete(receipt, self.request)
        self.assertEqual(len(logs.output), 1)
        async with self.pool.acquire() as conn:
            row = await conn.fetchrow('SELECT status,slot_ids,error FROM compute_leases WHERE id=$1', lease['lease_id'])
            self.assertEqual(row['status'], 'releasing')
            self.assertTrue(row['slot_ids'])
            self.assertIn('broken config', row['error'])
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_lease_actions WHERE id=$1', release['action_id']), 'claimed')
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM item_pushes WHERE text LIKE '%release remains unconfirmed%'"), 1)
            self.assertFalse(await conn.fetchval("SELECT enabled FROM compute_boxes WHERE name='d6'"))
        await api.executor_complete(api.ActionReceipt(action_id=release['action_id'], result={
            'lease_id': str(lease['lease_id']), 'released': True}), self.request)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', lease['lease_id']), 'released')

    async def test_expired_uncertain_extension_quarantines_then_releases(self):
        owner = self.body('owner')
        lease = await api.request_lease(owner, self.request)
        grant = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=grant['action_id'], result={
            'lease_id': str(lease['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        await api.extend(api.LeaseChange(actor=owner.actor, lease_id=lease['lease_id'], duration=60), self.request)
        extend = (await api.executor_poll(self.request))['action']
        self.assertEqual(extend['kind'], 'extend')
        receipt = api.ActionReceipt(action_id=extend['action_id'], result={
            'lease_id': str(lease['lease_id']), 'retryable': True, 'error': 'ssh exit 255'})
        self.assertTrue((await api.executor_complete(receipt, self.request))['retryable'])
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', lease['lease_id']), 'active')
            await conn.execute("UPDATE compute_leases SET expires_at=now()-interval '1 second' WHERE id=$1", lease['lease_id'])
        self.assertTrue((await api.executor_complete(receipt, self.request))['releasing'])
        self.assertTrue((await api.executor_complete(receipt, self.request))['already_done'])
        async with self.pool.acquire() as conn:
            row = await conn.fetchrow('SELECT status,slot_ids,error FROM compute_leases WHERE id=$1', lease['lease_id'])
            self.assertEqual(row['status'], 'releasing')
            self.assertTrue(row['slot_ids'])
            self.assertIn('ssh exit 255', row['error'])
            self.assertFalse(await conn.fetchval("SELECT enabled FROM compute_boxes WHERE name='d6'"))
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM item_pushes WHERE text LIKE '%extension remains unconfirmed%'"), 1)
        release = (await api.executor_poll(self.request))['action']
        self.assertEqual(release['kind'], 'release')
        await api.executor_complete(api.ActionReceipt(action_id=release['action_id'], result={
            'lease_id': str(lease['lease_id']), 'released': True}), self.request)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', lease['lease_id']), 'released')

    async def test_expired_extension_and_failed_release_share_outage_notice(self):
        owner = self.body('owner')
        lease = await api.request_lease(owner, self.request)
        grant = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=grant['action_id'], result={
            'lease_id': str(lease['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        await api.extend(api.LeaseChange(actor=owner.actor, lease_id=lease['lease_id'], duration=60), self.request)
        extension = (await api.executor_poll(self.request))['action']
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE compute_leases SET expires_at=now()-interval '1 second' WHERE id=$1", lease['lease_id'])
        with self.assertLogs('cm.api.compute', level='WARNING') as logs:
            await api.executor_complete(api.ActionReceipt(action_id=extension['action_id'], result={
                'lease_id': str(lease['lease_id']), 'retryable': True, 'error': 'IAP unavailable'}), self.request)
            release = (await api.executor_poll(self.request))['action']
            await api.executor_complete(api.ActionReceipt(action_id=release['action_id'], result={
                'lease_id': str(lease['lease_id']), 'retryable': True, 'error': 'IAP unavailable'}), self.request)
        self.assertEqual(len(logs.output), 1)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT count(*) FROM item_pushes WHERE pid=$1', owner.actor.pid), 1)
            evidence = api._json_object(await conn.fetchval("SELECT evidence FROM compute_boxes WHERE name='d6'"))
            self.assertEqual(len(evidence['quarantine_reasons']), 2)

    async def test_transport_threshold_quarantines_and_success_resets_count(self):
        owner = self.body('owner')
        lease = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        fail = api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'retryable': True, 'transport_error': True, 'error': 'IAP down'})
        await api.executor_complete(fail, self.request)
        async with self.pool.acquire() as conn:
            self.assertTrue(await conn.fetchval("SELECT enabled FROM compute_boxes WHERE name='d6'"))
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        async with self.pool.acquire() as conn:
            self.assertNotIn('consecutive_transport_failures', api._json_object(await conn.fetchval("SELECT evidence FROM compute_boxes WHERE name='d6'")))
        await api.release(api.LeaseChange(actor=owner.actor, lease_id=lease['lease_id']), self.request)
        release = (await api.executor_poll(self.request))['action']
        fail = api.ActionReceipt(action_id=release['action_id'], result={
            'lease_id': str(lease['lease_id']), 'retryable': True, 'transport_error': True, 'error': 'IAP down'})
        await api.executor_complete(fail, self.request)
        await api.executor_complete(fail, self.request)
        async with self.pool.acquire() as conn:
            self.assertTrue(await conn.fetchval("SELECT enabled FROM compute_boxes WHERE name='d6'"))
        await api.executor_complete(fail, self.request)
        async with self.pool.acquire() as conn:
            self.assertFalse(await conn.fetchval("SELECT enabled FROM compute_boxes WHERE name='d6'"))
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', lease['lease_id']), 'releasing')
            self.assertEqual(await conn.fetchval('SELECT count(*) FROM item_pushes WHERE pid=$1', owner.actor.pid), 1)

    async def test_release_wins_pending_extension_after_uncertain_result(self):
        owner = self.body('owner')
        lease = await api.request_lease(owner, self.request)
        grant = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=grant['action_id'], result={
            'lease_id': str(lease['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        await api.extend(api.LeaseChange(actor=owner.actor, lease_id=lease['lease_id'], duration=60), self.request)
        extend = (await api.executor_poll(self.request))['action']
        self.assertEqual((await api.release(api.LeaseChange(actor=owner.actor, lease_id=lease['lease_id']), self.request))['status'], 'release_requested')
        receipt = api.ActionReceipt(action_id=extend['action_id'], result={
            'lease_id': str(lease['lease_id']), 'retryable': True, 'transport_error': True, 'error': 'IAP down'})
        self.assertTrue((await api.executor_complete(receipt, self.request))['releasing'])
        release = (await api.executor_poll(self.request))['action']
        self.assertEqual(release['kind'], 'release')
        async with self.pool.acquire() as conn:
            self.assertTrue(await conn.fetchval('SELECT slot_ids FROM compute_leases WHERE id=$1', lease['lease_id']))

    async def test_extension_refusal_records_reason_without_quarantine(self):
        owner = self.body('owner')
        lease = await api.request_lease(owner, self.request)
        grant = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=grant['action_id'], result={
            'lease_id': str(lease['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        await api.extend(api.LeaseChange(actor=owner.actor, lease_id=lease['lease_id'], duration=60), self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'refused': True, 'error': 'expiry outside configured window'}), self.request)
        async with self.pool.acquire() as conn:
            row = await conn.fetchrow('SELECT status,error FROM compute_leases WHERE id=$1', lease['lease_id'])
            self.assertEqual(row['status'], 'active')
            self.assertIn('expiry outside configured window', row['error'])
            self.assertTrue(await conn.fetchval("SELECT enabled FROM compute_boxes WHERE name='d6'"))
            self.assertEqual(await conn.fetchval('SELECT count(*) FROM item_pushes WHERE pid=$1', owner.actor.pid), 1)

    async def test_enable_rejects_node_duration_below_pool_limit(self):
        with self.assertRaises(api.HTTPException) as exc:
            await api.executor_enable(api.BoxEnable(box='d6', local_drained=True, node_drained=True,
                node_max_slots=1, node_max_duration_seconds=43199), self.request)
        self.assertEqual(exc.exception.status_code, 409)
        self.assertIn('max_duration_seconds', exc.exception.detail)
        async with self.pool.acquire() as conn:
            self.assertFalse(await conn.fetchval("SELECT enabled FROM compute_boxes WHERE name='d6'"))

    async def test_retryable_grant_error_clears_on_success(self):
        owner = self.body('owner')
        lease = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'retryable': True, 'error': 'temporary error'}), self.request)
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        async with self.pool.acquire() as conn:
            row = await conn.fetchrow('SELECT status,error FROM compute_leases WHERE id=$1', lease['lease_id'])
            self.assertEqual(row['status'], 'active')
            self.assertIsNone(row['error'])

    async def test_expired_retryable_grant_quarantines_and_notifies_once_without_freeing(self):
        owner = self.body('owner')
        lease = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE compute_leases SET expires_at=now()-interval '1 second' WHERE id=$1", lease['lease_id'])
            # An operator may disable the box while the uncertain grant is in flight.
            await conn.execute("UPDATE compute_boxes SET enabled=false WHERE name='d6'")
        receipt = api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'retryable': True, 'error': 'invalid node config'})
        await api.executor_complete(receipt, self.request)
        await api.executor_complete(receipt, self.request)
        async with self.pool.acquire() as conn:
            row = await conn.fetchrow('SELECT status,error,slot_ids FROM compute_leases WHERE id=$1', lease['lease_id'])
            self.assertEqual(row['status'], 'granting')
            self.assertIn('invalid node config', row['error'])
            self.assertTrue(row['slot_ids'])
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_lease_actions WHERE id=$1', action['action_id']), 'claimed')
            pushes = await conn.fetch("SELECT pid,text FROM item_pushes WHERE kind='compute_lease'")
            self.assertEqual(len(pushes), 1)
            self.assertEqual(pushes[0]['pid'], owner.actor.pid)
            self.assertIn('invalid node config', pushes[0]['text'])
        status = await api.status(api.StatusRequest(actor=owner.actor, lease_id=lease['lease_id']), self.request)
        self.assertTrue(status['pool'][0]['quarantined'])
        self.assertFalse(status['pool'][0]['enabled'])
        with self.assertRaises(api.HTTPException) as exc:
            await api.request_lease(self.body('other'), self.request)
        self.assertEqual(exc.exception.status_code, 409)

    async def test_quarantine_preserves_double_encoded_existing_reason(self):
        lease = await api.request_lease(self.body('owner'), self.request)
        action = (await api.executor_poll(self.request))['action']
        async with self.pool.acquire() as conn:
            # The real codec encodes this string as a JSON string in jsonb.
            await conn.execute("UPDATE compute_boxes SET evidence=$1::jsonb WHERE name='d6'",
                               json.dumps({'quarantine_reason': 'original capacity reason'}))
            await conn.execute("UPDATE compute_leases SET expires_at=now()-interval '1 second' WHERE id=$1", lease['lease_id'])
        receipt = api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'retryable': True, 'error': 'broken config'})
        with patch.object(api.logger, 'warning') as warning:
            await api.executor_complete(receipt, self.request)
            await api.executor_complete(receipt, self.request)
        warning.assert_called_once()
        async with self.pool.acquire() as conn:
            evidence = api._json_object(await conn.fetchval("SELECT evidence FROM compute_boxes WHERE name='d6'"))
            self.assertEqual(evidence['quarantine_reason'], 'original capacity reason')
            self.assertEqual(evidence['quarantine_reasons'], ['original capacity reason', 'expired grant remains unconfirmed after retryable node error'])
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM item_pushes WHERE kind='compute_lease'"), 1)

    async def test_expiry_notices_deduplicate_legacy_and_new_jsonb_payloads(self):
        owner = self.body('owner')
        lease = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(lease['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        async with self.pool.acquire() as conn:
            epoch = await conn.fetchval("UPDATE compute_leases SET expires_at=date_trunc('second', now())+interval '2 minutes' WHERE id=$1 RETURNING extract(epoch from expires_at)::bigint", lease['lease_id'])
            # Through the actual codec, this is a JSON string inside JSONB.
            await conn.execute("INSERT INTO compute_lease_actions (id,lease_id,kind,payload) VALUES (gen_random_uuid(),$1,'expiry_notice',$2::jsonb)", lease['lease_id'], json.dumps({'expires_epoch': epoch}))
        for _ in range(3):
            await api.executor_poll(self.request)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM compute_lease_actions WHERE lease_id=$1 AND kind='expiry_notice'", lease['lease_id']), 1)
            await conn.execute("UPDATE compute_leases SET expires_at=expires_at+interval '10 seconds' WHERE id=$1", lease['lease_id'])
        for _ in range(3):
            await api.executor_poll(self.request)
        async with self.pool.acquire() as conn:
            rows = await conn.fetch("SELECT payload, jsonb_typeof(payload) AS json_type FROM compute_lease_actions WHERE lease_id=$1 AND kind='expiry_notice' ORDER BY created_at", lease['lease_id'])
            self.assertEqual([row['json_type'] for row in rows], ['string', 'object'])
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM item_pushes WHERE kind='compute_lease'"), 1)

    async def test_recent_history_is_bounded_but_explicit_old_lease_is_available(self):
        import uuid
        owner = self.body('owner')
        ids = [uuid.uuid4() for _ in range(api.RECENT_LEASE_LIMIT + 1)]
        async with self.pool.acquire() as conn:
            for i, ident in enumerate(ids):
                await conn.execute("""INSERT INTO compute_leases
                    (id,owner,purpose,priority,class,cpus,mem_gb,duration_seconds,status,released_at)
                    VALUES ($1,$2,'history','normal','burst',2,16,300,'released',now()+make_interval(secs => $3))""",
                    ident, owner.actor.pid, i)
        result = await api.status(api.StatusRequest(actor=owner.actor), self.request)
        self.assertEqual(len(result['leases']), api.RECENT_LEASE_LIMIT)
        self.assertNotIn(ids[0], [lease['id'] for lease in result['leases']])
        result = await api.status(api.StatusRequest(actor=owner.actor, lease_id=ids[0]), self.request)
        oldest = next(lease for lease in result['leases'] if lease['id'] == ids[0])
        self.assertIsNone(oldest['cleanup_skipped'])
        self.assertIsNone(oldest['workdir_bytes'])

    async def test_configured_box_stays_off_until_confirmed_drained(self):
        async with self.pool.acquire() as conn:
            await conn.execute('DELETE FROM compute_boxes')
        with self.assertRaises(api.HTTPException) as exc:
            await api.request_lease(self.body('one'), self.request)
        self.assertEqual(exc.exception.status_code, 409)
        self.assertEqual(exc.exception.detail, 'no enabled box fits this request')
        status = await api.status(api.StatusRequest(actor=self.body('one').actor), self.request)
        self.assertFalse(status['pool'][0]['enabled'])
        self.assertEqual(status['queue'], [])
        with self.assertRaises(api.HTTPException):
            await api.executor_enable(api.BoxEnable(box='d6', local_drained=True, node_drained=False, node_max_slots=1, node_max_duration_seconds=43200), self.request)
        await api.executor_enable(api.BoxEnable(box='d6', local_drained=True, node_drained=True, node_max_slots=1, node_max_duration_seconds=43200), self.request)
        result = await api.request_lease(self.body('one'), self.request)
        self.assertEqual(result['status'], 'granting')
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', result['lease_id']), 'granting')

    async def test_enabled_box_too_small_is_refused_until_wide_box_enabled(self):
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'].append({'name': 'wide', 'ssh_target': 'wide', 'enabled': True,
                             'slots': ['0,8', '1,9']})
        cfg_path.write_text(json.dumps(cfg))
        try:
            body = self.body('wide')
            body.cpus, body.mem_gb = 4, 32
            with self.assertRaises(api.HTTPException) as exc:
                await api.request_lease(body, self.request)
            self.assertEqual(exc.exception.status_code, 409)
            self.assertEqual(exc.exception.detail, 'no enabled box fits this request')
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval('SELECT count(*) FROM compute_leases'), 0)
                await conn.execute("INSERT INTO compute_boxes (name,enabled,checked_at,evidence) VALUES ('wide',true,now(),'{}'::jsonb)")
            self.assertEqual((await api.request_lease(body, self.request))['status'], 'granting')
        finally:
            cfg_path.write_text(original)

    async def test_enable_rejects_pool_capacity_larger_than_node(self):
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'][0]['slots'] = ['0,8', '1,9']
        cfg_path.write_text(json.dumps(cfg))
        try:
            # A previously enabled box must be disabled if revalidation
            # discovers that its configured pool exceeds node capacity.
            with self.assertRaises(api.HTTPException) as exc:
                await api.executor_enable(api.BoxEnable(box='d6', local_drained=True, node_drained=True,
                                                       node_max_slots=1, node_max_duration_seconds=43200), self.request)
            self.assertEqual(exc.exception.status_code, 409)
            self.assertIn('exceed node max_slots', exc.exception.detail)
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval('SELECT count(*) FROM compute_boxes WHERE enabled'), 0)
            await api.executor_enable(api.BoxEnable(box='d6', local_drained=True, node_drained=True,
                                                   node_max_slots=2, node_max_duration_seconds=43200), self.request)
            async with self.pool.acquire() as conn:
                self.assertTrue(await conn.fetchval("SELECT enabled FROM compute_boxes WHERE name='d6'"))
        finally:
            cfg_path.write_text(original)

    async def test_refused_grant_fails_after_confirmed_release_and_notifies_owner(self):
        owner = self.body('one')
        granted = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        refusal = api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(granted['lease_id']), 'refused': True, 'error': 'invalid owner'})
        await api.executor_complete(refusal, self.request)
        await api.executor_complete(refusal, self.request)  # duplicate receipt is idempotent
        waiting = await api.request_lease(self.body('two'), self.request)
        self.assertEqual(waiting['status'], 'queued')
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', granted['lease_id']), 'releasing')
            self.assertIn('invalid owner', await conn.fetchval('SELECT error FROM compute_leases WHERE id=$1', granted['lease_id']))
            pushes = await conn.fetch("SELECT pid,text FROM item_pushes WHERE kind='compute_lease'")
            self.assertEqual(len(pushes), 1)
            self.assertEqual(pushes[0]['pid'], owner.actor.pid)
            self.assertIn('invalid owner', pushes[0]['text'])
        release = (await api.executor_poll(self.request))['action']
        self.assertEqual(release['kind'], 'release')
        await api.executor_complete(api.ActionReceipt(action_id=release['action_id'], result={
            'lease_id': str(granted['lease_id']), 'released': True, 'already_absent': True}), self.request)
        status = await api.status(api.StatusRequest(actor=owner.actor, lease_id=granted['lease_id']), self.request)
        failed = next(x for x in status['leases'] if x['id'] == granted['lease_id'])
        self.assertEqual(failed['status'], 'failed')
        self.assertIn('invalid owner', failed['error'])
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', waiting['lease_id']), 'granting')

    async def test_capacity_refusal_quarantines_box_until_explicit_clear(self):
        owner = self.body('one')
        granted = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        receipt = api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(granted['lease_id']), 'refused': True, 'error': 'box capacity exceeded'})
        with self.assertLogs('cm.api.compute', level='WARNING') as logs:
            await api.executor_complete(receipt, self.request)
            await api.executor_complete(receipt, self.request)
        self.assertEqual(len(logs.output), 1)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM item_pushes WHERE text LIKE '%box d6 quarantined%'"), 1)
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM item_pushes WHERE pid=$1", owner.actor.pid), 1)
            box = await conn.fetchrow("SELECT enabled,evidence FROM compute_boxes WHERE name='d6'")
            self.assertFalse(box['enabled'])
            self.assertEqual(api._json_object(box['evidence'])['quarantine_reason'], 'node refused grant: box capacity exceeded')
        status = await api.status(api.StatusRequest(actor=owner.actor, lease_id=granted['lease_id']), self.request)
        self.assertTrue(status['pool'][0]['quarantined'])
        self.assertEqual(status['pool'][0]['quarantine_reason'], 'node refused grant: box capacity exceeded')
        with self.assertRaises(api.HTTPException) as exc:
            await api.executor_enable(api.BoxEnable(box='d6', local_drained=True, node_drained=True,
                                                   node_max_slots=1, node_max_duration_seconds=43200), self.request)
        self.assertEqual(exc.exception.status_code, 409)
        self.assertIn('quarantined', exc.exception.detail)
        release = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=release['action_id'], result={
            'lease_id': str(granted['lease_id']), 'released': True, 'already_absent': True}), self.request)
        status = await api.status(api.StatusRequest(actor=owner.actor, lease_id=granted['lease_id']), self.request)
        self.assertEqual(next(x for x in status['leases'] if x['id'] == granted['lease_id'])['status'], 'failed')
        await api.executor_enable(api.BoxEnable(box='d6', local_drained=True, node_drained=True,
                                               node_max_slots=1, node_max_duration_seconds=43200, clear_quarantine=True), self.request)
        self.assertEqual((await api.request_lease(self.body('two'), self.request))['status'], 'granting')

    async def test_quarantine_notifies_configured_operator_once(self):
        config = self.root / 'pool.json'
        original = config.read_text()
        cfg = json.loads(original)
        cfg['policy']['quarantine_operator_pid'] = 'agent:operator-daemon:operator-session'
        config.write_text(json.dumps(cfg))
        try:
            lease = await api.request_lease(self.body('owner'), self.request)
            action = (await api.executor_poll(self.request))['action']
            async with self.pool.acquire() as conn:
                await conn.execute("UPDATE compute_leases SET expires_at=now()-interval '1 second' WHERE id=$1", lease['lease_id'])
            receipt = api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id': str(lease['lease_id']), 'retryable': True, 'error': 'bad config'})
            await api.executor_complete(receipt, self.request)
            await api.executor_complete(receipt, self.request)
            async with self.pool.acquire() as conn:
                rows = await conn.fetch("SELECT pid,text FROM item_pushes WHERE kind='compute_lease'")
                self.assertEqual(len(rows), 2)
                operator = next(row for row in rows if row['pid'] == cfg['policy']['quarantine_operator_pid'])
                self.assertIn('box d6 quarantined', operator['text'])
                self.assertEqual(await conn.fetchval("SELECT count(*) FROM item_pushes WHERE pid=$1", self.body('owner').actor.pid), 1)
        finally:
            config.write_text(original)

    async def test_new_quarantine_reason_is_recorded_and_operator_notified_once(self):
        config = self.root / 'pool.json'
        original = config.read_text()
        cfg = json.loads(original)
        cfg['policy']['quarantine_operator_pid'] = 'agent:ops:watcher'
        config.write_text(json.dumps(cfg))
        try:
            owner = self.body('owner')
            lease = await api.request_lease(owner, self.request)
            grant = (await api.executor_poll(self.request))['action']
            await api.executor_complete(api.ActionReceipt(action_id=grant['action_id'], result={
                'lease_id': str(lease['lease_id']), 'refused': True, 'error': 'box capacity exceeded'}), self.request)
            release = (await api.executor_poll(self.request))['action']
            async with self.pool.acquire() as conn:
                await conn.execute("UPDATE compute_leases SET expires_at=now()-interval '1 second' WHERE id=$1", lease['lease_id'])
            receipt = api.ActionReceipt(action_id=release['action_id'], result={
                'lease_id': str(lease['lease_id']), 'retryable': True, 'error': 'bad config'})
            await api.executor_complete(receipt, self.request)
            await api.executor_complete(receipt, self.request)
            result = await api.status(api.StatusRequest(actor=owner.actor, lease_id=lease['lease_id']), self.request)
            self.assertEqual(result['pool'][0]['quarantine_reasons'], [
                'node refused grant: box capacity exceeded',
                'expired release remains unconfirmed after retryable node error'])
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval("SELECT count(*) FROM item_pushes WHERE pid='agent:ops:watcher'"), 2)
        finally:
            config.write_text(original)

    async def test_multiple_expired_grants_warn_and_notify_operator_once_per_box(self):
        config = self.root / 'pool.json'
        original = config.read_text()
        cfg = json.loads(original)
        cfg['boxes'][0]['slots'] = ['0,8', '1,9']
        cfg['policy']['quarantine_operator_pid'] = 'agent:ops:watcher'
        config.write_text(json.dumps(cfg))
        try:
            leases = [await api.request_lease(self.body(name), self.request) for name in ('one', 'two')]
            actions = [(await api.executor_poll(self.request))['action'] for _ in leases]
            async with self.pool.acquire() as conn:
                await conn.execute("UPDATE compute_leases SET expires_at=now()-interval '1 second'")
            with self.assertLogs('cm.api.compute', level='WARNING') as logs:
                receipts = [api.ActionReceipt(action_id=action['action_id'], result={
                    'lease_id': str(action['lease_id']), 'retryable': True, 'error': 'bad config'})
                    for action in actions]
                arrived = 0
                barrier = asyncio.Event()
                original_lock = api._lock
                async def overlapping_lock(conn):
                    nonlocal arrived
                    arrived += 1
                    if arrived == 2:
                        barrier.set()
                    await asyncio.wait_for(barrier.wait(), timeout=5)
                    await original_lock(conn)
                with patch.object(api, '_lock', side_effect=overlapping_lock):
                    await asyncio.gather(*(api.executor_complete(receipt, self.request) for receipt in receipts))
                await asyncio.gather(*(api.executor_complete(receipt, self.request) for receipt in receipts))
            self.assertEqual(len(logs.output), 1)
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval("SELECT count(*) FROM item_pushes WHERE pid='agent:ops:watcher'"), 1)
                for name in ('one', 'two'):
                    self.assertEqual(await conn.fetchval("SELECT count(*) FROM item_pushes WHERE pid=$1", self.body(name).actor.pid), 1)
                self.assertEqual(await conn.fetchval("SELECT count(*) FROM compute_leases WHERE status='granting'"), 2)
        finally:
            config.write_text(original)

    async def test_pre_enable_remembers_quarantined_or_mismatched_boxes(self):
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE compute_boxes SET enabled=false, ever_enabled=true WHERE name='d6'")
        report = await api.executor_pre_enable(self.request)
        self.assertTrue(report['has_ever_enabled'])
        self.assertEqual(report['enabled_boxes'], [])
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE compute_boxes SET ever_enabled=false, evidence='{}'::jsonb WHERE name='d6'")
        self.assertFalse((await api.executor_pre_enable(self.request))['has_ever_enabled'])
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE compute_boxes SET evidence=$1::jsonb WHERE name='d6'", json.dumps({'quarantine_reason': 'old refusal'}))
        self.assertTrue((await api.executor_pre_enable(self.request))['has_ever_enabled'])

    async def test_pre_enable_reports_legacy_queue_without_modifying_rows(self):
        async with self.pool.acquire() as conn:
            await conn.execute('UPDATE compute_boxes SET enabled=false')
            big = await self._queue_for_team(conn, 'a', 8, 1)
            small = await self._queue_for_team(conn, 'b', 1, 2)
        result = await api.executor_pre_enable(self.request)
        self.assertEqual(result['queued_count'], 2)
        self.assertEqual(result['enabled_boxes'], [])
        rows = {row['lease_id']: row for row in result['queued']}
        self.assertTrue(rows[big]['missing_justification'])
        self.assertFalse(rows[small]['missing_justification'])
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval("SELECT count(*) FROM compute_leases WHERE status='queued'"), 2)
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'][0]['slots'] = [f'{i},{i+8}' for i in range(8)]
        cfg_path.write_text(json.dumps(cfg))
        try:
            async with self.pool.acquire() as conn:
                await conn.execute('UPDATE compute_boxes SET enabled=true')
            status = await api.status(api.StatusRequest(actor=self.body('a-1').actor, lease_id=big), self.request)
            lease = next(item for item in status['leases'] if item['id'] == big)
            self.assertTrue(lease['missing_justification'])
            self.assertEqual(lease['waiting'], 'justification required')
        finally:
            cfg_path.write_text(original)

    async def test_box_disabled_in_config_cannot_accept_requests(self):
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'][0]['enabled'] = False  # database row is still enabled
        cfg_path.write_text(json.dumps(cfg))
        try:
            with self.assertRaises(api.HTTPException) as exc:
                await api.request_lease(self.body('config-off'), self.request)
            self.assertEqual(exc.exception.status_code, 409)
            self.assertEqual(exc.exception.detail, 'no enabled box fits this request')
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval('SELECT count(*) FROM compute_leases'), 0)
        finally:
            cfg_path.write_text(original)

    async def test_configured_iap_prefix_preserves_job_arguments(self):
        import shlex
        fake_gcloud = self.root / 'gcloud stub'
        fake_gcloud.write_text('#!/usr/bin/python3\nimport json,sys\nprint(json.dumps(sys.argv[1:]))\n')
        fake_gcloud.chmod(0o700)
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        route = [str(fake_gcloud), 'compute', 'ssh', 'instance-name', '--project=prediction-market-scalper',
                 '--zone=us-east4-b', '--tunnel-through-iap']
        cfg['boxes'][0]['run_argv'] = route
        cfg_path.write_text(json.dumps(cfg))
        try:
            owner = self.body('prefix')
            granted = await api.request_lease(owner, self.request)
            action = (await api.executor_poll(self.request))['action']
            await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id': str(granted['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
            status = await api.status(api.StatusRequest(actor=owner.actor, lease_id=granted['lease_id']), self.request)
            lease = next(x for x in status['leases'] if x['id'] == granted['lease_id'])
            self.assertEqual(lease['ssh_target'], 'd6')
            job = ['/bin/echo', 'space value', "single'quote", '$(not-executed);`also-not`', '', 'line\n\n']
            command = lease['run_prefix'] + ' ' + shlex.join(job)
            output = subprocess.run(['/bin/sh', '-c', command], check=True, capture_output=True, text=True).stdout
            arguments = json.loads(output)
            self.assertEqual(arguments[:-2], route[1:])
            self.assertEqual(arguments[-2], '--command')
            self.assertEqual(shlex.split(arguments[-1]), ['sudo', '-n', '/usr/local/bin/cm-lease',
                             'run', '--lease-id', str(granted['lease_id']), '--', *job])
        finally:
            cfg_path.write_text(original)

    async def test_ssh_alias_prefix_remains_available_without_run_argv(self):
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'][0].pop('run_argv', None)
        cfg_path.write_text(json.dumps(cfg))
        try:
            owner = self.body('alias')
            granted = await api.request_lease(owner, self.request)
            action = (await api.executor_poll(self.request))['action']
            await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id': str(granted['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
            status = await api.status(api.StatusRequest(actor=owner.actor), self.request)
            lease = next(x for x in status['leases'] if x['id'] == granted['lease_id'])
            self.assertEqual(lease['run_prefix'], f"ssh d6 sudo -n /usr/local/bin/cm-lease run --lease-id {granted['lease_id']} --")
        finally:
            cfg_path.write_text(original)

    async def test_invalid_run_argv_is_rejected_as_configuration_error(self):
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        try:
            for value in ([], ['gcloud', ''], ['gcloud', 1], ['gcloud', 'bad\0arg'], 'gcloud'):
                cfg['boxes'][0]['run_argv'] = value
                cfg_path.write_text(json.dumps(cfg))
                with self.subTest(value=value), self.assertRaises(api.HTTPException) as exc:
                    api.settings()
                self.assertEqual(exc.exception.status_code, 503)
                self.assertIn('run_argv', exc.exception.detail)
        finally:
            cfg_path.write_text(original)

    async def test_reclaim_grace_policy_requires_at_least_ten_minutes(self):
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        try:
            for value in (0, 599, True, 600.0, '600'):
                cfg['policy']['reclaim_notice_grace_seconds'] = value
                cfg_path.write_text(json.dumps(cfg))
                with self.subTest(value=value), self.assertRaises(api.HTTPException) as exc:
                    api.settings()
                self.assertEqual(exc.exception.status_code, 503)
            cfg['policy']['reclaim_notice_grace_seconds'] = 900
            cfg_path.write_text(json.dumps(cfg))
            self.assertEqual(api.settings()[0]['reclaim_notice_grace_seconds'], 900)
        finally:
            cfg_path.write_text(original)

    async def test_request_rejects_resources_larger_than_any_configured_box(self):
        body = self.body('large')
        body.cpus, body.mem_gb = 4, 16  # configured box has only one slot
        with self.assertRaises(api.HTTPException) as exc:
            await api.request_lease(body, self.request)
        self.assertEqual(exc.exception.status_code, 422)
        self.assertIn('cannot fit any configured box', exc.exception.detail)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT count(*) FROM compute_leases'), 0)
        body.cpus, body.mem_gb = 2, 17  # memory also makes this a two-slot request
        with self.assertRaises(api.HTTPException) as exc:
            await api.request_lease(body, self.request)
        self.assertIn('cannot fit any configured box', exc.exception.detail)

    async def test_real_request_requires_separate_justification_above_four_slots(self):
        import httpx
        import uuid
        from fastapi import FastAPI
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'][0]['slots'] = [f'{i},{i+8}' for i in range(8)]
        cfg_path.write_text(json.dumps(cfg))
        try:
            app = FastAPI()
            app.state.pool = self.pool
            app.dependency_overrides[api.verify_token] = lambda: None
            app.include_router(api.router)
            request = {'actor': self.body('large').actor.model_dump(), 'cpus': 10, 'mem_gb': 80,
                       'duration': 300, 'priority': 'normal', 'purpose': 'tests', 'class': 'burst'}
            async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url='http://test') as client:
                denied = await client.post('/compute/request', json=request)
                self.assertEqual(denied.status_code, 422)
                self.assertIn('justification', denied.json()['detail'])
                request['justification'] = 'five slots are needed for this parallel test'
                accepted = await client.post('/compute/request', json=request)
                self.assertEqual(accepted.status_code, 200)
                self.assertEqual(accepted.json()['status'], 'granting')
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval('SELECT justification FROM compute_leases WHERE id=$1',
                    uuid.UUID(accepted.json()['lease_id'])), request['justification'])
        finally:
            cfg_path.write_text(original)

    async def test_unavailable_wide_box_does_not_pin_team_queue(self):
        import uuid
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'] = [dict(name='d6', ssh_target='d6', enabled=True,
                             slots=[f'{i},{i+8}' for i in range(7)]),
                        dict(name='df2', ssh_target='df2', enabled=True,
                             slots=[f'{i},{i+8}' for i in range(8)])]
        # A is already at its soft cap. B's eight-slot waiter fits only the
        # disabled box and therefore must not activate the cap for A.
        cfg['policy']['max_slots_per_team'] = 5
        cfg_path.write_text(json.dumps(cfg))
        try:
            team_a, team_b = uuid.uuid4(), uuid.uuid4()
            async with self.pool.acquire() as conn:
                await conn.execute("INSERT INTO compute_boxes (name,enabled,checked_at,evidence) VALUES ('df2',false,now(),'{}'::jsonb)")
                for uid, team in [('a-big', team_a), ('a-small', team_a), ('b-big', team_b)]:
                    await conn.execute('INSERT INTO members (id,primary_team_id) VALUES ($1,$2)', uid, team)
                    await conn.execute('INSERT INTO incarnations (member_id,pid) VALUES ($1,$2)', uid, f'agent:d:{uid}')
                for i in range(5):
                    await conn.execute("""INSERT INTO compute_leases
                        (id,owner,team,purpose,priority,class,cpus,mem_gb,duration_seconds,status,box,slot_ids,expires_at)
                        VALUES ($1,$2,$3,'running','normal','burst',2,16,300,'active','d6',$4,now()+interval '5 minutes')""",
                        uuid.uuid4(), f'agent:d:running{i}', str(team_a), [cfg['boxes'][0]['slots'][i]])
            # Rows already queued before this admission check still must not
            # pin the team or activate the soft cap after df2 is disabled.
            async with self.pool.acquire() as conn:
                first = {'lease_id': await self._queue_for_team(conn, str(team_a), 8, 1)}
                second = {'lease_id': await self._queue_for_team(conn, str(team_b), 8, 2)}
                await conn.execute("UPDATE compute_leases SET created_at=created_at-interval '1 minute' WHERE id=ANY($1::uuid[])",
                                   [first['lease_id'], second['lease_id']])
            a_big = self.body(f'{team_a}-1')
            small = await api.request_lease(self.body('a-small'), self.request)
            self.assertEqual(small['status'], 'granting')
            status = await api.status(api.StatusRequest(actor=a_big.actor, lease_id=first['lease_id']), self.request)
            self.assertEqual(status['leases'][0]['waiting'], 'no enabled box fits')
            waiting = {entry['lease_id']: entry.get('waiting') for entry in status['queue']}
            self.assertEqual(waiting[first['lease_id']], 'no enabled box fits')
            self.assertEqual(waiting[second['lease_id']], 'no enabled box fits')
            # Configuration-level disablement has the same status semantics.
            cfg['boxes'][1]['enabled'] = False
            cfg_path.write_text(json.dumps(cfg))
            status = await api.status(api.StatusRequest(actor=a_big.actor, lease_id=first['lease_id']), self.request)
            self.assertEqual(status['leases'][0]['waiting'], 'no enabled box fits')
            config_disabled = self.body('config-disabled')
            config_disabled.cpus, config_disabled.mem_gb = 16, 128
            config_disabled.justification = 'requires eight slots'
            with self.assertRaises(api.HTTPException) as exc:
                await api.request_lease(config_disabled, self.request)
            self.assertEqual(exc.exception.status_code, 409)
            cfg['boxes'][1]['enabled'] = True
            cfg_path.write_text(json.dumps(cfg))
            async with self.pool.acquire() as conn:
                await conn.execute("UPDATE compute_boxes SET enabled=true WHERE name='df2'")
            status = await api.status(api.StatusRequest(actor=a_big.actor, lease_id=first['lease_id']), self.request)
            self.assertEqual(status['leases'][0]['waiting'], 'justification required')
            self.assertEqual(status['queue'][0]['waiting'], 'justification required')
        finally:
            cfg_path.write_text(original)

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

    async def test_status_only_answers_an_issued_notice(self):
        owner = self.body('one')
        granted = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(granted['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE compute_leases SET last_activity_at=now()-interval '3 hours' WHERE id=$1", granted['lease_id'])
        await api.status(api.StatusRequest(actor=owner.actor, lease_id=granted['lease_id']), self.request)
        async with self.pool.acquire() as conn, conn.transaction():
            before = await conn.fetchval('SELECT last_activity_at FROM compute_leases WHERE id=$1', granted['lease_id'])
            self.assertTrue(await conn.fetchval("SELECT $1::timestamptz < now()-interval '2 hours'", before))
            row = await conn.fetchrow('SELECT * FROM compute_leases WHERE id=$1', granted['lease_id'])
            await api._notice_action(conn, row, 'idle_notice')
        await api.status(api.StatusRequest(actor=owner.actor, lease_id=granted['lease_id']), self.request)
        async with self.pool.acquire() as conn:
            answered = await conn.fetchval('SELECT answered_at FROM compute_lease_notices WHERE lease_id=$1', granted['lease_id'])
            self.assertIsNotNone(answered)
            self.assertTrue(await conn.fetchval('SELECT last_activity_at > $2 FROM compute_leases WHERE id=$1', granted['lease_id'], before))
            await conn.execute("UPDATE compute_leases SET last_activity_at=now()-interval '3 hours' WHERE id=$1", granted['lease_id'])
        await api.status(api.StatusRequest(actor=owner.actor, lease_id=granted['lease_id']), self.request)
        async with self.pool.acquire() as conn:
            self.assertTrue(await conn.fetchval("SELECT last_activity_at < now()-interval '2 hours' FROM compute_leases WHERE id=$1", granted['lease_id']))

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
        cfg['policy'].update(idle_threshold_seconds=60, idle_answer_seconds=60, reclaim_notice_grace_seconds=900,
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
                await conn.execute('INSERT INTO compute_lease_sampling_epochs (lease_id) VALUES ($1)', granted['lease_id'])
            await api.sample(api.SampleReceipt(lease_id=granted['lease_id'], cpu_usage_usec=6_000_000), self.request)
            await api.executor_poll(self.request)
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', granted['lease_id']), 'active')
                push = await conn.fetchval("SELECT n.push_id FROM compute_lease_notices n JOIN compute_lease_actions a ON a.id=n.action_id WHERE a.kind='reclaim_notice'")
                self.assertIsNotNone(push)
                await conn.execute("UPDATE item_pushes SET delivered_at=now(), dropped_reason='offline' WHERE id=$1", push)
            self.assertIsNone((await api.executor_poll(self.request))['action'])
            # Failed delivery must not create a new notice every sample.
            for _ in range(2):
                await api.sample(api.SampleReceipt(lease_id=granted['lease_id'], cpu_usage_usec=6_000_000), self.request)
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval("SELECT count(*) FROM compute_lease_actions WHERE kind='reclaim_notice'"), 1)
                await conn.execute("UPDATE compute_leases SET last_activity_at=now()-interval '1 hour' WHERE id=$1", granted['lease_id'])
                await conn.execute("UPDATE compute_lease_actions SET created_at=now()-interval '901 seconds' WHERE kind='reclaim_notice'")
            await api.sample(api.SampleReceipt(lease_id=granted['lease_id'], cpu_usage_usec=6_000_000), self.request)
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval("SELECT count(*) FROM compute_lease_actions WHERE kind='reclaim_notice'"), 2)
                push = await conn.fetchval("""SELECT n.push_id FROM compute_lease_notices n JOIN compute_lease_actions a
                    ON a.id=n.action_id WHERE a.kind='reclaim_notice' AND a.status <> 'done'""")
                await conn.execute('UPDATE item_pushes SET delivered_at=now() WHERE id=$1', push)
            self.assertIsNone((await api.executor_poll(self.request))['action'])
            # An observed final notice must wait for the configured 15 minutes.
            await api.sample(api.SampleReceipt(lease_id=granted['lease_id'], cpu_usage_usec=6_000_000), self.request)
            self.assertIsNone((await api.executor_poll(self.request))['action'])
            async with self.pool.acquire() as conn:
                await conn.execute('UPDATE compute_lease_notices SET sent_at=now()-interval \'601 seconds\' WHERE push_id=$1', push)
            await api.sample(api.SampleReceipt(lease_id=granted['lease_id'], cpu_usage_usec=6_000_000), self.request)
            self.assertIsNone((await api.executor_poll(self.request))['action'])
            async with self.pool.acquire() as conn:
                await conn.execute('UPDATE compute_lease_notices SET sent_at=now()-interval \'901 seconds\' WHERE push_id=$1', push)
            await api.sample(api.SampleReceipt(lease_id=granted['lease_id'], cpu_usage_usec=6_000_000), self.request)
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
            self.assertEqual(notice['result'], {'delivered': False})

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

    async def test_resumed_interval_resets_clock_before_any_reclaim(self):
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
        cfg['policy'].update(idle_threshold_seconds=600, idle_answer_seconds=60,
                             sample_max_gap_seconds=20, notice_expiry_guard_seconds=30)
        cfg_path.write_text(json.dumps(cfg))
        try:
            now = datetime.now(timezone.utc)
            async with self.pool.acquire() as conn, conn.transaction():
                await conn.execute('UPDATE compute_leases SET last_activity_at=$2, expires_at=$3 WHERE id=$1',
                    granted['lease_id'], now-timedelta(hours=3), now+timedelta(hours=1))
                row = await conn.fetchrow('SELECT * FROM compute_leases WHERE id=$1', granted['lease_id'])
                for age in (130, 65):
                    await api._notice_action(conn, row, 'idle_notice')
                    notice = await conn.fetchrow('SELECT action_id,push_id FROM compute_lease_notices WHERE sent_at IS NULL ORDER BY push_id DESC LIMIT 1')
                    await conn.execute("UPDATE compute_lease_actions SET status='done' WHERE id=$1", notice['action_id'])
                    await conn.execute('UPDATE compute_lease_notices SET sent_at=$2 WHERE action_id=$1', notice['action_id'], now-timedelta(seconds=age))
                    await conn.execute('UPDATE item_pushes SET delivered_at=$2 WHERE id=$1', notice['push_id'], now-timedelta(seconds=age))
                # A final notice has been observed, but no new CPU interval has been read yet.
                await api._notice_action(conn, row, 'reclaim_notice')
                push = await conn.fetchval('SELECT push_id FROM compute_lease_notices WHERE sent_at IS NULL')
                await conn.execute('UPDATE item_pushes SET delivered_at=now() WHERE id=$1', push)
                for i in range(61):
                    await conn.execute('INSERT INTO compute_lease_samples (lease_id,sampled_at,usage_usec) VALUES ($1,$2,0)',
                        granted['lease_id'], now-timedelta(seconds=610-i*10))
                await conn.execute('INSERT INTO compute_lease_sampling_epochs (lease_id) VALUES ($1)', granted['lease_id'])
            self.assertIsNone((await api.executor_poll(self.request))['action'])
            # One full-speed interval is only ~1.67% of the 10-minute window.
            await api.sample(api.SampleReceipt(lease_id=granted['lease_id'], cpu_usage_usec=20_000_000), self.request)
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', granted['lease_id']), 'active')
                self.assertTrue(await conn.fetchval("SELECT last_activity_at > now()-interval '10 seconds' FROM compute_leases WHERE id=$1", granted['lease_id']))
                self.assertEqual(await conn.fetchval('SELECT count(*) FROM compute_lease_notices WHERE lease_id=$1 AND invalidated_at IS NULL', granted['lease_id']), 0)
        finally:
            cfg_path.write_text(original)

    async def test_api_restart_requires_fresh_idle_window(self):
        from datetime import datetime, timedelta, timezone
        from fastapi import FastAPI
        from api import main

        owner = self.body('one')
        granted = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(action['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        now = datetime.now(timezone.utc)
        async with self.pool.acquire() as conn, conn.transaction():
            await conn.execute("UPDATE compute_leases SET last_activity_at=$2 WHERE id=$1",
                               granted['lease_id'], now-timedelta(hours=3))
            row = await conn.fetchrow('SELECT * FROM compute_leases WHERE id=$1', granted['lease_id'])
            await api._notice_action(conn, row, 'idle_notice')
            await conn.execute('INSERT INTO compute_lease_samples (lease_id,sampled_at,usage_usec) VALUES ($1,$2,0)',
                               granted['lease_id'], now-timedelta(seconds=10))
            await conn.execute('INSERT INTO compute_lease_sampling_epochs (lease_id) VALUES ($1)', granted['lease_id'])

        async def wait_forever(*args):
            await asyncio.Event().wait()

        broker = MagicMock()
        broker.stop = AsyncMock()
        # Keep the real test pool open for tearDown while exercising startup.
        pool_proxy = SimpleNamespace(acquire=self.pool.acquire, close=AsyncMock())
        with patch.object(main.db, 'get_pool', AsyncMock(return_value=pool_proxy)), \
             patch.object(main.db, 'init_db', AsyncMock()), \
             patch.object(main, 'dispatch_loop', wait_forever), \
             patch.object(main, 'warm_pool_loop', wait_forever), \
             patch.object(main, 'change_log_maintenance_loop', wait_forever), \
             patch.object(main, 'board_loop', wait_forever), \
             patch.object(main, 'ChangeBroker', return_value=broker):
            async with main.lifespan(FastAPI()):
                async with self.pool.acquire() as conn:
                    self.assertEqual(await conn.fetchval('SELECT count(*) FROM compute_lease_sampling_epochs'), 0)
                await api.sample(api.SampleReceipt(lease_id=granted['lease_id'], cpu_usage_usec=0), self.request)
                async with self.pool.acquire() as conn:
                    self.assertTrue(await conn.fetchval("SELECT last_activity_at > now()-interval '10 seconds' FROM compute_leases WHERE id=$1", granted['lease_id']))
                    self.assertEqual(await conn.fetchval('SELECT count(*) FROM compute_lease_notices WHERE invalidated_at IS NULL'), 0)

    async def test_status_answer_precedes_delayed_delivery_ack(self):
        owner = self.body('one')
        granted = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(action['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        async with self.pool.acquire() as conn, conn.transaction():
            row = await conn.fetchrow('SELECT * FROM compute_leases WHERE id=$1', granted['lease_id'])
            await api._notice_action(conn, row, 'idle_notice')
            push = await conn.fetchval('SELECT push_id FROM compute_lease_notices WHERE lease_id=$1', granted['lease_id'])
        await api.status(api.StatusRequest(actor=owner.actor, lease_id=granted['lease_id']), self.request)
        async with self.pool.acquire() as conn:
            await conn.execute("UPDATE item_pushes SET delivered_at=now()+interval '1 second' WHERE id=$1", push)
        await api.executor_poll(self.request)
        async with self.pool.acquire() as conn:
            notice = await conn.fetchrow('SELECT n.answered_at,n.sent_at,l.last_activity_at FROM compute_lease_notices n JOIN compute_leases l ON l.id=n.lease_id WHERE n.push_id=$1', push)
            self.assertIsNotNone(notice['answered_at'])
            self.assertGreater(notice['sent_at'], notice['last_activity_at'])
            self.assertEqual(await conn.fetchval("""SELECT count(*) FROM compute_lease_notices n
                JOIN compute_lease_actions a ON a.id=n.action_id WHERE n.lease_id=$1
                AND a.kind='idle_notice' AND n.answered_at IS NULL""", granted['lease_id']), 0)

    async def test_confirmed_missing_extension_retains_slot_until_release_ack(self):
        owner = self.body('one')
        granted = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id': str(action['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
        await api.extend(api.LeaseChange(actor=owner.actor, lease_id=granted['lease_id'], duration=60), self.request)
        extend_action = (await api.executor_poll(self.request))['action']
        self.assertEqual(extend_action['kind'], 'extend')
        await api.executor_complete(api.ActionReceipt(action_id=extend_action['action_id'], result={
            'lease_id': str(granted['lease_id']), 'absent': True, 'unit_inactive': True}), self.request)
        waiting = await api.request_lease(self.body('two'), self.request)
        self.assertEqual(waiting['status'], 'queued')
        release = (await api.executor_poll(self.request))['action']
        self.assertEqual(release['kind'], 'release')
        await api.executor_complete(api.ActionReceipt(action_id=release['action_id'], result={
            'lease_id': str(granted['lease_id']), 'released': True, 'already_absent': True}), self.request)
        async with self.pool.acquire() as conn:
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', waiting['lease_id']), 'granting')
            self.assertIn('node lease absent', await conn.fetchval('SELECT error FROM compute_leases WHERE id=$1', granted['lease_id']))
            result = await conn.fetchval('SELECT result FROM compute_lease_actions WHERE id=$1', extend_action['action_id'])
            self.assertIs(result['ok'], False)
        status = await api.status(api.StatusRequest(actor=owner.actor, lease_id=granted['lease_id']), self.request)
        terminal = next(lease for lease in status['leases'] if lease['id'] == granted['lease_id'])
        self.assertEqual(terminal['status'], 'released')
        self.assertIn('extension failed', terminal['error'])

    async def test_soft_cap_ineligible_head_yields_to_other_team(self):
        import uuid
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'][0]['slots'] = ['0,8', '1,9']
        cfg['policy']['max_slots_per_team'] = 1
        cfg_path.write_text(json.dumps(cfg))
        try:
            teams = {'a': uuid.uuid4(), 'b': uuid.uuid4(), 'c': uuid.uuid4()}
            async with self.pool.acquire() as conn:
                for uid, team in [('a1', 'a'), ('a2', 'a'), ('b', 'b'), ('c', 'c')]:
                    await conn.execute('INSERT INTO members (id,primary_team_id) VALUES ($1,$2)', uid, teams[team])
                    await conn.execute('INSERT INTO incarnations (member_id,pid) VALUES ($1,$2)', uid, f'agent:d:{uid}')
            async def activate(uid):
                body = self.body(uid)
                lease = await api.request_lease(body, self.request)
                action = (await api.executor_poll(self.request))['action']
                await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                    'lease_id': str(action['lease_id']), 'configured': True, 'workdir': '/work/id'}), self.request)
                return body, lease
            _, first = await activate('a1')
            c_body, c = await activate('c')
            a_wait = await api.request_lease(self.body('a2'), self.request)
            b_wait = await api.request_lease(self.body('b'), self.request)
            self.assertEqual((a_wait['status'], b_wait['status']), ('queued', 'queued'))
            await api.release(api.LeaseChange(actor=c_body.actor, lease_id=c['lease_id']), self.request)
            action = (await api.executor_poll(self.request))['action']
            self.assertEqual(action['kind'], 'release')
            await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id': str(c['lease_id']), 'released': True}), self.request)
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', a_wait['lease_id']), 'queued')
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', b_wait['lease_id']), 'granting')
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', first['lease_id']), 'active')
        finally:
            cfg_path.write_text(original)

    async def _setup_two_busy_teams(self):
        import uuid
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'] = [dict(name=f'box{i}', ssh_target=f'box{i}', enabled=True,
                             slots=[f'{j},{j+8}' for j in range(8)]) for i in range(4)]
        cfg['policy']['max_slots_per_team'] = 12
        cfg_path.write_text(json.dumps(cfg))
        async with self.pool.acquire() as conn:
            for box in cfg['boxes']:
                await conn.execute("INSERT INTO compute_boxes (name,enabled,checked_at,evidence) VALUES ($1,true,now(),'{}'::jsonb)", box['name'])
            for i in range(22):
                box, slot = cfg['boxes'][i // 8], i % 8
                await conn.execute("""INSERT INTO compute_leases
                    (id,owner,team,purpose,priority,class,cpus,mem_gb,duration_seconds,status,box,slot_ids,expires_at)
                    VALUES ($1,$2,$3,'running','normal','burst',2,16,300,'active',$4,$5,now()+interval '5 minutes')""",
                    uuid.uuid4(), f'agent:d:running{i}', 'a' if i % 2 == 0 else 'b', box['name'], [box['slots'][slot]])
        return cfg_path, original

    async def _queue_for_team(self, conn, team, slots, order, priority='normal'):
        import uuid
        ident = uuid.uuid4()
        await conn.execute("""INSERT INTO compute_leases
            (id,owner,team,purpose,priority,class,cpus,mem_gb,duration_seconds,status,created_at)
            VALUES ($1,$2,$3,'queued test',$4,'burst',$5,$6,300,'queued',now()+make_interval(secs => $7))""",
            ident, f'agent:d:{team}-{order}', team, priority, slots * 2, slots * 16, order)
        return ident

    async def test_soft_deferred_head_blocks_later_requests_from_each_team(self):
        cfg_path, original = await self._setup_two_busy_teams()
        try:
            p, boxes = api.settings()
            async with self.pool.acquire() as conn, conn.transaction():
                a_big = await self._queue_for_team(conn, 'a', 4, 1)
                b_big = await self._queue_for_team(conn, 'b', 4, 2)
                a_small = await self._queue_for_team(conn, 'a', 1, 3)
                b_small = await self._queue_for_team(conn, 'b', 1, 4, 'low')
                c_small = await self._queue_for_team(conn, 'c', 1, 5, 'low')
                await api._schedule(conn, p, boxes)
                await api._schedule(conn, p, boxes)  # another poll must not rotate small leases in
                for ident in (a_big, b_big, a_small, b_small):
                    self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', ident), 'queued')
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', c_small), 'granting')
                # Once A's head goes away, its later request is eligible again.
                await conn.execute("UPDATE compute_leases SET status='failed' WHERE id=$1", a_big)
                await api._schedule(conn, p, boxes)
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', a_small), 'granting')
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', b_small), 'queued')
        finally:
            cfg_path.write_text(original)

    async def test_gate_above_team_soft_cap_is_scheduled(self):
        cfg_path, original = await self._setup_two_busy_teams()
        try:
            p, boxes = api.settings()
            async with self.pool.acquire() as conn, conn.transaction():
                gate = await self._queue_for_team(conn, 'a', 4, 1, 'v221-gate')
                other = await self._queue_for_team(conn, 'b', 4, 2)
                small = await self._queue_for_team(conn, 'a', 1, 3)
                await api._schedule(conn, p, boxes)
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', gate), 'granting')
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', other), 'queued')
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', small), 'queued')
        finally:
            cfg_path.write_text(original)

    async def _capacity_order_scenario(self, first_priority, later_priority, other_box=False):
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'][0]['slots'] = ['0,8', '1,9', '2,10', '3,11']
        if other_box:
            cfg['boxes'].append({'name': 'small', 'ssh_target': 'small', 'enabled': True,
                                 'slots': ['0,8', '1,9']})
        cfg_path.write_text(json.dumps(cfg))
        try:
            if other_box:
                async with self.pool.acquire() as conn:
                    await conn.execute("INSERT INTO compute_boxes (name,enabled,checked_at,evidence) VALUES ('small',true,now(),'{}'::jsonb)")
            busy_body = self.body('busy')
            busy_body.cpus, busy_body.mem_gb = 4, 32
            busy = await api.request_lease(busy_body, self.request)
            action = (await api.executor_poll(self.request))['action']
            await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id': str(busy['lease_id']), 'configured': True, 'workdir': '/work/busy'}), self.request)
            first = self.body('first')
            first.cpus, first.mem_gb, first.priority = 8, 64, first_priority
            earlier = await api.request_lease(first, self.request)
            self.assertEqual(earlier['status'], 'queued')
            later = self.body('later')
            later.cpus, later.mem_gb, later.priority = 4, 32, later_priority
            second = await api.request_lease(later, self.request)
            self.assertEqual(second['status'], 'granting' if other_box else 'queued')
            if other_box:
                async with self.pool.acquire() as conn:
                    self.assertEqual(await conn.fetchval('SELECT box FROM compute_leases WHERE id=$1', second['lease_id']), 'small')
                return
            await api.release(api.LeaseChange(actor=busy_body.actor, lease_id=busy['lease_id']), self.request)
            action = (await api.executor_poll(self.request))['action']
            self.assertEqual(action['kind'], 'release')
            await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id': str(busy['lease_id']), 'released': True}), self.request)
            async with self.pool.acquire() as conn:
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', earlier['lease_id']), 'granting')
                self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', second['lease_id']), 'queued')
        finally:
            cfg_path.write_text(original)

    async def test_capacity_blocked_gate_holds_box_from_low_priority(self):
        await self._capacity_order_scenario('v221-gate', 'low')

    async def test_capacity_blocked_request_holds_box_within_priority_class(self):
        await self._capacity_order_scenario('normal', 'normal')

    async def test_capacity_blocked_request_allows_use_of_too_small_box(self):
        await self._capacity_order_scenario('v221-gate', 'low', other_box=True)

    async def test_extension_ignores_queue_that_cannot_fit_this_box(self):
        cfg_path = self.root / 'pool.json'
        original = cfg_path.read_text()
        cfg = json.loads(original)
        cfg['boxes'].append({'name':'wide', 'ssh_target':'wide', 'enabled':True,
                             'slots':['0,8','1,9']})
        cfg_path.write_text(json.dumps(cfg))
        try:
            async with self.pool.acquire() as conn:
                await conn.execute("INSERT INTO compute_boxes (name, enabled, checked_at, evidence) VALUES ('wide',true,now(),'{}'::jsonb)")
            owner = self.body('one')
            small = await api.request_lease(owner, self.request)
            action = (await api.executor_poll(self.request))['action']
            await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id':str(action['lease_id']), 'configured':True, 'workdir':'/work/small'}), self.request)
            wide_owner = self.body('wide-owner')
            wide_owner.cpus, wide_owner.mem_gb = 4, 32
            wide = await api.request_lease(wide_owner, self.request)
            action = (await api.executor_poll(self.request))['action']
            await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id':str(action['lease_id']), 'configured':True, 'workdir':'/work/wide'}), self.request)
            waiter = self.body('wide-waiter')
            waiter.cpus, waiter.mem_gb = 4, 32
            self.assertEqual((await api.request_lease(waiter, self.request))['status'], 'queued')
            extension = await api.extend(api.LeaseChange(actor=owner.actor, lease_id=small['lease_id'], duration=60), self.request)
            self.assertEqual(extension['status'], 'extending')
            action = (await api.executor_poll(self.request))['action']
            self.assertEqual(action['kind'], 'extend')
            await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
                'lease_id':str(small['lease_id']), 'expires':action['payload']['expires_epoch']}), self.request)
            await api.request_lease(self.body('small-waiter'), self.request)
            with self.assertRaises(api.HTTPException) as exc:
                await api.extend(api.LeaseChange(actor=owner.actor, lease_id=small['lease_id'], duration=60), self.request)
            self.assertEqual(exc.exception.status_code, 409)
        finally:
            cfg_path.write_text(original)

    async def test_upgrade_does_not_reclaim_using_legacy_notice_or_samples(self):
        from datetime import datetime, timedelta, timezone
        owner = self.body('one')
        granted = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id':str(action['lease_id']), 'configured':True, 'workdir':'/work/id'}), self.request)
        await api.request_lease(self.body('two'), self.request)
        now = datetime.now(timezone.utc)
        async with self.pool.acquire() as conn, conn.transaction():
            await conn.execute('UPDATE compute_leases SET last_activity_at=$2, expires_at=$3 WHERE id=$1',
                granted['lease_id'], now-timedelta(hours=3), now+timedelta(hours=1))
            row = await conn.fetchrow('SELECT * FROM compute_leases WHERE id=$1', granted['lease_id'])
            await api._notice_action(conn, row, 'idle_notice')
            await conn.execute('UPDATE compute_lease_notices SET answer_tracking=false WHERE lease_id=$1', granted['lease_id'])
            await conn.execute('INSERT INTO compute_lease_samples (lease_id,sampled_at,usage_usec) VALUES ($1,$2,0)',
                granted['lease_id'], now-timedelta(seconds=10))
        await api.sample(api.SampleReceipt(lease_id=granted['lease_id'], cpu_usage_usec=0), self.request)
        async with self.pool.acquire() as conn:
            self.assertTrue(await conn.fetchval("SELECT last_activity_at > now()-interval '10 seconds' FROM compute_leases WHERE id=$1", granted['lease_id']))
            self.assertTrue(await conn.fetchval('SELECT invalidated_at IS NOT NULL FROM compute_lease_notices WHERE lease_id=$1', granted['lease_id']))
            self.assertEqual(await conn.fetchval('SELECT status FROM compute_leases WHERE id=$1', granted['lease_id']), 'active')

    async def test_legacy_notices_cannot_authorize_reclaim_after_upgrade(self):
        from datetime import datetime, timedelta, timezone
        owner = self.body('one')
        granted = await api.request_lease(owner, self.request)
        action = (await api.executor_poll(self.request))['action']
        await api.executor_complete(api.ActionReceipt(action_id=action['action_id'], result={
            'lease_id':str(action['lease_id']), 'configured':True, 'workdir':'/work/id'}), self.request)
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
                await conn.execute('UPDATE compute_leases SET last_activity_at=$2, expires_at=$3 WHERE id=$1',
                    granted['lease_id'], now-timedelta(hours=3), now+timedelta(hours=1))
                row = await conn.fetchrow('SELECT * FROM compute_leases WHERE id=$1', granted['lease_id'])
                for age in (130,65):
                    await api._notice_action(conn, row, 'idle_notice')
                    await conn.execute('''UPDATE compute_lease_notices SET answer_tracking=false,sent_at=$2
                        WHERE action_id=(SELECT action_id FROM compute_lease_notices
                        WHERE lease_id=$1 AND sent_at IS NULL ORDER BY push_id DESC LIMIT 1)''',
                        granted['lease_id'], now-timedelta(seconds=age))
                for i in range(7):
                    await conn.execute('INSERT INTO compute_lease_samples (lease_id,sampled_at,usage_usec) VALUES ($1,$2,0)',
                        granted['lease_id'], now-timedelta(seconds=60-i*10))
                p, boxes = api.settings()
                decision = await api._idle_decision(conn, dict(row), p, boxes, now)
                self.assertNotEqual(decision, 'reclaim')
        finally:
            cfg_path.write_text(original)
