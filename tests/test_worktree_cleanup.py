from __future__ import annotations

import dataclasses
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch
import uuid

from scripts import worktree_cleanup as cleanup, worktree_lineage as lineage, worktree_reaper as reaper


class CleanupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='cm-cleanup-test-')
        self.base = Path(self.temp.name)
        self.cm = self.base / 'cm'
        self.root = self.cm / 'worktrees'
        self.root.mkdir(parents=True)
        self.env = patch.dict(os.environ, {'CM_LINEAGE_HOME': str(self.cm), 'HOME': str(self.base), 'CM_TUI_SESSION_ID': ''})
        self.env.start()
        self.repo = self.base / 'repo'
        self.repo.mkdir()
        self.git(self.repo, 'init', '-b', 'main')
        self.git(self.repo, 'config', 'user.name', 'Cleanup test')
        self.git(self.repo, 'config', 'user.email', 'test@example.invalid')
        (self.repo / '.gitignore').write_text('data/\n.env\n')
        (self.repo / 'file.txt').write_text('base\n')
        self.git(self.repo, 'add', 'file.txt', '.gitignore')
        self.git(self.repo, 'commit', '-qm', 'base')
        self.git(self.repo, 'update-ref', 'refs/remotes/origin/main', 'HEAD')
        self.worktree = self.root / 'parent'
        self.git(self.repo, 'worktree', 'add', '-b', 'parent', str(self.worktree))
        self.parent = lineage.register(self.worktree, task_ids=['task-1'], inherit_session=False)
        self.context = reaper.ScanContext(root=self.root, cm_home=self.cm, config_path=self.cm / 'daemon.toml',
            manifest_paths=(), now=time.time(), retention_days=7, unowned_retention_days=7,
            workspaces={self.worktree: reaper.WorkspaceFacts(task_ids={'task-1'})},
            tasks={'task-1': reaper.TaskFacts('task-1', 'done', 'oneshot', None, None, None)},
            tasks_by_branch={}, live_paths=set(), process_paths={}, task_state_available=True,
            session_state_available=True, fetch=False, artifact_root=self.cm / 'worktree-artifacts')

    def tearDown(self):
        self.env.stop()
        self.temp.cleanup()

    @staticmethod
    def git(path, *args):
        return subprocess.check_output(['git', '-C', str(path), *args], text=True, stderr=subprocess.DEVNULL).strip()

    def child(self, name, parent=None):
        path = self.root / name
        self.git(parent or self.worktree, 'worktree', 'add', '-b', name, str(path))
        return path

    def job(self, rows=None):
        return {'id': str(uuid.uuid4()), 'phase': 'queued', 'task_id': 'task-1', 'family': ['task-1'],
                'candidates': rows or [self.parent]}

    def run_apply(self, job):
        with patch.object(cleanup, 'base_context', return_value=self.context), patch.object(reaper, 'refresh_dynamic_state'), patch.object(reaper, 'process_references', return_value={}):
            cleanup.apply(job)
        return cleanup.read_job(job['id'])

    def test_raw_git_hook_tracks_nested_descendants_and_chains_existing_hook(self):
        hook = self.repo / '.git/hooks/post-checkout'
        evidence = self.base / 'old-hook.log'
        hook.write_text(f'#!/bin/sh\nprintf "%s\\n" "$3" >> "{evidence}"\n')
        hook.chmod(0o755)
        lineage.install_hook(self.repo)
        child = self.child('raw-child')
        grandchild = self.child('raw-grandchild', child)
        records = lineage.records()
        child_record = next(row for row in records.values() if row['path'] == str(child))
        grand_record = next(row for row in records.values() if row['path'] == str(grandchild))
        self.assertIn(self.parent['id'], child_record['parents'])
        self.assertIn(child_record['id'], grand_record['parents'])
        self.assertEqual(evidence.read_text(), '1\n1\n')
        self.assertEqual(cleanup.inherited_owners(records)[grand_record['id']], {'task-1'})
        lineage.install_hook(self.repo)
        self.assertEqual(hook.read_text().count(lineage.MARKER), 1)

    def test_hook_keeps_preexisting_failure_and_does_not_reparent_branch_switches(self):
        hook = self.repo / '.git/hooks/post-checkout'
        hook.write_text('#!/bin/sh\nexit 7\n')
        hook.chmod(0o755)
        lineage.install_hook(self.repo)
        child = self.root / 'hook-exit'
        proc = subprocess.run(['git', '-C', str(self.worktree), 'worktree', 'add', '-b', 'hook-exit', str(child)], capture_output=True)
        self.assertEqual(proc.returncode, 7)
        before = next(row for row in lineage.records().values() if row['path'] == str(child))
        subprocess.run(['git', '-C', str(child), 'checkout', '-b', 'other'], capture_output=True)
        after = lineage.records()[before['id']]
        self.assertEqual(before['parents'], after['parents'])

    def test_checkout_local_hooks_are_not_modified_or_claimed_as_tracked(self):
        hooks = self.repo / '.hooks'
        hooks.mkdir()
        hook = hooks / 'post-checkout'
        original = '#!/bin/sh\nexit 0\n'
        hook.write_text(original)
        hook.chmod(0o755)
        for configured in ('.hooks', str(hooks)):
            with self.subTest(configured=configured):
                self.git(self.repo, 'config', 'core.hooksPath', configured)
                with self.assertRaisesRegex(ValueError, 'leaving checkout-'):
                    lineage.install_hook(self.repo)
                self.assertEqual(hook.read_text(), original)
                self.assertFalse(hook.with_name('post-checkout.before-cm-lineage').exists())

    def test_absolute_shared_custom_hook_directory_is_chained(self):
        hooks = self.base / 'shared-hooks'
        hooks.mkdir()
        hook = hooks / 'post-checkout'
        hook.write_text('#!/bin/sh\nexit 0\n')
        hook.chmod(0o755)
        self.git(self.repo, 'config', 'core.hooksPath', str(hooks))
        lineage.install_hook(self.repo)
        child = self.child('custom-hook-child')
        row = next(r for r in lineage.records().values() if r['path'] == str(child))
        self.assertIn(self.parent['id'], row['parents'])
        self.assertIn(lineage.MARKER, hook.read_text())

    def test_shared_hook_in_a_separate_dotfiles_repository_is_not_modified(self):
        hooks = self.base / 'dotfiles'
        hooks.mkdir()
        self.git(hooks, 'init')
        hook = hooks / 'post-checkout'
        hook.write_text('#!/bin/sh\nexit 0\n')
        self.git(hooks, 'add', 'post-checkout')
        self.git(self.repo, 'config', 'core.hooksPath', str(hooks))
        with self.assertRaisesRegex(ValueError, 'tracked hook'):
            lineage.install_hook(self.repo)
        self.assertNotIn(lineage.MARKER, hook.read_text())

    def test_deleted_recreated_path_gets_a_different_checkout_identity(self):
        old = self.parent['id']
        self.git(self.repo, 'worktree', 'remove', str(self.worktree))
        self.git(self.repo, 'worktree', 'add', str(self.worktree), 'parent')
        self.assertNotEqual(lineage.checkout(self.worktree)['id'], old)
        self.assertNotIn(old, lineage.records())
        result = self.run_apply(self.job())
        self.assertTrue(self.worktree.exists())
        self.assertIn('identity changed', result['results'][0]['message'])

    def test_task_family_and_raw_descendants_exclude_unrelated_checkouts(self):
        self.context.tasks['child-task'] = reaper.TaskFacts('child-task', 'done', 'oneshot', None, None, None, 'task-1')
        child = self.child('cm-child')
        child_record = lineage.register(child, task_ids=['child-task'], inherit_session=False)
        raw = self.child('raw-grandchild', child)
        raw_record = lineage.register(raw, child, inherit_session=False)
        unrelated = self.child('unrelated')
        unrelated_record = lineage.register(unrelated, task_ids=['task-2'], inherit_session=False)
        rows = cleanup.selected_records(lineage.records(), cleanup.task_family(self.context.tasks, 'task-1'), self.parent)
        self.assertEqual({r['id'] for r in rows}, {self.parent['id'], child_record['id'], raw_record['id']})
        self.assertNotIn(unrelated_record['id'], {r['id'] for r in rows})

    def test_immediate_cleanup_overrides_age_but_preserves_wip_branch_and_artifacts(self):
        (self.worktree / 'file.txt').write_text('unique uncommitted work\n')
        (self.worktree / 'data').mkdir()
        (self.worktree / 'data/result.txt').write_text('important result\n')
        self.assertFalse(reaper.decision(self.worktree, self.context).eligible)
        result = self.run_apply(self.job())
        self.assertTrue(result['results'][0]['removed'], result)
        self.assertFalse(self.worktree.exists())
        self.assertEqual(self.git(self.repo, 'show', 'parent:file.txt'), 'unique uncommitted work')
        artifacts = list((self.cm / 'worktree-artifacts').rglob('result.txt'))
        self.assertEqual(len(artifacts), 1)
        self.assertEqual(artifacts[0].read_text(), 'important result\n')

    def test_live_sessions_shared_tasks_and_continuous_work_are_retained(self):
        for mode in ('live', 'shared', 'continuous'):
            with self.subTest(mode=mode):
                original = self.context
                self.context = dataclasses.replace(original, live_paths={self.worktree} if mode == 'live' else set())
                if mode == 'shared':
                    lineage.register(self.worktree, task_ids=['other-task'], inherit_session=False)
                if mode == 'continuous':
                    self.context = dataclasses.replace(original, workspaces={self.worktree: reaper.WorkspaceFacts(continuous=True)})
                result = self.run_apply(self.job())
                self.assertFalse(result['results'][0]['removed'])
                self.assertTrue(self.worktree.exists())
                expected = {'live': 'live_session', 'shared': 'shared with another task', 'continuous': 'continuous_workspace'}[mode]
                self.assertIn(expected, result['results'][0]['message'])
                lineage.atomic_json(self.cm / 'worktree-lineage' / (self.parent['id'] + '.json'), self.parent)
                self.context = original

    def test_primary_checkout_is_never_removed(self):
        primary = lineage.register(self.repo, task_ids=['task-1'], inherit_session=False)
        result = self.run_apply(self.job([primary]))
        self.assertFalse(result['results'][0]['removed'])
        self.assertIn('primary', result['results'][0]['message'])
        self.assertTrue(self.repo.exists())

    def test_missing_state_and_live_process_guard_remain_effective(self):
        ctx = cleanup.scoped_context(self.context, self.parent, {'task-1'})
        for field in ('task_state_available', 'session_state_available'):
            self.assertFalse(reaper.decision(self.worktree, dataclasses.replace(ctx, **{field: False})).eligible)
        ctx.process_paths = {self.worktree: {123}}
        self.assertEqual(reaper.decision(self.worktree, ctx).reason, 'live_process_reference')

    def test_preview_is_read_only_and_apply_request_is_idempotent(self):
        job = self.job()
        job.update(worktree_path=str(self.worktree), phase='scanning')
        with patch.object(cleanup, 'base_context', return_value=self.context):
            cleanup.preview(job)
        self.assertTrue(self.worktree.exists())
        self.assertEqual(cleanup.read_job(job['id'])['phase'], 'preview')
        with patch.object(cleanup, 'spawn') as spawn:
            first = cleanup.request({'action': 'apply', 'id': job['id']})
            second = cleanup.request({'action': 'apply', 'id': job['id']})
        self.assertEqual(first['phase'], 'queued')
        self.assertEqual(second['phase'], 'queued')
        spawn.assert_called_once_with(job['id'])

    def test_changed_parent_and_nested_untracked_checkout_protect_parent(self):
        nested = self.worktree / 'nested'
        self.git(self.repo, 'worktree', 'add', '-b', 'nested', str(nested))
        result = self.run_apply(self.job())
        self.assertFalse(result['results'][0]['removed'])
        self.assertIn('nested checkout', result['results'][0]['message'])
        self.assertTrue(nested.exists())

    def workspace_preview(self):
        job = {'id': str(uuid.uuid4()), 'phase': 'scanning', 'task_id': None, 'worktree_path': str(self.worktree)}
        with patch.object(cleanup, 'base_context', return_value=self.context), patch.object(reaper, 'process_references', return_value={}):
            cleanup.preview(job)
        return job

    def test_workspace_close_owns_its_bound_task_and_reaps_terminal_descendants(self):
        child = self.child('workspace-raw-child')
        lineage.register(child, self.worktree, inherit_session=False)
        job = self.workspace_preview()
        self.assertEqual(job['root_task_ids'], ['task-1'])
        self.assertEqual(job['family'], ['task-1'])
        self.assertTrue(all(r['reason'] != 'shared_with_another_task' for r in job['candidates']))
        result = self.run_apply(job)
        self.assertEqual(sum(r['removed'] for r in result['results']), 2)
        self.assertFalse(child.exists())

    def test_workspace_close_explains_running_task_instead_of_false_sharing(self):
        self.context.tasks['task-1'] = dataclasses.replace(self.context.tasks['task-1'], status='running')
        job = self.workspace_preview()
        self.assertEqual(job['candidates'][0]['reason'], 'task_not_terminal')
        self.assertEqual(job['root_tasks'][0]['status'], 'running')
        with patch.object(cleanup.time, 'monotonic', side_effect=[0, 31]):
            result = self.run_apply(job)
        self.assertFalse(result['results'][0]['removed'])
        self.assertIn('task_not_terminal', result['results'][0]['message'])

    def test_resolved_workspace_task_waits_for_done_update_before_cleanup(self):
        self.context.tasks['task-1'] = dataclasses.replace(self.context.tasks['task-1'], status='running')
        job = self.workspace_preview()
        def finish_task(_):
            self.context.tasks['task-1'] = dataclasses.replace(self.context.tasks['task-1'], status='done')
        with patch.object(cleanup.time, 'sleep', side_effect=finish_task) as sleep:
            result = self.run_apply(job)
        sleep.assert_any_call(1)
        self.assertTrue(result['results'][0]['removed'])

    def test_workspace_close_rechecks_new_owners_against_preview_scope(self):
        job = self.workspace_preview()
        lineage.register(self.worktree, task_ids=['new-owner'], inherit_session=False)
        result = self.run_apply(job)
        self.assertFalse(result['results'][0]['removed'])
        self.assertIn('shared with another task', result['results'][0]['message'])

    def test_already_reaped_workspace_can_preview_and_complete_again(self):
        first = self.run_apply(self.workspace_preview())
        self.assertTrue(first['results'][0]['removed'])
        job = self.workspace_preview()
        self.assertEqual(job['phase'], 'preview')
        self.assertEqual(job['root_task_ids'], ['task-1'])
        self.assertEqual(job['candidates'], [])
        self.assertEqual(job['warnings'], [])
        self.assertIn('already absent', job['message'])
        result = self.run_apply(job)
        self.assertEqual(result['phase'], 'complete')
        self.assertIn('retained 0', result['message'])
        self.assertEqual(self.git(self.repo, 'show', 'parent:file.txt'), 'base')

    def test_missing_workspace_still_finds_durable_descendants_without_manifest(self):
        child = self.child('surviving-child')
        child_record = lineage.register(child, self.worktree, inherit_session=False)
        self.git(self.repo, 'worktree', 'remove', str(self.worktree))
        self.context.workspaces = {}
        job = self.workspace_preview()
        self.assertEqual([row['id'] for row in job['candidates']], [child_record['id']])
        self.assertEqual(job['root_task_ids'], ['task-1'])
        result = self.run_apply(job)
        self.assertTrue(result['results'][0]['removed'], result)
        self.assertFalse(child.exists())

    def test_unknown_missing_workspace_closes_without_claiming_other_checkouts(self):
        unrelated = self.child('unrelated')
        lineage.register(unrelated, task_ids=['task-2'], inherit_session=False)
        self.git(self.repo, 'worktree', 'remove', str(self.worktree))
        (self.cm / 'worktree-lineage' / (self.parent['id'] + '.json')).unlink()
        self.context.workspaces = {}
        job = self.workspace_preview()
        self.assertEqual(job['root_task_ids'], [])
        self.assertEqual(job['candidates'], [])
        self.assertEqual(self.run_apply(job)['phase'], 'complete')
        self.assertTrue(unrelated.exists())

    def test_missing_checkout_during_apply_is_a_noop_without_weakening_reused_path_guard(self):
        job = self.workspace_preview()
        self.git(self.repo, 'worktree', 'remove', str(self.worktree))
        result = self.run_apply(job)
        self.assertTrue(result['results'][0]['already_absent'])
        self.assertFalse(result['results'][0]['removed'])
        self.assertIn('already absent 1; retained 0', result['message'])
        self.git(self.repo, 'worktree', 'add', str(self.worktree), 'parent')
        lineage.register(self.worktree, task_ids=['new-owner'], inherit_session=False)
        result = self.run_apply(job)
        self.assertTrue(self.worktree.exists())
        self.assertFalse(result['results'][0]['already_absent'])
        self.assertIn('identity changed', result['results'][0]['message'])

    def test_checkout_recreated_after_missing_preview_is_outside_approved_candidates(self):
        self.git(self.repo, 'worktree', 'remove', str(self.worktree))
        job = self.workspace_preview()
        self.git(self.repo, 'worktree', 'add', str(self.worktree), 'parent')
        lineage.register(self.worktree, task_ids=['new-owner'], inherit_session=False)
        self.run_apply(job)
        self.assertTrue(self.worktree.exists())

    def test_missing_root_does_not_bypass_active_descendant_protection(self):
        child = self.child('live-child')
        lineage.register(child, self.worktree, inherit_session=False)
        self.context.live_paths = {child}
        self.git(self.repo, 'worktree', 'remove', str(self.worktree))
        job = self.workspace_preview()
        self.assertEqual(job['candidates'][0]['reason'], 'live_session')
        result = self.run_apply(job)
        self.assertFalse(result['results'][0]['removed'])
        self.assertTrue(child.exists())

    def test_invalid_existing_checkout_and_dangling_symlink_are_not_absent(self):
        self.git(self.repo, 'worktree', 'remove', str(self.worktree))
        self.worktree.symlink_to(self.root / 'nonexistent')
        with self.assertRaisesRegex(ValueError, 'symlink'):
            self.workspace_preview()
        self.worktree.unlink()
        self.worktree.mkdir()
        with self.assertRaises(subprocess.CalledProcessError):
            self.workspace_preview()

    def test_request_ids_cannot_escape_job_directory(self):
        with self.assertRaises(ValueError):
            cleanup.request({'action': 'status', 'id': '../daemon-sessions'})

    def test_creation_links_are_not_inferred_from_branch_ancestry(self):
        child = self.child('unattributed')
        with patch.object(cleanup, 'base_context', return_value=self.context):
            records, _ = cleanup.inventory(self.context, self.worktree)
        record = next(row for row in records.values() if row['path'] == str(child))
        self.assertEqual(record['parents'], [])
        self.assertEqual(record['task_ids'], [])
        self.assertNotIn(record['id'], {r['id'] for r in cleanup.selected_records(records, {'task-1'}, self.parent)})

    def test_ownership_changed_during_wip_preservation_prevents_removal(self):
        original = reaper.preserve_dirty_worktree
        def change_owner(*args):
            result = original(*args)
            lineage.register(self.worktree, task_ids=['another-task'], inherit_session=False)
            return result
        with patch.object(reaper, 'preserve_dirty_worktree', side_effect=change_owner):
            result = self.run_apply(self.job())
        self.assertFalse(result['results'][0]['removed'])
        self.assertIn('cleanup_scope_changed', result['results'][0]['message'])
        self.assertTrue(self.worktree.exists())

    def test_nested_unrelated_repository_created_during_archive_is_retained(self):
        original = reaper.archive_ignored_paths
        def add_checkout(*args):
            result = original(*args)
            nested = self.worktree / 'data/nested'
            nested.mkdir(parents=True)
            self.git(nested, 'init')
            return result
        with patch.object(reaper, 'archive_ignored_paths', side_effect=add_checkout):
            result = self.run_apply(self.job())
        self.assertFalse(result['results'][0]['removed'])
        self.assertTrue((self.worktree / 'data/nested/.git').exists())

    def test_files_created_after_preservation_are_not_discarded(self):
        original = reaper.archive_ignored_paths
        for ignored in (False, True):
            with self.subTest(ignored=ignored):
                late = self.worktree / ('data/late.txt' if ignored else 'late.txt')
                def add_file(*args):
                    result = original(*args)
                    late.parent.mkdir(exist_ok=True)
                    late.write_text('late important work')
                    return result
                with patch.object(reaper, 'archive_ignored_paths', side_effect=add_file):
                    result = self.run_apply(self.job())
                self.assertFalse(result['results'][0]['removed'])
                self.assertEqual(late.read_text(), 'late important work')
                late.unlink()

    def test_reparented_cm_child_and_pinned_ancestor_protect_raw_descendants(self):
        child = self.child('child')
        row = lineage.register(child, self.worktree, task_ids=['child-task'], inherit_session=False)
        self.context.tasks['child-task'] = reaper.TaskFacts('child-task', 'done', 'oneshot', None, None, None, 'different-parent')
        job = self.job([row])
        job['family'].append('child-task')
        result = self.run_apply(job)
        self.assertIn('shared with another task', result['results'][0]['message'])
        self.context.tasks['child-task'] = dataclasses.replace(self.context.tasks['child-task'], parent_task_id='task-1')
        self.context.workspaces[self.worktree].pinned = True
        result = self.run_apply(job)
        self.assertIn('pinned_workspace', result['results'][0]['message'])
        self.assertTrue(child.exists())

    def test_deleted_parent_keeps_descendant_lineage_without_claiming_reused_path(self):
        child = self.child('child')
        row = lineage.register(child, self.worktree, inherit_session=False)
        self.git(self.repo, 'worktree', 'remove', str(self.worktree))
        self.git(self.repo, 'worktree', 'add', str(self.worktree), 'parent')
        lineage.register(self.worktree, task_ids=['unrelated'], inherit_session=False)
        rows = cleanup.selected_records(lineage.records(), {'task-1'}, None)
        self.assertEqual([r['id'] for r in rows], [row['id']])
        facts = cleanup.scope_facts(self.context, row, {'task-1'}, 'task-1')
        self.assertEqual(facts.task_ids, {'task-1'})

    def test_symlink_checkout_is_rejected(self):
        link = self.root / 'alias'
        link.symlink_to(self.worktree)
        with self.assertRaisesRegex(ValueError, 'symlink'):
            lineage.checkout(link)

    def test_session_task_is_precise_and_git_cwd_and_session_parent_are_both_recorded(self):
        lineage.atomic_json(self.cm / 'daemon-sessions.json', {
            'workspaces': {'ws': {'id': 'ws', 'worktree_path': str(self.worktree),
                                  'sessions': [{'session_uid': 'session-1', 'task_id': 'task-1'}]}},
            'bindings': {'task-1': 'ws', 'other-task': 'ws'}})
        child = self.child('from-primary', self.repo)
        with patch.dict(os.environ, {'GIT_DIR': str(self.repo / '.git')}):
            row = lineage.register(child, self.repo, session_uid='session-1')
        self.assertEqual(row['task_ids'], ['task-1'])
        self.assertIn(self.parent['id'], row['parents'])
        self.assertIn(lineage.checkout(self.repo)['id'], row['parents'])

    def test_nonterminal_task_after_failed_close_is_not_removed(self):
        self.context.tasks['task-1'] = dataclasses.replace(self.context.tasks['task-1'], status='running')
        with patch.object(cleanup.time, 'monotonic', side_effect=[0, 31]):
            result = self.run_apply(self.job())
        self.assertFalse(result['results'][0]['removed'])
        self.assertIn('task_not_terminal', result['results'][0]['message'])

    def test_preview_retry_and_worker_resume_are_idempotent(self):
        job = self.job()
        job.update(phase='scanning', worktree_path=str(self.worktree))
        cleanup.write_job(job)
        with patch.object(cleanup, 'spawn') as spawn:
            result = cleanup.request({'action': 'preview', 'id': job['id'], 'task_id': 'task-1', 'worktree_path': str(self.worktree)})
        self.assertEqual(result['id'], job['id'])
        spawn.assert_not_called()
        with patch.object(cleanup, 'base_context', return_value=self.context), patch.object(reaper, 'refresh_dynamic_state'), patch.object(reaper, 'process_references', return_value={}):
            cleanup.worker(job['id'])
            with patch.object(cleanup, 'spawn'):
                cleanup.request({'action': 'apply', 'id': job['id']})
            cleanup.worker(job['id'])
            before = cleanup.read_job(job['id'])
            cleanup.worker(job['id'])
            self.assertEqual(cleanup.read_job(job['id']), before)
        self.assertTrue(before['results'][0]['removed'])


if __name__ == '__main__':
    unittest.main()
