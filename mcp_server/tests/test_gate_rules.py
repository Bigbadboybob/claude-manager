"""Gate receipt verdicts (doc/gate-receipts.md §3-§8, §12), without Postgres.

One test per case EP checked the design against (§12), plus the parser,
grant and staleness rules they rest on.
"""

from __future__ import annotations

import copy
import unittest

from dispatch import gate_rules as g

HEAD = "a" * 40
HEAD2 = "b" * 40
MF = "c" * 40
MF2 = "d" * 40
BASE = "e" * 40
BASE2 = "f" * 40
TREE = "1" * 40
RUNNER = "2" * 40
EP = "agent:d1:ep-uid"

PROFILE_RAW = {
    "steps": [
        {"name": "must_fail", "kind": "pytest", "at": "mf",
         "argv": ["pytest", "tests/sejd/test_fuse.py::test_new_path"],
         "nodes": ["tests/sejd/test_fuse.py::test_new_path"],
         "expect_fail": {"default": "AssertionError",
                         "tests/sejd/test_fuse.py::test_new_path": ["AssertionError",
                                                                    "AttributeError"]}},
        {"name": "pytest", "kind": "pytest", "argv": ["pytest", "-n", "2", "tests/sejd", "tests/a320"],
         "min_passed": 1, "must_run": ["tests/a320/test_nodes.py"],
         "skip_policy": {"allow_reasons": ["^requires CUDA$"]}},
        {"name": "mypy", "kind": "mypy", "argv": ["mypy", "prediction"]},
        {"name": "ruff", "kind": "ruff", "argv": ["ruff", "check", "."]},
        {"name": "fetch", "kind": "rc", "void_on_fail": True},
    ],
    "waivable": ["mypy", "pytest"],
    "require": {"runner": True},
    "base": {"ref": "origin/main", "scope": "paths"},
    "inputs": [{"name": "harvest_a320", "kind": "dir", "root_env": "A320_HARVEST"},
               {"name": "ticks", "kind": "db"}],
    "allowed_env": ["JAX_PLATFORMS", "A320_HARVEST"],
}
PROFILE = g.validate_profile(PROFILE_RAW)

PYTEST_NODES = [
    {"id": "tests/sejd/test_a.py::test_one", "outcome": "passed"},
    {"id": "tests/sejd/test_a.py::test_two", "outcome": "passed"},
    {"id": "tests/a320/test_nodes.py::test_node", "outcome": "passed"},
    {"id": "tests/sejd/test_gpu.py::test_cuda", "outcome": "skipped"},
]
GRANTED = [n["id"] for n in PYTEST_NODES]


def receipt(**over) -> dict:
    raw = {
        "gate": "gate", "head": HEAD, "mf": MF, "attestation": "cm-gate",
        "runner": {"ref": "origin/main:scripts/gate.sh", "sha": RUNNER},
        "base": {"ref": "origin/main", "sha_before": BASE, "sha_after": BASE,
                 "merge_tree_before": TREE, "merge_tree_after": TREE},
        "closure": {"paths": ["prediction/sejd/fuse.py", "tests/sejd/test_a.py"]},
        "worktree": {"head_before": HEAD, "head_after": HEAD, "dirty_before": False,
                     "dirty_after": False, "path": "/w/lane"},
        "inputs": [
            {"name": "harvest_a320", "kind": "dir", "locator": "$A320_HARVEST",
             "resolved_root": "/data/harvest", "content_id": "tree:" + "9" * 64, "bytes": 10, "hashed_by": "cm-gate"},
            {"name": "ticks", "kind": "db", "locator": "SELECT * FROM ticks WHERE t BETWEEN 1 AND 2",
             "content_id": "snapshot:ticks-2026-10-20"},
        ],
        "steps": [
            {"name": "must_fail", "state": "ran", "rc": 1, "at_sha": MF,
             "effective_cmd": {"argv": ["pytest", "tests/sejd/test_fuse.py::test_new_path"]},
             "summary": "=== 1 failed in 0.50s ===",
             "nodes": [{"id": "tests/sejd/test_fuse.py::test_new_path", "outcome": "failed",
                        "exc_type": "AssertionError"}],
             "granted_nodes": ["tests/sejd/test_fuse.py::test_new_path"],
             "skips": [], "deselected": []},
            {"name": "pytest", "state": "ran", "rc": 0, "at_sha": HEAD,
             "effective_cmd": {"argv": ["pytest", "-n", "2", "tests/sejd", "tests/a320"]},
             "summary": "==== 3 passed, 1 skipped in 12.30s ====",
             "nodes": copy.deepcopy(PYTEST_NODES), "granted_nodes": list(GRANTED),
             "skips": [{"node": "tests/sejd/test_gpu.py::test_cuda", "reason": "requires CUDA"}],
             "deselected": []},
            {"name": "mypy", "state": "ran", "rc": 0, "effective_cmd": {"argv": ["mypy", "prediction"]},
             "summary": "Success: no issues found in 412 source files"},
            {"name": "ruff", "state": "ran", "rc": 0, "effective_cmd": {"argv": ["ruff", "check", "."]},
             "summary": "All checks passed!"},
            {"name": "fetch", "state": "ran", "rc": 0},
        ],
    }
    raw.update(over)
    return g.normalize_receipt(raw)


def step(r: dict, name: str) -> dict:
    return next(s for s in r["steps"] if s["name"] == name)


def grant(**over) -> dict:
    gr = {"label": "g-sejd-14-1", "kind": "waiver", "gate": "gate", "grantor_pid": EP,
          "head_sha": HEAD, "base_sha": None, "step": "mypy", "nodes": None, "cmd_digest": None,
          "reason": "mypy stubs broken upstream", "risk": "type errors may slip through",
          "message_id": "m-1",
          "message_check": {"found": True, "sender": EP, "contains_head": True}}
    gr.update(over)
    gr["problems"] = g.grant_problems(gr, PROFILE, {"owner", EP})
    return gr


def verdict(r, grants=()) -> dict:
    return g.evaluate(r, PROFILE, list(grants))


def texts(v) -> str:
    return " | ".join(x["text"] for x in v["reasons"])


class Baseline(unittest.TestCase):
    def test_a_complete_run_passes(self):
        v = verdict(receipt())
        self.assertEqual(v["verdict"], "PASS", texts(v))

    def test_no_profile_never_passes(self):
        v = g.evaluate(receipt(), None, [], klass="py-lane")
        self.assertEqual(v["verdict"], "INCOMPLETE")
        self.assertIn("no gate profile for class py-lane", texts(v))

    def test_a_missing_required_step_is_incomplete(self):
        r = receipt()
        r["steps"] = [s for s in r["steps"] if s["name"] != "ruff"]
        v = verdict(r)
        self.assertEqual(v["verdict"], "INCOMPLETE")
        self.assertIn("ruff: missing", texts(v))

    def test_skipped_and_not_run_steps_are_kept_and_incomplete(self):
        r = receipt()
        step(r, "mypy")["state"] = "not_run"
        v = verdict(r)
        self.assertEqual(v["verdict"], "INCOMPLETE")
        self.assertIn("mypy: not run", texts(v))


class EPCases(unittest.TestCase):
    """doc/gate-receipts.md §12, one test per row."""

    def test_stale_mf_sha(self):
        r = receipt()
        self.assertEqual(g.arrival_stale(r, {"mf": MF2})["reason"], "mf_moved")
        newer = receipt(mf=MF2)
        stale = g.superseded_by(r, newer)
        self.assertEqual(stale, {"reason": "mf_moved", "prev": MF[:7], "new": MF2[:7]})
        self.assertEqual(g.headline(verdict(r), stale), f"STALE (was PASS): mf_moved {MF[:7]} → {MF2[:7]}")
        self.assertEqual(g.binding_stale(r, {"mf": MF2}, {"mf"})["reason"], "mf_moved")
        self.assertIsNone(g.binding_stale(r, {"mf": MF}, {"mf"}))

    def test_red_and_void_reruns_keep_their_own_verdicts(self):
        red = receipt()
        node = step(red, "pytest")["nodes"][0]
        node["outcome"], node["exc_type"] = "failed", "AssertionError"
        step(red, "pytest")["rc"] = 1
        step(red, "pytest")["summary"] = "== 1 failed, 2 passed, 1 skipped in 3.00s =="
        v = verdict(red)
        self.assertEqual(v["verdict"], "FAIL", texts(v))
        void = receipt()
        step(void, "fetch").update(state="void", rc=None, reason="FETCH_FAIL")
        v = verdict(void)
        self.assertEqual(v["verdict"], "VOID", texts(v))
        self.assertNotIn("fail", {x["level"] for x in v["reasons"]}, "VOID is never RED")

    def test_fetch_fail_after_a_pytest_pass_is_void(self):
        r = receipt()
        step(r, "fetch").update(state="void", rc=None, reason="FETCH_FAIL")
        v = verdict(r)
        self.assertEqual(v["verdict"], "VOID")
        self.assertIn("fetch: void (FETCH_FAIL)", texts(v))
        self.assertEqual(step(r, "pytest")["state"], "ran", "the pytest pass is kept, not promoted")

    def test_missing_harvest_skips_never_pass(self):
        r = receipt()
        r["inputs"][0].update(state="missing", content_id=None)
        st = step(r, "pytest")
        st["nodes"][2]["outcome"] = "skipped"
        st["skips"].append({"node": "tests/a320/test_nodes.py::test_node", "reason": "harvest missing"})
        st["summary"] = "==== 2 passed, 2 skipped in 1.00s ===="
        v = verdict(r)
        self.assertEqual(v["verdict"], "VOID", texts(v))
        t = texts(v)
        self.assertIn("input harvest_a320 missing", t)
        self.assertIn('must_run tests/a320/test_nodes.py: 1 node(s) skipped "harvest missing"', t)
        self.assertIn('skip not allowed: "harvest missing"', t)
        # With the input present the skip still blocks PASS.
        r["inputs"][0].update(state="bound", content_id="tree:" + "9" * 64)
        self.assertEqual(verdict(r)["verdict"], "INCOMPLETE")

    def test_mf_import_error_or_missing_data_fail_does_not_count(self):
        r = receipt()
        mf = step(r, "must_fail")
        mf["nodes"][0].update(outcome="error", exc_type="ImportError")
        self.assertEqual(verdict(r)["verdict"], "FAIL")
        mf["nodes"][0].update(outcome="failed", exc_type="Failed")  # pytest.fail("no data")
        v = verdict(r)
        self.assertEqual(v["verdict"], "FAIL")
        self.assertIn("failed with Failed, expected AssertionError or AttributeError", texts(v))
        mf["nodes"][0]["exc_type"] = "AttributeError"  # the declared alternative
        self.assertEqual(verdict(r)["verdict"], "PASS")

    def test_lane_override_that_drops_nodes_or_adds_k_is_weakened(self):
        r = receipt()
        st = step(r, "pytest")
        st["effective_cmd"]["argv"] = ["pytest", "-n", "2", "-k", "not slow", "tests/sejd", "tests/a320"]
        v = verdict(r)
        self.assertEqual(v["verdict"], "FAIL")
        self.assertIn("filter -k not slow narrows the granted selection", texts(v))
        # A variance grant for exactly this head and command clears it.
        var = grant(kind="variance", step="pytest", cmd_digest=g.cmd_digest(st["effective_cmd"]))
        self.assertEqual(verdict(r, [var])["verdict"], "PASS", texts(verdict(r, [var])))
        # ... but not one bound to another command.
        other = grant(kind="variance", step="pytest", cmd_digest="sha256:" + "0" * 64)
        self.assertEqual(verdict(r, [other])["verdict"], "FAIL")

    def test_shrunk_node_set_is_red_until_acked(self):
        r = receipt()
        st = step(r, "pytest")
        st["nodes"] = [n for n in st["nodes"] if "test_two" not in n["id"]]
        st["summary"] = "==== 2 passed, 1 skipped in 9.00s ===="
        v = verdict(r)
        self.assertEqual(v["verdict"], "FAIL")
        self.assertIn("1 granted node(s) not run: tests/sejd/test_a.py::test_two", texts(v))
        # Deselecting a granted node is shrinkage too.
        r2 = receipt()
        st2 = step(r2, "pytest")
        st2["nodes"][1]["outcome"] = "deselected"
        st2["deselected"] = [{"node": "tests/sejd/test_a.py::test_two", "reason": "no data"}]
        st2["summary"] = "==== 2 passed, 1 skipped, 1 deselected in 9.00s ===="
        self.assertEqual(verdict(r2)["verdict"], "FAIL")
        # Runner attestation never substitutes for the grantor's ack.
        self.assertEqual(r["attestation"], "cm-gate")

    def test_allowed_execution_settings_need_no_review(self):
        r = receipt()
        st = step(r, "pytest")
        st["effective_cmd"] = {"argv": ["systemd-run", "--user", "--scope", "-p", "MemoryMax=8G",
                                        "timeout", "3600", "taskset", "-c", "4,5",
                                        "pytest", "-n", "1", "--timeout=600", "-q",
                                        "tests/sejd", "tests/a320", "tests/sejd/test_extra.py"],
                               "env": {"JAX_PLATFORMS": "cpu"}}
        v = verdict(r)
        self.assertEqual(v["verdict"], "PASS", texts(v))
        st["effective_cmd"]["argv"][st["effective_cmd"]["argv"].index("1")] = "4"
        self.assertIn("workers 4", texts(verdict(r)))
        st["effective_cmd"]["argv"][st["effective_cmd"]["argv"].index("4")] = "1"
        st["effective_cmd"]["env"]["PYTEST_ADDOPTS"] = "-k fast"
        self.assertIn("env PYTEST_ADDOPTS is not an allowed variable", texts(verdict(r)))

    def test_wrong_sha_or_generic_waiver_never_passes(self):
        r = receipt()
        step(r, "mypy").update(rc=1, summary="Found 3 errors in 2 files (checked 412 source files)")
        r["waivers"] = ["g-sejd-14-1"]
        self.assertEqual(verdict(r)["verdict"], "FAIL")
        wrong = grant(head_sha=HEAD2)
        v = verdict(r, [wrong])
        self.assertEqual(v["verdict"], "FAIL")
        self.assertIn(f"waiver g-sejd-14-1: unverified (head {HEAD[:7]} ≠ grant {HEAD2[:7]})", texts(v))
        generic = grant(step="the gate")
        self.assertIn("step the gate is not in the profile", generic["problems"])
        self.assertEqual(verdict(r, [generic])["verdict"], "FAIL")
        forged = grant(message_check={"found": True, "sender": "agent:d1:lane-uid", "contains_head": True})
        self.assertIn("message was not sent by the grantor", forged["problems"])
        self.assertEqual(verdict(r, [forged])["verdict"], "FAIL")
        no_sha = grant(message_check={"found": True, "sender": EP, "contains_head": False})
        self.assertEqual(verdict(r, [no_sha])["verdict"], "FAIL")
        uncited = receipt()
        step(uncited, "mypy").update(rc=1, summary="Found 3 errors in 2 files")
        self.assertEqual(verdict(uncited, [grant()])["verdict"], "FAIL", "the runner must cite it")
        good = verdict(r, [grant()])
        self.assertEqual(good["verdict"], "PASS", texts(good))
        self.assertIn("mypy: waived by g-sejd-14-1", texts(good))

    def test_unbound_db_window_is_incomplete(self):
        r = receipt()
        r["inputs"][1].update(content_id=None, state="unbound")
        v = verdict(r)
        self.assertEqual(v["verdict"], "INCOMPLETE")
        self.assertIn("input ticks unbound (a query/window is not an immutable identity", texts(v))
        raw = receipt()
        raw["inputs"][1]["content_id"] = "SELECT * FROM ticks"
        self.assertIn("content id must start with snapshot: or rows:", texts(verdict(raw)))

    def test_ruling_changed_before_the_gate_started(self):
        r = receipt(ruling="EP-A1021")
        self.assertEqual(g.arrival_stale(r, {"ruling_id": "EP-A1022"}),
                         {"reason": "ruling_superseded", "prev": "EP-A1021", "new": "EP-A1022"})
        self.assertIsNone(g.arrival_stale(r, {"ruling_id": "EP-A1021"}))

    def test_unrelated_landing_keeps_paths_scope_and_restales_tree_scope(self):
        r = receipt()
        unrelated = {"base_sha": BASE2, "changed_paths": ["docs/README.md"], "merge_tree": "3" * 40}
        self.assertIsNone(g.refresh_stale(r, PROFILE, unrelated))
        tree = dict(PROFILE, base={"ref": "origin/main", "scope": "tree"})
        self.assertEqual(g.refresh_stale(r, tree, unrelated)["reason"], "base_moved")
        hit = dict(unrelated, changed_paths=["prediction/sejd/fuse.py"])
        self.assertEqual(g.refresh_stale(r, PROFILE, hit)["reason"], "base_moved")
        self.assertEqual(g.refresh_stale(r, PROFILE, {"runner_sha": "4" * 40})["reason"],
                         "runner_changed")
        self.assertEqual(g.refresh_stale(r, PROFILE, {"branch_head": HEAD2})["reason"], "head_moved")

    def test_staging_artifact_size_and_identity_are_bound(self):
        r = receipt(inputs=[{"name": "staging", "kind": "artifact", "locator": "/s/out.bin",
                             "content_id": "sha256:" + "8" * 64, "bytes": 12_980_000_000,
                             "produced_by": "r-sejd-9-2", "hashed_by": "cm-gate"}])
        inp = r["inputs"][0]
        self.assertEqual((inp["bytes"], inp["produced_by"]), (12_980_000_000, "r-sejd-9-2"))
        self.assertEqual(inp["verification"], "hashed_at_run")


class Evidence(unittest.TestCase):
    def test_absent_skip_lists_are_incomplete_but_empty_lists_are_explicit(self):
        r = receipt()
        step(r, "must_fail")["skips"] = None
        v = verdict(r)
        self.assertEqual(v["verdict"], "INCOMPLETE")
        self.assertIn("must_fail: no skips list", texts(v))

    def test_inconsistent_rc_and_summary_fail(self):
        r = receipt()
        step(r, "pytest")["rc"] = 1
        self.assertIn("inconsistent: rc 1 with no failing node", texts(verdict(r)))
        r = receipt()
        step(r, "mypy")["rc"] = 1
        self.assertIn("inconsistent: rc 1 with Success", texts(verdict(r)))
        r = receipt()
        step(r, "pytest")["summary"] = "==== 4 passed in 1.00s ===="
        self.assertIn("summary says 4 passed, nodes say 3", texts(verdict(r)))

    def test_unparsed_summary_is_never_a_pass(self):
        r = receipt()
        step(r, "ruff")["summary"] = "ok"
        v = verdict(r)
        self.assertEqual(v["verdict"], "INCOMPLETE")
        self.assertIn("ruff: summary has no ruff result line (unparsed)", texts(v))

    def test_vacuous_pytest_is_incomplete(self):
        r = receipt()
        st = step(r, "pytest")
        st.update(rc=5, summary="no tests ran in 0.01s", nodes=[], skips=[], deselected=[],
                  granted_nodes=[])
        v = verdict(r)
        self.assertEqual(v["verdict"], "INCOMPLETE")
        self.assertIn("vacuous: 0 passed", texts(v))

    def test_code_moving_or_dirty_during_the_run_is_incomplete(self):
        r = receipt(worktree={"head_before": HEAD, "head_after": HEAD2, "dirty_before": False,
                              "dirty_after": True})
        t = texts(verdict(r))
        self.assertIn("code moved during the run", t)
        self.assertIn("worktree was dirty", t)

    def test_cm_gate_attestation_can_be_required(self):
        prof = g.validate_profile(dict(PROFILE_RAW, require={"runner": True, "attestation": "cm-gate"}))
        r = receipt(attestation="self")
        v = g.evaluate(r, prof, [])
        self.assertEqual(v["verdict"], "INCOMPLETE")
        self.assertIn("requires a cm-gate run", texts(v))

    def test_input_verification_is_computed_not_posted(self):
        r = receipt(attestation="self")
        self.assertEqual({i["verification"] for i in r["inputs"]}, {"self_supplied_unverified"})
        r = receipt()
        self.assertEqual([i["verification"] for i in r["inputs"]],
                         ["hashed_at_run", "self_supplied_unverified"])


class Parsers(unittest.TestCase):
    def test_pytest_summary_lines(self):
        self.assertEqual(g.parse_pytest("x\n=== 3 failed, 412 passed, 2 skipped, 1 xfailed, 1 error in 9.1s ===")
                         ["errors"], 1)
        self.assertEqual(g.parse_pytest("412 passed, 3 warnings in 1.20s")["passed"], 412)
        self.assertIsNone(g.parse_pytest("collected 3 items"))

    def test_mypy_and_ruff(self):
        self.assertEqual(g.parse_mypy("Found 2 errors in 1 file (checked 9 source files)"),
                         {"ok": False, "errors": 2, "files": 1})
        self.assertTrue(g.parse_mypy("Success: no issues found in 1 source file")["ok"])
        self.assertEqual(g.parse_ruff("Found 7 errors.\n[*] 3 fixable"), {"violations": 7})


class Profiles(unittest.TestCase):
    def test_mf_steps_require_an_mf_sha_and_expect_assertion_error(self):
        prof = g.validate_profile({"steps": [{"name": "mf", "kind": "pytest", "at": "mf"}]})
        self.assertTrue(prof["require"]["mf_sha"])
        self.assertEqual(prof["steps"][0]["expect_fail"], {"default": "AssertionError"})

    def test_bad_profiles_are_refused(self):
        for raw in ({"steps": []}, {"steps": [{"name": "x", "kind": "go"}]},
                    {"steps": [{"name": "x"}], "waivable": ["y"]},
                    {"steps": [{"name": "x", "skip_policy": {"allow_reasons": ["("]}}]}):
            with self.assertRaises(g.GateError):
                g.validate_profile(raw)


if __name__ == "__main__":
    unittest.main()
