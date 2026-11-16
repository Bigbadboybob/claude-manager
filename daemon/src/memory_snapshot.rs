//! On-demand operator diagnostics; no background workers or changes to retention.
//!
//! mallinfo2 takes glibc arena locks internally. There is NO hard total-time
//! bound and a disconnected/timed-out caller cannot cancel the C call. Keep
//! one process-wide permit on the SAME handler through aggregate Value
//! construction. Ordinary wire encoding and socket writes follow its release.
use crate::{
    control::{
        operator,
        protocol::{ErrorCode, Request, Response},
    },
    messaging::StoreMemorySnapshot,
    state::DaemonState,
};
use serde::Serialize;
use std::{
    io::Read,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const COLLECTION_BUDGET: Duration = Duration::from_millis(20);
const PARTICIPANT_LIMIT: usize = 4096;
pub(crate) static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

pub(crate) struct Admission<'a>(&'a AtomicBool);
impl<'a> Admission<'a> {
    pub(crate) fn enter(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(flag))
    }
}
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct Section<T> {
    pub(crate) status: &'static str,
    pub(crate) reason: Option<&'static str>,
    pub(crate) elapsed_us: u64,
    pub(crate) data: Option<T>,
}
impl<T> Section<T> {
    pub(crate) fn unavailable(reason: &'static str) -> Self {
        Self {
            status: "unavailable",
            reason: Some(reason),
            elapsed_us: 0,
            data: None,
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct Allocator {
    pub(crate) units: &'static str,
    pub(crate) arena_bytes: usize,
    pub(crate) used_arena_bytes: usize,
    pub(crate) free_arena_bytes: usize,
    pub(crate) mmapped_bytes: usize,
}

#[derive(Serialize)]
struct Snapshot {
    version: u32,
    status: &'static str,
    reason: Option<&'static str>,
    started_at: String,
    finished_at: String,
    elapsed_us: u64,
    brain_pid: u32,
    process_start_ticks: Option<u64>,
    process_start_reason: Option<&'static str>,
    holder_epoch: Option<u64>,
    build_id: &'static str,
    daemon_id: Option<String>,
    messaging_ready: Option<bool>,
    collection_budget_ms: u64,
    participant_limit: usize,
    timing_limit: &'static str,
    measurement_scope: &'static str,
    allocator_limitations: &'static str,
    allocator: Section<Allocator>,
    messaging: Section<StoreMemorySnapshot>,
}

pub(crate) fn dispatch(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    if let Err((code, _)) = operator::require_strong_operator(&req.caller) {
        return Response::err(
            req.id.clone(),
            code,
            "daemon.memory_snapshot requires a configured strong operator token",
        );
    }
    if !req.params.is_null() && !req.params.as_object().is_some_and(|p| p.is_empty()) {
        return Response::err(
            req.id.clone(),
            ErrorCode::InvalidParams,
            "daemon.memory_snapshot takes no parameters",
        );
    }
    Response::ok(
        req.id.clone(),
        capture(state, &IN_FLIGHT, COLLECTION_BUDGET, allocator),
    )
}

pub(crate) fn capture(
    state: &Arc<Mutex<DaemonState>>,
    in_flight: &AtomicBool,
    budget: Duration,
    allocator_read: impl FnOnce() -> Section<Allocator>,
) -> serde_json::Value {
    capture_with_value(state, in_flight, budget, allocator_read, |out| {
        serde_json::to_value(out).expect("memory snapshot contains serializable scalars")
    })
}

fn capture_with_value(
    state: &Arc<Mutex<DaemonState>>,
    in_flight: &AtomicBool,
    budget: Duration,
    allocator_read: impl FnOnce() -> Section<Allocator>,
    construct_value: impl FnOnce(&Snapshot) -> serde_json::Value,
) -> serde_json::Value {
    let began = Instant::now();
    let deadline = began + budget;
    let admission = Admission::enter(in_flight);
    let mut out = Snapshot {
        version: 1, status: "partial", reason: None,
        started_at: crate::messaging::now(), finished_at: String::new(), elapsed_us: 0,
        brain_pid: std::process::id(), process_start_ticks: None,
        process_start_reason: Some("not_collected"),
        holder_epoch: crate::holder_mode::global().map(|c| c.epoch),
        build_id: crate::build_id(), daemon_id: None, messaging_ready: None,
        collection_budget_ms: budget.as_millis() as u64, participant_limit: PARTICIPANT_LIMIT,
        timing_limit: "cooperative only; mallinfo2 arena locks and OS scheduling can exceed it; client timeout does not cancel capture",
        measurement_scope: "brain process only; sequential sections, not an atomic whole-process snapshot; no session/holder child memory",
        allocator_limitations: "glibc counters, not RSS or logical object bytes; arena used/free exclude mmapped blocks; tcache/allocator bookkeeping affect used; no trimming; allocator sections are not globally atomic across arenas",
        allocator: Section::unavailable("in_flight"),
        messaging: Section::unavailable("in_flight"),
    };
    if admission.is_none() {
        out.status = "busy";
        out.reason = Some("snapshot_in_flight");
    } else {
        out.process_start_ticks = process_start_ticks();
        out.process_start_reason = out
            .process_start_ticks
            .is_none()
            .then_some("proc_stat_unavailable");
        let start = Instant::now();
        out.allocator = if start >= deadline {
            Section::unavailable("deadline")
        } else {
            // No daemon or Store lock is acquired until this C call returns.
            allocator_read()
        };
        out.allocator.elapsed_us = start.elapsed().as_micros() as u64;
        let start = Instant::now();
        collect_messaging(state, deadline, &mut out);
        out.messaging.elapsed_us = start.elapsed().as_micros() as u64;
        out.status = if out.allocator.status == "ok"
            && out.messaging.status == "ok"
            && out.process_start_ticks.is_some()
        {
            "ok"
        } else {
            "partial"
        };
    }
    out.finished_at = crate::messaging::now();
    out.elapsed_us = began.elapsed().as_micros() as u64;
    // All state/store guards have gone out of scope before JSON allocation.
    let value = construct_value(&out);
    // Admission covers Value construction, including after a client timeout.
    // Ordinary response wire encoding and socket writes happen after this
    // returns and may overlap another capture; they have no diagnostic permit.
    drop(admission);
    value
}

fn collect_messaging(state: &Arc<Mutex<DaemonState>>, deadline: Instant, out: &mut Snapshot) {
    if Instant::now() >= deadline {
        out.messaging = Section::unavailable("deadline");
        return;
    }
    let handle = match state.try_lock() {
        Ok(state) => state.messaging.clone(),
        Err(std::sync::TryLockError::WouldBlock) => {
            out.messaging = Section::unavailable("daemon_state_busy");
            return;
        }
        Err(std::sync::TryLockError::Poisoned(_)) => {
            out.messaging = Section::unavailable("daemon_state_poisoned");
            return;
        }
    }; // Never hold registry and Store locks together.
    if Instant::now() >= deadline {
        out.messaging = Section::unavailable("deadline");
        return;
    }
    let slot = match handle.try_lock() {
        Ok(slot) => slot,
        Err(std::sync::TryLockError::WouldBlock) => {
            out.messaging = Section::unavailable("store_busy");
            return;
        }
        Err(std::sync::TryLockError::Poisoned(_)) => {
            out.messaging = Section::unavailable("store_poisoned");
            return;
        }
    };
    let Some(store) = slot.as_ref() else {
        out.messaging_ready = Some(false);
        out.messaging = Section::unavailable("messaging_not_ready");
        return;
    };
    out.messaging_ready = Some(true);
    out.daemon_id = Some(store.daemon_id.clone()); // Validated UUID, not participant/payload data.
    let counts = store.memory_snapshot(deadline, PARTICIPANT_LIMIT);
    out.messaging = Section {
        status: if counts.participants.complete {
            "ok"
        } else {
            "partial"
        },
        reason: counts.participants.reason,
        elapsed_us: 0,
        data: Some(counts),
    };
}

pub(crate) fn process_start_ticks() -> Option<u64> {
    let mut text = String::new();
    std::fs::File::open("/proc/self/stat")
        .ok()?
        .take(4096)
        .read_to_string(&mut text)
        .ok()?;
    parse_start_ticks(&text)
}
fn parse_start_ticks(text: &str) -> Option<u64> {
    text.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

pub(crate) fn allocator() -> Section<Allocator> {
    #[cfg(target_env = "gnu")]
    {
        // glibc takes each arena's mutex. This can block without a finite bound;
        // admission MUST NOT be released merely because the client gives up.
        let info = unsafe { libc::mallinfo2() };
        Section {
            status: "ok",
            reason: None,
            elapsed_us: 0,
            data: Some(Allocator {
                units: "bytes",
                arena_bytes: info.arena,
                used_arena_bytes: info.uordblks,
                free_arena_bytes: info.fordblks,
                mmapped_bytes: info.hblkhd,
            }),
        }
    }
    #[cfg(not(target_env = "gnu"))]
    {
        Section {
            status: "unsupported",
            ..Section::unavailable("mallinfo2_requires_glibc")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{dispatch::dispatch_request, protocol::Caller};

    fn state() -> Arc<Mutex<DaemonState>> {
        Arc::new(Mutex::new(DaemonState::default()))
    }
    fn fake() -> Section<Allocator> {
        Section {
            status: "ok",
            reason: None,
            elapsed_us: 0,
            data: Some(Allocator {
                units: "bytes",
                arena_bytes: 100,
                used_arena_bytes: 60,
                free_arena_bytes: 40,
                mmapped_bytes: 20,
            }),
        }
    }

    #[test]
    fn memory_snapshot_busy_and_missing_store_are_not_zero() {
        let state = state();
        let gate = AtomicBool::new(false);
        let locked = state.lock().unwrap();
        let busy = capture(&state, &gate, Duration::from_secs(1), fake);
        assert_eq!(busy["messaging"]["reason"], "daemon_state_busy");
        assert!(busy["messaging_ready"].is_null());
        assert!(busy["messaging"]["data"].is_null());
        let handle = locked.messaging.clone();
        drop(locked);
        let locked = handle.lock().unwrap();
        let busy = capture(&state, &gate, Duration::from_secs(1), fake);
        assert_eq!(busy["messaging"]["reason"], "store_busy");
        drop(locked);
        let absent = capture(&state, &gate, Duration::from_secs(1), fake);
        assert_eq!(absent["messaging_ready"], false);
        assert_eq!(absent["messaging"]["reason"], "messaging_not_ready");
        assert_eq!(absent["allocator"]["data"]["used_arena_bytes"], 60);
        assert_eq!(absent["allocator"]["data"]["units"], "bytes");
        assert!(absent["process_start_ticks"].as_u64().is_some());
    }

    #[test]
    fn memory_snapshot_deadline_and_unsupported_are_explicit() {
        let gate = AtomicBool::new(false);
        let expired = capture(&state(), &gate, Duration::ZERO, || panic!("past deadline"));
        assert_eq!(expired["allocator"]["reason"], "deadline");
        assert_eq!(expired["messaging"]["reason"], "deadline");
        assert!(expired["allocator"]["data"].is_null());
        let unsupported = capture(&state(), &gate, Duration::from_secs(1), || Section {
            status: "unsupported",
            ..Section::unavailable("mallinfo2_requires_glibc")
        });
        assert_eq!(unsupported["allocator"]["status"], "unsupported");
        assert!(unsupported["allocator"]["data"].is_null());
    }

    #[test]
    fn memory_snapshot_allocator_has_no_cm_locks_and_admission_survives_timeout() {
        let state = state();
        let gate = Arc::new(AtomicBool::new(false));
        let (entered, receive) = std::sync::mpsc::sync_channel(1);
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        let worker_state = state.clone();
        let worker_gate = gate.clone();
        let worker = std::thread::spawn(move || {
            capture(&worker_state, &worker_gate, Duration::from_secs(1), || {
                let handle = worker_state
                    .try_lock()
                    .expect("no daemon lock during allocator")
                    .messaging
                    .clone();
                assert!(handle.try_lock().is_ok(), "no Store lock during allocator");
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                fake()
            })
        });
        receive.recv_timeout(Duration::from_secs(5)).unwrap();
        // Simulate a client giving up while the uncancellable allocator stays in-flight.
        std::thread::sleep(Duration::from_millis(1100));
        for _ in 0..3 {
            let busy = capture(&state, &gate, Duration::from_secs(1), || {
                panic!("second allocator")
            });
            assert_eq!(busy["status"], "busy");
            assert_eq!(busy["reason"], "snapshot_in_flight");
            assert!(busy["allocator"]["data"].is_null());
        }
        release.send(()).unwrap();
        let result = worker.join().unwrap();
        assert_eq!(result["messaging"]["reason"], "deadline");
        assert!(!gate.load(Ordering::Acquire));
        assert_ne!(
            capture(&state, &gate, Duration::from_secs(1), fake)["status"],
            "busy"
        );
    }

    #[test]
    fn memory_snapshot_admission_releases_on_unwind() {
        let gate = AtomicBool::new(false);
        let result = std::panic::catch_unwind(|| {
            capture_with_value(&state(), &gate, Duration::from_secs(1), fake, |_| {
                assert!(gate.load(Ordering::Acquire));
                panic!("Value construction fixture");
            });
        });
        assert!(result.is_err());
        assert!(Admission::enter(&gate).is_some());
    }

    #[test]
    fn memory_snapshot_admission_covers_value_construction_but_not_wire_io() {
        let state = state();
        let gate = Arc::new(AtomicBool::new(false));
        let (entered, receive) = std::sync::mpsc::sync_channel(1);
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        let worker_state = state.clone();
        let worker_gate = gate.clone();
        let worker = std::thread::spawn(move || {
            capture_with_value(
                &worker_state,
                &worker_gate,
                Duration::from_secs(1),
                fake,
                |out| {
                    let handle = worker_state.try_lock().unwrap().messaging.clone();
                    assert!(
                        handle.try_lock().is_ok(),
                        "no CM locks during Value construction"
                    );
                    entered.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                    serde_json::to_value(out).unwrap()
                },
            )
        });
        receive.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            capture(&state, &gate, Duration::from_secs(1), || panic!(
                "second allocator"
            ))["status"],
            "busy"
        );
        release.send(()).unwrap();
        let response = Response::ok("snapshot".into(), worker.join().unwrap());
        assert!(
            !gate.load(Ordering::Acquire),
            "released before ordinary wire encoding"
        );
        // A disconnected writer cannot retain or resurrect diagnostic admission.
        struct Disconnected<'a>(&'a AtomicBool);
        impl std::io::Write for Disconnected<'_> {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                assert!(!self.0.load(Ordering::Acquire));
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let err =
            crate::control::wire::write_response(&mut Disconnected(&gate), &response).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        assert_ne!(
            capture(&state, &gate, Duration::from_secs(1), fake)["status"],
            "busy"
        );
    }

    #[test]
    fn memory_snapshot_ready_serializes_identity_units_and_counts_without_payloads() {
        let state = state();
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::messaging::Store::open(tmp.path()).unwrap();
        let id = store.daemon_id.clone();
        let position = store.publication_position();
        let handle = state.lock().unwrap().messaging.clone();
        *handle.lock().unwrap() = Some(store);
        let result = capture(
            &state,
            &AtomicBool::new(false),
            Duration::from_secs(1),
            fake,
        );
        assert_eq!(result["status"], "ok");
        assert_eq!(result["messaging_ready"], true);
        assert_eq!(result["daemon_id"], id);
        assert_eq!(result["brain_pid"], std::process::id());
        assert_eq!(result["build_id"], crate::build_id());
        assert!(result["started_at"].as_str().unwrap().ends_with('Z'));
        assert!(result["finished_at"].is_string());
        assert!(result["allocator"]["elapsed_us"].is_u64());
        assert!(result["messaging"]["elapsed_us"].is_u64());
        assert_eq!(
            result["messaging"]["data"]["participants"]["complete"],
            true
        );
        assert!(result["messaging"]["data"]["search_disposal_queue_count"].is_null());
        assert_eq!(
            handle
                .try_lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .publication_position(),
            position
        );
        let text = result.to_string();
        assert!(!text.contains("transcript"));
        assert!(!text.contains("event_id"));
        // The capture schema contains no per-event/participant arrays or strings.
        // Even maximum scalar counters remain small; this excludes the RPC ID.
        fn maximize_scalars(value: &mut serde_json::Value) -> usize {
            match value {
                serde_json::Value::Object(fields) => {
                    fields.values_mut().map(maximize_scalars).sum()
                }
                serde_json::Value::Array(_) => panic!("variable-length diagnostic array"),
                serde_json::Value::String(s) => {
                    assert!(s.len() <= 512);
                    1
                }
                serde_json::Value::Number(_) => {
                    *value = u64::MAX.into();
                    1
                }
                _ => 1,
            }
        }
        let mut largest = result;
        assert!(maximize_scalars(&mut largest) <= 80);
        assert!(serde_json::to_vec(&largest).unwrap().len() < 8192);
    }

    #[test]
    fn memory_snapshot_caller_id_uses_ordinary_frame_limits() {
        use crate::control::wire;
        let result = capture(
            &state(),
            &AtomicBool::new(false),
            Duration::from_secs(1),
            fake,
        );
        let mut req = Request {
            id: "i".repeat(1024 * 1024),
            caller: Caller::operator("fixture"),
            method: "daemon.memory_snapshot".into(),
            params: serde_json::json!({}),
        };
        let mut request_wire = Vec::new();
        wire::write_request(&mut request_wire, &req).unwrap();
        let parsed = wire::read_request(&mut request_wire.as_slice())
            .unwrap()
            .unwrap();
        let mut response = Response::ok(parsed.id, result);
        let mut response_wire = Vec::new();
        wire::write_response(&mut response_wire, &response).unwrap();
        let decoded = wire::read_response(&mut response_wire.as_slice())
            .unwrap()
            .unwrap();
        assert_eq!(decoded.id, req.id);

        // A legal request can still produce an over-cap response due to its ID.
        response.id.clear();
        let overhead = serde_json::to_vec(&response).unwrap().len();
        req.id = "i".repeat(wire::MAX_REQUEST_BYTES as usize - overhead + 1);
        request_wire.clear();
        wire::write_request(&mut request_wire, &req).unwrap();
        let parsed = wire::read_request(&mut request_wire.as_slice())
            .unwrap()
            .unwrap();
        response.id = parsed.id;
        response_wire.clear();
        let error = wire::write_response(&mut response_wire, &response).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(response_wire.is_empty(), "cap refusal precedes writes");
        wire::write_response_or_error(&mut response_wire, &response).unwrap();
        let fallback = wire::read_response(&mut response_wire.as_slice())
            .unwrap()
            .unwrap();
        assert_eq!(fallback.id, req.id);
        assert_eq!(fallback.error.unwrap().code, ErrorCode::Internal);
    }

    #[test]
    fn memory_snapshot_auth_precedes_locks_and_preserves_other_callers() {
        let _env = crate::test_support::env_lock();
        let state = state();
        let mut req = Request {
            id: "memory".into(),
            caller: Caller::operator("token"),
            method: "daemon.memory_snapshot".into(),
            params: serde_json::json!({}),
        };
        {
            let _auth = operator::test_override::set(None);
            let _locked = state.lock().unwrap();
            assert_eq!(
                dispatch_request(&state, &req)
                    .into_response()
                    .error
                    .unwrap()
                    .code,
                ErrorCode::Unauthorized
            );
            assert!(
                operator::validate_operator(&req.caller).is_ok(),
                "legacy compatibility unchanged"
            );
        }
        let _auth = operator::test_override::set(Some("token"));
        for caller in [
            Caller::session("session"),
            Caller::operator("wrong"),
            Caller::operator("cm-board-v1:wrong"),
        ] {
            req.caller = caller;
            let _locked = state.lock().unwrap();
            assert_eq!(
                dispatch_request(&state, &req)
                    .into_response()
                    .error
                    .unwrap()
                    .code,
                ErrorCode::Unauthorized
            );
        }
        req.caller = Caller::operator("token");
        req.params = serde_json::json!({"trim":true});
        assert_eq!(
            dispatch_request(&state, &req)
                .into_response()
                .error
                .unwrap()
                .code,
            ErrorCode::InvalidParams
        );
        req.params = serde_json::json!({});
        let _locked = state.lock().unwrap();
        let result = dispatch_request(&state, &req).into_response();
        assert!(result.ok);
        assert!(result.result.unwrap()["messaging"]["data"].is_null());
        assert!(
            !crate::control::dispatch::method_counts_as_restart_mutation("daemon.memory_snapshot")
        );
        assert!(crate::control::dispatch::method_counts_as_restart_mutation(
            "unknown.method"
        ));
    }
}
