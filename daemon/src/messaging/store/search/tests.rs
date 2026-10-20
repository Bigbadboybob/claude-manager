use super::*;
use std::cell::Cell;
thread_local! {
    static VISITS: Cell<usize> = const { Cell::new(0) };
    static SKIP_HOOK: Cell<bool> = const { Cell::new(false) };
}
pub(super) fn skip_hook() -> bool {
    SKIP_HOOK.with(Cell::get)
}
pub(super) fn visit() {
    VISITS.with(|n| n.set(n.get() + 1));
}

fn setup(root: &Path) -> (Store, Vec<Person>) {
    let mut s = Store::open(root).unwrap();
    let people: Vec<_> = ["a", "b", "c"]
        .into_iter()
        .map(|uid| Person {
            id: s.participant_id(uid),
            name: format!("Scout-{uid}"),
            session_uid: uid.into(),
            task: None,
            present: true,
            kind: "agent".into(),
            engine: None,
        })
        .collect();
    for p in &people {
        s.send(
            &p.id,
            &p.session_uid,
            "agent",
            &json!({"channel":"general","body":"Ready",
            "name":p.name,"request_id":"claim"}),
            &people,
        )
        .unwrap();
    }
    (s, people)
}
fn post(s: &mut Store, people: &[Person], who: usize, body: &str, extra: Value) -> Value {
    let mut p = json!({"body":body,"request_id":uuid()});
    p.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let a = &people[who];
    s.send(&a.id, &a.session_uid, "agent", &p, people).unwrap()["event"].clone()
}
fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| strv(e, "id").to_owned())
        .collect()
}
fn all(s: &mut Store, actor: &str, mut q: Value) -> Vec<String> {
    let mut result = Vec::new();
    loop {
        let page = match s.read(actor, &q, &[]) {
            Err(e) if e.code == "search_indexing" => {
                s.finish_search_for_test();
                s.read(actor, &q, &[]).unwrap()
            }
            result => result.unwrap(),
        };
        result.extend(ids(&page));
        if page["next_cursor"].is_null() {
            break;
        }
        q["cursor"] = page["next_cursor"].clone();
    }
    result
}
#[test]
fn chat_search_sender_phrase_scope_and_read_preview() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, people) = setup(tmp.path());
    let a = &people[0].id;
    let reader = &people[1].id;
    let yes = post(
        &mut s,
        &people,
        0,
        "Final VERDICT: Straße approved!",
        json!({"channel":"general","tags":["verdict"]}),
    );
    post(
        &mut s,
        &people,
        0,
        "final unrelated verdict",
        json!({"channel":"general"}),
    );
    post(
        &mut s,
        &people,
        1,
        "final verdict",
        json!({"channel":"general"}),
    );
    let private = post(
        &mut s,
        &people,
        0,
        "final verdict",
        json!({"dm":people[2].id}),
    );
    let cursor = s.read_cursor(reader, strv(&yes, "conversation_id"));
    let query =
        json!({"channel":"general","sender":"Scout-a","query":"FINAL verdict","tags":["verdict"]});
    assert_eq!(all(&mut s, reader, query), [strv(&yes, "id")]);
    assert_eq!(
        s.read_cursor(reader, strv(&yes, "conversation_id")),
        cursor,
        "search must remain a preview"
    );
    assert_eq!(
        all(
            &mut s,
            reader,
            json!({"channel":"general","sender":a,"query":"strasse APPROVED"})
        ),
        [strv(&yes, "id")]
    );
    assert_eq!(
        all(
            &mut s,
            reader,
            json!({"channel":"*","sender":"Scout-a","query":"final verdict"})
        ),
        [strv(&yes, "id")]
    );
    VISITS.with(|n| n.set(0));
    assert!(all(
        &mut s,
        reader,
        json!({"dms":true,"query":"final verdict","sender":a})
    )
    .is_empty());
    assert_eq!(
        VISITS.with(Cell::get),
        0,
        "DM search must skip already-indexed public history"
    );
    assert_eq!(
        s.read(
            reader,
            &json!({"conversation":private["conversation_id"],"query":"verdict"}),
            &[]
        )
        .unwrap_err()
        .code,
        "not_found"
    );
    assert_eq!(
        all(
            &mut s,
            &people[2].id,
            json!({"dms":true,"sender":a,"query":"final verdict"})
        ),
        [strv(&private, "id")]
    );
    assert!(
        all(&mut s, reader, json!({"channel":"general","query":"ver"})).is_empty(),
        "whole words, not prefixes"
    );
    assert!(all(
        &mut s,
        reader,
        json!({"channel":"general","sender":"agent:missing:historical"})
    )
    .is_empty());
    s.names.get_mut(a).unwrap().aliases.push("Old-Scout".into());
    assert_eq!(
        all(
            &mut s,
            reader,
            json!({"channel":"general","sender":"old scout","query":"strasse"})
        ),
        [strv(&yes, "id")]
    );
}

// Synthetic immutable publications avoid 44,000 fsyncs while exercising the
// actual publication/index entry point and normal read/cursor implementation.
fn indexed(
    s: &mut Store,
    cid: &str,
    sender: &str,
    body: &str,
    logical: u64,
    created: &str,
) -> String {
    s.position += 1;
    let id = format!("test:{:08}", s.position);
    s.push_published(Published {
        event: json!({"id":id,"type":"message.create","conversation_id":cid,
        "actor":{"id":sender,"name":"Synthetic"},"body":body,"logical_time":logical.to_string(),
        "created_at":created,"data":{}}),
        position: s.position,
        received_at: "2026-10-20T21:00:00Z".into(),
        event_sha256: String::new(),
    });
    id
}
#[test]
fn chat_search_indexed_44k_history_and_pagination_work_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, people) = setup(tmp.path());
    let cid = s.channel_id_at_path("general").unwrap().to_owned();
    let mut expected = Vec::new();
    for i in 0..44_000 {
        let hit = [10, 22_000, 43_990].contains(&i);
        let id = indexed(
            &mut s,
            &cid,
            if hit { &people[0].id } else { &people[2].id },
            if hit {
                "Final verdict GO"
            } else {
                "ordinary telemetry samples"
            },
            i + 100,
            "2026-10-20T20:00:00Z",
        );
        if hit {
            expected.push(id);
        }
    }
    for q in [
        json!({"query":"verdict"}),
        json!({"sender":"Scout-a"}),
        json!({"sender":"Scout-a","query":"final verdict"}),
    ] {
        let mut q = q;
        q["channel"] = json!("general");
        q["limit"] = json!(1);
        // Sender-only also includes the original ready message.
        q["time"] = json!({"start":"2026-10-20T19:00:00Z","end":"2026-10-20T20:30:00Z"});
        VISITS.with(|n| n.set(0));
        let start = std::time::Instant::now();
        assert_eq!(all(&mut s, &people[1].id, q.clone()), expected);
        let visits = VISITS.with(Cell::get);
        assert!(
            visits <= 9 && visits >= 3,
            "{q}: {visits} visits for 44k history"
        );
        println!(
            "SEARCH_44K query={q} visits={visits} elapsed_us={}",
            start.elapsed().as_micros()
        );
    }
    VISITS.with(|n| n.set(0));
    assert!(all(
        &mut s,
        &people[1].id,
        json!({"channel":"general","query":"nonexistentword"})
    )
    .is_empty());
    assert_eq!(VISITS.with(Cell::get), 0);
}
#[test]
fn chat_search_cursor_snapshot_time_and_arrival_order() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, people) = setup(tmp.path());
    let cid = s.channel_id_at_path("general").unwrap().to_owned();
    let after = s.position_token(s.position);
    let newer = indexed(
        &mut s,
        &cid,
        &people[0].id,
        "verdict GO",
        200,
        "2026-10-20T20:02:00Z",
    );
    let older = indexed(
        &mut s,
        &cid,
        &people[0].id,
        "verdict GO",
        100,
        "2026-10-20T20:01:00Z",
    );
    let tie = indexed(
        &mut s,
        &cid,
        &people[0].id,
        "verdict GO",
        100,
        "2026-10-20T20:01:00Z",
    );
    let actor = &people[1].id;
    let q = json!({"channel":"general","sender":"Scout-a","query":"verdict go","limit":1,
        "time":{"start":"2026-10-20T20:01:00Z","end":"2026-10-20T20:03:00Z"}});
    assert_eq!(
        all(&mut s, actor, q.clone()),
        [older.clone(), tie.clone(), newer.clone()]
    );
    let mut reverse = q.clone();
    reverse["newest_first"] = json!(true);
    assert_eq!(
        all(&mut s, actor, reverse),
        [newer.clone(), tie.clone(), older.clone()]
    );
    let first = s.read(actor, &q, &[]).unwrap();
    let unseen = indexed(
        &mut s,
        &cid,
        &people[0].id,
        "verdict GO",
        150,
        "2026-10-20T20:01:00Z",
    );
    let mut next = q.clone();
    next["cursor"] = first["next_cursor"].clone();
    assert_eq!(
        all(&mut s, actor, next.clone()),
        [tie.clone(), newer.clone()]
    );
    for (key, value) in [
        ("sender", json!(people[2].id)),
        ("query", json!("go")),
        ("channel", json!("*")),
        ("time", json!({"since":"10m"})),
    ] {
        let mut changed = next.clone();
        changed[key] = value;
        assert_eq!(
            s.read(actor, &changed, &[]).unwrap_err().code,
            "invalid_cursor"
        );
    }
    let mut arrival = q.clone();
    arrival["after"] = after;
    arrival["newest_first"] = json!(true);
    assert_eq!(
        all(&mut s, actor, arrival),
        [newer.clone(), older.clone(), tie.clone(), unseen.clone()]
    );
    let mut received = q.clone();
    received["time_basis"] = json!("received");
    assert!(all(&mut s, actor, received.clone()).is_empty());
    received["time"] = json!({"start":"2026-10-20T21:00:00Z","end":"2026-10-20T21:01:00Z"});
    assert_eq!(
        all(&mut s, actor, received),
        [older.clone(), tie.clone(), unseen.clone(), newer.clone()]
    );
    // Frozen sender identity survives a released name being claimed by another actor.
    let name = s.names[&people[0].id].name.clone();
    s.names.get_mut(&people[0].id).unwrap().name = "Renamed".into();
    s.names.get_mut(&people[2].id).unwrap().name = name;
    assert_eq!(all(&mut s, actor, next), [tie, newer]);
}
#[test]
fn chat_search_validation_and_reopen_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, people) = setup(tmp.path());
    let a = people[0].id.clone();
    let reader = people[1].id.clone();
    let msg = post(
        &mut s,
        &people,
        0,
        "VerDICT: go go",
        json!({"channel":"general"}),
    );
    let q = json!({"channel":"general","sender":"Scout-a","query":"go go"});
    let expected = all(&mut s, &reader, q.clone());
    assert_eq!(expected, [strv(&msg, "id")]);
    for bad in [
        json!({"sender":""}),
        json!({"query":""}),
        json!({"query":"!!!"}),
        json!({"query":12}),
        json!({"sender":[]}),
        json!({"inbox":true,"query":"go","mark_read_before":"2026-10-20T22:00:00Z"}),
    ] {
        assert_eq!(
            s.read(&reader, &bad, &[]).unwrap_err().code,
            "invalid_params"
        );
    }
    let owner = s
        .send(
            "owner",
            "",
            "owner",
            &json!({"channel":"general", "body":"Owner verdict", "request_id":"owner-verdict"}),
            &people,
        )
        .unwrap();
    assert_eq!(
        all(
            &mut s,
            &reader,
            json!({"channel":"general", "sender":"Owner", "query":"verdict"})
        ),
        [strv(&owner, "event_id")]
    );
    assert_eq!(
        s.read(&reader, &json!({"sender":"unknown-person"}), &[])
            .unwrap_err()
            .code,
        "not_found"
    );
    let name = s.names[&a].name.clone();
    s.names.get_mut(&people[2].id).unwrap().name = name;
    assert_eq!(
        s.read(&reader, &q, &[]).unwrap_err().code,
        "ambiguous_sender"
    );
    drop(s);
    let mut s = Store::open(tmp.path()).unwrap();
    assert!(s.degraded.is_none(), "{:?}", s.degraded);
    assert_eq!(all(&mut s, &reader, q), expected);
}

#[test]
fn chat_search_replica_live_before_backfill_and_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut hub = Store::open(&tmp.path().join("hub")).unwrap();
    let mut replica =
        Store::provision_replica(&tmp.path().join("replica"), &hub.space_descriptor()).unwrap();
    let people: Vec<_> = ["a", "b"]
        .map(|uid| Person {
            id: replica.participant_id(uid),
            name: format!("Scout-{uid}"),
            session_uid: uid.into(),
            task: None,
            present: true,
            kind: "agent".into(),
            engine: None,
        })
        .into();
    let host = replica.daemon_id.clone();
    hub.register_host(&host, true, true, &people).unwrap();
    for p in &people {
        hub.coordinate(
            &host,
            &p.id,
            "messaging.send",
            &json!({"channel":"general","body":"ready",
            "name":p.name,"request_id":format!("claim-{}",p.session_uid)}),
        )
        .unwrap();
    }
    let sync = |hub: &Store, replica: &mut Store| {
        let mut after = 0;
        loop {
            let page = hub
                .export_page(&host, &BTreeSet::new(), after, hub.position)
                .unwrap();
            replica
                .ingest_replica_page(page["items"].as_array().unwrap())
                .unwrap();
            after = page["cursor"].as_u64().unwrap();
            if page["complete"] == true {
                break;
            }
        }
    };
    sync(&hub, &mut replica);
    let covered = hub.position;
    let earlier = hub
        .coordinate(
            &host,
            &people[0].id,
            "messaging.send",
            &json!({"channel":"general",
        "body":"Final verdict: earlier","request_id":"earlier"}),
        )
        .unwrap();
    let live_start = hub.position;
    let later = hub
        .coordinate(
            &host,
            &people[0].id,
            "messaging.send",
            &json!({"channel":"general",
        "body":"Final verdict: later","mentions":[people[1].id],"request_id":"later"}),
        )
        .unwrap();
    let live = hub
        .export_live(&host, &BTreeSet::new(), covered, live_start, hub.position)
        .unwrap();
    replica
        .ingest_replica_page(live["items"].as_array().unwrap())
        .unwrap();
    let q = json!({"channel":"general","sender":"Scout-a","query":"final verdict","limit":1});
    assert_eq!(
        all(&mut replica, &people[1].id, q.clone()),
        [strv(&later, "event_id")]
    );
    sync(&hub, &mut replica);
    assert_eq!(
        all(&mut replica, &people[1].id, q.clone()),
        [strv(&earlier, "event_id"), strv(&later, "event_id")]
    );
    drop(replica);
    let mut replica = Store::open(&tmp.path().join("replica")).unwrap();
    assert!(replica.degraded.is_none(), "{:?}", replica.degraded);
    assert_eq!(
        all(&mut replica, &people[1].id, q),
        [strv(&earlier, "event_id"), strv(&later, "event_id")]
    );
}

#[test]
fn chat_search_lazy_build_and_incremental_maintenance() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, people) = setup(tmp.path());
    let cid = s.channel_id_at_path("general").unwrap().to_owned();
    let first = post(
        &mut s,
        &people,
        0,
        "unique verdict",
        json!({"channel":"general"}),
    );
    assert!(
        s.read_model.search.conversations.is_empty(),
        "publication/replay must stay cold"
    );
    let q = json!({"channel":"general","query":"unique verdict"});
    assert_eq!(all(&mut s, &people[1].id, q.clone()), [strv(&first, "id")]);
    let docs = s.read_model.search.conversations[&cid].documents.len();
    let second = post(
        &mut s,
        &people,
        0,
        "unique verdict",
        json!({"channel":"general"}),
    );
    assert_eq!(
        s.read_model.search.conversations[&cid].documents.len(),
        docs + 1
    );
    assert_eq!(
        all(&mut s, &people[1].id, q.clone()),
        [strv(&first, "id"), strv(&second, "id")]
    );
    let stable = s.read_model.search.conversations[&cid].documents.as_ptr();
    all(&mut s, &people[1].id, q);
    assert_eq!(
        s.read_model.search.conversations[&cid].documents.as_ptr(),
        stable,
        "warm read must not rebuild"
    );
    drop(s);
    let s = Store::open(tmp.path()).unwrap();
    assert!(
        s.read_model.search.conversations.is_empty(),
        "restart has no search allocations"
    );
}

#[test]
fn chat_search_background_catches_backfill_behind_snapshot_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, people) = setup(tmp.path());
    let cid = s.channel_id_at_path("general").unwrap().to_owned();
    for i in 0..512 {
        indexed(
            &mut s,
            &cid,
            &people[0].id,
            "ordinary telemetry",
            i + 100,
            "2026-10-20T20:00:00Z",
        );
    }
    let (entered, release) = s.pause_search_for_test();
    let q = json!({"channel":"general","query":"verdict","limit":1});
    assert_eq!(
        s.read(&people[1].id, &q, &[]).unwrap_err().code,
        "search_indexing"
    );
    let handle = Arc::new(Mutex::new(Some(s)));
    handle
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .start_search_worker(&handle)
        .unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    let (newer, older) = {
        let mut slot = handle.lock().unwrap();
        let s = slot.as_mut().unwrap();
        let newer = indexed(
            s,
            &cid,
            &people[0].id,
            "new verdict",
            900,
            "2026-10-20T20:00:00Z",
        );
        let older = indexed(
            s,
            &cid,
            &people[0].id,
            "backfilled verdict",
            1,
            "2026-10-20T20:00:00Z",
        );
        (newer, older)
    };
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if handle
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .read_model
            .search
            .worker
            .is_none()
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut s = handle.lock().unwrap().take().unwrap();
    assert_eq!(
        all(&mut s, &people[1].id, q.clone()),
        [older.clone(), newer.clone()]
    );
    let mut q = q;
    q["after"] = s.position_token(0);
    assert_eq!(all(&mut s, &people[1].id, q), [newer, older]);
    assert!(
        s.read_model
            .search
            .probe
            .max_lock_ns
            .load(std::sync::atomic::Ordering::Relaxed)
            < 50_000_000
    );
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn rss_bytes() -> u64 {
    fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| {
            l.strip_prefix("VmRSS:")
                .and_then(|r| r.split_whitespace().next())
                .and_then(|n| n.parse::<u64>().ok())
        })
        .unwrap()
        * 1024
}
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn allocated_bytes() -> usize {
    let info = unsafe { libc::mallinfo2() };
    info.uordblks + info.hblkhd
}
#[test]
#[ignore = "manual startup/memory benchmark; optionally set CM_SEARCH_CORPUS to message-body JSONL"]
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn chat_search_startup_and_resident_bench() {
    use std::time::Instant;
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, people) = setup(tmp.path());
    let corpus: Vec<Value> = std::env::var("CM_SEARCH_CORPUS")
        .ok()
        .map(|p| {
            fs::read_to_string(p)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        })
        .unwrap_or_default();
    let cid = s.channel_id_at_path("general").unwrap().to_owned();
    // Valid event/journal fixtures, written without per-record fsync: setup is
    // excluded from timings. Store::open still validates/replays the real files.
    let count = 44_000;
    let fixture_start = Instant::now();
    for i in 0..count {
        let source = (!corpus.is_empty()).then(|| &corpus[i % corpus.len()]);
        let generated=format!("Worker lane {}: final verdict approved after checking run {} at checkpoint {}. No new regression in acceptance checks.",i%39,i/31,i);
        let body = source.map(|v| strv(v, "body")).unwrap_or(&generated);
        let sender = source
            .map(|v| strv(v, "sender"))
            .unwrap_or(&people[i % people.len()].id);
        let e = s
            .event(
                "message.create",
                Some(&cid),
                body,
                json!({"mentions":[],"tags":[]}),
                sender,
                "Fixture",
                "agent",
                &format!("bench-{i}"),
                "",
            )
            .unwrap();
        let bytes = serde_json::to_vec(&e).unwrap();
        let digest = hash(&bytes);
        let path = s.event_path(&e).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &bytes).unwrap();
        s.position += 1;
        let receipt = json!({"space_id":s.space_id,"coordinator_id":s.daemon_id,"generation":s.generation,
            "position":format!("{:020}",s.position),"event_id":e["id"],"event_sha256":digest});
        let journal = json!({"protocol":1,"replica_id":s.daemon_id,"generation":s.generation,
            "position":format!("{:020}",s.position),"recorded_at":now(),"kind":"publish","event_id":e["id"],
            "event_sha256":digest,"data":{"source":"coordinated","received_at":now(),"logical_clock":s.clock,
                "event_path":path.strip_prefix(&s.root).unwrap().to_string_lossy(),"hub_receipt":receipt}});
        fs::write(
            s.journal_dir().join(format!("{:020}.json", s.position)),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
    }
    drop(corpus);
    drop(s);
    println!(
        "SEARCH_BENCH fixture_messages={count} setup_s={:.3}",
        fixture_start.elapsed().as_secs_f64()
    );
    let mut enabled = Vec::new();
    let mut disabled = Vec::new();
    for round in 0..6 {
        SKIP_HOOK.with(|v| v.set(round % 2 == 0));
        let start = Instant::now();
        let s = Store::open(tmp.path()).unwrap();
        let elapsed = start.elapsed();
        assert!(s.degraded.is_none(), "{:?}", s.degraded);
        assert!(s.read_model.search.conversations.is_empty());
        assert_eq!(s.read_model.conversations[&cid].messages.len(), count + 3);
        if round % 2 == 0 {
            disabled.push(elapsed);
        } else {
            enabled.push(elapsed);
        }
        drop(s);
    }
    SKIP_HOOK.with(|v| v.set(false));
    enabled.sort();
    disabled.sort();
    let s = Store::open(tmp.path()).unwrap();
    let handle = Arc::new(Mutex::new(Some(s)));
    unsafe {
        libc::malloc_trim(0);
    }
    let rss_before = rss_bytes();
    let alloc_before = allocated_bytes();
    let start = Instant::now();
    let request_start = Instant::now();
    {
        let mut slot = handle.lock().unwrap();
        let s = slot.as_mut().unwrap();
        assert_eq!(
            s.prepare_search("owner", Some(&cid), false, false)
                .unwrap_err()
                .code,
            "search_indexing"
        );
        s.start_search_worker(&handle).unwrap();
    }
    let cold_request = request_start.elapsed();
    loop {
        let ready = {
            let slot = handle.lock().unwrap();
            let search = &slot.as_ref().unwrap().read_model.search;
            search.worker.is_none() && search.conversations.contains_key(&cid)
        };
        if ready {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(120));
        std::thread::sleep(Duration::from_millis(5));
    }
    let cold = start.elapsed();
    let mut s = handle.lock().unwrap().take().unwrap();
    unsafe {
        libc::malloc_trim(0);
    }
    let rss_after = rss_bytes();
    let alloc_after = allocated_bytes();
    let c = &s.read_model.search.conversations[&cid];
    let postings = c
        .words
        .values()
        .chain(c.senders.values())
        .map(|p| p.arrivals.len() + p.canonical.len())
        .sum::<usize>();
    let words = c.words.len();
    let senders = c.senders.len();
    let q = json!({"channel":"general","query":"verdict","limit":20,"mark_read":false});
    VISITS.with(|n| n.set(0));
    let start = Instant::now();
    let result = s.read("owner", &q, &[]).unwrap();
    let warm = start.elapsed();
    let max_lock_ns = s
        .read_model
        .search
        .probe
        .max_lock_ns
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        max_lock_ns < 50_000_000,
        "cold build held lock for {max_lock_ns}ns"
    );
    println!(
        "SEARCH_BENCH cold_request_us={} max_lock_us={}",
        cold_request.as_micros(),
        max_lock_ns / 1000
    );
    println!("SEARCH_BENCH startup_without_hook_median_ms={:.3} startup_lazy_hook_median_ms={:.3} cold_build_ms={:.3} rss_before={} rss_after={} rss_delta={} allocated_before={} allocated_after={} allocated_delta={} unique_words={} senders={} posting_entries={} warm_query_us={} warm_visits={} returned={}",
        disabled[1].as_secs_f64()*1000.,enabled[1].as_secs_f64()*1000.,cold.as_secs_f64()*1000.,
        rss_before,rss_after,rss_after.saturating_sub(rss_before),alloc_before,alloc_after,alloc_after.saturating_sub(alloc_before),
        words,senders,postings,warm.as_micros(),VISITS.with(Cell::get),ids(&result).len());
}
