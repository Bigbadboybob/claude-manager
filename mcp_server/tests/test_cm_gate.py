"""scripts/cm-gate against a scratch git repo (doc/gate-receipts.md §9).

Runs the real runner with --dry-run and a local profile, then judges the
receipt it would post with the real evaluator. Needs pytest in this
interpreter (the runner runs the scratch repo's tests with it).
"""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from dispatch import gate_rules as g

ROOT = Path(__file__).parents[2]
CM_GATE = ROOT / "scripts" / "cm-gate"
HAVE_PYTEST = importlib.util.find_spec("pytest") is not None
GIT_ENV = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
           "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"}


def sh(cwd, *argv):
    return subprocess.run(argv, cwd=cwd, env=GIT_ENV, check=True, capture_output=True,
                          text=True).stdout.strip()


@unittest.skipUnless(HAVE_PYTEST, "needs pytest in this interpreter")
class CmGateRun(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self.tmp.name) / "repo"
        self.repo.mkdir()
        self.logs = Path(self.tmp.name) / "logs"
        self.harvest = Path(self.tmp.name) / "harvest"
        self.harvest.mkdir()
        (self.harvest / "nodes.json").write_text("[1, 2]")
        r = self.repo
        sh(r, "git", "init", "-q", "-b", "main")
        (r / "pkg.py").write_text("def f():\n    return 1\n")
        (r / "tests").mkdir()
        (r / "tests" / "test_pkg.py").write_text(
            "import pytest\nimport pkg\n\n"
            "def test_f():\n    assert pkg.f() == 1\n\n"
            "def test_g():\n    assert pkg.f() + 1 == 2\n\n"
            "@pytest.mark.skip(reason='requires CUDA')\ndef test_cuda():\n    pass\n")
        (r / "tests" / "test_new.py").write_text(
            "import pkg\n\ndef test_new():\n    assert pkg.f() == 2\n")
        (r / "conftest.py").write_text("")
        sh(r, "git", "add", ".")
        sh(r, "git", "commit", "-qm", "mf: new test fails")
        self.mf = sh(r, "git", "rev-parse", "HEAD")
        sh(r, "git", "checkout", "-qb", "lane")
        (r / "pkg.py").write_text("def f():\n    return 2\n")
        (r / "tests" / "test_pkg.py").write_text(
            (r / "tests" / "test_pkg.py").read_text().replace("== 1", "== 2").replace("+ 1 == 2", "+ 1 == 3"))
        sh(r, "git", "commit", "-qam", "fix")
        self.head = sh(r, "git", "rev-parse", "HEAD")
        self.profile = {
            "steps": [
                {"name": "must_fail", "kind": "pytest", "at": "mf",
                 "argv": [sys.executable, "-m", "pytest", "tests/test_new.py"],
                 "nodes": ["tests/test_new.py::test_new"]},
                {"name": "pytest", "kind": "pytest",
                 "argv": [sys.executable, "-m", "pytest", "tests"],
                 "skip_policy": {"allow_reasons": ["^requires CUDA$"]}},
                {"name": "fetch", "kind": "rc", "argv": ["true"], "void_on_fail": True},
            ],
            "base": {"ref": "main", "scope": "paths"},
            "inputs": [{"name": "harvest", "kind": "dir", "root_env": "HARVEST"}],
            "allowed_env": ["HARVEST"],
        }

    def tearDown(self):
        self.tmp.cleanup()

    def run_gate(self, profile=None, *extra, env=None):
        pf = Path(self.tmp.name) / "profile.json"
        pf.write_text(json.dumps(profile or self.profile))
        proc = subprocess.run(
            [sys.executable, str(CM_GATE), "run", "--item", "1", "--profile-file", str(pf),
             "--no-fetch", "--dry-run", "--mf", self.mf, *extra],
            cwd=self.repo, capture_output=True, text=True,
            env={**os.environ, "CM_GATE_LOG_DIR": str(self.logs), "HARVEST": str(self.harvest),
                 **(env or {})})
        self.assertEqual(proc.returncode, 0, proc.stderr)
        receipt = g.normalize_receipt(json.loads(proc.stdout))
        prof = g.validate_profile(profile or self.profile)
        return receipt, g.evaluate(receipt, prof, [])

    def texts(self, v):
        return " | ".join(r["text"] for r in v["reasons"])

    def test_a_clean_run_passes_with_real_node_evidence(self):
        receipt, v = self.run_gate()
        self.assertEqual(v["verdict"], "PASS", self.texts(v))
        self.assertEqual(receipt["head_sha"], self.head)
        mf = next(s for s in receipt["steps"] if s["name"] == "must_fail")
        self.assertEqual(mf["at_sha"], self.mf)
        self.assertEqual(mf["nodes"], [{"id": "tests/test_new.py::test_new", "outcome": "failed",
                                        "exc_type": "AssertionError"}])
        pt = next(s for s in receipt["steps"] if s["name"] == "pytest")
        self.assertEqual(pt["skips"], [{"node": "tests/test_pkg.py::test_cuda",
                                        "reason": "requires CUDA"}])
        self.assertEqual(len(pt["granted_nodes"]), 4)
        self.assertEqual(receipt["inputs"][0]["verification"], "hashed_at_run")
        self.assertTrue(receipt["inputs"][0]["content_id"].startswith("tree:"))
        self.assertFalse(receipt["worktree"]["dirty_before"])
        self.assertIn("pkg.py", receipt["closure"]["paths"])
        self.assertEqual(receipt["branch"], "lane")
        self.assertEqual(receipt["base"]["sha_before"], self.mf)
        self.assertNotIn("mf", sh(self.repo, "git", "worktree", "list"), "the MF checkout is removed")

    def test_a_failing_test_is_red(self):
        (self.repo / "pkg.py").write_text("def f():\n    return 3\n")
        sh(self.repo, "git", "commit", "-qam", "break")
        _, v = self.run_gate()
        self.assertEqual(v["verdict"], "FAIL", self.texts(v))

    def test_a_missing_input_is_void(self):
        _, v = self.run_gate(env={"HARVEST": str(Path(self.tmp.name) / "nope")})
        self.assertEqual(v["verdict"], "VOID")
        self.assertIn("input harvest missing", self.texts(v))

    def test_a_failing_void_on_fail_step_is_void(self):
        prof = json.loads(json.dumps(self.profile))
        prof["steps"][2]["argv"] = ["false"]
        receipt, v = self.run_gate(prof)
        self.assertEqual(v["verdict"], "VOID", self.texts(v))
        self.assertEqual(receipt["steps"][2]["reason"], "rc 1 (void_on_fail)")

    def test_an_override_that_narrows_the_selection_is_weakened(self):
        ov = Path(self.tmp.name) / "lane.json"
        ov.write_text(json.dumps({"steps": {"pytest": {"extra_args": ["-k", "not test_g"]}}}))
        _, v = self.run_gate(None, "--override", str(ov))
        self.assertEqual(v["verdict"], "FAIL")
        self.assertIn("filter -k not test_g narrows the granted selection", self.texts(v))
        self.assertIn("granted node(s) not run", self.texts(v))

    def test_an_allowed_override_needs_no_review(self):
        ov = Path(self.tmp.name) / "lane.json"
        ov.write_text(json.dumps({"steps": {"pytest": {"prefix": ["timeout", "600"],
                                                       "extra_args": ["-q"]}}}))
        _, v = self.run_gate(None, "--override", str(ov))
        self.assertEqual(v["verdict"], "PASS", self.texts(v))

    def test_a_dirty_tree_is_incomplete(self):
        (self.repo / "pkg.py").write_text("def f():\n    return 2  # edited\n")
        _, v = self.run_gate()
        self.assertEqual(v["verdict"], "INCOMPLETE")
        self.assertIn("worktree was dirty", self.texts(v))


if __name__ == "__main__":
    unittest.main()
