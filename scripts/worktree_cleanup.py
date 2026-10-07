#!/usr/bin/env python3
"""Task-scoped cleanup previews and durable, detached cleanup jobs."""
from __future__ import annotations

import dataclasses
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid

try:
    from scripts import worktree_lineage as lineage, worktree_reaper as reaper
except ModuleNotFoundError:
    import worktree_lineage as lineage
    import worktree_reaper as reaper


def home() -> Path:
    return lineage.cm_home()


def job_path(ident: str) -> Path:
    ident = str(uuid.UUID(ident))
    return home() / 'worktree-cleanup' / (ident + '.json')


def read_job(ident: str) -> dict:
    return json.loads(job_path(ident).read_text())


def write_job(job: dict) -> None:
    job['updated_at'] = time.time()
    lineage.atomic_json(job_path(job['id']), job)


def task_family(tasks: dict[str, reaper.TaskFacts], root: str | None) -> set[str]:
    family = {root} if root else set()
    while True:
        added = {task.task_id for task in tasks.values() if task.parent_task_id in family} - family
        if not added:
            return family
        family |= added


def job_roots(job: dict) -> set[str]:
    """Legacy jobs retain their original scope; new workspace jobs pin owners."""
    return set(job.get('root_task_ids', [job['task_id']] if job.get('task_id') else []))


def task_families(tasks: dict[str, reaper.TaskFacts], roots: set[str]) -> set[str]:
    return set().union(*(task_family(tasks, root) for root in roots))


def inherited_owners(records: dict[str, dict]) -> dict[str, set[str]]:
    records = {**lineage.stored_records(), **records}
    owners = {key: set(row.get('task_ids', [])) for key, row in records.items()}
    # Fixed-point union also handles cycles conservatively: shared/ambiguous
    # ancestry inherits every claimant and is never guessed away.
    for _ in range(len(records)):
        changed = False
        for key, row in records.items():
            extra = set().union(*(owners.get(p, set()) for p in row.get('parents', [])))
            if not extra <= owners[key]:
                owners[key] |= extra
                changed = True
        if not changed:
            break
    return owners


def base_context() -> reaper.ScanContext:
    args = reaper.parser().parse_args(['--cm-home', str(home()), '--config', str(home() / 'daemon.toml'),
                                     '--root', str(home() / 'worktrees'), '--artifact-root', str(home() / 'worktree-artifacts'), '--no-fetch'])
    context, warnings = reaper.build_context(args)
    if not context.task_state_available or not context.session_state_available:
        raise RuntimeError('; '.join(warnings))
    return context


def path_absent(path: Path) -> bool:
    # lstat distinguishes a missing checkout from a dangling symlink. Only a
    # missing pathname is a no-op; inaccessible or invalid checkouts still fail.
    try:
        path.lstat()
    except FileNotFoundError:
        return True
    return False


def inventory(ctx: reaper.ScanContext, root: Path | None = None) -> tuple[dict[str, dict], list[str]]:
    warnings = []
    current = lineage.records()
    by_path = {Path(row['path']): row for row in current.values()}
    known = set(by_path) | set(ctx.workspaces)
    known.update(reaper.worktree_paths(home() / 'worktrees'))
    if root:
        known.add(root)
    repos: set[str] = set()
    # Git's registered inventory discovers raw worktrees outside ~/.cm/worktrees.
    # Discovery is not attribution: only exact manifest/native/creation evidence
    # confers task ownership.
    for path in sorted(known):
        try:
            if path_absent(path):
                continue
            common = lineage.common_dir(path)
            if common in repos:
                continue
            repos.add(common)
            for line in lineage.git(path, 'worktree', 'list', '--porcelain').splitlines():
                if line.startswith('worktree '):
                    known.add(Path(line[9:]).resolve())
            lineage.install_hook(path)
        except (OSError, ValueError, subprocess.SubprocessError) as exc:
            warnings.append(f'{path}: {exc}')
    registered = False
    links, _, native_warnings = reaper.load_native_parent_links(
        Path.home() / '.claude/projects', known, candidate_projects_only=True)
    warnings.extend(native_warnings)
    for path in sorted(known):
        try:
            if path_absent(path):
                continue
            facts = ctx.workspaces.get(path, reaper.WorkspaceFacts())
            task_ids = set(facts.task_ids)
            branch = lineage.current_branch(path)
            if ctx.tasks_by_branch.get(branch):
                url = lineage.origin_url(path)
                origin = reaper.canonical_repo_url(url) if url else None
                task_ids.update(task.task_id for task in ctx.tasks_by_branch.get(branch, [])
                                if origin and reaper.canonical_repo_url(task.repo_url) == origin)
            link = links.get(path)
            if up_to_date(by_path.get(path), task_ids, link, by_path):
                # Already registered with every owner and parent this scan
                # would add: registering again would only re-run Git to
                # recompute an identity the stored record already proves.
                continue
            lineage.register(path, link.parent_worktree if link else None, task_ids=task_ids,
                             source='native-metadata' if link else 'inventory', inherit_session=False)
            registered = True
        except (OSError, ValueError, subprocess.SubprocessError) as exc:
            warnings.append(f'{path}: {exc}')
    return (lineage.records() if registered else current), warnings


def up_to_date(record: dict | None, task_ids: set[str], link, by_path: dict[Path, dict]) -> bool:
    """True when `register` could not change this checkout's record: its
    identity is current (`lineage.records()` checked the stamp), it already
    lists every task id found, and any native parent link is already a
    recorded parent. Unknown parents always re-register."""
    if record is None or not task_ids <= set(record.get('task_ids', [])):
        return False
    if link is None:
        return True
    parent = by_path.get(Path(link.parent_worktree).resolve()) if link.parent_worktree else None
    return parent is not None and parent['id'] in record.get('parents', [])


def selected_records(records: dict[str, dict], family: set[str], root: dict | None) -> list[dict]:
    owners = inherited_owners(records)
    selected = {key for key in records if owners[key] & family}
    if root:
        selected.add(root['id'])
    while True:
        added = {key for key, row in records.items() if set(row.get('parents', [])) & selected} - selected
        if not added:
            return [records[key] for key in sorted(selected) if key in records]
        selected |= added


def scope_facts(ctx: reaper.ScanContext, row: dict, approved_family: set[str], root_task: str | None, root_task_ids: set[str] | None = None) -> reaper.WorkspaceFacts:
    """Called again by the reaper before/after preservation and final removal."""
    path = Path(row['path'])
    fresh = lineage.checkout(path, create=False)
    if fresh['id'] != row['id'] or fresh['primary']:
        raise ValueError('checkout identity changed or primary checkout')
    records = lineage.records()
    current = records.get(row['id'])
    if not current or set(current.get('parents', [])) != set(row.get('parents', [])):
        raise ValueError('checkout ownership changed since preview')
    ancestors = {row['id']}
    pending = [row['id']]
    facts = reaper.WorkspaceFacts()
    while pending:
        ident = pending.pop()
        record = records.get(ident)
        if record is None:
            # A reaped ancestor retains its durable ownership evidence, but its
            # path must never be reinterpreted as a new checkout's identity.
            record_path = home() / 'worktree-lineage' / (str(uuid.UUID(ident)) + '.json')
            record = json.loads(record_path.read_text())
        facts.task_ids.update(record.get('task_ids', []))
        if ident in records:
            ws = ctx.workspaces.get(Path(record['path']), reaper.WorkspaceFacts())
            facts.task_ids.update(ws.task_ids)
            facts.pinned |= ws.pinned
            facts.continuous |= ws.continuous
        for parent in record.get('parents', []):
            if parent not in ancestors:
                ancestors.add(parent)
                pending.append(parent)
    roots = root_task_ids if root_task_ids is not None else ({root_task} if root_task else set())
    family = approved_family & task_families(ctx.tasks, roots)
    if facts.task_ids - family:
        raise ValueError('shared with another task')
    for record in records.values():
        other = Path(record['path'])
        if other != path and other.is_relative_to(path):
            raise ValueError(f'nested checkout retained: {other}')
    for line in lineage.git(path, 'worktree', 'list', '--porcelain').splitlines():
        if line.startswith('worktree '):
            other = Path(line[9:]).resolve()
            if other != path and other.is_relative_to(path) and other.exists():
                raise ValueError(f'nested checkout retained: {other}')
    def failed_walk(error):
        raise ValueError(f'cannot inspect nested checkouts: {error}')
    for current_dir, dirs, files in os.walk(path, followlinks=False, onerror=failed_walk):
        current_path = Path(current_dir)
        if current_path != path and ('.git' in dirs or '.git' in files):
            raise ValueError(f'nested checkout retained: {current_path}')
        if '.git' in dirs:
            dirs.remove('.git')
    return facts


def scoped_context(ctx: reaper.ScanContext, row: dict, owners: set[str], job: dict | None = None) -> reaper.ScanContext:
    path = Path(row['path'])
    guard = None
    if job is not None:
        def guard(fresh, _):
            return scope_facts(fresh, row, set(job['family']), job.get('task_id'), job_roots(job))
    return dataclasses.replace(ctx, root=path.parent, retention_days=0, unowned_retention_days=0,
                               immediate_paths={path: row['id']}, ownership={path: reaper.WorkspaceFacts(task_ids=owners)},
                               scope_guard=guard, process_paths=reaper.process_references(path.parent))


#: How long ``apply`` waits for a checkout's OWN work to let go before it gives
#: up. The operator chose "reap" at close time and should not have to come back
#: and choose again: closing a session is asynchronous, the agent's stdio MCP
#: child exits on its own a moment later, and under memory pressure that whole
#: settle can take minutes (2026-09-18: 30 children killed at once were each
#: blocked in a swap-in fault for several minutes before they noticed stdin had
#: closed). Pre-fix ``apply`` took exactly one verdict per checkout, so a
#: checkout whose only blocker was a process seconds from exiting was retained
#: permanently and the operator's "reap now" silently became "keep".
SETTLE_TIMEOUT_SECS = 300
SETTLE_POLL_SECS = 5

#: Refusals worth waiting on, and WHEN. A process reference is always worth
#: waiting on: a process holding a checkout is either exiting or it is not, and
#: a few minutes settles it either way. A live SESSION is only worth waiting on
#: when this job just closed one — otherwise the session belongs to somebody
#: still using it, and no amount of waiting changes that. Everything else
#: (pinned, continuous, shared with another task, a non-terminal task outside
#: the family) is a real answer, reported at once, so the operator is never
#: left waiting on a "no" that can never become a "yes".
SETTLES_ALWAYS = {'live_process_reference'}
SETTLES_AFTER_CLOSING = {'live_session'}

#: Preview verdicts that mean the checkout was never this job's to remove.
#: A live process in one of these is somebody else's, so waiting is wrong.
NOT_OURS = {'shared_with_another_task', 'primary_checkout', 'unknown_ownership'}


def daemon_rpc(method: str, params: dict, timeout: float = 15) -> dict:
    """One operator-framed call on the host's control socket. Same framing as
    ``reaper.load_live_paths``; kept here rather than imported so the reaper
    stays a read-only scanner."""
    import socket
    import struct
    cm = home()
    token = (cm / 'operator-token').read_text().strip()
    request = json.dumps({'id': 'worktree-cleanup', 'caller': {'token_id': token},
                          'method': method, 'params': params}).encode()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(timeout)
        sock.connect(str(cm / 'daemon.sock'))
        sock.sendall(struct.pack('>I', len(request)) + request)
        length = struct.unpack('>I', reaper.recv_exact(sock, 4))[0]
        return json.loads(reaper.recv_exact(sock, length))


def close_job_sessions(job: dict) -> list[str]:
    """Close the sessions belonging to the tasks this job was approved for.

    Choosing "reap" IS the instruction to close them; before this the job only
    ever observed sessions, so it depended on something else having closed them
    first and refused when nothing had. Scoped to the approved family, never to
    a checkout's co-tenants, and best-effort: a daemon that cannot be reached
    leaves the sessions alone and the settle loop below reports the honest
    reason.
    """
    family = set(job.get('family') or ()) | job_roots(job)
    if not family:
        return []
    closed: list[str] = []
    try:
        response = daemon_rpc('list_sessions', {})
        rows = response.get('result') or [] if response.get('ok') else []
    except Exception as exc:  # noqa: BLE001 - never let this fail the job
        job.setdefault('warnings', []).append(f'could not list sessions to close: {exc}')
        return []
    for row in rows:
        if not isinstance(row, dict) or row.get('task_id') not in family:
            continue
        # The scheduler and the workflow engine own these two lifecycles; a
        # cleanup job must not reach into them even inside its own family.
        if row.get('continuous_task_id') or row.get('workflow_run_id'):
            continue
        uid = row.get('session_uid')
        if not isinstance(uid, str):
            continue
        try:
            daemon_rpc('kill_session', {'session_uid': uid})
            closed.append(uid)
        except Exception as exc:  # noqa: BLE001
            job.setdefault('warnings', []).append(f'could not close {uid}: {exc}')
    return closed


def settles(reason: str, row: dict, closed_any: bool) -> bool:
    if row.get('reason') in NOT_OURS:
        return False
    return reason in SETTLES_ALWAYS or (closed_any and reason in SETTLES_AFTER_CLOSING)


def settle_decision(path: Path, row: dict, job: dict, budget: dict, closed_any: bool):
    """The candidate's verdict, waiting out refusals that are this job's own
    work finishing. Returns the final decision; the caller reaps or records it.

    ``budget`` carries one deadline shared by every checkout in the job, started
    lazily at the first refusal worth waiting on — a job that never has to wait
    reads no clock at all, and a wide family cannot multiply one settle window
    into one per checkout.
    """
    while True:
        ctx = base_context()
        owners = scope_facts(ctx, row, set(job['family']), job.get('task_id'), job_roots(job)).task_ids
        context = scoped_context(ctx, row, owners, job)
        candidate = reaper.decision(path, context, refresh_processes=True)
        if candidate.eligible or not settles(candidate.reason, row, closed_any):
            return candidate, context
        if budget.get('deadline') is None:
            budget['deadline'] = time.monotonic() + SETTLE_TIMEOUT_SECS
        elif time.monotonic() >= budget['deadline']:
            return candidate, context
        time.sleep(SETTLE_POLL_SECS)


def preview(job: dict) -> None:
    ctx = base_context()
    root_path = Path(job['worktree_path']) if job.get('worktree_path') else None
    missing_root = root_path is not None and path_absent(root_path)
    root = lineage.checkout(root_path) if root_path is not None and not missing_root else None
    records, warnings = inventory(ctx, root_path if not missing_root else None)
    if missing_root:
        # Retained creation evidence can still name surviving descendants.
        # Never register a missing checkout or transfer a removed UUID onto a
        # new checkout that happens to use the same pathname.
        prior = [row for row in lineage.stored_records().values() if row['path'] == str(root_path.resolve())]
        if len(prior) == 1:
            root = prior[0]
        elif prior:
            warnings.append(f'{root_path}: multiple historical checkout identities; descendant ancestry was not inferred')
    roots = {job['task_id']} if job.get('task_id') else set()
    if not roots and root_path is not None:
        # A workspace close approves its own directly associated tasks. An
        # absent task_id is not evidence that every owner is an unrelated task.
        if root:
            roots.update(records.get(root['id'], root).get('task_ids', []))
        roots.update(ctx.workspaces.get(root_path, reaper.WorkspaceFacts()).task_ids)
    family = task_families(ctx.tasks, roots)
    job['root_task_ids'] = sorted(roots)
    job['root_tasks'] = [dataclasses.asdict(ctx.tasks[tid]) for tid in sorted(roots) if tid in ctx.tasks]
    owners = inherited_owners(records)
    rows = selected_records(records, family, root)
    planned = []
    for row in rows:
        own = owners[row['id']]
        reason = 'primary_checkout' if row['primary'] else 'unknown_ownership' if not own and not root else None
        # Owners outside the explicitly selected task/workspace family stay protected.
        if own - family:
            reason = 'shared_with_another_task'
        if not reason:
            check = reaper.decision(Path(row['path']), scoped_context(ctx, row, own, {'family': family, 'task_id': job.get('task_id'), 'root_task_ids': sorted(roots)}))
            reason = check.reason
        planned.append({**row, 'owners': sorted(own), 'reason': reason})
    prefix = 'Workspace checkout already absent. ' if missing_root else ''
    job.update(phase='preview', family=sorted(family), candidates=planned, warnings=warnings,
               missing_root=missing_root,
               message=prefix + f'{len(planned)} tracked checkout(s). Active/shared work is retained; state is checked again after closing.')
    finish_preview(job)


def finish_preview(job: dict) -> None:
    """Save the preview, honouring an apply that arrived while it scanned.

    The viewer submits Reap without waiting for the preview: `request(apply)`
    then only records `apply_requested` on the scanning job. Merge that flag
    under the request lock so this final write cannot drop it."""
    path = job_path(job['id'])
    with lineage.locked(path.with_suffix('.request-lock')):
        try:
            requested = read_job(job['id']).get('apply_requested')
        except FileNotFoundError:
            requested = False
        if requested:
            job.update(apply_requested=True, phase='queued',
                       message='Cleanup queued; waiting for task closure.')
        write_job(job)


def apply(job: dict) -> None:
    # The viewer queues the durable request before marking the root done. Wait
    # briefly for that API update; an update failure must never be a delete pass.
    deadline = time.monotonic() + 30
    while True:
        ctx = base_context()
        tasks = [ctx.tasks[tid] for tid in job_roots(job) if tid in ctx.tasks]
        root_path = Path(job['worktree_path']) if job.get('worktree_path') else None
        closed = (all(task.status in reaper.TERMINAL_TASK_STATUSES for task in tasks)
                  if tasks else not root_path or root_path not in ctx.live_paths)
        if closed or time.monotonic() >= deadline:
            break
        time.sleep(1)
    closed = close_job_sessions(job)
    if closed:
        job.update(message=f'Closing {len(closed)} session(s) this task still owned, then reaping.',
                   closed_sessions=closed)
        write_job(job)
    settle_budget: dict = {'deadline': None}
    result_by_id = {row['id']: row for row in job.get('results', [])}
    # Descendants physically inside a checkout must be considered first. A
    # retained nested checkout prevents removing its enclosing directory.
    rows = sorted(job['candidates'], key=lambda row: len(Path(row['path']).parts), reverse=True)
    for row in rows:
        if result_by_id.get(row['id'], {}).get('removed'):
            continue
        path = Path(row['path'])
        removed = False
        already_absent = False
        message = ''
        try:
            if path_absent(path):
                # Another cleanup may have finished since preview (or this
                # worker crashed after removing it but before saving results).
                already_absent = True
                message = 'already absent; nothing to reap'
            else:
                fresh = lineage.checkout(path, create=False)
                if fresh['id'] != row['id']:
                    raise ValueError('checkout identity changed since preview')
                if fresh['primary']:
                    raise ValueError('primary checkout is always retained')
                candidate, context = settle_decision(path, row, job, settle_budget, bool(closed))
                if not candidate.eligible:
                    waited = settles(candidate.reason, row, bool(closed))
                    message = (f'retained: {candidate.reason} (still held after '
                               f'{SETTLE_TIMEOUT_SECS}s)' if waited else f'retained: {candidate.reason}')
                else:
                    removed, message, _ = reaper.reap_one(candidate, context, home() / 'worktree-reaper.jsonl')
        except (OSError, ValueError, subprocess.SubprocessError) as exc:
            message = f'retained: {exc}'
        result_by_id[row['id']] = {'id': row['id'], 'path': str(path), 'removed': removed, 'already_absent': already_absent, 'message': message}
        job['results'] = list(result_by_id.values())
        write_job(job)
    count = sum(row['removed'] for row in job.get('results', []))
    absent = sum(row.get('already_absent', False) for row in job.get('results', []))
    prefix = 'Workspace checkout already absent. ' if job.get('missing_root') else ''
    job.update(phase='complete', message=prefix + f'Reaped {count}; already absent {absent}; retained {len(rows) - count - absent}. Branches and preserved artifacts remain available.')
    write_job(job)


def spawn(ident: str) -> None:
    log = job_path(ident).with_suffix('.log')
    with log.open('ab') as stream:
        child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), 'worker', ident],
                                 stdin=subprocess.DEVNULL, stdout=stream, stderr=stream, start_new_session=True,
                                 cwd=str(home()), env={**os.environ, 'PATH': str(Path.home() / '.local/bin') + ':' + os.environ.get('PATH', '/usr/bin:/bin')})
        # Reaped by a tiny daemon-side waiter when invoked through the RPC;
        # the detached worker keeps running if this launcher exits.
        del child


def request(params: dict) -> dict:
    action = params.get('action')
    if action == 'preview':
        if not params.get('task_id') and not params.get('worktree_path'):
            raise ValueError('a task or worktree is required')
        if params.get('worktree_path') and not Path(params['worktree_path']).is_absolute():
            raise ValueError('worktree_path must be absolute')
        ident = str(uuid.UUID(params['id']))
        path = job_path(ident)
        with lineage.locked(path.with_suffix('.request-lock')):
            if path.exists():
                old = read_job(ident)
                if old.get('task_id') != params.get('task_id') or old.get('worktree_path') != params.get('worktree_path'):
                    raise ValueError('request ID already belongs to a different scope')
                return old
            job = {'id': ident, 'phase': 'scanning', 'task_id': params.get('task_id'),
                   'worktree_path': params.get('worktree_path'), 'created_at': time.time(), 'message': 'Finding associated worktrees…'}
            write_job(job)
            spawn(ident)
            return job
    ident = str(uuid.UUID(params['id']))
    if action == 'status':
        return read_job(ident)
    if action == 'apply':
        with lineage.locked(job_path(ident).with_suffix('.request-lock')):
            job = read_job(ident)
            if job['phase'] in ('queued', 'running', 'complete'):
                return job
            if job['phase'] == 'scanning':
                # Accepted durably now; the scanning worker runs the cleanup
                # as soon as its preview is saved (finish_preview).
                job.update(apply_requested=True,
                           message='Cleanup accepted; it starts when the preview finishes.')
                write_job(job)
                return job
            if job['phase'] != 'preview':
                raise ValueError('preview is not ready')
            job.update(phase='queued', message='Cleanup queued; waiting for task closure.')
            write_job(job)
            spawn(ident)
            return job
    raise ValueError('unknown cleanup action')


def worker(ident: str) -> None:
    path = job_path(ident)
    with lineage.locked(path.with_suffix('.worker-lock')):
        job = read_job(ident)
        try:
            if job['phase'] == 'scanning':
                preview(job)
            # A preview that found an apply request continues straight on.
            if job['phase'] in ('queued', 'running'):
                # Serialize with the daily reaper. UI/RPC returns independently.
                with lineage.locked(home() / 'worktree-reaper.lock'):
                    job.update(phase='running')
                    write_job(job)
                    apply(job)
        except Exception as exc:
            job.update(phase='error', message=f'{type(exc).__name__}: {exc}')
            write_job(job)


def bootstrap() -> None:
    """Install hooks/backfill exact ownership; resume jobs after host reboot."""
    try:
        inventory(base_context())
    except Exception as exc:
        print(f'worktree inventory: {exc}', file=sys.stderr)
    for path in (home() / 'worktree-cleanup').glob('*.json'):
        try:
            job = json.loads(path.read_text())
            if job['phase'] in ('scanning', 'queued', 'running'):
                spawn(job['id'])
        except (OSError, ValueError, KeyError):
            continue


if __name__ == '__main__':
    os.umask(0o077)
    if sys.argv[1:2] == ['rpc']:
        try:
            print(json.dumps(request(json.loads(sys.argv[2]))))
        except Exception as exc:
            print(json.dumps({'error': str(exc)}))
            sys.exit(1)
    elif sys.argv[1:2] == ['worker']:
        worker(sys.argv[2])
    elif sys.argv[1:2] == ['bootstrap']:
        bootstrap()
