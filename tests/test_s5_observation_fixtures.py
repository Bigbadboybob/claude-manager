"""Offline contract: consume the exact envelope matched by the Rust fixture.

This deliberately narrow adapter exists only for these fixtures. It neither
captures production facts nor authorizes work.
"""
from copy import deepcopy
import json
from pathlib import Path

import pytest
from dispatch import shadow_policy as p

CASES = json.loads((Path(__file__).parent / "fixtures/s5-shadow/cases.json").read_text())
TRUSTED = {"registry", "org_snapshot"}
SOURCES = TRUSTED | {"api_response", "operator", "legacy", "unverified"}
REASONS = {"missing", "stale", "incomplete", "cyclic", "unbound", "unverified", "not_applicable"}


def value(knowledge, enum, fallback):
    """Untrusted or absent claims are unknown, never proof of a value."""
    assert isinstance(knowledge, dict)
    if knowledge.get("state") == "unknown":
        assert set(knowledge) == {"state", "reason"} and knowledge["reason"] in REASONS
        return fallback
    assert set(knowledge) == {"state", "value", "source", "revision"}
    assert knowledge["state"] == "known" and knowledge["source"] in SOURCES
    assert knowledge["revision"] is None or isinstance(knowledge["revision"], int)
    parsed = enum(knowledge["value"])
    return parsed if knowledge["source"] in TRUSTED else fallback


def truth(knowledge):
    return value(knowledge, p.Truth, p.Truth.UNKNOWN)


def evaluate(wire):
    assert set(wire) == {"id", "facts"}
    event, f = wire["id"], wire["facts"]
    assert set(event) == {"producer", "boot", "sequence", "child", "component"}
    assert len(event["boot"]) == 16 and event["sequence"] > 0
    assert event["child"] == f["child"] and event["component"] == f["component"]
    assert set(f) == {"family", "board_operation", "settings_kind", "actor", "target", "relations",
                      "endpoint", "actual_authorization", "actual_reason", "execution", "policy_version",
                      "actual_policy_version", "bootstrap_version", "source_site", "org_snapshot_version",
                      "task_snapshot_version", "board_snapshot_version", "brain_epoch", "monotonic_tick",
                      "wall_time_ms", "parent", "child", "component"}
    assert f["policy_version"] == p.POLICY_VERSION
    assert f["actual_authorization"] in {"pending", "allow", "deny", "error", "not_evaluated", "unknown"}
    assert f["execution"] in {"not_attempted", "pending", "accepted", "completed", "failed", "unknown"}
    actor, target, rel = f["actor"], f["target"], f["relations"]
    assert set(actor) == {"origin", "pid", "uid", "member", "task", "workspace", "role", "identity", "same_org", "global_perms"}
    assert set(target) == {"pid", "uid", "member", "task", "workspace", "board", "board_snapshot_identity", "item", "identity", "binding_epoch", "occupancy_generation", "holder_incarnation"}
    assert set(rel) == {"is_self", "own_assistant", "report_session", "own_spawn", "manager_chain_valid", "team_member", "own_item_holder", "delegated_item", "board_team_lead", "steward", "orchestrator", "poc_chain", "poc_chain_valid", "gate_editor", "gate_grantor"}
    a = p.Actor(value(actor["role"], p.Role, p.Role.UNKNOWN),
                value(actor["identity"], p.Identity, p.Identity.MISSING),
                truth(actor["same_org"]), truth(actor["global_perms"]))
    endpoint = p.Endpoint(f["endpoint"])
    if f["family"] == "session_control":
        assert f["component"] == "session_scope" and f["board_operation"] is None and f["settings_kind"] is None
        keys = ("is_self", "own_assistant", "report_session", "own_spawn", "manager_chain_valid")
        facts = p.SessionFacts(a, endpoint, value(target["identity"], p.Identity, p.Identity.MISSING),
                               **{k: truth(rel[k]) for k in keys})
        candidate = p.session_control(facts)
    else:
        assert f["family"] == "board_edit" and f["component"] == "board_scope"
        keys = ("team_member", "own_item_holder", "delegated_item", "board_team_lead", "steward", "orchestrator", "poc_chain", "poc_chain_valid", "gate_editor", "gate_grantor")
        # An identity claim without a trusted snapshot version is not verified.
        snapshot_version = value(f["board_snapshot_version"], int, None)
        identity = value(target["board_snapshot_identity"], p.Identity, p.Identity.MISSING)
        if snapshot_version is None:
            identity = p.Identity.MISSING
        facts = p.BoardFacts(a, endpoint, p.BoardOperation(f["board_operation"]),
                             p.SettingsKind(f["settings_kind"] or "unspecified"), identity,
                             **{k: truth(rel[k]) for k in keys})
        candidate = p.board_edit(facts)
    return candidate


@pytest.mark.parametrize("case", CASES, ids=lambda case: case["name"])
def test_rust_wire_to_candidate_preserves_original(case):
    wire = deepcopy(case["wire"])
    before = deepcopy(wire)
    candidate = evaluate(wire)
    assert [candidate.decision.result.value, candidate.decision.reason] == case["expected"]
    assert candidate.decision.policy_version == p.POLICY_VERSION
    f = wire["facts"]
    original = p.OriginalDecision(f["actual_authorization"], f["actual_reason"])
    observation = p.compare(original, candidate)
    assert observation.original is original
    expected_comparison = (p.Comparison.UNKNOWN if candidate.decision.result == p.Result.UNKNOWN
                           or candidate.precondition != p.Endpoint.READY
                           or original.outcome not in ("allow", "deny")
                           else p.Comparison.AGREEMENT if candidate.decision.result.value == original.outcome
                           else p.Comparison.DISAGREEMENT)
    assert observation.comparison == expected_comparison
    assert wire == before
