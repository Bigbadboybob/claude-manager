//! Rebuild messaging away from adoption and the control/attach accept loop.
use super::{ChatError, Store};
use crate::state::DaemonState;
use std::{
    io,
    path::Path,
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, Instant},
};

pub(super) fn starting() -> ChatError {
    ChatError {
        code: "messaging_starting".into(),
        message: "Messaging is starting; retry the same request shortly".into(),
    }
}

pub fn start(state: &Arc<Mutex<DaemonState>>) -> io::Result<()> {
    start_with(state, Store::open, Duration::from_secs(5))
}

fn start_with(
    state: &Arc<Mutex<DaemonState>>,
    mut open: impl FnMut(&Path) -> Result<Store, ChatError> + Send + 'static,
    retry_delay: Duration,
) -> io::Result<()> {
    let (started, root) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.messaging_startup.clone(), s.messaging_root.clone())
    };
    if started.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    let weak = Arc::downgrade(state);
    let result = std::thread::Builder::new()
        .name("cm-messaging-open".into())
        .spawn(move || loop {
            if weak.strong_count() == 0 {
                return;
            }
            let began = Instant::now();
            // In particular, do NOT hold the store, delivery or daemon lock
            // during journal reads/rebuild or while waiting to retry an error.
            let result = open(&root);
            eprintln!(
                "cm-daemon: messaging store open took {:.3}s ({})",
                began.elapsed().as_secs_f64(),
                if result.is_ok() { "ready" } else { "failed" },
            );
            match result {
                Ok(store) => {
                    let Some(state) = weak.upgrade() else {
                        return;
                    };
                    install(&state, store);
                    drop(state);
                    // A fast open may beat holder adoption. Do not advertise
                    // an empty session graph to peers before registry restore.
                    loop {
                        let Some(state) = weak.upgrade() else {
                            return;
                        };
                        let restored = state
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .messaging_registry_restored;
                        if restored {
                            super::sync::start(&state);
                            super::delivery::signal(&state);
                            break;
                        }
                        drop(state);
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    return;
                }
                Err(e) => {
                    eprintln!("cm-daemon: messaging unavailable: {e}; retrying in {retry_delay:?}");
                    std::thread::sleep(retry_delay);
                }
            }
        });
    if let Err(e) = result {
        started.store(false, Ordering::Release);
        return Err(e);
    }
    Ok(())
}

fn install(state: &Arc<Mutex<DaemonState>>, store: Store) {
    let (handle, gate) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.messaging.clone(), s.messaging_delivery.clone())
    };
    let _guard = gate.lock().unwrap_or_else(|p| p.into_inner());
    let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
    super::rpc::project_names(state, &store, false);
    *slot = Some(store);
}

/// Fixtures that need an already-open store deliberately bypass background
/// startup; the startup tests below exercise the production worker separately.
#[cfg(test)]
pub(crate) fn open_for_test(state: &Arc<Mutex<DaemonState>>) -> Result<(), ChatError> {
    let (root, handle) = {
        let s = state.lock().unwrap();
        (s.messaging_root.clone(), s.messaging.clone())
    };
    if handle.lock().unwrap().is_none() {
        install(state, Store::open(&root)?);
    }
    super::rpc::initialize(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{
        dispatch::{dispatch_request, DispatchOutcome},
        protocol::{Caller, Request},
    };
    use serde_json::json;
    use std::sync::mpsc;

    fn state(root: &Path) -> Arc<Mutex<DaemonState>> {
        let mut s = DaemonState::default();
        s.messaging_root = root.into();
        s.messaging_registry_restored = true;
        s.tui_sessions.insert(
            "test-agent".into(),
            serde_json::from_value(json!({"uid":"test-agent","label":"test"})).unwrap(),
        );
        Arc::new(Mutex::new(s))
    }

    fn wait_ready(state: &Arc<Mutex<DaemonState>>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while super::super::rpc::initialize(state).is_err() {
            assert!(Instant::now() < deadline, "messaging never became ready");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn messaging_startup_slow_open_keeps_control_and_messaging_responsive() {
        let tmp = tempfile::tempdir().unwrap();
        let seed = Store::open(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join("messaging-sync.json"),
            serde_json::to_vec(&json!({
                "version":1, "configured":true,
                "coordinator_id":seed.daemon_id, "space_id":seed.space_id,
            }))
            .unwrap(),
        )
        .unwrap();
        drop(seed);
        let state = state(tmp.path());
        state.lock().unwrap().messaging_registry_restored = false;
        state.lock().unwrap().sessions.insert(
            "test-agent".into(),
            crate::session::DaemonSession::spawn(crate::session::SpawnParams::new(
                "test-agent",
                "test",
                "/bin/cat",
            ))
            .unwrap(),
        );
        let (entered, opening) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        start_with(
            &state,
            move |root| {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
                Store::open(root)
            },
            Duration::from_millis(5),
        )
        .unwrap();
        opening.recv_timeout(Duration::from_secs(2)).unwrap();
        // Calling start again cannot create a second opener/writer.
        start_with(&state, |_| panic!("second opener"), Duration::ZERO).unwrap();
        let probe_state = state.clone();
        let (responded, response) = mpsc::channel();
        let probe = std::thread::spawn(move || {
            let (handle, gate) = {
                let s = probe_state.try_lock().expect("opener held daemon state");
                (s.messaging.clone(), s.messaging_delivery.clone())
            };
            assert!(handle.try_lock().unwrap().is_none());
            assert!(gate.try_lock().is_ok());
            assert!(crate::control::methods::daemon_health(&probe_state).is_ok());
            for method in ["ping", "list_sessions", "session.attach", "messaging.open"] {
                let DispatchOutcome::Done(reply) = dispatch_request(
                    &probe_state,
                    &Request {
                        id: "during-open".into(),
                        caller: if method == "session.attach" {
                            Caller::operator("")
                        } else {
                            Caller::session("test-agent")
                        },
                        method: method.into(),
                        params: json!({"uid":"test-agent"}),
                    },
                ) else {
                    panic!("unexpected streaming response");
                };
                if method == "messaging.open" {
                    assert!(!reply.ok);
                    assert!(reply
                        .error
                        .unwrap()
                        .message
                        .starts_with("messaging_starting:"));
                } else {
                    assert!(reply.ok, "{reply:?}");
                }
            }
            assert!(super::super::tasks::refresh(&probe_state)
                .unwrap()
                .is_empty());
            assert_eq!(
                super::super::sync::Runtime::start(&probe_state)
                    .err()
                    .unwrap()
                    .code,
                "messaging_starting"
            );
            super::super::delivery::tick(&probe_state);
            responded.send(()).unwrap();
        });
        let result = response.recv_timeout(Duration::from_secs(2));
        // Release even on a failing probe so the test never leaves an opener.
        release.send(()).unwrap();
        result.expect("control or messaging waited for the store open");
        probe.join().unwrap();
        wait_ready(&state);
        {
            let mut s = state.lock().unwrap();
            assert!(
                s.messaging_sync.is_none(),
                "replication beat registry restoration"
            );
            s.messaging_registry_restored = true;
        }
        assert!(
            super::super::rpc::dispatch(
                &state,
                &Request {
                    id: "ready".into(),
                    caller: Caller::session("test-agent"),
                    method: "messaging.open".into(),
                    params: json!({}),
                }
            )
            .ok
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(sync) = state.lock().unwrap().messaging_sync.as_ref() {
                sync.stop();
                break;
            }
            assert!(
                Instant::now() < deadline,
                "sync did not start after store open"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn messaging_startup_retries_failed_open_without_an_rpc() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let mut attempts = 0;
        start_with(
            &state,
            move |root| {
                attempts += 1;
                if attempts == 1 {
                    Err(starting())
                } else {
                    Store::open(root)
                }
            },
            Duration::from_millis(5),
        )
        .unwrap();
        wait_ready(&state);
        assert!(state.lock().unwrap().messaging.lock().unwrap().is_some());
    }
}
