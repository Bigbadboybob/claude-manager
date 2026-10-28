//! Frozen pre-change monitor loop and small deterministic fixture comparisons.
//! Requested allocations are measured per test thread; not RSS or retained heap.
use super::*;
use crate::test_support::allocations::measure;
use std::cell::RefCell;

thread_local! {
    static WRITE_FAULT: RefCell<Option<(PathBuf, &'static str)>> = const { RefCell::new(None) };
}
pub(super) fn write_fault(path: &Path, phase: &str) -> io::Result<()> {
    WRITE_FAULT.with(|slot| {
        if slot
            .borrow()
            .as_ref()
            .is_some_and(|(p, at)| p == path && *at == phase)
        {
            slot.borrow_mut().take();
            Err(io::Error::other("alloc56 injected persistence failure"))
        } else {
            Ok(())
        }
    })
}
fn with_fault<T>(path: PathBuf, phase: &'static str, run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            WRITE_FAULT.with(|s| s.borrow_mut().take());
        }
    }
    WRITE_FAULT.with(|s| {
        assert!(s.borrow().is_none());
        *s.borrow_mut() = Some((path, phase));
    });
    let _reset = Reset;
    let result = run();
    WRITE_FAULT.with(|s| assert!(s.borrow().is_none(), "write seam must be reached"));
    result
}

fn clock() -> DateTime<Utc> {
    parse_time("2026-10-28T12:00:00Z").unwrap()
}
fn monitor(id: &str, status: &str, scanned: u64) -> watches::Monitor {
    serde_json::from_value(json!({
        "id":id,"scope":{"channel":"general","dms":false,"include_children":false},
        "mode":"continuous","notify":"wake","include_self":true,
        "created_at":"2026-10-28T00:00:00Z","state":status,
        "start":0,"scanned":scanned,"hit_high":0,"hits":0,"acknowledged":0
    }))
    .unwrap()
}
fn fixture(root: &Path, mode: &str, operations: usize) -> Store {
    let mut s = Store::open(root).unwrap();
    s.send(
        "owner",
        "",
        "owner",
        &json!({"channel":"general","body":"match","request_id":"fixture-message"}),
        &[],
    )
    .unwrap();
    let mut p: Personal = serde_json::from_value(json!({
        "actor":"owner","revision":7,"bell_position":3,"bell_monitors":{"old":2},
        "norms_ack":"global-v1","channel_norms_ack":{"general":"v2"},
        "norms_supplied":["supplied"],"norms_offered":["offered"],
        "norm_pages":{"p":{"scope":"global","revision":"r","since":"s","action":"diff","digest":"d","through":12}},
        "monitor_receipts":["receipt1","receipt2"],
        "preferences":{"revision":9,"dnd":true,"bell":true,"rules":{"r":{
            "scope":{"channel":"general","dms":false,"include_children":false},
            "inbox":true,"wake":false,"muted":true,"since":1,"hub_since":2,"wake_since":3}}}
    })).unwrap();
    for i in 0..operations {
        p.operations.insert(
            format!("op-{i:05}"),
            personal::Operation {
                digest: "same-digest".into(),
                result: json!({"body":"x".repeat(8192),"nested":[i,true,null]}),
            },
        );
    }
    let mut inactive = monitor("inactive", "cancelled", 0);
    inactive.expires_at = Some("not a date but inactive".into());
    if mode != "empty" {
        p.monitors.insert(inactive.id.clone(), inactive);
    }
    if mode != "inactive" && mode != "empty" {
        let mut m = monitor(
            "active",
            "active",
            if mode == "unchanged" { s.position } else { 0 },
        );
        if mode == "once" {
            m.mode = "once".into();
        }
        if mode == "expired" {
            m.expires_at = Some("2026-10-28T11:00:00Z".into());
        }
        p.monitors.insert(m.id.clone(), m);
    }
    s.save_personal(p).unwrap();
    s
}
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &target);
        } else {
            fs::copy(e.path(), target).unwrap();
        }
    }
}
fn pair(mode: &str, operations: usize) -> (tempfile::TempDir, Store, Store) {
    let tmp = tempfile::tempdir().unwrap();
    let old = fixture(&tmp.path().join("old"), mode, operations);
    copy_tree(&old.root, &tmp.path().join("new"));
    let new = Store::open(&tmp.path().join("new")).unwrap();
    (tmp, old, new)
}
// Frozen implementation from 6a7ac3d, also used for behavioral comparisons.
fn old_monitor_pass(s: &mut Store, clock: DateTime<Utc>) -> Result<()> {
    let owners: Vec<_> = s.personal.keys().cloned().collect();
    for actor in owners {
        let mut state = s.personal_state(&actor);
        let mut changed = false;
        for monitor in state.monitors.values_mut() {
            changed |= s.scan_monitor(&actor, monitor, clock)?;
        }
        if changed {
            s.save_personal(state)?;
        }
    }
    Ok(())
}
fn states(s: &Store) -> Value {
    serde_json::to_value(&s.personal).unwrap()
}
fn assert_same(old: &Store, new: &Store) {
    assert_eq!(states(old), states(new));
    assert_eq!(old.position, new.position);
    for actor in old.personal.keys() {
        assert_eq!(
            load(&old.personal_path(actor)).unwrap(),
            load(&new.personal_path(actor)).unwrap()
        );
    }
}

#[test]
fn alloc56_monitor_behavior_and_requested_allocations() {
    for mode in [
        "empty",
        "inactive",
        "unchanged",
        "changed",
        "once",
        "expired",
    ] {
        let (_tmp, mut old, mut new) = pair(mode, 128);
        let old_signal = old.change_signal().version();
        let new_signal = new.change_signal().version();
        let old_before = states(&old);
        let (result, before) = measure(|| old_monitor_pass(&mut old, clock()));
        result.unwrap();
        let (result, after) = measure(|| new.advance_monitors_at(clock()));
        result.unwrap();
        assert_same(&old, &new);
        assert_eq!(
            old.change_signal().version() - old_signal,
            new.change_signal().version() - new_signal
        );
        assert!(
            after.bytes < before.bytes,
            "{mode}: {before:?} -> {after:?}"
        );
        if mode == "empty" || mode == "inactive" || mode == "unchanged" {
            assert_eq!(states(&old), old_before);
            assert!(
                after.bytes * 100 < before.bytes,
                "avoid operation-history copying"
            );
        } else {
            let m = &new.personal["owner"].monitors["active"];
            assert_eq!(m.hits, 1);
            assert_eq!(
                m.state,
                match mode {
                    "once" => "matched",
                    "expired" => "expired",
                    _ => "active",
                }
            );
            assert_eq!(m.scanned, old.position);
        }
        println!("ALLOC56 monitor {mode} old_calls={} new_calls={} old_requested_bytes={} new_requested_bytes={}", before.calls, after.calls, before.bytes, after.bytes);
        // Identical retry, persistence shape, all untouched Personal fields and restart.
        old_monitor_pass(&mut old, clock()).unwrap();
        new.advance_monitors_at(clock()).unwrap();
        assert_same(&old, &new);
        let expected = states(&new);
        let root = new.root.clone();
        drop(new);
        let reopened = Store::open(&root).unwrap();
        assert_eq!(states(&reopened), expected);
    }
}

#[test]
fn alloc56_monitor_scan_errors_do_not_commit_owner_candidates() {
    for kind in ["invalid_expiry", "hit_overflow", "revision_overflow"] {
        let (_tmp, mut old, mut new) = pair("changed", 2);
        for s in [&mut old, &mut new] {
            let p = s.personal.get_mut("owner").unwrap();
            match kind {
                "revision_overflow" => p.revision = u64::MAX,
                _ => {
                    let mut m = monitor("z-error", "active", 0);
                    if kind == "invalid_expiry" {
                        m.expires_at = Some("invalid".into());
                    } else {
                        m.hits = u64::MAX;
                    }
                    p.monitors.insert(m.id.clone(), m);
                }
            }
        }
        let prior = states(&new);
        let old_error = old_monitor_pass(&mut old, clock()).unwrap_err();
        let new_error = new.advance_monitors_at(clock()).unwrap_err();
        assert_eq!(old_error.code, new_error.code, "{kind}");
        assert_eq!(
            states(&new),
            prior,
            "{kind}: earlier changed monitor must remain uncommitted"
        );
        assert_same(&old, &new);
        assert_eq!(old.degraded, new.degraded);
    }
}

#[test]
fn alloc56_monitor_persistence_failures_keep_committed_ram_and_uncertainty() {
    for phase in ["before_rename", "after_rename"] {
        let (_tmp, mut old, mut new) = pair("changed", 2);
        let prior = states(&old);
        let mut expected_disk = None;
        for (s, legacy) in [(&mut old, true), (&mut new, false)] {
            let signal = s.change_signal().version();
            let path = s.personal_path("owner");
            let error = with_fault(path.clone(), phase, || {
                if legacy {
                    old_monitor_pass(s, clock())
                } else {
                    s.advance_monitors_at(clock())
                }
            })
            .unwrap_err();
            assert_eq!(error.code, "outcome_unknown");
            assert_eq!(states(s), prior);
            assert_eq!(s.change_signal().version(), signal);
            assert!(s
                .degraded
                .as_ref()
                .unwrap()
                .starts_with("Personal-state outcome requires reconciliation:"));
            let disk = load(&path).unwrap();
            if let Some(expected) = &expected_disk {
                assert_eq!(&disk, expected);
            } else {
                expected_disk = Some(disk.clone());
            }
            assert_eq!(disk == prior["owner"], phase == "before_rename");
            let retry = if legacy {
                old_monitor_pass(s, clock())
            } else {
                s.advance_monitors_at(clock())
            };
            assert_eq!(retry.unwrap_err().code, "store_read_only");
        }
        assert_same(&old, &new);
        let expected = load(&new.personal_path("owner")).unwrap();
        let root = new.root.clone();
        drop(new);
        let reopened = Store::open(&root).unwrap();
        assert_eq!(
            serde_json::to_value(&reopened.personal["owner"]).unwrap(),
            expected
        );
    }
}

#[test]
fn alloc56_monitor_prior_owner_commit_and_stale_task_guards_match() {
    let (_tmp, mut old, mut new) = pair("changed", 2);
    for s in [&mut old, &mut new] {
        let mut first = s.personal_state("owner");
        first.actor = "a-first".into();
        s.save_personal(first).unwrap();
        let mut stale = monitor("b-stale", "active", 0);
        stale.task_subscription = Some("missing-task".into());
        stale.expires_at = Some("invalid but not current".into());
        s.personal
            .get_mut("owner")
            .unwrap()
            .monitors
            .insert(stale.id.clone(), stale);
        let mut last = s.personal_state("owner");
        last.actor = "z-last".into();
        last.monitors.get_mut("active").unwrap().expires_at = Some("invalid".into());
        s.save_personal(last).unwrap();
    }
    let first_revision = new.personal["a-first"].revision;
    let last_before = serde_json::to_value(&new.personal["z-last"]).unwrap();
    assert_eq!(
        old_monitor_pass(&mut old, clock()).unwrap_err().code,
        new.advance_monitors_at(clock()).unwrap_err().code
    );
    assert_same(&old, &new);
    assert_eq!(new.personal["a-first"].revision, first_revision + 1);
    assert_eq!(
        serde_json::to_value(&new.personal["z-last"]).unwrap(),
        last_before
    );
    assert_eq!(new.personal["owner"].monitors["b-stale"].scanned, 0);
}

fn old_recent(marks: &BTreeMap<u64, Value>, pending: Option<&(u64, Value)>) -> Vec<Value> {
    let mut copied = marks.clone();
    if let Some((k, v)) = pending {
        copied.insert(*k, v.clone());
    }
    copied
        .iter()
        .rev()
        .take(256)
        .map(|(k, v)| json!([k, v]))
        .collect()
}
#[test]
fn alloc56_marks_overlay_order_boundaries_and_requested_allocations() {
    for n in [0, 1, 255, 256, 257, 4096] {
        let marks: BTreeMap<_, _> = (0..n)
            .map(|i| {
                (
                    i * 2,
                    json!({"cursor":i,"scopes":["general"],"payload":"m".repeat(256)}),
                )
            })
            .collect();
        for pending in [
            None,
            Some((0, json!({"replacement":true}))),
            Some((255, json!("middle"))),
            Some((u64::MAX, json!("newest"))),
        ] {
            let (before_value, before) = measure(|| old_recent(&marks, pending.as_ref()));
            let (after_value, after) =
                measure(|| replication::recent_marks(&marks, pending.as_ref()));
            assert_eq!(after_value, before_value, "n={n}, pending={pending:?}");
            if n == 4096 {
                assert!(after.bytes * 8 < before.bytes);
                println!("ALLOC56 marks n={n} pending={} old_calls={} new_calls={} old_requested_bytes={} new_requested_bytes={}", pending.is_some(), before.calls, after.calls, before.bytes, after.bytes);
            }
        }
    }
}

#[test]
fn alloc56_coverage_atomic_errors_replacement_and_history_retained() {
    for phase in ["before_rename", "after_rename", "success"] {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let generation = uuid();
        let scopes = BTreeSet::from(["general".into()]);
        // Many historical keys, including keys above the staged key: generic ordering.
        s.replication.marks = (0..300).map(|i| (i, json!({"cursor":i}))).collect();
        let old_marks = s.replication.marks.clone();
        let old_coverage = s.replication.coverage.clone();
        let old_reconciled = s.replication.last_reconciled.clone();
        let signal = s.change_signal().version();
        let path = s.root.join("_replication/coverage.json");
        let run = |s: &mut Store| {
            s.record_coverage_batch(
                &["general", "general"],
                &generation,
                44,
                Some((50, 2, &scopes)),
            )
        };
        if phase == "success" {
            run(&mut s).unwrap();
        } else {
            with_fault(path.clone(), phase, || run(&mut s)).unwrap_err();
            assert_eq!(s.replication.marks, old_marks);
            assert_eq!(s.replication.coverage, old_coverage);
            assert_eq!(s.replication.last_reconciled, old_reconciled);
            assert_eq!(s.change_signal().version(), signal);
            assert!(
                s.degraded.is_none(),
                "preserve pre-existing coverage error semantics"
            );
            assert_eq!(path.is_file(), phase == "after_rename");
            run(&mut s).unwrap();
        }
        let key = s.position.saturating_add(1);
        let replaced = s.replication.marks.get(&key).unwrap().clone();
        assert_eq!(
            s.replication.marks.len(),
            old_marks.len(),
            "same-position replacement"
        );
        for (k, v) in &old_marks {
            if *k != key {
                assert_eq!(&s.replication.marks[k], v);
            }
        }
        let mut expected_marks = old_marks;
        expected_marks.insert(key, replaced);
        assert_eq!(
            load(&path).unwrap()["marks"],
            json!(old_recent(&expected_marks, None))
        );
        assert_eq!(s.replication.coverage["general"]["through"], 44);
        let saved_bytes = fs::read(&path).unwrap();
        let version = s.change_signal().version();
        // Lower/equal scoped checkpoint is a no-op; no transport means no mark.
        s.record_coverage_batch(&["general"], &generation, 40, None)
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), saved_bytes);
        assert_eq!(s.change_signal().version(), version);
        s.record_coverage_batch(&["general"], &generation, 45, None)
            .unwrap();
        assert_eq!(s.replication.marks, expected_marks);
        assert_eq!(s.replication.coverage["general"]["through"], 45);
        s.position = 500;
        s.record_coverage_batch(&[], &generation, 0, Some((60, 3, &scopes)))
            .unwrap();
        assert_eq!(s.replication.marks.len(), 301);
        assert!(s.replication.marks.contains_key(&501));
        assert_eq!(
            load(&path).unwrap()["marks"],
            json!(old_recent(&s.replication.marks, None))
        );
    }
}

#[test]
fn alloc56_coverage_flush_precedes_checkpoint_and_failure_publishes_no_claim() {
    for flush_fails in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let generation = uuid();
        let scopes = BTreeSet::from(["general".into()]);
        let journal = s.journal_dir().join("99999999999999999999.json");
        let mut batch = replication::IngestBatch::default();
        batch.journal.push((journal.clone(), b"{}".to_vec()));
        if flush_fails {
            batch.dirs.insert(s.root.join("missing-ingest-dir"));
        }
        s.ingest_batch = Some(batch);
        let prior_coverage = s.replication.coverage.clone();
        let prior_marks = s.replication.marks.clone();
        let checkpoint = s.root.join("_replication/coverage.json");
        if flush_fails {
            let error = s
                .record_coverage_batch(&["general"], &generation, 10, Some((10, 1, &scopes)))
                .unwrap_err();
            assert_eq!(error.code, "outcome_unknown");
            assert!(s
                .degraded
                .as_ref()
                .unwrap()
                .starts_with("Publication outcome requires reconciliation:"));
            assert!(!journal.exists());
        } else {
            with_fault(checkpoint.clone(), "before_rename", || {
                s.record_coverage_batch(&["general"], &generation, 10, Some((10, 1, &scopes)))
            })
            .unwrap_err();
            assert_eq!(fs::read(journal).unwrap(), b"{}");
            assert!(s.degraded.is_none());
        }
        assert!(!checkpoint.exists());
        assert_eq!(s.replication.coverage, prior_coverage);
        assert_eq!(s.replication.marks, prior_marks);
        assert!(s.replication.last_reconciled.is_none());
    }
}
