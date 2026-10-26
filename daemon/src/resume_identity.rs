//! Durable identity and presentation metadata for explicit transcript resumes.
//! Kept separately from the live registry: closing a row or evicting an exit
//! tombstone must not turn a known conversation into a new messaging person.
use crate::control::protocol::ErrorCode;
use crate::{manifest::ManifestEntry, session::DaemonSession, state::DaemonState};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Preferences {
    pub uid: String,
    pub hidden: bool,
    pub idle_timeout_secs: u16,
    pub burst_threshold: u16,
    pub notify_on_idle: bool,
    pub color: Option<String>,
    pub seeded_from_snapshot: Option<String>,
}
impl Preferences {
    pub fn from_entry(e: &ManifestEntry) -> Self {
        Self {
            uid: e.uid.clone(),
            hidden: e.hidden,
            idle_timeout_secs: e.idle_timeout_secs,
            burst_threshold: e.burst_threshold,
            notify_on_idle: e.notify_on_idle,
            color: e.color.clone(),
            seeded_from_snapshot: e.seeded_from_snapshot.clone(),
        }
    }
    pub fn apply(&self, e: &mut ManifestEntry) {
        e.hidden = self.hidden;
        e.idle_timeout_secs = self.idle_timeout_secs;
        e.burst_threshold = self.burst_threshold;
        e.notify_on_idle = self.notify_on_idle;
        e.color = self.color.clone();
        e.seeded_from_snapshot = self.seeded_from_snapshot.clone();
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub workspace_id: String,
    pub worktree_path: PathBuf,
    pub entry: ManifestEntry,
    #[serde(default)]
    pub transcript_ids: BTreeSet<String>,
}
/// #25: one engine handoff of a CM session (same uid, another engine).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Handoff {
    pub uid: String,
    pub from_engine: String,
    pub from_id: String,
    pub to_engine: String,
    pub to_id: String,
    /// The handoff document the outgoing agent wrote.
    pub file: String,
    pub at: f64,
}
#[derive(Default, Serialize, Deserialize)]
pub struct History {
    records: BTreeMap<String, Record>,
    #[serde(default)]
    preferences: BTreeMap<String, Preferences>,
    /// #24 rotation lineage: superseded transcript id → the id it rotated
    /// to. CM never writes into Claude's own files, so this is the only
    /// record that a rotated transcript moved on (the A-s Resume picker
    /// labels it, `transcript_catalog::rotated_lineage_in`).
    #[serde(default)]
    rotated: BTreeMap<String, String>,
    /// #25 engine handoffs, keyed by the transcript id handed off from.
    #[serde(default)]
    handoffs: BTreeMap<String, Handoff>,
    #[serde(skip)]
    loaded: bool,
    #[serde(skip)]
    last_saved: String,
}
impl History {
    fn load(&mut self, state: &DaemonState) -> std::io::Result<()> {
        if self.loaded {
            return Ok(());
        }
        if let Some(path) = path(state) {
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    *self = serde_json::from_str(&text)?;
                    self.last_saved = text;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        self.loaded = true;
        Ok(())
    }
    fn remember(&mut self, mut record: Record) {
        if let Some(old) = self.records.get(&record.entry.uid) {
            record
                .transcript_ids
                .extend(old.transcript_ids.iter().cloned());
            // A restarting backend may not yet have rebound its file. Keep the
            // last known conversation instead of archiving a blank identity.
            if record.entry.transcript_id.is_none() {
                record.entry.transcript_id = old.entry.transcript_id.clone();
                record.entry.transcript_path = old.entry.transcript_path.clone();
            }
            // The identity's org object (its member id, doc/org-model.md
            // §12) outlives a record that was rebuilt without one.
            if record.entry.org.is_none() {
                record.entry.org = old.entry.org.clone();
            }
        }
        if let Some(id) = &record.entry.transcript_id {
            record.transcript_ids.insert(id.clone());
        }
        if let Some(p) = self.preferences.get(&record.entry.uid) {
            p.apply(&mut record.entry);
        }
        self.records.insert(record.entry.uid.clone(), record);
    }
    fn overlay(&self, e: &mut ManifestEntry) {
        if let Some(old) = self.records.get(&e.uid) {
            Preferences::from_entry(&old.entry).apply(e);
            e.generation = e.generation.max(old.entry.generation);
            if e.org.is_none() {
                e.org = old.entry.org.clone();
            }
        }
        if let Some(p) = self.preferences.get(&e.uid) {
            p.apply(e);
        }
    }
}
fn path(state: &DaemonState) -> Option<PathBuf> {
    state
        .daemon_sessions_path
        .as_ref()
        .map(|p| p.with_file_name("daemon-resume-identities.json"))
}
fn live_entry(state: &DaemonState, sess: &DaemonSession) -> ManifestEntry {
    let mut e = sess.to_manifest_entry();
    if let Some(saved) = state
        .workspaces
        .get(&sess.workspace_id)
        .and_then(|w| w.sessions.iter().find(|e| e.uid == sess.uid))
    {
        Preferences::from_entry(saved).apply(&mut e);
    }
    e
}
pub fn entry(state: &DaemonState, sess: &DaemonSession) -> ManifestEntry {
    let mut e = live_entry(state, sess);
    let mut history = state
        .resume_history
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if history.load(state).is_ok() {
        history.overlay(&mut e);
    }
    e
}
pub fn persist(state: &DaemonState) -> std::io::Result<()> {
    let mut h = state
        .resume_history
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    h.load(state)?;
    for sess in state.sessions.values() {
        let Some(wt) = state
            .workspaces
            .get(&sess.workspace_id)
            .and_then(|w| w.worktree_path.clone())
        else {
            continue;
        };
        let mut e = live_entry(state, sess);
        h.overlay(&mut e);
        h.remember(Record {
            workspace_id: sess.workspace_id.clone(),
            worktree_path: wt,
            entry: e,
            transcript_ids: Default::default(),
        });
    }
    let Some(path) = path(state) else {
        return Ok(());
    };
    let text = serde_json::to_string_pretty(&*h)?;
    if h.last_saved != text {
        crate::state::write_json_atomic(&path, &text, true)?;
        h.last_saved = text;
    }
    Ok(())
}
pub fn save_preferences(state: &DaemonState, preferences: Vec<Preferences>) -> std::io::Result<()> {
    {
        let mut h = state
            .resume_history
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        h.load(state)?;
        for p in preferences {
            // Presentation only. A viewer cannot replace daemon-owned task,
            // transcript, parent or authorization metadata through this path.
            if state.sessions.contains_key(&p.uid) || h.records.contains_key(&p.uid) {
                if let Some(r) = h.records.get_mut(&p.uid) {
                    p.apply(&mut r.entry);
                }
                h.preferences.insert(p.uid.clone(), p);
            }
        }
    }
    persist(state)
}
pub fn remember(state: &DaemonState, r: Record) -> std::io::Result<()> {
    let mut h = state
        .resume_history
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    h.load(state)?;
    h.remember(r);
    Ok(())
}
/// #24: record that transcript `from` rotated to `to`, durably.
pub fn record_rotation(state: &DaemonState, from: &str, to: &str) -> std::io::Result<()> {
    {
        let mut h = state
            .resume_history
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        h.load(state)?;
        h.rotated.insert(from.to_string(), to.to_string());
    }
    persist(state)
}
/// #25: record an engine handoff, durably. A session that never bound a
/// transcript has nothing to label, so it is kept under its uid.
/// #25: whether a handoff from transcript `from_id` (or, with no
/// transcript, of `uid`) completed and was recorded.
pub fn handoff_recorded(state: &DaemonState, uid: &str, from_id: &str) -> bool {
    let mut hist = state
        .resume_history
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if hist.load(state).is_err() {
        return false;
    }
    let key = if from_id.is_empty() { format!("uid:{uid}") } else { from_id.to_string() };
    hist.handoffs.get(&key).is_some_and(|h| h.uid == uid)
}
pub fn record_handoff(state: &DaemonState, h: Handoff) -> std::io::Result<()> {
    {
        let mut hist = state
            .resume_history
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        hist.load(state)?;
        let key = if h.from_id.is_empty() { format!("uid:{}", h.uid) } else { h.from_id.clone() };
        hist.handoffs.insert(key, h);
    }
    persist(state)
}
pub fn lookup_uid(state: &DaemonState, uid: &str) -> std::io::Result<Option<Record>> {
    let mut h = state
        .resume_history
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    h.load(state)?;
    Ok(h.records.get(uid).cloned())
}
fn engine(s: &str) -> &str {
    if s == "claude" {
        "claude-code"
    } else {
        s
    }
}
type Failure = (ErrorCode, String);
fn io_error(e: std::io::Error) -> Failure {
    (
        ErrorCode::Internal,
        format!("Cannot read resume identity archive: {e}"),
    )
}

/// Reservation spans resolution, argv composition and spawn. A second request
/// cannot mint another identity while the first backend is still starting.
pub struct Claim {
    state: Arc<Mutex<DaemonState>>,
    key: (String, String),
}
impl Drop for Claim {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .resume_claims
            .remove(&self.key);
    }
}
pub fn resolve_and_claim(
    state: &Arc<Mutex<DaemonState>>,
    kind: &str,
    id: &str,
) -> Result<(Claim, Option<Record>), Failure> {
    let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
    persist(&s).map_err(io_error)?;
    let key = (engine(kind).to_string(), id.to_string());
    if s.resume_claims.contains(&key) {
        return Err((
            ErrorCode::Conflict,
            "This conversation is already being resumed".into(),
        ));
    }
    for sess in s.sessions.values() {
        if engine(&sess.session_type) == engine(kind)
            && sess
                .transcript_path
                .as_deref()
                .and_then(crate::session::transcript_id_from_path)
                .as_deref()
                == Some(id)
        {
            return Err((
                ErrorCode::Conflict,
                format!(
                    "Conversation is already running as '{}' ({}); attach to it instead",
                    sess.title, sess.uid
                ),
            ));
        }
    }
    let mut candidates: BTreeMap<String, Record> = {
        let h = s.resume_history.lock().unwrap_or_else(|p| p.into_inner());
        h.records
            .iter()
            .filter(|(_, r)| {
                engine(&r.entry.session_type) == engine(kind) && r.transcript_ids.contains(id)
            })
            .map(|(uid, r)| (uid.clone(), r.clone()))
            .collect()
    };
    // Upgrade fallback: full saved workspace entries predate the archive.
    if candidates.is_empty() {
        for (ws_id, ws) in &s.workspaces {
            for e in &ws.sessions {
                if engine(&e.session_type) == engine(kind) && e.transcript_id.as_deref() == Some(id)
                {
                    if let Some(wt) = &ws.worktree_path {
                        candidates.insert(
                            e.uid.clone(),
                            Record {
                                workspace_id: ws_id.clone(),
                                worktree_path: wt.clone(),
                                entry: e.clone(),
                                transcript_ids: [id.to_string()].into(),
                            },
                        );
                    }
                }
            }
        }
    }
    // Older daemons retained only execution tombstones. Recover the identity
    // and all metadata actually recorded there, rather than minting a person.
    if candidates.is_empty() {
        for t in &s.recently_exited {
            if engine(&t.session_type) != engine(kind)
                || t.transcript_path
                    .as_deref()
                    .and_then(crate::session::transcript_id_from_path)
                    .as_deref()
                    != Some(id)
            {
                continue;
            }
            let Some(wt) = t.worktree_path.as_ref() else {
                continue;
            };
            let e: ManifestEntry = serde_json::from_value(serde_json::json!({
                "uid": t.session_uid, "label": t.label, "session_type": t.session_type,
                "transcript_id": id, "transcript_path": t.transcript_path,
                "generation": t.generation, "task_id": t.task_id,
                "managed_by_uid": t.managed_by_uid, "global_perms": t.global_perms,
                "workflow_run_id": t.workflow_run_id, "workflow_role": t.workflow_role,
                "org": t.org,
            }))
            .map_err(|e| (ErrorCode::Internal, e.to_string()))?;
            candidates.insert(
                e.uid.clone(),
                Record {
                    workspace_id: t.workspace_id.clone(),
                    worktree_path: PathBuf::from(wt),
                    entry: e,
                    transcript_ids: [id.to_string()].into(),
                },
            );
        }
        // Viewer-era tombstones may outlive the bounded daemon exit cache.
        for (ws_id, ws) in &s.workspaces {
            for t in &ws.tombstones {
                if engine(&t.session_type) != engine(kind)
                    || t.last_transcript_id.as_deref() != Some(id)
                {
                    continue;
                }
                let Some(wt) = t.worktree_path.as_ref().or(ws.worktree_path.as_ref()) else {
                    continue;
                };
                let e: ManifestEntry = serde_json::from_value(serde_json::json!({
                    "uid": t.uid, "label": t.label, "session_type": kind, "transcript_id": id,
                    "task_id": t.task_id, "managed_by_uid": t.managed_by_uid, "generation": t.generation,
                })).map_err(|e| (ErrorCode::Internal, e.to_string()))?;
                let e = t.entry.clone().unwrap_or(e);
                candidates.entry(e.uid.clone()).or_insert(Record {
                    workspace_id: ws_id.clone(),
                    worktree_path: wt.clone(),
                    entry: e,
                    transcript_ids: [id.to_string()].into(),
                });
            }
        }
    }
    if candidates.len() > 1 {
        return Err((ErrorCode::Conflict, format!("Conversation has multiple recorded CM identities ({}); resolve the original identity before resuming", candidates.keys().cloned().collect::<Vec<_>>().join(", "))));
    }
    let record = candidates.into_values().next();
    if let Some(r) = &record {
        if s.sessions.contains_key(&r.entry.uid) {
            return Err((
                ErrorCode::Conflict,
                format!(
                    "Session '{}' ({}) is already running; attach to it instead",
                    r.entry.label, r.entry.uid
                ),
            ));
        }
        if r.entry.workflow_run_id.is_some() || r.entry.continuous_task_id.is_some() {
            return Err((ErrorCode::Conflict, "This session is owned by a workflow or continuous scheduler; resume it through that controller".into()));
        }
    }
    s.resume_claims.insert(key.clone());
    Ok((
        Claim {
            state: Arc::clone(state),
            key,
        },
        record,
    ))
}

pub fn claim_uid(state: &Arc<Mutex<DaemonState>>, uid: &str) -> Result<Claim, Failure> {
    let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
    let key = ("uid".into(), uid.to_string());
    if s.sessions.contains_key(uid) || !s.resume_claims.insert(key.clone()) {
        return Err((
            ErrorCode::Conflict,
            format!("Session '{uid}' is already running or being resumed"),
        ));
    }
    Ok(Claim {
        state: Arc::clone(state),
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn record(uid: &str, id: &str) -> Record {
        Record {
            workspace_id: "original-workspace".into(),
            worktree_path: PathBuf::from("/original/worktree"),
            entry: serde_json::from_value(json!({
                "uid": uid, "label": "Winners", "session_type": "codex", "transcript_id": id,
                "task_id": "original-task", "managed_by_uid": "ts-a-0", "global_perms": true,
                "hidden": true, "idle_timeout_secs": 17, "burst_threshold": 19,
                "notify_on_idle": true, "color": "green", "generation": 42,
                "memory_cap_soft_bytes": 1024, "memory_cap_hard_bytes": 2048,
                "cgroup_prefix": "/a/cgroup", "seeded_from_snapshot": "original-seed",
            }))
            .unwrap(),
            transcript_ids: Default::default(),
        }
    }
    #[test]
    fn resume_identity_survives_closed_rows_tombstone_eviction_and_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = DaemonState::new();
        s.daemon_sessions_path = Some(tmp.path().join("daemon-sessions.json"));
        let original = record("ts-abc-0", "thread-a");
        remember(&s, original.clone()).unwrap();
        persist(&s).unwrap();
        // No workspace, live session or recent tombstone survives into this process.
        let mut restarted = DaemonState::new();
        restarted.daemon_sessions_path = s.daemon_sessions_path.clone();
        let state = Arc::new(Mutex::new(restarted));
        let (_claim, recovered) = resolve_and_claim(&state, "codex", "thread-a").unwrap();
        let recovered = recovered.unwrap();
        assert_eq!(
            serde_json::to_value(&recovered.entry).unwrap(),
            serde_json::to_value(&original.entry).unwrap()
        );
        assert_eq!(recovered.workspace_id, original.workspace_id);
        assert_eq!(recovered.worktree_path, original.worktree_path);
        // Messaging ids are daemon-id + this UID; no person is minted on Resume.
        assert_eq!(recovered.entry.uid, "ts-abc-0");
    }
    /// #24: rotation lineage is durable and readable by the transcript
    /// catalog from the archive file alone (the TUI reads it locally).
    #[test]
    fn rotation_lineage_persists_for_the_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let cm = tmp.path().join(".cm");
        std::fs::create_dir_all(&cm).unwrap();
        let mut s = DaemonState::new();
        s.daemon_sessions_path = Some(cm.join("daemon-sessions.json"));
        remember(&s, record("ts-abc-0", "old-id")).unwrap();
        record_rotation(&s, "old-id", "new-id").unwrap();
        let lineage = crate::transcript_catalog::rotated_lineage_in(tmp.path());
        assert_eq!(lineage.get("old-id").map(String::as_str), Some("new-id"));
        // A later persist keeps it.
        persist(&s).unwrap();
        assert_eq!(crate::transcript_catalog::rotated_lineage_in(tmp.path()).len(), 1);
    }
    /// S0b (doc/org-model.md §12): a resumed identity keeps its org
    /// object (its member id) across a restart, and a later record rebuilt
    /// without one does not erase it.
    #[test]
    fn resume_identity_keeps_the_org_object() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = DaemonState::new();
        s.daemon_sessions_path = Some(tmp.path().join("daemon-sessions.json"));
        let mut original = record("ts-org-0", "thread-o");
        original.entry.org = Some(crate::manifest::tests::bridge_org());
        remember(&s, original.clone()).unwrap();
        remember(&s, record("ts-org-0", "thread-o")).unwrap();
        persist(&s).unwrap();
        let mut restarted = DaemonState::new();
        restarted.daemon_sessions_path = s.daemon_sessions_path.clone();
        let state = Arc::new(Mutex::new(restarted));
        let (_claim, recovered) = resolve_and_claim(&state, "codex", "thread-o").unwrap();
        let entry = recovered.unwrap().entry;
        assert_eq!(entry.org, original.entry.org);
        assert_eq!(entry.org.as_ref().unwrap()["member_id"], "m-7");
        assert_eq!(entry.generation, 42, "the transcript generation is untouched");
    }
    #[test]
    fn resume_identity_claims_reject_simultaneous_resumes_and_release_on_failure() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let (first, _) = resolve_and_claim(&state, "codex", "thread-a").unwrap();
        assert!(matches!(
            resolve_and_claim(&state, "codex", "thread-a")
                .err()
                .unwrap()
                .0,
            ErrorCode::Conflict
        ));
        let uid_claim = claim_uid(&state, "ts-abc-0").unwrap();
        assert!(claim_uid(&state, "ts-abc-0").is_err());
        drop(first);
        drop(uid_claim);
        assert!(resolve_and_claim(&state, "codex", "thread-a").is_ok());
        assert!(claim_uid(&state, "ts-abc-0").is_ok());
    }
    #[test]
    fn resume_identity_rejects_live_conversation_even_under_a_duplicate_uid() {
        let mut s = DaemonState::new();
        let mut sp = crate::session::SpawnParams::new("ts-def-0", "already-running", "/bin/sleep");
        sp.args = vec!["120".into()];
        sp.session_type = "codex".into();
        let mut live = DaemonSession::spawn(sp).unwrap();
        live.transcript_path = Some(
            "/x/rollout-2026-09-09T00-12-58-01a08382-be3c-7793-80af-2d67da5faf1f.jsonl".into(),
        );
        s.sessions.insert(live.uid.clone(), live);
        let state = Arc::new(Mutex::new(s));
        let failure = resolve_and_claim(&state, "codex", "01a08382-be3c-7793-80af-2d67da5faf1f")
            .err()
            .unwrap();
        assert!(matches!(failure.0, ErrorCode::Conflict));
        assert!(failure.1.contains("already-running"));
        assert_eq!(state.lock().unwrap().sessions.len(), 1);
    }
    #[test]
    fn resume_identity_rejects_active_uid_even_after_conversation_rotation() {
        let mut s = DaemonState::new();
        remember(&s, record("ts-abc-0", "old-thread")).unwrap();
        let mut sp = crate::session::SpawnParams::new("ts-abc-0", "Winners", "/bin/sleep");
        sp.args = vec!["120".into()];
        s.sessions
            .insert("ts-abc-0".into(), DaemonSession::spawn(sp).unwrap());
        let state = Arc::new(Mutex::new(s));
        assert!(matches!(
            resolve_and_claim(&state, "codex", "old-thread")
                .err()
                .unwrap()
                .0,
            ErrorCode::Conflict
        ));
    }
    #[test]
    fn resume_identity_ambiguity_and_scheduler_ownership_fail_closed() {
        let s = DaemonState::new();
        remember(&s, record("ts-abc-0", "same")).unwrap();
        remember(&s, record("ts-def-0", "same")).unwrap();
        let state = Arc::new(Mutex::new(s));
        assert!(resolve_and_claim(&state, "codex", "same")
            .err()
            .unwrap()
            .1
            .contains("multiple"));
        for field in ["continuous", "workflow"] {
            let s = DaemonState::new();
            let mut r = record("ts-123-0", "owned");
            if field == "continuous" {
                r.entry.continuous_task_id = Some("ct".into());
            } else {
                r.entry.workflow_run_id = Some("wf".into());
            }
            remember(&s, r).unwrap();
            assert!(
                resolve_and_claim(&Arc::new(Mutex::new(s)), "codex", "owned")
                    .err()
                    .unwrap()
                    .1
                    .contains("controller")
            );
        }
    }
    #[test]
    fn resume_identity_corruption_is_never_replaced_by_empty_history() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = DaemonState::new();
        s.daemon_sessions_path = Some(tmp.path().join("daemon-sessions.json"));
        let archive = path(&s).unwrap();
        std::fs::write(&archive, "broken").unwrap();
        assert!(persist(&s).is_err());
        assert_eq!(std::fs::read_to_string(archive).unwrap(), "broken");
        assert!(resolve_and_claim(&Arc::new(Mutex::new(s)), "codex", "unknown").is_err());
    }
    #[test]
    fn resume_identity_preferences_update_does_not_change_authority_or_binding() {
        let s = DaemonState::new();
        let original = record("ts-abc-0", "thread");
        remember(&s, original.clone()).unwrap();
        let mut prefs = Preferences::from_entry(&original.entry);
        prefs.color = Some("blue".into());
        prefs.hidden = false;
        save_preferences(&s, vec![prefs]).unwrap();
        let r = lookup_uid(&s, "ts-abc-0").unwrap().unwrap();
        assert_eq!(r.entry.color.as_deref(), Some("blue"));
        assert!(!r.entry.hidden);
        assert!(r.entry.global_perms);
        assert_eq!(r.entry.task_id, original.entry.task_id);
        assert_eq!(r.entry.managed_by_uid, original.entry.managed_by_uid);
        assert_eq!(r.entry.transcript_id, original.entry.transcript_id);
    }
}
