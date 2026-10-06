use super::*;
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    process::{Child, Command as ProcessCommand, Stdio},
    thread,
};
const MAX_FRAME: usize = 4 * 1024 * 1024;
type Writer = Arc<Mutex<Box<dyn Write + Send>>>;
pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn read_frame(reader: &mut dyn Read) -> io::Result<Value> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid messaging frame length",
        ));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
fn write_frame(writer: &Writer, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Messaging frame exceeds 4 MiB",
        ));
    }
    let mut writer = writer.lock().unwrap_or_else(|p| p.into_inner());
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}
fn wire_error(e: ChatError) -> Value {
    json!({"code":e.code,"message":e.message})
}
fn unmarshal_error(v: &Value) -> ChatError {
    error(
        v["code"].as_str().unwrap_or("sync_error"),
        v["message"].as_str().unwrap_or("Messaging transport error"),
    )
}

#[derive(Clone)]
pub(super) struct PeerConnection {
    writer: Writer,
    closed: Arc<AtomicBool>,
    shutdown: Arc<dyn Fn() + Send + Sync>,
}
impl PeerConnection {
    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        (self.shutdown)();
    }
}
struct Link {
    reader: Box<dyn Read + Send>,
    connection: PeerConnection,
}
// Every exit path closes pipes/sockets, including failed handshakes and spawn.
struct ConnectionGuard(PeerConnection);
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}
fn unix_link(stream: UnixStream) -> io::Result<Link> {
    stream.set_read_timeout(Some(Duration::from_secs(75)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let stop = stream.try_clone()?;
    Ok(Link {
        reader: Box::new(stream.try_clone()?),
        connection: PeerConnection {
            writer: Arc::new(Mutex::new(Box::new(stream))),
            closed: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(move || {
                let _ = stop.shutdown(std::net::Shutdown::Both);
            }),
        },
    })
}
fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn connect(endpoint: &Endpoint) -> io::Result<Link> {
    match endpoint {
        Endpoint::Unix { path } => unix_link(UnixStream::connect(path)?),
        Endpoint::Ssh { host, binary, root } => {
            if host.is_empty() || host.starts_with('-') || host.chars().any(char::is_whitespace) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Invalid SSH host",
                ));
            }
            let remote = format!(
                "{} --messaging-sync-stdio {}",
                shell_word(binary),
                shell_word(root)
            );
            let mut child = ProcessCommand::new("ssh")
                .args([
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=15",
                    "-o",
                    "ServerAliveInterval=15",
                    "-o",
                    "ServerAliveCountMax=3",
                    "--",
                    host,
                    &remote,
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?;
            let input = child.stdin.take().unwrap();
            let output = child.stdout.take().unwrap();
            let child: Arc<Mutex<Child>> = Arc::new(Mutex::new(child));
            Ok(Link {
                reader: Box::new(output),
                connection: PeerConnection {
                    writer: Arc::new(Mutex::new(Box::new(input))),
                    closed: Arc::new(AtomicBool::new(false)),
                    shutdown: Arc::new(move || {
                        let mut child = child.lock().unwrap_or_else(|p| p.into_inner());
                        let _ = child.kill();
                        let _ = child.wait();
                    }),
                },
            })
        }
    }
}
/// SSH helper. It owns no store and cannot forward to the control socket.
pub fn stdio_bridge(root: &Path) -> io::Result<()> {
    let stream = UnixStream::connect(root.join("messaging-sync.sock"))?;
    let mut input = stream.try_clone()?;
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut input);
        let _ = input.shutdown(std::net::Shutdown::Write);
    });
    // Stdout is line buffered even when bridged to SSH. Compact framed JSON
    // contains no literal newline: flush each chunk or the hello can wait forever.
    let mut output = io::stdout().lock();
    let mut input = &stream;
    let mut buffer = [0u8; 32768];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n])?;
        output.flush()?;
    }
    Ok(())
}
#[derive(Deserialize)]
struct PeerKey {
    token_sha256: String,
    #[serde(default)]
    owner_access: bool,
    #[serde(default)]
    active: bool,
}
fn authenticate(runtime: &Runtime, hello: &Value) -> Result<(String, bool, bool)> {
    let host = hello["daemon_id"]
        .as_str()
        .ok_or_else(|| error("unauthorized", "Missing peer identity"))?;
    uuid::Uuid::parse_str(host).map_err(|_| error("unauthorized", "Invalid peer identity"))?;
    if host == runtime.daemon_id || hello["space_id"] != runtime.config.space_id {
        return Err(error("unauthorized", "Wrong messaging space/peer"));
    }
    let token = hello["token"].as_str().unwrap_or("");
    if !(32..=512).contains(&token.len()) {
        return Err(error("unauthorized", "Invalid pairing token"));
    }
    let path = runtime
        .root
        .join("messaging-peers")
        .join(format!("{host}.json"));
    let key: PeerKey = serde_json::from_slice(
        &fs::read(path).map_err(|_| error("unauthorized", "Peer is not paired"))?,
    )?;
    let supplied = digest(token.as_bytes());
    let different = key
        .token_sha256
        .as_bytes()
        .iter()
        .zip(supplied.as_bytes())
        .fold(0u8, |diff, (a, b)| diff | (a ^ b));
    if key.token_sha256.len() != supplied.len() || different != 0 {
        return Err(error(
            "unauthorized",
            "Pairing was revoked or token is invalid",
        ));
    }
    Ok((host.into(), key.owner_access, key.active))
}
/// Hello feature: per-scope progress. Both peers must list it; otherwise the
/// connection keeps the legacy single-cursor stream, which restarts from zero
/// whenever the resolved interest set changes.
///
/// With it, the main stream keeps its cursor for every covered scope. A scope
/// added later is backfilled from its own cursor (zero, or the replica's
/// recorded coverage) in separate `backfill` pages while the main stream goes
/// on; the main stream carries that scope only after the backfill's fixed
/// `through` position. Shrinking the set never replays anything.
pub(super) const FEATURE_SCOPED: &str = "scoped_backfill";
/// Bulk page bounds `(records, bytes)` on scoped connections. Each page costs
/// an acknowledgement round trip over SSH; a 4 MiB frame comfortably carries
/// 2 MiB of records plus framing.
pub(super) const BULK_PAGE: (usize, usize) = (1024, 2 * 1024 * 1024);

struct Backfill {
    id: u64,
    /// Acknowledged: every message of the scope in `(.., cursor]` is durable
    /// at the replica (together with its earlier coverage).
    cursor: u64,
    dependency_offset: usize,
    /// Fixed when the scope was added. The main stream carries the scope only
    /// above it, so the backfill must reach it before coverage is claimed.
    through: u64,
    total: u64,
    done: u64,
    /// Outstanding page: (next cursor, dependency offset, done after ack).
    sent: Option<(u64, usize, u64)>,
}
impl Backfill {
    fn complete(&self) -> bool {
        self.cursor >= self.through && self.dependency_offset == 0 && self.sent.is_none()
    }
    fn state(&self) -> Value {
        json!({"id":self.id,"done":self.done,"total":self.total,"complete":self.complete(),"cursor":self.cursor,"through":self.through})
    }
}
struct Subscription {
    interests: BTreeSet<String>,
    resolved: BTreeSet<String>,
    revision: u64,
    cursor: u64,
    dependency_offset: usize,
    sent: Option<(u64, u64, usize)>,
    /// `FEATURE_SCOPED` negotiated. Everything below is unused otherwise.
    scoped: bool,
    /// Scopes the main stream carries in full from `cursor`.
    covered: BTreeSet<String>,
    backfills: BTreeMap<String, Backfill>,
    /// Replica-reported coverage per scope: where a (re-)added scope's
    /// backfill may start instead of zero.
    hints: BTreeMap<String, u64>,
    next_backfill: u64,
    /// Backfill states/cancellations the replica has not been told yet.
    announce: BTreeMap<String, Value>,
}
impl Subscription {
    fn joined(&self) -> BTreeMap<String, u64> {
        self.backfills
            .iter()
            .map(|(scope, b)| (scope.clone(), b.through))
            .collect()
    }
    fn backfill_states(&self) -> Value {
        Value::Object(
            self.backfills
                .iter()
                .map(|(scope, b)| (scope.clone(), b.state()))
                .collect(),
        )
    }
    /// Scoped mode: fold the host's current resolved interests into
    /// per-scope progress. New scopes start a backfill; removed scopes are
    /// dropped (never replayed); finished backfills join the covered set once
    /// the main stream has passed their `through`.
    fn reconcile(&mut self, store: &Store, host: &str) -> Result<Vec<String>> {
        let mut started = Vec::new();
        let now = store.host_transport_interests(host, &self.interests)?;
        let high = store.publication_position();
        let known = self
            .covered
            .iter()
            .chain(self.backfills.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for scope in now.difference(&known) {
            let start = self.hints.get(scope).copied().unwrap_or(0);
            if start >= high {
                self.covered.insert(scope.clone());
                continue;
            }
            self.next_backfill += 1;
            let backfill = Backfill {
                id: self.next_backfill,
                cursor: start,
                dependency_offset: 0,
                through: high,
                total: store.scope_message_count(scope, start, high),
                done: 0,
                sent: None,
            };
            self.announce.insert(scope.clone(), backfill.state());
            self.backfills.insert(scope.clone(), backfill);
            started.push(scope.clone());
        }
        // A removal could change which event a partially sent reply chain
        // resumes at; apply it only on a main-stream page boundary.
        if self.dependency_offset == 0 && self.sent.is_none() {
            for scope in known.difference(&now) {
                // Remember how far the replica holds this scope, so adding it
                // again only fetches what it missed meanwhile.
                let held = if self.covered.remove(scope) {
                    Some(self.cursor)
                } else {
                    self.backfills.remove(scope).map(|b| {
                        self.announce
                            .insert(scope.clone(), json!({"id":b.id,"cancelled":true}));
                        b.cursor
                    })
                };
                if let Some(held) = held {
                    let hint = self.hints.entry(scope.clone()).or_default();
                    *hint = (*hint).max(held);
                }
            }
        }
        let cursor = self.cursor;
        let promoted = self
            .backfills
            .iter()
            .filter(|(_, b)| b.complete() && cursor >= b.through)
            .map(|(scope, _)| scope.clone())
            .collect::<Vec<_>>();
        for scope in promoted {
            self.backfills.remove(&scope);
            self.covered.insert(scope);
        }
        self.resolved = self
            .covered
            .iter()
            .chain(self.backfills.keys())
            .cloned()
            .collect();
        Ok(started)
    }
}
fn hints_from(value: &Value) -> BTreeMap<String, u64> {
    value
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_u64()?)))
                .collect()
        })
        .unwrap_or_default()
}
fn has_feature(frame: &Value, feature: &str) -> bool {
    frame["features"]
        .as_array()
        .is_some_and(|f| f.iter().any(|v| v == feature))
}

pub(super) fn serve(runtime: Arc<Runtime>) -> Result<()> {
    let socket = runtime.root.join("messaging-sync.sock");
    if socket.exists() {
        if UnixStream::connect(&socket).is_ok() {
            return Err(error(
                "writer_busy",
                "Messaging replication socket is already serving",
            ));
        }
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    thread::Builder::new()
        .name("cm-chat-peer-listener".into())
        .spawn(move || {
            let mut directory_refresh = Instant::now();
            while !runtime.stopped.load(Ordering::Acquire) {
                if Instant::now() >= directory_refresh {
                    let people = runtime.local_people();
                    let mut slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
                    if !runtime.stopped.load(Ordering::Acquire) {
                        if let Some(store) = slot.as_mut().filter(|s| !s.messaging_frozen()) {
                            if let Err(e) =
                                store.register_host(&runtime.daemon_id, true, true, &people)
                            {
                                eprintln!("cm messaging directory: {e}");
                            }
                        }
                    }
                    directory_refresh = Instant::now() + Duration::from_secs(20);
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        if active.load(Ordering::Acquire) >= 64 {
                            drop(stream);
                            continue;
                        }
                        active.fetch_add(1, Ordering::AcqRel);
                        let count = active.clone();
                        let cloned = runtime.clone();
                        let result =
                            thread::Builder::new()
                                .name("cm-chat-peer".into())
                                .spawn(move || {
                                    if let Err(e) = serve_peer(&cloned, stream) {
                                        eprintln!("cm messaging peer: {e}");
                                    }
                                    count.fetch_sub(1, Ordering::AcqRel);
                                });
                        if result.is_err() {
                            active.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50))
                    }
                    Err(e) => {
                        eprintln!("cm messaging listener: {e}");
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        })?;
    Ok(())
}
fn serve_peer(runtime: &Arc<Runtime>, stream: UnixStream) -> Result<()> {
    let timer = stream.try_clone()?;
    let mut link = unix_link(stream)?;
    timer.set_read_timeout(Some(Duration::from_secs(10)))?;
    let _close = ConnectionGuard(link.connection.clone());
    let hello = read_frame(link.reader.as_mut())?;
    let (host, owner_access, active) = match authenticate(runtime, &hello) {
        Ok(peer) => peer,
        Err(e) => {
            let _ = write_frame(
                &link.connection.writer,
                &json!({"kind":"error","error":wire_error(e)}),
            );
            return Ok(());
        }
    };
    timer.set_read_timeout(Some(Duration::from_secs(75)))?;
    let revoked = !active
        || runtime
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .unwrap()
            .host_info(&host)
            .is_some_and(|h| h["active"] != true);
    if revoked {
        if let Some(old) = runtime
            .peers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(host.clone(), link.connection.clone())
        {
            old.close();
        }
        return serve_revoked(runtime, &host, owner_access, &mut link);
    }
    let people: Vec<Person> = serde_json::from_value(hello["people"].clone())?;
    if people.len() > 512 {
        return Err(error(
            "invalid_params",
            "Peer directory exceeds 512 live participants",
        ));
    }
    let own_people = runtime.local_people();
    {
        let mut slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
        let store = slot.as_mut().unwrap();
        if store.host_info(&host).is_some_and(|h| h["active"] != true) {
            return Err(error("host_revoked", "Host enrollment is revoked"));
        }
        store.register_host(&runtime.daemon_id, true, true, &own_people)?;
        store.register_host(&host, true, owner_access, &people)?;
    }
    let initial: BTreeSet<String> = serde_json::from_value(hello["interests"].clone())?;
    let scoped = has_feature(&hello, FEATURE_SCOPED);
    let (generation, subscription, live_start) = {
        let slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
        let store = slot.as_ref().unwrap();
        let resolved = store.host_transport_interests(&host, &initial)?;
        let resume = &hello["resume"];
        let old_scopes: BTreeSet<String> =
            serde_json::from_value(resume["scopes"].clone()).unwrap_or_default();
        let same_generation = resume["generation"] == store.generation;
        let resumed = || {
            resume["cursor"]
                .as_u64()
                .filter(|c| *c <= store.publication_position())
        };
        let mut sub = Subscription {
            interests: initial,
            resolved: resolved.clone(),
            revision: 1,
            cursor: 0,
            dependency_offset: 0,
            sent: None,
            scoped,
            covered: BTreeSet::new(),
            backfills: BTreeMap::new(),
            hints: BTreeMap::new(),
            next_backfill: 0,
            announce: BTreeMap::new(),
        };
        if !scoped {
            if same_generation && old_scopes == resolved {
                sub.cursor = resumed().unwrap_or(0);
            }
        } else if let Some(cursor) = resumed().filter(|_| same_generation) {
            // Unchanged scopes resume at the stored cursor; only scopes the
            // checkpoint did not cover are backfilled.
            sub.cursor = cursor;
            sub.covered = old_scopes.intersection(&resolved).cloned().collect();
            if hello["coverage_generation"] == store.generation {
                sub.hints = hints_from(&hello["coverage"]);
            }
            sub.reconcile(store, &host)?;
        } else {
            // First contact or a new hub generation: the main stream replays
            // every resolved scope from zero, as before.
            sub.covered = resolved;
        }
        (
            store.generation.clone(),
            Arc::new(Mutex::new(sub)),
            store.publication_position(),
        )
    };
    let cursor = subscription
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .cursor;
    let mut reply = json!({"kind":"hello","coordinator_id":runtime.daemon_id,"space_id":runtime.config.space_id,"generation":generation,"cursor":cursor,"revision":1});
    if scoped {
        reply["features"] = json!([FEATURE_SCOPED]);
    }
    write_frame(&link.connection.writer, &reply)?;
    if let Some(old) = runtime
        .peers
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(host.clone(), link.connection.clone())
    {
        old.close();
    }
    let cloned = runtime.clone();
    let peer = host.clone();
    let conn = link.connection.clone();
    let sub = subscription.clone();
    thread::Builder::new()
        .name("cm-chat-peer-push".into())
        .spawn(move || {
            if let Err(e) = push_loop(&cloned, &peer, &conn, &sub, live_start) {
                eprintln!("cm messaging push: {e}");
            }
            conn.close();
            cloned.signal.signal();
        })?;
    let result = (|| {
        while !runtime.stopped.load(Ordering::Acquire)
            && !link.connection.closed.load(Ordering::Acquire)
        {
            let frame = read_frame(link.reader.as_mut())?;
            match frame["kind"].as_str().unwrap_or("") {
                "ack" => {
                    let cursor = frame["cursor"]
                        .as_u64()
                        .ok_or_else(|| error("invalid_cursor", "Missing acknowledgement cursor"))?;
                    let revision = frame["revision"].as_u64().unwrap_or(0);
                    let mut sub = subscription.lock().unwrap_or_else(|p| p.into_inner());
                    if revision == sub.revision {
                        let offset = frame["dependency_offset"].as_u64().unwrap_or(0) as usize;
                        if sub.sent != Some((cursor, revision, offset)) {
                            return Err(error(
                                "invalid_cursor",
                                "Acknowledgement does not match the outstanding page",
                            ));
                        }
                        sub.cursor = cursor;
                        sub.dependency_offset = offset;
                        sub.sent = None;
                    }
                    runtime.signal.signal();
                }
                "backfill_ack" => {
                    let scope = frame["scope"].as_str().unwrap_or("");
                    let cursor = frame["cursor"]
                        .as_u64()
                        .ok_or_else(|| error("invalid_cursor", "Missing backfill cursor"))?;
                    let offset = frame["dependency_offset"].as_u64().unwrap_or(0) as usize;
                    let mut sub = subscription.lock().unwrap_or_else(|p| p.into_inner());
                    // A cancelled or replaced backfill's late ack is harmless.
                    if let Some(b) = sub
                        .backfills
                        .get_mut(scope)
                        .filter(|b| frame["id"].as_u64() == Some(b.id))
                    {
                        match b.sent {
                            Some((next, o, done)) if next == cursor && o == offset => {
                                b.cursor = cursor;
                                b.dependency_offset = offset;
                                b.done = done;
                                b.sent = None;
                            }
                            _ => {
                                return Err(error(
                                    "invalid_cursor",
                                    "Backfill acknowledgement does not match the outstanding page",
                                ))
                            }
                        }
                    }
                    runtime.signal.signal();
                }
                "interests" if subscription.lock().unwrap_or_else(|p| p.into_inner()).scoped => {
                    let interests: BTreeSet<String> =
                        serde_json::from_value(frame["interests"].clone())?;
                    let generation = runtime
                        .store
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .as_ref()
                        .unwrap()
                        .generation
                        .clone();
                    let mut sub = subscription.lock().unwrap_or_else(|p| p.into_inner());
                    if frame["coverage_generation"] == generation.as_str() {
                        for (scope, through) in hints_from(&frame["coverage"]) {
                            let hint = sub.hints.entry(scope).or_default();
                            *hint = (*hint).max(through);
                        }
                    }
                    // No reset: the push loop backfills only added scopes.
                    sub.interests = interests;
                    drop(sub);
                    runtime.signal.signal();
                }
                "interests" => {
                    let interests: BTreeSet<String> =
                        serde_json::from_value(frame["interests"].clone())?;
                    let mut sub = subscription.lock().unwrap_or_else(|p| p.into_inner());
                    if interests != sub.interests {
                        sub.interests = interests;
                        sub.revision += 1;
                        sub.cursor = 0;
                        sub.dependency_offset = 0;
                        sub.sent = None;
                    }
                    runtime.signal.signal();
                }
                "upload" => {
                    let outcome = runtime
                        .store
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .as_mut()
                        .unwrap()
                        .accept_upload(&host, &frame["wire"]);
                    let reply = match outcome {
                        Ok(receipt) => json!({"kind":"receipt","id":frame["id"],"receipt":receipt}),
                        Err(e) => {
                            json!({"kind":"rejection","id":frame["id"],"error":wire_error(e)})
                        }
                    };
                    write_frame(&link.connection.writer, &reply)?;
                    runtime.publish_to_sessions();
                }
                "call" => {
                    let actor = frame["actor"].as_str().unwrap_or("");
                    let method = frame["method"].as_str().unwrap_or("");
                    let people: Vec<Person> = serde_json::from_value(frame["people"].clone())?;
                    if people.len() > 512 {
                        return Err(error(
                            "invalid_params",
                            "Peer directory exceeds 512 live participants",
                        ));
                    }
                    let outcome = {
                        let mut slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
                        let store = slot.as_mut().unwrap();
                        if !store.host_authorized(&host, None) {
                            return Err(error("host_revoked", "Host enrollment was revoked"));
                        }
                        store.register_host(&host, true, owner_access, &people)?;
                        if !store.host_authorized(&host, Some(actor)) {
                            return Err(error(
                                "unauthorized",
                                "Host cannot act for this participant",
                            ));
                        }
                        let mut result = if method == "sync.barrier" {
                            let requested: BTreeSet<String> =
                                serde_json::from_value(frame["params"]["interests"].clone())?;
                            let mut sub = subscription.lock().unwrap_or_else(|p| p.into_inner());
                            sub.interests.extend(requested);
                            if !sub.scoped {
                                sub.revision += 1;
                                sub.cursor = 0;
                                sub.dependency_offset = 0;
                                sub.sent = None;
                            }
                            Ok(json!({"caught_up":true}))
                        } else {
                            store.coordinate(&host, actor, method, &frame["params"])
                        };
                        if actor == "owner" {
                            if let Ok(v) = &mut result {
                                v["owner_snapshot"] = store.owner_snapshot()?;
                            }
                        }
                        // Scoped: start any backfill this call implies (a join,
                        // a barrier's new interest) before replying, so the
                        // reply can tell the replica what history is coming.
                        let mut sub = subscription.lock().unwrap_or_else(|p| p.into_inner());
                        let backfills = if sub.scoped && result.is_ok() {
                            let started = sub.reconcile(store, &host)?;
                            Some((sub.backfill_states(), started))
                        } else {
                            None
                        };
                        (result, store.publication_position(), sub.revision, backfills)
                    };
                    let (outcome, barrier, revision, backfills) = outcome;
                    let reply = match outcome {
                        Ok(result) => {
                            let mut reply = json!({"kind":"reply","id":frame["id"],"result":result,"barrier":barrier,"revision":revision});
                            if let Some((backfills, started)) = backfills {
                                reply["backfills"] = backfills;
                                reply["backfill_started"] = json!(started);
                            }
                            reply
                        }
                        Err(e) => json!({"kind":"reply","id":frame["id"],"error":wire_error(e)}),
                    };
                    write_frame(&link.connection.writer, &reply)?;
                    runtime.signal.signal();
                    runtime.publish_to_sessions();
                }
                "ping" => {
                    let people: Vec<Person> = serde_json::from_value(frame["people"].clone())?;
                    if people.len() > 512 {
                        return Err(error(
                            "invalid_params",
                            "Peer directory exceeds 512 live participants",
                        ));
                    }
                    let mut slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
                    let store = slot.as_mut().unwrap();
                    if !store.host_authorized(&host, None) {
                        return Err(error("host_revoked", "Host enrollment was revoked"));
                    }
                    store.register_host(&host, true, owner_access, &people)?;
                    drop(slot);
                    write_frame(&link.connection.writer, &json!({"kind":"pong"}))?;
                }
                _ => {
                    return Err(error(
                        "unsupported_feature",
                        "Unknown messaging peer command",
                    ))
                }
            }
        }
        Ok(())
    })();
    link.connection.close();
    runtime.signal.signal();
    let current = {
        let mut peers = runtime.peers.lock().unwrap_or_else(|p| p.into_inner());
        let same = peers
            .get(&host)
            .is_some_and(|c| Arc::ptr_eq(&c.closed, &link.connection.closed));
        if same {
            peers.remove(&host);
        }
        same
    };
    if current && !runtime.stopped.load(Ordering::Acquire) {
        let mut slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
        let store = slot.as_mut().unwrap();
        if let Some(info) = store.host_info(&host).filter(|h| h["active"] == true) {
            let mut people: Vec<Person> = serde_json::from_value(info["people"].clone())?;
            for p in &mut people {
                p.present = false;
            }
            store.register_host(&host, true, owner_access, &people)?;
        }
    }
    result
}
/// A revoked pairing can reconcile only bytes it already owns. This is needed
/// when the hub accepted a message but its receipt was lost before revocation.
/// It receives no conversation history, metadata edits or session-control access.
fn serve_revoked(runtime: &Arc<Runtime>, host: &str, owner: bool, link: &mut Link) -> Result<()> {
    let (generation, wire) = {
        let mut slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
        let store = slot.as_mut().unwrap();
        let people = store
            .host_info(host)
            .and_then(|v| serde_json::from_value::<Vec<Person>>(v["people"].clone()).ok())
            .unwrap_or_default();
        store.register_host(host, false, owner, &people)?;
        (store.generation.clone(), store.revocation_wire(host)?)
    };
    write_frame(
        &link.connection.writer,
        &json!({"kind":"hello","revoked":true,"coordinator_id":runtime.daemon_id,"space_id":runtime.config.space_id,"generation":generation,"cursor":0,"revision":1}),
    )?;
    write_frame(
        &link.connection.writer,
        &json!({"kind":"enrollment","wire":wire}),
    )?;
    while !runtime.stopped.load(Ordering::Acquire)
        && !link.connection.closed.load(Ordering::Acquire)
    {
        let frame = read_frame(link.reader.as_mut())?;
        match frame["kind"].as_str().unwrap_or("") {
            "upload" => {
                let result = runtime
                    .store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_mut()
                    .unwrap()
                    .accept_upload(host, &frame["wire"]);
                let reply = match result {
                    Ok(receipt) => json!({"kind":"receipt","id":frame["id"],"receipt":receipt}),
                    Err(e) => json!({"kind":"rejection","id":frame["id"],"error":wire_error(e)}),
                };
                write_frame(&link.connection.writer, &reply)?;
            }
            "interests" => {}
            "ping" => {
                write_frame(&link.connection.writer, &json!({"kind":"pong"}))?;
            }
            "call" => {
                write_frame(
                    &link.connection.writer,
                    &json!({"kind":"reply","id":frame["id"],"error":{"code":"host_revoked","message":"Host enrollment revoked"}}),
                )?;
            }
            _ => {
                return Err(error(
                    "unauthorized",
                    "Revoked hosts may reconcile their existing uploads only",
                ))
            }
        }
    }
    Ok(())
}
fn push_loop(
    runtime: &Arc<Runtime>,
    host: &str,
    conn: &PeerConnection,
    subscription: &Arc<Mutex<Subscription>>,
    mut live_cursor: u64,
) -> Result<()> {
    if subscription
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .scoped
    {
        return push_loop_scoped(runtime, host, conn, subscription, live_cursor);
    }
    let mut last_owner = Value::Null;
    while !conn.closed.load(Ordering::Acquire) && !runtime.stopped.load(Ordering::Acquire) {
        let signal = runtime.signal.version();
        let (interests, previous_resolved, revision, cursor, dependency_offset, inflight) = {
            let sub = subscription.lock().unwrap_or_else(|p| p.into_inner());
            (
                sub.interests.clone(),
                sub.resolved.clone(),
                sub.revision,
                sub.cursor,
                sub.dependency_offset,
                sub.sent.is_some(),
            )
        };
        if inflight {
            runtime.signal.wait(signal, Duration::from_secs(20));
            continue;
        }
        let (resolved, high, page, live) = {
            let slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
            let store = slot.as_ref().unwrap();
            let resolved = store.host_transport_interests(host, &interests)?;
            let high = store.publication_position();
            let page = if resolved == previous_resolved {
                store.export_slice(host, &resolved, cursor.min(high), high, dependency_offset)?
            } else {
                Value::Null
            };
            let live = store.export_live(
                host,
                &resolved,
                cursor.min(high),
                live_cursor.max(cursor).min(high),
                high,
            )?;
            (resolved, high, page, live)
        };
        let mut sub = subscription.lock().unwrap_or_else(|p| p.into_inner());
        if sub.revision != revision {
            continue;
        }
        if resolved != sub.resolved {
            sub.resolved = resolved;
            sub.revision += 1;
            sub.cursor = 0;
            sub.dependency_offset = 0;
            sub.sent = None;
            drop(sub);
            continue;
        }
        if cursor == high {
            drop(sub);
            let snapshot = {
                let mut slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
                let store = slot.as_mut().unwrap();
                if store.host_authorized(host, Some("owner")) {
                    Some(store.owner_snapshot()?)
                } else {
                    None
                }
            };
            if let Some(snapshot) = snapshot {
                if snapshot != last_owner {
                    write_frame(
                        &conn.writer,
                        &json!({"kind":"owner_snapshot","snapshot":snapshot}),
                    )?;
                    last_owner = snapshot;
                }
            }
            runtime.signal.wait(signal, Duration::from_secs(20));
            continue;
        }
        let next = page["cursor"].as_u64().unwrap();
        sub.sent = Some((
            next,
            revision,
            page["dependency_offset"].as_u64().unwrap_or(0) as usize,
        ));
        let scopes = sub.resolved.clone();
        drop(sub);
        if live["items"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
        {
            write_frame(&conn.writer, &json!({"kind":"live","items":live["items"]}))?;
        }
        live_cursor = live["scanned"].as_u64().unwrap();
        write_frame(
            &conn.writer,
            &json!({"kind":"batch","page":page,"revision":revision,"scopes":scopes}),
        )?;
    }
    Ok(())
}

/// Scoped-backfill push loop: one main page and one backfill page may be
/// outstanding at once, so new history never stalls live traffic and an
/// interest change never restarts the main stream.
fn push_loop_scoped(
    runtime: &Arc<Runtime>,
    host: &str,
    conn: &PeerConnection,
    subscription: &Arc<Mutex<Subscription>>,
    mut live_cursor: u64,
) -> Result<()> {
    let mut last_owner = Value::Null;
    while !conn.closed.load(Ordering::Acquire) && !runtime.stopped.load(Ordering::Acquire) {
        let signal = runtime.signal.version();
        let mut frames = Vec::new();
        let idle = {
            let slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
            let store = slot.as_ref().unwrap();
            let mut sub = subscription.lock().unwrap_or_else(|p| p.into_inner());
            sub.reconcile(store, host)?;
            let high = store.publication_position();
            let announce = std::mem::take(&mut sub.announce);
            if !announce.is_empty() {
                frames.push(json!({"kind":"backfills","items":announce}));
            }
            if sub.sent.is_none() && sub.cursor < high {
                let cursor = sub.cursor.min(high);
                let page = store.export_scoped_slice(
                    host,
                    &sub.covered,
                    &sub.joined(),
                    cursor,
                    high,
                    sub.dependency_offset,
                    BULK_PAGE,
                )?;
                let live =
                    store.export_live(host, &sub.resolved, cursor, live_cursor.max(cursor), high)?;
                if live["items"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                {
                    frames.push(json!({"kind":"live","items":live["items"]}));
                }
                live_cursor = live["scanned"].as_u64().unwrap_or(live_cursor);
                let revision = sub.revision;
                sub.sent = Some((
                    page["cursor"].as_u64().unwrap(),
                    revision,
                    page["dependency_offset"].as_u64().unwrap_or(0) as usize,
                ));
                frames.push(json!({"kind":"batch","page":page,"revision":revision,"scopes":sub.covered}));
            }
            // Backfill pages are bounded by the acknowledged main cursor: the
            // channel metadata their messages depend on travels there.
            let main = sub.cursor;
            if !sub.backfills.values().any(|b| b.sent.is_some()) {
                let next = sub
                    .backfills
                    .iter_mut()
                    // A partly sent reply chain sits above `cursor` and at or
                    // below the previous bound, so this also resumes it.
                    .filter(|(_, b)| b.cursor < b.through.min(main))
                    .min_by_key(|(_, b)| b.id);
                if let Some((scope, b)) = next {
                    let upper = b.through.min(main);
                    let page = store.export_backfill(
                        host,
                        scope,
                        b.cursor,
                        upper,
                        b.dependency_offset,
                        BULK_PAGE,
                    )?;
                    let cursor = page["cursor"].as_u64().unwrap();
                    let offset = page["dependency_offset"].as_u64().unwrap_or(0) as usize;
                    let done = (b.done + store.scope_message_count(scope, b.cursor, cursor))
                        .min(b.total);
                    let complete = page["complete"] == true && upper == b.through;
                    b.sent = Some((cursor, offset, done));
                    frames.push(json!({"kind":"backfill","scope":scope,"id":b.id,"page":page,"done":done,"total":b.total,"through":b.through,"complete":complete}));
                }
            }
            frames.is_empty() && sub.sent.is_none() && sub.cursor >= high
        };
        for frame in &frames {
            write_frame(&conn.writer, frame)?;
        }
        if !frames.is_empty() {
            continue;
        }
        if idle {
            let snapshot = {
                let mut slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
                let store = slot.as_mut().unwrap();
                if store.host_authorized(host, Some("owner")) {
                    Some(store.owner_snapshot()?)
                } else {
                    None
                }
            };
            if let Some(snapshot) = snapshot.filter(|s| *s != last_owner) {
                write_frame(
                    &conn.writer,
                    &json!({"kind":"owner_snapshot","snapshot":snapshot}),
                )?;
                last_owner = snapshot;
            }
        }
        runtime.signal.wait(signal, Duration::from_secs(20));
    }
    Ok(())
}

pub(super) fn run_client(runtime: Arc<Runtime>) {
    let mut failures = 0u32;
    while !runtime.stopped.load(Ordering::Acquire) {
        let result = client_connection(&runtime);
        runtime.connected.store(false, Ordering::Release);
        if runtime.stopped.load(Ordering::Acquire) {
            break;
        }
        if let Some(store) = runtime
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
        {
            store.set_sync_connection(
                false,
                if runtime.stopped.load(Ordering::Acquire) {
                    None
                } else {
                    result.as_ref().err().map(|e| e.to_string())
                },
            );
        }
        runtime.fail_pending("Messaging connection closed");
        if runtime.stopped.load(Ordering::Acquire) {
            break;
        }
        if result.is_ok() {
            failures = 0;
        } else {
            failures = failures.saturating_add(1);
        }
        let seconds = (1u64 << failures.min(5)).min(30);
        let jitter = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_millis() as u64
            % 500;
        let end = Instant::now() + Duration::from_millis(seconds * 1000 + jitter);
        while Instant::now() < end && !runtime.stopped.load(Ordering::Acquire) {
            runtime.signal.wait(
                runtime.signal.version(),
                end.saturating_duration_since(Instant::now()),
            );
        }
    }
}
/// Per-scope history this replica already holds in hub `generation`: durable
/// coverage plus acknowledged progress of interrupted backfills. A scoped hub
/// starts a (re-)added scope's backfill from here instead of zero.
fn coverage_hints(runtime: &Runtime, generation: Option<&str>) -> (Value, Value) {
    let Some(generation) = generation else {
        return (Value::Null, json!({}));
    };
    let mut hints = runtime
        .store
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .unwrap()
        .transport_coverage_hints(generation);
    for (scope, b) in runtime
        .backfills
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .filter(|(_, b)| b.cursor > 0)
    {
        let hint = hints.entry(scope.clone()).or_default();
        *hint = (*hint).max(b.cursor);
    }
    (json!(generation), json!(hints))
}
fn client_connection(runtime: &Arc<Runtime>) -> Result<()> {
    let mut link = connect(runtime.config.endpoint.as_ref().unwrap())?;
    let _close = ConnectionGuard(link.connection.clone());
    let (handshake_done, deadline) = mpsc::channel::<()>();
    let guard = link.connection.clone();
    thread::spawn(move || {
        if matches!(
            deadline.recv_timeout(Duration::from_secs(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            guard.close();
        }
    });
    let token = fs::read_to_string(runtime.config.token_file.as_ref().unwrap())?;
    let interests = runtime.interests();
    let resume = runtime
        .store
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .unwrap()
        .download_checkpoint();
    // A hub that predates FEATURE_SCOPED ignores the extra fields.
    let (coverage_generation, coverage) = coverage_hints(runtime, resume["generation"].as_str());
    runtime.backfills.lock().unwrap_or_else(|p| p.into_inner()).clear();
    runtime.scoped.store(false, Ordering::Release);
    write_frame(
        &link.connection.writer,
        &json!({"kind":"hello","daemon_id":runtime.daemon_id,"space_id":runtime.config.space_id,"token":token.trim(),"people":runtime.local_people(),"interests":interests,"resume":resume,"features":[FEATURE_SCOPED],"coverage_generation":coverage_generation,"coverage":coverage}),
    )?;
    let hello = read_frame(link.reader.as_mut())?;
    if hello["kind"] == "error" {
        return Err(unmarshal_error(&hello["error"]));
    }
    if hello["kind"] != "hello"
        || hello["coordinator_id"] != runtime.config.coordinator_id
        || hello["space_id"] != runtime.config.space_id
    {
        link.connection.close();
        return Err(error(
            "unauthorized",
            "Connected endpoint is not the configured messaging coordinator",
        ));
    }
    let generation = hello["generation"]
        .as_str()
        .ok_or_else(|| error("invalid_cursor", "Missing hub generation"))?
        .to_owned();
    uuid::Uuid::parse_str(&generation)
        .map_err(|_| error("invalid_cursor", "Invalid hub generation"))?;
    drop(handshake_done);
    let scoped = has_feature(&hello, FEATURE_SCOPED);
    runtime.scoped.store(scoped, Ordering::Release);
    let inflight = Arc::new(Mutex::new(BTreeSet::<String>::new()));
    runtime
        .connected
        .store(hello["revoked"] != true, Ordering::Release);
    runtime
        .peers
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(
            runtime.config.coordinator_id.clone(),
            link.connection.clone(),
        );
    runtime
        .store
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_mut()
        .unwrap()
        .set_sync_connection(
            hello["revoked"] != true,
            if hello["revoked"] == true {
                Some("Host enrollment revoked; reconciling prior receipts".into())
            } else {
                None
            },
        );
    let initial_cursor = hello["cursor"].as_u64().unwrap_or(0);
    runtime
        .stream_cursor
        .store(initial_cursor, Ordering::Release);
    runtime.stream_revision.store(1, Ordering::Release);
    let hub_generation = generation.clone();
    let cloned = runtime.clone();
    let conn = link.connection.clone();
    let uploading = inflight.clone();
    let reader_failure = Arc::new(Mutex::new(None));
    let failed = reader_failure.clone();
    thread::Builder::new()
        .name("cm-chat-sync-reader".into())
        .spawn(move || {
            let result = client_reader(
                &cloned,
                &mut *link.reader,
                &conn,
                &uploading,
                &generation,
                initial_cursor,
            );
            if let Err(e) = result {
                if !cloned.stopped.load(Ordering::Acquire) {
                    if let Some(store) = cloned
                        .store
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .as_mut()
                    {
                        store.set_sync_connection(false, Some(e.to_string()));
                    }
                }
                *failed.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
            }
            conn.close();
            cloned.signal.signal();
        })?;
    let connection = runtime
        .peers
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&runtime.config.coordinator_id)
        .unwrap()
        .clone();
    let result = (|| {
        let mut last_interests = interests;
        let mut last_ping = Instant::now();
        while !runtime.stopped.load(Ordering::Acquire) && !connection.closed.load(Ordering::Acquire)
        {
            let version = runtime.signal.version();
            let interests = runtime.interests();
            if interests != last_interests {
                let mut frame = json!({"kind":"interests","interests":interests});
                if scoped {
                    let (generation, coverage) = coverage_hints(runtime, Some(&hub_generation));
                    frame["coverage_generation"] = generation;
                    frame["coverage"] = coverage;
                }
                write_frame(&connection.writer, &frame)?;
                last_interests = interests;
            }
            loop {
                let command = runtime
                    .commands
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .try_recv();
                let Ok(command) = command else {
                    break;
                };
                if !runtime
                    .pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .contains_key(&command.id)
                {
                    continue;
                }
                write_frame(
                    &connection.writer,
                    &json!({"kind":"call","id":command.id,"actor":command.actor,"method":command.method,"params":command.params,"people":runtime.local_people()}),
                )?;
            }
            let uploads = runtime
                .store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .unwrap()
                .pending_uploads(64)?;
            for wire in uploads {
                let e: Value = serde_json::from_str(wire["event"].as_str().unwrap())?;
                let id = e["id"].as_str().unwrap().to_owned();
                if runtime
                    .upload_retry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&id)
                    .is_some_and(|at| *at > Instant::now())
                {
                    continue;
                }
                runtime
                    .upload_retry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                if inflight
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(id.clone())
                {
                    write_frame(
                        &connection.writer,
                        &json!({"kind":"upload","id":id,"wire":wire}),
                    )?;
                }
            }
            if last_ping.elapsed() >= Duration::from_secs(20) {
                write_frame(
                    &connection.writer,
                    &json!({"kind":"ping","people":runtime.local_people()}),
                )?;
                last_ping = Instant::now();
            }
            runtime.signal.wait(version, Duration::from_secs(10));
        }
        Ok(())
    })();
    connection.close();
    runtime.connected.store(false, Ordering::Release);
    result.and_then(|()| {
        reader_failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .map_or(Ok(()), Err)
    })
}
fn client_reader(
    runtime: &Arc<Runtime>,
    reader: &mut dyn Read,
    connection: &PeerConnection,
    inflight: &Arc<Mutex<BTreeSet<String>>>,
    generation: &str,
    initial_cursor: u64,
) -> Result<()> {
    let mut cursor = initial_cursor;
    let mut revision = 1;
    while !connection.closed.load(Ordering::Acquire) && !runtime.stopped.load(Ordering::Acquire) {
        let frame = read_frame(reader)?;
        if runtime.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        match frame["kind"].as_str().unwrap_or("") {
            "live" => {
                let items = frame["items"]
                    .as_array()
                    .ok_or_else(|| error("invalid_record", "Missing live records"))?;
                if items.len() > 64 {
                    return Err(error("invalid_record", "Oversized live page"));
                }
                {
                    let mut slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
                    let store = slot.as_mut().unwrap();
                    store.ingest_replica_page(items)?;
                }
                // Sparse priority delivery is an arrival, not coverage. The
                // following bulk acknowledgement bounds this lane as well.
                runtime.publish_to_sessions();
            }
            "batch" => {
                let page = &frame["page"];
                if page["space_id"] != runtime.config.space_id || page["generation"] != generation {
                    return Err(error(
                        "resync_required",
                        "Replication page changed space/generation",
                    ));
                }
                let items = page["items"]
                    .as_array()
                    .ok_or_else(|| error("invalid_record", "Missing page records"))?;
                let next = page["cursor"]
                    .as_u64()
                    .ok_or_else(|| error("invalid_cursor", "Missing page cursor"))?;
                let next_revision = frame["revision"]
                    .as_u64()
                    .ok_or_else(|| error("invalid_cursor", "Missing subscription revision"))?;
                let scopes: BTreeSet<String> = serde_json::from_value(frame["scopes"].clone())?;
                {
                    let mut slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
                    let store = slot.as_mut().unwrap();
                    store.ingest_replica_page(items)?;
                    for wire in items {
                        if let Some(id) = wire["receipt"]["event_id"].as_str() {
                            inflight
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .remove(id);
                        }
                    }
                    // Scoped hubs list only fully covered scopes here; a
                    // backfilling scope is claimed when its backfill completes.
                    let list: Vec<&str> = scopes
                        .iter()
                        .map(String::as_str)
                        .chain(["metadata"])
                        .collect();
                    store.record_coverage_batch(
                        &list,
                        generation,
                        next,
                        Some((next, next_revision, &scopes)),
                    )?;
                    store.set_sync_connection(true, None);
                }
                cursor = next;
                revision = next_revision;
                runtime.stream_cursor.store(cursor, Ordering::Release);
                runtime.stream_revision.store(revision, Ordering::Release);
                write_frame(
                    &connection.writer,
                    &json!({"kind":"ack","cursor":cursor,"revision":revision,"dependency_offset":page["dependency_offset"]}),
                )?;
                runtime.finish_pending(cursor, revision);
                runtime.publish_to_sessions();
            }
            "backfill" => {
                let page = &frame["page"];
                if page["space_id"] != runtime.config.space_id || page["generation"] != generation {
                    return Err(error(
                        "resync_required",
                        "Backfill page changed space/generation",
                    ));
                }
                let scope = frame["scope"]
                    .as_str()
                    .ok_or_else(|| error("invalid_record", "Missing backfill scope"))?;
                let items = page["items"]
                    .as_array()
                    .ok_or_else(|| error("invalid_record", "Missing backfill records"))?;
                let next = page["cursor"]
                    .as_u64()
                    .ok_or_else(|| error("invalid_cursor", "Missing backfill cursor"))?;
                let through = frame["through"]
                    .as_u64()
                    .ok_or_else(|| error("invalid_cursor", "Missing backfill bound"))?;
                let complete = frame["complete"] == true;
                {
                    let mut slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
                    let store = slot.as_mut().unwrap();
                    store.ingest_replica_page(items)?;
                    // Coverage is claimed only for a finished backfill; partial
                    // progress is reported separately and never as coverage.
                    if complete {
                        store.record_coverage(scope, generation, through)?;
                    }
                }
                // Even mid-chain, `next` only counts fully delivered messages.
                runtime.merge_backfills(&json!({ scope: {
                    "id": frame["id"], "done": frame["done"], "total": frame["total"],
                    "complete": complete, "cursor": next,
                }}));
                write_frame(
                    &connection.writer,
                    &json!({"kind":"backfill_ack","scope":scope,"id":frame["id"],"cursor":next,"dependency_offset":page["dependency_offset"]}),
                )?;
                runtime.finish_pending(cursor, revision);
                runtime.publish_to_sessions();
            }
            "backfills" => {
                if let Some(items) = frame["items"].as_object() {
                    let mut map = runtime.backfills.lock().unwrap_or_else(|p| p.into_inner());
                    for (scope, v) in items {
                        if v["cancelled"] == true
                            && map.get(scope).is_some_and(|b| Some(b.id) == v["id"].as_u64())
                        {
                            map.remove(scope);
                        }
                    }
                }
                runtime.merge_backfills(&json!(frame["items"]
                    .as_object()
                    .map(|m| m
                        .iter()
                        .filter(|(_, v)| v["cancelled"] != true)
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<serde_json::Map<_, _>>())
                    .unwrap_or_default()));
                runtime.finish_pending(cursor, revision);
            }
            "receipt" => {
                runtime
                    .store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_mut()
                    .unwrap()
                    .record_receipt(&frame["receipt"])?;
                if let Some(id) = frame["id"].as_str() {
                    inflight
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(id);
                }
            }
            "rejection" => {
                let e = unmarshal_error(&frame["error"]);
                let id = frame["id"]
                    .as_str()
                    .ok_or_else(|| error("invalid_record", "Missing rejected event ID"))?;
                if e.code == "dependency_missing" {
                    runtime
                        .upload_retry
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(id.into(), Instant::now() + Duration::from_secs(1));
                    inflight
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(id);
                } else if matches!(
                    e.code.as_str(),
                    "unauthorized"
                        | "event_conflict"
                        | "idempotency_conflict"
                        | "outcome_unknown"
                        | "store_read_only"
                        | "storage_error"
                ) {
                    return Err(e);
                } else {
                    runtime
                        .store
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .as_mut()
                        .unwrap()
                        .reject_replication(id, &e.code)?;
                    inflight
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(id);
                    runtime.publish_to_sessions();
                }
            }
            "reply" => {
                let id = frame["id"].as_str().unwrap_or("");
                let mut pending = runtime.pending.lock().unwrap_or_else(|p| p.into_inner());
                if frame["error"].is_object() {
                    if let Some(p) = pending.remove(id) {
                        let _ = p.reply.send(Err(unmarshal_error(&frame["error"])));
                    }
                } else if let Some(p) = pending.get_mut(id) {
                    // Merge before readiness is evaluated: a pending history
                    // wait must see the backfill this call started.
                    runtime.merge_backfills(&frame["backfills"]);
                    if let Some(started) = frame["backfill_started"].as_array() {
                        p.scopes
                            .extend(started.iter().filter_map(|s| s.as_str().map(str::to_owned)));
                    }
                    p.response = Some(frame["result"].clone());
                    p.barrier = frame["barrier"].as_u64().unwrap_or(u64::MAX);
                    p.revision = frame["revision"].as_u64().unwrap_or(u64::MAX);
                }
                drop(pending);
                runtime.finish_pending(cursor, revision);
            }
            "enrollment" => {
                let mut slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
                let store = slot.as_mut().unwrap();
                let event: Value = serde_json::from_str(
                    frame["wire"]["event"]
                        .as_str()
                        .ok_or_else(|| error("invalid_record", "Missing enrollment event"))?,
                )?;
                if event["type"] != "host.update"
                    || event["data"]["host_id"] != runtime.daemon_id
                    || event["data"]["active"] != false
                {
                    return Err(error("invalid_record", "Invalid revocation record"));
                }
                store.ingest_replica(&frame["wire"])?;
            }
            "owner_snapshot" => {
                runtime
                    .store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_mut()
                    .unwrap()
                    .apply_owner_snapshot(&frame["snapshot"])?;
            }
            "pong" => {}
            "error" => return Err(unmarshal_error(&frame["error"])),
            _ => {
                return Err(error(
                    "invalid_record",
                    "Unexpected messaging stream response",
                ))
            }
        }
        runtime.signal.signal();
    }
    Ok(())
}
