"""Behavioral contracts for disconnected candidate policy evaluation."""
from dataclasses import FrozenInstanceError, replace
import unittest

from dispatch.shadow_policy import (
    Actor, BoardFacts, BoardOperation as Op, Comparison, Endpoint, Identity,
    OriginalDecision, POLICY_VERSION, Result, Role, SessionFacts, SettingsKind, Truth,
    board_edit, compare, session_control,
)

Y, N, U = Truth.YES, Truth.NO, Truth.UNKNOWN
VERIFIED = Identity.VERIFIED
READY = Endpoint.READY


def actor(role=Role.MAIN, *, identity=VERIFIED, same_org=Y, global_perms=N):
    return Actor(role, identity, same_org, global_perms)


def session(role=Role.MAIN, **changes):
    facts = SessionFacts(actor(role), READY, target_identity=VERIFIED, is_self=N,
                         own_assistant=N, report_session=N, own_spawn=N,
                         manager_chain_valid=Y)
    return replace(facts, **changes)


def board(op=Op.EDIT_ITEM, role=Role.MAIN, **changes):
    facts = BoardFacts(actor(role), READY, op, settings_kind=SettingsKind.GENERAL, snapshot_identity=VERIFIED,
                       team_member=N, own_item_holder=N, delegated_item=N,
                       board_team_lead=N, steward=N, orchestrator=N,
                       gate_editor=N, gate_grantor=N, poc_chain=N, poc_chain_valid=Y)
    return replace(facts, **changes)


class SessionCandidateTests(unittest.TestCase):
    def test_known_scope_and_independent_spawn(self):
        cases = [
            (session(Role.OWNER), Result.ALLOW, "owner"),
            (session(actor=actor(global_perms=Y)), Result.ALLOW, "global_same_org"),
            (session(is_self=Y), Result.ALLOW, "self"),
            (session(own_assistant=Y), Result.ALLOW, "own_assistant"),
            (session(report_session=Y), Result.ALLOW, "report_session"),
            (session(), Result.DENY, "outside_manager_scope"),
            (session(Role.ASSISTANT, own_spawn=Y), Result.ALLOW, "own_spawn"),
            (session(Role.ASSISTANT, report_session=Y), Result.DENY, "not_own_spawn"),
            (session(Role.WORKER, own_spawn=Y), Result.UNKNOWN, "actor_role_policy_unspecified"),
            (session(Role.WORKFLOW, own_spawn=Y), Result.UNKNOWN, "actor_role_policy_unspecified"),
            (session(Role.SHELL), Result.UNKNOWN, "actor_role_policy_unspecified"),
        ]
        for facts, expected, reason in cases:
            with self.subTest(facts=facts):
                candidate = session_control(facts)
                self.assertEqual((candidate.decision.result, candidate.decision.reason), (expected, reason))
                self.assertEqual(candidate.decision.policy_version, POLICY_VERSION)

    def test_incomplete_cyclic_and_unknown_knowledge(self):
        cases = [
            (session(manager_chain_valid=N), "manager_chain_incomplete"),
            (session(manager_chain_valid=N, report_session=Y), "manager_chain_incomplete"),
            (session(manager_chain_valid=U, report_session=Y), "manager_chain_incomplete"),
            (session(report_session=U), "relation_unknown"),
            (session(Role.ASSISTANT, own_spawn=U), "relation_unknown"),
            (session(actor=actor(identity=Identity.STALE)), "actor_identity_unverified"),
            (session(actor=actor(identity=Identity.UNBOUND)), "actor_identity_unverified"),
            (session(target_identity=Identity.STALE), "target_identity_unverified"),
            (session(actor=actor(same_org=U)), "org_relation_unknown"),
            (session(actor=actor(global_perms=U)), "global_or_self_unknown"),
            (session(is_self=U), "global_or_self_unknown"),
            (session(own_spawn=None), "malformed_facts"),
        ]
        for facts, reason in cases:
            with self.subTest(facts=facts):
                self.assertEqual(session_control(facts).decision.result, Result.UNKNOWN)
                self.assertEqual(session_control(facts).decision.reason, reason)
        # A positive own-assistant fact does not require a manager traversal.
        self.assertEqual(session_control(session(own_assistant=Y, manager_chain_valid=U)).decision.result, Result.ALLOW)
        self.assertEqual(session_control(session(own_assistant=Y, actor=actor(global_perms=U))).decision.result, Result.ALLOW)

    def test_cross_org_and_endpoint_are_not_bypassed(self):
        for role, global_perms in [(Role.OWNER, N), (Role.MAIN, Y)]:
            candidate = session_control(session(role, actor=actor(role, same_org=N, global_perms=global_perms)))
            self.assertEqual((candidate.decision.result, candidate.decision.reason), (Result.DENY, "cross_org"))
        for endpoint in (Endpoint.MISSING_TARGET, Endpoint.EXITED_TARGET, Endpoint.UNKNOWN):
            candidate = session_control(session(Role.OWNER, endpoint=endpoint))
            self.assertEqual(candidate.precondition, endpoint)
            self.assertEqual(candidate.decision.result, Result.UNKNOWN)


class BoardCandidateTests(unittest.TestCase):
    def test_distinct_operation_scopes(self):
        cases = [
            (board(Op.CREATE, team_member=Y), Result.ALLOW, "board_team_member"),
            (board(Op.CREATE, own_item_holder=Y), Result.DENY, "no_board_scope"),
            (board(Op.EDIT_ITEM, own_item_holder=Y), Result.ALLOW, "item_holder"),
            (board(Op.EDIT_ITEM, Role.ASSISTANT, delegated_item=Y), Result.ALLOW, "delegated_item"),
            (board(Op.EDIT_ITEM, Role.ASSISTANT, team_member=Y), Result.DENY, "not_delegated"),
            (board(Op.PLACE, Role.ASSISTANT, delegated_item=Y), Result.DENY, "assistant_operation_not_delegated"),
            (board(Op.SETTINGS, board_team_lead=Y), Result.ALLOW, "board_team_lead"),
            (board(Op.SETTINGS, steward=Y), Result.ALLOW, "steward"),
            (board(Op.SETTINGS, team_member=Y), Result.DENY, "no_settings_scope"),
            (board(Op.SET_POC, board_team_lead=Y), Result.ALLOW, "board_team_lead"),
            (board(Op.SET_POC, steward=Y), Result.ALLOW, "steward"),
            (board(Op.SET_POC, Role.ASSISTANT, steward=Y), Result.ALLOW, "steward"),
            (board(Op.SET_POC, Role.ASSISTANT, poc_chain=Y), Result.DENY, "no_poc_scope"),
            (board(Op.SET_POC, orchestrator=Y), Result.ALLOW, "orchestrator"),
            (board(Op.SET_POC, poc_chain=Y), Result.ALLOW, "current_epic_poc_chain"),
            (board(Op.SET_POC), Result.DENY, "no_poc_scope"),
            (board(Op.CLOSE_EPIC, orchestrator=Y), Result.ALLOW, "orchestrator"),
            (board(Op.CLOSE_EPIC, Role.ASSISTANT, orchestrator=Y), Result.ALLOW, "orchestrator"),
            (board(Op.CLOSE_EPIC, steward=Y, poc_chain=Y), Result.DENY, "no_epic_close_scope"),
            (board(Op.SETTINGS, settings_kind=SettingsKind.GOVERNED, steward=Y), Result.DENY, "governed_setting_owner_or_orchestrator"),
            (board(Op.SETTINGS, settings_kind=SettingsKind.GOVERNED, orchestrator=Y), Result.ALLOW, "settings_scope_and_field_authorized"),
            (board(Op.SETTINGS, settings_kind=SettingsKind.GATE_DEFAULT, board_team_lead=Y), Result.DENY, "not_gate_editor"),
            (board(Op.SETTINGS, settings_kind=SettingsKind.GATE_DEFAULT, board_team_lead=Y, gate_editor=Y), Result.ALLOW, "settings_scope_and_field_authorized"),
            (board(Op.SETTINGS, settings_kind=SettingsKind.GATE_ALLOWLIST, board_team_lead=Y, gate_editor=Y), Result.DENY, "owner_only_gate_allowlist"),
            (board(Op.GATE_PROFILE, gate_editor=Y), Result.ALLOW, "gate_editor"),
            (board(Op.GATE_PROFILE, steward=Y), Result.DENY, "not_gate_editor"),
            (board(Op.GATE_GRANT, gate_grantor=Y), Result.ALLOW, "gate_grantor"),
            (board(Op.GATE_GRANT, team_member=Y), Result.DENY, "not_gate_grantor"),
            (board(Op.GATE_ALLOWLIST, orchestrator=Y), Result.DENY, "owner_only_gate_allowlist"),
            (board(Op.GATE_ALLOWLIST, Role.OWNER), Result.ALLOW, "owner"),
        ]
        for facts, expected, reason in cases:
            with self.subTest(facts=facts):
                candidate = board_edit(facts)
                self.assertEqual((candidate.decision.result, candidate.decision.reason), (expected, reason))
                self.assertEqual(candidate.decision.policy_version, POLICY_VERSION)

    def test_no_authority_invented_for_guest_contact_or_unknown(self):
        cases = [
            (board(team_member=U), "relation_unknown"),  # guest eligibility unverified
            (board(Op.GATE_GRANT, gate_grantor=U), "relation_unknown"),
            (board(Op.SET_POC, poc_chain=U), "relation_unknown"),
            (board(Op.SET_POC, poc_chain=Y, poc_chain_valid=N), "relation_unknown"),
            (board(Op.SETTINGS, settings_kind=SettingsKind.UNSPECIFIED, orchestrator=Y), "settings_specialization_required"),
            (board(Op.SETTINGS, settings_kind=SettingsKind.GATE_DEFAULT, board_team_lead=Y, gate_editor=U), "settings_restriction_unknown"),
            (board(Op.GATE_GRANT, Role.ASSISTANT, gate_grantor=Y), "assistant_independent_gate_authority_unspecified"),
            (board(role=Role.WORKER), "actor_role_policy_unspecified"),
            (board(role=Role.WORKFLOW), "actor_role_policy_unspecified"),
            (board(actor=actor(global_perms=Y)), "board_global_policy_unspecified"),
            (board(actor=actor(identity=Identity.UNBOUND)), "actor_identity_unverified"),
            (board(snapshot_identity=Identity.STALE), "board_snapshot_unverified"),
            (board(operation="fake"), "malformed_facts"),
        ]
        for facts, reason in cases:
            with self.subTest(facts=facts):
                self.assertEqual(board_edit(facts).decision.result, Result.UNKNOWN)
                self.assertEqual(board_edit(facts).decision.reason, reason)
        # Contact/team membership alone is not a gate grant.
        self.assertEqual(board_edit(board(Op.GATE_GRANT, team_member=Y)).decision.result, Result.DENY)
        self.assertEqual(board_edit(board(actor=actor(global_perms=Y), team_member=Y)).decision.result, Result.ALLOW)

    def test_cross_org_and_missing_target_precede_owner(self):
        candidate = board_edit(board(role=Role.OWNER, actor=actor(Role.OWNER, same_org=N)))
        self.assertEqual((candidate.decision.result, candidate.decision.reason), (Result.DENY, "cross_org"))
        candidate = board_edit(board(role=Role.OWNER, endpoint=Endpoint.MISSING_TARGET))
        self.assertEqual((candidate.precondition, candidate.decision.result), (Endpoint.MISSING_TARGET, Result.UNKNOWN))


class ObservationTests(unittest.TestCase):
    def test_original_result_is_not_recomputed_and_unknown_is_not_agreement(self):
        original = OriginalDecision("allow", "existing_task_tree")
        agreeing = compare(original, session_control(session(own_assistant=Y)))
        disagreeing = compare(original, board_edit(board()))
        indeterminate = compare(original, session_control(session(manager_chain_valid=U)))
        errored = compare(OriginalDecision("error", "target_not_found"), board_edit(board(team_member=Y)))
        self.assertEqual((agreeing.comparison, disagreeing.comparison, indeterminate.comparison, errored.comparison),
                         (Comparison.AGREEMENT, Comparison.DISAGREEMENT, Comparison.UNKNOWN, Comparison.UNKNOWN))
        self.assertIs(agreeing.original, original)
        self.assertIs(disagreeing.original, original)
        self.assertEqual(original.reason, "existing_task_tree")
        with self.assertRaises(FrozenInstanceError):
            original.outcome = "deny"

    def test_generic_settings_cannot_claim_effective_agreement(self):
        original = OriginalDecision("allow", "old_board_editor")
        generic = board_edit(board(Op.SETTINGS, settings_kind=SettingsKind.UNSPECIFIED,
                                   orchestrator=Y))
        self.assertEqual(compare(original, generic).comparison, Comparison.UNKNOWN)
        restricted = board_edit(board(Op.SETTINGS, settings_kind=SettingsKind.GATE_ALLOWLIST,
                                      orchestrator=Y))
        self.assertEqual(compare(original, restricted).comparison, Comparison.DISAGREEMENT)

    def test_bootstrap_input_does_not_enforce_or_change_decision(self):
        for evaluator, facts in ((session_control, session()), (board_edit, board())):
            self.assertEqual(evaluator(facts), evaluator(replace(facts, bootstrap_version="s6-bootstrap-v1")))


if __name__ == "__main__":
    unittest.main()
