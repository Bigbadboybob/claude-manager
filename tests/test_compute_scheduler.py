from datetime import datetime, timedelta, timezone

import pytest

from dispatch.compute_scheduler import Box, Policy, limit_reason, queue_eta, queue_order, select_box, slots_needed

NOW = datetime(2026, 11, 25, tzinfo=timezone.utc)
POLICY = Policy(cpus_per_slot=2, mem_gb_per_slot=16, max_slots_per_session=8,
                max_slots_per_lease=4, max_slots_per_team=12)
BOX = Box("lane", "lane-alias", ("0,8", "1,9", "2,10", "3,11"))


def lease(ident, **overrides):
    row = dict(id=ident, owner=ident, team=ident, cpus=2, mem_gb=16,
               priority="normal", status="queued", created_at=NOW)
    row.update(overrides)
    return row


def test_fifo_with_priority_classes():
    rows = [lease("n1"), lease("low", priority="low", created_at=NOW-timedelta(hours=1)),
            lease("n2", created_at=NOW+timedelta(seconds=1)),
            lease("gate", priority="v221-gate", created_at=NOW+timedelta(hours=1))]
    assert [r["id"] for r in queue_order(rows)] == ["gate", "n1", "n2", "low"]


def test_reservations_survive_inflight_grant_and_release():
    existing = [lease("a", status="granting", box="lane", slot_ids=["0,8"]),
                lease("b", status="releasing", box="lane", slot_ids=["1,9"])]
    request = lease("c", cpus=4, mem_gb=32)
    assert select_box(request, existing, [BOX], POLICY) == (BOX, ("2,10", "3,11"))
    assert select_box(lease("d", cpus=6), existing, [BOX], POLICY) is None
    existing[0]["slot_ids"] = None
    with pytest.raises(ValueError, match="missing slot reservation"):
        select_box(request, existing, [BOX], POLICY)


def test_memory_also_consumes_slots_and_session_limit_is_enforced():
    assert slots_needed(lease("a", cpus=1, mem_gb=17), POLICY) == 2
    existing = [lease("a", owner="same", status="active", box="lane", slot_ids=list(BOX.slots),
                      cpus=8, mem_gb=64)]
    limit = Policy(2, 16, 4, 4, 12)
    assert select_box(lease("b", owner="same"), existing, [BOX, Box("other", "x", BOX.slots)], limit) is None


def test_session_limit_takes_precedence_over_soft_team_deferral():
    policy = Policy(2, 16, 1, 4, 1)
    active = lease('active', owner='same', team='a', status='active', box='lane', slot_ids=['0,8'])
    request = lease('request', owner='same', team='a')
    other = lease('other', team='b')
    assert limit_reason(request, [active, request, other], policy, [BOX]) == 'hard'
    assert limit_reason(lease('another', team='a'), [active, request, other], policy, [BOX]) == 'soft_team'


def test_gate_ignores_team_soft_cap_but_obeys_session_cap():
    policy = Policy(2, 16, 8, 4, 1)
    active = lease('active', owner='same', team='a', status='active', box='lane', slot_ids=['0,8'])
    other = lease('other', team='b')
    gate = lease('gate', team='a', priority='v221-gate', cpus=8, mem_gb=64)
    assert limit_reason(gate, [active, other, gate], policy, [BOX]) is None
    active.update(cpus=10, mem_gb=80, slot_ids=[f'{i},{i+8}' for i in range(5)])
    gate['owner'] = 'same'
    assert limit_reason(gate, [active, other, gate], policy, [BOX]) == 'hard'


def test_soft_cap_counts_only_waiters_that_fit_an_enabled_box():
    policy = Policy(2, 16, 8, 4, 1)
    small = Box('small', 'small', ('0,8', '1,9'))
    disabled = Box('disabled', 'disabled', BOX.slots, configured_enabled=False)
    active = lease('active', team='a', status='active', box='small', slot_ids=['0,8'])
    request = lease('request', team='a')
    other = lease('other', team='b', cpus=8, mem_gb=64)
    rows = [active, request, other]
    assert limit_reason(request, rows, policy, [small, disabled]) is None
    assert select_box(request, rows, [small], policy, enabled_boxes=[small, disabled]) == (small, ('1,9',))
    # A fitting waiter still counts when its enabled box is reserved for an
    # earlier request and is absent from this placement's candidate list.
    assert limit_reason(request, rows, policy, [small, BOX]) == 'soft_team'
    assert select_box(request, rows, [small], policy, enabled_boxes=[small, BOX]) is None


def test_eta_is_earliest_possible_release_not_claimed_start():
    rows = [lease("a", status="active", expires_at=NOW+timedelta(minutes=20)),
            lease("b", status="active", expires_at=NOW+timedelta(minutes=10))]
    assert queue_eta(rows, NOW) == NOW+timedelta(minutes=10)
    assert queue_eta([], NOW) is None


def test_above_normal_lease_size_requires_a_visible_purpose():
    box = Box('big', 'big', tuple(f'{i},{i+8}' for i in range(8)))
    big = lease('big', cpus=10, mem_gb=80)
    assert select_box(big, [], [box], POLICY) is None
    big['purpose'] = 'explain why five slots are needed'
    assert select_box(big, [], [box], POLICY) == (box, box.slots[:5])
