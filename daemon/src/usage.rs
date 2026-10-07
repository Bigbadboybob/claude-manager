//! Usage recording, phase 1 (doc/usage-recording.md).
//!
//! Every 60 s each daemon appends one sample line to
//! `~/.cm/metrics/usage/YYYY-MM-DD.jsonl` (UTC day): Owner's availability
//! level, live agent counts split by continuous vs Owner work, by engine
//! state, engine and model, and a subagent census. Every change of Owner's
//! availability level is written as its own `availability` line as soon as it
//! is seen (checked every 5 s). Files older than two days are gzipped in place
//! and files older than 180 days are deleted. `usage.read` and
//! `scripts/cm-usage` read them back.

use crate::state::DaemonState;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SAMPLE_EVERY: Duration = Duration::from_secs(60);
const AVAILABILITY_EVERY: Duration = Duration::from_secs(5);
const COMPACT_EVERY: Duration = Duration::from_secs(3600);
const GZIP_AFTER_DAYS: i64 = 2;
const KEEP_DAYS: i64 = 180;
/// A subagent transcript appended this recently is working.
const ACTIVE_WITHIN_S: f64 = 90.0;
/// ... or one waiting on its own tool call, up to this long.
const TOOL_WAIT_WITHIN_S: f64 = 600.0;
const TAIL_BYTES: u64 = 64 * 1024;
const READ_LIMIT: usize = 5000;

fn now_unix() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn dir(cm_root: &Path) -> PathBuf {
    cm_root.join("metrics/usage")
}

fn day_file(dir: &Path, ts: f64) -> PathBuf {
    let day = chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".into());
    dir.join(format!("{day}.jsonl"))
}

fn append(dir: &Path, record: &Value) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let ts = record["ts"].as_f64().unwrap_or_else(now_unix);
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(day_file(dir, ts))?;
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    // One write per record: concurrent appenders never interleave a line.
    file.write_all(&line)
}

fn mtime(path: &Path) -> Option<f64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
}

fn tail(path: &Path, bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(bytes))).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The model of the newest turn in a transcript tail: Claude's assistant
/// `message.model`, Codex's `turn_context.model`. Synthetic error turns skip.
pub(crate) fn model_from_tail(text: &str) -> Option<String> {
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let model = v["message"]["model"]
            .as_str()
            .filter(|_| v["type"] == "assistant")
            .or_else(|| v["payload"]["model"].as_str().filter(|_| v["type"] == "turn_context"));
        if let Some(m) = model.filter(|m| !m.is_empty() && !m.starts_with('<')) {
            return Some(m.to_owned());
        }
    }
    None
}

/// True when the newest record of a transcript is an assistant tool call that
/// has no result yet (the agent is waiting on its own tool).
pub(crate) fn awaiting_tool(text: &str) -> bool {
    let Some(last) = text.lines().rev().find(|l| !l.trim().is_empty()) else { return false };
    let Ok(v) = serde_json::from_str::<Value>(last) else { return false };
    v["type"] == "assistant"
        && v["message"]["content"]
            .as_array()
            .is_some_and(|c| c.iter().any(|part| part["type"] == "tool_use"))
}

/// Claude subagents of one session: `<project>/<session-id>/subagents/agent-*.jsonl`.
pub(crate) fn claude_subagents(transcript: &Path, now: f64) -> (usize, usize) {
    let dir = transcript.with_extension("").join("subagents");
    let Ok(entries) = std::fs::read_dir(&dir) else { return (0, 0) };
    let (mut total, mut active) = (0, 0);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        total += 1;
        let Some(m) = mtime(&path) else { continue };
        let age = now - m;
        if age <= ACTIVE_WITHIN_S
            || (age <= TOOL_WAIT_WITHIN_S && tail(&path, 16 * 1024).is_some_and(|t| awaiting_tool(&t)))
        {
            active += 1;
        }
    }
    (total, active)
}

/// Codex rollout ids are the trailing UUID of `rollout-<time>-<uuid>.jsonl`.
fn rollout_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    (stem.len() >= 36).then(|| stem[stem.len() - 36..].to_owned())
}

/// Maps Codex child rollouts to their parent thread. The first line of a
/// rollout (`session_meta`) never changes, so each file is read once.
#[derive(Default)]
pub(crate) struct CodexChildren {
    parents: HashMap<PathBuf, Option<String>>,
}

impl CodexChildren {
    /// Index the rollouts of the last two days under `sessions_root`.
    pub(crate) fn refresh(&mut self, sessions_root: &Path, now: f64) {
        for days_back in 0..2 {
            let Some(day) = chrono::DateTime::from_timestamp(now as i64 - days_back * 86_400, 0) else { continue };
            let dir = sessions_root.join(day.format("%Y/%m/%d").to_string());
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if self.parents.contains_key(&path) || path.extension().is_none_or(|e| e != "jsonl") {
                    continue;
                }
                let parent = std::fs::File::open(&path).ok().and_then(|f| {
                    let mut first = String::new();
                    std::io::BufReader::new(f).read_line(&mut first).ok()?;
                    let v: Value = serde_json::from_str(&first).ok()?;
                    let p = &v["payload"];
                    (p["thread_source"] == "subagent")
                        .then(|| p["parent_thread_id"].as_str().map(str::to_owned))
                        .flatten()
                });
                self.parents.insert(path, parent);
            }
        }
        // Forget files that are gone or older than the window.
        self.parents.retain(|p, _| p.exists());
    }

    /// (total, active) children of the thread whose rollout is `transcript`.
    pub(crate) fn count(&self, transcript: &Path, now: f64) -> (usize, usize) {
        let Some(id) = rollout_id(transcript) else { return (0, 0) };
        let (mut total, mut active) = (0, 0);
        for (path, parent) in &self.parents {
            if parent.as_deref() == Some(id.as_str()) {
                total += 1;
                if mtime(path).is_some_and(|m| now - m <= ACTIVE_WITHIN_S) {
                    active += 1;
                }
            }
        }
        (total, active)
    }
}

/// One live agent session, captured under the state lock.
#[derive(Clone, Debug)]
pub(crate) struct Agent {
    pub uid: String,
    pub engine: String,
    pub continuous: bool,
    pub state: String,
    pub transcript: Option<PathBuf>,
}

/// What one 5 s tick reads under the lock.
pub(crate) struct Captured {
    pub agents: Vec<Agent>,
    /// (running, idle) bash panes.
    pub shells: (u64, u64),
    /// Each live session's last Owner keystroke (W0b viewer-input tracking).
    pub inputs: Vec<(String, std::time::Instant)>,
    pub cm_root: PathBuf,
    pub daemon_id: Option<String>,
}

fn capture(state: &Arc<Mutex<DaemonState>>) -> Captured {
    let s = state.lock().unwrap_or_else(|p| p.into_inner());
    let mut shells = (0u64, 0u64);
    let mut inputs = Vec::new();
    for v in s.sessions.values() {
        if let Some(at) = *v.last_operator_input_at.lock().unwrap_or_else(|p| p.into_inner()) {
            inputs.push((v.uid.clone(), at));
        }
        if v.session_type == "bash" {
            match crate::agent_state::current(v).state.as_str() {
                "exited" => {}
                "working" | "working-background" => shells.0 += 1,
                _ => shells.1 += 1,
            }
        }
    }
    let continuous_uids: std::collections::HashSet<&str> = s
        .sessions
        .values()
        .filter(|v| v.continuous_task_id.is_some())
        .map(|v| v.uid.as_str())
        .collect();
    let agents = s
        .sessions
        .values()
        .filter(|v| matches!(v.session_type.as_str(), "claude-code" | "codex"))
        .filter_map(|v| {
            let st = crate::agent_state::current(v);
            let state = st.state.as_str();
            (state != "exited").then(|| Agent {
                uid: v.uid.clone(),
                engine: v.session_type.clone(),
                // A continuous orchestrator, or a worker it manages.
                continuous: v.continuous_task_id.is_some()
                    || v.managed_by_uid.as_deref().is_some_and(|m| continuous_uids.contains(m)),
                state: state.to_owned(),
                transcript: v.transcript_path.as_ref().map(PathBuf::from),
            })
        })
        .collect();
    let daemon_id = s
        .messaging
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(|store| store.daemon_id.clone()));
    Captured { agents, shells, inputs, cm_root: s.messaging_root.clone(), daemon_id }
}

/// Owner keystrokes between samples, at the 5 s tick resolution: a tick in
/// which any session got a new keystroke counts as 5 input seconds.
#[derive(Default)]
pub(crate) struct OwnerInput {
    last_seen: HashMap<String, std::time::Instant>,
    active_ticks: u64,
    sessions: std::collections::BTreeSet<String>,
}

impl OwnerInput {
    pub(crate) fn observe(&mut self, inputs: &[(String, std::time::Instant)]) {
        let mut typed = false;
        for (uid, at) in inputs {
            if self.last_seen.get(uid).is_none_or(|prev| at > prev) {
                // The first sighting after start is history, not new input.
                if self.last_seen.contains_key(uid) || at.elapsed() <= AVAILABILITY_EVERY {
                    typed = true;
                    self.sessions.insert(uid.clone());
                }
                self.last_seen.insert(uid.clone(), *at);
            }
        }
        if typed {
            self.active_ticks += 1;
        }
    }

    /// `{input_s, sessions, resolution_s}` since the last sample; resets.
    pub(crate) fn take(&mut self) -> Value {
        let out = json!({
            "input_s": self.active_ticks * AVAILABILITY_EVERY.as_secs(),
            "sessions": std::mem::take(&mut self.sessions),
            "resolution_s": AVAILABILITY_EVERY.as_secs(),
        });
        self.active_ticks = 0;
        out
    }
}

/// Model cache keyed by transcript path and size: only grown files are re-read.
#[derive(Default)]
pub(crate) struct Models {
    seen: HashMap<PathBuf, (u64, Option<String>)>,
}

impl Models {
    fn model(&mut self, path: &Path) -> Option<String> {
        let len = std::fs::metadata(path).ok()?.len();
        if let Some((l, m)) = self.seen.get(path) {
            if *l == len {
                return m.clone();
            }
        }
        let model = tail(path, TAIL_BYTES).and_then(|t| model_from_tail(&t));
        // Keep the last known model while a new tail has no turn yet.
        let model = model.or_else(|| self.seen.get(path).and_then(|(_, m)| m.clone()));
        self.seen.insert(path.to_owned(), (len, model.clone()));
        model
    }
}

fn bump(map: &mut BTreeMap<String, u64>, key: &str) {
    *map.entry(key.to_owned()).or_default() += 1;
}

/// Build one sample from captured agents plus per-agent model and subagent
/// counts (pure, for tests).
pub(crate) fn build_sample(
    ts: f64,
    host: Option<&str>,
    daemon_id: Option<&str>,
    owner: &Value,
    agents: &[(Agent, Option<String>, (usize, usize))],
    shells: (u64, u64),
    owner_input: Value,
) -> Value {
    let mut by_state = BTreeMap::new();
    let mut by_kind_state: BTreeMap<&str, BTreeMap<String, u64>> = BTreeMap::new();
    let mut by_engine = BTreeMap::new();
    let mut by_model = BTreeMap::new();
    let (mut continuous, mut owner_work) = (0u64, 0u64);
    let (mut sub_total, mut sub_active) = (0usize, 0usize);
    let mut sessions = Vec::new();
    for (a, model, (total, active)) in agents {
        let kind = if a.continuous { "continuous" } else { "owner" };
        if a.continuous { continuous += 1 } else { owner_work += 1 }
        bump(&mut by_state, &a.state);
        bump(by_kind_state.entry(kind).or_default(), &a.state);
        bump(&mut by_engine, &a.engine);
        bump(&mut by_model, model.as_deref().unwrap_or("unknown"));
        sub_total += total;
        sub_active += active;
        if *total > 0 {
            sessions.push(json!({"uid": a.uid, "engine": a.engine, "total": total, "active": active}));
        }
    }
    json!({
        "v": 1,
        "type": "sample",
        "ts": ts,
        "host": host,
        "daemon_id": daemon_id,
        "owner": owner,
        "agents": {
            "total": agents.len(),
            "continuous": continuous,
            "owner": owner_work,
            "by_state": by_state,
            "by_kind_state": by_kind_state,
            "by_engine": by_engine,
            "by_model": by_model,
        },
        "subagents": {"total": sub_total, "active": sub_active, "sessions": sessions},
        // Bash panes: Owner counts long shell jobs as active work.
        "shells": {"total": shells.0 + shells.1, "running": shells.0, "idle": shells.1},
        "owner_input": owner_input,
    })
}

fn availability_record(cm_root: &Path) -> Value {
    std::fs::read(crate::owner_availability::projection_path(cm_root))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(Value::Null)
}

fn owner_summary(record: &Value) -> Value {
    json!({"level": record["level"], "changed_at": record["changed_at"]})
}

fn host_label() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: valid buffer; gethostname NUL-terminates within its length.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    (rc == 0)
        .then(|| {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).split('.').next().unwrap_or_default().to_owned()
        })
        .filter(|s| !s.is_empty())
}

/// gzip day files older than `GZIP_AFTER_DAYS` and delete ones older than
/// `KEEP_DAYS` (by the date in the name, not mtime).
pub(crate) fn compact(dir: &Path, now: f64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let today = (now / 86_400.0).floor() as i64;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(day) = name.get(..10).and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()) else {
            continue;
        };
        let age = today - day.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp() / 86_400).unwrap_or(today);
        if age > KEEP_DAYS {
            let _ = std::fs::remove_file(&path);
        } else if age > GZIP_AFTER_DAYS && name.ends_with(".jsonl") {
            let status = std::process::Command::new("gzip")
                .arg("-9")
                .arg("-f")
                .arg(&path)
                .stdin(std::process::Stdio::null())
                .status();
            if !status.is_ok_and(|s| s.success()) {
                eprintln!("cm usage: gzip {} failed", path.display());
            }
        }
    }
}

struct Sampler {
    models: Models,
    input: OwnerInput,
    codex: CodexChildren,
    last_level_event: Option<String>,
    last_sample: Option<std::time::Instant>,
    last_compact: Option<std::time::Instant>,
    host: Option<String>,
}

impl Sampler {
    fn tick(&mut self, state: &Arc<Mutex<DaemonState>>) {
        let Captured { agents, shells, inputs, cm_root, daemon_id } = capture(state);
        self.input.observe(&inputs);
        let dir = dir(&cm_root);
        let now = now_unix();
        // Availability transitions, as soon as they are seen.
        let record = availability_record(&cm_root);
        let event = record["event_id"].as_str().map(str::to_owned);
        if event.is_some() && event != self.last_level_event {
            // After a restart, the level already on disk is not a new change.
            let already = self.last_level_event.is_none() && last_recorded_event(&dir) == event;
            if !already {
                let line = json!({
                    "v": 1, "type": "availability", "ts": now,
                    "host": self.host, "daemon_id": daemon_id,
                    "from": record["previous"], "to": record["level"],
                    "changed_at": record["changed_at"], "event_id": event,
                });
                if let Err(e) = append(&dir, &line) {
                    eprintln!("cm usage: append availability: {e}");
                }
            }
            self.last_level_event = event;
        }
        if self.last_sample.is_some_and(|t| t.elapsed() < SAMPLE_EVERY) {
            return;
        }
        self.last_sample = Some(std::time::Instant::now());
        if let Some(home) = std::env::var_os("HOME") {
            self.codex.refresh(&PathBuf::from(home).join(".codex/sessions"), now);
        }
        let rows: Vec<(Agent, Option<String>, (usize, usize))> = agents
            .into_iter()
            .map(|a| {
                let (model, subs) = match a.transcript.as_deref() {
                    Some(t) => (
                        self.models.model(t),
                        if a.engine == "codex" { self.codex.count(t, now) } else { claude_subagents(t, now) },
                    ),
                    None => (None, (0, 0)),
                };
                (a, model, subs)
            })
            .collect();
        let sample = build_sample(now, self.host.as_deref(), daemon_id.as_deref(), &owner_summary(&record), &rows,
                                  shells, self.input.take());
        if let Err(e) = append(&dir, &sample) {
            eprintln!("cm usage: append sample: {e}");
        }
        if self.last_compact.is_none_or(|t| t.elapsed() >= COMPACT_EVERY) {
            self.last_compact = Some(std::time::Instant::now());
            compact(&dir, now);
        }
    }
}

/// The newest `availability` event id already on disk, so a restart does not
/// record the current level again as a transition.
fn last_recorded_event(dir: &Path) -> Option<String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    files.sort();
    for file in files.iter().rev().take(3) {
        let text = std::fs::read_to_string(file).ok()?;
        if let Some(v) = text.lines().rev().filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["type"] == "availability") {
            return v["event_id"].as_str().map(str::to_owned);
        }
    }
    None
}

pub fn start(state: &Arc<Mutex<DaemonState>>) {
    let state = Arc::clone(state);
    let spawned = std::thread::Builder::new().name("usage-recorder".into()).spawn(move || {
        let mut sampler = Sampler {
            models: Models::default(),
            input: OwnerInput::default(),
            codex: CodexChildren::default(),
            last_level_event: None,
            last_sample: None,
            last_compact: None,
            host: host_label(),
        };
        loop {
            sampler.tick(&state);
            std::thread::sleep(AVAILABILITY_EVERY);
        }
    });
    if let Err(e) = spawned {
        eprintln!("cm usage: recorder thread failed to start: {e}");
    }
}

fn parse_time(v: &Value, default: f64) -> Result<f64, String> {
    match v {
        Value::Null => Ok(default),
        Value::Number(n) => n.as_f64().ok_or_else(|| "bad time".into()),
        Value::String(s) => {
            if let Some(rest) = s.strip_suffix('h').and_then(|h| h.parse::<f64>().ok()) {
                return Ok(now_unix() - rest * 3600.0);
            }
            if let Some(rest) = s.strip_suffix('d').and_then(|d| d.parse::<f64>().ok()) {
                return Ok(now_unix() - rest * 86_400.0);
            }
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|d| d.timestamp() as f64)
                .map_err(|_| format!("time must be unix seconds, RFC 3339, or like 6h / 2d: {s}"))
        }
        _ => Err("bad time".into()),
    }
}

fn read_day(path: &Path) -> Option<String> {
    if path.extension().is_some_and(|e| e == "gz") {
        let out = std::process::Command::new("gzip").arg("-dc").arg(path).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        std::fs::read_to_string(path).ok()
    }
}

/// Records in `[since, until]`, oldest first, optionally of one `type`.
pub(crate) fn read(dir: &Path, since: f64, until: f64, kind: Option<&str>, limit: usize) -> Value {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|it| it.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    let first_day = (since / 86_400.0).floor() as i64 - 1;
    let last_day = (until / 86_400.0).floor() as i64 + 1;
    let mut records = Vec::new();
    let mut truncated = false;
    for file in files {
        let Some(name) = file.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(day) = name.get(..10).and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()) else { continue };
        let day_n = day.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp() / 86_400).unwrap_or(0);
        if day_n < first_day || day_n > last_day {
            continue;
        }
        let Some(text) = read_day(&file) else { continue };
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let ts = v["ts"].as_f64().unwrap_or(0.0);
            if ts < since || ts > until || kind.is_some_and(|k| v["type"] != k) {
                continue;
            }
            if records.len() >= limit {
                truncated = true;
                break;
            }
            records.push(v);
        }
    }
    let mut out = Map::new();
    out.insert("records".into(), Value::Array(records));
    out.insert("truncated".into(), json!(truncated));
    Value::Object(out)
}

/// `usage.read {since?, until?, type?, limit?}`: this host's records.
pub fn read_rpc(state: &Arc<Mutex<DaemonState>>, params: &Value) -> Result<Value, String> {
    let now = now_unix();
    let since = parse_time(&params["since"], now - 3600.0)?;
    let until = parse_time(&params["until"], now)?;
    let limit = params["limit"].as_u64().map_or(READ_LIMIT, |l| (l as usize).min(READ_LIMIT));
    let root = state.lock().unwrap_or_else(|p| p.into_inner()).messaging_root.clone();
    Ok(read(&dir(&root), since, until, params["type"].as_str(), limit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(uid: &str, engine: &str, continuous: bool, state: &str) -> Agent {
        Agent { uid: uid.into(), engine: engine.into(), continuous, state: state.into(), transcript: None }
    }

    #[test]
    fn sample_counts_by_kind_state_engine_and_model() {
        let rows = vec![
            (agent("a", "claude-code", false, "working"), Some("claude-opus-5-5".into()), (3, 1)),
            (agent("b", "codex", true, "idle"), Some("gpt-6.1-sol".into()), (0, 0)),
            (agent("c", "codex", false, "waiting-on-human"), None, (2, 2)),
        ];
        let owner = json!({"level": "focused", "changed_at": "2026-10-07T19:13:17Z"});
        let s = build_sample(100.0, Some("h"), Some("d1"), &owner, &rows, (2, 1), json!({"input_s": 10}));
        assert_eq!(s["type"], "sample");
        assert_eq!(s["owner"]["level"], "focused");
        let a = &s["agents"];
        assert_eq!((a["total"].as_u64(), a["continuous"].as_u64(), a["owner"].as_u64()), (Some(3), Some(1), Some(2)));
        assert_eq!(a["by_state"], json!({"idle": 1, "waiting-on-human": 1, "working": 1}));
        assert_eq!(a["by_kind_state"]["continuous"], json!({"idle": 1}));
        assert_eq!(a["by_kind_state"]["owner"], json!({"waiting-on-human": 1, "working": 1}));
        assert_eq!(a["by_engine"], json!({"claude-code": 1, "codex": 2}));
        assert_eq!(a["by_model"], json!({"claude-opus-5-5": 1, "gpt-6.1-sol": 1, "unknown": 1}));
        assert_eq!(s["subagents"]["total"], 5);
        assert_eq!(s["subagents"]["active"], 3);
        assert_eq!(s["subagents"]["sessions"].as_array().unwrap().len(), 2);
        assert_eq!(s["shells"], json!({"total": 3, "running": 2, "idle": 1}));
        assert_eq!(s["owner_input"]["input_s"], 10);
    }

    #[test]
    fn owner_input_counts_ticks_with_new_keystrokes_and_the_sessions_typed_into() {
        use std::time::Instant;
        let mut input = OwnerInput::default();
        let old = Instant::now() - Duration::from_secs(3600);
        input.observe(&[("a".into(), old)]); // history at start: not new input
        assert_eq!(input.take()["input_s"], 0);
        let t1 = Instant::now();
        input.observe(&[("a".into(), t1)]);
        input.observe(&[("a".into(), t1)]); // no new keystroke this tick
        let t2 = t1 + Duration::from_millis(1);
        input.observe(&[("a".into(), t2), ("b".into(), t2)]);
        let out = input.take();
        assert_eq!(out["input_s"], 10);
        assert_eq!(out["sessions"], json!(["a", "b"]));
        assert_eq!(input.take()["input_s"], 0, "reset after each sample");
    }

    #[test]
    fn model_comes_from_the_newest_real_turn() {
        let claude = concat!(
            r#"{"type":"assistant","message":{"model":"claude-sonnet-5-5","content":[]}}"#, "\n",
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5","content":[]}}"#, "\n",
            r#"{"type":"assistant","message":{"model":"<synthetic>","content":[]}}"#, "\n",
            r#"{"type":"user","message":{"content":"hi"}}"#, "\n",
        );
        assert_eq!(model_from_tail(claude).as_deref(), Some("claude-opus-5-5"));
        let codex = concat!(
            r#"{"type":"turn_context","payload":{"model":"gpt-6.1-sol"}}"#, "\n",
            r#"{"type":"response_item","payload":{"type":"message"}}"#, "\n",
        );
        assert_eq!(model_from_tail(codex).as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(model_from_tail("partial line {\n"), None);
    }

    fn touch(path: &Path, age_s: u64) {
        let t = SystemTime::now() - Duration::from_secs(age_s);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn claude_subagents_count_recent_and_tool_waiting_as_active() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("sid.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let subs = tmp.path().join("sid/subagents");
        std::fs::create_dir_all(&subs).unwrap();
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#;
        let text = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#;
        for (name, body, age) in [("agent-a.jsonl", text, 10), ("agent-b.jsonl", tool, 300),
                                  ("agent-c.jsonl", text, 300), ("agent-d.jsonl", tool, 3600)] {
            let p = subs.join(name);
            std::fs::write(&p, body).unwrap();
            touch(&p, age);
        }
        std::fs::write(subs.join("agent-a.meta.json"), "{}").unwrap();
        assert_eq!(claude_subagents(&transcript, now_unix()), (4, 2));
        assert_eq!(claude_subagents(&tmp.path().join("other.jsonl"), now_unix()), (0, 0));
    }

    #[test]
    fn codex_children_are_found_by_parent_thread_id() {
        let tmp = tempfile::tempdir().unwrap();
        let now = now_unix();
        let day = chrono::DateTime::from_timestamp(now as i64, 0).unwrap().format("%Y/%m/%d").to_string();
        let dir = tmp.path().join(day);
        std::fs::create_dir_all(&dir).unwrap();
        let parent_id = "01a107a7-89f2-7eb2-9a79-8b9ae0d3c64e";
        let parent = dir.join(format!("rollout-2026-10-04T16-03-07-{parent_id}.jsonl"));
        std::fs::write(&parent, r#"{"type":"session_meta","payload":{"thread_source":"user"}}"#).unwrap();
        let child = |id: &str, age: u64, parent: &str| {
            let p = dir.join(format!("rollout-2026-10-04T16-05-00-{id}.jsonl"));
            std::fs::write(&p, format!(
                r#"{{"type":"session_meta","payload":{{"thread_source":"subagent","parent_thread_id":"{parent}"}}}}"#)).unwrap();
            touch(&p, age);
        };
        child("01a1087a-2844-7710-b44a-0f0125fc6e48", 5, parent_id);
        child("01a1087b-0690-7261-abca-24a6a3999b09", 900, parent_id);
        child("01a1087c-674c-7422-9e5a-bfdb4594c778", 5, "someone-else-0000-0000-0000-000000000000");
        let mut index = CodexChildren::default();
        index.refresh(tmp.path(), now);
        assert_eq!(index.count(&parent, now), (2, 1));
    }

    #[test]
    fn records_append_by_day_read_back_by_range_and_compact() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let day = 86_400.0;
        let t0 = 20_000.0 * day + 100.0;
        append(dir, &json!({"type": "sample", "ts": t0})).unwrap();
        append(dir, &json!({"type": "availability", "ts": t0 + 5.0})).unwrap();
        append(dir, &json!({"type": "sample", "ts": t0 + day})).unwrap();
        let all = read(dir, t0 - 1.0, t0 + 2.0 * day, None, 100);
        assert_eq!(all["records"].as_array().unwrap().len(), 3);
        let only = read(dir, t0 - 1.0, t0 + 10.0, Some("availability"), 100);
        assert_eq!(only["records"].as_array().unwrap().len(), 1);
        let capped = read(dir, t0 - 1.0, t0 + 2.0 * day, None, 2);
        assert_eq!(capped["truncated"], true);
        // Three days later the first day is gzipped and still readable.
        compact(dir, t0 + 3.5 * day);
        let gz: Vec<_> = std::fs::read_dir(dir).unwrap().flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "gz")).collect();
        assert_eq!(gz.len(), 1);
        assert_eq!(read(dir, t0 - 1.0, t0 + 2.0 * day, None, 100)["records"].as_array().unwrap().len(), 3);
        // Past the retention window everything is removed.
        compact(dir, t0 + 200.0 * day);
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
    }

    #[test]
    fn read_times_accept_unix_rfc3339_and_relative() {
        assert_eq!(parse_time(&json!(5), 0.0).unwrap(), 5.0);
        assert_eq!(parse_time(&json!("2026-10-07T00:00:00Z"), 0.0).unwrap(), 1_791_331_200.0);
        assert!((parse_time(&json!("2h"), 0.0).unwrap() - (now_unix() - 7200.0)).abs() < 5.0);
        assert!(parse_time(&json!("soon"), 0.0).is_err());
    }
}
