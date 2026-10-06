//! Coverage checkpoint file, legacy coverage journal compatibility and
//! compaction, and page ingest with the event-ID index.
use super::*;

struct Pair {
    tmp: tempfile::TempDir,
    hub: Store,
    replica: Store,
}
fn pair() -> Pair {
    let tmp = tempfile::tempdir().unwrap();
    let mut hub = Store::open(&tmp.path().join("hub")).unwrap();
    let replica =
        Store::provision_replica(&tmp.path().join("replica"), &hub.space_descriptor()).unwrap();
    hub.register_host(&replica.daemon_id, true, true, &[])
        .unwrap();
    let mut p = Pair { tmp, hub, replica };
    sync(&mut p);
    p
}
/// Bulk catch-up the way transport.rs does: one page ingest + one checkpoint.
fn sync(p: &mut Pair) -> u64 {
    let mut after = p
        .replica
        .download_checkpoint()
        .get("cursor")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let through = p.hub.position;
    let scopes: BTreeSet<String> = p.hub.channels.values().cloned().collect();
    loop {
        let page = p
            .hub
            .export_page(&p.replica.daemon_id, &BTreeSet::new(), after, through)
            .unwrap();
        p.replica
            .ingest_replica_page(page["items"].as_array().unwrap())
            .unwrap();
        after = page["cursor"].as_u64().unwrap();
        let list: Vec<&str> = scopes
            .iter()
            .map(String::as_str)
            .chain(["metadata"])
            .collect();
        p.replica
            .record_coverage_batch(
                &list,
                &p.hub.generation.clone(),
                after,
                Some((after, 7, &scopes)),
            )
            .unwrap();
        if page["complete"] == true {
            return after;
        }
    }
}
fn post(p: &mut Pair, body: &str) -> Value {
    p.hub
        .send(
            "owner",
            "",
            "owner",
            &json!({"channel":"general","body":body,"request_id":body}),
            &[],
        )
        .unwrap()
}
fn reopen(p: &mut Pair) {
    let root = p.tmp.path().join("replica");
    let old = std::mem::replace(
        &mut p.replica,
        Store::open(&p.tmp.path().join("scratch")).unwrap(),
    );
    drop(old);
    p.replica = Store::open(&root).unwrap();
    assert!(p.replica.degraded.is_none(), "{:?}", p.replica.degraded);
}
fn journal_files(s: &Store) -> Vec<u64> {
    let mut v: Vec<u64> = fs::read_dir(s.journal_dir())
        .unwrap()
        .filter_map(|e| journal_file_position(&e.unwrap().path()))
        .collect();
    v.sort();
    v
}
/// Everything a reader can observe that compaction must not change.
fn fingerprint(s: &Store) -> Value {
    let general = s.channels["general"].clone();
    json!({
        "position": s.position,
        "events": s.events.iter().map(|e| json!([e.event["id"], e.position, e.received_at])).collect::<Vec<_>>(),
        "coverage": s.replication.coverage,
        "download": s.download_checkpoint(),
        "general": s.cached_coverage(&general),
        "metadata": s.cached_coverage("metadata"),
        "receipts": s.replication.receipts,
        "channels": s.channels(),
        "last_reconciled": s.replication.last_reconciled,
    })
}
fn assert_index_consistent(s: &Store) {
    assert_eq!(s.event_index.len(), s.events.len());
    for (i, e) in s.events.iter().enumerate() {
        assert_eq!(s.event_index[strv(&e.event, "id")], i);
    }
}
fn legacy_coverage(s: &mut Store, data: Value) {
    s.journal_status("coverage", None, None, data).unwrap();
}

#[test]
fn coverage_checkpoint_is_one_file_write_and_survives_reopen() {
    let mut p = pair();
    post(&mut p, "first");
    let journal_before = journal_files(&p.replica);
    let position = p.replica.position;
    let through = sync(&mut p);
    // One new publication -> one new journal record; coverage adds none.
    assert_eq!(journal_files(&p.replica).len(), journal_before.len() + 1);
    assert_eq!(p.replica.position, position + 1);
    assert!(p.replica.root.join(COVERAGE_FILE).exists());
    let general = p.replica.channels["general"].clone();
    assert_eq!(
        p.replica.cached_coverage(&general)["checkpoint"]["through"],
        through
    );
    let before = fingerprint(&p.replica);
    reopen(&mut p);
    assert_eq!(fingerprint(&p.replica), before);
    assert_eq!(p.replica.download_checkpoint()["revision"], 7);
    assert_eq!(p.replica.download_checkpoint()["cursor"], through);
    // Monotonic: an older claim never replaces a newer one.
    p.replica
        .record_coverage(&general, &p.hub.generation.clone(), 1)
        .unwrap();
    assert_eq!(
        p.replica.cached_coverage(&general)["checkpoint"]["through"],
        through
    );
    // Old single-call API still works and persists.
    p.replica
        .record_download_checkpoint(&p.hub.generation.clone(), through + 5, 9, &BTreeSet::new())
        .unwrap();
    reopen(&mut p);
    assert_eq!(p.replica.download_checkpoint()["revision"], 9);
    assert_eq!(p.replica.download_checkpoint()["cursor"], through + 5);
}

#[test]
fn legacy_coverage_journal_is_honoured_then_compacted_without_changing_state() {
    let mut p = pair();
    let generation = p.hub.generation.clone();
    let coordinator = p.hub.daemon_id.clone();
    let general = p.replica.channels["general"].clone();
    // Reproduce the old journal shape: per-scope + transport coverage records
    // around real publications.
    let mut legacy_marks = vec![];
    for round in 0..4u64 {
        post(&mut p, &format!("round-{round}"));
        let page = p
            .hub
            .export_page(&p.replica.daemon_id, &BTreeSet::new(), 0, p.hub.position)
            .unwrap();
        p.replica
            .ingest_replica_page(page["items"].as_array().unwrap())
            .unwrap();
        let cursor = p.hub.position;
        for scope in [general.as_str(), "metadata"] {
            legacy_coverage(
                &mut p.replica,
                json!({"scope":scope,"coordinator_id":coordinator,"generation":generation,"through":cursor,"complete":true}),
            );
        }
        legacy_coverage(
            &mut p.replica,
            json!({"scope":"transport","coordinator_id":coordinator,"generation":generation,"cursor":cursor,"revision":round,"scopes":[general],"complete":false}),
        );
        legacy_marks.push((p.replica.position, cursor));
    }
    // The checkpoint file (from pair()'s sync) predates these records, so
    // load must merge: legacy wins for general/metadata/transport.
    let token = p.replica.position_token(p.replica.position);
    let mid_token = p.replica.position_token(legacy_marks[1].0);
    let before = fingerprint(&p.replica);
    let files_before = journal_files(&p.replica);
    reopen(&mut p);
    // Legacy records are honoured on load.
    let mut after = fingerprint(&p.replica);
    after["last_reconciled"] = before["last_reconciled"].clone();
    assert_eq!(after, before);
    assert_eq!(p.replica.download_checkpoint()["revision"], 3);
    p.replica.wait_for_compaction();
    let files_after = journal_files(&p.replica);
    // Superseded coverage records are gone; publications and the newest
    // record (the position floor) remain; positions are not renumbered.
    assert!(files_after.len() < files_before.len());
    assert_eq!(files_after.last(), files_before.last());
    for pos in &files_after {
        let j = load(&p.replica.journal_dir().join(format!("{pos:020}.json"))).unwrap();
        assert!(j["kind"] != "coverage" || *pos == p.replica.position, "{j}");
    }
    let compacted = fingerprint(&p.replica);
    reopen(&mut p);
    assert_eq!(fingerprint(&p.replica), compacted);
    assert_eq!(compacted["coverage"], before["coverage"]);
    assert_eq!(compacted["position"], before["position"]);
    assert_eq!(compacted["events"], before["events"]);
    // Position tokens handed out before compaction stay valid, and keep
    // translating to the hub checkpoint known at that boundary.
    assert_eq!(
        p.replica.check_position(&token).unwrap(),
        before["position"]
    );
    let owner = p.replica.owner_execution_position(&token).unwrap();
    assert_eq!(owner["position"], legacy_marks[3].1);
    let owner = p.replica.owner_execution_position(&mid_token).unwrap();
    assert_eq!(owner["position"], legacy_marks[1].1);
    // New publications continue above the retained floor.
    post(&mut p, "after-compaction");
    sync(&mut p);
    assert_eq!(
        p.replica.position,
        compacted["position"].as_u64().unwrap() + 1
    );
    assert_index_consistent(&p.replica);
    // A token at the current position may predate the newest checkpoint, so
    // it translates conservatively to the previous one (never past it).
    let fresh_token = p.replica.position_token(p.replica.position);
    let owner = p.replica.owner_execution_position(&fresh_token).unwrap();
    assert_eq!(owner["position"], legacy_marks[3].1);
}

#[test]
fn superseded_leftovers_are_skipped_unread_and_deleted_on_a_later_open() {
    let mut p = pair();
    let generation = p.hub.generation.clone();
    let coordinator = p.hub.daemon_id.clone();
    for i in 0..5u64 {
        legacy_coverage(
            &mut p.replica,
            json!({"scope":"metadata","coordinator_id":coordinator,"generation":generation,"through":i,"complete":true}),
        );
    }
    post(&mut p, "tail");
    sync(&mut p);
    let doomed: Vec<u64> = journal_files(&p.replica)
        .into_iter()
        .filter(|pos| {
            load(&p.replica.journal_dir().join(format!("{pos:020}.json"))).unwrap()["kind"]
                == "coverage"
        })
        .collect();
    assert_eq!(doomed.len(), 5);
    // Simulate a crash before background deletion: reopen, then put a
    // superseded file back with unreadable contents.
    reopen(&mut p);
    p.replica.wait_for_compaction();
    let path = p
        .replica
        .journal_dir()
        .join(format!("{:020}.json", doomed[2]));
    assert!(!path.exists());
    fs::write(&path, b"not json").unwrap();
    let state = fingerprint(&p.replica);
    reopen(&mut p);
    assert_eq!(fingerprint(&p.replica), state);
    p.replica.wait_for_compaction();
    assert!(!path.exists());
}

#[test]
fn newer_legacy_record_wins_over_an_older_checkpoint_file() {
    // Downgrade then upgrade: an old binary journals coverage after the file.
    let mut p = pair();
    let generation = p.hub.generation.clone();
    let coordinator = p.hub.daemon_id.clone();
    p.replica
        .record_coverage("metadata", &generation, 10)
        .unwrap();
    legacy_coverage(
        &mut p.replica,
        json!({"scope":"metadata","coordinator_id":coordinator,"generation":generation,"through":20,"complete":true}),
    );
    reopen(&mut p);
    assert_eq!(
        p.replica.cached_coverage("metadata")["checkpoint"]["through"],
        20
    );
    // ...and an older legacy record does not override a newer file.
    p.replica
        .record_coverage("metadata", &generation, 30)
        .unwrap();
    reopen(&mut p);
    assert_eq!(
        p.replica.cached_coverage("metadata")["checkpoint"]["through"],
        30
    );
}

#[test]
fn page_ingest_uses_the_id_index_and_survives_reopen() {
    let mut p = pair();
    let root = post(&mut p, "root");
    let root_id = root["event"]["id"].as_str().unwrap().to_owned();
    let channel = p.hub.channels["general"].clone();
    let reply = p
        .hub
        .send(
            "owner",
            "",
            "owner",
            &json!({"channel":"general","body":"reply","request_id":"reply","reply_to":root_id}),
            &[],
        )
        .unwrap();
    let reply_id = reply["event"]["id"].as_str().unwrap().to_owned();
    let page = p
        .hub
        .export_page(&p.replica.daemon_id, &BTreeSet::new(), 0, p.hub.position)
        .unwrap();
    let items = page["items"].as_array().unwrap();
    let fresh = p.replica.ingest_replica_page(items).unwrap();
    assert_eq!(fresh.iter().filter(|b| **b).count(), 2, "{fresh:?}");
    // Replays are recognised by ID, byte-identical, and not re-journaled.
    let position = p.replica.position;
    assert!(p
        .replica
        .ingest_replica_page(items)
        .unwrap()
        .iter()
        .all(|b| !b));
    assert!(!p.replica.ingest_replica(&items[items.len() - 1]).unwrap());
    assert_eq!(p.replica.position, position);
    assert_index_consistent(&p.replica);
    let wire = p.replica.wire_event(&reply_id).unwrap();
    assert_eq!(wire["event"], p.hub.wire_event(&reply_id).unwrap()["event"]);
    assert_eq!(
        p.replica.published(&reply_id).unwrap().event["data"]["reply_to"],
        root_id
    );
    assert_eq!(
        p.replica.published(&root_id).unwrap().event["conversation_id"],
        channel
    );
    // A forged duplicate (same ID, different bytes) is still refused.
    let mut forged = items[items.len() - 1].clone();
    forged["event"] = json!(forged["event"].as_str().unwrap().replace("reply", "forged"));
    assert!(p.replica.ingest_replica_page(&[forged]).is_err());
    reopen(&mut p);
    assert_index_consistent(&p.replica);
    assert!(p.replica.has_event(&reply_id));
}

#[test]
fn a_failing_wire_keeps_earlier_page_records_durable() {
    let mut p = pair();
    post(&mut p, "kept");
    let page = p
        .hub
        .export_page(&p.replica.daemon_id, &BTreeSet::new(), 0, p.hub.position)
        .unwrap();
    let mut items = page["items"].as_array().unwrap().clone();
    items.push(json!({"event":"{}"}));
    assert!(p.replica.ingest_replica_page(&items).is_err());
    let state = fingerprint(&p.replica);
    assert!(p.replica.events.iter().any(|e| e.event["body"] == "kept"));
    reopen(&mut p);
    assert_eq!(fingerprint(&p.replica)["events"], state["events"]);
}
