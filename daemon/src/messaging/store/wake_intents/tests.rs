use super::*;
use std::cell::Cell;
use std::time::{Duration, Instant};

thread_local! {
    static CANDIDATES: Cell<usize> = const { Cell::new(0) };
    static ORACLE_VISITS: Cell<usize> = const { Cell::new(0) };
}
pub(super) fn count_candidate() {
    CANDIDATES.with(|c| c.set(c.get() + 1));
}
pub(super) fn count_oracle() {
    ORACLE_VISITS.with(|c| c.set(c.get() + 1));
}

// Small deterministic generator keeps failing seeds reproducible, without a new
// production/dev dependency. Choices include states the RPC no longer creates
// but that can still exist in retained personal files.
struct Rng(u64);
impl Rng {
    fn n(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as usize % n
    }
}
fn channel(s: &mut Store, path: &str, cid: &str, key: &str) {
    s.reduce(
        &json!({"id":key,"type":"channel.create","actor":{"id":"owner"},
        "request":{"key":key},"data":{"channels":[{"path":path,"id":cid}]}}),
    )
    .unwrap();
}
fn monitor(id: &str, scope: Scope, after: u64, high: u64) -> Monitor {
    Monitor {
        id: id.into(),
        task_subscription: None,
        scope,
        mode: "continuous".into(),
        notify: "wake".into(),
        include_self: false,
        created_at: now(),
        expires_at: None,
        state: "active".into(),
        start: after,
        scanned: high,
        hit_high: high,
        hits: 1,
        acknowledged: after,
        closing_fence: None,
    }
}
fn publish(s: &mut Store, mut event: Value) -> String {
    s.position += 1;
    let id = format!("fixture:{:08}", s.position);
    event["id"] = json!(id);
    s.push_published(Published {
        event,
        position: s.position,
        received_at: now(),
        event_sha256: String::new(),
    });
    id
}
fn scope(rng: &mut Rng, cids: &[String], actors: &[String], ids: &[String]) -> Scope {
    let mut s = match rng.n(9) {
        0 => Scope {
            channel: Some("*".into()),
            ..Scope::default()
        },
        1 => Scope {
            channel: Some("work".into()),
            include_children: rng.n(2) == 0,
            ..Scope::default()
        },
        2 => Scope {
            channel: Some(format!("work/{}", rng.n(4))),
            ..Scope::default()
        },
        3 => Scope {
            conversation: Some(cids[rng.n(cids.len())].clone()),
            ..Scope::default()
        },
        4 => Scope {
            dms: true,
            ..Scope::default()
        },
        5 => Scope {
            peer: Some(actors[rng.n(actors.len())].clone()),
            ..Scope::default()
        },
        6 => Scope {
            thread: Some(ids[rng.n(ids.len())].clone()),
            ..Scope::default()
        },
        7 => Scope {
            channel: Some("missing".into()),
            ..Scope::default()
        },
        _ => Scope::default(),
    };
    if rng.n(4) == 0 {
        s.thread = Some(ids[rng.n(ids.len())].clone());
    }
    s
}
fn assert_actor(s: &Store, actor: &str, context: &str) {
    let actual = s.wake_intents_for_actor(actor);
    assert_eq!(actual, s.wake_intents_oracle(actor), "{context}: {actor}");
    // Exercise the per-event delivery-status entry point as well as enumeration.
    let mut one_at_a_time = Vec::new();
    for e in &s.events {
        s.event_wake_intents(actor, s.personal.get(actor), e, &mut one_at_a_time);
    }
    assert_eq!(actual, one_at_a_time, "per-event: {context}: {actor}");
}

#[test]
fn p4_randomized_differential_intent_keys_and_order() {
    for seed in 1..=48 {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let mut rng = Rng(seed);
        let actors: Vec<_> = ["a", "b", "c", "d"]
            .map(|uid| s.participant_id(uid))
            .into_iter()
            .chain(["owner".into()])
            .collect();
        let mut cids = Vec::new();
        for i in 0..6 {
            let path = if i == 4 {
                "work".into()
            } else if i == 5 {
                "workish".into()
            } else {
                format!("work/{i}")
            };
            let cid = format!("public-{i}");
            channel(&mut s, &path, &cid, &format!("channel-{i}"));
            cids.push(cid);
        }
        for (i, members) in [vec![0, 1], vec![1, 2, 3], vec![0, 4], vec![2, 3]]
            .iter()
            .enumerate()
        {
            let cid = format!("dm-{i}");
            s.conversations.insert(
                cid.clone(),
                members.iter().map(|j| actors[*j].clone()).collect(),
            );
            cids.push(cid);
        }
        // Unknown conversation traffic must never become visible via a follow.
        cids.push("unknown".into());
        let mut ids = Vec::new();
        for i in 0..320 {
            let cid = &cids[rng.n(cids.len())];
            let sender = &actors[rng.n(actors.len())];
            let mention = &actors[rng.n(actors.len())];
            let mut data = json!({"mentions": [mention]});
            if rng.n(3) == 0 {
                // Frozen @here differs from both direct mentions and today's
                // membership, including an explicitly empty frozen audience.
                data["mention_here"] = json!(true);
                data["mention_recipients"] = if rng.n(3) == 0 {
                    json!([])
                } else {
                    json!([actors[rng.n(actors.len())], actors[rng.n(actors.len())]])
                };
            }
            if !ids.is_empty() && rng.n(3) == 0 {
                data["thread_root"] = json!(ids[rng.n(ids.len())]);
            }
            let id = publish(
                &mut s,
                json!({"type":if rng.n(12)==0 {"message.edit"} else {"message.create"},
                "actor":{"id":sender},"conversation_id":cid,"body":"fixture",
                "logical_time":rng.n(240).to_string(),"origin_daemon_id":"remote", "data":data}),
            );
            if rng.n(15) == 0 {
                s.replication
                    .rejections
                    .insert(id.clone(), json!({"reason":"rejected"}));
            }
            if rng.n(2) == 0 {
                s.replication
                    .receipts
                    .insert(id.clone(), json!({"position":(900+i).to_string()}));
            }
            ids.push(id);
        }
        for actor in &actors {
            let mut p = s.personal_state(actor);
            for i in 0..12 {
                let scope = scope(&mut rng, &cids, &actors, &ids);
                p.preferences.rules.insert(
                    format!("rule-{i:02}"),
                    Follow {
                        scope,
                        inbox: rng.n(2) == 0,
                        wake: rng.n(2) == 0,
                        muted: rng.n(9) == 0,
                        since: rng.n(350) as u64,
                        hub_since: Some(rng.n(1000) as u64),
                        wake_since: if rng.n(2) == 0 {
                            Some(rng.n(350) as u64)
                        } else {
                            None
                        },
                    },
                );
            }
            for i in 0..14 {
                let mut m = monitor(
                    &format!("monitor-{i:02}"),
                    scope(&mut rng, &cids, &actors, &ids),
                    rng.n(350) as u64,
                    rng.n(350) as u64,
                );
                m.state =
                    ["active", "matched", "expired", "cancelled", "dismissed"][rng.n(5)].into();
                m.notify = ["wake", "wake", "badge", "none"][rng.n(4)].into();
                m.include_self = rng.n(2) == 0;
                if i < 5 {
                    let task = format!("task-{actor}-{i}");
                    m.id = format!("task-{task}-1");
                    m.task_subscription = Some(task.clone());
                    if i != 0 {
                        s.task_bindings.insert(
                            task.clone(),
                            TaskBinding {
                                id: task,
                                task_id: "t".into(),
                                channel_id: cids[0].clone(),
                                actor: if i == 1 {
                                    "other".into()
                                } else {
                                    actor.clone()
                                },
                                revision: if i == 2 { 2 } else { 1 },
                                active: i != 3,
                                acknowledged: ids.iter().step_by(3).cloned().collect(),
                            },
                        );
                    }
                }
                p.monitors.insert(m.id.clone(), m);
            }
            s.personal.insert(actor.clone(), p);
            for cid in &cids {
                if rng.n(3) == 0 {
                    s.reads.entry(actor.clone()).or_default().cursors.insert(
                        cid.clone(),
                        ReadCursor {
                            logical_time: rng.n(240) as u64,
                            event_id: ids[rng.n(ids.len())].clone(),
                        },
                    );
                }
            }
        }
        for actor in &actors {
            assert_actor(&s, actor, &format!("seed {seed}"));
        }
        // Membership churn cannot reinterpret frozen mentions. DND suppresses
        // both direct and monitor intents, and toggling it restores the same set.
        s.memberships.clear();
        for actor in &actors {
            let before = s.wake_intents_for_actor(actor);
            s.personal.get_mut(actor).unwrap().preferences.dnd = true;
            assert_actor(&s, actor, "dnd");
            assert!(s.wake_intents_for_actor(actor).is_empty());
            s.personal.get_mut(actor).unwrap().preferences.dnd = false;
            assert_eq!(before, s.wake_intents_for_actor(actor));
        }
    }
}

#[test]
fn p4_channel_reverse_index_preserves_alias_replacement_and_partial_reduce() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    for (path, cid) in [("z", "one"), ("a", "one"), ("a", "two"), ("z", "two")] {
        channel(&mut s, path, cid, &format!("{path}-{cid}"));
        for id in ["one", "two", "missing"] {
            assert_eq!(
                s.channel_path_of(id),
                s.channels
                    .iter()
                    .find_map(|(p, c)| (c == id).then_some(p.as_str()))
            );
            assert_eq!(
                s.visible("stranger", id),
                s.channels.values().any(|c| c == id)
            );
        }
    }
    assert!(s
        .reduce(&json!({"id":"partial","data":{"channels":[
        {"path":"early","id":"three"},{"path":"../bad","id":"bad"}]}}))
        .is_err());
    assert_eq!(s.channel_path_of("three"), Some("early"));
    assert!(!s.visible("stranger", "bad"));
}

#[test]
fn p4_default_path_visits_only_unread_dm_and_frozen_mentions() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    channel(&mut s, "busy", "busy", "create");
    let actor = s.participant_id("a");
    for _ in 0..2000 {
        publish(
            &mut s,
            json!({"type":"message.create","actor":{"id":"other"},
        "conversation_id":"busy","logical_time":"1","data":{}}),
        );
    }
    let yes = publish(
        &mut s,
        json!({"type":"message.create","actor":{"id":"other"},
        "conversation_id":"busy","logical_time":"2","data":{"mention_recipients":[actor]}}),
    );
    publish(
        &mut s,
        json!({"type":"message.create","actor":{"id":"other"},
        "conversation_id":"busy","logical_time":"3","data":{"mentions":[actor],"mention_recipients":[]}}),
    );
    CANDIDATES.with(|c| c.set(0));
    let got = s.wake_intents_for_actor(&actor);
    assert_eq!(
        got.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
        [yes.as_str()]
    );
    assert_eq!(CANDIDATES.with(Cell::get), 1);
    assert_actor(&s, &actor, "default candidates");
    let mut p = s.personal_state(&actor);
    let scope = Scope {
        channel: Some("*".into()),
        ..Scope::default()
    };
    p.preferences.rules.insert(
        "all".into(),
        Follow {
            scope: scope.clone(),
            inbox: true,
            wake: true,
            muted: false,
            since: 0,
            hub_since: None,
            wake_since: None,
        },
    );
    p.monitors
        .insert("all".into(), monitor("all", scope, 0, s.position));
    s.personal.insert(actor.clone(), p);
    s.reads.entry(actor.clone()).or_default().cursors.insert(
        "busy".into(),
        ReadCursor {
            logical_time: u64::MAX,
            event_id: String::new(),
        },
    );
    CANDIDATES.with(|c| c.set(0));
    assert!(s.wake_intents_for_actor(&actor).is_empty());
    assert_eq!(
        CANDIDATES.with(Cell::get),
        0,
        "fully read broad scopes must not scan history"
    );
    assert_actor(&s, &actor, "fully read follow and monitor");
}

#[test]
#[ignore = "manual before/after microbenchmark; run with --ignored --nocapture"]
fn p4_wake_microbenchmark_39_recipients() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    let actors: Vec<_> = (0..39)
        .map(|i| s.participant_id(&format!("recipient-{i}")))
        .collect();
    for c in 0..60 {
        channel(
            &mut s,
            &format!("work/{c}"),
            &format!("public-{c}"),
            &format!("create-{c}"),
        );
    }
    for (i, actor) in actors.iter().enumerate() {
        s.conversations
            .insert(format!("dm-{i}"), vec![actor.clone(), "sender".into()]);
    }
    let mut rng = Rng(73421);
    for i in 0..40_000 {
        let mut data = json!({});
        if i % 200 == 1 {
            data["mention_recipients"] = json!([actors[rng.n(actors.len())]]);
        }
        let cid = if i % 40 == 0 {
            format!("dm-{}", rng.n(39))
        } else {
            format!("public-{}", rng.n(60))
        };
        publish(
            &mut s,
            json!({"type":"message.create","actor":{"id":"sender"},
            "conversation_id":cid,"logical_time":i.to_string(),"data":data,"body":"retained message"}),
        );
    }
    // Live inventory: 34 recipients have no extra scopes; five have 123
    // retained wake monitors. Add a wake follow to each of those five too.
    for (i, actor) in actors.iter().take(5).enumerate() {
        let mut p = s.personal_state(actor);
        for m in 0..if i < 3 { 25 } else { 24 } {
            let scope = Scope {
                channel: Some(format!("work/{}", (i * 11 + m) % 60)),
                ..Scope::default()
            };
            let monitor = monitor(&format!("m-{m:02}"), scope, 39_000, 39_900);
            p.monitors.insert(monitor.id.clone(), monitor);
        }
        p.preferences.rules.insert(
            "follow".into(),
            Follow {
                scope: Scope {
                    channel: Some(format!("work/{i}")),
                    ..Scope::default()
                },
                inbox: true,
                wake: true,
                muted: false,
                since: 39_000,
                hub_since: None,
                wake_since: None,
            },
        );
        s.personal.insert(actor.clone(), p);
    }
    let fast = || {
        actors
            .iter()
            .map(|a| s.wake_intents_for_actor(a))
            .collect::<Vec<_>>()
    };
    let old = || {
        actors
            .iter()
            .map(|a| s.wake_intents_oracle(a))
            .collect::<Vec<_>>()
    };
    CANDIDATES.with(|c| c.set(0));
    let expected = fast();
    let candidates = CANDIDATES.with(Cell::get);
    ORACLE_VISITS.with(|c| c.set(0));
    assert_eq!(expected, old());
    let visits = ORACLE_VISITS.with(Cell::get);
    let time = |f: &dyn Fn() -> Vec<Vec<WakeIntent>>| {
        let start = Instant::now();
        std::hint::black_box(f());
        start.elapsed()
    };
    let mut before: Vec<Duration> = Vec::new();
    let mut after = Vec::new();
    for round in 0..7 {
        if round % 2 == 0 {
            before.push(time(&old));
            after.push(time(&fast));
        } else {
            after.push(time(&fast));
            before.push(time(&old));
        }
    }
    before.sort();
    after.sort();
    println!("P4_BENCH channels=60 messages=40000 recipients=39 scoped_recipients=5 monitors=123 iterations=7 before_median_ms={:.3} after_median_ms={:.3} speedup={:.2} before_visits={} after_candidates={} intents={} arrivals_extra_bytes={}",
        before[3].as_secs_f64()*1000.,after[3].as_secs_f64()*1000.,before[3].as_secs_f64()/after[3].as_secs_f64(),
        visits,candidates,expected.iter().map(Vec::len).sum::<usize>(),40_000*std::mem::size_of::<usize>());
    assert!(
        candidates < visits / 100,
        "default traffic must not be evaluated per recipient"
    );
}

#[test]
fn p4_boundaries_sticky_mutes_monitor_states_and_task_acknowledgments() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path()).unwrap();
    channel(&mut s, "work", "public", "create");
    let actor = s.participant_id("a");
    let post = |s: &mut Store, author: &str, logical: &str, data: Value| {
        publish(
            s,
            json!({
        "type":"message.create","actor":{"id":author},"logical_time":logical,
        "conversation_id":"public","data":data}),
        )
    };
    let first = post(&mut s, "other", "100", json!({"mentions":[actor]}));
    let boundary = s.position;
    let late = post(&mut s, "other", "1", json!({}));
    post(&mut s, &actor, "101", json!({"mentions":[actor]}));
    let last = post(&mut s, "other", "102", json!({}));
    let high = s.position;
    let muted = post(&mut s, "other", "103", json!({"mentions":[actor]}));
    let channel_scope = Scope {
        channel: Some("work".into()),
        ..Scope::default()
    };
    let mut p = s.personal_state(&actor);
    p.preferences.rules.insert(
        "follow".into(),
        Follow {
            scope: channel_scope.clone(),
            inbox: false,
            wake: true,
            muted: false,
            since: high,
            wake_since: Some(boundary + 1),
            hub_since: Some(u64::MAX),
        },
    );
    for (id, state) in [
        ("a", "expired"),
        ("b", "matched"),
        ("cancel", "cancelled"),
        ("dismiss", "dismissed"),
    ] {
        let mut m = monitor(id, channel_scope.clone(), boundary, high);
        m.state = state.into();
        m.include_self = true;
        p.monitors.insert(id.into(), m);
    }
    let mut m = monitor("task-t-1", channel_scope.clone(), boundary, high);
    m.task_subscription = Some("t".into());
    p.monitors.insert(m.id.clone(), m);
    s.task_bindings.insert(
        "t".into(),
        TaskBinding {
            id: "t".into(),
            task_id: "t".into(),
            channel_id: "public".into(),
            actor: actor.clone(),
            revision: 1,
            active: true,
            acknowledged: BTreeSet::from([late.clone()]),
        },
    );
    // A mute cannot be undone by a later, equally specific unmuted follow.
    let thread_scope = Scope {
        thread: Some(muted),
        ..channel_scope
    };
    for (key, mute) in [("thread-a", true), ("thread-z", false)] {
        p.preferences.rules.insert(
            key.into(),
            Follow {
                scope: thread_scope.clone(),
                inbox: true,
                wake: true,
                muted: mute,
                since: 0,
                hub_since: None,
                wake_since: None,
            },
        );
    }
    s.personal.insert(actor.clone(), p);
    let keys = |s: &Store| {
        s.wake_intents_for_actor(&actor)
            .into_iter()
            .map(|i| i.key)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        keys(&s),
        vec![
            first.clone(),
            format!("monitor:a:{late}"),
            format!("monitor:b:{late}"),
            last.clone(),
            format!("monitor:a:{last}"),
            format!("monitor:b:{last}"),
            format!("monitor:task-t-1:{last}")
        ]
    );
    assert_actor(&s, &actor, "explicit boundaries");
    s.reads.entry(actor.clone()).or_default().cursors.insert(
        "public".into(),
        ReadCursor {
            logical_time: 100,
            event_id: first,
        },
    );
    assert_eq!(
        keys(&s),
        vec![
            last.clone(),
            format!("monitor:a:{last}"),
            format!("monitor:b:{last}"),
            format!("monitor:task-t-1:{last}")
        ]
    );
    assert_actor(&s, &actor, "late arrival behind read cursor");
}

#[test]
fn p4_replication_live_before_bulk_frozen_here_and_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut hub = Store::open(&tmp.path().join("hub")).unwrap();
    let mut replica =
        Store::provision_replica(&tmp.path().join("replica"), &hub.space_descriptor()).unwrap();
    let people: Vec<_> = ["a", "b"]
        .map(|uid| Person {
            id: replica.participant_id(uid),
            name: uid.into(),
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
        hub.coordinate(&host,&p.id,"messaging.send",&json!({"channel":"general",
        "body":"ready","name":format!("Scout-{}",p.session_uid),"request_id":format!("claim-{}",p.session_uid)})).unwrap();
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
    let actor = &people[1].id;
    let start = hub.position;
    replica
        .register_monitor(
            actor,
            &json!({"scope":{"channel":"general"},"mode":"continuous",
        "notify":"wake","request_id":"watch"}),
            &people,
        )
        .unwrap();
    replica
        .follow(
            actor,
            &json!({"scope":{"channel":"general"},"wake":true,
        "inbox":true,"request_id":"follow","action":"set"}),
            &people,
        )
        .unwrap();
    for i in 0..8 {
        hub.coordinate(
            &host,
            &people[0].id,
            "messaging.send",
            &json!({"channel":"general",
        "body":"backlog","request_id":format!("backlog-{i}")}),
        )
        .unwrap();
    }
    let live_start = hub.position;
    let here = hub
        .coordinate(
            &host,
            &people[0].id,
            "messaging.send",
            &json!({"channel":"general",
        "body":"frozen here","mention_here":true,"request_id":"here"}),
        )
        .unwrap();
    let dm = hub
        .coordinate(
            &host,
            &people[0].id,
            "messaging.send",
            &json!({"dm":actor,"body":"dm","request_id":"dm"}),
        )
        .unwrap();
    let live = hub
        .export_live(&host, &BTreeSet::new(), start, live_start, hub.position)
        .unwrap();
    replica
        .ingest_replica_page(live["items"].as_array().unwrap())
        .unwrap();
    replica.advance_monitors_at(Utc::now()).unwrap();
    assert_actor(&replica, actor, "sparse live ingest");
    let before = replica.wake_intents_for_actor(actor);
    assert!(before
        .iter()
        .any(|i| i.key == here["event_id"].as_str().unwrap()));
    assert!(before
        .iter()
        .any(|i| i.key == dm["event_id"].as_str().unwrap()));
    hub.coordinate(
        &host,
        actor,
        "messaging.channels",
        &json!({"action":"leave","path":"general","request_id":"leave"}),
    )
    .unwrap();
    sync(&hub, &mut replica);
    replica.advance_monitors_at(Utc::now()).unwrap();
    assert_actor(&replica, actor, "bulk catches up after live and leave");
    assert!(replica
        .wake_intents_for_actor(actor)
        .iter()
        .any(|i| i.key == here["event_id"].as_str().unwrap()));
    replica
        .acknowledge(
            actor,
            &json!({"actor":actor,"space_id":replica.space_id,"ids":[here["event_id"]]}),
        )
        .unwrap();
    assert_actor(&replica, actor, "read cursor crosses late bulk");
    let expected = replica.wake_intents_for_actor(actor);
    let root = tmp.path().join("replica");
    drop(replica);
    let replica = Store::open(&root).unwrap();
    assert!(replica.degraded.is_none(), "{:?}", replica.degraded);
    assert_actor(&replica, actor, "restart rebuild");
    assert_eq!(replica.wake_intents_for_actor(actor), expected);
}
