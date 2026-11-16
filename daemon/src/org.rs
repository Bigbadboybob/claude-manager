//! Org hierarchy rules shared by the daemon and the TUI.
//!
//! Reporting lines follow the task tree (`parent_task_id`); a division is an
//! initiative with `kind = 'division'`. The one rule both sides must agree on
//! is which session LEADS a task — the TUI draws the ★ on it and the `org`
//! MCP tool reports it — so it lives here once and the TUI calls it directly
//! (the TUI already depends on this crate).
//!
//! Owner can lead too (2026-10-22): `metadata.lead_session_uid = "owner"`
//! names Owner explicitly, and a division root with no live lead falls back
//! to Owner as stand-in. [`task_lead`] is the full rule, typed so callers can
//! tell an explicit Owner from the fallback.
//!
//! Reporting is not approval authority: nothing here grants control rights.

use serde_json::Value;

/// `tasks.metadata` key holding the explicitly chosen lead session uid
/// (set from the task header's A-e form or `org(action="set_lead")`).
pub const LEAD_SESSION_METADATA_KEY: &str = "lead_session_uid";

/// The `lead_session_uid` value meaning "Owner leads this task".
pub const OWNER_LEAD: &str = "owner";

/// Whether a pin names Owner (case-insensitive, trimmed).
pub fn is_owner_pin(pinned: &str) -> bool {
    pinned.trim().eq_ignore_ascii_case(OWNER_LEAD)
}

/// Who leads a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lead {
    /// A live agent session.
    Session(String),
    /// Owner: pinned explicitly (`stand_in: false`), or standing in for a
    /// division root that has no live lead (`stand_in: true`).
    Owner { stand_in: bool },
    /// Nobody (a non-division task with no live agent session).
    None,
}

impl Lead {
    pub fn session(&self) -> Option<&str> {
        match self {
            Lead::Session(uid) => Some(uid),
            _ => None,
        }
    }

    pub fn is_owner(&self) -> bool {
        matches!(self, Lead::Owner { .. })
    }
}

/// One session bound to the task whose lead is being resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeadCandidate<'a> {
    pub uid: &'a str,
    /// Engine / session type: `claude`, `claude-code`, `codex`, `bash`, …
    pub engine: &'a str,
    /// Exited sessions are never the lead.
    pub live: bool,
}

/// Engines that can lead: agent sessions, never a bash shell. Unknown
/// engines are not agents, so a new shell-like kind can't silently lead.
pub fn is_agent_engine(engine: &str) -> bool {
    matches!(
        engine.trim().to_ascii_lowercase().as_str(),
        "claude" | "claude-code" | "codex"
    )
}

/// The pinned lead from a task's `metadata` bag, if any. Absent, null, or
/// blank means "use the fallback".
pub fn pinned_lead(metadata: &Value) -> Option<&str> {
    metadata
        .get(LEAD_SESSION_METADATA_KEY)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|uid| !uid.is_empty())
}

/// Spawn order key for a session uid. Both sides mint
/// `ts-<spawn nanos hex>-<counter hex>`, and A-R keeps the uid, so the uid
/// is a restart-stable spawn order. Unparseable uids sort after every
/// parseable one (then by the uid string). The TUI's A-e Lead field lists
/// its choices in this order.
pub fn spawn_order(uid: &str) -> (u8, u128, u64) {
    let parsed = uid.strip_prefix("ts-").and_then(|rest| {
        let (nanos, counter) = rest.split_once('-')?;
        Some((
            u128::from_str_radix(nanos, 16).ok()?,
            u64::from_str_radix(counter, 16).ok()?,
        ))
    });
    match parsed {
        Some((nanos, counter)) => (0, nanos, counter),
        None => (1, 0, 0),
    }
}

/// The lead session of one task.
///
/// `candidates` are the sessions bound to that task. The rule:
///   1. the `pinned` uid, if it is a live agent session among the
///      candidates;
///   2. otherwise the earliest-spawned live agent (non-bash) session;
///   3. otherwise none. Exited sessions never lead.
pub fn lead_session_uid(pinned: Option<&str>, candidates: &[LeadCandidate<'_>]) -> Option<String> {
    let eligible = || {
        candidates
            .iter()
            .filter(|c| c.live && is_agent_engine(c.engine))
    };
    if let Some(pinned) = pinned.map(str::trim).filter(|p| !p.is_empty()) {
        if eligible().any(|c| c.uid == pinned) {
            return Some(pinned.to_string());
        }
    }
    eligible()
        .min_by(|a, b| {
            spawn_order(a.uid)
                .cmp(&spawn_order(b.uid))
                .then_with(|| a.uid.cmp(b.uid))
        })
        .map(|c| c.uid.to_string())
}

/// The lead of one task, Owner included:
///   1. a pin of `"owner"` → Owner (explicit);
///   2. else the session rule ([`lead_session_uid`]);
///   3. else, for a division root (the `coordinator_task_id` of an
///      initiative with `kind = 'division'`), Owner as stand-in;
///   4. else nobody.
pub fn task_lead(pinned: Option<&str>, candidates: &[LeadCandidate<'_>], division_root: bool) -> Lead {
    if pinned.is_some_and(is_owner_pin) {
        return Lead::Owner { stand_in: false };
    }
    match lead_session_uid(pinned, candidates) {
        Some(uid) => Lead::Session(uid),
        None if division_root => Lead::Owner { stand_in: true },
        None => Lead::None,
    }
}

/// Whether a task row (the planning API's shape: `id`, and an embedded
/// `initiative` with `kind` and `coordinator_task_id`) is a division root.
pub fn is_division_root(task: &Value) -> bool {
    let init = &task["initiative"];
    init["kind"].as_str() == Some("division")
        && init["coordinator_task_id"].as_str().is_some()
        && init["coordinator_task_id"].as_str() == task["id"].as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn c<'a>(uid: &'a str, engine: &'a str, live: bool) -> LeadCandidate<'a> {
        LeadCandidate { uid, engine, live }
    }

    /// Pins the shared lead rule (PLAN.md "Lead session of a task"). The TUI
    /// renders ★ from this same function; change both only together.
    #[test]
    fn lead_rule_is_pinned() {
        let early = "ts-100-1";
        let late = "ts-200-0";
        // No sessions → no lead.
        assert_eq!(lead_session_uid(None, &[]), None);
        // Only (live) bash sessions → no lead.
        assert_eq!(lead_session_uid(None, &[c(early, "bash", true)]), None);
        // claude, claude-code and codex all count.
        for engine in ["claude", "claude-code", "codex"] {
            assert_eq!(lead_session_uid(None, &[c(early, engine, true)]), Some(early.into()));
        }
        // A uid with no counter is unparseable and sorts last.
        assert_eq!(
            lead_session_uid(None, &[c("ts-zz", "codex", true), c("ts-ff-0", "codex", true)]),
            Some("ts-ff-0".into())
        );
        // Numeric nanos: 0x9 before 0x10.
        assert_eq!(
            lead_session_uid(None, &[c("ts-10-0", "codex", true), c("ts-9-0", "codex", true)]),
            Some("ts-9-0".into())
        );
        // Earliest-spawned live agent, regardless of list order.
        assert_eq!(
            lead_session_uid(None, &[c(late, "codex", true), c(early, "claude", true)]),
            Some(early.into())
        );
        // Counter breaks a nanos tie.
        assert_eq!(
            lead_session_uid(None, &[c("ts-100-2", "codex", true), c("ts-100-1", "codex", true)]),
            Some("ts-100-1".into())
        );
        // Hex, not lexical: 0xff00 > 0xfff.
        assert_eq!(
            lead_session_uid(None, &[c("ts-ff00-0", "codex", true), c("ts-fff-0", "codex", true)]),
            Some("ts-fff-0".into())
        );
        // Bash never leads, even when earliest; nor do unknown engines.
        assert_eq!(
            lead_session_uid(None, &[c(early, "bash", true), c(late, "claude-code", true)]),
            Some(late.into())
        );
        assert_eq!(lead_session_uid(None, &[c(early, "shell", true)]), None);
        // Exited sessions never lead.
        assert_eq!(
            lead_session_uid(None, &[c(early, "claude", false), c(late, "codex", true)]),
            Some(late.into())
        );
        assert_eq!(lead_session_uid(None, &[c(early, "claude", false)]), None);
        // A live pinned agent wins over spawn order.
        assert_eq!(
            lead_session_uid(Some(late), &[c(early, "claude", true), c(late, "codex", true)]),
            Some(late.into())
        );
        // A pinned uid that exited, is bash, or isn't bound to the task falls back.
        assert_eq!(
            lead_session_uid(Some(late), &[c(early, "claude", true), c(late, "codex", false)]),
            Some(early.into())
        );
        assert_eq!(
            lead_session_uid(Some(late), &[c(early, "claude", true), c(late, "bash", true)]),
            Some(early.into())
        );
        assert_eq!(
            lead_session_uid(Some("ts-999-0"), &[c(late, "codex", true)]),
            Some(late.into())
        );
        // Blank pin = unset.
        assert_eq!(
            lead_session_uid(Some("  "), &[c(late, "codex", true)]),
            Some(late.into())
        );
        // Unparseable uids sort after parseable ones, then by string.
        assert_eq!(
            lead_session_uid(None, &[c("legacy-b", "codex", true), c(late, "codex", true)]),
            Some(late.into())
        );
        assert_eq!(
            lead_session_uid(None, &[c("legacy-b", "codex", true), c("legacy-a", "codex", true)]),
            Some("legacy-a".into())
        );
    }

    /// Owner as lead (2026-10-22): explicit pin, stand-in for division roots
    /// only, and a live session overriding the stand-in.
    #[test]
    fn owner_leads_when_pinned_or_standing_in_for_a_division_root() {
        let live = [c("ts-10-0", "codex", true)];
        let bash = [c("ts-10-0", "bash", true)];
        // Explicit Owner wins over live sessions, in any case and spacing.
        for pin in ["owner", " Owner ", "OWNER"] {
            assert_eq!(task_lead(Some(pin), &live, false), Lead::Owner { stand_in: false }, "{pin:?}");
        }
        assert_eq!(task_lead(Some("owner"), &[], true), Lead::Owner { stand_in: false });
        // A division root with no live agent: Owner stands in.
        assert_eq!(task_lead(None, &[], true), Lead::Owner { stand_in: true });
        assert_eq!(task_lead(None, &bash, true), Lead::Owner { stand_in: true });
        assert_eq!(task_lead(Some("ts-gone-0"), &[], true), Lead::Owner { stand_in: true });
        // ...but a live session overrides the fallback.
        assert_eq!(task_lead(None, &live, true), Lead::Session("ts-10-0".into()));
        // Any other task with no lead keeps having none.
        assert_eq!(task_lead(None, &[], false), Lead::None);
        assert_eq!(task_lead(None, &bash, false), Lead::None);
        assert_eq!(task_lead(None, &live, false), Lead::Session("ts-10-0".into()));
        // The session rule itself never treats "owner" as a session.
        assert_eq!(lead_session_uid(Some("owner"), &live), Some("ts-10-0".into()));
        // Division roots come from the task row.
        let root = json!({"id": "t1", "initiative": {"kind": "division", "coordinator_task_id": "t1"}});
        let member = json!({"id": "t2", "initiative": {"kind": "division", "coordinator_task_id": "t1"}});
        let plain = json!({"id": "t1", "initiative": {"kind": "initiative", "coordinator_task_id": "t1"}});
        assert!(is_division_root(&root));
        assert!(!is_division_root(&member) && !is_division_root(&plain) && !is_division_root(&json!({"id": "t1"})));
    }

    #[test]
    fn pinned_lead_reads_metadata() {
        assert_eq!(pinned_lead(&json!({"lead_session_uid": "ts-1-2"})), Some("ts-1-2"));
        assert_eq!(pinned_lead(&json!({"lead_session_uid": null})), None);
        assert_eq!(pinned_lead(&json!({"lead_session_uid": ""})), None);
        assert_eq!(pinned_lead(&json!({})), None);
        assert_eq!(pinned_lead(&Value::Null), None);
    }
}
