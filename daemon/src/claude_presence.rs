//! Claude's undocumented per-process status files (2.1.291).
//! Files and /proc are inspected outside DaemonState. Cached JSON never exempts
//! a process from identity validation; losing a source never invents idle.
use crate::agent_state::{PresenceObs, PresenceStatus};
use crate::continuous::probe::{probe_transcript_tail, TailProbe, TailShape};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 4096;
const MAX_ANCESTRY: usize = 64;

pub(crate) struct Target {
    pub uid: String,
    pub pid: u32,
    pub child_start_time: Option<u64>,
    pub transcript: Option<PathBuf>,
    pub had_presence: bool,
    pub needs_turn_probe: bool,
    pub wait_hooks: crate::agent_state::HookEdges,
    pub wait_floor: f64,
    pub prior_wait: Option<crate::claude_wait::Resolution>,
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
    changed: (i64, i64),
}
impl Stamp {
    fn read(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        meta.is_file().then(|| Self {
            modified: meta.modified().ok(),
            len: meta.len(),
            inode: meta.ino(),
            changed: (meta.ctime(), meta.ctime_nsec()),
        })
    }
}
#[derive(Clone)]
struct PresenceFile {
    pid: u32,
    proc_start: u64,
    observation: PresenceObs,
}
struct CachedFile {
    stamp: Stamp,
    value: FileValue,
    last_good: Option<PresenceFile>,
    parse_failures: u8,
    tick: u64,
}
#[derive(Clone)]
enum FileValue {
    Valid(PresenceFile),
    ParseFailure,
    Invalid,
}
impl CachedFile {
    fn observed(&mut self, tick: u64) -> Result<Option<PresenceFile>, ()> {
        if self.tick != tick {
            self.tick = tick;
            match &self.value {
                FileValue::Valid(file) => {
                    self.parse_failures = 0;
                    self.last_good = Some(file.clone());
                }
                FileValue::ParseFailure => {
                    self.parse_failures = self.parse_failures.saturating_add(1);
                }
                FileValue::Invalid => {
                    self.parse_failures = 0;
                    self.last_good = None;
                }
            }
        }
        match &self.value {
            FileValue::Valid(file) => Ok(Some(file.clone())),
            FileValue::ParseFailure if self.parse_failures < 2 => Ok(self.last_good.clone()),
            _ => Err(()),
        }
    }
}
struct CachedTail {
    identity: (u32, Option<u64>),
    path: PathBuf,
    stamp: Stamp,
    value: Option<TailProbe>,
}
pub(crate) struct Reader {
    root: PathBuf,
    proc_root: PathBuf,
    files: HashMap<PathBuf, CachedFile>,
    tails: HashMap<String, CachedTail>,
    waits: crate::claude_wait::Reader,
    directory: Option<Result<Vec<u32>, ()>>,
    ancestry: HashMap<u32, Option<(u32, u64)>>,
    tick: u64,
    #[cfg(test)]
    parses: usize,
    #[cfg(test)]
    tail_reads: usize,
}
impl Default for Reader {
    fn default() -> Self {
        let config = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
                    .join(".claude")
            });
        Self::new(config.join("sessions"), PathBuf::from("/proc"))
    }
}
impl Reader {
    pub(crate) fn new(root: PathBuf, proc_root: PathBuf) -> Self {
        Self {
            root,
            proc_root,
            files: HashMap::new(),
            tails: HashMap::new(),
            waits: crate::claude_wait::Reader::default(),
            directory: None,
            ancestry: HashMap::new(),
            tick: 0,
            #[cfg(test)]
            parses: 0,
            #[cfg(test)]
            tail_reads: 0,
        }
    }

    pub(crate) fn observe(&mut self, targets: &[Target], now: f64) -> Vec<Option<PresenceObs>> {
        self.tick = self.tick.wrapping_add(1);
        self.directory = None;
        self.ancestry.clear();
        let observations = targets.iter().map(|t| self.sample(t, now)).collect();
        self.files.retain(|_, cached| cached.tick == self.tick);
        let live: HashSet<_> = targets.iter().map(|t| t.uid.as_str()).collect();
        self.tails.retain(|uid, _| live.contains(uid.as_str()));
        self.waits.retain(&live);
        observations
    }

    fn file(&mut self, pid: u32, now: f64) -> Result<Option<PresenceFile>, ()> {
        let path = self.root.join(format!("{pid}.json"));
        let stamp = Stamp::read(&path).ok_or(())?;
        if let Some(cached) = self.files.get_mut(&path).filter(|c| c.stamp == stamp) {
            return cached.observed(self.tick);
        }
        #[cfg(test)]
        {
            self.parses += 1;
        }
        let value = (|| -> Option<FileValue> {
            if stamp.len > MAX_FILE_BYTES {
                return None;
            }
            let mut bytes = Vec::new();
            std::fs::File::open(&path)
                .ok()?
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() as u64 > MAX_FILE_BYTES {
                return None;
            }
            Some(match serde_json::from_slice(&bytes) {
                Ok(value) => parse(value, now)
                    .map(FileValue::Valid)
                    .unwrap_or(FileValue::Invalid),
                Err(_) => FileValue::ParseFailure,
            })
        })()
        .unwrap_or(FileValue::Invalid);
        let previous = self.files.remove(&path);
        let mut cached = CachedFile {
            stamp,
            value,
            last_good: previous.as_ref().and_then(|c| c.last_good.clone()),
            parse_failures: previous.as_ref().map_or(0, |c| c.parse_failures),
            tick: self.tick.wrapping_sub(1),
        };
        let result = cached.observed(self.tick);
        self.files.insert(path, cached);
        result
    }

    fn process(&self, pid: u32) -> Option<(u32, u64)> {
        let stat = std::fs::read_to_string(self.proc_root.join(format!("{pid}/stat"))).ok()?;
        // comm can contain spaces and ')'; fields after its LAST ')' start at 3.
        let fields: Vec<_> = stat
            .get(stat.rfind(')')? + 1..)?
            .split_whitespace()
            .collect();
        if matches!(*fields.first()?, "Z" | "X" | "x") {
            return None;
        }
        Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
    }
    fn descendant(&mut self, mut pid: u32, root: u32) -> bool {
        for _ in 0..MAX_ANCESTRY {
            let process = match self.ancestry.get(&pid) {
                Some(process) => *process,
                None => {
                    let process = self.process(pid);
                    self.ancestry.insert(pid, process);
                    process
                }
            };
            let Some((parent, _)) = process else {
                return false;
            };
            if parent == root {
                return true;
            }
            if parent == 0 || parent == pid {
                return false;
            }
            pid = parent;
        }
        false
    }
    fn directory_pids(&mut self) -> Result<Vec<u32>, ()> {
        if let Some(pids) = &self.directory {
            return pids.clone();
        }
        // One bounded directory/ancestry scan per tick, shared by old engines
        // and wrappers. Unreadable or truncated scans cannot establish uniqueness.
        let result = (|| {
            let entries = match std::fs::read_dir(&self.root) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(_) => return Err(()),
            };
            let mut pids = Vec::new();
            for (index, entry) in entries.enumerate() {
                if index >= MAX_DIRECTORY_ENTRIES {
                    return Err(());
                }
                let entry = entry.map_err(|_| ())?;
                let name = entry.file_name();
                if let Some(pid) = name
                    .to_str()
                    .and_then(|n| n.strip_suffix(".json"))
                    .and_then(|n| n.parse::<u32>().ok())
                {
                    pids.push(pid);
                }
            }
            Ok(pids)
        })();
        self.directory = Some(result.clone());
        result
    }
    fn validated(&mut self, pid: u32, now: f64) -> Result<Option<PresenceObs>, ()> {
        let file = self.file(pid, now)?;
        let (_, start) = self.process(pid).ok_or(())?;
        match file {
            Some(file) if file.pid == pid && file.proc_start == start => Ok(Some(file.observation)),
            Some(_) => Err(()),
            None => Ok(None),
        }
    }
    fn sample(&mut self, target: &Target, now: f64) -> Option<PresenceObs> {
        let invalid = || PresenceObs {
            valid: false,
            status: PresenceStatus::Unknown,
            observed_at: now,
            status_updated_at: None, native_session_id: None,
            engine_version: None,
            waiting_for: None,
            main_turn_open: None,
            transcript_error: None, transcript_end: None, transcript_evidence_path: None, wait_resolution: None,
        };
        let root_valid = self
            .process(target.pid)
            .is_some_and(|(_, start)| Some(start) == target.child_start_time);
        if !root_valid {
            return Some(invalid());
        }
        let direct = self.root.join(format!("{}.json", target.pid));
        let mut observation = match std::fs::symlink_metadata(&direct) {
            Ok(_) => match self.validated(target.pid, now) {
                Ok(Some(observation)) => observation,
                Ok(None) => return None, // First parse failure: retain current source.
                Err(()) => invalid(),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Wrapper launches have no root PID file. Only an unambiguous,
                // live interactive descendant can supply the root's state.
                let mut candidates = Vec::new();
                let mut invalid_descendant = false;
                let mut deferred_descendant = false;
                if let Ok(pids) = self.directory_pids() {
                    for pid in pids {
                        if !self.descendant(pid, target.pid) {
                            continue;
                        }
                        match self.validated(pid, now) {
                            Ok(Some(p)) => candidates.push(p),
                            Ok(None) => deferred_descendant = true,
                            Err(()) => invalid_descendant = true,
                        }
                    }
                } else {
                    return Some(invalid());
                }
                if deferred_descendant && !invalid_descendant {
                    return None;
                }
                if candidates.len() == 1 && !invalid_descendant {
                    candidates.pop().unwrap()
                } else if target.had_presence || !candidates.is_empty() || invalid_descendant {
                    invalid()
                } else {
                    return None; // Older Claude: no presence ever observed.
                }
            }
            Err(_) => invalid(),
        };
        // Revalidate root after the scan to avoid accepting a replaced wrapper.
        if !self
            .process(target.pid)
            .is_some_and(|(_, start)| Some(start) == target.child_start_time)
        {
            return Some(invalid());
        }
        observation.observed_at = now;
        observation.valid &= observation.matches_transcript(target.transcript.as_deref());
        if observation.valid
            && (observation.status == PresenceStatus::Idle
                || (observation.status == PresenceStatus::Busy && target.needs_turn_probe))
        {
            let tail = self.tail(target);
            observation.transcript_evidence_path = target.transcript.clone();
            if tail.as_ref().is_some_and(|p| p.claude_identity_mismatch) {
                observation.valid = false;
                return Some(observation);
            }
            if observation.status == PresenceStatus::Busy {
                // Missing/unreadable transcripts cannot prove a closed turn.
                observation.main_turn_open = Some(
                    tail.as_ref()
                        .is_none_or(|p| p.shape != TailShape::TurnComplete),
                );
            } else {
                observation.transcript_end = tail.as_ref().and_then(|p| p.claude_end.clone())
                    .filter(|end| end.ended_at.is_some_and(|at| at <= now + 5.0));
                observation.transcript_error = tail.and_then(|p| {
                    p.api_error
                        .or_else(|| p.auth_error.map(|_| "authentication_failed".into()))
                        .or_else(|| p.usage_limit.map(|_| "usage_limit".into()))
                });
            }
        }
        if observation.valid {
            if let (Some(path), Some(key)) = (target.transcript.as_deref(),
                crate::claude_wait::Key::new(&target.wait_hooks, &observation, target.wait_floor)) {
                observation.transcript_evidence_path = Some(path.to_owned());
                observation.wait_resolution = self.waits.observe(&target.uid,
                    (target.pid,target.child_start_time), path, key, target.prior_wait.as_ref(), now);
                if !self.process(target.pid).is_some_and(|(_,start)| Some(start) == target.child_start_time) {
                    return Some(invalid());
                }
            }
        }
        Some(observation)
    }
    fn tail(&mut self, target: &Target) -> Option<TailProbe> {
        let path = target.transcript.as_ref()?;
        let stamp = match Stamp::read(path) {
            Some(stamp) => stamp,
            None => {
                self.tails.remove(&target.uid);
                return None;
            }
        };
        let identity = (target.pid, target.child_start_time);
        if let Some(cached) = self
            .tails
            .get(&target.uid)
            .filter(|c| c.identity == identity && c.path == *path && c.stamp == stamp)
        {
            return cached.value.clone();
        }
        #[cfg(test)]
        {
            self.tail_reads += 1;
        }
        let value = probe_transcript_tail(path);
        self.tails.insert(
            target.uid.clone(),
            CachedTail {
                identity,
                path: path.clone(),
                stamp,
                value: value.clone(),
            },
        );
        value
    }
}

fn short(value: Option<&serde_json::Value>, cap: usize) -> Option<String> {
    value?
        .as_str()
        .filter(|s| s.len() <= cap)
        .map(str::to_string)
}
fn parse(v: serde_json::Value, now: f64) -> Option<PresenceFile> {
    if v.get("kind")?.as_str()? != "interactive" {
        return None;
    }
    let pid = u32::try_from(v.get("pid")?.as_u64()?).ok()?;
    let proc_start = v.get("procStart").and_then(|v| {
        v.as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| v.as_u64())
    })?;
    let status = match v.get("status").and_then(|s| s.as_str()) {
        Some("busy") => PresenceStatus::Busy,
        Some("idle") => PresenceStatus::Idle,
        Some("waiting") => PresenceStatus::Waiting,
        Some("shell") => PresenceStatus::Shell,
        _ => PresenceStatus::Unknown,
    };
    let status_updated_at = match v.get("statusUpdatedAt") {
        None => None,
        Some(v) => {
            let seconds = v.as_f64()? / 1000.0;
            if !seconds.is_finite() || seconds < 0.0 || seconds > now + 5.0 {
                return None;
            }
            Some(seconds)
        }
    };
    Some(PresenceFile {
        pid,
        proc_start,
        observation: PresenceObs {
            valid: true,
            status,
            observed_at: now,
            status_updated_at,
            native_session_id: match v.get("sessionId") {
                None => None,
                Some(id) => Some(short(Some(id), 256).filter(|id| !id.is_empty())?),
            },
            engine_version: short(v.get("version"), 256),
            waiting_for: short(v.get("waitingFor"), 4096),
            main_turn_open: None,
            transcript_error: None, transcript_end: None, transcript_evidence_path: None, wait_resolution: None,
        },
    })
}

/// Test fixture using the production file/process/bound-transcript reader.
#[cfg(test)]
pub(crate) fn fixture_observation(transcript: &Path, native_id: &str, process_valid: bool, now: f64) -> PresenceObs {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let proc_root = dir.path().join("proc");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(proc_root.join("1001")).unwrap();
    let mut fields = vec!["0"; 20];
    fields[0] = "S";
    fields[1] = "1";
    fields[19] = if process_valid { "42" } else { "43" };
    std::fs::write(proc_root.join("1001/stat"), format!("1001 (claude) {}", fields.join(" "))).unwrap();
    std::fs::write(root.join("1001.json"), serde_json::json!({"kind":"interactive", "pid":1001,
        "procStart":"42", "status":"idle", "sessionId":native_id}).to_string()).unwrap();
    Reader::new(root, proc_root).observe(&[Target {
        uid: "fixture".into(), pid: 1001, child_start_time: Some(42), transcript: Some(transcript.to_path_buf()),
        wait_hooks: Default::default(), wait_floor: 0.0, prior_wait: None,
        had_presence: true, needs_turn_probe: false, // A real hook already supplies turn-open evidence.
    }], now).pop().unwrap().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_state::{derive, State, StateCell};

    struct Fixture {
        _dir: tempfile::TempDir,
        reader: Reader,
        target: Target,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("sessions");
            let proc_root = dir.path().join("proc");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(&proc_root).unwrap();
            let f = Self {
                reader: Reader::new(root, proc_root),
                target: Target {
                    uid: "fixture".into(),
                    pid: 1001,
                    child_start_time: Some(42),
                    transcript: None,
                    had_presence: false,
                    needs_turn_probe: true,
                    wait_hooks: Default::default(), wait_floor: 0.0, prior_wait: None,
                },
                _dir: dir,
            };
            f.process(1001, 1, 42, "S");
            f
        }
        fn process(&self, pid: u32, parent: u32, start: u64, state: &str) {
            let dir = self.reader.proc_root.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let mut fields = vec!["0".to_string(); 20];
            fields[0] = state.into();
            fields[1] = parent.to_string();
            fields[19] = start.to_string();
            std::fs::write(
                dir.join("stat"),
                format!("{pid} (wrapper ) with spaces) {}", fields.join(" ")),
            )
            .unwrap();
        }
        fn install(&self, name: &str) {
            let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/claude-presence-2.1.291")
                .join(format!("{name}.json"));
            std::fs::copy(fixture, self.reader.root.join("1001.json")).unwrap();
        }
        fn sample(&mut self) -> Option<PresenceObs> {
            self.reader
                .observe(std::slice::from_ref(&self.target), 1000.0)
                .pop()
                .unwrap()
        }
        fn transcript(&mut self, text: &str) {
            let path = self._dir.path().join("transcript.jsonl");
            std::fs::write(&path, text).unwrap();
            self.target.transcript = Some(path);
        }
        fn state(&mut self) -> State {
            let mut cell = StateCell::new(0.0, Some(42));
            cell.inputs.presence = self.sample();
            derive(&cell.inputs, 1000.0).state
        }
    }

    #[test]
    fn needs_human81_matching_foreground_result_clears_stale_presence_wait() {
        use serde_json::json;
        let mut f = Fixture::new();
        f.install("waiting-permission"); // Real wait edge at 990, not poll time.
        let record = |kind: &str, at: i64, content: serde_json::Value| json!({
            "type":kind, "sessionId":"transcript", "isSidechain":false,
            "timestamp":chrono::DateTime::from_timestamp(at,0).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),
            "message":{"role":kind,"content":content}}).to_string();
        let mut lines = vec![
            record("user",950,json!("do the task")),
            record("assistant",985,json!([{"type":"tool_use","id":"agent-a","name":"Agent","input":{}}])),
        ];
        f.transcript(&(lines.join("\n")+"\n"));
        let mut c = StateCell::new(0.0, Some(42));
        c.inputs.presence = f.sample();
        let blocked = derive(&c.inputs,1000.0);
        assert_eq!(blocked.state,State::WaitingOnHuman);
        assert_eq!(blocked.since,990.0);
        c.inputs.previous = Some(blocked);
        lines.push(record("user",991,json!([{"type":"tool_result","tool_use_id":"agent-a","content":"completed"}])));
        lines.push(record("assistant",992,json!([{"type":"tool_use","id":"bash-next","name":"Bash","input":{}}])));
        f.transcript(&(lines.join("\n")+"\n"));
        c.inputs.presence = f.sample();
        let resumed = derive(&c.inputs,1002.0);
        assert_eq!(resumed.state,State::Working,"matching foreground result must resolve stale waiting presence");
        assert_eq!(resumed.since,991.0,"the resolving event dates the transition, not the poll or original prompt");
    }

    #[test]
    fn needs_human81_hook_and_presence_resolve_together_since_survives_heartbeat() {
        use crate::agent_state::{HookEvent, HookPayload, Report};
        use crate::claude_wait::tests::{rows, log, row, call, result};
        use serde_json::json;
        let mut f=Fixture::new(); f.install("waiting-permission");
        let mut c=StateCell::new(900.0,Some(42));
        let hook=|event,at,id:Option<&str>| Report::Hook {event,payload:HookPayload {
            observed_at:Some(at), tool_use_id:id.map(str::to_owned),
            tool_name:id.map(|_| "Agent".into()), ..Default::default() }};
        c.apply(hook(HookEvent::UserPromptSubmit,950.0,None),1000.0).unwrap();
        c.apply(hook(HookEvent::PermissionRequest,990.0,Some("a")),1000.0).unwrap();
        f.target.wait_hooks=c.inputs.hooks.clone();
        f.target.wait_floor=crate::agent_state::wait_floor(&c.inputs);
        f.transcript(&log(&rows()[..2]));
        c.inputs.presence=f.sample();
        assert_eq!(c.recompute(1000.0).state,State::WaitingOnHuman);
        assert_eq!(c.inputs.previous.as_ref().unwrap().since,990.0);
        // Heartbeats, PTY/chat/subagent-directory progress do not resolve it.
        c.inputs.last_progress_at=Some(1001.0);
        c.inputs.presence=f.sample();
        assert_eq!(c.recompute(1002.0).state,State::WaitingOnHuman);
        assert_eq!(c.inputs.previous.as_ref().unwrap().since,990.0);
        let mut transcript=rows();
        transcript.push(row("assistant",992,json!([call("b","Bash",false)])));
        f.transcript(&log(&transcript));
        c.inputs.presence=f.sample();
        let resumed=c.recompute(1003.0);
        assert_eq!((resumed.state,resumed.since),(State::Working,991.0));
        assert_eq!(resumed.turn_seq,1,"resolution neither injects a prompt nor invents a new turn");
        assert!(resumed.detail.waiting_for.is_none());
        f.target.prior_wait=c.inputs.presence.as_ref().unwrap().wait_resolution.clone();
        c.inputs.presence=f.sample();
        assert_eq!(c.recompute(1004.0).since,991.0);
        // A later real approval remains blocked even with the old cached proof.
        c.apply(hook(HookEvent::PermissionRequest,994.0,Some("b")),1004.0).unwrap();
        f.target.wait_hooks=c.inputs.hooks.clone();
        c.inputs.presence=f.sample();
        let blocked=c.recompute(1005.0);
        assert_eq!((blocked.state,blocked.since),(State::WaitingOnHuman,994.0));
        transcript.push(row("user",993,json!([result("b")])));
        f.transcript(&log(&transcript));
        c.inputs.presence=f.sample();
        assert_eq!(c.recompute(1006.0).state,State::WaitingOnHuman,"an older result cannot clear the new wait");
        // A real Stop remains authoritative, as do the ordinary Busy/Idle paths.
        c.apply(hook(HookEvent::Stop,996.0,None),1006.0).unwrap();
        f.target.wait_floor=crate::agent_state::wait_floor(&c.inputs);
        c.inputs.presence=f.sample();
        assert_eq!(c.recompute(1007.0).state,State::Idle);
    }

    #[test]
    fn needs_human81_busy_idle_and_unrelated_result_cannot_clear_hook_permission() {
        use crate::agent_state::HookEdges;
        use crate::claude_wait::tests::{rows,log,row,result};
        use serde_json::json;
        for status in ["busy","idle","waiting"] {
            let mut f=Fixture::new(); f.install("waiting-permission");
            let presence_path=f.reader.root.join("1001.json");
            let mut v:serde_json::Value=serde_json::from_slice(&std::fs::read(&presence_path).unwrap()).unwrap();
            v["status"]=status.into();
            std::fs::write(&presence_path,v.to_string()).unwrap();
            let mut c=StateCell::new(900.0,Some(42));
            c.inputs.latest_start=Some(950.0);
            c.inputs.hooks=HookEdges {prompt_at:Some(950.0),waiting_at:Some(990.0),
                waiting_for:Some("permission".into()),waiting_tool_id:Some("a".into()),
                open_tool:Some("Agent".into()),..Default::default()};
            f.target.wait_hooks=c.inputs.hooks.clone(); f.target.wait_floor=950.0;
            let mut unrelated=rows(); unrelated[2]=row("user",991,json!([result("other")]));
            f.transcript(&log(&unrelated));
            c.inputs.presence=f.sample();
            assert_eq!(derive(&c.inputs,1000.0).state,State::WaitingOnHuman,"{status}");
            f.transcript(&log(&rows()));
            c.inputs.presence=f.sample();
            assert_eq!(derive(&c.inputs,1000.0).state,State::Working,"{status} must honor matched result");
            f.process(1001,1,43,"S");
            c.inputs.presence=f.sample();
            assert_eq!(derive(&c.inputs,1001.0).state,State::Unknown,"PID reuse cannot carry proof");
        }
    }

    #[test]
    fn native_conversation_mismatch_rechecks_cached_presence_after_rebinding() {
        let mut f = Fixture::new();
        f.install("idle");
        let path = f.reader.root.join("1001.json");
        let mut file: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let original = "11111111-1111-4111-8111-111111111111";
        let replacement = "22222222-2222-4222-8222-222222222222";
        file["sessionId"] = original.into();
        std::fs::write(&path, file.to_string()).unwrap();
        f.target.transcript = Some(f._dir.path().join(format!("{original}.jsonl")));
        assert_eq!(f.state(), State::Idle);
        let parses = f.reader.parses;
        f.target.transcript = Some(f._dir.path().join(format!("{replacement}.jsonl")));
        assert_eq!(f.state(), State::Unknown, "PID identity alone cannot validate another conversation");
        assert_eq!(f.reader.parses, parses, "cached status must revalidate the current binding");
        file["sessionId"] = replacement.into();
        std::fs::write(&path, file.to_string()).unwrap();
        assert_eq!(f.state(), State::Idle, "matching new conversation recovers");
        file.as_object_mut().unwrap().remove("sessionId");
        std::fs::write(&path, file.to_string()).unwrap();
        assert_eq!(f.state(), State::Idle, "older engines may omit conversation identity");
        f.process(1001, 1, 43, "S");
        assert_eq!(f.state(), State::Unknown, "conversation match never bypasses process identity");
    }

    #[test]
    fn missing_status_timestamp_rewrites_never_close_confirmed_hook_turn() {
        let mut f = Fixture::new();
        f.install("idle");
        let path = f.reader.root.join("1001.json");
        let mut file: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        file.as_object_mut().unwrap().remove("statusUpdatedAt");
        let mut c = StateCell::new(0.0, Some(42));
        c.apply(crate::agent_state::Report::Hook {
            event: crate::agent_state::HookEvent::UserPromptSubmit,
            payload: crate::agent_state::HookPayload { observed_at: Some(950.0), ..Default::default() },
        }, 950.0).unwrap();
        for status in ["idle", "waiting", "idle"] {
            file["status"] = status.into();
            std::fs::write(&path, file.to_string()).unwrap();
            c.inputs.presence = f.sample();
            assert_eq!(derive(&c.inputs, 1000.0).state, State::Working,
                "a parse time or file rewrite is not an engine edge");
        }
    }

    #[test]
    fn timestamped_current_transcript_boundaries_resolve_stale_presence_without_stop() {
        use crate::agent_state::{HookEvent, HookPayload, Report, TurnStatus};
        let mut f = Fixture::new();
        f.install("idle");
        let presence_path = f.reader.root.join("1001.json");
        let mut file: serde_json::Value = serde_json::from_slice(&std::fs::read(&presence_path).unwrap()).unwrap();
        file.as_object_mut().unwrap().remove("statusUpdatedAt");
        std::fs::write(&presence_path, file.to_string()).unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        let path = f._dir.path().join(format!("{id}.jsonl"));
        f.target.transcript = Some(path.clone());
        let mut c = StateCell::new(0.0, Some(42));
        c.apply(Report::Hook { event: HookEvent::UserPromptSubmit,
            payload: HookPayload { observed_at: Some(950.0), ..Default::default() } }, 950.0).unwrap();
        let error = serde_json::json!({"type":"assistant", "sessionId":id,
            "timestamp":"1970-01-01T00:16:00.000Z", "isApiErrorMessage":true,
            "error":"authentication_failed"});
        for (record, expected) in [
            (error.clone(), State::Errored),
            ({ let mut v = error.clone(); v["timestamp"] = "1970-01-01T00:15:00.000Z".into(); v }, State::Working),
            ({ let mut v = error.clone(); v.as_object_mut().unwrap().remove("timestamp"); v }, State::Unknown),
            ({ let mut v = error.clone(); v["sessionId"] = "22222222-2222-4222-8222-222222222222".into(); v }, State::Unknown),
            ({ let mut v = error.clone(); v["timestamp"] = "1970-01-01T00:33:20.000Z".into(); v }, State::Unknown),
            ({ let mut v = error.clone(); v["isSidechain"] = true.into(); v }, State::Working),
            (serde_json::json!({"type":"assistant", "sessionId":id,
                "timestamp":"1970-01-01T00:16:00.000Z", "message":{"stop_reason":"end_turn"}}), State::Working),
            (serde_json::json!({"type":"system", "subtype":"turn_duration", "sessionId":id,
                "timestamp":"1970-01-01T00:16:00.000Z"}), State::Idle),
        ] {
            std::fs::write(&path, format!("{record}\n")).unwrap();
            c.inputs.presence = f.sample();
            let state = derive(&c.inputs, 1000.0);
            assert_eq!(state.state, expected, "{record}");
            if expected == State::Errored {
                assert_eq!(state.last_turn.ended_at, Some(960.0));
                assert_eq!(state.last_turn.status, Some(TurnStatus::Failed));
                assert_eq!(state.detail.error_kind.as_deref(), Some("authentication_failed"));
            } else if expected == State::Idle {
                assert_eq!(state.last_turn.ended_at, Some(960.0));
                assert_eq!(state.last_turn.status, Some(TurnStatus::Completed));
            }
        }
        let mut prior_error = error.clone();
        prior_error["timestamp"] = "1970-01-01T00:15:00.000Z".into();
        let duration = serde_json::json!({"type":"system", "subtype":"turn_duration", "sessionId":id,
            "timestamp":"1970-01-01T00:16:01.000Z"});
        for (assistant, expected) in [(&prior_error, State::Working), (&error, State::Errored)] {
            std::fs::write(&path, format!("{assistant}\n{duration}\n")).unwrap();
            c.inputs.presence = f.sample();
            assert_eq!(derive(&c.inputs, 1000.0).state, expected,
                "an error must use its own event time, never borrow a later completion");
        }
        // A current user row blocks old completion/error proof even if appending
        // it merely changes the mtime of the same bound file.
        let user = serde_json::json!({"type":"user", "sessionId":id,
            "timestamp":"1970-01-01T00:16:01.000Z"});
        std::fs::write(&path, format!("{error}\n{user}\n")).unwrap();
        c.inputs.presence = f.sample();
        assert_eq!(derive(&c.inputs, 1000.0).state, State::Working);
        let assistant = serde_json::json!({"type":"assistant", "sessionId":id,
            "timestamp":"1970-01-01T00:16:00.000Z", "message":{"stop_reason":"end_turn"}});
        std::fs::write(&path, format!("{assistant}\n")).unwrap();
        c.inputs.presence = f.sample();
        assert_eq!(derive(&c.inputs, 1000.0).state, State::Working,
            "end_turn while the Stop hook is pending is not a safe boundary");
        c.apply(Report::Hook { event: HookEvent::Stop,
            payload: HookPayload { observed_at: Some(970.0), continuing: true, ..Default::default() } }, 970.0).unwrap();
        assert_eq!(derive(&c.inputs, 1000.0).state, State::Working,
            "a blocked Stop continuing on inbox work retains foreground activity");
        c.apply(Report::Hook { event: HookEvent::Stop,
            payload: HookPayload { observed_at: Some(980.0), ..Default::default() } }, 980.0).unwrap();
        assert_eq!(derive(&c.inputs, 1000.0).state, State::Idle,
            "a later settled Stop still closes the turn");
    }

    #[test]
    fn wrapper_presence_must_match_bound_native_conversation() {
        let mut f = Fixture::new();
        f.process(2001, 1001, 84, "S");
        let id = "11111111-1111-4111-8111-111111111111";
        f.target.transcript = Some(f._dir.path().join(format!("{id}.jsonl")));
        let path = f.reader.root.join("2001.json");
        let mut file = serde_json::json!({
            "pid": 2001, "procStart": "84", "kind": "interactive",
            "status": "idle", "statusUpdatedAt": 990000, "sessionId": id,
        });
        std::fs::write(&path, file.to_string()).unwrap();
        assert_eq!(f.state(), State::Idle);
        file["sessionId"] = "22222222-2222-4222-8222-222222222222".into();
        std::fs::write(&path, file.to_string()).unwrap();
        assert_eq!(f.state(), State::Unknown);
    }

    #[test]
    fn fixtures_validate_identity_and_map_all_statuses() {
        for (name, expected) in [
            ("busy", State::Working),
            ("idle", State::Idle),
            ("shell", State::WorkingBackground),
            ("tempo", State::Idle),
            ("unknown", State::Unknown),
            ("missing-status", State::Unknown),
            ("pid-mismatch", State::Unknown),
            ("proc-mismatch", State::Unknown),
            ("truncated", State::Unknown),
            ("noninteractive", State::Unknown),
            ("waiting-permission", State::WaitingOnHuman),
            ("waiting-input", State::WaitingOnHuman),
            ("waiting-worker", State::WaitingOnHuman),
            ("waiting-dialog", State::WaitingOnHuman),
        ] {
            let mut f = Fixture::new();
            f.install(name);
            if name == "truncated" {
                assert!(f.sample().is_none(), "first partial read deferred");
            }
            assert_eq!(f.state(), expected, "{name}");
            if name == "idle" {
                let p = f.sample().unwrap();
                assert_eq!(p.status_updated_at, Some(990.0), "milliseconds -> seconds");
                assert_eq!(p.engine_version.as_deref(), Some("2.1.291"));
            }
        }
        let mut f = Fixture::new();
        f.install("dead-pid");
        std::fs::remove_file(f.reader.proc_root.join("1001/stat")).unwrap();
        assert_eq!(f.state(), State::Unknown);
    }

    #[test]
    fn cached_json_still_checks_live_process_and_source_loss() {
        let mut f = Fixture::new();
        assert!(f.sample().is_none(), "old engines retain fallback");
        f.install("idle");
        assert_eq!(f.state(), State::Idle);
        assert_eq!(f.state(), State::Idle);
        assert_eq!(f.reader.parses, 1, "unchanged file parses once");
        f.target.had_presence = true;
        f.process(1001, 1, 43, "S");
        assert_eq!(
            f.state(),
            State::Unknown,
            "PID reuse cannot use cached idle"
        );
        f.process(1001, 1, 42, "Z");
        assert_eq!(f.state(), State::Unknown, "zombie is not live");
        f.process(1001, 1, 42, "S");
        f.install("idle");
        assert_eq!(f.state(), State::Idle);
        f.install("truncated");
        assert_eq!(
            f.state(),
            State::Idle,
            "one partial write keeps the last valid observation"
        );
        assert_eq!(f.state(), State::Unknown);
        f.install("idle");
        assert_eq!(f.state(), State::Idle, "recovery is observed");
        std::fs::remove_file(f.reader.root.join("1001.json")).unwrap();
        assert_eq!(
            f.state(),
            State::Unknown,
            "source loss never falls back to idle"
        );
    }

    #[test]
    fn partial_writes_require_two_ticks_and_recovery_resets_the_streak() {
        let mut f = Fixture::new();
        f.install("busy");
        assert_eq!(f.state(), State::Working);
        f.install("truncated");
        assert_eq!(
            f.state(),
            State::Working,
            "first parse failure retains busy"
        );
        assert!(
            f.reader.validated(1001, 1000.0).unwrap().is_some(),
            "same tick is not a second failure"
        );
        assert_eq!(
            f.state(),
            State::Unknown,
            "unchanged corrupt JSON counts again next tick"
        );
        f.install("busy");
        assert_eq!(f.state(), State::Working);
        f.install("truncated");
        assert_eq!(
            f.state(),
            State::Working,
            "successful parse resets failure streak"
        );
        f.process(1001, 1, 43, "S");
        assert_eq!(
            f.state(),
            State::Unknown,
            "identity failures are never debounced"
        );
    }

    #[test]
    fn wrappers_require_unique_live_interactive_descendant_and_ignore_key_files() {
        let mut f = Fixture::new();
        f.process(1002, 1001, 52, "S");
        let child = serde_json::json!({"pid":1002,"procStart":"52","kind":"interactive","status":"waiting","waitingFor":"permission prompt"});
        std::fs::write(f.reader.root.join("1002.json"), child.to_string()).unwrap();
        std::fs::write(f.reader.root.join("1001.key"), "not json").unwrap();
        assert_eq!(f.state(), State::WaitingOnHuman);
        f.process(1002, 9000, 52, "S");
        assert!(
            f.sample().is_none(),
            "foreign process does not supply state"
        );
        f.process(1002, 1001, 52, "S");
        f.process(1003, 1001, 53, "S");
        let mut sibling = child.clone();
        sibling["pid"] = 1003.into();
        sibling["procStart"] = "53".into();
        std::fs::write(f.reader.root.join("1003.json"), sibling.to_string()).unwrap();
        assert_eq!(
            f.state(),
            State::Unknown,
            "ambiguous descendants do not guess"
        );
        f.install("idle");
        assert_eq!(
            f.state(),
            State::Idle,
            "direct process wins over descendants"
        );
    }

    #[test]
    fn transcript_cache_distinguishes_main_turn_and_clears_old_errors() {
        let mut f = Fixture::new();
        f.install("busy");
        f.transcript("{\"type\":\"assistant\",\"message\":{\"content\":[]}}\n");
        assert_eq!(f.state(), State::Working);
        assert_eq!(f.state(), State::Working);
        assert_eq!(f.reader.tail_reads, 1);
        f.transcript("{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n");
        assert_eq!(f.state(), State::WorkingBackground);
        let error = "{\"type\":\"assistant\",\"isApiErrorMessage\":true,\"error\":\"server_error\"}\n{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n";
        f.transcript(error);
        assert_eq!(
            f.state(),
            State::WorkingBackground,
            "busy never uses an error verdict"
        );
        f.install("idle");
        assert_eq!(f.state(), State::Errored);
        assert_eq!(
            f.sample().unwrap().transcript_error.as_deref(),
            Some("server_error")
        );
        assert_eq!(f.reader.tail_reads, 3, "idle reused unchanged tail");
        f.transcript("{\"type\":\"user\",\"message\":{\"content\":\"next\"}}\n");
        assert_eq!(
            f.state(),
            State::Idle,
            "old error cannot cross a new prompt"
        );
        f.install("busy");
        assert_eq!(f.state(), State::Working);
        f.target.needs_turn_probe = false;
        let reads = f.reader.tail_reads;
        f.transcript(error);
        f.sample();
        assert_eq!(
            f.reader.tail_reads, reads,
            "hooks avoid busy transcript probing"
        );
        f.install("waiting-permission");
        f.sample();
        assert_eq!(
            f.reader.tail_reads, reads,
            "waiting never probes transcript"
        );
    }

    #[test]
    fn malformed_optional_fields_and_oversized_files_never_invent_idle() {
        let mut f = Fixture::new();
        for fields in [
            serde_json::json!({"statusUpdatedAt": "yesterday"}),
            serde_json::json!({"statusUpdatedAt": 2000000}),
            serde_json::json!({"procStart": null}),
        ] {
            let mut v = serde_json::json!({"pid":1001,"procStart":"42","kind":"interactive","status":"idle"});
            v.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            std::fs::write(f.reader.root.join("1001.json"), v.to_string()).unwrap();
            assert_eq!(f.state(), State::Unknown);
        }
        std::fs::write(
            f.reader.root.join("1001.json"),
            vec![b' '; MAX_FILE_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(f.state(), State::Unknown);
        f.reader.observe(&[], 1000.0);
        assert!(f.reader.files.is_empty());
        assert!(f.reader.tails.is_empty());
    }
}
