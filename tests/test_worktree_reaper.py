from __future__ import annotations

import subprocess
import tempfile
import time
import unittest
from pathlib import Path

from scripts import worktree_reaper as reaper


class WorktreeReaperTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.base = Path(self.tempdir.name)
        self.main_repo = self.base / "repo"
        self.root = self.base / "cm" / "worktrees"
        self.worktree = self.root / "feature"
        self.root.mkdir(parents=True)
        self.git(self.base, "init", "-b", "main", str(self.main_repo))
        self.git(self.main_repo, "config", "user.name", "Test User")
        self.git(self.main_repo, "config", "user.email", "test@example.invalid")
        (self.main_repo / ".gitignore").write_text(".env\ndata/\n")
        (self.main_repo / "NOTES.md").write_text("base\n")
        self.git(self.main_repo, "add", ".gitignore", "NOTES.md")
        self.git(self.main_repo, "commit", "-m", "base")
        self.git(self.main_repo, "update-ref", "refs/remotes/origin/main", "HEAD")
        self.git(
            self.main_repo,
            "worktree",
            "add",
            "-b",
            "feature",
            str(self.worktree),
            "main",
        )
        self.now = time.time() + 10 * reaper.DAY

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    @staticmethod
    def git(cwd: Path, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ("git", "-C", str(cwd), *args),
            check=True,
            capture_output=True,
            text=True,
        )

    def context(self, *, status: str = "done", task_owned: bool = True) -> reaper.ScanContext:
        task = reaper.TaskFacts(
            task_id="task-1",
            status=status,
            kind="oneshot",
            branch="feature" if task_owned else None,
            repo_url=None,
            updated_at=self.now - 9 * reaper.DAY,
        )
        workspace = reaper.WorkspaceFacts(task_ids={"task-1"} if task_owned else set())
        return reaper.ScanContext(
            root=self.root,
            cm_home=self.base / "cm",
            config_path=self.base / "daemon.toml",
            manifest_paths=(),
            now=self.now,
            retention_days=7,
            unowned_retention_days=30,
            workspaces={self.worktree.resolve(): workspace},
            tasks={"task-1": task} if task_owned else {},
            tasks_by_branch={"feature": [task]} if task_owned else {},
            live_paths=set(),
            process_paths={},
            task_state_available=True,
            session_state_available=True,
            fetch=False,
        )

    def test_terminal_inactive_clean_worktree_is_eligible(self) -> None:
        result = reaper.decision(self.worktree, self.context())
        self.assertTrue(result.eligible)
        self.assertEqual(result.reason, "terminal_and_inactive")
        self.assertEqual(result.landed_reason, "head_is_ancestor")

    def test_nonterminal_and_young_unowned_worktrees_fail_closed(self) -> None:
        running = reaper.decision(self.worktree, self.context(status="running"))
        self.assertFalse(running.eligible)
        self.assertEqual(running.reason, "task_not_terminal")

        unowned = reaper.decision(self.worktree, self.context(task_owned=False))
        self.assertFalse(unowned.eligible)
        self.assertEqual(unowned.reason, "unowned_below_retention")

    def test_missing_task_row_falls_back_to_unowned_policy(self) -> None:
        context = self.context()
        context.tasks = {}
        context.tasks_by_branch = {}
        context.unowned_retention_days = 7

        result = reaper.decision(self.worktree, context)

        self.assertTrue(result.eligible)
        self.assertEqual(result.reason, "unowned_inactive")
        self.assertEqual(result.task_ids, ("task-1",))

    def test_branch_fallback_requires_same_repository(self) -> None:
        context = self.context(task_owned=False)
        unrelated = reaper.TaskFacts(
            task_id="other-task",
            status="done",
            kind="oneshot",
            branch="feature",
            repo_url="https://github.com/example/a-different-repo.git",
            updated_at=self.now - 9 * reaper.DAY,
        )
        context.tasks[unrelated.task_id] = unrelated
        context.tasks_by_branch["feature"] = [unrelated]

        result = reaper.decision(self.worktree, context)
        self.assertFalse(result.eligible)
        self.assertEqual(result.reason, "unowned_below_retention")

        origin = "https://github.com/example/repo.git"
        self.git(self.main_repo, "remote", "add", "origin", origin)
        context.tasks[unrelated.task_id] = reaper.dataclasses.replace(unrelated, repo_url=origin)
        context.tasks_by_branch["feature"] = [context.tasks[unrelated.task_id]]
        matched = reaper.decision(self.worktree, context)
        self.assertTrue(matched.eligible)

    def test_old_clean_unowned_worktree_preserves_branch_even_when_not_landed(
        self,
    ) -> None:
        context = self.context(task_owned=False)
        context.now += 25 * reaper.DAY
        landed = reaper.decision(self.worktree, context)
        self.assertTrue(landed.eligible)
        self.assertEqual(landed.reason, "unowned_inactive")
        self.assertEqual(landed.landed_reason, "head_is_ancestor")

        (self.worktree / "NOTES.md").write_text("unowned change\n")
        self.git(self.worktree, "add", "NOTES.md")
        self.git(self.worktree, "commit", "-m", "unlanded")
        context.now += reaper.DAY
        unlanded = reaper.decision(self.worktree, context)
        self.assertTrue(unlanded.eligible)
        self.assertEqual(unlanded.reason, "unowned_inactive")
        self.assertEqual(unlanded.landed_reason, "branch_changes_not_proven_in_main")

    def test_old_dirty_unowned_worktree_gets_preservation_commit(self) -> None:
        context = self.context(task_owned=False)
        context.now += 25 * reaper.DAY
        (self.worktree / "NOTES.md").write_text("unfinished unowned work\n")
        old = context.now - 31 * reaper.DAY
        reaper.os.utime(self.worktree / "NOTES.md", (old, old))

        result = reaper.decision(self.worktree, context)
        self.assertTrue(result.eligible)
        self.assertEqual(result.reason, "unowned_inactive_auto_commit")

    def test_unowned_fetch_failure_is_reported_but_branch_is_still_preserved(
        self,
    ) -> None:
        context = self.context(task_owned=False)
        context.now += 25 * reaper.DAY
        context.fetch = True

        result = reaper.decision(self.worktree, context)
        self.assertTrue(result.eligible)
        self.assertEqual(result.reason, "unowned_inactive")
        self.assertIsNotNone(result.landed_reason)
        assert result.landed_reason is not None
        self.assertIn("origin fetch failed", result.landed_reason)

    def test_external_ignored_symlink_is_safe_and_real_ignored_data_is_archived(
        self,
    ) -> None:
        canonical_env = self.main_repo / ".env"
        canonical_env.write_text("test-only\n")
        (self.worktree / ".env").symlink_to(canonical_env)
        safe = reaper.decision(self.worktree, self.context())
        self.assertTrue(safe.eligible)

        data = self.worktree / "data"
        data.mkdir()
        (data / "unique.txt").write_text("unique\n")
        planned = reaper.decision(self.worktree, self.context())
        self.assertTrue(planned.eligible)
        self.assertEqual(planned.archive_paths, ("data/",))
        self.assertGreater(planned.archive_bytes, 0)

    def test_empty_ignored_directory_is_discardable(self) -> None:
        (self.worktree / "data").mkdir()
        result = reaper.decision(self.worktree, self.context())
        self.assertTrue(result.eligible)
        self.assertEqual(result.archive_paths, ())
        self.assertEqual(result.discard_paths, ("data/",))

    def test_divergent_secret_like_ignored_file_is_archived_securely(self) -> None:
        (self.worktree / ".env").write_text("test-only\n")
        result = reaper.decision(self.worktree, self.context())
        self.assertTrue(result.eligible)
        self.assertEqual(result.archive_paths, (".env",))

    def test_canonical_secret_copy_is_discardable(self) -> None:
        (self.main_repo / ".env").write_text("same-test-value\n")
        (self.worktree / ".env").write_text("same-test-value\n")
        result = reaper.decision(self.worktree, self.context())
        self.assertTrue(result.eligible)
        self.assertEqual(result.archive_paths, ())
        self.assertEqual(result.discard_paths, (".env",))

    def test_public_env_template_is_safe_to_auto_commit(self) -> None:
        (self.worktree / ".env.template").write_text("NAME=\n")
        result = reaper.decision(self.worktree, self.context())
        self.assertTrue(result.eligible)
        self.assertEqual(result.reason, "terminal_inactive_auto_commit")

    def test_ignored_artifact_is_moved_to_vault_with_manifest(self) -> None:
        data = self.worktree / "data"
        data.mkdir()
        (data / "unique.txt").write_text("unique\n")
        candidate = reaper.decision(self.worktree, self.context())
        vault = self.base / "artifacts"

        ok, archived, error = reaper.archive_ignored_paths(candidate, vault)

        self.assertTrue(ok, error)
        self.assertIsNotNone(archived)
        assert archived is not None
        self.assertFalse(data.exists())
        self.assertEqual((archived / "files" / "data" / "unique.txt").read_text(), "unique\n")
        manifest = reaper.json.loads((archived / "manifest.json").read_text())
        self.assertEqual(manifest["source_worktree"], str(self.worktree))
        self.assertEqual(manifest["paths"], ["data/"])
        self.assertEqual(len(manifest["inventory"]), 1)

    def test_artifact_gc_plans_old_unpinned_archives_only(self) -> None:
        root = self.base / "artifacts"
        old = root / "repo" / "tree" / "old"
        old.mkdir(parents=True)
        (old / "payload").write_text("old\n")
        (old / "manifest.json").write_text(reaper.json.dumps({"archived_at": "2026-01-01T00:00:00+00:00"}))
        pinned = root / "repo" / "tree" / "pinned"
        pinned.mkdir()
        (pinned / "manifest.json").write_text(reaper.json.dumps({"archived_at": "2026-01-01T00:00:00+00:00"}))
        (pinned / ".keep").write_text("")
        recent = root / "repo" / "tree" / "recent"
        recent.mkdir()
        (recent / "manifest.json").write_text(reaper.json.dumps({"archived_at": "2026-08-25T00:00:00+00:00"}))
        now = reaper.dt.datetime(2026, 9, 1, tzinfo=reaper.dt.timezone.utc).timestamp()

        candidates = reaper.artifact_gc_candidates(root, now, 30)

        self.assertEqual([candidate.path for candidate in candidates], [old.resolve()])
        self.assertGreater(candidates[0].size_bytes, 0)

    def test_task_status_timestamp_and_root_mtime_do_not_reset_activity(self) -> None:
        context = self.context()
        context.tasks["task-1"] = reaper.dataclasses.replace(context.tasks["task-1"], updated_at=context.now)
        context.tasks_by_branch["feature"] = [context.tasks["task-1"]]
        reaper.os.utime(self.worktree, (context.now, context.now))

        result = reaper.decision(self.worktree, context)

        self.assertTrue(result.eligible)

    def test_native_metadata_links_child_to_parent_and_carries_activity(self) -> None:
        projects = self.base / "claude" / "projects"
        parent = self.base / "cm" / "worktrees" / "parent"
        metadata_dir = projects / reaper.encode_claude_project_path(parent) / "session-1" / "subagents"
        metadata_dir.mkdir(parents=True)
        metadata = metadata_dir / "agent-a.meta.json"
        metadata.write_text(
            reaper.json.dumps(
                {
                    "spawnedWithWorktree": True,
                    "worktreePath": str(self.worktree),
                }
            )
        )
        transcript = metadata_dir / "agent-a.jsonl"
        transcript.write_text("{}\n")

        links, roots, warnings = reaper.load_native_parent_links(projects, [parent])

        self.assertEqual(warnings, [])
        self.assertEqual(roots, {self.worktree.parent.resolve()})
        self.assertEqual(links[self.worktree.resolve()].parent_worktree, parent.resolve())
        self.assertEqual(len(links[self.worktree.resolve()].activity_at), 2)

    def test_standalone_repo_under_managed_root_gets_verified_bundle_plan(self) -> None:
        standalone = self.root / "standalone"
        self.git(self.base, "clone", str(self.main_repo), str(standalone))
        context = self.context(task_owned=False)
        context.now += 25 * reaper.DAY
        context.workspaces = {standalone.resolve(): reaper.WorkspaceFacts()}

        candidate = reaper.decision(standalone, context)

        self.assertTrue(candidate.eligible)
        self.assertTrue(candidate.standalone_repo)
        vault = self.base / "artifacts"
        ok, bundle, error = reaper.archive_standalone_bundle(candidate, vault)
        self.assertTrue(ok, error)
        self.assertIsNotNone(bundle)
        verify = self.git(standalone, "bundle", "verify", str(bundle))
        self.assertEqual(verify.returncode, 0)

    def test_secret_like_untracked_file_blocks_auto_commit(self) -> None:
        (self.worktree / "private.pem").write_text("not-a-real-key\n")
        result = reaper.decision(self.worktree, self.context())
        self.assertFalse(result.eligible)
        self.assertEqual(result.reason, "unsafe_untracked")

    def test_dirty_tracked_change_gets_preservation_commit(self) -> None:
        (self.worktree / "NOTES.md").write_text("unfinished work\n")
        result = reaper.decision(self.worktree, self.context())
        self.assertTrue(result.eligible)
        self.assertEqual(result.reason, "terminal_inactive_auto_commit")

        ok, branch, error = reaper.preserve_dirty_worktree(self.worktree, ("task-1",))
        self.assertTrue(ok, error)
        self.assertEqual(branch, "feature")
        self.assertEqual(self.git(self.worktree, "status", "--porcelain").stdout, "")
        self.assertEqual(
            self.git(self.worktree, "log", "-1", "--format=%s").stdout.strip(),
            "chore: preserve stale worktree before reaping",
        )

    def test_recent_dirty_file_activity_resets_retention_clock(self) -> None:
        self.now = time.time()
        context = self.context()
        context.now = self.now
        context.tasks["task-1"] = reaper.dataclasses.replace(
            context.tasks["task-1"],
            updated_at=self.now - 9 * reaper.DAY,
        )
        context.tasks_by_branch["feature"] = [context.tasks["task-1"]]
        (self.worktree / "NOTES.md").write_text("touched now\n")

        result = reaper.decision(self.worktree, context)
        self.assertFalse(result.eligible)
        self.assertEqual(result.reason, "below_retention")
        self.assertIsNotNone(result.age_days)
        assert result.age_days is not None
        self.assertLess(result.age_days, 1 / 24)

    def test_detached_head_gets_rescue_branch(self) -> None:
        self.git(self.worktree, "switch", "--detach")
        ok, branch, error = reaper.preserve_dirty_worktree(self.worktree, ("task-1",))
        self.assertTrue(ok, error)
        self.assertIsNotNone(branch)
        assert branch is not None
        self.assertTrue(branch.startswith("cm-reaper/rescue-"))
        self.git(self.worktree, "show-ref", "--verify", f"refs/heads/{branch}")

    def test_worktree_enumerator_ignores_plain_cache_directories(self) -> None:
        (self.root / ".pytest_cache").mkdir()
        self.assertEqual(reaper.worktree_paths(self.root), [self.worktree])


if __name__ == "__main__":
    unittest.main()
