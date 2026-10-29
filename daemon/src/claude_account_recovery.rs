//! Evidence for one-shot recovery after a Claude subscription limit.
//!
//! Only main-conversation synthetic failures can authorize a retry. A bounded
//! tail that cannot be read or classified is unknown, never an empty history.
use serde_json::Value;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

const MAX_TAIL: u64 = 2 * 1024 * 1024;

/// Reload and effect admission share this lock: disabling the feature waits
/// for an already admitted write, and prevents all later admissions.
pub(crate) fn config_gate() -> &'static RwLock<()> {
    static GATE: OnceLock<RwLock<()>> = OnceLock::new();
    GATE.get_or_init(|| RwLock::new(()))
}

pub(crate) fn failed_boundary(agent: &crate::agent_state::AgentState) -> bool {
    use crate::agent_state::{Source, State, TurnStatus};
    agent.source == Source::Presence && agent.state == State::Errored
        && agent.last_turn.status == Some(TurnStatus::Failed)
        && agent.detail.error_kind.as_deref() == Some("rate_limit")
        && agent.detail.input_pending != Some(true)
        && agent.background.complete && agent.background.jobs.is_empty()
        && agent.background.crons.is_empty()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LimitRecord {
    pub id: String,
    pub session_id: String,
    pub at: i64,
    pub reset_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Episode {
    uid: String,
    record_id: String,
    native_id: String,
    baseline_account: Option<String>,
    reset_at: Option<i64>,
    /// Claimed before any terminal write. A crash leaves an uncertain outcome;
    /// it is never retried automatically.
    attempted: bool,
    #[serde(default)]
    manager: Option<String>,
    #[serde(default)]
    label: String,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    unmanaged_logged: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Journal {
    version: u32,
    episodes: BTreeMap<String, Episode>,
    #[serde(default)]
    digests: BTreeMap<String, DigestNotice>,
    #[serde(default)]
    last_account: Option<String>,
    #[serde(skip)]
    unstaged: bool,
    #[serde(skip)]
    unsent: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct DigestNotice {
    manager: String,
    text: String,
    marker: String,
    sent: bool,
}

fn flush_digests(journal: &mut Journal, journal_path: &Path, root: &Path) -> io::Result<()> {
    if !journal.unsent { return Ok(()); }
    let pending: Vec<_> = journal.digests.iter().filter(|(_, n)| !n.sent)
        .map(|(id, n)| (id.clone(), n.clone())).collect();
    for (id, notice) in pending {
        if let Err(e) = crate::notifications::publish(root, &notice.manager, &id, "cm-recovery", &notice.text, &notice.marker) {
            eprintln!("cm-daemon: Claude recovery notice to {} deferred: {e}", notice.manager);
            continue;
        }
        journal.digests.get_mut(&id).unwrap().sent = true;
        journal.save(journal_path)?;
    }
    journal.unsent = journal.digests.values().any(|n| !n.sent);
    Ok(())
}

fn stage_digests(journal: &mut Journal, journal_path: &Path,
    unchecked: BTreeMap<String, Vec<String>>, account_changed: bool) -> io::Result<()> {
    if !journal.unstaged && unchecked.is_empty() && !account_changed { return Ok(()); }
    let mut groups: BTreeMap<String, Vec<(Option<String>, String)>> = BTreeMap::new();
    let mut dirty = account_changed;
    for (key, episode) in &journal.episodes {
        if !episode.attempted || episode.digest.is_some() { continue; }
        if let Some(manager) = &episode.manager {
            let outcome = episode.outcome.as_deref().unwrap_or("attempt outcome unknown");
            groups.entry(manager.clone()).or_default().push((Some(key.clone()), format!("{} ({}): {outcome}", episode.label, episode.uid)));
        } else {
            eprintln!("cm-daemon: Claude account recovery {}: {}", episode.uid, episode.outcome.as_deref().unwrap_or("attempt outcome unknown"));
        }
    }
    for (manager, rows) in unchecked {
        groups.entry(manager).or_default().extend(rows.into_iter().map(|row| (None, format!("{row}: could not check"))));
    }
    for (manager, rows) in groups {
        let mut hasher = Sha256::new();
        hasher.update(manager.as_bytes());
        hasher.update(journal.last_account.as_deref().unwrap_or("").as_bytes());
        for (key, line) in &rows { hasher.update(key.as_deref().unwrap_or(line).as_bytes()); }
        let id = format!("claude-account-recovery:{:x}", hasher.finalize());
        let marker = format!("[cm-recovery {id}]");
        let text = format!("{marker} Claude account recovery:\n{}", rows.iter().map(|(_, line)| line.as_str()).collect::<Vec<_>>().join("\n"));
        for (key, _) in &rows { if let Some(key) = key { journal.episodes.get_mut(key).unwrap().digest = Some(id.clone()); } }
        journal.digests.entry(id).or_insert(DigestNotice { manager, text, marker, sent: false });
        journal.unsent = true;
        dirty = true;
    }
    // No-manager outcomes have no notification target and are logged once.
    for episode in journal.episodes.values_mut().filter(|e| e.attempted && e.manager.is_none() && e.digest.is_none()) {
        episode.digest = Some("logged".into());
        dirty = true;
    }
    if dirty { journal.save(journal_path)?; }
    journal.unstaged = false;
    Ok(())
}

impl Journal {
    fn new() -> Self { Self { version: 1, episodes: BTreeMap::new(), digests: BTreeMap::new(), last_account: None, unstaged: false, unsent: false } }
    fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let mut journal: Self = serde_json::from_slice(&bytes)?;
                if journal.version != 1 { return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown account recovery journal")); }
                journal.unstaged = journal.episodes.values().any(|e| e.attempted && e.digest.is_none());
                journal.unsent = journal.digests.values().any(|n| !n.sent);
                Ok(journal)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(e),
        }
    }
    fn save(&self, path: &Path) -> io::Result<()> {
        crate::state::write_json_atomic(path, &serde_json::to_string(self)?, true)
    }
    fn observe(&mut self, uid: &str, record: &LimitRecord, account: Option<&str>) -> &mut Episode {
        // Include both identity components: a record UUID in another session
        // cannot inherit or reset this session's attempt.
        let key = format!("{uid}:{}", record.id);
        self.episodes.entry(key).or_insert_with(|| Episode {
            uid: uid.into(), record_id: record.id.clone(), native_id: record.session_id.clone(),
            baseline_account: account.map(str::to_owned), reset_at: record.reset_at, attempted: false,
            manager: None, label: uid.into(), outcome: None, digest: None,
            unmanaged_logged: false,
        })
    }
}

impl Episode {
    fn armed(&self, account: Option<&str>, now: i64) -> bool {
        !self.attempted && (self.reset_at.is_some_and(|reset| now >= reset)
            || self.baseline_account.as_deref().zip(account).is_some_and(|(old, new)| old != new))
    }
}

/// Return a one-way, stable account identity, never a token or e-mail address.
/// Token refresh and unrelated profile changes do not count as a switch.
fn account_fingerprint(path: &Path) -> io::Result<String> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > 8 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Claude profile too large"));
    }
    let profile: Value = serde_json::from_reader(file)?;
    let id = profile["oauthAccount"]["accountUuid"].as_str()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Claude account identity unavailable"))?;
    Ok(format!("{:x}", Sha256::digest(id.as_bytes())))
}

fn profile_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|s| !s.is_empty()) {
        Some(PathBuf::from(dir).join(".claude.json"))
    } else {
        Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude.json"))
    }
}

/// A session can override HOME or CLAUDE_CONFIG_DIR at launch. Never use the
/// host account change as evidence for an independently configured client.
fn process_uses_profile(pid: libc::pid_t, profile: &Path) -> io::Result<bool> {
    use std::os::unix::ffi::OsStringExt;
    if pid <= 0 { return Ok(false); }
    let file = std::fs::File::open(format!("/proc/{pid}/environ"))?;
    let mut bytes = Vec::new(); file.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 { return Err(io::Error::new(io::ErrorKind::InvalidData, "process environment too large")); }
    let mut home = None; let mut config = None;
    for entry in bytes.split(|b| *b == 0) {
        if let Some(value) = entry.strip_prefix(b"HOME=") { home = Some(PathBuf::from(std::ffi::OsString::from_vec(value.to_vec()))); }
        if let Some(value) = entry.strip_prefix(b"CLAUDE_CONFIG_DIR=") {
            if !value.is_empty() { config = Some(PathBuf::from(std::ffi::OsString::from_vec(value.to_vec()))); }
        }
    }
    let expected = if let Some(dir) = config { dir.join(".claude.json") }
        else if let Some(home) = home { home.join(".claude.json") }
        else { return Ok(false); };
    Ok(std::fs::canonicalize(expected)? == std::fs::canonicalize(profile)?)
}

#[derive(Clone, PartialEq, Eq)]
struct FileStamp { inode: u64, len: u64, mtime: (i64, i64), ctime: (i64, i64) }
impl FileStamp {
    fn read(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path)?;
        if !m.is_file() { return Err(io::Error::new(io::ErrorKind::InvalidData, "not a regular file")); }
        Ok(Self { inode: m.ino(), len: m.len(), mtime: (m.mtime(), m.mtime_nsec()), ctime: (m.ctime(), m.ctime_nsec()) })
    }
}

#[derive(Default)]
struct AccountObservation {
    account: Option<String>,
    seen_at: i64,
    profile: Option<(PathBuf, FileStamp, String)>,
    journal: Option<(PathBuf, FileStamp, Journal)>,
    tails: BTreeMap<PathBuf, (FileStamp, String, Result<Option<LimitRecord>, String>)>,
    screens: BTreeMap<String, (libc::pid_t, u64, u64, u16, u16)>,
    #[cfg(test)] profile_reads: usize,
    #[cfg(test)] journal_reads: usize,
    #[cfg(test)] tail_reads: usize,
}
impl AccountObservation {
    fn fingerprint(&mut self, path: &Path) -> io::Result<String> {
        let stamp = FileStamp::read(path)?;
        if let Some((p, s, value)) = &self.profile {
            if p == path && *s == stamp { return Ok(value.clone()); }
        }
        #[cfg(test)] { self.profile_reads += 1; }
        let value = account_fingerprint(path)?;
        self.profile = Some((path.to_owned(), stamp, value.clone()));
        Ok(value)
    }
    fn read_journal(&mut self, path: &Path) -> io::Result<Journal> {
        if let Ok(stamp) = FileStamp::read(path) {
            if let Some((p, s, journal)) = self.journal.take() {
                if p == path && s == stamp { return Ok(journal); }
            }
        }
        #[cfg(test)] { self.journal_reads += 1; }
        Journal::load(path)
    }
    fn cache_journal(&mut self, path: &Path, journal: Journal) {
        self.journal = FileStamp::read(path).ok().map(|stamp| (path.to_owned(), stamp, journal));
    }
    fn limit(&mut self, path: &Path, native_id: &str) -> io::Result<Option<LimitRecord>> {
        let stamp = FileStamp::read(path)?;
        if let Some((s, id, result)) = self.tails.get(path) {
            if *s == stamp && id == native_id {
                return result.clone().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
            }
        }
        #[cfg(test)] { self.tail_reads += 1; }
        let result = latest_limit(path, native_id).map_err(|e| e.to_string());
        if self.tails.len() >= 512 { self.tails.clear(); }
        self.tails.insert(path.to_owned(), (stamp, native_id.to_owned(), result.clone()));
        result.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

fn baseline_for_record<'a>(prior: &'a AccountObservation, current: Option<&'a str>, failed_at: i64) -> Option<&'a str> {
    match (prior.account.as_deref(), current) {
        (Some(old), Some(new)) if old != new && prior.seen_at > failed_at => Some(old),
        (_, current) => current,
    }
}

struct Target {
    uid: String,
    label: String,
    manager: Option<String>,
    native_id: String,
    path: Option<PathBuf>,
    fallback_cwd: Option<PathBuf>,
    generation: u64,
    pid: libc::pid_t,
    incarnation: Option<u64>,
    agent: crate::agent_state::AgentState,
    input: crate::session::InputHandle,
    fanout: Arc<crate::session::PtyByteFanout>,
    cols: u16,
    rows: u16,
    captured: Instant,
}

fn targets(state: &crate::state::DaemonState) -> Vec<Target> {
    let mut result = Vec::new();
    for (uid, session) in &state.sessions {
        if session.session_type != "claude-code" || session.last_exit.kernel_set()
            || session.workflow_run_id.is_some() || session.continuous_task_id.is_some()
            || crate::rotation::in_progress(uid) || crate::handoff::in_progress(uid)
            || crate::codex_recovery::in_progress(uid) || crate::claude_recovery::admission_blocked(uid) { continue; }
        let agent = crate::agent_state::current(session);
        let presence = session.agent_state.lock().unwrap_or_else(|p| p.into_inner()).inputs.presence.clone();
        let Some(presence) = presence.filter(|p| p.valid && p.status == crate::agent_state::PresenceStatus::Idle) else { continue; };
        // A not-yet-bound idle Claude needs a chance to establish its exact
        // transcript before the agent-state reader can classify its error.
        if !failed_boundary(&agent) && session.transcript_path.is_some() { continue; }
        let native_id = presence.native_session_id.filter(|id| uuid::Uuid::parse_str(id).is_ok()).unwrap_or_default();
        let fallback_cwd = if session.transcript_path.is_none() {
            state.workspaces.get(&session.workspace_id).and_then(|w| w.worktree_path.clone())
        } else { None };
        let path = session.transcript_path.as_ref().map(PathBuf::from).or_else(|| {
            (!native_id.is_empty()).then(|| fallback_cwd.as_ref().and_then(|cwd| crate::transcript_detect::claude_transcript_path(cwd, &native_id))).flatten()
        });
        result.push(Target { uid: uid.clone(), label: session.title.clone(), manager: session.managed_by_uid.clone(),
            native_id, path, fallback_cwd, generation: session.generation, pid: session.pid,
            incarnation: session.holder_incarnation, agent, input: session.input_handle(), fanout: Arc::clone(&session.fanout),
            cols: session.last_cols, rows: session.last_rows, captured: Instant::now() });
    }
    result.sort_by(|a, b| a.uid.cmp(&b.uid));
    result
}

struct WakeHold(String);
impl Drop for WakeHold {
    fn drop(&mut self) { crate::rotation::unhold(&self.0, crate::rotation::HoldOwner::ClaudeAccountRecovery); }
}

/// Check fallback cwd from the exact error record; an encoded directory can
/// collide for different paths, so a matching filename alone is insufficient.
fn fallback_cwd_matches(path: &Path, record: &LimitRecord, cwd: &Path) -> io::Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(MAX_TAIL)))?;
    let mut bytes = Vec::new(); file.take(MAX_TAIL + 1).read_to_end(&mut bytes)?;
    for line in bytes.split(|b| *b == b'\n').rev() {
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue; };
        if v["uuid"] == record.id {
            let Some(found) = v["cwd"].as_str() else { return Ok(false); };
            return Ok(std::fs::canonicalize(found)? == std::fs::canonicalize(cwd)?);
        }
    }
    Ok(false)
}

/// Bind only the previously absent path, using the same lifecycle exclusion as
/// ordinary transcript updates. A concurrent resume/rebind always wins.
fn bind_fallback(state: &Arc<Mutex<crate::state::DaemonState>>, target: &Target,
    record: &LimitRecord, path: &Path, cwd: &Path) -> io::Result<bool> {
    let gate = crate::claude_recovery::lifecycle_gate(&target.uid);
    let Ok(_guard) = gate.try_write() else { return Ok(false); };
    if latest_limit(path, &target.native_id)?.as_ref() != Some(record)
        || !fallback_cwd_matches(path, record, cwd)? { return Ok(false); }
    let mut st = state.lock().unwrap_or_else(|p| p.into_inner());
    if st.restarting || st.draining || !st.config.claude_limit_recovery || target.manager.is_none() { return Ok(false); }
    let Some(session) = st.sessions.get_mut(&target.uid) else { return Ok(false); };
    let presence = session.agent_state.lock().unwrap_or_else(|p| p.into_inner()).inputs.presence.clone();
    if session.pid != target.pid || session.holder_incarnation != target.incarnation
        || session.generation != target.generation || session.transcript_path.is_some()
        || !target.input.belongs_to(session) || session.last_exit.kernel_set()
        || session.managed_by_uid != target.manager || session.workflow_run_id.is_some() || session.continuous_task_id.is_some()
        || !presence.is_some_and(|p| p.valid && p.status == crate::agent_state::PresenceStatus::Idle
            && p.native_session_id.as_deref() == Some(target.native_id.as_str())) { return Ok(false); }
    let Some(path) = path.to_str() else { return Ok(false); };
    session.transcript_path = Some(path.to_owned());
    session.generation = session.generation.saturating_add(1);
    let entry = crate::control::methods::transcript_updated_entry(session);
    st.persist_sessions_best_effort();
    crate::control::methods::broadcast_transcript_updated(&st, &target.uid, entry);
    Ok(true)
}

fn attempt(state: &Arc<Mutex<crate::state::DaemonState>>, target: &Target, record: &LimitRecord,
    profile: &Path, journal: &mut Journal, journal_path: &Path, messaging_root: &Path,
    observation: &mut AccountObservation) -> io::Result<bool> {
    if target.manager.is_none() { return Ok(false); }
    let lifecycle = crate::claude_recovery::lifecycle_gate(&target.uid);
    let Ok(_lifecycle) = lifecycle.try_write() else { return Ok(false); };
    if crate::claude_recovery::admission_blocked(&target.uid)
        || crate::claude_recovery::has_pending_input(&target.uid) { return Ok(false); }
    let cursor = target.fanout.snapshot_since(Some(u64::MAX));
    if cursor.closed { return Ok(false); }
    let screen_key = (target.pid, target.generation, cursor.cursor, target.cols, target.rows);
    if observation.screens.get(&target.uid) == Some(&screen_key) { return Ok(false); }
    let snapshot = target.fanout.snapshot_since(None);
    if snapshot.closed { return Ok(false); }
    let screen_key = (target.pid, target.generation, snapshot.cursor, target.cols, target.rows);
    let mut screen = crate::workflow::pty_tracker::PtyModeTracker::with_size(target.cols.max(1) as usize, target.rows.max(1) as usize);
    screen.feed(&snapshot.bytes, Instant::now());
    let mode = target.fanout.keyboard().0.or_else(|| screen.composer_ready().then(|| screen.term_mode()));
    if mode.is_none() || !screen.claude_recovery_composer_ready() {
        if observation.screens.len() >= 512 { observation.screens.clear(); }
        observation.screens.insert(target.uid.clone(), screen_key);
        return Ok(false);
    }
    observation.screens.remove(&target.uid);
    let _config = config_gate().read().unwrap_or_else(|p| p.into_inner());
    {
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(session) = st.sessions.get(&target.uid) else { return Ok(false); };
        if st.restarting || st.draining || !st.config.claude_limit_recovery
            || session.pid != target.pid || session.holder_incarnation != target.incarnation
            || session.generation != target.generation || !target.input.belongs_to(session)
            || session.last_exit.kernel_set() || session.workflow_run_id.is_some() || session.continuous_task_id.is_some()
            || session.managed_by_uid.is_none() || session.managed_by_uid != target.manager
            || crate::rotation::in_progress(&target.uid) || crate::handoff::in_progress(&target.uid)
            || !failed_boundary(&crate::agent_state::current(session)) { return Ok(false); }
        if !crate::rotation::hold(&st, &target.uid, crate::rotation::HoldOwner::ClaudeAccountRecovery) { return Ok(false); }
    }
    let _hold = WakeHold(target.uid.clone());
    let Some(_native) = crate::notifications::recovery_input_guard(messaging_root, &target.uid, record.at as f64)? else { return Ok(false); };
    let Some(path) = target.path.as_ref() else { return Ok(false); };
    let key = format!("{}:{}", target.uid, record.id);
    let enter = crate::workflow::pty_tracker::enter_bytes_for_mode(mode.unwrap());
    target.input.write_claude_limit_resume(target.captured, target.agent.turn_seq, target.agent.last_turn.ended_at, enter, || {
        let fresh = target.fanout.snapshot_since(Some(snapshot.cursor));
        if fresh.closed || fresh.cursor != snapshot.cursor || latest_limit(path, &target.native_id)?.as_ref() != Some(record) { return Ok(false); }
        if let Some(cwd) = &target.fallback_cwd { if !fallback_cwd_matches(path, record, cwd)? { return Ok(false); } }
        let account = if process_uses_profile(target.pid, profile).unwrap_or(false) {
            account_fingerprint(profile).ok()
        } else { None };
        let Some(episode) = journal.episodes.get_mut(&key) else { return Ok(false); };
        if !episode.armed(account.as_deref(), chrono::Utc::now().timestamp()) { return Ok(false); }
        episode.attempted = true;
        let was_unstaged = journal.unstaged;
        journal.unstaged = true;
        if let Err(e) = journal.save(journal_path) {
            // No terminal byte has been written when persistence fails.
            journal.episodes.get_mut(&key).unwrap().attempted = false;
            journal.unstaged = was_unstaged;
            return Err(e);
        }
        Ok(true)
    })
}

fn pass(state: &Arc<Mutex<crate::state::DaemonState>>, observation: &mut AccountObservation) -> io::Result<()> {
    let (journal_path, root, enabled, candidates) = {
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(base) = st.daemon_sessions_path.as_ref() else { return Ok(()); };
        (base.with_file_name("daemon-claude-account-recovery.json"), st.messaging_root.clone(),
            st.config.claude_limit_recovery && !st.restarting && !st.draining,
            if st.config.claude_limit_recovery && !st.restarting && !st.draining { targets(&st) } else { Vec::new() })
    };
    let mut journal = observation.read_journal(&journal_path)?;
    flush_digests(&mut journal, &journal_path, &root)?;
    if !enabled { observation.cache_journal(&journal_path, journal); return Ok(()); }
    let Some(profile) = profile_path() else { observation.cache_journal(&journal_path, journal); return Ok(()); };
    let account = observation.fingerprint(&profile).ok();
    let observed_at = chrono::Utc::now().timestamp();
    // Only a successfully read, changed account identity can indicate a switch.
    let switched = journal.last_account.as_deref().zip(account.as_deref()).is_some_and(|(a, b)| a != b);
    let account_changed = account.is_some() && journal.last_account != account;
    let mut could_not_check: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for target in &candidates {
        let record = target.path.as_deref().map(|path| observation.limit(path, &target.native_id));
        let record = match record {
            Some(Ok(Some(record))) => record,
            Some(Ok(None)) => continue,
            Some(Err(_)) | None => {
                if switched {
                    if let Some(manager) = &target.manager {
                        could_not_check.entry(manager.clone()).or_default().push(format!("{} ({})", target.label, target.uid));
                    } else { eprintln!("cm-daemon: Claude account recovery {}: could not check transcript", target.uid); }
                }
                continue;
            }
        };
        if let (Some(path), Some(cwd)) = (&target.path, &target.fallback_cwd) {
            if !fallback_cwd_matches(path, &record, cwd).unwrap_or(false) {
                if switched {
                    if let Some(manager) = &target.manager {
                        could_not_check.entry(manager.clone()).or_default().push(format!("{} ({})", target.label, target.uid));
                    }
                }
                continue;
            }
        }
        let key = format!("{}:{}", target.uid, record.id);
        let new = !journal.episodes.contains_key(&key);
        let matching_profile = process_uses_profile(target.pid, &profile).unwrap_or(false);
        let target_account = if matching_profile { account.as_deref() } else { None };
        let baseline = baseline_for_record(observation, target_account, record.at);
        let episode = journal.observe(&target.uid, &record, baseline);
        let metadata_changed = episode.manager != target.manager || episode.label != target.label;
        if new || metadata_changed { episode.manager = target.manager.clone(); episode.label = target.label.clone(); journal.save(&journal_path)?; }
        let armed = journal.episodes[&key].armed(target_account, chrono::Utc::now().timestamp());
        if switched && !matching_profile {
            if let Some(manager) = &target.manager {
                could_not_check.entry(manager.clone()).or_default().push(format!("{} ({})", target.label, target.uid));
            }
        }
        if !armed { continue; }
        if target.manager.is_none() {
            let episode = journal.episodes.get_mut(&key).unwrap();
            if !episode.unmanaged_logged {
                eprintln!("cm-daemon: Claude account recovery {}: unmanaged session requires operator resume", target.uid);
                episode.unmanaged_logged = true;
                journal.save(&journal_path)?;
            }
            continue;
        }
        if let (Some(path), Some(cwd)) = (&target.path, &target.fallback_cwd) {
            // The normal presence reader needs the binding before it can
            // classify the error. Its next tick will supply the safe edge.
            if !bind_fallback(state, target, &record, path, cwd).unwrap_or(false) && switched {
                if let Some(manager) = &target.manager {
                    could_not_check.entry(manager.clone()).or_default().push(format!("{} ({})", target.label, target.uid));
                }
            }
            continue;
        }
        let result = attempt(state, target, &record, &profile, &mut journal, &journal_path, &root, observation);
        let episode = journal.episodes.get_mut(&key).unwrap();
        if episode.attempted {
            episode.outcome = Some(match result {
                Ok(true) => "resume input written; turn not yet confirmed".into(),
                Ok(false) => "attempt outcome unknown".into(),
                Err(_) => "input outcome unknown; no automatic retry".into(),
            });
            journal.save(&journal_path)?;
        } else if let Err(e) = result {
            eprintln!("cm-daemon: Claude account recovery {} deferred: {e}", target.uid);
        }
    }
    // Freeze each system notice before publishing so a crash cannot create a
    // different digest or repeat an accepted notice on the next pass.
    if let Some(account) = &account { journal.last_account = Some(account.clone()); }
    stage_digests(&mut journal, &journal_path, could_not_check, account_changed)?;
    flush_digests(&mut journal, &journal_path, &root)?;
    observation.cache_journal(&journal_path, journal);
    if let Some(account) = account { observation.account = Some(account); observation.seen_at = observed_at; }
    Ok(())
}

/// Runs independently of the agent-state ticker; filesystem and PTY I/O never
/// hold the global state lock. Processing attempts sequentially naturally
/// staggers sends (each paste/submit is serialized through its settle gap).
pub(crate) fn start(state: &Arc<Mutex<crate::state::DaemonState>>) -> io::Result<()> {
    let weak = Arc::downgrade(state);
    std::thread::Builder::new().name("cm-claude-account-recovery".into()).spawn(move || {
      let mut observation = AccountObservation::default();
      loop {
        std::thread::sleep(Duration::from_secs(2));
        let Some(state) = weak.upgrade() else { break; };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pass(&state, &mut observation))) {
            Ok(Ok(())) => {},
            Ok(Err(e)) => {
                eprintln!("cm-daemon: Claude account recovery unavailable: {e}");
                observation = AccountObservation::default();
                std::thread::sleep(Duration::from_secs(30));
            }
            Err(_) => {
                eprintln!("cm-daemon: Claude account recovery panicked; retrying with fresh evidence");
                observation = AccountObservation::default();
                std::thread::sleep(Duration::from_secs(30));
            }
        }
      }
    })?;
    Ok(())
}

/// Read only a complete tail and require the filename-bound native identity.
/// A later user or assistant record invalidates the old error. Sidechain
/// records cannot authorize recovery of the foreground conversation.
pub(crate) fn latest_limit(path: &Path, native_id: &str) -> io::Result<Option<LimitRecord>> {
    if uuid::Uuid::parse_str(native_id).is_err()
        || path.file_stem().and_then(|v| v.to_str()) != Some(native_id) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unbound Claude transcript"));
    }
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(MAX_TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(MAX_TAIL + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TAIL || !bytes.ends_with(b"\n") {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "incomplete transcript tail"));
    }
    let bytes = if start > 0 {
        let cut = bytes.iter().position(|b| *b == b'\n').ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no complete transcript records"))?;
        &bytes[cut + 1..]
    } else { &bytes[..] };
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()).collect();
    while let Some(line) = lines.pop() {
        let v: Value = serde_json::from_slice(line).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid transcript record"))?;
        if v["isSidechain"] == true { continue; }
        match v["type"].as_str() {
            Some("user") => return Ok(None),
            Some("assistant") => {
                if v["sessionId"] != native_id {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "transcript session identity mismatch"));
                }
                if v["error"] != "rate_limit"
                    || v["isApiErrorMessage"] != true || v["message"]["model"] != "<synthetic>" {
                    return Ok(None);
                }
                let id = v["uuid"].as_str().filter(|s| uuid::Uuid::parse_str(s).is_ok())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing limit record identity"))?;
                let at = v["timestamp"].as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.timestamp()).filter(|t| *t > 0)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid limit record time"))?;
                let reset_at = v["quotaLimits"]["resetsAt"].as_i64().filter(|reset| *reset >= at);
                return Ok(Some(LimitRecord { id: id.to_owned(), session_id: native_id.to_owned(), at, reset_at }));
            }
            Some("system" | "progress" | "file-history-snapshot" | "last-prompt" | "summary" | "queue-operation" | "attachment") => continue,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown transcript record")),
        }
    }
    if start > 0 { Err(io::Error::new(io::ErrorKind::InvalidData, "no boundary in bounded tail")) } else { Ok(None) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const SESSION: &str = "93686392-56ae-455c-803d-6ce59cc55e29";
    const RECORD: &str = "e3c16692-eddd-4ec9-9fd5-282de7a3de11";
    fn fixture() -> Value { json!({"type":"assistant", "sessionId":SESSION, "uuid":RECORD,
        "timestamp":"2026-10-29T14:42:20.405Z", "error":"rate_limit", "isApiErrorMessage":true,
        "message":{"model":"<synthetic>","content":[{"type":"text","text":"You've hit your 5-hour limit · resets 4pm (UTC)"}]},
        "quotaLimits":{"status":"rejected","resetsAt":1793289600,"rateLimitType":"five_hour"}}) }
    fn write(path: &Path, rows: &[Value]) { std::fs::write(path, rows.iter().map(|v| format!("{v}\n")).collect::<String>()).unwrap(); }
    #[test]
    fn exact_synthetic_failure_and_subsequent_user_controls() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join(format!("{SESSION}.jsonl"));
        write(&path, &[fixture(), json!({"type":"system","subtype":"turn_duration"})]);
        let record = latest_limit(&path, SESSION).unwrap().unwrap();
        assert_eq!(record.id, RECORD); assert_eq!(record.reset_at, Some(1793289600));
        write(&path, &[fixture(), json!({"type":"user","message":{"role":"user","content":"resume"}})]);
        assert_eq!(latest_limit(&path, SESSION).unwrap(), None);
        let mut ordinary = fixture(); ordinary["isApiErrorMessage"] = json!(false);
        write(&path, &[ordinary]); assert_eq!(latest_limit(&path, SESSION).unwrap(), None);
        write(&path, &[fixture(), json!({"type":"assistant","sessionId":SESSION,"message":{"model":"claude"}})]);
        assert_eq!(latest_limit(&path, SESSION).unwrap(), None);
    }
    #[test]
    fn refuses_bad_identity_and_partial_or_unknown_history() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join(format!("{SESSION}.jsonl"));
        write(&path, &[fixture(), json!({"type":"unknown"})]); assert!(latest_limit(&path, SESSION).is_err());
        std::fs::write(&path, format!("{}", fixture())).unwrap(); assert!(latest_limit(&path, SESSION).is_err());
        write(&path, &[fixture()]); assert!(latest_limit(&path, "not-a-uuid").is_err());
        let mut other = fixture(); other["sessionId"] = json!("different"); write(&path, &[other]);
        assert!(latest_limit(&path, SESSION).is_err());
    }
    #[test]
    fn one_attempt_per_record_survives_restart_and_account_refresh() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("journal.json");
        let profile = dir.path().join("profile.json");
        std::fs::write(&profile, json!({"oauthAccount":{"accountUuid":"123e4567-e89b-12d3-a456-426614174000"},"token":"first"}).to_string()).unwrap();
        let first = account_fingerprint(&profile).unwrap();
        std::fs::write(&profile, json!({"oauthAccount":{"accountUuid":"123e4567-e89b-12d3-a456-426614174000"},"token":"refreshed"}).to_string()).unwrap();
        assert_eq!(account_fingerprint(&profile).unwrap(), first);
        let record = LimitRecord { id: RECORD.into(), session_id: SESSION.into(), at: 1000, reset_at: Some(2000) };
        let mut journal = Journal::new(); let episode = journal.observe("worker", &record, Some(&first));
        assert!(!episode.armed(Some(&first), 1999)); assert!(episode.armed(Some(&first), 2000));
        assert!(episode.armed(Some("different-account"), 1100));
        episode.attempted = true; journal.save(&path).unwrap();
        let mut restored = Journal::load(&path).unwrap();
        assert!(!restored.observe("worker", &record, Some("different-account")).armed(Some("different-account"), 3000));
        std::fs::write(&path, "{").unwrap(); assert!(Journal::load(&path).is_err());
    }
    #[test]
    fn unknown_baseline_does_not_prove_a_switch() {
        let record = LimitRecord { id: RECORD.into(), session_id: SESSION.into(), at: 1000, reset_at: None };
        let mut journal = Journal::new();
        assert!(!journal.observe("worker", &record, None).armed(Some("new-account"), 3000));
        let prior = AccountObservation { account: Some("old".into()), seen_at: 1001, ..Default::default() };
        assert_eq!(baseline_for_record(&prior, Some("new"), 1000), Some("old"));
        assert_eq!(baseline_for_record(&prior, Some("new"), 1001), Some("new"));
        assert_eq!(baseline_for_record(&prior, None, 1000), None);
    }
    #[test]
    fn unchanged_passes_reuse_parses_and_do_not_rewrite_journal() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile.json");
        std::fs::write(&profile, json!({"oauthAccount":{"accountUuid":"123e4567-e89b-12d3-a456-426614174000"}}).to_string()).unwrap();
        let transcript = dir.path().join(format!("{SESSION}.jsonl")); write(&transcript, &[fixture()]);
        let path = dir.path().join("journal.json"); Journal::new().save(&path).unwrap();
        let before = FileStamp::read(&path).unwrap();
        let mut runtime = AccountObservation::default();
        for _ in 0..20 {
            runtime.fingerprint(&profile).unwrap();
            runtime.limit(&transcript, SESSION).unwrap();
            let mut journal = runtime.read_journal(&path).unwrap();
            stage_digests(&mut journal, &path, BTreeMap::new(), false).unwrap();
            runtime.cache_journal(&path, journal);
        }
        assert_eq!((runtime.profile_reads, runtime.tail_reads, runtime.journal_reads), (1, 1, 1));
        assert!(before == FileStamp::read(&path).unwrap(), "idle pass rewrote the journal");
        write(&transcript, &[fixture(), json!({"type":"user"})]);
        assert_eq!(runtime.limit(&transcript, SESSION).unwrap(), None);
        assert_eq!(runtime.tail_reads, 2);
    }
    #[test]
    fn notice_batch_combines_attempts_and_unknowns_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("journal.json");
        let record = LimitRecord { id: RECORD.into(), session_id: SESSION.into(), at: 1000, reset_at: Some(2000) };
        let mut journal = Journal::new(); let episode = journal.observe("worker", &record, Some("a"));
        episode.manager = Some("manager".into()); episode.label = "Worker".into();
        episode.attempted = true; episode.outcome = Some("resume submitted".into());
        let unchecked = [("manager".into(), vec!["Other (other-uid)".into()])].into_iter().collect();
        stage_digests(&mut journal, &path, unchecked, false).unwrap();
        assert_eq!(journal.digests.len(), 1);
        flush_digests(&mut journal, &path, dir.path()).unwrap();
        let (events, _) = crate::notifications::snapshot(dir.path(), "manager").unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0]["text"].as_str().unwrap().contains("resume submitted"));
        assert!(events[0]["text"].as_str().unwrap().contains("could not check"));
        let mut recovered = Journal::load(&path).unwrap();
        stage_digests(&mut recovered, &path, BTreeMap::new(), false).unwrap();
        flush_digests(&mut recovered, &path, dir.path()).unwrap();
        assert_eq!(crate::notifications::snapshot(dir.path(), "manager").unwrap().0.len(), 1);
    }
    #[test]
    fn claude_composer_requires_empty_current_input_row() {
        for (row, col, expected) in [("❯ ", 3, true), ("❯ draft", 3, false), ("❯ ", 7, false)] {
            let mut screen = crate::workflow::pty_tracker::PtyModeTracker::with_size(80, 24);
            screen.feed(format!("\x1b[?25h\x1b[H{row}\x1b[1;{col}H").as_bytes(), Instant::now());
            assert_eq!(screen.claude_recovery_composer_ready(), expected, "{row} {col}");
        }
    }
    #[test]
    fn write_guard_refuses_new_input_and_failed_commit_before_bytes() {
        let (input, writes) = crate::session::InputHandle::test_handle_capturing();
        let now = crate::agent_state::unix_now();
        let cell = input.test_agent_state();
        {
            let mut cell = cell.lock().unwrap();
            cell.inputs.presence = Some(crate::agent_state::PresenceObs {
                valid: true, status: crate::agent_state::PresenceStatus::Idle,
                observed_at: now, status_updated_at: Some(now - 1.0),
                native_session_id: Some(SESSION.into()), engine_version: Some("2.1.312".into()),
                waiting_for: None, main_turn_open: None, transcript_error: Some("rate_limit".into()),
                transcript_end: Some(crate::agent_state::LastTurn { ended_at: Some(now - 1.0), status: Some(crate::agent_state::TurnStatus::Failed) }),
                transcript_evidence_path: None,
            });
            cell.inputs.latest_start = Some(now - 10.0);
            cell.inputs.background.complete = true;
        }
        let agent = cell.lock().unwrap().recompute(now);
        assert!(failed_boundary(&agent), "{agent:?}");
        let captured = Instant::now();
        assert!(!input.write_claude_limit_resume(captured, agent.turn_seq, agent.last_turn.ended_at, b"\r", || Ok(false)).unwrap());
        assert!(writes.lock().unwrap().is_empty());
        input.stamp_operator_input_at(Instant::now());
        assert!(!input.write_claude_limit_resume(captured, agent.turn_seq, agent.last_turn.ended_at, b"\r", || panic!("must not commit after human input")).unwrap());
        assert!(writes.lock().unwrap().is_empty());
    }
    #[test]
    fn actual_attempt_rechecks_manager_disable_and_new_user_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let uid = format!("ts-limit-test-{}", uuid::Uuid::new_v4());
        let transcript = dir.path().join(format!("{SESSION}.jsonl"));
        let profile = dir.path().join("profile.json");
        std::fs::write(&profile, json!({"oauthAccount":{"accountUuid":"123e4567-e89b-12d3-a456-426614174000"}}).to_string()).unwrap();
        let now = chrono::Utc::now().timestamp();
        let mut failure = fixture();
        failure["timestamp"] = json!(chrono::DateTime::from_timestamp(now - 120, 0).unwrap().to_rfc3339());
        failure["quotaLimits"]["resetsAt"] = json!(now - 60);
        write(&transcript, &[failure.clone(), json!({"type":"system","subtype":"turn_duration"})]);
        let record = latest_limit(&transcript, SESSION).unwrap().unwrap();
        let mut params = crate::session::SpawnParams::new(&uid, "worker", "/bin/sleep");
        params.args = vec!["60".into()]; params.session_type = "claude-code".into();
        params.workspace_id = "workspace".into(); params.managed_by_uid = Some("manager".into());
        params.transcript_path = Some(transcript.to_string_lossy().into());
        let mut session = crate::session::PendingSession::spawn(params).unwrap().arm_reaper(None).unwrap();
        let at = crate::agent_state::unix_now();
        {
            let mut cell = session.agent_state.lock().unwrap();
            cell.inputs.presence = Some(crate::agent_state::PresenceObs { valid:true,
                status:crate::agent_state::PresenceStatus::Idle, observed_at:at, status_updated_at:Some(at - 1.0),
                native_session_id:Some(SESSION.into()), engine_version:Some("2.1.312".into()),
                waiting_for:None, main_turn_open:None, transcript_error:Some("rate_limit".into()),
                transcript_end:Some(crate::agent_state::LastTurn { ended_at:Some((now - 120) as f64), status:Some(crate::agent_state::TurnStatus::Failed) }),
                transcript_evidence_path:Some(transcript.clone()) });
            cell.inputs.latest_start = Some((now - 180) as f64); cell.inputs.background.complete = true;
        }
        session.fanout.push("\x1b[>1u\x1b[?25h\x1b[HUsage limit\r\n❯ \x1b[2;3H".as_bytes());
        let mut daemon = crate::state::DaemonState::new(); daemon.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        daemon.messaging_root = dir.path().into(); daemon.sessions.insert(uid.clone(), session);
        let state = Arc::new(Mutex::new(daemon));
        let target = targets(&state.lock().unwrap()).pop().expect("failed managed candidate");
        let journal_path = dir.path().join("recovery.json"); let mut journal = Journal::new();
        journal.observe(&uid, &record, None).manager = Some("manager".into()); journal.save(&journal_path).unwrap();
        let mut runtime = AccountObservation::default();
        state.lock().unwrap().config.claude_limit_recovery = false;
        assert!(!attempt(&state, &target, &record, &profile, &mut journal, &journal_path, dir.path(), &mut runtime).unwrap());
        assert!(!journal.episodes.values().next().unwrap().attempted);
        state.lock().unwrap().config.claude_limit_recovery = true;
        state.lock().unwrap().sessions.get_mut(&uid).unwrap().managed_by_uid = None;
        assert!(!attempt(&state, &target, &record, &profile, &mut journal, &journal_path, dir.path(), &mut runtime).unwrap());
        assert!(!journal.episodes.values().next().unwrap().attempted);
        state.lock().unwrap().sessions.get_mut(&uid).unwrap().managed_by_uid = Some("manager".into());
        write(&transcript, &[failure.clone(), json!({"type":"user","message":{"content":"resume"}})]);
        assert!(!attempt(&state, &target, &record, &profile, &mut journal, &journal_path, dir.path(), &mut runtime).unwrap());
        assert!(!journal.episodes.values().next().unwrap().attempted);
        write(&transcript, &[failure, json!({"type":"system","subtype":"turn_duration"})]);
        assert!(attempt(&state, &target, &record, &profile, &mut journal, &journal_path, dir.path(), &mut runtime).unwrap());
        assert!(Journal::load(&journal_path).unwrap().episodes.values().next().unwrap().attempted);
        assert!(!attempt(&state, &target, &record, &profile, &mut journal, &journal_path, dir.path(), &mut runtime).unwrap());
        let _ = state.lock().unwrap().sessions.get_mut(&uid).unwrap().kill();
    }
}
