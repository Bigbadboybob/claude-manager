use super::*;
use crate::{
    control::{dispatch::dispatch_request, protocol::Caller},
    state::DaemonState,
};
use std::sync::atomic::{AtomicUsize, Ordering};

fn identity() -> Identity {
    Identity {
        brain_pid: 7,
        process_start_ticks: Some(42),
        holder_epoch: Some(3),
        build_id: "fixture",
    }
}
fn params() -> Params {
    Params {
        request_id: "one-attempt".into(),
        brain_pid: 7,
        process_start_ticks: 42,
        holder_epoch: Some(3),
    }
}
fn allocator(free: usize) -> Section<Allocator> {
    Section {
        status: "ok",
        reason: None,
        elapsed_us: 0,
        data: Some(Allocator {
            units: "bytes",
            arena_bytes: 100 + free,
            used_arena_bytes: 100,
            free_arena_bytes: free,
            mmapped_bytes: 0,
        }),
    }
}
fn sample(free: usize) -> Metrics {
    Metrics {
        allocator: allocator(free),
        process: parse_process_memory("VmRSS: 200 kB\nVmSwap: 12 kB\n"),
    }
}
fn take<'a>(service: &Service<'a>, now: Instant) -> Option<Work<'a>> {
    service.take_work(&mut service.control.lock().unwrap(), now)
}

#[test]
fn memory_trim_defaults_reload_disable_stale_wake_and_no_catchup() {
    let defaults: crate::config::DaemonConfig = toml::from_str("").unwrap();
    assert_eq!(defaults.memory, MemoryConfig::default());
    assert_eq!(defaults.memory.trim_interval_secs, 600);
    assert_eq!(defaults.memory.trim_min_free_bytes, 1 << 30);
    let partial: crate::config::DaemonConfig =
        toml::from_str("[memory]\ntrim_interval_secs=0").unwrap();
    assert_eq!(partial.memory.trim_min_free_bytes, 1 << 30);
    assert!(
        toml::from_str::<crate::config::DaemonConfig>("[memory]\ntrim_intervl_secs=0").is_err()
    );
    let now = Instant::now();
    let gate = AtomicBool::new(false);
    let service = Service::new(defaults.memory, identity(), &gate, now);
    assert!(take(&service, now).is_none(), "no immediate startup check");
    assert!(take(&service, now + Duration::from_secs(599)).is_none());
    service.reload(partial.memory, now + Duration::from_secs(599));
    assert!(
        take(&service, now + Duration::from_secs(6000)).is_none(),
        "old wake cannot admit after disable"
    );
    let enabled_at = now + Duration::from_secs(6000);
    service.reload(
        MemoryConfig {
            trim_interval_secs: 10,
            trim_min_free_bytes: 10,
        },
        enabled_at,
    );
    assert!(
        take(&service, enabled_at).is_none(),
        "re-enable starts a fresh interval"
    );
    let late = enabled_at + Duration::from_secs(500);
    let work = take(&service, late).unwrap();
    assert!(take(&service, late).is_none(), "only one late slot");
    let finish = late + Duration::from_secs(500);
    let result = run(
        &identity(),
        &work,
        || sample(1),
        || panic!("below threshold"),
    );
    service.finish(work, &Ok(result), finish);
    assert!(take(&service, finish).is_none());
    assert!(take(&service, finish + Duration::from_secs(9)).is_none());
    assert!(take(&service, finish + Duration::from_secs(10)).is_some());
    assert!(next_due(
        &MemoryConfig {
            trim_interval_secs: u32::MAX,
            trim_min_free_bytes: 0
        },
        now
    )
    .is_some());
}

#[test]
fn memory_trim_reload_preserves_due_except_interval_change_and_manual_survives_off() {
    let now = Instant::now();
    let gate = AtomicBool::new(false);
    let service = Service::new(MemoryConfig::default(), identity(), &gate, now);
    let original_due = service.control.lock().unwrap().due;
    service.reload(MemoryConfig::default(), now + Duration::from_secs(599));
    assert_eq!(service.control.lock().unwrap().due, original_due);
    let changed_threshold = MemoryConfig {
        trim_min_free_bytes: 17,
        ..MemoryConfig::default()
    };
    service.reload(changed_threshold, now + Duration::from_secs(599));
    assert_eq!(service.control.lock().unwrap().due, original_due);
    let work = take(&service, now + Duration::from_secs(600)).unwrap();
    assert_eq!(work.min_free_bytes, Some(17));
    service.reload(
        MemoryConfig {
            trim_interval_secs: 100,
            trim_min_free_bytes: 9,
        },
        now + Duration::from_secs(601),
    );
    service.finish(work, &Err(()), now + Duration::from_secs(602));
    assert_eq!(
        service.control.lock().unwrap().due,
        Some(now + Duration::from_secs(701)),
        "completion preserves new interval anchor"
    );
    service.request(params()).unwrap();
    service.reload(
        MemoryConfig {
            trim_interval_secs: 0,
            trim_min_free_bytes: u64::MAX,
        },
        now,
    );
    assert_eq!(
        service.request(params()).unwrap()["attempt"]["status"],
        "pending"
    );
    let work = take(&service, now).expect("manual accepted before disable survives");
    let calls = AtomicUsize::new(0);
    let result = run(
        &identity(),
        &work,
        || sample(0),
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(1)
        },
    );
    service.finish(work, &Ok(result), now);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(take(&service, now + Duration::from_secs(10000)).is_none());
    assert_eq!(
        service.request(params()).unwrap()["attempt"]["status"],
        "complete"
    );
    drop(service);
    let disabled = Service::new(
        MemoryConfig {
            trim_interval_secs: 0,
            ..MemoryConfig::default()
        },
        identity(),
        &gate,
        now,
    );
    assert_eq!(
        disabled.request(params()).unwrap()["attempt"]["status"],
        "pending",
        "explicit request remains available when periodic is off"
    );
}

#[test]
fn memory_trim_threshold_boundary_unsupported_and_missing_metrics() {
    let gate = AtomicBool::new(false);
    let work = Work {
        _admission: Admission::enter(&gate).unwrap(),
        source: "periodic",
        min_free_bytes: Some(40),
        schedule_version: 0,
    };
    let below = run(
        &identity(),
        &work,
        || sample(39),
        || panic!("below threshold"),
    );
    assert_eq!(below.threshold_decision, "below_threshold");
    assert_eq!(below.status, "skipped");
    assert!(below.after.is_none());
    let calls = AtomicUsize::new(0);
    let result = run(
        &identity(),
        &work,
        || sample(40),
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(1)
        },
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.threshold_decision, "at_or_above_threshold");
    assert_eq!(result.malloc_trim_return, Some(1));
    // fordblks need not decrease after madvise; never interpret it as reclaimable.
    assert_eq!(
        result
            .after
            .as_ref()
            .unwrap()
            .allocator
            .data
            .as_ref()
            .unwrap()
            .free_arena_bytes,
        40
    );
    for status in ["unavailable", "unsupported"] {
        let result = run(
            &identity(),
            &work,
            || Metrics {
                allocator: Section {
                    status,
                    ..Section::unavailable("fixture")
                },
                process: Section::unavailable("proc_status_unreadable"),
            },
            || panic!("unknown bytes cannot pass a periodic threshold"),
        );
        let value = serde_json::to_value(result).unwrap();
        assert!(value["before"]["allocator"]["data"].is_null());
        assert!(value["before"]["process"]["data"].is_null());
        assert!(value["malloc_trim_return"].is_null());
        assert_eq!(value["threshold_decision"], "unavailable");
    }
    let unsupported = run(&identity(), &work, || sample(40), || None);
    assert_eq!(unsupported.status, "unsupported");
    assert!(unsupported.malloc_trim_return.is_none());
}

#[test]
fn memory_trim_operator_bypasses_threshold_and_preserves_failed_measurements() {
    let gate = AtomicBool::new(false);
    let work = Work {
        _admission: Admission::enter(&gate).unwrap(),
        source: "operator",
        min_free_bytes: None,
        schedule_version: 0,
    };
    let result = run(
        &identity(),
        &work,
        || Metrics {
            allocator: Section::unavailable("fixture"),
            process: parse_process_memory("VmRSS: bad kB\nVmSwap: 0 kB\n"),
        },
        || Some(0),
    );
    assert_eq!(
        result.status, "complete",
        "call completed, not a claim metrics succeeded"
    );
    assert_eq!(result.threshold_decision, "explicit_operator_bypass");
    let value = serde_json::to_value(result).unwrap();
    assert_eq!(value["malloc_trim_return"], 0);
    assert!(value["before"]["allocator"]["data"].is_null());
    assert!(value["after"]["process"]["data"]["rss_bytes"].is_null());
    assert_eq!(value["after"]["process"]["data"]["swap_bytes"], 0);
    assert_eq!(value["after"]["process"]["status"], "partial");
    let large = parse_process_memory("VmRSS: 18446744073709551615 kB\nVmSwap: 5 bytes\n");
    assert!(large.data.unwrap().rss_bytes.is_none());
}

#[test]
fn memory_trim_pending_running_dedup_excludes_snapshot_and_periodic_through_timeout() {
    let now = Instant::now();
    let gate = AtomicBool::new(false);
    let service = Service::new(MemoryConfig::default(), identity(), &gate, now);
    let state = Arc::new(Mutex::new(DaemonState::default()));
    assert_eq!(
        service.request(params()).unwrap()["attempt"]["status"],
        "pending"
    );
    // A dropped reply does not release the permit or create another queue entry.
    assert_eq!(
        service.request(params()).unwrap()["attempt"]["status"],
        "pending"
    );
    assert_eq!(
        memory_snapshot::capture(&state, &gate, Duration::from_secs(1), || panic!("overlap"))
            ["status"],
        "busy"
    );
    let work = take(&service, now).unwrap();
    let (entered, arrived) = std::sync::mpsc::sync_channel(1);
    let (release, wait) = std::sync::mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let service = &service;
        let state = &state;
        let worker = scope.spawn(move || {
            let result = run(
                &service.identity,
                &work,
                || sample(0),
                || {
                    assert!(service.control.try_lock().is_ok());
                    let store = state.try_lock().unwrap().messaging.clone();
                    assert!(store.try_lock().is_ok());
                    entered.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                    Some(1)
                },
            );
            service.finish(work, &Ok(result), Instant::now());
        });
        arrived.recv_timeout(Duration::from_secs(5)).unwrap();
        for _ in 0..3 {
            assert_eq!(
                service.request(params()).unwrap()["attempt"]["status"],
                "running"
            );
            assert_eq!(
                memory_snapshot::capture(state, &gate, Duration::ZERO, || panic!("overlap"))
                    ["status"],
                "busy"
            );
        }
        assert!(take(service, now + Duration::from_secs(1000)).is_none());
        let mut other = params();
        other.request_id = "other".into();
        assert!(service.request(other).is_err());
        service.reload(
            MemoryConfig {
                trim_interval_secs: 0,
                ..MemoryConfig::default()
            },
            Instant::now(),
        );
        assert!(gate.load(Ordering::Acquire), "reload off cannot cancel C");
        release.send(()).unwrap();
        worker.join().unwrap();
    });
    assert!(!gate.load(Ordering::Acquire));
    let receipt = service.request(params()).unwrap();
    assert_eq!(receipt["attempt"]["status"], "complete");
    assert_eq!(receipt["attempt"]["result"]["malloc_trim_return"], 1);
    assert_eq!(service.request(params()).unwrap(), receipt);
    assert!(take(&service, now + Duration::from_secs(10000)).is_none());
    assert_ne!(
        memory_snapshot::capture(&state, &gate, Duration::from_secs(1), || allocator(0))["status"],
        "busy"
    );
    assert!(serde_json::to_vec(&receipt).unwrap().len() < 8192);
}

#[test]
fn memory_trim_periodic_and_snapshot_busy_do_not_consume_operator_attempt() {
    let now = Instant::now();
    let gate = AtomicBool::new(false);
    let service = Service::new(
        MemoryConfig {
            trim_interval_secs: 1,
            trim_min_free_bytes: 1,
        },
        identity(),
        &gate,
        now,
    );
    let work = take(&service, now + Duration::from_secs(1)).unwrap();
    let state = Arc::new(Mutex::new(DaemonState::default()));
    let result = run(
        &identity(),
        &work,
        || sample(2),
        || {
            assert_eq!(service.request(params()).unwrap()["status"], "busy");
            assert!(service.control.lock().unwrap().receipt.is_none());
            assert_eq!(
                memory_snapshot::capture(&state, &gate, Duration::ZERO, || panic!("overlap"))
                    ["status"],
                "busy"
            );
            Some(0)
        },
    );
    service.finish(work, &Ok(result), now + Duration::from_secs(10));
    memory_snapshot::capture(&state, &gate, Duration::from_secs(1), || {
        assert_eq!(service.request(params()).unwrap()["status"], "busy");
        assert!(take(&service, now + Duration::from_secs(100)).is_none());
        allocator(0)
    });
    assert_eq!(
        service.request(params()).unwrap()["attempt"]["status"],
        "pending"
    );
    let work = take(&service, now).unwrap();
    // An unwind after entering C leaves an unknown receipt, never a re-admission.
    service.finish(work, &Err(()), now);
    assert_eq!(
        service.request(params()).unwrap()["attempt"]["status"],
        "unknown"
    );
    assert!(!gate.load(Ordering::Acquire));
    assert!(service.control.lock().unwrap().pending.is_none());
}

#[test]
fn memory_trim_auth_identity_quota_and_worker_failure_are_fail_closed() {
    let _env = crate::test_support::env_lock();
    let state = Arc::new(Mutex::new(DaemonState::default()));
    let mut req = Request {
        id: "frame".into(),
        caller: Caller::operator("token"),
        method: "daemon.memory_trim".into(),
        params: serde_json::to_value(params()).unwrap(),
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
            "ordinary legacy auth unchanged"
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
    let gate = AtomicBool::new(false);
    let service = Service::new(MemoryConfig::default(), identity(), &gate, Instant::now());
    for key in ["brain_pid", "process_start_ticks", "holder_epoch"] {
        req.params = serde_json::to_value(params()).unwrap();
        req.params[key] = 999.into();
        assert_eq!(
            dispatch_with(&req, Some(&service), true)
                .error
                .unwrap()
                .code,
            ErrorCode::Conflict
        );
        assert!(!gate.load(Ordering::Acquire));
    }
    for bad in [serde_json::json!({}), serde_json::json!({"trim":true})] {
        req.params = bad;
        assert_eq!(
            dispatch_with(&req, Some(&service), true)
                .error
                .unwrap()
                .code,
            ErrorCode::InvalidParams
        );
    }
    req.params = serde_json::to_value(params()).unwrap();
    req.params.as_object_mut().unwrap().remove("holder_epoch");
    assert_eq!(
        dispatch_with(&req, Some(&service), true)
            .error
            .unwrap()
            .code,
        ErrorCode::InvalidParams
    );
    req.params = serde_json::to_value(params()).unwrap();
    assert_eq!(
        dispatch_with(&req, Some(&service), false).result.unwrap()["status"],
        "unsupported"
    );
    assert!(service.control.lock().unwrap().receipt.is_none());
    service.control.lock().unwrap().worker_available = false;
    assert_eq!(
        dispatch_with(&req, Some(&service), true).result.unwrap()["reason"],
        "trim_worker_unavailable"
    );
    assert!(service.control.lock().unwrap().receipt.is_none());
    service.control.lock().unwrap().worker_available = true;
    assert_eq!(
        dispatch_with(&req, Some(&service), true).result.unwrap()["attempt"]["status"],
        "pending"
    );
    req.params["request_id"] = "new".into();
    assert_eq!(
        dispatch_with(&req, Some(&service), true)
            .error
            .unwrap()
            .code,
        ErrorCode::Conflict
    );
    assert!(!crate::control::dispatch::method_counts_as_restart_mutation("daemon.memory_trim"));
}

#[test]
fn memory_trim_monolith_and_unavailable_identity() {
    let gate = AtomicBool::new(false);
    let mut id = identity();
    id.holder_epoch = None;
    let service = Service::new(MemoryConfig::default(), id, &gate, Instant::now());
    let mut p = params();
    p.holder_epoch = None;
    assert_eq!(service.request(p).unwrap()["attempt"]["status"], "pending");
    drop(service);
    assert!(!gate.load(Ordering::Acquire));
    let mut id = identity();
    id.process_start_ticks = None;
    let service = Service::new(MemoryConfig::default(), id, &gate, Instant::now());
    assert_eq!(
        service.request(params()).unwrap()["reason"],
        "process_identity_unavailable"
    );
    assert!(!gate.load(Ordering::Acquire));
}
