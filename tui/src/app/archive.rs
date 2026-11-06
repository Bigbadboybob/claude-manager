//! Cold storage for long-closed workspaces, and bounds for per-workspace
//! state that otherwise only grows (#109).
//!
//! Every closed workspace used to stay in `tui-sessions.json` forever, so the
//! viewer kept them all in `self.workspaces`. Owner's laptop held 4,196
//! workspaces, 3,942 of them closed. Each manifest save serialized and fsynced
//! all of them, and every pass over `self.workspaces` walked them.
//!
//! At restore, a closed workspace moves to `~/.cm/tui-workspace-archive.jsonl`
//! once it has been quiet for [`ARCHIVE_AFTER_SECS`]. A workspace is never
//! archived when it is pinned, a cloud workspace, still has rows, is bound to
//! a task (archiving one bound to a running task is how a fresh empty
//! workspace gets provisioned for it), has a checkout that exists on this
//! machine (`TasksUpdated` reconcile matches an unbound task to a workspace by
//! that path, and without a match it provisions a new open workspace), or has
//! a tombstone whose uid the local daemon still runs. An auto-close agent wrapper is archived with its
//! flag. The A-O picker reads the archive lazily, and reopening a record moves
//! it back into the manifest with its section assignment and auto-close flag.
//! An adoption that reuses an archived id gets a fresh in-memory workspace. If
//! that workspace is archived again, the two records merge their tombstones. The archive is written (fsync + rename)
//! BEFORE the manifest save that drops the workspaces. If that write fails,
//! nothing is archived. A corrupt or missing archive is never fatal: bad
//! lines are skipped.
//!
//! Tombstones (the A-O / revive source) are deduplicated by uid, keeping the
//! newest, and capped at [`TOMBSTONES_PER_WORKSPACE`], dropping the oldest
//! first and never one whose uid is still live.

use super::*;

/// A closed workspace is archived once its newest activity is this old.
pub(crate) const ARCHIVE_AFTER_SECS: f64 = 14.0 * 24.0 * 3600.0;
/// Tombstones kept per workspace, newest first. Live uids are always kept.
pub(crate) const TOMBSTONES_PER_WORKSPACE: usize = 50;
const ARCHIVE_FILE: &str = ".cm/tui-workspace-archive.jsonl";
const ARCHIVE_VERSION: u32 = 1;

/// One line of the archive.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub(crate) struct ArchivedWorkspace {
    pub v: u32,
    pub archived_at: f64,
    /// The workspace's explicit section assignment, restored on reopen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// It was an auto-close agent wrapper (`auto_close_workspaces`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_close: bool,
    pub workspace: ManifestWorkspace,
}

pub(crate) fn archive_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(ARCHIVE_FILE)
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Creation time encoded in a `ws-<16 hex digits of unix nanos>` id. UUID-style
/// ids carry no time and return `None`.
pub(crate) fn id_created_at(id: &str) -> Option<f64> {
    let hex = id.strip_prefix("ws-")?;
    if hex.len() != 16 {
        return None;
    }
    let nanos = u64::from_str_radix(hex, 16).ok()?;
    Some(nanos as f64 / 1e9)
}

/// The newest moment a closed workspace is known to have been used: its newest
/// tombstone, else its creation time. `None` means unknown, so it is never archived.
pub(crate) fn last_activity(mw: &ManifestWorkspace) -> Option<f64> {
    let newest_exit = mw
        .tombstones
        .iter()
        .map(|t| t.exited_at)
        .fold(None, |acc: Option<f64>, t| Some(acc.map_or(t, |a| a.max(t))));
    newest_exit.or_else(|| id_created_at(&mw.id))
}

/// Deduplicate by uid, keeping the newest `exited_at`. Then drop the oldest
/// tombstones beyond `cap`, but never one whose uid is in `live`. The result
/// is ordered oldest first, which is the push order.
pub(crate) fn compact_tombstones(
    tombstones: &mut Vec<SessionTombstone>,
    live: &HashSet<String>,
    cap: usize,
) {
    if tombstones.len() <= 1 {
        return;
    }
    let mut newest: HashMap<&str, usize> = HashMap::new();
    for (i, t) in tombstones.iter().enumerate() {
        match newest.get(t.uid.as_str()) {
            Some(&j) if tombstones[j].exited_at >= t.exited_at => {}
            _ => {
                newest.insert(t.uid.as_str(), i);
            }
        }
    }
    if newest.len() == tombstones.len() && tombstones.len() <= cap {
        return;
    }
    let mut keep: Vec<usize> = newest.into_values().collect();
    // Newest first, then trim the tail (the oldest), sparing live uids.
    keep.sort_by(|&a, &b| tombstones[b].exited_at.total_cmp(&tombstones[a].exited_at).then(b.cmp(&a)));
    let mut kept = 0usize;
    keep.retain(|&i| {
        if live.contains(&tombstones[i].uid) {
            return true;
        }
        kept += 1;
        kept <= cap
    });
    keep.sort_by(|&a, &b| tombstones[a].exited_at.total_cmp(&tombstones[b].exited_at).then(a.cmp(&b)));
    let mut slots: Vec<Option<SessionTombstone>> = std::mem::take(tombstones).into_iter().map(Some).collect();
    *tombstones = keep.into_iter().filter_map(|i| slots[i].take()).collect();
}

/// Read every parseable line. Returns the records and the number of lines
/// skipped as unreadable. A missing file is an empty archive.
pub(crate) fn read_archive(path: &Path) -> (Vec<ArchivedWorkspace>, usize) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (Vec::new(), 0);
    };
    let mut bad = 0;
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<ArchivedWorkspace>(line) {
            Ok(rec) => out.push(rec),
            Err(_) => bad += 1,
        }
    }
    (out, bad)
}

/// Crash-safe full rewrite: stage, fsync, rename, fsync the directory.
pub(crate) fn write_archive(path: &Path, records: &[ArchivedWorkspace]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = Vec::new();
    for rec in records {
        serde_json::to_writer(&mut body, rec).map_err(std::io::Error::other)?;
        body.push(b'\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Merge `new` into the archive at `path`. A record whose id is already
/// archived replaces the old one but keeps the old one's tombstones.
fn append_records(path: &Path, mut new: Vec<ArchivedWorkspace>) -> std::io::Result<()> {
    let (mut records, _bad) = read_archive(path);
    let mut old: HashMap<String, Vec<SessionTombstone>> = HashMap::new();
    let ids: HashSet<String> = new.iter().map(|r| r.workspace.id.clone()).collect();
    records.retain_mut(|r| {
        if !ids.contains(&r.workspace.id) {
            return true;
        }
        old.entry(r.workspace.id.clone()).or_default().append(&mut r.workspace.tombstones);
        false
    });
    for rec in &mut new {
        if let Some(mut earlier) = old.remove(&rec.workspace.id) {
            earlier.append(&mut rec.workspace.tombstones);
            earlier.sort_by(|a, b| a.exited_at.total_cmp(&b.exited_at));
            rec.workspace.tombstones = earlier;
            compact_tombstones(&mut rec.workspace.tombstones, &HashSet::new(), TOMBSTONES_PER_WORKSPACE);
        }
    }
    records.extend(new);
    write_archive(path, &records)
}

impl App {
    /// Restore-time compaction (hook in `restore_sessions`): bound every
    /// workspace's tombstones, then move stale closed workspaces from
    /// `manifest` to the archive. Returns how many were archived.
    pub(super) fn compact_and_archive_manifest(
        &mut self,
        manifest: &mut Manifest,
        live_daemon_uids: &HashSet<String>,
    ) -> usize {
        self.compact_and_archive_manifest_at(manifest, live_daemon_uids, &archive_path(), now_secs())
    }

    pub(super) fn compact_and_archive_manifest_at(
        &mut self,
        manifest: &mut Manifest,
        live_daemon_uids: &HashSet<String>,
        path: &Path,
        now: f64,
    ) -> usize {
        let mut live: HashSet<String> = live_daemon_uids.clone();
        for mw in manifest.workspaces.values() {
            live.extend(mw.sessions.iter().map(|e| e.uid.clone()));
        }
        for mw in manifest.workspaces.values_mut() {
            compact_tombstones(&mut mw.tombstones, &live, TOMBSTONES_PER_WORKSPACE);
        }

        let mut bound: HashSet<&str> = manifest.bindings.values().map(String::as_str).collect();
        bound.extend(self.tasks.iter().filter_map(|t| t.workspace_id.as_deref()));
        let in_memory: HashSet<&str> = self.workspaces.iter().map(|w| w.id.as_str()).collect();
        let stale: Vec<String> = manifest
            .workspaces
            .values()
            .filter(|mw| {
                mw.is_closed
                    && !mw.pinned
                    && !mw.is_cloud
                    && mw.sessions.is_empty()
                    && !bound.contains(mw.id.as_str())
                    && !in_memory.contains(mw.id.as_str())
                    && !self.user_reopened.contains(&mw.id)
                    && !mw.tombstones.iter().any(|t| live.contains(&t.uid))
                    && last_activity(mw).is_some_and(|at| now - at >= ARCHIVE_AFTER_SECS)
                    && !mw.worktree_path.as_deref().is_some_and(Path::exists)
            })
            .map(|mw| mw.id.clone())
            .collect();
        if stale.is_empty() {
            return 0;
        }
        let records: Vec<ArchivedWorkspace> = stale
            .iter()
            .filter_map(|id| manifest.workspaces.get(id))
            .map(|mw| ArchivedWorkspace {
                v: ARCHIVE_VERSION,
                archived_at: now,
                section: self.workspace_sections.get(&mw.id).cloned(),
                auto_close: self.auto_close_workspaces.contains(&mw.id)
                    || manifest.auto_close_workspaces.contains(&mw.id),
                workspace: mw.clone(),
            })
            .collect();
        if let Err(e) = append_records(path, records) {
            eprintln!(
                "cm-tui: workspace archive write to {} failed ({e}); keeping {} closed workspaces in the manifest",
                path.display(),
                stale.len()
            );
            return 0;
        }
        for id in &stale {
            manifest.workspaces.remove(id);
            self.workspace_sections.remove(id);
            self.auto_close_workspaces.remove(id);
            self.folded_workspaces.remove(id);
        }
        let stale_ids: HashSet<&String> = stale.iter().collect();
        manifest.auto_close_workspaces.retain(|id| !stale_ids.contains(id));
        eprintln!(
            "cm-tui: archived {} closed workspaces idle for {}+ days to {} (A-O still reopens them)",
            stale.len(),
            (ARCHIVE_AFTER_SECS / 86400.0) as u64,
            path.display()
        );
        stale.len()
    }

    /// A-O candidates for archived workspaces (hook in
    /// `open_past_workspace_picker`). Reads the archive on each open, which is
    /// an explicit Owner action. Ids also present in memory are skipped.
    pub(super) fn archived_past_candidates(&self) -> Vec<super::input::PastCandidate> {
        self.archived_past_candidates_from(&archive_path())
    }

    pub(super) fn archived_past_candidates_from(&self, path: &Path) -> Vec<super::input::PastCandidate> {
        let (records, bad) = read_archive(path);
        if bad > 0 {
            eprintln!("cm-tui: skipped {bad} unreadable lines in {}", path.display());
        }
        let in_memory: HashSet<&str> = self.workspaces.iter().map(|w| w.id.as_str()).collect();
        records
            .into_iter()
            .filter(|r| !in_memory.contains(r.workspace.id.as_str()))
            .map(|r| {
                let mw = r.workspace;
                let path_str = mw
                    .worktree_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let tasks = self.tasks.iter().filter(|t| t.workspace_id.as_deref() == Some(mw.id.as_str()));
                let search_fields = super::input::PastCandidate::search_fields_from(
                    std::iter::once(mw.name.as_str())
                        .chain(tasks.flat_map(|t| std::iter::once(t.name.as_str()).chain(t.wip_branch.as_deref())))
                        .chain(std::iter::once(path_str.as_str()))
                        .chain(mw.tombstones.iter().map(|t| t.label.as_str()))
                        .chain(std::iter::once(mw.host_id.as_str())),
                );
                let worktree_exists = match mw.worktree_path.as_ref() {
                    None => Some(false),
                    Some(_) if mw.host_id != cm_daemon::host_id::HostId::local() => None,
                    Some(p) => Some(p.exists()),
                };
                super::input::PastCandidate {
                    ws_id: mw.id.clone(),
                    display: mw.name.clone(),
                    worktree_path: mw.worktree_path.clone(),
                    worktree_exists,
                    last_exited_at: mw.tombstones.iter().map(|t| t.exited_at).fold(0.0f64, f64::max),
                    search_fields,
                    note: Some("archived"),
                }
            })
            .collect()
    }

    /// Bring an archived workspace back into `self.workspaces`, still closed,
    /// so `reopen_workspace_by_id` can reopen it (hook in the A-O submit).
    /// No-op returning false when `ws_id` is not archived.
    pub(super) fn unarchive_workspace(&mut self, ws_id: &str) -> bool {
        self.unarchive_workspace_from(ws_id, &archive_path(), now_secs())
    }

    pub(super) fn unarchive_workspace_from(&mut self, ws_id: &str, path: &Path, now: f64) -> bool {
        let (mut records, _bad) = read_archive(path);
        let Some(pos) = records.iter().rposition(|r| r.workspace.id == ws_id) else {
            return false;
        };
        let rec = records.remove(pos);
        records.retain(|r| r.workspace.id != ws_id);
        if let Err(e) = write_archive(path, &records) {
            // The in-memory copy wins: the next save writes it to the manifest,
            // and the picker hides archived ids that are in memory.
            eprintln!("cm-tui: workspace archive rewrite failed ({e}); reopening `{ws_id}` anyway");
        }
        let mw = rec.workspace;
        let tombstones: Vec<SessionTombstone> = mw
            .tombstones
            .iter()
            .filter(|t| now - t.exited_at < TOMBSTONE_RETENTION_SECS)
            .cloned()
            .collect();
        if let Some(ws) = self.workspaces.iter_mut().find(|w| w.id == ws_id) {
            // The id came back meanwhile (a launch or adoption reused it).
            // Keep that workspace and merge in the archived closed sessions.
            // Keep push order (oldest first): the archived exits predate most
            // of the new workspace's, but not necessarily all of them.
            ws.tombstones.extend(tombstones);
            ws.tombstones.sort_by(|a, b| a.exited_at.total_cmp(&b.exited_at));
            compact_tombstones(&mut ws.tombstones, &HashSet::new(), TOMBSTONES_PER_WORKSPACE);
        } else {
            let name = if mw.name.is_empty() {
                mw.worktree_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("workspace")
                    .to_string()
            } else {
                mw.name.clone()
            };
            self.workspaces.push(Workspace {
                org: mw.org.clone(),
                id: mw.id.clone(),
                name,
                is_closed: true,
                is_cloud: mw.is_cloud,
                repo_url: mw.repo_url.clone(),
                worktree_path: mw.worktree_path.clone(),
                main_repo_path: mw.main_repo_path.clone(),
                worker_vm: mw.worker_vm.clone(),
                worker_zone: mw.worker_zone.clone(),
                host_id: mw.host_id.clone(),
                color: mw.color.clone(),
                pinned: mw.pinned,
                sessions: vec![],
                tombstones,
            });
        }
        if let Some(section) = rec.section {
            if self.section_index(&section).is_some() && !self.workspace_sections.contains_key(ws_id) {
                self.workspace_sections.insert(ws_id.to_string(), section);
            }
        }
        if rec.auto_close {
            // As before archiving. A-O's reopen then claims it for Owner.
            self.auto_close_workspaces.insert(ws_id.to_string());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tomb(uid: &str, at: f64) -> SessionTombstone {
        SessionTombstone {
            entry: None,
            uid: uid.into(),
            managed_by_uid: None,
            label: format!("label-{uid}"),
            session_type: "codex".into(),
            task_id: None,
            last_transcript_id: None,
            worktree_path: None,
            generation: 0,
            exited_at: at,
        }
    }

    fn mws(id: &str, closed: bool, tombstones: Vec<SessionTombstone>) -> ManifestWorkspace {
        ManifestWorkspace {
            id: id.into(),
            name: format!("name-{id}"),
            is_closed: closed,
            worktree_path: Some(PathBuf::from(format!("/remote/{id}"))),
            host_id: cm_daemon::host_id::HostId::new("sessions"),
            tombstones,
            ..Default::default()
        }
    }

    fn app() -> App {
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        app.workspaces.clear();
        app.tasks.clear();
        app.auto_close_workspaces.clear();
        app.workspace_sections.clear();
        app
    }

    const DAY: f64 = 86400.0;
    const NOW: f64 = 1_793_917_805.0;

    #[test]
    fn id_time_decodes_hex_nanos_and_ignores_uuids() {
        let t = id_created_at("ws-18caf17c13959413").unwrap();
        // 0x18caf17c13959413 ns = 1,786,505,717.4 s (2026-08-12).
        assert!((1_786_505_717.0..1_786_505_718.0).contains(&t), "{t}");
        assert_eq!(id_created_at("9b688537ea4b4c929b93c4f305851c5f"), None);
        assert_eq!(id_created_at("ws-momentum-detective"), None);
    }

    #[test]
    fn tombstones_dedup_by_uid_keeping_newest_then_cap_oldest_first_sparing_live() {
        let mut t = vec![tomb("a", 1.0), tomb("b", 2.0), tomb("a", 5.0), tomb("c", 3.0), tomb("d", 4.0)];
        compact_tombstones(&mut t, &HashSet::new(), 10);
        let got: Vec<(&str, f64)> = t.iter().map(|t| (t.uid.as_str(), t.exited_at)).collect();
        assert_eq!(got, vec![("b", 2.0), ("c", 3.0), ("d", 4.0), ("a", 5.0)]);

        let live: HashSet<String> = ["b".to_string()].into();
        compact_tombstones(&mut t, &live, 2);
        let got: Vec<&str> = t.iter().map(|t| t.uid.as_str()).collect();
        assert_eq!(got, vec!["b", "d", "a"], "oldest non-live dropped; live b kept beyond the cap");
    }

    #[test]
    fn restore_archives_only_stale_unprotected_closed_workspaces_and_reopen_brings_one_back() {
        let _guard = crate::test_support::home_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.jsonl");
        let mut app = app();
        app.sections.push(cm_daemon::manifest::SidebarSection {
            id: "sec-1".into(),
            name: "One".into(),
            color: None,
            folded: false,
        });
        let old = NOW - 20.0 * DAY;
        let mut m = Manifest::default();
        for mw in [
            mws("stale", true, vec![tomb("t1", old)]),
            mws("recent", true, vec![tomb("t2", NOW - DAY)]),
            mws("open", false, vec![tomb("t3", old)]),
            mws("bound", true, vec![tomb("t4", old)]),
            mws("autoclose", true, vec![tomb("t5", old)]),
            mws("livetomb", true, vec![tomb("t6", old)]),
            mws("unknown-age", true, vec![]),
            mws("ws-18caf17c13959413", true, vec![]),
        ] {
            m.workspaces.insert(mw.id.clone(), mw);
        }
        let mut pinned = mws("pinned", true, vec![tomb("t7", old)]);
        pinned.pinned = true;
        m.workspaces.insert("pinned".into(), pinned);
        // Its checkout is on this disk: an unbound task would match it by path.
        let mut local = mws("local-checkout", true, vec![tomb("t8", old)]);
        local.worktree_path = Some(dir.path().to_path_buf());
        m.workspaces.insert("local-checkout".into(), local);
        m.bindings.insert("task-x".into(), "bound".into());
        app.auto_close_workspaces.insert("autoclose".into());
        m.auto_close_workspaces.push("autoclose".into());
        app.workspace_sections.insert("stale".into(), "sec-1".into());
        app.folded_workspaces.insert("stale".into());
        let live: HashSet<String> = ["t6".to_string()].into();

        let n = app.compact_and_archive_manifest_at(&mut m, &live, &path, NOW);

        assert_eq!(n, 3);
        let mut kept: Vec<&str> = m.workspaces.keys().map(String::as_str).collect();
        kept.sort();
        assert_eq!(kept, vec!["bound", "livetomb", "local-checkout", "open", "pinned", "recent", "unknown-age"]);
        assert!(!app.workspace_sections.contains_key("stale"), "section moves into the record");
        assert!(!app.folded_workspaces.contains("stale"), "an archived workspace keeps no fold");
        assert!(!app.auto_close_workspaces.contains("autoclose"), "the flag moves into the record");
        assert!(m.auto_close_workspaces.is_empty());
        let (records, bad) = read_archive(&path);
        assert_eq!(bad, 0);
        assert_eq!(records.len(), 3);
        assert_eq!(records.iter().find(|r| r.workspace.id == "stale").unwrap().section.as_deref(), Some("sec-1"));
        assert!(records.iter().find(|r| r.workspace.id == "autoclose").unwrap().auto_close);
        assert!(!records.iter().find(|r| r.workspace.id == "stale").unwrap().auto_close);

        // A-O lists archived workspaces and reopening one restores it.
        let cands = app.archived_past_candidates_from(&path);
        assert_eq!(cands.len(), 3);
        let stale = cands.iter().find(|c| c.ws_id == "stale").unwrap();
        assert_eq!(stale.note, Some("archived"));
        assert_eq!(stale.worktree_exists, None, "remote path is validated by its host");
        assert!(stale.search_fields.iter().any(|f| f == "label-t1"));

        assert!(app.unarchive_workspace_from("stale", &path, NOW));
        let ws = app.workspaces.iter().find(|w| w.id == "stale").unwrap();
        assert!(ws.is_closed, "reopen_workspace_by_id does the reopen");
        assert_eq!(ws.tombstones.len(), 1);
        assert_eq!(app.workspace_sections.get("stale").map(String::as_str), Some("sec-1"));
        let (records, _) = read_archive(&path);
        assert_eq!(records.len(), 2, "the reopened record leaves the archive");
        assert!(!app.unarchive_workspace_from("stale", &path, NOW), "second reopen is a no-op");
        assert_eq!(app.archived_past_candidates_from(&path).len(), 2);
        assert!(!app.auto_close_workspaces.contains("stale"));
        assert!(app.unarchive_workspace_from("autoclose", &path, NOW));
        assert!(app.auto_close_workspaces.contains("autoclose"), "wrapper flag restored on reopen");
    }

    #[test]
    fn archiving_an_id_again_keeps_the_earlier_records_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.jsonl");
        let rec = |tombs| ArchivedWorkspace {
            v: 1,
            archived_at: NOW,
            section: None,
            auto_close: true,
            workspace: mws("again", true, tombs),
        };
        append_records(&path, vec![rec(vec![tomb("first", NOW - 40.0 * DAY)])]).unwrap();
        append_records(&path, vec![rec(vec![tomb("second", NOW - 20.0 * DAY)])]).unwrap();
        let (records, _) = read_archive(&path);
        assert_eq!(records.len(), 1);
        let uids: Vec<&str> = records[0].workspace.tombstones.iter().map(|t| t.uid.as_str()).collect();
        assert_eq!(uids, vec!["first", "second"]);
    }

    #[test]
    fn archive_write_failure_keeps_workspaces_in_the_manifest() {
        let _guard = crate::test_support::home_lock();
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be: the rename fails.
        let path = dir.path().join("archive.jsonl");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"x").unwrap();
        let mut app = app();
        let mut m = Manifest::default();
        m.workspaces.insert("stale".into(), mws("stale", true, vec![tomb("t1", NOW - 30.0 * DAY)]));
        assert_eq!(app.compact_and_archive_manifest_at(&mut m, &HashSet::new(), &path, NOW), 0);
        assert!(m.workspaces.contains_key("stale"));
    }

    #[test]
    fn corrupt_archive_lines_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.jsonl");
        let good = ArchivedWorkspace { v: 1, archived_at: NOW, section: None, auto_close: false, workspace: mws("good", true, vec![]) };
        let mut text = serde_json::to_string(&good).unwrap();
        text.push_str("\n{not json\n\n");
        std::fs::write(&path, text).unwrap();
        let (records, bad) = read_archive(&path);
        assert_eq!((records.len(), bad), (1, 1));
        assert_eq!(read_archive(&dir.path().join("missing.jsonl")).0.len(), 0);
    }

    #[test]
    fn unarchive_merges_into_a_workspace_whose_id_came_back() {
        let _guard = crate::test_support::home_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.jsonl");
        let mut app = app();
        let rec = ArchivedWorkspace {
            v: 1,
            archived_at: NOW,
            section: None,
            auto_close: false,
            workspace: mws("again", true, vec![tomb("old", NOW - 2.0 * DAY)]),
        };
        write_archive(&path, &[rec]).unwrap();
        app.workspaces.push(Workspace {
            org: None,
            id: "again".into(),
            name: "again".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: None,
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::new("sessions"),
            color: None,
            pinned: false,
            sessions: vec![],
            tombstones: vec![tomb("new", NOW)],
        });
        assert!(app.archived_past_candidates_from(&path).is_empty(), "in-memory id hides its archive record");
        assert!(app.unarchive_workspace_from("again", &path, NOW));
        assert_eq!(app.workspaces.len(), 1);
        let uids: Vec<&str> = app.workspaces[0].tombstones.iter().map(|t| t.uid.as_str()).collect();
        assert_eq!(uids, vec!["old", "new"]);
        assert!(read_archive(&path).0.is_empty());
    }
}
