use super::*;

fn fixture() -> (tempfile::TempDir, Store, Vec<Person>) {
    let root = tempfile::tempdir().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let people: Vec<_> = ["a", "b", "c"]
        .into_iter()
        .map(|uid| Person {
            id: store.participant_id(uid),
            name: format!("{uid} agent"),
            session_uid: uid.into(),
            kind: "agent".into(),
            present: true,
            task: None,
        })
        .collect();
    store.enroll_participants(&people).unwrap();
    (root, store, people)
}
fn change(store: &mut Store, actor: &str, path: &str, action: &str) -> Value {
    store
        .channel_action(
            actor,
            &json!({"action":action,"path":path,"request_id":uuid()}),
            &[],
        )
        .unwrap()
}
fn send(store: &mut Store, who: &Person, people: &[Person], mut p: Value) -> Result<Value> {
    p["body"] = json!("Quick update");
    p["name"] = json!(who.name);
    if p["request_id"].is_null() {
        p["request_id"] = json!(uuid());
    }
    store.send(&who.id, &who.session_uid, "agent", &p, people)
}

#[test]
fn membership_requires_join_to_post_but_preserves_public_history_and_owner_admin() {
    let (_root, mut s, p) = fixture();
    s.create_channel(&p[0].id, &json!({"path":"work","request_id":"new"}))
        .unwrap();
    let msg = send(&mut s, &p[0], &p, json!({"channel":"work"})).unwrap();
    assert_eq!(
        send(&mut s, &p[1], &p, json!({"channel":"work"}))
            .unwrap_err()
            .code,
        "join_required"
    );
    assert_eq!(
        s.read(&p[1].id, &json!({"channel":"work"}), &p).unwrap()["items"][0]["id"],
        msg["event_id"]
    );
    assert_eq!(
        s.channel_action("owner", &json!({"action":"get","path":"work"}), &p)
            .unwrap()["can_manage"],
        true
    );
    change(&mut s, &p[1].id, "work", "join");
    send(&mut s, &p[1], &p, json!({"channel":"work"})).unwrap();
    change(&mut s, &p[1].id, "work", "leave");
    s.enroll_participants(&p).unwrap();
    assert_eq!(
        send(&mut s, &p[1], &p, json!({"channel":"work"}))
            .unwrap_err()
            .code,
        "join_required"
    );
}

#[test]
fn here_freezes_audience_retries_do_not_expand_and_ordinary_posts_stay_quiet() {
    let (_root, mut s, p) = fixture();
    s.create_channel(&p[0].id, &json!({"path":"work","request_id":"new"}))
        .unwrap();
    change(&mut s, &p[1].id, "work", "join");
    send(&mut s, &p[0], &p, json!({"channel":"work"})).unwrap();
    assert!(s.wake_intents().values().all(Vec::is_empty));
    let request = json!({"channel":"work","mention_here":true,"request_id":"broadcast"});
    let posted = send(&mut s, &p[0], &p, request.clone()).unwrap();
    let recipients = posted["event"]["data"]["mention_recipients"]
        .as_array()
        .unwrap();
    assert!(recipients.contains(&json!(p[1].id)));
    assert!(!recipients.contains(&json!(p[2].id)));
    assert_eq!(s.wake_intents()["b"].len(), 1);
    change(&mut s, &p[2].id, "work", "join");
    let retry = send(&mut s, &p[0], &p, request).unwrap();
    assert_eq!(retry["event"], posted["event"]);
    assert!(
        s.read(&p[2].id, &json!({"inbox":true}), &p).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    change(&mut s, &p[1].id, "work", "leave");
    let next = send(
        &mut s,
        &p[0],
        &p,
        json!({"channel":"work","mention_here":true}),
    )
    .unwrap();
    assert!(!next["event"]["data"]["mention_recipients"]
        .as_array()
        .unwrap()
        .contains(&json!(p[1].id)));
    let direct = send(
        &mut s,
        &p[0],
        &p,
        json!({"channel":"work","mentions":[p[1].id]}),
    )
    .unwrap();
    assert!(direct["event"]["data"]["mention_recipients"]
        .as_array()
        .unwrap()
        .contains(&json!(p[1].id)));
    assert_eq!(
        send(&mut s, &p[0], &p, json!({"dm":p[1].id,"mention_here":true}))
            .unwrap_err()
            .code,
        "invalid_mention"
    );
    s.follow(
        &p[2].id,
        &json!({"action":"set","dnd":true,"request_id":"dnd"}),
        &p,
    )
    .unwrap();
    assert!(s.wake_intents()["c"].is_empty());
}

#[test]
fn general_is_default_but_explicit_leave_survives_reconnect_and_restart() {
    let (root, mut s, p) = fixture();
    let general = s.channels["general"].clone();
    assert!(s.joined("owner", &general));
    assert!(s.joined(&p[1].id, &general));
    change(&mut s, &p[1].id, "general", "leave");
    s.enroll_participants(&p).unwrap();
    assert!(!s.joined(&p[1].id, &general));
    drop(s);
    let mut s = Store::open(root.path()).unwrap();
    s.enroll_participants(&p).unwrap();
    assert!(!s.joined(&p[1].id, &general));
    assert_eq!(
        send(&mut s, &p[1], &p, json!({"channel":"general"}))
            .unwrap_err()
            .code,
        "join_required"
    );
}

#[test]
fn legacy_membership_migrates_creator_posters_and_explicit_followers_once() {
    let (root, mut s, p) = fixture();
    let id = uuid();
    s.publish(
        "channel.create",
        None,
        "Created legacy",
        json!({"channels":[{"path":"legacy","id":id}]}),
        &p[0].id,
        "a",
        "agent",
        "legacy-create",
        "",
    )
    .unwrap();
    s.publish(
        "message.create",
        Some(&id),
        "Old post",
        json!({}),
        &p[1].id,
        "b",
        "agent",
        "legacy-post",
        "",
    )
    .unwrap();
    s.follow(
        &p[2].id,
        &json!({"action":"set","scope":{"channel":"legacy"},"inbox":true,"request_id":"follow"}),
        &p,
    )
    .unwrap();
    drop(s);
    let mut s = Store::open(root.path()).unwrap();
    assert!(s.degraded.is_none(), "{:?}", s.degraded);
    for person in &p {
        assert!(s.joined(&person.id, &id));
    }
    assert!(!s.joined("owner", &id));
    change(&mut s, &p[1].id, "legacy", "leave");
    drop(s);
    let s = Store::open(root.path()).unwrap();
    assert!(!s.joined(&p[1].id, &id));
    assert_eq!(
        s.events
            .iter()
            .filter(|e| e.event["type"] == "channel.membership.initialize"
                && e.event["data"]["channel_id"] == id)
            .count(),
        1
    );
}

#[test]
fn default_join_is_admin_controlled_and_only_applies_on_first_enrollment() {
    let (_root, mut s, p) = fixture();
    let created = s
        .create_channel(
            &p[0].id,
            &json!({"path":"welcome","allow_agent_edits":true,"request_id":"create"}),
        )
        .unwrap();
    let update = json!({"action":"update","path":"welcome","default_join":true,"expected_revision":created["channel"]["revision"],"request_id":"defaults"});
    assert_eq!(
        s.channel_action(&p[1].id, &update, &p).unwrap_err().code,
        "unauthorized"
    );
    let channel = s.channel_action(&p[0].id, &update, &p).unwrap()["channel"].clone();
    let id = channel["id"].as_str().unwrap();
    assert!(!s.joined(&p[1].id, id));
    let newcomer = s.participant_id("new");
    s.enroll_participant(&newcomer).unwrap();
    assert!(s.joined(&newcomer, id));
    change(&mut s, &newcomer, "welcome", "leave");
    s.enroll_participant(&newcomer).unwrap();
    assert!(!s.joined(&newcomer, id));
}

#[test]
fn joins_are_self_only_retries_do_not_rejoin_and_roster_matches_membership() {
    let (_root, mut s, p) = fixture();
    s.create_channel(&p[0].id, &json!({"path":"work","request_id":"create"}))
        .unwrap();
    let query = json!({"joined_only":true,"query":"work"});
    assert!(s.channel_action(&p[1].id, &query, &p).unwrap()["items"]
        .as_array()
        .unwrap()
        .is_empty());
    let join = json!({"action":"join","path":"work","request_id":"join"});
    let mut forged = join.clone();
    forged["participant_id"] = json!(p[2].id);
    assert_eq!(
        s.channel_action(&p[1].id, &forged, &p).unwrap_err().code,
        "invalid_params"
    );
    let first = s.channel_action(&p[1].id, &join, &p).unwrap();
    let id = s.channels["work"].clone();
    assert!(s.joined(&p[1].id, &id));
    assert!(!s.joined(&p[2].id, &id)); // supplied identities cannot join someone else
    assert_eq!(
        s.channel_action(&p[1].id, &query, &p).unwrap()["items"][0]["id"],
        id
    );
    let members = s
        .channel_action("owner", &json!({"action":"members","path":"work"}), &p)
        .unwrap();
    assert_eq!(members["items"].as_array().unwrap().len(), 2);
    change(&mut s, &p[1].id, "work", "leave");
    let retry = s.channel_action(&p[1].id, &join, &p).unwrap();
    assert_eq!(retry["event_id"], first["event_id"]);
    assert!(!s.joined(&p[1].id, &id));
    let mut conflict = join;
    conflict["action"] = json!("leave");
    assert_eq!(
        s.channel_action(&p[1].id, &conflict, &p).unwrap_err().code,
        "idempotency_conflict"
    );
    // Metadata should never add unread timeline entries.
    assert!(s
        .read(&p[1].id, &json!({"channel":"work","unread_only":true}), &p)
        .unwrap()["items"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn adding_members_requires_admin_even_with_open_editing_and_never_grants_admin() {
    let (_root, mut s, p) = fixture();
    let created = s
        .create_channel(
            &p[0].id,
            &json!({"path":"work","request_id":"create","allow_agent_edits":true}),
        )
        .unwrap();
    let id = s.channels["work"].clone();
    let add = json!({"action":"add_member","conversation":id,"participant_id":p[2].id,"request_id":"add"});
    let before = s.position;
    assert_eq!(
        s.channel_action(&p[1].id, &add, &p).unwrap_err().code,
        "unauthorized"
    );
    assert_eq!(s.position, before);
    let added = s.channel_action(&p[0].id, &add, &p).unwrap();
    assert!(s.joined(&p[2].id, &id));
    assert_eq!(added["event"]["actor"]["id"], p[0].id);
    assert_eq!(added["event"]["data"]["participant_id"], p[2].id);
    assert_eq!(added["membership"]["added_by"], p[0].id);
    assert_eq!(
        s.channel_action(&p[2].id, &json!({"action":"get","path":"work"}), &p)
            .unwrap()["can_manage"],
        false
    );
    assert!(s.wake_intents().values().all(Vec::is_empty));
    assert!(
        s.read(&p[2].id, &json!({"inbox":true}), &p).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        s.read(&p[2].id, &json!({"channel":"work"}), &p).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Owner administers without joining or changing the added member's role.
    assert!(!s.joined("owner", &id));
    let add_b = json!({"action":"add_member","path":"work","participant_id":p[1].id,"request_id":"owner-add"});
    s.channel_action("owner", &add_b, &p).unwrap();
    assert!(s.joined(&p[1].id, &id));
    assert!(!s.joined("owner", &id));
    let settings = s
        .channel_action(
            "owner",
            &json!({"action":"update","path":"work","admins":[p[1].id],
        "expected_revision":created["channel"]["revision"],"request_id":"delegate"}),
            &p,
        )
        .unwrap();
    change(&mut s, &p[0].id, "work", "leave");
    let add_a = json!({"action":"add_member","path":"work","participant_id":p[0].id,"request_id":"delegated-add"});
    s.channel_action(&p[1].id, &add_a, &p).unwrap();
    assert!(s.joined(&p[0].id, &id));
    s.channel_action(
        "owner",
        &json!({"action":"update","path":"work","admins":[],
        "expected_revision":settings["channel"]["revision"],"request_id":"revoke"}),
        &p,
    )
    .unwrap();
    let mut next = add_a.clone();
    next["request_id"] = json!("after-revoke");
    assert_eq!(
        s.channel_action(&p[1].id, &next, &p).unwrap_err().code,
        "unauthorized"
    );
    // An accepted request remains retrievable after the caller loses admin.
    assert_eq!(
        s.channel_action(&p[1].id, &add_a, &p).unwrap()["status"],
        "saved"
    );
}

#[test]
fn added_members_can_leave_and_old_add_retries_never_restore_membership() {
    let (root, mut s, p) = fixture();
    s.create_channel(&p[0].id, &json!({"path":"work","request_id":"create"}))
        .unwrap();
    let id = s.channels["work"].clone();
    let old = send(
        &mut s,
        &p[0],
        &p,
        json!({"channel":"work","mention_here":true}),
    )
    .unwrap();
    let request =
        json!({"action":"add_member","path":"work","participant_id":p[1].id,"request_id":"add"});
    let added = s.channel_action(&p[0].id, &request, &p).unwrap();
    assert!(!mention_recipients(&old["event"]).contains(&p[1].id.as_str()));
    assert!(s.wake_intents().get("b").is_none_or(Vec::is_empty));
    send(&mut s, &p[1], &p, json!({"channel":"work"})).unwrap();
    let next = send(
        &mut s,
        &p[0],
        &p,
        json!({"channel":"work","mention_here":true}),
    )
    .unwrap();
    assert!(mention_recipients(&next["event"]).contains(&p[1].id.as_str()));
    change(&mut s, &p[1].id, "work", "leave");
    let next = send(
        &mut s,
        &p[0],
        &p,
        json!({"channel":"work","mention_here":true}),
    )
    .unwrap();
    assert!(!mention_recipients(&next["event"]).contains(&p[1].id.as_str()));
    drop(s);
    let mut s = Store::open(root.path()).unwrap();
    assert!(s.degraded.is_none(), "{:?}", s.degraded);
    s.enroll_participants(&p).unwrap();
    let position = s.position;
    let retry = s.channel_action(&p[0].id, &request, &p).unwrap();
    assert_eq!(retry["event_id"], added["event_id"]);
    assert_eq!(retry["membership"]["joined"], true);
    assert_eq!(retry["membership"]["current_joined"], false);
    assert_eq!(s.position, position);
    assert!(!s.joined(&p[1].id, &id));
    let mut changed = request.clone();
    changed["participant_id"] = json!(p[2].id);
    assert_eq!(
        s.channel_action(&p[0].id, &changed, &p).unwrap_err().code,
        "idempotency_conflict"
    );
    changed["request_id"] = json!("new-intent");
    changed["participant_id"] = json!(p[1].id);
    s.channel_action(&p[0].id, &changed, &p).unwrap();
    assert!(s.joined(&p[1].id, &id));
}

#[test]
fn add_member_rejects_invalid_targets_and_has_no_remove_someone_else_action() {
    let (_root, mut s, p) = fixture();
    s.create_channel(&p[0].id, &json!({"path":"work","request_id":"create"}))
        .unwrap();
    let before = s.position;
    for (member, code) in [
        (Value::Null, "invalid_params"),
        (json!(42), "invalid_params"),
        (json!(""), "invalid_params"),
        (json!("system"), "not_found"),
        (json!("unknown"), "not_found"),
        (json!(p[1].name), "not_found"),
    ] {
        assert_eq!(s.channel_action(&p[0].id,
            &json!({"action":"add_member","path":"work","participant_id":member,"request_id":"bad"}),
            &p).unwrap_err().code, code);
    }
    assert_eq!(s.position, before);
    assert_eq!(
        s.channel_action(
            "owner",
            &json!({"action":"leave","path":"work","participant_id":p[0].id,
        "request_id":"remove"}),
            &p
        )
        .unwrap_err()
        .code,
        "invalid_params"
    );
    assert_eq!(
        s.channel_action(
            "owner",
            &json!({"action":"remove_member","path":"work","participant_id":p[0].id,
        "request_id":"remove"}),
            &p
        )
        .unwrap_err()
        .code,
        "unsupported_feature"
    );
    s.channel_action(
        &p[0].id,
        &json!({"action":"add_member","path":"work","participant_id":"owner",
        "request_id":"add-owner"}),
        &p,
    )
    .unwrap();
    assert!(s.joined("owner", &s.channels["work"]));
}

#[test]
fn legacy_follow_scopes_and_mutes_remain_independent_of_membership() {
    let (root, mut s, p) = fixture();
    let parent = uuid();
    let child = uuid();
    s.publish(
        "channel.create",
        None,
        "Legacy channel tree",
        json!({"channels":[{"id":parent,"path":"tree"},{"id":child,"path":"tree/child"}]}),
        "owner",
        "Owner",
        "owner",
        "tree-create",
        "",
    )
    .unwrap();
    s.follow(&p[0].id,&json!({"action":"set","scope":{"channel":"tree","include_children":true},"inbox":true,"muted":true,"request_id":"muted-follow"}),&p).unwrap();
    s.follow(
        &p[1].id,
        &json!({"action":"set","scope":{"channel":"*"},"inbox":true,"request_id":"all-follow"}),
        &p,
    )
    .unwrap();
    s.follow(&p[2].id,&json!({"action":"set","scope":{"channel":"tree"},"inbox":false,"wake":false,"request_id":"disabled-follow"}),&p).unwrap();
    drop(s);
    let mut s = Store::open(root.path()).unwrap();
    for id in [&parent, &child] {
        assert!(s.joined(&p[0].id, id));
        assert!(s.joined(&p[1].id, id));
        assert!(!s.joined(&p[2].id, id));
    }
    s.send(
        "owner",
        "",
        "owner",
        &json!({"channel":"tree/child","body":"Broadcast","mention_here":true,"request_id":"here"}),
        &p,
    )
    .unwrap();
    assert!(s.wake_intents()["a"].is_empty()); // a's inherited mute still wins
}

#[test]
fn cm_general_enrolls_every_known_participant_by_default_but_never_undoes_leaves() {
    let (root, mut s, p) = fixture();
    let id = s.channels["cm-general"].clone();
    assert_eq!(s.channel_info("cm-general", &id)["default_join"], true);
    assert!(s.joined("owner", &id));
    for person in &p {
        assert!(s.joined(&person.id, &id));
    }
    change(&mut s, &p[1].id, "cm-general", "leave");
    drop(s);
    let mut s = Store::open(root.path()).unwrap();
    s.enroll_participants(&p).unwrap();
    assert!(!s.joined(&p[1].id, &id));
    let next = s.participant_id("new-session");
    s.enroll_participant(&next).unwrap();
    assert!(s.joined(&next, &id));
}
