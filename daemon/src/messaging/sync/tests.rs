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
fn messaging_sync_equivalent_views_and_barriers_preserve_inflight_progress() {
    use std::os::unix::net::UnixStream;
    use std::io::{Read, Write};
    fn send(stream: &mut UnixStream, frame: Value) {
        let bytes = serde_json::to_vec(&frame).unwrap();
        stream.write_all(&(bytes.len() as u32).to_be_bytes()).unwrap();
        stream.write_all(&bytes).unwrap();
    }
    fn receive(stream: &mut UnixStream) -> Value {
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
        call(&n.hub, "hub-agent", "send", json!({"channel":"general","name":"Hub","body":format!("Backlog {i}"),"request_id":format!("churn-{i}")}));
    }
    let root = n.hub.lock().unwrap().messaging_root.clone();
    let local_root = b.lock().unwrap().messaging_root.clone();
    let config = Config::load(&local_root).unwrap().unwrap();
    let token = fs::read_to_string(config.token_file.unwrap()).unwrap();
    let (host, actor, space) = with_store(b, |s| (s.daemon_id.clone(), s.participant_id("client-1"), s.space_id.clone()));
    let people = json!([{"id":actor,"name":"Scout-1","session_uid":"client-1","present":true,"kind":"agent"}]);
    let mut socket = UnixStream::connect(root.join("messaging-sync.sock")).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    send(&mut socket, json!({"kind":"hello","daemon_id":host,"space_id":space,"token":token,"people":people,"interests":["general"],"resume":null}));
    assert_eq!(receive(&mut socket)["kind"], "hello");
    let bulk = receive(&mut socket);
    assert_eq!(bulk["kind"], "batch");
    assert_eq!(bulk["page"]["complete"], false);
    let general = with_store(&n.hub, |s| s.channel_id_at_path("general").unwrap().to_owned());
    // Open a path view, use its id alias, then expire it. Membership already
    // covers this channel, so none may invalidate the unacknowledged page.
    for interests in [json!([general]), json!([]), json!(["general"])] {
        send(&mut socket, json!({"kind":"interests","interests":interests}));
        send(&mut socket, json!({"kind":"call","id":"barrier","actor":actor,"method":"sync.barrier","params":{"interests":["general"]},"people":people}));
        let reply = receive(&mut socket);
        assert_eq!(reply["kind"], "reply", "selector churn must not send another page: {reply}");
        assert_eq!(reply["revision"], bulk["revision"], "same scopes must keep revision: {reply}");
        assert_eq!(reply["result"]["caught_up"], true);
    }
    send(&mut socket, json!({"kind":"ack","cursor":bulk["page"]["cursor"],"revision":bulk["revision"],"dependency_offset":bulk["page"]["dependency_offset"]}));
    let next = receive(&mut socket);
    assert_eq!(next["kind"], "batch", "{next}");
    assert_eq!(next["revision"], bulk["revision"]);
    assert!(next["page"]["cursor"].as_u64().unwrap() > bulk["page"]["cursor"].as_u64().unwrap(), "history must advance after the original ACK: {next}");
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

const FEATURE: &str = transport::FEATURE_SCOPED;
fn send_frame(stream: &mut std::os::unix::net::UnixStream, frame: &Value) {
    use std::io::Write;
    let bytes = serde_json::to_vec(frame).unwrap();
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&bytes).unwrap();
}
fn receive_frame(stream: &mut std::os::unix::net::UnixStream) -> Value {
    use std::io::Read;
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).unwrap();
    let mut bytes = vec![0u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
fn runtime(state: &Arc<Mutex<DaemonState>>) -> Arc<Runtime> {
    state.lock().unwrap().messaging_sync.clone().unwrap()
}
/// Post `count` messages to a new hub channel the replicas do not follow.
fn hub_channel(n: &Network, path: &str, count: usize) -> (String, Vec<String>) {
    let channel = call(
        &n.hub,
        "hub-agent",
        "channels",
        json!({"action":"create","path":path,"request_id":format!("create-{path}")}),
    );
    let cid = channel["channel"]["id"].as_str().unwrap().to_owned();
    let ids = (0..count)
        .map(|i| {
            call(
                &n.hub,
                "hub-agent",
                "send",
                json!({"channel":path,"name":"Hub","body":format!("{path} {i}"),"request_id":format!("{path}-{i}")}),
            )["event_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    (cid, ids)
}
/// Post to #general and wait until `state` ingested it: everything the hub
/// published before it has then been through the main stream.
fn settle(n: &Network, state: &Arc<Mutex<DaemonState>>, key: &str) {
    let marker = call(
        &n.hub,
        "hub-agent",
        "send",
        json!({"channel":"general","name":"Hub","body":key,"request_id":key}),
    );
    let id = marker["event_id"].as_str().unwrap().to_owned();
    wait_for(|| with_store(state, |s| s.wire_event(&id).is_ok()));
}
fn all_present(state: &Arc<Mutex<DaemonState>>, ids: &[String]) -> bool {
    with_store(state, |s| ids.iter().all(|id| s.wire_event(id).is_ok()))
}
/// Records whether the replica's main-stream cursor ever moved backwards.
struct CursorWatch {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<bool>>,
}
impl CursorWatch {
    fn start(runtime: Arc<Runtime>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut last = runtime.stream_cursor.load(Ordering::Acquire);
            let mut regressed = false;
            while !flag.load(Ordering::Acquire) {
                let now = runtime.stream_cursor.load(Ordering::Acquire);
                regressed |= now < last;
                last = now;
                std::thread::sleep(Duration::from_millis(1));
            }
            regressed
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
    fn regressed(mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap()
    }
}

#[test]
fn messaging_sync_interest_growth_backfills_only_the_new_scope() {
    let n = Network::new();
    let b = &n.clients[1];
    let rt = runtime(b);
    assert!(rt.scoped(), "current peers negotiate scoped backfill");
    let (cid, ids) = hub_channel(&n, "big", 150);
    let (_, unrelated) = hub_channel(&n, "unrelated", 3);
    settle(&n, b, "settled-before-growth");
    assert!(with_store(b, |s| s.wire_event(&ids[0]).is_err()));
    let revision = with_store(b, |s| s.download_checkpoint()["revision"].clone());
    let before = rt.stream_cursor.load(Ordering::Acquire);
    let watch = CursorWatch::start(rt.clone());
    // `G` on one channel: a hub-freshness read. Small history arrives within
    // the barrier's bounded wait, so the read already returns it.
    let read = call(
        b,
        "client-1",
        "read",
        json!({"channel":"big","freshness":"hub","limit":200}),
    );
    assert_eq!(read["items"].as_array().unwrap().len(), 150, "{read}");
    assert!(all_present(b, &ids));
    assert!(with_store(b, |s| s.cached_coverage(&cid)["checkpoint"]["through"].is_u64()));
    settle(&n, b, "settled-after-growth");
    assert!(!watch.regressed(), "main stream cursor was reset");
    assert!(rt.stream_cursor.load(Ordering::Acquire) >= before);
    // A legacy subscription bumps the revision and replays from zero.
    assert_eq!(
        with_store(b, |s| s.download_checkpoint()["revision"].clone()),
        revision
    );
    let state = rt.backfills.lock().unwrap().get(&cid).cloned().unwrap();
    assert!(state.complete && state.total == 150 && state.done == 150, "{state:?}");
    assert_eq!(rt.backfills.lock().unwrap().len(), 1, "only the new scope");
    assert!(with_store(b, |s| unrelated.iter().all(|id| s.wire_event(id).is_err())));
}

#[test]
fn messaging_sync_released_view_never_replays_and_readding_fetches_only_the_gap() {
    let n = Network::new();
    let b = &n.clients[1];
    let rt = runtime(b);
    let (cid, mut ids) = hub_channel(&n, "big", 40);
    rpc(b, "client-1", "read", json!({"channel":"big"}));
    wait_for(|| all_present(b, &ids));
    wait_for(|| rt.backfill_status(&cid).is_none());
    let first = rt.backfills.lock().unwrap().get(&cid).cloned().unwrap();
    let revision = with_store(b, |s| s.download_checkpoint()["revision"].clone());
    let watch = CursorWatch::start(rt.clone());
    // What the 60s expiry used to do: drop the view. The hub must not replay.
    rt.views.lock().unwrap().clear();
    rt.signal.signal();
    // The client loop sends the changed interests before any queued call, so
    // the hub has processed the release once this barrier replies.
    call(b, "client-1", "read", json!({"channel":"general","freshness":"hub"}));
    for i in 0..5 {
        ids.push(
            call(
                &n.hub,
                "hub-agent",
                "send",
                json!({"channel":"big","body":format!("gap {i}"),"request_id":format!("gap-{i}")}),
            )["event_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    settle(&n, b, "after-release");
    assert!(
        with_store(b, |s| s.wire_event(&ids[40]).is_err()),
        "a released scope stops streaming"
    );
    // Viewing again backfills only what was missed, never the whole channel.
    rpc(b, "client-1", "read", json!({"channel":"big"}));
    wait_for(|| all_present(b, &ids));
    settle(&n, b, "after-readd");
    assert!(!watch.regressed(), "main stream cursor was reset");
    assert_eq!(
        with_store(b, |s| s.download_checkpoint()["revision"].clone()),
        revision
    );
    let second = rt.backfills.lock().unwrap().get(&cid).cloned().unwrap();
    assert!(second.id > first.id, "{first:?} {second:?}");
    assert_eq!(second.total, 5, "{second:?}");
}

#[test]
fn messaging_sync_join_replies_before_history_and_history_follows() {
    let n = Network::new();
    let b = &n.clients[1];
    b.lock().unwrap().messaging_sync.take().unwrap().stop();
    let (cid, ids) = hub_channel(&n, "big", 120);
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
    let (generation, position, scopes) = with_store(&n.hub, |s| {
        (
            s.generation.clone(),
            s.publication_position(),
            s.host_transport_interests(&host, &BTreeSet::new()).unwrap(),
        )
    });
    // Catch b's store up to the hub's position through the page API, then
    // resume as a caught-up scoped replica.
    let mut after = with_store(b, |s| s.download_checkpoint()["cursor"].as_u64().unwrap_or(0));
    loop {
        let page = with_store(&n.hub, |s| s.export_page(&host, &scopes, after, position).unwrap());
        with_store(b, |s| {
            for wire in page["items"].as_array().unwrap() {
                s.ingest_replica(wire).unwrap();
            }
        });
        after = page["cursor"].as_u64().unwrap();
        if page["complete"] == true {
            break;
        }
    }
    let person = json!([{"id":actor,"name":"Scout-1","session_uid":"client-1","present":true,"kind":"agent"}]);
    let mut socket =
        std::os::unix::net::UnixStream::connect(root.join("messaging-sync.sock")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    send_frame(
        &mut socket,
        &json!({"kind":"hello","daemon_id":host,"space_id":space,"token":token,"people":person,
            "interests":[],"features":[FEATURE],"resume":{"generation":generation,"cursor":position,"scopes":scopes}}),
    );
    let hello = receive_frame(&mut socket);
    assert_eq!(hello["features"], json!([FEATURE]), "{hello}");
    assert_eq!(hello["cursor"], position, "resumed, not replayed");
    send_frame(
        &mut socket,
        &json!({"kind":"call","id":"join","actor":actor,"method":"messaging.channels",
            "params":{"action":"join","conversation":cid,"request_id":"join-big"},"people":person}),
    );
    // Nothing is acknowledged before the reply: it cannot wait for history.
    let mut pending = std::collections::VecDeque::new();
    let reply = loop {
        let frame = receive_frame(&mut socket);
        if frame["kind"] == "reply" {
            break frame;
        }
        pending.push_back(frame);
    };
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(reply["backfill_started"], json!([cid]));
    assert_eq!(reply["backfills"][&cid]["total"], 120);
    assert_eq!(reply["backfills"][&cid]["complete"], false);
    let barrier = reply["barrier"].as_u64().unwrap();
    // The main stream resumes above the old cursor and brings the membership
    // event; the backfill lane brings exactly the channel's history.
    let (mut backfilled, mut main_through, mut complete) = (0, position, false);
    while !complete || main_through < barrier {
        let frame = pending
            .pop_front()
            .unwrap_or_else(|| receive_frame(&mut socket));
        match frame["kind"].as_str().unwrap() {
            "batch" => {
                let page = &frame["page"];
                assert!(page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|w| w["position"].as_u64().unwrap() > position));
                with_store(b, |s| {
                    for wire in page["items"].as_array().unwrap() {
                        s.ingest_replica(wire).unwrap();
                    }
                });
                main_through = page["cursor"].as_u64().unwrap();
                send_frame(
                    &mut socket,
                    &json!({"kind":"ack","cursor":page["cursor"],"revision":frame["revision"],"dependency_offset":page["dependency_offset"]}),
                );
            }
            "backfill" => {
                assert_eq!(frame["scope"], cid);
                let page = &frame["page"];
                with_store(b, |s| {
                    for wire in page["items"].as_array().unwrap() {
                        s.ingest_replica(wire).unwrap();
                    }
                });
                backfilled += page["items"].as_array().unwrap().len();
                complete = frame["complete"] == true;
                send_frame(
                    &mut socket,
                    &json!({"kind":"backfill_ack","scope":cid,"id":frame["id"],"cursor":page["cursor"],"dependency_offset":page["dependency_offset"]}),
                );
            }
            "backfills" | "live" | "owner_snapshot" | "pong" => {}
            other => panic!("unexpected frame {other}: {frame}"),
        }
    }
    assert_eq!(backfilled, 120, "exactly the channel's history");
    assert!(all_present(b, &ids));
}

#[test]
fn messaging_sync_join_through_replica_returns_and_history_arrives() {
    let n = Network::new();
    let b = &n.clients[1];
    let rt = runtime(b);
    let (cid, ids) = hub_channel(&n, "big", 200);
    settle(&n, b, "before-join");
    let started = Instant::now();
    let joined = call(
        b,
        "client-1",
        "channels",
        json!({"action":"join","conversation":cid,"request_id":"join-big"}),
    );
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(5), "join took {elapsed:?}");
    assert!(joined["error"].is_null(), "{joined}");
    wait_for(|| all_present(b, &ids));
    wait_for(|| rt.backfill_status(&cid).is_none());
    let read = call(b, "client-1", "read", json!({"channel":"big","limit":5}));
    assert_eq!(read["cache"]["status"], "complete_through_checkpoint", "{read}");
    assert!(read["cache"]["backfill"].is_null());
}

#[test]
fn messaging_sync_legacy_replica_keeps_single_cursor_protocol() {
    let n = Network::new();
    let b = &n.clients[1];
    b.lock().unwrap().messaging_sync.take().unwrap().stop();
    hub_channel(&n, "big", 3);
    let root = n.hub.lock().unwrap().messaging_root.clone();
    let local_root = b.lock().unwrap().messaging_root.clone();
    let config = Config::load(&local_root).unwrap().unwrap();
    let token = fs::read_to_string(config.token_file.unwrap()).unwrap();
    let (host, actor, space) = with_store(b, |s| {
        (s.daemon_id.clone(), s.participant_id("client-1"), s.space_id.clone())
    });
    let mut socket =
        std::os::unix::net::UnixStream::connect(root.join("messaging-sync.sock")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    send_frame(
        &mut socket,
        &json!({"kind":"hello","daemon_id":host,"space_id":space,"token":token,
            "people":[{"id":actor,"name":"Scout-1","session_uid":"client-1","present":true,"kind":"agent"}],
            "interests":["general"],"resume":null}),
    );
    let hello = receive_frame(&mut socket);
    assert!(hello["features"].is_null(), "no feature offered to an old peer");
    let first = receive_frame(&mut socket);
    assert_eq!(first["kind"], "batch");
    assert_eq!(first["revision"], 1);
    send_frame(
        &mut socket,
        &json!({"kind":"interests","interests":["general","big"]}),
    );
    send_frame(
        &mut socket,
        &json!({"kind":"ack","cursor":first["page"]["cursor"],"revision":1,"dependency_offset":first["page"]["dependency_offset"]}),
    );
    // Legacy semantics: an interest change restarts the stream at zero under a
    // new revision, and no scoped frames are ever sent.
    let next = loop {
        let frame = receive_frame(&mut socket);
        assert!(
            !matches!(frame["kind"].as_str(), Some("backfill" | "backfills")),
            "{frame}"
        );
        if frame["kind"] == "batch" && frame["revision"].as_u64().unwrap() > 1 {
            break frame;
        }
    };
    let first_position = next["page"]["items"][0]["position"].as_u64().unwrap();
    assert!(first_position <= 5, "replayed from zero: {first_position}");
}

#[test]
fn messaging_sync_scoped_replica_falls_back_with_a_legacy_hub() {
    use std::os::unix::net::UnixListener;
    let tmp = tempfile::tempdir().unwrap();
    let hub_store = Store::open(&tmp.path().join("hub")).unwrap();
    let descriptor = hub_store.space_descriptor();
    let generation = hub_store.generation.clone();
    let root = tmp.path().join("replica");
    drop(Store::provision_replica(&root, &descriptor).unwrap());
    let socket_path = tmp.path().join("legacy-hub.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    fs::write(root.join("sync-token"), uuid::Uuid::new_v4().to_string()).unwrap();
    let config = json!({"version":1,"configured":true,"coordinator_id":descriptor["coordinator_id"],
        "space_id":descriptor["space_id"],"endpoint":{"kind":"unix","path":socket_path},
        "token_file":root.join("sync-token")});
    fs::write(root.join("messaging-sync.json"), serde_json::to_vec(&config).unwrap()).unwrap();
    let replica = state(&root, "client");
    super::super::rpc::initialize(&replica).unwrap();
    start(&replica);
    let (mut hub, _) = listener.accept().unwrap();
    hub.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let hello = receive_frame(&mut hub);
    assert_eq!(hello["features"], json!([FEATURE]));
    // A hub from before the feature answers with the old hello shape.
    send_frame(
        &mut hub,
        &json!({"kind":"hello","coordinator_id":descriptor["coordinator_id"],"space_id":descriptor["space_id"],
            "generation":generation,"cursor":0,"revision":1}),
    );
    send_frame(
        &mut hub,
        &json!({"kind":"batch","revision":1,"scopes":[],"page":{"items":[],"cursor":0,"dependency_offset":0,
            "through":0,"complete":true,"generation":generation,"space_id":descriptor["space_id"]}}),
    );
    let ack = loop {
        let frame = receive_frame(&mut hub);
        if frame["kind"] == "ack" {
            break frame;
        }
    };
    assert_eq!(ack["revision"], 1);
    let rt = runtime(&replica);
    assert!(!rt.scoped());
    // A barrier with a legacy hub uses the old cursor/revision fence only.
    let caller = rt.clone();
    let barrier = std::thread::spawn(move || {
        caller.request_scoped(
            "owner",
            "sync.barrier",
            &json!({"interests":["general"]}),
            Some((vec!["general".into()], Duration::from_secs(5))),
        )
    });
    let request = loop {
        let frame = receive_frame(&mut hub);
        if frame["kind"] == "call" {
            break frame;
        }
    };
    let started = Instant::now();
    send_frame(
        &mut hub,
        &json!({"kind":"reply","id":request["id"],"result":{"caught_up":true},"barrier":0,"revision":1}),
    );
    assert_eq!(barrier.join().unwrap().unwrap()["caught_up"], true);
    assert!(started.elapsed() < Duration::from_secs(2));
    let stopped = replica.lock().unwrap().messaging_sync.take();
    if let Some(r) = stopped {
        r.stop();
    }
}

/// Timing probe (not a gate): join a channel with `CM_SYNC_PROBE_MESSAGES`
/// (default 6300) messages over a local socket.
#[test]
#[ignore]
fn messaging_sync_probe_large_channel_join_timing() {
    let count = std::env::var("CM_SYNC_PROBE_MESSAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6300);
    let n = Network::new();
    let b = &n.clients[1];
    let rt = runtime(b);
    let made = Instant::now();
    let (cid, ids) = hub_channel(&n, "big", count);
    eprintln!("probe: created {count} hub messages in {:?}", made.elapsed());
    settle(&n, b, "before-join");
    let started = Instant::now();
    let joined = call(
        b,
        "client-1",
        "channels",
        json!({"action":"join","conversation":cid,"request_id":"join-big"}),
    );
    let reply = started.elapsed();
    let progress = call(b, "client-1", "read", json!({"channel":"big","limit":5}));
    eprintln!(
        "probe: join replied in {reply:?}; cache {} (join result cache {})",
        progress["cache"], joined["cache"]
    );
    let deadline = Instant::now() + Duration::from_secs(600);
    while rt.backfill_status(&cid).is_some() || !all_present(b, &ids[ids.len() - 1..]) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!(
        "probe: history of {count} complete {:?} after join",
        started.elapsed()
    );
    assert!(all_present(b, &ids));
}
