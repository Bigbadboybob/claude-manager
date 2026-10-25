//! The org model as the TUI reads it (doc/org-model.md S2/S3).
//!
//! Members and teams come from the daemon's `org.snapshot` (the Operator
//! caller is Owner), local daemon first, then the other hosts, exactly like
//! `board.read`. The snapshot is polled with `since_version` only while a
//! screen that needs it is open (F6, the roster, the Team sub-view, the place
//! dialog); nothing polls it otherwise (no periodic full polls, CLAUDE.md).
//!
//! Every org and S3 feature sits behind a feature check: an old daemon
//! answers `UnknownMethod`, and a daemon in front of an old planning API
//! answers `unsupported` or a bare route-miss `Not Found`. Either turns the
//! feature off quietly and the screens fall back to today's views; an off
//! feature is asked again after `OFF_RETRY`.
use super::*;
use crate::client_session::DaemonRpcError;
use cm_daemon::control::protocol::ErrorCode;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::mpsc;

const POLL: Duration = Duration::from_secs(5);
/// Session links (main, occupancy) can move without the org version, so the
/// poll also re-reads the whole snapshot this often.
const FULL_READ: Duration = Duration::from_secs(30);
const ERROR_RETRY: Duration = Duration::from_secs(15);
/// An unsupported feature is asked again this long after it was turned off.
pub(super) const OFF_RETRY: Duration = Duration::from_secs(300);

/// Whether the daemon and API behind it serve a feature.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Support {
    /// Not asked yet: screens draw today's views until the first answer.
    #[default]
    Unknown,
    On,
    Off,
}

/// A failed owner RPC: `unsupported` when every reachable daemon said the
/// method or its API route doesn't exist.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct RpcFail {
    pub unsupported: bool,
    pub message: String,
}

/// Whether `err` says the method (or the API route behind it) doesn't exist:
/// an old daemon (`UnknownMethod`), a daemon's own `unsupported`, or a
/// FastAPI route miss proxied as `NotFound` with the bare default detail.
pub(super) fn is_unsupported(err: &anyhow::Error) -> bool {
    err.chain().any(|c| {
        c.downcast_ref::<DaemonRpcError>().is_some_and(|e| {
            e.code == ErrorCode::UnknownMethod
                || e.message.contains("unsupported")
                || (e.code == ErrorCode::NotFound && e.message.trim() == "Not Found")
        })
    })
}

/// One owner RPC across the reachable daemons, local first; the first answer
/// wins. Blocking: call it on a worker thread.
pub(super) fn owner_rpc(targets: &[(String, PathBuf, String)], method: &str, params: &Value) -> Result<Value, RpcFail> {
    let mut errors = Vec::new();
    let mut unsupported = !targets.is_empty();
    for (host, socket, token) in targets {
        match crate::client_session::rpc_messaging_board(socket, token, method, params.clone()) {
            Ok(v) => return Ok(v),
            Err(e) => {
                unsupported &= is_unsupported(&e);
                errors.push(format!("{host}: {e}"));
            }
        }
    }
    let message = if errors.is_empty() { "No daemon is reachable".to_owned() } else { errors.join("; ") };
    Err(RpcFail { unsupported, message })
}

/// Run `owner_rpc` on a thread; the reply arrives on the returned channel.
pub(super) fn spawn_owner_rpc(targets: Vec<(String, PathBuf, String)>, method: &'static str, params: Value) -> mpsc::Receiver<Result<Value, RpcFail>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(owner_rpc(&targets, method, &params));
    });
    rx
}

// ---- shapes (org-tui-shapes, agreed with #13 and #14) ----------------------

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct MainSession {
    pub pid: String,
    pub daemon_id: Option<String>,
    pub session_uid: Option<String>,
    pub occupancy_generation: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct HomeWorkspace {
    pub daemon_id: Option<String>,
    pub workspace_id: Option<String>,
    pub path: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct Member {
    pub id: String,
    pub org_id: Option<String>,
    pub handle: String,
    pub aliases: Vec<String>,
    /// `agent`, `continuous` or `owner`.
    pub kind: String,
    /// `active`, `vacant`, `suspended` or `retired`.
    pub status: String,
    pub title: Option<String>,
    pub mandate: Option<String>,
    /// Derived focus (S5); null until then.
    pub focus: Option<String>,
    pub focus_note: Option<String>,
    pub focus_pinned: Option<String>,
    pub manager_member_id: Option<String>,
    pub primary_team_id: Option<String>,
    pub main: Option<MainSession>,
    /// Set only while vacant: the deputy, else the manager.
    pub stand_in: Option<String>,
    pub home_workspace: Option<HomeWorkspace>,
    pub revision: i64,
}

impl Member {
    pub(super) fn is_owner(&self) -> bool {
        self.kind == "owner"
    }

    pub(super) fn vacant(&self) -> bool {
        self.status == "vacant"
    }

    pub(super) fn retired(&self) -> bool {
        self.status == "retired"
    }

    /// What the member is on now: the derived focus once S5 lands, else the
    /// pinned focus, else the free-text note.
    pub(super) fn focus_text(&self) -> Option<&str> {
        self.focus.as_deref().or(self.focus_pinned.as_deref()).or(self.focus_note.as_deref()).filter(|s| !s.is_empty())
    }

    pub(super) fn main_uid(&self) -> Option<&str> {
        self.main.as_ref()?.session_uid.as_deref()
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct TeamMembership {
    pub member_id: String,
    /// `primary`, `guest` or `deputy`.
    pub role: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct Team {
    pub id: String,
    pub slug: String,
    pub name: String,
    /// `division`, `team` or `initiative`.
    pub kind: String,
    /// `active`, `standby` or `archived`.
    pub status: String,
    pub parent_team_id: Option<String>,
    pub lead_member_id: Option<String>,
    pub board: Option<String>,
    pub channel: Option<String>,
    pub members: Vec<TeamMembership>,
    pub revision: i64,
}

impl Team {
    pub(super) fn standby(&self) -> bool {
        self.status == "standby"
    }

    pub(super) fn archived(&self) -> bool {
        self.status == "archived"
    }

    pub(super) fn role_of(&self, member_id: &str) -> Option<&str> {
        self.members.iter().find(|m| m.member_id == member_id).map(|m| m.role.as_str())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct OrgSnapshot {
    pub version: i64,
    pub members: Vec<Member>,
    pub teams: Vec<Team>,
}

impl OrgSnapshot {
    pub(super) fn member(&self, id: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    pub(super) fn team(&self, id: &str) -> Option<&Team> {
        self.teams.iter().find(|t| t.id == id)
    }

    /// A member by id, handle or alias (`owner` names Owner).
    pub(super) fn resolve_member(&self, name: &str) -> Option<&Member> {
        let name = name.trim().trim_start_matches('@');
        if name.eq_ignore_ascii_case("owner") {
            return self.owner();
        }
        self.member(name).or_else(|| {
            self.members.iter().find(|m| m.handle.eq_ignore_ascii_case(name) || m.aliases.iter().any(|a| a.eq_ignore_ascii_case(name)))
        })
    }

    pub(super) fn owner(&self) -> Option<&Member> {
        self.members.iter().find(|m| m.is_owner())
    }

    /// A team by id or slug.
    pub(super) fn resolve_team(&self, name: &str) -> Option<&Team> {
        let name = name.trim();
        self.team(name).or_else(|| self.teams.iter().find(|t| t.slug.eq_ignore_ascii_case(name) || t.name.eq_ignore_ascii_case(name)))
    }

    /// `handle`, or a short id when the member isn't in the snapshot.
    pub(super) fn handle(&self, id: &str) -> String {
        match self.member(id) {
            Some(m) if m.is_owner() => "Owner".into(),
            Some(m) => m.handle.clone(),
            None => id.chars().take(8).collect(),
        }
    }

    /// Whether any member other than Owner exists: until then F6 and the
    /// sidebar keep today's task views.
    pub(super) fn has_members(&self) -> bool {
        self.members.iter().any(|m| !m.is_owner() && !m.retired())
    }

    /// The member whose main session is `uid`.
    pub(super) fn member_by_main(&self, uid: &str) -> Option<&Member> {
        self.members.iter().find(|m| !m.retired() && m.main_uid() == Some(uid))
    }
}

/// One proposed member on the roster (`org.roster`).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct Proposal {
    /// A stable source key (`continuous:<daemon>:<id>`, `lead:<initiative>`,
    /// `lane:<pid>`), regenerated on every `org.roster`.
    pub proposal_id: String,
    pub handle: String,
    pub kind: String,
    pub title: Option<String>,
    pub team_id: Option<String>,
    /// A member id, handle or `owner`.
    pub manager: Option<String>,
    pub home_workspace: Option<HomeWorkspace>,
    pub pids: Vec<String>,
    /// `continuous`, `lead` or `lane`.
    pub source: String,
    pub conflicts: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct RosterReply {
    pub proposals: Vec<Proposal>,
    pub declined: Vec<String>,
    pub last_applied_at: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct RosterRejection {
    pub proposal_id: String,
    /// `stale`, `handle_taken`, `cycle`, `pid_already_member` or
    /// `unknown_manager`.
    pub code: String,
    pub message: String,
}

// ---- state -----------------------------------------------------------------

#[derive(Default)]
pub(super) struct OrgState {
    pub(super) support: Support,
    pub(super) snapshot: Option<OrgSnapshot>,
    rx: Option<mpsc::Receiver<Result<Value, RpcFail>>>,
    last_poll: Option<Instant>,
    last_full: Option<Instant>,
    retry_at: Option<Instant>,
    pub(super) error: String,
}

impl OrgState {
    /// The snapshot when the org model is live and has members.
    pub(super) fn live(&self) -> Option<&OrgSnapshot> {
        (self.support == Support::On).then_some(self.snapshot.as_ref()).flatten().filter(|s| s.has_members())
    }

    /// Apply an `org.snapshot` reply: `{unchanged}` keeps the current one.
    pub(super) fn accept(&mut self, v: Value) {
        self.support = Support::On;
        self.error.clear();
        if v["unchanged"] == true && self.snapshot.is_some() {
            return;
        }
        match serde_json::from_value::<OrgSnapshot>(v) {
            Ok(s) => self.snapshot = Some(s),
            Err(e) => self.error = format!("org.snapshot: {e}"),
        }
    }

    pub(super) fn fail(&mut self, f: RpcFail) {
        if f.unsupported {
            self.support = Support::Off;
            self.snapshot = None;
            self.retry_at = Some(Instant::now() + OFF_RETRY);
        } else {
            self.error = f.message;
            self.retry_at = Some(Instant::now() + ERROR_RETRY);
        }
    }
}

impl App {
    /// Whether a screen that draws the org model is open.
    pub(super) fn org_wanted(&self) -> bool {
        self.orgchart.visible || self.roster.visible || self.team_view_shown() || self.planning_place_open()
    }

    /// The Team sub-view is showing (it polls the org model).
    pub(super) fn team_view_shown(&self) -> bool {
        false
    }

    /// The live org snapshot (support on, members present), else `None`:
    /// every screen falls back to today's view on `None`.
    pub(super) fn org_live(&self) -> Option<&OrgSnapshot> {
        self.org.live()
    }

    /// Ask for the snapshot now (a screen just opened).
    pub(super) fn org_refresh_soon(&mut self) {
        self.org.last_poll = None;
        if self.org.support == Support::Off && self.org.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        self.org.retry_at = None;
    }

    /// Replies, and the visible-only poll.
    pub fn org_tick(&mut self) {
        if let Some(r) = self.org.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.org.rx = None;
            self.needs_redraw = true;
            match r {
                Ok(v) => self.org.accept(v),
                Err(f) => self.org.fail(f),
            }
        }
        if !self.org_wanted() || self.org.rx.is_some() {
            return;
        }
        if self.org.retry_at.is_some_and(|t| Instant::now() < t) || self.org.last_poll.is_some_and(|t| t.elapsed() < POLL) {
            return;
        }
        let mut params = json!({});
        let full_due = self.org.last_full.is_none_or(|t| t.elapsed() >= FULL_READ);
        match (&self.org.snapshot, full_due) {
            (Some(s), false) => params["since_version"] = json!(s.version),
            _ => self.org.last_full = Some(Instant::now()),
        }
        self.org.last_poll = Some(Instant::now());
        self.org.retry_at = None;
        let targets = self.owner_rpc_targets();
        if targets.is_empty() {
            // Tests, or no daemon yet: nothing to ask.
            self.org.retry_at = Some(Instant::now() + ERROR_RETRY);
            return;
        }
        self.org.rx = Some(spawn_owner_rpc(targets, "org.snapshot", params));
    }
}

#[cfg(test)]
pub(super) mod fixtures {
    //! JSON in the agreed shapes: a shape change is a fixture change.
    use serde_json::{json, Value};

    pub(crate) fn member(id: &str, handle: &str, manager: Option<&str>, team: Option<&str>, main: Option<&str>) -> Value {
        json!({
            "id": id, "org_id": "org-1", "handle": handle, "aliases": [],
            "kind": "agent", "status": "active", "title": format!("{handle} title"),
            "mandate": format!("{handle} mandate"), "focus": null,
            "focus_note": null, "focus_pinned": null,
            "manager_member_id": manager, "primary_team_id": team,
            "main": main.map(|uid| json!({"pid": format!("agent:d1:{uid}"), "daemon_id": "d1", "session_uid": uid, "occupancy_generation": 1})),
            "stand_in": null, "home_workspace": null, "revision": 1,
        })
    }

    pub(crate) fn owner() -> Value {
        let mut o = member("mem-owner", "owner", None, None, None);
        o["kind"] = json!("owner");
        o
    }

    pub(crate) fn team(id: &str, name: &str, parent: Option<&str>, lead: Option<&str>, members: &[(&str, &str)]) -> Value {
        json!({
            "id": id, "slug": name.to_lowercase().replace(' ', "-"), "name": name,
            "kind": "team", "status": "active", "parent_team_id": parent,
            "lead_member_id": lead, "board": null, "channel": null,
            "members": members.iter().map(|(m, r)| json!({"member_id": m, "role": r})).collect::<Vec<_>>(),
            "revision": 1,
        })
    }

    pub(crate) fn snapshot(version: i64, members: Vec<Value>, teams: Vec<Value>) -> Value {
        json!({"version": version, "members": members, "teams": teams})
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn rpc_err(code: ErrorCode, message: &str) -> anyhow::Error {
        anyhow::Error::new(DaemonRpcError { method: "org.snapshot".into(), code, message: message.into() })
    }

    #[test]
    fn old_daemons_and_apis_read_as_unsupported() {
        assert!(is_unsupported(&rpc_err(ErrorCode::UnknownMethod, "unknown method: org.snapshot")));
        assert!(is_unsupported(&rpc_err(ErrorCode::Internal, "org.snapshot unsupported by this planning API")));
        // A FastAPI route miss, proxied by the daemon.
        assert!(is_unsupported(&rpc_err(ErrorCode::NotFound, "Not Found")));
        // A real not-found (a member or board that isn't there) is not.
        assert!(!is_unsupported(&rpc_err(ErrorCode::NotFound, "not_found: no member mem-9")));
        assert!(!is_unsupported(&rpc_err(ErrorCode::Unauthorized, "operator token does not match")));
        assert!(!is_unsupported(&anyhow::anyhow!("connect: connection refused")));
        // Wrapped in context, as the RPC helpers do.
        assert!(is_unsupported(&rpc_err(ErrorCode::UnknownMethod, "x").context("org.snapshot")));
    }

    #[test]
    fn no_daemon_is_an_error_not_unsupported() {
        let f = owner_rpc(&[], "org.snapshot", &json!({})).unwrap_err();
        assert!(!f.unsupported);
        assert_eq!(f.message, "No daemon is reachable");
    }

    #[test]
    fn snapshot_parses_the_agreed_shape_and_unchanged_keeps_it() {
        let mut st = OrgState::default();
        assert!(st.live().is_none());
        st.accept(snapshot(7, vec![owner(), member("m1", "ep", Some("mem-owner"), Some("t1"), Some("ts-1-0"))], vec![team("t1", "Swarm", None, Some("m1"), &[("m1", "primary")])]));
        assert_eq!(st.support, Support::On);
        let s = st.live().expect("members present");
        assert_eq!(s.version, 7);
        assert_eq!(s.member("m1").unwrap().main_uid(), Some("ts-1-0"));
        assert_eq!(s.member_by_main("ts-1-0").unwrap().handle, "ep");
        assert_eq!(s.resolve_member("@EP").unwrap().id, "m1");
        assert_eq!(s.resolve_member("owner").unwrap().id, "mem-owner");
        assert_eq!(s.resolve_team("swarm").unwrap().id, "t1");
        assert_eq!(s.team("t1").unwrap().role_of("m1"), Some("primary"));
        st.accept(json!({"unchanged": true, "version": 7}));
        assert_eq!(st.live().unwrap().version, 7);
    }

    #[test]
    fn unknown_fields_and_missing_ones_parse() {
        let mut st = OrgState::default();
        st.accept(json!({"version": 1, "members": [{"id": "m1", "handle": "x", "kind": "agent", "status": "active", "later_field": 3}], "teams": [{"id": "t", "name": "T"}]}));
        let s = st.live().unwrap();
        assert!(s.member("m1").unwrap().main.is_none());
        assert_eq!(s.team("t").unwrap().status, "");
    }

    #[test]
    fn owner_only_snapshot_is_not_live() {
        let mut st = OrgState::default();
        st.accept(snapshot(1, vec![owner()], vec![]));
        assert_eq!(st.support, Support::On);
        assert!(st.live().is_none(), "F6 keeps the task chart until members exist");
    }

    #[test]
    fn unsupported_turns_off_and_drops_the_snapshot() {
        let mut st = OrgState::default();
        st.accept(snapshot(1, vec![owner(), member("m1", "ep", None, None, None)], vec![]));
        st.fail(RpcFail { unsupported: true, message: "old".into() });
        assert_eq!(st.support, Support::Off);
        assert!(st.live().is_none());
        assert!(st.error.is_empty());
        // A transient error keeps support and the last snapshot.
        let mut st = OrgState::default();
        st.accept(snapshot(1, vec![owner(), member("m1", "ep", None, None, None)], vec![]));
        st.fail(RpcFail { unsupported: false, message: "timeout".into() });
        assert!(st.live().is_some());
        assert_eq!(st.error, "timeout");
    }

    #[test]
    fn focus_prefers_derived_then_pinned_then_note() {
        let mut m: Member = serde_json::from_value(member("m", "x", None, None, None)).unwrap();
        assert_eq!(m.focus_text(), None);
        m.focus_note = Some("note".into());
        assert_eq!(m.focus_text(), Some("note"));
        m.focus_pinned = Some("pinned".into());
        assert_eq!(m.focus_text(), Some("pinned"));
        m.focus = Some("derived".into());
        assert_eq!(m.focus_text(), Some("derived"));
    }
}
