//! "Finishing work never kills an employee" (doc/org-model.md §7.1, S3).
//!
//! Two protections, advertised to the planning API as `lifecycle_guard_v1`:
//!
//! * A **board-originated close** (the status mirror writes `done` with
//!   `metadata.terminal_via = "item"`) never arms the terminal sweep against
//!   non-ephemeral sessions, whatever their workspace kind. Until S0b records
//!   workspace kinds every workspace is *unclassified*, so such a close sweeps
//!   nothing: the task's sessions are only unassigned.
//! * **Every teardown path protects members**: the periodic sweep and
//!   `mark_subtask_done` skip a member's main session and never remove a
//!   `home`, `stream` or pinned workspace. Ephemeral and unclassified
//!   workspaces keep today's teardown.
//!
//! The API turns central status mirroring on for a task only when every
//! daemon holding its sessions advertises the capability, so a host without
//! this guard never sees a mirrored `done` for a task it holds sessions of.

use crate::manifest::ManifestWorkspace;
use crate::session::DaemonSession;

/// Advertised in every board heartbeat (`capabilities`).
pub const CAPABILITY: &str = "lifecycle_guard_v1";

/// What a task's close says about who closed it.
pub const TERMINAL_VIA_ITEM: &str = "item";

/// A workspace's kind (doc/org-model.md §5). S0b records it; until then every
/// workspace on a host is `Unclassified`, which keeps today's teardown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceKind {
    Unclassified,
    Ephemeral,
    Home,
    Stream,
}

/// The kind this host knows for `ws`. No record carries one yet (S0b adds the
/// `org.workspace_kind` field), so this is `Unclassified` everywhere.
pub fn workspace_kind(_ws: &ManifestWorkspace) -> WorkspaceKind {
    WorkspaceKind::Unclassified
}

/// Why a workspace must never be removed by a teardown, if it must not.
pub fn protected_workspace(ws: &ManifestWorkspace) -> Option<&'static str> {
    if ws.pinned {
        return Some("pinned");
    }
    match workspace_kind(ws) {
        WorkspaceKind::Home => Some("home"),
        WorkspaceKind::Stream => Some("stream"),
        WorkspaceKind::Ephemeral | WorkspaceKind::Unclassified => None,
    }
}

/// True when `sess` is a member's main session, by org S2's member cache
/// (refreshed every board heartbeat; `acting_as: "main"`). A session with no
/// member binding is not protected. The cache is a leaf lock, safe to read
/// under the daemon state lock.
pub fn is_member_main(sess: &DaemonSession) -> bool {
    crate::items::org::cached_member(&sess.uid)["acting_as"] == "main"
}

/// Whether the terminal sweep may close `sess` for a task whose close was
/// board-originated: only an ephemeral workspace's sessions.
pub fn sweepable_after_item_close(ws: Option<&ManifestWorkspace>) -> bool {
    ws.is_some_and(|ws| workspace_kind(ws) == WorkspaceKind::Ephemeral)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(pinned: bool) -> ManifestWorkspace {
        ManifestWorkspace { id: "w".into(), pinned, ..Default::default() }
    }

    #[test]
    fn pinned_workspaces_are_protected_and_unclassified_ones_keep_todays_teardown() {
        assert_eq!(protected_workspace(&ws(true)), Some("pinned"));
        assert_eq!(protected_workspace(&ws(false)), None);
        assert_eq!(workspace_kind(&ws(false)), WorkspaceKind::Unclassified);
    }

    #[test]
    fn an_item_close_sweeps_only_ephemeral_workspaces() {
        // Nothing is classified ephemeral before S0b, so nothing is swept.
        assert!(!sweepable_after_item_close(Some(&ws(false))));
        assert!(!sweepable_after_item_close(None));
    }
}
