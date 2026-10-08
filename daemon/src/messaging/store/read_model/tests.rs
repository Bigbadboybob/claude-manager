use super::*;
fn setup(root: &Path) -> (Store, Person, Person) {
    let mut s = Store::open(root).unwrap();
    let people = ["a", "b"].map(|uid| Person {
        id: s.participant_id(uid),
        name: uid.into(),
        session_uid: uid.into(),
        task: None,
        present: true,
        kind: "agent".into(),
    });
    for p in &people {
        s.send(&p.id,&p.session_uid,"agent",&json!({"channel":"general","body":"hello","name":format!("Scout-{}",p.session_uid),"request_id":"claim"}),&people).unwrap();
    }
    (s, people[0].clone(), people[1].clone())
}
fn post(s: &mut Store, p: &Person, key: &str, extra: Value) -> Value {
    let mut params = json!({"channel":"general","body":key,"request_id":key});
    params
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    s.send(&p.id, &p.session_uid, "agent", &params, &[p.clone()])
        .unwrap()["event"]
        .clone()
}
#[test]
fn migration_keeps_legacy_file_and_collapses_gaps_to_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, a, _) = setup(tmp.path());
    let first = post(&mut s, &a, "first", json!({}));
    let _gap = post(&mut s, &a, "gap", json!({}));
    let last = post(&mut s, &a, "last", json!({}));
    let legacy = s
        .root
        .join("_state")
        .join(format!("{}.json", hash(b"owner")));
    let bytes = serde_json::to_vec(&json!({"ids":[first["id"],last["id"]]})).unwrap();
    fs::write(&legacy, &bytes).unwrap();
    drop(s);
    let mut s = Store::open(tmp.path()).unwrap();
    assert!(s.degraded.is_none(), "{:?}", s.degraded);
    assert_eq!(
        s.counts("owner").unwrap()["conversations"][strv(&last, "conversation_id")]["unread"],
        0
    );
    let next = post(&mut s, &a, "next", json!({}));
    s.mark_read(
        "owner",
        &json!({"channel":"general","through":next["id"]}),
        &[],
    )
    .unwrap();
    assert_eq!(fs::read(&legacy).unwrap(), bytes);
    drop(s);
    let mut s = Store::open(tmp.path()).unwrap();
    assert_eq!(s.counts("owner").unwrap()["unread"], 0);
    assert_eq!(
        s.read_cursor("owner", strv(&last, "conversation_id"))["event_id"],
        next["id"]
    );
}
#[test]
fn direct_reads_advance_but_preview_inbox_and_filters_do_not() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, a, b) = setup(tmp.path());
    let one = post(&mut s, &a, "one", json!({"mentions":[b.id]}));
    let two = post(&mut s, &a, "two", json!({"mentions":[b.id]}));
    let cid = strv(&one, "conversation_id");
    let before = s.unread_counts(&b.id, cid);
    for q in [
        json!({"inbox":true}),
        json!({"channel":"general","mark_read":false}),
        json!({"channel":"general","time":{"since":"1h"}}),
    ] {
        s.read(&b.id, &q, &[]).unwrap();
        assert_eq!(s.unread_counts(&b.id, cid), before);
    }
    let page = s
        .read(
            &b.id,
            &json!({"channel":"general","newest_first":true,"limit":1}),
            &[],
        )
        .unwrap();
    assert_eq!(page["items"][0]["id"], two["id"]);
    assert_eq!(page["read_cursor"]["event_id"], two["id"]);
    assert_eq!(s.unread_counts(&b.id, cid), (0, 0));
    let tail = post(&mut s, &a, "tail", json!({}));
    s.acknowledge(
        &b.id,
        &json!({"actor":b.id,"space_id":s.space_id,"ids":[one["id"]]}),
    )
    .unwrap();
    assert!(!s.is_read(&b.id, &tail));
    assert_eq!(s.unread_counts(&b.id, cid), (1, 0));
}
#[test]
fn counts_match_legacy_prefix_and_bulk_mentions_stop_at_last_mention() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, a, b) = setup(tmp.path());
    for i in 0..30 {
        post(
            &mut s,
            &a,
            &format!("m{i}"),
            if i % 3 == 0 {
                json!({"mentions":[b.id]})
            } else {
                json!({})
            },
        );
    }
    let cid = s.channels["general"].clone();
    let messages: Vec<_> = s.messages_in(&cid).map(|e| e.event.clone()).collect();
    for prefix in [0, 7, 15, 28] {
        if prefix > 0 {
            s.mark_read(
                &b.id,
                &json!({"conversation":cid,"through":messages[prefix-1]["id"]}),
                &[],
            )
            .unwrap();
        }
        let expected = messages
            .iter()
            .skip(prefix)
            .filter(|e| e["actor"]["id"] != b.id)
            .count();
        assert_eq!(s.unread_counts(&b.id, &cid).0, expected);
    }
    let result = s.mark_read(&b.id, &json!({"mentions":true}), &[]).unwrap();
    assert_eq!(result["counts"]["conversations"][&cid]["mentions"], 0);
    assert_eq!(result["counts"]["conversations"][&cid]["unread"], 2);
    let all = s.mark_read(&b.id, &json!({"all":true}), &[]).unwrap();
    assert_eq!(all["counts"]["conversations"][&cid]["unread"], 0);
    assert_eq!(
        s.mark_read(&b.id, &json!({"all":true}), &[]).unwrap()["changed"],
        false
    );
}
#[test]
fn reactions_toggle_idempotently_without_wakes_and_read_mentions() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, a, b) = setup(tmp.path());
    let e = post(&mut s, &a, "mention", json!({"mentions":[b.id]}));
    let p = json!({"message_id":e["id"],"emoji":"✅","request_id":"react"});
    let first = s.react(&b.id, &p).unwrap();
    let position = s.position;
    assert_eq!(s.react(&b.id, &p).unwrap()["event_id"], first["event_id"]);
    assert_eq!(s.position, position);
    assert_eq!(first["reactions"]["✅"]["mine"], true);
    assert_eq!(first["reactions"]["✅"]["count"], 1);
    assert_eq!(
        s.reaction_summary(&a.id, strv(&e, "id"))["✅"]["mine"],
        false
    );
    assert!(s.is_read(&b.id, &e));
    assert!(s
        .wake_intents_for_event(strv(&first, "event_id"))
        .is_empty());
    let removed = s
        .react(
            &b.id,
            &json!({"message_id":e["id"],"emoji":"✅","remove":true,"request_id":"remove"}),
        )
        .unwrap();
    assert_eq!(removed["reactions"], json!({}));
    assert_eq!(
        s.react(
            &b.id,
            &json!({"message_id":e["id"],"emoji":"👀","request_id":"react"})
        )
        .unwrap_err()
        .code,
        "idempotency_conflict"
    );
    assert_eq!(
        s.react(
            &b.id,
            &json!({"message_id":e["id"],"emoji":"no","request_id":"bad"})
        )
        .unwrap_err()
        .code,
        "invalid_reaction"
    );
    drop(s);
    let mut s = Store::open(tmp.path()).unwrap();
    assert!(s.degraded.is_none(), "{:?}", s.degraded);
    assert_eq!(s.react(&b.id, &p).unwrap()["event_id"], first["event_id"]);
    assert_eq!(s.reaction_summary(&b.id, strv(&e, "id")), json!({}));
}
fn sync(hub: &Store, replica: &mut Store) {
    let mut after = 0;
    loop {
        let page = hub
            .export_page(
                &replica.daemon_id,
                &BTreeSet::from(["*".into()]),
                after,
                hub.position,
            )
            .unwrap();
        for wire in page["items"].as_array().unwrap() {
            replica.ingest_replica(wire).unwrap();
        }
        after = page["cursor"].as_u64().unwrap();
        if page["complete"] == true {
            break;
        }
    }
}
#[test]
fn owner_cursor_and_reactions_replicate_across_different_arrival_positions() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut hub, a, _) = setup(&tmp.path().join("hub"));
    let mut replica =
        Store::provision_replica(&tmp.path().join("replica"), &hub.space_descriptor()).unwrap();
    hub.register_host(&replica.daemon_id, true, true, &[])
        .unwrap();
    let e = post(&mut hub, &a, "for-owner", json!({"mentions":["owner"]}));
    sync(&hub, &mut replica);
    // Replica-local position differs because owner cursor publications arrive
    // independently; the cursor uses the canonical message order instead.
    let before = replica.position;
    replica
        .mark_read("owner", &json!({"channel":"general"}), &[])
        .unwrap();
    assert_eq!(replica.position, before + 1);
    let cursor = replica.events.last().unwrap().event.clone();
    assert_eq!(cursor["type"], "read.cursor");
    let wire = replica.wire_event(strv(&cursor, "id")).unwrap();
    let receipt = hub.accept_upload(&replica.daemon_id, &wire).unwrap();
    replica.record_receipt(&receipt).unwrap();
    assert_eq!(
        hub.read_cursor("owner", strv(&e, "conversation_id")),
        replica.read_cursor("owner", strv(&e, "conversation_id"))
    );
    assert_eq!(hub.counts("owner").unwrap()["mentions"], 0);
    let result = replica
        .react(
            "owner",
            &json!({"message_id":e["id"],"emoji":"👀","request_id":"seen"}),
        )
        .unwrap();
    hub.accept_upload(
        &replica.daemon_id,
        &replica.wire_event(strv(&result, "event_id")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        hub.reaction_summary("owner", strv(&e, "id"))["👀"]["mine"],
        true
    );
    drop(replica);
    let mut reopened = Store::open(&tmp.path().join("replica")).unwrap();
    assert!(reopened.degraded.is_none(), "{:?}", reopened.degraded);
    assert_eq!(reopened.counts("owner").unwrap()["mentions"], 0);
}
#[test]
fn legacy_ack_before_backfill_advances_when_message_arrives() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, a, _) = setup(tmp.path());
    let e = post(&mut s, &a, "pending", json!({}));
    let mut future = e.clone();
    future["id"] = json!(format!("{}:{}", s.daemon_id, uuid()));
    future["logical_time"] = json!((s.clock + 10).to_string());
    future["request"]["key"] = json!("future");
    let ack = json!({"type":"read.ack","actor":{"id":"owner","kind":"owner"},"data":{"ids":[future["id"]]},"conversation_id":null});
    s.reduce_private_sync(&ack).unwrap();
    assert!(!s.is_read("owner", &future));
    s.commit(future.clone()).unwrap();
    assert!(s.is_read("owner", &future));
}

#[test]
fn migration_keeps_unavailable_legacy_anchors_until_later_backfill() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, a, _) = setup(tmp.path());
    let mut event = post(&mut s, &a, "template", json!({}));
    event["id"] = json!(format!("{}:{}", s.daemon_id, uuid()));
    event["request"]["key"] = json!("later");
    event["logical_time"] = json!((s.clock + 10).to_string());
    let legacy = s
        .root
        .join("_state")
        .join(format!("{}.json", hash(b"owner")));
    fs::write(
        legacy,
        serde_json::to_vec(&json!({"ids":[event["id"]]})).unwrap(),
    )
    .unwrap();
    drop(s);
    // First open writes migration marker but cannot resolve the anchor yet.
    drop(Store::open(tmp.path()).unwrap());
    let mut s = Store::open(tmp.path()).unwrap();
    s.commit(event.clone()).unwrap();
    assert!(s.is_read("owner", &event));
}
#[test]
fn cursor_and_reaction_authorization_and_owner_export_filter() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut hub, a, b) = setup(&tmp.path().join("hub"));
    let mut replica =
        Store::provision_replica(&tmp.path().join("replica"), &hub.space_descriptor()).unwrap();
    hub.register_host(&replica.daemon_id, true, false, &[])
        .unwrap();
    let dm = hub
        .send(
            &a.id,
            &a.session_uid,
            "agent",
            &json!({"dm":b.id,"body":"private","request_id":"private"}),
            &[a.clone(), b.clone()],
        )
        .unwrap()["event"]
        .clone();
    assert_eq!(
        hub.react(
            "owner",
            &json!({"message_id":dm["id"],"emoji":"✅","request_id":"no"})
        )
        .unwrap_err()
        .code,
        "not_found"
    );
    assert_eq!(
        hub.mark_read("owner", &json!({"conversation":dm["conversation_id"]}), &[])
            .unwrap_err()
            .code,
        "not_found"
    );
    hub.replication.enabled = true;
    let e = post(&mut hub, &a, "public", json!({}));
    hub.mark_read("owner", &json!({"channel":"general"}), &[])
        .unwrap();
    let cursor = hub.events.last().unwrap().event.clone();
    let mut forged = cursor.clone();
    forged["data"]["cursors"][strv(&e, "conversation_id")]["logical_time"] =
        json!(u64::MAX.to_string());
    assert_eq!(
        hub.validate_cursor_event(&forged, true).unwrap_err().code,
        "unauthorized"
    );
    // Public history never transports Owner read boundaries to non-Owner hosts.
    sync(&hub, &mut replica);
    assert!(!replica.has_event(strv(&cursor, "id")));
}
#[test]
fn large_channel_mark_and_counts_have_constant_publication_cost() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, a, _) = setup(tmp.path());
    let template = post(&mut s, &a, "template", json!({"mentions":["owner"]}));
    for i in 0..30_000 {
        let mut e = template.clone();
        s.clock += 1;
        s.position += 1;
        e["id"] = json!(format!(
            "{}:{:08x}-0000-4000-8000-000000000000",
            s.daemon_id, i
        ));
        e["logical_time"] = json!(s.clock.to_string());
        s.push_published(Published {
            event: e,
            position: s.position,
            received_at: now(),
            event_sha256: String::new(),
        });
    }
    s.replication.enabled = true;
    let before = s.events.len();
    let began = std::time::Instant::now();
    let page = s
        .read(
            "owner",
            &json!({"channel":"general","newest_first":true,"limit":20,"mark_read":false}),
            &[],
        )
        .unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 20);
    assert_eq!(s.counts("owner").unwrap()["mentions"], 30_001);
    let result = s
        .mark_read("owner", &json!({"channel":"general"}), &[])
        .unwrap();
    assert_eq!(result["counts"]["mentions"], 0);
    assert_eq!(s.events.len(), before + 1);
    assert_eq!(s.events.last().unwrap().event["type"], "read.cursor");
    assert_eq!(
        s.mark_read("owner", &json!({"channel":"general"}), &[])
            .unwrap()["changed"],
        false
    );
    assert_eq!(s.events.len(), before + 1);
    for _ in 0..100 {
        assert_eq!(s.counts("owner").unwrap()["mentions"], 0);
    }
    eprintln!(
        "B4 30k-message newest page + mark + 100 counts: {:?}",
        began.elapsed()
    );
    assert!(began.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn reactions_merge_by_event_order_when_delivery_is_reversed() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut hub, a, b) = setup(&tmp.path().join("hub"));
    let mut replica =
        Store::provision_replica(&tmp.path().join("replica"), &hub.space_descriptor()).unwrap();
    hub.register_host(&replica.daemon_id, true, true, &[])
        .unwrap();
    let e = post(&mut hub, &a, "react-target", json!({}));
    sync(&hub, &mut replica);
    let added = hub
        .react(
            &b.id,
            &json!({"message_id":e["id"],"emoji":"👍","request_id":"add"}),
        )
        .unwrap();
    let removed = hub
        .react(
            &b.id,
            &json!({"message_id":e["id"],"emoji":"👍","request_id":"remove","remove":true}),
        )
        .unwrap();
    replica
        .ingest_replica(&hub.wire_event(strv(&removed, "event_id")).unwrap())
        .unwrap();
    replica
        .ingest_replica(&hub.wire_event(strv(&added, "event_id")).unwrap())
        .unwrap();
    assert_eq!(replica.reaction_summary(&b.id, strv(&e, "id")), json!({}));
}

#[test]
fn failed_cursor_checkpoint_does_not_claim_a_successful_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut s, a, b) = setup(tmp.path());
    let e = post(&mut s, &a, "unread", json!({}));
    s.load_read(&b.id).unwrap();
    let path = s.read_path(&b.id);
    mkdir(path.parent().unwrap()).unwrap();
    mkdir(&path).unwrap();
    assert_eq!(
        s.mark_read(&b.id, &json!({"channel":"general"}), &[])
            .unwrap_err()
            .code,
        "outcome_unknown"
    );
    assert!(!s.is_read(&b.id, &e));
    assert_eq!(
        s.mark_read(&b.id, &json!({"channel":"general"}), &[])
            .unwrap_err()
            .code,
        "store_read_only"
    );
}
