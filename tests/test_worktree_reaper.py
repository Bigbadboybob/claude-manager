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
        self.git(self.main_repo, "worktree", "add", "-b", "feature", str(self.worktree), "main")
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

    def test_old_clean_unowned_worktree_must_be_landed(self) -> None:
        context = self.context(task_owned=False)
        context.now += 25 * reaper.DAY
        landed = reaper.decision(self.worktree, context)
        self.assertTrue(landed.eligible)
        self.assertEqual(landed.reason, "unowned_landed_and_inactive")
        self.assertEqual(landed.landed_reason, "head_is_ancestor")

        (self.worktree / "NOTES.md").write_text("unowned change\n")
        self.git(self.worktree, "add", "NOTES.md")
        self.git(self.worktree, "commit", "-m", "unlanded")
        context.now += reaper.DAY
        unlanded = reaper.decision(self.worktree, context)
        self.assertFalse(unlanded.eligible)
        self.assertEqual(unlanded.reason, "unowned_not_landed")

    def test_old_dirty_unowned_worktree_is_never_auto_committed(self) -> None:
        context = self.context(task_owned=False)
        context.now += 25 * reaper.DAY
        (self.worktree / "NOTES.md").write_text("unfinished unowned work\n")
        old = context.now - 31 * reaper.DAY
        reaper.os.utime(self.worktree / "NOTES.md", (old, old))

        result = reaper.decision(self.worktree, context)
        self.assertFalse(result.eligible)
        self.assertEqual(result.reason, "unowned_dirty")

    def test_unowned_landed_gate_fails_closed_when_origin_fetch_fails(self) -> None:
        context = self.context(task_owned=False)
        context.now += 25 * reaper.DAY
        context.fetch = True

        result = reaper.decision(self.worktree, context)
        self.assertFalse(result.eligible)
        self.assertEqual(result.reason, "unowned_not_landed")
        self.assertIn("origin fetch failed", result.landed_reason)

    def test_canonical_ignored_symlink_is_safe_but_real_ignored_data_is_not(self) -> None:
        canonical_env = self.main_repo / ".env"
        canonical_env.write_text("test-only\n")
        (self.worktree / ".env").symlink_to(canonical_env)
        safe = reaper.decision(self.worktree, self.context())
        self.assertTrue(safe.eligible)

        data = self.worktree / "data"
        data.mkdir()
        (data / "unique.txt").write_text("unique\n")
        blocked = reaper.decision(self.worktree, self.context())
        self.assertFalse(blocked.eligible)
        self.assertEqual(blocked.reason, "meaningful_ignored")
        self.assertIn("data/", blocked.details)

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
        self.assertLess(result.age_days, 1 / 24)

    def test_detached_head_gets_rescue_branch(self) -> None:
        self.git(self.worktree, "switch", "--detach")
        ok, branch, error = reaper.preserve_dirty_worktree(self.worktree, ("task-1",))
        self.assertTrue(ok, error)
        self.assertIsNotNone(branch)
        self.assertTrue(branch.startswith("cm-reaper/rescue-"))
        self.git(self.worktree, "show-ref", "--verify", f"refs/heads/{branch}")

    def test_worktree_enumerator_ignores_plain_cache_directories(self) -> None:
        (self.root / ".pytest_cache").mkdir()
        self.assertEqual(reaper.worktree_paths(self.root), [self.worktree])


if __name__ == "__main__":
    unittest.main()
