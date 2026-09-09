use super::*;
use crate::control::protocol::{Caller, Request};

struct Network {
    _tmp: tempfile::TempDir,
    hub: Arc<Mutex<DaemonState>>,
    clients: Vec<Arc<Mutex<DaemonState>>>,
}
fn state(root: &Path, uid: &str) -> Arc<Mutex<DaemonState>> {
    let mut state = DaemonState::default();
    state.messaging_root = root.into();
    state.tui_sessions.insert(
        uid.into(),
        serde_json::from_value(json!({"uid":uid,"label":"default"})).unwrap(),
    );
    Arc::new(Mutex::new(state))
}
fn rpc(state: &Arc<Mutex<DaemonState>>, uid: &str, method: &str, params: Value) -> Value {
    let r = super::super::rpc::dispatch(
        state,
        &Request {
            id: "sync-test".into(),
            caller: Caller::session(uid),
            method: format!("messaging.{method}"),
            params,
        },
    );
    serde_json::to_value(r).unwrap()
}
fn call(state: &Arc<Mutex<DaemonState>>, uid: &str, method: &str, params: Value) -> Value {
    let result = rpc(state, uid, method, params);
    assert!(result["error"].is_null(), "{result}");
    result["result"].clone()
}
fn wait_for(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !check() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for messaging synchronization"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn with_store<T>(state: &Arc<Mutex<DaemonState>>, f: impl FnOnce(&mut Store) -> T) -> T {
    let handle = state.lock().unwrap().messaging.clone();
    let mut slot = handle.lock().unwrap();
    f(slot.as_mut().unwrap())
}
impl Network {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let hub_root = tmp.path().join("hub");
        let hub_store = Store::open(&hub_root).unwrap();
        let descriptor = hub_store.space_descriptor();
        let config = json!({"version":1,"configured":true,"coordinator_id":descriptor["coordinator_id"],"space_id":descriptor["space_id"]});
        fs::write(
            hub_root.join("messaging-sync.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        drop(hub_store);
        let hub = state(&hub_root, "hub-agent");
        super::super::rpc::initialize(&hub).unwrap();
        fs::create_dir(hub_root.join("messaging-peers")).unwrap();
        let mut clients = Vec::new();
        for i in 0..2 {
            let root = tmp.path().join(format!("client-{i}"));
            let client = Store::provision_replica(&root, &descriptor).unwrap();
            let id = client.daemon_id.clone();
            drop(client);
            let token = uuid::Uuid::new_v4().to_string();
            fs::write(root.join("sync-token"), &token).unwrap();
            fs::write(hub_root.join("messaging-peers").join(format!("{id}.json")),serde_json::to_vec(&json!({"active":true,"owner_access":true,"token_sha256":transport::digest(token.as_bytes())})).unwrap()).unwrap();
            let mut config = config.clone();
            config["endpoint"] = json!({"kind":"unix","path":hub_root.join("messaging-sync.sock")});
            config["token_file"] = json!(root.join("sync-token"));
            fs::write(
                root.join("messaging-sync.json"),
                serde_json::to_vec(&config).unwrap(),
            )
            .unwrap();
            let client = state(&root, &format!("client-{i}"));
            super::super::rpc::initialize(&client).unwrap();
            clients.push(client);
        }
        start(&hub);
        for c in &clients {
            start(c);
        }
        let network = Self {
            _tmp: tmp,
            hub,
            clients,
        };
        for (i, c) in network.clients.iter().enumerate() {
            wait_for(|| {
                with_store(c, |s| {
                    s.sync_status()["connected"] == true && s.host_info(&s.daemon_id).is_some()
                })
            });
            call(
                c,
                &format!("client-{i}"),
                "send",
                json!({"channel":"general","name":format!("Scout-{i}"),"body":"Ready","request_id":"claim"}),
            );
        }
        network
    }
}
impl Drop for Network {
    fn drop(&mut self) {
        for s in self.clients.iter().chain(std::iter::once(&self.hub)) {
            if let Some(r) = s.lock().unwrap().messaging_sync.take() {
                r.stop();
            }
        }
    }
}
#[test]
fn messaging_sync_admin_adds_agent_on_another_host_through_normal_rpc() {
    let n = Network::new();
    let a = &n.clients[0];
    let b = &n.clients[1];
    let b_id = with_store(b, |s| s.participant_id("client-1"));
    wait_for(|| with_store(a, |s| s.remote_people().iter().any(|p| p.id == b_id)));
    let channel = call(a, "client-0", "channels",
        json!({"action":"create","path":"remote-team","request_id":"create-team"}));
    let cid = channel["channel"]["id"].as_str().unwrap().to_owned();
    wait_for(|| with_store(b, |s| s.channels().as_array().unwrap().iter().any(|c| c["path"] == "remote-team")));
    let add = json!({"action":"add_member","conversation":cid,"participant_id":b_id,"request_id":"add-team"});
    let denied = rpc(b, "client-1", "channels", add.clone());
    assert_eq!(denied["ok"], false);
    assert!(denied["error"]["message"].as_str().unwrap().contains("Only channel admins"), "{denied}");
    let accepted = call(a, "client-0", "channels", add.clone());
    assert_eq!(accepted["membership"]["participant_id"], b_id);
    wait_for(|| with_store(b, |s| s.channel_info("remote-team", &cid)["member_count"] == 2));
    assert_eq!(call(b, "client-1", "channels", json!({"action":"get","conversation":cid}))["joined"], true);
    let posted = call(b, "client-1", "send",
        json!({"channel":"remote-team","body":"Joined from the other host","mention_here":true,"request_id":"post"}));
    wait_for(|| with_store(&n.hub, |s| s.wire_event(posted["event_id"].as_str().unwrap()).is_ok()));
    call(b, "client-1", "channels", json!({"action":"leave","conversation":cid,"request_id":"leave-team"}));
    let retry = call(a, "client-0", "channels", add);
    assert_eq!(retry["event_id"], accepted["event_id"]);
    assert_eq!(retry["membership"]["current_joined"], false);
    assert_eq!(call(b, "client-1", "channels", json!({"action":"get","conversation":cid}))["joined"], false);
}

#[test]
fn messaging_sync_runtime_coordinates_claims_and_streams_offline_messages_once() {
    let n = Network::new();
    let a = &n.clients[0];
    let b = &n.clients[1];
    let b_id = with_store(b, |s| s.participant_id("client-1"));
    wait_for(|| with_store(a, |s| s.remote_people().iter().any(|p| p.id == b_id)));
    let monitor = call(
        b,
        "client-1",
        "monitor",
        json!({"scope":{"channel":"general"},"mode":"continuous","notify":"none","request_id":"watch"}),
    );
    let runtime = a.lock().unwrap().messaging_sync.take().unwrap();
    runtime.stop();
    let request = json!({"channel":"general","body":"Working offline","request_id":"offline","mentions":[b_id]});
    let sent = call(a, "client-0", "send", request.clone());
    assert_eq!(sent["replication"], "pending_sync");
    start(a);
    wait_for(|| {
        with_store(b, |s| {
            s.wire_event(sent["event_id"].as_str().unwrap()).is_ok()
        })
    });
    wait_for(|| {
        with_store(a, |s| {
            s.event_replication(sent["event_id"].as_str().unwrap())["status"] == "replicated"
        })
    });
    assert_eq!(
        call(a, "client-0", "send", request)["event_id"],
        sent["event_id"]
    );
    let read = call(
        b,
        "client-1",
        "read",
        json!({"channel":"general","freshness":"hub"}),
    );
    assert_eq!(
        read["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["id"] == sent["event_id"])
            .count(),
        1
    );
    let hits = call(
        b,
        "client-1",
        "monitors",
        json!({"action":"get","monitor_id":monitor["id"]}),
    );
    assert_eq!(
        hits["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event_id"] == sent["event_id"] || e["id"] == sent["event_id"])
            .count(),
        1,
        "{hits}"
    );
}
#[test]
fn messaging_sync_private_first_contact_does_not_leak_to_unrelated_host() {
    let n = Network::new();
    let a_id = with_store(&n.clients[0], |s| s.participant_id("client-0"));
    let sent = call(
        &n.hub,
        "hub-agent",
        "send",
        json!({"dm":a_id,"name":"Private-Scout","body":"Private first contact","request_id":"private"}),
    );
    let id = sent["event_id"].as_str().unwrap();
    wait_for(|| with_store(&n.clients[0], |s| s.wire_event(id).is_ok()));
    call(
        &n.clients[1],
        "client-1",
        "read",
        json!({"channel":"general","freshness":"hub"}),
    );
    assert!(with_store(&n.clients[1], |s| s.wire_event(id).is_err()));
    assert!(
        call(&n.clients[1], "client-1", "people", json!({}))["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "Private-Scout")
    );
}

#[test]
fn messaging_sync_owner_reads_merge_and_monitors_have_one_executor() {
    let n = Network::new();
    let a = &n.clients[0];
    let b = &n.clients[1];
    let sent = call(
        &n.hub,
        "hub-agent",
        "send",
        json!({"channel":"general","name":"Reporter","body":"For Owner","mentions":["owner"],"request_id":"owner-message"}),
    );
    let id = sent["event_id"].as_str().unwrap();
    wait_for(|| {
        with_store(a, |s| s.wire_event(id).is_ok()) && with_store(b, |s| s.wire_event(id).is_ok())
    });
    with_store(a, |s| {
        s.acknowledge(
            "owner",
            &json!({"actor":"owner","space_id":s.space_id,"ids":[id]}),
        )
        .unwrap()
    });
    wait_for(|| {
        with_store(b, |s| {
            s.read("owner", &json!({"channel":"general"}), &[]).unwrap()["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["id"] == id && m["read"] == true)
        })
    });
    let runtime = a.lock().unwrap().messaging_sync.clone().unwrap();
    let monitor=runtime.request("owner","messaging.monitor",&json!({"scope":{"channel":"general"},"mode":"continuous","notify":"badge","request_id":"owner-watch"})).unwrap();
    assert_eq!(
        monitor["execution_host"],
        with_store(&n.hub, |s| s.daemon_id.clone())
    );
    let next = call(
        b,
        "client-1",
        "send",
        json!({"channel":"general","body":"One hit","request_id":"owner-hit"}),
    );
    wait_for(|| {
        with_store(&n.hub, |s| {
            s.wire_event(next["event_id"].as_str().unwrap()).is_ok()
        })
    });
    let runtime_b = b.lock().unwrap().messaging_sync.clone().unwrap();
    let hits = runtime_b
        .request(
            "owner",
            "messaging.monitors",
            &json!({"action":"get","monitor_id":monitor["id"]}),
        )
        .unwrap();
    assert_eq!(hits["items"].as_array().unwrap().len(), 1, "{hits}");
    let cached = with_store(b, |s| s.cached_owner_snapshot().cloned());
    assert!(cached.is_some());
    let pref=runtime.request("owner","messaging.follow",&json!({"scope":{"channel":"general"},"inbox":true,"action":"set","request_id":"owner-follow"})).unwrap();
    assert_eq!(pref["status"], "saved");
    wait_for(|| {
        with_store(b, |s| {
            s.cached_owner_snapshot()
                .is_some_and(|v| v["preferences"]["revision"] == 1)
        })
    });
    let request = json!({"channel":"general","body":"Owner retry","request_id":"owner-retry"});
    let owner = with_store(a, |s| s.send("owner", "", "owner", &request, &[]).unwrap());
    wait_for(|| {
        with_store(&n.hub, |s| {
            s.wire_event(owner["event_id"].as_str().unwrap()).is_ok()
        })
    });
    let mut retry = request.clone();
    retry["origin_daemon_id"] = owner["operation"]["origin_daemon_id"].clone();
    assert_eq!(
        runtime_b
            .request("owner", "messaging.send", &retry)
            .unwrap()["event_id"],
        owner["event_id"]
    );
    retry["request_id"] = json!("unknown-origin-operation");
    assert_eq!(
        runtime_b
            .request("owner", "messaging.send", &retry)
            .unwrap_err()
            .code,
        "retry_origin_unavailable"
    );
}

#[test]
fn messaging_sync_revocation_resolves_lost_acceptance_before_rejecting_pending_children() {
    let n = Network::new();
    let a = &n.clients[0];
    let runtime = a.lock().unwrap().messaging_sync.take().unwrap();
    runtime.stop();
    let accepted = call(
        a,
        "client-0",
        "send",
        json!({"channel":"general","body":"Accepted with a lost receipt","request_id":"lost-receipt"}),
    );
    let parent = call(
        a,
        "client-0",
        "send",
        json!({"channel":"general","body":"Not accepted","request_id":"reject"}),
    );
    let child = call(
        a,
        "client-0",
        "send",
        json!({"channel":"general","body":"Dependent reply","reply_to":parent["event_id"],"request_id":"child"}),
    );
    let (host, wire) = with_store(a, |s| {
        (
            s.daemon_id.clone(),
            s.wire_event(accepted["event_id"].as_str().unwrap())
                .unwrap(),
        )
    });
    with_store(&n.hub, |s| s.accept_upload(&host, &wire).unwrap());
    with_store(&n.hub, |s| {
        s.sync_admin(&json!({"action":"revoke","host_id":host}), &[])
            .unwrap()
    });
    start(a);
    wait_for(|| {
        with_store(a, |s| {
            s.event_replication(child["event_id"].as_str().unwrap())["status"]
                == "replication_rejected"
        })
    });
    assert_eq!(
        with_store(a, |s| s
            .event_replication(accepted["event_id"].as_str().unwrap())
            ["status"]
            .clone()),
        "replicated"
    );
    assert_eq!(
        with_store(a, |s| s
            .event_replication(parent["event_id"].as_str().unwrap())
            ["decision"]["reason"]
            .clone()),
        "host_revoked"
    );
    let rejected = rpc(
        a,
        "client-0",
        "send",
        json!({"channel":"general","body":"Must stay a draft","request_id":"new-after-revoke"}),
    );
    assert!(
        rejected["error"].to_string().contains("host_revoked"),
        "{rejected}"
    );
}
#[test]
fn messaging_sync_new_name_race_and_history_barrier_preserve_origin_and_cursor() {
    let n = Network::new();
    for s in &n.clients {
        s.lock().unwrap().tui_sessions.insert(
            "same-uid".into(),
            serde_json::from_value(json!({"uid":"same-uid","label":"default"})).unwrap(),
        );
    }
    let (a, b) = std::thread::scope(|scope| {
        let left = scope.spawn(|| {
            call(
                &n.clients[0],
                "same-uid",
                "send",
                json!({"channel":"general","name":"Racer","body":"Ready","request_id":"race"}),
            )
        });
        let right = scope.spawn(|| {
            call(
                &n.clients[1],
                "same-uid",
                "send",
                json!({"channel":"general","name":"Racer","body":"Ready","request_id":"race"}),
            )
        });
        (left.join().unwrap(), right.join().unwrap())
    });
    assert_ne!(a["name"], b["name"]);
    assert_ne!(a["event"]["actor"]["id"], b["event"]["actor"]["id"]);
    assert_eq!(a["position"]["replica_id"], with_store(&n.clients[0], |s|s.daemon_id.clone()));
    call(&n.clients[0], "same-uid", "monitor", json!({"scope":{"channel":"general"},"after":a["position"],"notify":"none","request_id":"after-coordinated-send"}));
    let creator = &n.clients[0];
    let reader = &n.clients[1];
    call(
        creator,
        "client-0",
        "channels",
        json!({"action":"create","path":"later","request_id":"later"}),
    );
    let mut thread_id = Value::Null;
    for i in 0..3 {
        let sent = call(
            creator,
            "client-0",
            "send",
            json!({"channel":"later","body":format!("History {i}"),"request_id":format!("history-{i}")}),
        );
        if i == 0 { thread_id = sent["event_id"].clone(); }
    }
    wait_for(|| with_store(creator, |s| s.sync_status()["pending"] == 0));
    let thread = call(reader, "client-1", "read", json!({"thread":thread_id,"freshness":"hub"}));
    assert_eq!(thread["items"].as_array().unwrap().len(), 1, "{thread}");
    assert_eq!(thread["items"][0]["body"], "History 0");
    let p = json!({"channel":"later","freshness":"hub","limit":1});
    let first = call(reader, "client-1", "read", p.clone());
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    assert!(first["next_cursor"].is_object());
    let mut next = p.clone();
    next["cursor"] = first["next_cursor"].clone();
    let second = call(reader, "client-1", "read", next);
    assert_ne!(first["items"][0]["id"], second["items"][0]["id"]);
}

#[test]
fn messaging_sync_prioritizes_new_dm_while_bulk_page_is_in_flight() {
    use std::io::{Read, Write};
    fn send(stream: &mut std::os::unix::net::UnixStream, frame: Value) {
        let bytes = serde_json::to_vec(&frame).unwrap();
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&bytes).unwrap();
    }
    fn receive(stream: &mut std::os::unix::net::UnixStream) -> Value {
        let mut length = [0u8; 4];
        stream.read_exact(&mut length).unwrap();
        let mut bytes = vec![0u8; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut bytes).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    let n = Network::new();
    let b = &n.clients[1];
    b.lock().unwrap().messaging_sync.take().unwrap().stop();
    for i in 0..70 {
        call(
            &n.hub,
            "hub-agent",
            "send",
            json!({"channel":"general","name":"Hub","body":format!("Backlog {i}"),"request_id":format!("bulk-{i}")}),
        );
    }
    let root = n.hub.lock().unwrap().messaging_root.clone();
    let local_root = b.lock().unwrap().messaging_root.clone();
    let config = Config::load(&local_root).unwrap().unwrap();
    let token = fs::read_to_string(config.token_file.unwrap()).unwrap();
    let (host, actor, space) = with_store(b, |s| {
        (
            s.daemon_id.clone(),
            s.participant_id("client-1"),
            s.space_id.clone(),
        )
    });
    let mut socket =
        std::os::unix::net::UnixStream::connect(root.join("messaging-sync.sock")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    send(
        &mut socket,
        json!({"kind":"hello","daemon_id":host,"space_id":space,"token":token,
        "people":[{"id":actor,"name":"Scout-1","session_uid":"client-1","present":true,"kind":"agent"}],"interests":["general"],"resume":null}),
    );
    assert_eq!(receive(&mut socket)["kind"], "hello");
    let bulk = receive(&mut socket);
    assert_eq!(bulk["kind"], "batch");
    assert_eq!(bulk["page"]["complete"], false);
    with_store(b, |s| {
        for wire in bulk["page"]["items"].as_array().unwrap() {
            s.ingest_replica(wire).unwrap();
        }
    });
    let dm = call(
        &n.hub,
        "hub-agent",
        "send",
        json!({"dm":actor,"body":"Live urgent DM","request_id":"live-after-bulk"}),
    );
    send(
        &mut socket,
        json!({"kind":"ack","cursor":bulk["page"]["cursor"],"revision":bulk["revision"],"dependency_offset":bulk["page"]["dependency_offset"]}),
    );
    let live = receive(&mut socket);
    assert_eq!(live["kind"], "live", "{live}");
    assert!(live["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|wire| wire["receipt"]["event_id"] == dm["event_id"]));
    with_store(b, |s| {
        for wire in live["items"].as_array().unwrap() {
            s.ingest_replica(wire).unwrap();
        }
        let result = s
            .read(
                &actor,
                &json!({"conversation":dm["event"]["conversation_id"]}),
                &[],
            )
            .unwrap();
        assert!(result["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == dm["event_id"]));
        let backlog = s
            .read(&actor, &json!({"channel":"general","limit":200}), &[])
            .unwrap();
        assert!(!backlog["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["body"] == "Backlog 69"));
    });
    assert_eq!(
        receive(&mut socket)["kind"],
        "batch",
        "bulk still gets a turn"
    );
}
