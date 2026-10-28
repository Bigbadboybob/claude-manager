"""Offline, content-free shared observation contract fixtures; no live capture."""
import json
from pathlib import Path

import pytest
from dispatch import shadow_policy as p

CASES = json.loads((Path(__file__).parent / "fixtures/s5-shadow/cases.json").read_text())


@pytest.mark.parametrize("case", CASES, ids=lambda case: case["name"])
def test_candidate_and_independent_original(case):
    actor = p.Actor(p.Role(case["role"]), p.Identity(case.get("identity", "verified")),
                    p.Truth(case.get("same_org", "yes")), p.Truth(case.get("global_perms", "no")))
    endpoint = p.Endpoint(case.get("endpoint", "ready"))
    relations = {k: p.Truth(v) for k, v in case.get("relations", {}).items()}
    if case["family"] == "session_control":
        values = {k: p.Truth.NO for k in ("is_self", "own_assistant", "report_session", "own_spawn", "manager_chain_valid")}
        values.update({k: v for k, v in relations.items() if k in values})
        facts = p.SessionFacts(actor, endpoint, p.Identity(case.get("target_identity", "verified")), **values)
        candidate = p.session_control(facts)
    else:
        values = {k: p.Truth.NO for k in ("team_member", "own_item_holder", "delegated_item", "board_team_lead", "steward", "orchestrator", "poc_chain", "poc_chain_valid", "gate_editor", "gate_grantor")}
        values.update({k: v for k, v in relations.items() if k in values})
        facts = p.BoardFacts(actor, endpoint, p.BoardOperation(case["operation"]),
                             p.SettingsKind(case.get("settings_kind", "unspecified")),
                             p.Identity(case.get("target_identity", "verified")), **values)
        candidate = p.board_edit(facts)
    assert [candidate.decision.result.value, candidate.decision.reason] == case["expected"]
    assert candidate.decision.policy_version == p.POLICY_VERSION
    original = p.OriginalDecision("error", "independent_original")
    observation = p.compare(original, candidate)
    assert observation.original is original
    assert observation.comparison == p.Comparison.UNKNOWN
    assert original == p.OriginalDecision("error", "independent_original")
    known = p.OriginalDecision("allow", "independent_original")
    comparison = p.compare(known, candidate).comparison
    expected = (p.Comparison.UNKNOWN if candidate.decision.result == p.Result.UNKNOWN or endpoint != p.Endpoint.READY
                else p.Comparison.AGREEMENT if candidate.decision.result == p.Result.ALLOW
                else p.Comparison.DISAGREEMENT)
    assert comparison == expected
