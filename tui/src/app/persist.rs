//! Manifest persistence + startup restore: save/load tui-sessions.json, restore_sessions, daemon-session adoption.

use super::*;

impl App {
    /// Path to the session manifest file.
    fn manifest_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".cm/tui-sessions.json")
    }

    /// Crash-safe write: stage to a sibling `.tmp`, fsync, then rename.
    /// On Linux, rename is atomic across the same filesystem, so a reader
    /// either sees the old complete file or the new complete file — never
    /// a truncated/partial one. The fsync before rename ensures the new
    /// content has hit disk before the directory entry flips.
    fn atomic_write_manifest(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;
        let tmp = match path.file_name() {
            Some(name) => {
                let mut s = name.to_os_string();
                s.push(".tmp");
                path.with_file_name(s)
            }
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "manifest path has no file name",
                ));
            }
        };
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    /// Save session manifest to disk.
    pub(crate) fn save_session_manifest(&self) {
        // Refuse to persist before the on-disk manifest has been hydrated
        // into `self.workspaces` by `restore_sessions`. This writer does a
        // FULL REPLACE of `~/.cm/tui-sessions.json` from `self.workspaces`
        // (no merge with disk), so a save while that map is still a partial
        // startup view — empty, or only the live agent sessions adoption
        // surfaced — silently drops every workspace/session not yet
        // restored. That is exactly the "lost sessions / lost workspaces on
        // restart" clobber: adoption (or any RPC-triggered save) firing
        // before restore overwrote a 14-workspace manifest down to the
        // 3 live agent sessions. `maybe_restore_sessions` flips
        // `sessions_restored` to true before `restore_sessions` runs its own
        // internal save, so the hydrated write still goes through.
        if !self.sessions_restored {
            return;
        }
        let mut workspaces: HashMap<String, ManifestWorkspace> = HashMap::new();
        for ws in &self.workspaces {
            let mut entries: Vec<ManifestEntry> = ws
                .sessions
                .iter()
                .map(TerminalSession::to_manifest_entry)
                .collect();
            // 12e-r7 F1: append any manifest entries that
            // were skipped at restore time because their
            // `host_id` failed the local-host guard. They're
            // preserved verbatim — clone the full entry so a
            // remote-pinned session survives a TUI restart on
            // local active_host (or post-Phase-3 daemon-side
            // reattach support) untouched.
            if let Some(skipped) = self.skipped_manifest_entries.get(&ws.id) {
                entries.extend(skipped.iter().cloned());
            }
            workspaces.insert(
                ws.id.clone(),
                ManifestWorkspace {
                    id: ws.id.clone(),
                    name: ws.name.clone(),
                    is_closed: ws.is_closed,
                    is_cloud: ws.is_cloud,
                    worktree_path: ws.worktree_path.clone(),
                    main_repo_path: ws.main_repo_path.clone(),
                    repo_url: ws.repo_url.clone(),
                    worker_vm: ws.worker_vm.clone(),
                    worker_zone: ws.worker_zone.clone(),
                    host_id: ws.host_id.clone(),
                    color: ws.color.clone(),
                    pinned: ws.pinned,
                    sessions: entries,
                    tombstones: ws.tombstones.clone(),
                },
            );
        }

        let mut bindings: HashMap<String, String> = HashMap::new();
        for task in &self.tasks {
            if let (Some(tid), Some(wsid)) = (&task.task_id, &task.workspace_id) {
                bindings.insert(tid.clone(), wsid.clone());
            }
        }

        let view = match self.sidebar_view {
            SidebarView::Status => "status",
            SidebarView::Task => "task",
        };
        let manifest = Manifest {
            workspaces,
            bindings,
            // Daemon-only field (daemon-sessions.json); the TUI never
            // mints agent task edges and never writes them here.
            agent_task_edges: Default::default(),
            view: Some(view.to_string()),
            hide_continuous: self.hide_continuous,
            continuous_column_on: self.continuous_column_on,
            task_colors: self.task_colors.clone(),
        };

        let path = Self::manifest_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&manifest) {
            if let Err(e) = Self::atomic_write_manifest(&path, json.as_bytes()) {
                eprintln!(
                    "failed to write session manifest at {}: {}",
                    path.display(),
                    e
                );
            }
        }

        // 10d-1: every session-list mutation site is required by
        // the convention at the top of the helper section to
        // call `save_session_manifest` before returning Ok (see
        // the doc comment on the `start_session_for_new_task`
        // family). That makes this the single canonical funnel
        // for "session list / per-session fields changed";
        // pushing the snapshot to the daemon here gives the
        // 10d-2 auth consumer universal coverage without a
        // call-site audit. Cost: one extra local UDS round-trip
        // per save (opt-in gated; sub-ms vs. the disk write
        // above). Failure surfaces via the helper's own
        // `eprintln!` (round-11 invariant: don't silently
        // swallow under opt-in).
        self.push_tui_sessions_to_daemon();
    }

    /// Load session manifest from disk. On parse failure, the corrupt file is
    /// preserved at `<path>.corrupt-<unix_ts>` so the user can recover state.
    pub(super) fn load_manifest() -> Manifest {
        let path = Self::manifest_path();
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Manifest::default(),
        };
        match serde_json::from_str(&contents) {
            Ok(m) => m,
            Err(e) => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let backup = path.with_extension(format!("json.corrupt-{}", ts));
                let backup_msg = match std::fs::rename(&path, &backup) {
                    Ok(()) => format!("backed up to {}", backup.display()),
                    Err(rename_err) => match std::fs::write(&backup, &contents) {
                        Ok(()) => format!(
                            "rename failed ({}); copied to {}",
                            rename_err,
                            backup.display()
                        ),
                        Err(copy_err) => format!(
                            "could not preserve corrupt file (rename: {}; copy: {})",
                            rename_err, copy_err
                        ),
                    },
                };
                eprintln!(
                    "session manifest at {} failed to parse ({}); {}. Starting with empty state.",
                    path.display(),
                    e,
                    backup_msg
                );
                Manifest::default()
            }
        }
    }

    /// Collapse duplicate-uid slots in a freshly-loaded manifest. A daemon
    /// session is unique by `uid`, but a pre-fix adopt race (the off-thread
    /// attach in-flight window, before the `attaching` dedup gate) could persist
    /// the SAME uid across multiple synthetic workspaces — surfacing as phantom
    /// duplicate sidebar entries (and, for a continuous orchestrator, a
    /// duplicate orchestrator). Keep each uid in exactly ONE workspace (first by
    /// sorted id, for determinism) and strip it from the rest; drop a workspace
    /// that DEDUP emptied when it's open + unbound (pure adopt debris). Closed
    /// (explicit user action) and bound workspaces are preserved even if
    /// emptied; pre-existing empty workspaces are left untouched. Self-healing —
    /// the duplicates evaporate on the next restart with no manual file edit.
    fn dedup_manifest_uids(manifest: &mut cm_daemon::manifest::Manifest) {
        let bound: HashSet<String> = manifest.bindings.values().cloned().collect();
        let mut seen: HashSet<String> = HashSet::new();
        let mut emptied: HashSet<String> = HashSet::new();
        // Deterministic keeper: the lowest workspace id wins each uid.
        let mut ids: Vec<String> = manifest.workspaces.keys().cloned().collect();
        ids.sort();
        for id in ids {
            if let Some(w) = manifest.workspaces.get_mut(&id) {
                let had_sessions = !w.sessions.is_empty();
                w.sessions.retain(|e| seen.insert(e.uid.clone()));
                if had_sessions
                    && w.sessions.is_empty()
                    && !w.is_closed
                    && !bound.contains(&id)
                {
                    emptied.insert(id);
                }
            }
        }
        manifest.workspaces.retain(|id, _| !emptied.contains(id));
    }

    /// Hydrate `self.workspaces` from the on-disk manifest exactly once,
    /// decoupled from the cloud/planning API. Driven from the main loop.
    ///
    /// Restore was historically gated on the first
    /// `BackendEvent::TasksUpdated`, which only fires after a successful
    /// `list_tasks` API round-trip (see `backend::do_refresh`). In the
    /// common pure-local case — or whenever the API host is slow/unreachable
    /// (e.g. a failing remote host) — that event never arrives, so the
    /// manifest sat intact on disk while `self.workspaces` stayed empty.
    /// With the `save_session_manifest` / adoption guards in place that would
    /// block persistence indefinitely; without them, adoption clobbered the
    /// manifest (the "lost sessions on restart" bug). The manifest is a
    /// local file, so its restore must not depend on the API at all.
    ///
    /// Idempotent via `sessions_restored`; cheap to call every tick. Sets the
    /// flag *before* delegating so `restore_sessions`' own internal
    /// `save_session_manifest` / adoption (which run at its tail) are not
    /// blocked by the guards above. All prerequisites — `host_pool`,
    /// `workflow_runs`, and the daemon socket — are ready by the time the
    /// main loop first turns (loaded in `App::new` / `ensure_daemon_at_startup`).
    pub fn maybe_restore_sessions(&mut self) {
        if self.sessions_restored {
            return;
        }
        self.sessions_restored = true;
        self.restore_sessions();
    }

    /// Restore workspaces + sessions from the manifest. Runs after an
    /// initial API tasks fetch so `bindings` can be cross-referenced with
    /// real tasks, but also works standalone (workspaces without any bound
    /// tasks are legal).
    pub(super) fn restore_sessions(&mut self) {
        let mut manifest = Self::load_manifest();
        // Heal duplicate-uid slots BEFORE any downstream pass reads the
        // workspaces (bound/useful computation, the spawn/reattach loop) so a
        // manifest carrying pre-fix phantom duplicates collapses to one slot per
        // daemon session on this restart.
        Self::dedup_manifest_uids(&mut manifest);
        if manifest.workspaces.is_empty() && manifest.bindings.is_empty() {
            return;
        }

        let (cols, rows) = self.last_term_size;

        // migrate-tui-local Issue 1: probe the local daemon's
        // session registry ONCE before the spawn loop. Pre-fix
        // the restore unconditionally called `start_session`
        // against UIDs that the daemon ALREADY owned (because
        // the daemon survives TUI restarts), and the daemon's
        // collision guard returned Conflict — restored sessions
        // disappeared from `ws.sessions`. Now: for each manifest
        // entry whose UID is in this set, `spawn_restored_session`
        // routes through `session.attach` instead of
        // `start_session`.
        //
        // Probe is best-effort. RPC failure → empty set →
        // pre-fix behavior (spawn-then-Conflict for any UIDs the
        // daemon already had). The list_sessions call is
        // O(daemon's session count) and runs once.
        let live_daemon_uids: std::collections::HashSet<String> = self
            .host_pool
            .for_host(&cm_daemon::host_id::HostId::local())
            .ok()
            .and_then(|h| h.socket_path())
            .and_then(|sock| {
                crate::client_session::rpc_list_session_uids(
                    &sock,
                    crate::daemon_launch::operator_token(),
                )
                .ok()
            })
            .unwrap_or_default();

        // Identify worktree paths that are "covered" by a useful workspace —
        // one with sessions or referenced in bindings. We use this to drop
        // orphan-duplicate empty workspaces that accumulated from the pre-fix
        // auto-provision-before-restore bug.
        let bound_ws_ids: HashSet<&String> = manifest.bindings.values().collect();
        let useful_worktree_paths: HashSet<PathBuf> = manifest
            .workspaces
            .values()
            .filter(|w| !w.sessions.is_empty() || bound_ws_ids.contains(&w.id))
            .filter_map(|w| w.worktree_path.clone())
            .collect();

        // 10d-3 R3 recovery (round-2 ordering fix): compute the
        // active-runs set BEFORE the spawn loop so we can untag
        // stale `workflow_run_id` / `workflow_role` on manifest
        // entries IN-PLACE before `spawn_restored_session` reads
        // them into the agent's MCP env. Pre-r2 the reconciliation
        // ran after spawn, so a restored agent had stale
        // `CM_WORKFLOW_RUN_ID` / `CM_ROLE` env vars pointing at a
        // now-Detached run. The pre-r2 reconciliation step (below
        // the spawn loop) is replaced by this in-place cleanup.
        let active_run_ids: std::collections::HashSet<String> = self
            .workflow_runs
            .iter()
            .map(|r| r.run_id.clone())
            .collect();

        // Phase 4 (remote-session-execution): bound an OFFLINE remote host to
        // ONE reachability dial per restore pass. `for_host` runs
        // `ensure_alive` (a ~3s ssh-tunnel spawn for a down ssh-unix host) and
        // does NOT consult the reachability backoff cache, so without this an
        // N-session offline host would stall startup ~3s×N. We cache a host
        // ONLY on a host-unreachable failure (`for_host` Err) — NEVER on a
        // session-gone failure (a reachable host where one specific session
        // exited), so sibling LIVE sessions on a reachable host still reattach.
        let mut unreachable_hosts: std::collections::HashSet<
            cm_daemon::host_id::HostId,
        > = std::collections::HashSet::new();

        // Rebuild self.workspaces from the manifest. Closed workspaces are
        // loaded with empty sessions (their PTY state is gone anyway).
        for (_, mw) in manifest.workspaces.iter() {
            let already = self.workspaces.iter().any(|w| w.id == mw.id);
            if already {
                continue;
            }
            // Skip orphan-duplicate: empty, open, not in bindings, and shares a
            // worktree_path with a useful sibling. User-closed workspaces are
            // preserved (is_closed=true) since closing is an explicit action.
            if !mw.is_closed
                && mw.sessions.is_empty()
                && !bound_ws_ids.contains(&mw.id)
                && mw
                    .worktree_path
                    .as_ref()
                    .map_or(false, |p| useful_worktree_paths.contains(p))
            {
                continue;
            }
            // Prune tombstones older than the retention window before
            // copying them into the live workspace. Cheap — these lists
            // stay small in normal use.
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let restored_tombstones: Vec<SessionTombstone> = mw
                .tombstones
                .iter()
                .filter(|t| now_secs - t.exited_at < TOMBSTONE_RETENTION_SECS)
                .cloned()
                .collect();
            // P1 (Feature 3): soft-close accumulated "spent" litter on restore.
            // A local, open, session-less, UNBOUND (no live task binding)
            // workspace that still carries a recent tombstone is a finished
            // subtask/detective workspace the pre-fix teardown left behind (its
            // unique branch worktree dodged the orphan-duplicate skip above).
            // Restore it hidden (is_closed) — keeping its tombstones so
            // `read_session_output` still resolves — so the old pile-up clears on
            // the next TUI start instead of lingering as empty headers. Gated on
            // the manifest's own bindings (not self.tasks, which isn't loaded
            // yet), so there's no "task not reconciled" race. Fresh unused slots
            // (no tombstone) and still-bound workspaces are untouched.
            let spent_litter = !mw.is_closed
                && !mw.is_cloud
                && mw.sessions.is_empty()
                && !bound_ws_ids.contains(&mw.id)
                && !restored_tombstones.is_empty();
            let mut ws = Workspace {
                id: mw.id.clone(),
                name: if mw.name.is_empty() {
                    mw.worktree_path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .unwrap_or("workspace")
                        .to_string()
                } else {
                    mw.name.clone()
                },
                is_closed: mw.is_closed || spent_litter,
                is_cloud: mw.is_cloud,
                repo_url: mw.repo_url.clone(),
                worktree_path: mw.worktree_path.clone(),
                main_repo_path: mw.main_repo_path.clone(),
                worker_vm: mw.worker_vm.clone(),
                worker_zone: mw.worker_zone.clone(),
                // Backward-compat derive: a legacy manifest's workspace has no
                // host_id (serde-defaults to local), so prefer the persisted
                // sessions' host (authoritative — a worktree's host == its
                // sessions' host); fall back to the persisted workspace host.
                host_id: mw
                    .sessions
                    .first()
                    .map(|s| s.host_id.clone())
                    .unwrap_or_else(|| mw.host_id.clone()),
                color: mw.color.clone(),
                pinned: mw.pinned,
                sessions: vec![],
                tombstones: restored_tombstones,
                is_pushing: false,
            };
            if !ws.is_closed {
                for entry in &mw.sessions {
                    // Phase 4 (remote-session-execution): a REMOTE entry
                    // reattaches to its session on ITS host, ungated, via
                    // `try_reattach_remote_session` →
                    // `try_attach_via_daemon_with_deps(&entry.host_id, ...)`
                    // (which routes the attach RPCs through that host's
                    // socket). On ANY failure (host unreachable, session
                    // gone) PRESERVE the raw `ManifestEntry` in
                    // `skipped_manifest_entries` so the next
                    // `save_session_manifest` round-trips it back to disk
                    // untouched — remote re-spawn from restore is out of
                    // scope (it needs the Phase-3 create/add path), and
                    // dropping the entry would be the 12e-r7 F1 data-loss
                    // regression. Local entries fall through to the
                    // unchanged spawn/attach path below.
                    if entry.host_id != cm_daemon::host_id::HostId::local() {
                        // Phase 4 startup-freeze fix: a host whose dial can
                        // BLOCK (an `ssh-unix` tunnel spawn waits up to ~3s for
                        // the local socket to bind; `tcp-tls` opens a fresh
                        // connect) must NOT be dialed on the MAIN thread here —
                        // `restore_sessions` runs before the first frame paints,
                        // so a `for_host(remote)` here froze the UI for ~1-3s
                        // per configured remote. DEFER instead: preserve the RAW
                        // entry in `skipped_manifest_entries` (round-trips on
                        // save — the 12e-r7 F1 data-loss protection is retained)
                        // AND queue it in `pending_remote_reattach`. The per-host
                        // `manifest.watch` consumer (already on its own thread
                        // with reconnect) warms the tunnel; the main loop's
                        // `drain_deferred_remote_reattach` reattaches the session
                        // once the tunnel is connectable. Unix-transport remote
                        // hosts (whose `ensure_alive` is a no-op) and unknown
                        // hosts (`for_host` errors instantly) fall through to the
                        // synchronous reattach/preserve path below — their dial
                        // can't block, so local behavior there is unchanged.
                        if self.host_pool.dial_may_block(&entry.host_id) {
                            self.skipped_manifest_entries
                                .entry(ws.id.clone())
                                .or_default()
                                .push(entry.clone());
                            self.pending_remote_reattach.push(
                                PendingRemoteReattach::new(
                                    ws.id.clone(),
                                    entry.clone(),
                                ),
                            );
                            continue;
                        }
                        // Already-known-offline host this pass → preserve the
                        // RAW entry WITHOUT re-dialing (bounds an offline host
                        // to one ~3s reachability dial, not one per session).
                        if unreachable_hosts.contains(&entry.host_id) {
                            self.skipped_manifest_entries
                                .entry(ws.id.clone())
                                .or_default()
                                .push(entry.clone());
                            continue;
                        }
                        // Reachability probe FIRST. A host-unreachable failure
                        // (`for_host` Err — unknown host / down ssh tunnel)
                        // marks the host offline for the rest of the pass and
                        // preserves the raw entry. CRITICAL: this does NOT run
                        // for a session-gone failure below, so sibling LIVE
                        // sessions on a REACHABLE host still reattach.
                        if self.host_pool.for_host(&entry.host_id).is_err() {
                            eprintln!(
                                "cm-tui: host {} unreachable; preserving remote \
                                 session {} ({}) + skipping further dials this pass",
                                entry.host_id.as_str(),
                                entry.uid,
                                entry.label,
                            );
                            unreachable_hosts.insert(entry.host_id.clone());
                            self.skipped_manifest_entries
                                .entry(ws.id.clone())
                                .or_default()
                                .push(entry.clone());
                            continue;
                        }
                        // 10d-3 R3 parity with the local path below: if this
                        // entry's workflow_run_id points to a non-active
                        // (Detached/Done) run, reattach with the tags CLEARED
                        // — otherwise dead workflow context is pushed to the
                        // remote daemon (the attach RPC + the row tags). The
                        // SUCCESSFUL reattach uses the cleaned entry; a FAILED
                        // reattach preserves the RAW entry (F1 data-loss
                        // protection — the on-disk tags stay authoritative for
                        // the next restart).
                        let cleaned =
                            untag_stale_workflow(entry, &active_run_ids);
                        let entry_for_attach = cleaned.as_ref().unwrap_or(entry);
                        match self.try_reattach_remote_session(
                            entry_for_attach,
                            &ws,
                            (cols, rows),
                        ) {
                            Ok(ts) => ws.sessions.push(ts),
                            Err(_) => {
                                // Attach failed on a REACHABLE host (for_host
                                // succeeded above) — session gone OR a transient
                                // transport hiccup. Either way preserve THIS
                                // entry only; do NOT mark the host unreachable —
                                // sibling live sessions on it must still reattach,
                                // and the deferred-reattach drain retries it.
                                eprintln!(
                                    "cm-tui: skip restore of remote session {} \
                                     ({}) on host {} (session gone; entry \
                                     preserved for next save)",
                                    entry.uid,
                                    entry.label,
                                    entry.host_id.as_str(),
                                );
                                self.skipped_manifest_entries
                                    .entry(ws.id.clone())
                                    .or_default()
                                    .push(entry.clone());
                            }
                        }
                        continue;
                    }
                    // 10d-3 R3 in-place untag: if this session's
                    // workflow_run_id references a non-active
                    // run (Detached/Done), clone the entry and
                    // clear the tags before spawning. Without
                    // this the spawned agent inherits stale
                    // `CM_WORKFLOW_RUN_ID` / `CM_ROLE` in its
                    // MCP env (set by `mcp_config::WorkflowMeta`
                    // at spawn time).
                    let cleaned = untag_stale_workflow(entry, &active_run_ids);
                    let entry_to_spawn = cleaned.as_ref().unwrap_or(entry);
                    // migrate-tui-local Issue 1: pass the live-
                    // daemon-UID set so the restore can attach
                    // to surviving daemon sessions instead of
                    // colliding on start_session.
                    //
                    // migrate-tui-local Issue J: the returned
                    // `RestoreOutcome` distinguishes Attached
                    // (daemon kept the session across restart)
                    // from Spawned (fresh start_session). The
                    // Codex JSONL rebind primer inside
                    // `spawn_restored_session` consults this so
                    // attached restores skip the rebind window
                    // — the daemon's transcript binding is
                    // already authoritative. `_outcome` is
                    // pulled here so future call sites can
                    // log/observe it; the gating itself lives
                    // inside spawn_restored_session.
                    if let Some((ts, _outcome)) = self.spawn_restored_session(
                        entry_to_spawn,
                        &ws,
                        (cols, rows),
                        &live_daemon_uids,
                    ) {
                        ws.sessions.push(ts);
                    }
                }
            }
            self.workspaces.push(ws);
        }

        // (10d-3 R3 recovery moved to in-place pre-spawn untag
        // above; see the `active_run_ids` block + `cleaned_entry`
        // Cow. Pre-r2 the untag ran HERE — after the spawn loop —
        // which meant `spawn_restored_session` had already fed
        // stale `workflow_run_id` / `workflow_role` into the
        // agent's MCP env via `WorkflowMeta`. Now the cleaning
        // happens on the `ManifestEntry` clone passed into spawn,
        // so the MCP env never sees a stale tag.)

        // Apply task bindings onto any existing TaskEntries (from the API
        // fetch). Tasks that aren't in self.tasks yet (task still backlog
        // or API hasn't come back) will get their workspace_id set later
        // in reconcile_tasks when they arrive.
        for (task_id, ws_id) in &manifest.bindings {
            if let Some(task) = self
                .tasks
                .iter_mut()
                .find(|t| t.task_id.as_deref() == Some(task_id.as_str()))
            {
                task.workspace_id = Some(ws_id.clone());
            }
        }

        // If we restored sessions, put cursor on the first workspace with one.
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if !ws.sessions.is_empty() {
                self.cursor = Cursor::Session(wi, 0);
                break;
            }
        }

        // 10d-1 startup-ordering fix: `drain_backend_events` fires
        // `reconcile_tasks` BEFORE this hydration runs on the first
        // `TasksUpdated`, and that path's `push_state_to_daemon`
        // call would otherwise send the daemon a full-replace
        // snapshot of empty `tui_sessions` plus a workspaces map
        // missing every restored entry — semantically a lie about
        // TUI state, and once 10d-2 wires the workflow-method auth
        // consumer to `tui_sessions`, every TUI-minted session
        // restored from manifest would be rejected as "caller
        // session not found" until some later mutation triggered a
        // re-push. Push here so the populated state always lands.
        // Idempotent: the second push fully replaces the first
        // (full-replace semantics — see
        // `rpc_tui_update_sessions_snapshot_full_replace`).
        self.push_state_to_daemon();
        // Part 1: surface agent-spawned ("phantom") daemon sessions that
        // aren't in the restored manifest (e.g. an agent spawned a worker
        // while the TUI was down). Runs after restore so manifest-restored
        // sessions are already tracked and won't be double-adopted.
        self.adopt_untracked_daemon_sessions();
    }

    /// Throttled entry point for daemon-session adoption, called from the
    /// main tick. Bounds the `list_sessions` RPC to once per
    /// `ADOPT_SCAN_INTERVAL` so the scan cost stays off the hot path.
    pub fn maybe_adopt_daemon_sessions(&mut self) {
        // Drain the off-thread session pollers into the per-host cache FIRST
        // (cheap, non-blocking). The remote branch of the adopt scan reads this
        // cache instead of issuing a synchronous remote `list_sessions` RPC on
        // the main thread. Keep the latest list per host (later sends supersede
        // earlier). Collect-then-insert to avoid an immutable+mutable `self`
        // borrow overlap.
        let drained: Vec<(
            cm_daemon::host_id::HostId,
            Vec<crate::client_session::DaemonSessionSummary>,
        )> = self
            .session_poll_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for (host, summaries) in drained {
            self.remote_session_lists.insert(host, summaries);
        }
        // Drain the dispatch-pending pollers likewise (cheap, non-blocking).
        // Each delivery REPLACES that host's report wholesale, so a directive
        // the orchestrator has since acked/dispatched drops off the panel.
        let dp_drained: Vec<(
            cm_daemon::host_id::HostId,
            std::collections::HashMap<
                String,
                Vec<cm_daemon::continuous::dispatch_pending::PendingIssue>,
            >,
        )> = self
            .dispatch_pending_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for (host, map) in dp_drained {
            if self.continuous_dispatch_pending.get(&host) != Some(&map) {
                self.needs_redraw = true;
            }
            self.continuous_dispatch_pending.insert(host, map);
        }
        // Never adopt before the manifest is restored. Against an empty
        // `self.workspaces` every manifest-backed daemon session looks
        // "untracked", so adoption would mint duplicate workspaces and
        // trigger a full-replace `save_session_manifest` that clobbers the
        // on-disk manifest. `maybe_restore_sessions` runs first in the main
        // loop; this guard is defense-in-depth (and pairs with the guard in
        // `save_session_manifest`).
        if !self.sessions_restored {
            return;
        }
        const ADOPT_SCAN_INTERVAL: Duration = Duration::from_secs(5);
        let now = Instant::now();
        if let Some(last) = self.last_adopt_scan {
            if now.duration_since(last) < ADOPT_SCAN_INTERVAL {
                return;
            }
        }
        self.last_adopt_scan = Some(now);
        self.adopt_untracked_daemon_sessions();
    }

    /// Surface agent-spawned ("phantom") daemon sessions in the sidebar.
    ///
    /// MCP `start_session` registers a session only in the daemon's
    /// `state.sessions`; it never reaches the TUI via `manifest.watch`
    /// (which broadcasts `state.workspaces`, not `state.sessions`), so the
    /// TUI never learns about it. This pass polls the daemon, attaches each
    /// agent-spawned session the TUI doesn't already track, and places it
    /// into the matching workspace (by `workspace_id`) so it shows up
    /// grouped under its task like a manually-launched session.
    ///
    /// Best-effort and local-host only; any RPC/attach error is skipped.
    /// Reuses the restore-path attach machinery
    /// (`try_attach_via_daemon_with_deps`) wholesale.
    fn adopt_untracked_daemon_sessions(&mut self) {
        // Surface daemon-spawned sessions (MCP agents + continuous-task
        // sessions like the bug-triage orchestrator) on EVERY configured host,
        // but NEVER block the main thread on a remote host:
        //   - LOCAL host: discover + attach SYNCHRONOUSLY here (cheap local
        //     socket; the round-trip never leaves the box).
        //   - REMOTE hosts: read the off-thread session-poller cache and DEFER
        //     each attach through `pending_remote_reattach` (the same machinery
        //     the restore path uses). A synchronous remote `list_sessions`/
        //     attach on the main thread over a slow/flaky WAN tunnel was the
        //     "TUI freezes / can't type" regression — this removes it entirely.
        let local = cm_daemon::host_id::HostId::local();
        let want = self.last_term_size;
        let (cols, rows) = self.last_term_size;

        // ===================== LOCAL host (synchronous) ======================
        let mut adopted_any = false;
        if let Some(socket) = self.host_pool.live_socket_path(&local) {
            if let Ok(summaries) = crate::client_session::rpc_list_daemon_sessions(
                &socket,
                crate::daemon_launch::operator_token(),
            ) {
                // Self-healing size reconcile (local only): re-assert this
                // terminal's pane size on any LOCAL session whose daemon PTY
                // drifted. Closes the "skinny session" gap that the best-effort
                // attach-stream resize leaves open.
                let drift_uids = {
                    let tracked_local: std::collections::HashSet<&str> = self
                        .workspaces
                        .iter()
                        .flat_map(|w| w.sessions.iter())
                        .filter(|s| s.host_id == local)
                        .map(|s| s.uid.as_str())
                        .collect();
                    Self::select_size_drift_uids(&summaries, &tracked_local, want)
                };
                for uid in &drift_uids {
                    if let Err(e) = crate::client_session::rpc_session_resize(
                        &socket,
                        crate::daemon_launch::operator_token(),
                        uid,
                        want.0,
                        want.1,
                    ) {
                        eprintln!(
                            "cm-tui: size reconcile resize {} -> {}x{} failed: {} \
                             (will retry next adopt scan)",
                            uid, want.0, want.1, e,
                        );
                    }
                }

                let tracked: std::collections::HashSet<&str> = self
                    .workspaces
                    .iter()
                    .flat_map(|w| w.sessions.iter().map(|s| s.uid.as_str()))
                    .collect();
                let adoptees = Self::select_daemon_adoptees(summaries, &tracked);
                drop(tracked);
                for s in adoptees {
                    let (target_ws_id, worktree) = self
                        .resolve_adopt_workspace(&s, &cm_daemon::host_id::HostId::local());
                    // Attach to the existing daemon session (uid-only; the
                    // daemon already owns argv/env/cwd). Transcript binding is
                    // deferred — the attach-stream replays the ring buffer.
                    let session = match try_attach_via_daemon_with_deps(
                        &self.host_pool,
                        &s.session_uid,
                        &target_ws_id,
                        &worktree,
                        &s.session_type,
                        &s.label,
                        cols,
                        rows,
                        s.task_id.as_deref(),
                        s.workflow_run_id.as_deref(),
                        s.workflow_role.as_deref(),
                        &local,
                        None,
                    ) {
                        Ok(sess) => sess,
                        // TOCTOU: the session exited between list and attach.
                        Err(_) => continue,
                    };
                    let ts = self.build_adopted_terminal_session(&s, session, &local);
                    if let Some(w) =
                        self.workspaces.iter_mut().find(|w| w.id == target_ws_id)
                    {
                        w.sessions.push(ts);
                        adopted_any = true;
                    }
                }
            }
        }
        if adopted_any {
            // Persist (round-trips to tui-sessions.json so the adopted session
            // re-attaches on restart via the restore path) + redraw.
            self.save_session_manifest();
            self.needs_redraw = true;
        }

        // ============== REMOTE hosts (off-thread cache + defer) ==============
        self.adopt_remote_via_deferred_reattach(&local);
    }

    /// Queue untracked REMOTE daemon sessions for deferred attach. Reads the
    /// off-thread session-poller cache (never a synchronous remote RPC) and
    /// pushes each new adoptee into `pending_remote_reattach`, which
    /// `drain_deferred_remote_reattach` attaches off the blocking path (gated
    /// on tunnel warmth, throttled). The workspace slot is created here (cheap,
    /// local) so the drain has somewhere to land the session.
    fn adopt_remote_via_deferred_reattach(&mut self, local: &cm_daemon::host_id::HostId) {
        // Snapshot the cache (cloned) so the per-adoptee `&mut self` workspace
        // work below doesn't conflict with borrowing `remote_session_lists`.
        let remote: Vec<(
            cm_daemon::host_id::HostId,
            Vec<crate::client_session::DaemonSessionSummary>,
        )> = self
            .remote_session_lists
            .iter()
            .filter(|(h, _)| *h != local)
            .map(|(h, s)| (h.clone(), s.clone()))
            .collect();
        if remote.is_empty() {
            return;
        }
        let tracked: std::collections::HashSet<String> = self
            .workspaces
            .iter()
            .flat_map(|w| w.sessions.iter().map(|s| s.uid.clone()))
            .collect();
        let pending: std::collections::HashSet<String> = self
            .pending_remote_reattach
            .iter()
            .map(|p| p.entry.uid.clone())
            .collect();
        // In-flight uids: dispatched to the attach worker but not yet bound into
        // a slot (so absent from BOTH `tracked` and `pending`). Without this set
        // the 5s poll re-adopts a uid during the dispatch→result window, and
        // because no workspace holds it yet, `resolve_adopt_workspace` mints a
        // FRESH synthetic workspace each time — one phantom duplicate (and, for
        // continuous sessions, a duplicate orchestrator) per poll until it binds.
        let attaching: std::collections::HashSet<String> =
            self.attaching.keys().cloned().collect();
        for (host, summaries) in remote {
            for s in summaries {
                if !Self::is_remote_adoptee(&s, &tracked, &pending, &attaching) {
                    continue;
                }
                let (target_ws_id, _worktree) = self.resolve_adopt_workspace(&s, &host);
                let entry = Self::manifest_entry_from_summary(&s, &host);
                self.pending_remote_reattach
                    .push(PendingRemoteReattach::new(target_ws_id, entry));
            }
        }
    }

    /// Resolve (target workspace id, worktree) for an adoptee, creating a
    /// synthetic workspace if needed. Shared by the local (synchronous) and
    /// remote (deferred) adoption paths. A CONTINUOUS session reuses the
    /// workspace already holding that continuous task and drops the stale dead
    /// predecessor session(s) — so a persistent-task respawn (new uid) shows
    /// ONE sidebar entry, not one per respawn. Otherwise: the existing
    /// workspace matching the daemon's `workspace_id`, else a fresh synthetic.
    fn resolve_adopt_workspace(
        &mut self,
        s: &crate::client_session::DaemonSessionSummary,
        host: &cm_daemon::host_id::HostId,
    ) -> (String, PathBuf) {
        // worktree: daemon-reported > matching workspace's > temp_dir. Must end
        // up `Some` on the workspace so the restore path re-attaches it.
        let worktree: PathBuf = s
            .worktree_path
            .clone()
            .map(PathBuf::from)
            .or_else(|| {
                s.workspace_id.as_deref().and_then(|wid| {
                    self.workspaces
                        .iter()
                        .find(|w| w.id.as_str() == wid)
                        .and_then(|w| w.worktree_path.clone())
                })
            })
            .unwrap_or_else(std::env::temp_dir);

        let continuous_ws: Option<String> =
            s.continuous_task_id.as_deref().and_then(|ct| {
                self.workspaces
                    .iter()
                    .find(|w| {
                        w.sessions
                            .iter()
                            .any(|sess| sess.continuous_task_id.as_deref() == Some(ct))
                    })
                    .map(|w| w.id.clone())
            });
        let target_ws_id: String = if let Some(wid) = continuous_ws {
            if let Some(ct) = s.continuous_task_id.as_deref() {
                if let Some(w) = self.workspaces.iter_mut().find(|w| w.id == wid) {
                    w.sessions
                        .retain(|sess| sess.continuous_task_id.as_deref() != Some(ct));
                }
            }
            wid
        } else {
            match s
                .workspace_id
                .as_deref()
                .filter(|wid| self.workspaces.iter().any(|w| w.id.as_str() == *wid))
            {
                Some(wid) => wid.to_string(),
                None => {
                    let new_id = new_workspace_id();
                    self.workspaces.push(Workspace {
                        color: None,
                        pinned: false,
                        id: new_id.clone(),
                        name: format!("agent: {}", s.label),
                        is_closed: false,
                        is_cloud: false,
                        repo_url: None,
                        worktree_path: Some(worktree.clone()),
                        main_repo_path: None,
                        worker_vm: None,
                        worker_zone: None,
                        // The synthetic workspace adopts the session's host.
                        host_id: host.clone(),
                        sessions: Vec::new(),
                        tombstones: Vec::new(),
                        is_pushing: false,
                    });
                    new_id
                }
            }
        };
        (target_ws_id, worktree)
    }

    /// Build a freshly-adopted `TerminalSession` from a daemon summary + an
    /// already-opened attach `session`, on `host`. Used by the LOCAL adopt path
    /// (the remote path defers the attach + builds the slot in the drain).
    fn build_adopted_terminal_session(
        &self,
        s: &crate::client_session::DaemonSessionSummary,
        session: crate::session::Session,
        host: &cm_daemon::host_id::HostId,
    ) -> TerminalSession {
        TerminalSession {
            color: None,
            uid: s.session_uid.clone(),
            label: s.label.clone(),
            session_type: normalize_session_type_to_internal(&s.session_type).to_string(),
            session,
            status: SessionStatus::Running,
            idle_since: None,
            last_write_at: None,
            transcript_id: None,
            generation: 0,
            pending_jsonl_files: None,
            hidden: false,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: s.workflow_run_id.clone(),
            workflow_role: s.workflow_role.clone(),
            continuous_task_id: s.continuous_task_id.clone(),
            last_delivery: None,
            task_id: s.task_id.clone(),
            notify_on_idle: false,
            // Carry the grant from the adopted daemon-owned session
            // so the TUI's send_input auth agrees with the daemon.
            global_perms: s.global_perms,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: s.managed_by_uid.clone(),
            seeded_from_snapshot: None,
            preserved_last_exit: None,
            host_id: host.clone(),
        }
    }

    /// Build a `ManifestEntry` from a daemon session summary for a REMOTE
    /// adoptee, so it can ride `pending_remote_reattach` → the deferred-reattach
    /// drain (which attaches off the main thread). Mirrors
    /// `TerminalSession::to_manifest_entry`'s field mapping.
    fn manifest_entry_from_summary(
        s: &crate::client_session::DaemonSessionSummary,
        host: &cm_daemon::host_id::HostId,
    ) -> cm_daemon::manifest::ManifestEntry {
        cm_daemon::manifest::ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: s.session_uid.clone(),
            managed_by_uid: s.managed_by_uid.clone(),
            generation: 0,
            label: s.label.clone(),
            session_type: normalize_session_type_to_internal(&s.session_type).to_string(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            burst_threshold: 0,
            workflow_run_id: s.workflow_run_id.clone(),
            workflow_role: s.workflow_role.clone(),
            continuous_task_id: s.continuous_task_id.clone(),
            task_id: s.task_id.clone(),
            notify_on_idle: false,
            global_perms: s.global_perms,
            seeded_from_snapshot: None,
            last_exit: None,
            host_id: host.clone(),
        }
    }

    /// Gate for the REMOTE deferred-adopt path: a summary is adopted iff it's
    /// agent-spawned (`managed_by_uid`) or continuous-tagged AND its uid is not
    /// already represented ANYWHERE in the adopt pipeline — bound in a slot
    /// (`tracked`), queued for a deferred attach (`pending`), or in-flight in the
    /// off-thread attach worker (`attaching`). The `attaching` check is the one
    /// that prevents phantom duplicates: during the dispatch→result window a uid
    /// is in neither `tracked` nor `pending`, so without it the 5s poll re-adopts
    /// and mints a fresh synthetic workspace each tick. Pure so the dedup is
    /// unit-testable without a live daemon.
    fn is_remote_adoptee(
        s: &crate::client_session::DaemonSessionSummary,
        tracked: &std::collections::HashSet<String>,
        pending: &std::collections::HashSet<String>,
        attaching: &std::collections::HashSet<String>,
    ) -> bool {
        (s.managed_by_uid.is_some() || s.continuous_task_id.is_some())
            && !tracked.contains(&s.session_uid)
            && !pending.contains(&s.session_uid)
            && !attaching.contains(&s.session_uid)
    }

    /// Select which daemon-session summaries the adoption pass should take:
    /// agent-spawned (`managed_by_uid.is_some()`) OR continuous-tagged
    /// (`continuous_task_id.is_some()` — scheduler/operator spawns whose
    /// `managed_by_uid` is `None`), and not already tracked in any TUI
    /// workspace. Plain TUI/operator spawns (both fields `None`) are excluded —
    /// they're tracked through the normal spawn path. Pure (no `self`) so the
    /// gate + dedup is unit-testable without a live daemon.
    fn select_daemon_adoptees(
        summaries: Vec<crate::client_session::DaemonSessionSummary>,
        tracked_uids: &std::collections::HashSet<&str>,
    ) -> Vec<crate::client_session::DaemonSessionSummary> {
        summaries
            .into_iter()
            .filter(|s| {
                // Agent-spawned (managed_by_uid) OR a continuous-task session
                // (scheduler/operator-spawned, managed_by_uid=None) — both are
                // daemon-owned sessions the TUI never launched, so both must be
                // surfaced. Plain TUI-/operator-spawned sessions stay excluded.
                (s.managed_by_uid.is_some() || s.continuous_task_id.is_some())
                    && !tracked_uids.contains(s.session_uid.as_str())
            })
            .collect()
    }

    /// Pick which already-tracked sessions need a size re-assertion: those
    /// the daemon reports at a PTY size different from `want` (the current
    /// pane size). Skips summaries with no measurable size (older daemon)
    /// and any uid the TUI doesn't already track locally — fresh adoptees
    /// get the right size from `attach.open`, so reconciling them here would
    /// be redundant. Pure (no `self`) so the drift gate is unit-testable
    /// without a live daemon.
    fn select_size_drift_uids(
        summaries: &[crate::client_session::DaemonSessionSummary],
        tracked_local_uids: &std::collections::HashSet<&str>,
        want: (u16, u16),
    ) -> Vec<String> {
        summaries
            .iter()
            .filter_map(|s| {
                let size = (s.cols?, s.rows?);
                if tracked_local_uids.contains(s.session_uid.as_str()) && size != want {
                    Some(s.session_uid.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Spawn a session from a ManifestEntry within a Workspace context.
    /// Extracted so both restore + manual creation paths can share it.
    ///
    /// migrate-tui-local: takes `&self` so the claude/codex branches
    /// can route through `try_spawn_via_daemon` (the cloud-VM and
    /// bash branches stay local). Pre-migrate this was a free
    /// associated fn that took `config`/`cap_status`/`kill_tx` —
    /// those now come from `self` for the daemon-routed branches.
    ///
    /// migrate-tui-local Issue 1: `live_daemon_uids` is the set of
    /// session UIDs the daemon currently owns (probed once at the
    /// top of `restore_sessions`). When the entry's UID is in
    /// that set, we route through `session.attach` instead of
    /// `start_session` — the daemon survives TUI restarts and
    /// would otherwise return Conflict on the duplicate UID.
    ///
    /// migrate-tui-local Issue J: returns the outcome
    /// (`Attached` vs `Spawned`) alongside the TerminalSession
    /// so the Codex JSONL rebind primer can be skipped on the
    /// attach path. The daemon's transcript binding survived the
    /// TUI restart for attached sessions; priming
    /// `pending_jsonl_files` for Codex during the rebind window
    /// would otherwise let an unrelated rollout claim the binding
    /// and overwrite the correct transcript_id.
    pub(super) fn spawn_restored_session(
        &self,
        entry: &ManifestEntry,
        ws: &Workspace,
        (cols, rows): (u16, u16),
        live_daemon_uids: &std::collections::HashSet<String>,
    ) -> Option<(TerminalSession, RestoreOutcome)> {
        let config = &self.config;
        // migrate-tui-local Issue J: default is Spawned — the
        // common case for fresh manifest entries / clean daemon
        // restarts. The attach-success arm below upgrades to
        // Attached.
        let mut outcome = RestoreOutcome::Spawned;
        // 12e-r7 F1: the round-6 in-function host-guard moved
        // to the caller (`restore_sessions`) so the caller can
        // preserve the raw `ManifestEntry` in
        // `App.skipped_manifest_entries` for later save
        // round-trip. Pre-r7 the guard returned None here and
        // the entry was permanently dropped on the next save
        // — exactly the data-loss bug round-7 F1 closes.
        // `None` from this function now means a genuine spawn
        // failure (mcp_config, PTY, etc.), which is the
        // legitimate drop-from-disk case.

        // Resolve the UID ONCE here so the MCP config and the
        // TerminalSession agree. Earlier this had two separate
        // `new_session_uid()` calls — they generated different values
        // for legacy manifests (no `entry.uid`), which made the agent's
        // env-supplied CM_TUI_SESSION_ID never match `ts.uid` and every
        // tool call from a restored session failed `caller_ctx`.
        let restored_uid = if entry.uid.is_empty() {
            new_session_uid()
        } else {
            entry.uid.clone()
        };
        let cloud_vm = ws.worker_vm.as_deref().filter(|s| !s.is_empty());
        let codex_resume_baseline =
            if entry.session_type == "codex" && entry.transcript_id.is_some() {
                ws.worktree_path
                    .as_ref()
                    .map(|p| Self::list_codex_sessions(p))
            } else {
                None
            };
        let result = if cloud_vm.is_some() && entry.session_type == "bash" {
            let vm = cloud_vm.unwrap().to_string();
            let zone = ws
                .worker_zone
                .clone()
                .unwrap_or_else(|| config.gcp_zone.clone());
            let tmux_name = &entry.label;
            let args = vec![
                "compute".to_string(),
                "ssh".to_string(),
                vm,
                format!("--zone={}", zone),
                format!("--project={}", config.gcp_project),
                "--".to_string(),
                "-t".to_string(),
                format!(
                    "TERM=xterm-256color sudo su - worker -c 'cd /workspace && tmux new-session -As {}'",
                    tmux_name
                ),
            ];
            Session::new("gcloud", &args, cols, rows, None, Default::default(), None)
        } else if matches!(entry.session_type.as_str(), "claude" | "codex" | "bash") {
            // migrate-tui-local: route claude/codex/bash restores through
            // the daemon RPC with `--resume <transcript_id>` plumbed
            // as `resume_session_id`. The daemon then registers the
            // restored session in `state.sessions` with the resumed
            // transcript bound from spawn time. Without a worktree
            // the daemon can't auto-register the workspace, so the
            // restore is skipped (matches pre-migrate behavior for
            // worktree-less manifest entries).
            //
            // migrate-tui-local Issue H: bash MUST join the
            // daemon-routed branch. A-s spawns bash daemon-owned;
            // pre-fix the restore path fell back to
            // `Session::new("/bin/bash", ...)` and produced a
            // local TUI-owned session — silently breaking the
            // "every local session is daemon-owned" invariant,
            // and (worse) duplicating a live daemon bash session
            // under the same UID if the daemon survived the
            // restart. Bash carries no transcript and no resume
            // arg, so the daemon-spawn shape is the same as
            // claude/codex except `transcript_id` /
            // `resume_session_id` / `pre_spawn_transcript` are
            // all None.
            let Some(wt_path) = ws.worktree_path.as_deref() else {
                return None;
            };
            let session_uid_for_mcp = restored_uid.clone();
            // migrate-tui-local Issue 3: manifest entries with a
            // known transcript_id can hand the deterministic
            // claude path to the daemon up front so MCP
            // `read_session_output` can serve the restored
            // transcript without waiting for the post-spawn
            // detector. Codex resumes get None — the new
            // rollout id is fresh and only discoverable post-
            // spawn.
            let pre_spawn_transcript =
                entry.transcript_id.as_deref().and_then(|sid| {
                    pre_spawn_transcript_path(&entry.session_type, wt_path, sid)
                });

            // migrate-tui-local Issue 1: the daemon survives TUI
            // restarts. If the daemon already has this UID, we
            // MUST `session.attach` to the live child instead of
            // calling `start_session` (which would Conflict and
            // drop the session). Only call `start_session` when
            // the daemon doesn't know the UID (clean daemon
            // restart, fresh manifest entry, etc.).
            //
            // migrate-tui-local Issue F: the live-UID probe is a
            // snapshot. The daemon's session can exit between
            // the probe and the `session.attach` call — that
            // race used to drop the manifest entry via the bare
            // `result.ok()?` site below. Restructured so the
            // attach-Err arm falls through to the spawn helper
            // with the same args, distinct from (but symmetric
            // with) the "UID not in live set → spawn" arm. Now
            // the only way the entry gets dropped is a genuine
            // spawn failure or a misconfigured non-daemon-
            // eligible type.
            let attach_result: Option<anyhow::Result<Session>> =
                if live_daemon_uids.contains(&session_uid_for_mcp) {
                    Some(try_attach_via_daemon_with_deps(
                        &self.host_pool,
                        &session_uid_for_mcp,
                        &ws.id,
                        wt_path,
                        &entry.session_type,
                        &entry.label,
                        cols,
                        rows,
                        entry.task_id.as_deref(),
                        entry.workflow_run_id.as_deref(),
                        entry.workflow_role.as_deref(),
                        &entry.host_id,
                        pre_spawn_transcript.as_deref(),
                    ))
                } else {
                    None
                };
            match attach_result {
                Some(Ok(s)) => {
                    // migrate-tui-local Issue J: mark this
                    // restore as Attached so the post-dispatch
                    // `pending_jsonl_files` priming below knows
                    // to skip the Codex rebind window. The
                    // daemon's transcript binding survived the
                    // TUI restart; any unrelated rollout that
                    // appears now would be a category error,
                    // not a legitimate rebind candidate.
                    outcome = RestoreOutcome::Attached;
                    Ok(s)
                }
                Some(Err(attach_err)) => {
                    // Issue F: probe-vs-attach TOCTOU. The
                    // daemon-side session exited between the
                    // `rpc_list_session_uids` probe at the top
                    // of `restore_sessions` and this attach.
                    // Falling through to spawn re-creates the
                    // session under the same UID — `start_session`
                    // is now safe because the daemon's registry
                    // no longer holds the entry. Don't preserve
                    // the manifest in limbo waiting for next
                    // restart.
                    eprintln!(
                        "cm-tui: attach({}) failed after live-UID \
                         probe ({}); falling back to start_session",
                        session_uid_for_mcp, attach_err,
                    );
                    match self.try_spawn_via_daemon(
                        &session_uid_for_mcp,
                        &ws.id,
                        wt_path,
                        &entry.session_type,
                        &entry.label,
                        entry.transcript_id.as_deref(),
                        cols,
                        rows,
                        entry.task_id.as_deref(),
                        entry.workflow_run_id.as_deref(),
                        entry.workflow_role.as_deref(),
                        &entry.host_id,
                        pre_spawn_transcript.as_deref(),
                        entry.global_perms, // global_perms
                    ) {
                        Some(Ok(s)) => Ok(s),
                        Some(Err(e)) => Err(e),
                        None => {
                            // Unexpected: claude/codex/bash are daemon-eligible.
                            return None;
                        }
                    }
                }
                None => {
                    // UID not in the live-daemon set: clean
                    // start_session path. Reached either when
                    // the daemon never had this UID OR when the
                    // daemon restarted between TUI sessions and
                    // its registry is empty.
                    match self.try_spawn_via_daemon(
                        &session_uid_for_mcp,
                        &ws.id,
                        wt_path,
                        &entry.session_type,
                        &entry.label,
                        entry.transcript_id.as_deref(),
                        cols,
                        rows,
                        entry.task_id.as_deref(),
                        entry.workflow_run_id.as_deref(),
                        entry.workflow_role.as_deref(),
                        &entry.host_id,
                        pre_spawn_transcript.as_deref(),
                        entry.global_perms, // global_perms
                    ) {
                        Some(Ok(s)) => Ok(s),
                        Some(Err(e)) => Err(e),
                        None => {
                            // Unexpected: claude/codex/bash are daemon-eligible.
                            return None;
                        }
                    }
                }
            }
        } else {
            let wt = ws.worktree_path.clone();
            Session::new("/bin/bash", &[], cols, rows, wt, Default::default(), None)
        };
        let s = result.ok()?;
        // migrate-tui-local Issue J: the Codex rebind primer
        // exists for the SPAWN path — codex resume writes a
        // fresh rollout id and the post-spawn detector binds
        // the live transcript_id to that rollout. The ATTACH
        // path doesn't spawn a new rollout: the daemon's
        // existing transcript binding survived the TUI restart.
        // Priming `pending_jsonl_files` in that case would let
        // an unrelated rollout (created elsewhere on the
        // system during the rebind window) overwrite the
        // legitimate transcript_id via the detector at
        // `app.rs::6203`. Skip the primer entirely when
        // attached; the daemon's binding is authoritative.
        let pending = match outcome {
            RestoreOutcome::Attached => None,
            RestoreOutcome::Spawned => {
                if entry.transcript_id.is_some() {
                    codex_resume_baseline
                } else if matches!(entry.session_type.as_str(), "claude" | "codex") {
                    Some(Vec::new())
                } else {
                    None
                }
            }
        };
        // `restored_uid` was computed at the top of this function — same
        // value used in `session_uid_for_mcp` above. Don't generate a
        // fresh one here.
        let ts = TerminalSession {
            color: entry.color.clone(),
            uid: restored_uid,
            label: entry.label.clone(),
            session_type: entry.session_type.clone(),
            session: s,
            status: SessionStatus::Running,
            idle_since: None,
            last_write_at: None,
            transcript_id: entry.transcript_id.clone(),
            generation: entry.generation,
            pending_jsonl_files: pending,
            hidden: entry.hidden,
            idle_timeout_secs: entry.idle_timeout_secs,
            burst_threshold: entry.burst_threshold,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: entry.workflow_run_id.clone(),
            workflow_role: entry.workflow_role.clone(),
            continuous_task_id: entry.continuous_task_id.clone(),
            last_delivery: None,
            task_id: entry.task_id.clone(),
            notify_on_idle: entry.notify_on_idle,
            global_perms: entry.global_perms,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: entry.managed_by_uid.clone(),
            seeded_from_snapshot: entry.seeded_from_snapshot.clone(),
            // Preserve the daemon-written `last_exit` across the
            // load. The TUI doesn't yet inspect it; this passthrough
            // ensures the next save doesn't clobber it to None.
            preserved_last_exit: entry.last_exit.clone(),
            // 12b: read-through from the manifest. Pre-12 entries
            // already had `host_id` filled with `HostId::local()`
            // by the `#[serde(default)]` constructor; nothing
            // special to do here.
            host_id: entry.host_id.clone(),
        };
        Some((ts, outcome))
    }
}

#[cfg(test)]
mod adopt_daemon_session_tests {
    //! Pins the adoption gate that surfaces agent-spawned ("phantom")
    //! daemon sessions in the sidebar: only `managed_by_uid.is_some()`
    //! (agent-spawned) AND not-already-tracked sessions are adopted.
    use super::App;
    use crate::client_session::DaemonSessionSummary;
    use std::collections::HashSet;

    fn summary(uid: &str, managed_by_uid: Option<&str>) -> DaemonSessionSummary {
        DaemonSessionSummary {
            session_uid: uid.to_string(),
            label: "w".to_string(),
            session_type: "claude-code".to_string(),
            managed_by_uid: managed_by_uid.map(str::to_string),
            workspace_id: None,
            task_id: None,
            workflow_run_id: None,
            workflow_role: None,
            worktree_path: None,
            continuous_task_id: None,
            cols: None,
            rows: None,
            global_perms: false,
        }
    }

    #[test]
    fn adopts_only_agent_spawned_and_untracked() {
        let tracked: HashSet<&str> = ["already-here"].into_iter().collect();
        let summaries = vec![
            summary("agent-1", Some("ts-parent")), // adopt
            summary("operator-1", None),           // skip: not agent-spawned
            summary("already-here", Some("ts-parent")), // skip: already tracked
        ];
        let picked = App::select_daemon_adoptees(summaries, &tracked);
        let uids: Vec<&str> = picked.iter().map(|s| s.session_uid.as_str()).collect();
        assert_eq!(uids, vec!["agent-1"]);
    }

    #[test]
    fn plain_operator_spawn_is_never_adopted() {
        // Neither agent-spawned (managed_by_uid) nor continuous-tagged → skip.
        let tracked: HashSet<&str> = HashSet::new();
        let picked = App::select_daemon_adoptees(vec![summary("x", None)], &tracked);
        assert!(
            picked.is_empty(),
            "plain TUI/operator-spawned sessions (both fields None) must not be adopted"
        );
    }

    #[test]
    fn continuous_tagged_is_adopted_even_without_managed_by() {
        // A continuous-task session is scheduler/operator-spawned, so its
        // managed_by_uid is None — but it must still be adopted so the
        // orchestrator surfaces in the sidebar's Continuous section.
        let tracked: HashSet<&str> = HashSet::new();
        let mut s = summary("bug-triage-orch", None);
        s.continuous_task_id = Some("bug-triage".to_string());
        let picked = App::select_daemon_adoptees(vec![s], &tracked);
        let uids: Vec<&str> = picked.iter().map(|s| s.session_uid.as_str()).collect();
        assert_eq!(uids, vec!["bug-triage-orch"]);
    }

    #[test]
    fn continuous_tagged_but_tracked_is_skipped() {
        // Even continuous-tagged, an already-tracked uid is not re-adopted.
        let tracked: HashSet<&str> = ["bug-triage-orch"].into_iter().collect();
        let mut s = summary("bug-triage-orch", None);
        s.continuous_task_id = Some("bug-triage".to_string());
        let picked = App::select_daemon_adoptees(vec![s], &tracked);
        assert!(picked.is_empty(), "tracked continuous session must not double-adopt");
    }

    #[test]
    fn dedup_manifest_uids_collapses_duplicate_uid_workspaces() {
        // A manifest carrying the phantom-duplicate shape (one daemon uid in
        // several synthetic workspaces) must collapse to ONE open slot per uid
        // on load — distinct uids untouched, closed/bound workspaces preserved.
        use cm_daemon::manifest::Manifest;
        let entry = |uid: &str| {
            serde_json::json!({
                "uid": uid, "label": "x", "session_type": "claude-code",
                "transcript_id": null,
            })
        };
        let mut m: Manifest = serde_json::from_value(serde_json::json!({
            "workspaces": {
                "ws-a": {"id":"ws-a","name":"agent: orch","sessions":[entry("u-orch")]},
                "ws-b": {"id":"ws-b","name":"agent: orch","sessions":[entry("u-orch")]},
                "ws-c": {"id":"ws-c","name":"agent: orch","sessions":[entry("u-orch")]},
                "ws-keep": {"id":"ws-keep","name":"real","sessions":[entry("u-other")]},
                "ws-closed": {"id":"ws-closed","name":"x","is_closed":true,
                              "sessions":[entry("u-orch")]},
            },
            "bindings": {},
        }))
        .unwrap();

        App::dedup_manifest_uids(&mut m);

        // u-orch survives in exactly one OPEN workspace (lowest id, ws-a);
        // the emptied open duplicates ws-b/ws-c are dropped.
        assert!(m.workspaces.contains_key("ws-a"));
        assert!(!m.workspaces.contains_key("ws-b"));
        assert!(!m.workspaces.contains_key("ws-c"));
        // Distinct uid is left alone.
        assert_eq!(m.workspaces["ws-keep"].sessions[0].uid, "u-other");
        // Closed workspace preserved (explicit user action) though its dup uid
        // was stripped — stays as an empty closed slot.
        assert!(m.workspaces.contains_key("ws-closed"));
        assert!(m.workspaces["ws-closed"].sessions.is_empty());
        // Exactly one slot for u-orch remains across the whole manifest.
        let orch_slots = m
            .workspaces
            .values()
            .flat_map(|w| w.sessions.iter())
            .filter(|e| e.uid == "u-orch")
            .count();
        assert_eq!(orch_slots, 1, "one slot per daemon uid after dedup");
    }

    #[test]
    fn in_flight_attaching_uid_is_not_re_adopted() {
        // The remote deferred-adopt gate must skip a uid that's already
        // in-flight in the off-thread attach worker (`attaching`). During the
        // dispatch→result window that uid is in NEITHER `tracked` nor `pending`,
        // so without the `attaching` check the 5s poll re-adopts it every tick
        // and `resolve_adopt_workspace` mints a fresh synthetic workspace each
        // time — the phantom-duplicate / 2-orchestrators bug.
        use std::collections::HashSet as Set;
        let tracked: Set<String> = Set::new();
        let pending: Set<String> = Set::new();
        let mut s = summary("orch", None);
        s.continuous_task_id = Some("bug-triage".to_string());

        // Not yet in-flight → it IS an adoptee (proves the path is live).
        let empty: Set<String> = Set::new();
        assert!(
            App::is_remote_adoptee(&s, &tracked, &pending, &empty),
            "a fresh continuous adoptee must be picked up",
        );

        // In-flight in the attach worker → must be skipped.
        let attaching: Set<String> = ["orch".to_string()].into_iter().collect();
        assert!(
            !App::is_remote_adoptee(&s, &tracked, &pending, &attaching),
            "an in-flight (attaching) uid must NOT be re-adopted — it would mint \
             a duplicate workspace per poll",
        );
    }

    fn sized(uid: &str, cols: Option<u16>, rows: Option<u16>) -> DaemonSessionSummary {
        let mut s = summary(uid, Some("ts-parent"));
        s.cols = cols;
        s.rows = rows;
        s
    }

    #[test]
    fn size_drift_picks_only_tracked_mismatched_sessions() {
        // want = the current pane size; the reconcile re-asserts it on any
        // tracked local session whose daemon PTY drifted.
        let want = (324u16, 97u16);
        let tracked: HashSet<&str> = ["skinny", "right", "untracked-skinny"]
            .into_iter()
            .take(2) // only "skinny" and "right" are tracked
            .collect();
        let summaries = vec![
            sized("skinny", Some(80), Some(24)),   // tracked + drift → resize
            sized("right", Some(324), Some(97)),   // tracked + correct → skip
            sized("untracked-skinny", Some(80), Some(24)), // not tracked → skip (adoption sizes it)
        ];
        let picked = App::select_size_drift_uids(&summaries, &tracked, want);
        assert_eq!(picked, vec!["skinny".to_string()]);
    }

    #[test]
    fn size_drift_skips_unmeasured_sessions() {
        // An older daemon doesn't report cols/rows; we can't tell drift,
        // so we must not resize (could clobber a correct size to garbage).
        let want = (324u16, 97u16);
        let tracked: HashSet<&str> = ["a", "b"].into_iter().collect();
        let summaries = vec![
            sized("a", None, None),        // unmeasured → skip
            sized("b", Some(100), None),   // half-measured → skip
        ];
        let picked = App::select_size_drift_uids(&summaries, &tracked, want);
        assert!(picked.is_empty(), "unmeasured sessions must be skipped: {picked:?}");
    }
}
