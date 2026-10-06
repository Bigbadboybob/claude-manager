//! Engine activity and its compatibility projection. See doc/SESSION_STATE.md.
//! Lock order: DaemonState -> activity cells -> AgentStateCell. No PTY writes or
//! engine/file probing while holding the state cell. Holder wire types stay fixed.
use crate::manifest::ManifestDiff;
use crate::session::DaemonSession;
use crate::state::DaemonState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Working,
    WorkingBackground,
    WaitingOnHuman,
    Errored,
    Idle,
    Starting,
    Exited,
    Unknown,
}
impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::WorkingBackground => "working-background",
            Self::WaitingOnHuman => "waiting-on-human",
            Self::Errored => "errored",
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Exited => "exited",
            Self::Unknown => "unknown",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Presence,
    Hooks,
    Relay,
    Transcript,
    Pty,
}
impl Source {
    pub fn engine_reported(self) -> bool {
        matches!(self, Self::Presence | Self::Hooks | Self::Relay)
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Detail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumes_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrying: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_tool: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LastTurn {
    pub ended_at: Option<f64>,
    pub status: Option<TurnStatus>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Shell,
    Subagent,
    Monitor,
    Workflow,
    Terminal,
    Thread,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub kind: JobKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<f64>,
    pub first_seen_at: f64,
    pub wakes_agent: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cron {
    pub id: String,
    pub schedule: String,
    pub recurring: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EndedJob {
    pub id: String,
    pub label: String,
    pub ended_at: f64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Background {
    pub complete: bool,
    pub observed_at: Option<f64>,
    pub jobs: Vec<Job>,
    pub crons: Vec<Cron>,
    pub ended: Vec<EndedJob>,
}
impl Background {
    fn update(&mut self, mut next: Self, now: f64) {
        // A missing job proves an ending only when both enumerations are complete.
        if self.complete && next.complete {
            for old in &self.jobs {
                if !next.jobs.iter().any(|job| job.id == old.id) {
                    self.ended.push(EndedJob {
                        id: old.id.clone(),
                        label: old.label.clone(),
                        ended_at: now,
                    });
                }
            }
        }
        for ended in next.ended.drain(..) {
            if !self.ended.contains(&ended) {
                self.ended.push(ended);
            }
        }
        self.ended.sort_by(|a, b| a.ended_at.total_cmp(&b.ended_at));
        let drop_count = self.ended.len().saturating_sub(10);
        self.ended.drain(..drop_count);
        next.ended = std::mem::take(&mut self.ended);
        next.jobs.sort_by(|a, b| a.id.cmp(&b.id));
        next.crons.sort_by(|a, b| a.id.cmp(&b.id));
        next.observed_at = Some(now);
        *self = next;
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    pub state: State,
    pub since: f64,
    pub detail: Detail,
    pub source: Source,
    pub observed_at: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    pub turn_seq: u64,
    pub last_turn: LastTurn,
    pub background: Background,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stalled_since: Option<f64>,
}
impl AgentState {
    pub fn compatibility_idle(&self, pty_idle: bool) -> bool {
        if self.source.engine_reported() {
            !matches!(self.state, State::Working | State::Starting)
        } else {
            pty_idle
        }
    }
    fn publication_eq(&self, other: &Self) -> bool {
        let mut a = self.clone();
        let mut b = other.clone();
        // Heartbeats update freshness on reads, without repainting every viewer.
        a.observed_at = 0.0;
        b.observed_at = 0.0;
        a.background.observed_at = None;
        b.background.observed_at = None;
        a == b
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresenceStatus {
    Busy,
    Idle,
    Waiting,
    Shell,
    Unknown,
}
/// S2 supplies validated PID/procStart observations. Once seen, invalid presence
/// remains Some(valid=false), so a lost file cannot silently fall back to idle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PresenceObs {
    pub valid: bool,
    pub status: PresenceStatus,
    pub observed_at: f64,
    pub status_updated_at: f64,
    pub engine_version: Option<String>,
    pub waiting_for: Option<String>,
    pub main_turn_open: Option<bool>,
    pub transcript_error: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HookEdges {
    pub prompt_at: Option<f64>,
    pub stop_at: Option<f64>,
    pub failure_at: Option<f64>,
    pub waiting_at: Option<f64>,
    pub waiting_for: Option<String>,
    pub error_kind: Option<String>,
    pub resumes_at: Option<f64>,
    pub open_tool: Option<String>,
}
impl HookEdges {
    fn observed_at(&self) -> Option<f64> {
        [
            self.prompt_at,
            self.stop_at,
            self.failure_at,
            self.waiting_at,
        ]
        .into_iter()
        .flatten()
        .max_by(f64::total_cmp)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RelayStatus {
    #[default]
    Idle,
    Active,
    SystemError,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Approval,
    UserInput,
    Elicitation,
    ToolCall,
    AuthRefresh,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingRequest {
    pub kind: RequestKind,
    pub since: f64,
}
/// Normalized latest-value snapshot. The relay counts foreground starts even
/// when its single-flight publisher coalesces started/completed notifications.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RelaySnapshot {
    pub backend_connected: bool,
    pub foreground: RelayStatus,
    #[serde(default)]
    pub observed_at: f64,
    pub engine_version: Option<String>,
    pub turn_seq: u64,
    pub turn_started_at: Option<f64>,
    pub last_turn: LastTurn,
    #[serde(default)]
    pub active_flags: Vec<String>,
    #[serde(default)]
    pub pending_requests: Vec<PendingRequest>,
    #[serde(default)]
    pub child_active: bool,
    #[serde(default)]
    pub retrying: bool,
    pub error_kind: Option<String>,
    #[serde(default)]
    pub background: Background,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LegacyObs {
    pub has_transcript: bool,
    pub pty_idle: bool,
    pub semantic_idle: Option<bool>,
    pub activity_at: Option<f64>,
    pub input_at: Option<f64>,
    pub turn_ended_at: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inputs {
    pub spawned_at: f64,
    pub exited: bool,
    pub legacy: LegacyObs,
    pub presence: Option<PresenceObs>,
    pub hooks: HookEdges,
    pub relay: Option<RelaySnapshot>,
    pub turn_seq: u64,
    pub latest_start: Option<f64>,
    pub background: Background,
    pub last_progress_at: Option<f64>,
    pub previous: Option<AgentState>,
}
fn newer(edge: Option<f64>, than: Option<f64>) -> bool {
    edge.is_some_and(|at| than.is_none_or(|other| at > other))
}
fn max_time(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    a.into_iter().chain(b).max_by(f64::total_cmp)
}

/// Pure precedence/freshness function. `previous` preserves entered-at times and
/// supplies the working-to-idle debounce; source observations retain true times.
pub fn derive(input: &Inputs, now: f64) -> AgentState {
    let hooks = &input.hooks;
    let presence = input.presence.as_ref();
    let relay = input.relay.as_ref();
    let (source, observed_at, version) = if let Some(r) = relay {
        (Source::Relay, r.observed_at, r.engine_version.clone())
    } else if let Some(p) = presence {
        (Source::Presence, p.observed_at, p.engine_version.clone())
    } else if let Some(at) = hooks.observed_at() {
        (Source::Hooks, at, None)
    } else {
        (
            Source::Pty,
            max_time(input.legacy.activity_at, input.legacy.turn_ended_at)
                .unwrap_or(input.spawned_at),
            None,
        )
    };
    let start = if source.engine_reported() {
        input.latest_start
    } else {
        max_time(input.latest_start, input.legacy.input_at)
    };
    let last_turn = if let Some(r) = relay {
        r.last_turn.clone()
    } else if newer(hooks.failure_at, hooks.stop_at) {
        LastTurn {
            ended_at: hooks.failure_at,
            status: Some(TurnStatus::Failed),
        }
    } else if hooks.stop_at.is_some() {
        LastTurn {
            ended_at: hooks.stop_at,
            status: Some(TurnStatus::Completed),
        }
    } else {
        LastTurn {
            ended_at: input.legacy.turn_ended_at,
            status: input.legacy.turn_ended_at.map(|_| TurnStatus::Completed),
        }
    };
    let mut detail = Detail::default();
    let mut entered = observed_at;
    let state = if input.exited {
        entered = now;
        State::Exited
    } else if !source.engine_reported()
        && !input.legacy.has_transcript
        && now - input.spawned_at < 120.0
    {
        entered = input.spawned_at;
        State::Starting
    } else if presence.is_some_and(|p| !p.valid || p.status == PresenceStatus::Unknown)
        || relay.is_some_and(|r| !r.backend_connected || now - r.observed_at > 90.0)
    {
        entered = relay
            .filter(|r| r.backend_connected)
            .map_or(observed_at, |r| r.observed_at + 90.0);
        State::Unknown
    } else if source.engine_reported() {
        let pending = relay.and_then(|r| {
            r.pending_requests.iter().find(|p| {
                matches!(
                    p.kind,
                    RequestKind::Approval | RequestKind::UserInput | RequestKind::Elicitation
                )
            })
        });
        let flag = relay.and_then(|r| {
            r.active_flags
                .iter()
                .find(|f| matches!(f.as_str(), "waitingOnApproval" | "waitingOnUserInput"))
        });
        let hook_wait = newer(
            hooks.waiting_at,
            max_time(start, max_time(hooks.stop_at, hooks.failure_at)),
        );
        if pending.is_some()
            || flag.is_some()
            || hook_wait
            || presence.is_some_and(|p| {
                p.status == PresenceStatus::Waiting && !newer(start, Some(p.status_updated_at))
            })
        {
            detail.waiting_for = pending
                .map(|p| format!("{:?}", p.kind))
                .or_else(|| flag.cloned())
                .or_else(|| {
                    if hook_wait {
                        hooks.waiting_for.clone()
                    } else {
                        presence.and_then(|p| p.waiting_for.clone())
                    }
                });
            detail.open_tool = if hook_wait {
                hooks.open_tool.clone()
            } else {
                None
            };
            entered = pending
                .map(|p| p.since)
                .or(if hook_wait { hooks.waiting_at } else { None })
                .or_else(|| presence.map(|p| p.status_updated_at))
                .unwrap_or(observed_at);
            State::WaitingOnHuman
        } else if (last_turn.status == Some(TurnStatus::Failed)
            && !newer(start, last_turn.ended_at))
            || relay.is_some_and(|r| {
                r.foreground == RelayStatus::SystemError && !newer(start, Some(r.observed_at))
            })
            || presence.is_some_and(|p| {
                p.status == PresenceStatus::Idle
                    && p.transcript_error.is_some()
                    && !newer(start, Some(p.status_updated_at))
            })
        {
            detail.error_kind = relay
                .and_then(|r| r.error_kind.clone())
                .or_else(|| presence.and_then(|p| p.transcript_error.clone()))
                .or_else(|| hooks.error_kind.clone());
            detail.resumes_at = hooks.resumes_at;
            entered = last_turn.ended_at.unwrap_or(observed_at);
            State::Errored
        } else {
            let open = if hooks.prompt_at.is_some() {
                newer(start, last_turn.ended_at)
            } else {
                presence
                    .and_then(|p| p.main_turn_open)
                    .unwrap_or(input.legacy.semantic_idle != Some(true))
            };
            let pending_input = newer(start, Some(observed_at));
            if pending_input
                || relay.is_some_and(|r| r.foreground == RelayStatus::Active)
                || presence.is_some_and(|p| p.status == PresenceStatus::Busy && open)
                || (source == Source::Hooks && newer(start, last_turn.ended_at))
            {
                if relay.is_some_and(|r| r.retrying) {
                    detail.retrying = Some(true);
                }
                entered = start.unwrap_or(observed_at);
                State::Working
            } else if presence
                .is_some_and(|p| matches!(p.status, PresenceStatus::Busy | PresenceStatus::Shell))
                || relay.is_some_and(|r| r.child_active || !input.background.jobs.is_empty())
                || (source == Source::Hooks && !input.background.jobs.is_empty())
            {
                State::WorkingBackground
            } else {
                entered = presence
                    .map(|p| p.status_updated_at)
                    .or(last_turn.ended_at)
                    .unwrap_or(observed_at);
                State::Idle
            }
        }
    } else if input.legacy.semantic_idle.unwrap_or(input.legacy.pty_idle) {
        entered = input
            .legacy
            .turn_ended_at
            .or_else(|| input.legacy.activity_at.map(|at| at + 2.0))
            .unwrap_or(input.spawned_at);
        State::Idle
    } else {
        entered = start
            .or(input.legacy.activity_at)
            .unwrap_or(input.spawned_at);
        State::Working
    };
    let mut state = state;
    if let Some(old) = &input.previous {
        if state == State::Idle && old.state == State::Working && now - entered < 1.5 {
            state = State::Working;
            entered = old.since;
        } else if old.state == state && old.source == source {
            entered = old.since;
        }
    }
    let progress = max_time(input.last_progress_at, start).unwrap_or(input.spawned_at);
    let stalled_since =
        if matches!(state, State::Working | State::WorkingBackground) && now - progress >= 900.0 {
            Some(progress + 900.0)
        } else {
            None
        };
    AgentState {
        state,
        since: entered.min(now),
        detail,
        source,
        observed_at,
        engine_version: version,
        turn_seq: input.turn_seq,
        last_turn,
        background: input.background.clone(),
        stalled_since,
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum HookEvent {
    UserPromptSubmit,
    Stop,
    StopFailure,
    PermissionRequest,
    Notification,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct HookPayload {
    pub observed_at: Option<f64>,
    pub prompt_id: Option<String>,
    pub continuing: bool,
    pub transcript_path: Option<String>,
    pub waiting_for: Option<String>,
    pub error_kind: Option<String>,
    pub resumes_at: Option<f64>,
    pub tool_name: Option<String>,
    pub notification_type: Option<String>,
    pub background: Option<Background>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Report {
    Hook {
        event: HookEvent,
        #[serde(default)]
        payload: HookPayload,
    },
    Snapshot {
        epoch: String,
        seq: u64,
        snapshot: RelaySnapshot,
    },
}
#[derive(Default, Debug)]
pub struct Applied {
    pub accepted: bool,
    pub started: bool,
    pub ended: bool,
    pub transcript_path: Option<String>,
}
pub type AgentStateCell = Arc<Mutex<StateCell>>;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateCell {
    pub child_start_time: Option<u64>,
    pub inputs: Inputs,
    epoch: Option<String>,
    seq: u64,
    retired_epochs: BTreeSet<String>,
    prompt_ids: VecDeque<String>,
    pending_starts: u64,
    #[serde(skip)]
    published: Option<AgentState>,
}
impl StateCell {
    pub fn new(now: f64, child_start_time: Option<u64>) -> Self {
        Self {
            child_start_time,
            inputs: Inputs {
                spawned_at: now,
                exited: false,
                legacy: LegacyObs::default(),
                presence: None,
                hooks: HookEdges::default(),
                relay: None,
                turn_seq: 0,
                latest_start: None,
                background: Background::default(),
                last_progress_at: None,
                previous: None,
            },
            epoch: None,
            seq: 0,
            retired_epochs: BTreeSet::new(),
            prompt_ids: VecDeque::new(),
            pending_starts: 0,
            published: None,
        }
    }
    pub fn for_process(pid: libc::pid_t) -> AgentStateCell {
        Arc::new(Mutex::new(Self::new(
            unix_now(),
            crate::adopt::proc_starttime(pid).ok(),
        )))
    }
    pub fn note_input(&mut self, now: f64) {
        self.inputs.turn_seq = self.inputs.turn_seq.saturating_add(1);
        self.pending_starts = self.pending_starts.saturating_add(1);
        self.inputs.latest_start = Some(now);
    }
    fn engine_starts(&mut self, count: u64, at: f64) {
        let covered = self.pending_starts.min(count);
        self.pending_starts -= covered;
        self.inputs.turn_seq = self.inputs.turn_seq.saturating_add(count - covered);
        self.inputs.latest_start = max_time(self.inputs.latest_start, Some(at));
    }
    pub fn recompute(&mut self, now: f64) -> AgentState {
        let state = derive(&self.inputs, now);
        self.inputs.previous = Some(state.clone());
        state
    }
    pub fn apply(&mut self, report: Report, now: f64) -> Result<Applied, String> {
        validate_report_bounds(&report)?;
        let mut applied = Applied::default();
        match report {
            Report::Snapshot {
                epoch,
                seq,
                mut snapshot,
            } => {
                let epoch = uuid::Uuid::parse_str(&epoch)
                    .map_err(|_| "epoch must be a UUID".to_string())?
                    .to_string();
                if self.retired_epochs.contains(&epoch)
                    || (self.epoch.as_ref() == Some(&epoch) && seq <= self.seq)
                {
                    return Ok(applied);
                }
                let same_epoch = self.epoch.as_ref() == Some(&epoch);
                if !same_epoch
                    && self.epoch.is_some()
                    && self.retired_epochs.len() >= MAX_RETIRED_EPOCHS
                {
                    return Err("relay epoch history is full; restart the session before another relay epoch".into());
                }
                let prior = self.inputs.relay.as_ref();
                let prior_turns = if same_epoch {
                    prior.map_or(0, |r| r.turn_seq)
                } else {
                    0
                };
                if snapshot.turn_seq < prior_turns {
                    return Err("turn_seq regressed within relay epoch".into());
                }
                validate_time(snapshot.turn_started_at, now)?;
                validate_time(snapshot.last_turn.ended_at, now)?;
                for pending in &snapshot.pending_requests {
                    validate_time(Some(pending.since), now)?;
                }
                validate_background(&snapshot.background, now)?;
                let starts = snapshot.turn_seq - prior_turns;
                applied.started = starts > 0;
                applied.ended = snapshot.last_turn.ended_at.is_some()
                    && !newer(snapshot.turn_started_at, snapshot.last_turn.ended_at)
                    && (!same_epoch || prior.is_none_or(|r| r.last_turn != snapshot.last_turn));
                let at = snapshot.turn_started_at.unwrap_or_else(|| {
                    if snapshot.foreground == RelayStatus::Active {
                        now
                    } else {
                        snapshot.last_turn.ended_at.unwrap_or(now)
                    }
                });
                if starts > 0 {
                    self.engine_starts(starts, at);
                }
                if !same_epoch {
                    if let Some(old) = self.epoch.replace(epoch) {
                        self.retired_epochs.insert(old);
                    }
                }
                self.seq = seq;
                snapshot.observed_at = now;
                for job in &mut snapshot.background.jobs {
                    if job.kind == JobKind::Terminal {
                        job.wakes_agent = false;
                    }
                }
                self.inputs
                    .background
                    .update(snapshot.background.clone(), now);
                self.inputs.relay = Some(snapshot);
            }
            Report::Hook { event, payload } => {
                let at = payload.observed_at.unwrap_or(now);
                validate_time(Some(at), now)?;
                validate_time(payload.resumes_at, f64::MAX)?;
                if let Some(bg) = &payload.background {
                    validate_background(bg, now)?;
                }
                match event {
                    HookEvent::UserPromptSubmit => {
                        if !newer(Some(at), self.inputs.hooks.prompt_at)
                            || payload
                                .prompt_id
                                .as_ref()
                                .is_some_and(|id| self.prompt_ids.contains(id))
                        {
                            return Ok(applied);
                        }
                        if let Some(id) = payload.prompt_id {
                            if self.prompt_ids.len() == MAX_PROMPT_IDS {
                                self.prompt_ids.pop_front();
                            }
                            self.prompt_ids.push_back(id);
                        }
                        self.inputs.hooks.prompt_at = Some(at);
                        self.engine_starts(1, at);
                        applied.started = !newer(
                            max_time(self.inputs.hooks.stop_at, self.inputs.hooks.failure_at),
                            Some(at),
                        );
                    }
                    HookEvent::Stop | HookEvent::StopFailure => {
                        let ended =
                            max_time(self.inputs.hooks.stop_at, self.inputs.hooks.failure_at);
                        if !newer(Some(at), ended) {
                            return Ok(applied);
                        }
                        if matches!(event, HookEvent::StopFailure) {
                            self.inputs.hooks.failure_at = Some(at);
                            self.inputs.hooks.error_kind = payload.error_kind;
                            self.inputs.hooks.resumes_at = payload.resumes_at;
                        } else {
                            self.inputs.hooks.stop_at = Some(at);
                        }
                        applied.ended = !newer(self.inputs.latest_start, Some(at));
                        if payload.continuing && applied.ended {
                            self.engine_starts(1, at + 0.000001);
                            self.inputs.hooks.prompt_at = Some(at + 0.000001);
                            self.inputs.latest_start = self.inputs.hooks.prompt_at;
                            applied.started = true;
                            applied.ended = false;
                        }
                    }
                    HookEvent::PermissionRequest | HookEvent::Notification => {
                        if matches!(event, HookEvent::Notification)
                            && !matches!(
                                payload.notification_type.as_deref(),
                                Some("permission_prompt" | "elicitation_dialog" | "idle_prompt")
                            )
                        {
                            return Ok(applied);
                        }
                        if !newer(Some(at), self.inputs.hooks.waiting_at) {
                            return Ok(applied);
                        }
                        self.inputs.hooks.waiting_at = Some(at);
                        self.inputs.hooks.waiting_for = payload
                            .waiting_for
                            .or(payload.notification_type)
                            .or_else(|| Some("permission".into()));
                        self.inputs.hooks.open_tool = payload.tool_name;
                    }
                }
                if let Some(bg) = payload.background {
                    if !newer(self.inputs.background.observed_at, Some(at)) {
                        self.inputs.background.update(bg, at);
                    }
                }
                if !newer(self.inputs.latest_start, Some(at)) {
                    applied.transcript_path = payload.transcript_path;
                }
            }
        }
        applied.accepted = true;
        Ok(applied)
    }
}
const MAX_PROMPT_IDS: usize = 256;
const MAX_RETIRED_EPOCHS: usize = 64;
const MAX_TEXT_BYTES: usize = 4096;
const MAX_ID_BYTES: usize = 256;
const MAX_BACKGROUND_ENTRIES: usize = 256;

fn bounded(text: Option<&str>, cap: usize, field: &str) -> Result<(), String> {
    if text.is_some_and(|text| text.len() > cap) {
        Err(format!("{field} exceeds {cap} UTF-8 bytes"))
    } else {
        Ok(())
    }
}
fn background_bounds(bg: &Background) -> Result<(), String> {
    if bg.jobs.len() > MAX_BACKGROUND_ENTRIES
        || bg.crons.len() > MAX_BACKGROUND_ENTRIES
        || bg.ended.len() > 10
    {
        return Err("background exceeds 256 jobs/crons or 10 ended jobs".into());
    }
    for job in &bg.jobs {
        bounded(Some(&job.id), MAX_ID_BYTES, "job id")?;
        bounded(Some(&job.label), MAX_TEXT_BYTES, "job label")?;
    }
    for cron in &bg.crons {
        bounded(Some(&cron.id), MAX_ID_BYTES, "cron id")?;
        bounded(Some(&cron.schedule), MAX_TEXT_BYTES, "cron schedule")?;
    }
    for job in &bg.ended {
        bounded(Some(&job.id), MAX_ID_BYTES, "ended job id")?;
        bounded(Some(&job.label), MAX_TEXT_BYTES, "ended job label")?;
    }
    Ok(())
}
fn validate_report_bounds(report: &Report) -> Result<(), String> {
    match report {
        Report::Hook { payload, .. } => {
            bounded(payload.prompt_id.as_deref(), MAX_ID_BYTES, "prompt_id")?;
            for (field, text) in [
                ("transcript_path", &payload.transcript_path),
                ("waiting_for", &payload.waiting_for),
                ("error_kind", &payload.error_kind),
                ("tool_name", &payload.tool_name),
                ("notification_type", &payload.notification_type),
            ] {
                bounded(text.as_deref(), MAX_TEXT_BYTES, field)?;
            }
            if let Some(bg) = &payload.background {
                background_bounds(bg)?;
            }
        }
        Report::Snapshot {
            epoch, snapshot, ..
        } => {
            bounded(Some(epoch), 36, "epoch")?;
            bounded(
                snapshot.engine_version.as_deref(),
                MAX_ID_BYTES,
                "engine_version",
            )?;
            bounded(snapshot.error_kind.as_deref(), MAX_TEXT_BYTES, "error_kind")?;
            if snapshot.active_flags.len() > 16 || snapshot.pending_requests.len() > 256 {
                return Err("snapshot exceeds 16 active flags or 256 pending requests".into());
            }
            for flag in &snapshot.active_flags {
                bounded(Some(flag), MAX_ID_BYTES, "active flag")?;
            }
            background_bounds(&snapshot.background)?;
        }
    }
    Ok(())
}

fn validate_time(at: Option<f64>, now: f64) -> Result<(), String> {
    if at.is_some_and(|at| !at.is_finite() || at < 0.0 || at > now + 5.0) {
        Err("timestamps must be finite Unix seconds, no more than 5 seconds in the future".into())
    } else {
        Ok(())
    }
}
fn validate_background(bg: &Background, now: f64) -> Result<(), String> {
    for job in &bg.jobs {
        validate_time(Some(job.first_seen_at), now)?;
        if job.cpu.is_some_and(|cpu| !cpu.is_finite() || cpu < 0.0) {
            return Err("cpu must be a nonnegative percentage".into());
        }
    }
    for job in &bg.ended {
        validate_time(Some(job.ended_at), now)?;
    }
    Ok(())
}

pub fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
fn instant_unix(at: Instant) -> f64 {
    static ANCHOR: OnceLock<(Instant, f64)> = OnceLock::new();
    let (mono, wall) = *ANCHOR.get_or_init(|| (Instant::now(), unix_now()));
    if at >= mono {
        wall + at.duration_since(mono).as_secs_f64()
    } else {
        wall - mono.duration_since(at).as_secs_f64()
    }
}
fn legacy(session: &DaemonSession) -> LegacyObs {
    let activity = *session
        .last_activity_at
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let input = *session
        .last_input_at
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let end = *session
        .last_turn_end_at
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    LegacyObs {
        has_transcript: session.transcript_path.is_some(),
        pty_idle: activity.is_none_or(|at| at.elapsed() >= Duration::from_secs(2)),
        semantic_idle: end.map(|end| input.is_none_or(|input| end >= input)),
        activity_at: activity.map(instant_unix),
        input_at: input.map(instant_unix),
        turn_ended_at: end.map(instant_unix),
    }
}
pub fn current(session: &DaemonSession) -> AgentState {
    let legacy = legacy(session);
    let mut cell = session
        .agent_state
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    cell.inputs.legacy = legacy;
    cell.inputs.exited = session.last_exit.kernel_set();
    cell.recompute(unix_now())
}
/// The tick is the only publisher. Reports coalesce a latest value per session;
/// the bounded drain leaves room for unrelated lifecycle diffs in the watcher.
const MAX_STATE_DIFFS_PER_TICK: usize = crate::manifest::MANIFEST_WATCH_BUFFER / 4;
const PERSIST_INTERVAL: Duration = Duration::from_secs(5);
#[derive(Default)]
struct Publisher {
    started: bool,
    pending: BTreeMap<String, (AgentStateCell, AgentState)>,
    order: VecDeque<String>,
}
#[derive(Default)]
struct Persistence {
    last_attempt: Option<Instant>,
    saved: Option<String>,
}
#[derive(Default)]
pub struct Runtime {
    publisher: Mutex<Publisher>,
    presence: Mutex<crate::claude_presence::Reader>,
    // Lock order is persistence gate -> DaemonState. Disk writes keep only the
    // gate; a checked restart waits outside DaemonState and always lands last.
    persistence: Mutex<Persistence>,
}

/// Caller holds DaemonState. The first observation is a baseline, not a diff:
/// manifest subscribers already receive current state in their initial snapshot.
pub fn recompute_and_publish(state: &DaemonState, uid: &str) {
    let Some(session) = state.sessions.get(uid) else {
        return;
    };
    let next = current(session);
    let mut cell = session
        .agent_state
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let mut publisher = state
        .agent_state_runtime
        .publisher
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if cell.published.is_none() && !publisher.started {
        cell.published = Some(next);
        return;
    }
    if cell
        .published
        .as_ref()
        .is_some_and(|published| published.publication_eq(&next))
    {
        if publisher.pending.remove(uid).is_some() {
            publisher.order.retain(|queued| queued != uid);
        }
    } else {
        if !publisher.pending.contains_key(uid) {
            publisher.order.push_back(uid.to_string());
        }
        publisher
            .pending
            .insert(uid.to_string(), (Arc::clone(&session.agent_state), next));
    }
}
fn flush_publishes(state: &DaemonState) {
    let mut publisher = state
        .agent_state_runtime
        .publisher
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    publisher.started = true;
    // Dead/replaced sessions never publish after their exit/revive lifecycle diff.
    publisher.pending.retain(|uid, (cell, _)| {
        state
            .sessions
            .get(uid)
            .is_some_and(|s| Arc::ptr_eq(&s.agent_state, cell))
    });
    let live: BTreeSet<_> = publisher.pending.keys().cloned().collect();
    publisher.order.retain(|uid| live.contains(uid));
    let mut sent = 0;
    while sent < MAX_STATE_DIFFS_PER_TICK {
        let Some(uid) = publisher.order.pop_front() else {
            break;
        };
        let Some((cell, next)) = publisher.pending.remove(&uid) else {
            continue;
        };
        state.manifest_watcher.broadcast(ManifestDiff::Updated {
            uid,
            entry: serde_json::json!({"agent_state": next}),
        });
        cell.lock().unwrap_or_else(|p| p.into_inner()).published = Some(next);
        sent += 1;
    }
}
pub fn snapshot(state: &DaemonState) -> BTreeMap<String, AgentState> {
    state
        .sessions
        .iter()
        .map(|(uid, session)| {
            let next = current(session);
            let mut cell = session
                .agent_state
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if cell.published.is_none() {
                cell.published = Some(next.clone());
            }
            (uid.clone(), next)
        })
        .collect()
}

/// Restart facts only: no PTY clocks, transient progress samples, duplicate
/// derived state or publication caches. Source observations retain freshness.
#[derive(Serialize, Deserialize)]
struct RestartRecord {
    child_start_time: Option<u64>,
    spawned_at: f64,
    state: State,
    source: Source,
    since: f64,
    stalled_since: Option<f64>,
    turn_seq: u64,
    latest_start: Option<f64>,
    hooks: HookEdges,
    background: Background,
    presence: Option<PresenceObs>,
    relay: Option<RelaySnapshot>,
    epoch: Option<String>,
    seq: u64,
    retired_epochs: BTreeSet<String>,
    prompt_ids: VecDeque<String>,
    pending_starts: u64,
}
impl RestartRecord {
    fn capture(session: &DaemonSession) -> Self {
        let state = current(session);
        let c = session
            .agent_state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut relay = c.inputs.relay.clone();
        // The normalized background lives once, in inputs.background.
        if let Some(r) = &mut relay {
            r.background = Background::default();
        }
        Self {
            child_start_time: c.child_start_time,
            spawned_at: c.inputs.spawned_at,
            state: state.state,
            source: state.source,
            since: state.since,
            stalled_since: state.stalled_since,
            turn_seq: c.inputs.turn_seq,
            latest_start: c.inputs.latest_start,
            hooks: c.inputs.hooks.clone(),
            background: c.inputs.background.clone(),
            presence: c.inputs.presence.clone(),
            relay,
            epoch: c.epoch.clone(),
            seq: c.seq,
            retired_epochs: c.retired_epochs.clone(),
            prompt_ids: c.prompt_ids.clone(),
            pending_starts: c.pending_starts,
        }
    }
    fn restore(self, cell: &mut StateCell, legacy: LegacyObs, now: f64) {
        cell.inputs = Inputs {
            spawned_at: self.spawned_at,
            exited: false,
            legacy,
            presence: self.presence,
            hooks: self.hooks,
            relay: self.relay,
            turn_seq: self.turn_seq,
            latest_start: self.latest_start,
            background: self.background,
            last_progress_at: Some(self.stalled_since.map_or(now, |at| at - 900.0)),
            previous: None,
        };
        let mut state = derive(&cell.inputs, now);
        if state.state == self.state && state.source == self.source {
            state.since = self.since;
        }
        cell.inputs.previous = Some(state);
        cell.epoch = self.epoch;
        cell.seq = self.seq;
        cell.retired_epochs = self.retired_epochs;
        cell.prompt_ids = self.prompt_ids;
        cell.pending_starts = self.pending_starts;
        cell.published = None;
    }
}
#[derive(Serialize, Deserialize)]
struct Sidecar {
    version: u32,
    boot_id: String,
    sessions: BTreeMap<String, RestartRecord>,
}
fn boot_id() -> std::io::Result<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|id| id.trim().to_string())
}
fn path(state: &DaemonState) -> Option<PathBuf> {
    state
        .daemon_sessions_path
        .as_ref()
        .map(|p| p.with_file_name("daemon-agent-state.json"))
}
fn capture(state: &DaemonState) -> BTreeMap<String, RestartRecord> {
    state
        .sessions
        .iter()
        .map(|(uid, s)| (uid.clone(), RestartRecord::capture(s)))
        .collect()
}
fn persist_with(
    state: &Arc<Mutex<DaemonState>>,
    force: bool,
    now: Instant,
    write: impl FnOnce(&std::path::Path, &str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let runtime = Arc::clone(
        &state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .agent_state_runtime,
    );
    let mut persistence = runtime
        .persistence
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if !force
        && persistence
            .last_attempt
            .is_some_and(|at| now.saturating_duration_since(at) < PERSIST_INTERVAL)
    {
        return Ok(());
    }
    let (path, sessions) = {
        let state = state.lock().unwrap_or_else(|p| p.into_inner());
        if !force && state.restarting {
            return Ok(());
        }
        let Some(path) = path(&state) else {
            return Ok(());
        };
        (path, capture(&state))
    };
    persistence.last_attempt = Some(now);
    let encoded = serde_json::to_string(&Sidecar {
        version: 2,
        boot_id: boot_id()?,
        sessions,
    })
    .map_err(std::io::Error::other)?;
    if force || persistence.saved.as_ref() != Some(&encoded) {
        write(&path, &encoded)?;
        persistence.saved = Some(encoded);
    }
    Ok(())
}
/// Call outside DaemonState while the restart writer/reader barriers are held.
/// The gate waits for any older tick write, then this fresh checked flush wins.
pub fn save_checked(state: &Arc<Mutex<DaemonState>>) -> std::io::Result<()> {
    persist_with(state, true, Instant::now(), |path, json| {
        crate::state::write_json_atomic(path, json, true)
    })
}
pub fn restore(state: &DaemonState) -> std::io::Result<usize> {
    let Some(path) = path(state) else {
        return Ok(0);
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let sidecar: Sidecar = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    if sidecar.version != 2 || sidecar.boot_id != boot_id()? {
        return Ok(0);
    }
    let mut restored = 0;
    for (uid, saved) in sidecar.sessions {
        if let Some(session) = state.sessions.get(&uid) {
            let legacy = legacy(session);
            let mut cell = session
                .agent_state
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if saved.child_start_time.is_some() && saved.child_start_time == cell.child_start_time {
                if saved.prompt_ids.len() > MAX_PROMPT_IDS
                    || saved.retired_epochs.len() > MAX_RETIRED_EPOCHS
                {
                    return Err(std::io::Error::other(
                        "agent state sidecar exceeds history limits",
                    ));
                }
                saved.restore(&mut cell, legacy, unix_now());
                restored += 1;
            }
        }
    }
    Ok(restored)
}
/// Stat transcript/subagent growth outside the daemon mutex; verify cell identity
/// again before applying, since a UID may have been revived while stat ran.
pub fn tick(state: &Arc<Mutex<DaemonState>>) -> std::io::Result<()> {
    let (runtime, targets, probes) = {
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        if st.restarting {
            return Ok(());
        }
        let targets: Vec<_> = st
            .sessions
            .iter()
            .filter_map(|(uid, s)| {
                if s.session_type != "claude-code" || s.last_exit.kernel_set() {
                    return None;
                }
                let cell = s.agent_state.lock().unwrap_or_else(|p| p.into_inner());
                Some(crate::claude_presence::Target {
                    uid: uid.clone(),
                    pid: s.pid as u32,
                    child_start_time: cell.child_start_time,
                    transcript: s.transcript_path.as_ref().map(PathBuf::from),
                    had_presence: cell.inputs.presence.is_some(),
                    needs_turn_probe: cell.inputs.hooks.prompt_at.is_none(),
                })
            })
            .collect();
        let probes: Vec<_> = st
            .sessions
            .iter()
            .map(|(uid, s)| {
                (
                    uid.clone(),
                    Arc::clone(&s.agent_state),
                    s.transcript_path.clone(),
                )
            })
            .collect();
        (Arc::clone(&st.agent_state_runtime), targets, probes)
    };
    let observations = runtime
        .presence
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .observe(&targets, unix_now());
    let mut observations: BTreeMap<_, _> = targets
        .into_iter()
        .zip(observations)
        .map(|(target, observation)| (target.uid, (target.transcript, observation)))
        .collect();
    let progress: Vec<_> = probes
        .into_iter()
        .map(|(uid, cell, path)| {
            let at = path.and_then(|p| {
                let p = PathBuf::from(p);
                let subagents = p.with_extension("").join("subagents");
                [p, subagents]
                    .iter()
                    .filter_map(|p| {
                        std::fs::metadata(p)
                            .ok()?
                            .modified()
                            .ok()?
                            .duration_since(UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs_f64())
                    })
                    .max_by(f64::total_cmp)
            });
            (uid, cell, at)
        })
        .collect();
    let st = state.lock().unwrap_or_else(|p| p.into_inner());
    if st.restarting {
        return Ok(());
    }
    for (uid, cell, progress_at) in progress {
        if st
            .sessions
            .get(&uid)
            .is_some_and(|s| Arc::ptr_eq(&s.agent_state, &cell))
        {
            {
                let mut cell = cell.lock().unwrap_or_else(|p| p.into_inner());
                cell.inputs.last_progress_at = max_time(cell.inputs.last_progress_at, progress_at);
                if let Some((transcript, Some(observation))) = observations.remove(&uid) {
                    // A transcript may rotate during the unlocked read. Keep the
                    // source, but never carry a verdict from the old transcript.
                    let mut observation = observation;
                    if st.sessions[&uid]
                        .transcript_path
                        .as_ref()
                        .map(PathBuf::from)
                        != transcript
                    {
                        observation.main_turn_open = None;
                        observation.transcript_error = None;
                    }
                    cell.inputs.presence = Some(observation);
                }
            }
            recompute_and_publish(&st, &uid);
        }
    }
    flush_publishes(&st);
    drop(st);
    persist_with(state, false, Instant::now(), |path, json| {
        crate::state::write_json_atomic(path, json, true)
    })
}

pub fn start(state: &Arc<Mutex<DaemonState>>) -> std::io::Result<()> {
    let weak = Arc::downgrade(state);
    std::thread::Builder::new()
        .name("cm-agent-state".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            let Some(state) = weak.upgrade() else {
                break;
            };
            if let Err(e) = tick(&state) {
                eprintln!("cm-daemon: agent state tick: {e}");
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> Inputs {
        let mut i = StateCell::new(0.0, Some(42)).inputs;
        i.legacy.has_transcript = true;
        i.legacy.activity_at = Some(990.0);
        i.last_progress_at = Some(990.0);
        i
    }
    fn presence(status: PresenceStatus) -> Inputs {
        let mut i = inputs();
        i.presence = Some(PresenceObs {
            valid: true,
            status,
            observed_at: 995.0,
            status_updated_at: 990.0,
            engine_version: Some("2.1.291".into()),
            waiting_for: None,
            main_turn_open: Some(true),
            transcript_error: None,
        });
        i
    }
    fn relay(status: RelayStatus) -> Inputs {
        let mut i = inputs();
        i.relay = Some(RelaySnapshot {
            backend_connected: true,
            foreground: status,
            observed_at: 995.0,
            ..Default::default()
        });
        i
    }
    fn job(id: &str) -> Job {
        Job {
            id: id.into(),
            kind: JobKind::Terminal,
            label: "sleep 30".into(),
            pid: Some(321),
            cpu: Some(0.0),
            first_seen_at: 980.0,
            wakes_agent: true,
        }
    }
    fn report(epoch: &str, seq: u64, snapshot: RelaySnapshot) -> Report {
        Report::Snapshot {
            epoch: epoch.into(),
            seq,
            snapshot,
        }
    }
    fn hook(event: HookEvent, at: f64) -> Report {
        Report::Hook {
            event,
            payload: HookPayload {
                observed_at: Some(at),
                ..Default::default()
            },
        }
    }

    #[test]
    fn derive_claude_research_hard_cases() {
        let mut cases = Vec::new();
        for name in [
            "normal turn",
            "foreground subagent",
            "compact",
            "retry backoff",
            "queued input",
            "channel wake",
        ] {
            cases.push((name, presence(PresenceStatus::Busy), State::Working));
        }
        for name in [
            "normal ended",
            "interrupted Esc without Stop",
            "unsubmitted draft",
            "resume or clear",
        ] {
            let mut i = presence(PresenceStatus::Idle);
            i.latest_start = Some(900.0);
            i.hooks.prompt_at = Some(900.0);
            cases.push((name, i, State::Idle));
        }
        let mut background = presence(PresenceStatus::Busy);
        background.presence.as_mut().unwrap().main_turn_open = Some(false);
        cases.push((
            "background agents after Stop",
            background,
            State::WorkingBackground,
        ));
        cases.push((
            "background bash",
            presence(PresenceStatus::Shell),
            State::WorkingBackground,
        ));
        for reason in [
            "permission prompt",
            "input needed",
            "dialog open",
            "worker request",
        ] {
            let mut i = presence(PresenceStatus::Waiting);
            i.presence.as_mut().unwrap().waiting_for = Some(reason.into());
            assert_eq!(
                derive(&i, 1000.0).detail.waiting_for.as_deref(),
                Some(reason)
            );
            cases.push((reason, i, State::WaitingOnHuman));
        }
        for kind in ["authentication_failed", "rate_limited", "api_error"] {
            let mut i = presence(PresenceStatus::Idle);
            i.presence.as_mut().unwrap().transcript_error = Some(kind.into());
            assert_eq!(derive(&i, 1000.0).detail.error_kind.as_deref(), Some(kind));
            cases.push((kind, i, State::Errored));
        }
        let mut failed = inputs();
        failed.hooks.failure_at = Some(995.0);
        failed.hooks.error_kind = Some("rate_limited".into());
        failed.hooks.resumes_at = Some(1200.0);
        assert_eq!(derive(&failed, 1000.0).detail.resumes_at, Some(1200.0));
        cases.push(("StopFailure without presence", failed, State::Errored));
        let mut missing = presence(PresenceStatus::Idle);
        missing.presence.as_mut().unwrap().valid = false;
        cases.push((
            "missing/truncated/procStart mismatch",
            missing.clone(),
            State::Unknown,
        ));
        missing.exited = true;
        cases.push(("crashed wins over stale presence", missing, State::Exited));
        let mut hung = presence(PresenceStatus::Busy);
        hung.last_progress_at = Some(10.0);
        assert_eq!(derive(&hung, 1000.0).stalled_since, Some(910.0));
        cases.push(("hung process", hung, State::Working));
        let fresh = StateCell::new(990.0, Some(42)).inputs;
        cases.push(("startup without transcript", fresh, State::Starting));
        for (name, i, expected) in cases {
            let actual = derive(&i, 1000.0);
            assert_eq!(actual.state, expected, "{name}");
            if actual.state == State::Idle {
                assert_eq!(actual.since, 990.0, "{name}");
            }
        }
    }

    #[test]
    fn derive_codex_research_hard_cases() {
        let mut cases = Vec::new();
        for name in [
            "long foreground exec",
            "quiet thinking",
            "compaction",
            "detached viewer",
            "resumed active",
        ] {
            cases.push((name, relay(RelayStatus::Active), State::Working));
        }
        let mut retry = relay(RelayStatus::Active);
        retry.relay.as_mut().unwrap().retrying = true;
        assert_eq!(derive(&retry, 1000.0).detail.retrying, Some(true));
        cases.push(("retrying error", retry, State::Working));
        let mut bg = relay(RelayStatus::Idle);
        bg.background.jobs.push(job("bg"));
        cases.push(("background terminal", bg, State::WorkingBackground));
        let mut child = relay(RelayStatus::Idle);
        child.relay.as_mut().unwrap().child_active = true;
        cases.push((
            "child active after parent ends",
            child,
            State::WorkingBackground,
        ));
        for kind in [
            RequestKind::Approval,
            RequestKind::UserInput,
            RequestKind::Elicitation,
        ] {
            let mut i = relay(RelayStatus::Active);
            i.relay
                .as_mut()
                .unwrap()
                .pending_requests
                .push(PendingRequest { kind, since: 980.0 });
            cases.push(("pending request in thread tree", i, State::WaitingOnHuman));
        }
        for flag in ["waitingOnApproval", "waitingOnUserInput"] {
            let mut i = relay(RelayStatus::Active);
            i.relay.as_mut().unwrap().active_flags.push(flag.into());
            cases.push((flag, i, State::WaitingOnHuman));
        }
        for kind in [RequestKind::ToolCall, RequestKind::AuthRefresh] {
            let mut i = relay(RelayStatus::Active);
            i.relay
                .as_mut()
                .unwrap()
                .pending_requests
                .push(PendingRequest { kind, since: 980.0 });
            cases.push(("tool/auth does not need a human", i, State::Working));
        }
        let mut failed = relay(RelayStatus::Idle);
        failed.relay.as_mut().unwrap().last_turn = LastTurn {
            ended_at: Some(980.0),
            status: Some(TurnStatus::Failed),
        };
        failed.relay.as_mut().unwrap().error_kind = Some("usageLimitExceeded".into());
        cases.push(("fatal usage/pool error", failed.clone(), State::Errored));
        failed.latest_start = Some(999.0);
        cases.push(("new turn clears failed turn", failed, State::Working));
        cases.push((
            "system error",
            relay(RelayStatus::SystemError),
            State::Errored,
        ));
        let mut interrupted = relay(RelayStatus::Idle);
        interrupted.relay.as_mut().unwrap().last_turn = LastTurn {
            ended_at: Some(980.0),
            status: Some(TurnStatus::Interrupted),
        };
        assert_eq!(
            derive(&interrupted, 1000.0).last_turn.status,
            Some(TurnStatus::Interrupted)
        );
        cases.push(("interrupted", interrupted, State::Idle));
        cases.push((
            "parked composer without engine start",
            relay(RelayStatus::Idle),
            State::Idle,
        ));
        let mut queued = relay(RelayStatus::Idle);
        queued.latest_start = Some(999.0);
        cases.push(("CM submitted pending start", queued, State::Working));
        let mut lost = relay(RelayStatus::Active);
        lost.relay.as_mut().unwrap().backend_connected = false;
        cases.push(("backend disconnected", lost, State::Unknown));
        let mut stale = relay(RelayStatus::Active);
        stale.relay.as_mut().unwrap().observed_at = 900.0;
        cases.push(("relay heartbeat expired", stale.clone(), State::Unknown));
        stale.exited = true;
        cases.push(("launcher crashed", stale, State::Exited));
        for (name, i, expected) in cases {
            assert_eq!(derive(&i, 1000.0).state, expected, "{name}");
        }
    }

    #[test]
    fn fallback_debounce_preserves_true_idle_since_and_compatibility() {
        let mut i = inputs();
        i.legacy.semantic_idle = Some(false);
        i.previous = Some(derive(&i, 1000.0));
        i.legacy.semantic_idle = Some(true);
        i.legacy.turn_ended_at = Some(1001.0);
        assert_eq!(derive(&i, 1002.0).state, State::Working);
        let idle = derive(&i, 1003.0);
        assert_eq!(
            (idle.state, idle.since, idle.source),
            (State::Idle, 1001.0, Source::Pty)
        );
        assert!(
            !idle.compatibility_idle(false),
            "fallback keeps raw PTY compatibility"
        );
        let mut bg = relay(RelayStatus::Idle);
        bg.relay.as_mut().unwrap().child_active = true;
        let state = derive(&bg, 1000.0);
        assert_eq!(state.state, State::WorkingBackground);
        assert!(
            state.compatibility_idle(false),
            "legacy turn_end monitors may fire while background work continues"
        );
        let mut fresh = StateCell::new(990.0, None).inputs;
        fresh.presence = presence(PresenceStatus::Waiting).presence;
        assert_eq!(
            derive(&fresh, 1000.0).state,
            State::WaitingOnHuman,
            "trust dialog engine report beats starting"
        );
    }

    #[test]
    fn relay_ordering_epochs_and_coalesced_turns() {
        let epoch = uuid::Uuid::new_v4().to_string();
        let next_epoch = uuid::Uuid::new_v4().to_string();
        let mut c = StateCell::new(0.0, Some(42));
        c.note_input(900.0);
        let mut r = RelaySnapshot {
            backend_connected: true,
            turn_seq: 1,
            turn_started_at: Some(901.0),
            foreground: RelayStatus::Idle,
            last_turn: LastTurn {
                ended_at: Some(980.0),
                status: Some(TurnStatus::Completed),
            },
            ..Default::default()
        };
        let applied = c.apply(report(&epoch, 2, r.clone()), 990.0).unwrap();
        assert!(applied.started && applied.ended);
        assert_eq!(
            c.inputs.turn_seq, 1,
            "send plus engine confirmation counts once"
        );
        assert_eq!(c.recompute(990.0).state, State::Idle);
        r.foreground = RelayStatus::Active;
        assert!(
            !c.apply(report(&epoch, 1, r.clone()), 991.0)
                .unwrap()
                .accepted
        );
        assert!(
            !c.apply(report(&epoch, 2, r.clone()), 991.0)
                .unwrap()
                .accepted
        );
        assert_eq!(
            c.recompute(991.0).state,
            State::Idle,
            "late start never replaces completed snapshot"
        );
        r.turn_seq = 3;
        r.turn_started_at = Some(995.0);
        c.apply(report(&epoch, 3, r.clone()), 996.0).unwrap();
        assert_eq!(
            c.inputs.turn_seq, 3,
            "coalesced starts retained by producer counter"
        );
        c.apply(
            report(
                &next_epoch,
                1,
                RelaySnapshot {
                    backend_connected: true,
                    ..Default::default()
                },
            ),
            998.0,
        )
        .unwrap();
        assert!(
            !c.apply(report(&epoch, 100, r), 999.0).unwrap().accepted,
            "retired epoch cannot return"
        );
        let after = c.inputs.turn_seq;
        c.apply(
            report(
                &next_epoch,
                2,
                RelaySnapshot {
                    backend_connected: true,
                    ..Default::default()
                },
            ),
            1000.0,
        )
        .unwrap();
        assert_eq!(c.inputs.turn_seq, after, "heartbeats are not new turns");
    }

    #[test]
    fn hooks_clear_wait_error_continue_and_ignore_late_edges() {
        let mut c = StateCell::new(0.0, Some(42));
        c.apply(hook(HookEvent::UserPromptSubmit, 900.0), 900.0)
            .unwrap();
        c.apply(hook(HookEvent::PermissionRequest, 910.0), 910.0)
            .unwrap();
        assert_eq!(c.recompute(915.0).state, State::WaitingOnHuman);
        c.apply(hook(HookEvent::StopFailure, 920.0), 920.0).unwrap();
        assert_eq!(c.recompute(925.0).state, State::Errored);
        c.apply(hook(HookEvent::UserPromptSubmit, 930.0), 930.0)
            .unwrap();
        assert_eq!(c.recompute(935.0).state, State::Working);
        let old = c.apply(hook(HookEvent::Stop, 925.0), 935.0).unwrap();
        assert!(
            !old.ended,
            "late Stop must not stamp semantic idle for a newer turn"
        );
        assert_eq!(c.recompute(935.0).state, State::Working);
        assert!(
            !c.apply(hook(HookEvent::UserPromptSubmit, 900.0), 940.0)
                .unwrap()
                .accepted
        );
        c.apply(
            Report::Hook {
                event: HookEvent::Stop,
                payload: HookPayload {
                    observed_at: Some(945.0),
                    continuing: true,
                    ..Default::default()
                },
            },
            945.0,
        )
        .unwrap();
        assert_eq!(c.recompute(948.0).state, State::Working);
        assert_eq!(c.inputs.turn_seq, 3);
    }

    #[test]
    fn background_history_is_bounded_and_incomplete_does_not_prove_ending() {
        let mut bg = Background::default();
        for n in 0..12 {
            bg.update(
                Background {
                    complete: true,
                    jobs: vec![job(&n.to_string())],
                    ..Default::default()
                },
                1000.0 + n as f64,
            );
        }
        assert_eq!(bg.ended.len(), 10);
        assert_eq!(bg.ended.first().unwrap().id, "1");
        bg.update(Background::default(), 1020.0);
        assert_eq!(bg.ended.len(), 10);
        assert!(!bg.ended.iter().any(|j| j.id == "11"));
    }

    fn session_state() -> DaemonState {
        let mut params =
            crate::session::SpawnParams::new("ts-agent-state", "agent-state", "/bin/sleep");
        params.args = vec!["120".into()];
        let session = DaemonSession::spawn(params).unwrap();
        let mut state = DaemonState::new();
        state.sessions.insert(session.uid.clone(), session);
        state
    }

    #[test]
    fn claude_presence_tick_publishes_status_and_source_loss_for_existing_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(session_state()));
        let uid = "ts-agent-state";
        let (pid, identity, rx, _guard) = {
            let st = state.lock().unwrap();
            *st.agent_state_runtime.presence.lock().unwrap() = crate::claude_presence::Reader::new(
                dir.path().to_path_buf(),
                PathBuf::from("/proc"),
            );
            let s = &st.sessions[uid];
            let identity = s.agent_state.lock().unwrap().child_start_time.unwrap();
            let (rx, guard) = st.manifest_watcher.subscribe();
            snapshot(&st);
            (s.pid, identity, rx, guard)
        };
        let path = dir.path().join(format!("{pid}.json"));
        let since = unix_now().floor() - 10.0;
        let mut file = serde_json::json!({"pid":pid,"procStart":identity.to_string(),"kind":"interactive","version":"2.1.291","status":"waiting","waitingFor":"permission prompt","statusUpdatedAt":since * 1000.0});
        std::fs::write(&path, file.to_string()).unwrap();
        tick(&state).unwrap();
        let ManifestDiff::Updated { entry, .. } = rx.try_recv().unwrap() else {
            panic!("state update");
        };
        assert_eq!(entry["agent_state"]["state"], "waiting-on-human");
        assert_eq!(entry["agent_state"]["source"], "presence");
        assert_eq!(
            entry["agent_state"]["detail"]["waiting_for"],
            "permission prompt"
        );
        tick(&state).unwrap();
        assert!(
            rx.try_recv().is_err(),
            "unchanged status heartbeat does not publish"
        );
        file["status"] = "idle".into();
        std::fs::write(&path, file.to_string()).unwrap();
        tick(&state).unwrap();
        let next = current(&state.lock().unwrap().sessions[uid]);
        assert_eq!(next.state, State::Idle);
        assert_eq!(next.since, since);
        std::fs::remove_file(&path).unwrap();
        tick(&state).unwrap();
        assert_eq!(
            current(&state.lock().unwrap().sessions[uid]).state,
            State::Unknown
        );
    }

    #[test]
    fn publication_suppresses_heartbeats_but_keeps_turn_and_stall_edges() {
        let state = session_state();
        let uid = "ts-agent-state";
        let (rx, _guard) = state.manifest_watcher.subscribe();
        snapshot(&state);
        let cell = &state.sessions[uid].agent_state;
        let epoch = uuid::Uuid::new_v4().to_string();
        let now = unix_now();
        let r = RelaySnapshot {
            backend_connected: true,
            foreground: RelayStatus::Active,
            turn_seq: 1,
            turn_started_at: Some(now - 10.0),
            ..Default::default()
        };
        cell.lock()
            .unwrap()
            .apply(report(&epoch, 1, r.clone()), now)
            .unwrap();
        recompute_and_publish(&state, uid);
        flush_publishes(&state);
        assert!(matches!(
            rx.try_recv().unwrap(),
            ManifestDiff::Updated { .. }
        ));
        cell.lock()
            .unwrap()
            .apply(report(&epoch, 2, r.clone()), now + 0.1)
            .unwrap();
        recompute_and_publish(&state, uid);
        flush_publishes(&state);
        assert!(rx.try_recv().is_err(), "observation-only heartbeat");
        let mut next = r;
        next.turn_seq = 2;
        next.turn_started_at = Some(now - 1.0);
        cell.lock()
            .unwrap()
            .apply(report(&epoch, 3, next), now + 0.2)
            .unwrap();
        recompute_and_publish(&state, uid);
        flush_publishes(&state);
        assert!(
            rx.try_recv().is_ok(),
            "new turn while still working must publish"
        );
        {
            let mut c = cell.lock().unwrap();
            c.inputs.latest_start = Some(now - 1000.0);
            c.inputs.last_progress_at = Some(now - 1000.0);
        }
        recompute_and_publish(&state, uid);
        flush_publishes(&state);
        let ManifestDiff::Updated { entry, .. } = rx.try_recv().unwrap() else {
            panic!("updated");
        };
        assert!(entry["agent_state"]["stalled_since"].is_number());
    }

    #[test]
    fn sidecar_round_trip_preserves_state_and_rejects_replacement_or_other_boot() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = session_state();
        state.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        let uid = "ts-agent-state";
        // Whole seconds make the exact structural equality below independent
        // of sub-microsecond f64 rounding in JSON deserialization.
        let now = unix_now().floor();
        {
            let mut c = state.sessions[uid].agent_state.lock().unwrap();
            c.apply(hook(HookEvent::UserPromptSubmit, now - 20.0), now)
                .unwrap();
            c.apply(hook(HookEvent::Stop, now - 10.0), now).unwrap();
            c.inputs.background.update(
                Background {
                    complete: true,
                    crons: vec![Cron {
                        id: "cron".into(),
                        schedule: "* * * * *".into(),
                        recurring: true,
                    }],
                    ..Default::default()
                },
                now - 10.0,
            );
        }
        let before = current(&state.sessions[uid]);
        let state = Arc::new(Mutex::new(state));
        save_checked(&state).unwrap();
        let state = state.lock().unwrap();
        let identity = state.sessions[uid]
            .agent_state
            .lock()
            .unwrap()
            .child_start_time;
        *state.sessions[uid].agent_state.lock().unwrap() = StateCell::new(now, identity);
        assert_eq!(restore(&state).unwrap(), 1);
        let after = current(&state.sessions[uid]);
        assert_eq!(before, after);
        state.sessions[uid]
            .agent_state
            .lock()
            .unwrap()
            .child_start_time = Some(u64::MAX);
        assert_eq!(restore(&state).unwrap(), 0);
        state.sessions[uid]
            .agent_state
            .lock()
            .unwrap()
            .child_start_time = identity;
        let path = path(&state).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["boot_id"] = "another boot".into();
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(restore(&state).unwrap(), 0);
    }

    #[test]
    fn manifest_snapshot_and_health_expose_current_agent_state() {
        use crate::control::dispatch::{dispatch_request, DispatchOutcome};
        use crate::control::protocol::{Caller, Request};
        let state = Arc::new(Mutex::new(session_state()));
        let request = |method: &str| Request {
            id: "state-wire".into(),
            method: method.into(),
            caller: Caller::operator("op-default"),
            params: serde_json::json!({}),
        };
        let DispatchOutcome::ManifestWatchStream { handle, .. } =
            dispatch_request(&state, &request("manifest.watch"))
        else {
            panic!("manifest stream");
        };
        assert_eq!(
            handle.initial_snapshot["agent_states"]["ts-agent-state"]["state"],
            "starting"
        );
        assert_eq!(
            handle.initial_snapshot["agent_states"]["ts-agent-state"]["source"],
            "pty"
        );
        let DispatchOutcome::Done(response) = dispatch_request(&state, &request("daemon.health"))
        else {
            panic!("health reply");
        };
        let health = serde_json::to_value(response).unwrap();
        assert_eq!(health["result"]["sessions_by_state"]["starting"], 1);
        let now = unix_now();
        {
            let st = state.lock().unwrap();
            let mut c = st.sessions["ts-agent-state"].agent_state.lock().unwrap();
            c.apply(hook(HookEvent::UserPromptSubmit, now - 3.0), now)
                .unwrap();
            c.apply(hook(HookEvent::Stop, now - 2.0), now).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        state.lock().unwrap().daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        tick(&state).unwrap();
        let ManifestDiff::Updated { entry, .. } = handle.diff_rx.try_recv().unwrap() else {
            panic!("state diff");
        };
        assert_eq!(entry["agent_state"]["state"], "idle");
        assert_eq!(entry["agent_state"]["turn_seq"], 1);
        let stored: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.path().join("daemon-agent-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(stored["sessions"]["ts-agent-state"]["turn_seq"], 1);
    }

    #[test]
    fn fifty_sessions_seed_silently_and_coalesce_to_bounded_ticks() {
        let mut state = DaemonState::new();
        let now = unix_now();
        for n in 0..50 {
            let uid = format!("ts-state-burst-{n}");
            let mut params = crate::session::SpawnParams::new(&uid, "burst", "/bin/sleep");
            params.args = vec!["120".into()];
            let session = DaemonSession::spawn(params).unwrap();
            state.sessions.insert(uid, session);
        }
        let (rx, _guard) = state.manifest_watcher.subscribe();
        let state = Arc::new(Mutex::new(state));
        tick(&state).unwrap();
        assert!(
            rx.try_recv().is_err(),
            "first tick is a silent baseline for all 50 sessions"
        );
        assert_eq!(
            state
                .lock()
                .unwrap()
                .manifest_watcher
                .subscriber_slot_count(),
            1
        );
        {
            let st = state.lock().unwrap();
            for (uid, session) in &st.sessions {
                for round in 1..=3 {
                    session
                        .agent_state
                        .lock()
                        .unwrap()
                        .apply(
                            hook(HookEvent::UserPromptSubmit, now + round as f64 / 10.0),
                            now + 1.0,
                        )
                        .unwrap();
                    recompute_and_publish(&st, uid);
                }
            }
            assert!(
                rx.try_recv().is_err(),
                "RPC-shaped recomputations do not broadcast a burst"
            );
            let publisher = st.agent_state_runtime.publisher.lock().unwrap();
            assert_eq!(publisher.pending.len(), 50);
            assert_eq!(publisher.order.len(), 50, "one queue entry per UID");
        }
        let mut seen = BTreeSet::new();
        for _ in 0..7 {
            tick(&state).unwrap();
            let updates: Vec<_> = rx.try_iter().collect();
            assert!(updates.len() <= MAX_STATE_DIFFS_PER_TICK);
            for update in updates {
                let ManifestDiff::Updated { uid, entry } = update else {
                    panic!("state update");
                };
                assert_eq!(
                    entry["agent_state"]["turn_seq"], 3,
                    "only latest value is sent"
                );
                assert!(
                    seen.insert(uid),
                    "no duplicate emission without a state change"
                );
            }
            assert_eq!(
                state
                    .lock()
                    .unwrap()
                    .manifest_watcher
                    .subscriber_slot_count(),
                1
            );
        }
        assert_eq!(
            seen.len(),
            50,
            "bounded draining must also make progress for every session"
        );
    }

    #[test]
    fn sidecar_skips_pty_clock_churn_debounces_and_writes_outside_state_lock() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = session_state();
        st.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        let state = Arc::new(Mutex::new(st));
        let writes = std::cell::RefCell::new(Vec::new());
        let write = |_: &std::path::Path, json: &str| {
            assert!(
                state.try_lock().is_ok(),
                "fsync callback must not hold DaemonState"
            );
            writes.borrow_mut().push(json.to_string());
            Ok(())
        };
        let start = Instant::now();
        persist_with(&state, false, start, write).unwrap();
        for n in 1..=6 {
            *state.lock().unwrap().sessions["ts-agent-state"]
                .last_activity_at
                .lock()
                .unwrap() = Some(Instant::now());
            persist_with(&state, false, start + Duration::from_secs(n), write).unwrap();
        }
        assert_eq!(
            writes.borrow().len(),
            1,
            "PTY repaint clocks do not dirty restart facts"
        );
        let json: serde_json::Value = serde_json::from_str(&writes.borrow()[0]).unwrap();
        let record = &json["sessions"]["ts-agent-state"];
        for key in [
            "inputs",
            "previous",
            "legacy",
            "published",
            "last_progress_at",
        ] {
            assert!(
                record.get(key).is_none(),
                "transient {key} must not be persisted"
            );
        }
        state.lock().unwrap().sessions["ts-agent-state"]
            .input_handle()
            .stamp_activity();
        persist_with(&state, false, start + Duration::from_secs(7), write).unwrap();
        assert_eq!(
            writes.borrow().len(),
            1,
            "dirty record waits for 5-second debounce"
        );
        persist_with(&state, false, start + Duration::from_secs(10), write).unwrap();
        assert_eq!(writes.borrow().len(), 2);
        persist_with(&state, true, start + Duration::from_secs(11), write).unwrap();
        assert_eq!(
            writes.borrow().len(),
            3,
            "checked restart flush bypasses debounce"
        );
    }

    #[test]
    fn final_restart_write_follows_in_flight_periodic_write() {
        use std::sync::mpsc;
        let dir = tempfile::tempdir().unwrap();
        let mut st = session_state();
        st.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        let state = Arc::new(Mutex::new(st));
        let (writing_tx, writing_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let s = Arc::clone(&state);
        let writer = std::thread::spawn(move || {
            persist_with(&s, false, Instant::now(), |path, json| {
                writing_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                crate::state::write_json_atomic(path, json, true)
            })
        });
        writing_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        {
            let mut st = state.lock().unwrap();
            st.sessions["ts-agent-state"]
                .input_handle()
                .stamp_activity();
            st.restarting = true;
        }
        let (start_tx, start_rx) = mpsc::channel();
        let s = Arc::clone(&state);
        let finalizer = std::thread::spawn(move || {
            start_tx.send(()).unwrap();
            save_checked(&s)
        });
        start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while state.try_lock().is_err() {
            assert!(
                Instant::now() < deadline,
                "waiting final flush cannot monopolize DaemonState"
            );
            std::thread::yield_now();
        }
        release_tx.send(()).unwrap();
        writer.join().unwrap().unwrap();
        finalizer.join().unwrap().unwrap();
        let json: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.path().join("daemon-agent-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            json["sessions"]["ts-agent-state"]["turn_seq"], 1,
            "older captured snapshot must not overwrite the checked final state"
        );
    }

    #[test]
    fn report_history_and_free_text_are_bounded_without_losing_epoch_rejection() {
        let mut c = StateCell::new(0.0, Some(42));
        for n in 0..MAX_PROMPT_IDS + 10 {
            c.apply(
                Report::Hook {
                    event: HookEvent::UserPromptSubmit,
                    payload: HookPayload {
                        prompt_id: Some(format!("p-{n}")),
                        observed_at: Some(n as f64 + 1.0),
                        ..Default::default()
                    },
                },
                1000.0,
            )
            .unwrap();
        }
        assert_eq!(c.prompt_ids.len(), MAX_PROMPT_IDS);
        assert_eq!(c.prompt_ids.front().unwrap(), "p-10");
        assert!(
            !c.apply(hook(HookEvent::UserPromptSubmit, 1.0), 1000.0)
                .unwrap()
                .accepted
        );
        let first = uuid::Uuid::new_v4().to_string();
        c.apply(report(&first, 1, RelaySnapshot::default()), 1000.0)
            .unwrap();
        for _ in 0..MAX_RETIRED_EPOCHS {
            c.apply(
                report(
                    &uuid::Uuid::new_v4().to_string(),
                    1,
                    RelaySnapshot::default(),
                ),
                1000.0,
            )
            .unwrap();
        }
        let before = serde_json::to_value(&c).unwrap();
        assert!(c
            .apply(
                report(
                    &uuid::Uuid::new_v4().to_string(),
                    1,
                    RelaySnapshot::default()
                ),
                1000.0
            )
            .is_err());
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            before,
            "overflow is atomic"
        );
        assert!(
            !c.apply(report(&first, u64::MAX, RelaySnapshot::default()), 1000.0)
                .unwrap()
                .accepted
        );
        for oversized in [
            Report::Hook {
                event: HookEvent::StopFailure,
                payload: HookPayload {
                    error_kind: Some("x".repeat(MAX_TEXT_BYTES + 1)),
                    ..Default::default()
                },
            },
            Report::Hook {
                event: HookEvent::UserPromptSubmit,
                payload: HookPayload {
                    prompt_id: Some("x".repeat(MAX_ID_BYTES + 1)),
                    ..Default::default()
                },
            },
        ] {
            assert!(c.apply(oversized, 1000.0).is_err());
            assert_eq!(serde_json::to_value(&c).unwrap(), before);
        }
    }

    #[test]
    fn invalid_report_is_atomic_and_missing_foreground_is_rejected() {
        let mut c = StateCell::new(0.0, Some(42));
        let epoch = uuid::Uuid::new_v4().to_string();
        let before = serde_json::to_value(&c).unwrap();
        let r = RelaySnapshot {
            turn_seq: 100,
            turn_started_at: Some(2000.0),
            ..Default::default()
        };
        assert!(c.apply(report(&epoch, 1, r), 1000.0).is_err());
        assert_eq!(serde_json::to_value(&c).unwrap(), before);
        assert!(serde_json::from_value::<Report>(serde_json::json!({
            "kind": "snapshot", "epoch": epoch, "seq": 1, "snapshot": {"backend_connected": true}
        }))
        .is_err());
    }
}
