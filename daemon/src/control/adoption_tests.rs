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
            if current["inject_initial"] == "remove" {
                let mut s = local_state.lock().unwrap();
                if let Some(mut session) = s.sessions.remove("ts-adopt") {
                    let _ = session.kill();
                }
            }
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
            reply.as_object_mut().unwrap().remove("inject_initial");
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

fn adoption_paused_record(p: &Value) {
    let mut t = crate::continuous::task::ContinuousTask::new(
        "adopt-40".into(), "Existing".into(), "ws-1".into(),
        p["expected"]["worktree_path"].as_str().unwrap().into(),
        crate::continuous::task::Engine::Codex,
        crate::continuous::task::RunMode::Persistent,
        crate::continuous::task::Schedule::Periodic { every_secs: 60 },
        "Existing prompt".into(),
    );
    t.planning_task_id = Some(ADOPT_TASK.into());
    t.paused = true;
    t.current_session_uid = Some("ts-adopt".into());
    crate::continuous::task::save(&t).unwrap();
}

#[test]
fn adoption_refresh_snapshot_cannot_mutate_intent_only_crash() {
    with_temp_home(|| {
        let (state, p, _) = adoption_fixture();
        adoption_paused_record(&p);
        let commit = adoption_commit_request(&state, &p);
        let before = serde_json::to_value(crate::continuous::task::load_one("adopt-40")).unwrap();
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let after_crash = captured.clone();
        let race_state = state.clone();
        let race_commit = commit.clone();
        crate::messaging::tasks::before_refresh_effects_for_test(move || {
            crate::continuous::adoption::inject_fault(Some("intent"));
            let e = crate::continuous::adoption::handle(&race_state, &race_commit).unwrap_err();
            crate::continuous::adoption::inject_fault(None);
            assert!(e.1.contains("intent"), "{e:?}");
            *after_crash.borrow_mut() = Some(adoption_files(&crate::path::dot_cm_dir()));
        });
        arm_continuous_spawn_spy_for_test();
        arm_continuous_delivery_spy_for_test();
        assert!(matches!(crate::messaging::tasks::refresh(&state).unwrap(),
            crate::messaging::tasks::RefreshOutcome::Applied(joins) if joins.is_empty()));
        assert_eq!(before, serde_json::to_value(crate::continuous::task::load_one("adopt-40")).unwrap(),
            "stale refresh changed original_hash after durable intent");
        assert_eq!(*captured.borrow(), Some(adoption_files(&crate::path::dot_cm_dir())),
            "intent-only fence must cover channel/name/subscription/join effects too");
        let r = crate::continuous::adoption::handle(&state, &commit).unwrap();
        assert_eq!(r["adopted"], true);
        assert!(take_continuous_spawn_spy_for_test().is_empty());
        assert!(take_continuous_delivery_spy_for_test().is_empty());
        kill_all_sessions(&state);
    });
}

#[test]
fn adoption_refresh_snapshot_cannot_replace_same_uid_generation() {
    with_temp_home(|| {
        let (state, p, _) = adoption_fixture();
        let first = adoption_commit_request(&state, &p);
        crate::continuous::adoption::handle(&state, &first).unwrap();
        continuous_pause(&state, &json!({"task_id":"adopt-40","paused":true})).unwrap();
        // Legacy path backfill makes the stale pass try to persist its old binding.
        crate::continuous::task::modify("adopt-40", |t| t.messaging.as_mut().unwrap().path = None).unwrap();
        let old = crate::continuous::task::load_one("adopt-40").unwrap();
        let mut p = p.clone();
        p["request_id"] = json!("same-uid-race");
        let commit = adoption_commit_request(&state, &p);
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let after_adopt = captured.clone();
        let race_state = state.clone();
        crate::messaging::tasks::before_refresh_effects_for_test(move || {
            let r = crate::continuous::adoption::handle(&race_state, &commit).unwrap();
            assert_eq!(r["adopted"], true);
            *after_adopt.borrow_mut() = Some(r["current"]["messaging"].clone());
        });
        arm_continuous_spawn_spy_for_test();
        arm_continuous_delivery_spy_for_test();
        crate::messaging::tasks::refresh(&state).unwrap();
        let current = crate::continuous::task::load_one("adopt-40").unwrap();
        assert_eq!(serde_json::to_value(&current.messaging).unwrap(), captured.borrow().as_ref().unwrap().clone());
        let binding = current.messaging.unwrap();
        assert_eq!(binding.revision, old.messaging.unwrap().revision + 1);
        let handle = state.lock().unwrap().messaging.clone();
        let mut slot = handle.lock().unwrap();
        let store = slot.as_mut().unwrap();
        let subscription = store.task_binding_for("adopt-40").expect("refresh must not fence the new generation").clone();
        assert_eq!(subscription.revision, binding.revision);
        assert_eq!(subscription.actor, store.participant_id("ts-adopt"));
        assert!(subscription.start_position.is_some());
        let hits = store.monitors(&subscription.actor, &json!({"action":"get",
            "monitor_id":format!("task-{}-{}", subscription.id, subscription.revision)})).unwrap();
        assert!(hits["items"].as_array().unwrap().is_empty(), "{hits}");
        drop(slot);
        assert!(take_continuous_spawn_spy_for_test().is_empty());
        assert!(take_continuous_delivery_spy_for_test().is_empty());
        kill_all_sessions(&state);
    });
}

#[test]
fn adoption_refresh_snapshot_cannot_fence_a_new_task() {
    with_temp_home(|| {
        let (state, p, _) = adoption_fixture();
        let commit = adoption_commit_request(&state, &p);
        let race_state = state.clone();
        crate::messaging::tasks::before_refresh_effects_for_test(move || {
            assert_eq!(crate::continuous::adoption::handle(&race_state, &commit).unwrap()["adopted"], true);
        });
        crate::messaging::tasks::refresh(&state).unwrap();
        let handle = state.lock().unwrap().messaging.clone();
        let slot = handle.lock().unwrap();
        assert!(slot.as_ref().unwrap().task_binding_for("adopt-40").is_some(),
            "stale active set disabled the newly adopted subscription");
        drop(slot);
        kill_all_sessions(&state);
    });
}

#[test]
fn adoption_pending_refresh_and_explicit_ensure_preserve_all_effects() {
    for stage in ["intent", "staged", "channel"] {
        with_temp_home(|| {
            let (state, p, _) = adoption_fixture();
            let first = adoption_commit_request(&state, &p);
            crate::continuous::adoption::handle(&state, &first).unwrap();
            continuous_pause(&state, &json!({"task_id":"adopt-40","paused":true})).unwrap();
            let mut next = p.clone();
            next["request_id"] = json!(format!("pending-{stage}"));
            let commit = adoption_commit_request(&state, &next);
            crate::continuous::adoption::inject_fault(Some(stage));
            assert!(crate::continuous::adoption::handle(&state, &commit).unwrap_err().1.contains(stage));
            crate::continuous::adoption::inject_fault(None);
            let before = adoption_files(&crate::path::dot_cm_dir());
            crate::messaging::tasks::refresh(&state).unwrap();
            let report = crate::messaging::tasks::ensure_for_task(&state, "adopt-40").unwrap();
            assert!(!report.bound && !report.deferred.is_empty());
            assert_eq!(before, adoption_files(&crate::path::dot_cm_dir()), "{stage}");
            let handle = state.lock().unwrap().messaging.clone();
            assert!(handle.lock().unwrap().as_ref().unwrap().task_binding_for("adopt-40").is_some(), "{stage}");
            assert_eq!(crate::continuous::adoption::handle(&state, &commit).unwrap()["adopted"], true);
            kill_all_sessions(&state);
        });
    }
}

fn adoption_deferred_join(state: &Arc<Mutex<DaemonState>>) -> crate::messaging::tasks::DeferredJoin {
    let t = crate::continuous::task::load_one("adopt-40").unwrap();
    let handle = state.lock().unwrap().messaging.clone();
    let actor = handle.lock().unwrap().as_ref().unwrap().participant_id("ts-adopt");
    crate::messaging::tasks::DeferredJoin {
        task_id: t.task_id,
        binding: t.messaging.unwrap(),
        uid: "ts-adopt".into(),
        actor,
        params: json!({"action":"join","request_id":"deferred-test"}),
    }
}

#[test]
fn adoption_deferred_join_requires_current_generation_and_no_pending_intent() {
    with_temp_home(|| {
        let (state, p, _) = adoption_fixture();
        let first = adoption_commit_request(&state, &p);
        crate::continuous::adoption::handle(&state, &first).unwrap();
        let old = adoption_deferred_join(&state);
        let mut sent = 0;
        crate::messaging::tasks::forward_joins(&state, vec![old.clone()], |_, _| {
            let handle = state.try_lock().expect("no state lock across network").messaging.clone();
            assert!(handle.try_lock().is_ok(), "no store lock across network");
            assert!(crate::continuous::adoption::try_binding_lock().is_none(), "hold generation gate across effect");
            crate::continuous::task::modify("adopt-40", |_| {}).unwrap(); // No task-file lock held.
            sent += 1;
        });
        assert_eq!(sent, 1, "current generation forwards");
        continuous_pause(&state, &json!({"task_id":"adopt-40","paused":true})).unwrap();
        let mut next = p.clone();
        next["request_id"] = json!("deferred-second");
        let second = adoption_commit_request(&state, &next);
        crate::continuous::adoption::handle(&state, &second).unwrap();
        crate::messaging::tasks::forward_joins(&state, vec![old], |_, _| panic!("stale same-UID join sent"));
        let current = adoption_deferred_join(&state);
        continuous_pause(&state, &json!({"task_id":"adopt-40","paused":true})).unwrap();
        next["request_id"] = json!("deferred-intent");
        let third = adoption_commit_request(&state, &next);
        crate::continuous::adoption::inject_fault(Some("intent"));
        assert!(crate::continuous::adoption::handle(&state, &third).is_err());
        crate::continuous::adoption::inject_fault(None);
        crate::messaging::tasks::forward_joins(&state, vec![current], |_, _| panic!("pending intent join sent"));
        kill_all_sessions(&state);
    });
}

#[test]
fn adoption_refresh_does_not_wait_or_reenter_the_binding_gate() {
    with_temp_home(|| {
        let (state, p, _) = adoption_fixture();
        adoption_paused_record(&p);
        let before = adoption_files(&crate::path::dot_cm_dir());
        let guard = crate::continuous::adoption::binding_lock();
        assert!(matches!(crate::messaging::tasks::refresh(&state).unwrap(),
            crate::messaging::tasks::RefreshOutcome::Deferred));
        assert_eq!(before, adoption_files(&crate::path::dot_cm_dir()));
        // create/adopt already own the gate; the internal entrypoint must not
        // reacquire it. The public normal entrypoint below also remains usable.
        assert!(crate::messaging::tasks::ensure_for_task_locked(&state, "adopt-40", &guard, None).unwrap().bound);
        drop(guard);
        assert!(crate::messaging::tasks::ensure_for_task(&state, "adopt-40").unwrap().bound);
        kill_all_sessions(&state);
    });
}

#[test]
fn adoption_session_removed_during_planning_get_is_a_conflict_without_intent() {
    for preview in [false, true] {
        with_temp_home(|| {
            let (state, p, _, remote) = adoption_fixture_remote();
            let commit = adoption_commit_request(&state, &p);
            let before = adoption_files(&crate::path::dot_cm_dir());
            remote.lock().unwrap()["inject_initial"] = json!("remove");
            let error = crate::continuous::adoption::handle(&state, if preview { &p } else { &commit }).unwrap_err();
            assert!(matches!(error.0, ErrorCode::Conflict), "{error:?}");
            assert!(error.1.contains("not live"), "{error:?}");
            assert_eq!(before, adoption_files(&crate::path::dot_cm_dir()));
            assert!(state.lock().unwrap().sessions.is_empty());
        });
    }
}

#[test]
fn adoption_validates_durable_and_guarded_workspace_bindings() {
    for case in ["volatile_conflict", "durable_conflict", "guarded_missing", "guarded_inferred", "epoch_changed"] {
        with_temp_home(|| {
            let (state, p, _) = adoption_fixture();
            let commit = adoption_commit_request(&state, &p);
            {
                let mut s = state.lock().unwrap();
                match case {
                    "volatile_conflict" => { s.task_workspaces.insert(ADOPT_TASK.into(), "elsewhere".into()); }
                    "durable_conflict" => { s.bindings.insert(ADOPT_TASK.into(), "elsewhere".into()); }
                    _ => {
                        cm_holder_proto::org::set(&mut s.workspaces.get_mut("ws-1").unwrap().org, "binding_epoch", json!(4));
                        if case != "guarded_missing" { s.bindings.insert(ADOPT_TASK.into(), "ws-1".into()); }
                        if case == "guarded_inferred" { s.inferred_bindings.push((ADOPT_TASK.into(), "ws-1".into())); }
                    }
                }
            }
            let before = adoption_files(&crate::path::dot_cm_dir());
            let error = crate::continuous::adoption::handle(&state, &commit).unwrap_err();
            assert!(matches!(error.0, ErrorCode::Conflict), "{case}: {error:?}");
            assert_eq!(before, adoption_files(&crate::path::dot_cm_dir()), "{case}");
            kill_all_sessions(&state);
        });
    }
}

#[test]
fn adoption_persists_workspace_binding_across_guarded_and_plain_restart() {
    for guarded in [false, true] {
        with_temp_home(|| {
            let (state, p, _) = adoption_fixture();
            if guarded {
                let mut s = state.lock().unwrap();
                cm_holder_proto::org::set(&mut s.workspaces.get_mut("ws-1").unwrap().org, "binding_epoch", json!(7));
                s.bindings.insert(ADOPT_TASK.into(), "ws-1".into());
            }
            let commit = adoption_commit_request(&state, &p);
            assert_eq!(commit["expected"]["workspace_binding_epoch"], if guarded { json!(7) } else { Value::Null });
            crate::continuous::adoption::handle(&state, &commit).unwrap();
            let path = state.lock().unwrap().daemon_sessions_path.clone().unwrap();
            let disk: crate::manifest::Manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(disk.bindings.get(ADOPT_TASK).map(String::as_str), Some("ws-1"));
            let mut restored = DaemonState::new();
            restored.load_manifest_from_disk(&path).unwrap();
            assert_eq!(restored.bindings.get(ADOPT_TASK).map(String::as_str), Some("ws-1"));
            assert!(restored.inferred_bindings.is_empty(), "binding must be recorded, not reconstructed");
            assert_eq!(cm_holder_proto::org::binding_epoch(restored.workspaces["ws-1"].org.as_ref()), guarded.then_some(7));
            let headless = make_state_arc();
            headless.lock().unwrap().daemon_sessions_path = Some(path);
            rehydrate_derived_state(&headless);
            assert_eq!(headless.lock().unwrap().bindings.get(ADOPT_TASK).map(String::as_str), Some("ws-1"));
            kill_all_sessions(&state);
        });
    }
}

#[test]
fn adoption_ensure_preserves_uninitialized_store_readiness() {
    with_temp_home(|| {
        let state = make_state_arc();
        let error = crate::messaging::tasks::ensure_for_task(&state, "adopt-40").unwrap_err();
        assert_eq!(error.code, "messaging_starting");
        assert!(matches!(crate::messaging::tasks::refresh(&state).unwrap(),
            crate::messaging::tasks::RefreshOutcome::Deferred));
        let handle = state.lock().unwrap().messaging.clone();
        assert!(handle.lock().unwrap().is_none(), "ensure must not synchronously open a store");
    });
}

fn adoption_chat_call(state: &Arc<Mutex<DaemonState>>, uid: &str, method: &str, params: Value) -> crate::control::protocol::Response {
    crate::messaging::rpc::dispatch(state, &crate::control::protocol::Request {
        id: "adoption-chat-test".into(), caller: Caller::session(uid),
        method: format!("messaging.{method}"), params,
    })
}

fn adoption_monitor_fixture() -> (Arc<Mutex<DaemonState>>, StubApi, Value) {
    let (state, stub, old, _) = adoption_monitor_fixture_with_params();
    (state, stub, old)
}

fn adoption_monitor_fixture_with_params() -> (Arc<Mutex<DaemonState>>, StubApi, Value, Value) {
    let (state, p, stub) = adoption_fixture();
    let commit = adoption_commit_request(&state, &p);
    crate::continuous::adoption::handle(&state, &commit).unwrap();
    seed_mcp_spawn_fixture(&state, "ts-replacement", Some(ADOPT_TASK));
    state.lock().unwrap().sessions.get_mut("ts-replacement").unwrap().session_type = "codex".into();
    let binding = crate::continuous::task::load_one("adopt-40").unwrap().messaging.unwrap();
    let handle = state.lock().unwrap().messaging.clone();
    let message = handle.lock().unwrap().as_mut().unwrap()
        .post_system_notice(&binding.channel_id, "Work intended for the current orchestrator", "authority-race").unwrap();
    let id = format!("task-{}-{}", binding.subscription_id, binding.revision);
    let read = adoption_chat_call(&state, "ts-adopt", "monitors", json!({"action":"get","monitor_id":id}));
    assert!(read.ok, "{:?}", read.error);
    let result = read.result.unwrap();
    assert!(result["items"].to_string().contains(message["event_id"].as_str().unwrap()));
    (state, stub, json!({"monitor_id":id,"receipt":result["receipt"],"event_id":message["event_id"]}), p)
}

fn adoption_change_authoritative_monitor_owner() {
    // This is the scheduler handover shape, which does not take the global gate.
    crate::continuous::task::modify("adopt-40", |t| {
        t.current_session_uid = Some("ts-replacement".into());
        t.messaging.as_mut().unwrap().handover("ts-replacement");
    }).unwrap();
}

fn adoption_current_owner_can_read_and_ack(state: &Arc<Mutex<DaemonState>>, event: &Value) {
    let binding = crate::continuous::task::load_one("adopt-40").unwrap().messaging.unwrap();
    let id = format!("task-{}-{}", binding.subscription_id, binding.revision);
    let read = adoption_chat_call(state, "ts-replacement", "monitors", json!({"action":"get","monitor_id":id}));
    assert!(read.ok, "{:?}", read.error);
    let result = read.result.unwrap();
    assert!(result["items"].to_string().contains(event.as_str().unwrap()), "replacement lost unacknowledged work: {result}");
    let ack = adoption_chat_call(state, "ts-replacement", "monitors", json!({"action":"ack","monitor_id":id,"receipt":result["receipt"],"request_id":"replacement-ack"}));
    assert!(ack.ok, "{:?}", ack.error);
}

#[test]
fn adoption_contended_refresh_refuses_stale_monitor_ack() {
    with_temp_home(|| {
        let (state, _stub, old) = adoption_monitor_fixture();
        let guard = crate::continuous::adoption::binding_lock();
        adoption_change_authoritative_monitor_owner();
        let ack = adoption_chat_call(&state, "ts-adopt", "monitors", json!({"action":"ack","monitor_id":old["monitor_id"],"receipt":old["receipt"],"request_id":"stale-ack"}));
        assert!(!ack.ok, "stale owner acknowledged replacement work while refresh was skipped");
        let handle = state.lock().unwrap().messaging.clone();
        assert!(handle.lock().unwrap().as_ref().unwrap().task_binding_for("adopt-40").unwrap().acknowledged.is_empty());
        let ordinary = adoption_chat_call(&state, "ts-adopt", "send", json!({"channel":"general","body":"ordinary chat remains responsive","request_id":"ordinary-under-gate"}));
        assert!(ordinary.ok, "{:?}", ordinary.error);
        let personal = adoption_chat_call(&state, "ts-adopt", "monitor", json!({"scope":{"dms":true},"request_id":"personal-under-gate"}));
        assert!(personal.ok, "{:?}", personal.error);
        let personal_id = personal.result.unwrap()["id"].clone();
        let page = adoption_chat_call(&state, "ts-adopt", "monitors", json!({"action":"get","monitor_id":personal_id}));
        assert!(page.ok, "{:?}", page.error);
        let ack = adoption_chat_call(&state, "ts-adopt", "monitors", json!({"action":"ack","monitor_id":personal_id,"receipt":page.result.unwrap()["receipt"],"request_id":"personal-ack-under-gate"}));
        assert!(ack.ok, "{:?}", ack.error);
        drop(guard);
        adoption_current_owner_can_read_and_ack(&state, &old["event_id"]);
        kill_all_sessions(&state);
    });
}

#[test]
fn adoption_contended_refresh_refuses_stale_monitor_get() {
    with_temp_home(|| {
        let (state, _stub, old) = adoption_monitor_fixture();
        let guard = crate::continuous::adoption::binding_lock();
        adoption_change_authoritative_monitor_owner();
        let read = adoption_chat_call(&state, "ts-adopt", "monitors", json!({"action":"get","monitor_id":old["monitor_id"]}));
        assert!(!read.ok, "stale owner received task-monitor results while refresh was skipped");
        drop(guard);
        adoption_current_owner_can_read_and_ack(&state, &old["event_id"]);
        kill_all_sessions(&state);
    });
}

fn adoption_native_events(state: &Arc<Mutex<DaemonState>>, uid: &str) -> usize {
    let root = state.lock().unwrap().messaging_root.clone();
    std::fs::read_dir(crate::notifications::directory(&root, uid)).into_iter().flatten().flatten()
        .filter(|e| e.path().extension().is_some_and(|s| s == "json")).count()
}

#[test]
fn adoption_contended_refresh_refuses_stale_native_task_wake() {
    with_temp_home(|| {
        let (state, _stub, _) = adoption_monitor_fixture();
        let guard = crate::continuous::adoption::binding_lock();
        adoption_change_authoritative_monitor_owner();
        crate::messaging::delivery::tick(&state);
        assert_eq!(adoption_native_events(&state, "ts-adopt"), 0, "native task wake published to stale owner");
        drop(guard);
        crate::messaging::tasks::refresh(&state).unwrap();
        crate::messaging::delivery::tick(&state);
        assert!(adoption_native_events(&state, "ts-replacement") > 0, "current owner must receive retained work");
        kill_all_sessions(&state);
    });
}

#[test]
fn adoption_deferred_task_authority_retains_queue_and_correct_owner_retry() {
    with_temp_home(|| {
        let (state, _stub, old) = adoption_monitor_fixture();
        crate::messaging::delivery::tick(&state);
        assert!(adoption_native_events(&state, "ts-adopt") > 0);
        let (root, handle) = {
            let s = state.lock().unwrap();
            (s.messaging_root.clone(), s.messaging.clone())
        };
        let queue_dir = handle.lock().unwrap().as_ref().unwrap().root.join("_delivery");
        let native_dir = crate::notifications::directory(&root, "ts-adopt");
        let queue_before = adoption_files(&queue_dir);
        let native_before = adoption_files(&native_dir);
        let personal = adoption_chat_call(&state, "ts-adopt", "monitor", json!({"scope":{"dms":true},"request_id":"busy-personal"}));
        assert!(personal.ok);
        let personal_id = personal.result.unwrap()["id"].clone();
        // Model an overlapping local scheduler/adoption lifecycle operation.
        // The current owner has not changed, but cannot yet be verified.
        let lifecycle = crate::continuous::task::lock_lifecycle("adopt-40").unwrap();
        let ack_params = json!({"action":"ack","monitor_id":old["monitor_id"],"receipt":old["receipt"],"request_id":"deferred-owner-ack"});
        assert!(!adoption_chat_call(&state, "ts-adopt", "monitors", ack_params.clone()).ok);
        assert!(!adoption_chat_call(&state, "ts-adopt", "monitors", json!({"action":"get","monitor_id":old["monitor_id"]})).ok);
        let send = adoption_chat_call(&state, "ts-adopt", "send", json!({"channel":"general","body":"send while task authority is deferred","request_id":"busy-ordinary"}));
        assert!(send.ok, "{:?}", send.error);
        let page = adoption_chat_call(&state, "ts-adopt", "monitors", json!({"action":"get","monitor_id":personal_id}));
        assert!(page.ok, "{:?}", page.error);
        let personal_ack = adoption_chat_call(&state, "ts-adopt", "monitors", json!({"action":"ack","monitor_id":personal_id,"receipt":page.result.unwrap()["receipt"],"request_id":"busy-personal-ack"}));
        assert!(personal_ack.ok, "{:?}", personal_ack.error);
        crate::messaging::delivery::tick(&state);
        assert_eq!(queue_before, adoption_files(&queue_dir), "Deferred must not settle/cancel queued work");
        assert_eq!(native_before, adoption_files(&native_dir), "pending native receipt must be unchanged");
        assert!(handle.lock().unwrap().as_ref().unwrap().task_binding_for("adopt-40").unwrap().acknowledged.is_empty());
        drop(lifecycle);
        let read = adoption_chat_call(&state, "ts-adopt", "monitors", json!({"action":"get","monitor_id":old["monitor_id"]}));
        assert!(read.ok, "{:?}", read.error);
        assert!(read.result.unwrap()["items"].to_string().contains(old["event_id"].as_str().unwrap()));
        let ack = adoption_chat_call(&state, "ts-adopt", "monitors", ack_params.clone());
        assert!(ack.ok, "{:?}", ack.error);
        let replay = adoption_chat_call(&state, "ts-adopt", "monitors", ack_params);
        assert!(replay.ok, "identical valid-owner retry remains idempotent");
        assert!(handle.lock().unwrap().as_ref().unwrap().task_binding_for("adopt-40").unwrap().acknowledged.contains(old["event_id"].as_str().unwrap()));
        kill_all_sessions(&state);
    });
}

fn adoption_personal_native_control(pending_intent: bool) {
    with_temp_home(|| {
        let (state, _stub, old, mut p) = adoption_monitor_fixture_with_params();
        crate::messaging::delivery::tick(&state);
        let (root, handle) = {
            let s = state.lock().unwrap();
            (s.messaging_root.clone(), s.messaging.clone())
        };
        let native_dir = crate::notifications::directory(&root, "ts-adopt");
        let native_before = adoption_files(&native_dir);
        let queue_dir = handle.lock().unwrap().as_ref().unwrap().root.join("_delivery");
        let queue_before = adoption_files(&queue_dir);
        assert!(adoption_native_events(&state, "ts-adopt") > 0);
        let actor = handle.lock().unwrap().as_ref().unwrap().participant_id("ts-adopt");
        let personal = adoption_chat_call(&state, "ts-adopt", "monitor",
            json!({"scope":{"channel":"general"},"mode":"continuous","notify":"wake","request_id":"personal-native-monitor"}));
        assert!(personal.ok, "{:?}", personal.error);
        let personal_id = personal.result.unwrap()["id"].clone();
        let lifecycle;
        let retry;
        if pending_intent {
            continuous_pause(&state, &json!({"task_id":"adopt-40","paused":true})).unwrap();
            p["request_id"] = json!("native-intent-retry");
            retry = Some(adoption_commit_request(&state, &p));
            crate::continuous::adoption::inject_fault(Some("intent"));
            assert!(crate::continuous::adoption::handle(&state, retry.as_ref().unwrap()).unwrap_err().1.contains("intent"));
            crate::continuous::adoption::inject_fault(None);
            lifecycle = None;
        } else {
            retry = None;
            lifecycle = Some(crate::continuous::task::lock_lifecycle("adopt-40").unwrap());
        }
        for (index, kind) in ["dm", "mention", "monitor"].into_iter().enumerate() {
            let body = format!("personal-{kind}");
            let mut params = json!({"body":body,"name":"personal-sender","request_id":format!("personal-native-{index}")});
            if kind == "dm" { params["dm"] = json!(actor); }
            else { params["channel"] = json!("general"); }
            if kind == "mention" { params["mentions"] = json!([actor]); }
            let sent = adoption_chat_call(&state, "ts-replacement", "send", params);
            assert!(sent.ok, "{:?}", sent.error);
            crate::messaging::delivery::tick(&state);
            let files = adoption_files(&native_dir);
            let matching: Vec<_> = files.iter().filter(|(name, bytes)| {
                name.ends_with(".json") && serde_json::from_slice::<Value>(bytes).ok()
                    .is_some_and(|e| e["text"].as_str().is_some_and(|text| text.contains(&body)))
            }).collect();
            assert_eq!(matching.len(), 1, "independent {kind} native wake must be delivered while task authority is deferred");
            let event = serde_json::from_slice::<Value>(matching[0].1).unwrap();
            assert!(!event["text"].as_str().unwrap().contains(old["monitor_id"].as_str().unwrap()));
            for (name, bytes) in &native_before {
                assert_eq!(files.get(name), Some(bytes), "preexisting task native receipt changed");
            }
            let queues = adoption_files(&queue_dir);
            for (name, bytes) in &queue_before {
                if !name.ends_with(".json") { continue; }
                let before: Value = serde_json::from_slice(bytes).unwrap();
                let after: Value = serde_json::from_slice(&queues[name]).unwrap();
                for batch in before["batches"].as_array().unwrap() {
                    assert!(after["batches"].as_array().unwrap().contains(batch), "deferred task batch changed");
                }
            }
            let mut read_params = json!({});
            if kind == "dm" {
                read_params["dm"] = json!(handle.lock().unwrap().as_ref().unwrap().participant_id("ts-replacement"));
            } else { read_params["channel"] = json!("general"); }
            let read = adoption_chat_call(&state, "ts-adopt", "read", read_params);
            assert!(read.ok, "{:?}", read.error);
            let monitor_page = adoption_chat_call(&state, "ts-adopt", "monitors", json!({"action":"get","monitor_id":personal_id}));
            assert!(monitor_page.ok);
            let ack = adoption_chat_call(&state, "ts-adopt", "monitors", json!({"action":"ack","monitor_id":personal_id,
                "receipt":monitor_page.result.unwrap()["receipt"],"request_id":format!("personal-native-ack-{index}")}));
            assert!(ack.ok, "{:?}", ack.error);
            let count = adoption_native_events(&state, "ts-adopt");
            crate::messaging::delivery::tick(&state);
            assert_eq!(adoption_native_events(&state, "ts-adopt"), count, "personal work duplicated");
        }
        assert!(handle.lock().unwrap().as_ref().unwrap().task_binding_for("adopt-40").unwrap().acknowledged.is_empty());
        drop(lifecycle);
        if let Some(retry) = retry {
            assert_eq!(crate::continuous::adoption::handle(&state, &retry).unwrap()["adopted"], true);
        }
        let binding = crate::continuous::task::load_one("adopt-40").unwrap().messaging.unwrap();
        let expected_hit = if pending_intent {
            // Completing adoption intentionally establishes a new quiet
            // generation. Historical work remains readable, without an
            // initial wake; test new-owner get/ack with a post-activation hit.
            let history = adoption_chat_call(&state, "ts-adopt", "read", json!({"conversation":binding.channel_id}));
            assert!(history.ok);
            assert!(history.result.unwrap()["items"].to_string().contains(old["event_id"].as_str().unwrap()));
            handle.lock().unwrap().as_mut().unwrap()
                .post_system_notice(&binding.channel_id, "Work after recovery", "after-native-recovery").unwrap()["event_id"].clone()
        } else { old["event_id"].clone() };
        let monitor = format!("task-{}-{}", binding.subscription_id, binding.revision);
        let read = adoption_chat_call(&state, "ts-adopt", "monitors", json!({"action":"get","monitor_id":monitor}));
        assert!(read.ok, "{:?}", read.error);
        let page = read.result.unwrap();
        assert!(page["items"].to_string().contains(expected_hit.as_str().unwrap()), "task work lost");
        let ack_params = json!({"action":"ack","monitor_id":monitor,"receipt":page["receipt"],"request_id":"task-after-personal"});
        assert!(adoption_chat_call(&state, "ts-adopt", "monitors", ack_params.clone()).ok);
        assert!(adoption_chat_call(&state, "ts-adopt", "monitors", ack_params).ok);
        kill_all_sessions(&state);
    });
}

#[test]
fn adoption_deferred_personal_native_wakes_during_pending_intent() {
    adoption_personal_native_control(true);
}
#[test]
fn adoption_deferred_personal_native_wakes_during_lifecycle_contention() {
    adoption_personal_native_control(false);
}

fn adoption_rename(state: &Arc<Mutex<DaemonState>>, name: &str, request_id: &str) -> crate::control::protocol::Response {
    let handle = state.lock().unwrap().messaging.clone();
    let revision = {
        let slot = handle.lock().unwrap();
        let store = slot.as_ref().unwrap();
        store.names[&store.participant_id("ts-adopt")].revision
    };
    crate::messaging::rpc::dispatch(state, &crate::control::protocol::Request {
        id: "adoption-name-test".into(), caller: Caller::session("ts-adopt"), method: "session.set_name".into(),
        params: json!({"uid":"ts-adopt","name":name,"expected_name_revision":revision,"request_id":request_id}),
    })
}
fn adoption_reserved_name_control(stale: bool) {
    with_temp_home(|| {
        let (state, _stub, _) = adoption_monitor_fixture();
        let gate = crate::continuous::adoption::binding_lock();
        let lifecycle = if stale {
            adoption_change_authoritative_monitor_owner();
            None
        } else { Some(crate::continuous::task::lock_lifecycle("adopt-40").unwrap()) };
        let refused = adoption_rename(&state, "borrowed-orchestrator", "reserved-refusal");
        assert!(!refused.ok, "stale/deferred cached owner claimed reserved name");
        assert!(refused.error.unwrap().message.contains("reserved"));
        assert!(adoption_rename(&state, "ordinary-codename", "ordinary-rename").ok);
        assert!(adoption_chat_call(&state, "ts-adopt", "send",
            json!({"channel":"general","body":"ordinary send after rename","request_id":"renamed-send"})).ok);
        drop(lifecycle);
        if stale {
            crate::continuous::task::modify("adopt-40", |t| {
                t.current_session_uid = Some("ts-adopt".into());
                t.messaging.as_mut().unwrap().session_uid = Some("ts-adopt".into());
                // Restore the store's original generation for the control.
                t.messaging.as_mut().unwrap().revision -= 1;
            }).unwrap();
        }
        let current = adoption_rename(&state, "valid-orchestrator", "current-reserved-name");
        assert!(current.ok, "{:?}", current.error);
        drop(gate);
        kill_all_sessions(&state);
    });
}
#[test]
fn adoption_reserved_name_refuses_stale_authority() { adoption_reserved_name_control(true); }
#[test]
fn adoption_reserved_name_refuses_deferred_authority() { adoption_reserved_name_control(false); }
