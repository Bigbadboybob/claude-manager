//! Disconnected S5 observation types and a bounded in-memory queue.
//!
//! This module is not used by an authorization path. It starts no worker,
//! performs no filesystem or network IO, and cannot assert durability. All
//! limits are caller-supplied inactive engineering ceilings. A future caller
//! must capture facts from trusted sources and guarantee a unique producer
//! boot ID across processes; this library only allocates sequences within one
//! producer. `&mut self` requires exclusive ownership: it never acquires a
//! lock, waits for a consumer, or changes the separately supplied decision.

use serde::{Serialize, Serializer};
use std::collections::VecDeque;
use std::io::{self, Write};
use std::mem::size_of;

/// Hard bounds on configuration, not enabled runtime defaults.
pub const MAX_SLOTS: usize = 256;
pub const MAX_EVENT_BYTES: usize = 4096;
pub const MAX_QUEUED_JSON_BYTES: usize = 1024 * 1024;
pub const MAX_ID_BYTES: usize = 128;

/// A UTF-8 identifier bounded before any observation is constructed. It is
/// not a license to store credentials, prompts, paths or arbitrary text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Id {
    len: u8,
    bytes: [u8; MAX_ID_BYTES],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdError {
    Empty,
    TooLong,
}

impl Id {
    pub fn new(value: &str) -> Result<Self, IdError> {
        if value.is_empty() { return Err(IdError::Empty); }
        if value.len() > MAX_ID_BYTES { return Err(IdError::TooLong); }
        let mut bytes = [0; MAX_ID_BYTES];
        bytes[..value.len()].copy_from_slice(value.as_bytes());
        Ok(Self { len: value.len() as u8, bytes })
    }

    pub fn as_str(&self) -> &str {
        // Only `new` creates an Id, from valid UTF-8 without truncation.
        std::str::from_utf8(&self.bytes[..self.len as usize]).expect("valid Id")
    }
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

macro_rules! wire_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}

// These names match the accepted Python evaluator's wire values; there is no
// second policy implementation here.
wire_enum!(Truth { Yes, No, Unknown });
wire_enum!(Role { Owner, Main, Assistant, Worker, Workflow, Shell, Unknown });
wire_enum!(Identity { Verified, Missing, Stale, Unbound });
wire_enum!(Endpoint { Ready, MissingTarget, ExitedTarget, Unknown });
wire_enum!(BoardOperation { Create, EditItem, Place, Settings, SetPoc, CloseEpic, GateProfile, GateGrant, GateAllowlist });
wire_enum!(SettingsKind { General, Governed, GateDefault, GateAllowlist, Unspecified });
wire_enum!(CandidateResult { Allow, Deny, Unknown });
wire_enum!(FactSource { Registry, OrgSnapshot, ApiResponse, Operator, Legacy, Unverified });
wire_enum!(UnknownReason { Missing, Stale, Incomplete, Cyclic, Unbound, Unverified, NotApplicable });
wire_enum!(Origin { DaemonSession, ValidatedOperator, DirectApi, System, Unknown });
wire_enum!(Component { SessionScope, BoardScope, Endpoint, ApiResponse, Execution });
wire_enum!(Authorization { Pending, Allow, Deny, Error, NotEvaluated, Unknown });
wire_enum!(Execution { NotAttempted, Pending, Accepted, Completed, Failed, Unknown });
wire_enum!(Family { SessionControl, BoardEdit });

/// The source and revision accompany each established value. Unknown remains
/// explicit even when a current registry later gains more information. A
/// `Known` value with `Legacy` or `Unverified` source records a claim, not
/// trusted proof; the future adapter/evaluator must reject it as authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Knowledge<T> {
    Known { value: T, source: FactSource, revision: Option<u64> },
    Unknown { reason: UnknownReason },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct EventId {
    pub producer: Id,
    /// Opaque caller-provided boot UUID bytes, not a PID or transcript epoch.
    pub boot: [u8; 16],
    pub sequence: u64,
    pub child: u32,
    pub component: Component,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ActorSnapshot {
    pub origin: Origin,
    pub pid: Knowledge<Id>,
    pub uid: Knowledge<Id>,
    pub member: Knowledge<Id>,
    pub task: Knowledge<Id>,
    pub workspace: Knowledge<Id>,
    pub role: Knowledge<Role>,
    pub identity: Knowledge<Identity>,
    pub same_org: Knowledge<Truth>,
    pub global_perms: Knowledge<Truth>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TargetSnapshot {
    pub pid: Knowledge<Id>,
    pub uid: Knowledge<Id>,
    pub member: Knowledge<Id>,
    pub task: Knowledge<Id>,
    pub workspace: Knowledge<Id>,
    pub board: Knowledge<Id>,
    pub item: Knowledge<Id>,
    pub identity: Knowledge<Identity>,
    pub binding_epoch: Knowledge<u64>,
    pub occupancy_generation: Knowledge<u64>,
    pub holder_incarnation: Knowledge<u64>,
}

/// Facts are a bounded snapshot, not a graph or a display-derived identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Relations {
    pub is_self: Knowledge<Truth>,
    pub own_assistant: Knowledge<Truth>,
    pub report_session: Knowledge<Truth>,
    pub own_spawn: Knowledge<Truth>,
    pub manager_chain_valid: Knowledge<Truth>,
    pub team_member: Knowledge<Truth>,
    pub own_item_holder: Knowledge<Truth>,
    pub delegated_item: Knowledge<Truth>,
    pub board_team_lead: Knowledge<Truth>,
    pub steward: Knowledge<Truth>,
    pub orchestrator: Knowledge<Truth>,
    pub poc_chain: Knowledge<Truth>,
    pub poc_chain_valid: Knowledge<Truth>,
    pub gate_editor: Knowledge<Truth>,
    pub gate_grantor: Knowledge<Truth>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Draft {
    pub family: Family,
    pub board_operation: Option<BoardOperation>,
    pub settings_kind: Option<SettingsKind>,
    pub actor: ActorSnapshot,
    pub target: TargetSnapshot,
    pub relations: Relations,
    pub endpoint: Endpoint,
    /// Already computed by the authoritative path, never recalculated here.
    pub actual_authorization: Authorization,
    pub actual_reason: Id,
    pub execution: Execution,
    /// Candidate version is a reference only; no candidate result is computed.
    pub policy_version: Id,
    pub actual_policy_version: Knowledge<Id>,
    pub bootstrap_version: Knowledge<Id>,
    pub source_site: Id,
    pub org_snapshot_version: Knowledge<u64>,
    pub task_snapshot_version: Knowledge<u64>,
    pub board_snapshot_version: Knowledge<u64>,
    pub brain_epoch: Knowledge<u64>,
    pub monotonic_tick: Knowledge<u64>,
    pub wall_time_ms: Knowledge<u64>,
    pub parent: Option<EventId>,
    pub child: u32,
    pub component: Component,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Envelope {
    pub id: EventId,
    pub facts: Draft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub slots: usize,
    /// Sum of encoded JSON payload bytes currently queued; excludes framing.
    pub queued_json_bytes: usize,
    pub event_json_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError { InvalidLimit, InvalidProducer, SameBoot }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission { Admitted(EventId), Full(EventId), Oversize(EventId), Invalid(EventId), Closed, SequenceExhausted }

/// Counts here are per producer interval, not per site. Future integration
/// must account for uninstrumented sites and reserve per-site counters.
/// `ClosedUnverified` means only that the in-memory queue was drained without
/// a known loss. It is NOT a durable close or a claim of complete coverage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage { Open, Incomplete, Unknown, ClosedUnverified }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub attempted: u64,
    pub admitted: u64,
    pub loss: u64,
    pub oversize: u64,
    pub invalid: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntervalReport {
    pub producer: Id,
    pub boot: [u8; 16],
    pub next_sequence: u64,
    pub counters: Counters,
    pub coverage: Coverage,
}

/// An in-memory snapshot. Calling `checkpoint` does not write or fsync it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryCheckpoint { report: IntervalReport }

#[derive(Clone, Copy)]
struct Entry { envelope: Envelope, json_bytes: usize }

pub struct Queue {
    producer: Id,
    boot: [u8; 16],
    next_sequence: u64,
    limits: Limits,
    entries: VecDeque<Entry>,
    queued_json_bytes: usize,
    counters: Counters,
    uncertain: bool,
    closed: bool,
}

/// A counting writer computes the actual JSON payload length without storing
/// it. JSON serialization writes only to this bounded fixed-state counter;
/// there is no disk, socket or growable event buffer in this module.
#[derive(Default)]
struct Counter { bytes: usize, overflow: bool }
impl Write for Counter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.bytes.checked_add(buf.len()) {
            Some(bytes) => self.bytes = bytes,
            None => self.overflow = true,
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

impl Queue {
    /// Allocates ring storage at construction, outside any future auth path.
    /// Allocation failure is not recovered here; integration must arrange
    /// initialization so observation failure cannot affect authorization.
    pub fn new(producer: Id, boot: [u8; 16], limits: Limits) -> Result<Self, ConfigError> {
        if boot == [0; 16] { return Err(ConfigError::InvalidProducer); }
        if limits.slots == 0 || limits.slots > MAX_SLOTS || limits.event_json_bytes == 0
            || limits.event_json_bytes > MAX_EVENT_BYTES || limits.queued_json_bytes == 0
            || limits.queued_json_bytes > MAX_QUEUED_JSON_BYTES {
            return Err(ConfigError::InvalidLimit);
        }
        Ok(Self { producer, boot, next_sequence: 1, limits,
            entries: VecDeque::with_capacity(limits.slots), queued_json_bytes: 0,
            counters: Counters::default(), uncertain: false, closed: false })
    }

    /// Allocated queue entry storage, excluding allocator bookkeeping and the
    /// stack-owned `Queue` object. It is independent of the JSON-byte limit.
    pub fn reserved_entry_bytes(&self) -> usize { self.entries.capacity() * size_of::<Entry>() }
    pub fn queue_object_bytes(&self) -> usize { size_of::<Self>() }
    pub fn queued_json_bytes(&self) -> usize { self.queued_json_bytes }
    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    fn increment(value: &mut u64, uncertain: &mut bool) {
        match value.checked_add(1) {
            Some(v) => *value = v,
            None => *uncertain = true,
        }
    }

    fn allocate(&mut self, child: u32, component: Component) -> Option<EventId> {
        Self::increment(&mut self.counters.attempted, &mut self.uncertain);
        if self.next_sequence == u64::MAX {
            Self::increment(&mut self.counters.loss, &mut self.uncertain);
            self.uncertain = true;
            return None;
        }
        let id = EventId { producer: self.producer, boot: self.boot,
            sequence: self.next_sequence, child, component };
        self.next_sequence += 1;
        Some(id)
    }

    /// Account for an invalid/oversize fixed-field input rejected before a
    /// `Draft` could be built. Caller must use this on that path; otherwise
    /// completeness of the future producer is unknown.
    pub fn reject_input(&mut self, child: u32, component: Component, oversize: bool) -> Admission {
        if self.closed {
            Self::increment(&mut self.counters.attempted, &mut self.uncertain);
            Self::increment(&mut self.counters.loss, &mut self.uncertain);
            self.uncertain = true;
            return Admission::Closed;
        }
        let Some(id) = self.allocate(child, component) else { return Admission::SequenceExhausted };
        Self::increment(&mut self.counters.loss, &mut self.uncertain);
        if oversize {
            Self::increment(&mut self.counters.oversize, &mut self.uncertain);
            Admission::Oversize(id)
        } else {
            Self::increment(&mut self.counters.invalid, &mut self.uncertain);
            Admission::Invalid(id)
        }
    }

    /// Bounded admission; `draft` and the authoritative result are unchanged.
    /// Each failed attempt consumes an ID and increases the loss counter.
    pub fn try_enqueue(&mut self, draft: &Draft) -> Admission {
        if self.closed {
            Self::increment(&mut self.counters.attempted, &mut self.uncertain);
            Self::increment(&mut self.counters.loss, &mut self.uncertain);
            self.uncertain = true;
            return Admission::Closed;
        }
        let Some(id) = self.allocate(draft.child, draft.component) else { return Admission::SequenceExhausted };
        let envelope = Envelope { id, facts: *draft };
        let mut size = Counter::default();
        if serde_json::to_writer(&mut size, &envelope).is_err() || size.overflow {
            Self::increment(&mut self.counters.loss, &mut self.uncertain);
            Self::increment(&mut self.counters.invalid, &mut self.uncertain);
            self.uncertain = true;
            return Admission::Invalid(id);
        }
        if size.bytes > self.limits.event_json_bytes {
            Self::increment(&mut self.counters.loss, &mut self.uncertain);
            Self::increment(&mut self.counters.oversize, &mut self.uncertain);
            return Admission::Oversize(id);
        }
        if self.entries.len() >= self.limits.slots
            || size.bytes > self.limits.queued_json_bytes.saturating_sub(self.queued_json_bytes) {
            Self::increment(&mut self.counters.loss, &mut self.uncertain);
            return Admission::Full(id);
        }
        self.entries.push_back(Entry { envelope, json_bytes: size.bytes });
        self.queued_json_bytes += size.bytes;
        Self::increment(&mut self.counters.admitted, &mut self.uncertain);
        Admission::Admitted(id)
    }

    pub fn pop(&mut self) -> Option<Envelope> {
        let entry = self.entries.pop_front()?;
        self.queued_json_bytes -= entry.json_bytes;
        Some(entry.envelope)
    }

    pub fn report(&self) -> IntervalReport {
        let coverage = if self.uncertain { Coverage::Unknown }
            else if self.counters.loss != 0 { Coverage::Incomplete }
            else if self.closed && self.entries.is_empty() { Coverage::ClosedUnverified }
            else { Coverage::Open };
        IntervalReport { producer: self.producer, boot: self.boot, next_sequence: self.next_sequence,
            counters: self.counters, coverage }
    }

    pub fn checkpoint(&self) -> MemoryCheckpoint { MemoryCheckpoint { report: self.report() } }

    /// Close the in-memory producer; queued events may still be drained. No
    /// durable acknowledgment exists here, so even a drained close is unverified.
    pub fn close(&mut self) -> IntervalReport { self.closed = true; self.report() }

    /// Simulate losing a producer after its last captured checkpoint. The
    /// suffix (including uncheckpointed counters and pending entries) is
    /// unknown; the copied counters are lower bounds, not a durable receipt.
    pub fn simulate_crash(checkpoint: MemoryCheckpoint) -> IntervalReport {
        IntervalReport { coverage: Coverage::Unknown, ..checkpoint.report }
    }

    /// Simulate a new boot after a crash; never re-use the old producer/boot
    /// pair. Real cross-process uniqueness and persistence are future work.
    pub fn simulate_restart(checkpoint: MemoryCheckpoint, new_boot: [u8; 16], limits: Limits) -> Result<Self, ConfigError> {
        if new_boot == checkpoint.report.boot { return Err(ConfigError::SameBoot); }
        Self::new(checkpoint.report.producer, new_boot, limits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use serde_json::{json, Value};

    fn parse<T: DeserializeOwned>(v: &Value) -> T { serde_json::from_value(v.clone()).unwrap() }
    fn known<T>(value: T) -> Knowledge<T> {
        Knowledge::Known { value, source: FactSource::OrgSnapshot, revision: Some(7) }
    }
    fn unknown<T>() -> Knowledge<T> { Knowledge::Unknown { reason: UnknownReason::Missing } }
    fn id(s: &str) -> Id { Id::new(s).unwrap() }
    fn fact(case: &Value, name: &str) -> Knowledge<Truth> {
        let v = case["relations"].get(name).and_then(Value::as_str).unwrap_or("no");
        if v == "unknown" { Knowledge::Unknown { reason: UnknownReason::Incomplete } }
        else { known(parse(&json!(v))) }
    }
    fn draft(case: &Value) -> Draft {
        let role = known(parse(&case["role"]));
        let identity = known(parse(case.get("identity").unwrap_or(&json!("verified"))));
        let target_identity = known(parse(case.get("target_identity").unwrap_or(&json!("verified"))));
        Draft {
            family: parse(&case["family"]),
            board_operation: case.get("operation").map(parse),
            settings_kind: case.get("settings_kind").map(parse),
            actor: ActorSnapshot { origin: Origin::DaemonSession, pid: known(id("actor")),
                uid: known(id("actor-uid")), member: known(id("member")),
                task: if case["task"] == "missing" { unknown() } else { known(id("task")) },
                workspace: unknown(), role, identity,
                same_org: known(parse(case.get("same_org").unwrap_or(&json!("yes")))),
                global_perms: known(parse(case.get("global_perms").unwrap_or(&json!("no")))) },
            target: TargetSnapshot { pid: known(id("target")), uid: unknown(), member: unknown(),
                task: unknown(), workspace: unknown(), board: unknown(), item: unknown(),
                identity: target_identity, binding_epoch: known(1), occupancy_generation: known(2),
                holder_incarnation: unknown() },
            relations: Relations { is_self: fact(case, "is_self"), own_assistant: fact(case, "own_assistant"),
                report_session: fact(case, "report_session"), own_spawn: fact(case, "own_spawn"),
                manager_chain_valid: if case["chain_reason"] == "cyclic" {
                    Knowledge::Unknown { reason: UnknownReason::Cyclic }
                } else { fact(case, "manager_chain_valid") },
                team_member: fact(case, "team_member"), own_item_holder: fact(case, "own_item_holder"),
                delegated_item: fact(case, "delegated_item"), board_team_lead: fact(case, "board_team_lead"),
                steward: fact(case, "steward"), orchestrator: fact(case, "orchestrator"),
                poc_chain: fact(case, "poc_chain"), poc_chain_valid: fact(case, "poc_chain_valid"),
                gate_editor: fact(case, "gate_editor"), gate_grantor: fact(case, "gate_grantor") },
            endpoint: parse(case.get("endpoint").unwrap_or(&json!("ready"))),
            actual_authorization: Authorization::Error, actual_reason: id("original_error"),
            execution: Execution::Unknown, policy_version: id("s5-candidate-v2"),
            actual_policy_version: unknown(), bootstrap_version: unknown(), source_site: id("fixture"),
            org_snapshot_version: known(7), task_snapshot_version: unknown(), board_snapshot_version: unknown(), brain_epoch: known(3),
            monotonic_tick: known(10), wall_time_ms: unknown(), parent: None, child: 0,
            component: Component::SessionScope,
        }
    }
    fn cases() -> Vec<Value> {
        serde_json::from_str(include_str!("../../tests/fixtures/s5-shadow/cases.json")).unwrap()
    }
    fn limits(slots: usize, bytes: usize, event: usize) -> Limits {
        Limits { slots, queued_json_bytes: bytes, event_json_bytes: event }
    }
    fn queue(limits: Limits) -> Queue { Queue::new(id("producer"), [1; 16], limits).unwrap() }

    #[test]
    fn shared_fixtures_preserve_wire_and_immutable_facts() {
        for case in cases() {
            let input = draft(&case);
            let before = input;
            let mut q = queue(limits(1, MAX_QUEUED_JSON_BYTES, MAX_EVENT_BYTES));
            assert!(matches!(q.try_enqueue(&input), Admission::Admitted(_)), "{}", case["name"]);
            assert_eq!(input, before);
            let out = q.pop().unwrap();
            assert_eq!(out.facts, input);
            let wire = serde_json::to_value(out).unwrap();
            assert_eq!(wire["facts"]["family"], case["family"]);
            assert_eq!(wire["facts"]["actor"]["role"]["value"], case["role"]);
            assert_eq!(wire["facts"]["endpoint"], case.get("endpoint").cloned().unwrap_or(json!("ready")));
            assert_eq!(wire["facts"]["actual_authorization"], "error");
            assert_eq!(wire["facts"]["execution"], "unknown");
            assert_eq!(wire["facts"]["policy_version"], "s5-candidate-v2");
            if case["chain_reason"] == "cyclic" {
                assert_eq!(wire["facts"]["relations"]["manager_chain_valid"],
                    json!({"state":"unknown", "reason":"cyclic"}));
            }
            assert_eq!(parse::<Family>(&wire["facts"]["family"]), input.family);
            if let Some(op) = input.board_operation {
                assert_eq!(parse::<BoardOperation>(&wire["facts"]["board_operation"]), op);
            }
        }
    }

    #[test]
    fn batch_children_full_oversize_and_unchanged_original() {
        let mut q = queue(limits(2, MAX_QUEUED_JSON_BYTES, MAX_EVENT_BYTES));
        let mut a = draft(&cases()[0]);
        let original = a;
        let Admission::Admitted(parent) = q.try_enqueue(&a) else { panic!("parent") };
        a.parent = Some(parent); a.child = 1; a.component = Component::ApiResponse;
        a.actual_authorization = Authorization::Allow; a.execution = Execution::Completed;
        let Admission::Admitted(child) = q.try_enqueue(&a) else { panic!("child") };
        assert_ne!(parent.sequence, child.sequence);
        assert_eq!(q.try_enqueue(&a), Admission::Full(EventId { sequence: 3, ..child }));
        assert_eq!(q.pop().unwrap().facts, original);
        assert_eq!(q.pop().unwrap().facts.parent, Some(parent));
        assert_eq!(q.report().counters, Counters { attempted: 3, admitted: 2, loss: 1, ..Counters::default() });
        assert_eq!(q.close().coverage, Coverage::Incomplete);
        assert_eq!(a.actual_authorization, Authorization::Allow);
        let mut small = queue(limits(1, MAX_QUEUED_JSON_BYTES, 1));
        assert!(matches!(small.try_enqueue(&a), Admission::Oversize(_)));
        assert_eq!(small.report().counters.oversize, 1);
        let mut byte_bound = queue(limits(2, 1, MAX_EVENT_BYTES));
        assert!(matches!(byte_bound.try_enqueue(&a), Admission::Full(_)));
        assert!(byte_bound.is_empty());
    }

    #[test]
    fn interval_checkpoint_crash_restart_and_counter_exhaustion() {
        let mut q = queue(limits(1, MAX_QUEUED_JSON_BYTES, MAX_EVENT_BYTES));
        assert!(matches!(q.reject_input(0, Component::Endpoint, true), Admission::Oversize(_)));
        let cp = q.checkpoint();
        assert_eq!(Queue::simulate_crash(cp).coverage, Coverage::Unknown);
        assert!(matches!(Queue::simulate_restart(cp, [1; 16], q.limits), Err(ConfigError::SameBoot)));
        let mut restarted = Queue::simulate_restart(cp, [2; 16], q.limits).unwrap();
        let d = draft(&cases()[0]);
        let Admission::Admitted(new_id) = restarted.try_enqueue(&d) else { panic!("new boot") };
        assert_ne!(new_id.boot, cp.report.boot);
        // The library cannot prevent independently constructed producers
        // from reusing a boot ID; a future durable registry must fence it.
        let mut duplicate = Queue::new(id("producer"), [2; 16], q.limits).unwrap();
        assert_eq!(duplicate.try_enqueue(&d), Admission::Admitted(new_id));
        assert_eq!(restarted.close().coverage, Coverage::Open);
        restarted.pop();
        assert_eq!(restarted.report().coverage, Coverage::ClosedUnverified);
        assert_eq!(restarted.try_enqueue(&d), Admission::Closed);
        assert_eq!(restarted.report().coverage, Coverage::Unknown);
        let mut overflow = queue(limits(1, MAX_QUEUED_JSON_BYTES, MAX_EVENT_BYTES));
        overflow.counters.attempted = u64::MAX;
        overflow.next_sequence = u64::MAX;
        assert_eq!(overflow.try_enqueue(&d), Admission::SequenceExhausted);
        assert_eq!(overflow.report().coverage, Coverage::Unknown);
        assert_eq!(overflow.report().counters.loss, 1);
        assert_eq!(Id::new(&"x".repeat(MAX_ID_BYTES + 1)), Err(IdError::TooLong));
    }
}
