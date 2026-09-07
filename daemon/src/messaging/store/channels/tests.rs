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
fn update(
    s: &mut Store,
    actor: &str,
    id: &str,
    fields: Value,
    people: &[Person],
    key: &str,
) -> Result<Value> {
    let c = s.channel_action(actor, &json!({"action":"get","conversation":id}), people)?;
    let mut p = fields;
    p["action"] = json!("update");
    p["conversation"] = json!(id);
    p["expected_revision"] = c["revision"].clone();
    p["request_id"] = json!(key);
    s.channel_action(actor, &p, people)
}
fn message(s: &mut Store, actor: &Person, target: Value, key: &str, people: &[Person]) -> Value {
    let mut p = target;
    p["body"] = json!(key);
    p["request_id"] = json!(key);
    p["name"] = json!(format!("{} agent", actor.name));
    s.send(&actor.id, &actor.session_uid, "agent", &p, people)
        .unwrap()
}
fn pin(
    s: &mut Store,
    actor: &str,
    conv: &str,
    message: &str,
    action: &str,
    key: &str,
    people: &[Person],
) -> Result<Value> {
    s.pins(
        actor,
        &json!({"action":action,"conversation":conv,"message_id":message,
        "expected_revision":s.pins_revision(conv,s.position),"request_id":key}),
        people,
    )
}

#[test]
fn messaging_channel_admin_defaults_open_editing_and_owner_override_survive_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    let a = person(&s, "creator");
    let b = person(&s, "editor");
    let people = vec![a.clone(), b.clone()];
    let created = s.channel_action(&a.id,&json!({"action":"create","path":"work/parser","description":"Parser handoffs","request_id":"create"}),&people).unwrap();
    let id = created["channel"]["id"].as_str().unwrap().to_owned();
    let original_rev = created["channel"]["revision"].clone();
    assert_eq!(created["channel"]["created_by"], a.id);
    assert_eq!(created["channel"]["allow_agent_edits"], false);
    let parent = s
        .channel_action(&b.id, &json!({"action":"get","path":"work"}), &people)
        .unwrap();
    assert_eq!(parent["created_by"], a.id);
    assert_eq!(parent["can_edit"], false);
    assert_eq!(
        update(
            &mut s,
            &b.id,
            &id,
            json!({"name":"Hijacked"}),
            &people,
            "denied"
        )
        .unwrap_err()
        .code,
        "unauthorized"
    );
    // Restricting metadata never restricts ordinary conversation.
    let sent = message(
        &mut s,
        &b,
        json!({"conversation":id}),
        "A normal message",
        &people,
    );
    let event_id = sent["event_id"].as_str().unwrap();
    assert_eq!(
        pin(&mut s, &b.id, &id, event_id, "set", "bad-pin", &people)
            .unwrap_err()
            .code,
        "unauthorized"
    );
    update(
        &mut s,
        &a.id,
        &id,
        json!({"allow_agent_edits":true}),
        &people,
        "open",
    )
    .unwrap();
    update(
        &mut s,
        &b.id,
        &id,
        json!({"name":"Parser review","description":"Current review"}),
        &people,
        "rename",
    )
    .unwrap();
    pin(&mut s, &b.id, &id, event_id, "set", "pin", &people).unwrap();
    assert_eq!(
        update(
            &mut s,
            &b.id,
            &id,
            json!({"admins":[b.id]}),
            &people,
            "self-promote"
        )
        .unwrap_err()
        .code,
        "unauthorized"
    );
    update(
        &mut s,
        &a.id,
        &id,
        json!({"allow_agent_edits":false,"admins":[b.id]}),
        &people,
        "appoint",
    )
    .unwrap();
    update(
        &mut s,
        &b.id,
        &id,
        json!({"description":"Admin edit"}),
        &people,
        "admin-edit",
    )
    .unwrap();
    update(
        &mut s,
        "owner",
        &id,
        json!({"admins":[]}),
        &people,
        "remove",
    )
    .unwrap();
    assert_eq!(
        update(
            &mut s,
            &b.id,
            &id,
            json!({"description":"No access"}),
            &people,
            "revoked"
        )
        .unwrap_err()
        .code,
        "unauthorized"
    );
    let conflict = s.channel_action("owner",&json!({"action":"update","conversation":id,"name":"Stale","expected_revision":original_rev,"request_id":"stale"}),&people).unwrap();
    assert_eq!(conflict["status"], "conflict");
    drop(s);
    let mut s = Store::open(tmp.path()).unwrap();
    assert!(s.degraded.is_none(), "{:?}", s.degraded);
    let c = s
        .channel_action(
            &a.id,
            &json!({"action":"get","path":"work/parser"}),
            &people,
        )
        .unwrap();
    assert_eq!(c["name"], "Parser review");
    assert_eq!(c["id"], id);
    assert_eq!(c["can_manage"], false);
    assert_eq!(c["admins"], json!([]));
    assert_eq!(
        update(
            &mut s,
            &a.id,
            &id,
            json!({"description":"Former creator edit"}),
            &people,
            "creator-revoked"
        )
        .unwrap_err()
        .code,
        "unauthorized"
    );
    let pins = s.pins(&b.id, &json!({"conversation":id}), &people).unwrap();
    assert_eq!(pins["items"][0]["id"], event_id);
    assert_eq!(pins["items"][0]["actor"]["id"], b.id);
    assert_eq!(pins["items"][0]["pin"]["actor"]["id"], b.id);
    pin(
        &mut s,
        "owner",
        &id,
        event_id,
        "remove",
        "owner-unpin",
        &people,
    )
    .unwrap();
}

#[test]
fn messaging_channel_pins_snapshot_retry_and_no_notifications_or_unread_activity() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    let a = person(&s, "a");
    let b = person(&s, "b");
    let people = vec![a.clone(), b.clone()];
    let c = s
        .create_channel(&a.id, &json!({"path":"pins","request_id":"create"}))
        .unwrap();
    let id = c["channel"]["id"].as_str().unwrap();
    let first = message(&mut s, &a, json!({"conversation":id}), "first", &people);
    let second = message(&mut s, &a, json!({"conversation":id}), "second", &people);
    let read = s.read(&b.id, &json!({"conversation":id}), &people).unwrap();
    s.acknowledge(&b.id, &read["receipt"]).unwrap();
    let watch = s
        .register_monitor(
            &b.id,
            &json!({"scope":{"channel":"pins"},"request_id":"watch"}),
            &people,
        )
        .unwrap();
    let p = json!({"action":"set","conversation":id,"message_id":first["event_id"],"expected_revision":id,"request_id":"first-pin"});
    let accepted = s.pins(&a.id, &p, &people).unwrap();
    pin(
        &mut s,
        &a.id,
        id,
        second["event_id"].as_str().unwrap(),
        "set",
        "second-pin",
        &people,
    )
    .unwrap();
    let page = s
        .pins(&b.id, &json!({"conversation":id,"limit":1}), &people)
        .unwrap();
    assert!(!page["next_cursor"].is_null());
    pin(
        &mut s,
        &a.id,
        id,
        second["event_id"].as_str().unwrap(),
        "remove",
        "unpin",
        &people,
    )
    .unwrap();
    assert_eq!(
        s.pins(&a.id, &p, &people).unwrap()["event_id"],
        accepted["event_id"]
    );
    let next = s
        .pins(
            &b.id,
            &json!({"conversation":id,"limit":1,"cursor":page["next_cursor"]}),
            &people,
        )
        .unwrap();
    assert_eq!(next["items"][0]["id"], second["event_id"]);
    assert_eq!(next["items"][0]["pinned"], true);
    assert_eq!(next["revision"], page["revision"]);
    assert!(s
        .read(
            &b.id,
            &json!({"conversation":id,"unread_only":true}),
            &people
        )
        .unwrap()["items"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        s.monitors(&b.id, &json!({"action":"get","monitor_id":watch["id"]}))
            .unwrap()["monitor"]["hits"],
        0
    );
    assert_eq!(
        s.read(&a.id, &json!({"conversation":id}), &people).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let bad = json!({"action":"set","conversation":id,"message_id":first["event_id"],"expected_revision":id,"request_id":"stale"});
    assert_eq!(s.pins(&a.id, &bad, &people).unwrap()["status"], "conflict");
}

#[test]
fn messaging_channel_legacy_creator_is_recovered_and_dm_pins_do_not_leak() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    let a = person(&s, "a");
    let b = person(&s, "b");
    let people = vec![a.clone(), b.clone()];
    let id = uuid();
    s.publish(
        "channel.create",
        None,
        "Created legacy channel",
        json!({"channels":[{"id":id,"path":"legacy","description":"Original"}]}),
        &a.id,
        "A",
        "agent",
        "legacy",
        "",
    )
    .unwrap();
    drop(s);
    let mut s = Store::open(tmp.path()).unwrap();
    let old = s
        .channel_action(&a.id, &json!({"action":"get","path":"legacy"}), &people)
        .unwrap();
    assert_eq!(old["created_by"], a.id);
    assert_eq!(old["can_manage"], true);
    assert_eq!(
        s.channel_action(&b.id, &json!({"action":"get","path":"general"}), &people)
            .unwrap()["can_edit"],
        false
    );
    let dm = message(&mut s, &a, json!({"dm":b.id}), "Private", &people);
    let cid = dm["event"]["conversation_id"].as_str().unwrap();
    let mid = dm["event_id"].as_str().unwrap();
    pin(&mut s, &b.id, cid, mid, "set", "dm-pin", &people).unwrap();
    assert_eq!(
        s.pins("owner", &json!({"conversation":cid}), &people)
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(
        pin(
            &mut s,
            "owner",
            cid,
            mid,
            "remove",
            "owner-private",
            &people
        )
        .unwrap_err()
        .code,
        "not_found"
    );
    assert_eq!(
        pin(
            &mut s,
            &a.id,
            &id,
            mid,
            "set",
            "cross-conversation",
            &people
        )
        .unwrap_err()
        .code,
        "not_found"
    );
    assert!(s
        .pins(&a.id, &json!({"conversation":cid}), &people)
        .unwrap()["items"][0]["pinned"]
        .as_bool()
        .unwrap());
}
