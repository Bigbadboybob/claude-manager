"""backtest_startup.sh::build_artifact_json — the failure-evidence merge.

Runs the REAL bash function (extracted from the worker script and executed
in a subprocess) rather than a reimplementation: this exact body builder
already burned a month once by silently emitting empty bodies (the 422
incident documented in the script), so the guard must exercise the shipped
text. Covered here: the CM_FAIL_EXIT_CODE merge (exit_code + log_tail into
the summary, so a failure's reason reaches get_backtest_result instead of
dying with the reaped VM) and that the success shape is untouched.
"""

from __future__ import annotations

import json
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

_SCRIPT = Path(__file__).resolve().parents[2] / "worker" / "backtest_startup.sh"


def _extract_function(name: str) -> str:
    """Pull one top-level `name() { ... }` block out of the worker script."""
    text = _SCRIPT.read_text()
    m = re.search(rf"^{re.escape(name)}\(\) \{{\n.*?^\}}$", text,
                  re.MULTILINE | re.DOTALL)
    if not m:
        raise AssertionError(f"{name}() not found in {_SCRIPT}")
    return m.group(0)


class BuildArtifactJsonTests(unittest.TestCase):
    def _run(self, *, partial: str, summary: dict | None, env: dict) -> dict:
        with tempfile.TemporaryDirectory() as td:
            results = Path(td) / "results"
            results.mkdir()
            if summary is not None:
                (results / "backtest_summary.json").write_text(json.dumps(summary))
            script = (
                _extract_function("build_artifact_json")
                + f'\nRESULTS_DIR={results}\nGCS_PREFIX="gs://b/run"\nRUN_KEY="rk-1"\n'
                + f"build_artifact_json {partial}\n"
            )
            proc = subprocess.run(
                ["bash", "-c", script], capture_output=True, text=True,
                env={"PATH": "/usr/bin:/bin", **env}, timeout=30,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertTrue(proc.stdout.strip(), "empty artifact body")
            return json.loads(proc.stdout)

    def test_success_shape_has_no_failure_fields(self):
        body = self._run(partial="false", summary={"total_pnl": 1.5}, env={})
        self.assertEqual(body["kind"], "backtest-result")
        self.assertFalse(body["partial"])
        self.assertNotIn("exit_code", body["summary"])
        self.assertNotIn("log_tail", body["summary"])
        self.assertEqual(body["summary"]["run_key"], "rk-1")

    def test_failure_merges_exit_code_and_log_tail(self):
        with tempfile.NamedTemporaryFile("w", suffix=".log", delete=False) as f:
            f.write("line-1\n" + "x" * 5000 + "\nFATAL: download_events died\n")
            log_path = f.name
        body = self._run(
            partial="true", summary=None,
            env={"CM_FAIL_EXIT_CODE": "11", "CM_PIPELINE_LOG": log_path},
        )
        s = body["summary"]
        self.assertTrue(body["partial"])
        self.assertEqual(s["exit_code"], 11)
        # No summary file -> the builder's no-summary stub keeps its own
        # error; the merge must not clobber an existing one (setdefault).
        self.assertEqual(s["error"], "no-summary")
        self.assertIn("FATAL: download_events died", s["log_tail"])
        self.assertLessEqual(len(s["log_tail"].encode()), 4000)

    def test_failure_with_real_partial_summary_keeps_metrics(self):
        with tempfile.NamedTemporaryFile("w", suffix=".log", delete=False) as f:
            f.write("boom\n")
            log_path = f.name
        body = self._run(
            partial="true", summary={"total_pnl": 2.0},
            env={"CM_FAIL_EXIT_CODE": "12", "CM_PIPELINE_LOG": log_path},
        )
        s = body["summary"]
        self.assertEqual(s["total_pnl"], 2.0)
        self.assertEqual(s["error"], "pipeline-failed")
        self.assertEqual(s["exit_code"], 12)
        self.assertEqual(s["log_tail"], "boom\n")

    def test_missing_log_file_is_not_fatal(self):
        body = self._run(
            partial="true", summary=None,
            env={"CM_FAIL_EXIT_CODE": "10", "CM_PIPELINE_LOG": "/nonexistent/x.log"},
        )
        self.assertEqual(body["summary"]["exit_code"], 10)
        self.assertNotIn("log_tail", body["summary"])


if __name__ == "__main__":
    unittest.main()
