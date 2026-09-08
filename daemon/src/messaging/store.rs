use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

mod operations;
mod owner_sync;
mod private_sync;
mod replication;
mod task_subscriptions;
pub use replication::ChangeSignal;
pub use task_subscriptions::TaskBinding;
mod personal;
#[cfg(test)]
mod tests_b;
mod wake_intents;
pub use wake_intents::WakeIntent;
mod norms;
pub use norms::textual_diff as norms_diff;
mod channels;
mod membership;
mod preferences;
mod watches;
use membership::mention_recipients;
use personal::Personal;

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn uuid() -> String {
    Uuid::new_v4().to_string()
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn err(code: &str, message: impl Into<String>) -> ChatError {
    ChatError {
        code: code.into(),
        message: message.into(),
    }
}
#[derive(Debug)]
pub struct ChatError {
    pub code: String,
    pub message: String,
}
impl From<io::Error> for ChatError {
    fn from(e: io::Error) -> Self {
        err("storage_error", e.to_string())
    }
}
impl From<serde_json::Error> for ChatError {
    fn from(e: serde_json::Error) -> Self {
        err("invalid_record", e.to_string())
    }
}
impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ChatError {}
type Result<T> = std::result::Result<T, ChatError>;

fn mkdir(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        if fs::symlink_metadata(ancestor).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(io::Error::other(
                "symlinks are not supported in messaging paths",
            ));
        }
    }
    if let Ok(m) = fs::symlink_metadata(path) {
        return if m.is_dir() && !m.file_type().is_symlink() {
            Ok(())
        } else {
            Err(io::Error::other("messaging path is not a real directory"))
        };
    }
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            mkdir(parent)?;
        }
    }
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}
fn write_file(path: &Path, bytes: &[u8], replace: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    mkdir(parent)?;
    let temp = parent.join(format!(".tmp-{}", uuid()));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if replace {
            fs::rename(&temp, path)?;
        } else {
            fs::hard_link(&temp, path)?;
            fs::remove_file(&temp)?;
        }
        File::open(parent)?.sync_all()
    })();
    let _ = fs::remove_file(temp);
    result
}
pub fn atomic_replace(path: &Path, value: &Value) -> io::Result<()> {
    write_file(path, &serde_json::to_vec_pretty(value)?, true)
}
// Projections are disposable. Avoid fsyncing identical directory/name/norm
// copies on every ordinary message, while retaining repair on the next write.
fn project_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    mkdir(
        path.parent()
            .ok_or_else(|| io::Error::other("Missing projection directory"))?,
    )?;
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
        && fs::read(path).is_ok_and(|old| old == bytes)
    {
        return Ok(());
    }
    write_file(path, bytes, true)
}
fn project_json(path: &Path, value: &Value) -> io::Result<()> {
    project_file(path, &serde_json::to_vec_pretty(value)?)
}
fn load(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn strv<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
fn required(v: &Value, key: &str) -> Result<String> {
    let s = strv(v, key);
    if s.is_empty() {
        Err(err("invalid_params", format!("{key} is required")))
    } else {
        Ok(s.to_owned())
    }
}
fn normalize(s: &str) -> String {
    s.nfkc()
        .collect::<String>()
        .case_fold()
        .nfkc()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn dashed_name(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join("-")
}
fn name_key(s: &str) -> String {
    dashed_name(&normalize(s))
}
fn channel_path(s: &str) -> Result<()> {
    if s.len() > 512
        || s.split('/').count() > 16
        || s.is_empty()
        || s.split('/').any(|x| {
            x.is_empty()
                || x.starts_with('-')
                || x.ends_with('-')
                || x.contains("--")
                || !x
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
    {
        return Err(err(
            "invalid_channel",
            "Use lowercase channel segments, e.g. news/parser; no traversal or underscores",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Name {
    pub name: String,
    pub revision: u64,
    #[serde(default)]
    pub revision_id: String,
    pub aliases: Vec<String>,
    pub session_uid: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Person {
    pub id: String,
    pub name: String,
    pub session_uid: String,
    pub task: Option<String>,
    pub present: bool,
    pub kind: String,
}
#[derive(Clone, Debug)]
struct Published {
    event: Value,
    position: u64,
    received_at: String,
}
#[derive(Default, Serialize, Deserialize)]
struct ReadState {
    ids: BTreeSet<String>,
}

pub struct Store {
    pub root: PathBuf,
    pub daemon_id: String,
    pub space_id: String,
    pub generation: String,
    pub names: BTreeMap<String, Name>,
    pub degraded: Option<String>,
    replication: replication::Replication,
    task_bindings: BTreeMap<String, TaskBinding>,
    _lock: File,
    events: Vec<Published>,
    channels: BTreeMap<String, String>,
    memberships: BTreeMap<String, BTreeSet<String>>,
    membership_revisions: BTreeMap<String, String>,
    enrolled: BTreeSet<String>,
    conversations: BTreeMap<String, Vec<String>>,
    requests: BTreeMap<String, (String, String, String)>,
    position: u64,
    clock: u64,
    reads: BTreeMap<String, ReadState>,
    personal: BTreeMap<String, Personal>,
    pub norms: Value,
    enrollment_revision: String,
    owner_identity_revision: String,
}
impl Store {
    pub fn open(cm_root: &Path) -> Result<Self> {
        mkdir(cm_root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(cm_root.join("messaging.lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(err(
                "writer_busy",
                "Another messaging writer owns this CM state root",
            ));
        }
        Self::open_locked(cm_root, lock)
    }
    fn open_locked(cm_root: &Path, lock: File) -> Result<Self> {
        operations::finish_install(cm_root)?;
        let root = cm_root.join("messages/main");
        let id_path = cm_root.join("daemon-id");
        let daemon_id = if id_path.exists() {
            fs::read_to_string(&id_path)?.trim().to_string()
        } else {
            if root.join("space.json").exists() {
                return Err(err("identity_missing","Restore daemon-id; existing messaging enrollment must not get another identity"));
            }
            let id = uuid();
            write_file(&id_path, id.as_bytes(), false)?;
            id
        };
        Uuid::parse_str(&daemon_id)
            .map_err(|_| err("identity_invalid", "daemon-id is not a UUID"))?;
        mkdir(&root)?;
        let space_path = root.join("space.json");
        if !space_path.exists() {
            atomic_replace(
                &space_path,
                &json!({"protocol":1,"space_id":uuid(),"coordinator_id":daemon_id,"replica_id":daemon_id,"generation":uuid(),"enrollment_revision":uuid(),"owner_identity_revision":uuid()}),
            )?;
        }
        let meta = load(&space_path)?;
        if meta["protocol"] != 1
            || meta["replica_id"] != daemon_id
            || (meta["coordinator_id"] != daemon_id
                && !cm_root.join("messaging-sync.json").exists())
        {
            return Err(err(
                "unsupported_space",
                "Replica identity needs an explicit messaging sync configuration",
            ));
        }
        let mut s = Self {
            root,
            daemon_id,
            space_id: required(&meta, "space_id")?,
            generation: required(&meta, "generation")?,
            names: BTreeMap::new(),
            task_bindings: BTreeMap::new(),
            degraded: None,
            replication: replication::Replication::new(
                &meta,
                cm_root.join("messaging-sync.json").exists(),
            )?,
            _lock: lock,
            events: Vec::new(),
            channels: BTreeMap::new(),
            memberships: BTreeMap::new(),
            membership_revisions: BTreeMap::new(),
            enrolled: BTreeSet::new(),
            conversations: BTreeMap::new(),
            requests: BTreeMap::new(),
            position: 0,
            clock: 0,
            reads: BTreeMap::new(),
            personal: BTreeMap::new(),
            norms: json!({}),
            enrollment_revision: required(&meta, "enrollment_revision")?,
            owner_identity_revision: required(&meta, "owner_identity_revision")?,
        };
        for id in [
            &s.space_id,
            &s.generation,
            &s.enrollment_revision,
            &s.owner_identity_revision,
        ] {
            Uuid::parse_str(id).map_err(|_| {
                err(
                    "invalid_space",
                    "Space and revision identifiers must be UUIDs",
                )
            })?;
        }
        mkdir(&s.journal_dir())?;
        if let Err(e) = s.rebuild() {
            s.degraded = Some(e.to_string());
            return Ok(s);
        }
        if let Err(e) = s
            .load_personal()
            .and_then(|_| s.load_task_bindings())
            .and_then(|_| s.load_known_reads())
        {
            s.degraded = Some(e.to_string());
            return Ok(s);
        }
        let owner_snapshot = s.root.join("_state/owner-sync.json");
        if owner_snapshot.exists() && !s.is_coordinator() {
            match load(&owner_snapshot) {
                Ok(v) => s.replication.owner_snapshot = Some(v),
                Err(e) => {
                    s.degraded = Some(e.to_string());
                    return Ok(s);
                }
            }
        }
        if let Err(e) = s
            .quarantine_staged()
            .and_then(|_| File::open(s.journal_dir())?.sync_all())
        {
            s.degraded = Some(format!("Recovery reconciliation failed: {e}"));
            return Ok(s);
        }
        if s.is_coordinator() && !s.messaging_frozen() {
            for (path, description) in [
                ("general", "Shared discussion"),
                (
                    "cm-general",
                    "Claude Manager usage, coordination, upcoming changes and release notes",
                ),
            ] {
                if !s.channels.contains_key(path) {
                    s.publish("channel.create", None, &format!("System created #{path}"),
                    json!({"membership_version":1,"channels":[{"path":path,"id":uuid(),"description":description,"default_join":true}]}),
                    "system", "System", "system", &format!("bootstrap-{path}"), "")?;
                }
            }
            if s.norms["revision"].is_null() {
                s.publish(
                    "norms.update",
                    None,
                    "System initialized shared norms",
                    json!({"scope":"global","revision":uuid(),"text":super::NORMS}),
                    "system",
                    "System",
                    "system",
                    "bootstrap-norms",
                    "",
                )?;
            }
        }
        let recovered = if s.is_coordinator() && !s.messaging_frozen() {
            s.initialize_memberships()
                .and_then(|_| s.enroll_participants(&[]))
                .and_then(|_| s.repair_attestations())
                .and_then(|_| s.normalize_existing_names())
        } else {
            Ok(())
        };
        if let Err(e) = recovered.and_then(|_| s.project()) {
            s.degraded = Some(format!("Projection recovery failed: {e}"));
        }
        Ok(s)
    }
    /// Unreferenced objects never become messages by directory scanning. Move
    /// them aside before accepting retries, so recovery explicitly settles the
    /// pre-publication crash case without deleting evidence.
    fn quarantine_staged(&self) -> io::Result<()> {
        fn visit(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
            if !dir.exists() {
                return Ok(());
            }
            for item in fs::read_dir(dir)? {
                let entry = item?;
                let path = entry.path();
                let kind = entry.file_type()?;
                if kind.is_symlink() {
                    return Err(io::Error::other("Symlink in event tree"));
                }
                if kind.is_dir() {
                    visit(&path, out)?;
                } else if dir.file_name().is_some_and(|s| s == "_events")
                    && path.extension().is_some_and(|s| s == "json")
                {
                    out.push(path);
                }
            }
            Ok(())
        }
        let known: BTreeSet<_> = self
            .events
            .iter()
            .map(|e| self.event_path(&e.event).map_err(io::Error::other))
            .collect::<io::Result<_>>()?;
        let mut files = vec![];
        for folder in ["_events", "channels", "direct"] {
            visit(&self.root.join(folder), &mut files)?;
        }
        for path in files {
            if known.contains(&path) {
                continue;
            }
            let destination = self
                .root
                .join("_quarantine")
                .join(format!("staged-{}.json", uuid()));
            mkdir(destination.parent().unwrap())?;
            atomic_replace(
                &destination.with_extension("source"),
                &json!({"source":path.strip_prefix(&self.root).unwrap().to_string_lossy(),"reason":"No retained publication; original request may now be retried"}),
            )?;
            fs::rename(&path, &destination)?;
            File::open(path.parent().unwrap())?.sync_all()?;
            File::open(destination.parent().unwrap())?.sync_all()?;
        }
        Ok(())
    }
    fn journal_dir(&self) -> PathBuf {
        self.root.join("_journal").join(&self.generation)
    }
    fn event_path(&self, e: &Value) -> Result<PathBuf> {
        let id = required(e, "id")?;
        let (origin, event) = id
            .split_once(':')
            .ok_or_else(|| err("invalid_record", "Bad event ID"))?;
        Uuid::parse_str(origin)
            .and_then(|_| Uuid::parse_str(event))
            .map_err(|_| err("invalid_record", "Bad event UUID"))?;
        let dir = if let Some(cid) = e["conversation_id"].as_str() {
            if let Some((path, _)) = self.channels.iter().find(|(_, v)| v.as_str() == cid) {
                self.root.join("channels").join(path)
            } else {
                Uuid::parse_str(cid).map_err(|_| err("invalid_record", "Bad conversation ID"))?;
                self.root.join("direct").join(cid)
            }
        } else {
            self.root.clone()
        };
        Ok(dir.join("_events").join(format!("{origin}--{event}.json")))
    }
    fn rebuild(&mut self) -> Result<()> {
        let mut paths = fs::read_dir(self.journal_dir())?
            .map(|x| x.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()?;
        paths.sort();
        let mut seen = BTreeSet::new();
        for path in paths {
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let j = load(&path)?;
            let pos = required(&j, "position")?
                .parse::<u64>()
                .map_err(|_| err("invalid_record", "Bad journal position"))?;
            if pos <= self.position
                || j["protocol"] != 1
                || path.file_name().and_then(|s| s.to_str())
                    != Some(format!("{pos:020}.json").as_str())
                || j["replica_id"] != self.daemon_id
                || j["generation"] != self.generation
            {
                return Err(err(
                    "invalid_record",
                    "Conflicting or unsupported journal record",
                ));
            }
            if j["kind"] != "publish" {
                self.apply_replication_journal(&j)?;
                self.position = pos;
                continue;
            }
            let rel = required(&j["data"], "event_path")?;
            if Path::new(&rel).is_absolute()
                || Path::new(&rel)
                    .components()
                    .any(|x| !matches!(x, std::path::Component::Normal(_)))
            {
                return Err(err("invalid_record", "Unsafe event path"));
            }
            let bytes = fs::read(self.root.join(&rel))?;
            if hash(&bytes) != strv(&j, "event_sha256") {
                return Err(err("corrupt_event", "Event digest mismatch"));
            }
            let event: Value = serde_json::from_slice(&bytes)?;
            if event["id"] != j["event_id"]
                || event["space_id"] != self.space_id
                || !seen.insert(required(&event, "id")?)
            {
                return Err(err("invalid_record", "Duplicate event or wrong space"));
            }
            if event["protocol"] != 1
                || event["origin_daemon_id"].as_str()
                    != strv(&event, "id").split_once(':').map(|s| s.0)
                || !event["data"].is_object()
                || !event["extensions"].is_object()
                || strv(&event["actor"], "id").is_empty()
                || strv(&event, "body").is_empty()
                || strv(&event["request"], "key").is_empty()
                || strv(&event["request"], "sha256").len() != 64
            {
                return Err(err("invalid_record", "Malformed event envelope"));
            }
            parse_time(&required(&event, "created_at")?)?;
            parse_time(&required(&j["data"], "received_at")?)?;
            required(&event, "logical_time")?
                .parse::<u64>()
                .map_err(|_| err("invalid_record", "Bad logical time"))?;
            let expected = self.event_path(&event)?;
            if expected != self.root.join(rel) {
                return Err(err(
                    "invalid_record",
                    "Event in incorrect conversation directory",
                ));
            }
            self.clock = self.clock.max(
                event["logical_time"]
                    .as_str()
                    .unwrap_or("0")
                    .parse()
                    .map_err(|_| err("invalid_record", "Bad logical time"))?,
            );
            self.clock = self
                .clock
                .max(j["data"]["logical_clock"].as_u64().unwrap_or(0));
            self.position = pos;
            self.apply_replication_journal(&j)?;
            self.reduce(&event)?;
            self.events.push(Published {
                event,
                position: pos,
                received_at: required(&j["data"], "received_at")?,
            });
        }
        Ok(())
    }
    fn reduce(&mut self, e: &Value) -> Result<()> {
        if let Some(items) = e["data"]["channels"].as_array() {
            for item in items {
                let path = required(item, "path")?;
                channel_path(&path)?;
                self.channels.insert(path, required(item, "id")?);
            }
        }
        self.reduce_membership(e)?;
        self.reduce_replication(e)?;
        self.reduce_private_sync(e)?;
        if let Some(c) = e["data"].get("conversation_create") {
            let id = required(c, "id")?;
            let members: Vec<String> = serde_json::from_value(c["members"].clone())?;
            if !(2..=32).contains(&members.len())
                || members.windows(2).any(|pair| pair[0] >= pair[1])
                || !members.iter().any(|m| m == strv(&e["actor"], "id"))
                || members.iter().any(|m| m.is_empty() || m == "system")
                || self
                    .conversations
                    .get(&id)
                    .is_some_and(|old| old != &members)
            {
                return Err(err(
                    "invalid_record",
                    "DM needs 2–32 sorted, distinct, immutable members including its sender",
                ));
            }
            self.conversations.insert(id, members);
        }
        let name = if e["type"] == "identity.update" {
            e["data"].get("identity")
        } else {
            e["data"].get("identity_claim")
        };
        if let Some(n) = name {
            let id = required(n, "participant_id")?;
            let v: Name = serde_json::from_value(n.clone())?;
            if let Some(old) = self.names.get(&id) {
                if old.revision == v.revision && old.name != v.name {
                    return Err(err(
                        "identity_conflict",
                        "Same revision with different name",
                    ));
                }
            }
            if self
                .names
                .get(&id)
                .is_none_or(|old| old.revision <= v.revision)
            {
                self.names.insert(id, v);
            }
        }
        if e["type"] == "norms.update" {
            self.norms = e["data"].clone();
        }
        let key = format!(
            "{}\n{}",
            strv(&e["actor"], "id"),
            strv(&e["request"], "key")
        );
        let value = (
            required(e, "id")?,
            strv(&e["request"], "sha256").into(),
            strv(&e["request"], "origin_daemon_id").into(),
        );
        if let Some(old) = self.requests.insert(key, value.clone()) {
            if old != value {
                return Err(err(
                    "idempotency_conflict",
                    "Duplicate request in retained history",
                ));
            }
        }
        Ok(())
    }
    fn project(&self) -> Result<()> {
        project_file(&self.root.join("PROTOCOL.md"), super::PROTOCOL.as_bytes())?;
        project_file(
            &self.root.join("NORMS.md"),
            strv(&self.norms, "text").as_bytes(),
        )?;
        for (path, id) in &self.channels {
            project_json(
                &self.root.join("channels").join(path).join("CHANNEL.json"),
                &self.channel_info(path, id),
            )?;
        }
        for (id, members) in &self.conversations {
            project_json(
                &self.root.join("direct").join(id).join("CONVERSATION.json"),
                &json!({"id":id,"kind":"dm","members":members}),
            )?;
        }
        for (id, n) in &self.names {
            project_json(
                &self
                    .root
                    .join("participants")
                    .join(format!("{}.json", hash(id.as_bytes()))),
                &serde_json::to_value(n)?,
            )?;
        }
        Ok(())
    }
    fn repair_attestations(&mut self) -> Result<()> {
        let missing = self
            .events
            .iter()
            .filter_map(|p| p.event["data"]["identity_attestation"].as_str())
            .map(|s| serde_json::from_str::<Value>(s))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for event in missing {
            if !self.events.iter().any(|p| p.event["id"] == event["id"]) {
                self.commit(event)?;
            }
        }
        Ok(())
    }
    fn event(
        &mut self,
        ty: &str,
        conv: Option<&str>,
        body: &str,
        data: Value,
        actor: &str,
        name: &str,
        kind: &str,
        key: &str,
        digest: &str,
    ) -> Result<Value> {
        let generated_digest;
        let digest = if digest.is_empty() {
            generated_digest = hash(&serde_json::to_vec(
                &json!({"type":ty,"actor":actor,"data":data,"body":body}),
            )?);
            &generated_digest
        } else {
            digest
        };
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or_else(|| err("clock_overflow", "Logical clock exhausted"))?;
        Ok(
            json!({"protocol":1,"space_id":self.space_id,"id":format!("{}:{}",self.daemon_id,uuid()),"origin_daemon_id":self.daemon_id,"logical_time":self.clock.to_string(),"type":ty,"created_at":now(),"actor":{"id":actor,"name":name,"kind":kind},"conversation_id":conv,"body":body,"data":data,"extensions":{},"request":{"key":key,"sha256":digest,"origin_daemon_id":if actor == "system" {&self.daemon_id} else {self.operation_origin()}}}),
        )
    }
    fn commit(&mut self, event: Value) -> Result<Value> {
        let bytes = serde_json::to_vec_pretty(&event)?;
        self.commit_bytes(
            event,
            bytes,
            if self.is_coordinator() {
                "coordinated"
            } else {
                "local"
            },
            None,
        )
    }
    fn commit_bytes(
        &mut self,
        event: Value,
        bytes: Vec<u8>,
        source: &str,
        receipt: Option<Value>,
    ) -> Result<Value> {
        self.ensure_messaging_writable()?;
        if let Some(reason) = &self.degraded {
            return Err(err("store_read_only", reason.clone()));
        }
        // Expiry and message publication share this writer lock. A closed
        // watch retains the arrival fence captured before a later publication.
        self.advance_monitors_at(Utc::now())?;
        let path = self.event_path(&event)?;
        if bytes.len() > 65536 {
            return Err(err("event_too_large", "Serialized event exceeds 64 KiB"));
        }
        let pos = self
            .position
            .checked_add(1)
            .ok_or_else(|| err("position_overflow", "Journal exhausted"))?;
        let received = now();
        self.clock = self.clock.max(
            required(&event, "logical_time")?
                .parse::<u64>()
                .map_err(|_| err("invalid_record", "Bad logical time"))?,
        );
        let receipt = receipt.or_else(|| self.is_coordinator().then(|| json!({"space_id":self.space_id,"coordinator_id":self.daemon_id,"generation":self.generation,"position":format!("{pos:020}"),"event_id":event["id"],"event_sha256":hash(&bytes)})));
        let j = json!({"protocol":1,"replica_id":self.daemon_id,"generation":self.generation,"position":format!("{pos:020}"),"recorded_at":received,"kind":"publish","event_id":event["id"],"event_sha256":hash(&bytes),"data":{"source":source,"received_at":received,"logical_clock":self.clock,"event_path":path.strip_prefix(&self.root).unwrap().to_string_lossy(),"hub_receipt":receipt}});
        // No reader sees the body until the durable publication record exists.
        let result = (|| -> io::Result<()> {
            write_file(&path, &bytes, false)?;
            write_file(
                &self.journal_dir().join(format!("{pos:020}.json")),
                &serde_json::to_vec_pretty(&j)?,
                false,
            )
        })();
        if let Err(e) = result {
            self.degraded = Some(format!("Publication outcome requires reconciliation: {e}"));
            return Err(err("outcome_unknown", self.degraded.clone().unwrap()));
        }
        self.position = pos;
        self.apply_replication_journal(&j)?;
        if let Err(e) = self.reduce(&event) {
            self.degraded = Some(e.to_string());
            return Err(e);
        }
        self.events.push(Published {
            event: event.clone(),
            position: pos,
            received_at: received,
        });
        // A durable message stays successful if a derived checkpoint fails;
        // restart replays from the prior saved monitor scan position.
        if let Err(e) = self.advance_monitors_at(Utc::now()) {
            self.degraded = Some(format!("Monitor checkpoint requires reconciliation: {e}"));
        }
        self.replication.signal.signal();
        Ok(event)
    }
    fn publish(
        &mut self,
        ty: &str,
        conv: Option<&str>,
        body: &str,
        data: Value,
        actor: &str,
        name: &str,
        kind: &str,
        key: &str,
        digest: &str,
    ) -> Result<Value> {
        if !matches!(ty, "message.create" | "read.ack") {
            self.shared_mutation_allowed()?;
        }
        let e = self.event(ty, conv, body, data, actor, name, kind, key, digest)?;
        self.commit(e)
    }
    pub fn participant_id(&self, uid: &str) -> String {
        format!("agent:{}:{uid}", self.daemon_id)
    }
    fn allocate(&self, actor: &str, base: &str, uid: &str) -> Result<Name> {
        if base.chars().any(char::is_control) {
            return Err(err("invalid_name", "Choose a name without controls"));
        }
        let base = dashed_name(base);
        let count = base.graphemes(true).count();
        if !(2..=40).contains(&count) {
            return Err(err(
                "invalid_name",
                "Choose a name of 2–40 characters without controls",
            ));
        }
        if ["owner", "system"].contains(&name_key(&base).as_str()) {
            return Err(err("reserved_name", "Owner and System are reserved"));
        }
        let occupied = |s: &str| {
            let n = name_key(s);
            self.names.iter().any(|(id, v)| {
                id != actor
                    && (name_key(&v.name) == n || v.aliases.iter().any(|a| name_key(a) == n))
            })
        };
        let mut name = base.to_string();
        if occupied(&name) {
            let suffix = hash(actor.as_bytes());
            for len in 3..=32 {
                let tail = format!("-{}", &suffix[..len]);
                let take = 40 - tail.graphemes(true).count();
                name = format!(
                    "{}{}",
                    base.graphemes(true).take(take).collect::<String>(),
                    tail
                );
                if !occupied(&name) {
                    break;
                }
            }
            if occupied(&name) {
                return Err(err("name_conflict", "No unique name available"));
            }
        }
        let old = self.names.get(actor);
        let mut aliases = old.map(|n| n.aliases.clone()).unwrap_or_default();
        if let Some(old) = old {
            if old.name != name && !aliases.contains(&old.name) {
                aliases.push(old.name.clone());
            }
        }
        Ok(Name {
            name,
            aliases,
            revision: old
                .map(|n| n.revision.checked_add(1))
                .unwrap_or(Some(1))
                .ok_or_else(|| err("revision_overflow", "Name revision exhausted"))?,
            revision_id: uuid(),
            session_uid: uid.into(),
        })
    }
    /// Upgrade chosen chat names through retained identity events. The old
    /// spelling remains an alias; replay/reopen cannot rename the same record
    /// twice, and provisional session labels are outside this store.
    fn normalize_existing_names(&mut self) -> Result<()> {
        let pending: Vec<_> = self.names.iter()
            .filter(|(_, n)| dashed_name(&n.name) != n.name)
            .map(|(actor, n)| (actor.clone(), n.clone()))
            .collect();
        for (actor, old) in pending {
            let name = self.allocate(&actor, &old.name, &old.session_uid)?;
            let mut identity = serde_json::to_value(&name)?;
            identity["participant_id"] = json!(actor);
            self.publish(
                "identity.update", None,
                &format!("System normalized session name to {}", name.name),
                json!({"identity":identity}),
                "system", "System", "system",
                &format!("normalize-name-v1:{actor}:{}", old.revision_id), "",
            )?;
        }
        Ok(())
    }
    fn visible(&self, actor: &str, conv: &str) -> bool {
        self.channels.values().any(|c| c == conv)
            || self
                .conversations
                .get(conv)
                .is_some_and(|m| m.iter().any(|p| p == actor))
    }
    fn resolve(
        &self,
        actor: &str,
        p: &Value,
        people: &[Person],
        create_dm: bool,
    ) -> Result<(String, Option<Value>)> {
        let selectors = ["channel", "dm", "conversation"]
            .iter()
            .filter(|k| p.get(**k).is_some_and(|v| !v.is_null()))
            .count();
        if selectors != 1 {
            return Err(err(
                "invalid_target",
                "Choose exactly one channel, dm, or conversation",
            ));
        }
        if let Some(path) = p["channel"].as_str() {
            return self
                .channels
                .get(path)
                .map(|id| (id.clone(), None))
                .ok_or_else(|| {
                    err(
                        "not_found",
                        "Channel not found; create it with chat_channels",
                    )
                });
        }
        if let Some(id) = p["conversation"].as_str() {
            return if self.visible(actor, id) {
                Ok((id.into(), None))
            } else {
                Err(err("not_found", "Conversation not found"))
            };
        }
        let peers: Vec<String> = match &p["dm"] {
            Value::String(peer) => vec![peer.clone()],
            Value::Array(peers) => peers
                .iter()
                .map(|v| {
                    v.as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            err(
                                "invalid_target",
                                "DM recipients must be participant IDs or names",
                            )
                        })
                })
                .collect::<Result<_>>()?,
            _ => {
                return Err(err(
                    "invalid_target",
                    "dm must be a participant or a list of recipients",
                ))
            }
        };
        if peers.is_empty() || peers.len() > 31 {
            return Err(err(
                "invalid_target",
                "Choose 1–31 DM recipients; the sender is included automatically",
            ));
        }
        let mut members = vec![actor.to_owned()];
        for peer in peers {
            let peer = if peer == "owner"
                || self.names.contains_key(&peer)
                || people.iter().any(|x| x.id == peer)
            {
                peer
            } else {
                let mut hits: Vec<_> = self
                    .names
                    .iter()
                    .filter(|(_, n)| {
                        normalize(&n.name) == normalize(&peer)
                            || n.aliases.iter().any(|a| normalize(a) == normalize(&peer))
                    })
                    .collect();
                // Honor exact legacy names/aliases first: before this policy,
                // "Build Scout" and "Build-Scout" could belong to different IDs.
                if hits.is_empty() {
                    hits = self
                        .names
                        .iter()
                        .filter(|(_, n)| {
                            name_key(&n.name) == name_key(&peer)
                                || n.aliases.iter().any(|a| name_key(a) == name_key(&peer))
                        })
                        .collect();
                }
                if hits.len() != 1 {
                    return Err(err(
                        "not_found",
                        "Peer not found; use chat_people to resolve a participant ID",
                    ));
                }
                hits[0].0.clone()
            };
            if members.contains(&peer) {
                return Err(err(
                    "invalid_target",
                    "Choose distinct recipients and omit yourself",
                ));
            }
            members.push(peer);
        }
        members.sort();
        if let Some((id, _)) = self.conversations.iter().find(|(_, m)| **m == members) {
            return Ok((id.clone(), None));
        }
        if !create_dm {
            return Ok((String::new(), None));
        }
        let id = uuid();
        Ok((
            id.clone(),
            Some(json!({"id":id,"kind":"dm","members":members})),
        ))
    }
    fn request(
        &self,
        actor: &str,
        p: &Value,
        operation: &str,
    ) -> Result<(String, String, Option<Value>)> {
        if !p.is_object() {
            return Err(err("invalid_params", "Expected an object"));
        }
        let key = required(p, "request_id")?;
        if self
            .personal
            .get(actor)
            .is_some_and(|s| s.operations.contains_key(&key))
        {
            return Err(err(
                "idempotency_conflict",
                "This request_id belongs to a personal-state operation",
            ));
        }
        if key.len() > 160 || key.chars().any(char::is_control) {
            return Err(err("invalid_params", "Invalid request_id"));
        }
        if p["origin_daemon_id"]
            .as_str()
            .is_some_and(|o| o != self.operation_origin())
        {
            return Err(err(
                "retry_origin_unavailable",
                "Retry this operation through its original daemon",
            ));
        }
        let mut intent = p.clone();
        for key in ["ack_receipt", "norms_seen", "origin_daemon_id"] {
            intent.as_object_mut().unwrap().remove(key);
        }
        let digest = hash(&serde_json::to_vec(
            &json!({"operation":operation,"params":intent}),
        )?);
        if let Some((id, old, _)) = self.requests.get(&format!("{actor}\n{key}")) {
            if old != &digest {
                return Err(err(
                    "idempotency_conflict",
                    "This request_id already has different content",
                ));
            }
            return Ok((
                key,
                digest,
                self.events
                    .iter()
                    .find(|x| strv(&x.event, "id") == id)
                    .map(|x| x.event.clone()),
            ));
        }
        Ok((key, digest, None))
    }
    pub fn send(
        &mut self,
        actor: &str,
        uid: &str,
        kind: &str,
        p: &Value,
        people: &[Person],
    ) -> Result<Value> {
        let (key, digest, prior) = self.request(actor, p, "message.create")?;
        if let Some(e) = prior {
            return Ok(self.send_response(&e));
        }
        if !self.is_coordinator()
            && self
                .host_info(&self.daemon_id)
                .is_some_and(|h| h["active"] == false)
        {
            return Err(err(
                "host_revoked",
                "This host's messaging enrollment was revoked; retain the draft",
            ));
        }
        let body = required(p, "body")?.replace("\r\n", "\n");
        if body.trim().is_empty() {
            return Err(err("invalid_message", "Message must contain text"));
        }
        if body.chars().count() > 3000 {
            return Err(err("message_too_long",format!("{} characters exceeds 3000; send a short summary and reference a file. Quick replies are welcome.",body.chars().count())));
        }
        for (field, max, len) in [("mentions", 32, 256), ("tags", 16, 32), ("links", 8, 0)] {
            if let Some(v) = p.get(field) {
                let a = v
                    .as_array()
                    .ok_or_else(|| err("invalid_params", format!("{field} must be an array")))?;
                if a.len() > max {
                    return Err(err("invalid_params", format!("Too many {field}")));
                }
                if len > 0
                    && a.iter().any(|v| {
                        v.as_str()
                            .is_none_or(|x| x.is_empty() || x.chars().count() > len)
                    })
                {
                    return Err(err("invalid_params", format!("Invalid {field}")));
                }
            }
        }
        let (conv, create) = self.resolve(actor, p, people, true)?;
        if create.is_some() || actor != "owner" && !self.names.contains_key(actor) {
            self.shared_mutation_allowed()?;
        }
        if p.get("mention_here").is_some_and(|v| !v.is_boolean()) {
            return Err(err("invalid_params", "mention_here must be a boolean"));
        }
        self.enroll_participant(actor)?;
        if self.channels.values().any(|id| id == &conv) && !self.joined(actor, &conv) {
            return Err(err("join_required", "Join this channel before posting: chat_channels(action=\"join\", conversation=<channel_id>, request_id=<new_id>). Public history remains browsable."));
        }
        if let Some((path, id)) = self.channels.iter().find(|(_, id)| *id == &conv) {
            if self.channel_info(path, id)["archived"] == true {
                return Err(err(
                    "conversation_archived",
                    "This channel is archived; retain the draft until an admin reopens it",
                ));
            }
        }
        let reply = p["reply_to"].as_str();
        let root = if let Some(id) = reply {
            let parent = self
                .events
                .iter()
                .find(|e| {
                    e.event["id"] == id
                        && e.event["type"] == "message.create"
                        && e.event["conversation_id"] == conv
                })
                .ok_or_else(|| err("not_found", "Reply parent not found in this conversation"))?;
            Some(
                parent.event["data"]["thread_root"]
                    .as_str()
                    .unwrap_or(id)
                    .to_string(),
            )
        } else {
            None
        };
        let members = create
            .as_ref()
            .and_then(|c| c["members"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .or_else(|| self.conversations.get(&conv).cloned());
        let mentions: BTreeSet<String> = p["mentions"].as_array().into_iter().flatten()
            .filter_map(Value::as_str).map(str::to_owned).collect();
        for m in &mentions {
            let id = m.as_str();
            if !people.iter().any(|x| x.id == id) && !self.names.contains_key(id) && id != "owner" {
                return Err(err("not_found", "Mention participant not found"));
            }
            if members.as_ref().is_some_and(|v| !v.iter().any(|x| x == id)) {
                return Err(err(
                    "invalid_mention",
                    "DM mentions are restricted to its members",
                ));
            }
        }
        let mention_here = p["mention_here"] == true;
        if mention_here && members.is_some() {
            return Err(err(
                "invalid_mention",
                "@here is available in channels; DMs already notify their members",
            ));
        }
        let mut recipients = mentions.clone();
        if mention_here {
            recipients.extend(self.memberships.get(&conv).into_iter().flatten().cloned());
        }
        let links = p["links"].as_array().cloned().unwrap_or_default();
        for l in &links {
            if strv(l, "label").chars().count() > 120
                || strv(l, "uri").is_empty()
                || strv(l, "uri").chars().count() > 2048
            {
                return Err(err(
                    "invalid_link",
                    "Links need a URI (max 2048) and a short label (max 120)",
                ));
            }
        }
        let chosen = if actor == "owner" {
            None
        } else if self.names.contains_key(actor) {
            None
        } else {
            Some(self.allocate(actor, &required(p, "name")?, uid)?)
        };
        let name = chosen
            .as_ref()
            .map(|n| n.name.as_str())
            .or_else(|| self.names.get(actor).map(|n| n.name.as_str()))
            .unwrap_or("Owner")
            .to_owned();
        if p.get("norms_seen").is_some_and(|v| !v.is_object()) {
            return Err(err("invalid_params", "norms_seen must be an object"));
        }
        let identity_revision = chosen
            .as_ref()
            .or_else(|| self.names.get(actor))
            .map(|n| n.revision_id.as_str())
            .unwrap_or(&self.owner_identity_revision);
        let enrollment_revision = self
            .replication
            .hosts
            .get(self.operation_origin())
            .and_then(|h| h["enrollment_revision"].as_str())
            .unwrap_or(&self.enrollment_revision);
        let conversation_revision = self
            .channels
            .iter()
            .find(|(_, id)| *id == &conv)
            .map(|(path, id)| self.channel_info(path, id)["revision"].clone())
            .unwrap_or(json!(conv));
        let mut data = json!({"reply_to":reply,"thread_root":root,"mentions":mentions,"tags":p.get("tags").cloned().unwrap_or(json!([])),"links":links,"norms_seen":p.get("norms_seen").cloned().unwrap_or(json!({})),"metadata_seen":{"identity":identity_revision,"conversation":conversation_revision,"enrollment":enrollment_revision}});
        data["metadata_seen"]["membership"] = json!(self.membership_revisions.get(&conv));
        data["mention_here"] = json!(mention_here);
        data["mention_recipients"] = json!(recipients);
        if let Some(c) = create {
            data["conversation_create"] = c;
        }
        if let Some(n) = chosen {
            let mut claim = serde_json::to_value(n)?;
            claim["participant_id"] = json!(actor);
            data["identity_claim"] = claim.clone();
            let att = self.event(
                "identity.update",
                None,
                &format!("{name} joined messaging"),
                json!({"record_kind":"attestation","identity":claim}),
                "system",
                "System",
                "system",
                &uuid(),
                "",
            )?;
            data["identity_attestation"] = json!(serde_json::to_string(&att)?);
        }
        let e = self.publish(
            "message.create",
            Some(&conv),
            &body,
            data,
            actor,
            &name,
            kind,
            &key,
            &digest,
        )?;
        let repair = self.repair_attestations().and_then(|_| self.project());
        let mut result = self.send_response(&e);
        if let Err(e) = repair {
            result["projection_status"] = json!(e.to_string());
        }
        Ok(result)
    }
    fn send_response(&self, e: &Value) -> Value {
        json!({"event":e,"event_id":e["id"],"name":e["actor"]["name"],"operation":{"space_id":self.space_id,"actor_id":e["actor"]["id"],"origin_daemon_id":e["request"]["origin_daemon_id"],"request_id":e["request"]["key"]},"position":self.position_token(self.position),"replication":self.event_replication(strv(e,"id"))["status"],"sync":self.sync_status(),"notification":self.notification_status(e),"name_publication":if e["data"]["identity_claim"].is_null() || self.events.iter().any(|v|v.event["type"]=="identity.update" && v.event["data"]["identity"]==e["data"]["identity_claim"]){"published"}else{"pending"},"norms":self.norms})
    }
    pub fn rename(&mut self, actor: &str, uid: &str, p: &Value) -> Result<Value> {
        let initiator = p["_authenticated_actor"].as_str().unwrap_or(actor);
        let (key, digest, prior) = self.request(initiator, p, "identity.update")?;
        if let Some(e) = prior {
            return Ok(self.send_response(&e));
        }
        let old = self.names.get(actor).ok_or_else(|| {
            err(
                "not_enrolled",
                "This session has not chosen a messaging name yet",
            )
        })?;
        if p["expected_name_revision"].as_u64() != Some(old.revision) {
            return Err(err(
                "name_revision_conflict",
                format!("Current name revision is {}", old.revision),
            ));
        }
        let n = self.allocate(actor, &required(p, "name")?, uid)?;
        let mut v = serde_json::to_value(&n)?;
        v["participant_id"] = json!(actor);
        let e = self.publish(
            "identity.update",
            None,
            &format!("Session renamed to {}", n.name),
            json!({"identity":v}),
            initiator,
            if initiator == "owner" {
                "Owner"
            } else {
                &n.name
            },
            if initiator == "owner" {
                "owner"
            } else {
                "agent"
            },
            &key,
            &digest,
        )?;
        let _ = self.project();
        Ok(self.send_response(&e))
    }
    pub fn people(&self, live: &[Person]) -> Value {
        let mut people = live.to_vec();
        for (id, n) in &self.names {
            if let Some(p) = people.iter_mut().find(|p| p.id == *id) {
                p.name = n.name.clone();
            } else {
                people.push(Person {
                    id: id.clone(),
                    name: n.name.clone(),
                    session_uid: n.session_uid.clone(),
                    task: None,
                    present: false,
                    kind: "agent".into(),
                });
            }
        }
        json!(people
            .into_iter()
            .map(|p| {
                let mut v = serde_json::to_value(&p).unwrap();
                if let Some(n) = self.names.get(&p.id) {
                    v["name_revision"] = json!(n.revision);
                    v["aliases"] = json!(n.aliases);
                }
                v
            })
            .collect::<Vec<_>>())
    }
    pub fn channel_info(&self, path: &str, id: &str) -> Value {
        let mut channel = self.channel_at(path, id, self.position);
        channel["member_count"] = json!(self.memberships.get(id).map_or(0, BTreeSet::len));
        channel["membership_revision"] = json!(self.membership_revisions.get(id));
        channel
    }
    pub fn channels(&self) -> Value {
        json!(self
            .channels
            .iter()
            .map(|(path, id)| self.channel_info(path, id))
            .collect::<Vec<_>>())
    }
    pub fn channels_for(&mut self, actor: &str) -> Result<Vec<Value>> {
        self.load_read(actor)?;
        let mut channels = self.channels().as_array().cloned().unwrap_or_default();
        for channel in &mut channels {
            let unread: Vec<_> = self
                .events
                .iter()
                .filter(|e| {
                    e.event["type"] == "message.create"
                        && e.event["conversation_id"] == channel["id"]
                        && e.event["actor"]["id"] != actor
                        && !self.reads[actor].ids.contains(strv(&e.event, "id"))
                })
                .collect();
            self.channel_permissions(actor, channel);
            channel["unread"] = json!(unread.len());
            channel["mentions"] = json!(unread
                .iter()
                .filter(|e| mention_recipients(&e.event).contains(&actor))
                .count());
        }
        Ok(channels)
    }
    pub(super) fn position_token(&self, pos: u64) -> Value {
        json!({"space_id":self.space_id,"replica_id":self.daemon_id,"generation":self.generation,"position":pos})
    }
    fn check_position(&self, v: &Value) -> Result<u64> {
        if v["space_id"] != self.space_id
            || v["replica_id"] != self.daemon_id
            || v["generation"] != self.generation
        {
            return Err(err(
                "resync_required",
                "Position belongs to another replica or generation",
            ));
        }
        v["position"]
            .as_u64()
            .filter(|p| *p <= self.position)
            .ok_or_else(|| err("invalid_cursor", "Invalid position"))
    }
    fn load_read(&mut self, actor: &str) -> Result<()> {
        if !self.reads.contains_key(actor) {
            let p = self
                .root
                .join("_state")
                .join(format!("{}.json", hash(actor.as_bytes())));
            let state = if p.exists() {
                serde_json::from_value(load(&p)?)?
            } else {
                ReadState::default()
            };
            self.reads.insert(actor.into(), state);
        }
        Ok(())
    }
    pub fn acknowledge(&mut self, actor: &str, receipt: &Value) -> Result<()> {
        self.load_read(actor)?;
        if receipt.is_null() {
            return Ok(());
        }
        if receipt["actor"] != actor || receipt["space_id"] != self.space_id {
            return Err(err("invalid_receipt", "Receipt belongs to another reader"));
        }
        let ids: Vec<String> = serde_json::from_value(receipt["ids"].clone())?;
        if ids.len() > 200 {
            return Err(err("invalid_receipt", "Receipt too large"));
        }
        for id in &ids {
            if !self.events.iter().any(|e| {
                e.event["id"] == *id && self.visible(actor, strv(&e.event, "conversation_id"))
            }) {
                return Err(err("not_found", "Receipt message not found"));
            }
        }
        let unseen: BTreeSet<_> = ids
            .into_iter()
            .filter(|id| !self.reads[actor].ids.contains(id))
            .collect();
        if unseen.is_empty() {
            return Ok(());
        }
        self.ensure_messaging_writable()?;
        if actor == "owner" && self.sync_enabled() {
            self.publish(
                "read.ack",
                None,
                "Owner acknowledged messages",
                json!({"ids":unseen}),
                "owner",
                "Owner",
                "owner",
                &uuid(),
                "",
            )?;
            Ok(())
        } else {
            self.merge_read_ids(actor, unseen)
        }
    }

    pub fn read(&mut self, actor: &str, p: &Value, people: &[Person]) -> Result<Value> {
        if !p.is_object() {
            return Err(err("invalid_params", "Expected an object"));
        }
        let mut normalized;
        let p = if p["inbox"] != true
            && p["dms"] != true
            && ["channel", "dm", "conversation"]
                .iter()
                .all(|k| p.get(*k).is_none_or(Value::is_null))
        {
            normalized = p.clone();
            if let Some(thread) = p["thread"].as_str() {
                let event = self.events.iter().find(|e| e.event["id"] == thread)
                    .ok_or_else(|| err("not_found", "Thread is not available in the local cache; request hub freshness to fetch it"))?;
                normalized["conversation"] = event.event["conversation_id"].clone();
            } else {
                normalized["channel"] = json!("general");
            }
            &normalized
        } else {
            p
        };
        self.acknowledge(actor, &p["ack_receipt"])?;
        if p["freshness"]
            .as_str()
            .is_some_and(|s| !["cached", "hub"].contains(&s))
        {
            return Err(err("invalid_params", "freshness must be cached or hub"));
        }
        let public = p["channel"] == "*";
        let broad = p["dms"] == true || p["inbox"] == true;
        if p["dms"] == true && p["inbox"] == true {
            return Err(err("invalid_target", "Choose inbox or incoming DMs"));
        }
        if broad
            && ["channel", "dm", "conversation"]
                .iter()
                .any(|k| p.get(*k).is_some_and(|v| !v.is_null()))
        {
            return Err(err(
                "invalid_target",
                "Do not combine inbox with a conversation",
            ));
        }
        let conv = if broad || public {
            None
        } else {
            Some(self.resolve(actor, p, people, false)?.0)
        };
        let mut filter = p.clone();
        for k in ["cursor", "ack_receipt", "limit"] {
            filter.as_object_mut().unwrap().remove(k);
        }
        let digest = hash(&serde_json::to_vec(&filter)?);
        let cursor = p.get("cursor").filter(|v| !v.is_null());
        let high = if let Some(c) = cursor {
            if c["filter"] != digest {
                return Err(err("invalid_cursor", "Cursor filters changed"));
            }
            self.check_position(&c["snapshot"])?
        } else {
            self.position
        };
        let after = if let Some(a) = p.get("after").filter(|v| !v.is_null()) {
            self.check_position(a)?
        } else {
            0
        };
        let last_id = cursor.and_then(|c| c["last_id"].as_str());
        let basis = p["time_basis"].as_str().unwrap_or("created");
        if !["created", "received"].contains(&basis) {
            return Err(err(
                "invalid_params",
                "time_basis must be created or received",
            ));
        }
        let (start, end) = if let Some(c) = cursor {
            (
                c["start"].as_str().map(str::to_string),
                c["end"].as_str().map(str::to_string),
            )
        } else {
            time_bounds(&p["time"])?
        };
        let start_dt = start.as_deref().map(parse_time).transpose()?;
        let end_dt = end.as_deref().map(parse_time).transpose()?;
        let tags: Vec<String> = if p.get("tags").is_some() {
            serde_json::from_value(p["tags"].clone())?
        } else {
            vec![]
        };
        let pin_states = self.pin_states(high);
        let read = &self.reads[actor].ids;
        let mut items: Vec<_> = self
            .events
            .iter()
            .filter(|e| {
                let v = &e.event;
                let cid = strv(v, "conversation_id");
                if v["type"] == "conversation.pin" || v["type"] == "channel.membership"
                    || (p["pinned_only"] == true && !pin_states.contains_key(strv(v, "id")))
                    || v["conversation_id"].is_null()
                    || e.position > high
                    || e.position <= after
                    || !self.visible(actor, cid)
                {
                    return false;
                }
                if let Some(conv) = &conv {
                    if cid != conv {
                        return false;
                    }
                }
                if public && !self.channels.values().any(|id| id == cid) {
                    return false;
                }
                if broad {
                    let incoming = v["actor"]["id"] != actor;
                    let eligible = if p["inbox"] == true {
                        self.preference_for_event(actor, e).0 || self.monitor_inbox(actor,e)
                    } else {
                        self.conversations.contains_key(cid)
                    };
                    if (!incoming && !(p["inbox"] == true && self.monitor_inbox(actor,e))) || !eligible {
                        return false;
                    }
                }
                if p["unread_only"] == true
                    && (read.contains(strv(v, "id")) || v["actor"]["id"] == actor)
                {
                    return false;
                }
                if let Some(thread) = p["thread"].as_str() {
                    if v["id"] != thread && v["data"]["thread_root"] != thread {
                        return false;
                    }
                }
                if !tags.iter().all(|t| {
                    v["data"]["tags"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|v| v == t))
                }) {
                    return false;
                }
                let time = if basis == "received" {
                    &e.received_at
                } else {
                    strv(v, "created_at")
                };
                let Ok(t) = parse_time(time) else {
                    return false;
                };
                start_dt.is_none_or(|s| t >= s) && end_dt.is_none_or(|end| t < end)
            })
            .collect();
        if p.get("after").is_some() {
            items.sort_by_key(|e| e.position);
        } else {
            items.sort_by_key(|e| {
                (
                    e.event["logical_time"]
                        .as_str()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0),
                    strv(&e.event, "id"),
                )
            });
        }
        let newest = p["newest_first"] == true && p.get("after").is_none();
        if newest {
            items.reverse();
        }
        if let Some(last) = last_id {
            let anchor = self
                .events
                .iter()
                .find(|e| strv(&e.event, "id") == last)
                .ok_or_else(|| err("invalid_cursor", "Missing page anchor"))?;
            // Cursor advances through the ordering, even if acknowledged items
            // disappear from an unread-only query between pages.
            items.retain(|e| {
                if p.get("after").is_some() {
                    e.position > anchor.position
                } else {
                    let order = (
                        strv(&e.event, "logical_time").parse::<u64>().unwrap_or(0),
                        strv(&e.event, "id"),
                    )
                        .cmp(&(
                            strv(&anchor.event, "logical_time")
                                .parse::<u64>()
                                .unwrap_or(0),
                            strv(&anchor.event, "id"),
                        ));
                    if newest {
                        order.is_lt()
                    } else {
                        order.is_gt()
                    }
                }
            });
        }
        let limit = p["limit"].as_u64().unwrap_or(50).clamp(1, 200) as usize;
        let mut out = vec![];
        let mut chars = 0;
        let mut bytes = 0;
        let mut index = 0;
        while index < items.len() && out.len() < limit {
            let e = items[index];
            let n = strv(&e.event, "body").chars().count();
            let mut v = e.event.clone();
            v["pinned"] = json!(pin_states.contains_key(strv(&v, "id")));
            v["pin"] = pin_states.get(strv(&v, "id")).cloned().unwrap_or(Value::Null);
            v["received_at"] = json!(e.received_at);
            v["read"] = json!(read.contains(strv(&e.event, "id")) || e.event["actor"]["id"] == actor);
            v["conversation_kind"] = json!(if self.conversations.contains_key(strv(&e.event, "conversation_id")) { "dm" } else { "channel" });
            if e.event["actor"]["id"] == actor {
                v["notification"] = self.notification_status(&e.event);
            }
            let size = serde_json::to_vec(&v)?.len();
            if chars + n > 16000 || bytes + size > 450000 {
                break;
            }
            chars += n;
            bytes += size;
            out.push(v);
            index += 1;
        }
        let next = if index < items.len() {
            json!({"filter":digest,"snapshot":self.position_token(high),"last_id":out.last().map(|v|&v["id"]),"start":start,"end":end})
        } else {
            Value::Null
        };
        let ids = out.iter().map(|v| v["id"].clone()).collect::<Vec<_>>();
        let mut context = vec![];
        for v in &out {
            if let Some(parent) = v["data"]["reply_to"].as_str() {
                if !ids.iter().any(|id| id == parent)
                    && !context.iter().any(|x: &Value| x["id"] == parent)
                {
                    if let Some(e) = self.events.iter().find(|e| e.event["id"] == parent) {
                        if e.position <= high
                            && self.visible(actor, strv(&e.event, "conversation_id"))
                            && chars + strv(&e.event, "body").chars().count() <= 16000
                            && bytes + serde_json::to_vec(&e.event)?.len() <= 450000
                        {
                            bytes += serde_json::to_vec(&e.event)?.len();
                            chars += strv(&e.event, "body").chars().count();
                            context.push(e.event.clone());
                        }
                    }
                }
            }
        }
        let target = conv
            .as_deref()
            .and_then(|id| self.channels.iter().find(|(_, cid)| cid.as_str() == id))
            .map(|(path, id)| {
                let mut c = self.channel_at(path, id, high);
                self.channel_permissions(actor, &mut c);
                c
            });
        let pins_revision = conv.as_deref().map(|id| self.pins_revision(id, high));
        let pins: Option<BTreeMap<_, _>> = conv.as_deref().map(|cid| {
            self.events
                .iter()
                .filter(|e| e.event["conversation_id"] == cid)
                .filter_map(|e| {
                    pin_states
                        .get(strv(&e.event, "id"))
                        .map(|pin| (strv(&e.event, "id"), pin.clone()))
                })
                .collect()
        });
        Ok(
            json!({"target":target,"pins_revision":pins_revision,"pins":pins,"items":out,"context":context,"next_cursor":next,"position":self.position_token(high),"receipt":{"actor":actor,"space_id":self.space_id,"ids":ids},"time":{"start":start,"end":end,"basis":basis},"coverage":if self.degraded.is_some(){"partial"}else{"complete"},"connection":"local","norms":self.norms,"degraded":self.degraded}),
        )
    }
    pub fn dms(&mut self, actor: &str, unread: bool) -> Result<Value> {
        self.dms_page(actor, &json!({"unread_only":unread}))
    }
    pub fn dms_page(&mut self, actor: &str, p: &Value) -> Result<Value> {
        self.load_read(actor)?;
        let mut out = vec![];
        for (id, m) in &self.conversations {
            if !m.iter().any(|x| x == actor) {
                continue;
            }
            let peers: Vec<_> = m.iter().filter(|x| x.as_str() != actor).collect();
            let peer = (peers.len() == 1).then(|| peers[0]);
            if p["peer"].as_str().is_some_and(|s| !peers.iter().any(|id| id.as_str() == s)) {
                continue;
            }
            let events: Vec<_> = self
                .events
                .iter()
                .filter(|e| {
                    e.event["conversation_id"] == *id && e.event["type"] == "message.create"
                })
                .collect();
            let count = events
                .iter()
                .filter(|e| {
                    e.event["actor"]["id"] != actor
                        && !self.reads[actor].ids.contains(strv(&e.event, "id"))
                })
                .count();
            if p["unread_only"] == true && count == 0 {
                continue;
            }
            let last=events.last().map(|e|json!({"id":e.event["id"],"actor":e.event["actor"],"created_at":e.event["created_at"],"preview":strv(&e.event,"body").chars().take(180).collect::<String>()}));
            out.push(json!({"id":id,"peer":peer,"peers":peers,"members":m,"group":m.len()>2,"unread":count,"last":last}));
        }
        self.directory_page(actor, p, out)
    }
    pub fn directory_page(&self, actor: &str, p: &Value, mut items: Vec<Value>) -> Result<Value> {
        items.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        let mut filter = p.clone();
        filter
            .as_object_mut()
            .ok_or_else(|| err("invalid_params", "Expected object"))?
            .remove("cursor");
        filter.as_object_mut().unwrap().remove("limit");
        let digest = hash(format!("{actor}:{filter}").as_bytes());
        let cursor = p.get("cursor").filter(|v| !v.is_null());
        if let Some(c) = cursor {
            self.check_position(&c["snapshot"])?;
            if c["filter"] != digest {
                return Err(err(
                    "invalid_cursor",
                    "Directory cursor belongs to another query",
                ));
            }
        }
        let last = cursor.and_then(|v| v["last"].as_str()).unwrap_or("");
        items.retain(|v| v["id"].as_str().unwrap_or("") > last);
        let limit = p["limit"].as_u64().unwrap_or(50).clamp(1, 200) as usize;
        let mut bytes = 0;
        let mut chars = 0;
        let mut count = 0;
        for item in items.iter().take(limit) {
            let size = serde_json::to_vec(item)?.len();
            let text_size = item.to_string().chars().count();
            if count > 0 && (bytes + size > 450000 || chars + text_size > 14000) {
                break;
            }
            bytes += size;
            chars += text_size;
            count += 1;
        }
        let more = items.len() > count;
        items.truncate(count);
        let next = if more {
            json!({"filter":digest,"snapshot":self.position_token(self.position),"last":items.last().unwrap()["id"]})
        } else {
            Value::Null
        };
        Ok(
            json!({"items":items,"next_cursor":next,"coverage":"complete","daemon_id":self.daemon_id,"space_id":self.space_id,"degraded":self.degraded}),
        )
    }
    fn notification_status(&self, event: &Value) -> Value {
        let mut statuses = vec![];
        for (uid, id, _) in self
            .notifications()
            .into_iter()
            .filter(|(_, id, _)| event["id"] == *id)
        {
            statuses.push(super::delivery::status(&self.root, &uid, &id));
        }
        json!(statuses)
    }
    pub fn notifications(&self) -> Vec<(String, String, String)> {
        self.wake_intents()
            .into_iter()
            .flat_map(|(uid, items)| {
                items
                    .into_iter()
                    .map(move |i| (uid.clone(), i.event_id, String::new()))
            })
            .collect()
    }
}
fn parse_time(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|x| x.with_timezone(&Utc))
        .map_err(|_| {
            err(
                "invalid_time",
                "Use RFC3339 timestamps with an explicit timezone",
            )
        })
}
fn time_bounds(v: &Value) -> Result<(Option<String>, Option<String>)> {
    if v.is_null() {
        return Ok((None, None));
    }
    if let Some(s) = v["since"].as_str() {
        if v.get("start").is_some() || v.get("end").is_some() {
            return Err(err(
                "invalid_time",
                "Do not combine relative and absolute ranges",
            ));
        }
        if !s.is_ascii() {
            return Err(err(
                "invalid_time",
                "Duration must use ASCII digits and units",
            ));
        }
        let (n, u) = s.split_at(s.len().saturating_sub(1));
        let n: i64 = n
            .parse()
            .map_err(|_| err("invalid_time", "Use a positive duration such as 10m"))?;
        let factor = match u {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86400,
            _ => return Err(err("invalid_time", "Use s, m, h or d")),
        };
        let seconds = n
            .checked_mul(factor)
            .filter(|n| *n > 0 && *n <= 315360000)
            .ok_or_else(|| err("invalid_time", "Duration out of range"))?;
        let end = Utc::now();
        return Ok((
            Some((end - chrono::Duration::seconds(seconds)).to_rfc3339()),
            Some(end.to_rfc3339()),
        ));
    }
    let start = v["start"].as_str().map(str::to_owned);
    let end = v["end"].as_str().map(str::to_owned);
    if start.is_none() && end.is_none() {
        return Err(err("invalid_time", "Specify a start or end"));
    }
    let a = start.as_deref().map(parse_time).transpose()?;
    let b = end.as_deref().map(parse_time).transpose()?;
    if a.zip(b).is_some_and(|(a, b)| a >= b) {
        return Err(err("invalid_time", "end must be after start"));
    }
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn person(s: &Store, uid: &str) -> Person {
        Person {
            id: s.participant_id(uid),
            name: uid.into(),
            session_uid: uid.into(),
            task: None,
            present: true,
            kind: "agent".into(),
        }
    }
    fn send(s: &mut Store, p: &Person, body: &str, key: &str, name: &str) -> Value {
        s.send(
            &p.id,
            &p.session_uid,
            "agent",
            &json!({"channel":"general","body":body,"request_id":key,"name":name}),
            &[p.clone()],
        )
        .unwrap()
    }
    #[test]
    fn names_collide_on_unicode_case_and_retries_survive_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let a = person(&s, "a");
        let b = person(&s, "b");
        let first = send(&mut s, &a, "Ready", "one", "Straße Scout");
        assert_eq!(first["name"], "Straße-Scout");
        let second = send(&mut s, &b, "Me too", "two", "STRASSE SCOUT");
        assert_ne!(
            normalize(first["name"].as_str().unwrap()),
            normalize(second["name"].as_str().unwrap())
        );
        assert_eq!(
            send(&mut s, &a, "Ready", "one", "Straße Scout")["event_id"],
            first["event_id"]
        );
        assert_eq!(s.send(&a.id,"a","agent",&json!({"channel":"general","body":"Changed","request_id":"one","name":"Straße Scout"}),&[a.clone()]).unwrap_err().code,"idempotency_conflict");
        let space = s.space_id.clone();
        drop(s);
        let mut s = Store::open(tmp.path()).unwrap();
        assert_eq!(s.space_id, space);
        assert_eq!(
            send(&mut s, &a, "Ready", "one", "Straße Scout")["event_id"],
            first["event_id"]
        );
    }
    #[test]
    fn dashed_names_protect_unicode_aliases_reserved_names_and_length() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let a = person(&s, "a");
        let b = person(&s, "b");
        assert_eq!(
            send(&mut s, &a, "Hi", "one", "  Build\u{a0}  Scout  ")["name"],
            "Build-Scout"
        );
        let second = send(&mut s, &b, "Hi", "two", "ＢＵＩＬＤ-ＳＣＯＵＴ");
        assert!(second["name"].as_str().unwrap().contains('-'));
        assert_ne!(
            name_key(second["name"].as_str().unwrap()),
            name_key("Build-Scout")
        );
        assert_eq!(
            s.resolve("owner", &json!({"dm":"build scout"}), &[], true)
                .unwrap()
                .1
                .unwrap()["members"],
            json!([a.id, "owner"])
        );
        s.rename(
            &a.id,
            "a",
            &json!({"name":"Gardener","request_id":"rename","expected_name_revision":1}),
        )
        .unwrap();
        assert_ne!(
            s.allocate(&b.id, "Build Scout", "b").unwrap().name,
            "Build-Scout"
        );
        for invalid in ["OＷNER", "system", "x", "\tScout", "Scout\nName"] {
            assert!(s.allocate(&b.id, invalid, "b").is_err(), "{invalid:?}");
        }
        let c = person(&s, "c");
        let long = "🐱".repeat(40);
        send(&mut s, &c, "Hi", "long", &long);
        let collision = s.allocate(&b.id, &long, "b").unwrap().name;
        assert_eq!(collision.graphemes(true).count(), 40);
        assert!(!collision.chars().any(char::is_whitespace));
    }
    #[test]
    fn legacy_name_migration_preserves_alias_routing_and_is_restart_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let a = person(&s, "a");
        let b = person(&s, "b");
        let c = person(&s, "c");
        // Retained records from the old policy could distinguish spaces/dashes.
        for (p, old) in [(&a, "Build Scout"), (&b, "Build-Scout"), (&c, "Alpha Beta")] {
            let mut n = s.allocate(&p.id, "Temporary", &p.session_uid).unwrap();
            n.name = old.into();
            let mut identity = serde_json::to_value(n).unwrap();
            identity["participant_id"] = json!(p.id);
            s.publish(
                "identity.update",
                None,
                "Legacy name",
                json!({"identity":identity}),
                "system",
                "System",
                "system",
                &format!("legacy-{}", p.session_uid),
                "",
            )
            .unwrap();
        }
        let dm = s
            .send(
                "owner",
                "",
                "owner",
                &json!({"dm":a.id,"body":"Hello","request_id":"hello"}),
                &[],
            )
            .unwrap();
        drop(s);
        let mut s = Store::open(tmp.path()).unwrap();
        assert!(s.names[&a.id].name.starts_with("Build-Scout-"));
        assert_eq!(s.names[&a.id].aliases, vec!["Build Scout"]);
        assert_eq!(s.names[&a.id].revision, 2);
        assert_eq!(s.names[&b.id].name, "Build-Scout");
        assert_eq!(s.names[&b.id].revision, 1);
        assert_eq!(s.names[&c.id].name, "Alpha-Beta");
        assert!(!s.names.contains_key("owner"));
        assert_eq!(s.resolve("owner", &json!({"dm":"Build Scout"}), &[], false).unwrap().0, dm["event"]["conversation_id"].as_str().unwrap());
        assert_eq!(s.resolve("owner", &json!({"dm":"Build-Scout"}), &[], true).unwrap().1.unwrap()["members"], json!([b.id, "owner"]));
        let count = s.events.len();
        let revision = s.names[&a.id].revision_id.clone();
        drop(s);
        s = Store::open(tmp.path()).unwrap();
        assert_eq!(s.events.len(), count);
        assert_eq!(s.names[&a.id].revision_id, revision);
        assert!(s.degraded.is_none());
    }
    #[test]
    fn rejected_first_message_never_claims_name_or_creates_dm() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let a = person(&s, "a");
        let b = person(&s, "b");
        let count = s.events.len();
        let e = s
            .send(
                &a.id,
                "a",
                "agent",
                &json!({"dm":b.id,"body":"x".repeat(3001),"name":"Scout","request_id":"bad"}),
                &[a.clone(), b],
            )
            .unwrap_err();
        assert_eq!(e.code, "message_too_long");
        assert_eq!(s.events.len(), count);
        assert!(s.names.is_empty() && s.conversations.is_empty());
    }
    #[test]
    fn private_first_claim_rebuilds_public_identity_without_dm() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let a = person(&s, "a");
        let b = person(&s, "b");
        let result = s
            .send(
                &a.id,
                "a",
                "agent",
                &json!({"dm":b.id,"body":"Private","name":"Scout","request_id":"private"}),
                &[a.clone(), b.clone()],
            )
            .unwrap();
        let cid = result["event"]["conversation_id"].as_str().unwrap();
        assert_eq!(
            s.read("owner", &json!({"conversation":cid}), &[])
                .unwrap_err()
                .code,
            "not_found"
        );
        assert_eq!(s.dms("owner", false).unwrap()["items"], json!([]));
        assert_eq!(
            s.read(
                &b.id,
                &json!({"dms":true,"unread_only":true}),
                &[a, b.clone()]
            )
            .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let attest = s
            .events
            .iter()
            .find(|e| e.event["data"]["record_kind"] == "attestation")
            .unwrap()
            .event
            .clone();
        assert!(!attest.to_string().contains(cid));
        assert!(!attest.to_string().contains("Private"));
        s.names.clear();
        s.reduce(&attest).unwrap();
        assert_eq!(s.names.values().next().unwrap().name, "Scout");
    }
    #[test]
    fn snapshot_time_pages_and_explicit_reads_do_not_consume_unrelated_posts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let a = person(&s, "a");
        send(&mut s, &a, "One", "1", "Scout");
        send(&mut s, &a, "Two", "2", "Scout");
        let q = json!({"channel":"general","time":{"since":"10m"},"limit":1});
        let page = s.read("owner", &q, &[a.clone()]).unwrap();
        send(&mut s, &a, "Three", "3", "Scout");
        let mut q2 = q.clone();
        q2["cursor"] = page["next_cursor"].clone();
        let page2 = s.read("owner", &q2, &[a]).unwrap();
        assert_eq!(page2["items"][0]["body"], "Two");
        assert!(page2["next_cursor"].is_null());
        assert_eq!(page["time"], page2["time"]);
        assert_eq!(
            s.read(
                "owner",
                &json!({"channel":"general","unread_only":true}),
                &[]
            )
            .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        s.acknowledge("owner", &page["receipt"]).unwrap();
        assert_eq!(
            s.read(
                "owner",
                &json!({"channel":"general","unread_only":true}),
                &[]
            )
            .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn corruption_retains_readable_prefix_and_disables_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let a = person(&s, "a");
        let e = send(&mut s, &a, "One", "1", "Scout");
        let path = s.event_path(&e["event"]).unwrap();
        drop(s);
        fs::write(path, b"corrupt").unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        assert!(s.degraded.is_some());
        assert_eq!(
            s.send(
                "owner",
                "",
                "owner",
                &json!({"channel":"general","body":"No","request_id":"no"}),
                &[]
            )
            .unwrap_err()
            .code,
            "store_read_only"
        );
    }
    #[test]
    fn independent_writer_and_foreign_retry_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        assert!(matches!(Store::open(tmp.path()),Err(e) if e.code=="writer_busy"));
        assert_eq!(s.send("owner","","owner",&json!({"channel":"general","body":"No","request_id":"no","origin_daemon_id":uuid()}),&[]).unwrap_err().code,"retry_origin_unavailable");
    }
    #[test]
    fn journal_failure_has_an_honest_uncertain_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let next = s.journal_dir().join(format!("{:020}.json", s.position + 1));
        fs::create_dir(next).unwrap();
        let e = s
            .send(
                "owner",
                "",
                "owner",
                &json!({"channel":"general","body":"No","request_id":"no"}),
                &[],
            )
            .unwrap_err();
        assert_eq!(e.code, "outcome_unknown");
        assert!(s.degraded.is_some());
    }
    #[test]
    fn nested_channels_and_time_validation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        s.create_channel(
            "owner",
            &json!({"action":"create","path":"news/parser","request_id":"create"}),
        )
        .unwrap();
        assert!(s.channels.contains_key("news") && s.channels.contains_key("news/parser"));
        for v in [
            json!({"since":"é"}),
            json!({"since":"0m"}),
            json!({"start":"2026-01-01T00:00:00"}),
            json!({"since":"10m","end":"2026-01-01T00:00:00Z"}),
        ] {
            assert!(time_bounds(&v).is_err());
        }
        assert!(channel_path("../escape").is_err());
    }
    #[test]
    fn unread_paging_after_ack_never_skips_remaining_messages() {
        let t = tempfile::tempdir().unwrap();
        let mut s = Store::open(t.path()).unwrap();
        let a = person(&s, "a");
        for n in 0..4 {
            send(
                &mut s,
                &a,
                &format!("Message {n}"),
                &format!("{n}"),
                "Scout",
            );
        }
        let mut q =
            json!({"channel":"general","unread_only":true,"limit":1,"time":{"since":"10m"}});
        let mut seen = vec![];
        loop {
            let page = s.read("owner", &q, &[]).unwrap();
            for v in page["items"].as_array().unwrap() {
                seen.push(v["body"].clone());
            }
            if page["next_cursor"].is_null() {
                break;
            }
            q["cursor"] = page["next_cursor"].clone();
            q["ack_receipt"] = page["receipt"].clone();
        }
        assert_eq!(
            seen,
            json!(["Message 0", "Message 1", "Message 2", "Message 3"])
                .as_array()
                .unwrap()
                .clone()
        );
    }
    #[test]
    fn historical_aliases_suffix_collisions_owner_rename_and_missing_bootstrap_recover() {
        let t = tempfile::tempdir().unwrap();
        let mut s = Store::open(t.path()).unwrap();
        let a = person(&s, "a");
        let b = person(&s, "b");
        let c = person(&s, "c");
        send(&mut s, &a, "Hi", "a", "Scout");
        let suffix = hash(b.id.as_bytes());
        send(&mut s, &c, "Hi", "c", &format!("Scout-{}", &suffix[..3]));
        let named = send(&mut s, &b, "Hi", "b", "Scout");
        assert!(named["name"].as_str().unwrap().ends_with(&suffix[..4]));
        let renamed=s.rename(&a.id,"a",&json!({"_authenticated_actor":"owner","uid":"a","name":"Gardener","expected_name_revision":1,"request_id":"rename"})).unwrap();
        assert_eq!(renamed["event"]["actor"]["id"], "owner");
        let d = person(&s, "d");
        assert_ne!(send(&mut s, &d, "Hi", "d", "Scout")["name"], "Scout");
        let revision = s.names[&a.id].revision;
        drop(s);
        let s = Store::open(t.path()).unwrap();
        assert_eq!(s.names[&a.id].revision, revision);
        assert_eq!(s.names[&a.id].name, "Gardener");
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path()).unwrap();
        let norm = s
            .events
            .iter()
            .find(|e| e.event["type"] == "norms.update")
            .unwrap();
        fs::remove_file(s.journal_dir().join(format!("{:020}.json", norm.position))).unwrap();
        drop(s);
        let s = Store::open(t.path()).unwrap();
        assert!(s.norms["text"].as_str().unwrap().contains("Quick replies"));
        assert!(s.degraded.is_none());
    }
    #[test]
    fn staged_events_are_invisible_and_unknown_conversation_activity_remains_readable() {
        let t = tempfile::tempdir().unwrap();
        let mut s = Store::open(t.path()).unwrap();
        let channel = s.channels["general"].clone();
        let staged = s
            .event(
                "message.create",
                Some(&channel),
                "Not published",
                json!({}),
                "owner",
                "Owner",
                "owner",
                "staged",
                "",
            )
            .unwrap();
        write_file(
            &s.event_path(&staged).unwrap(),
            &serde_json::to_vec_pretty(&staged).unwrap(),
            false,
        )
        .unwrap();
        s.publish(
            "future.activity",
            Some(&channel),
            "Future readable activity",
            json!({}),
            "owner",
            "Owner",
            "owner",
            "future",
            "",
        )
        .unwrap();
        drop(s);
        let mut s = Store::open(t.path()).unwrap();
        let items = s.read("owner", &json!({"channel":"general"}), &[]).unwrap()["items"].clone();
        assert_eq!(items.as_array().unwrap().len(), 1);
        assert_eq!(items[0]["type"], "future.activity");
    }
    #[test]
    fn request_keys_cannot_cross_mutation_types_with_identical_parameters() {
        let t = tempfile::tempdir().unwrap();
        let mut s = Store::open(t.path()).unwrap();
        let params = json!({"channel":"general","body":"Hello","request_id":"same","path":"new","action":"create"});
        s.create_channel("owner", &params).unwrap();
        assert_eq!(
            s.send("owner", "", "owner", &params, &[]).unwrap_err().code,
            "idempotency_conflict"
        );
    }
}
