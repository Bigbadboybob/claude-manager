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
        atomic::{AtomicBool, AtomicU64, Ordering},
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
    /// Scoped peers only: also wait (until `soft_until`) for these scopes'
    /// history backfills, plus any the hub reports this call started. The
    /// reply never waits for history beyond that.
    scopes: Vec<String>,
    wait_history: bool,
    soft_until: Instant,
}
/// Replica-side view of one hub backfill (scoped-backfill peers only).
#[derive(Clone, Debug, Default)]
pub struct BackfillProgress {
    pub id: u64,
    pub done: u64,
    pub total: u64,
    pub complete: bool,
    /// Acknowledged backfill cursor: all of the scope's messages at or below it
    /// are durable here, so a reconnect can resume the backfill from it.
    pub cursor: u64,
}
/// Most "currently viewed" selectors kept as transport interests. Views no
/// longer expire (each expiry used to restart the whole replica stream); the
/// oldest is released past this bound, which never triggers a replay.
const MAX_VIEWS: usize = 256;
/// How long a `freshness="hub"` barrier waits for a newly added scope's
/// history before replying with backfill progress instead.
pub const BARRIER_HISTORY_WAIT: Duration = Duration::from_secs(5);

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
    /// Negotiated `scoped_backfill` on the current hub connection.
    pub(super) scoped: AtomicBool,
    pub(super) stream_cursor: AtomicU64,
    pub(super) stream_revision: AtomicU64,
    pub(super) backfills: Mutex<BTreeMap<String, BackfillProgress>>,
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
            scoped: AtomicBool::new(false),
            stream_cursor: AtomicU64::new(0),
            stream_revision: AtomicU64::new(1),
            backfills: Mutex::new(BTreeMap::new()),
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
        self.request_scoped(actor, method, params, None)
    }
    /// True when the hub connection negotiated per-scope backfill.
    pub fn scoped(&self) -> bool {
        self.scoped.load(Ordering::Acquire)
    }
    /// Like [`Self::request`]; with a scoped hub, `history` additionally waits
    /// up to its duration for those scopes' history backfills, and for any
    /// backfill the call itself started, to finish.
    pub fn request_scoped(
        &self,
        actor: &str,
        method: &str,
        params: &Value,
        history: Option<(Vec<String>, Duration)>,
    ) -> Result<Value> {
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
                    soft_until: Instant::now()
                        + history.as_ref().map_or(Duration::ZERO, |(_, d)| *d),
                    wait_history: history.is_some(),
                    scopes: history.map(|(scopes, _)| scopes).unwrap_or_default(),
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
        let deadline = Instant::now() + Duration::from_secs(30);
        let result = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(result) => break result,
                Err(mpsc::RecvTimeoutError::Timeout) if !left.is_zero() => {
                    // A history wait ends on time, not only on a stream frame.
                    self.poll_pending();
                }
                Err(_) => {
                    break Err(error(
                        "outcome_unknown",
                        "Coordinator response timed out; retry the same request ID",
                    ))
                }
            }
        };
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        result
    }
    /// Keep a viewed conversation streaming live. Views persist for this
    /// daemon's lifetime (bounded by [`MAX_VIEWS`]): they used to expire after
    /// 60s, and every expiry or re-view restarted the replica's whole stream.
    pub fn view(&self, selector: &str) {
        let mut views = self.views.lock().unwrap_or_else(|p| p.into_inner());
        let added = views.insert(selector.into(), Instant::now()).is_none();
        while views.len() > MAX_VIEWS {
            let oldest = views
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(k, _)| k.clone())
                .unwrap();
            views.remove(&oldest);
        }
        drop(views);
        if added {
            self.signal.signal();
        }
    }
    /// Backfill progress for `scope` (a conversation ID) while its history is
    /// still arriving from the hub.
    pub fn backfill_status(&self, scope: &str) -> Option<BackfillProgress> {
        self.backfills
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(scope)
            .filter(|b| !b.complete)
            .cloned()
    }
    /// Merge hub-reported backfill states; never regress a newer or completed one.
    pub(super) fn merge_backfills(&self, items: &Value) {
        let Some(items) = items.as_object() else {
            return;
        };
        let mut map = self.backfills.lock().unwrap_or_else(|p| p.into_inner());
        for (scope, v) in items {
            let id = v["id"].as_u64().unwrap_or(0);
            let incoming = BackfillProgress {
                id,
                done: v["done"].as_u64().unwrap_or(0),
                total: v["total"].as_u64().unwrap_or(0),
                complete: v["complete"] == true,
                cursor: v["cursor"].as_u64().unwrap_or(0),
            };
            match map.get_mut(scope) {
                Some(old) if old.id == id => {
                    old.done = old.done.max(incoming.done);
                    old.cursor = old.cursor.max(incoming.cursor);
                    old.complete |= incoming.complete;
                }
                Some(old) if old.id > id => {}
                _ => {
                    map.insert(scope.clone(), incoming);
                }
            }
        }
    }
    fn interests(&self) -> BTreeSet<String> {
        let views = self.views.lock().unwrap_or_else(|p| p.into_inner());
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
    fn poll_pending(&self) {
        self.finish_pending(
            self.stream_cursor.load(Ordering::Acquire),
            self.stream_revision.load(Ordering::Acquire),
        );
    }
    fn finish_pending(&self, cursor: u64, revision: u64) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let ready = pending
            .iter()
            .filter(|(_, p)| p.response.is_some() && p.barrier <= cursor && p.revision <= revision)
            .filter(|(_, p)| {
                now >= p.soft_until
                    || !p.wait_history
                    || p.scopes.iter().all(|scope| self.backfill_status(scope).is_none())
            })
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
