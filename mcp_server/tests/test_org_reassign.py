"""Reassignment boundaries and durable API attribution, on a scratch database."""
import unittest

from mcp_server.tests.test_org_promote import PromoteDb
from mcp_server.tests.test_org_api import DSN, OWNER, pid, uid_at


@unittest.skipUnless(DSN, "set CM_ORG_TEST_DSN to a scratch Postgres database")
class ReassignDb(unittest.IsolatedAsyncioTestCase):
    asyncSetUp = PromoteDb.asyncSetUp
    asyncTearDown = PromoteDb.asyncTearDown
    task = PromoteDb.task
    initiative = PromoteDb.initiative
    board_with_holder = PromoteDb.board_with_holder
    roster = PromoteDb.roster
    apply = PromoteDb.apply
    snapshot = PromoteDb.snapshot
    swarm = PromoteDb.swarm
    beat = PromoteDb.beat
    members = PromoteDb.members
    report = PromoteDb.report
    promote = PromoteDb.promote

    async def reassign(self, actor, target, member, status=200, **extra):
        r = await self.http.post('/org/assistants/reassign', json={
            'actor': {'pid': actor}, 'pid': target, 'member': member,
            'reason': 'new team', **extra})
        self.assertEqual(r.status_code, status, r.text)
        return r.json()

    async def test_subtree_boundaries_replay_and_stale_heartbeat(self):
        await self.members()
        child, grandchild = uid_at(.1, 20), uid_at(.1, 21)
        rows = [
            {'uid': child, 'managed_by': self.helper, 'member_id': self.ids['org'], 'acting_as': 'assistant'},
            {'uid': grandchild, 'managed_by': child, 'member_id': self.ids['org'], 'acting_as': 'assistant'},
        ]
        await self.report(rows)
        # A nested member is a boundary, even when spawned by the root.
        nested = await self.promote(OWNER['pid'], pid(child), handle='nested', with_subtree=False)
        new = await self.reassign(OWNER['pid'], pid(self.helper), 'swarm-coord', request_id='r1')
        self.assertEqual([x['session_uid'] for x in new['sessions']], [self.helper])
        again = await self.reassign(OWNER['pid'], pid(self.helper), 'swarm-coord', request_id='r1')
        self.assertEqual(new['sessions'], again['sessions'])
        await self.reassign(OWNER['pid'], pid(self.helper), 'org', request_id='r2')
        replay = await self.reassign(OWNER['pid'], pid(self.helper), 'swarm-coord', request_id='r1')
        self.assertEqual(replay['current_assignments'][self.helper], self.ids['org'])
        await self.reassign(OWNER['pid'], pid(self.helper), 'swarm-coord', request_id='r3')
        reply = await self.report(rows)  # reports the old stamps after a lost local update
        self.assertEqual(reply['org']['assignments'][self.helper], self.ids['swarm-coord'])
        async with self.pool.acquire() as c:
            facts = await c.fetch('SELECT session_uid, member_id FROM org_session_facts WHERE session_uid = ANY($1::text[])',
                                  [self.helper, self.rev, grandchild])
            mapping = {r['session_uid']: r['member_id'] for r in facts}
            count = await c.fetchval("SELECT count(*) FROM org_events WHERE type='assistant_reassigned'")
        self.assertEqual(mapping[self.helper], self.ids['swarm-coord'])
        self.assertEqual(mapping[self.rev], self.ids['org'])  # sibling from the old main
        self.assertEqual(mapping[grandchild], self.ids['org'])  # below nested main
        self.assertEqual(count, 3)
        self.assertEqual(nested['member']['main']['pid'], pid(child))

    async def test_reassign_moves_root_and_all_descendants(self):
        await self.members()
        child, grandchild = uid_at(.1, 50), uid_at(.1, 51)
        await self.report([
            {'uid': child, 'managed_by': self.helper, 'member_id': self.ids['org'], 'acting_as': 'assistant'},
            {'uid': grandchild, 'managed_by': child, 'member_id': self.ids['org'], 'acting_as': 'assistant'},
        ])
        before = (await self.snapshot())['version']
        out = await self.reassign(OWNER['pid'], pid(self.helper), 'swarm-coord',
                                  expected_sessions=[self.helper, child, grandchild])
        self.assertGreater(out['version'], before)
        self.assertNotIn('unchanged', await self.snapshot(before))
        self.assertEqual([row['session_uid'] for row in out['sessions']], [self.helper, child, grandchild])
        self.assertEqual(set(out['current_assignments']), {self.helper, child, grandchild})

    async def test_promote_default_moves_descendants_and_opt_out(self):
        await self.members()
        child, sibling, grandchild = uid_at(.1, 30), uid_at(.1, 31), uid_at(.1, 32)
        rows = [
            {'uid': child, 'managed_by': self.helper, 'member_id': self.ids['org'], 'acting_as': 'assistant'},
            {'uid': sibling, 'managed_by': self.rev, 'member_id': self.ids['org'], 'acting_as': 'assistant'},
            {'uid': grandchild, 'managed_by': child, 'member_id': self.ids['org'], 'acting_as': 'assistant'},
        ]
        await self.report(rows)
        promoted = await self.promote(pid(self.lane), pid(self.helper), handle='new-lead')
        self.assertEqual([r['session_uid'] for r in promoted['sessions']], [child, grandchild])
        await self.report(rows)
        async with self.pool.acquire() as c:
            self.assertEqual(await c.fetchval('SELECT member_id FROM org_session_facts WHERE pid=$1', pid(child)), promoted['member']['id'])
        optout = await self.promote(pid(self.lane), pid(self.rev), handle='other', with_subtree=False)
        self.assertEqual(optout['sessions'], [])

    async def test_stale_daemon_snapshot_refuses_without_changes(self):
        await self.members()
        before = await self.snapshot()
        body = await self.reassign(OWNER['pid'], pid(self.helper), 'swarm-coord', status=409,
                                   expected_sessions=[self.helper, 'unreported-child'])
        self.assertEqual(body['detail']['code'], 'stale_session_facts')
        self.assertEqual((await self.snapshot())['version'], before['version'])
        body = await self.promote(OWNER['pid'], pid(self.helper), status=409,
                                  expected_sessions=['unreported-child'])
        self.assertEqual(body['detail']['code'], 'stale_session_facts')
        self.assertEqual((await self.snapshot())['version'], before['version'])

    async def test_authority_and_refused_targets(self):
        await self.members()
        # The old member cannot place an assistant under its own manager.
        body = await self.reassign(pid(self.lane), pid(self.helper), 'swarm-coord', status=403)
        self.assertIn(body['detail']['code'], ('not_manager', 'not_member'))
        # The common manager may act on both sides.
        await self.reassign(pid(self.coord), pid(self.helper), 'swarm-coord')
        await self.reassign(OWNER['pid'], pid(self.lane), 'swarm-coord', status=409)
        await self.reassign(OWNER['pid'], pid(self.orch), 'swarm-coord', status=409)
        shell, worker = uid_at(.1, 40), uid_at(.1, 41)
        await self.report([
            {'uid': shell, 'engine': 'bash', 'member_id': self.ids['org'], 'acting_as': 'assistant'},
            {'uid': worker, 'managed_by': self.orch, 'member_id': self.ids['bug-triage'], 'acting_as': 'worker'},
        ])
        await self.reassign(OWNER['pid'], pid(shell), 'swarm-coord', status=409)
        await self.reassign(OWNER['pid'], pid(worker), 'swarm-coord', status=409)
        async with self.pool.acquire() as c:
            await c.execute("UPDATE session_states SET state='exited' WHERE pid=$1", pid(self.rev))
        await self.reassign(OWNER['pid'], pid(self.rev), 'swarm-coord', status=404)
        await self.reassign(OWNER['pid'], pid('missing'), 'swarm-coord', status=404)
