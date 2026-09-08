//! Disposable messaging-only interoperability/latency probe. It never starts
//! sessions, native notifications, a PTY holder, or a CM control socket.
use cm_daemon::{
    control::protocol::{Caller, Request},
    messaging::{self, Store},
    state::DaemonState,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
fn call(s: &Arc<Mutex<DaemonState>>, uid: &str, p: Value) -> anyhow::Result<Value> {
    let response = serde_json::to_value(messaging::rpc::dispatch(
        s,
        &Request {
            id: "lab".into(),
            caller: Caller::session(uid),
            method: "messaging.send".into(),
            params: p,
        },
    ))?;
    if !response["error"].is_null() {
        anyhow::bail!("{}", response["error"]);
    }
    Ok(response["result"].clone())
}
fn main() -> anyhow::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    let root = Path::new(args.get(2).ok_or_else(|| {
        anyhow::anyhow!("Usage: messaging-lab serve|exercise|--messaging-sync-stdio ROOT")
    })?);
    if mode == "--messaging-sync-stdio" {
        messaging::sync::stdio_bridge(root)?;
        return Ok(());
    }
    if !root.join("DISPOSABLE_MESSAGING_LAB").is_file() {
        anyhow::bail!("Refusing a root without DISPOSABLE_MESSAGING_LAB marker");
    }
    let mut state = DaemonState::default();
    state.messaging_root = root.into();
    state.tui_sessions.insert(
        "lab-agent".into(),
        serde_json::from_value(json!({"uid":"lab-agent","label":"lab-agent"}))?,
    );
    let state = Arc::new(Mutex::new(state));
    messaging::rpc::initialize(&state)?;
    if mode == "serve" {
        let handle = state.lock().unwrap().messaging.clone();
        let mut slot = handle.lock().unwrap();
        let store = slot.as_mut().unwrap();
        store.sync_admin(&json!({"action":"enable_hub"}), &[])?;
        println!(
            "{}",
            json!({"descriptor":store.space_descriptor(),"pid":std::process::id()})
        );
        drop(slot);
        messaging::sync::start(&state);
        let deadline = Instant::now() + Duration::from_secs(180);
        while Instant::now() < deadline && !root.join("STOP").exists() {
            std::thread::sleep(Duration::from_millis(100));
        }
    } else if mode == "exercise" {
        messaging::sync::start(&state);
        let wait = |test: &dyn Fn(&Store) -> bool| -> anyhow::Result<()> {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let handle = state.lock().unwrap().messaging.clone();
                if handle.lock().unwrap().as_ref().is_some_and(test) {
                    return Ok(());
                }
                if Instant::now() > deadline {
                    let handle = state.lock().unwrap().messaging.clone();
                    let status = handle.lock().unwrap().as_ref().unwrap().sync_status();
                    anyhow::bail!("Timed out waiting for remote replication: {status}");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        wait(&|s| s.sync_status()["connected"] == true && s.host_info(&s.daemon_id).is_some())?;
        call(
            &state,
            "lab-agent",
            json!({"channel":"general","name":"Latency-Probe","body":"Disposable cross-machine probe","request_id":"claim"}),
        )?;
        let mut samples = Vec::new();
        for n in 0..12 {
            let started = Instant::now();
            let sent = call(
                &state,
                "lab-agent",
                json!({"channel":"general","body":format!("Disposable latency sample {n}"),"request_id":format!("sample-{n}")}),
            )?;
            let commit = started.elapsed().as_secs_f64() * 1000.;
            let id = sent["event_id"].as_str().unwrap();
            wait(&|s| s.event_replication(id)["status"] == "replicated")?;
            samples.push(json!({"local_commit_ms":commit,"hub_ack_ms":started.elapsed().as_secs_f64()*1000.}));
        }
        println!(
            "{}",
            json!({"samples":samples,"scope":"disposable messaging-only SSH relay","includes":"durable local commit and durable hub receipt; excludes native agent wake delay"})
        );
    } else {
        anyhow::bail!("Unknown lab mode");
    }
    let runtime = state.lock().unwrap().messaging_sync.take();
    if let Some(runtime) = runtime {
        runtime.stop();
    }
    let _ = fs::write(root.join("FINISHED"), b"done");
    Ok(())
}
