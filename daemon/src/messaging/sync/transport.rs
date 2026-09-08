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
struct Subscription {
    interests: BTreeSet<String>,
    resolved: BTreeSet<String>,
    revision: u64,
    cursor: u64,
    dependency_offset: usize,
    sent: Option<(u64, u64, usize)>,
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
    let (generation, resolved, cursor, live_start) = {
        let slot = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
        let store = slot.as_ref().unwrap();
        let resolved = store.host_transport_interests(&host, &initial)?;
        let resume = &hello["resume"];
        let old_scopes: BTreeSet<String> =
            serde_json::from_value(resume["scopes"].clone()).unwrap_or_default();
        let cursor = if resume["generation"] == store.generation && old_scopes == resolved {
            resume["cursor"]
                .as_u64()
                .filter(|c| *c <= store.publication_position())
                .unwrap_or(0)
        } else {
            0
        };
        (
            store.generation.clone(),
            resolved,
            cursor,
            store.publication_position(),
        )
    };
    let subscription = Arc::new(Mutex::new(Subscription {
        interests: initial,
        resolved,
        revision: 1,
        cursor,
        dependency_offset: 0,
        sent: None,
    }));
    write_frame(
        &link.connection.writer,
        &json!({"kind":"hello","coordinator_id":runtime.daemon_id,"space_id":runtime.config.space_id,"generation":generation,"cursor":cursor,"revision":1}),
    )?;
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
                    let (outcome, barrier) = {
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
                            sub.revision += 1;
                            sub.cursor = 0;
                            sub.dependency_offset = 0;
                            sub.sent = None;
                            Ok(json!({"caught_up":true}))
                        } else {
                            store.coordinate(&host, actor, method, &frame["params"])
                        };
                        if actor == "owner" {
                            if let Ok(v) = &mut result {
                                v["owner_snapshot"] = store.owner_snapshot()?;
                            }
                        }
                        (result, store.publication_position())
                    };
                    let revision = subscription
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .revision;
                    let reply = match outcome {
                        Ok(result) => {
                            json!({"kind":"reply","id":frame["id"],"result":result,"barrier":barrier,"revision":revision})
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
    write_frame(
        &link.connection.writer,
        &json!({"kind":"hello","daemon_id":runtime.daemon_id,"space_id":runtime.config.space_id,"token":token.trim(),"people":runtime.local_people(),"interests":interests,"resume":resume}),
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
                write_frame(
                    &connection.writer,
                    &json!({"kind":"interests","interests":interests}),
                )?;
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
                    for wire in items {
                        store.ingest_replica(wire)?;
                    }
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
                    for wire in items {
                        store.ingest_replica(wire)?;
                        if let Some(id) = wire["receipt"]["event_id"].as_str() {
                            inflight
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .remove(id);
                        }
                    }
                    for scope in &scopes {
                        store.record_coverage(scope, generation, next)?;
                    }
                    store.record_coverage("metadata", generation, next)?;
                    store.record_download_checkpoint(generation, next, next_revision, &scopes)?;
                    store.set_sync_connection(true, None);
                }
                cursor = next;
                revision = next_revision;
                write_frame(
                    &connection.writer,
                    &json!({"kind":"ack","cursor":cursor,"revision":revision,"dependency_offset":page["dependency_offset"]}),
                )?;
                runtime.finish_pending(cursor, revision);
                runtime.publish_to_sessions();
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
