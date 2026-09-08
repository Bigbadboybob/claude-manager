//! Daemon-owned messaging transport. The separate peer protocol cannot dispatch
//! session-control RPCs; SSH carries its already scoped authenticated stream.
use super::{ChatError, Person, Store};
use crate::state::DaemonState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

mod transport;
pub use transport::stdio_bridge;
type Result<T> = std::result::Result<T, ChatError>;
fn error(code: &str, message: impl Into<String>) -> ChatError {
    ChatError {
        code: code.into(),
        message: message.into(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Endpoint {
    Unix {
        path: PathBuf,
    },
    Ssh {
        host: String,
        binary: String,
        root: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub coordinator_id: String,
    pub space_id: String,
    #[serde(default)]
    pub endpoint: Option<Endpoint>,
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    #[serde(default)]
    pub configured: bool,
}
impl Config {
    pub fn load(root: &Path) -> Result<Option<Self>> {
        let path = root.join("messaging-sync.json");
        if !path.exists() {
            return Ok(None);
        }
        let config: Self = serde_json::from_slice(&fs::read(path)?)?;
        if config.version != 1 {
            return Err(error(
                "invalid_config",
                "Unsupported messaging transport version",
            ));
        }
        for id in [&config.coordinator_id, &config.space_id] {
            uuid::Uuid::parse_str(id)
                .map_err(|_| error("invalid_config", "Invalid messaging UUID"))?;
        }
        Ok(Some(config))
    }
}
struct Command {
    id: String,
    actor: String,
    method: String,
    params: Value,
}
struct Pending {
    reply: mpsc::SyncSender<Result<Value>>,
    response: Option<Value>,
    barrier: u64,
    revision: u64,
}

pub struct Runtime {
    state: Weak<Mutex<DaemonState>>,
    pub(super) store: Arc<Mutex<Option<Store>>>,
    pub(super) root: PathBuf,
    pub(super) daemon_id: String,
    pub(super) config: Config,
    pub(super) connected: AtomicBool,
    pub(super) stopped: AtomicBool,
    signal: Arc<super::store::ChangeSignal>,
    outgoing: mpsc::SyncSender<Command>,
    commands: Mutex<mpsc::Receiver<Command>>,
    pending: Mutex<BTreeMap<String, Pending>>,
    views: Mutex<BTreeMap<String, Instant>>,
    upload_retry: Mutex<BTreeMap<String, Instant>>,
    peers: Mutex<BTreeMap<String, transport::PeerConnection>>,
}

impl Runtime {
    pub fn start(state: &Arc<Mutex<DaemonState>>) -> Result<Option<Arc<Self>>> {
        let (root, handle) = {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            (s.messaging_root.clone(), s.messaging.clone())
        };
        let Some(config) = Config::load(&root)?.filter(|c| c.configured) else {
            return Ok(None);
        };
        let (daemon_id, signal, is_hub) = {
            let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_none() {
                *slot = Some(Store::open(&root)?);
            }
            let store = slot.as_ref().unwrap();
            if store.space_id != config.space_id || store.coordinator_id() != config.coordinator_id
            {
                return Err(error(
                    "invalid_config",
                    "Transport and store identities disagree",
                ));
            }
            (
                store.daemon_id.clone(),
                store.change_signal(),
                store.is_coordinator(),
            )
        };
        let (outgoing, commands) = mpsc::sync_channel(64);
        let runtime = Arc::new(Self {
            state: Arc::downgrade(state),
            store: handle,
            root,
            daemon_id,
            config,
            connected: AtomicBool::new(is_hub),
            stopped: AtomicBool::new(false),
            signal,
            outgoing,
            commands: Mutex::new(commands),
            pending: Mutex::new(BTreeMap::new()),
            views: Mutex::new(BTreeMap::new()),
            upload_retry: Mutex::new(BTreeMap::new()),
            peers: Mutex::new(BTreeMap::new()),
        });
        if is_hub {
            transport::serve(runtime.clone())?;
        } else {
            if runtime.config.endpoint.is_none() || runtime.config.token_file.is_none() {
                return Err(error(
                    "invalid_config",
                    "Replica needs an endpoint and token file",
                ));
            }
            let cloned = runtime.clone();
            std::thread::Builder::new()
                .name("cm-chat-sync".into())
                .spawn(move || transport::run_client(cloned))?;
        }
        Ok(Some(runtime))
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if self.daemon_id == self.config.coordinator_id {
            let _ = fs::remove_file(self.root.join("messaging-sync.sock"));
        }
        self.signal.signal();
        self.fail_pending("Messaging transport stopped");
        for peer in self
            .peers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            peer.close();
        }
    }
    pub fn disconnect_peer(&self, host: &str) {
        if let Some(peer) = self
            .peers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(host)
        {
            peer.close();
        }
        self.signal.signal();
    }
    pub fn request(&self, actor: &str, method: &str, params: &Value) -> Result<Value> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(error(
                "coordinator_unavailable",
                "Messaging coordinator is offline; keep this request ID and draft",
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                id.clone(),
                Pending {
                    reply: tx,
                    response: None,
                    barrier: u64::MAX,
                    revision: 0,
                },
            );
        if self
            .outgoing
            .try_send(Command {
                id: id.clone(),
                actor: actor.into(),
                method: method.into(),
                params: params.clone(),
            })
            .is_err()
        {
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(error(
                "sync_busy",
                "Messaging request queue is full; retry the same request",
            ));
        }
        self.signal.signal();
        let result = rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| {
                Err(error(
                    "outcome_unknown",
                    "Coordinator response timed out; retry the same request ID",
                ))
            });
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        result
    }
    pub fn view(&self, selector: &str) {
        self.views
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(selector.into(), Instant::now());
        self.signal.signal();
    }
    fn interests(&self) -> BTreeSet<String> {
        let mut views = self.views.lock().unwrap_or_else(|p| p.into_inner());
        views.retain(|_, time| time.elapsed() < Duration::from_secs(60));
        let mut out = views.keys().cloned().collect::<BTreeSet<_>>();
        drop(views);
        if let Some(store) = self
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            out.extend(store.local_transport_interests());
        }
        out
    }
    fn local_people(&self) -> Vec<Person> {
        let Some(state) = self.state.upgrade() else {
            return Vec::new();
        };
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        s.sessions
            .values()
            .map(|p| (p.uid.clone(), p.title.clone(), p.task_id.clone()))
            .chain(
                s.tui_sessions
                    .values()
                    .filter(|t| !s.sessions.contains_key(&t.uid))
                    .map(|t| {
                        (
                            t.uid.clone(),
                            t.label.clone().unwrap_or_else(|| t.uid.clone()),
                            t.task_id.clone(),
                        )
                    }),
            )
            .map(|(uid, name, task)| Person {
                id: format!("agent:{}:{uid}", self.daemon_id),
                name,
                session_uid: uid,
                task,
                present: true,
                kind: "agent".into(),
            })
            .collect()
    }
    fn fail_pending(&self, reason: &str) {
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|p| p.into_inner()));
        for (_, p) in pending {
            let _ = p.reply.send(Err(error(
                "outcome_unknown",
                format!("{reason}; retry the same request ID"),
            )));
        }
    }
    fn finish_pending(&self, cursor: u64, revision: u64) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let ready = pending
            .iter()
            .filter(|(_, p)| p.response.is_some() && p.barrier <= cursor && p.revision <= revision)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ready {
            if let Some(p) = pending.remove(&id) {
                let _ = p.reply.send(Ok(p.response.unwrap()));
            }
        }
    }
    fn publish_to_sessions(&self) {
        if let Some(state) = self.state.upgrade() {
            {
                let slot = self.store.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(store) = slot.as_ref() {
                    super::rpc::project_names(&state, store, true);
                }
            }
            super::delivery::signal(&state);
        }
    }
}

pub fn start(state: &Arc<Mutex<DaemonState>>) {
    match Runtime::start(state) {
        Ok(runtime) => {
            state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .messaging_sync = runtime
        }
        Err(e) => eprintln!("cm messaging sync: {e}"),
    }
}

#[cfg(test)]
mod tests;
