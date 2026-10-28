"""Pure, proposed S5 decisions. This module is not an authorization path.

Facts must be captured from trusted, versioned sources by a future observer;
this module performs no IO and does not compute or replace the old decision.
Unknown facts do not imply denial. Endpoint preconditions are reported
separately from the candidate rule, so a missing target cannot become an allow.
For settings, the observer must supply the exact field category; mixed or
unclassified patches are UNSPECIFIED, not a general-settings grant. READY is
an independently established endpoint condition, never inferred from scope.
"""
from dataclasses import dataclass
from enum import Enum

POLICY_VERSION = "s5-candidate-v2"


class Truth(Enum):
    YES = "yes"
    NO = "no"
    UNKNOWN = "unknown"


class Result(Enum):
    ALLOW = "allow"
    DENY = "deny"
    UNKNOWN = "unknown"


class Identity(Enum):
    VERIFIED = "verified"
    MISSING = "missing"
    STALE = "stale"
    UNBOUND = "unbound"


class Role(Enum):
    OWNER = "owner"
    MAIN = "main"
    ASSISTANT = "assistant"
    WORKER = "worker"
    WORKFLOW = "workflow"
    SHELL = "shell"
    UNKNOWN = "unknown"


class Endpoint(Enum):
    # READY is an observer-supplied assertion that all applicable endpoint
    # preconditions were checked; it is not inferred from policy scope.
    READY = "ready"
    MISSING_TARGET = "missing_target"
    EXITED_TARGET = "exited_target"
    UNKNOWN = "unknown"


class BoardOperation(Enum):
    CREATE = "create"
    EDIT_ITEM = "edit_item"
    PLACE = "place"
    SETTINGS = "settings"
    SET_POC = "set_poc"
    CLOSE_EPIC = "close_epic"
    GATE_PROFILE = "gate_profile"
    GATE_GRANT = "gate_grant"
    GATE_ALLOWLIST = "gate_allowlist"


class SettingsKind(Enum):
    # One concrete field category, not an aggregate mixed-field patch.
    GENERAL = "general"
    GOVERNED = "governed"
    GATE_DEFAULT = "gate_default"
    GATE_ALLOWLIST = "gate_allowlist"
    UNSPECIFIED = "unspecified"


@dataclass(frozen=True)
class Decision:
    result: Result
    reason: str
    policy_version: str = POLICY_VERSION


@dataclass(frozen=True)
class Candidate:
    # The caller must handle this separately; it is not a policy decision.
    precondition: Endpoint
    decision: Decision


@dataclass(frozen=True)
class Actor:
    role: Role
    identity: Identity
    same_org: Truth
    global_perms: Truth = Truth.UNKNOWN


@dataclass(frozen=True)
class SessionFacts:
    actor: Actor
    endpoint: Endpoint
    target_identity: Identity = Identity.MISSING
    is_self: Truth = Truth.UNKNOWN
    # These are independently verified relations, not display attachments.
    own_assistant: Truth = Truth.UNKNOWN
    report_session: Truth = Truth.UNKNOWN
    own_spawn: Truth = Truth.UNKNOWN
    # False means incomplete/cyclic/invalid snapshot, never negative proof.
    manager_chain_valid: Truth = Truth.UNKNOWN
    # Reserved for future observer context. S6 bootstrap is not applied here.
    bootstrap_version: str | None = None


@dataclass(frozen=True)
class BoardFacts:
    actor: Actor
    endpoint: Endpoint
    operation: BoardOperation
    settings_kind: SettingsKind = SettingsKind.UNSPECIFIED
    snapshot_identity: Identity = Identity.MISSING
    # YES means confirmed eligibility on this board; guest status alone is
    # not evidence of eligibility. Incomplete membership is UNKNOWN.
    team_member: Truth = Truth.UNKNOWN
    own_item_holder: Truth = Truth.UNKNOWN
    delegated_item: Truth = Truth.UNKNOWN
    board_team_lead: Truth = Truth.UNKNOWN
    steward: Truth = Truth.UNKNOWN
    orchestrator: Truth = Truth.UNKNOWN
    # Exact epic current PoC or its manager chain, not a board-wide role.
    poc_chain: Truth = Truth.UNKNOWN
    poc_chain_valid: Truth = Truth.UNKNOWN
    # Independently checked current gate permission, not a contact/approver.
    gate_editor: Truth = Truth.UNKNOWN
    gate_grantor: Truth = Truth.UNKNOWN
    bootstrap_version: str | None = None


def _candidate(endpoint: Endpoint, result: Result, reason: str) -> Candidate:
    return Candidate(endpoint, Decision(result, reason))


def _base(actor: Actor, endpoint: Endpoint) -> Candidate | None:
    if endpoint is not Endpoint.READY:
        return _candidate(endpoint, Result.UNKNOWN, "endpoint_not_ready")
    if actor.identity is not Identity.VERIFIED:
        return _candidate(endpoint, Result.UNKNOWN, "actor_identity_unverified")
    if actor.same_org is Truth.UNKNOWN:
        return _candidate(endpoint, Result.UNKNOWN, "org_relation_unknown")
    if actor.same_org is Truth.NO:
        return _candidate(endpoint, Result.DENY, "cross_org")
    if actor.role is Role.UNKNOWN:
        return _candidate(endpoint, Result.UNKNOWN, "actor_role_unknown")
    return None


def _any_relation(endpoint: Endpoint, choices: tuple[tuple[Truth, str], ...], denial: str) -> Candidate:
    for value, reason in choices:
        if value is Truth.YES:
            return _candidate(endpoint, Result.ALLOW, reason)
    if any(value is Truth.UNKNOWN for value, _ in choices):
        return _candidate(endpoint, Result.UNKNOWN, "relation_unknown")
    return _candidate(endpoint, Result.DENY, denial)


def session_control(facts: SessionFacts) -> Candidate:
    """Evaluate §6.3 only; no old policy, task tree or runtime side effects."""
    actor, endpoint = facts.actor, facts.endpoint
    if not isinstance(endpoint, Endpoint):
        return _candidate(Endpoint.UNKNOWN, Result.UNKNOWN, "malformed_facts")
    if (not isinstance(actor, Actor) or not isinstance(actor.identity, Identity)
            or not isinstance(actor.same_org, Truth) or not isinstance(actor.global_perms, Truth)
            or not isinstance(actor.role, Role) or not isinstance(facts.target_identity, Identity)
            or any(not isinstance(v, Truth) for v in (facts.is_self, facts.own_assistant,
                facts.report_session, facts.own_spawn, facts.manager_chain_valid))):
        return _candidate(endpoint, Result.UNKNOWN, "malformed_facts")
    if endpoint is Endpoint.READY and facts.target_identity is not Identity.VERIFIED:
        return _candidate(endpoint, Result.UNKNOWN, "target_identity_unverified")
    base = _base(actor, endpoint)
    if base:
        return base
    if actor.role is Role.OWNER:
        return _candidate(endpoint, Result.ALLOW, "owner")
    if actor.global_perms is Truth.YES:
        return _candidate(endpoint, Result.ALLOW, "global_same_org")
    if facts.is_self is Truth.YES:
        return _candidate(endpoint, Result.ALLOW, "self")
    if actor.role is Role.MAIN:
        if facts.own_assistant is Truth.YES:
            result = _candidate(endpoint, Result.ALLOW, "own_assistant")
        elif facts.manager_chain_valid is not Truth.YES:
            result = _candidate(endpoint, Result.UNKNOWN, "manager_chain_incomplete")
        else:
            result = _any_relation(endpoint, ((facts.own_assistant, "own_assistant"),
                                             (facts.report_session, "report_session")), "outside_manager_scope")
    elif actor.role is Role.ASSISTANT:
        result = _any_relation(endpoint, ((facts.own_spawn, "own_spawn"),), "not_own_spawn")
    else:
        # Worker/workflow/shell semantics are not automatically assistant rights.
        result = _candidate(endpoint, Result.UNKNOWN, "actor_role_policy_unspecified")
    if result.decision.result is not Result.ALLOW and (
            actor.global_perms is Truth.UNKNOWN or facts.is_self is Truth.UNKNOWN):
        return _candidate(endpoint, Result.UNKNOWN, "global_or_self_unknown")
    return result


def _board_edit(facts: BoardFacts) -> Candidate:
    """Evaluate an operation-specific candidate, without authorizing a write."""
    actor, endpoint, op = facts.actor, facts.endpoint, facts.operation
    if not isinstance(endpoint, Endpoint):
        return _candidate(Endpoint.UNKNOWN, Result.UNKNOWN, "malformed_facts")
    if (not isinstance(actor, Actor) or not isinstance(actor.identity, Identity)
            or not isinstance(actor.same_org, Truth) or not isinstance(actor.global_perms, Truth)
            or not isinstance(actor.role, Role) or not isinstance(op, BoardOperation)
            or not isinstance(facts.settings_kind, SettingsKind)
            or not isinstance(facts.snapshot_identity, Identity)
            or any(not isinstance(v, Truth) for v in (facts.team_member, facts.own_item_holder,
                facts.delegated_item, facts.board_team_lead, facts.steward, facts.orchestrator,
                facts.gate_editor, facts.gate_grantor, facts.poc_chain, facts.poc_chain_valid))):
        return _candidate(endpoint, Result.UNKNOWN, "malformed_facts")
    if endpoint is Endpoint.READY and facts.snapshot_identity is not Identity.VERIFIED:
        return _candidate(endpoint, Result.UNKNOWN, "board_snapshot_unverified")
    base = _base(actor, endpoint)
    if base:
        return base
    if op is BoardOperation.SETTINGS and facts.settings_kind is SettingsKind.UNSPECIFIED:
        return _candidate(endpoint, Result.UNKNOWN, "settings_specialization_required")
    if actor.role is Role.OWNER:
        return _candidate(endpoint, Result.ALLOW, "owner")
    # A global bit is not itself an approval or a specialized board grant.
    # Unknown bypass semantics are accounted for when a candidate would deny.
    if actor.role in (Role.WORKER, Role.WORKFLOW, Role.SHELL):
        return _candidate(endpoint, Result.UNKNOWN, "actor_role_policy_unspecified")
    if op is BoardOperation.GATE_ALLOWLIST:
        return _candidate(endpoint, Result.DENY, "owner_only_gate_allowlist")
    if actor.role is Role.ASSISTANT:
        if op is BoardOperation.SET_POC:
            # Shipped steward/orchestrator checks are explicit PID roles;
            # they do not turn an assistant into its member's PoC.
            return _any_relation(endpoint, ((facts.steward, "steward"),
                                            (facts.orchestrator, "orchestrator")), "no_poc_scope")
        if op is BoardOperation.CLOSE_EPIC:
            return _any_relation(endpoint, ((facts.orchestrator, "orchestrator"),), "no_epic_close_scope")
        if op is BoardOperation.EDIT_ITEM:
            return _any_relation(endpoint, ((facts.delegated_item, "delegated_item"),), "not_delegated")
        if op in (BoardOperation.GATE_PROFILE, BoardOperation.GATE_GRANT):
            return _candidate(endpoint, Result.UNKNOWN, "assistant_independent_gate_authority_unspecified")
        return _candidate(endpoint, Result.DENY, "assistant_operation_not_delegated")
    if actor.role is not Role.MAIN:
        return _candidate(endpoint, Result.UNKNOWN, "actor_role_policy_unspecified")
    if op in (BoardOperation.CREATE, BoardOperation.PLACE):
        return _any_relation(endpoint, ((facts.team_member, "board_team_member"),
                                        (facts.board_team_lead, "board_team_lead"),
                                        (facts.steward, "steward"),
                                        (facts.orchestrator, "orchestrator")), "no_board_scope")
    if op is BoardOperation.EDIT_ITEM:
        return _any_relation(endpoint, ((facts.team_member, "board_team_member"),
                                        (facts.own_item_holder, "item_holder"),
                                        (facts.board_team_lead, "board_team_lead"),
                                        (facts.steward, "steward"),
                                        (facts.orchestrator, "orchestrator")), "no_item_scope")
    if op is BoardOperation.SETTINGS:
        scope = _any_relation(endpoint, ((facts.board_team_lead, "board_team_lead"),
                                         (facts.steward, "steward"),
                                         (facts.orchestrator, "orchestrator")), "no_settings_scope")
        if facts.settings_kind is SettingsKind.GATE_ALLOWLIST:
            return _candidate(endpoint, Result.DENY, "owner_only_gate_allowlist")
        if facts.settings_kind is SettingsKind.GENERAL:
            return scope
        if facts.settings_kind is SettingsKind.GOVERNED:
            restriction = _any_relation(endpoint, ((facts.orchestrator, "orchestrator"),), "governed_setting_owner_or_orchestrator")
        else:  # gate_default has a distinct, independently verified gate check
            restriction = _any_relation(endpoint, ((facts.gate_editor, "gate_editor"),
                                                    (facts.orchestrator, "orchestrator")), "not_gate_editor")
        if scope.decision.result is Result.DENY or restriction.decision.result is Result.DENY:
            return scope if scope.decision.result is Result.DENY else restriction
        if scope.decision.result is Result.UNKNOWN or restriction.decision.result is Result.UNKNOWN:
            return _candidate(endpoint, Result.UNKNOWN, "settings_restriction_unknown")
        return _candidate(endpoint, Result.ALLOW, "settings_scope_and_field_authorized")
    if op is BoardOperation.SET_POC:
        # #28's documented per-epic exception survives the older §6.4 table.
        direct = _any_relation(endpoint, ((facts.board_team_lead, "board_team_lead"),
                                           (facts.steward, "steward"),
                                           (facts.orchestrator, "orchestrator")), "no_board_poc_scope")
        if direct.decision.result is Result.ALLOW:
            return direct
        chain = (facts.poc_chain if facts.poc_chain_valid is Truth.YES else Truth.UNKNOWN)
        return _any_relation(endpoint, ((Truth.UNKNOWN if direct.decision.result is Result.UNKNOWN else Truth.NO, "board_poc_scope"),
                                         (chain, "current_epic_poc_chain")), "no_poc_scope")
    if op is BoardOperation.CLOSE_EPIC:
        # The #28 steward and current-PoC-chain exceptions are for set_poc.
        return _any_relation(endpoint, ((facts.board_team_lead, "board_team_lead"),
                                        (facts.orchestrator, "orchestrator")), "no_epic_close_scope")
    if op is BoardOperation.GATE_PROFILE:
        return _any_relation(endpoint, ((facts.gate_editor, "gate_editor"),
                                        (facts.orchestrator, "orchestrator")), "not_gate_editor")
    if op is BoardOperation.GATE_GRANT:
        return _any_relation(endpoint, ((facts.gate_grantor, "gate_grantor"),), "not_gate_grantor")
    return _candidate(endpoint, Result.UNKNOWN, "operation_unspecified")


def board_edit(facts: BoardFacts) -> Candidate:
    """Evaluate board candidate; unknown global bypass cannot manufacture a denial."""
    result = _board_edit(facts)
    if (result.decision.result is Result.DENY
            and result.decision.reason != "cross_org"
            and result.decision.reason != "owner_only_gate_allowlist"
            and facts.actor.global_perms is not Truth.NO):
        return _candidate(result.precondition, Result.UNKNOWN, "board_global_policy_unspecified")
    return result


class Comparison(Enum):
    AGREEMENT = "agreement"
    DISAGREEMENT = "disagreement"
    UNKNOWN = "unknown"


@dataclass(frozen=True)
class OriginalDecision:
    # An already-computed authoritative result; error is not a policy deny.
    outcome: str
    reason: str


@dataclass(frozen=True)
class Observation:
    original: OriginalDecision
    candidate: Candidate
    comparison: Comparison


def compare(original: OriginalDecision, candidate: Candidate) -> Observation:
    """Pair an externally supplied result unchanged with the candidate."""
    if (candidate.precondition is not Endpoint.READY
            or candidate.decision.result is Result.UNKNOWN
            or original.outcome not in ("allow", "deny")):
        comparison = Comparison.UNKNOWN
    elif original.outcome == candidate.decision.result.value:
        comparison = Comparison.AGREEMENT
    else:
        comparison = Comparison.DISAGREEMENT
    return Observation(original, candidate, comparison)
