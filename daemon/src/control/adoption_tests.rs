// Included in methods::tests to share the live-child and routed-API fixtures.
const ADOPT_TASK: &str = "4bfa326a-f086-4a3d-9d24-ce13bd9f5867";

fn adoption_fixture() -> (Arc<Mutex<DaemonState>>, Value, StubApi) {
    let (state, params, stub, _) = adoption_fixture_remote();
    (state, params, stub)
}
fn adoption_fixture_remote() -> (Arc<Mutex<DaemonState>>, Value, StubApi, Arc<Mutex<Value>>) {
    let state = make_state_arc();
    let wt = seed_mcp_spawn_fixture(&state, "ts-adopt", Some(ADOPT_TASK));
    let remote = Arc::new(Mutex::new(
        json!({"task_id":ADOPT_TASK,"kind":"oneshot","status":"running","continuous":null,"binding":null,"api_host_owners":[]}),
    ));
    let remote_writer = remote.clone();
    let requests = Arc::new(Mutex::new(std::collections::HashMap::<String, Value>::new()));
    let local_state = state.clone();
    let stub = spawn_routed_stub(move |method, _, body| {
        let mut current = remote.lock().unwrap();
        if method == "GET" {
            if !current["binding"].is_null() {
                if let Some(change) = current["inject_local"].as_str() {
                    let mut s = local_state.lock().unwrap();
                    let session = s.sessions.get_mut("ts-adopt").unwrap();
                    match change {
                        "busy" => {
                            session
                                .agent_state
                                .lock()
                                .unwrap()
                                .inputs
                                .relay
                                .as_mut()
                                .unwrap()
                                .foreground = crate::agent_state::RelayStatus::Active
                        }
                        "rotated" => session.holder_incarnation = Some(999),
                        _ => unreachable!(),
                    }
                }
            }
            let mut reply = current.clone();
            reply.as_object_mut().unwrap().remove("inject_local");
            return (200, reply.to_string());
        }
        let p: Value = serde_json::from_str(body).unwrap();
        let id = p["request_id"].as_str().unwrap().to_owned();
        let mut requests = requests.lock().unwrap();
        if let Some(prior) = requests.get(&id) {
            if prior != &p {
                return (409, "{\"detail\":\"changed request\"}".into());
            }
            return (
                200,
                json!({"registered":true,"receipt":p["binding"],"current":*current}).to_string(),
            );
        }
        if p["expected_kind"] != current["kind"] || p["expected_binding"] != current["binding"] {
            return (409, "{\"detail\":\"stale\"}".into());
        }
        current["binding"] = p["binding"].clone();
        current["kind"] = json!("continuous");
        current["continuous"] = json!({"task_id":p["binding"]["continuous_task_id"],"host":p["binding"]["host"],"engine":p["binding"]["engine"],"run_mode":"persistent"});
        requests.insert(id, p.clone());
        // Server committed but the first caller receives an uncertain error.
        if p["request_id"] == "adopt-uncertain" {
            return (503, "{\"detail\":\"lost response\"}".into());
        }
        (
            200,
            json!({"registered":true,"receipt":p["binding"],"current":*current}).to_string(),
        )
    });
    {
        let mut s = state.lock().unwrap();
        s.config.scheduler.enabled = true;
        s.config.api_url = format!("http://127.0.0.1:{}", stub.port);
        s.config.api_token = "test".into();
        s.messaging_root = crate::path::dot_cm_dir().join("messaging");
        s.daemon_sessions_path = Some(crate::path::dot_cm_dir().join("daemon-sessions.json"));
        let session = s.sessions.get_mut("ts-adopt").unwrap();
        session.session_type = "codex".into();
        session.model = Some("gpt-6.1-terra".into());
        session.effort = Some("xhigh".into());
        let mut cell = session.agent_state.lock().unwrap();
        cell.inputs.relay = Some(crate::agent_state::RelaySnapshot {
            backend_connected: true,
            observed_at: crate::agent_state::unix_now(),
            ..Default::default()
        });
        cell.inputs.background.complete = true;
    }
    crate::messaging::startup::open_for_test(&state).unwrap();
    let handle = state.lock().unwrap().messaging.clone();
    let daemon_id = handle.lock().unwrap().as_ref().unwrap().daemon_id.clone();
    let p = json!({"task_id":"adopt-40","request_id":"adopt-first","expected":{
        "daemon_id":daemon_id,"host":crate::items::heartbeat::host_label().unwrap(),"planning_task_id":ADOPT_TASK,"session_uid":"ts-adopt",
        "workspace_id":"ws-1","worktree_path":wt.to_string_lossy(),"engine":"codex"},
        "schedule":{"kind":"periodic","every_secs":10800},"first_fire_at":crate::continuous::task::now_unix()+3600,
        "label":"Existing planning session","default_prompt":"Reconcile planning",
        "native_cron":{"status":"none","evidence":"operator verified no native scheduler"},"dry_run":true});
    (state, p, stub, remote_writer)
}
fn adoption_commit_request(state: &Arc<Mutex<DaemonState>>, p: &Value) -> Value {
    let preview = crate::continuous::adoption::handle(state, p).expect("dry-run");
    let mut commit = p.clone();
    for key in ["expected", "expected_state_hash", "expected_registration"] {
        commit[key] = preview[key].clone();
    }
    commit["dry_run"] = json!(false);
    commit
}
fn adoption_files(root: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(
        root: &std::path::Path,
        path: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, Vec<u8>>,
    ) {
        for e in std::fs::read_dir(path).unwrap() {
            let p = e.unwrap().path();
            let k = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                out.insert(k, vec![]);
                walk(root, &p, out);
            } else if p.is_file() {
                out.insert(k, std::fs::read(p).unwrap());
            }
        }
    }
    let mut out = Default::default();
    walk(root, root, &mut out);
    out
}
#[test]
fn adoption_dry_run_is_read_only_and_activation_never_spawns_or_fires() {
    with_temp_home(|| {
        let (state, p, stub) = adoption_fixture();
        let before = adoption_files(&crate::path::dot_cm_dir());
        let commit = adoption_commit_request(&state, &p);
        assert_eq!(
            before,
            adoption_files(&crate::path::dot_cm_dir()),
            "preview must not even create lock files"
        );
        assert!(stub
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.method == "GET"));
        let before_session = state.lock().unwrap().sessions["ts-adopt"].to_manifest_entry();
        arm_continuous_spawn_spy_for_test();
        arm_continuous_delivery_spy_for_test();
        let r = crate::continuous::adoption::handle(&state, &commit).unwrap();
        assert_eq!(r["adopted"], true, "{r}");
        assert_eq!(r["receipt"]["spawned"], false);
        assert_eq!(r["receipt"]["initial_fire"], false);
        let t = crate::continuous::task::load_one("adopt-40").unwrap();
        assert_eq!(t.current_session_uid.as_deref(), Some("ts-adopt"));
        assert_eq!(t.run_count, 0);
        assert!(t.last_run.is_none() && t.in_flight.is_none());
        assert!(t.next_fire_at > crate::continuous::task::now_unix());
        assert!(!t.paused);
        assert_eq!(state.lock().unwrap().sessions.len(), 1);
        crate::continuous::ContinuousScheduler::new(state.clone()).tick_once();
        assert_eq!(
            crate::continuous::task::load_one("adopt-40")
                .unwrap()
                .run_count,
            0
        );
        let after = state.lock().unwrap().sessions["ts-adopt"].to_manifest_entry();
        assert_eq!(before_session.model, after.model);
        assert_eq!(before_session.transcript_path, after.transcript_path);
        assert!(take_continuous_spawn_spy_for_test().is_empty());
        assert!(take_continuous_delivery_spy_for_test().is_empty());
        let disk: Value = serde_json::from_slice(
            &std::fs::read(crate::path::dot_cm_dir().join("daemon-sessions.json")).unwrap(),
        )
        .unwrap();
        assert!(
            disk.to_string().contains("adopt-40"),
            "session binding persisted: {disk}"
        );
        assert!(state.lock().unwrap().workspaces["ws-1"].pinned);
        // A completed receipt never resumes a later operator pause.
        continuous_pause(&state, &json!({"task_id":"adopt-40","paused":true})).unwrap();
        let replay = crate::continuous::adoption::handle(&state, &commit).unwrap();
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["current"]["paused"], true);
        let mut changed = commit.clone();
        changed["first_fire_at"] = json!(1234);
        assert!(crate::continuous::adoption::handle(&state, &changed)
            .unwrap_err()
            .1
            .contains("request_id"));
        assert_eq!(
            stub.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.method == "PUT")
                .count(),
            1
        );
        kill_all_sessions(&state);
    });
}
#[test]
fn adoption_crash_receipts_resume_each_boundary_without_duplicate_work() {
    for stage in [
        "intent",
        "staged",
        "registered",
        "session",
        "channel",
        "committed",
    ] {
        with_temp_home(|| {
            let (state, p, _) = adoption_fixture();
            let commit = adoption_commit_request(&state, &p);
            let pid = state.lock().unwrap().sessions["ts-adopt"].pid;
            arm_continuous_spawn_spy_for_test();
            arm_continuous_delivery_spy_for_test();
            crate::continuous::adoption::inject_fault(Some(stage));
            assert!(crate::continuous::adoption::handle(&state, &commit)
                .unwrap_err()
                .1
                .contains(stage));
            crate::continuous::adoption::inject_fault(None);
            if stage != "committed" {
                assert!(crate::continuous::adoption::ensure_available("adopt-40").is_err());
                for edit in [
                    json!({"task_id":"adopt-40","paused":false}),
                    json!({"task_id":"adopt-40","paused":true}),
                ] {
                    assert!(continuous_pause(&state, &edit).is_err());
                }
                assert!(continuous_update(
                    &state,
                    &json!({"task_id":"adopt-40","schedule":{"kind":"periodic","every_secs":1}})
                )
                .is_err());
                assert!(continuous_delete(&state, &json!({"task_id":"adopt-40"})).is_err());
                if let Some(t) = crate::continuous::task::load_one("adopt-40") {
                    assert!(t.paused && t.adoption_pending.is_some());
                    let r = trigger(
                        &state,
                        &Caller::operator(""),
                        &json!({"task_id":"adopt-40","fire_token":"forbidden"}),
                    )
                    .unwrap();
                    assert_eq!(r["fired"], false);
                }
            }
            let r = crate::continuous::adoption::handle(&state, &commit).unwrap();
            assert_eq!(r["adopted"], true, "{stage}: {r}");
            let replay = crate::continuous::adoption::handle(&state, &commit).unwrap();
            assert_eq!(replay["replayed"], true);
            assert_eq!(state.lock().unwrap().sessions["ts-adopt"].pid, pid);
            assert!(take_continuous_spawn_spy_for_test().is_empty());
            assert!(take_continuous_delivery_spy_for_test().is_empty());
            assert!(crate::continuous::adoption::ensure_available("adopt-40").is_ok());
            kill_all_sessions(&state);
        });
    }
}
#[test]
fn adoption_identity_busy_workflow_cron_and_schedule_refusals() {
    with_temp_home(|| {
        let (state, p, stub) = adoption_fixture();
        let commit = adoption_commit_request(&state, &p);
        let before = adoption_files(&crate::path::dot_cm_dir());
        for (key, value) in [
            ("session_uid", json!("remote")),
            (
                "planning_task_id",
                json!("00000000-0000-0000-0000-000000000000"),
            ),
            ("workspace_id", json!("wrong")),
            ("worktree_path", json!("/wrong")),
            ("engine", json!("claude")),
            ("daemon_id", json!("wrong")),
            ("host", json!("manager")),
            ("pid", json!(1)),
            ("child_start_time", json!(1)),
        ] {
            let mut bad = commit.clone();
            bad["expected"][key] = value;
            assert!(
                crate::continuous::adoption::handle(&state, &bad).is_err(),
                "{key}"
            );
        }
        let mut bad = commit.clone();
        bad["schedule"] = json!({"kind":"cron","expr":"17 */3 * * *"});
        assert!(crate::continuous::adoption::handle(&state, &bad)
            .unwrap_err()
            .1
            .contains("unsupported_schedule"));
        bad = commit.clone();
        bad["native_cron"]["status"] = json!("retire");
        assert!(crate::continuous::adoption::handle(&state, &bad)
            .unwrap_err()
            .1
            .contains("unsupported_owner"));
        for kind in ["busy", "unknown", "shell", "workflow", "continuous", "cron"] {
            {
                let mut s = state.lock().unwrap();
                let session = s.sessions.get_mut("ts-adopt").unwrap();
                match kind {
                    "shell" => session.session_type = "bash".into(),
                    "workflow" => session.workflow_run_id = Some("wf".into()),
                    "continuous" => session.continuous_task_id = Some("other".into()),
                    _ => {
                        let mut c = session.agent_state.lock().unwrap();
                        match kind {
                            "busy" => {
                                c.inputs.relay.as_mut().unwrap().foreground =
                                    crate::agent_state::RelayStatus::Active
                            }
                            "unknown" => c.inputs.relay.as_mut().unwrap().backend_connected = false,
                            "cron" => c.inputs.background.crons.push(crate::agent_state::Cron {
                                id: "c9aa57a6".into(),
                                schedule: "17 */3 * * *".into(),
                                recurring: true,
                            }),
                            _ => unreachable!(),
                        }
                    }
                }
            }
            assert!(
                crate::continuous::adoption::handle(&state, &commit).is_err(),
                "{kind}"
            );
            let mut s = state.lock().unwrap();
            let session = s.sessions.get_mut("ts-adopt").unwrap();
            session.session_type = "codex".into();
            session.workflow_run_id = None;
            session.continuous_task_id = None;
            let mut c = session.agent_state.lock().unwrap();
            c.inputs.relay.as_mut().unwrap().foreground = crate::agent_state::RelayStatus::Idle;
            c.inputs.relay.as_mut().unwrap().backend_connected = true;
            c.inputs.background.crons.clear();
        }
        assert_eq!(before, adoption_files(&crate::path::dot_cm_dir()));
        assert!(stub
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.method == "GET"));
        kill_all_sessions(&state);
    });
}
#[test]
fn adoption_rebind_preserves_existing_history_and_rejects_live_old_owner() {
    with_temp_home(|| {
        let (state, p, _) = adoption_fixture();
        let wt = p["expected"]["worktree_path"].as_str().unwrap();
        let mut t = crate::continuous::task::ContinuousTask::new(
            "adopt-40".into(),
            "Original label".into(),
            "ws-1".into(),
            wt.into(),
            crate::continuous::task::Engine::Codex,
            crate::continuous::task::RunMode::Persistent,
            crate::continuous::task::Schedule::Periodic { every_secs: 60 },
            "Original prompt".into(),
        );
        t.planning_task_id = Some(ADOPT_TASK.into());
        t.paused = true;
        t.run_count = 17;
        t.current_session_uid = Some("ts-old".into());
        crate::continuous::task::save(&t).unwrap();
        let history = crate::continuous::task::task_dir("adopt-40").join("runs.jsonl");
        std::fs::write(&history, "old history\n").unwrap();
        seed_mcp_spawn_fixture(&state, "ts-old", Some(ADOPT_TASK));
        assert!(crate::continuous::adoption::handle(&state, &p)
            .unwrap_err()
            .1
            .contains("another live orchestrator"));
        {
            let mut s = state.lock().unwrap();
            let mut old = s.sessions.remove("ts-old").unwrap();
            let _ = old.kill();
        }
        let commit = adoption_commit_request(&state, &p);
        let r = crate::continuous::adoption::handle(&state, &commit).unwrap();
        assert_eq!(r["adopted"], true);
        let saved = crate::continuous::task::load_one("adopt-40").unwrap();
        assert_eq!(saved.run_count, 17);
        assert_eq!(saved.label, "Original label");
        assert_eq!(saved.default_prompt, "Original prompt");
        assert_eq!(std::fs::read_to_string(history).unwrap(), "old history\n");
        assert!(set_workflow_context(
            &state,
            &json!({"uid":"ts-adopt","workflow_run_id":"wf","workflow_role":"lead"})
        )
        .is_err());
        kill_all_sessions(&state);
    });
}

#[test]
fn adoption_uncertain_remote_response_retries_the_durable_payload() {
    with_temp_home(|| {
        let (state, mut p, stub) = adoption_fixture();
        p["request_id"] = json!("adopt-uncertain");
        p["expected"].as_object_mut().unwrap().remove("daemon_id");
        p["expected"].as_object_mut().unwrap().remove("host");
        let commit = adoption_commit_request(&state, &p);
        let first = crate::continuous::adoption::handle(&state, &commit).unwrap_err();
        assert!(first.1.contains("lost response"));
        let pending = crate::continuous::task::load_one("adopt-40").unwrap();
        assert!(!pending.enabled && pending.paused && pending.adoption_pending.is_some());
        assert!(state.lock().unwrap().sessions["ts-adopt"]
            .continuous_task_id
            .is_none());
        let r = crate::continuous::adoption::handle(&state, &commit).unwrap();
        assert_eq!(r["adopted"], true);
        let reqs = stub.requests.lock().unwrap();
        let puts: Vec<_> = reqs.iter().filter(|r| r.method == "PUT").collect();
        assert_eq!(puts.len(), 2);
        assert_eq!(puts[0].body, puts[1].body);
        drop(reqs);
        kill_all_sessions(&state);
    });
}

#[test]
fn adoption_registered_retry_refuses_task_closure_and_metadata_drift() {
    for drift in ["closed", "kind", "engine", "host", "scheduler"] {
        with_temp_home(|| {
            let (state, p, _, remote) = adoption_fixture_remote();
            let commit = adoption_commit_request(&state, &p);
            crate::continuous::adoption::inject_fault(Some("registered"));
            assert!(crate::continuous::adoption::handle(&state, &commit)
                .unwrap_err()
                .1
                .contains("registered"));
            crate::continuous::adoption::inject_fault(None);
            {
                let mut r = remote.lock().unwrap();
                match drift {
                    "closed" => r["status"] = json!("done"),
                    "kind" => r["kind"] = json!("oneshot"),
                    "engine" => r["continuous"]["engine"] = json!("claude"),
                    "host" => r["continuous"]["host"] = json!("another-host"),
                    "scheduler" => r["continuous"]["task_id"] = json!("another-scheduler"),
                    _ => unreachable!(),
                }
            }
            let error = crate::continuous::adoption::handle(&state, &commit).unwrap_err();
            assert!(
                error.1.contains("planning_claim_changed"),
                "{drift}: {error:?}"
            );
            let t = crate::continuous::task::load_one("adopt-40").unwrap();
            assert!(t.paused && !t.enabled && t.adoption_pending.is_some());
            assert!(state.lock().unwrap().sessions["ts-adopt"]
                .continuous_task_id
                .is_none());
            assert_eq!(state.lock().unwrap().sessions.len(), 1);
            kill_all_sessions(&state);
        });
    }
}

#[test]
fn adoption_rechecks_local_agent_after_final_remote_read() {
    for change in ["busy", "rotated"] {
        with_temp_home(|| {
            let (state, p, _, remote) = adoption_fixture_remote();
            let commit = adoption_commit_request(&state, &p);
            remote.lock().unwrap()["inject_local"] = json!(change);
            let error = crate::continuous::adoption::handle(&state, &commit).unwrap_err();
            assert!(
                error.1.contains("idle") || error.1.contains("identity_changed"),
                "{error:?}"
            );
            let t = crate::continuous::task::load_one("adopt-40").unwrap();
            assert!(t.paused && t.adoption_pending.is_some());
            assert!(t.adoption_receipt.is_none());
            kill_all_sessions(&state);
        });
    }
}
#[test]
fn adoption_same_uid_rebind_mints_quiet_generation_and_old_retry_is_read_only() {
    with_temp_home(|| {
        let (state, p, _) = adoption_fixture();
        let first = adoption_commit_request(&state, &p);
        crate::continuous::adoption::handle(&state, &first).unwrap();
        let before = crate::continuous::task::load_one("adopt-40").unwrap();
        continuous_pause(&state, &json!({"task_id":"adopt-40","paused":true})).unwrap();
        let mut next = p.clone();
        next["request_id"] = json!("adopt-second");
        next["schedule"]["every_secs"] = json!(7200);
        let second = adoption_commit_request(&state, &next);
        crate::continuous::adoption::handle(&state, &second).unwrap();
        let after = crate::continuous::task::load_one("adopt-40").unwrap();
        assert_eq!(after.current_session_uid, before.current_session_uid);
        assert_eq!(
            after.messaging.as_ref().unwrap().revision,
            before.messaging.as_ref().unwrap().revision + 1
        );
        assert_eq!(
            after.messaging.as_ref().unwrap().subscription_id,
            before.messaging.as_ref().unwrap().subscription_id
        );
        let replay = crate::continuous::adoption::handle(&state, &first).unwrap();
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["current"]["schedule"]["every_secs"], 7200);
        kill_all_sessions(&state);
    });
}

#[test]
fn adoption_requires_the_old_native_cron_owners_verified_removal() {
    with_temp_home(|| {
        let (state, mut p, stub) = adoption_fixture();
        seed_mcp_spawn_fixture(&state, "ts-old-claude", Some(ADOPT_TASK));
        {
            let mut s = state.lock().unwrap();
            let old = s.sessions.get_mut("ts-old-claude").unwrap();
            old.session_type = "claude-code".into();
            old.agent_state
                .lock()
                .unwrap()
                .inputs
                .background
                .crons
                .push(crate::agent_state::Cron {
                    id: "c9aa57a6".into(),
                    schedule: "17 */3 * * *".into(),
                    recurring: true,
                });
        }
        p["native_cron"] = json!({"status":"removed_by_owner","owner_session_uid":"ts-old-claude","cron_ids":["c9aa57a6"],"evidence":"operator removal receipt"});
        let before = adoption_files(&crate::path::dot_cm_dir());
        let error = crate::continuous::adoption::handle(&state, &p).unwrap_err();
        assert!(error.1.contains("unsupported_owner") && error.1.contains("ts-old-claude"));
        assert_eq!(before, adoption_files(&crate::path::dot_cm_dir()));
        assert!(stub.requests.lock().unwrap().is_empty());
        state.lock().unwrap().sessions["ts-old-claude"]
            .agent_state
            .lock()
            .unwrap()
            .inputs
            .background
            .crons
            .clear();
        let commit = adoption_commit_request(&state, &p);
        let r = crate::continuous::adoption::handle(&state, &commit).unwrap();
        assert_eq!(r["adopted"], true);
        assert_eq!(r["current"]["current_session_uid"], "ts-adopt");
        assert_eq!(
            state.lock().unwrap().sessions.len(),
            2,
            "neither agent replaced or killed"
        );
        kill_all_sessions(&state);
    });
}
