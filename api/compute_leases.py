"""Compute lease API. Node operations are performed by the external executor.

All reservation transitions run under one PostgreSQL transaction advisory
lock. An uncertain remote operation holds capacity until reconciled.
"""
import json
import logging
import os
import re
import shlex
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

from fastapi import APIRouter, Depends, HTTPException, Request
from pydantic import BaseModel, ConfigDict, Field

from api.auth import verify_token
from dispatch.compute_scheduler import Box, Policy, justification_missing, limit_reason, queue_eta, queue_order, reservation_state, select_box, session_cap_blocked, slots_needed
from dispatch.compute_idle import Sample, idle_average, next_idle_action

router = APIRouter(prefix='/compute', dependencies=[Depends(verify_token)])
logger = logging.getLogger('cm.api.compute')
LOCK_KEY = 746392015
RECENT_LEASE_LIMIT = 50


class Actor(BaseModel):
    pid: str
    session_uid: str
    daemon_id: str
    task_id: str | None = None

    def checked(self):
        if not self.session_uid or not self.daemon_id or self.pid != f'agent:{self.daemon_id}:{self.session_uid}':
            raise HTTPException(422, 'invalid caller identity')
        return self.pid


class LeaseRequest(BaseModel):
    model_config = ConfigDict(populate_by_name=True)
    actor: Actor
    cpus: int = Field(gt=0, strict=True)
    mem_gb: int = Field(gt=0, strict=True)
    duration: int = Field(gt=0, strict=True)
    priority: str
    purpose: str = Field(min_length=1, max_length=2000)
    lease_class: str = Field(alias='class')
    pinned: bool = False
    justification: str = Field(default='', max_length=2000)


class LeaseChange(BaseModel):
    actor: Actor
    lease_id: uuid.UUID
    duration: int | None = Field(default=None, gt=0, strict=True)


class StatusRequest(BaseModel):
    actor: Actor
    lease_id: uuid.UUID | None = None


def settings():
    path = os.environ.get('CM_COMPUTE_POOL_CONFIG')
    if not path:
        raise HTTPException(503, 'compute pool is not configured')
    try:
        data = json.loads(Path(path).read_text())
        p = data['policy']
        for key in ('cpus_per_slot', 'mem_gb_per_slot', 'max_slots_per_session',
                    'max_slots_per_lease', 'max_slots_per_team', 'burst_max_seconds',
                    'long_max_seconds', 'absolute_max_seconds', 'idle_threshold_seconds',
                    'idle_answer_seconds', 'sample_max_gap_seconds', 'notice_limit',
                    'notice_expiry_guard_seconds', 'expiry_warning_seconds',
                    'reclaim_notice_grace_seconds'):
            if not isinstance(p[key], int) or isinstance(p[key], bool) or p[key] <= 0:
                raise ValueError(f'{key} must be a positive integer')
        operator = p.get('quarantine_operator_pid')
        if operator is not None:
            if not isinstance(operator, str) or not re.fullmatch(r'agent:[^:]+:[^:]+', operator):
                raise ValueError('quarantine_operator_pid must be an agent:daemon_id:session_uid or null')
        if p['reclaim_notice_grace_seconds'] < 600:
            raise ValueError('reclaim_notice_grace_seconds must be at least 600')
        if not isinstance(p['idle_percent'], (int, float)) or isinstance(p['idle_percent'], bool) or not 0 < p['idle_percent'] <= 100:
            raise ValueError('idle_percent must be in (0, 100]')
        policy = Policy(**{k: p[k] for k in Policy.__dataclass_fields__})
        boxes = []
        names = set()
        for item in data['boxes']:
            if (item['name'] in names or
                    not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.@-]{0,127}', item['ssh_target'])):
                raise ValueError('duplicate name or missing ssh target')
            names.add(item['name'])
            slots = item['slots']
            cpus = [int(cpu) for slot in slots for cpu in slot.split(',')]
            if len(cpus) != len(set(cpus)) or any(len(s.split(',')) != policy.cpus_per_slot for s in slots):
                raise ValueError('slot CPU lists overlap or have the wrong size')
            run_argv = item.get('run_argv')
            if run_argv is not None and (not isinstance(run_argv, list) or not run_argv or
                    any(not isinstance(arg, str) or not arg or '\0' in arg for arg in run_argv)):
                raise ValueError('run_argv must be a non-empty list of non-empty command arguments')
            boxes.append(Box(item['name'], item['ssh_target'], tuple(slots), item.get('enabled') is True,
                             tuple(run_argv) if run_argv is not None else None))
        return p, boxes
    except (OSError, KeyError, ValueError, TypeError) as exc:
        raise HTTPException(503, f'invalid compute pool configuration: {exc}') from exc


def _pool(request):
    return request.app.state.pool


async def _lock(conn):
    await conn.execute('SELECT pg_advisory_xact_lock($1)', LOCK_KEY)


async def _rows(conn):
    return [dict(row) for row in await conn.fetch(
        "SELECT * FROM compute_leases WHERE status IN ('queued','granting','active','releasing')")]


def _fits_any_box(row, policy, boxes):
    return any(len(box.slots) >= slots_needed(row, policy) for box in boxes)


def _json_object(value):
    # Some older writes stored a JSON string inside JSONB.
    for _ in range(2):
        if not isinstance(value, str):
            break
        try:
            value = json.loads(value)
        except ValueError:
            return None
    return value if isinstance(value, dict) else None


async def _record_activity(conn, lease_id, owner=None, *, answer=True, only_issued_notice=False):
    """Reset idleness and close out every notice already issued for this lease.

    Delivery acknowledgements may arrive after this transaction, so comparing
    their timestamps alone can incorrectly turn an answer into silence. CPU
    activity invalidates notices without mislabeling it as an owner answer.
    """
    if only_issued_notice:
        issued = await conn.fetchval("""SELECT EXISTS(SELECT 1 FROM compute_lease_notices n
            JOIN compute_lease_actions a ON a.id=n.action_id WHERE n.lease_id=$1
            AND a.kind IN ('idle_notice','reclaim_notice') AND n.answered_at IS NULL
            AND n.invalidated_at IS NULL)""", lease_id)
        if not issued:
            return False
    changed = await conn.fetchval('''UPDATE compute_leases SET last_activity_at=now()
        WHERE id=$1 AND ($2::text IS NULL OR owner=$2) RETURNING id''', lease_id, owner)
    if changed:
        field = 'answered_at' if answer else 'invalidated_at'
        await conn.execute(f'''UPDATE compute_lease_notices n SET {field}=now()
            FROM compute_lease_actions a WHERE n.action_id=a.id AND n.lease_id=$1
            AND a.kind IN ('idle_notice','reclaim_notice') AND n.{field} IS NULL''', lease_id)
    return bool(changed)


async def _action(conn, lease_id, kind, payload):
    action_id = uuid.uuid4()
    await conn.execute('INSERT INTO compute_lease_actions (id, lease_id, kind, payload) VALUES ($1,$2,$3,$4::jsonb)',
                       action_id, lease_id, kind, payload)
    return action_id


async def _owner_push(conn, row, text, dedupe):
    pieces = row['owner'].split(':', 2)
    if len(pieces) != 3 or pieces[0] != 'agent' or not pieces[1] or not pieces[2]:
        raise ValueError('invalid stored owner identity')
    return await conn.fetchval("""INSERT INTO item_pushes
        (daemon_id, session_uid, pid, kind, text, dedupe)
        VALUES ($1,$2,$3,'compute_lease',$4,$5)
        ON CONFLICT (dedupe) DO NOTHING RETURNING id""",
        pieces[1], pieces[2], row['owner'], text, dedupe)


async def _quarantine_box(conn, box, reason, lease, policy, *, transport=False):
    # The caller holds the scheduling advisory lock. Older writes may have
    # encoded JSON twice, so inspect the decoded object rather than SQL ->>.
    row = await conn.fetchrow('SELECT evidence FROM compute_boxes WHERE name=$1', box)
    if row is None:
        raise ValueError(f'unknown compute box {box}')
    evidence = _json_object(row['evidence']) or {}
    original = evidence.get('quarantine_reason')
    existing_outage = evidence.get('transport_outage_id')
    history = evidence.get('quarantine_reasons')
    if not isinstance(history, list):
        history = [original] if original else []
    if reason in history or reason == original:
        await conn.execute('UPDATE compute_boxes SET enabled=false WHERE name=$1', box)
        return False
    evidence['quarantine_reason'] = original or reason
    evidence['quarantine_reasons'] = [*history, reason]
    if transport and not existing_outage:
        evidence['transport_outage_id'] = str(uuid.uuid4())
    await conn.execute("""UPDATE compute_boxes SET enabled=false, ever_enabled=true, checked_at=now(),
        evidence=$2::jsonb WHERE name=$1""", box, evidence)
    if transport and existing_outage:
        return False
    logger.warning('compute box %s quarantined: %s', box, reason)
    operator = policy.get('quarantine_operator_pid')
    if operator and operator != lease['owner']:
        await _owner_push(conn, {'owner': operator},
            f"[cm-lease] box {box} quarantined: {reason}; manual review required",
            f'compute-box-quarantine-operator:{uuid.uuid4()}')
    return True


async def _outage_owner_push(conn, lease, text):
    evidence = _json_object(await conn.fetchval('SELECT evidence FROM compute_boxes WHERE name=$1', lease['box'])) or {}
    outage = evidence['transport_outage_id']
    await _owner_push(conn, lease, text, f"compute-lease-outage:{lease['id']}:{outage}")


async def _notice_action(conn, row, kind, extra=None):
    payload = {'box': row['box'], 'owner': row['owner'], **(extra or {})}
    action_id = await _action(conn, row['id'], kind, payload)
    text = {
        'idle_notice': f"[cm-lease] lease {row['id']} is idle; release, extend, or check its status",
        'reclaim_notice': f"[cm-lease] lease {row['id']} may be reclaimed after two unanswered idle notices",
        'expiry_notice': f"[cm-lease] lease {row['id']} is approaching expiry",
    }[kind]
    push_id = await _owner_push(conn, row, text, f'compute-lease:{action_id}')
    await conn.execute('INSERT INTO compute_lease_notices (action_id, lease_id, push_id, answer_tracking) VALUES ($1,$2,$3,true)',
                       action_id, row['id'], push_id)


async def _schedule(conn, policy_data, boxes):
    enabled = {r['name'] for r in await conn.fetch('SELECT name FROM compute_boxes WHERE enabled')}
    boxes = [box for box in boxes if box.configured_enabled and box.name in enabled]
    policy = Policy(**{k: policy_data[k] for k in Policy.__dataclass_fields__})
    rows = await _rows(conn)
    held_boxes = set()
    deferred_teams = set()
    for row in queue_order(rows):
        # A box that is disabled or too small cannot help this request yet;
        # it must not hold up its team or reserve other boxes.
        if not _fits_any_box(row, policy, boxes):
            continue
        if session_cap_blocked(row, rows, policy) or justification_missing(row, policy):
            continue
        # Yield capacity to other teams, but never let this team's later
        # requests pass its soft-cap-deferred head.
        if row['team'] in deferred_teams:
            continue
        if limit_reason(row, rows, policy, boxes) == 'soft_team':
            deferred_teams.add(row['team'])
            continue
        choice = select_box(row, rows, [b for b in boxes if b.name not in held_boxes], policy,
                            enabled_boxes=boxes)
        if not choice:
            # Keep all boxes that could accommodate this request available for
            # it, even if their current free slots would fit a later request.
            # Boxes too small for it may still serve later requests.
            needed = slots_needed(row, policy)
            held_boxes.update(b.name for b in boxes if len(b.slots) >= needed)
            continue
        box, selected = choice
        expires = datetime.now(timezone.utc) + timedelta(seconds=row['duration_seconds'])
        cpuset = ','.join(selected)
        await conn.execute("UPDATE compute_leases SET status='granting', box=$2, cpuset=$3, slot_ids=$4, expires_at=$5, ssh_target=$6 WHERE id=$1",
                           row['id'], box.name, cpuset, list(selected), expires, box.ssh_target)
        await _action(conn, row['id'], 'grant', dict(box=box.name, owner=row['owner'],
                      slots=len(selected), mem_gb=row['mem_gb'], pin=cpuset if row['pinned'] else None,
                      expires_epoch=int(expires.timestamp())))
        row.update(status='granting', box=box.name, slot_ids=list(selected), expires_at=expires)


def _public(row, boxes):
    result = dict(row)
    if row['status'] == 'active':
        if row['ssh_target'] and row['workdir']:
            box = next((b for b in boxes if b.name == row['box']), None)
            if box and box.run_argv:
                remote = ['sudo', '-n', '/usr/local/bin/cm-lease', 'run', '--lease-id', str(row['id']), '--']
                # `gcloud --command` takes one shell string. Quote each
                # appended job argument remotely, preserving the prefix API.
                code = (f'import os, shlex, sys; argv = {list(box.run_argv)!r}; '
                        f'command = {remote!r} + sys.argv[1:]; '
                        "os.execvp(argv[0], argv + ['--command', shlex.join(command)])")
                result['run_prefix'] = shlex.join(['python3', '-c', code])
            else:
                result['run_prefix'] = (f"ssh {shlex.quote(row['ssh_target'])} sudo -n /usr/local/bin/cm-lease run "
                                        f"--lease-id {row['id']} --")
    return result


@router.post('/request')
async def request_lease(body: LeaseRequest, request: Request):
    owner = body.actor.checked()
    p, boxes = settings()
    if body.priority not in ('v221-gate', 'normal', 'low') or body.lease_class not in ('burst', 'long'):
        raise HTTPException(422, 'invalid priority or class')
    if not body.purpose.strip() or body.duration > p[body.lease_class + '_max_seconds']:
        raise HTTPException(422, 'invalid purpose or duration')
    policy = Policy(**{k: p[k] for k in Policy.__dataclass_fields__})
    requested = body.model_dump()
    if slots_needed(requested, policy) > policy.max_slots_per_session:
        raise HTTPException(422, 'request exceeds the configured per-session limit')
    if slots_needed(requested, policy) > policy.max_slots_per_lease and not body.justification.strip():
        raise HTTPException(422, 'justification is required for requests above the per-lease slot cap')
    if not _fits_any_box(requested, policy, boxes):
        raise HTTPException(422, 'request cannot fit any configured box')
    ident = uuid.uuid4()
    async with _pool(request).acquire() as conn, conn.transaction():
        await _lock(conn)
        enabled = {r['name'] for r in await conn.fetch('SELECT name FROM compute_boxes WHERE enabled')}
        enabled_boxes = [b for b in boxes if b.configured_enabled and b.name in enabled]
        if not _fits_any_box(requested, policy, enabled_boxes):
            raise HTTPException(409, 'no enabled box fits this request')
        team = await conn.fetchval("""SELECT m.primary_team_id::text FROM incarnations i
            JOIN members m ON m.id=i.member_id WHERE i.pid=$1 AND i.ended_at IS NULL""", owner)
        await conn.execute("""INSERT INTO compute_leases
            (id,owner,team,purpose,priority,class,cpus,mem_gb,duration_seconds,pinned,justification,status)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'queued')""", ident, owner,
            team, body.purpose, body.priority, body.lease_class, body.cpus,
            body.mem_gb, body.duration, body.pinned, body.justification.strip() or None)
        await _schedule(conn, p, boxes)
        rows = await _rows(conn)
        row = next(x for x in rows if x['id'] == ident)
        if row['status'] == 'queued':
            ordered = queue_order(rows)
            result = {'lease_id': ident, 'status': 'queued', 'queue_position': 1 + ordered.index(row),
                      'eta': queue_eta(rows, datetime.now(timezone.utc)),
                      'eta_basis': 'earliest_capacity_change_not_guaranteed_start'}
        else:
            result = {'lease_id': ident, 'status': row['status'], 'queue_position': 0,
                      'eta': None}
    if body.priority == 'v221-gate':
        logger.info('v221-gate request lease=%s owner=%r team=%r purpose=%r time=%s',
                    ident, owner, team, body.purpose, row['created_at'].isoformat())
    return result


@router.post('/status')
async def status(body: StatusRequest, request: Request):
    owner = body.actor.checked()
    p, boxes = settings()
    async with _pool(request).acquire() as conn, conn.transaction():
        await _lock(conn)
        if body.lease_id:
            row = await conn.fetchrow('SELECT * FROM compute_leases WHERE id=$1 AND owner=$2', body.lease_id, owner)
            if not row:
                raise HTTPException(404, 'lease not found')
            await _record_activity(conn, body.lease_id, owner, only_issued_notice=True)
        rows = await _rows(conn)
        box_rows = await conn.fetch('SELECT name,enabled,evidence FROM compute_boxes')
        box_state = {r['name']: r for r in box_rows}
        enabled = {r['name'] for r in box_rows if r['enabled']}
        enabled_boxes = [b for b in boxes if b.configured_enabled and b.name in enabled]
        policy = Policy(**{k: p[k] for k in Policy.__dataclass_fields__})
        no_fit = {r['id'] for r in rows if r['status'] == 'queued' and not _fits_any_box(r, policy, enabled_boxes)}
        unjustified = {r['id'] for r in rows if r['status'] == 'queued' and justification_missing(r, policy)}
        mine = [_public(r, boxes) for r in rows if r['owner'] == owner]
        for lease in mine:
            if lease['id'] in no_fit:
                lease['waiting'] = 'no enabled box fits'
            elif lease['id'] in unjustified:
                lease['waiting'] = 'justification required'
            if lease['id'] in unjustified:
                lease['missing_justification'] = True
        recent = await conn.fetch("""SELECT * FROM compute_leases
            WHERE owner=$1 AND status IN ('released','failed')
            ORDER BY COALESCE(released_at, created_at) DESC, id DESC LIMIT $2""",
                                  owner, RECENT_LEASE_LIMIT)
        mine.extend(_public(dict(r), boxes) for r in recent)
        if (body.lease_id and row['status'] not in ('queued', 'granting', 'active', 'releasing')
                and not any(lease['id'] == row['id'] for lease in mine)):
            mine.append(_public(dict(row), boxes))
        released = {lease['id']: lease for lease in mine
                    if lease['status'] == 'released' or (lease['status'] == 'failed' and lease['released_at'])}
        if released:
            # An expired grant can itself release the node lease. Otherwise
            # the completed release action contains the cleanup outcome.
            results = await conn.fetch("""SELECT lease_id, kind, result
                FROM compute_lease_actions WHERE lease_id=ANY($1::uuid[]) AND status='done'
                AND kind IN ('release', 'grant')
                ORDER BY lease_id, (kind='release') DESC, created_at DESC, id DESC""", list(released))
            for lease in released.values():
                lease.update(cleanup_skipped=None, workdir_bytes=None)
            seen = set()
            for action in results:
                if action['lease_id'] in seen:
                    continue
                result = action['result']
                # Older writes may be JSON strings inside JSONB. A pool with
                # the JSONB codec already decodes the outer layer; a pool
                # without it returns both layers as text.
                for _ in range(2):
                    if not isinstance(result, str):
                        break
                    try:
                        result = json.loads(result)
                    except ValueError:
                        break
                if (isinstance(result, dict) and result.get('released') is True and
                        (action['kind'] == 'release' or result.get('expired') is True)):
                    released[action['lease_id']].update(
                        cleanup_skipped=result.get('cleanup_skipped'),
                        workdir_bytes=result.get('workdir_bytes'))
                    seen.add(action['lease_id'])
        ordered = queue_order(rows)
        queue = [{'lease_id': r['id'], 'priority': r['priority'], 'cpus': r['cpus'],
                  'owner': r['owner'], 'team': r['team'], 'purpose': r['purpose'], 'justification': r['justification'],
                  'position': i + 1,
                  **({'missing_justification': True} if r['id'] in unjustified else {}),
                  **({'waiting': 'no enabled box fits'} if r['id'] in no_fit else
                     {'waiting': 'justification required'} if r['id'] in unjustified else {})}
                 for i, r in enumerate(ordered)]
        occupancy = []
        for b in boxes:
            evidence = _json_object(box_state[b.name]['evidence']) if b.name in box_state else {}
            _, reservation_error = reservation_state(b, rows, policy)
            reason = (evidence or {}).get('quarantine_reason') or reservation_error
            occupancy.append({'box': b.name, 'enabled': b.configured_enabled and b.name in enabled,
                              'total_slots': len(b.slots),
                              'occupied_slots': sum(len(r['slot_ids'] or []) for r in rows
                                                    if r['box'] == b.name and r['status'] in ('granting', 'active', 'releasing')),
                              'quarantined': bool(reason), 'quarantine_reason': reason,
                              'quarantine_reasons': (evidence or {}).get('quarantine_reasons', [reason] if reason else [])})
        return {'leases': mine, 'queue': queue, 'pool': occupancy,
                'eta': queue_eta(rows, datetime.now(timezone.utc))}


@router.post('/release')
async def release(body: LeaseChange, request: Request):
    owner = body.actor.checked()
    p, boxes = settings()
    # This is evidence of attention even if the requested transition is busy.
    async with _pool(request).acquire() as conn, conn.transaction():
        await _lock(conn)
        await _record_activity(conn, body.lease_id, owner)
    async with _pool(request).acquire() as conn, conn.transaction():
        await _lock(conn)
        row = await conn.fetchrow('SELECT * FROM compute_leases WHERE id=$1 AND owner=$2', body.lease_id, owner)
        if not row:
            raise HTTPException(404, 'lease not found')
        if row['status'] == 'queued':
            await conn.execute("UPDATE compute_leases SET status='released', released_at=now(), last_activity_at=now() WHERE id=$1", body.lease_id)
            await _schedule(conn, p, boxes)
            return {'lease_id': body.lease_id, 'status': 'released'}
        if row['status'] == 'granting':
            # The remote grant may already be in flight. Keep capacity until
            # it is confirmed and then immediately dispatch teardown.
            await conn.execute('UPDATE compute_leases SET release_requested=true WHERE id=$1', body.lease_id)
            return {'lease_id': body.lease_id, 'status': 'release_requested'}
        if row['status'] == 'active':
            pending = await conn.fetchval("SELECT EXISTS(SELECT 1 FROM compute_lease_actions WHERE lease_id=$1 AND kind='extend' AND status <> 'done')", body.lease_id)
            if pending:
                raise HTTPException(409, 'extension in progress; retry')
            await conn.execute("UPDATE compute_leases SET status='releasing', last_activity_at=now() WHERE id=$1", body.lease_id)
            await _action(conn, body.lease_id, 'release', {'box': row['box']})
            return {'lease_id': body.lease_id, 'status': 'releasing'}
        if row['status'] in ('released', 'releasing'):
            return {'lease_id': body.lease_id, 'status': row['status']}
        raise HTTPException(409, 'lease transition in progress; retry')


@router.post('/extend')
async def extend(body: LeaseChange, request: Request):
    owner = body.actor.checked()
    if body.duration is None:
        raise HTTPException(422, 'duration is required')
    p, boxes = settings()
    # Commit the acknowledgment separately: a denied extension rolls back its
    # transition, but must not erase evidence that its owner is paying attention.
    async with _pool(request).acquire() as conn, conn.transaction():
        await _lock(conn)
        await _record_activity(conn, body.lease_id, owner)
    async with _pool(request).acquire() as conn, conn.transaction():
        await _lock(conn)
        row = await conn.fetchrow('SELECT * FROM compute_leases WHERE id=$1 AND owner=$2', body.lease_id, owner)
        if not row:
            raise HTTPException(404, 'lease not found')
        if row['status'] != 'active':
            raise HTTPException(409, 'lease is not active')
        if row['expires_at'] <= datetime.now(timezone.utc):
            raise HTTPException(409, 'lease has expired')
        rows = await _rows(conn)
        target = next((box for box in boxes if box.name == row['box']), None)
        if target is None:
            raise HTTPException(503, 'lease box is missing from the pool configuration')
        policy = Policy(**{k: p[k] for k in Policy.__dataclass_fields__})
        if any(r['status'] == 'queued' and r['owner'] != owner and not session_cap_blocked(r, rows, policy) and
               not justification_missing(r, policy) and
               slots_needed(r, policy) <= len(target.slots) for r in rows):
            raise HTTPException(409, 'other sessions are waiting')
        if body.duration > p[row['class'] + '_max_seconds']:
            raise HTTPException(422, 'extension exceeds class duration limit')
        expires = row['expires_at'] + timedelta(seconds=body.duration)
        if not row['granted_at'] or expires > row['granted_at'] + timedelta(seconds=p['absolute_max_seconds']):
            raise HTTPException(422, 'extension exceeds configured total duration')
        pending = await conn.fetchval("SELECT EXISTS(SELECT 1 FROM compute_lease_actions WHERE lease_id=$1 AND kind='extend' AND status <> 'done')", body.lease_id)
        if pending:
            raise HTTPException(409, 'extension already pending')
        await conn.execute('UPDATE compute_leases SET last_activity_at=now() WHERE id=$1', body.lease_id)
        await _action(conn, body.lease_id, 'extend', {'box': row['box'], 'expires_epoch': int(expires.timestamp())})
        return {'lease_id': body.lease_id, 'status': 'extending', 'requested_expires_at': expires}


class ActionReceipt(BaseModel):
    action_id: uuid.UUID
    result: dict


class BoxEnable(BaseModel):
    box: str
    local_drained: bool
    node_drained: bool
    node_max_slots: int = Field(gt=0, strict=True)
    clear_quarantine: bool = False


@router.post('/executor/pre-enable')
async def executor_pre_enable(request: Request):
    """Read-only inventory of queued work before the first box is enabled."""
    p, boxes = settings()
    policy = Policy(**{k: p[k] for k in Policy.__dataclass_fields__})
    async with _pool(request).acquire() as conn, conn.transaction(isolation='repeatable_read', readonly=True):
        rows = await _rows(conn)
        box_rows = await conn.fetch('SELECT name,enabled,ever_enabled,evidence FROM compute_boxes')
        enabled = {r['name'] for r in box_rows if r['enabled']}
        def was_enabled(row):
            evidence = _json_object(row['evidence']) or {}
            return (row['enabled'] or row['ever_enabled'] or bool(evidence.get('quarantine_reason')) or
                    (evidence.get('local_drained') is True and evidence.get('node_drained') is True))
        queued = [{'lease_id': row['id'], 'owner': row['owner'], 'team': row['team'],
                   'priority': row['priority'], 'cpus': row['cpus'], 'mem_gb': row['mem_gb'],
                   'purpose': row['purpose'], 'created_at': row['created_at'],
                   'missing_justification': justification_missing(row, policy)}
                  for row in queue_order(rows)]
        return {'queued_count': len(queued), 'queued': queued,
                'has_ever_enabled': any(was_enabled(row) for row in box_rows),
                'quarantined_boxes': [row['name'] for row in box_rows
                                      if (_json_object(row['evidence']) or {}).get('quarantine_reason')],
                'enabled_boxes': [box.name for box in boxes if box.configured_enabled and box.name in enabled
                                  and not reservation_state(box, rows, policy)[1]]}


async def _reconcile_notices(conn, p, boxes):
    notices = await conn.fetch("""SELECT n.action_id, n.lease_id, a.kind,
            ip.delivered_at, ip.dropped_reason
        FROM compute_lease_notices n JOIN compute_lease_actions a ON a.id=n.action_id
        JOIN item_pushes ip ON ip.id=n.push_id
        WHERE a.status <> 'done' AND ip.delivered_at IS NOT NULL""")
    for notice in notices:
        await conn.execute("UPDATE compute_lease_actions SET status='done', result=$2::jsonb WHERE id=$1",
            notice['action_id'], {'delivered': notice['dropped_reason'] is None})
        if notice['dropped_reason'] is not None:
            continue
        await conn.execute('UPDATE compute_lease_notices SET sent_at=$2 WHERE action_id=$1',
                           notice['action_id'], notice['delivered_at'])
        # Reclamation is considered only on a fresh cgroup sample. The
        # notification receipt alone says nothing about current CPU activity.


@router.post('/executor/enable')
async def executor_enable(body: BoxEnable, request: Request):
    p, boxes = settings()
    box = next((b for b in boxes if b.name == body.box and b.configured_enabled), None)
    if not box or not body.local_drained or not body.node_drained:
        raise HTTPException(409, 'box is not configured for cutover or has not drained')
    if len(box.slots) > body.node_max_slots:
        # Revalidation may discover drift on a previously enabled box. Stop
        # new grants without disturbing its existing reservations.
        async with _pool(request).acquire() as conn, conn.transaction():
            await _lock(conn)
            await conn.execute('UPDATE compute_boxes SET enabled=false WHERE name=$1', body.box)
        raise HTTPException(409, 'configured pool slots exceed node max_slots')
    async with _pool(request).acquire() as conn, conn.transaction():
        await _lock(conn)
        existing = await conn.fetchrow('SELECT evidence FROM compute_boxes WHERE name=$1', body.box)
        evidence = _json_object(existing['evidence']) if existing else {}
        if evidence is None:
            raise HTTPException(409, 'box evidence is invalid; manual review is required')
        if evidence.get('quarantine_reason') and not body.clear_quarantine:
            raise HTTPException(409, 'box is quarantined; manual review is required')
        await conn.execute("""INSERT INTO compute_boxes (name, enabled, checked_at, evidence, ever_enabled)
            VALUES ($1,true,now(),$2::jsonb,true) ON CONFLICT (name) DO UPDATE
            SET enabled=true, checked_at=now(), evidence=EXCLUDED.evidence, ever_enabled=true""",
            body.box, {'local_drained': True, 'node_drained': True,
                       'node_max_slots': body.node_max_slots})
        await _schedule(conn, p, boxes)
    return {'enabled': True, 'box': body.box}


@router.post('/executor/poll')
async def executor_poll(request: Request):
    p, boxes = settings()
    async with _pool(request).acquire() as conn, conn.transaction():
        await _lock(conn)
        await _reconcile_notices(conn, p, boxes)
        due = await conn.fetch("""SELECT * FROM compute_leases WHERE status='active'
            AND expires_at > now() AND expires_at <= now() + make_interval(secs => $1)""",
            float(p['expiry_warning_seconds']))
        for row in due:
            epoch = int(row['expires_at'].timestamp())
            # Pre-fix JSONB rows can contain a JSON string instead of an
            # object. Recognize both forms to avoid reissuing old warnings.
            previous = await conn.fetch("SELECT payload FROM compute_lease_actions WHERE lease_id=$1 AND kind='expiry_notice'", row['id'])
            warned = any((_json_object(action['payload']) or {}).get('expires_epoch') == epoch
                         for action in previous)
            if not warned:
                await _notice_action(conn, row, 'expiry_notice', {'expires_epoch': epoch})
        # Do not free a reservation merely because its clock elapsed. The
        # node release must be confirmed first, even across API restarts.
        expired = await conn.fetch("SELECT * FROM compute_leases WHERE status='active' AND expires_at <= now()")
        for row in expired:
            if await conn.fetchval("SELECT EXISTS(SELECT 1 FROM compute_lease_actions WHERE lease_id=$1 AND kind IN ('grant','extend','release') AND status <> 'done')", row['id']):
                continue
            await conn.execute("""UPDATE item_pushes SET delivered_at=now(), dropped_reason='lease expired'
                WHERE id IN (SELECT n.push_id FROM compute_lease_notices n JOIN compute_lease_actions a ON a.id=n.action_id
                             WHERE n.lease_id=$1 AND a.status <> 'done') AND delivered_at IS NULL""", row['id'])
            await conn.execute("""UPDATE compute_lease_actions SET status='done', result='{"cancelled":"expired"}'::jsonb
                WHERE lease_id=$1 AND kind IN ('idle_notice','reclaim_notice','expiry_notice') AND status <> 'done'""", row['id'])
            await conn.execute("UPDATE compute_leases SET status='releasing' WHERE id=$1", row['id'])
            await _action(conn, row['id'], 'release', {'box': row['box']})
        await _schedule(conn, p, boxes)
        action = await conn.fetchrow("""SELECT * FROM compute_lease_actions
            WHERE kind IN ('grant','extend','release') AND
                  (status='pending' OR (status='claimed' AND claimed_until < now()))
            ORDER BY created_at, id LIMIT 1 FOR UPDATE""")
        if not action:
            return {'action': None}
        await conn.execute("UPDATE compute_lease_actions SET status='claimed', claimed_until=now() + interval '2 minutes' WHERE id=$1", action['id'])
        payload = action['payload']
        if isinstance(payload, str):
            payload = json.loads(payload)
        return {'action': {'action_id': action['id'], 'lease_id': action['lease_id'],
                           'kind': action['kind'], 'payload': payload}}


@router.post('/executor/complete')
async def executor_complete(body: ActionReceipt, request: Request):
    p, boxes = settings()
    async with _pool(request).acquire() as conn, conn.transaction():
        await _lock(conn)
        action = await conn.fetchrow('SELECT * FROM compute_lease_actions WHERE id=$1 FOR UPDATE', body.action_id)
        if not action:
            raise HTTPException(404, 'action not found')
        if action['status'] == 'done':
            return {'ok': True, 'already_done': True}
        if body.result.get('lease_id') != str(action['lease_id']):
            raise HTTPException(422, 'node result lease ID mismatch')
        lease = await conn.fetchrow('SELECT * FROM compute_leases WHERE id=$1', action['lease_id'])
        recorded_result = body.result
        if action['kind'] == 'grant':
            if lease['status'] != 'granting':
                raise HTTPException(409, 'unexpected or incomplete grant result')
            if body.result.get('retryable') is True:
                reason = body.result.get('error')
                if not isinstance(reason, str) or not reason.strip():
                    raise HTTPException(422, 'retryable grant error must include a reason')
                reason = 'grant attempt failed: ' + reason[:2000]
                await conn.execute('UPDATE compute_leases SET error=$2 WHERE id=$1', lease['id'], reason)
                if lease['expires_at'] and lease['expires_at'] <= datetime.now(timezone.utc):
                    quarantine = 'expired grant remains unconfirmed after retryable node error'
                    await _quarantine_box(conn, lease['box'], quarantine, lease, p, transport=True)
                    await _outage_owner_push(conn, lease,
                        f"[cm-lease] lease {lease['id']} expired while grant remains unconfirmed; box {lease['box']} quarantined: {reason}")
                if lease['release_requested']:
                    # The grant may have partially succeeded. Release it on the
                    # node before freeing the reservation.
                    await conn.execute("UPDATE compute_leases SET status='releasing' WHERE id=$1", lease['id'])
                    await _action(conn, lease['id'], 'release', {'box': lease['box'], 'failed_grant': True})
                    await conn.execute("UPDATE compute_lease_actions SET status='done', result=$2::jsonb WHERE id=$1",
                                       action['id'], body.result)
                    return {'ok': True, 'releasing': True}
                # Preserve claimed_until to avoid a tight retry loop.
                return {'ok': True, 'retryable': True}
            if body.result.get('refused') is True:
                reason = body.result.get('error')
                if not isinstance(reason, str) or not reason.strip():
                    raise HTTPException(422, 'node refusal must include a reason')
                reason = reason[:2000]
                await conn.execute("UPDATE compute_leases SET status='releasing', error=$2 WHERE id=$1",
                                   lease['id'], 'node refused grant: ' + reason)
                if reason == 'box capacity exceeded':
                    quarantine = 'node refused grant: box capacity exceeded'
                    await _quarantine_box(conn, lease['box'], quarantine, lease, p)
                # A refusal may follow a partial node operation. Keep the slot
                # until release confirms that the node is no longer using it.
                await _action(conn, lease['id'], 'release', {'box': lease['box'], 'failed_grant': True})
                detail = (f"; box {lease['box']} quarantined; manual review required"
                          if reason == 'box capacity exceeded' else '')
                await _owner_push(conn, lease, f"[cm-lease] lease {lease['id']} grant refused: {reason}{detail}",
                                  f"compute-lease-grant-refused:{action['id']}")
            elif body.result.get('expired') is True and body.result.get('released') is True:
                reason = lease['error'] or 'grant expired before activation'
                await conn.execute("UPDATE compute_leases SET status='failed', released_at=now(), error=$2 WHERE id=$1",
                                   lease['id'], reason)
                await _owner_push(conn, lease, f"[cm-lease] lease {lease['id']} failed: {reason}",
                                  f"compute-lease-grant-expired:{action['id']}")
            elif body.result.get('configured') and body.result.get('workdir'):
                if lease['release_requested']:
                    await conn.execute("UPDATE compute_leases SET status='releasing', granted_at=now(), workdir=$2, error=NULL WHERE id=$1", lease['id'], body.result['workdir'])
                    await _action(conn, lease['id'], 'release', {'box': lease['box']})
                else:
                    await conn.execute("UPDATE compute_leases SET status='active', granted_at=now(), workdir=$2, error=NULL WHERE id=$1", lease['id'], body.result['workdir'])
            else:
                raise HTTPException(409, 'unexpected or incomplete grant result')
        elif action['kind'] == 'extend':
            if lease['status'] != 'active':
                raise HTTPException(409, 'unexpected extension result')
            if body.result.get('retryable') is True:
                reason = body.result.get('error')
                if not isinstance(reason, str) or not reason.strip():
                    raise HTTPException(422, 'retryable extension error must include a reason')
                reason = 'extension attempt failed: ' + reason[:2000]
                await conn.execute('UPDATE compute_leases SET error=$2 WHERE id=$1', lease['id'], reason)
                if lease['expires_at'] and lease['expires_at'] <= datetime.now(timezone.utc):
                    quarantine = 'expired extension remains unconfirmed after retryable node error'
                    await _quarantine_box(conn, lease['box'], quarantine, lease, p, transport=True)
                    await _outage_owner_push(conn, lease,
                        f"[cm-lease] lease {lease['id']} expired while extension remains unconfirmed; box {lease['box']} quarantined: {reason}")
                    # An extension may have reached the node. Reconcile by
                    # releasing it, retaining the reservation until confirmed.
                    await conn.execute("UPDATE compute_leases SET status='releasing' WHERE id=$1", lease['id'])
                    await _action(conn, lease['id'], 'release', {'box': lease['box']})
                    await conn.execute("UPDATE compute_lease_actions SET status='done', result=$2::jsonb WHERE id=$1",
                                       action['id'], body.result)
                    return {'ok': True, 'releasing': True}
                return {'ok': True, 'retryable': True}
            if body.result.get('absent') is True and body.result.get('unit_inactive') is True:
                # The extend failed and a node probe proved the slice absent.
                # Still keep the reservation until release is acknowledged.
                await conn.execute("UPDATE compute_leases SET status='releasing', error='extension failed: node lease absent' WHERE id=$1", lease['id'])
                recorded_result = {**body.result, 'ok': False, 'error': 'extension failed: node lease absent'}
                await _action(conn, lease['id'], 'release', {'box': lease['box']})
            else:
                payload = action['payload']
                if isinstance(payload, str):
                    payload = json.loads(payload)
                if body.result.get('expires') != payload['expires_epoch']:
                    raise HTTPException(422, 'node expiry mismatch')
                await conn.execute('UPDATE compute_leases SET expires_at=to_timestamp($2), error=NULL WHERE id=$1', lease['id'], payload['expires_epoch'])
        elif action['kind'] == 'release':
            if lease['status'] != 'releasing':
                raise HTTPException(409, 'unexpected release result')
            if body.result.get('retryable') is True:
                reason = body.result.get('error')
                if not isinstance(reason, str) or not reason.strip():
                    raise HTTPException(422, 'retryable release error must include a reason')
                reason = 'release attempt failed: ' + reason[:2000]
                if lease['granted_at'] is not None or not lease['error']:
                    await conn.execute('UPDATE compute_leases SET error=$2 WHERE id=$1', lease['id'], reason)
                if lease['expires_at'] and lease['expires_at'] <= datetime.now(timezone.utc):
                    quarantine = 'expired release remains unconfirmed after retryable node error'
                    await _quarantine_box(conn, lease['box'], quarantine, lease, p, transport=True)
                    await _outage_owner_push(conn, lease,
                        f"[cm-lease] lease {lease['id']} expired while release remains unconfirmed; box {lease['box']} quarantined: {reason}")
                return {'ok': True, 'retryable': True}
            if body.result.get('released') is not True:
                raise HTTPException(409, 'unexpected release result')
            payload = action['payload']
            if isinstance(payload, str):
                payload = json.loads(payload)
            terminal = 'failed' if payload.get('failed_grant') is True else 'released'
            await conn.execute('UPDATE compute_leases SET status=$2, released_at=now() WHERE id=$1', lease['id'], terminal)
            await conn.execute('DELETE FROM compute_lease_samples WHERE lease_id=$1', lease['id'])
            await conn.execute('DELETE FROM compute_lease_sampling_epochs WHERE lease_id=$1', lease['id'])
        else:
            raise HTTPException(409, 'notice delivery is acknowledged by the owner daemon')
        await conn.execute("UPDATE compute_lease_actions SET status='done', result=$2::jsonb WHERE id=$1", body.action_id, recorded_result)
        await _schedule(conn, p, boxes)
        return {'ok': True}


class SampleReceipt(BaseModel):
    lease_id: uuid.UUID
    cpu_usage_usec: int = Field(ge=0)


@router.post('/executor/samples')
async def sample_targets(request: Request):
    async with _pool(request).acquire() as conn:
        rows = await conn.fetch("SELECT id, box FROM compute_leases WHERE status='active'")
        return {'leases': [{'lease_id': r['id'], 'box': r['box']} for r in rows]}


async def _idle_decision(conn, row, p, boxes, now):
    records = await conn.fetch("""SELECT sampled_at, usage_usec FROM compute_lease_samples
        WHERE lease_id=$1 AND sampled_at >= $2 ORDER BY sampled_at""", row['id'],
        now - timedelta(seconds=p['idle_threshold_seconds'] + p['sample_max_gap_seconds']))
    samples = [Sample(r['sampled_at'], r['usage_usec']) for r in records]
    average = idle_average(samples, now, len(row['slot_ids']) * p['cpus_per_slot'],
                           p['idle_threshold_seconds'], p['sample_max_gap_seconds'])
    if average is not None and average >= p['idle_percent']:
        return None
    notices = await conn.fetch("""SELECT n.sent_at FROM compute_lease_notices n
        JOIN compute_lease_actions a ON a.id=n.action_id
        WHERE n.lease_id=$1 AND a.kind='idle_notice' AND n.answer_tracking
          AND n.answered_at IS NULL AND n.invalidated_at IS NULL
          AND n.sent_at>$2 ORDER BY n.sent_at""",
                               row['id'], row['last_activity_at'])
    rows = await _rows(conn)
    enabled = {r['name'] for r in await conn.fetch('SELECT name FROM compute_boxes WHERE enabled')}
    enabled_boxes = [b for b in boxes if b.configured_enabled and b.name in enabled]
    target = next((b for b in enabled_boxes if b.name == row['box']), None)
    policy = Policy(**{k: p[k] for k in Policy.__dataclass_fields__})
    without = [r for r in rows if r['id'] != row['id']]
    fits = bool(target and any(select_box(q, without, [target], policy, enabled_boxes=enabled_boxes)
                               for q in queue_order(without)))
    return next_idle_action(now=now, average=average, threshold=p['idle_percent'],
        last_activity=row['last_activity_at'], notices=[n['sent_at'] for n in notices],
        idle_seconds=p['idle_threshold_seconds'], answer_seconds=p['idle_answer_seconds'],
        notice_limit=p['notice_limit'], expires_at=row['expires_at'],
        expiry_guard_seconds=p['notice_expiry_guard_seconds'], queued_fit=fits)


@router.post('/executor/sample')
async def sample(body: SampleReceipt, request: Request):
    p, boxes = settings()
    async with _pool(request).acquire() as conn, conn.transaction():
        await _lock(conn)
        row = await conn.fetchrow("SELECT * FROM compute_leases WHERE id=$1 AND status='active'", body.lease_id)
        if not row:
            return {'accepted': False}
        now = datetime.now(timezone.utc)
        previous = await conn.fetchrow('''SELECT sampled_at, usage_usec FROM compute_lease_samples
            WHERE lease_id=$1 ORDER BY sampled_at DESC LIMIT 1''', body.lease_id)
        initialized = await conn.fetchval('''INSERT INTO compute_lease_sampling_epochs (lease_id)
            VALUES ($1) ON CONFLICT (lease_id) DO NOTHING RETURNING lease_id''', body.lease_id)
        reset = previous is None or initialized is not None
        if previous and initialized is None:
            elapsed = (now - previous['sampled_at']).total_seconds()
            used = body.cpu_usage_usec - previous['usage_usec']
            if elapsed <= 0 or elapsed > p['sample_max_gap_seconds'] or used < 0:
                reset = True  # a gap or counter reset cannot prove idleness
            else:
                cores = len(row['slot_ids']) * p['cpus_per_slot']
                reset = 100 * used / (elapsed * 1_000_000 * cores) >= p['idle_percent']
        await conn.execute('INSERT INTO compute_lease_samples (lease_id, sampled_at, usage_usec) VALUES ($1,$2,$3)',
                           body.lease_id, now, body.cpu_usage_usec)
        await conn.execute('DELETE FROM compute_lease_samples WHERE lease_id=$1 AND sampled_at<$2', body.lease_id,
            now - timedelta(seconds=p['idle_threshold_seconds'] + 2*p['sample_max_gap_seconds']))
        if reset:
            await _record_activity(conn, row['id'], answer=False)
            return {'accepted': True}
        pending = await conn.fetchval("SELECT EXISTS(SELECT 1 FROM compute_lease_actions WHERE lease_id=$1 AND kind IN ('idle_notice','reclaim_notice') AND status <> 'done')", body.lease_id)
        decision = await _idle_decision(conn, dict(row), p, boxes, now)
        if decision and not pending:
            if decision == 'reclaim':
                notified_at = await conn.fetchval("""SELECT max(n.sent_at) FROM compute_lease_notices n
                    JOIN compute_lease_actions a ON a.id=n.action_id WHERE n.lease_id=$1
                    AND a.kind='reclaim_notice' AND n.answer_tracking AND n.answered_at IS NULL
                    AND n.invalidated_at IS NULL AND n.sent_at>$2""", row['id'], row['last_activity_at'])
                pending_extend = await conn.fetchval("SELECT EXISTS(SELECT 1 FROM compute_lease_actions WHERE lease_id=$1 AND kind='extend' AND status <> 'done')", row['id'])
                if (notified_at and (now-notified_at).total_seconds() >= p['reclaim_notice_grace_seconds']
                        and not pending_extend):
                    await conn.execute("UPDATE compute_leases SET status='releasing' WHERE id=$1", row['id'])
                    await _action(conn, row['id'], 'release', {'box': row['box']})
                elif not notified_at:
                    attempted = await conn.fetchval("""SELECT EXISTS(SELECT 1 FROM compute_lease_actions
                        WHERE lease_id=$1 AND kind='reclaim_notice' AND created_at>$2
                        AND created_at>now()-make_interval(secs => $3))""",
                        row['id'], row['last_activity_at'], float(p['reclaim_notice_grace_seconds']))
                    if not attempted:
                        await _notice_action(conn, row, 'reclaim_notice')
            else:
                # Failed deliveries do not count as unanswered notices and do
                # not cause a tight requeue loop when a session is offline.
                attempted = await conn.fetchval("""SELECT EXISTS(SELECT 1 FROM compute_lease_actions
                    WHERE lease_id=$1 AND kind='idle_notice' AND created_at>$2
                    AND created_at>now()-make_interval(secs => $3))""",
                    row['id'], row['last_activity_at'], float(p['idle_answer_seconds']))
                if not attempted:
                    await _notice_action(conn, row, 'idle_notice')
        return {'accepted': True}
