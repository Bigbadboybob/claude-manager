//! Backend-event plumbing: terminal/backend/control/manifest/workflow/planning drains, control dispatch, pending-write delivery + PTY encoding, activity log.

use super::*;

/// Interval between filesystem checks for session_id detection.
const SESSION_ID_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Wall-clock budget for a single `drain_terminal_events` pass. The PTY
/// reader threads push an unbounded stream of events (one `Wakeup` per
/// output batch — kHz under heavy output) into per-session channels; the
/// drain used to empty every channel completely in one tick. A session
/// flooding output (most visibly the burst right after `start_workflow`
/// spawns fresh agents) could then hold the main loop inside this single
/// phase for tens of seconds. Because the loop reaches `drain_control_events`
/// only *after* this phase returns, any control request queued during the
/// stall isn't answered until it ends — blowing the control server's 30s
/// reply timeout (MCP `start_workflow` would report "main loop did not reply
/// within timeout") and freezing the UI meanwhile. Capping the drain keeps
/// the loop cycling; events left unread stay in their channels and are
/// picked up on the next tick (~1ms later). 50ms is comfortably above a
/// normal tick's full drain, so steady state never hits the cap — only
/// pathological bursts get spread across ticks.
const TERMINAL_DRAIN_BUDGET: Duration = Duration::from_millis(50);

/// How often a TUI that failed to bind the control socket (`tui.sock`)
/// re-attempts the bind while running degraded. Short enough that recovery
/// feels instant once the conflicting instance exits, long enough that a
/// persistent conflict (e.g. an orphaned older TUI a user forgot to kill)
/// doesn't spin. See `App::maybe_rebind_control_socket`. The scenario this
/// guards: rebuild + relaunch where the old TUI lingers, keeps `tui.sock`,
/// and the fresh TUI would otherwise run silently with no control plane —
/// every MCP/agent call routing to the stale binary instead.
pub(super) const CONTROL_REBIND_INTERVAL: Duration = Duration::from_secs(2);

/// Gap between writing a workflow prompt body and the trailing Enter. Implemented
/// as a deferred write (not `thread::sleep`) so the UI thread keeps draining
/// events. Generous to leave codex's PTY paste detector no doubt about Enter
/// being a fresh keystroke.
const ENTER_GAP: Duration = Duration::from_secs(10);

/// Phase 6: format a `SystemTime` as `HH:MM:SS` in UTC. Used by the
/// activity-feed renderer; UTC keeps the implementation tiny (no chrono /
/// libc dep) and the absolute ordering of entries is what matters for
/// the feed, not local-clock alignment.
pub(super) fn format_utc_hms(ts: std::time::SystemTime) -> String {
    let secs = ts
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

/// Phase 6: build a one-line summary for the activity feed from a
/// completed control-socket method call. Returns `Some(_)` only for
/// mutating methods; read-only methods (list_*, get_*, ping,
/// resolve_authorized_session, read_session_output) return `None` so
/// they don't pollute the feed.
///
/// Each summary is at-a-glance and includes the most relevant arg(s) —
/// session uid prefixes are shortened to 8 chars (uids are ASCII so
/// byte slicing is safe), text payloads are truncated to ~40 chars
/// with an ellipsis, and the result's primary id (e.g. new task_id
/// from create_subtask) is appended where useful.
fn activity_summary_for(
    method: &str,
    params: &serde_json::Value,
    result_value: &serde_json::Value,
) -> Option<String> {
    use serde_json::Value as V;
    /// Truncate a session uid / task id to the first 8 ASCII chars.
    fn short(s: &str) -> String {
        s.chars().take(8).collect()
    }
    /// Compact text snippet for send_input: first ~40 chars + ellipsis.
    fn snippet(s: &str) -> String {
        let trimmed: String = s.chars().take(40).collect();
        if s.chars().count() > 40 {
            format!("{}…", trimmed)
        } else {
            trimmed
        }
    }
    match method {
        "send_input" => {
            let target = params.get("session_uid").and_then(V::as_str).unwrap_or("?");
            let text = params.get("text").and_then(V::as_str).unwrap_or("");
            Some(format!("send_input({}, {:?})", short(target), snippet(text)))
        }
        "kill_session" => {
            let target = params.get("session_uid").and_then(V::as_str).unwrap_or("?");
            Some(format!("kill_session({})", short(target)))
        }
        "notify_user" => {
            let msg = params.get("message").and_then(V::as_str).unwrap_or("");
            if msg.trim().is_empty() {
                Some("notify_user".to_string())
            } else {
                Some(format!("notify_user({:?})", snippet(msg)))
            }
        }
        "start_session" => {
            let label = params.get("label").and_then(V::as_str).unwrap_or("?");
            let typ = params.get("type").and_then(V::as_str).unwrap_or("?");
            Some(format!("start_session({}, {})", label, typ))
        }
        "start_workflow" => {
            let name = params.get("workflow_name").and_then(V::as_str).unwrap_or("?");
            let task = params.get("task_id").and_then(V::as_str).unwrap_or("?");
            Some(format!("start_workflow({}, task={})", name, short(task)))
        }
        "stop_workflow" => {
            let run = params.get("run_id").and_then(V::as_str).unwrap_or("?");
            // Run ids are `wf_<hex>`; show the wf_ prefix + 8 chars of
            // hex so they're distinguishable from task ids.
            Some(format!("stop_workflow({})", run.chars().take(15).collect::<String>()))
        }
        "create_subtask" => {
            let name = params.get("name").and_then(V::as_str).unwrap_or("?");
            let mode = params
                .get("worktree_mode")
                .and_then(V::as_str)
                .unwrap_or("inherit");
            let new_id = result_value
                .get("task_id")
                .and_then(V::as_str)
                .unwrap_or("?");
            Some(format!(
                "create_subtask({}, {}) → {}",
                name,
                mode,
                short(new_id)
            ))
        }
        "mark_subtask_done" => {
            let task = params.get("task_id").and_then(V::as_str).unwrap_or("?");
            let close = params
                .get("close_worktree")
                .and_then(V::as_bool)
                .unwrap_or(true);
            Some(format!(
                "mark_subtask_done({}, close_worktree={})",
                short(task),
                close
            ))
        }
        // Read-only — explicitly NOT logged. List intentional so adding
        // a new method without thinking about it (the default arm below)
        // ALSO doesn't get logged accidentally; if you add a mutating
        // method, add a branch for it here.
        "ping"
        | "resolve_authorized_session"
        | "list_sessions"
        | "list_workflows"
        | "list_subtasks"
        | "get_workflow_state" => None,
        // Default: don't log unknown methods. New mutating methods must
        // be explicitly added above to surface in the feed.
        _ => None,
    }
}

/// Compute the workflow-level aggregate indicator.
/// Running = any participant session active; Idle = none active; plus Paused/Done.
/// Coalesce window for output wakeups: alacritty fires one `Wakeup` per
/// PTY-output batch (kHz under heavy output), so we record at most one
/// timestamp per 50ms. Pulled out as a named constant so the daemon-side
/// tracker (`cm_daemon::workflow::pty_tracker`) can pin the same value via the
/// parity test — coalescing shifts the quiet boundary, so both sides must
/// agree on the interval.
const WAKEUP_COALESCE: Duration = Duration::from_millis(50);

/// Record an output wakeup at `now`, coalescing to one entry per
/// [`WAKEUP_COALESCE`]. Extracted from the main-loop `TermEvent::Wakeup`
/// handler so the recording rule is a single pure function shared by
/// production and the daemon-parity test.
fn record_wakeup(wakeups: &mut Vec<Instant>, now: Instant) {
    let should_record = wakeups
        .last()
        .map_or(true, |last| now.duration_since(*last) >= WAKEUP_COALESCE);
    if should_record {
        wakeups.push(now);
    }
}

/// Drop wakeups older than `window` relative to `now`. Extracted from the
/// main-loop per-tick prune so the rule is shared with the daemon-parity test.
fn prune_wakeups(wakeups: &mut Vec<Instant>, now: Instant, window: Duration) {
    wakeups.retain(|t| now.duration_since(*t) < window);
}

/// Core readiness predicate for a queued PendingWrite. Pure over inputs so
/// the semantics can be unit-tested without a real PTY.
fn pending_write_ready(wakeups: &[Instant], pw: &PendingWrite, now: Instant) -> bool {
    if now >= pw.hard_deadline {
        return true;
    }
    if now < pw.earliest_deliver_at {
        return false;
    }
    let window = pw.require_quiet;
    !wakeups.iter().any(|t| now.duration_since(*t) < window)
}

/// Return the byte sequence that means "Enter" to whatever's reading the
/// session's PTY right now. Most modern TUIs (codex, claude code) enable
/// the Kitty keyboard protocol (CSI >1u) at startup, which encodes Enter as
/// `\x1b[13u`, not raw `\r`. A raw `\r` written in that mode gets interpreted
/// as a literal carriage-return character appended to the input box instead
/// of as the Enter keystroke — which matches the "prompt shows up with a
/// newline but isn't submitted" symptom.
fn enter_bytes_for(session: &crate::session::Session) -> &'static [u8] {
    enter_bytes_for_mode(*session.term.lock().mode())
}

/// Pure mode → Enter-encoding mapping. Split out from `enter_bytes_for` so
/// the encoding choice is unit-testable without constructing a real `Term`.
fn enter_bytes_for_mode(mode: TermMode) -> &'static [u8] {
    if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        // Kitty: Enter = CSI 13 u
        b"\x1b[13u"
    } else {
        b"\r"
    }
}

/// Encode a terminal-pane mouse event into the SGR mouse report bytes the inner
/// app expects, translating screen coordinates into 0-based PTY cell coordinates
/// (`grid_col`/`viewport_row`); the SGR encoder re-adds the 1-based offset.
///
/// Callers gate on `TermMode::MOUSE_MODE` before invoking — once an app is
/// tracking the mouse, the event is consumed regardless. This only decides
/// *whether bytes are produced*: motion the app didn't ask for (a `Moved`
/// without any-motion tracking, a `Drag` without button/any-motion tracking)
/// returns `None` so we swallow it silently instead of flooding the PTY.
pub(super) fn encode_mouse_for_pty(
    me: &crossterm::event::MouseEvent,
    term_mode: TermMode,
    grid_col: usize,
    viewport_row: usize,
) -> Option<Vec<u8>> {
    let wanted = match me.kind {
        MouseEventKind::Moved => term_mode.contains(TermMode::MOUSE_MOTION),
        MouseEventKind::Drag(_) => {
            term_mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG)
        }
        _ => true,
    };
    if !wanted {
        return None;
    }
    let translated = crossterm::event::MouseEvent {
        column: grid_col as u16,
        row: viewport_row as u16,
        ..*me
    };
    crate::input::event_to_bytes(&CrosstermEvent::Mouse(translated), &term_mode)
}

/// Decide the actual byte sequence to write for a workflow delivery body,
/// given the inner program's current terminal mode.
///
/// When the inner program has enabled bracketed-paste mode (`\x1b[?2004h`)
/// AND the body contains at least one newline, wrap the body in
/// `\x1b[200~ … \x1b[201~`. This matches the wrapping used for user-typed
/// pastes (`CrosstermEvent::Paste` handler) and is what codex's input
/// handler expects for large multi-line input. Without it, codex can wedge
/// in a state where the trailing Enter is ignored — the symptom that
/// motivated this helper (see `wf_69fd318f1ad8c4d0` tick.log).
///
/// Single-line bodies stay raw so slash commands like `/clear` aren't
/// rendered as pasted text — the agent needs to recognise them as typed
/// commands. The newline test is conservative: real activation prompts
/// always span multiple lines.
fn format_body_for_delivery(body: &str, term_mode: TermMode) -> Vec<u8> {
    if body.contains('\n') && term_mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = Vec::with_capacity(body.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        body.as_bytes().to_vec()
    }
}

impl App {
    /// Append a Phase 6 activity-feed entry. `caller_uid` is resolved to
    /// a friendly label (workflow role, else session label, else uid
    /// prefix). Capped at `ACTIVITY_LOG_CAP` — oldest entry evicted on
    /// overflow. Called by mutating control-socket method handlers.
    /// Read-only methods MUST NOT call this.
    pub fn log_activity(&mut self, caller_uid: &str, summary: String) {
        let caller_label = self.resolve_activity_caller_label(caller_uid);
        if self.activity_log.len() >= ACTIVITY_LOG_CAP {
            self.activity_log.pop_front();
        }
        self.activity_log.push_back(ActivityEntry {
            ts: std::time::SystemTime::now(),
            caller_label,
            summary,
        });
        self.needs_redraw = true;
    }

    fn resolve_activity_caller_label(&self, caller_uid: &str) -> String {
        for ws in &self.workspaces {
            for ts in &ws.sessions {
                if ts.uid == caller_uid {
                    if let Some(role) = &ts.workflow_role {
                        return role.clone();
                    }
                    return ts.label.clone();
                }
            }
            for tomb in &ws.tombstones {
                if tomb.uid == caller_uid {
                    return tomb.label.clone();
                }
            }
        }
        // Unknown caller — fall back to a uid prefix so the feed still
        // renders something searchable rather than the full opaque uid.
        caller_uid.chars().take(12).collect()
    }

    /// True if the session is ready to receive a queued write. Ready means
    /// either we've hit the hard deadline (deliver anyway), or:
    ///   1. We've passed the earliest-deliver floor, AND
    ///   2. The PTY has been quiet for `require_quiet` (no wakeups in that window).
    pub(super) fn ready_for_write(session: &Session, pw: &PendingWrite, now: Instant) -> bool {
        pending_write_ready(&session.wakeup_times, pw, now)
    }

    /// Write a PendingWrite's bytes (plus correctly-encoded Enter if submit)
    /// to the session's PTY and log the outcome.
    ///
    /// IMPORTANT: a deliberate gap separates the body write from the Enter
    /// write so the receiving agent sees them as two separate keystroke
    /// events rather than a single paste. Without this, codex treats the
    /// whole sequence (body + \r) as pasted content — literal text including
    /// the \r character — and never submits. The gap is implemented by
    /// queueing the Enter into `ts.pending_enter` and letting the main drain
    /// loop fire it after `fire_at`. We MUST NOT block the UI thread here.
    fn deliver_pending_write(
        ts: &mut TerminalSession,
        pw: &PendingWrite,
        kind: &str,
    ) -> std::io::Result<()> {
        let body = pw.text.trim_end_matches(['\r', '\n']);
        let enter = enter_bytes_for(&ts.session);
        let kitty = enter != b"\r";
        let exited = ts.session.exited;
        let term_mode = *ts.session.term.lock().mode();
        let payload = format_body_for_delivery(body, term_mode);
        let bracketed = payload.len() != body.len();
        let write_result = ts.session.write(&payload);
        // Only queue the trailing Enter once the body has fully landed —
        // otherwise we'd submit a half-written prompt to the agent.
        if write_result.is_ok() && pw.submit {
            ts.pending_enter = Some(PendingEnter {
                fire_at: Instant::now() + ENTER_GAP,
            });
        }
        // Remember the first chunk of the delivered text + delivery time so
        // an unbound workflow session can be correlated to its new sid in
        // ~/.claude/history.jsonl. Only record for workflow sessions that
        // still need binding — and only when the body write actually
        // succeeded in full. A failed/partial body never lands in
        // history.jsonl, so recording it would just leave the detector
        // permanently bypassed for this session.
        if write_result.is_ok()
            && ts.workflow_run_id.is_some()
            && ts.transcript_id.is_none()
        {
            let prefix: String = body.chars().take(120).collect();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            ts.last_delivery = Some((prefix, now_ms));
        }
        if let Some(run_id) = ts.workflow_run_id.clone() {
            log_tick(
                &run_id,
                &format!(
                    "delivered {}: {} body bytes + submit={} to session '{}' role='{}' exited={} kitty_enter={} bracketed={} write_ok={}",
                    kind,
                    body.len(),
                    pw.submit,
                    ts.label,
                    ts.workflow_role.as_deref().unwrap_or("?"),
                    exited,
                    kitty,
                    bracketed,
                    write_result.is_ok(),
                ),
            );
        }
        write_result
    }

    /// Process all pending terminal events (non-blocking).
    pub fn drain_terminal_events(&mut self) {
        let now = Instant::now();
        let should_check_session_ids =
            now.duration_since(self.last_session_id_check) >= SESSION_ID_CHECK_INTERVAL;

        // Only PTY events that affect the *visible* UI should force a
        // redraw — Wakeup floods from a background session don't change
        // anything on screen (the alacritty grid is kept in sync inside
        // the FairMutex regardless; we just don't repaint it when the
        // user isn't looking at it). Without this gate, a chatty agent
        // running in a non-focused pane drives the redraw loop at PTY-
        // batch frequency and starves keystroke→paint latency.
        let focused_idx: Option<(usize, usize)> = match &self.cursor {
            Cursor::Session(wi, si) => Some((*wi, *si)),
            _ => None,
        };
        let mut visible_dirty = false;
        struct DetectedSid {
            ws_id: String,
            sid: String,
            workflow: Option<(String, String)>,
            old_sid: Option<String>,
            /// Sub-2b-1 review #1: workspace + session index of
            /// the bound session, so the post-binding-loop
            /// daemon push can reach the immutable
            /// `&Workspace`/`&TerminalSession` without
            /// re-resolving via `ws_id`.
            ws_index: usize,
            session_index: usize,
        }
        let mut sid_detections: Vec<DetectedSid> = Vec::new();
        let mut manifest_needs_save = false;
        // Sids already bound to some live session in this TUI. The detector
        // must exclude these so two sessions sharing a worktree (e.g. a
        // workflow reviewer + a regular codex pane) can't both pick the
        // same newly-written transcript file. Updated as the loop binds new
        // sids so later iterations see them too.
        //
        // Only build it on ticks where we're actually going to run sid
        // detection — otherwise this allocates a fresh HashSet + clones
        // every session's transcript_id on every drain (5+ Hz) for nothing.
        let mut bound_sids: std::collections::HashSet<String> = if should_check_session_ids {
            self.workspaces
                .iter()
                .flat_map(|w| w.sessions.iter())
                .filter_map(|s| s.transcript_id.clone())
                .collect()
        } else {
            std::collections::HashSet::new()
        };
        // Status-bar notes for write failures encountered during this drain.
        // Collected here and applied after the loop because we cannot borrow
        // `&mut self.status_msg` while iterating `&mut self.workspaces`.
        let mut write_failure_notes: Vec<String> = Vec::new();
        // 10e-d: cap-kill uids observed via the attach-stream
        // End frame path. Collected here for the same borrow-shape
        // reason as `write_failure_notes` — the de-dup +
        // activity-feed mutation happens outside the workspaces
        // loop via `try_emit_cap_kill_toast`, which also marks
        // `cap_kill_toasted` so the matching manifest.watch
        // broadcast (which arrives via a separate channel) is
        // suppressed.
        let mut cap_kill_notes: Vec<String> = Vec::new();
        // Remote auto-reconnect: (wi, si) of REMOTE sessions whose
        // attach I/O stream died (transport EOF, not a daemon `End`
        // frame) this pass. Collected here for the same borrow-shape
        // reason as `cap_kill_notes` — after the workspaces loop we
        // mark each uid reconnecting and requeue its manifest entry
        // into `pending_remote_reattach`, which can't happen inside
        // the `&mut self.workspaces` iteration.
        let mut remote_reconnect_requeue: Vec<(usize, usize)> = Vec::new();
        // Bound how long this pass spends draining PTY events so a flooding
        // session can't starve the rest of the main loop (control queue, UI).
        // See `TERMINAL_DRAIN_BUDGET`. Once the deadline passes, sessions not
        // yet reached skip their event drain this tick but still run their
        // (cheap, O(1)-ish) idle-detection and pending-write logic below;
        // unread events wait for the next tick.
        let drain_deadline = now + TERMINAL_DRAIN_BUDGET;
        let mut drain_over_budget = false;
        // Remote auto-reconnect: snapshot the set of sessions whose attach
        // stream is currently dead so the per-session pending-write gate below
        // can read it without re-borrowing `self` while `self.workspaces` is
        // mutably borrowed. Cheap (the set is empty in the common case); the
        // post-loop requeue is the only writer and it runs after this loop, so
        // a clone here is a consistent read.
        let reconnecting_snapshot = self.reconnecting_sessions.clone();
        for (wi, ws) in self.workspaces.iter_mut().enumerate() {
            for (si, ts) in ws.sessions.iter_mut().enumerate() {
                let is_focused = focused_idx == Some((wi, si));
                // True while this remote session's PTY I/O stream is dead and
                // awaiting reattach. Seeded from the snapshot (sessions already
                // reconnecting from a prior tick) and flipped on below if THIS
                // tick's drain observes the transport death. Gates pending-write
                // delivery so a queued workflow prompt isn't consumed (and
                // silently lost) against the dead EventLoop — it stays queued
                // and flushes naturally once the PTY rebinds.
                let mut session_reconnecting =
                    reconnecting_snapshot.contains(&ts.uid);
                // `drain_over_budget` short-circuits the event drain for this
                // and every later session once the per-tick budget is spent.
                let mut drained_this_session: usize = 0;
                while !drain_over_budget {
                    let event = match ts.session.event_rx.try_recv() {
                        Ok(event) => event,
                        Err(_) => break,
                    };
                    match event {
                        TermEvent::Exit | TermEvent::ChildExit(_) => {
                            visible_dirty = true;
                            // Slice 10c-e-3b-fix4b (+ 10e-d
                            // unification): daemon-attached
                            // cap-kill toast. The reader half of
                            // the attach stream latches
                            // `memory_cap_kill` into this Arc
                            // BEFORE delivering the exit event
                            // (slice-10c-e-2 review-5 fix #2b
                            // ordering), so by the time we observe
                            // `Exit`/`ChildExit` here the flag is
                            // already populated. Read-and-clear via
                            // `swap(false, SeqCst)`; if true,
                            // queue the uid for the post-loop
                            // emit. The post-loop call to
                            // `try_emit_cap_kill_toast` handles
                            // BOTH activity-feed insertion AND the
                            // cap_kill_toasted set-marking that
                            // suppresses a duplicate toast when
                            // the same uid's manifest.watch
                            // broadcast arrives. A cap-kill is
                            // always an explicit `End` frame, so
                            // it's mutually exclusive with the
                            // transport-EOF case handled below.
                            if let Some(flag) =
                                ts.session.daemon_memory_cap_kill.as_ref()
                            {
                                use std::sync::atomic::Ordering;
                                if flag.swap(false, Ordering::SeqCst) {
                                    cap_kill_notes.push(ts.uid.clone());
                                }
                            }
                            // Remote auto-reconnect: distinguish a
                            // TRANSPORT death (the attach socket
                            // EOF'd with no daemon `End` frame —
                            // typically the SSH tunnel dropped when
                            // the laptop lost connectivity) from a
                            // genuine daemon-side child exit. Only
                            // the former is recoverable: the daemon-
                            // side PTY + workflow keep running
                            // (daemon-side execution), so instead of
                            // tearing the session slot down we keep
                            // it alive, mark it reconnecting, and
                            // requeue it for reattach once the per-
                            // host manifest.watch consumer warms the
                            // tunnel back up. The `daemon_transport_eof`
                            // flag is the GROUND-TRUTH signal latched
                            // by the attach reader on EOF — we do NOT
                            // infer transport death from a transient
                            // `live_socket_path()==None`, which can
                            // momentarily be None during a HEALTHY
                            // tunnel respawn. LOCAL sessions have no
                            // `daemon_transport_eof` Arc (None) and
                            // always take the normal exit path —
                            // completely unaffected.
                            let transport_died = ts
                                .session
                                .daemon_transport_eof
                                .as_ref()
                                .map(|f| {
                                    f.swap(
                                        false,
                                        std::sync::atomic::Ordering::SeqCst,
                                    )
                                })
                                .unwrap_or(false);
                            let is_remote = ts.host_id
                                != cm_daemon::host_id::HostId::local();
                            if is_remote && transport_died {
                                // Keep the slot (do NOT set
                                // `exited`); the post-loop requeue
                                // marks it reconnecting and enqueues
                                // the rebind. Drop it out of the
                                // Running sort so the green spinner
                                // doesn't imply live output while the
                                // stream is dead.
                                ts.set_status(SessionStatus::Idle);
                                // Gate this tick's pending-write
                                // delivery too: the stream just died,
                                // so don't consume a queued prompt
                                // against the now-dead EventLoop.
                                session_reconnecting = true;
                                remote_reconnect_requeue.push((wi, si));
                            } else {
                                ts.session.exited = true;
                            }
                        }
                        TermEvent::Title(title) => {
                            ts.session.title = title;
                            visible_dirty = true;
                        }
                        TermEvent::Wakeup => {
                            // Background sessions can chatter at any rate
                            // without forcing a repaint — only the focused
                            // pane's grid is on screen.
                            if is_focused {
                                visible_dirty = true;
                            }
                            // Coalesce wakeup_times: alacritty fires one
                            // Wakeup per PTY-output batch, which during heavy
                            // output lands at kHz rates. Burst detection
                            // only needs `>=5 in 2s`, so dropping to one
                            // entry per 50ms keeps the in-memory window
                            // bounded to ~40 entries even for the chattiest
                            // sessions without changing observed behavior.
                            // The recording rule lives in `record_wakeup` so
                            // the daemon-side tracker can pin the same logic.
                            record_wakeup(&mut ts.session.wakeup_times, now);
                        }
                        TermEvent::ClipboardStore(_, text) => {
                            // Forward OSC 52 clipboard store to the outer terminal.
                            use base64::Engine;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&text);
                            let osc = format!("\x1b]52;c;{}\x07", b64);
                            let _ = std::io::Write::write_all(
                                &mut std::io::stdout(),
                                osc.as_bytes(),
                            );
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                        }
                        TermEvent::ClipboardLoad(_, formatter) => {
                            // Read clipboard via OSC 52 is unreliable; try xclip/xsel.
                            if let Ok(output) = std::process::Command::new("xclip")
                                .args(["-selection", "clipboard", "-o"])
                                .output()
                            {
                                if output.status.success() {
                                    let text = String::from_utf8_lossy(&output.stdout);
                                    let response = formatter(&text);
                                    let _ = ts.session.write(response.as_bytes());
                                }
                            }
                        }
                        _ => {}
                    }
                    drained_this_session += 1;
                    // Check the wall-clock budget periodically rather than per
                    // event — `Instant::now()` on every iteration would add
                    // measurable overhead under kHz wakeup floods, which is the
                    // exact case this guard exists to bound.
                    if drained_this_session % 256 == 0 && Instant::now() >= drain_deadline {
                        drain_over_budget = true;
                    }
                }

                // Two windows: a short one for detecting activity bursts (idle→running),
                // and the per-session timeout for detecting quiet (running→idle).
                let activity_window = Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS as u64);
                let idle_secs = if ts.idle_timeout_secs > 0 {
                    ts.idle_timeout_secs as u64
                } else {
                    DEFAULT_IDLE_TIMEOUT_SECS as u64
                };
                let idle_window = Duration::from_secs(idle_secs);

                // Prune old wakeups outside the longer window.
                prune_wakeups(&mut ts.session.wakeup_times, now, idle_window);

                // Detect idle/active for sessions with a local terminal.
                // Freeze while user is typing to avoid flicker from echo.
                if !ts.session.exited {
                    let user_typing = ts
                        .last_write_at
                        .map_or(false, |t| now.duration_since(t) < activity_window);
                    if !user_typing {
                        // Burst = recent wakeups in the short activity window → mark running.
                        let recent_count = ts.session.wakeup_times.iter()
                            .filter(|t| now.duration_since(**t) < activity_window)
                            .count();
                        let burst_threshold = if ts.burst_threshold > 0 {
                            ts.burst_threshold as usize
                        } else {
                            WAKEUP_BURST_THRESHOLD
                        };
                        let burst = recent_count >= burst_threshold;
                        // Quiet = no wakeups at all in the full idle window → mark idle.
                        let quiet = ts.session.wakeup_times.is_empty();
                        if quiet && ts.status == SessionStatus::Running {
                            ts.set_status(SessionStatus::Idle);
                            visible_dirty = true;
                            if ts.notify_on_idle {
                                notify_session_idle(&ts.label);
                            }
                        } else if burst && ts.status != SessionStatus::Running {
                            ts.set_status(SessionStatus::Running);
                            visible_dirty = true;
                        }
                    }
                }

                // Deliver queued `/clear` first once the PTY is quiet (or
                // the hard deadline hits). Sequenced before pending_prompt so
                // the prompt always lands AFTER /clear has been processed.
                //
                // On write failure, the pending slot has already been taken,
                // so we don't retry the same payload — preventing an infinite
                // loop against a wedged PTY. We surface the timeout to the
                // status bar so the user knows to investigate.
                // `.filter(|_| !session_reconnecting)` short-circuits delivery
                // while the attach stream is dead: the `Option` becomes `None`,
                // so `pending_clear` is NEITHER read-as-ready NOR `take()`n — it
                // survives untouched until the PTY rebinds.
                if let Some(clear) =
                    ts.pending_clear.as_ref().filter(|_| !session_reconnecting)
                {
                    if Self::ready_for_write(&ts.session, clear, now) {
                        let pw = ts.pending_clear.take().unwrap();
                        if let Err(e) = Self::deliver_pending_write(ts, &pw, "pending_clear") {
                            // A partial /clear in the PTY buffer can't be
                            // recovered: any follow-up prompt would land on
                            // top of the truncated slash-command and produce
                            // malformed input. Tear down the whole queued
                            // sequence so the user can re-issue it cleanly.
                            ts.pending_prompt = None;
                            ts.pending_enter = None;
                            write_failure_notes.push(format!(
                                "write to {}: {}",
                                ts.label, e
                            ));
                        }
                    }
                }

                // Only deliver the prompt once the /clear (if any) is gone
                // AND its trailing Enter has fired. Without the pending_enter
                // gate, the prompt's deliver_pending_write call would overwrite
                // ts.pending_enter before the clear's Enter ever fires —
                // codex then sees `/clearCan you review unstaged changes...\r`
                // as a single slash command and rejects it.
                // Same reconnect gate as `pending_clear`: don't consume a
                // queued prompt against a dead EventLoop — leave it for the
                // post-rebind flush.
                if ts.pending_clear.is_none()
                    && ts.pending_enter.is_none()
                    && !session_reconnecting
                {
                    if let Some(prompt) = &ts.pending_prompt {
                        if Self::ready_for_write(&ts.session, prompt, now) {
                            let pw = ts.pending_prompt.take().unwrap();
                            if let Err(e) =
                                Self::deliver_pending_write(ts, &pw, "pending_prompt")
                            {
                                write_failure_notes.push(format!(
                                    "write to {}: {}",
                                    ts.label, e
                                ));
                            }
                        }
                    }
                }

                // Fire any deferred Enter that's reached its `fire_at`. The
                // body of the prompt has already gone to the PTY; this writes
                // just the Enter keystroke separately so codex doesn't classify
                // it as paste tail. See `deliver_pending_write` for context.
                //
                // Encoding is recomputed here (not snapshotted at body-write
                // time) because the agent often flips to Kitty keyboard mode
                // during the gap; the Enter keystroke must match the mode in
                // effect right now or the agent treats it as a literal `\r`.
                // Reconnect gate again: a leftover deferred Enter (its body
                // already reached the daemon PTY before the stream died) must
                // NOT fire against the dead EventLoop — that would lose the
                // submit and leave the body un-submitted on the daemon side.
                // Hold it; once the PTY rebinds it fires (its `fire_at` is past)
                // and submits the body cleanly.
                if let Some(pe) =
                    ts.pending_enter.as_ref().filter(|_| !session_reconnecting)
                {
                    if now >= pe.fire_at {
                        ts.pending_enter = None;
                        let enter = enter_bytes_for(&ts.session);
                        let mode_label = if enter == b"\r" { "raw" } else { "kitty" };
                        if let Err(e) = ts.session.write(enter) {
                            // Without a successful Enter the agent never
                            // submits the body, so nothing will land in
                            // history.jsonl for the correlator to match.
                            // Clear `last_delivery` so the listing-based
                            // detector isn't permanently bypassed for this
                            // session.
                            ts.last_delivery = None;
                            // If a `pending_prompt` is still queued at this
                            // point, this Enter belonged to a `/clear` body
                            // (the prompt only fires after pending_enter
                            // clears). The /clear didn't submit, so a prompt
                            // landing on top would concatenate with the
                            // half-applied slash-command — drop it.
                            ts.pending_prompt = None;
                            write_failure_notes.push(format!(
                                "enter to {}: {}",
                                ts.label, e
                            ));
                        }
                        if let Some(run_id) = ts.workflow_run_id.clone() {
                            log_tick(
                                &run_id,
                                &format!(
                                    "enter_fired mode={} session='{}' role='{}'",
                                    mode_label,
                                    ts.label,
                                    ts.workflow_role.as_deref().unwrap_or("?"),
                                ),
                            );
                        }
                    }
                }

                // Session-id detection is run in a separate ordered pass
                // after this loop (see below) so older sessions get first
                // pick when two sessions in the same worktree race for the
                // same newly-written transcript file.

            }
        }

        // Session-id detection: oldest-first, so when two sessions in the
        // same worktree race for the same newly-written transcript, the one
        // that's been waiting longer (and is therefore more likely to
        // actually own the file) wins. `bound_sids` is also extended as we
        // go so once a sid is taken, no other session can claim it on this
        // tick.
        //
        // Skip claude WORKFLOW sessions when there's a pending delivery:
        // the listing heuristic is unreliable for them when /clear
        // rotations or other claude processes are racing the project
        // directory. After a workflow-launch respawn (Existing claude
        // slot) we don't deliver an activation prompt, so there's nothing
        // pending — fall back to the listing detector in that case.
        if should_check_session_ids {
            let mut detection_order: Vec<(usize, usize, Instant)> = Vec::new();
            for (wi, ws) in self.workspaces.iter().enumerate() {
                for (si, ts) in ws.sessions.iter().enumerate() {
                    let skip_workflow_claude = ts.session_type == "claude"
                        && ts.workflow_run_id.is_some()
                        && ts.last_delivery.is_some();
                    if skip_workflow_claude {
                        continue;
                    }
                    if !matches!(ts.session_type.as_str(), "claude" | "codex") {
                        continue;
                    }
                    let pending_detection = ts.pending_jsonl_files.is_some();
                    let initial_bind = ts.transcript_id.is_none() && pending_detection;
                    let codex_resume_rebind = ts.session_type == "codex"
                        && ts.transcript_id.is_some()
                        && pending_detection
                        && now.duration_since(ts.created_at) <= CODEX_RESUME_REBIND_WINDOW;
                    if !(initial_bind || codex_resume_rebind) {
                        continue;
                    }
                    detection_order.push((wi, si, ts.created_at));
                }
            }
            detection_order.sort_by_key(|t| t.2);
            for (wi, si, _) in detection_order {
                let Some(ws) = self.workspaces.get(wi) else { continue };
                let ws_id_here = ws.id.clone();
                let Some(wt) = ws.worktree_path.clone() else { continue };
                let Some(ts) = self.workspaces[wi].sessions.get_mut(si) else { continue };
                let mut existing: Vec<String> =
                    ts.pending_jsonl_files.as_ref().cloned().unwrap_or_default();
                existing.extend(bound_sids.iter().cloned());
                let sid = if ts.session_type == "codex" {
                    Self::detect_codex_session_id(&wt, &existing)
                } else {
                    Self::detect_session_id(&wt, &existing)
                };
                if let Some(sid) = sid {
                    let old_sid = ts.transcript_id.clone();
                    if old_sid.is_some() {
                        ts.rebind_transcript(Some(sid.clone()));
                    } else {
                        ts.transcript_id = Some(sid.clone());
                    }
                    ts.pending_jsonl_files = None;
                    let workflow = match (ts.workflow_run_id.clone(), ts.workflow_role.clone()) {
                        (Some(run_id), Some(role)) => Some((run_id, role)),
                        _ => None,
                    };
                    bound_sids.insert(sid.clone());
                    sid_detections.push(DetectedSid {
                        ws_id: ws_id_here,
                        sid,
                        workflow,
                        old_sid,
                        // Sub-2b-1 review #1: carry (wi, si)
                        // so the post-loop daemon push can
                        // reach the immutable workspace+session
                        // without re-resolving via ws_id.
                        ws_index: wi,
                        session_index: si,
                    });
                    manifest_needs_save = true;
                }
            }
        }

        // Sub-2b-1 review #1: now that the mutable
        // binding-loop scope has ended, push the resolved
        // transcript_path to the daemon for each
        // freshly-detected (or rebound) sid. Immutable borrow
        // is now safe.
        for detected in &sid_detections {
            let Some(ws) = self.workspaces.get(detected.ws_index) else {
                continue;
            };
            let Some(ts) = ws.sessions.get(detected.session_index) else {
                continue;
            };
            Self::push_transcript_path_to_daemon_if_attached(&self.host_pool, ts, ws);
        }

        // Sync any newly detected session_ids to the DB. Resolve each ws_id
        // to bound tasks and push an update per bound task.
        for detected in &sid_detections {
            for task in &self.tasks {
                if task.workspace_id.as_deref() != Some(&detected.ws_id) {
                    continue;
                }
                let Some(task_id) = task.task_id.clone() else {
                    continue;
                };
                let mut fields = HashMap::new();
                fields.insert(
                    "session_id".to_string(),
                    serde_json::Value::String(detected.sid.clone()),
                );
                self.backend.update_task(task_id, fields);
            }
            if let Some((run_id, role)) = &detected.workflow {
                if note_workflow_transcript_binding(
                    &mut self.workflow_runs,
                    run_id,
                    role,
                    detected.old_sid.as_deref(),
                    &detected.sid,
                ) {
                    // 10d-2c-1 review round-6 (F1): apply the
                    // same field-level mutations to the on-disk
                    // run so a concurrent daemon write (active
                    // role, iteration, status) survives the
                    // RMW. TUI owns role_sessions /
                    // role_baselines / current-active-role's
                    // history.last() correlation — daemon owns
                    // everything else.
                    let new_sid = detected.sid.clone();
                    let old_sid = detected.old_sid.clone();
                    let role_owned = role.clone();
                    let run_id_owned = run_id.clone();
                    let run_id_for_closure = run_id_owned.clone();
                    let updated = workflow::run::modify(&run_id_owned, move |r| {
                        note_workflow_transcript_binding(
                            std::slice::from_mut(r),
                            &run_id_for_closure,
                            &role_owned,
                            old_sid.as_deref(),
                            &new_sid,
                        );
                    });
                    if let Ok(updated) = updated {
                        if let Some(slot) =
                            self.workflow_runs.iter_mut().find(|r| &r.run_id == run_id)
                        {
                            *slot = updated;
                        }
                    }
                    if let Some(old_sid) = detected.old_sid.as_deref() {
                        log_tick(
                            run_id,
                            &format!(
                                "codex-resume-rebind: role={} {} -> {}",
                                role, old_sid, detected.sid
                            ),
                        );
                    }
                }
            }
        }
        if manifest_needs_save {
            self.save_session_manifest();
        }

        if should_check_session_ids {
            self.last_session_id_check = now;
        }
        // sid_detections always result in sidebar/transcript binding changes,
        // and manifest_needs_save tracks the same set of mutations that change
        // what's painted. Mark them visible_dirty here so the focused-gate
        // above doesn't accidentally suppress an important repaint.
        if !sid_detections.is_empty() || manifest_needs_save {
            visible_dirty = true;
        }
        if visible_dirty {
            self.needs_redraw = true;
        }

        // Surface any write timeouts collected during the per-session loop.
        // Last note wins (status_msg holds a single string), which is fine —
        // a stalled PTY tends to fail repeatedly and the user just needs to
        // see *something*, not every individual failure.
        if let Some(note) = write_failure_notes.into_iter().next_back() {
            self.set_status_msg(&note);
        }

        // 10c-e-3b-fix4b + 10e-d: route attach-stream-detected
        // cap-kills through `try_emit_cap_kill_toast`. The helper
        // is idempotent and marks `cap_kill_toasted` so the
        // matching manifest.watch broadcast (arriving via a
        // different channel — see `apply_manifest_diff` /
        // `apply_manifest_snapshot`) for the same uid produces
        // exactly ONE activity-feed entry regardless of arrival
        // order.
        for uid in cap_kill_notes {
            self.try_emit_cap_kill_toast(&uid);
        }

        // Remote auto-reconnect: for each REMOTE session whose attach
        // stream died this pass, mark it reconnecting (drives the
        // `⟳` sidebar indicator + keeps the slot) and requeue its
        // manifest entry into the existing deferred-reattach flow.
        // The per-host manifest.watch consumer (its own thread, with
        // exponential-backoff reconnect) re-warms the tunnel on its
        // own; `drain_deferred_remote_reattach` then rebinds the PTY
        // to the still-alive daemon session — no work lost. We do NOT
        // add to `skipped_manifest_entries` here: the slot stays live
        // in `ws.sessions`, so `save_session_manifest` already
        // round-trips it; a skipped copy would double-write.
        if !remote_reconnect_requeue.is_empty() {
            // Idempotent per uid inside the helper (the dead EventLoop emits one
            // exit, but guard anyway). Shared with the stale-generation watchdog.
            for (wi, si) in remote_reconnect_requeue {
                self.requeue_remote_reconnect(wi, si, "transport EOF");
            }
            self.needs_redraw = true;
        }

        // Poll `~/.claude/history.jsonl` for `/clear` and `/compact` events
        // targeting any active workflow role's bound session, and migrate
        // to the new transcript file.
        self.apply_history_rotations();

        // Drive workflow transitions after per-session bookkeeping — this way
        // any session state changes above (idle detection, new session_id) are
        // visible to the workflow engine.
        //
        // Throttled: each tick does several transcript reads per active
        // workflow role. At drain frequency (loop cadence) that adds up fast
        // when workflows are running. 100ms latency on transition firing is
        // imperceptible to the user.
        const WORKFLOW_TICK_MIN_INTERVAL: Duration = Duration::from_millis(100);
        if !self.workflow_runs.is_empty()
            && now.duration_since(self.last_workflow_tick) >= WORKFLOW_TICK_MIN_INTERVAL
        {
            self.last_workflow_tick = now;
            self.tick_workflows();
        }
    }

    /// Process all pending backend events (non-blocking).
    pub fn drain_backend_events(&mut self) {
        while let Ok(event) = self.backend.event_rx.try_recv() {
            self.needs_redraw = true;
            match event {
                BackendEvent::TasksUpdated(tasks) => {
                    self.reconcile_tasks(tasks);
                    // Fallback restore trigger. The main loop now calls
                    // `maybe_restore_sessions` every tick (decoupled from the
                    // API), so by the time the first tasks fetch lands this is
                    // normally a no-op. Kept so a code path that drives
                    // `drain_backend_events` without the loop still hydrates.
                    self.maybe_restore_sessions();
                }
                BackendEvent::Connected => {
                    self.connected = true;
                    self.set_status_msg("Connected to API");
                    // Restore sessions from manifest on first connect
                    // (tasks may not be populated yet, but they will be
                    // after TasksUpdated fires — see below).
                }
                BackendEvent::Disconnected => {
                    self.connected = false;
                }
                BackendEvent::ApiError(msg) => {
                    self.set_status_msg(&format!("API: {}", msg));
                }
                BackendEvent::Progress(msg) => {
                    self.set_status_msg(&msg);
                }
                BackendEvent::PullComplete {
                    task_id,
                    worktree_path,
                    main_repo,
                    session_id,
                    repo_url,
                    prompt,
                } => {
                    self.spawn_resumed_session(
                        Some(task_id),
                        worktree_path,
                        main_repo,
                        session_id,
                        repo_url,
                        prompt,
                    );
                }
                BackendEvent::PushComplete {
                    workspace_id,
                    task_id,
                } => {
                    // Local mutation gated on PushComplete: see
                    // `push_active` for the invariant. Reaching here
                    // means git push + GCS upload + API write all
                    // succeeded, so it's now safe to drop the local
                    // worktree state and flip to cloud.
                    self.finish_push(&workspace_id, task_id);
                }
                BackendEvent::PushFailed {
                    workspace_id,
                    error,
                } => {
                    if let Some(ws) = self
                        .workspaces
                        .iter_mut()
                        .find(|w| w.id == workspace_id)
                    {
                        ws.is_pushing = false;
                    }
                    self.set_status_msg(&format!("Push failed: {}", error));
                }
                BackendEvent::PlanTasksUpdated(tasks) => {
                    self.planning.update_from_api(tasks);
                }
                BackendEvent::PlanTaskUpdated(task) => {
                    self.planning.on_task_updated(task);
                }
                BackendEvent::PlanTaskCreated(task) => {
                    self.planning.on_task_created(task);
                }
                BackendEvent::PlanTaskDeleted(id) => {
                    self.planning.on_task_deleted(&id);
                }
            }
        }
    }

    /// While running without a control plane (another instance owned
    /// `tui.sock` at our startup), periodically re-attempt the bind. The
    /// moment the conflicting instance exits — releasing the socket — this
    /// succeeds and the TUI regains its MCP/agent control plane, clearing
    /// the degraded-mode banner. No-op (one cheap branch) once bound, which
    /// is the overwhelmingly common case. Throttled to
    /// `CONTROL_REBIND_INTERVAL`.
    ///
    /// A failed retry is cheap: `server::start` connect-probes the existing
    /// socket and returns before binding (no listener thread spawned), so
    /// repeated attempts against a still-live holder don't leak resources.
    pub fn maybe_rebind_control_socket(&mut self) {
        if self.control_bound {
            return;
        }
        let now = Instant::now();
        if now < self.control_rebind_at {
            return;
        }
        self.control_rebind_at = now + CONTROL_REBIND_INTERVAL;
        match crate::control::server::start(self.control_queue.clone()) {
            Ok(path) => {
                eprintln!(
                    "control socket bound at {} (recovered — prior holder exited)",
                    path.display()
                );
                self.control_bound = true;
                self.control_conflict_pid = None;
                self.needs_redraw = true;
            }
            Err(_) => {
                // Still held (or a transient bind failure). Refresh the
                // owner PID in case the holder changed since last check so
                // the banner always names the current squatter.
                let owner =
                    crate::control::server::read_owner_pid(&self.control_socket_path);
                if owner != self.control_conflict_pid {
                    self.control_conflict_pid = owner;
                    self.needs_redraw = true;
                }
            }
        }
    }

    /// Process pending control-socket requests. The socket server thread
    /// pushes (Request, reply_tx) tuples onto a shared queue; we pop each
    /// one, dispatch to a method handler, and send the Response back.
    /// Handlers run on the main loop so they have free `&mut self` access
    /// to App state without any extra locking.
    pub fn drain_control_events(&mut self) {
        let pending = self.control_queue.drain();
        if pending.is_empty() {
            return;
        }
        for entry in pending {
            let resp = self.dispatch_control(&entry.request);
            let _ = entry.reply.send(resp);
        }
        self.needs_redraw = true;
    }

    /// Dispatch a single control-socket request to its method handler.
    /// New handlers are added here as Phases 1+3 fill out the surface.
    ///
    /// **Persistence invariant**: any handler that mutates state which
    /// lives in `ManifestEntry` / `Workspace.tombstones` MUST call
    /// `self.save_session_manifest()` before returning Ok. A TUI crash
    /// between the mutation and the next unrelated save would otherwise
    /// lose the change — most painfully for tombstones, where a killed
    /// session would restore as live on next boot. Handlers that only
    /// touch in-memory state (`pending_prompt`, runtime status) don't
    /// need the save.
    fn dispatch_control(
        &mut self,
        req: &crate::control::protocol::Request,
    ) -> crate::control::protocol::Response {
        use crate::control::methods;
        use crate::control::protocol::{ErrorCode, Response};
        // Every method currently dispatched here is session-scoped (the
        // pre-Phase-1 surface). Operator-only methods (session.attach,
        // attach.open) land in a later slice and short-circuit before
        // reaching this match; rejecting Operator callers here keeps the
        // existing methods exactly as strict as they were when `Caller`
        // was a flat struct.
        let caller = match req.caller.session_uid() {
            Some(uid) => uid,
            None => {
                return Response::err(
                    req.id.clone(),
                    ErrorCode::Unauthorized,
                    "method requires a session-scoped caller",
                );
            }
        };
        let result: methods::MethodResult = match req.method.as_str() {
            "ping" => Ok(serde_json::json!({
                "pong": true,
                "uid": caller,
            })),
            "resolve_authorized_session" => {
                methods::resolve_authorized_session(self, caller, &req.params)
            }
            "get_caller_task" => methods::get_caller_task(self, caller, &req.params),
            "list_sessions" => methods::list_sessions(self, caller, &req.params),
            "send_input" => methods::send_input(self, caller, &req.params),
            "kill_session" => methods::kill_session(self, caller, &req.params),
            "start_session" => methods::start_session(self, caller, &req.params),
            // Phase 4 §D: start_workflow relocated to the daemon socket; the TUI
            // no longer launches workflows (the MCP tool + A-f both route to the
            // daemon). The catch-all returns UnknownMethod if anything still
            // dials the TUI for it.
            "stop_workflow" => methods::stop_workflow(self, caller, &req.params),
            "get_workflow_state" => methods::get_workflow_state(self, caller, &req.params),
            "list_workflows" => methods::list_workflows(self, caller, &req.params),
            "create_subtask" => methods::create_subtask(self, caller, &req.params),
            "list_subtasks" => methods::list_subtasks(self, caller, &req.params),
            "mark_subtask_done" => methods::mark_subtask_done(self, caller, &req.params),
            "notify_user" => methods::notify_user(self, caller, &req.params),
            other => Err((
                ErrorCode::UnknownMethod,
                format!("unknown method: {}", other),
            )),
        };
        match result {
            Ok(value) => {
                // Phase 6 activity feed. Only mutating methods land here;
                // read-only ones (`list_*`, `get_*`, `ping`,
                // `resolve_authorized_session`) are intentionally skipped.
                if let Some(summary) =
                    activity_summary_for(req.method.as_str(), &req.params, &value)
                {
                    self.log_activity(caller, summary);
                }
                Response::ok(req.id.clone(), value)
            }
            Err((code, msg)) => Response::err(req.id.clone(), code, msg),
        }
    }

    /// 12e-r8 F1: resolve the caller's `host_id` by walking
    /// `self.workspaces[*].sessions[*]` looking for a uid
    /// match. The MCP `start_session` flow passes the
    /// calling agent's uid; the caller's session SHOULD be
    /// findable in the App's state. Returns `Err(NotFound)`
    /// defensively if not — should never happen in
    /// production (the daemon's auth path would have rejected
    /// the caller first), but the explicit error beats a
    /// silent panic.
    pub(crate) fn resolve_caller_host(
        &self,
        caller_uid: &str,
    ) -> std::io::Result<cm_daemon::host_id::HostId> {
        for ws in &self.workspaces {
            for ts in &ws.sessions {
                if ts.uid == caller_uid {
                    return Ok(ts.host_id.clone());
                }
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "MCP caller uid `{}` not found in any workspace's \
                 sessions; cannot resolve caller host for \
                 spawn_managed_session",
                caller_uid,
            ),
        ))
    }

    /// Spawn a new agent session in the given workspace, owned by the
    /// caller (managed_by_uid recorded). Used by the `start_session`
    /// MCP tool. Returns the new session's UID.
    pub fn spawn_managed_session(
        &mut self,
        ws_index: usize,
        caller_uid: &str,
        type_: &str,
        label: &str,
        task_id: Option<String>,
        prompt: Option<&str>,
        global_perms: bool,
    ) -> std::io::Result<String> {
        // 12e-r8 F1: derive the target host from the CALLER's
        // pinned host_id, NOT from `self.active_host`.
        // Pre-r8 the round-5 guard used active_host — the
        // operator's transient A-H selection — which meant
        // an agent on host=local couldn't spawn a child if
        // the operator happened to have cycled active_host
        // to "manager". The agent's spawn rights belong to
        // the agent's context, not the UI state. Same shape
        // as Unix `fork()` inheriting the parent's working
        // directory.
        let caller_host = self.resolve_caller_host(caller_uid)?;

        // 12e-r6: shared fail-fast helper. Pre-r8 this
        // checked active_host; round-8 routes the check
        // through caller_host instead.
        guard_local_host_only(&caller_host, "MCP `start_session`")?;
        let worktree_path = self.workspaces[ws_index]
            .worktree_path
            .clone()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "workspace has no worktree",
                )
            })?;
        let workspace_id = self.workspaces[ws_index].id.clone();
        let (cols, rows) = self.last_term_size;
        let session_uid = new_session_uid();

        // 12e-r4 F1.2: snapshot transcript baseline BEFORE
        // the spawn. The new agent (Claude/Codex) creates its
        // transcript JSONL within ms of spawn; capturing this
        // list AFTER `try_spawn_via_daemon` returns would
        // include the newly-spawned agent's own file —
        // `detect_new_transcript_jsonl` would then treat that
        // file as preexisting, never bind transcript_id, and
        // `resolve_authorized_session` would return `pending`
        // forever. Round-3 introduced this reordering bug;
        // round-4 puts the snapshot back in front of the
        // spawn.
        let pending = if type_ == "bash" {
            None
        } else {
            let engine = match type_ {
                "codex" => workflow::toml_schema::Engine::Codex,
                _ => workflow::toml_schema::Engine::ClaudeCode,
            };
            Some(match engine {
                workflow::toml_schema::Engine::ClaudeCode => {
                    Self::list_jsonl_files(&worktree_path)
                }
                workflow::toml_schema::Engine::Codex => {
                    Self::list_codex_sessions(&worktree_path)
                }
            })
        };

        // 12e-r8 F1: route the spawn through `try_spawn_via_daemon`
        // against the CALLER's host (not active_host). The
        // PTY child runs on the same daemon the agent itself
        // lives under, mirroring the `fork()` semantics.
        let session = match self.try_spawn_via_daemon(
            &session_uid,
            &workspace_id,
            &worktree_path,
            type_,
            label,
            None, // resume_session_id
            cols,
            rows,
            task_id.as_deref(),
            None, // workflow_run_id
            None, // workflow_role
            &caller_host,
            // MCP-driven spawn is always fresh — post-spawn
            // detector handles transcript path discovery.
            None,
        ) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                return Err(std::io::Error::other(format!(
                    "daemon spawn on host {} failed: {}",
                    caller_host.as_str(),
                    e,
                )));
            }
            None => {
                return Err(std::io::Error::other(format!(
                    "session type `{}` is not daemon-eligible",
                    type_,
                )));
            }
        };
        // 12e-r4 F1.1: map both legacy `"claude"` AND wire
        // `"claude-code"` inputs to the canonical TUI session
        // type string `"claude"`. Matches the round-1 mapping
        // pre-r3 + the `try_spawn_via_daemon` alias.
        // 12e-r4 F1.1: map both legacy `"claude"` AND wire
        // `"claude-code"` inputs to the canonical TUI session
        // type string `"claude"`. Matches the round-1 mapping
        // pre-r3 + the `try_spawn_via_daemon` alias.
        let session_type = match type_ {
            "codex" => "codex".to_string(),
            "bash" => "bash".to_string(),
            _ => "claude".to_string(),
        };
        let mut pending_prompt = None;
        if let Some(text) = prompt {
            if !text.trim().is_empty() {
                pending_prompt = Some(PendingWrite::wait_for_quiet(
                    text.trim_end().to_string(),
                    true,
                    Duration::from_secs(1),
                    Duration::from_secs(2),
                    Duration::from_secs(180),
                ));
            }
        }
        let ts = TerminalSession {
            color: None,
            uid: session_uid.clone(),
            label: label.to_string(),
            session_type,
            session,
            status: SessionStatus::Running,
            idle_since: None,
            last_write_at: None,
            transcript_id: None,
            generation: 0,
            pending_jsonl_files: pending,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            pending_prompt,
            pending_clear: None,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            last_delivery: None,
            task_id,
            notify_on_idle: false,
            global_perms,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: Some(caller_uid.to_string()),
            seeded_from_snapshot: None,
            preserved_last_exit: None,
            // 12e-r8 F1: tag with the CALLER's host_id —
            // same value the spawn dialed against. The
            // child inherits the caller's host, NOT the
            // operator's transient active_host.
            host_id: caller_host.clone(),
        };
        self.workspaces[ws_index].sessions.push(ts);
        // Mirror the grant onto the daemon-owned session so the
        // daemon's Session-caller auth honors it (the TerminalSession
        // flag above only covers TUI-routed methods like send_input).
        // The escalation guard ran in `control::methods::start_session`
        // before this call, so the grant here is already authorized.
        if global_perms {
            if let Some(socket) = self.host_pool.live_socket_path(&caller_host) {
                if let Err(e) = crate::client_session::rpc_set_global_perms(
                    &socket,
                    crate::daemon_launch::operator_token(),
                    &session_uid,
                    true,
                ) {
                    self.set_status_msg(&format!(
                        "spawned {} but failed to set global perms on the daemon: {}",
                        session_uid, e,
                    ));
                }
            }
        }
        self.save_session_manifest();
        Ok(session_uid)
    }

    /// Grant or revoke a live session's global-permissions flag (the
    /// operator path: A-e session settings). Updates the in-memory
    /// `TerminalSession`, pushes the change to the session's host
    /// daemon so its Session-caller auth honors it immediately, and
    /// persists the manifest. Returns the new value, or an error
    /// string for the status line.
    pub fn set_session_global_perms(
        &mut self,
        uid: &str,
        value: bool,
    ) -> Result<bool, String> {
        let (wi, si) = crate::control::methods::find_live_session(&self.workspaces, uid)
            .ok_or_else(|| format!("session {} not found", uid))?;
        let host = self.workspaces[wi].sessions[si].host_id.clone();
        // Push to the daemon first so a failed RPC doesn't leave the
        // TUI's view diverged from the daemon's auth state.
        if let Some(socket) = self.host_pool.live_socket_path(&host) {
            crate::client_session::rpc_set_global_perms(
                &socket,
                crate::daemon_launch::operator_token(),
                uid,
                value,
            )
            .map_err(|e| format!("daemon rejected global-perms change: {}", e))?;
        }
        self.workspaces[wi].sessions[si].global_perms = value;
        self.save_session_manifest();
        Ok(value)
    }

    /// Process planning editor events (non-blocking).
    /// Drain pending `MemoryKillEvent`s pushed by per-session watcher
    /// threads into the activity feed. Called once per main-loop tick
    /// alongside the other `drain_*` methods.
    pub fn drain_memory_kill_events(&mut self) {
        loop {
            let evt = match self.memory_kill_rx.try_recv() {
                Ok(e) => e,
                Err(_) => return,
            };
            let (caller, summary) = match evt {
                crate::session_watch::MemoryKillEvent::Killed {
                    session_uid,
                    pid,
                    comm,
                    argc,
                    argv_sha256_prefix,
                    rss_kb,
                    soft_cap_bytes,
                    ..
                } => {
                    // `comm` arrives sanitized, but re-escape at the
                    // render boundary (defense-in-depth — the writer
                    // is in another module and could regress).
                    let safe_comm = crate::session_watch::sanitize(comm.as_bytes(), 16);
                    let summary = format!(
                        "killed PID {} comm={} argc={} sha={} — {:.1} GiB RSS, soft cap {:.0} GiB",
                        pid,
                        safe_comm,
                        argc,
                        argv_sha256_prefix,
                        rss_kb as f64 / (1024.0 * 1024.0),
                        soft_cap_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                    );
                    (session_uid, summary)
                }
                crate::session_watch::MemoryKillEvent::KillFailed {
                    session_uid,
                    reason,
                    ..
                } => (session_uid, format!("memory cap kill failed: {}", reason)),
            };
            self.log_activity(&caller, summary);
        }
    }

    /// 10e-c: drain the manifest.watch consumer's channel and
    /// apply each diff to in-memory TUI state. Called per tick
    /// from the main loop. No-op when the consumer wasn't
    /// spawned (legacy single-process mode — `manifest_watch_rx`
    /// is `None`).
    pub fn drain_manifest_watch_events(&mut self) {
        // Collect first so we can `&mut self` apply without
        // holding the immutable `Receiver` borrow across the
        // mutating call. mpsc::Receiver doesn't lend itself to
        // splitting borrows; the drain → buffer → apply shape
        // is the canonical Rust workaround.
        let mut events: Vec<crate::manifest_watch::ManifestEvent> = Vec::new();
        if let Some(rx) = self.manifest_watch_rx.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(ev) => events.push(ev),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        // Consumer thread exited (App being
                        // dropped, or the consumer hit its own
                        // SendError exit path — though that
                        // fires on OUR side dropping the rx, so
                        // this branch is mostly "consumer died
                        // for some reason"). Either way, no more
                        // events incoming; stop draining.
                        break;
                    }
                }
            }
        }
        for ev in events {
            match ev {
                crate::manifest_watch::ManifestEvent::Diff { host, diff } => {
                    self.apply_manifest_diff_from_host(host, diff);
                }
                crate::manifest_watch::ManifestEvent::Snapshot(payload) => {
                    self.apply_manifest_snapshot(payload);
                }
            }
        }
    }

    /// 11d: drain the events.subscribe consumer's channel.
    /// Called per tick from the main loop. Snapshot frames apply
    /// conservative-merge: the daemon's `WorkflowRun` becomes
    /// authoritative for fields the TUI hasn't observed yet
    /// (history past local `events_offset`); local in-memory
    /// state wins for everything else (mirrors 10e-c r1 F1).
    /// Event frames append to the run's history via the same
    /// per-event tail logic the file-watcher path uses today —
    /// in 11d they're forwarded raw to the workflow controller's
    /// existing tail-observer entry point so 11e's file-tail
    /// removal swaps the source without touching the apply
    /// pipeline.
    /// Drain queued `events.subscribe` frames and fold them into the observer
    /// mirror. Thin wrapper — the logic lives in `workflow::observer` (split
    /// out of this file). Channel drain and mirror update are separated so the
    /// `workflow_watch_rx` borrow doesn't conflict with the `workflow_runs` /
    /// `needs_redraw` borrows the mirror needs.
    pub fn drain_workflow_watch_events(&mut self) {
        let events =
            crate::workflow::observer::drain_watch_channel(self.workflow_watch_rx.as_ref());
        crate::workflow::observer::RunMirror {
            runs: &mut self.workflow_runs,
            needs_redraw: &mut self.needs_redraw,
        }
        .apply_events(events);
    }

    /// Apply one snapshot frame to the observer mirror (see
    /// [`crate::workflow::observer::RunMirror::apply_snapshot`]). Kept as an
    /// `App` method for the direct callers/tests; the logic lives in the
    /// observer module.
    pub(crate) fn apply_workflow_watch_snapshot(
        &mut self,
        run: cm_daemon::workflow::run::WorkflowRun,
    ) {
        crate::workflow::observer::RunMirror {
            runs: &mut self.workflow_runs,
            needs_redraw: &mut self.needs_redraw,
        }
        .apply_snapshot(run);
    }

    /// 10e-c r1 F1: apply a post-(re)connect snapshot.
    /// Conservative-merge per uid:
    ///   - If the snapshot has `Some(last_exit)` AND the local
    ///     session's `preserved_last_exit` is `None`, adopt the
    ///     snapshot's value. This is the load-bearing case: a
    ///     disconnect window during which the daemon broadcast
    ///     `Exited` diffs we missed — the snapshot recovers them.
    ///   - If the local is already `Some`, leave it. The TUI
    ///     either processed the broadcast live OR loaded the
    ///     value from disk at startup; either way it's already
    ///     authoritative locally.
    ///   - If the snapshot has `None` for a uid, no-op (no
    ///     `Some → None` regression).
    ///
    /// R5 still applies: snapshot entries for uids the TUI
    /// doesn't track are silently ignored (untagged-session
    /// case — daemon's snapshot may reflect sessions in
    /// workspaces this TUI hasn't loaded yet).
    pub(crate) fn apply_manifest_snapshot(
        &mut self,
        snapshot: crate::manifest_watch::ManifestSnapshotPayload,
    ) {
        // 10e-d: collect uids we adopted with memory_cap_kill=true
        // so we can fire toasts AFTER the workspaces-iteration
        // is done — avoids the &mut self contention from calling
        // try_emit_cap_kill_toast (which mutates activity_log)
        // inside the iteration.
        //
        // Toast IFF the conservative-merge actually adopted. T28
        // (local already Some) is naturally handled: the inner
        // if-None branch doesn't fire so we don't push the uid.
        // T27 (None → Some(cap=true)) lands here.
        let mut adopted_cap_kills: Vec<String> = Vec::new();
        for (uid, snap_last_exit) in snapshot.session_last_exits {
            let Some(snap) = snap_last_exit else {
                continue;
            };
            let cap_kill = snap.memory_cap_kill;
            for ws in &mut self.workspaces {
                for ts in &mut ws.sessions {
                    if ts.uid == uid {
                        if ts.preserved_last_exit.is_none() {
                            ts.preserved_last_exit = Some(snap.clone());
                            self.needs_redraw = true;
                            if cap_kill {
                                adopted_cap_kills.push(uid.clone());
                            }
                        }
                        // Inner break: we found the matching
                        // session in this workspace, no need to
                        // keep walking it. Outer loop continues
                        // to the next uid (a snapshot uid
                        // appears in at most one workspace).
                        break;
                    }
                }
            }
        }
        for uid in adopted_cap_kills {
            self.try_emit_cap_kill_toast(&uid);
        }
    }

    /// 10e-c: apply a single `ManifestDiff` to in-memory state.
    /// Extracted from `drain_manifest_watch_events` so tests can
    /// drive the application logic without spinning up the
    /// consumer thread + a fake daemon listener.
    ///
    /// 10e-c scope: only `Exited` is consumed. The other variants
    /// (Added / Updated / Tombstoned) are no-ops — they're for
    /// 10e-d / 10f to consume when broader manifest sync lands.
    /// Unknown uids are silent no-ops (R5 from the 10e plan).
    ///
    /// 10e-d: when the diff's `last_exit.memory_cap_kill` is true,
    /// dispatch the unified daemon-path cap-kill toast via
    /// `try_emit_cap_kill_toast`. The helper is idempotent — a
    /// duplicate `Exited` diff (network replay, snapshot+diff
    /// overlap) emits once. The attach-stream path
    /// (`drain_terminal_events`) shares the same `cap_kill_toasted`
    /// set, so a session that was attached at kill time only
    /// toasts once regardless of arrival order.
    /// Host-agnostic entry point (defaults the source host to local).
    /// Retained for the many existing tests + any caller that doesn't
    /// track a source host; the production drain loop calls
    /// [`apply_manifest_diff_from_host`] with the diff's real host.
    pub(crate) fn apply_manifest_diff(
        &mut self,
        diff: cm_daemon::manifest::ManifestDiff,
    ) {
        self.apply_manifest_diff_from_host(
            cm_daemon::host_id::HostId::local(),
            diff,
        );
    }

    /// Phase 3 (remote-session-execution): apply a manifest diff that
    /// arrived from `host`'s `manifest.watch` stream. Identical to the
    /// pre-Phase-3 behavior for `host == local`; for a remote host, an
    /// adopted row is tagged with that host (see
    /// [`adopt_daemon_workflow_participant_on_host`]).
    pub(crate) fn apply_manifest_diff_from_host(
        &mut self,
        host: cm_daemon::host_id::HostId,
        diff: cm_daemon::manifest::ManifestDiff,
    ) {
        use cm_daemon::manifest::ManifestDiff;
        match diff {
            ManifestDiff::Exited { uid, last_exit } => {
                let memory_cap_kill = last_exit.memory_cap_kill;
                let mut found = false;
                // Workspace index holding the exited session IFF it's an
                // agent-spawned ephemeral that should be PRUNED (see below).
                // `None` = leave the row in place (ghost / user-owned).
                let mut prune_ws: Option<usize> = None;
                'outer: for (wi, ws) in self.workspaces.iter_mut().enumerate() {
                    for ts in &mut ws.sessions {
                        if ts.uid == uid {
                            ts.preserved_last_exit = Some(last_exit);
                            // Status may visibly change (cap-kill
                            // toast surfacing in 10e-d below reads
                            // memory_cap_kill on the diff). Mark
                            // dirty so the next render picks up
                            // any indicator changes.
                            self.needs_redraw = true;
                            found = true;
                            // Prune AGENT-SPAWNED, non-workflow sessions on
                            // exit so they don't linger as frozen sidebar
                            // rows. An orchestrator's spawn-and-kill children
                            // (momentum-detective spawns/kills ~10/day) are
                            // killed via the `kill_session` RPC/MCP from
                            // OUTSIDE the TUI — the daemon drops them from its
                            // live registry and broadcasts this `Exited` diff,
                            // but the TUI's own row-removal only ever fired on
                            // TUI-initiated (A-w) kills. Without a prune here
                            // the diff just stamped `preserved_last_exit` and
                            // the row stayed put, indistinguishable from a live
                            // session. Gate:
                            //   - `managed_by_uid.is_some()` — agent-spawned;
                            //     the daemon already considers it gone and
                            //     `list_sessions` (default) omits it, so match
                            //     that by removing the row. User-created
                            //     sessions (`None`) stay a ghost — the user
                            //     owns their lifecycle (A-w to close).
                            //   - `workflow_run_id.is_none()` — a workflow
                            //     participant's slot must SURVIVE a fresh-
                            //     context respawn (the daemon kills+respawns
                            //     the PTY under the same slot), so never prune
                            //     one on an exit diff.
                            // Host-agnostic: the per-host manifest.watch
                            // consumer routes remote (cm-manager) exit diffs
                            // here too, so the remote repro is covered.
                            if ts.managed_by_uid.is_some()
                                && ts.workflow_run_id.is_none()
                            {
                                prune_ws = Some(wi);
                            }
                            // Break both loops via label so
                            // `last_exit`'s move into the assignment
                            // above happens exactly once (rustc
                            // E0382 sees nested-break as a
                            // potential double-move otherwise).
                            break 'outer;
                        }
                    }
                }
                if found && memory_cap_kill {
                    // 10e-d: route through the de-dup'd helper so
                    // a daemon-attached session's matching attach-
                    // stream toast (or vice versa) doesn't double-
                    // fire. Out-of-loop call so &mut self isn't
                    // contended by the workspaces iteration above.
                    // Fire BEFORE the prune below so the toast's
                    // caller-label lookup still resolves (the prune
                    // tombstones the row, and `log_activity` falls
                    // back to the tombstone label anyway, but firing
                    // first keeps the live-session path unchanged).
                    self.try_emit_cap_kill_toast(&uid);
                }
                if let Some(wi) = prune_ws {
                    // Same removal convention as A-w / task-close:
                    // `tombstone_and_remove` kills any lingering daemon
                    // binding (no-op if already dead), records a tombstone so
                    // `read_session_output` still serves the final state,
                    // drops the row, forgets reconnect bookkeeping, and
                    // persists the manifest. Re-clamp the cursor in case it
                    // sat on the removed row (the bulk helper doesn't).
                    let target = uid.clone();
                    self.tombstone_and_remove(wi, |ts| ts.uid == target);
                    self.clamp_cursor();
                    // P1 (Feature 3): auto-reap the workspace of a finished
                    // agent-spawned / detective worker once its row is pruned,
                    // if nothing live remains and its task is Done/gone — stops
                    // the empty-workspace pile-up (~10/day) without waiting for
                    // a manual A-W.
                    self.maybe_reap_spent_workspace(wi);
                }
                // R5: untracked uid — `found` stays false; silent
                // no-op. The diff referenced a session the TUI
                // doesn't know about (e.g. a session the daemon
                // spawned via MCP into a workspace this TUI
                // hasn't loaded; future divergence cases). No
                // panic, no log, no toast.
            }
            ManifestDiff::Added { uid, entry }
            | ManifestDiff::Updated { uid, entry } => {
                // Option B (criterion #4): adopt daemon-launched WORKFLOW
                // PARTICIPANTS into the sidebar from broadcasts. The helper is
                // deliberately scoped to entries carrying `workflow_run_id` —
                // those are daemon-created in Phase 4 and have NO locally-built
                // TerminalSession, so adopting them can't duplicate or race the
                // TUI-local row creation that A-n/A-s/mcp_start_session use.
                // Non-workflow daemon sessions keep their existing behavior
                // (the broader manifest-sync consumer stays deferred — 10e-d/10f)
                // EXCEPT remote ones — see `adopt_daemon_workflow_participant_on_host`.
                self.adopt_daemon_workflow_participant_on_host(&host, &uid, &entry);
            }
            ManifestDiff::Tombstoned { .. } => {
                // Session removal reaches the TUI via `Exited` (and the
                // attach-stream teardown); tombstone is a no-op here.
            }
        }
    }

    /// Option B (criterion #4): adopt a daemon-launched workflow PARTICIPANT
    /// into `workspaces[*].sessions` from a manifest `Added`/`Updated`
    /// broadcast, so it renders as a selectable + attachable row under its
    /// workflow header without a reconnect. Bounded + safe:
    ///   - Only entries with a `workflow_run_id` are adopted (Phase-4 daemon
    ///     participants; the TUI never builds these locally, so no dup/race).
    ///   - R5: an untracked workspace is a silent no-op.
    ///   - Idempotent: a uid already present is a no-op (covers duplicate Added
    ///     / Updated diffs).
    ///   - Attaches to the LOCAL daemon's existing PTY by uid (the A-f / MCP
    ///     launch + acceptance case); a failed attach logs and skips rather than
    ///     panicking, so a headless / racing teardown can't crash the observer.
    pub(crate) fn adopt_daemon_workflow_participant(
        &mut self,
        uid: &str,
        entry: &serde_json::Value,
    ) {
        // Host-agnostic wrapper: defaults the source host to local (existing
        // callers + tests). The production drain loop calls the host-aware
        // form with the diff's real host.
        self.adopt_daemon_workflow_participant_on_host(
            &cm_daemon::host_id::HostId::local(),
            uid,
            entry,
        );
    }

    /// Phase 3 (remote-session-execution): host-aware adoption of a
    /// daemon-created session into the sidebar from a `manifest.watch`
    /// Added/Updated broadcast, tagged with the producing `host`.
    ///
    /// Adoption set:
    ///   - Workflow PARTICIPANTS (entry carries `workflow_run_id`) on ANY
    ///     host — the daemon-launched-participant case; the row is tagged
    ///     with `host` (Phase 5 wires the remote-workflow producer).
    ///   - REMOTE non-workflow daemon sessions — Phase 3's remote A-n/A-s
    ///     created by ANOTHER client land here. For THIS TUI's own remote
    ///     create the row is built directly (create+attach), so the
    ///     uid-present check below makes the echoed broadcast an idempotent
    ///     no-op.
    ///   - LOCAL non-workflow sessions are NOT adopted here (unchanged):
    ///     the direct A-n/A-s build + the `adopt_untracked_daemon_sessions`
    ///     poller own them. Keeping local non-workflow out preserves
    ///     byte-for-byte local behavior.
    ///
    /// Other invariants are unchanged: untracked workspace → silent no-op;
    /// uid already present → idempotent (workflow tags stamped in place for
    /// an existing-session bind); a failed attach logs + skips rather than
    /// panicking.
    pub(crate) fn adopt_daemon_workflow_participant_on_host(
        &mut self,
        host: &cm_daemon::host_id::HostId,
        uid: &str,
        entry: &serde_json::Value,
    ) {
        let run_id = entry
            .get("workflow_run_id")
            .and_then(|v| v.as_str())
            .filter(|r| !r.is_empty())
            .map(String::from);
        let is_local = host == &cm_daemon::host_id::HostId::local();
        // Gate: workflow participant (any host) OR a remote non-workflow
        // session. A local non-workflow session stays the prior no-op.
        if run_id.is_none() && is_local {
            return;
        }
        let ws_id = match entry.get("workspace_id").and_then(|v| v.as_str()) {
            Some(w) if !w.is_empty() => w.to_string(),
            _ => return,
        };
        let role = entry
            .get("workflow_role")
            .and_then(|v| v.as_str())
            .map(String::from);
        let label = entry
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("participant")
            .to_string();
        let session_type = entry
            .get("session_type")
            .and_then(|v| v.as_str())
            .unwrap_or("claude-code")
            .to_string();
        let task_id = entry
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let continuous_task_id = entry
            .get("continuous_task_id")
            .and_then(|v| v.as_str())
            .filter(|c| !c.is_empty())
            .map(String::from);

        // R5: untracked workspace → silent no-op.
        let ws_idx = match self.workspaces.iter().position(|w| w.id == ws_id) {
            Some(i) => i,
            None => return,
        };
        // Already present: a duplicate `Added` / a re-broadcast is a no-op, but
        // an existing row that just BECAME a participant — an existing-session
        // bind, where the bound worker keeps its pre-existing TUI row — must get
        // its workflow tags stamped in place so it re-groups under the workflow
        // header and workflow ops recognize it. Don't rebuild the row (its PTY,
        // label, and transcript are already correct). A non-workflow
        // re-broadcast (run_id None) is a plain no-op.
        //
        // Dedup by uid across ALL workspaces, not just `ws_idx` — every other
        // adopt/bind path (drain_attach_results, is_remote_adoptee,
        // adopt_untracked_daemon_sessions) does. The 5s remote-adoption poll can
        // mint a synthetic `agent:<label>` workspace for the same uid (e.g. after
        // an A-d teardown re-adopts from a stale session cache); a ws_idx-only
        // check misses that copy and pushes a SECOND row → the transient
        // duplicate seen after close-out.
        if let Some(existing) = self
            .workspaces
            .iter_mut()
            .flat_map(|w| w.sessions.iter_mut())
            .find(|s| s.uid == uid)
        {
            if let Some(rid) = run_id.as_deref() {
                if existing.workflow_run_id.as_deref() != Some(rid)
                    || existing.workflow_role != role
                {
                    existing.workflow_run_id = Some(rid.to_string());
                    existing.workflow_role = role;
                    self.needs_redraw = true;
                }
            }
            return;
        }

        // Attach to the PRODUCING host's daemon (local or remote, via the
        // host_pool) and tag the row with that host — this is what makes a
        // remote-host diff render a row with `ts.host_id = remote`.
        let host_id = host.clone();
        let socket = match self
            .host_pool
            .for_host(&host_id)
            .ok()
            .and_then(|h| h.socket_path())
        {
            Some(s) => s,
            None => return,
        };
        let (cols, rows) = self.last_term_size;
        let worktree = self.workspaces[ws_idx].worktree_path.clone();
        let working_dir: &Path = worktree.as_deref().unwrap_or_else(|| Path::new("/"));

        let config = crate::client_session::ClientSessionConfig {
            daemon_socket: &socket,
            operator_token_id: crate::daemon_launch::operator_token(),
            uid,
            workspace_id: &ws_id,
            label: &label,
            session_type: &session_type,
            // attach_existing ignores argv/env — the daemon already owns the PTY.
            argv: &[],
            working_dir,
            env: std::collections::BTreeMap::new(),
            cols,
            rows,
            memory_cap_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            cgroup_path: None,
            worktree_path: worktree.as_deref(),
            task_id: task_id.as_deref(),
            transcript_path: None,
            // None for non-workflow remote sessions; the daemon ignores
            // these on the attach path either way.
            workflow_run_id: run_id.as_deref(),
            workflow_role: role.as_deref(),
        };
        let session = match crate::session::Session::new_attached_existing(config) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "cm-tui: failed to adopt daemon session {} ({}): {} \
                     — skipping row (will appear on next reconnect)",
                    uid, label, e
                );
                return;
            }
        };
        // The `entry`/config carry the daemon WIRE type ("claude-code"), but the
        // TerminalSession stores the INTERNAL vocabulary ("claude"/"codex"/"bash")
        // that the rest of the TUI matches on (transcript handling, type-specific
        // rendering). Normalize before storing — otherwise an adopted Claude row
        // is mislabeled "claude-code" and every `"claude"` match misses it, so
        // the sidebar does NOT render correctly. Same mapping the normal spawn
        // path applies.
        let internal_type = normalize_session_type_to_internal(&session_type).to_string();
        let mut ts = make_simple_session_with_uid(
            uid.to_string(),
            &label,
            &internal_type,
            session,
            None,
        );
        ts.workflow_run_id = run_id;
        ts.workflow_role = role;
        ts.task_id = task_id;
        ts.continuous_task_id = continuous_task_id;
        ts.host_id = host_id;
        // Kick a resize so a participant spawned at a smaller PTY size (e.g.
        // a headless launch before any TUI attached) immediately matches this
        // terminal and repaints — same rationale as the respawn-path kick.
        ts.session.resize(cols, rows);
        self.workspaces[ws_idx].sessions.push(ts);
        self.needs_redraw = true;
    }

    pub fn drain_planning_events(&mut self) {
        if let Some(action) = self.planning.drain_editor_events() {
            match action {
                PlanAction::UpdateTask { id, fields, status_msg } => {
                    self.backend.update_plan_task(id, fields);
                    if let Some(msg) = status_msg {
                        self.set_status_msg(&msg);
                    }
                }
                _ => {}
            }
            self.needs_redraw = true;
        }
        if self.planning.needs_redraw {
            self.needs_redraw = true;
            self.planning.needs_redraw = false;
        }
    }

    /// Reconcile API tasks with local task entries + auto-provision a
    /// Workspace for each running/blocked task that doesn't have one bound.
    fn reconcile_tasks(&mut self, tasks: Vec<Task>) {
        // Save cursor context for restoration: remember the workspace id and
        // session label the cursor was on.
        let saved_ws_id = match &self.cursor {
            Cursor::Workspace(wi) => self.workspaces.get(*wi).map(|w| w.id.clone()),
            Cursor::Session(wi, _) => self.workspaces.get(*wi).map(|w| w.id.clone()),
            Cursor::Task { ws_idx, .. } => self.workspaces.get(*ws_idx).map(|w| w.id.clone()),
        };
        let saved_session_uid = match &self.cursor {
            Cursor::Session(wi, si) => self
                .workspaces
                .get(*wi)
                .and_then(|w| w.sessions.get(*si))
                .map(|s| s.uid.clone()),
            _ => None,
        };

        let mut seen_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for task in &tasks {
            // Only show active tasks in the sessions view; backlog/draft/done
            // stay in the planning view.
            match task.status.as_str() {
                "running" | "blocked" => {}
                _ => continue,
            }
            seen_ids.insert(task.id.clone());

            let display_name = task
                .name
                .as_deref()
                .or(task.prompt.as_deref())
                .unwrap_or(&task.id[..8.min(task.id.len())])
                .chars()
                .take(60)
                .collect::<String>();

            let is_cloud = task.is_cloud;
            // Local recovery covers both top-level CM branches (`cm/...`)
            // AND subtask branches (`cm-sub/...`). Pre-fix, `cm-sub/`
            // tasks reloaded with `workspace_id = None` after a manifest
            // loss because reconcile only matched `cm/` — leaving
            // start_session, workflow launch, and cleanup unable to
            // find the workspace.
            let is_local = !is_cloud
                && task.wip_branch.as_ref().map_or(false, |b| {
                    b.starts_with("cm/") || b.starts_with("cm-sub/")
                });

            // Upsert TaskEntry.
            if let Some(entry) = self
                .tasks
                .iter_mut()
                .find(|e| e.task_id.as_deref() == Some(&task.id))
            {
                entry.name = display_name.clone();
                entry.api_status = TaskStatus::from_api(&task.status);
                entry.repo_url = Some(task.repo_url.clone());
                entry.prompt = task.prompt.clone();
                entry.wip_branch = task.wip_branch.clone();
                entry.session_id = task.session_id.clone();
                entry.blocked_at = task.blocked_at.clone();
                entry.is_cloud = is_cloud;
                entry.project = task.project.clone();
                entry.parent_task_id = task.parent_task_id.clone();
                entry.worktree_mode = parse_worktree_mode(&task.worktree_mode);
                entry.metadata = task.metadata.clone();
            } else {
                self.tasks.push(TaskEntry {
                    task_id: Some(task.id.clone()),
                    name: display_name.clone(),
                    api_status: TaskStatus::from_api(&task.status),
                    repo_url: Some(task.repo_url.clone()),
                    prompt: task.prompt.clone(),
                    wip_branch: task.wip_branch.clone(),
                    session_id: task.session_id.clone(),
                    blocked_at: task.blocked_at.clone(),
                    is_cloud,
                    workspace_id: None,
                    project: task.project.clone(),
                    parent_task_id: task.parent_task_id.clone(),
                    worktree_mode: parse_worktree_mode(&task.worktree_mode),
                    metadata: task.metadata.clone(),
                });
            }

            // Link (or create) a Workspace for this task if it doesn't already
            // have one. Multi-task workspaces: users explicitly bind via the
            // launch-into-workspace picker, so we only auto-bind when the
            // task's own worktree (local) or VM (cloud) matches.
            let task_idx = self
                .tasks
                .iter()
                .position(|t| t.task_id.as_deref() == Some(&task.id))
                .expect("just inserted");
            if self.tasks[task_idx].workspace_id.is_some() {
                continue;
            }

            // Honor manifest binding before auto-provisioning. On the first
            // reconcile tick self.workspaces is still empty (restore_sessions
            // runs right after), so without this we'd spawn an orphan that
            // restore_sessions later supersedes via bindings.
            if let Some(ws_id) = self.manifest_bindings.get(&task.id) {
                self.tasks[task_idx].workspace_id = Some(ws_id.clone());
                continue;
            }

            let (worktree_path, main_repo_path) = if is_local {
                // Single resolver handles both `cm/<slug>` and
                // `cm-sub/<chain>-<short>` layouts. See
                // `worktree::recover_worktree_path`.
                let wt = task
                    .wip_branch
                    .as_ref()
                    .and_then(|b| worktree::recover_worktree_path(&task.repo_url, b));
                let main = wt.is_some().then(|| worktree::find_local_repo(&task.repo_url)).flatten();
                (wt, main)
            } else {
                (None, None)
            };

            // Match an existing workspace:
            //   - local: same worktree_path
            //   - cloud: same worker_vm (VM uniquely identifies the cloud workspace)
            let existing_ws_idx = if is_cloud {
                task.worker_vm.as_deref().filter(|s| !s.is_empty()).and_then(|vm| {
                    self.workspaces
                        .iter()
                        .position(|w| w.is_cloud && w.worker_vm.as_deref() == Some(vm))
                })
            } else {
                worktree_path.as_ref().and_then(|wt| {
                    self.workspaces
                        .iter()
                        .position(|w| w.worktree_path.as_deref() == Some(wt.as_path()))
                })
            };

            let ws_id = if let Some(wi) = existing_ws_idx {
                self.workspaces[wi].id.clone()
            } else if is_cloud || worktree_path.is_some() {
                // Auto-provision a workspace so this task gets a sidebar row.
                let ws = Workspace {
                    color: None,
                    pinned: false,
                    id: new_workspace_id(),
                    name: display_name.clone(),
                    is_closed: false,
                    is_cloud,
                    repo_url: Some(task.repo_url.clone()),
                    worktree_path,
                    main_repo_path,
                    worker_vm: task.worker_vm.clone(),
                    worker_zone: task.worker_zone.clone(),
                    // Cloud (GCP-worker) tasks aren't a daemon `hosts.toml` host;
                    // default the daemon-host attribute to local.
                    host_id: cm_daemon::host_id::HostId::local(),
                    sessions: vec![],
                    tombstones: Vec::new(),
                    is_pushing: false,
                };
                let id = ws.id.clone();
                self.workspaces.push(ws);
                id
            } else {
                continue;
            };
            self.tasks[task_idx].workspace_id = Some(ws_id);
        }

        // Retain tasks: keep those still seen by the API, plus anything still
        // referenced by a workspace (in case a bound task temporarily vanished
        // from the API — unlikely but defensive).
        let ws_bound_task_ids: std::collections::HashSet<String> = self
            .workspaces
            .iter()
            .flat_map(|w| {
                self.tasks
                    .iter()
                    .filter(move |t| t.workspace_id.as_deref() == Some(&w.id))
                    .filter_map(|t| t.task_id.clone())
            })
            .collect();
        self.tasks.retain(|t| {
            if t.api_status == TaskStatus::Done {
                return false;
            }
            match &t.task_id {
                Some(id) => {
                    seen_ids.contains(id)
                        || ws_bound_task_ids.contains(id)
                }
                None => false,
            }
        });

        // Also GC workspaces whose worker_vm-based cloud task is gone.
        // Keep local workspaces always (they survive task lifecycle).
        self.workspaces.retain(|w| {
            if !w.is_cloud {
                return true;
            }
            let vm = match w.worker_vm.as_deref() {
                Some(vm) if !vm.is_empty() => vm,
                _ => return true,
            };
            tasks.iter().any(|t| {
                t.is_cloud
                    && t.worker_vm.as_deref() == Some(vm)
                    && matches!(t.status.as_str(), "running" | "blocked")
            })
        });

        // Sort workspaces by effective status (via their first bound task if
        // any). No bound task → put last.
        let status_rank = |s: &TaskStatus| -> u8 {
            match s {
                TaskStatus::Running => 0,
                TaskStatus::Blocked => 1,
                TaskStatus::Backlog => 2,
                TaskStatus::Done => 3,
            }
        };
        let workspace_rank: Vec<(String, u8)> = self
            .workspaces
            .iter()
            .map(|w| {
                let rank = match self.first_task_for_ws(&w.id) {
                    Some(t) => status_rank(&self.task_status(t)),
                    // No bound task: rank by the workspace's own session
                    // activity, mirroring how `task_status` derives a bound
                    // task's status. This makes a taskless workspace sort
                    // identically to a task-bound one — an active (running)
                    // session floats up alongside running tasks instead of
                    // always sinking to the bottom. A sessionless taskless
                    // workspace still ranks last (it's a fresh, empty slot).
                    None => {
                        if w.sessions.iter().any(|s| s.status == SessionStatus::Running) {
                            status_rank(&TaskStatus::Running)
                        } else if w.sessions.iter().any(|s| s.status == SessionStatus::Idle) {
                            status_rank(&TaskStatus::Blocked)
                        } else {
                            4
                        }
                    }
                };
                (w.id.clone(), rank)
            })
            .collect();
        let rank_of = |id: &str| -> u8 {
            workspace_rank
                .iter()
                .find(|(i, _)| i == id)
                .map(|(_, r)| *r)
                .unwrap_or(4)
        };
        // Pinned workspaces float to the top; status rank orders each
        // group. Stable sort keeps ties in their previous order.
        self.workspaces
            .sort_by_key(|w| (!w.pinned, rank_of(&w.id)));

        // Restore cursor by workspace id.
        if let Some(ref id) = saved_ws_id {
            if let Some(wi) = self.workspaces.iter().position(|w| &w.id == id) {
                if let Some(ref uid) = saved_session_uid {
                    if let Some(si) = self.workspaces[wi]
                        .sessions
                        .iter()
                        .position(|s| &s.uid == uid)
                    {
                        self.cursor = Cursor::Session(wi, si);
                    } else {
                        self.cursor = Cursor::Workspace(wi);
                    }
                } else {
                    self.cursor = Cursor::Workspace(wi);
                }
            }
        }
        self.clamp_cursor();
        // Sub-2a Finding #1: push the refreshed task tree to the
        // daemon. Covers TasksUpdated (startup + every backend
        // refresh), parent_task_id changes, and task add/remove
        // diffs from the API. Per-site pushes elsewhere catch the
        // local-only mutations that bypass this path
        // (delete_task, launch_*, resume_locally).
        self.push_state_to_daemon();
    }
}

/// 10e-c: tests for the `App::apply_manifest_diff` consumer-side
/// application of daemon-broadcast `ManifestDiff`s. Exercises the
/// integration point between the manifest.watch consumer thread
/// (which sends `ManifestDiff`s through `manifest_watch_rx`) and
/// the TUI's in-memory session state.
///
/// Constructing a full `App` for these tests is heavy (backend
/// thread, control socket, preflight, etc.). Instead we build a
/// minimal `App` shell via the same trick the existing
/// `stop_workflow_local_cleanup_tests` module uses: focused
/// field-level state + direct method invocation. The method is
/// `pub(crate)` to enable this.
#[cfg(test)]
mod apply_manifest_diff_tests {
    use super::*;
    use cm_daemon::manifest::{LastExit, ManifestDiff};

    /// Build a minimal `App` with a single workspace containing
    /// one terminal session. The session's `preserved_last_exit`
    /// starts as `None` (the default for a fresh session per
    /// `make_simple_session_with_uid` line 480).
    ///
    /// The session's PTY is `/bin/true` because constructing a
    /// `Session` requires a real PTY. The test only cares about
    /// `preserved_last_exit`; the PTY exit doesn't matter (and
    /// Drop SIGKILLs the child cleanly anyway).
    fn build_app_with_session(uid: &str) -> App {
        // Spawn a real PTY via /bin/true so Session::new succeeds.
        // The session immediately exits; that's irrelevant for
        // the preserved_last_exit assertion.
        let session = crate::session::Session::new(
            "/bin/true",
            &[],
            80,
            24,
            None,
            std::collections::HashMap::new(),
            None,
        )
        .expect("test PTY");
        let ts = make_simple_session_with_uid(
            uid.to_string(),
            "test-label",
            "claude",
            session,
            None,
        );
        // Minimal Workspace.
        let ws = Workspace {
            color: None,
            pinned: false,
            id: "ws-test".into(),
            name: "test-ws".into(),
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
        };

        // Build App via the standard ctor THEN inject the
        // workspace. App::new needs a Config; the test_support
        // home_lock guards against $HOME mutations colliding
        // with other tests (App::new reads ~/.cm/...).
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        // Restore HOME for the rest of the test process.
        if let Some(h) = orig_home {
            unsafe {
                std::env::set_var("HOME", h);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }
        // Inject our test workspace. Leak the tempdir so the
        // backend thread (still alive) doesn't error on later
        // ~/.cm accesses — App::new spawns it before we restored
        // HOME, so it captured the tempdir path. Acceptable
        // memory cost for a unit test.
        std::mem::forget(tmp);
        app.workspaces.push(ws);
        app
    }

    /// T15 — `apply_manifest_diff` with `ManifestDiff::Exited`
    /// updates `preserved_last_exit` on the matching session.
    #[test]
    fn apply_exited_diff_updates_preserved_last_exit_on_matching_session() {
        let mut app = build_app_with_session("ts-t15");
        // Pre-condition: preserved_last_exit is None.
        assert!(app.workspaces[0].sessions[0].preserved_last_exit.is_none());

        let diff = ManifestDiff::Exited {
            uid: "ts-t15".into(),
            last_exit: LastExit {
                code: None,
                memory_cap_kill: true,
                kills_file_offset: Some(42),
                exited_at: 1.0,
            },
        };
        app.apply_manifest_diff(diff);

        let got = app.workspaces[0].sessions[0]
            .preserved_last_exit
            .as_ref()
            .expect("apply must populate preserved_last_exit");
        assert!(got.memory_cap_kill, "memory_cap_kill flag must transfer");
        assert_eq!(got.kills_file_offset, Some(42));
        assert!(
            app.needs_redraw,
            "needs_redraw must be set so the next render \
             picks up status indicator changes",
        );
    }

    /// T16 — `ManifestDiff::Exited` with an unknown uid is a
    /// silent no-op (R5). No panic; no mutation to unrelated
    /// sessions.
    #[test]
    fn apply_exited_diff_with_unknown_uid_is_silent_noop() {
        let mut app = build_app_with_session("ts-t16-known");
        app.needs_redraw = false; // reset

        let diff = ManifestDiff::Exited {
            uid: "ts-t16-stranger".into(),
            last_exit: LastExit {
                code: Some(1),
                memory_cap_kill: false,
                kills_file_offset: None,
                exited_at: 1.0,
            },
        };
        app.apply_manifest_diff(diff);

        // Unrelated session unchanged.
        assert!(
            app.workspaces[0].sessions[0]
                .preserved_last_exit
                .is_none(),
            "unknown uid must NOT mutate unrelated sessions' \
             preserved_last_exit",
        );
        // No redraw triggered by a no-op apply.
        assert!(
            !app.needs_redraw,
            "unknown uid must NOT trigger a redraw",
        );
    }

    /// Build a fresh `/bin/true`-backed `TerminalSession` so the
    /// exit-prune tests can add distinct rows to a workspace. Fields
    /// default (`managed_by_uid == None`, `workflow_run_id == None`,
    /// `host_id == local`); tests mutate the ones they care about.
    fn make_exit_prune_session(uid: &str) -> TerminalSession {
        let session = crate::session::Session::new(
            "/bin/true",
            &[],
            80,
            24,
            None,
            std::collections::HashMap::new(),
            None,
        )
        .expect("test PTY");
        make_simple_session_with_uid(
            uid.to_string(),
            "test-label",
            "claude",
            session,
            None,
        )
    }

    /// An agent-spawned (`managed_by_uid` set), non-workflow session
    /// that receives `ManifestDiff::Exited` is PRUNED from the sidebar
    /// — the orchestrator spawn-and-kill ghost-row fix. The daemon has
    /// already dropped it from its live registry (a `kill_session` RPC
    /// from outside the TUI), so the row must go, matching the A-w
    /// removal convention.
    #[test]
    fn exited_diff_prunes_agent_managed_session() {
        let mut app = build_app_with_session("ts-agent");
        app.workspaces[0].sessions[0].managed_by_uid = Some("orch".into());
        assert_eq!(app.workspaces[0].sessions.len(), 1);

        app.apply_manifest_diff(ManifestDiff::Exited {
            uid: "ts-agent".into(),
            last_exit: LastExit {
                code: Some(0),
                memory_cap_kill: false,
                kills_file_offset: None,
                exited_at: 1.0,
            },
        });

        assert!(
            app.workspaces[0].sessions.is_empty(),
            "an agent-managed session must be removed from the \
             sidebar on exit",
        );
    }

    /// A USER-created session (`managed_by_uid == None`) is NOT
    /// auto-removed on exit — it stays a ghost row with
    /// `preserved_last_exit` set so the user (who owns its lifecycle)
    /// closes it with A-w. Only its exit metadata is recorded.
    #[test]
    fn exited_diff_keeps_user_session_as_ghost() {
        let mut app = build_app_with_session("ts-user");
        assert!(app.workspaces[0].sessions[0].managed_by_uid.is_none());

        app.apply_manifest_diff(ManifestDiff::Exited {
            uid: "ts-user".into(),
            last_exit: LastExit {
                code: Some(0),
                memory_cap_kill: false,
                kills_file_offset: None,
                exited_at: 1.0,
            },
        });

        assert_eq!(
            app.workspaces[0].sessions.len(),
            1,
            "user-created session must NOT be auto-removed on exit",
        );
        assert!(
            app.workspaces[0].sessions[0].preserved_last_exit.is_some(),
            "the ghost row still records its exit metadata",
        );
    }

    /// A WORKFLOW PARTICIPANT (`workflow_run_id` set) is never pruned
    /// on an exit diff even when it carries `managed_by_uid` — its slot
    /// must SURVIVE a fresh-context respawn (the daemon kills+respawns
    /// the PTY under the same slot), which would otherwise delete the
    /// row mid-respawn.
    #[test]
    fn exited_diff_keeps_workflow_participant() {
        let mut app = build_app_with_session("ts-wf");
        app.workspaces[0].sessions[0].managed_by_uid = Some("orch".into());
        app.workspaces[0].sessions[0].workflow_run_id = Some("run-1".into());

        app.apply_manifest_diff(ManifestDiff::Exited {
            uid: "ts-wf".into(),
            last_exit: LastExit {
                code: None,
                memory_cap_kill: false,
                kills_file_offset: None,
                exited_at: 1.0,
            },
        });

        assert_eq!(
            app.workspaces[0].sessions.len(),
            1,
            "workflow-participant slot must survive a fresh-respawn \
             exit diff",
        );
    }

    /// Remote-host repro: a cm-manager-hosted agent session whose exit
    /// diff arrives on the per-host `manifest.watch` stream is pruned
    /// via the host-aware apply path (the actual reported bug — a
    /// remote session viewed from the laptop TUI).
    #[test]
    fn exited_diff_prunes_remote_agent_managed_session() {
        let mut app = build_app_with_session("ts-remote");
        app.workspaces[0].sessions[0].managed_by_uid = Some("orch".into());
        let remote = cm_daemon::host_id::HostId::new("manager");
        app.workspaces[0].sessions[0].host_id = remote.clone();

        app.apply_manifest_diff_from_host(
            remote,
            ManifestDiff::Exited {
                uid: "ts-remote".into(),
                last_exit: LastExit {
                    code: None,
                    memory_cap_kill: false,
                    kills_file_offset: None,
                    exited_at: 1.0,
                },
            },
        );

        assert!(
            app.workspaces[0].sessions.is_empty(),
            "a remote agent-managed session must be pruned on exit \
             (host-agnostic apply path)",
        );
    }

    /// Cursor safety: when the cursor sits on the pruned row, the apply
    /// path's `clamp_cursor` demotes it to the workspace rather than
    /// leaving a dangling `Session(wi, si)` that later indexing (e.g.
    /// the focused-terminal render) would panic on.
    #[test]
    fn exited_diff_prune_clamps_cursor_off_removed_row() {
        let mut app = build_app_with_session("ts-cursor");
        app.workspaces[0].sessions[0].managed_by_uid = Some("orch".into());
        app.cursor = Cursor::Session(0, 0);

        app.apply_manifest_diff(ManifestDiff::Exited {
            uid: "ts-cursor".into(),
            last_exit: LastExit {
                code: Some(0),
                memory_cap_kill: false,
                kills_file_offset: None,
                exited_at: 1.0,
            },
        });

        assert!(app.workspaces[0].sessions.is_empty());
        assert!(
            matches!(app.cursor, Cursor::Workspace(0)),
            "cursor must clamp off the removed row, got {:?}",
            app.cursor,
        );
    }

    /// A cap-killed agent session still fires the cap-kill toast AND is
    /// pruned — the toast fires before the removal so the activity feed
    /// records why it died.
    #[test]
    fn exited_diff_cap_kill_toasts_and_prunes_agent_session() {
        let mut app = build_app_with_session("ts-capkill");
        app.workspaces[0].sessions[0].managed_by_uid = Some("orch".into());

        app.apply_manifest_diff(ManifestDiff::Exited {
            uid: "ts-capkill".into(),
            last_exit: LastExit {
                code: None,
                memory_cap_kill: true,
                kills_file_offset: Some(9),
                exited_at: 1.0,
            },
        });

        assert!(
            app.cap_kill_toasted.contains("ts-capkill"),
            "cap-kill toast must fire for the killed agent session",
        );
        assert!(
            app.workspaces[0].sessions.is_empty(),
            "a cap-killed agent session must also be pruned",
        );
    }

    /// Idempotent: a duplicate `Exited` diff for an already-pruned uid
    /// is a silent no-op (`found` stays false) — no panic, no spurious
    /// redraw. Covers replayed diffs / snapshot+diff overlap.
    #[test]
    fn exited_diff_duplicate_after_prune_is_noop() {
        let mut app = build_app_with_session("ts-dup");
        app.workspaces[0].sessions[0].managed_by_uid = Some("orch".into());

        let make = || ManifestDiff::Exited {
            uid: "ts-dup".into(),
            last_exit: LastExit {
                code: Some(0),
                memory_cap_kill: false,
                kills_file_offset: None,
                exited_at: 1.0,
            },
        };
        app.apply_manifest_diff(make());
        assert!(app.workspaces[0].sessions.is_empty());

        app.needs_redraw = false;
        // Second diff for the now-absent uid: R5 silent no-op.
        app.apply_manifest_diff(make());
        assert!(app.workspaces[0].sessions.is_empty());
        assert!(
            !app.needs_redraw,
            "a duplicate exit diff for a removed uid must not redraw",
        );
    }

    /// Only the TARGET row is pruned — a sibling agent session in the
    /// same workspace is untouched and stays indexable. Guards against
    /// the predicate matching too broadly.
    #[test]
    fn exited_diff_prune_leaves_sibling_sessions() {
        let mut app = build_app_with_session("ts-keep");
        app.workspaces[0].sessions[0].managed_by_uid = Some("orch".into());
        let mut sib = make_exit_prune_session("ts-drop");
        sib.managed_by_uid = Some("orch".into());
        app.workspaces[0].sessions.push(sib);
        assert_eq!(app.workspaces[0].sessions.len(), 2);

        app.apply_manifest_diff(ManifestDiff::Exited {
            uid: "ts-drop".into(),
            last_exit: LastExit {
                code: Some(0),
                memory_cap_kill: false,
                kills_file_offset: None,
                exited_at: 1.0,
            },
        });

        assert_eq!(
            app.workspaces[0].sessions.len(),
            1,
            "only the exited target is removed",
        );
        assert_eq!(app.workspaces[0].sessions[0].uid, "ts-keep");
    }

    /// T20-companion — non-Exited variants (`Added`, `Updated`,
    /// `Tombstoned`) are no-ops in 10e-c. Pin to catch future
    /// scope creep that would silently start consuming them
    /// without explicit handling.
    #[test]
    fn apply_non_exited_variants_are_noop_in_10e_c() {
        let mut app = build_app_with_session("ts-t20");
        app.needs_redraw = false;

        // Added: no preserved_last_exit mutation, no redraw.
        app.apply_manifest_diff(ManifestDiff::Added {
            uid: "ts-t20".into(),
            entry: serde_json::Value::Null,
        });
        assert!(app.workspaces[0].sessions[0].preserved_last_exit.is_none());
        assert!(!app.needs_redraw);

        // Updated: same.
        app.apply_manifest_diff(ManifestDiff::Updated {
            uid: "ts-t20".into(),
            entry: serde_json::Value::Null,
        });
        assert!(app.workspaces[0].sessions[0].preserved_last_exit.is_none());
        assert!(!app.needs_redraw);

        // Tombstoned: same.
        app.apply_manifest_diff(ManifestDiff::Tombstoned {
            uid: "ts-t20".into(),
            exited_at: 1.0,
        });
        assert!(app.workspaces[0].sessions[0].preserved_last_exit.is_none());
        assert!(!app.needs_redraw);
    }

    /// T22 (10e-c r1 F1) — `apply_manifest_snapshot` adopts the
    /// daemon's last_exit when the local field is `None`, AND
    /// triggers a redraw. This is the "stale-None clobber" fix:
    /// pre-r1 the snapshot was ignored on reconnect, so a session
    /// the daemon already knew about (with last_exit set) would
    /// keep showing as live until something else updated it.
    #[test]
    fn apply_snapshot_adopts_last_exit_when_local_is_none() {
        let mut app = build_app_with_session("ts-t22");
        app.needs_redraw = false;
        assert!(app.workspaces[0].sessions[0].preserved_last_exit.is_none());

        let last_exit = LastExit {
            code: Some(137),
            memory_cap_kill: true,
            kills_file_offset: Some(7),
            exited_at: 1.0,
        };
        let payload = crate::manifest_watch::ManifestSnapshotPayload {
            session_last_exits: vec![(
                "ts-t22".into(),
                Some(last_exit.clone()),
            )],
        };
        app.apply_manifest_snapshot(payload);

        let got = app.workspaces[0].sessions[0]
            .preserved_last_exit
            .as_ref()
            .expect("snapshot must populate preserved_last_exit when local is None");
        assert_eq!(got, &last_exit);
        assert!(app.needs_redraw, "snapshot adoption must request redraw");
    }

    /// T23 (10e-c r1 F1) — conservative merge: when the local
    /// `preserved_last_exit` is already `Some(...)`, the snapshot's
    /// value must NOT overwrite it. Local information wins because
    /// it's typically fresher than the daemon's startup snapshot.
    #[test]
    fn apply_snapshot_does_not_overwrite_existing_local_last_exit() {
        let mut app = build_app_with_session("ts-t23");
        let local_exit = LastExit {
            code: Some(0),
            memory_cap_kill: false,
            kills_file_offset: None,
            exited_at: 2.0,
        };
        app.workspaces[0].sessions[0].preserved_last_exit = Some(local_exit.clone());
        app.needs_redraw = false;

        let snapshot_exit = LastExit {
            code: Some(137),
            memory_cap_kill: true,
            kills_file_offset: Some(99),
            exited_at: 1.0,
        };
        let payload = crate::manifest_watch::ManifestSnapshotPayload {
            session_last_exits: vec![(
                "ts-t23".into(),
                Some(snapshot_exit),
            )],
        };
        app.apply_manifest_snapshot(payload);

        let got = app.workspaces[0].sessions[0]
            .preserved_last_exit
            .as_ref()
            .expect("local last_exit must be preserved");
        assert_eq!(
            got, &local_exit,
            "conservative merge: local Some(...) wins over snapshot's value",
        );
        assert!(
            !app.needs_redraw,
            "no mutation → no redraw",
        );
    }

    // -- 10e-d cap-kill toast surfacing tests (T24-T29) --
    //
    // The unified daemon-path toast (CAP_KILL_TOAST_MESSAGE) is
    // emitted via `try_emit_cap_kill_toast`. Both wire paths
    // (attach-stream End-frame → `cap_kill_notes` → drain; and
    // manifest.watch Exited diff → `apply_manifest_diff`) call
    // the helper and share the `cap_kill_toasted` set.

    fn count_cap_kill_entries(app: &App) -> usize {
        app.activity_log
            .iter()
            .filter(|e| e.summary == CAP_KILL_TOAST_MESSAGE)
            .count()
    }

    /// T24 — DETACHED session (no `daemon_memory_cap_kill` Arc;
    /// truly detached from the attach-stream path) gets a
    /// cap-kill toast when the manifest.watch `Exited` diff
    /// arrives. This is the named-criterion path: detached
    /// sessions surface cap kills via manifest.watch.
    #[test]
    fn detached_cap_kill_via_manifest_watch_fires_toast() {
        let mut app = build_app_with_session("ts-t24");
        // Sanity: build_app_with_session uses /bin/true so
        // `daemon_memory_cap_kill` is None — this session is
        // detached for the purposes of the cap-kill path.
        assert!(app.workspaces[0].sessions[0]
            .session
            .daemon_memory_cap_kill
            .is_none());

        app.apply_manifest_diff(ManifestDiff::Exited {
            uid: "ts-t24".into(),
            last_exit: LastExit {
                code: Some(137),
                memory_cap_kill: true,
                kills_file_offset: Some(1),
                exited_at: 1.0,
            },
        });

        assert_eq!(
            count_cap_kill_entries(&app), 1,
            "detached cap-kill must produce exactly one toast",
        );
        assert!(
            app.cap_kill_toasted.contains("ts-t24"),
            "cap_kill_toasted must mark the uid after emit",
        );
        // Toast string parity check (also pinned by T29 but
        // immediate sanity here).
        assert_eq!(
            app.activity_log.back().unwrap().summary,
            CAP_KILL_TOAST_MESSAGE,
        );
    }

    /// T25 — ATTACHED session's cap-kill produces exactly ONE
    /// toast regardless of which path's signal arrives first.
    /// Runs the scenario twice with the arrival order swapped
    /// to pin order-independence durably.
    #[test]
    fn attached_cap_kill_does_not_double_toast() {
        for order in &["attach-first", "manifest-first"] {
            let mut app = build_app_with_session("ts-t25");
            // Simulate the attached-session shape: install a
            // `daemon_memory_cap_kill` Arc that the attach-stream
            // reader would latch to true on End frame. We can't
            // run a real PTY here, so the attach-drain path is
            // simulated by directly calling
            // `try_emit_cap_kill_toast` (which is what
            // `drain_terminal_events` calls after observing the
            // flag).
            let diff = ManifestDiff::Exited {
                uid: "ts-t25".into(),
                last_exit: LastExit {
                    code: Some(137),
                    memory_cap_kill: true,
                    kills_file_offset: Some(2),
                    exited_at: 1.0,
                },
            };

            if *order == "attach-first" {
                app.try_emit_cap_kill_toast("ts-t25");
                app.apply_manifest_diff(diff);
            } else {
                app.apply_manifest_diff(diff);
                app.try_emit_cap_kill_toast("ts-t25");
            }

            assert_eq!(
                count_cap_kill_entries(&app), 1,
                "order={}: attached + manifest paths must produce \
                 exactly one toast",
                order,
            );
            assert!(app.cap_kill_toasted.contains("ts-t25"));
        }
    }

    /// T26 — applying the same `Exited` diff twice (network
    /// replay, snapshot+diff overlap during reconnect) toasts
    /// exactly once. The set is the load-bearing de-dup.
    #[test]
    fn duplicate_manifest_diff_toasts_once() {
        let mut app = build_app_with_session("ts-t26");
        let make_diff = || ManifestDiff::Exited {
            uid: "ts-t26".into(),
            last_exit: LastExit {
                code: Some(137),
                memory_cap_kill: true,
                kills_file_offset: Some(3),
                exited_at: 1.0,
            },
        };
        app.apply_manifest_diff(make_diff());
        app.apply_manifest_diff(make_diff());
        assert_eq!(count_cap_kill_entries(&app), 1);
    }

    /// T27 — `apply_manifest_snapshot` adopts last_exit when
    /// local is None AND fires toast iff the adopted value has
    /// `memory_cap_kill=true`. This is the load-bearing F1
    /// reconciliation case: post-reconnect snapshot recovers
    /// cap-kill state the TUI missed.
    #[test]
    fn snapshot_adoption_with_cap_kill_fires_toast() {
        let mut app = build_app_with_session("ts-t27");
        assert!(app.workspaces[0].sessions[0]
            .preserved_last_exit
            .is_none());

        let payload = crate::manifest_watch::ManifestSnapshotPayload {
            session_last_exits: vec![(
                "ts-t27".into(),
                Some(LastExit {
                    code: Some(137),
                    memory_cap_kill: true,
                    kills_file_offset: Some(4),
                    exited_at: 1.0,
                }),
            )],
        };
        app.apply_manifest_snapshot(payload);
        assert_eq!(count_cap_kill_entries(&app), 1);
        assert!(app.cap_kill_toasted.contains("ts-t27"));
    }

    /// T28 — `apply_manifest_snapshot` when local
    /// `preserved_last_exit` is already `Some` (TUI loaded the
    /// value from disk at startup) does NOT re-fire the toast.
    /// Conservative-merge skips the adoption; we only call the
    /// helper inside the if-None branch, so try_emit isn't
    /// reached.
    #[test]
    fn snapshot_when_local_already_some_does_not_toast() {
        let mut app = build_app_with_session("ts-t28");
        // Pretend disk-load already populated this from the
        // previous TUI session's manifest write.
        app.workspaces[0].sessions[0].preserved_last_exit =
            Some(LastExit {
                code: Some(137),
                memory_cap_kill: true,
                kills_file_offset: Some(5),
                exited_at: 1.0,
            });
        assert!(app.cap_kill_toasted.is_empty());

        let payload = crate::manifest_watch::ManifestSnapshotPayload {
            session_last_exits: vec![(
                "ts-t28".into(),
                Some(LastExit {
                    code: Some(137),
                    memory_cap_kill: true,
                    kills_file_offset: Some(5),
                    exited_at: 1.0,
                }),
            )],
        };
        app.apply_manifest_snapshot(payload);

        assert_eq!(
            count_cap_kill_entries(&app), 0,
            "snapshot reconciliation when local is already Some \
             MUST NOT re-toast: the cap-kill happened in the past, \
             toasting now would surface stale UI noise",
        );
        assert!(
            app.cap_kill_toasted.is_empty(),
            "no emit → set stays empty",
        );
    }

    /// T29 (parity, daemon-path only) — both the attach-stream
    /// drain helper call AND the manifest.watch diff path
    /// produce IDENTICAL `summary` strings in the activity feed.
    /// Local-spawn `MemoryKillEvent::Killed` uses a richer
    /// PID/comm/RSS format and is intentionally different
    /// (separate surface, separate signal); not asserted here.
    #[test]
    fn attached_and_detached_daemon_paths_produce_identical_toast_string() {
        // Attached path (simulate by direct helper call as in
        // T25's "attach-first" half).
        let mut app_attached = build_app_with_session("ts-t29a");
        app_attached.try_emit_cap_kill_toast("ts-t29a");
        let attached_summary =
            app_attached.activity_log.back().unwrap().summary.clone();

        // Detached path via manifest.watch diff.
        let mut app_detached = build_app_with_session("ts-t29d");
        app_detached.apply_manifest_diff(ManifestDiff::Exited {
            uid: "ts-t29d".into(),
            last_exit: LastExit {
                code: Some(137),
                memory_cap_kill: true,
                kills_file_offset: Some(7),
                exited_at: 1.0,
            },
        });
        let detached_summary =
            app_detached.activity_log.back().unwrap().summary.clone();

        assert_eq!(
            attached_summary, detached_summary,
            "daemon-path toast strings must match (T29 parity)",
        );
        assert_eq!(
            attached_summary, CAP_KILL_TOAST_MESSAGE,
            "both paths must use the unified constant",
        );
    }

    /// T-respawn (10e-d helper-pin) — `clear_cap_kill_toast_state`
    /// releases the de-dup entry so a re-spawned session under
    /// the same uid can toast again. Production uids are
    /// monotonic so this case is rare, but the helper exists
    /// for test paths AND as the spawn-path contract anchor.
    #[test]
    fn clear_cap_kill_toast_state_re_enables_emit_for_same_uid() {
        let mut app = build_app_with_session("ts-respawn");
        // First spawn: cap-killed.
        assert!(app.try_emit_cap_kill_toast("ts-respawn"));
        assert_eq!(count_cap_kill_entries(&app), 1);
        // Same uid again WITHOUT clearing → suppressed.
        assert!(!app.try_emit_cap_kill_toast("ts-respawn"));
        assert_eq!(count_cap_kill_entries(&app), 1);

        // Simulate re-spawn under the same uid: clear releases.
        app.clear_cap_kill_toast_state("ts-respawn");
        assert!(!app.cap_kill_toasted.contains("ts-respawn"));
        // After clear, helper emits again.
        assert!(app.try_emit_cap_kill_toast("ts-respawn"));
        assert_eq!(
            count_cap_kill_entries(&app), 2,
            "post-clear re-emit must land a second activity-feed \
             entry — pre-fix the stale set entry would suppress it",
        );
    }
}

#[cfg(test)]
mod ready_tests {
    use super::*;

    fn pw(floor_secs: u64, quiet_secs: u64, deadline_secs: u64) -> (PendingWrite, Instant) {
        let now = Instant::now();
        (
            PendingWrite {
                text: "hi".into(),
                submit: true,
                earliest_deliver_at: now + Duration::from_secs(floor_secs),
                require_quiet: Duration::from_secs(quiet_secs),
                hard_deadline: now + Duration::from_secs(deadline_secs),
            },
            now,
        )
    }

    #[test]
    fn not_ready_before_floor() {
        let (p, now) = pw(5, 2, 60);
        // Early — floor not reached
        assert!(!pending_write_ready(&[], &p, now));
        // At floor with no wakeups — ready
        assert!(pending_write_ready(&[], &p, now + Duration::from_secs(5)));
    }

    #[test]
    fn not_ready_while_pty_noisy() {
        let (p, now) = pw(1, 2, 60);
        let check_at = now + Duration::from_secs(3);
        // Wakeup 0.5s ago — still within quiet window
        let recent = check_at - Duration::from_millis(500);
        assert!(!pending_write_ready(&[recent], &p, check_at));
    }

    #[test]
    fn ready_after_pty_goes_quiet() {
        let (p, now) = pw(1, 2, 60);
        let check_at = now + Duration::from_secs(10);
        // Last wakeup 5s ago — outside 2s quiet window
        let old = check_at - Duration::from_secs(5);
        assert!(pending_write_ready(&[old], &p, check_at));
    }

    #[test]
    fn deadline_forces_delivery_even_if_noisy() {
        let (p, now) = pw(1, 2, 10);
        let check_at = now + Duration::from_secs(11);
        let recent = check_at - Duration::from_millis(100); // noisy
        assert!(pending_write_ready(&[recent], &p, check_at));
    }

    #[test]
    fn empty_wakeups_is_ready_past_floor() {
        let (p, now) = pw(1, 2, 60);
        assert!(pending_write_ready(&[], &p, now + Duration::from_secs(2)));
    }
}

#[cfg(test)]
mod enter_encoding_tests {
    use super::*;

    #[test]
    fn raw_cr_when_kitty_mode_off() {
        let mode = TermMode::empty();
        assert_eq!(enter_bytes_for_mode(mode), b"\r");
    }

    #[test]
    fn kitty_csi_when_disambiguate_on() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(enter_bytes_for_mode(mode), b"\x1b[13u");
    }

    #[test]
    fn kitty_csi_when_disambiguate_set_alongside_other_modes() {
        // Real sessions carry many mode bits at once. We only care about the
        // one that drives Enter encoding.
        let mode = TermMode::DISAMBIGUATE_ESC_CODES
            | TermMode::ALT_SCREEN
            | TermMode::BRACKETED_PASTE;
        assert_eq!(enter_bytes_for_mode(mode), b"\x1b[13u");
    }
}

#[cfg(test)]
mod body_delivery_tests {
    //! Pins down the byte-formatting we use when delivering a workflow
    //! activation prompt body to a session's PTY. The hypothesis driving
    //! these tests: codex's input handler wedges on large multi-line raw
    //! writes — the trailing Enter is ignored — but accepts the same content
    //! cleanly when wrapped in bracketed-paste markers (`\x1b[200~ … \x1b[201~`),
    //! the same wrapping we already use for user-typed pastes (see the
    //! `CrosstermEvent::Paste` handler).
    //!
    //! These are unit tests over the byte-formatting helper alone; they
    //! don't validate codex's runtime behavior. Final confirmation is a
    //! manual reproduction in the TUI against a real codex worker session.

    use super::*;

    #[test]
    fn multiline_body_wrapped_when_bracketed_paste_enabled() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::BRACKETED_PASTE;
        let body = "do thing\n\nstep 1\nstep 2";
        let out = format_body_for_delivery(body, mode);
        assert!(
            out.starts_with(b"\x1b[200~"),
            "multi-line body should start with paste-begin marker: {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            out.ends_with(b"\x1b[201~"),
            "multi-line body should end with paste-end marker: {:?}",
            String::from_utf8_lossy(&out)
        );
        let expected = format!("\x1b[200~{}\x1b[201~", body);
        assert_eq!(out, expected.as_bytes());
    }

    #[test]
    fn multiline_body_raw_when_bracketed_paste_disabled() {
        // Older / non-bracket-paste-aware agents see raw bytes. We must not
        // emit paste markers because the agent would render them as literal
        // `[200~`, `[201~` in its input box.
        let mode = TermMode::DISAMBIGUATE_ESC_CODES; // no BRACKETED_PASTE
        let body = "do thing\nstep 1\nstep 2";
        let out = format_body_for_delivery(body, mode);
        assert_eq!(out, body.as_bytes());
    }

    #[test]
    fn single_line_body_stays_raw_even_with_bracketed_paste() {
        // Slash commands (`/clear`, `/compact`, etc.) are always single-line.
        // Wrapping them in paste markers risks the agent treating them as
        // pasted text instead of a typed command. Newline absence is the
        // signal: real activation prompts always span multiple lines.
        let mode = TermMode::BRACKETED_PASTE;
        let body = "/clear";
        let out = format_body_for_delivery(body, mode);
        assert_eq!(out, body.as_bytes());
    }

    #[test]
    fn empty_body_stays_raw() {
        let mode = TermMode::BRACKETED_PASTE;
        let out = format_body_for_delivery("", mode);
        assert!(out.is_empty());
    }

    #[test]
    fn embedded_paste_end_marker_is_preserved_verbatim() {
        // We don't try to escape an embedded \x1b[201~ in the body — if the
        // user really included one in an activation prompt, the agent would
        // see paste-end early. This test pins that we do NOT silently
        // mutate the body; if escaping is ever needed, this test will be
        // the place to revisit.
        let mode = TermMode::BRACKETED_PASTE;
        let body = "line one\nweird \x1b[201~ marker\nline three";
        let out = format_body_for_delivery(body, mode);
        let expected = format!("\x1b[200~{}\x1b[201~", body);
        assert_eq!(out, expected.as_bytes());
    }
}

#[cfg(test)]
mod mouse_forwarding_tests {
    //! When the inner app enables mouse tracking (e.g. Claude Code's fullscreen
    //! renderer enters the alternate screen and turns on `?1000`/`?1002` +
    //! `?1006`), the TUI must forward mouse reports to the PTY rather than
    //! consuming the wheel for its own — empty, in the alt screen — scrollback.
    //! These pin the encoding + the per-kind forwarding decision in
    //! `encode_mouse_for_pty`.
    use super::*;
    use crossterm::event::{
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    fn ev(kind: MouseEventKind) -> MouseEvent {
        MouseEvent { kind, column: 7, row: 3, modifiers: KeyModifiers::empty() }
    }

    #[test]
    fn scroll_up_forwards_sgr_with_translated_coords() {
        // grid_col/viewport_row are 0-based PTY cells; the SGR encoder re-adds
        // the 1-based offset, so cell (5, 9) reports as column 6, row 10.
        // Wheel-up is SGR button 64.
        let bytes =
            encode_mouse_for_pty(&ev(MouseEventKind::ScrollUp), TermMode::MOUSE_MODE, 5, 9)
                .expect("wheel-up should forward in mouse mode");
        assert_eq!(bytes, b"\x1b[<64;6;10M");
    }

    #[test]
    fn scroll_down_forwards_sgr() {
        // Wheel-down is SGR button 65.
        let bytes =
            encode_mouse_for_pty(&ev(MouseEventKind::ScrollDown), TermMode::MOUSE_MODE, 0, 0)
                .expect("wheel-down should forward in mouse mode");
        assert_eq!(bytes, b"\x1b[<65;1;1M");
    }

    #[test]
    fn left_click_press_and_release_forward() {
        let down = encode_mouse_for_pty(
            &ev(MouseEventKind::Down(MouseButton::Left)),
            TermMode::MOUSE_REPORT_CLICK,
            5,
            9,
        )
        .expect("press should forward");
        assert_eq!(down, b"\x1b[<0;6;10M");
        // Release uses the SGR `m` terminator.
        let up = encode_mouse_for_pty(
            &ev(MouseEventKind::Up(MouseButton::Left)),
            TermMode::MOUSE_REPORT_CLICK,
            5,
            9,
        )
        .expect("release should forward");
        assert_eq!(up, b"\x1b[<0;6;10m");
    }

    #[test]
    fn drag_suppressed_when_only_click_tracking() {
        // ?1000 reports button press/release but not motion. A drag must not be
        // forwarded — returning None lets the caller swallow it.
        let out = encode_mouse_for_pty(
            &ev(MouseEventKind::Drag(MouseButton::Left)),
            TermMode::MOUSE_REPORT_CLICK,
            5,
            9,
        );
        assert!(out.is_none(), "drag should be suppressed without motion tracking");
    }

    #[test]
    fn drag_forwarded_when_button_motion_tracking() {
        // ?1002 (MOUSE_DRAG) reports motion while a button is held.
        let out = encode_mouse_for_pty(
            &ev(MouseEventKind::Drag(MouseButton::Left)),
            TermMode::MOUSE_DRAG,
            5,
            9,
        );
        assert!(out.is_some(), "drag should forward under button-motion tracking");
    }

    #[test]
    fn bare_motion_only_when_any_motion_tracking() {
        assert!(
            encode_mouse_for_pty(&ev(MouseEventKind::Moved), TermMode::MOUSE_DRAG, 5, 9)
                .is_none(),
            "bare motion needs ?1003, not just ?1002"
        );
        assert!(
            encode_mouse_for_pty(&ev(MouseEventKind::Moved), TermMode::MOUSE_MOTION, 5, 9)
                .is_some(),
            "bare motion forwards under any-motion tracking"
        );
    }
}

#[cfg(test)]
mod pty_tracker_parity {
    //! Phase 1 acceptance test for `doc/daemon-side-workflow-orchestration.md`:
    //! replaying a recorded PTY byte stream through the daemon-side tracker
    //! (`cm_daemon::workflow::pty_tracker`) yields the SAME quiet-window and
    //! keyboard-mode decisions the TUI drainer computes for the same bytes.
    //!
    //! Two genuinely independent implementations are pinned equal:
    //!   - keyboard mode: the daemon drives its own alacritty `Term`; the
    //!     reference here drives a second `Term` configured the way
    //!     `tui/src/session.rs` configures the live session term
    //!     (`kitty_keyboard = true`). The shared parser makes mode parity
    //!     structural, but the test still guards the daemon's config + feed
    //!     wiring (drop `kitty_keyboard` and DISAMBIGUATE never sets -> fail).
    //!   - quiet timing: the daemon's `quiet_for` (its own `record_wakeup` +
    //!     `is_quiet`) vs the TUI drainer's `pending_write_ready` over a
    //!     reference wakeup ring built with the TUI's own `record_wakeup` /
    //!     `prune_wakeups`. These are separate code paths in the two crates.
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::term::{Config as TermConfig, Term};
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
    use cm_daemon::workflow::pty_tracker as dpt;

    struct Dims;
    impl Dimensions for Dims {
        fn total_lines(&self) -> usize {
            24
        }
        fn screen_lines(&self) -> usize {
            24
        }
        fn columns(&self) -> usize {
            80
        }
    }

    #[derive(serde::Deserialize)]
    struct Batch {
        t_ms: u64,
        bytes: String,
        #[allow(dead_code)]
        note: Option<String>,
    }

    /// (relative-ms, observed bytes). `<ESC>` in the committed fixture stands
    /// for the 0x1b control byte (JSON strings can't carry it literally).
    fn load_fixture() -> Vec<(u64, Vec<u8>)> {
        let raw = include_str!("../testdata/pty_parity_stream.json");
        let batches: Vec<Batch> =
            serde_json::from_str(raw).expect("fixture is valid JSON");
        batches
            .into_iter()
            .map(|b| (b.t_ms, b.bytes.replace("<ESC>", "\u{1b}").into_bytes()))
            .collect()
    }

    /// A `Term` configured the way the live TUI session term is
    /// (`tui/src/session.rs`: `kitty_keyboard = true`), so its `mode()` is the
    /// exact source the TUI drainer reads at fire time.
    fn reference_term() -> Term<VoidListener> {
        let mut config = TermConfig::default();
        config.kitty_keyboard = true;
        Term::new(config, &Dims, VoidListener)
    }

    /// A `PendingWrite` whose floor/deadline gates are neutralised so
    /// `pending_write_ready` reduces to the bare quiet predicate (no wakeup
    /// within `window`) — the same thing the daemon's `quiet_for` computes.
    fn neutral_pw(base: Instant, window: Duration) -> PendingWrite {
        PendingWrite {
            text: String::new(),
            submit: false,
            earliest_deliver_at: base,
            require_quiet: window,
            hard_deadline: base + Duration::from_secs(86_400),
        }
    }

    #[test]
    fn daemon_tracker_matches_tui_drainer_decisions() {
        let fixture = load_fixture();
        assert!(!fixture.is_empty(), "fixture must not be empty");
        let base = Instant::now();

        let mut daemon = dpt::PtyModeTracker::new();
        let mut ref_term = reference_term();
        let mut ref_proc = Processor::<StdSyncHandler>::new();
        let mut ref_wakeups: Vec<Instant> = Vec::new();

        // A multi-line body so the bracketed-paste framing branch is exercised
        // at every mode checkpoint.
        let sample_body = "review the diff\n\n- step one\n- step two";
        let idle_window = Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS as u64);

        for i in 0..fixture.len() {
            let (t_ms, bytes) = &fixture[i];
            let at = base + Duration::from_millis(*t_ms);

            // Feed both implementations the same batch, in lockstep.
            daemon.feed(bytes, at);
            ref_proc.advance(&mut ref_term, bytes);
            record_wakeup(&mut ref_wakeups, at);
            // Mirror the production TUI's per-tick prune; decision-invariant for
            // quiet windows <= idle_window, and exercises the shared helper.
            prune_wakeups(&mut ref_wakeups, at, idle_window);

            // --- keyboard-mode decision parity ---
            let dmode = daemon.term_mode();
            let rmode = *ref_term.mode();
            assert_eq!(
                dmode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
                rmode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
                "kitty/DISAMBIGUATE diverged after t={}ms",
                t_ms
            );
            assert_eq!(
                dmode.contains(TermMode::BRACKETED_PASTE),
                rmode.contains(TermMode::BRACKETED_PASTE),
                "bracketed-paste diverged after t={}ms",
                t_ms
            );
            // Derived decisions: Enter encoding + body framing, daemon impl vs
            // TUI impl (distinct functions in the two crates).
            assert_eq!(
                dpt::enter_bytes_for_mode(dmode),
                enter_bytes_for_mode(rmode),
                "Enter encoding diverged after t={}ms",
                t_ms
            );
            assert_eq!(daemon.enter_bytes(), enter_bytes_for_mode(rmode));
            assert_eq!(
                dpt::format_body_for_delivery(sample_body, dmode),
                format_body_for_delivery(sample_body, rmode),
                "body framing diverged after t={}ms",
                t_ms
            );

            // --- quiet-window decision parity over [t_i, next_t) ---
            // Probe only at now >= the latest observed output, the realistic
            // regime delivery runs in (no future wakeups to reason about).
            let end_ms = if i + 1 < fixture.len() {
                fixture[i + 1].0
            } else {
                t_ms + 4000
            };
            let mut probe = *t_ms;
            while probe < end_ms {
                let now = base + Duration::from_millis(probe);
                for win_secs in [1u64, 2] {
                    let window = Duration::from_secs(win_secs);
                    let pw = neutral_pw(base, window);
                    assert_eq!(
                        daemon.quiet_for(window, now),
                        pending_write_ready(&ref_wakeups, &pw, now),
                        "quiet decision diverged: window={}s probe={}ms",
                        win_secs,
                        probe
                    );
                }
                probe += 25;
            }
        }

        // The fixture must drive non-trivial transitions, otherwise the parity
        // above is vacuous: both bits ON mid-stream, both OFF at the end.
        assert!(
            !daemon.term_mode().contains(TermMode::DISAMBIGUATE_ESC_CODES),
            "fixture should leave kitty mode OFF at the end"
        );
        assert!(
            !daemon.term_mode().contains(TermMode::BRACKETED_PASTE),
            "fixture should leave bracketed paste OFF at the end"
        );
    }
}

/// 11g-1: tests for the per-run pending-events buffer that
/// `drain_workflow_watch_events` populates and that 11g-2's
/// controller flip will consume from. File-tail in the
/// controller's tick remains the production source of truth
/// in 11g-1; these tests pin the buffer's behavior in
/// isolation so 11g-2 can flip the consumer with confidence.
#[cfg(test)]
pub(super) mod pending_workflow_events_tests {
    use super::*;

    fn build_app_for_buffer_tests() -> App {
        // Mirror `build_app_with_session`'s home_lock + tempdir
        // dance — App::new reads ~/.cm/ and spawns daemon-watch
        // threads; isolating $HOME prevents cross-test bleed.
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        if let Some(h) = orig_home {
            unsafe {
                std::env::set_var("HOME", h);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }
        // Leak the tempdir so the consumer threads (already
        // spawned with the test HOME in their env) don't error
        // on later ~/.cm accesses.
        std::mem::forget(tmp);
        app
    }

    fn make_event(id: &str, run_id: &str) -> cm_daemon::workflow::events::Event {
        cm_daemon::workflow::events::Event {
            id: id.into(),
            ts: 0.0,
            run_id: run_id.into(),
            role: "worker".into(),
            tool: "workflow_transition".into(),
            args: serde_json::json!({"to": "reviewer", "prompt": ""}),
            source: "daemon".into(),
            from_role: None,
            iteration: 0,
        }
    }

    /// T_g1f — end-to-end: `drain_workflow_watch_events` push
    /// path. Substitute `app.workflow_watch_rx` with a synthetic
    /// channel, send Events through it, call drain, assert the
    /// per-run buffer contains them in the order sent. Pins the
    /// drain Event arm's push behavior; the in-isolation drain
    /// helpers above (T_g1b/T_g1d) cover the take/requeue
    /// halves.
    ///
    /// P-A: an incoming `Event` is now a redraw nudge ONLY — it must NOT
    /// accumulate into `pending_workflow_events`. The local controller that
    /// used to drain that buffer was deleted for criterion #5, so buffering
    /// here would grow unbounded with nothing reading it. Run STATE arrives via
    /// `Snapshot` frames instead (see
    /// `apply_workflow_watch_snapshot_updates_existing_run`).
    #[test]
    fn drain_workflow_watch_events_does_not_buffer_events() {
        let mut app = build_app_for_buffer_tests();
        let (tx, rx) = std::sync::mpsc::channel::<
            crate::workflow_watch::WorkflowWatchEvent,
        >();
        app.workflow_watch_rx = Some(rx);

        let run_id = "wf_g1f";
        for id in ["ev-1", "ev-2", "ev-3"] {
            tx.send(crate::workflow_watch::WorkflowWatchEvent::Event(
                make_event(id, run_id),
            ))
            .expect("send");
        }

        app.drain_workflow_watch_events();

        // The per-run buffer was removed entirely (criterion #5): an
        // incoming `Event` is now a pure redraw nudge with nowhere to
        // accumulate. The remaining observable behavior is that the
        // drain still flips `needs_redraw`.
        assert!(
            app.needs_redraw,
            "drain still sets needs_redraw on each Event",
        );
    }

    /// P-A (criterion #4) — the decisive regression test. A run ALREADY in
    /// `self.workflow_runs` must have its `active_role`, `history`, and terminal
    /// `status` UPDATED from a later snapshot. Before the fix
    /// `apply_workflow_watch_snapshot` refused to overwrite an existing run
    /// (the stale "controller absorbed live diffs" rule), so a daemon-driven
    /// run stayed frozen at its creation snapshot — failing "sidebar + history
    /// render correctly from broadcasts alone." This test FAILS against the old
    /// insert-only body and passes against the overwrite body.
    #[test]
    fn apply_workflow_watch_snapshot_updates_existing_run() {
        use std::collections::BTreeMap;
        let mut app = build_app_for_buffer_tests();

        let run_id = "wf_obs";
        // Creation snapshot: active worker, single seeded history entry, Running.
        let initial = cm_daemon::workflow::run::WorkflowRun::new(
            run_id.to_string(),
            "feedback".into(),
            "ws-1".into(),
            BTreeMap::new(),
            "worker".into(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        );
        let initial_history_len = initial.history.len();
        app.apply_workflow_watch_snapshot(initial.clone());
        assert_eq!(app.workflow_runs.len(), 1, "creation snapshot inserts");
        assert_eq!(app.workflow_runs[0].active_role.as_deref(), Some("worker"));

        // A LATER snapshot: workflow advanced to manager, history grew, Done.
        let mut later = initial;
        later.active_role = Some("manager".into());
        later.status = cm_daemon::workflow::RunStatus::Done;
        // Append a couple of history entries (clone the seeded one to keep the
        // shape valid without hand-building HistoryEntry fields).
        let extra = later.history[0].clone();
        later.history.push(extra.clone());
        later.history.push(extra);

        app.apply_workflow_watch_snapshot(later);

        assert_eq!(app.workflow_runs.len(), 1, "same run_id updates in place, no dup");
        let r = &app.workflow_runs[0];
        assert_eq!(
            r.active_role.as_deref(),
            Some("manager"),
            "active_role must advance from the later snapshot",
        );
        assert!(
            matches!(r.status, cm_daemon::workflow::RunStatus::Done),
            "terminal status must render from the later snapshot",
        );
        assert_eq!(
            r.history.len(),
            initial_history_len + 2,
            "history growth must render from the later snapshot",
        );
    }

    /// P-A end-to-end OBSERVER test (the verification gap the reviewer called
    /// out): an open TUI consuming the broadcast plane via the REAL
    /// `drain_workflow_watch_events` path must see a daemon-driven run advance
    /// worker -> reviewer -> manager to completion — active_role transitions,
    /// history grows, terminal status renders — WITHOUT a reconnect. This is
    /// the TUI half of "render correctly from broadcasts alone"; the daemon
    /// half (the poller pushing a fresh snapshot on each change) is
    /// `broadcast_snapshot_reaches_existing_subscriber_with_full_run` +
    /// `broadcast_changed_snapshots` in cm-daemon. Each stage arrives as a
    /// `Snapshot` frame on the watch channel, exactly as the daemon emits.
    #[test]
    fn observer_renders_full_workflow_advance_from_broadcast_snapshots() {
        use std::collections::BTreeMap;
        let mut app = build_app_for_buffer_tests();
        let (tx, rx) = std::sync::mpsc::channel::<
            crate::workflow_watch::WorkflowWatchEvent,
        >();
        app.workflow_watch_rx = Some(rx);

        let run_id = "wf_adv";
        let base = cm_daemon::workflow::run::WorkflowRun::new(
            run_id.to_string(),
            "feedback".into(),
            "ws-1".into(),
            BTreeMap::new(),
            "worker".into(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        );

        // Helper: emit a snapshot of `base` mutated to a given (role, status,
        // history_len), then drain the channel and return the observed run.
        let send = |app: &mut App,
                    tx: &std::sync::mpsc::Sender<crate::workflow_watch::WorkflowWatchEvent>,
                    role: &str,
                    status: cm_daemon::workflow::RunStatus,
                    hist_len: usize| {
            let mut snap = base.clone();
            snap.active_role = Some(role.to_string());
            snap.status = status;
            while snap.history.len() < hist_len {
                let e = snap.history[0].clone();
                snap.history.push(e);
            }
            tx.send(crate::workflow_watch::WorkflowWatchEvent::Snapshot(snap))
                .expect("send snapshot");
            app.drain_workflow_watch_events();
        };

        use cm_daemon::workflow::RunStatus;

        // Stage 1: creation — worker active, Running.
        send(&mut app, &tx, "worker", RunStatus::Running, 1);
        assert_eq!(app.workflow_runs.len(), 1);
        assert_eq!(app.workflow_runs[0].active_role.as_deref(), Some("worker"));
        assert!(matches!(app.workflow_runs[0].status, RunStatus::Running));

        // Stage 2: worker -> reviewer, history grew.
        send(&mut app, &tx, "reviewer", RunStatus::Running, 2);
        assert_eq!(app.workflow_runs.len(), 1, "no duplicate run");
        assert_eq!(app.workflow_runs[0].active_role.as_deref(), Some("reviewer"));
        assert_eq!(app.workflow_runs[0].history.len(), 2);

        // Stage 3: reviewer -> manager.
        send(&mut app, &tx, "manager", RunStatus::Running, 3);
        assert_eq!(app.workflow_runs[0].active_role.as_deref(), Some("manager"));
        assert_eq!(app.workflow_runs[0].history.len(), 3);

        // Stage 4: completion — terminal status renders.
        send(&mut app, &tx, "manager", RunStatus::Done, 3);
        assert!(
            matches!(app.workflow_runs[0].status, RunStatus::Done),
            "terminal Done must render from the final broadcast",
        );
    }

    fn test_workspace(id: &str, worktree: std::path::PathBuf) -> Workspace {
        Workspace {
            color: None,
            pinned: false,
            id: id.to_string(),
            name: id.to_string(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: Some(worktree),
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: Vec::new(),
            tombstones: Vec::new(),
            is_pushing: false,
        }
    }

    /// Option B (criterion #4): the consumer's bounded-scope + safety guards,
    /// all deterministic (no daemon). `adopt_daemon_workflow_participant` must:
    /// (1) ignore non-workflow entries (no `workflow_run_id`) — those keep their
    /// existing TUI-local / deferred-sync behavior; (2) no-op for an untracked
    /// workspace (R5); (3) fail gracefully (no row, no panic) when the daemon
    /// can't be attached. Together these prove the adopt arm can't duplicate
    /// locally-created rows, can't crash the observer, and stays scoped.
    #[test]
    fn adopt_daemon_workflow_participant_guards() {
        let mut app = build_app_for_buffer_tests();
        let tmp = tempfile::tempdir().unwrap();
        app.workspaces.push(test_workspace("ws-g", tmp.path().to_path_buf()));

        // (1) Non-workflow entry → no adoption.
        app.adopt_daemon_workflow_participant(
            "ts-a",
            &serde_json::json!({"workspace_id": "ws-g", "session_type": "claude-code"}),
        );
        assert!(app.workspaces[0].sessions.is_empty(), "non-workflow entry must not adopt");

        // (2) Workflow entry, UNTRACKED workspace → R5 no-op.
        app.adopt_daemon_workflow_participant(
            "ts-b",
            &serde_json::json!({
                "workspace_id": "ws-untracked", "workflow_run_id": "wf",
                "workflow_role": "worker", "session_type": "claude-code"
            }),
        );
        assert!(app.workspaces[0].sessions.is_empty(), "untracked workspace must no-op (R5)");

        // (3) Workflow entry, tracked workspace, but no daemon to attach →
        //     graceful (no row, no panic).
        app.adopt_daemon_workflow_participant(
            "ts-c",
            &serde_json::json!({
                "workspace_id": "ws-g", "workflow_run_id": "wf",
                "workflow_role": "worker", "session_type": "claude-code"
            }),
        );
        assert!(
            app.workspaces[0].sessions.is_empty(),
            "attach failure (no daemon) must be graceful — no row",
        );
    }

    /// Regression (existing-session bind): when a session that is ALREADY a TUI
    /// row becomes a workflow participant (the bound worker keeps its
    /// pre-existing row), an `Updated` broadcast must STAMP the workflow tags
    /// onto that row in place — not no-op — so it re-groups under the workflow
    /// header. Before the fix the idempotent early-return dropped the tags and
    /// the bound worker rendered OUTSIDE its own workflow group.
    #[test]
    fn adopt_stamps_tags_on_existing_bound_row_in_place() {
        let mut app = build_app_for_buffer_tests();
        let tmp = tempfile::tempdir().unwrap();
        app.workspaces.push(test_workspace("ws-b", tmp.path().to_path_buf()));

        // A pre-existing, UNTAGGED row — a session the user was working in, about
        // to be bound as the worker.
        let session = crate::session::Session::new(
            "/bin/true", &[], 80, 24, None, HashMap::new(), None,
        )
        .expect("session for test");
        let row = make_simple_session_with_uid(
            "ts-bound".to_string(), "worker: live", "claude", session, None,
        );
        assert!(row.workflow_run_id.is_none(), "precondition: row starts untagged");
        app.workspaces.last_mut().unwrap().sessions.push(row);

        // The daemon's bind broadcast: an Updated entry tagging the existing row.
        app.adopt_daemon_workflow_participant(
            "ts-bound",
            &serde_json::json!({
                "uid": "ts-bound",
                "workspace_id": "ws-b",
                "session_type": "claude-code",
                "workflow_run_id": "wf_bind",
                "workflow_role": "worker",
            }),
        );

        let ws = app.workspaces.iter().find(|w| w.id == "ws-b").unwrap();
        assert_eq!(ws.sessions.len(), 1, "no duplicate row — tags applied in place");
        assert_eq!(ws.sessions[0].workflow_run_id.as_deref(), Some("wf_bind"), "run tag stamped in place");
        assert_eq!(ws.sessions[0].workflow_role.as_deref(), Some("worker"), "role tag stamped in place");
    }

    /// Option B (criterion #4) — the end-to-end proof the reviewer asked for: a
    /// daemon-launched workflow PARTICIPANT appears as a selectable, attachable
    /// session row under its workflow header purely from the manifest broadcast
    /// stream (no manual refresh). Drives a REAL in-process daemon: spawn a
    /// participant session, then feed the consumer the `Added` entry (the exact
    /// shape `start_session` broadcasts) and assert a tagged row materializes.
    /// Idempotent: a second identical `Added` does not duplicate.
    #[test]
    fn adopt_daemon_workflow_participant_renders_row_from_broadcast_e2e() {
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicBool, Ordering};

        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let sock = home.join(".cm/daemon.sock");

        // In-process daemon on the App's default local socket path.
        let mut state_inner = cm_daemon::state::DaemonState::new();
        state_inner.attach_addr = sock.to_string_lossy().into_owned();
        state_inner.workspaces.insert(
            "ws-e2e".into(),
            cm_daemon::manifest::ManifestWorkspace {
                color: None,
                pinned: false,
                id: "ws-e2e".into(),
                name: "e2e".into(),
                is_closed: false,
                is_cloud: false,
                worktree_path: Some(wt.clone()),
                main_repo_path: None,
                repo_url: None,
                worker_vm: None,
                worker_zone: None,
                host_id: cm_daemon::host_id::HostId::local(),
                sessions: Vec::new(),
                tombstones: Vec::new(),
            },
        );
        let state = std::sync::Arc::new(std::sync::Mutex::new(state_inner));
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));

        let dstate = state.clone();
        let dstop = stop.clone();
        let dhandle = std::thread::spawn(move || {
            while !dstop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let st = dstate.clone();
                        std::thread::spawn(move || {
                            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
                            let req = match cm_daemon::control::wire::read_request(&mut stream) {
                                Ok(Some(r)) => r,
                                _ => return,
                            };
                            use cm_daemon::control::dispatch::DispatchOutcome::*;
                            match cm_daemon::control::dispatch::dispatch_request(&st, &req) {
                                Done(resp) => {
                                    let _ = cm_daemon::control::wire::write_response(&mut stream, &resp);
                                }
                                AttachStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_attach_stream(&mut stream, st, handle);
                                    }
                                }
                                ManifestWatchStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_manifest_watch_stream(&mut stream, handle);
                                    }
                                }
                                EventsSubscribeStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_events_subscribe_stream(&mut stream, handle);
                                    }
                                }
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        // Build the App so its default host_pool dials `sock`. `CM_DAEMON_SOCKET`
        // is read first by `default_socket_path` (ahead of HOME), so point it at
        // the test socket; restore both afterward.
        let orig_home = std::env::var_os("HOME");
        let orig_sock = std::env::var_os("CM_DAEMON_SOCKET");
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("CM_DAEMON_SOCKET", &sock);
        }
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        app.workspaces.push(test_workspace("ws-e2e", wt.clone()));

        // Spawn a real participant session on the daemon (creates the uid the
        // consumer will attach to).
        let uid = new_session_uid();
        let argv = vec!["/bin/bash".to_string()];
        let cfg = crate::client_session::ClientSessionConfig {
            daemon_socket: &sock,
            operator_token_id: crate::daemon_launch::operator_token(),
            uid: &uid,
            workspace_id: "ws-e2e",
            label: "reviewer",
            session_type: "claude-code",
            argv: &argv,
            working_dir: &wt,
            env: std::collections::BTreeMap::new(),
            cols: 80,
            rows: 24,
            memory_cap_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            cgroup_path: None,
            worktree_path: Some(&wt),
            task_id: None,
            transcript_path: None,
            workflow_run_id: Some("wf_e2e"),
            workflow_role: Some("reviewer"),
        };
        crate::client_session::rpc_start_session_full(&cfg).expect("daemon spawns participant");

        // Feed the consumer the Added entry exactly as start_session broadcasts.
        let entry = serde_json::json!({
            "uid": uid,
            "workspace_id": "ws-e2e",
            "label": "reviewer",
            "session_type": "claude-code",
            "workflow_run_id": "wf_e2e",
            "workflow_role": "reviewer",
            "task_id": serde_json::Value::Null,
        });
        app.adopt_daemon_workflow_participant(&uid, &entry);
        // Idempotent: a duplicate Added must not add a second row.
        app.adopt_daemon_workflow_participant(&uid, &entry);

        let ws = app.workspaces.iter().find(|w| w.id == "ws-e2e").unwrap();
        let rows: Vec<&TerminalSession> =
            ws.sessions.iter().filter(|s| s.uid == uid).collect();
        assert_eq!(rows.len(), 1, "exactly one participant row from the broadcast (idempotent)");
        let row = rows[0];
        assert_eq!(row.workflow_run_id.as_deref(), Some("wf_e2e"), "row tagged with its run");
        assert_eq!(row.workflow_role.as_deref(), Some("reviewer"), "row tagged with its role");
        // Must-fix #1: the wire type "claude-code" must be normalized to the
        // INTERNAL "claude" on the row, or every `"claude"` match (transcript
        // handling, type-specific rendering) misses it.
        assert_eq!(
            row.session_type, "claude",
            "adopted claude-code participant must store internal type 'claude'",
        );

        // Cleanup.
        let _ = crate::client_session::rpc_kill_session(&sock, crate::daemon_launch::operator_token(), &uid);
        stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&sock);
        let _ = dhandle.join();
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match orig_sock {
            Some(s) => unsafe { std::env::set_var("CM_DAEMON_SOCKET", s) },
            None => unsafe { std::env::remove_var("CM_DAEMON_SOCKET") },
        }
    }

    // === Phase 3 (remote-session-execution): remote A-n / A-s ===

    fn status_text(app: &App) -> String {
        app.status_msg
            .as_ref()
            .map(|(m, _)| m.clone())
            .unwrap_or_default()
    }

    /// Spin a best-effort in-process cm-daemon on `sock` for the remote-host
    /// TUI tests (real `dispatch_request`). Returns `(stop, handle)`; set the
    /// flag + poke the socket to stop it.
    ///
    /// `pub(super)` so sibling test modules (e.g. `remote_reconnect_tests`)
    /// can drive a real daemon that returns a genuine `NotFound` on attach.
    pub(in crate::app) fn spawn_inproc_daemon(
        sock: std::path::PathBuf,
        state: std::sync::Arc<std::sync::Mutex<cm_daemon::state::DaemonState>>,
    ) -> (
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let dstop = stop.clone();
        let handle = std::thread::spawn(move || {
            while !dstop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let st = state.clone();
                        std::thread::spawn(move || {
                            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
                            let req = match cm_daemon::control::wire::read_request(&mut stream) {
                                Ok(Some(r)) => r,
                                _ => return,
                            };
                            use cm_daemon::control::dispatch::DispatchOutcome::*;
                            match cm_daemon::control::dispatch::dispatch_request(&st, &req) {
                                Done(resp) => {
                                    let _ = cm_daemon::control::wire::write_response(&mut stream, &resp);
                                }
                                AttachStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_attach_stream(&mut stream, st, handle);
                                    }
                                }
                                ManifestWatchStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_manifest_watch_stream(&mut stream, handle);
                                    }
                                }
                                EventsSubscribeStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_events_subscribe_stream(&mut stream, handle);
                                    }
                                }
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        (stop, handle)
    }

    /// Spin a minimal RECORDING control-socket listener on `sock`: it captures
    /// each request's `{method, params}` and replies OK `{run_id:"wf_test"}`,
    /// WITHOUT running the real dispatcher (so a `start_workflow` routing test
    /// never spawns participants). Returns `(captured, stop, handle)`.
    #[allow(clippy::type_complexity)]
    fn spawn_recording_listener(
        sock: std::path::PathBuf,
    ) -> (
        std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let dstop = stop.clone();
        let cap = captured.clone();
        let handle = std::thread::spawn(move || {
            while !dstop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let cap = cap.clone();
                        std::thread::spawn(move || {
                            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
                            if let Ok(Some(req)) =
                                cm_daemon::control::wire::read_request(&mut stream)
                            {
                                cap.lock().unwrap_or_else(|p| p.into_inner()).push(
                                    serde_json::json!({
                                        "method": req.method,
                                        "params": req.params,
                                    }),
                                );
                                let resp = cm_daemon::control::protocol::Response::ok(
                                    req.id.clone(),
                                    serde_json::json!({ "run_id": "wf_test" }),
                                );
                                let _ = cm_daemon::control::wire::write_response(&mut stream, &resp);
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        (captured, stop, handle)
    }

    /// Criterion: a remote A-n with `in_place` is rejected with a status
    /// message and issues NO RPC (the rejection precedes any host_pool /
    /// daemon access), and creates no workspace.
    #[test]
    fn remote_a_n_rejects_in_place_no_rpc() {
        let mut app = build_app_for_buffer_tests();
        // Routing keys off the CHOSEN host param, not the global active_host
        // (which stays local here) — proving the host-picker choice drives it.
        let chosen = cm_daemon::host_id::HostId::new("manager");
        let before = app.workspaces.len();
        app.create_local_session(&chosen, "somerepo", "label", None, 0, None, true);
        assert!(
            status_text(&app).contains("in-place"),
            "remote in_place must be rejected with a clear message; got {:?}",
            status_text(&app),
        );
        assert_eq!(app.workspaces.len(), before, "no workspace created on rejection");
    }

    /// Criterion: a remote A-n with `seed_from` is rejected, no RPC, no
    /// workspace.
    #[test]
    fn remote_a_n_rejects_seed_from_no_rpc() {
        let mut app = build_app_for_buffer_tests();
        let chosen = cm_daemon::host_id::HostId::new("manager");
        let before = app.workspaces.len();
        app.create_local_session(&chosen, "somerepo", "label", None, 0, Some("snap-1"), false);
        assert!(
            status_text(&app).contains("snapshot"),
            "remote seed_from must be rejected with a clear message; got {:?}",
            status_text(&app),
        );
        assert_eq!(app.workspaces.len(), before, "no workspace created on rejection");
    }

    /// Criterion: the SAME options on a LOCAL host are NOT rejected — local
    /// A-n with `in_place` runs the existing local path (here it stops at
    /// "Repo not found locally" because the test has no repo, proving it got
    /// past the remote-rejection branch into the local path).
    #[test]
    fn local_a_n_in_place_not_rejected() {
        let mut app = build_app_for_buffer_tests();
        let chosen = cm_daemon::host_id::HostId::local();
        app.create_local_session(&chosen, "no-such-repo-xyz", "label", None, 0, None, true);
        let st = status_text(&app);
        assert!(
            !st.contains("in-place") && !st.contains("remote host"),
            "local in_place must NOT hit the remote rejection; got {:?}",
            st,
        );
        assert_eq!(
            st, "Repo not found locally",
            "local A-n runs the existing local path (stops at repo lookup)",
        );
    }

    /// Host-picker A-n: the chosen host on the form (carried via
    /// `SubmitAction::CreateLocalSession.host_id`) drives the create path.
    /// A chosen REMOTE host routes to the remote path (proven by the in-place
    /// rejection that path issues); a chosen LOCAL host routes to the existing
    /// local path. Pins that the per-form host choice — not any global default
    /// — is what selects the create path.
    #[test]
    fn a_n_submit_routes_by_chosen_host() {
        // Remote choice → remote create path (rejects in-place, no workspace).
        let mut app = build_app_for_buffer_tests();
        let before = app.workspaces.len();
        app.apply_submit_action(SubmitAction::CreateLocalSession {
            repo_url: "somerepo".into(),
            label: "label".into(),
            branch: None,
            idle_timeout_secs: 0,
            seed_from: None,
            in_place: true,
            host_id: cm_daemon::host_id::HostId::new("manager"),
        });
        assert!(
            status_text(&app).contains("in-place"),
            "remote chosen host must route to the remote path (in-place \
             rejection); got {:?}",
            status_text(&app),
        );
        assert_eq!(app.workspaces.len(), before, "no workspace on remote rejection");

        // Local choice → existing local path (stops at the repo lookup).
        let mut app = build_app_for_buffer_tests();
        app.apply_submit_action(SubmitAction::CreateLocalSession {
            repo_url: "no-such-repo-xyz".into(),
            label: "label".into(),
            branch: None,
            idle_timeout_secs: 0,
            seed_from: None,
            in_place: true,
            host_id: cm_daemon::host_id::HostId::local(),
        });
        assert_eq!(
            status_text(&app),
            "Repo not found locally",
            "local chosen host runs the local create path",
        );
    }

    /// Criterion: a remote A-s with `seed_from` is rejected, no RPC, no new
    /// session. Tested directly on `add_remote_session` (the rejection
    /// precedes any host_pool / daemon access).
    #[test]
    fn remote_a_s_rejects_seed_from_no_rpc() {
        let mut app = build_app_for_buffer_tests();
        let tmp = tempfile::tempdir().unwrap();
        app.workspaces.push(test_workspace("ws-rs", tmp.path().to_path_buf()));
        let before = app.workspaces[0].sessions.len();
        app.add_remote_session(
            &cm_daemon::host_id::HostId::new("manager"),
            0,
            "claude",
            None,
            Some("snap-2"),
        );
        assert!(
            status_text(&app).contains("snapshot"),
            "remote A-s seed_from must be rejected; got {:?}",
            status_text(&app),
        );
        assert_eq!(
            app.workspaces[0].sessions.len(),
            before,
            "no session added on rejection",
        );
    }

    /// Criterion: a diff carrying a REMOTE source host adopts a sidebar row
    /// with `ts.host_id = remote`. End-to-end via an in-process daemon at a
    /// "manager" unix socket + a 2-host pool; the non-workflow Added diff
    /// (the remote A-s/other-client case) is adopted and tagged with the
    /// producing host.
    #[test]
    fn remote_diff_adopts_row_with_remote_host_id() {
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cm")).unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let local_sock = home.join(".cm/daemon.sock");
        let mgr_sock = home.join(".cm/manager.sock");

        // In-process daemon at the MANAGER socket with a known workspace.
        let mut state_inner = cm_daemon::state::DaemonState::new();
        state_inner.attach_addr = mgr_sock.to_string_lossy().into_owned();
        state_inner.workspaces.insert(
            "ws-mgr".into(),
            cm_daemon::manifest::ManifestWorkspace {
                color: None,
                pinned: false,
                id: "ws-mgr".into(),
                name: "mgr".into(),
                is_closed: false,
                is_cloud: false,
                worktree_path: Some(wt.clone()),
                main_repo_path: None,
                repo_url: None,
                worker_vm: None,
                worker_zone: None,
                host_id: cm_daemon::host_id::HostId::local(),
                sessions: Vec::new(),
                tombstones: Vec::new(),
            },
        );
        let state = Arc::new(std::sync::Mutex::new(state_inner));
        let listener = UnixListener::bind(&mgr_sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let dstate = state.clone();
        let dstop = stop.clone();
        let dhandle = std::thread::spawn(move || {
            while !dstop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let st = dstate.clone();
                        std::thread::spawn(move || {
                            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
                            let req = match cm_daemon::control::wire::read_request(&mut stream) {
                                Ok(Some(r)) => r,
                                _ => return,
                            };
                            use cm_daemon::control::dispatch::DispatchOutcome::*;
                            match cm_daemon::control::dispatch::dispatch_request(&st, &req) {
                                Done(resp) => {
                                    let _ = cm_daemon::control::wire::write_response(&mut stream, &resp);
                                }
                                AttachStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_attach_stream(&mut stream, st, handle);
                                    }
                                }
                                ManifestWatchStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_manifest_watch_stream(&mut stream, handle);
                                    }
                                }
                                EventsSubscribeStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_events_subscribe_stream(&mut stream, handle);
                                    }
                                }
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

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
        // Replace the default (local-only) pool with one that resolves the
        // "manager" host to the in-process daemon's unix socket.
        let hosts = crate::hosts::HostsConfig {
            hosts: vec![
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::local(),
                    transport: crate::hosts::HostTransport::Unix { socket: local_sock.clone() },
                    default: true,
                },
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::new("manager"),
                    transport: crate::hosts::HostTransport::Unix { socket: mgr_sock.clone() },
                    default: false,
                },
            ],
        };
        app.host_pool =
            std::sync::Arc::new(crate::host_pool::HostPool::from_config(&hosts).expect("pool"));
        app.workspaces.push(test_workspace("ws-mgr", wt.clone()));

        // Spawn a NON-workflow session on the manager daemon (the uid the
        // adopt path attaches to).
        let uid = new_session_uid();
        let argv = vec!["/bin/bash".to_string()];
        let cfg = crate::client_session::ClientSessionConfig {
            daemon_socket: &mgr_sock,
            operator_token_id: crate::daemon_launch::operator_token(),
            uid: &uid,
            workspace_id: "ws-mgr",
            label: "claude-code",
            session_type: "claude-code",
            argv: &argv,
            working_dir: &wt,
            env: std::collections::BTreeMap::new(),
            cols: 80,
            rows: 24,
            memory_cap_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            cgroup_path: None,
            worktree_path: Some(&wt),
            task_id: None,
            transcript_path: None,
            workflow_run_id: None,
            workflow_role: None,
        };
        crate::client_session::rpc_start_session_full(&cfg)
            .expect("manager daemon spawns session");

        // A NON-workflow Added diff from the MANAGER host → adopt with
        // host_id = manager.
        let entry = serde_json::json!({
            "uid": uid,
            "workspace_id": "ws-mgr",
            "label": "claude-code",
            "session_type": "claude-code",
            "workflow_run_id": serde_json::Value::Null,
            "workflow_role": serde_json::Value::Null,
            "task_id": serde_json::Value::Null,
        });
        app.apply_manifest_diff_from_host(
            cm_daemon::host_id::HostId::new("manager"),
            cm_daemon::manifest::ManifestDiff::Added {
                uid: uid.clone(),
                entry,
            },
        );

        let ws = app.workspaces.iter().find(|w| w.id == "ws-mgr").unwrap();
        let row = ws
            .sessions
            .iter()
            .find(|s| s.uid == uid)
            .expect("a remote-host diff must adopt a sidebar row");
        assert_eq!(
            row.host_id,
            cm_daemon::host_id::HostId::new("manager"),
            "adopted row must be tagged with the PRODUCING remote host",
        );
        // Wire type normalized to internal for the rest of the TUI.
        assert_eq!(row.session_type, "claude");

        // Cleanup.
        let _ = crate::client_session::rpc_kill_session(
            &mgr_sock,
            crate::daemon_launch::operator_token(),
            &uid,
        );
        stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&mgr_sock);
        let _ = dhandle.join();
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Phase 4 (remote-session-execution): `restore_sessions` reattaches a
    /// session with a REMOTE `host_id`, routing the attach RPCs through that
    /// host's socket, ungated. End-to-end via an in-process daemon at a
    /// "manager" unix socket + a 2-host pool: a live session is spawned on
    /// the manager daemon, a manifest pins it to host "manager", and restore
    /// reattaches it (no skip/preserve) tagged with host_id = manager.
    ///
    /// Local reattach is unchanged — the local path
    /// (`spawn_restored_session`) is untouched and covered by the existing
    /// restore suite + the `restore_sessions_reattaches_remote_else_preserves`
    /// pin. The cm-manager live e2e (run a command, resize, Enter, tunnel
    /// respawn) is a deferred MANUAL operator pass (no live TUI + VM here).
    #[test]
    fn remote_reattach_routes_through_host_socket() {
        use cm_daemon::manifest::{Manifest, ManifestEntry, ManifestWorkspace};
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let local_sock = cm_dir.join("daemon.sock");
        let mgr_sock = cm_dir.join("manager.sock");

        // In-process daemon at the MANAGER socket with workspace ws-r.
        let mut state_inner = cm_daemon::state::DaemonState::new();
        state_inner.attach_addr = mgr_sock.to_string_lossy().into_owned();
        state_inner.workspaces.insert(
            "ws-r".into(),
            cm_daemon::manifest::ManifestWorkspace {
                color: None,
                pinned: false,
                id: "ws-r".into(),
                name: "r".into(),
                is_closed: false,
                is_cloud: false,
                worktree_path: Some(wt.clone()),
                main_repo_path: None,
                repo_url: None,
                worker_vm: None,
                worker_zone: None,
                host_id: cm_daemon::host_id::HostId::local(),
                sessions: Vec::new(),
                tombstones: Vec::new(),
            },
        );
        let state = Arc::new(std::sync::Mutex::new(state_inner));
        let listener = UnixListener::bind(&mgr_sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let dstate = state.clone();
        let dstop = stop.clone();
        let dhandle = std::thread::spawn(move || {
            while !dstop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let st = dstate.clone();
                        std::thread::spawn(move || {
                            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
                            let req = match cm_daemon::control::wire::read_request(&mut stream) {
                                Ok(Some(r)) => r,
                                _ => return,
                            };
                            use cm_daemon::control::dispatch::DispatchOutcome::*;
                            match cm_daemon::control::dispatch::dispatch_request(&st, &req) {
                                Done(resp) => {
                                    let _ = cm_daemon::control::wire::write_response(&mut stream, &resp);
                                }
                                AttachStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_attach_stream(&mut stream, st, handle);
                                    }
                                }
                                ManifestWatchStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_manifest_watch_stream(&mut stream, handle);
                                    }
                                }
                                EventsSubscribeStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_events_subscribe_stream(&mut stream, handle);
                                    }
                                }
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        // Spawn the live session on the manager daemon (the uid restore
        // will reattach to).
        let uid = new_session_uid();
        let argv = vec!["/bin/bash".to_string()];
        let cfg = crate::client_session::ClientSessionConfig {
            daemon_socket: &mgr_sock,
            operator_token_id: crate::daemon_launch::operator_token(),
            uid: &uid,
            workspace_id: "ws-r",
            label: "claude-code",
            session_type: "claude-code",
            argv: &argv,
            working_dir: &wt,
            env: std::collections::BTreeMap::new(),
            cols: 80,
            rows: 24,
            memory_cap_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            cgroup_path: None,
            worktree_path: Some(&wt),
            task_id: None,
            transcript_path: None,
            workflow_run_id: None,
            workflow_role: None,
        };
        crate::client_session::rpc_start_session_full(&cfg)
            .expect("manager daemon spawns session");

        // Manifest on disk: workspace ws-r with one session pinned to host
        // "manager".
        let entry = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: uid.clone(),
            managed_by_uid: None,
            generation: 0,
            label: "claude".into(),
            session_type: "claude".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            seeded_from_snapshot: None,
            last_exit: None,
            host_id: cm_daemon::host_id::HostId::new("manager"),
        };
        let mw = ManifestWorkspace {
            color: None,
            pinned: false,
            id: "ws-r".into(),
            name: "r".into(),
            is_closed: false,
            is_cloud: false,
            worktree_path: Some(wt.clone()),
            main_repo_path: None,
            repo_url: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![entry],
            tombstones: Vec::new(),
        };
        let mut workspaces = HashMap::new();
        workspaces.insert(mw.id.clone(), mw);
        let manifest = Manifest {
            task_colors: Default::default(),
            workspaces,
            bindings: HashMap::new(),
            view: None,
            hide_continuous: false,
            continuous_column_on: false,
        };
        std::fs::write(
            cm_dir.join("tui-sessions.json"),
            serde_json::to_string(&manifest).expect("ser"),
        )
        .expect("write manifest");

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
        // Inject a 2-host pool so "manager" resolves to the in-process daemon.
        let hosts = crate::hosts::HostsConfig {
            hosts: vec![
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::local(),
                    transport: crate::hosts::HostTransport::Unix { socket: local_sock.clone() },
                    default: true,
                },
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::new("manager"),
                    transport: crate::hosts::HostTransport::Unix { socket: mgr_sock.clone() },
                    default: false,
                },
            ],
        };
        app.host_pool =
            std::sync::Arc::new(crate::host_pool::HostPool::from_config(&hosts).expect("pool"));
        app.sessions_restored = true;
        app.restore_sessions();

        // The remote session reattached — ungated — tagged with host manager.
        let ws = app
            .workspaces
            .iter()
            .find(|w| w.id == "ws-r")
            .expect("workspace restored");
        let row = ws
            .sessions
            .iter()
            .find(|s| s.uid == uid)
            .expect("remote session must reattach over its host's socket");
        assert_eq!(
            row.host_id,
            cm_daemon::host_id::HostId::new("manager"),
            "reattached row stays pinned to its remote host",
        );
        // It reattached (not skipped/preserved).
        assert!(
            app.skipped_manifest_entries
                .get("ws-r")
                .map_or(true, |v| v.is_empty()),
            "a reattachable remote session must NOT be skipped",
        );

        // Cleanup.
        let _ = crate::client_session::rpc_kill_session(
            &mgr_sock,
            crate::daemon_launch::operator_token(),
            &uid,
        );
        stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&mgr_sock);
        let _ = dhandle.join();
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Phase 4 (remote-session-execution): a remote entry whose
    /// `workflow_run_id` points to a non-active (Detached/Done) run
    /// reattaches with its workflow tags CLEARED — stale workflow context is
    /// NOT propagated to the remote daemon. Parity with the local restore
    /// path's `untag_stale_workflow`.
    #[test]
    fn remote_reattach_clears_stale_workflow_tags() {
        use cm_daemon::manifest::{Manifest, ManifestEntry, ManifestWorkspace};
        use std::sync::atomic::Ordering;

        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let local_sock = cm_dir.join("daemon.sock");
        let mgr_sock = cm_dir.join("manager.sock");

        let mut state_inner = cm_daemon::state::DaemonState::new();
        state_inner.attach_addr = mgr_sock.to_string_lossy().into_owned();
        state_inner.workspaces.insert(
            "ws-r".into(),
            ManifestWorkspace {
                color: None,
                pinned: false,
                id: "ws-r".into(),
                name: "r".into(),
                is_closed: false,
                is_cloud: false,
                worktree_path: Some(wt.clone()),
                main_repo_path: None,
                repo_url: None,
                worker_vm: None,
                worker_zone: None,
                host_id: cm_daemon::host_id::HostId::local(),
                sessions: Vec::new(),
                tombstones: Vec::new(),
            },
        );
        let state = std::sync::Arc::new(std::sync::Mutex::new(state_inner));
        let (stop, dhandle) = spawn_inproc_daemon(mgr_sock.clone(), state.clone());

        // Live session on the manager daemon (spawned WITHOUT workflow ctx).
        let uid = new_session_uid();
        let argv = vec!["/bin/bash".to_string()];
        let cfg = crate::client_session::ClientSessionConfig {
            daemon_socket: &mgr_sock,
            operator_token_id: crate::daemon_launch::operator_token(),
            uid: &uid,
            workspace_id: "ws-r",
            label: "claude-code",
            session_type: "claude-code",
            argv: &argv,
            working_dir: &wt,
            env: std::collections::BTreeMap::new(),
            cols: 80,
            rows: 24,
            memory_cap_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            cgroup_path: None,
            worktree_path: Some(&wt),
            task_id: None,
            transcript_path: None,
            workflow_run_id: None,
            workflow_role: None,
        };
        crate::client_session::rpc_start_session_full(&cfg).expect("spawn");

        // Manifest entry pinned to "manager" with a STALE workflow_run_id
        // (no active runs exist → it's Detached/Done from the TUI's POV).
        let entry = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: uid.clone(),
            managed_by_uid: None,
            generation: 0,
            label: "claude".into(),
            session_type: "claude".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: Some("wf_dead".into()),
            workflow_role: Some("worker".into()),
            continuous_task_id: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            seeded_from_snapshot: None,
            last_exit: None,
            host_id: cm_daemon::host_id::HostId::new("manager"),
        };
        let mw = ManifestWorkspace {
            color: None,
            pinned: false,
            id: "ws-r".into(),
            name: "r".into(),
            is_closed: false,
            is_cloud: false,
            worktree_path: Some(wt.clone()),
            main_repo_path: None,
            repo_url: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![entry],
            tombstones: Vec::new(),
        };
        let mut workspaces = HashMap::new();
        workspaces.insert("ws-r".to_string(), mw);
        let manifest = Manifest {
            task_colors: Default::default(),
            workspaces,
            bindings: HashMap::new(),
            view: None,
            hide_continuous: false,
            continuous_column_on: false,
        };
        std::fs::write(
            cm_dir.join("tui-sessions.json"),
            serde_json::to_string(&manifest).expect("ser"),
        )
        .expect("write manifest");

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
        let hosts = crate::hosts::HostsConfig {
            hosts: vec![
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::local(),
                    transport: crate::hosts::HostTransport::Unix { socket: local_sock.clone() },
                    default: true,
                },
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::new("manager"),
                    transport: crate::hosts::HostTransport::Unix { socket: mgr_sock.clone() },
                    default: false,
                },
            ],
        };
        app.host_pool =
            std::sync::Arc::new(crate::host_pool::HostPool::from_config(&hosts).expect("pool"));
        app.sessions_restored = true;
        // No active workflow runs → "wf_dead" is stale and must be cleaned.
        assert!(app.workflow_runs.is_empty(), "precondition: no active runs");
        app.restore_sessions();

        let ws = app
            .workspaces
            .iter()
            .find(|w| w.id == "ws-r")
            .expect("workspace restored");
        let row = ws
            .sessions
            .iter()
            .find(|s| s.uid == uid)
            .expect("remote session must reattach");
        assert_eq!(row.host_id, cm_daemon::host_id::HostId::new("manager"));
        assert!(
            row.workflow_run_id.is_none(),
            "stale workflow_run_id must be CLEARED on remote reattach, not propagated",
        );
        assert!(
            row.workflow_role.is_none(),
            "stale workflow_role must be CLEARED on remote reattach",
        );

        // Cleanup.
        let _ = crate::client_session::rpc_kill_session(
            &mgr_sock,
            crate::daemon_launch::operator_token(),
            &uid,
        );
        stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&mgr_sock);
        let _ = dhandle.join();
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Phase 4 anti-poisoning (the property that matters most): two remote
    /// entries on the SAME REACHABLE host where the FIRST session is gone
    /// (reattach `None`) but the SECOND is live → the second STILL reattaches.
    /// A session-gone failure must NOT mark the host unreachable, or it would
    /// poison sibling live sessions.
    #[test]
    fn remote_reattach_session_gone_does_not_poison_live_sibling() {
        use cm_daemon::manifest::{Manifest, ManifestEntry, ManifestWorkspace};
        use std::sync::atomic::Ordering;

        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let local_sock = cm_dir.join("daemon.sock");
        let mgr_sock = cm_dir.join("manager.sock");

        let mut state_inner = cm_daemon::state::DaemonState::new();
        state_inner.attach_addr = mgr_sock.to_string_lossy().into_owned();
        state_inner.workspaces.insert(
            "ws-r".into(),
            ManifestWorkspace {
                color: None,
                pinned: false,
                id: "ws-r".into(),
                name: "r".into(),
                is_closed: false,
                is_cloud: false,
                worktree_path: Some(wt.clone()),
                main_repo_path: None,
                repo_url: None,
                worker_vm: None,
                worker_zone: None,
                host_id: cm_daemon::host_id::HostId::local(),
                sessions: Vec::new(),
                tombstones: Vec::new(),
            },
        );
        let state = std::sync::Arc::new(std::sync::Mutex::new(state_inner));
        let (stop, dhandle) = spawn_inproc_daemon(mgr_sock.clone(), state.clone());

        // Spawn ONLY the live session on the manager daemon.
        let uid_live = new_session_uid();
        let uid_gone = new_session_uid(); // never spawned → attach will fail
        let argv = vec!["/bin/bash".to_string()];
        let cfg = crate::client_session::ClientSessionConfig {
            daemon_socket: &mgr_sock,
            operator_token_id: crate::daemon_launch::operator_token(),
            uid: &uid_live,
            workspace_id: "ws-r",
            label: "claude-code",
            session_type: "claude-code",
            argv: &argv,
            working_dir: &wt,
            env: std::collections::BTreeMap::new(),
            cols: 80,
            rows: 24,
            memory_cap_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            cgroup_path: None,
            worktree_path: Some(&wt),
            task_id: None,
            transcript_path: None,
            workflow_run_id: None,
            workflow_role: None,
        };
        crate::client_session::rpc_start_session_full(&cfg).expect("spawn live");

        // Manifest: [gone, live] — gone FIRST so its failure precedes live.
        let mk = |uid: &str| ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: uid.into(),
            managed_by_uid: None,
            generation: 0,
            label: "claude".into(),
            session_type: "claude".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            seeded_from_snapshot: None,
            last_exit: None,
            host_id: cm_daemon::host_id::HostId::new("manager"),
        };
        let mw = ManifestWorkspace {
            color: None,
            pinned: false,
            id: "ws-r".into(),
            name: "r".into(),
            is_closed: false,
            is_cloud: false,
            worktree_path: Some(wt.clone()),
            main_repo_path: None,
            repo_url: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![mk(&uid_gone), mk(&uid_live)],
            tombstones: Vec::new(),
        };
        let mut workspaces = HashMap::new();
        workspaces.insert("ws-r".to_string(), mw);
        let manifest = Manifest {
            task_colors: Default::default(),
            workspaces,
            bindings: HashMap::new(),
            view: None,
            hide_continuous: false,
            continuous_column_on: false,
        };
        std::fs::write(
            cm_dir.join("tui-sessions.json"),
            serde_json::to_string(&manifest).expect("ser"),
        )
        .expect("write manifest");

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
        let hosts = crate::hosts::HostsConfig {
            hosts: vec![
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::local(),
                    transport: crate::hosts::HostTransport::Unix { socket: local_sock.clone() },
                    default: true,
                },
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::new("manager"),
                    transport: crate::hosts::HostTransport::Unix { socket: mgr_sock.clone() },
                    default: false,
                },
            ],
        };
        app.host_pool =
            std::sync::Arc::new(crate::host_pool::HostPool::from_config(&hosts).expect("pool"));
        app.sessions_restored = true;
        app.restore_sessions();

        let ws = app
            .workspaces
            .iter()
            .find(|w| w.id == "ws-r")
            .expect("workspace restored");
        // The LIVE sibling reattached despite the gone sibling failing first.
        assert!(
            ws.sessions.iter().any(|s| s.uid == uid_live),
            "live sibling MUST reattach even though a prior sibling on the \
             same reachable host was gone (no host poisoning)",
        );
        // The gone session is preserved; the live one is NOT skipped.
        let skipped = app
            .skipped_manifest_entries
            .get("ws-r")
            .expect("ws-r has a skipped entry");
        assert!(skipped.iter().any(|e| e.uid == uid_gone));
        assert!(
            !skipped.iter().any(|e| e.uid == uid_live),
            "the live session must NOT be skipped",
        );

        // Cleanup.
        let _ = crate::client_session::rpc_kill_session(
            &mgr_sock,
            crate::daemon_launch::operator_token(),
            &uid_live,
        );
        stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&mgr_sock);
        let _ = dhandle.join();
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Phase 4 latency hardening: two remote entries on an UNREACHABLE host
    /// (here an unknown host → `for_host` errors) are BOTH preserved, and the
    /// host is dialed at most once (the `unreachable_hosts` cache skips the
    /// second dial). No daemon needed — `for_host` on an unknown host errors
    /// instantly.
    #[test]
    fn restore_preserves_all_entries_on_unreachable_host() {
        use cm_daemon::manifest::{Manifest, ManifestEntry, ManifestWorkspace};

        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();

        let mk = |uid: &str| ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: uid.into(),
            managed_by_uid: None,
            generation: 0,
            label: "claude".into(),
            session_type: "claude".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            seeded_from_snapshot: None,
            last_exit: None,
            // Unknown host → host_pool.for_host returns Err.
            host_id: cm_daemon::host_id::HostId::new("ghost"),
        };
        let mw = ManifestWorkspace {
            color: None,
            pinned: false,
            id: "ws-g".into(),
            name: "g".into(),
            is_closed: false,
            is_cloud: false,
            worktree_path: None,
            main_repo_path: None,
            repo_url: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![mk("uid-g1"), mk("uid-g2")],
            tombstones: Vec::new(),
        };
        let mut workspaces = HashMap::new();
        workspaces.insert("ws-g".to_string(), mw);
        let manifest = Manifest {
            task_colors: Default::default(),
            workspaces,
            bindings: HashMap::new(),
            view: None,
            hide_continuous: false,
            continuous_column_on: false,
        };
        std::fs::write(
            cm_dir.join("tui-sessions.json"),
            serde_json::to_string(&manifest).expect("ser"),
        )
        .expect("write manifest");

        let orig_home = std::env::var_os("HOME");
        // Pin CM_DAEMON_SOCKET into the temp home too. The HOME override
        // alone does NOT isolate this test: `default_socket_path()` prefers
        // $CM_DAEMON_SOCKET, which every cm-TUI-hosted session exports as
        // the OPERATOR'S live daemon socket — so `restore_sessions()`'s
        // trailing adopt scan would dial the real daemon and attach (and
        // 80×24-resize) any real agent-spawned sessions it finds there.
        let orig_sock = std::env::var_os("CM_DAEMON_SOCKET");
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("CM_DAEMON_SOCKET", home.join(".cm/daemon.sock"));
        }
        // App::new synthesizes a local-only pool → "ghost" is unknown.
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        app.sessions_restored = true;
        app.restore_sessions();

        // Both entries preserved; none adopted.
        let skipped = app
            .skipped_manifest_entries
            .get("ws-g")
            .expect("ws-g has skipped entries");
        assert_eq!(
            skipped.len(),
            2,
            "both entries on an unreachable host must be preserved",
        );
        let ws = app.workspaces.iter().find(|w| w.id == "ws-g").expect("ws-g");
        assert!(
            ws.sessions.is_empty(),
            "no session adopted from an unreachable host",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match orig_sock {
            Some(s) => unsafe { std::env::set_var("CM_DAEMON_SOCKET", s) },
            None => unsafe { std::env::remove_var("CM_DAEMON_SOCKET") },
        }
    }

    /// Phase 4 startup-freeze fix (named acceptance): `restore_sessions` must
    /// NOT perform a blocking remote dial on the main thread for a remote
    /// entry whose host uses a tunnel transport (`ssh-unix`). Such a dial
    /// spawns `ssh -N -L ...` and BLOCKS up to ~3s for the local socket to
    /// bind — running it before the first frame paints was the startup
    /// freeze. Instead the entry is DEFERRED: queued in
    /// `pending_remote_reattach` for the main loop to reattach once the
    /// tunnel is warm, and preserved verbatim in `skipped_manifest_entries`
    /// so a save during the window round-trips it.
    ///
    /// Determinism / no real ssh: the `ssh-unix` "manager" pool is INJECTED
    /// after `App::new` (which spawned its `manifest.watch` consumers for the
    /// synthesized local-only config only), so nothing warms the manager
    /// tunnel during the test. The proof that no synchronous main-thread dial
    /// happened is `has_live_tunnel_for_test() == false` right after
    /// `restore_sessions` — `restore_sessions` never called `for_host` on the
    /// manager handle, so no `SshTunnel` was spawned.
    #[test]
    fn restore_defers_blocking_remote_host_no_synchronous_dial() {
        use cm_daemon::manifest::{Manifest, ManifestEntry, ManifestWorkspace};

        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let local_sock = cm_dir.join("daemon.sock");

        let entry = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: "uid-ssh".into(),
            managed_by_uid: None,
            generation: 0,
            label: "claude".into(),
            session_type: "claude".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            seeded_from_snapshot: None,
            last_exit: None,
            host_id: cm_daemon::host_id::HostId::new("manager"),
        };
        let mw = ManifestWorkspace {
            color: None,
            pinned: false,
            id: "ws-ssh".into(),
            name: "ssh".into(),
            is_closed: false,
            is_cloud: false,
            worktree_path: Some(home.join("wt")),
            main_repo_path: None,
            repo_url: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![entry.clone()],
            tombstones: Vec::new(),
        };
        let mut workspaces = HashMap::new();
        workspaces.insert("ws-ssh".to_string(), mw);
        let manifest = Manifest {
            task_colors: Default::default(),
            workspaces,
            bindings: HashMap::new(),
            view: None,
            hide_continuous: false,
            continuous_column_on: false,
        };
        std::fs::write(
            cm_dir.join("tui-sessions.json"),
            serde_json::to_string(&manifest).expect("ser"),
        )
        .expect("write manifest");

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
        // Inject a 2-host pool where "manager" is a tunnel transport
        // (`ssh-unix`) — exactly the case whose `for_host` blocks ~3s. The
        // bogus ssh_host is never dialed because restore defers it; building
        // the handle does NOT spawn ssh.
        let hosts = crate::hosts::HostsConfig {
            hosts: vec![
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::local(),
                    transport: crate::hosts::HostTransport::Unix {
                        socket: local_sock.clone(),
                    },
                    default: true,
                },
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::new("manager"),
                    transport: crate::hosts::HostTransport::SshUnix {
                        ssh_host: "cm-test-nonexistent-host".into(),
                        ssh_user: None,
                        remote_socket: PathBuf::from("/remote/daemon.sock"),
                    },
                    default: false,
                },
            ],
        };
        app.host_pool =
            std::sync::Arc::new(crate::host_pool::HostPool::from_config(&hosts).expect("pool"));
        app.sessions_restored = true;
        app.restore_sessions();

        // DEFERRED, not synchronously reattached: the entry sits in the retry
        // queue tagged with its remote host.
        assert_eq!(
            app.pending_remote_reattach.len(),
            1,
            "the ssh-unix remote entry must be queued for deferred reattach",
        );
        assert_eq!(app.pending_remote_reattach[0].entry.uid, "uid-ssh");
        assert_eq!(
            app.pending_remote_reattach[0].entry.host_id,
            cm_daemon::host_id::HostId::new("manager"),
        );

        // PRESERVED on disk (no data loss during the deferral window).
        let skipped = app
            .skipped_manifest_entries
            .get("ws-ssh")
            .expect("ws-ssh has a preserved skipped entry");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].uid, "uid-ssh");

        // NOT reattached into the live workspace yet (tunnel never warmed).
        let ws = app.workspaces.iter().find(|w| w.id == "ws-ssh").expect("ws-ssh");
        assert!(
            ws.sessions.is_empty(),
            "a deferred remote entry must not be synchronously reattached",
        );

        // THE no-synchronous-dial proof: restore_sessions never called
        // `for_host` on the manager handle, so no SSH tunnel was spawned on
        // the main thread (a real dial would have blocked ~3s and left a live
        // tunnel here).
        let manager = app
            .host_pool
            .get_handle_for_test(&cm_daemon::host_id::HostId::new("manager"))
            .expect("manager handle");
        assert!(
            !manager.has_live_tunnel_for_test(),
            "restore_sessions must NOT spawn the ssh tunnel on the main \
             thread — the dial is deferred to the manifest.watch consumer / \
             main-loop reattach",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Phase 4 startup-freeze fix (#2 guard): the deferred-reattach window can
    /// outlive a user CLOSING the workspace (it's effectively unbounded while
    /// the remote host is down), so `drain_deferred_remote_reattach` must NOT
    /// resurrect a live session into a workspace the user closed. With a LIVE
    /// in-process daemon at the manager socket the reattach WOULD succeed
    /// absent the guard — so an empty `ws.sessions` after the drain proves the
    /// `is_closed` guard fired. The raw entry stays preserved in
    /// `skipped_manifest_entries` (closed workspaces ride their entries on
    /// disk), and the retry is dropped from the queue.
    #[test]
    fn drain_deferred_remote_reattach_skips_closed_workspace() {
        use cm_daemon::manifest::ManifestEntry;
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let local_sock = cm_dir.join("daemon.sock");
        let mgr_sock = cm_dir.join("manager.sock");

        // In-process daemon at the MANAGER socket holding workspace ws-c.
        let mut state_inner = cm_daemon::state::DaemonState::new();
        state_inner.attach_addr = mgr_sock.to_string_lossy().into_owned();
        state_inner.workspaces.insert(
            "ws-c".into(),
            cm_daemon::manifest::ManifestWorkspace {
                color: None,
                pinned: false,
                id: "ws-c".into(),
                name: "c".into(),
                is_closed: false,
                is_cloud: false,
                worktree_path: Some(wt.clone()),
                main_repo_path: None,
                repo_url: None,
                worker_vm: None,
                worker_zone: None,
                host_id: cm_daemon::host_id::HostId::local(),
                sessions: Vec::new(),
                tombstones: Vec::new(),
            },
        );
        let state = Arc::new(std::sync::Mutex::new(state_inner));
        let listener = UnixListener::bind(&mgr_sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let dstate = state.clone();
        let dstop = stop.clone();
        let dhandle = std::thread::spawn(move || {
            while !dstop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let st = dstate.clone();
                        std::thread::spawn(move || {
                            let _ = stream.set_read_timeout(Some(
                                std::time::Duration::from_secs(2),
                            ));
                            let _ = stream.set_write_timeout(Some(
                                std::time::Duration::from_secs(2),
                            ));
                            let req = match cm_daemon::control::wire::read_request(&mut stream) {
                                Ok(Some(r)) => r,
                                _ => return,
                            };
                            use cm_daemon::control::dispatch::DispatchOutcome::*;
                            match cm_daemon::control::dispatch::dispatch_request(&st, &req) {
                                Done(resp) => {
                                    let _ = cm_daemon::control::wire::write_response(&mut stream, &resp);
                                }
                                AttachStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_attach_stream(&mut stream, st, handle);
                                    }
                                }
                                ManifestWatchStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_manifest_watch_stream(&mut stream, handle);
                                    }
                                }
                                EventsSubscribeStream { response, handle } => {
                                    if cm_daemon::control::wire::write_response(&mut stream, &response).is_ok() {
                                        cm_daemon::control::stream::handle_events_subscribe_stream(&mut stream, handle);
                                    }
                                }
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        // A genuinely live session on the manager daemon — the reattach target.
        let uid = new_session_uid();
        let argv = vec!["/bin/bash".to_string()];
        let cfg = crate::client_session::ClientSessionConfig {
            daemon_socket: &mgr_sock,
            operator_token_id: crate::daemon_launch::operator_token(),
            uid: &uid,
            workspace_id: "ws-c",
            label: "claude-code",
            session_type: "claude-code",
            argv: &argv,
            working_dir: &wt,
            env: std::collections::BTreeMap::new(),
            cols: 80,
            rows: 24,
            memory_cap_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            cgroup_path: None,
            worktree_path: Some(&wt),
            task_id: None,
            transcript_path: None,
            workflow_run_id: None,
            workflow_role: None,
        };
        crate::client_session::rpc_start_session_full(&cfg)
            .expect("manager daemon spawns the live session");

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
        // Manager is a Unix-direct host → `live_socket_path` reports it live
        // immediately, so the drain proceeds to the workspace/`is_closed`
        // check (the bit under test) rather than parking on the probe.
        let hosts = crate::hosts::HostsConfig {
            hosts: vec![
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::local(),
                    transport: crate::hosts::HostTransport::Unix {
                        socket: local_sock.clone(),
                    },
                    default: true,
                },
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::new("manager"),
                    transport: crate::hosts::HostTransport::Unix {
                        socket: mgr_sock.clone(),
                    },
                    default: false,
                },
            ],
        };
        app.host_pool =
            std::sync::Arc::new(crate::host_pool::HostPool::from_config(&hosts).expect("pool"));
        app.sessions_restored = true;

        // Simulate the close-during-pending-window: a now-CLOSED workspace
        // plus a queued reattach (with its preserved-on-disk copy) for the
        // live session.
        app.workspaces.push(Workspace {
            color: None,
            pinned: false,
            id: "ws-c".into(),
            name: "c".into(),
            is_closed: true,
            is_cloud: false,
            repo_url: None,
            worktree_path: Some(wt.clone()),
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: Vec::new(),
            tombstones: Vec::new(),
            is_pushing: false,
        });
        let entry = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: uid.clone(),
            managed_by_uid: None,
            generation: 0,
            label: "claude".into(),
            session_type: "claude".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            seeded_from_snapshot: None,
            last_exit: None,
            host_id: cm_daemon::host_id::HostId::new("manager"),
        };
        app.skipped_manifest_entries
            .insert("ws-c".into(), vec![entry.clone()]);
        app.pending_remote_reattach.push(PendingRemoteReattach::new(
            "ws-c".into(),
            entry.clone(),
        ));

        app.drain_deferred_remote_reattach();

        // The session is NOT resurrected into the closed workspace — even
        // though the live daemon would have attached successfully.
        let ws = app.workspaces.iter().find(|w| w.id == "ws-c").expect("ws-c");
        assert!(
            ws.sessions.is_empty(),
            "drain must not reattach a session into a CLOSED workspace",
        );
        // Retry dropped from the queue...
        assert!(
            app.pending_remote_reattach.is_empty(),
            "closed-workspace entry must be dropped from the retry queue",
        );
        // ...but the raw entry stays preserved on disk (no data loss).
        assert_eq!(
            app.skipped_manifest_entries
                .get("ws-c")
                .map(|v| v.len()),
            Some(1),
            "the entry must remain preserved in skipped_manifest_entries",
        );

        // Cleanup.
        let _ = crate::client_session::rpc_kill_session(
            &mgr_sock,
            crate::daemon_launch::operator_token(),
            &uid,
        );
        stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&mgr_sock);
        let _ = dhandle.join();
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Phase 5 (remote-session-execution): `A-f` (launch_workflow_via_daemon)
    /// routes `start_workflow` to the WORKSPACE's host socket — a remote-hosted
    /// workspace → the remote daemon (with the remote worktree), a local
    /// workspace → the local daemon, unchanged. Recording listeners on both
    /// sockets capture where the RPC landed (no real participant spawn).
    ///
    /// The cm-manager live e2e (A-f a feedback workflow on a remote worktree,
    /// watch worker→reviewer→manager drive to done from the TUI) is a deferred
    /// MANUAL operator pass — no live TUI + VM in this loop.
    #[test]
    fn a_f_routes_start_workflow_to_workspace_host() {
        use std::sync::atomic::Ordering;

        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cm_dir = home.join(".cm");
        std::fs::create_dir_all(&cm_dir).unwrap();
        let local_sock = cm_dir.join("daemon.sock");
        let mgr_sock = cm_dir.join("manager.sock");

        let (local_cap, local_stop, local_h) =
            spawn_recording_listener(local_sock.clone());
        let (mgr_cap, mgr_stop, mgr_h) = spawn_recording_listener(mgr_sock.clone());

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
        let hosts = crate::hosts::HostsConfig {
            hosts: vec![
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::local(),
                    transport: crate::hosts::HostTransport::Unix { socket: local_sock.clone() },
                    default: true,
                },
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::new("manager"),
                    transport: crate::hosts::HostTransport::Unix { socket: mgr_sock.clone() },
                    default: false,
                },
            ],
        };
        app.host_pool =
            std::sync::Arc::new(crate::host_pool::HostPool::from_config(&hosts).expect("pool"));

        // Local-hosted workspace (its session's host_id defaults to local).
        let local_sess = make_simple_session(
            "claude",
            "claude",
            Session::new("/bin/bash", &[], 80, 24, None, Default::default(), None)
                .expect("local sess"),
            None,
        );
        app.workspaces.push(Workspace {
            color: None,
            pinned: false,
            id: "ws-local".into(),
            name: "l".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: Some(PathBuf::from("/tmp/wt-local")),
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![local_sess],
            tombstones: Vec::new(),
            is_pushing: false,
        });

        // Remote-hosted workspace (its session pinned to host "manager").
        let mut remote_sess = make_simple_session(
            "claude",
            "claude",
            Session::new("/bin/bash", &[], 80, 24, None, Default::default(), None)
                .expect("remote sess"),
            None,
        );
        remote_sess.host_id = cm_daemon::host_id::HostId::new("manager");
        app.workspaces.push(Workspace {
            color: None,
            pinned: false,
            id: "ws-remote".into(),
            name: "r".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: Some(PathBuf::from("/remote/wt")),
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![remote_sess],
            tombstones: Vec::new(),
            is_pushing: false,
        });

        // A-f on each workspace (empty slots → fresh-spawn launch; the
        // recording listener doesn't run the real spawn).
        app.launch_workflow_via_daemon("ws-local", "feedback", &[], Some("goal-l".into()), None);
        app.launch_workflow_via_daemon("ws-remote", "feedback", &[], Some("goal-r".into()), None);

        let find_sw = |caps: &std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>|
         -> Option<serde_json::Value> {
            caps.lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .find(|c| c["method"] == "start_workflow")
                .cloned()
        };

        // Remote A-f → the MANAGER socket, with the REMOTE worktree.
        let mgr_sw = find_sw(&mgr_cap)
            .expect("remote A-f must send start_workflow to the manager socket");
        assert_eq!(mgr_sw["params"]["worktree"], "/remote/wt");
        assert_eq!(mgr_sw["params"]["workspace_id"], "ws-remote");

        // Local A-f → the LOCAL socket, with the local worktree (unchanged).
        let local_sw = find_sw(&local_cap)
            .expect("local A-f must send start_workflow to the local socket");
        assert_eq!(local_sw["params"]["worktree"], "/tmp/wt-local");
        assert_eq!(local_sw["params"]["workspace_id"], "ws-local");

        // Cross-check: neither socket got the OTHER workspace's launch.
        assert!(
            mgr_cap
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .all(|c| c["method"] != "start_workflow"
                    || c["params"]["workspace_id"] == "ws-remote"),
            "the manager socket must only receive the remote workspace's launch",
        );
        assert!(
            local_cap
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .all(|c| c["method"] != "start_workflow"
                    || c["params"]["workspace_id"] == "ws-local"),
            "the local socket must only receive the local workspace's launch",
        );

        // Cleanup.
        local_stop.store(true, Ordering::SeqCst);
        mgr_stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&local_sock);
        let _ = std::os::unix::net::UnixStream::connect(&mgr_sock);
        let _ = local_h.join();
        let _ = mgr_h.join();
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

#[cfg(test)]
mod activity_summary_tests {
    //! Phase 6 activity feed. Pins the read-only/mutating partition (only
    //! mutating methods get logged) and the formatting of the most-used
    //! summary lines. The doc on `activity_summary_for` says new mutating
    //! methods MUST be added explicitly; these tests fail-loud if a
    //! mutating method is added without surfacing in the feed.

    use super::activity_summary_for;
    use serde_json::json;

    #[test]
    fn read_only_methods_are_silent() {
        // None of these may produce a summary — they're high-frequency
        // observability calls and would drown out real mutations.
        for m in [
            "ping",
            "resolve_authorized_session",
            "list_sessions",
            "list_workflows",
            "list_subtasks",
            "get_workflow_state",
        ] {
            assert!(
                activity_summary_for(m, &json!({}), &json!({})).is_none(),
                "{m} must NOT produce an activity-feed entry"
            );
        }
    }

    #[test]
    fn unknown_method_silent_by_default() {
        // Defensive: a method that isn't in the explicit list (e.g. a
        // new control-socket method added without thinking about
        // observability) defaults to no-log. The author has to come
        // here and add a branch — surfacing the omission rather than
        // silently dropping it.
        assert!(
            activity_summary_for("totally_made_up_method", &json!({}), &json!({})).is_none()
        );
    }

    #[test]
    fn send_input_summarizes_target_and_text_snippet() {
        let s = activity_summary_for(
            "send_input",
            &json!({"session_uid": "ts-12345678abcdefXX", "text": "hello world"}),
            &json!({}),
        )
        .expect("send_input is mutating");
        // Truncates uid to 8 chars and quotes the text.
        assert!(s.contains("ts-12345"), "{s}");
        assert!(s.contains("\"hello world\""), "{s}");
    }

    #[test]
    fn send_input_truncates_long_text_with_ellipsis() {
        let long = "x".repeat(200);
        let s = activity_summary_for(
            "send_input",
            &json!({"session_uid": "ts-AAAAAAAA", "text": long}),
            &json!({}),
        )
        .expect("send_input is mutating");
        // Snippet is at most ~40 chars + a "…" suffix.
        assert!(s.contains("…"), "expected truncation marker in {s}");
        // Sanity: the full 200-char run isn't in there.
        assert!(!s.contains(&"x".repeat(200)));
    }

    #[test]
    fn create_subtask_appends_new_task_id() {
        let s = activity_summary_for(
            "create_subtask",
            &json!({"name": "demo", "worktree_mode": "branch"}),
            &json!({"task_id": "abcd1234-deadbeef", "worktree_path": "/tmp/wt"}),
        )
        .expect("create_subtask is mutating");
        // Format is "create_subtask(<name>, <mode>) → <new-id-prefix>".
        assert!(s.starts_with("create_subtask(demo, branch)"), "{s}");
        assert!(s.contains("→"), "{s}");
        assert!(s.contains("abcd1234"), "{s}");
    }

    #[test]
    fn mark_subtask_done_includes_close_worktree_flag() {
        let s = activity_summary_for(
            "mark_subtask_done",
            &json!({"task_id": "task-uuid-v1", "close_worktree": true}),
            &json!({"ok": true, "worktree_removed": true}),
        )
        .expect("mark_subtask_done is mutating");
        assert!(s.contains("close_worktree=true"), "{s}");
    }

    #[test]
    fn start_workflow_truncates_task_id() {
        let s = activity_summary_for(
            "start_workflow",
            &json!({
                "workflow_name": "feedback",
                "task_id": "1914682b-b633-4d15-9df6-20ba036427bc",
                "goal": "anything",
            }),
            &json!({"run_id": "wf_xxx"}),
        )
        .expect("start_workflow is mutating");
        assert!(s.starts_with("start_workflow(feedback"), "{s}");
        assert!(s.contains("task=1914682b"), "{s}");
        // Full UUID must not bleed through — the column would overflow.
        assert!(!s.contains("20ba036427bc"), "{s}");
    }
}
