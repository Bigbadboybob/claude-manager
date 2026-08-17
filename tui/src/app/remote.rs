//! Remote-session reattach machinery: deferred reattach queue, tunnel-death detection, reconnect retries.

use super::*;

/// Phase 4 startup-freeze fix: a remote manifest entry whose host uses a
/// transport whose dial can BLOCK (`ssh-unix` tunnel spawn ~3s / `tcp-tls`
/// connect), deferred out of `restore_sessions`' synchronous loop so the
/// first frame paints immediately. The raw entry is held here as the retry
/// worklist; an identical copy is preserved in `skipped_manifest_entries`
/// so a save during the deferral window round-trips it (no data loss). The
/// per-host `manifest.watch` consumer (its own thread) warms the tunnel and
/// `drain_deferred_remote_reattach` reattaches once it's connectable.
#[derive(Clone)]
pub(super) struct PendingRemoteReattach {
    ws_id: String,
    pub(super) entry: cm_daemon::manifest::ManifestEntry,
    /// Remote auto-reconnect bound: consecutive failed reattach attempts made
    /// while the tunnel was UP. A transient post-respawn race (the forwarded
    /// socket exists but `session.attach`/`attach.open` lost the race) fails
    /// here even though the daemon session is alive, so — like the
    /// `manifest.watch` consumer — we keep retrying instead of giving up on
    /// the first failure. Only after [`REMOTE_REATTACH_MAX_ATTEMPTS`]
    /// sustained failures do we treat the session as genuinely gone. The
    /// restore-deferred (non-reconnecting) FRESH-attach path uses this bound
    /// too — a transient failure there re-queues with `attempts + 1` rather
    /// than dropping the entry (which stranded a live session until the next
    /// restart); on give-up the raw entry stays preserved in
    /// `skipped_manifest_entries`.
    attempts: u32,
    /// Wall-clock of the last reattach attempt, for throttling. The main loop
    /// can spin at ~1ms, so without this the bound would be exhausted in
    /// milliseconds; we retry at most once per
    /// [`REMOTE_REATTACH_RETRY_INTERVAL`] (mirrors manifest.watch's backoff
    /// cadence). `None` until the first attempt.
    last_attempt_at: Option<Instant>,
}

impl PendingRemoteReattach {
    /// Fresh worklist item — no reattach attempts yet.
    pub(super) fn new(ws_id: String, entry: cm_daemon::manifest::ManifestEntry) -> Self {
        Self {
            ws_id,
            entry,
            attempts: 0,
            last_attempt_at: None,
        }
    }

    /// Target workspace of this deferred reattach. The spent-workspace sweep
    /// uses it to exempt workspaces whose sessions are merely unreachable —
    /// pending here — from being reaped as sessionless.
    pub(super) fn ws_id(&self) -> &str {
        &self.ws_id
    }
}

/// Remote auto-reconnect: retry a dead attach at most this often. The main
/// loop ticks far faster (~1ms when idle), so this throttle is what spreads
/// the give-up bound below over a meaningful window — mirroring
/// manifest.watch's bounded-backoff cadence rather than hammering the RPC.
const REMOTE_REATTACH_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Give up reconnecting (mark the slot exited) only after this many
/// CONSECUTIVE failed reattach attempts WITH THE TUNNEL UP — ~30s of
/// sustained failure at [`REMOTE_REATTACH_RETRY_INTERVAL`]. A transient
/// post-respawn race resolves well within this; a genuinely-gone daemon
/// session settles instead of spinning "⟳ reconnecting" forever.
const REMOTE_REATTACH_MAX_ATTEMPTS: u32 = 15;

impl App {
    /// Phase 4 (remote-session-execution): reattach a single REMOTE manifest
    /// entry to its live session on `entry.host_id`, ungated. Routes the
    /// attach RPCs through that host's socket via
    /// `try_attach_via_daemon_with_deps` (which dials
    /// `host_pool.for_host(&entry.host_id)`). Returns `Some(ts)` on a
    /// successful reattach; `None` on any failure (host unreachable, session
    /// gone) — the caller PRESERVES the raw entry rather than dropping it.
    /// NEVER spawns: remote re-spawn from restore is out of scope (it needs
    /// the Phase-3 create/add path), so a session that's gone is preserved
    /// for a later restart, not re-created locally with the wrong paths.
    ///
    /// The local restore path (`spawn_restored_session`) is deliberately
    /// untouched — only non-local entries reach this helper.
    pub(super) fn try_reattach_remote_session(
        &self,
        entry: &ManifestEntry,
        ws: &Workspace,
        (cols, rows): (u16, u16),
    ) -> Result<TerminalSession, crate::attach_worker::AttachFailureKind> {
        use crate::attach_worker::AttachFailureKind;
        // A reattach binds to an EXISTING daemon session by uid; an empty
        // uid (legacy entry) has nothing to attach to — treat as gone.
        if entry.uid.is_empty() {
            return Err(AttachFailureKind::SessionGone);
        }
        // The remote worktree path (the Phase-3 create/add stored it on the
        // workspace). Needed as the attach's working_dir. A missing path is a
        // setup/transient condition, not a gone session — keep retrying.
        let Some(wt) = ws.worktree_path.clone() else {
            return Err(AttachFailureKind::TransportDown);
        };

        let session = match try_attach_via_daemon_with_deps(
            &self.host_pool,
            &entry.uid,
            &ws.id,
            &wt,
            &entry.session_type,
            &entry.label,
            cols,
            rows,
            entry.task_id.as_deref(),
            entry.workflow_run_id.as_deref(),
            entry.workflow_role.as_deref(),
            &entry.host_id,
            // Transcript binding survived on the remote daemon; don't push a
            // (locally-computed, wrong-for-remote) path over it. The daemon's
            // existing binding is authoritative — same as the attached-restore
            // / adoption paths.
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                let kind = if crate::client_session::attach_failure_is_session_gone(&e) {
                    AttachFailureKind::SessionGone
                } else {
                    AttachFailureKind::TransportDown
                };
                eprintln!(
                    "cm-tui: remote reattach of session {} ({}) on host {} \
                     failed: {} ({:?}; entry preserved for next save)",
                    entry.uid,
                    entry.label,
                    entry.host_id.as_str(),
                    e,
                    kind,
                );
                return Err(kind);
            }
        };

        Ok(Self::build_remote_terminal_session(entry, session))
    }

    /// Build the `TerminalSession` slot for a remote reattach/adopt from its
    /// `ManifestEntry` + an already-opened attach `session`. Split out of
    /// `try_reattach_remote_session` so the OFF-THREAD attach worker can do the
    /// (blocking, tunnel-bound) attach while the MAIN thread does only this
    /// (cheap) slot build when the result comes back.
    fn build_remote_terminal_session(
        entry: &ManifestEntry,
        session: crate::session::Session,
    ) -> TerminalSession {
        TerminalSession {
            color: entry.color.clone(),
            uid: entry.uid.clone(),
            label: entry.label.clone(),
            session_type: entry.session_type.clone(),
            session,
            status: SessionStatus::Running,
            idle_since: None,
            last_write_at: None,
            transcript_id: entry.transcript_id.clone(),
            generation: entry.generation,
            // Attached → no local spawn detection (mirrors the
            // `RestoreOutcome::Attached` case in spawn_restored_session).
            pending_jsonl_files: None,
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
            preserved_last_exit: entry.last_exit.clone(),
            host_id: entry.host_id.clone(),
        }
    }

    /// Phase 4 startup-freeze fix: reattach remote sessions that
    /// `restore_sessions` deferred (their host's dial can block, so it was
    /// kept off the main thread). Called per tick from the main loop; a cheap
    /// no-op once the queue drains.
    ///
    /// For each queued entry we probe the host's tunnel WITHOUT spawning it
    /// (`HostPool::live_socket_path` — no `ensure_alive`); only once it's
    /// already connectable — the per-host `manifest.watch` consumer warmed it
    /// on its own thread — do we reattach, so the main thread never pays the
    /// ~3s tunnel-spawn wait. On a successful reattach the session moves from
    /// `skipped_manifest_entries` into its workspace and we persist the move.
    /// A session-gone failure drops the entry from the retry queue but LEAVES
    /// it preserved in `skipped_manifest_entries` (parity with the synchronous
    /// path's session-gone posture: no retry, no data loss). An entry whose
    /// tunnel never comes up simply stays queued (and preserved) — cheap to
    /// re-probe and never lost.
    /// Production dispatch phase of the deferred remote reattach: for each
    /// queued entry whose tunnel is ALREADY warm (non-blocking probe), hand the
    /// blocking attach to the off-thread `attach_worker` and move it to
    /// `attaching` (in-flight). Cold tunnels / missing workspaces stay queued;
    /// closed workspaces drop (their raw entry rides on disk). The ready
    /// sessions are bound by `drain_attach_results`.
    fn dispatch_deferred_remote_attaches(&mut self) {
        let now = Instant::now();
        let (cols, rows) = self.last_term_size;
        let active_run_ids: std::collections::HashSet<String> = self
            .workflow_runs
            .iter()
            .map(|r| r.run_id.clone())
            .collect();
        let pending = std::mem::take(&mut self.pending_remote_reattach);
        let mut still_pending: Vec<PendingRemoteReattach> = Vec::new();
        // At most one dispatch per host per tick. The attach WORKER re-warms a
        // dead/respawning tunnel itself (`try_attach_via_daemon_with_deps` →
        // `host_pool.for_host` → `ensure_alive`), and its `SshTunnel::spawn`
        // blocks ~1-3s; capping to one in-flight attach per host means an
        // outage triggers ONE respawn attempt per tick rather than one per
        // pending session (a genuinely-offline host would otherwise queue a
        // spawn storm on the single worker thread). Remaining sessions on the
        // same host ride the next tick.
        let mut dispatched_hosts: std::collections::HashSet<cm_daemon::host_id::HostId> =
            std::collections::HashSet::new();
        for p in pending {
            // NOTE: no `live_socket_path` gate here (removed in
            // `cm/fix-frozen-remote-session`). That non-blocking probe never
            // respawns a dead tunnel, and the only code that does
            // (`ensure_alive`) was reachable ONLY from the off-thread watch
            // consumers — so while the tunnel was down/respawning the drain
            // gated off and could not self-heal, waiting on a 30s-backed-off
            // consumer. Dispatching to the worker (which re-warms via
            // `for_host`) makes the reattach path itself rebuild the tunnel.
            let Some(ws_idx) = self.workspaces.iter().position(|w| w.id == p.ws_id) else {
                still_pending.push(p);
                continue;
            };
            if self.workspaces[ws_idx].is_closed {
                continue;
            }
            // Retry throttle: don't re-dispatch the SAME session faster than the
            // retry interval — applies to a reconnecting slot AND a
            // restore-deferred fresh attach (both carry `attempts` /
            // `last_attempt_at`), so a genuinely-gone session can't burn an
            // attach dispatch every tick. First attempt has `last_attempt_at`
            // None → not throttled.
            if let Some(last) = p.last_attempt_at {
                if now.duration_since(last) < REMOTE_REATTACH_RETRY_INTERVAL {
                    still_pending.push(p);
                    continue;
                }
            }
            let Some(worktree) = self.workspaces[ws_idx].worktree_path.clone() else {
                still_pending.push(p);
                continue;
            };
            // One dispatch per host per tick (see `dispatched_hosts`).
            if !dispatched_hosts.insert(p.entry.host_id.clone()) {
                still_pending.push(p);
                continue;
            }
            let cleaned = untag_stale_workflow(&p.entry, &active_run_ids);
            let entry_for_attach = cleaned.unwrap_or_else(|| p.entry.clone());
            let req = crate::attach_worker::AttachRequest {
                ws_id: p.ws_id.clone(),
                entry: entry_for_attach,
                worktree,
                cols,
                rows,
                attempts: p.attempts,
            };
            let dispatched = self
                .attach_worker
                .as_ref()
                .map(|w| w.request(req))
                .unwrap_or(false);
            if dispatched {
                self.attaching.insert(p.entry.uid.clone(), p);
            } else {
                still_pending.push(p); // worker gone — retry next tick
            }
        }
        self.pending_remote_reattach = still_pending;
    }

    /// Bind the results the off-thread `attach_worker` produced. Called per
    /// main-loop tick (non-blocking `try_iter`). On success: rebind an existing
    /// (reconnecting) slot in place, or create a new slot. On failure: re-queue
    /// for retry, or — past the reconnect cap — mark the slot exited.
    pub fn drain_attach_results(&mut self) {
        let results: Vec<crate::attach_worker::AttachResult> = match self.attach_worker.as_ref()
        {
            Some(w) => w.result_rx.try_iter().collect(),
            None => return,
        };
        if results.is_empty() {
            return;
        }
        let mut changed = false;
        for result in results {
            let queued = self.attaching.remove(&result.entry.uid);
            match result.session {
                Some(session) => {
                    // Record the tunnel generation this stream was dialed under
                    // (captured on the worker thread) so the stale-generation
                    // watchdog can re-queue it when the tunnel is later
                    // replaced. Fall back to a live read if the worker didn't
                    // supply one.
                    let gen = result.tunnel_generation.unwrap_or_else(|| {
                        self.host_pool.tunnel_generation(&result.entry.host_id)
                    });
                    self.attached_tunnel_generation
                        .insert(result.entry.uid.clone(), gen);
                    // Bind by uid across ALL workspaces, not just `result.ws_id`.
                    // A daemon session is unique by uid, so if a slot for it
                    // already exists ANYWHERE (a reconnect, or a stray duplicate
                    // that split into another synthetic workspace), rebind that
                    // slot in place rather than pushing a second one.
                    let existing =
                        self.workspaces.iter().enumerate().find_map(|(wi, w)| {
                            w.sessions
                                .iter()
                                .position(|s| s.uid == result.entry.uid)
                                .map(|si| (wi, si))
                        });
                    if let Some((wi, si)) = existing {
                        // Reconnect: rebind the PTY in place, preserving slot
                        // metadata. Dropping the old dead `Session` only sends
                        // `Msg::Shutdown` — it does NOT kill the daemon session.
                        let slot = &mut self.workspaces[wi].sessions[si];
                        slot.session = session;
                        slot.set_status(SessionStatus::Running);
                        self.reconnecting_sessions.remove(&result.entry.uid);
                        self.remove_skipped_entry(&result.ws_id, &result.entry.uid);
                        changed = true;
                    } else if let Some(ws_idx) =
                        self.workspaces.iter().position(|w| w.id == result.ws_id)
                    {
                        let ts =
                            Self::build_remote_terminal_session(&result.entry, session);
                        self.workspaces[ws_idx].sessions.push(ts);
                        self.remove_skipped_entry(&result.ws_id, &result.entry.uid);
                        changed = true;
                    }
                    // else: workspace vanished — drop the fresh session.
                }
                None => {
                    if let Some(mut p) = queued {
                        p.last_attempt_at = Some(Instant::now());
                        // Only a daemon-confirmed NotFound (SessionGone) burns
                        // the give-up budget. TransportDown (tunnel down /
                        // respawning, connect refused, RPC I/O timeout, any
                        // non-NotFound code) means the daemon session is almost
                        // certainly still alive — keep the slot reconnecting and
                        // retry indefinitely (the worker re-warms the tunnel on
                        // the next dispatch), so a deploy restart or tunnel churn
                        // never settles a live session to `exited`.
                        let session_gone = matches!(
                            result.failure,
                            Some(crate::attach_worker::AttachFailureKind::SessionGone)
                        );
                        if !session_gone {
                            self.pending_remote_reattach.push(p);
                            continue;
                        }
                        p.attempts = p.attempts.saturating_add(1);
                        let reconnecting =
                            self.reconnecting_sessions.contains(&result.entry.uid);
                        if p.attempts >= REMOTE_REATTACH_MAX_ATTEMPTS {
                            // Bound reached → stop retrying (both a reconnecting
                            // slot AND a restore-deferred fresh attach — pre-fix
                            // a fresh attach re-queued FOREVER, spinning on a
                            // genuinely-gone session). A reconnecting slot settles
                            // to `exited`; a fresh entry has no slot — drop it from
                            // the queue, leaving the raw entry preserved in
                            // `skipped_manifest_entries` (rides on disk).
                            if reconnecting {
                                if let Some(ws_idx) =
                                    self.workspaces.iter().position(|w| w.id == p.ws_id)
                                {
                                    if let Some(idx) = self.workspaces[ws_idx]
                                        .sessions
                                        .iter()
                                        .position(|s| s.uid == p.entry.uid)
                                    {
                                        self.workspaces[ws_idx].sessions[idx]
                                            .session
                                            .exited = true;
                                    }
                                }
                                self.reconnecting_sessions.remove(&result.entry.uid);
                            } else {
                                eprintln!(
                                    "cm-tui: deferred remote reattach of session {} \
                                     on host {} gave up after {} attempts (session \
                                     gone; entry preserved in skipped)",
                                    p.entry.uid,
                                    p.entry.host_id.as_str(),
                                    p.attempts,
                                );
                            }
                            changed = true;
                        } else {
                            self.pending_remote_reattach.push(p);
                        }
                    }
                }
            }
        }
        if changed {
            self.needs_redraw = true;
            self.save_session_manifest();
        }
    }

    /// Mark a REMOTE session's slot `reconnecting` and enqueue it into the
    /// deferred-reattach flow (idempotent per uid). Shared by the transport-EOF
    /// exit path (`drain_pty_events`) and the tunnel-generation watchdog
    /// (`requeue_stale_generation_remote_sessions`) so both route into the
    /// exact same recovery machinery. `reason` is logged. Returns true if it
    /// NEWLY enqueued the uid (so the caller can set `needs_redraw`); false if
    /// the uid was already queued.
    pub(super) fn requeue_remote_reconnect(&mut self, wi: usize, si: usize, reason: &str) -> bool {
        let ws_id = self.workspaces[wi].id.clone();
        let entry = self.workspaces[wi].sessions[si].to_manifest_entry();
        self.reconnecting_sessions.insert(entry.uid.clone());
        // Drop the stale generation record — it's re-recorded on the next
        // successful attach. Without this the watchdog would re-fire for this
        // uid every tick until the reattach lands.
        self.attached_tunnel_generation.remove(&entry.uid);
        if self
            .pending_remote_reattach
            .iter()
            .any(|p| p.entry.uid == entry.uid)
        {
            return false;
        }
        eprintln!(
            "cm-tui: remote session {} ({}) on host {} lost its attach stream \
             ({}) — marking reconnecting and requeuing for reattach",
            entry.uid,
            entry.label,
            entry.host_id.as_str(),
            reason,
        );
        self.pending_remote_reattach
            .push(PendingRemoteReattach::new(ws_id, entry));
        true
    }

    /// Catch a remote attach stream that's dead but that never produced a clean
    /// EOF — the half-open freeze (renders stale, normal indicator, dead to
    /// input; `transport_eof` never fires because there's no `read()==0`, and
    /// alacritty's EventLoop SPINS at 100% CPU on the fd's `POLLHUP`). Two
    /// triggers per attached remote session:
    ///
    ///   - **S5 socket HUP** — `session.attach_socket_hung_up()` polls the
    ///     dup'd attach fd for `POLLHUP`/`POLLERR`. This is direct + fast (<1
    ///     tick after the tunnel process dies) and is the same fd the spinning
    ///     EventLoop can't act on. On a hit we `request_shutdown` the EventLoop
    ///     to STOP the spin immediately, then re-queue.
    ///   - **S3 stale generation** — the session's dialed-under tunnel
    ///     generation is behind the host's current one (its tunnel was replaced
    ///     under it). Catches the case where the fd hasn't HUP'd yet but a
    ///     respawn already happened; `ServerAlive` turns a half-open tunnel into
    ///     a respawn within ~15s, bumping the generation.
    ///
    /// Both re-queue through the same path a transport EOF takes. Un-recorded
    /// live sessions are baselined at the current generation (safety net for
    /// attach paths that don't record explicitly). Cheap: one non-blocking
    /// `poll`, one HashMap lookup + atomic read per remote session per tick.
    pub fn requeue_stale_generation_remote_sessions(&mut self) {
        let local = cm_daemon::host_id::HostId::local();
        // (wi, si, reason). Two triggers, both meaning "this attached remote
        // stream is dead": S5 socket HUP (fast, direct — the fd this session's
        // spinning alacritty EventLoop can't act on) and S3 stale generation
        // (the tunnel was replaced under it). HUP wins the label since it's the
        // stronger signal.
        let mut dead: Vec<(usize, usize, &'static str)> = Vec::new();
        let mut baseline: Vec<(String, u64)> = Vec::new();
        for wi in 0..self.workspaces.len() {
            if self.workspaces[wi].is_closed {
                continue;
            }
            for si in 0..self.workspaces[wi].sessions.len() {
                let ts = &self.workspaces[wi].sessions[si];
                if ts.host_id == local || ts.uid.is_empty() || ts.session.exited {
                    continue;
                }
                let uid = ts.uid.clone();
                let host = ts.host_id.clone();
                // S5: probe the attach socket for POLLHUP/POLLERR while we hold
                // the borrow (cheap non-blocking poll of the dup'd fd).
                let hung_up = ts.session.attach_socket_hung_up();
                // A session already in the reconnect flow is handled there.
                if self.reconnecting_sessions.contains(&uid) {
                    continue;
                }
                let current = self.host_pool.tunnel_generation(&host);
                let stale_gen = matches!(
                    self.attached_tunnel_generation.get(&uid),
                    Some(&recorded) if current > recorded,
                );
                if hung_up {
                    dead.push((wi, si, "socket hangup (POLLHUP)"));
                } else if stale_gen {
                    dead.push((wi, si, "tunnel replaced (no EOF)"));
                } else if !self.attached_tunnel_generation.contains_key(&uid) {
                    baseline.push((uid, current));
                }
            }
        }
        for (uid, gen) in baseline {
            self.attached_tunnel_generation.insert(uid, gen);
        }
        let mut changed = false;
        for (wi, si, reason) in dead {
            // S5: stop the spinning EventLoop NOW (it's burning a core on the
            // dead fd's POLLHUP) rather than waiting for the reattach to drop
            // it. request_shutdown leaves the slot intact for the rebind.
            self.workspaces[wi].sessions[si].session.request_shutdown();
            self.workspaces[wi].sessions[si].set_status(SessionStatus::Idle);
            if self.requeue_remote_reconnect(wi, si, reason) {
                changed = true;
            }
        }
        if changed {
            self.needs_redraw = true;
        }
    }

    pub fn drain_deferred_remote_reattach(&mut self) {
        if self.pending_remote_reattach.is_empty() {
            return;
        }
        // PRODUCTION: dispatch attaches to the off-thread worker so a slow
        // tunnel never blocks the main thread; the ready sessions are bound by
        // `drain_attach_results`. The inline body below is the SYNCHRONOUS
        // fallback used by tests (which build `App` with no `attach_worker`).
        if self.attach_worker.is_some() {
            self.dispatch_deferred_remote_attaches();
            return;
        }
        let now = Instant::now();
        let (cols, rows) = self.last_term_size;
        // Parity with the synchronous restore path: clear stale workflow tags
        // before reattach so a Detached/Done run isn't pushed to the remote
        // daemon. Recomputed here (vs. threaded from restore) so it reflects
        // the run set at reattach time.
        let active_run_ids: std::collections::HashSet<String> = self
            .workflow_runs
            .iter()
            .map(|r| r.run_id.clone())
            .collect();
        let pending = std::mem::take(&mut self.pending_remote_reattach);
        let mut still_pending: Vec<PendingRemoteReattach> = Vec::new();
        let mut reattached_any = false;
        // Throttle: at most ONE remote attach per drain call (per main-loop
        // tick). The attach itself is a synchronous ~1-2s round-trip over the
        // (possibly slow) tunnel, so a burst of pending sessions — e.g. a
        // continuous orchestrator plus its just-spawned agents all surfacing on
        // first connect — would freeze the UI for SECONDS if attached together.
        // One-per-tick keeps the app interactive (input is handled between
        // ticks); the rest stay queued and attach on subsequent ticks.
        let mut attached_one = false;
        for p in pending {
            // Non-blocking liveness probe. `live_socket_path` never calls
            // `ensure_alive`, so the MAIN thread can't trigger a tunnel spawn;
            // it returns `Some` only after the consumer thread brought the
            // tunnel up. Keep the entry queued until then.
            if self.host_pool.live_socket_path(&p.entry.host_id).is_none() {
                still_pending.push(p);
                continue;
            }
            let Some(ws_idx) =
                self.workspaces.iter().position(|w| w.id == p.ws_id)
            else {
                // Workspace not present (yet); keep retrying.
                still_pending.push(p);
                continue;
            };
            // The pending window can outlive a user CLOSING this workspace
            // (and is effectively unbounded while the remote host is down), so
            // the workspace may now be closed even though restore only queued
            // OPEN workspaces. Never resurrect a live session into a workspace
            // the user explicitly closed: drop the retry but LEAVE the raw
            // entry in `skipped_manifest_entries` — closed workspaces ride
            // their entries on disk (the save path serializes the closed
            // workspace's skipped entries), so this is no data loss.
            if self.workspaces[ws_idx].is_closed {
                continue;
            }
            // One attach per tick (see `attached_one` above) — defer the rest.
            if attached_one {
                still_pending.push(p);
                continue;
            }
            // A live slot with this uid already exists. Two cases:
            if let Some(existing_idx) = self.workspaces[ws_idx]
                .sessions
                .iter()
                .position(|s| s.uid == p.entry.uid)
            {
                // Case A — RECONNECTING slot: its attach stream died
                // and `drain_pty_events` kept the slot + requeued it.
                // Rebind the PTY IN PLACE: swap the freshly-attached
                // live `Session` into the existing slot, preserving
                // all the user's slot metadata (label, transcript
                // binding, workflow tags, pending prompts). Dropping
                // the old dead-EventLoop `Session` just best-effort
                // sends `Msg::Shutdown` (see `impl Drop for Session`)
                // — it does NOT issue a daemon `kill_session`, so the
                // daemon-side PTY the fresh attach binds to is the
                // SAME still-running session. This is the seamless,
                // no-work-lost recovery.
                if self.reconnecting_sessions.contains(&p.entry.uid) {
                    // Throttle: the main loop ticks far faster (~1ms idle)
                    // than we should retry a network reattach. Skip — but KEEP
                    // queued + reconnecting — until the retry interval elapses
                    // since the last attempt.
                    if let Some(last) = p.last_attempt_at {
                        if now.duration_since(last)
                            < REMOTE_REATTACH_RETRY_INTERVAL
                        {
                            still_pending.push(p);
                            continue;
                        }
                    }
                    let cleaned = untag_stale_workflow(&p.entry, &active_run_ids);
                    let entry_for_attach = cleaned.as_ref().unwrap_or(&p.entry);
                    attached_one = true; // this tick's one attach (see throttle)
                    let outcome = {
                        let ws_ref = &self.workspaces[ws_idx];
                        self.try_reattach_remote_session(
                            entry_for_attach,
                            ws_ref,
                            (cols, rows),
                        )
                    };
                    match outcome {
                        Ok(fresh) => {
                            {
                                let slot = &mut self.workspaces[ws_idx]
                                    .sessions[existing_idx];
                                slot.session = fresh.session;
                                slot.set_status(SessionStatus::Running);
                            }
                            self.reconnecting_sessions.remove(&p.entry.uid);
                            self.attached_tunnel_generation.insert(
                                p.entry.uid.clone(),
                                self.host_pool.tunnel_generation(&p.entry.host_id),
                            );
                            reattached_any = true;
                            eprintln!(
                                "cm-tui: remote session {} ({}) on host {} \
                                 reattached after transport recovery (attempt {})",
                                p.entry.uid,
                                p.entry.label,
                                p.entry.host_id.as_str(),
                                p.attempts + 1,
                            );
                        }
                        Err(crate::attach_worker::AttachFailureKind::TransportDown) => {
                            // Transport hiccup (tunnel down/respawning, connect
                            // refused, RPC I/O timeout) — the daemon session is
                            // almost certainly still alive. Keep the slot
                            // reconnecting and retry WITHOUT burning the give-up
                            // budget, so a deploy restart / tunnel churn never
                            // settles a live session to `exited`. Giving up here
                            // would reintroduce the freeze.
                            eprintln!(
                                "cm-tui: reattach for remote session {} ({}) on \
                                 host {} failed (transport down) — keeping \
                                 reconnecting, will retry",
                                p.entry.uid,
                                p.entry.label,
                                p.entry.host_id.as_str(),
                            );
                            still_pending.push(PendingRemoteReattach {
                                last_attempt_at: Some(now),
                                ..p
                            });
                        }
                        Err(crate::attach_worker::AttachFailureKind::SessionGone) => {
                            // The daemon reached us and reported NotFound — the
                            // session is genuinely gone. Count toward the cap;
                            // at the cap settle the slot to `exited` (revivable
                            // via A-r).
                            let attempts = p.attempts + 1;
                            if attempts >= REMOTE_REATTACH_MAX_ATTEMPTS {
                                {
                                    let slot = &mut self.workspaces[ws_idx]
                                        .sessions[existing_idx];
                                    slot.session.exited = true;
                                }
                                self.reconnecting_sessions.remove(&p.entry.uid);
                                reattached_any = true;
                                eprintln!(
                                    "cm-tui: reconnecting remote session {} ({}) \
                                     on host {} gone after {} attempts — marking \
                                     exited",
                                    p.entry.uid,
                                    p.entry.label,
                                    p.entry.host_id.as_str(),
                                    attempts,
                                );
                            } else {
                                still_pending.push(PendingRemoteReattach {
                                    attempts,
                                    last_attempt_at: Some(now),
                                    ..p
                                });
                            }
                        }
                    }
                    continue;
                }
                // Case B — already surfaced by another path (e.g. the
                // manifest.watch adoption). Don't double-add; just
                // retire the preserved copy.
                self.remove_skipped_entry(&p.ws_id, &p.entry.uid);
                continue;
            }
            // Retry throttle (parity with Case A + the off-thread dispatcher):
            // don't re-attempt a restore-deferred fresh attach faster than the
            // interval — keep it queued. First attempt has `last_attempt_at`
            // None → not throttled.
            if let Some(last) = p.last_attempt_at {
                if now.duration_since(last) < REMOTE_REATTACH_RETRY_INTERVAL {
                    still_pending.push(p);
                    continue;
                }
            }
            let cleaned = untag_stale_workflow(&p.entry, &active_run_ids);
            let entry_for_attach = cleaned.as_ref().unwrap_or(&p.entry);
            // The tunnel is warm (gated above), so `for_host` inside the helper
            // resolves the socket without a spawn wait. Two simultaneous
            // immutable borrows of `self` (the helper + the `&Workspace` arg)
            // are fine; the owned `Option<TerminalSession>` outlives them.
            attached_one = true; // this tick's one attach (see throttle)
            let outcome = {
                let ws_ref = &self.workspaces[ws_idx];
                self.try_reattach_remote_session(entry_for_attach, ws_ref, (cols, rows))
            };
            match outcome {
                Ok(ts) => {
                    self.workspaces[ws_idx].sessions.push(ts);
                    self.remove_skipped_entry(&p.ws_id, &p.entry.uid);
                    self.attached_tunnel_generation.insert(
                        p.entry.uid.clone(),
                        self.host_pool.tunnel_generation(&p.entry.host_id),
                    );
                    reattached_any = true;
                }
                Err(crate::attach_worker::AttachFailureKind::TransportDown) => {
                    // Transport hiccup (tunnel down/respawning, connect refused,
                    // RPC I/O timeout) — the daemon session is almost certainly
                    // alive. Keep retrying WITHOUT burning the give-up budget so
                    // an offline/churning host never strands a live restore-
                    // deferred session; the raw entry stays in
                    // `skipped_manifest_entries` meanwhile.
                    still_pending.push(PendingRemoteReattach {
                        last_attempt_at: Some(now),
                        ..p
                    });
                }
                Err(crate::attach_worker::AttachFailureKind::SessionGone) => {
                    // The daemon reported NotFound — genuinely gone. Count toward
                    // the cap; at the cap drop the queue entry (the raw entry is
                    // preserved in `skipped_manifest_entries`, rides on disk).
                    let attempts = p.attempts + 1;
                    if attempts >= REMOTE_REATTACH_MAX_ATTEMPTS {
                        eprintln!(
                            "cm-tui: deferred remote reattach of session {} ({}) \
                             on host {} gave up after {} attempts (session gone; \
                             entry preserved for next save)",
                            p.entry.uid,
                            p.entry.label,
                            p.entry.host_id.as_str(),
                            attempts,
                        );
                    } else {
                        still_pending.push(PendingRemoteReattach {
                            attempts,
                            last_attempt_at: Some(now),
                            ..p
                        });
                    }
                }
            }
        }
        self.pending_remote_reattach = still_pending;
        if reattached_any {
            self.needs_redraw = true;
            // Persist the skipped → live move so a restart before the next
            // natural save doesn't re-defer a session that's now attached.
            self.save_session_manifest();
        }
    }

    /// Drop the preserved copy of a manifest entry (matched by uid) from
    /// `skipped_manifest_entries[ws_id]`, removing the workspace bucket if it
    /// empties. Used by `drain_deferred_remote_reattach` once an entry is
    /// reattached into live state, so the save path doesn't double-write it
    /// (once from `ws.sessions`, once from the skipped list).
    fn remove_skipped_entry(&mut self, ws_id: &str, uid: &str) {
        if let Some(v) = self.skipped_manifest_entries.get_mut(ws_id) {
            v.retain(|e| e.uid != uid);
            if v.is_empty() {
                self.skipped_manifest_entries.remove(ws_id);
            }
        }
    }

    /// Remote auto-reconnect: drop any reconnect bookkeeping for `uid` when its
    /// session is closed/removed. Without this, closing a session during the
    /// offline window (when `kill_session` can't even reach the daemon because
    /// the tunnel is down) would leave the queued reattach work item behind,
    /// and `drain_deferred_remote_reattach` would RESURRECT the session — re-
    /// creating something the user explicitly removed — once the tunnel
    /// returns. We only touch `pending_remote_reattach` when the uid was
    /// actually reconnecting: a restore-deferred entry (queued but never
    /// surfaced as a live slot) never reaches a close path, so its work item is
    /// left intact. No-op for the common (non-reconnecting) close.
    ///
    /// `pub(crate)` so the control-socket `kill_session` handler
    /// (`control::methods`) routes its removal through here too — same
    /// resurrection guard as the operator close paths.
    pub(crate) fn forget_reconnect_state(&mut self, uid: &str) {
        if self.reconnecting_sessions.remove(uid) {
            self.pending_remote_reattach.retain(|p| p.entry.uid != uid);
        }
    }

    /// Manual "reconnect now" lever, invoked by `A-r` (refresh). The auto-
    /// reattach already recovers a remote session whose attach stream died
    /// (transport EOF → reconnecting → retry → rebind once the daemon/tunnel
    /// returns). This adds two operator overrides on top:
    ///
    ///   1. **Accelerate** in-flight reconnects — clear each queued entry's
    ///      retry throttle + reset its attempt budget, so the next drain tries
    ///      immediately and keeps trying (rather than waiting out the 2s
    ///      interval or having a partly-spent budget).
    ///   2. **Revive** remote sessions that already gave up (settled to
    ///      `exited` after the sustained-failure cap). With daemon-side session
    ///      durability the daemon may have RESTORED the session since it
    ///      settled — so clear the slot's `exited` flag, mark it reconnecting,
    ///      and requeue. A genuinely-gone session simply re-settles to exited.
    ///   3. **Force-reconnect the FOCUSED remote session** (S4), whatever state
    ///      it's in. Steps 1-2 only cover sessions already in the reconnect flow
    ///      (pending) or that gave up (exited). A session whose attach stream
    ///      died HALF-OPEN — no clean EOF and its tunnel generation not yet
    ///      bumped (so the S3 watchdog hasn't caught it) — is NEITHER, so A-r
    ///      couldn't clear it before (the reported "A-r doesn't clear a frozen
    ///      remote pane" gap). This is SURGICAL: only the one session the user
    ///      is looking at is torn down + re-queued, so A-r-as-refresh never
    ///      blips other (healthy) remote panes — unlike a blanket reconnect-all,
    ///      which is what left "all remote sessions unresponsive" before.
    ///      `cursor_session_uid` resolves the focus in BOTH the main and
    ///      continuous columns.
    ///
    /// Returns the number of sessions nudged (for the status line). No-op for
    /// local sessions (they have no remote daemon to reattach to).
    pub(super) fn nudge_remote_reconnects(&mut self) -> usize {
        // (1) Un-throttle + refresh the budget on every queued reconnect.
        for p in self.pending_remote_reattach.iter_mut() {
            p.last_attempt_at = None;
            p.attempts = 0;
        }
        let accelerated = self.pending_remote_reattach.len();

        // (2) Revive exited remote sessions. An exited session is never in
        // `reconnecting_sessions` (the cap path removes it), so no membership
        // check is needed. Clear `exited` in place, then requeue below.
        let local = cm_daemon::host_id::HostId::local();
        let mut revived: Vec<(String, ManifestEntry)> = Vec::new();
        for wi in 0..self.workspaces.len() {
            if self.workspaces[wi].is_closed {
                continue;
            }
            let ws_id = self.workspaces[wi].id.clone();
            for si in 0..self.workspaces[wi].sessions.len() {
                let ts = &mut self.workspaces[wi].sessions[si];
                if ts.host_id != local && ts.session.exited && !ts.uid.is_empty() {
                    ts.session.exited = false;
                    revived.push((ws_id.clone(), ts.to_manifest_entry()));
                }
            }
        }
        for (ws_id, entry) in &revived {
            self.reconnecting_sessions.insert(entry.uid.clone());
            if !self
                .pending_remote_reattach
                .iter()
                .any(|p| p.entry.uid == entry.uid)
            {
                self.pending_remote_reattach
                    .push(PendingRemoteReattach::new(ws_id.clone(), entry.clone()));
            }
        }
        if !revived.is_empty() {
            self.needs_redraw = true;
        }

        // (3) Force-reconnect the FOCUSED remote session (see doc comment).
        // Only fires when the focus resolves to a remote session that isn't
        // ALREADY in the reconnect flow (steps 1-2 / the S3 watchdog handle
        // those) — i.e. the attached-but-dead half-open case A-r couldn't clear.
        let mut forced = 0usize;
        if let Some(uid) = self.cursor_session_uid() {
            if !self.reconnecting_sessions.contains(&uid) {
                let found = self.workspaces.iter().enumerate().find_map(|(wi, w)| {
                    if w.is_closed {
                        return None;
                    }
                    w.sessions
                        .iter()
                        .position(|s| s.uid == uid && s.host_id != local)
                        .map(|si| (wi, si))
                });
                if let Some((wi, si)) = found {
                    // Clear a genuine exit + drop it out of the Running sort,
                    // then route through the shared reconnect helper (which
                    // marks it reconnecting, clears its stale generation record,
                    // and enqueues the rebind).
                    self.workspaces[wi].sessions[si].session.exited = false;
                    self.workspaces[wi].sessions[si].set_status(SessionStatus::Idle);
                    if self.requeue_remote_reconnect(wi, si, "A-r force reconnect") {
                        forced = 1;
                        self.needs_redraw = true;
                    }
                }
            }
        }

        accelerated + revived.len() + forced
    }

    /// Re-arm the deferred-reattach worklist for ONE workspace: accelerate any
    /// of its entries already queued in `pending_remote_reattach`, and re-queue
    /// any non-local entry that's stranded in `skipped_manifest_entries`
    /// (a deferred reattach that failed and was dropped from the retry queue,
    /// e.g. a transient post-restart attach race). Returns how many entries are
    /// now armed.
    ///
    /// Drives the `A-a`-on-an-empty-remote-workspace path: rather than spawning
    /// a junk local session, A-a asks the deferred-reattach machinery to
    /// reconnect the daemon-owned remote session for this workspace.
    pub(super) fn rearm_remote_reattach_for_workspace(&mut self, ws_id: &str) -> usize {
        let local = cm_daemon::host_id::HostId::local();
        let mut armed = 0usize;
        // (1) Accelerate entries already queued for THIS workspace.
        for p in self
            .pending_remote_reattach
            .iter_mut()
            .filter(|p| p.ws_id == ws_id)
        {
            p.last_attempt_at = None;
            p.attempts = 0;
            armed += 1;
        }
        // (2) Re-queue stranded skipped entries (preserved on disk but no
        //     longer being retried). Skip locals and any uid already pending.
        if let Some(entries) = self.skipped_manifest_entries.get(ws_id).cloned() {
            for entry in entries {
                if entry.host_id == local || entry.uid.is_empty() {
                    continue;
                }
                let already = self
                    .pending_remote_reattach
                    .iter()
                    .any(|p| p.entry.uid == entry.uid);
                if !already {
                    self.pending_remote_reattach
                        .push(PendingRemoteReattach::new(ws_id.to_string(), entry));
                    armed += 1;
                }
            }
        }
        if armed > 0 {
            self.needs_redraw = true;
        }
        armed
    }
}

/// Regression for the remote-attach auto-reconnect fix: when an
/// ATTACHED remote session's I/O stream dies (the SSH tunnel drops on
/// a connectivity blip) the daemon-side PTY + workflow keep running,
/// so the TUI must NOT tear the session slot down. Pre-fix
/// `drain_terminal_events` marked the slot `exited` on the synthesized
/// child-exit — the freeze the user had to restart out of. Post-fix
/// the latched `transport_eof` flag routes the session into the
/// reconnect path: the slot is kept, marked reconnecting, and requeued
/// into `pending_remote_reattach` so `drain_deferred_remote_reattach`
/// rebinds the PTY to the still-alive daemon session once the tunnel
/// returns.
#[cfg(test)]
mod remote_reconnect_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn manager_host() -> cm_daemon::host_id::HostId {
        cm_daemon::host_id::HostId::new("manager")
    }

    /// `App::new` with an injected 2-host pool: local (unix) +
    /// "manager" (ssh-unix, tunnel transport). The manager tunnel is
    /// never warmed (no real ssh), so `live_socket_path("manager")` is
    /// `None` throughout — exactly the "internet still down" window.
    fn app_with_manager_host(local_sock: &std::path::Path) -> App {
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        let hosts = crate::hosts::HostsConfig {
            hosts: vec![
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::local(),
                    transport: crate::hosts::HostTransport::Unix {
                        socket: local_sock.to_path_buf(),
                    },
                    default: true,
                },
                crate::hosts::HostConfig {
                    id: manager_host(),
                    transport: crate::hosts::HostTransport::SshUnix {
                        ssh_host: "cm-test-nonexistent-host".into(),
                        ssh_user: None,
                        remote_socket: PathBuf::from("/remote/daemon.sock"),
                    },
                    default: false,
                },
            ],
        };
        app.host_pool = std::sync::Arc::new(
            crate::host_pool::HostPool::from_config(&hosts).expect("pool"),
        );
        app
    }

    /// A live `TerminalSession` whose attach stream just died. A real
    /// local `Session` gives us a valid term/sender; we then REPLACE
    /// its event channel (so the child-exit is deterministic — no
    /// dependence on `/bin/true`'s real exit timing) and, when
    /// `transport_eof` is set, latch the `daemon_transport_eof` flag
    /// the attach reader would have set on a bare socket EOF.
    fn session_with_injected_exit(
        uid: &str,
        host: cm_daemon::host_id::HostId,
        transport_eof: bool,
    ) -> (
        TerminalSession,
        std::sync::mpsc::Sender<TermEvent>,
        Option<Arc<AtomicBool>>,
    ) {
        let mut session = crate::session::Session::new(
            "/bin/true",
            &[],
            80,
            24,
            None,
            HashMap::new(),
            None,
        )
        .expect("dummy session");
        let (tx, rx) = std::sync::mpsc::channel();
        session.event_rx = rx;
        let teof = if transport_eof {
            let f = Arc::new(AtomicBool::new(true));
            session.daemon_transport_eof = Some(f.clone());
            Some(f)
        } else {
            None
        };
        let ts = TerminalSession {
            color: None,
            uid: uid.into(),
            label: "claude".into(),
            session_type: "claude".into(),
            session,
            status: SessionStatus::Running,
            idle_since: None,
            last_write_at: None,
            transcript_id: None,
            generation: 0,
            pending_jsonl_files: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            task_id: None,
            last_delivery: None,
            notify_on_idle: false,
            global_perms: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
            seeded_from_snapshot: None,
            preserved_last_exit: None,
            host_id: host,
        };
        (ts, tx, teof)
    }

    fn workspace_with(ts: TerminalSession) -> Workspace {
        Workspace {
            color: None,
            pinned: false,
            id: "ws-remote".into(),
            name: "remote".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: None,
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![ts],
            tombstones: Vec::new(),
            is_pushing: false,
        }
    }

    /// THE freeze→recovery regression. RED without the fix (the
    /// detection branch in `drain_terminal_events`): the slot would be
    /// marked `exited` and never requeued. GREEN with it: the slot is
    /// preserved, flagged reconnecting, and queued for reattach.
    #[test]
    fn transport_death_on_attached_remote_session_reconnects_instead_of_freezing() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let local_sock = cm_dir.join("daemon.sock");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&local_sock);
        let (ts, tx, teof) =
            session_with_injected_exit("uid-remote", manager_host(), true);
        let teof = teof.expect("remote session has a transport_eof flag");
        app.workspaces.push(workspace_with(ts));
        // sessions_restored=false → any internal manifest save no-ops
        // (no disk writes); we assert in-memory state only.
        app.sessions_restored = false;

        // The attach stream died: the reader synthesized a child exit
        // (no End frame). Deliver it through the session's event
        // channel exactly as alacritty's EventLoop would.
        tx.send(TermEvent::Exit).expect("send exit");

        app.drain_terminal_events();

        // 1) The slot is NOT torn down — this is the freeze fix.
        let live = &app.workspaces[0].sessions[0];
        assert!(
            !live.session.exited,
            "a transport EOF on a remote attach must NOT mark the slot exited \
             — the daemon-side PTY is still alive",
        );
        // 2) Flagged reconnecting (drives the ⟳ sidebar indicator).
        assert!(
            app.reconnecting_sessions.contains("uid-remote"),
            "the session must be marked reconnecting",
        );
        // 3) Requeued into the existing deferred-reattach flow.
        assert_eq!(app.pending_remote_reattach.len(), 1);
        assert_eq!(app.pending_remote_reattach[0].entry.uid, "uid-remote");
        assert_eq!(
            app.pending_remote_reattach[0].entry.host_id,
            manager_host(),
        );
        // 4) The transport-EOF flag was consumed (read-and-clear).
        assert!(
            !teof.load(Ordering::SeqCst),
            "the transport_eof flag is cleared once consumed",
        );

        // --- recovery window: tunnel still down ---------------------
        // The per-host manifest.watch consumer hasn't warmed the
        // tunnel yet (no real ssh), so the reattach drain must PRESERVE
        // the reconnecting slot, never drop it.
        app.drain_deferred_remote_reattach();
        assert_eq!(
            app.pending_remote_reattach.len(),
            1,
            "while the tunnel is down the entry stays queued for retry",
        );
        assert!(
            app.reconnecting_sessions.contains("uid-remote"),
            "still reconnecting until the tunnel returns",
        );
        assert_eq!(
            app.workspaces[0].sessions.len(),
            1,
            "the session slot is preserved across the reconnect window",
        );
        assert!(
            !app.workspaces[0].sessions[0].session.exited,
            "the preserved slot is not exited",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Continuous-panel S2: `visual_items_continuous` nests the subtasks an
    /// orchestrator spawned (matched by `managed_by_uid`) under it, orders
    /// orchestrators by label, excludes plain sessions, and is empty when
    /// `hide_continuous`.
    #[test]
    fn visual_items_continuous_nests_subtasks_under_orchestrators() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&home.join(".cm/daemon.sock"));
        app.workspaces.clear();

        let mk = |uid: &str, cont: Option<&str>, mgr: Option<&str>, label: &str| {
            let (mut ts, _tx, _teof) = session_with_injected_exit(
                uid,
                cm_daemon::host_id::HostId::local(),
                false,
            );
            ts.continuous_task_id = cont.map(String::from);
            ts.managed_by_uid = mgr.map(String::from);
            ts.label = label.into();
            ts
        };
        // One workspace: Z-orchestrator + its 2 subtasks (out of label order),
        // a plain session, and an A-orchestrator (no subtasks).
        let mut ws = workspace_with(mk("orch-z", Some("ct-z"), None, "Z orch"));
        ws.sessions.push(mk("sub-b", None, Some("orch-z"), "BUG-2"));
        ws.sessions.push(mk("sub-a", None, Some("orch-z"), "BUG-1"));
        ws.sessions.push(mk("plain", None, None, "plain worker"));
        ws.sessions.push(mk("orch-a", Some("ct-a"), None, "A orch"));
        app.workspaces.push(ws);

        let rows = app.visual_items_continuous();
        let resolved: Vec<(&str, u8)> = rows
            .iter()
            .map(|r| {
                (
                    app.workspaces[r.ws_idx].sessions[r.sess_idx].uid.as_str(),
                    r.depth,
                )
            })
            .collect();
        // A-orch (label "A orch") first, no children; then Z-orch with BUG-1,
        // BUG-2 nested (sorted by label). Plain session excluded.
        assert_eq!(
            resolved,
            vec![("orch-a", 0), ("orch-z", 0), ("sub-a", 1), ("sub-b", 1)],
        );

        match orig {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Respawn-robust nesting: a subtask whose `managed_by_uid` points at a
    /// PRIOR orchestrator instance (now gone — the orchestrator respawned with a
    /// new uid) still nests under the live orchestrator via the TASK tree
    /// (`parent_task_id == orchestrator.task_id`). This is the BUG-007/008 case
    /// the user hit: only the current instance's child (BUG-009) nested before.
    #[test]
    fn visual_items_continuous_nests_orphaned_subtask_via_task_parent() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&home.join(".cm/daemon.sock"));
        app.workspaces.clear();

        let mk = |uid: &str,
                  cont: Option<&str>,
                  mgr: Option<&str>,
                  task: Option<&str>,
                  label: &str| {
            let (mut ts, _tx, _teof) =
                session_with_injected_exit(uid, manager_host(), false);
            ts.continuous_task_id = cont.map(String::from);
            ts.managed_by_uid = mgr.map(String::from);
            ts.task_id = task.map(String::from);
            ts.label = label.into();
            ts
        };
        let mk_task = |tid: &str, parent: Option<&str>| TaskEntry {
            task_id: Some(tid.into()),
            name: tid.into(),
            api_status: TaskStatus::Running,
            repo_url: None,
            prompt: None,
            wip_branch: None,
            session_id: None,
            blocked_at: None,
            is_cloud: false,
            workspace_id: None,
            project: None,
            parent_task_id: parent.map(String::from),
            worktree_mode: WorktreeMode::Inherit,
            metadata: None,
        };

        // Orchestrator (uid orch-NEW, task "orch-task"). Its subtask was spawned
        // by the PRIOR instance "orch-OLD" (not present) — managed_by points at
        // the dead uid, but the subtask's task parent is the orchestrator's task.
        let mut ws = workspace_with(mk("orch-NEW", Some("ct"), None, Some("orch-task"), "Orchestrator"));
        ws.sessions.push(mk("sub-orphan", None, Some("orch-OLD"), Some("sub-task"), "BUG-007"));
        app.workspaces.push(ws);
        app.tasks.push(mk_task("orch-task", None));
        app.tasks.push(mk_task("sub-task", Some("orch-task")));

        let rows = app.visual_items_continuous();
        let resolved: Vec<(&str, u8)> = rows
            .iter()
            .map(|r| {
                (
                    app.workspaces[r.ws_idx].sessions[r.sess_idx].uid.as_str(),
                    r.depth,
                )
            })
            .collect();
        assert_eq!(
            resolved,
            vec![("orch-NEW", 0), ("sub-orphan", 1)],
            "an orphaned subtask (managed_by a dead prior orchestrator uid) MUST \
             still nest under the live orchestrator via the task-tree parent link",
        );
        // And it's a continuous member → excluded from the main sidebar.
        assert!(
            app.continuous_members().contains(&(0, 1)),
            "the orphaned subtask must be classified as a continuous member",
        );

        match orig {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Dispatch-pending (○) wiring: `session_dispatch_pending` resolves the
    /// poller cache by (host, continuous_task_id) and applies the
    /// planning-liveness filter — the PERF-083 / PERF-088 exemplar from the
    /// 2026-07-18 gap: an issue with no subtask stays pending; one whose
    /// subtask maps to a live planning task drops; a report cached under a
    /// DIFFERENT host never bleeds onto this host's orchestrator row.
    #[test]
    fn session_dispatch_pending_keys_by_host_and_filters_live_subtasks() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&home.join(".cm/daemon.sock"));
        app.workspaces.clear();

        let (mut orch, _tx, _teof) =
            session_with_injected_exit("orch", manager_host(), false);
        orch.continuous_task_id = Some("perf-triage".into());
        orch.label = "Perf Triage Orchestrator".into();
        app.workspaces.push(workspace_with(orch));

        let issue = |id: &str, subtask: Option<&str>| {
            cm_daemon::continuous::dispatch_pending::PendingIssue {
                issue_id: id.into(),
                title: None,
                directive_date: "2026-07-18".into(),
                subtask_task_id: subtask.map(String::from),
            }
        };
        // The live subtask planning row that suppresses PERF-088.
        app.tasks.push(TaskEntry {
            task_id: Some("94e3b1aa".into()),
            name: "perf-088-investigate".into(),
            api_status: TaskStatus::Running,
            repo_url: None,
            prompt: None,
            wip_branch: None,
            session_id: None,
            blocked_at: None,
            is_cloud: false,
            workspace_id: None,
            project: None,
            parent_task_id: None,
            worktree_mode: WorktreeMode::Inherit,
            metadata: None,
        });
        let report: std::collections::HashMap<_, _> = [(
            "perf-triage".to_string(),
            vec![issue("PERF-088", Some("94e3b1aa")), issue("PERF-083", None)],
        )]
        .into();

        // Cached under the WRONG host → nothing renders for this session.
        app.continuous_dispatch_pending
            .insert(cm_daemon::host_id::HostId::local(), report.clone());
        let ts = &app.workspaces[0].sessions[0];
        assert!(
            app.session_dispatch_pending(ts).is_empty(),
            "a local-host report must not light a manager-host orchestrator",
        );

        // Cached under the session's host → PERF-083 only.
        app.continuous_dispatch_pending
            .insert(manager_host(), report);
        let ts = &app.workspaces[0].sessions[0];
        let pending = app.session_dispatch_pending(ts);
        assert_eq!(
            pending.iter().map(|i| i.issue_id.as_str()).collect::<Vec<_>>(),
            vec!["PERF-083"],
            "live-subtask PERF-088 must be filtered; subtask-less PERF-083 stays",
        );

        match orig {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// A subtask with MULTIPLE sessions — its agent + a bash the operator added
    /// to the SAME task — groups together: the agent (non-bash) is the depth-1
    /// anchor and the bash nests at depth 2, NOT as a flat depth-1 sibling.
    /// Other subtasks stay their own depth-1 rows. (The user-reported bug.)
    #[test]
    fn visual_items_continuous_nests_extra_session_under_its_subtask() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&home.join(".cm/daemon.sock"));
        app.workspaces.clear();

        let mk = |uid: &str,
                  cont: Option<&str>,
                  mgr: Option<&str>,
                  task: Option<&str>,
                  stype: &str,
                  label: &str| {
            let (mut ts, _tx, _teof) =
                session_with_injected_exit(uid, manager_host(), false);
            ts.continuous_task_id = cont.map(String::from);
            ts.managed_by_uid = mgr.map(String::from);
            ts.task_id = task.map(String::from);
            ts.session_type = stype.into();
            ts.label = label.into();
            ts
        };
        let mk_task = |tid: &str, parent: Option<&str>| TaskEntry {
            task_id: Some(tid.into()),
            name: tid.into(),
            api_status: TaskStatus::Running,
            repo_url: None,
            prompt: None,
            wip_branch: None,
            session_id: None,
            blocked_at: None,
            is_cloud: false,
            workspace_id: None,
            project: None,
            parent_task_id: parent.map(String::from),
            worktree_mode: WorktreeMode::Inherit,
            metadata: None,
        };

        // Orchestrator + two subtasks; the FIRST subtask (BUG-8) has BOTH its
        // agent AND a bash session the operator added to the same task.
        let mut ws = workspace_with(mk(
            "orch",
            Some("ct"),
            None,
            Some("orch-task"),
            "claude",
            "Orchestrator",
        ));
        ws.sessions
            .push(mk("agent-8", None, Some("orch"), Some("task-8"), "claude", "BUG-8"));
        ws.sessions
            .push(mk("bash-8", None, None, Some("task-8"), "bash", "bash"));
        ws.sessions
            .push(mk("agent-9", None, Some("orch"), Some("task-9"), "claude", "BUG-9"));
        app.workspaces.push(ws);
        app.tasks.push(mk_task("orch-task", None));
        app.tasks.push(mk_task("task-8", Some("orch-task")));
        app.tasks.push(mk_task("task-9", Some("orch-task")));

        let rows = app.visual_items_continuous();
        let resolved: Vec<(&str, u8)> = rows
            .iter()
            .map(|r| {
                (
                    app.workspaces[r.ws_idx].sessions[r.sess_idx].uid.as_str(),
                    r.depth,
                )
            })
            .collect();
        assert_eq!(
            resolved,
            vec![("orch", 0), ("agent-8", 1), ("bash-8", 2), ("agent-9", 1)],
            "the bash added to BUG-8's task nests UNDER its agent (depth 2), \
             while BUG-9 stays its own depth-1 row",
        );

        match orig {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Continuous sessions render ONLY in the dedicated column — never in the
    /// main sidebar, regardless of `continuous_column_on`. Column ON → shown in
    /// the column; column OFF → hidden entirely (the render gate skips the
    /// column). Either way the main sidebar excludes them, so a continuous task
    /// never shows in both places.
    #[test]
    fn continuous_column_on_moves_continuous_out_of_main_sidebar() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&home.join(".cm/daemon.sock"));
        app.workspaces.clear();
        let (mut orch, _t, _e) = session_with_injected_exit(
            "orch",
            cm_daemon::host_id::HostId::local(),
            false,
        );
        orch.continuous_task_id = Some("ct".into());
        orch.label = "orch".into();
        let mut ws = workspace_with(orch);
        let (mut plain, _t2, _e2) = session_with_injected_exit(
            "plain",
            cm_daemon::host_id::HostId::local(),
            false,
        );
        plain.label = "plain".into();
        ws.sessions.push(plain);
        app.workspaces.push(ws);

        // Regardless of the column toggle, the main sidebar NEVER carries the
        // orchestrator (Session(0,0)) nor a ContinuousHeader — but it keeps the
        // plain session (Session(0,1)). The dedicated column always carries the
        // orchestrator (its render is gated on `continuous_column_on`).
        for on in [false, true] {
            app.continuous_column_on = on;
            let main = app.visual_items();
            assert!(
                !main.iter().any(|v| matches!(v, VisualItem::ContinuousHeader)),
                "column_on={on}: main sidebar must never carry a ContinuousHeader",
            );
            assert!(
                !main.iter().any(|v| matches!(v, VisualItem::Session(0, 0))),
                "column_on={on}: the orchestrator must not appear in the main sidebar",
            );
            assert!(
                main.iter().any(|v| matches!(v, VisualItem::Session(0, 1))),
                "column_on={on}: the plain session must stay in the main sidebar",
            );
            assert!(
                !app.visual_items_continuous().is_empty(),
                "the dedicated column carries the orchestrator",
            );
        }

        match orig {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Continuous-panel S4: `A-l` steps the cursor into the continuous column
    /// (onto the first row on first entry, then onto the row you LEFT OFF at on
    /// re-entry), `A-j/k` navigate within it (wrapping), `A-h` returns to the
    /// saved main cursor, and stepping right is a no-op when the column is off.
    /// Also pins the per-column position memory + its stale-row fallback.
    #[test]
    fn column_nav_steps_into_continuous_navigates_and_returns() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&home.join(".cm/daemon.sock"));
        app.workspaces.clear();
        let (mut orch, _t, _e) = session_with_injected_exit(
            "orch",
            cm_daemon::host_id::HostId::local(),
            false,
        );
        orch.continuous_task_id = Some("ct".into());
        orch.label = "orch".into();
        let mut ws = workspace_with(orch);
        let (mut sub, _t2, _e2) = session_with_injected_exit(
            "sub",
            cm_daemon::host_id::HostId::local(),
            false,
        );
        sub.managed_by_uid = Some("orch".into());
        sub.label = "BUG-1".into();
        ws.sessions.push(sub);
        app.workspaces.push(ws);
        app.continuous_column_on = true;
        app.cursor = Cursor::Workspace(0);
        app.cursor_column = SidebarColumn::Main;

        let cur_uid = |app: &App| match &app.cursor {
            Cursor::Session(wi, si) => app.workspaces[*wi].sessions[*si].uid.clone(),
            other => format!("{:?}", other),
        };

        // A-l → into the continuous column, on the orchestrator (first row).
        app.step_column(1);
        assert_eq!(app.cursor_column, SidebarColumn::Continuous);
        assert_eq!(cur_uid(&app), "orch");

        // A-j → the subtask; A-j again → wraps back to the orchestrator.
        app.navigate(1);
        assert_eq!(cur_uid(&app), "sub");
        app.navigate(1);
        assert_eq!(cur_uid(&app), "orch");

        // Land on the subtask, then A-h → back to main (restores the saved main
        // cursor) — and stashes the continuous spot (sub).
        app.navigate(1);
        assert_eq!(cur_uid(&app), "sub");
        app.step_column(-1);
        assert_eq!(app.cursor_column, SidebarColumn::Main);
        assert_eq!(app.cursor, Cursor::Workspace(0));

        // A-l again → lands on the row we LEFT OFF at (sub), not the first row.
        app.step_column(1);
        assert_eq!(app.cursor_column, SidebarColumn::Continuous);
        assert_eq!(
            cur_uid(&app),
            "sub",
            "re-entering the column remembers the last continuous row",
        );

        // Stale-row fallback: leave (stashing sub), then remove the subtask so
        // the saved row no longer exists; re-entry falls back to the first row.
        app.step_column(-1);
        assert_eq!(app.cursor_column, SidebarColumn::Main);
        app.workspaces[0].sessions.pop(); // drop "sub"
        app.step_column(1);
        assert_eq!(app.cursor_column, SidebarColumn::Continuous);
        assert_eq!(
            cur_uid(&app),
            "orch",
            "a saved continuous row that's gone falls back to the first row",
        );
        app.step_column(-1);

        // Column off → stepping right is a no-op (stays in main).
        app.continuous_column_on = false;
        app.step_column(1);
        assert_eq!(app.cursor_column, SidebarColumn::Main);

        match orig {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Global-host removal Phase B: a "launch into existing workspace" path
    /// routes by the WORKSPACE's host. With the workspace on `manager`, the
    /// local-only guard fail-fasts on `manager` (proving it read the workspace
    /// host) instead of proceeding and mistagging the session — the latent bug
    /// this fixes.
    #[test]
    fn launch_into_workspace_guards_on_workspace_host_not_global() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&home.join(".cm/daemon.sock"));
        app.workspaces.clear();
        let (seed, _t, _e) = session_with_injected_exit(
            "seed",
            cm_daemon::host_id::HostId::local(),
            false,
        );
        let mut ws = workspace_with(seed);
        ws.id = "ws-mgr".into();
        ws.host_id = cm_daemon::host_id::HostId("manager".into()); // but workspace on manager
        ws.worktree_path = Some(std::path::PathBuf::from("/tmp/mgr-wt"));
        ws.sessions.clear();
        app.workspaces.push(ws);

        app.launch_into_workspace(
            "ws-mgr", "task-1", "Title", "https://repo", "proj", "do x", None, "claude",
        );

        let (msg, _) = app.status_msg.clone().expect("a status message was set");
        assert!(
            msg.contains("manager"),
            "fail-fast names the WORKSPACE host (manager): {}",
            msg,
        );
        let after = app.workspaces.iter().find(|w| w.id == "ws-mgr").unwrap();
        assert!(after.sessions.is_empty(), "guard fired before any spawn");

        match orig {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Reported bug: `A-a` on an EMPTY workspace whose host is REMOTE spawned a
    /// fresh LOCAL claude (in `$HOME`, since the remote worktree path doesn't
    /// exist locally) — orphaning the real daemon-owned session. `attach_active`
    /// hardcoded `spawn_host = local`. Fix: it derives the host from the
    /// WORKSPACE and, for a remote workspace, re-arms the deferred-reattach
    /// worklist (picking up an entry stranded in `skipped_manifest_entries`)
    /// instead of spawning anything local.
    #[test]
    fn attach_active_on_empty_remote_workspace_rearms_reattach_not_local_spawn() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&home.join(".cm/daemon.sock"));
        app.workspaces.clear();

        // An empty `manager` workspace whose remote session ended up stranded
        // in `skipped_manifest_entries` (the bug-001 ghost-row shape: the
        // restore-time reattach failed and the entry was dropped from the
        // retry queue, leaving the workspace with no live session).
        let (seed, _t, _e) =
            session_with_injected_exit("seed", manager_host(), false);
        let mut ws = workspace_with(seed);
        ws.id = "ws-mgr".into();
        ws.host_id = manager_host();
        ws.worktree_path = Some(std::path::PathBuf::from("/remote/only/wt"));
        ws.sessions.clear();
        app.workspaces.push(ws);

        let (ghost_ts, _t2, _e2) =
            session_with_injected_exit("uid-ghost", manager_host(), false);
        let entry = ghost_ts.to_manifest_entry();
        assert_eq!(entry.uid, "uid-ghost");
        assert_eq!(entry.host_id, manager_host());
        app.skipped_manifest_entries
            .insert("ws-mgr".into(), vec![entry]);

        // Focus the empty remote workspace and attach.
        app.cursor = Cursor::Workspace(0);
        let before_pending = app.pending_remote_reattach.len();
        app.attach_active();

        // NO local session was spawned into the workspace.
        assert!(
            app.workspaces[0].sessions.is_empty(),
            "A-a on a remote workspace MUST NOT spawn a local session",
        );
        // The stranded remote entry was re-armed for reattach.
        assert_eq!(
            app.pending_remote_reattach.len(),
            before_pending + 1,
            "the stranded skipped entry must be re-queued for deferred reattach",
        );
        assert!(
            app.pending_remote_reattach
                .iter()
                .any(|p| p.entry.uid == "uid-ghost"),
            "the re-armed entry must be the stranded ghost uid",
        );
        // Status tells the operator we're reconnecting the REMOTE session.
        let (msg, _) = app.status_msg.clone().expect("status message set");
        assert!(
            msg.contains("Reconnecting") && msg.contains("manager"),
            "status must say it's reconnecting the remote session: {}",
            msg,
        );

        match orig {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// S4: the `A-r` "reconnect now" lever (`nudge_remote_reconnects`). It
    /// REVIVES a remote session that gave up (settled to `exited` — the daemon
    /// may have restored it since), ACCELERATES an in-flight reconnect (clears
    /// the throttle + resets the attempt budget), and leaves LOCAL sessions
    /// alone (they have no remote daemon to reattach to).
    #[test]
    fn ar_nudge_revives_exited_remote_accelerates_pending_ignores_local() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let local_sock = home.join(".cm/daemon.sock");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&local_sock);
        app.sessions_restored = false;

        // (a) A remote session that GAVE UP (settled to exited).
        let (mut gaveup, _g_tx, _g_teof) =
            session_with_injected_exit("uid-gaveup", manager_host(), false);
        gaveup.session.exited = true;
        app.workspaces.push(workspace_with(gaveup));

        // (b) A LOCAL exited session — must be left alone.
        let (mut local_ts, _l_tx, _l_teof) = session_with_injected_exit(
            "uid-local",
            cm_daemon::host_id::HostId::local(),
            false,
        );
        local_ts.session.exited = true;
        app.workspaces[0].sessions.push(local_ts);

        // (c) An in-flight reconnect with a SPENT budget + recent attempt.
        let (recon, _r_tx, _r_teof) =
            session_with_injected_exit("uid-recon", manager_host(), false);
        let recon_entry = recon.to_manifest_entry();
        app.workspaces[0].sessions.push(recon);
        app.reconnecting_sessions.insert("uid-recon".into());
        let mut spent = PendingRemoteReattach::new("ws-remote".into(), recon_entry);
        spent.attempts = 10;
        spent.last_attempt_at = Some(Instant::now());
        app.pending_remote_reattach.push(spent);

        let nudged = app.nudge_remote_reconnects();
        assert!(nudged >= 2, "revived + accelerated counted");

        // (a) revived: exited cleared, reconnecting, queued.
        let g = app.workspaces[0]
            .sessions
            .iter()
            .find(|s| s.uid == "uid-gaveup")
            .unwrap();
        assert!(!g.session.exited, "exited cleared so the reconnect can rebind");
        assert!(app.reconnecting_sessions.contains("uid-gaveup"));
        assert!(app
            .pending_remote_reattach
            .iter()
            .any(|p| p.entry.uid == "uid-gaveup"));

        // (b) local untouched.
        let l = app.workspaces[0]
            .sessions
            .iter()
            .find(|s| s.uid == "uid-local")
            .unwrap();
        assert!(l.session.exited, "local sessions are never reattached");
        assert!(!app.reconnecting_sessions.contains("uid-local"));

        // (c) accelerated: throttle cleared + budget reset.
        let pr = app
            .pending_remote_reattach
            .iter()
            .find(|p| p.entry.uid == "uid-recon")
            .unwrap();
        assert_eq!(pr.attempts, 0, "A-r reset the attempt budget");
        assert!(pr.last_attempt_at.is_none(), "A-r cleared the retry throttle");

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    // --- S4: A-r force-reconnects the FOCUSED remote session ---------------

    /// THE S4 fix: A-r force-reconnects the focused remote session even when
    /// it's in the attached-but-dead LIMBO state (not exited, not reconnecting,
    /// not queued) that steps 1-2 miss — the "A-r doesn't clear a frozen remote
    /// pane" gap.
    #[test]
    fn a_r_force_reconnects_focused_limbo_remote_session() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&home.join(".cm/daemon.sock"));
        app.sessions_restored = false;
        // Attached-but-dead: not exited, not reconnecting, not queued.
        let (ts, _tx, _teof) = session_with_injected_exit("uid-limbo", manager_host(), false);
        app.workspaces.push(workspace_with(ts));
        app.cursor = Cursor::Session(0, 0); // focus it
        assert!(!app.reconnecting_sessions.contains("uid-limbo"));

        let n = app.nudge_remote_reconnects();

        assert!(n >= 1, "the forced reconnect is counted");
        assert!(
            app.reconnecting_sessions.contains("uid-limbo"),
            "A-r must force-reconnect the focused limbo remote session",
        );
        assert!(
            app.pending_remote_reattach
                .iter()
                .any(|p| p.entry.uid == "uid-limbo"),
            "and enqueue it for rebind",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Surgical: an UNFOCUSED healthy remote session is NOT torn down by A-r —
    /// only the focused one. This is what avoids the "A-r made all remote
    /// sessions unresponsive" blanket-blip behavior.
    #[test]
    fn a_r_force_reconnect_leaves_unfocused_remote_session_alone() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&home.join(".cm/daemon.sock"));
        app.sessions_restored = false;
        let (a, _a_tx, _a_teof) = session_with_injected_exit("uid-a", manager_host(), false);
        app.workspaces.push(workspace_with(a));
        let (b, _b_tx, _b_teof) = session_with_injected_exit("uid-b", manager_host(), false);
        app.workspaces[0].sessions.push(b);
        app.cursor = Cursor::Session(0, 0); // focus uid-a only

        app.nudge_remote_reconnects();

        assert!(
            app.reconnecting_sessions.contains("uid-a"),
            "focused remote session is force-reconnected",
        );
        assert!(
            !app.reconnecting_sessions.contains("uid-b"),
            "an UNFOCUSED healthy remote session must be left alone (no blanket blip)",
        );
        assert!(
            !app.pending_remote_reattach
                .iter()
                .any(|p| p.entry.uid == "uid-b"),
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// A focused LOCAL session is never force-reconnected (no remote daemon to
    /// reattach to).
    #[test]
    fn a_r_focused_local_session_not_reconnected() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&home.join(".cm/daemon.sock"));
        app.sessions_restored = false;
        let (ts, _tx, _teof) = session_with_injected_exit(
            "uid-local",
            cm_daemon::host_id::HostId::local(),
            false,
        );
        app.workspaces.push(workspace_with(ts));
        app.cursor = Cursor::Session(0, 0);

        app.nudge_remote_reconnects();

        assert!(!app.reconnecting_sessions.contains("uid-local"));
        assert!(app.pending_remote_reattach.is_empty());

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// LOCAL sessions are completely unaffected: a child exit marks
    /// them exited as before — no reconnect, no requeue.
    /// `daemon_transport_eof` is `None` for local sessions, so the
    /// transport-death branch can never fire.
    #[test]
    fn local_session_exit_is_unaffected_by_reconnect_path() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        let (ts, tx, teof) = session_with_injected_exit(
            "uid-local",
            cm_daemon::host_id::HostId::local(),
            false,
        );
        assert!(teof.is_none(), "local sessions carry no transport_eof flag");
        app.workspaces.push(workspace_with(ts));
        app.sessions_restored = false;

        tx.send(TermEvent::Exit).expect("send exit");
        app.drain_terminal_events();

        let live = &app.workspaces[0].sessions[0];
        assert!(
            live.session.exited,
            "a local child exit still marks the slot exited",
        );
        assert!(
            app.reconnecting_sessions.is_empty(),
            "local sessions never enter the reconnect path",
        );
        assert!(
            app.pending_remote_reattach.is_empty(),
            "local sessions are never requeued for remote reattach",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// "No lost work" guarantee: a queued workflow prompt must NOT be
    /// consumed (delivered against the dead EventLoop) while the remote
    /// session's attach stream is down — it stays queued for the post-rebind
    /// flush. Covers BOTH the in-tick path (the stream dies this drain) and
    /// the snapshot path (already reconnecting from a prior tick). RED without
    /// the pending-write gate: `deliver_pending_write` would `take()` the
    /// ready prompt and lose it.
    #[test]
    fn queued_prompt_survives_while_remote_session_reconnecting() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let local_sock = cm_dir.join("daemon.sock");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&local_sock);
        // Remote session with a workflow prompt queued and unconditionally
        // READY (hard_deadline in the past), so the ONLY thing that can stop
        // its delivery is the reconnect gate.
        let (mut ts, tx, _teof) =
            session_with_injected_exit("uid-remote", manager_host(), true);
        ts.pending_prompt = Some(PendingWrite {
            text: "review the unstaged diff".into(),
            submit: true,
            earliest_deliver_at: Instant::now() - Duration::from_secs(1),
            require_quiet: Duration::from_millis(0),
            hard_deadline: Instant::now() - Duration::from_millis(1),
        });
        app.workspaces.push(workspace_with(ts));
        app.sessions_restored = false;

        // Sanity: the prompt really is ready — without the gate it WOULD be
        // delivered/consumed this tick.
        assert!(
            App::ready_for_write(
                &app.workspaces[0].sessions[0].session,
                app.workspaces[0].sessions[0].pending_prompt.as_ref().unwrap(),
                Instant::now(),
            ),
            "prompt must be ready so the test actually exercises the gate",
        );

        // --- in-tick path: the stream dies THIS drain ---------------
        tx.send(TermEvent::Exit).expect("send exit");
        app.drain_terminal_events();

        assert!(
            app.reconnecting_sessions.contains("uid-remote"),
            "session entered reconnecting state",
        );
        assert!(
            app.workspaces[0].sessions[0].pending_prompt.is_some(),
            "a queued prompt must NOT be consumed in the same tick the stream \
             dies — it would be silently lost against the dead EventLoop",
        );

        // --- snapshot path: still reconnecting next tick ------------
        app.drain_terminal_events();
        assert!(
            app.workspaces[0].sessions[0].pending_prompt.is_some(),
            "the prompt must keep surviving on subsequent reconnecting ticks \
             (seeded from the reconnecting snapshot), not just the first",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Closing a session during the offline window must cancel its reconnect
    /// bookkeeping so `drain_deferred_remote_reattach` can't resurrect it once
    /// the tunnel returns. RED without the `forget_reconnect_state` call on the
    /// close path: the queued reattach work item survives the close and would
    /// re-create the session the user explicitly removed.
    #[test]
    fn closing_a_reconnecting_session_prevents_resurrection() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let local_sock = cm_dir.join("daemon.sock");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let mut app = app_with_manager_host(&local_sock);
        let (ts, _tx, _teof) =
            session_with_injected_exit("uid-remote", manager_host(), false);
        app.workspaces.push(workspace_with(ts));
        // Reconnecting state: marked + queued for reattach (what
        // drain_pty_events would have set up when the stream died).
        app.reconnecting_sessions.insert("uid-remote".to_string());
        app.pending_remote_reattach.push(PendingRemoteReattach::new(
            "ws-remote".to_string(),
            app.workspaces[0].sessions[0].to_manifest_entry(),
        ));
        app.sessions_restored = false;

        // User closes the session during the offline window.
        app.cursor = Cursor::Session(0, 0);
        app.close_active_session();

        // Both reconnect collections are cleared by the close.
        assert!(
            app.reconnecting_sessions.is_empty(),
            "the reconnecting marker must be cleared on close",
        );
        assert!(
            app.pending_remote_reattach.is_empty(),
            "the queued reattach work item must be cancelled on close",
        );
        assert!(
            app.workspaces[0].sessions.is_empty(),
            "the session slot is removed",
        );

        // The deferred drain has nothing to act on — no resurrection.
        app.drain_deferred_remote_reattach();
        assert!(
            app.workspaces[0].sessions.is_empty(),
            "a session closed while offline must NOT be resurrected on reconnect",
        );
        assert!(
            app.pending_remote_reattach.is_empty(),
            "still nothing queued after the drain",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// App + a REMOTE host on a Unix transport whose socket doesn't exist.
    /// `live_socket_path` for a UnixDirect handle is ALWAYS `Some` (it's
    /// "always bound"), so the deferred-reattach drain treats the tunnel as UP
    /// and ATTEMPTS the reattach — but the attach RPCs then fail (no daemon
    /// behind the socket). That's the transient-failure-with-tunnel-up shape
    /// the bounded retry must survive, without any real ssh.
    fn app_with_ghost_unix_host(
        cm_dir: &std::path::Path,
    ) -> (App, cm_daemon::host_id::HostId) {
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        let ghost = cm_daemon::host_id::HostId::new("ghost");
        let hosts = crate::hosts::HostsConfig {
            hosts: vec![
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::local(),
                    transport: crate::hosts::HostTransport::Unix {
                        socket: cm_dir.join("daemon.sock"),
                    },
                    default: true,
                },
                crate::hosts::HostConfig {
                    id: ghost.clone(),
                    transport: crate::hosts::HostTransport::Unix {
                        socket: cm_dir.join("ghost-nonexistent.sock"),
                    },
                    default: false,
                },
            ],
        };
        app.host_pool = std::sync::Arc::new(
            crate::host_pool::HostPool::from_config(&hosts).expect("pool"),
        );
        (app, ghost)
    }

    /// Like [`app_with_ghost_unix_host`] but the ghost host points at a
    /// specific (caller-provided) socket — used with `spawn_inproc_daemon` so
    /// an attach reaches a LIVE daemon and gets a genuine `NotFound`
    /// (classified `SessionGone`) rather than a dead-socket connect error
    /// (classified `TransportDown`).
    fn app_with_daemon_host(
        cm_dir: &std::path::Path,
        daemon_sock: &std::path::Path,
    ) -> (App, cm_daemon::host_id::HostId) {
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        let ghost = cm_daemon::host_id::HostId::new("ghost");
        let hosts = crate::hosts::HostsConfig {
            hosts: vec![
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::local(),
                    transport: crate::hosts::HostTransport::Unix {
                        socket: cm_dir.join("daemon.sock"),
                    },
                    default: true,
                },
                crate::hosts::HostConfig {
                    id: ghost.clone(),
                    transport: crate::hosts::HostTransport::Unix {
                        socket: daemon_sock.to_path_buf(),
                    },
                    default: false,
                },
            ],
        };
        app.host_pool = std::sync::Arc::new(
            crate::host_pool::HostPool::from_config(&hosts).expect("pool"),
        );
        (app, ghost)
    }

    /// A reconnecting remote session whose tunnel is UP but whose reattach
    /// attempt fails (no daemon) must KEEP retrying — not be flipped to exited
    /// on the first failure. `live_socket_path()` only proves the forwarded
    /// socket exists; the attach RPCs can still lose a transient race right
    /// after a tunnel respawn while the daemon session is alive. RED without
    /// the bounded-retry change: a single `None` marks the slot exited.
    #[test]
    fn transient_reattach_failure_keeps_retrying_not_immediately_exited() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let (mut app, ghost) = app_with_ghost_unix_host(&cm_dir);
        // Sanity: the drain WILL attempt (tunnel reported up).
        assert!(
            app.host_pool.live_socket_path(&ghost).is_some(),
            "a UnixDirect host always reports a live socket path",
        );

        let (ts, _tx, _teof) =
            session_with_injected_exit("uid-remote", ghost.clone(), false);
        let mut ws = workspace_with(ts);
        // A worktree_path so try_reattach proceeds to the (failing) dial
        // rather than short-circuiting on a missing path.
        ws.worktree_path = Some(home.join("wt"));
        app.workspaces.push(ws);
        app.reconnecting_sessions.insert("uid-remote".to_string());
        app.pending_remote_reattach.push(PendingRemoteReattach::new(
            "ws-remote".to_string(),
            app.workspaces[0].sessions[0].to_manifest_entry(),
        ));
        app.sessions_restored = false;

        // One drain tick: the reattach fails, but it's the FIRST failure.
        app.drain_deferred_remote_reattach();

        assert!(
            !app.workspaces[0].sessions[0].session.exited,
            "a transient reattach failure must NOT immediately mark the slot \
             exited — the daemon session may still be alive (post-respawn \
             race); this is the freeze the bounded retry prevents",
        );
        assert!(
            app.reconnecting_sessions.contains("uid-remote"),
            "the session stays reconnecting and keeps retrying",
        );
        assert_eq!(
            app.pending_remote_reattach.len(),
            1,
            "the reattach work item stays queued for the next attempt",
        );
        assert_eq!(
            app.pending_remote_reattach[0].attempts, 0,
            "a TRANSPORT-DOWN failure (dead socket) must NOT burn the give-up \
             budget — the daemon session is presumed alive, so it retries \
             indefinitely (cm/fix-frozen-remote-session); only a daemon-\
             confirmed NotFound counts toward the cap",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// A transport-down reattach NEVER settles to exited, no matter how long the
    /// outage lasts — the daemon session is presumed alive, so the slot stays
    /// reconnecting until the transport recovers. This is the core of the
    /// frozen-remote-session fix: a deploy restart / tunnel churn that keeps the
    /// forwarded socket un-dialable must not tear a live session down.
    #[test]
    fn transport_down_reattach_never_settles_to_exited() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let (mut app, ghost) = app_with_ghost_unix_host(&cm_dir);
        let (ts, _tx, _teof) =
            session_with_injected_exit("uid-remote", ghost.clone(), false);
        let mut ws = workspace_with(ts);
        ws.worktree_path = Some(home.join("wt"));
        app.workspaces.push(ws);
        app.reconnecting_sessions.insert("uid-remote".to_string());
        // Pre-load WAY past the old give-up bound to prove there is no cap for
        // transport-down failures.
        let mut item = PendingRemoteReattach::new(
            "ws-remote".to_string(),
            app.workspaces[0].sessions[0].to_manifest_entry(),
        );
        item.attempts = REMOTE_REATTACH_MAX_ATTEMPTS + 50;
        app.pending_remote_reattach.push(item);
        app.sessions_restored = false;

        app.drain_deferred_remote_reattach();

        assert!(
            !app.workspaces[0].sessions[0].session.exited,
            "a transport-down failure never marks the slot exited, even far past \
             the (session-gone-only) give-up bound",
        );
        assert!(
            app.reconnecting_sessions.contains("uid-remote"),
            "the slot stays reconnecting through a sustained transport outage",
        );
        assert_eq!(
            app.pending_remote_reattach.len(),
            1,
            "the work item stays queued to retry once the transport recovers",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    // --- S5: socket-HUP watchdog (spinning-reader / CPU fix) ---------------

    /// THE S5 fix: an attached remote session whose attach socket has HUNG UP
    /// (peer closed → `POLLHUP`) is re-queued for reconnect AND its EventLoop is
    /// shut down — even when the tunnel generation is CURRENT (the fd HUP'd
    /// before any respawn). RED before S5: alacritty spins on the `POLLHUP` at
    /// 100% CPU and the session freezes.
    #[test]
    fn hung_up_attach_socket_requeues_remote_session() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let cm_dir = tmp.path().join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let (mut app, ghost) = app_with_ghost_unix_host(&cm_dir);

        let (mut ts, _tx, _teof) = session_with_injected_exit("uid-hup", ghost.clone(), false);
        // Make the attach fd look hung-up: a connected pair whose peer we drop
        // → the kept end reports POLLHUP on poll.
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(b);
        ts.session.attach_hup_fd = Some(std::os::fd::OwnedFd::from(a));
        app.workspaces.push(workspace_with(ts));
        // Generation is CURRENT — so the HUP (not staleness) is the trigger.
        app.attached_tunnel_generation.insert("uid-hup".into(), 1);
        app.host_pool.set_tunnel_generation_for_test(&ghost, 1);

        app.requeue_stale_generation_remote_sessions();

        assert!(
            app.reconnecting_sessions.contains("uid-hup"),
            "a hung-up attach socket must be re-queued even at the current gen",
        );
        assert!(
            app.pending_remote_reattach
                .iter()
                .any(|p| p.entry.uid == "uid-hup"),
        );
        assert!(
            !app.attached_tunnel_generation.contains_key("uid-hup"),
            "the generation record is cleared on requeue",
        );
    }

    /// A HEALTHY (still-connected) attach socket is NOT requeued by the HUP
    /// watchdog — no false teardown of a working remote session.
    #[test]
    fn healthy_attach_socket_not_requeued_by_hup_watchdog() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let cm_dir = tmp.path().join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let (mut app, ghost) = app_with_ghost_unix_host(&cm_dir);

        let (mut ts, _tx, _teof) = session_with_injected_exit("uid-ok", ghost.clone(), false);
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        ts.session.attach_hup_fd = Some(std::os::fd::OwnedFd::from(a));
        app.workspaces.push(workspace_with(ts));
        app.attached_tunnel_generation.insert("uid-ok".into(), 1);
        app.host_pool.set_tunnel_generation_for_test(&ghost, 1);

        app.requeue_stale_generation_remote_sessions();

        assert!(
            !app.reconnecting_sessions.contains("uid-ok"),
            "a connected attach socket must NOT be torn down",
        );
        assert!(app.pending_remote_reattach.is_empty());
        drop(b); // keep the peer alive across the poll above
    }

    // --- S3: stale-tunnel-generation watchdog (half-open freeze) -----------

    /// THE S3 fix: an attached remote session whose recorded tunnel generation
    /// is behind the host's current one — its tunnel was replaced (died) with
    /// NO clean EOF, so `transport_eof` never fired — is re-queued for
    /// reconnect. RED before S3: the session sits frozen (stale render, normal
    /// indicator, dead to input) forever.
    #[test]
    fn stale_tunnel_generation_requeues_remote_session() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let cm_dir = tmp.path().join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let (mut app, ghost) = app_with_ghost_unix_host(&cm_dir);

        let (ts, _tx, _teof) = session_with_injected_exit("uid-r", ghost.clone(), false);
        app.workspaces.push(workspace_with(ts));
        // Attached under generation 1...
        app.attached_tunnel_generation.insert("uid-r".into(), 1);
        // ...but the host's tunnel has since respawned (generation 2).
        app.host_pool.set_tunnel_generation_for_test(&ghost, 2);
        assert!(!app.reconnecting_sessions.contains("uid-r"));

        app.requeue_stale_generation_remote_sessions();

        assert!(
            app.reconnecting_sessions.contains("uid-r"),
            "a stale-generation remote session must be marked reconnecting",
        );
        assert_eq!(
            app.pending_remote_reattach.len(),
            1,
            "and re-queued into the deferred-reattach flow",
        );
        assert_eq!(app.pending_remote_reattach[0].entry.uid, "uid-r");
        assert!(
            !app.attached_tunnel_generation.contains_key("uid-r"),
            "the stale record is cleared until the next successful attach \
             (else the watchdog would re-fire every tick)",
        );
    }

    /// A remote session recorded at the CURRENT generation is left alone.
    #[test]
    fn current_tunnel_generation_not_requeued() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let cm_dir = tmp.path().join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let (mut app, ghost) = app_with_ghost_unix_host(&cm_dir);
        let (ts, _tx, _teof) = session_with_injected_exit("uid-r", ghost.clone(), false);
        app.workspaces.push(workspace_with(ts));
        app.attached_tunnel_generation.insert("uid-r".into(), 3);
        app.host_pool.set_tunnel_generation_for_test(&ghost, 3);

        app.requeue_stale_generation_remote_sessions();

        assert!(
            !app.reconnecting_sessions.contains("uid-r"),
            "a session on the current generation must NOT be requeued",
        );
        assert!(app.pending_remote_reattach.is_empty());
        assert_eq!(app.attached_tunnel_generation.get("uid-r"), Some(&3));
    }

    /// An attached remote session with NO recorded generation is baselined at
    /// the current generation (safety net for attach paths that don't record
    /// explicitly), NOT requeued.
    #[test]
    fn unrecorded_remote_session_is_baselined_not_requeued() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let cm_dir = tmp.path().join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let (mut app, ghost) = app_with_ghost_unix_host(&cm_dir);
        let (ts, _tx, _teof) = session_with_injected_exit("uid-r", ghost.clone(), false);
        app.workspaces.push(workspace_with(ts));
        app.host_pool.set_tunnel_generation_for_test(&ghost, 5);
        // No record for uid-r.

        app.requeue_stale_generation_remote_sessions();

        assert!(!app.reconnecting_sessions.contains("uid-r"));
        assert!(app.pending_remote_reattach.is_empty());
        assert_eq!(
            app.attached_tunnel_generation.get("uid-r"),
            Some(&5),
            "an unrecorded live remote session is baselined at the current gen",
        );
    }

    /// Local sessions are never touched by the generation watchdog, even if a
    /// bogus generation is forced for the local host.
    #[test]
    fn local_session_ignored_by_generation_watchdog() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let cm_dir = tmp.path().join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let (mut app, _ghost) = app_with_ghost_unix_host(&cm_dir);
        let local = cm_daemon::host_id::HostId::local();
        let (ts, _tx, _teof) = session_with_injected_exit("uid-local", local.clone(), false);
        app.workspaces.push(workspace_with(ts));
        app.attached_tunnel_generation.insert("uid-local".into(), 1);
        app.host_pool.set_tunnel_generation_for_test(&local, 9);

        app.requeue_stale_generation_remote_sessions();

        assert!(
            !app.reconnecting_sessions.contains("uid-local"),
            "local sessions have no tunnel and must be ignored",
        );
        assert!(app.pending_remote_reattach.is_empty());
    }

    /// Bound check: a genuinely-gone session (daemon reachable, returns
    /// `NotFound`) settles to exited after the give-up threshold instead of
    /// spinning "⟳ reconnecting" forever. Drives a real in-proc daemon so the
    /// attach RPC returns a genuine `NotFound` → classified `SessionGone`.
    #[test]
    fn reconnect_settles_to_exited_after_session_gone() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        // A live in-proc daemon with NO sessions → any attach returns NotFound.
        let mgr_sock = cm_dir.join("manager.sock");
        let state = std::sync::Arc::new(std::sync::Mutex::new(
            cm_daemon::state::DaemonState::new(),
        ));
        let (stop, dhandle) =
            crate::app::events::pending_workflow_events_tests::spawn_inproc_daemon(mgr_sock.clone(), state);

        let (mut app, ghost) = app_with_daemon_host(&cm_dir, &mgr_sock);
        let (ts, _tx, _teof) =
            session_with_injected_exit("uid-gone", ghost.clone(), false);
        let mut ws = workspace_with(ts);
        ws.worktree_path = Some(wt);
        app.workspaces.push(ws);
        app.reconnecting_sessions.insert("uid-gone".to_string());
        // One short of the bound, no throttle delay → this drain is the FINAL
        // (cap-hitting) attempt.
        let mut item = PendingRemoteReattach::new(
            "ws-remote".to_string(),
            app.workspaces[0].sessions[0].to_manifest_entry(),
        );
        item.attempts = REMOTE_REATTACH_MAX_ATTEMPTS - 1;
        app.pending_remote_reattach.push(item);
        app.sessions_restored = false;

        app.drain_deferred_remote_reattach();

        assert!(
            app.workspaces[0].sessions[0].session.exited,
            "after the give-up bound a session-gone (NotFound) slot settles to \
             exited",
        );
        assert!(
            app.reconnecting_sessions.is_empty(),
            "the reconnecting marker is cleared on give-up",
        );
        assert!(
            app.pending_remote_reattach.is_empty(),
            "the work item is dropped from the retry queue on give-up",
        );

        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = dhandle.join();
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// The "stranding" fix + give-up bound: a FRESH restore-deferred reattach
    /// (no live slot, NOT reconnecting — the bug-001..006 ghost-row shape)
    /// against a REACHABLE daemon that reports `NotFound` (session gone) keeps
    /// retrying with a bound, then gives up — leaving the raw entry preserved
    /// in `skipped_manifest_entries` — after `REMOTE_REATTACH_MAX_ATTEMPTS`.
    /// (A transport-down failure would retry forever; here the daemon is up and
    /// the session is genuinely gone, so the cap applies.)
    #[test]
    fn fresh_deferred_reattach_session_gone_retries_then_gives_up_bounded() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        // A live in-proc daemon with NO sessions → any attach returns NotFound.
        let mgr_sock = cm_dir.join("manager.sock");
        let state = std::sync::Arc::new(std::sync::Mutex::new(
            cm_daemon::state::DaemonState::new(),
        ));
        let (stop, dhandle) =
            crate::app::events::pending_workflow_events_tests::spawn_inproc_daemon(mgr_sock.clone(), state);

        let (mut app, ghost) = app_with_daemon_host(&cm_dir, &mgr_sock);
        // An EMPTY workspace (no slot for the uid) → the drain takes the FRESH
        // attach path, not the reconnecting-slot (Case A) path.
        let (seed, _tx, _teof) =
            session_with_injected_exit("seed", ghost.clone(), false);
        let mut ws = workspace_with(seed);
        ws.id = "ws-remote".into();
        ws.worktree_path = Some(wt);
        ws.sessions.clear();
        app.workspaces.push(ws);

        let (ghost_ts, _t2, _e2) =
            session_with_injected_exit("uid-gone", ghost.clone(), false);
        let entry = ghost_ts.to_manifest_entry();
        app.skipped_manifest_entries
            .insert("ws-remote".into(), vec![entry.clone()]);
        app.pending_remote_reattach
            .push(PendingRemoteReattach::new("ws-remote".into(), entry));
        app.sessions_restored = false;
        assert!(!app.reconnecting_sessions.contains("uid-gone"));

        // First drain: NotFound (session gone) but it's the FIRST failure →
        // re-queued, NOT stranded, attempt counted.
        app.drain_deferred_remote_reattach();
        assert_eq!(
            app.pending_remote_reattach.len(),
            1,
            "a fresh attach failure must stay queued (the stranding fix), not drop",
        );
        assert_eq!(
            app.pending_remote_reattach[0].attempts, 1,
            "a session-gone (NotFound) attempt is counted toward the give-up bound",
        );
        assert!(
            app.skipped_manifest_entries.contains_key("ws-remote"),
            "the raw entry stays preserved on disk while retrying",
        );

        // Jump to one short of the bound + clear the throttle so this drain
        // makes the FINAL attempt → give up (dropped from pending, still
        // preserved in skipped).
        app.pending_remote_reattach[0].attempts = REMOTE_REATTACH_MAX_ATTEMPTS - 1;
        app.pending_remote_reattach[0].last_attempt_at = None;
        app.drain_deferred_remote_reattach();
        assert!(
            app.pending_remote_reattach.is_empty(),
            "after the sustained-failure bound the fresh entry stops retrying",
        );
        assert!(
            app.skipped_manifest_entries.contains_key("ws-remote"),
            "giving up PRESERVES the raw entry in skipped (no data loss)",
        );

        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = dhandle.join();
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// The MCP/control `kill_session` removal path must clear reconnect
    /// bookkeeping too — same resurrection guard as the operator close paths
    /// (round 2). RED without the `forget_reconnect_state` call in
    /// `control::methods::kill_session`: the queued reattach survives and the
    /// session would be resurrected when the tunnel returns.
    #[test]
    fn kill_session_control_path_clears_reconnect_state() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let (mut app, ghost) = app_with_ghost_unix_host(&cm_dir);
        // A live caller (no task scope, same workspace → authorized to kill)
        // plus the reconnecting target.
        let (caller, _c_tx, _c_t) =
            session_with_injected_exit("caller", ghost.clone(), false);
        let (target, _tx, _teof) =
            session_with_injected_exit("uid-remote", ghost.clone(), false);
        let mut ws = workspace_with(caller);
        ws.sessions.push(target);
        app.workspaces.push(ws);
        app.reconnecting_sessions.insert("uid-remote".to_string());
        app.pending_remote_reattach.push(PendingRemoteReattach::new(
            "ws-remote".to_string(),
            app.workspaces[0].sessions[1].to_manifest_entry(),
        ));
        app.sessions_restored = false;

        // Kill the reconnecting session via the control/MCP handler.
        let res = crate::control::methods::kill_session(
            &mut app,
            "caller",
            &serde_json::json!({ "session_uid": "uid-remote" }),
        );
        assert!(res.is_ok(), "kill_session should succeed: {:?}", res.err());

        assert!(
            app.reconnecting_sessions.is_empty(),
            "kill_session must clear the reconnecting marker",
        );
        assert!(
            app.pending_remote_reattach.is_empty(),
            "kill_session must cancel the queued reattach so the killed \
             session can't be resurrected on reconnect",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

/// 12a reviewer round: `App::new` falls back to
/// `HostsConfig::synthesized_local_default()` when the on-disk
/// `~/.cm/hosts.toml` is malformed. Pre-fix the fallback re-loaded
/// from `/dev/null/hosts.toml-nonexistent` and `.expect`ed
/// success; on Unix that sentinel path returns `NotADirectory`
/// rather than `NotFound`, so the `.expect` panicked and locked
/// the operator out of the TUI for any malformed config — the
/// exact failure mode the fallback was supposed to prevent.
#[cfg(test)]
mod hosts_malformed_fallback_tests {
    use super::*;

    /// Drive `App::new` with garbage at `~/.cm/hosts.toml`.
    /// Pre-fix this panics inside the fallback's `.expect`.
    /// Post-fix the synthesized-default constructor is
    /// infallible-by-construction and `app.hosts` ends up
    /// with the single local entry.
    #[test]
    fn malformed_hosts_toml_falls_back_without_panic() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let cm_dir = tmp.path().join(".cm");
        std::fs::create_dir_all(&cm_dir).expect("mkdir .cm");
        std::fs::write(
            cm_dir.join("hosts.toml"),
            b"this is not valid toml = = =",
        )
        .expect("write malformed hosts.toml");

        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        // The point of the test: `App::new` must NOT panic on a
        // malformed hosts.toml. Pre-reviewer-round-fix the
        // sentinel-path fallback's `.expect("synthesized-default
        // load is infallible")` panicked here because
        // `HostsConfig::load("/dev/null/hosts.toml-nonexistent")`
        // returned `Err(Error::Io(NotADirectory))` on Unix —
        // `/dev/null` is a device file, not a directory.
        let app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        // Capture the expected socket path WHILE $HOME is still
        // the tempdir — `cm_daemon::default_socket_path()` reads
        // $HOME at call time, so restoring HOME first would
        // resolve a different path than the one baked into
        // `app.hosts` during App::new.
        let expected_socket = cm_daemon::default_socket_path();

        if let Some(h) = orig_home {
            unsafe {
                std::env::set_var("HOME", h);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }
        // Leak the tempdir so daemon-watch threads spawned with the
        // test HOME don't error on later ~/.cm accesses.
        std::mem::forget(tmp);

        // The fallback produced the synthesized local default:
        // one entry, id="local", marked default=true, Unix
        // transport pointing at the canonical daemon socket.
        assert_eq!(
            app.hosts.hosts.len(),
            1,
            "fallback should synthesize a single local entry",
        );
        let entry = &app.hosts.hosts[0];
        assert_eq!(entry.id, crate::hosts::HostId::local());
        assert!(
            entry.default,
            "synthesized entry must be the default",
        );
        match &entry.transport {
            crate::hosts::HostTransport::Unix { socket } => {
                assert_eq!(
                    socket,
                    &expected_socket,
                    "synthesized socket must match \
                     cm_daemon::default_socket_path() resolved \
                     under the test HOME",
                );
            }
            other => panic!(
                "expected Unix transport, got {:?}",
                other,
            ),
        }
    }
}
