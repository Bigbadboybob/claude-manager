use super::*;
fn person(s: &Store, uid: &str) -> Person {
    Person {
        id: s.participant_id(uid),
        name: uid.into(),
        session_uid: uid.into(),
        task: None,
        present: true,
        kind: "agent".into(),
    }
}
fn post(s: &mut Store, p: &Person, target: Value, key: &str, people: &[Person]) -> Value {
    let mut q = target;
    q["body"] = json!(key);
    q["request_id"] = json!(key);
    q["name"] = json!(format!("{} builder", p.name));
    s.send(&p.id, &p.session_uid, "agent", &q, people).unwrap()
}
#[test]
fn messaging_monitors_first_contact_privacy_restart_and_receipt_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    let a = person(&s, "a");
    let b = person(&s, "b");
    let c = person(&s, "c");
    let people = vec![a.clone(), b.clone(), c.clone()];
    let before = s.position_token(s.position);
    let one = post(&mut s, &a, json!({"dm":b.id}), "one", &people);
    let p = json!({"scope":{"dm":a.id},"mode":"continuous","after":before,"request_id":"register"});
    let m = s.register_monitor(&b.id, &p, &people).unwrap();
    assert_eq!(m["hits"], 1);
    assert_eq!(
        s.register_monitor(&b.id, &p, &people).unwrap()["id"],
        m["id"]
    );
    assert_eq!(
        s.monitors(&c.id, &json!({"action":"get","monitor_id":m["id"]}))
            .unwrap_err()
            .code,
        "not_found"
    );
    assert!(s.register_monitor(&c.id,&json!({"scope":{"conversation":one["event"]["conversation_id"]},"request_id":"private"}),&people).is_err());
    post(&mut s, &a, json!({"dm":b.id}), "two", &people);
    let q = json!({"action":"get","monitor_id":m["id"],"limit":1,"unacknowledged_only":true});
    let first = s.monitors(&b.id, &q).unwrap();
    assert!(!first["next_cursor"].is_null());
    let mut invalid_page = q.clone();
    invalid_page["cursor"] = first["next_cursor"].clone();
    invalid_page["cursor"]["last_position"] = json!(0);
    assert_eq!(
        s.monitors(&b.id, &invalid_page).unwrap_err().code,
        "invalid_cursor"
    );
    invalid_page = q.clone();
    invalid_page["cursor"] = first["next_cursor"].clone();
    invalid_page["unacknowledged_only"] = json!(false);
    assert_eq!(
        s.monitors(&b.id, &invalid_page).unwrap_err().code,
        "invalid_cursor"
    );
    post(&mut s, &a, json!({"dm":b.id}), "three", &people);
    let mut forged = first["receipt"].clone();
    forged["through"] = json!(s.position);
    assert!(s
        .monitors(
            &b.id,
            &json!({"action":"ack","monitor_id":m["id"],"receipt":forged,"request_id":"forged"})
        )
        .is_err());
    s.monitors(&b.id,&json!({"action":"ack","monitor_id":m["id"],"receipt":first["receipt"],"request_id":"ack1"})).unwrap();
    let mut next = q.clone();
    next["cursor"] = first["next_cursor"].clone();
    let second = s.monitors(&b.id, &next).unwrap();
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    s.monitors(&b.id,&json!({"action":"ack","monitor_id":m["id"],"receipt":second["receipt"],"request_id":"ack2"})).unwrap();
    assert_eq!(s.dms(&b.id, true).unwrap()["items"][0]["unread"], 3);
    drop(s);
    let mut s = Store::open(tmp.path()).unwrap();
    let result = s.monitors(&b.id, &q).unwrap();
    assert_eq!(result["monitor"]["unacknowledged"], 1);
    assert_eq!(result["items"][0]["preview"], "three");
}
#[test]
fn messaging_monitors_once_descendants_threads_expiry_and_tombstones() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    let a = person(&s, "a");
    let people = vec![a.clone()];
    s.create_channel(
        "owner",
        &json!({"action":"create","path":"work/sub","request_id":"channel"}),
    )
    .unwrap();
    let m = s
        .register_monitor(
            "owner",
            &json!({"scope":{"channel":"work","include_children":true},"request_id":"once"}),
            &people,
        )
        .unwrap();
    s.channel_action(&a.id, &json!({"action":"join","path":"work/sub","request_id":"join"}), &people).unwrap();
    let root = post(&mut s, &a, json!({"channel":"work/sub"}), "root", &people);
    let thread=s.register_monitor("owner",&json!({"scope":{"thread":root["event_id"]},"mode":"continuous","expires_in":"10m","request_id":"thread"}),&people).unwrap();
    post(
        &mut s,
        &a,
        json!({"channel":"work/sub","reply_to":root["event_id"]}),
        "reply",
        &people,
    );
    post(
        &mut s,
        &a,
        json!({"channel":"work/sub"}),
        "unrelated",
        &people,
    );
    let first = s
        .monitors("owner", &json!({"action":"get","monitor_id":m["id"]}))
        .unwrap();
    assert_eq!(first["monitor"]["hits"], 1);
    assert_eq!(first["monitor"]["state"], "matched");
    s.advance_monitors_at(Utc::now() + chrono::Duration::minutes(11))
        .unwrap();
    post(
        &mut s,
        &a,
        json!({"channel":"work/sub","reply_to":root["event_id"]}),
        "late",
        &people,
    );
    let hit = s
        .monitors("owner", &json!({"action":"get","monitor_id":thread["id"]}))
        .unwrap();
    assert_eq!(hit["monitor"]["hits"], 1);
    assert_eq!(hit["monitor"]["state"], "expired");
    let dismiss = json!({"action":"dismiss","monitor_id":thread["id"],"request_id":"dismiss"});
    s.monitors("owner", &dismiss).unwrap();
    drop(s);
    let mut s = Store::open(tmp.path()).unwrap();
    s.monitors("owner", &dismiss).unwrap();
    assert!(s
        .monitors("owner", &json!({"action":"get","monitor_id":thread["id"]}))
        .is_err());
}
#[test]
fn messaging_follows_are_quiet_by_default_start_now_and_hard_mute_monitors() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    let a = person(&s, "a");
    let b = person(&s, "b");
    let people = vec![a.clone(), b.clone()];
    post(&mut s, &a, json!({"channel":"general"}), "old", &people);
    let follow = json!({"action":"set","scope":{"channel":"general"},"inbox":true,"wake":true,"request_id":"follow"});
    s.follow(&b.id, &follow, &people).unwrap();
    s.follow("owner", &follow, &people).unwrap();
    post(&mut s, &a, json!({"channel":"general"}), "new", &people);
    assert_eq!(
        s.read("owner", &json!({"inbox":true}), &people).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(s.wake_intents().get("owner").is_none());
    assert_eq!(
        s.follow("owner", &json!({"scope":{"channel":"general"}}), &people)
            .unwrap()["effective"]["wake"],
        false
    );
    assert_eq!(s.wake_intents()["b"].len(), 1);
    let m = s
        .register_monitor(
            &b.id,
            &json!({"scope":{"channel":"general"},"request_id":"watch"}),
            &people,
        )
        .unwrap();
    let dnd = json!({"action":"set","dnd":true,"expected_revision":1,"request_id":"dnd"});
    s.follow(&b.id, &dnd, &people).unwrap();
    post(&mut s, &a, json!({"channel":"general"}), "muted", &people);
    assert!(s.wake_intents()["b"].is_empty());
    assert_eq!(
        s.monitors(&b.id, &json!({"action":"get","monitor_id":m["id"]}))
            .unwrap()["monitor"]["hits"],
        1
    );
    let conflict = s
        .follow(
            &b.id,
            &json!({"action":"set","dnd":false,"expected_revision":1,"request_id":"stale"}),
            &people,
        )
        .unwrap();
    assert_eq!(conflict["status"], "conflict");
    drop(s);
    let mut s = Store::open(tmp.path()).unwrap();
    assert_eq!(s.follow(&b.id, &json!({}), &people).unwrap()["dnd"], true);
}
#[test]
fn messaging_monitor_scope_validation_limits_and_cancel_all() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    assert!(s
        .register_monitor(
            "owner",
            &json!({"scope":{"channel":12},"request_id":"bad"}),
            &[]
        )
        .is_err());
    for n in 0..64 {
        s.register_monitor(
            "owner",
            &json!({"scope":{"dms":true},"request_id":format!("m{n}")}),
            &[],
        )
        .unwrap();
    }
    assert_eq!(
        s.register_monitor(
            "owner",
            &json!({"scope":{"dms":true},"request_id":"overflow"}),
            &[]
        )
        .unwrap_err()
        .code,
        "monitor_limit"
    );
    assert_eq!(
        s.monitors(
            "owner",
            &json!({"action":"cancel_all","request_id":"cancel"})
        )
        .unwrap()["cancelled"],
        64
    );
    s.register_monitor(
        "owner",
        &json!({"scope":{"dms":true},"request_id":"again"}),
        &[],
    )
    .unwrap();
}
#[test]
fn messaging_first_dm_monitor_and_owner_bell_claim_survive_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    let a = person(&s, "a");
    let b = person(&s, "b");
    let people = vec![a.clone(), b.clone()];
    let m = s
        .register_monitor(
            &b.id,
            &json!({"scope":{"dm":a.id},"request_id":"first"}),
            &people,
        )
        .unwrap();
    assert!(s.conversations.is_empty());
    post(&mut s, &a, json!({"dm":b.id}), "first contact", &people);
    assert_eq!(
        s.monitors(&b.id, &json!({"action":"get","monitor_id":m["id"]}))
            .unwrap()["monitor"]["hits"],
        1
    );
    post(&mut s, &a, json!({"dm":"owner"}), "owner old", &people);
    assert_eq!(s.attention("owner", true).unwrap()["ring"], false);
    s.follow(
        "owner",
        &json!({"action":"set","bell":true,"request_id":"bell"}),
        &people,
    )
    .unwrap();
    assert_eq!(s.attention("owner", true).unwrap()["ring"], false);
    post(&mut s, &a, json!({"dm":"owner"}), "owner new", &people);
    assert_eq!(s.attention("owner", true).unwrap()["ring"], true);
    drop(s);
    let mut s = Store::open(tmp.path()).unwrap();
    assert_eq!(s.attention("owner", true).unwrap()["ring"], false);
    assert_eq!(s.attention("owner", false).unwrap()["unread"], 2);
}

#[test]
fn messaging_group_dms_are_canonical_private_and_survive_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    let a = person(&s, "a"); let b = person(&s, "b"); let c = person(&s, "c"); let outsider = person(&s, "outsider");
    let people = vec![a.clone(), b.clone(), c.clone(), outsider.clone()];
    assert!(s.read(&a.id, &json!({"dm":[b.id,c.id]}), &people).unwrap()["items"].as_array().unwrap().is_empty());
    assert!(s.dms(&a.id,false).unwrap()["items"].as_array().unwrap().is_empty());
    let first = post(&mut s,&a,json!({"dm":[c.id,b.id]}),"group first",&people);
    let cid = first["event"]["conversation_id"].clone();
    let retry = post(&mut s,&a,json!({"dm":[c.id,b.id]}),"group first",&people);
    assert_eq!(first["event"]["id"],retry["event"]["id"]);
    let reply = post(&mut s,&b,json!({"dm":[a.id,c.id]}),"group reply",&people);
    assert_eq!(reply["event"]["conversation_id"],cid);
    assert_eq!(s.dms(&c.id,true).unwrap()["items"][0]["unread"],2);
    let listing=s.dms_page(&a.id,&json!({"peer":c.id})).unwrap();
    assert_eq!(listing["items"][0]["members"].as_array().unwrap().len(),3);
    assert_eq!(listing["items"][0]["peer"],Value::Null);
    assert!(s.dms(&outsider.id,false).unwrap()["items"].as_array().unwrap().is_empty());
    assert!(s.read(&outsider.id,&json!({"conversation":cid}),&people).is_err());
    assert!(s.read("owner",&json!({"conversation":cid}),&people).is_err());
    assert!(s.send(&outsider.id,"outsider","agent",&json!({"conversation":cid,"body":"intrude","request_id":"intrude","name":"Other"}),&people).is_err());
    assert!(s.send(&a.id,"a","agent",&json!({"conversation":cid,"body":"mention","request_id":"mention","mentions":[outsider.id]}),&people).is_err());
    let pair=post(&mut s,&a,json!({"dm":b.id}),"pair only",&people);
    assert_ne!(cid,pair["event"]["conversation_id"]);
    let wakes=s.wake_intents();
    for uid in ["b","c"] { assert!(wakes[uid].iter().any(|w| w.event_id == first["event"]["id"].as_str().unwrap())); }
    assert!(!wakes.contains_key("outsider"));
    drop(s);
    let mut restored=Store::open(tmp.path()).unwrap();
    assert!(restored.degraded.is_none(),"{:?}",restored.degraded);
    assert_eq!(restored.read(&c.id,&json!({"conversation":cid}),&people).unwrap()["items"].as_array().unwrap().len(),2);
    for target in [json!([]),json!([a.id]),json!([b.id,b.id]),json!([b.id,42]),json!(["missing"])] {
        assert!(restored.send(&a.id,"a","agent",&json!({"dm":target,"body":"invalid","request_id":"invalid"}),&people).is_err());
    }
}

#[test]
fn messaging_group_monitors_and_mutes_do_not_spill_into_other_dms() {
    let tmp=tempfile::tempdir().unwrap(); let mut s=Store::open(tmp.path()).unwrap();
    let a=person(&s,"a");let b=person(&s,"b");let c=person(&s,"c");
    let people=vec![a.clone(),b.clone(),c.clone()];
    let first=post(&mut s,&a,json!({"dm":[b.id,c.id]}),"start",&people);
    let cid=first["event"]["conversation_id"].clone();
    let group=s.register_monitor(&b.id,&json!({"scope":{"conversation":cid},"mode":"continuous","request_id":"group-watch"}),&people).unwrap();
    let pair=s.register_monitor(&b.id,&json!({"scope":{"dm":a.id},"mode":"continuous","request_id":"pair-watch"}),&people).unwrap();
    let all=s.register_monitor(&b.id,&json!({"scope":{"dms":true},"mode":"continuous","request_id":"all-watch"}),&people).unwrap();
    post(&mut s,&a,json!({"dm":b.id}),"pair",&people);
    post(&mut s,&c,json!({"conversation":cid}),"group",&people);
    for (monitor,count) in [(&group,1),(&pair,1),(&all,2)] {
        assert_eq!(s.monitors(&b.id,&json!({"action":"get","monitor_id":monitor["id"]})).unwrap()["monitor"]["hits"],count);
    }
    s.follow(&b.id,&json!({"action":"set","scope":{"conversation":cid},"muted":true,"request_id":"mute"}),&people).unwrap();
    let group_msg=post(&mut s,&c,json!({"conversation":cid}),"muted group",&people);
    let pair_msg=post(&mut s,&a,json!({"dm":b.id}),"unmuted pair",&people);
    let wakes=s.wake_intents();
    assert!(!wakes["b"].iter().any(|w| w.event_id == group_msg["event"]["id"].as_str().unwrap()));
    assert!(wakes["b"].iter().any(|w| w.event_id == pair_msg["event"]["id"].as_str().unwrap()));
}
