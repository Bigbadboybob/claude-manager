//! Session + workspace lifecycle: daemon spawn/attach plumbing, create/close/tombstone, planning launch, push/pull, daemon state pushes.

use super::*;

/// 12e-r6 (interim fail-fast helper): every spawn site in the
/// TUI builds argv / env / cwd / mcp_config paths on the TUI's
/// LOCAL machine and either runs them locally OR sends them to
/// the daemon for execution. For `host_id == HostId::local()`
/// both are fine (daemon and TUI share the filesystem). For a
/// true remote host (cm-manager VM, Mac mini, etc.), the local
/// paths don't exist on the remote and the spawn either fails
/// opaquely (daemon-routed) or silently mistags the resulting
/// session (local-PTY routed, `ts.host_id` says remote but the
/// process is local).
///
/// Until Phase 3 ships daemon-side path resolution (slice 12g
/// cm-manager VM prep), the only honest behavior is to refuse
/// the operation. Every spawn site that can carry a non-local
/// host_id calls this helper at the TOP — before any work that
/// would need to be undone (worktree creation, file writes,
/// kill RPCs for prior sessions, etc.).
///
/// `op_name` shows up in the error message so operators can
/// tell A-n from A-s from MCP-spawn from workflow-respawn at
/// a glance.
pub(crate) fn guard_local_host_only(
    host_id: &cm_daemon::host_id::HostId,
    op_name: &str,
) -> std::io::Result<()> {
    if host_id == &cm_daemon::host_id::HostId::local() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "{} on non-local host `{}` is not yet supported — \
         daemon-side path resolution is deferred to Phase 3 \
         (see daemon/NOTES.md slice 12g). Retarget this \
         workspace to `local` before retrying, or wait for the \
         follow-up slice that adds remote-execution support.",
        op_name,
        host_id.as_str(),
    )))
}

/// migrate-tui-local Issue 1: attach to a daemon session whose
/// UID the daemon already knows. This is the manifest-restore
/// path on TUI startup: the daemon + its PTY children survive
/// across TUI restarts, so the restore must `session.attach` /
/// `attach.open` against the existing entry rather than
/// `start_session` (which would return Conflict on the duplicate
/// UID and drop the session from `ws.sessions`).
///
/// Returns:
///   - `Ok(Session)` — attach succeeded; caller wraps in a
///     TerminalSession that references the same daemon child.
///   - `Err(e)` — attach failed (e.g. the session exited between
///     the `list_sessions` probe and this call). Caller may
///     either surface to the user or fall back to spawning.
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_attach_via_daemon_with_deps(
    host_pool: &crate::host_pool::HostPool,
    session_uid: &str,
    workspace_id: &str,
    worktree_path: &Path,
    session_type: &str,
    label: &str,
    cols: u16,
    rows: u16,
    task_id: Option<&str>,
    workflow_run_id: Option<&str>,
    workflow_role: Option<&str>,
    host_id: &cm_daemon::host_id::HostId,
    transcript_path: Option<&str>,
) -> anyhow::Result<Session> {
    let internal_session_type = normalize_session_type_to_internal(session_type);
    let wire_session_type = match internal_session_type {
        "claude" => "claude-code",
        other => other,
    };
    let daemon_socket = host_pool
        .for_host(host_id)
        .map_err(|e| anyhow::anyhow!(
            "host_pool.for_host({}) unavailable: {}",
            host_id.as_str(),
            e,
        ))?
        .socket_path()
        .ok_or_else(|| anyhow::anyhow!(
            "host_pool.for_host({}) has no live socket path",
            host_id.as_str(),
        ))?;
    // The attach path doesn't need argv / env / memory cap
    // fields — the daemon's existing session owns those. We
    // still populate the ClientSessionConfig shape so the
    // attach call has a uniform builder.
    let argv: Vec<String> = Vec::new();
    let env: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let cs_config = crate::client_session::ClientSessionConfig {
        daemon_socket: &daemon_socket,
        operator_token_id: crate::daemon_launch::operator_token(),
        uid: session_uid,
        workspace_id,
        label,
        session_type: wire_session_type,
        argv: &argv,
        working_dir: worktree_path,
        env,
        cols,
        rows,
        memory_cap_bytes: None,
        memory_cap_hard_bytes: None,
        cgroup_prefix: None,
        cgroup_path: None,
        worktree_path: Some(worktree_path),
        task_id,
        transcript_path,
        workflow_run_id,
        workflow_role,
    };
    let session = crate::session::Session::new_attached_existing(cs_config)?;
    // Kick a resize so a session whose daemon PTY was spawned at a different
    // size (e.g. an 80×24 headless workflow spawn) immediately matches this
    // terminal and repaints — same rationale as the respawn-path kick. The
    // attach itself never resizes the daemon PTY (it only sizes the local
    // grid), so without this a small-spawned session renders cut off forever.
    session.resize(cols, rows);

    // migrate-tui-local Issue D: the `session.attach` RPC only
    // takes `{ uid }` — the daemon doesn't read transcript_path
    // / workflow tags from its params. The spawn branch threads
    // those via `start_session`'s param shape, but the attach
    // branch has to push them through the existing setter
    // channels AFTER attach succeeds. Without these pushes the
    // restored session stays `pending` for
    // `resolve_authorized_session` and MCP `read_session_output`
    // serves nothing.
    //
    // Conditional: only push fields the manifest carries. The
    // daemon already has whatever values were set on the
    // surviving session, and the setters are idempotent — a
    // no-change push is harmless.
    //
    // `task_id` has no setter RPC. That's fine: the attach
    // branch only fires when the daemon's `state.sessions` map
    // already has this UID, which means the daemon's
    // `task_id` field is whatever it was set to at the original
    // `start_session` time (preserved across TUI restart since
    // the daemon owns it).
    let operator_token_id = crate::daemon_launch::operator_token();
    if let Some(tp) = transcript_path {
        if let Err(e) = crate::client_session::rpc_set_transcript_path(
            &daemon_socket,
            operator_token_id,
            session_uid,
            tp,
        ) {
            eprintln!(
                "cm-tui: attach({}) set_transcript_path failed: {} \
                 (daemon's resolve_authorized_session may stay \
                 pending until next rebind retries)",
                session_uid, e,
            );
        }
    }
    // migrate-tui-local Issue E: push the manifest's workflow
    // state in BOTH directions, not just the set direction.
    //   - (Some, Some) → set the daemon-side tags.
    //   - (None, None) → CLEAR. `restore_sessions` runs
    //     `untag_stale_workflow` against the manifest entry
    //     before reaching this helper, so a manifest with
    //     `(None, None)` is the authoritative "this session is
    //     no longer a workflow participant" signal. Without the
    //     clear, the daemon's `lookup_session_any` keeps
    //     returning the old (Detached/Done) run id and
    //     `workflow_transition` / `workflow_done` authorize
    //     against the wrong run.
    //   - half-tagged (Some/None or None/Some) → log + skip.
    //     Represents a corrupted manifest entry; the daemon
    //     rejects half-tagged pushes by contract.
    match (workflow_run_id, workflow_role) {
        (Some(_), Some(_)) | (None, None) => {
            if let Err(e) = crate::client_session::rpc_set_workflow_context(
                &daemon_socket,
                operator_token_id,
                session_uid,
                workflow_run_id,
                workflow_role,
            ) {
                eprintln!(
                    "cm-tui: attach({}) set_workflow_context failed: {} \
                     (workflow auth from this role may target the \
                     wrong run until next push)",
                    session_uid, e,
                );
            }
        }
        _ => {
            eprintln!(
                "cm-tui: attach({}) half-tagged workflow state \
                 (run_id={:?}, role={:?}), skipping push — \
                 daemon rejects partial workflow tuples",
                session_uid, workflow_run_id, workflow_role,
            );
        }
    }

    Ok(session)
}

/// migrate-tui-local Issue 3: compute the on-disk transcript
/// path for a session_type + transcript_id pair WITHOUT a live
/// `TerminalSession`. Resume/restore call sites use this to
/// thread the path into `try_spawn_via_daemon_with_deps` so the
/// daemon's `resolve_authorized_session` resolves immediately
/// instead of returning `pending` until the post-spawn detector
/// pushes the path.
///
/// Returns `Some` only for Claude (the file is deterministic in
/// the worktree + transcript_id). Codex's path is discovered by
/// scanning `~/.codex/sessions/`, but on resume the new rollout
/// id is fresh post-spawn — pre-spawn the path is unknown, so
/// the post-spawn detector + `push_transcript_path_to_daemon_if_attached`
/// continues to handle codex resumes.
pub(crate) fn pre_spawn_transcript_path(
    session_type: &str,
    worktree_path: &Path,
    transcript_id: &str,
) -> Option<String> {
    let internal = normalize_session_type_to_internal(session_type);
    match internal {
        "claude" => crate::agent::claude_transcript_path(worktree_path, transcript_id)
            .map(|p| p.to_string_lossy().to_string()),
        _ => None,
    }
}

/// Build the `gcloud` argv that opens a live, READ-ONLY attach to a
/// cloud backtest worker's tmux. The backtest pipeline runs inside a
/// ROOT-owned tmux session named `backtest` (see
/// `worker/backtest_startup.sh`), so the remote command uses `sudo` to
/// reach root's tmux server and `tmux attach -r` (read-only) so the
/// watcher's input is not forwarded into the live run's pane. (This is
/// a run-safety convenience, not a security boundary — the attach is
/// root-over-ssh on a VM the operator already controls with their own
/// gcloud creds.)
///
/// `ssh -t` forces PTY allocation (tmux won't render otherwise) and the
/// `TERM=xterm-256color` prefix gives tmux a sane terminal type.
///
/// `use_iap` routes through `--tunnel-through-iap` — needed when the
/// client can't reach the VM's port 22 directly. When the target
/// project's firewall already exposes tcp:22 (the PMS
/// `default-allow-ssh 0.0.0.0/0` rule does), a direct SSH is ~3-5s
/// faster and needs no rule change, so `false` is the default.
///
/// `vm` / `project` / `zone` are passed as discrete argv elements (this
/// is spawned via `portable-pty`'s exec, NOT a shell), so no shell
/// metacharacter escaping is required or possible — and the remote
/// command string is a fixed literal with no interpolation, so a
/// hostile VM name cannot inject into it.
pub(crate) fn backtest_watch_ssh_args(
    vm: &str,
    project: &str,
    zone: &str,
    use_iap: bool,
) -> Vec<String> {
    let mut args = vec![
        "compute".to_string(),
        "ssh".to_string(),
        vm.to_string(),
        format!("--project={}", project),
        format!("--zone={}", zone),
    ];
    if use_iap {
        args.push("--tunnel-through-iap".to_string());
    }
    // `--` ends gcloud's own flags; everything after is passed to ssh.
    // `-t` forces a TTY; the final element is the remote command.
    args.push("--".to_string());
    args.push("-t".to_string());
    args.push(BACKTEST_WATCH_REMOTE_CMD.to_string());
    args
}

/// The remote command run over ssh to open a read-only view of the
/// backtest tmux.
///
/// It waits (bounded, ~120s) for the `backtest` session to appear before
/// attaching, because `worker_vm`/`ttyd_url` are stamped at VM-CREATE
/// time — before the in-VM startup script has created the tmux — so an
/// operator who hits `A-w` the instant the task shows a VM would
/// otherwise race the session into existence and get an immediate "no
/// sessions" exit. When it appears, `exec tmux attach -r` replaces the
/// wrapper (`-r` = read-only, the core safety guarantee); if it never
/// does, it prints a clear message and exits non-zero so the session
/// closes cleanly rather than hanging.
///
/// `sudo` is required because the pipeline tmux is ROOT-owned. On GCE
/// the ssh login user has passwordless sudo (google-sudoers), verified
/// against a live worker.
pub(crate) const BACKTEST_WATCH_REMOTE_CMD: &str = "TERM=xterm-256color sudo sh -c 'i=0; while [ $i -lt 60 ]; do tmux has-session -t backtest 2>/dev/null && exec tmux attach -r -t backtest; i=$((i+1)); sleep 2; done; echo \"cm-watch: backtest tmux not present after 120s\"; exit 1'";

/// Pure parse of the `CM_BACKTEST_SSH_IAP` toggle value: `None`
/// (unset) or an unrecognized value = direct SSH; a recognized truthy
/// token = tunnel through IAP. Kept separate from the env read so it can
/// be unit-tested without mutating the process-global environment (which
/// would race the other unit tests in this binary's parallel threads).
pub(crate) fn iap_flag_enabled(val: Option<&str>) -> bool {
    matches!(val.map(str::trim), Some("1" | "true" | "yes" | "on"))
}

/// Resolve whether the watch attach should tunnel through IAP. Direct
/// SSH is the default (port 22 already open on the backtest project);
/// the operator can force IAP — needed if their network blocks outbound
/// :22 — with `CM_BACKTEST_SSH_IAP=1`.
pub(crate) fn backtest_watch_use_iap() -> bool {
    iap_flag_enabled(std::env::var("CM_BACKTEST_SSH_IAP").ok().as_deref())
}

/// migrate-tui-local: free-function form of `App::try_spawn_via_daemon`
/// so spawn sites that don't have `&App` (workflow respawn path,
/// controller fresh-context respawn) can share the same daemon-
/// routing body. `App::try_spawn_via_daemon` is now a thin
/// wrapper around this; both forms produce identical wire
/// behavior.
///
/// Returns:
///   - `Some(Ok(Session))` — daemon spawn succeeded.
///   - `Some(Err(e))` — daemon spawn failed; caller surfaces.
///   - `None` — the session_type isn't daemon-eligible
///     (post-migrate this should not happen for the three
///     supported types `claude` / `codex` / `bash`; callers
///     surface `None` as an internal-invariant error).
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_spawn_via_daemon_with_deps(
    host_pool: &crate::host_pool::HostPool,
    config: &crate::config::Config,
    memory_cap_status: &crate::memory_cap::MemoryCapAvailability,
    session_uid: &str,
    workspace_id: &str,
    worktree_path: &Path,
    session_type: &str,
    label: &str,
    resume_session_id: Option<&str>,
    cols: u16,
    rows: u16,
    task_id: Option<&str>,
    workflow_run_id: Option<&str>,
    workflow_role: Option<&str>,
    host_id: &cm_daemon::host_id::HostId,
    // migrate-tui-local Issue 3: pre-known transcript path for
    // resume/restore flows. Fresh spawns pass `None` (post-spawn
    // detector + `push_transcript_path_to_daemon_if_attached`
    // handles them). Resume/restore callers thread the path so
    // the daemon's `resolve_authorized_session` flips ready
    // immediately and MCP `read_session_output` can serve.
    transcript_path: Option<&str>,
) -> Option<anyhow::Result<Session>> {
    // 10f: daemon-eligibility is now driven solely by
    // session_type. Pre-flip a `CM_USE_DAEMON_SOCKET` opt-in
    // gate sat here; with the daemon always-on it served no
    // purpose. Map TUI session_type to engine + program
    // builder. gcloud and other ad-hoc shells aren't daemon-
    // eligible — fall through to local.
    //
    // 12e-r5 F2: normalize the wire vocabulary to the
    // internal form ONCE at the top so every downstream
    // consumer (argv match, wire_session_type mapping,
    // memory_cap_for lookup) sees the same value.
    let internal_session_type = normalize_session_type_to_internal(session_type);
    // migrate-tui-local Issue 2: build the WorkflowMeta tuple
    // ONCE here so `build_args` writes `CM_WORKFLOW_RUN_ID` +
    // `CM_ROLE` into the MCP server's env block. Pre-fix the
    // workflow tuple was constructed by the local spawn path
    // (spawn_agent_session) but dropped on the daemon-routed
    // shared helper — daemon stored workflow context on the
    // session record but the spawned MCP server child saw no
    // CM_WORKFLOW_RUN_ID env, so `workflow_transition` /
    // `workflow_done` from inside the role failed.
    let workflow_meta = match (workflow_run_id, workflow_role) {
        (Some(run_id), Some(role)) => {
            Some(crate::mcp_config::WorkflowMeta { run_id, role })
        }
        _ => None,
    };
    let argv_result = match internal_session_type {
        "claude" => crate::mcp_config::build_args(
            crate::mcp_config::SpawnTarget::Daemon,
            &crate::workflow::toml_schema::Engine::ClaudeCode,
            session_uid,
            workflow_meta.clone(),
            resume_session_id,
        ),
        "codex" => crate::mcp_config::build_args(
            crate::mcp_config::SpawnTarget::Daemon,
            &crate::workflow::toml_schema::Engine::Codex,
            session_uid,
            workflow_meta.clone(),
            resume_session_id,
        ),
        "bash" => Ok(("/bin/bash".to_string(), Vec::new())),
        _ => return None,
    };
    let (program, args) = match argv_result {
        Ok(v) => v,
        Err(e) => {
            return Some(Err(anyhow::anyhow!(
                "build_args(SpawnTarget::Daemon) for {} failed: {}",
                session_type,
                e
            )));
        }
    };

    // Memory cap wrap (slice 10c-e-3b parity). Same resolution
    // the local `spawn_agent_session` path uses — preflight
    // status × per-engine config bytes. When the cap is
    // None, `wrap_with_systemd_run` is a passthrough.
    let memory_cap = match (
        memory_cap_status,
        // 12e-r5 F2: look caps up by the INTERNAL
        // vocabulary (`"claude"` not `"claude-code"`).
        config.memory_cap_for(internal_session_type),
    ) {
        (
            crate::memory_cap::MemoryCapAvailability::Available { cgroup_prefix },
            Some((soft_bytes, hard_bytes)),
        ) => Some(crate::memory_cap::MemoryCap {
            soft_bytes,
            hard_bytes,
            session_uid: session_uid.to_string(),
            cgroup_prefix: cgroup_prefix.clone(),
        }),
        _ => None,
    };
    let (final_program, final_args, cgroup_path) =
        crate::session::wrap_with_systemd_run(&program, &args, &memory_cap);

    // Compose final argv as Vec<String> for the wire.
    let mut argv = Vec::with_capacity(final_args.len() + 1);
    argv.push(final_program);
    argv.extend(final_args);

    // Daemon-spawned child's process env. Mirrors what the
    // local `spawn_agent_session` injects (CM_TUI_SESSION_ID
    // is the only one — the MCP routing pin lives in the MCP
    // config file via `build_args` above, not in the parent
    // process env).
    let mut env: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    env.insert("CM_TUI_SESSION_ID".into(), session_uid.to_string());

    // 12e-r2 F1: route through `host_pool.for_host(host_id)`,
    // NOT `default_handle`. Pre-r2 this dialed the default
    // daemon while the resulting `TerminalSession.host_id`
    // was tagged `active_host` — subsequent per-session
    // RPCs (via `host_pool.for_host(&ts.host_id)`) would
    // hit a daemon that had no record of the UID.
    let daemon_socket = match host_pool.for_host(host_id) {
        Ok(h) => h.socket_path().expect(
            "socket_path returns Some after ensure_alive succeeded",
        ),
        Err(e) => {
            return Some(Err(anyhow::anyhow!(
                "host_pool.for_host({}) unavailable: {}",
                host_id.as_str(),
                e,
            )));
        }
    };
    // Memory-cap wire fields. When `memory_cap` is Some, the
    // soft byte count signals the daemon to populate
    // `SpawnParams.kills_dir`.
    let memory_cap_bytes = memory_cap.as_ref().map(|c| c.soft_bytes);
    // Map TUI's session_type to the canonical wire vocabulary
    // the daemon dispatches on ("claude-code" / "codex" /
    // "bash"). The branches above gate to these three values.
    let wire_session_type = match internal_session_type {
        "claude" => "claude-code",
        "codex" => "codex",
        "bash" => "bash",
        other => other,
    };
    let cs_config = crate::client_session::ClientSessionConfig {
        daemon_socket: &daemon_socket,
        operator_token_id: crate::daemon_launch::operator_token(),
        uid: session_uid,
        workspace_id,
        label,
        session_type: wire_session_type,
        argv: &argv,
        working_dir: worktree_path,
        env,
        cols,
        rows,
        memory_cap_bytes,
        memory_cap_hard_bytes: memory_cap.as_ref().map(|c| c.hard_bytes),
        cgroup_prefix: memory_cap.as_ref().map(|c| c.cgroup_prefix.as_path()),
        cgroup_path: cgroup_path.as_deref(),
        worktree_path: Some(worktree_path),
        task_id,
        // migrate-tui-local Issue 3: thread the caller-supplied
        // transcript path so the daemon registers the session
        // with the path already known. Resume/restore callers
        // pass `Some(...)`; fresh spawns pass `None` and let the
        // post-spawn detector + `push_transcript_path_to_daemon_if_attached`
        // resolve it.
        transcript_path,
        workflow_run_id,
        workflow_role,
    };
    Some(crate::session::Session::new_attached(cs_config))
}

impl App {
    /// Slice 10c-e-3: opt-in daemon spawn branch.
    ///
    /// When `CM_USE_DAEMON_SOCKET=1`, route the spawn through the
    /// daemon's RPC dance (`start_session` → `session.attach` →
    /// dial → `attach.open`) and return a `Session` whose
    /// `pty_writer` is `None` — `Session::write` falls back through
    /// the EventLoop's input channel, which encodes keystrokes as
    /// `StreamKind::Input` frames on the attach socket.
    ///
    /// Returns:
    ///   - `Some(Ok(Session))` — daemon spawn succeeded; caller
    ///     should use it directly (skip the local PTY path).
    ///   - `Some(Err(e))` — opt-in was on but the daemon spawn
    ///     failed. Caller surfaces the error — we DO NOT silently
    ///     fall back to the local path, because that would mask
    ///     daemon issues during the smoke test (the opt-in's
    ///     purpose is to exercise the daemon path end-to-end).
    ///   - `None` — opt-in is off OR the session_type isn't in the
    ///     daemon's allowlist (e.g. `gcloud` SSH paths). Caller
    ///     proceeds with the existing local spawn.
    ///
    /// ## Argv parity (slice 10c-e-3b)
    ///
    /// The daemon execs argv verbatim — no agent-specific
    /// reconstruction. We build `argv` and `env` here using the
    /// same `mcp_config::build_args(SpawnTarget::Daemon, ...)` that
    /// the local `Session::new` path uses (with `SpawnTarget::TuiLocal`)
    /// so the spawned child sees `--mcp-config` / Codex MCP
    /// overrides / `--resume` tokens identically to a local spawn.
    /// Memory cap wrapping (`wrap_with_systemd_run`) is applied
    /// here too so the daemon-spawned PTY runs under the same
    /// scope unit a local cap would produce.
    ///
    /// `session_type` is the TUI's own label (`"claude"` / `"codex"`
    /// / `"bash"`).
    pub fn try_spawn_via_daemon(
        &self,
        session_uid: &str,
        workspace_id: &str,
        worktree_path: &Path,
        session_type: &str,
        label: &str,
        resume_session_id: Option<&str>,
        cols: u16,
        rows: u16,
        // Sub-2a Finding #1: caller passes the task_id this
        // session is being spawned under, so the daemon's
        // DaemonSession.task_id is set at spawn time. None for
        // genuinely taskless flows (A-n create_local_session).
        task_id: Option<&str>,
        // 10d-2c-1 review round-5 (F1): workflow context at
        // spawn time, when the caller spawns a daemon-attached
        // session that's already a workflow participant. `None`
        // for the regular A-n / A-s paths; the workflow-launch
        // on existing daemon-attached sessions uses
        // `rpc_set_workflow_context` after the fact.
        workflow_run_id: Option<&str>,
        workflow_role: Option<&str>,
        // 12e-r2 F1: the host this session is being spawned
        // ON. The caller passes a snapshot of `App.active_host`
        // taken at the top of the user-action handler (NOT
        // re-read here, NOT re-read at the TerminalSession
        // construction site). The daemon socket is resolved
        // via `host_pool.for_host(host_id)`; the resulting
        // `ts.host_id` MUST also equal this value so every
        // subsequent per-session RPC (kill, set_transcript,
        // set_workflow_context, push_*) routes to the daemon
        // we actually spawned against.
        host_id: &cm_daemon::host_id::HostId,
        // migrate-tui-local Issue 3: caller-known transcript
        // path for resume/restore flows. Fresh A-n / A-s
        // spawns pass `None`.
        transcript_path: Option<&str>,
    ) -> Option<anyhow::Result<Session>> {
        // migrate-tui-local: thin wrapper around the free
        // `try_spawn_via_daemon_with_deps` helper so workflow
        // respawn paths (which don't have `&App`) can share the
        // same daemon-routing body. App stays the canonical
        // entry point for sites that already hold &self.
        try_spawn_via_daemon_with_deps(
            &self.host_pool,
            &self.config,
            &self.memory_cap_status,
            session_uid,
            workspace_id,
            worktree_path,
            session_type,
            label,
            resume_session_id,
            cols,
            rows,
            task_id,
            workflow_run_id,
            workflow_role,
            host_id,
            transcript_path,
        )
    }

    /// List all .jsonl file stems in the Claude project directory for a worktree.
    pub(crate) fn list_jsonl_files(worktree_path: &Path) -> Vec<String> {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return Vec::new(),
        };
        let path_str = match worktree_path.to_str() {
            Some(s) => s,
            None => return Vec::new(),
        };
        let encoded = path_str.replace('/', "-").replace('.', "-");
        let session_dir = home.join(format!(".claude/projects/{}", encoded));
        if !session_dir.is_dir() {
            return Vec::new();
        }
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&session_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        files.push(stem.to_string());
                    }
                }
            }
        }
        files
    }

    /// Detect a new session_id by finding .jsonl files that weren't in the existing list.
    /// Returns the newest new file's stem.
    pub(crate) fn detect_session_id(worktree_path: &Path, existing_files: &[String]) -> Option<String> {
        let home = dirs::home_dir()?;
        let path_str = worktree_path.to_str()?;
        let encoded = path_str.replace('/', "-").replace('.', "-");
        let session_dir = home.join(format!(".claude/projects/{}", encoded));
        if !session_dir.is_dir() {
            return None;
        }
        let mut newest: Option<(std::time::SystemTime, String)> = None;
        for entry in std::fs::read_dir(&session_dir).ok()?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if !existing_files.contains(&stem.to_string()) {
                        if let Ok(meta) = entry.metadata() {
                            if let Ok(modified) = meta.modified() {
                                if newest.as_ref().map_or(true, |(t, _)| modified > *t) {
                                    newest = Some((modified, stem.to_string()));
                                }
                            }
                        }
                    }
                }
            }
        }
        newest.map(|(_, id)| id)
    }

    /// List codex session IDs (UUIDs) that were started in the given worktree.
    pub(crate) fn list_codex_sessions(worktree_path: &Path) -> Vec<String> {
        Self::list_codex_sessions_with_mtime(worktree_path)
            .into_iter()
            .map(|(_, id)| id)
            .collect()
    }

    fn list_codex_sessions_with_mtime(worktree_path: &Path) -> Vec<(SystemTime, String)> {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return Vec::new(),
        };
        let sessions_dir = home.join(".codex/sessions");
        if !sessions_dir.is_dir() {
            return Vec::new();
        }
        let wt_str = match worktree_path.to_str() {
            Some(s) => s.to_string(),
            None => return Vec::new(),
        };
        let mut ids = Vec::new();
        Self::walk_codex_sessions(&sessions_dir, &wt_str, &mut ids);
        ids
    }

    fn walk_codex_sessions(dir: &Path, wt_str: &str, ids: &mut Vec<(SystemTime, String)>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::walk_codex_sessions(&path, wt_str, ids);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                // Read just the first line — the JSONL files grow into the
                // megabytes and there are hundreds of them.
                let Some(first) = workflow::transcript::read_first_line(&path) else { continue };
                let Ok(val) = serde_json::from_str::<serde_json::Value>(first.trim()) else { continue };
                if val.pointer("/payload/cwd").and_then(|v| v.as_str()) != Some(wt_str) {
                    continue;
                }
                if let Some(id) = val.pointer("/payload/id").and_then(|v| v.as_str()) {
                    let modified = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(UNIX_EPOCH);
                    ids.push((modified, id.to_string()));
                }
            }
        }
    }

    /// Detect a new codex session_id by comparing against known IDs. Uses the
    /// user's default codex home.
    pub(crate) fn detect_codex_session_id(worktree_path: &Path, existing_ids: &[String]) -> Option<String> {
        Self::list_codex_sessions_with_mtime(worktree_path)
            .into_iter()
            .filter(|(_, id)| !existing_ids.contains(id))
            .max_by_key(|(modified, _)| *modified)
            .map(|(_, id)| id)
    }

    /// Reopen a past workspace by id: flip `is_closed` back to false and
    /// PATCH any bound done tasks back to `running` so the workspace
    /// re-enters the active sidebar. Refuses gracefully when the worktree
    /// directory is gone (manually deleted or `git worktree remove`'d).
    /// Returns true on success — callers in modal mode use it to close
    /// the picker only when the reopen actually went through.
    pub(super) fn reopen_workspace_by_id(&mut self, ws_id: &str) -> bool {
        let Some(wi) = self.workspaces.iter().position(|w| w.id == ws_id) else {
            self.set_status_msg("Workspace no longer in manifest");
            return false;
        };
        let worktree_path = self.workspaces[wi].worktree_path.clone();
        match worktree_path.as_deref() {
            Some(p) if p.exists() => {}
            Some(p) => {
                self.set_status_msg(&format!(
                    "Worktree gone: {} — can't reopen",
                    p.display()
                ));
                return false;
            }
            None => {
                self.set_status_msg("Workspace has no worktree to reopen");
                return false;
            }
        }

        self.workspaces[wi].is_closed = false;

        let bound_done: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| t.workspace_id.as_deref() == Some(ws_id))
            .filter(|t| matches!(t.api_status, TaskStatus::Done))
            .filter_map(|t| t.task_id.clone())
            .collect();
        for tid in &bound_done {
            let mut fields = HashMap::new();
            fields.insert(
                "status".to_string(),
                serde_json::Value::String("running".to_string()),
            );
            self.backend.update_task(tid.clone(), fields);
        }
        for tid in &bound_done {
            if let Some(entry) = self
                .tasks
                .iter_mut()
                .find(|t| t.task_id.as_deref() == Some(tid.as_str()))
            {
                entry.api_status = TaskStatus::Running;
            }
            self.planning.mark_task_running_by_id(tid);
        }

        let respawned = self.resurrect_designer_sessions_for_workspace(wi);

        self.save_session_manifest();
        self.cursor = Cursor::Workspace(wi);
        self.clamp_cursor();

        // Designer sessions tagged by `metadata.resume.designer_session_uid`
        // were already auto-resurrected above. If any tombstones remain
        // (workflow participants, ad-hoc sessions, etc.), offer to restore
        // them via the confirm dialog. Task status / workspace_id are
        // already wired up by this point.
        let tombstone_count = self.workspaces[wi].tombstones.len();
        if tombstone_count > 0 {
            self.input_mode = InputMode::Confirm {
                prompt: format!(
                    "Restore {} closed session{} in this workspace?",
                    tombstone_count,
                    if tombstone_count == 1 { "" } else { "s" },
                ),
                action: ConfirmAction::RestoreTombstones {
                    ws_id: ws_id.to_string(),
                },
            };
        } else if respawned > 0 {
            self.set_status_msg(&format!(
                "Workspace reopened — resurrected {} designer session{}",
                respawned,
                if respawned == 1 { "" } else { "s" },
            ));
        } else {
            self.set_status_msg("Workspace reopened — A-s to add session");
        }
        true
    }

    /// Respawn one PTY per tombstone in the named workspace. Claude/Codex
    /// sessions are revived via `--resume <transcript_id>`; bash starts
    /// fresh in the worktree (no transcript to resume). Tombstones that
    /// successfully spawn are consumed from the workspace; failures stay
    /// in the list and surface in the status bar so the user can retry.
    pub(super) fn restore_tombstones_for_workspace(&mut self, ws_id: &str) {
        let Some(wi) = self.workspaces.iter().position(|w| w.id == ws_id) else {
            self.set_status_msg("Workspace no longer exists");
            return;
        };
        if self.workspaces[wi].tombstones.is_empty() {
            return;
        }
        let (cols, rows) = self.last_term_size;
        let worktree = self.workspaces[wi].worktree_path.clone();
        let workspace_id_owned = self.workspaces[wi].id.clone();
        // Restore INTO this workspace → use the workspace's host (not the
        // global active_host, which could mismatch the worktree's host).
        let active_host = self.workspaces[wi].host_id.clone();
        // migrate-tui-local Issue C: the daemon-routed spawn
        // below sends local-only paths (worktree + per-session
        // MCP config under `~/.cm/mcp/...`). A non-local active
        // host would route those at a daemon that can't read
        // them. Fail fast with the shared helper — same shape
        // every other entry point uses.
        if let Err(e) = guard_local_host_only(
            &active_host,
            "A-O restore-tombstones-for-workspace",
        ) {
            self.set_status_msg(&format!("{}", e));
            return;
        }

        // Move tombstones out so the spawn loop can call &mut self helpers
        // without aliasing through `self.workspaces[wi]`.
        let tombstones: Vec<SessionTombstone> =
            std::mem::take(&mut self.workspaces[wi].tombstones);
        let total = tombstones.len();
        let mut restored = 0;
        let mut failed: Vec<SessionTombstone> = Vec::new();

        for tomb in tombstones {
            let session_uid = new_session_uid();
            let result = match tomb.session_type.as_str() {
                "claude" | "codex" | "bash" => {
                    // migrate-tui-local: route restored claude/codex/bash
                    // tombstones through the daemon. The `--resume
                    // <transcript_id>` arg is threaded as
                    // resume_session_id so daemon's start_session
                    // registers the session with the resumed
                    // transcript bound from spawn time — no rebind
                    // dance, no /resume-inside-PTY workaround.
                    //
                    // migrate-tui-local Issue H: bash joins the
                    // daemon-routed arm. A-s spawns bash daemon-
                    // owned; pre-fix the tombstone restore fell
                    // back to `Session::new("/bin/bash", ...)`
                    // and produced a local TUI-owned session,
                    // breaking the "every local session is
                    // daemon-owned" invariant and potentially
                    // colliding with a still-live daemon bash
                    // session under the same UID. Bash carries
                    // no transcript, so `resume` and
                    // `pre_spawn_transcript` resolve to None
                    // naturally.
                    let resume = tomb.last_transcript_id.as_deref();
                    let session_type = tomb.session_type.as_str();
                    let Some(wt_path) = worktree.as_deref() else {
                        self.set_status_msg(
                            "Restore failed: workspace has no worktree",
                        );
                        failed.push(tomb);
                        continue;
                    };
                    // migrate-tui-local Issue 3: claude tombstone
                    // restores have a deterministic transcript
                    // path (worktree + transcript_id); hand it
                    // to the daemon up front so MCP reads serve
                    // immediately. Codex resumes get None
                    // (post-spawn detector handles rebind).
                    let pre_spawn_transcript = resume.and_then(|sid| {
                        pre_spawn_transcript_path(session_type, wt_path, sid)
                    });
                    // migrate-tui-local Issue K: the label arg
                    // is what the daemon stores on
                    // `DaemonSession.title` and what MCP
                    // `list_sessions` surfaces. Pre-fix this
                    // passed `session_type` (e.g. "claude" /
                    // "codex" / "bash"), clobbering the user-
                    // visible label the tombstone carried (e.g.
                    // "reviewer", "planner"). The session_type
                    // belongs in the slot immediately above
                    // (which is the daemon's type field); the
                    // label slot takes `&tomb.label`.
                    match self.try_spawn_via_daemon(
                        &session_uid,
                        &workspace_id_owned,
                        wt_path,
                        session_type,
                        &tomb.label,
                        resume,
                        cols,
                        rows,
                        tomb.task_id.as_deref(),
                        None,
                        None,
                        &active_host,
                        pre_spawn_transcript.as_deref(),
                    ) {
                        Some(Ok(s)) => Ok(s),
                        Some(Err(e)) => {
                            self.set_status_msg(&format!(
                                "Restore failed (daemon spawn): {}",
                                e
                            ));
                            failed.push(tomb);
                            continue;
                        }
                        None => {
                            self.set_status_msg(
                                "Internal: try_spawn_via_daemon returned None for daemon-eligible type",
                            );
                            failed.push(tomb);
                            continue;
                        }
                    }
                }
                _ => Session::new(
                    "/bin/bash",
                    &[],
                    cols,
                    rows,
                    worktree.clone(),
                    Default::default(),
                    None,
                ),
            };

            match result {
                Ok(s) => {
                    let pending = match tomb.session_type.as_str() {
                        "claude" => worktree.as_ref().map(|p| Self::list_jsonl_files(p)),
                        "codex" => worktree.as_ref().map(|p| Self::list_codex_sessions(p)),
                        _ => None,
                    };
                    let mut ts = make_simple_session_with_uid(
                        session_uid,
                        &tomb.label,
                        &tomb.session_type,
                        s,
                        pending,
                    );
                    ts.task_id = tomb.task_id.clone();
                    // migrate-tui-local: claude/codex sessions are
                    // daemon-owned now; pin host to the snapshot
                    // taken at the top of this method. Bash bypass
                    // also tags local (which is what active_host
                    // is in non-cloud reopen flows).
                    ts.host_id = active_host.clone();
                    // For Claude `--resume` keeps writing to the same JSONL,
                    // so the transcript id IS the live id immediately. For
                    // Codex the live id is rebound by the detector when the
                    // post-resume rollout appears in `pending_jsonl_files`.
                    if tomb.session_type == "claude" {
                        ts.transcript_id = tomb.last_transcript_id.clone();
                    }
                    self.workspaces[wi].sessions.push(ts);
                    restored += 1;
                }
                Err(e) => {
                    self.set_status_msg(&format!("Restore failed: {}", e));
                    failed.push(tomb);
                }
            }
        }

        // Put any failures back so the user can A-O again later.
        self.workspaces[wi].tombstones.extend(failed);
        self.save_session_manifest();
        self.cursor = Cursor::Workspace(wi);
        self.clamp_cursor();
        self.set_status_msg(&format!(
            "Restored {}/{} session{}",
            restored,
            total,
            if total == 1 { "" } else { "s" },
        ));
    }

    /// Soft-close the workspace under the cursor: kill its session PTYs
    /// and hide from the sidebar. Worktree stays on disk; bindings persist.
    /// Each closed session leaves behind a `SessionTombstone` so the
    /// resolver can still answer `read_session_output` for it.
    pub(super) fn close_active_workspace(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };
        // Tombstone every session before dropping. Helper persists the
        // manifest as a side-effect so a TUI crash mid-close doesn't
        // resurrect tombstoned sessions.
        self.tombstone_and_remove(wi, |_| true);
        if let Some(ws) = self.workspaces.get_mut(wi) {
            ws.is_closed = true;
        }
        // Persist again in case `is_closed` flipped after the helper
        // already saved (cheap, just rewrites the same JSON).
        self.save_session_manifest();
        if let Some((nwi, _)) = self
            .workspaces
            .iter()
            .enumerate()
            .find(|(_, w)| !w.is_closed)
        {
            self.cursor = Cursor::Workspace(nwi);
        }
        self.clamp_cursor();
        self.set_status_msg("Workspace closed");
    }

    /// Resurrect tombstoned sessions referenced by a bound task's
    /// `metadata.resume.designer_session_uid`. Generic by design — any
    /// skill that stashes a session uid under that key gets the same
    /// behavior on workspace reopen. First and currently only caller is
    /// the design-doc bundle (skill writes the uid; reopen brings the
    /// session back as a live `claude --resume <transcript_id>`).
    ///
    /// Returns the number of tombstones successfully respawned so
    /// callers can include it in status messages. Failures (missing
    /// transcript id, spawn error, unsupported session type) leave the
    /// tombstone in place so a subsequent reopen can retry.
    fn resurrect_designer_sessions_for_workspace(&mut self, wi: usize) -> usize {
        if wi >= self.workspaces.len() {
            return 0;
        }
        let ws_id = self.workspaces[wi].id.clone();
        let target_uids: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| t.workspace_id.as_deref() == Some(ws_id.as_str()))
            .filter_map(|t| {
                t.metadata
                    .as_ref()
                    .and_then(|m| m.get("resume"))
                    .and_then(|r| r.get("designer_session_uid"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect();
        if target_uids.is_empty() {
            return 0;
        }

        let (cols, rows) = self.last_term_size;
        // Resurrect INTO this workspace → use the workspace's host (not the
        // global active_host).
        let active_host = self.workspaces[wi].host_id.clone();
        // migrate-tui-local Issue C: the resurrect path resolves a
        // claude transcript path on the local filesystem and
        // hands it (plus the workspace's worktree) to the daemon.
        // Fail fast on a non-local active host so we don't send
        // local-only paths to a remote daemon. Matches the
        // canonical guard pattern used by A-n / A-s / A-l.
        if let Err(e) = guard_local_host_only(
            &active_host,
            "designer-session resurrect",
        ) {
            self.set_status_msg(&format!("{}", e));
            return 0;
        }
        let workspace_id_owned = self.workspaces[wi].id.clone();
        let mut respawned = 0usize;
        for uid in target_uids {
            // Skip if the uid is already live (e.g. a previous resurrect
            // hop already brought it back) — never spawn a duplicate.
            if self.workspaces[wi]
                .sessions
                .iter()
                .any(|s| s.uid == uid)
            {
                continue;
            }
            let Some(ti) = self.workspaces[wi]
                .tombstones
                .iter()
                .position(|t| t.uid == uid)
            else {
                continue;
            };
            let tomb = &self.workspaces[wi].tombstones[ti];
            if tomb.session_type != "claude" {
                continue;
            }
            let Some(transcript_id) = tomb.last_transcript_id.clone() else {
                continue;
            };
            let worktree_path = tomb
                .worktree_path
                .clone()
                .or_else(|| self.workspaces[wi].worktree_path.clone());
            let label = tomb.label.clone();
            let task_id = tomb.task_id.clone();

            let Some(wt_path) = worktree_path.as_deref() else {
                // No worktree — can't daemon-spawn. Leave tombstone
                // for retry on next reopen.
                continue;
            };

            // migrate-tui-local: route the resume through the
            // daemon RPC with --resume <transcript_id> threaded as
            // resume_session_id. The daemon registers the session
            // with the resumed transcript bound from spawn time.
            //
            // migrate-tui-local Issue 3: hand the deterministic
            // claude transcript path to the daemon up front.
            let pre_spawn_transcript = pre_spawn_transcript_path(
                "claude",
                wt_path,
                transcript_id.as_str(),
            );
            match self.try_spawn_via_daemon(
                &uid,
                &workspace_id_owned,
                wt_path,
                "claude",
                &label,
                Some(transcript_id.as_str()),
                cols,
                rows,
                task_id.as_deref(),
                None,
                None,
                &active_host,
                pre_spawn_transcript.as_deref(),
            ) {
                Some(Ok(s)) => {
                    let mut ts = make_simple_session_with_uid(
                        uid.clone(),
                        &label,
                        "claude",
                        s,
                        None,
                    );
                    ts.transcript_id = Some(transcript_id);
                    ts.task_id = task_id;
                    ts.host_id = active_host.clone();
                    self.workspaces[wi].sessions.push(ts);
                    self.workspaces[wi].tombstones.remove(ti);
                    respawned += 1;
                }
                Some(Err(_)) | None => {
                    // Spawn failed (or daemon returned None for an
                    // unexpected reason) — leave the tombstone in
                    // place so the user can retry.
                }
            }
        }
        if respawned > 0 {
            self.save_session_manifest();
        }
        respawned
    }

    pub(super) fn toggle_session_hidden(&mut self) {
        let (wi, si) = match &self.cursor {
            Cursor::Session(wi, si) => (*wi, *si),
            Cursor::Workspace(wi) => {
                let wi = *wi;
                if self.workspaces.get(wi).map_or(false, |w| w.sessions.len() == 1) {
                    (wi, 0)
                } else {
                    return;
                }
            }
            Cursor::Task { ws_idx, task_id } => {
                // Toggle hidden on every session belonging to the task.
                // Uses the majority-hidden state as the "current" so one
                // keypress always flips everything in unison.
                let wi = *ws_idx;
                let tid = task_id.clone();
                let Some(ws) = self.workspaces.get_mut(wi) else {
                    return;
                };
                let matching: Vec<&mut TerminalSession> = ws
                    .sessions
                    .iter_mut()
                    .filter(|ts| ts.task_id.as_deref() == Some(tid.as_str()))
                    .collect();
                if matching.is_empty() {
                    return;
                }
                let hidden_count = matching.iter().filter(|ts| ts.hidden).count();
                let new_hidden = hidden_count * 2 < matching.len();
                for ts in matching {
                    ts.hidden = new_hidden;
                }
                self.save_session_manifest();
                self.needs_redraw = true;
                return;
            }
        };
        if let Some(ts) = self
            .workspaces
            .get_mut(wi)
            .and_then(|w| w.sessions.get_mut(si))
        {
            ts.hidden = !ts.hidden;
            self.save_session_manifest();
            self.needs_redraw = true;
        }
    }

    /// Enter input mode to create a new workspace (empty, no task binding).
    pub(super) fn start_new_session(&mut self) {
        // Seed with the first repo from config, sorted by name so the picker
        // is deterministic. ←/→ cycles through the rest.
        let repo_url = match sorted_repo_urls(&self.config.repos).first() {
            Some(url) => url.clone(),
            None => {
                self.set_status_msg("No repos configured");
                return;
            }
        };

        self.input_mode = InputMode::NewSession {
            label_text: String::new(),
            branch_text: String::new(),
            idle_timeout_text: DEFAULT_IDLE_TIMEOUT_SECS.to_string(),
            repo_url,
            seed_from: None,
            // Default the host to `local` (the overwhelmingly common case)
            // rather than a global mode the operator has to remember to set —
            // the global active_host is retired (DESIGN_REMOVE_GLOBAL_HOST.md).
            // ←/→ on the host field still picks any configured host per-task.
            host_id: cm_daemon::host_id::HostId::local(),
            active_field: 0,
        };
    }

    /// Enter input mode to add a terminal session to the active workspace.
    /// If the cursor is inside a task scope, the new session inherits that
    /// task_id so it appears under the task subheader.
    pub(super) fn start_new_terminal_session(&mut self) {
        let wi = match self.active_workspace_index() {
            Some(wi) => wi,
            None => {
                self.set_status_msg("No workspace selected");
                return;
            }
        };
        // A push in flight will tombstone every live session on the
        // workspace when `PushComplete` lands, so a session added now
        // would silently disappear seconds later — confusing enough
        // that we bounce the user with an explicit message instead.
        if self.workspaces[wi].is_pushing {
            self.set_status_msg("Workspace is being pushed to cloud, retry after");
            return;
        }
        let task_id = self.cursor_task_id();
        // Capture workspace_id (stable) instead of the index — backend
        // events fired while the form is open can reorder workspaces,
        // and a stored index would silently target the wrong workspace
        // by submit time.
        let workspace_id = self.workspaces[wi].id.clone();
        self.input_mode = InputMode::NewTerminalSession {
            workspace_id,
            session_type: "claude".to_string(),
            task_id,
            seed_from: None,
            active_field: 0,
        };
    }

    /// Public wrapper exposed to the control-socket method handlers
    /// (which live in `crate::control::methods`).
    pub(crate) fn tombstone_session_pub(ws: &mut Workspace, si: usize) {
        Self::tombstone_session(ws, si);
    }

    /// 12e: route through `host_pool.for_host(&ts.host_id)` so a
    /// session pinned to a non-default host (created via `A-H`
    /// + `A-n`) gets its kill RPC fired against the right
    /// daemon. Pre-12e this dialed `cm_daemon::default_socket_path()`
    /// — fine for local-only but wrong the moment 12e ships
    /// multi-host UX.
    pub(crate) fn kill_daemon_session_if_attached(
        host_pool: &crate::host_pool::HostPool,
        ts: &TerminalSession,
    ) {
        if let Some(uid) = ts.session.daemon_session_uid.as_deref() {
            let socket = match host_pool.for_host(&ts.host_id) {
                Ok(h) => match h.socket_path() {
                    Some(p) => p,
                    None => {
                        eprintln!(
                            "cm-tui: A-w kill_session({}) skipped — \
                             host_pool for {} has no live socket path",
                            uid,
                            ts.host_id.as_str(),
                        );
                        return;
                    }
                },
                Err(e) => {
                    eprintln!(
                        "cm-tui: A-w kill_session({}) skipped — \
                         host_pool.for_host({}) failed: {}",
                        uid,
                        ts.host_id.as_str(),
                        e,
                    );
                    return;
                }
            };
            if let Err(e) = crate::client_session::rpc_kill_session(
                &socket,
                crate::daemon_launch::operator_token(),
                uid,
            ) {
                eprintln!(
                    "cm-tui: A-w kill_session({}) failed: {} \
                     (orphan child will be reaped when it exits)",
                    uid, e,
                );
            }
        }
    }

    /// Sub-2b-1 (review #1): push the resolved transcript path
    /// to the daemon when the TUI's detector binds (or rebinds)
    /// `ts.transcript_id` for a daemon-attached session. Without
    /// this, the daemon's `resolve_authorized_session` returns
    /// `state: "pending"` forever — the wire would tell the
    /// Python MCP `read_session_output` tool to poll, and the
    /// tool would never see a transcript.
    ///
    /// **No-ops when**:
    ///   - opt-in is off (no daemon to push to).
    ///   - session is not daemon-attached (`daemon_session_uid`
    ///     is `None` → the TUI's own `resolve_authorized_session`
    ///     serves the resolver leg).
    ///   - the agent module can't resolve a path (rare; e.g.
    ///     transcript_id became invalid or the agent type has
    ///     no transcript like bash).
    ///
    /// Called from every site that sets `ts.transcript_id` to
    /// `Some`. Re-pushing on rebind (`/clear`, codex-resume) is
    /// intentional — the daemon stores the latest value.
    /// Best-effort: log on RPC error and continue (the next
    /// rebind retries).
    pub(crate) fn push_transcript_path_to_daemon_if_attached(
        host_pool: &crate::host_pool::HostPool,
        ts: &TerminalSession,
        ws: &Workspace,
    ) {
        let Some(daemon_uid) = ts.session.daemon_session_uid.as_deref() else {
            return;
        };
        // 10f: daemon-mandatory; no opt-in gate. `daemon_session_uid`
        // being Some already implies a daemon-spawned session.
        // Resolve path via the engine-specific agent module
        // (the TUI's source of truth for Claude/Codex conventions).
        let Some(wt) = ws.worktree_path.as_deref() else {
            return;
        };
        let agent = crate::agent::agent_for(&ts.session_type);
        let ctx = crate::agent::AgentCtx { ts, worktree_path: wt };
        let Some(path) = agent.transcript_path(ctx) else {
            return;
        };
        let path_str = path.to_string_lossy().to_string();
        // 12e: route through `host_pool.for_host(&ts.host_id)`
        // so a session on a non-default host pushes its
        // transcript path to the right daemon.
        let socket = match host_pool.for_host(&ts.host_id) {
            Ok(h) => match h.socket_path() {
                Some(p) => p,
                None => {
                    eprintln!(
                        "cm-tui: set_transcript_path({}) skipped — \
                         host_pool for {} has no live socket path",
                        daemon_uid,
                        ts.host_id.as_str(),
                    );
                    return;
                }
            },
            Err(e) => {
                eprintln!(
                    "cm-tui: set_transcript_path({}) skipped — \
                     host_pool.for_host({}) failed: {}",
                    daemon_uid,
                    ts.host_id.as_str(),
                    e,
                );
                return;
            }
        };
        if let Err(e) = crate::client_session::rpc_set_transcript_path(
            &socket,
            crate::daemon_launch::operator_token(),
            daemon_uid,
            &path_str,
        ) {
            eprintln!(
                "cm-tui: session.set_transcript_path({}, {}) failed: {} \
                 (daemon's resolve_authorized_session will stay pending \
                 until the next rebind retries)",
                daemon_uid, path_str, e,
            );
        }
    }

    /// Sub-2a Finding #1: full-replace task tree push to the
    /// daemon. Called after every `self.tasks` mutation so the
    /// daemon's `DaemonState.task_tree` stays current for the
    /// Session-caller descendant-task auth walk.
    ///
    /// Gated on `CM_USE_DAEMON_SOCKET=1`. With opt-in off the
    /// daemon isn't running and a connect attempt would just
    /// log noise; skip the RPC entirely. With opt-in on the
    /// daemon was launched at startup (see `main.rs:60`), so a
    /// connect failure here is a real fault — log it and
    /// continue (the next push will retry; auth meanwhile
    /// falls back to the pre-push tree).
    ///
    /// Full-replace semantics: the daemon's `task_update_tree`
    /// method clears + re-inserts on every call. Cheaper than
    /// computing diffs in the TUI and avoids drift if a single
    /// incremental push is lost.
    /// 10d-1: push the TUI's session snapshot to the daemon so
    /// the daemon recognizes TUI-minted sessions. Lands in
    /// `daemon::state::DaemonState::tui_sessions` via
    /// `tui.update_sessions_snapshot`. Full-replace semantics
    /// (replace-not-merge), same shape as
    /// [`push_task_tree_to_daemon`].
    ///
    /// **No auth consumer yet**: 10d-1 lands the push + storage.
    /// The workflow-method auth consumer in 10d-2 reads from
    /// `state.tui_sessions`; without that push wired here, 10d-2
    /// would have nothing to read.
    /// Build a per-host owned snapshot of TUI sessions and hand
    /// it off to the background `push_worker`. Main-thread cost:
    /// one clone of each session's small strings + an mpsc
    /// `send`. The fanout (per-host dial, RPC, reachability cache
    /// updates) happens on the worker thread — keystroke handling
    /// no longer waits on network RTT.
    pub(crate) fn push_tui_sessions_to_daemon(&self) {
        // Pre-seed each configured host with an empty vec so an
        // empty snapshot still produces a (host, []) entry and
        // clears the daemon's `tui_sessions` map on the
        // worker-side full-replace.
        let mut per_host: HashMap<
            cm_daemon::host_id::HostId,
            Vec<crate::push_worker::TuiSessionRow>,
        > = HashMap::new();
        for host in &self.hosts.hosts {
            per_host.insert(host.id.clone(), Vec::new());
        }
        // Filter out daemon-attached sessions (`daemon_session_uid.is_some()`):
        // those already live in `state.sessions` on the daemon and would
        // double-register if we also pushed them to `state.tui_sessions`.
        // Bucket each remaining session by its pinned `host_id` so each
        // daemon only hears about sessions it actually owns (12e-r8 F2).
        for w in &self.workspaces {
            for ts in &w.sessions {
                if ts.session.daemon_session_uid.is_some() {
                    continue;
                }
                if let Some(bucket) = per_host.get_mut(&ts.host_id) {
                    bucket.push(crate::push_worker::TuiSessionRow {
                        uid: ts.uid.clone(),
                        task_id: ts.task_id.clone(),
                        label: Some(ts.label.clone()),
                        session_type: Some(ts.session_type.clone()),
                        hidden: ts.hidden,
                        workflow_run_id: ts.workflow_run_id.clone(),
                        workflow_role: ts.workflow_role.clone(),
                        global_perms: ts.global_perms,
                    });
                }
            }
        }
        self.push_worker.push_tui_sessions(per_host);
    }

    /// 10d-1: unified state-snapshot push. Sites that mutate
    /// EITHER the task tree OR the session list should call
    /// this single helper. Pre-10d-1 those sites called
    /// `push_task_tree_to_daemon` directly; replacing with
    /// `push_state_to_daemon` means session-list mutations
    /// (add, remove, hide, label, task rebind) automatically
    /// keep the daemon's TUI-session view current.
    pub(crate) fn push_state_to_daemon(&self) {
        self.push_task_tree_to_daemon();
        self.push_tui_sessions_to_daemon();
        // 10d-2c-2-1: workflow definitions are static after TOML
        // load, but bundling the push here is the simplest way to
        // ensure they reach the daemon at least once during the
        // normal startup chain (App::new → first mutation →
        // push_state). The re-push cost is a small JSON object
        // over a local UDS — cheaper than wiring a one-shot at
        // startup-complete.
        self.push_workflow_definitions_to_daemon();
    }

    /// 10d-1 graceful-shutdown clear (now a no-op, post-review
    /// finding #14): pushing an empty snapshot at TUI shutdown
    /// locked out any TUI-local workflow participant whose PTY
    /// outlived the TUI (Session::Drop is detach-only — the
    /// child PTY survives until `kill_session` or natural exit).
    /// The orphan agent's MCP `workflow_transition` call then
    /// hit daemon-side `lookup_session_any` with an empty
    /// `tui_sessions` map and got Unauthorized mid-flight.
    ///
    /// The stale-rows concern that motivated the original clear
    /// is bounded the same way the crash case always was: the
    /// next TUI restart's first `reconcile_tasks` /
    /// `restore_sessions` push REPLACES (not merges) the
    /// snapshot. Any rows from the prior TUI session disappear
    /// the moment the next TUI launches. Until then, the rows
    /// are harmless to readers that gate on
    /// `state.sessions.contains_key(uid)` first (the daemon's
    /// dispatch paths do), and useful to the workflow-auth path
    /// for orphaned participants that need to keep transitioning.
    pub(crate) fn clear_tui_sessions_on_daemon(&self) {
        // Intentional no-op. See doc comment.
    }

    /// 10d-2c-2-1: push the in-memory workflow-definitions map
    /// (loaded from `workflows/*.toml`) to the daemon. Opt-in
    /// gated on `CM_USE_DAEMON_SOCKET` — same gate as the other
    /// daemon-side pushes. Errors are logged-and-continued so a
    /// daemon hiccup doesn't kill TUI startup.
    ///
    /// Called once at `App::new`, immediately after the TOML load.
    /// Workflow definitions are static after launch, so a single
    /// startup push is sufficient; the upcoming 2c-2-2 daemon
    /// driver reads from `DaemonState.workflow_definitions`.
    /// Clone the in-memory workflow-definitions map and hand off
    /// to the background `push_worker`. Static after TOML load;
    /// the worker's dedup will short-circuit re-pushes after the
    /// startup propagation.
    pub(crate) fn push_workflow_definitions_to_daemon(&self) {
        let hosts: Vec<cm_daemon::host_id::HostId> =
            self.hosts.hosts.iter().map(|h| h.id.clone()).collect();
        self.push_worker.push_workflow_defs(self.workflows.clone(), hosts);
    }

    /// Build owned task-tree + workspaces vecs and hand off to
    /// the background `push_worker`. Was the main source of the
    /// 5s-tick lag spike (reconcile_tasks fires this on every
    /// `TasksUpdated` event); now non-blocking on the main thread.
    pub(crate) fn push_task_tree_to_daemon(&self) {
        let tasks: Vec<(String, Option<String>, Option<String>)> = self
            .tasks
            .iter()
            .filter_map(|t| {
                t.task_id.as_ref().map(|id| {
                    (id.clone(), t.parent_task_id.clone(), t.workspace_id.clone())
                })
            })
            .collect();
        let workspaces: Vec<(String, Option<String>)> = self
            .workspaces
            .iter()
            .map(|w| {
                (
                    w.id.clone(),
                    w.worktree_path
                        .as_ref()
                        .map(|p| p.display().to_string()),
                )
            })
            .collect();
        let hosts: Vec<cm_daemon::host_id::HostId> =
            self.hosts.hosts.iter().map(|h| h.id.clone()).collect();
        self.push_worker.push_task_tree(tasks, workspaces, hosts);
    }

    /// Bulk session removal that preserves the tombstone invariant.
    /// Walks `ws.sessions`, tombstones each entry where `should_drop`
    /// returns true, marks the PTY exited, and removes it. Use this
    /// instead of `ws.sessions.retain(...)` or `ws.sessions.clear()` —
    /// otherwise `read_session_output` for the closed sessions returns
    /// `not_found` instead of `state: "exited"`.
    ///
    /// **Persists the manifest before returning** when anything was
    /// removed. This is deliberate — every previous round of review
    /// found another caller that forgot to persist, breaking Phase 2b
    /// across TUI crashes. Pushing the save into the helper makes it
    /// impossible to forget. Callers can ignore the return value if
    /// they don't need the count; the persist is unconditional.
    pub(crate) fn tombstone_and_remove(
        &mut self,
        ws_index: usize,
        mut should_drop: impl FnMut(&TerminalSession) -> bool,
    ) -> usize {
        // 12e: snapshot the pool Arc so we can call
        // `kill_daemon_session_if_attached` while `ws` holds a
        // mutable borrow of `self.workspaces`.
        let pool = std::sync::Arc::clone(&self.host_pool);
        // Collected here so reconnect bookkeeping can be cleared AFTER the
        // `&mut self.workspaces` borrow ends (forget_reconnect_state needs
        // `&mut self`).
        let mut removed_uids: Vec<String> = Vec::new();
        {
            let Some(ws) = self.workspaces.get_mut(ws_index) else {
                return 0;
            };
            let mut i = 0;
            while i < ws.sessions.len() {
                if should_drop(&ws.sessions[i]) {
                    // Slice 10c-e-3b-fix2: operator-driven kill
                    // before drop. See `kill_daemon_session_if_attached`
                    // for rationale. Bulk-cleanup paths (task close,
                    // workspace teardown) flow through here too.
                    Self::kill_daemon_session_if_attached(&pool, &ws.sessions[i]);
                    Self::tombstone_session(ws, i);
                    ws.sessions[i].session.exited = true;
                    removed_uids.push(ws.sessions[i].uid.clone());
                    ws.sessions.remove(i);
                } else {
                    i += 1;
                }
            }
        }
        // Cancel reconnect/reattach for every removed session so a
        // closed-while-offline remote session isn't resurrected on reconnect.
        for uid in &removed_uids {
            self.forget_reconnect_state(uid);
        }
        let removed = removed_uids.len();
        if removed > 0 {
            self.save_session_manifest();
        }
        removed
    }

    /// Build a tombstone from `ws.sessions[si]` and push it onto
    /// `ws.tombstones`. Doesn't remove the session — caller does that
    /// to keep the borrow flow simple. Snapshots the workspace's
    /// `worktree_path` into the tombstone so post-close mutations of
    /// the workspace (e.g. `push_active` clearing the path) don't
    /// silently break `read_session_output`.
    fn tombstone_session(ws: &mut Workspace, si: usize) {
        let Some(ts) = ws.sessions.get(si) else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let worktree_snapshot = ws.worktree_path.clone();
        ws.tombstones.push(SessionTombstone {
            uid: ts.uid.clone(),
            managed_by_uid: ts.managed_by_uid.clone(),
            label: ts.label.clone(),
            session_type: ts.session_type.clone(),
            task_id: ts.task_id.clone(),
            last_transcript_id: ts.transcript_id.clone(),
            worktree_path: worktree_snapshot,
            generation: ts.generation,
            exited_at: now,
        });
    }

    /// Close the current session: extract a `SessionTombstone` from its
    /// metadata, push it onto the workspace's tombstone list, then drop
    /// the live entry (which tears down the PTY). The resolver can still
    /// answer `read_session_output` for the closed session via the
    /// tombstone.
    pub(super) fn close_active_session(&mut self) {
        // 12e: snapshot the pool Arc for the same reason as
        // `tombstone_and_remove` — workspaces takes a mut
        // borrow of self below.
        let pool = std::sync::Arc::clone(&self.host_pool);
        match self.cursor.clone() {
            Cursor::Session(wi, si) => {
                if let Some(ws) = self.workspaces.get_mut(wi) {
                    if si < ws.sessions.len() {
                        // Slice 10c-e-3b-fix2: operator-driven
                        // kill BEFORE drop. Daemon-attached
                        // sessions need an explicit kill_session
                        // RPC because Drop is detach-only by
                        // design.
                        Self::kill_daemon_session_if_attached(&pool, &ws.sessions[si]);
                        Self::tombstone_session(ws, si);
                        let closed_uid = ws.sessions[si].uid.clone();
                        ws.sessions.remove(si);
                        if ws.sessions.is_empty() {
                            self.cursor = Cursor::Workspace(wi);
                        } else {
                            let new_si = si.min(ws.sessions.len() - 1);
                            self.cursor = Cursor::Session(wi, new_si);
                        }
                        // Cancel any reconnect/reattach for the closed session
                        // so it isn't resurrected once the tunnel returns.
                        self.forget_reconnect_state(&closed_uid);
                        self.save_session_manifest();
                        self.set_status_msg("Session closed");
                    }
                }
            }
            Cursor::Workspace(wi) => {
                if let Some(ws) = self.workspaces.get_mut(wi) {
                    if ws.sessions.len() == 1 {
                        // Same operator-kill semantics as the
                        // Session-cursor arm above.
                        Self::kill_daemon_session_if_attached(&pool, &ws.sessions[0]);
                        Self::tombstone_session(ws, 0);
                        let closed_uid = ws.sessions[0].uid.clone();
                        ws.sessions.remove(0);
                        self.cursor = Cursor::Workspace(wi);
                        // Cancel any reconnect/reattach for the closed session.
                        self.forget_reconnect_state(&closed_uid);
                        self.save_session_manifest();
                        self.set_status_msg("Session closed");
                    }
                }
            }
            Cursor::Task { ws_idx, task_id } => {
                // Close every session belonging to the task. The task remains
                // in the sidebar (as an empty subheader) until A-x removes it.
                // Tombstone each so `read_session_output` keeps working.
                if ws_idx < self.workspaces.len() {
                    let target = task_id.clone();
                    let removed = self.tombstone_and_remove(ws_idx, |ts| {
                        ts.task_id.as_deref() == Some(target.as_str())
                    });
                    if removed > 0 {
                        // Helper already saved the manifest.
                        self.set_status_msg(&format!("Closed {} session(s)", removed));
                    }
                }
            }
        }
    }

    /// Create a fresh standalone workspace — A-n flow. No task binding.
    /// Load the named snapshot and materialize it into `worktree_path`'s
    /// expected on-disk locations. Returns the full `ClonedSession` so
    /// the caller can pass `transcript_id` as `resume_session_id` to
    /// `build_args` / `codex_args` AND, if a later step (build_args,
    /// spawn) fails, remove the cloned `transcript_path` to keep retries
    /// unblocked. On clone/load error, toasts and returns `None`.
    ///
    /// **Engine-asymmetric integration** (see ClonedSession rustdoc):
    /// - Claude Code: returned id IS the live transcript id; caller sets
    ///   `ts.transcript_id = Some(id)`, `pending_jsonl_files = None`.
    /// - Codex: returned id is a *resume-source* id only — `codex resume`
    ///   reads our seed file once, then mints a fresh rollout. Caller
    ///   leaves `ts.transcript_id = None` and primes
    ///   `pending_jsonl_files` AFTER this call so the detector picks up
    ///   the new rollout (not the seed file).
    fn clone_snapshot_for_spawn(
        &mut self,
        name: &str,
        engine: Engine,
        worktree_path: &Path,
    ) -> Option<agent_memory::ClonedSession> {
        let snap = match agent_memory::load(name) {
            Ok(s) => s,
            Err(e) => {
                self.set_status_msg(&format!("Snapshot load failed: {e}"));
                return None;
            }
        };
        if snap.manifest.engine != engine {
            // Picker filters by engine but a hand-crafted manifest or a
            // stale form could still reach this branch — refuse rather
            // than producing an incoherent clone.
            self.set_status_msg(
                "Snapshot engine doesn't match this session type",
            );
            return None;
        }
        match agent_memory::clone_into_session(&snap, worktree_path) {
            Ok(cloned) => Some(cloned),
            Err(e) => {
                self.set_status_msg(&format!("Snapshot clone failed: {e}"));
                None
            }
        }
    }

    /// Undo a snapshot clone when a later step (build_args, PTY spawn)
    /// fails. Removes the transcript AND restores every merged memory
    /// file to its pre-clone state — otherwise a subsequent unseeded
    /// Claude session in the same worktree would silently inherit the
    /// snapshot's memory entries (the merge wrote them to disk and only
    /// the transcript would have been removed by a partial cleanup).
    pub(super) fn cleanup_failed_clone(cloned: &agent_memory::ClonedSession) {
        agent_memory::cleanup_clone(cloned);
    }

    pub(super) fn create_local_session(
        &mut self,
        chosen_host: &cm_daemon::host_id::HostId,
        repo_url: &str,
        label: &str,
        start_branch: Option<&str>,
        idle_timeout_secs: u16,
        seed_from: Option<&str>,
        in_place: bool,
    ) {
        // Host-picker (A-n): the host is now an explicit per-A-n choice on the
        // form (defaulting to `active_host`), passed in as `chosen_host` —
        // NOT read from the global `self.active_host`. Snapshot it ONCE here
        // and thread the same value through `try_spawn_via_daemon` AND the
        // TerminalSession.host_id assignment below, so a concurrent A-H cycle
        // can't tag the session with a different host mid-create. (Was the
        // 12e-r2 F1 active_host snapshot before the picker landed.)
        let active_host = chosen_host.clone();
        // Phase 3 (remote-session-execution): a non-local chosen host routes
        // A-n to the daemon-resolved `create_session` path — the daemon makes
        // the worktree and builds argv/env on its OWN filesystem, then the TUI
        // attaches over the host's socket. Local A-n runs the existing path
        // below, unchanged (it always passed the old `guard_local_host_only`,
        // so removing that guard is a no-op for the local branch).
        if active_host != crate::hosts::HostId::local() {
            self.create_remote_session(
                &active_host,
                repo_url,
                label,
                start_branch,
                idle_timeout_secs,
                seed_from,
                in_place,
            );
            return;
        }
        let main_repo = match worktree::find_local_repo(repo_url) {
            Some(p) => p,
            None => {
                self.set_status_msg("Repo not found locally");
                return;
            }
        };

        let slug = worktree::slugify(label);
        if slug.is_empty() {
            self.set_status_msg("Invalid name");
            return;
        }

        // Fail-fast on snapshot load BEFORE touching git. Without this,
        // a non-existent / corrupt snapshot would leave a freshly-created
        // worktree + branch orphaned on disk, and a retry would fail
        // because the worktree path is taken. The later
        // `clone_snapshot_for_spawn` re-validates (load is idempotent
        // and cheap) — this early check just keeps git off the failure
        // path.
        if let Some(name) = seed_from {
            if let Err(e) = validate_seed_loadable(name) {
                self.set_status_msg(&e);
                return;
            }
        }

        // In-place: the working directory IS the main checkout. Skip BOTH
        // `create_worktree` (would mint a `cm/<slug>` branch + worktree dir)
        // AND `setup_worktree` (would re-run `setup_worktree.sh` against the
        // live repo). Cloning `main_repo` keeps `worktree_path` byte-equal to
        // `main_repo_path` so `Workspace::is_in_place()` is true.
        let worktree_path = if in_place {
            main_repo.clone()
        } else {
            match worktree::create_worktree(&main_repo, &slug, start_branch) {
                // `_created` flag (created-vs-reused) is unused on this path.
                Ok((p, _created)) => p,
                Err(e) => {
                    self.set_status_msg(&format!("Worktree: {}", e));
                    return;
                }
            }
        };
        if !in_place {
            worktree::setup_worktree(&main_repo, &worktree_path);
        }

        // If the user picked a snapshot, materialize it into the new
        // worktree's expected paths before we spawn. The returned id is
        // what `claude --resume <id>` reads — for Claude this is also
        // the live transcript id (post-resume Claude keeps writing to
        // the same file), so we set it directly on `ts` below.
        let cloned: Option<agent_memory::ClonedSession> = match seed_from {
            Some(name) => match self.clone_snapshot_for_spawn(
                name,
                Engine::ClaudeCode,
                &worktree_path,
            ) {
                Some(c) => Some(c),
                None => return, // clone failure already toasted
            },
            None => None,
        };

        let (cols, rows) = self.last_term_size;
        // Generate uid first so the MCP config carries the matching
        // CM_TUI_SESSION_ID. A-n sessions are taskless — pass None for
        // workflow meta.
        let session_uid = new_session_uid();
        let cloned_transcript_id = cloned.as_ref().map(|c| c.transcript_id.clone());
        // For a seeded Claude session, the JSONL is already on disk and
        // `--resume` keeps writing to it — there's no "new file" for the
        // detector to find, so leave `pending_jsonl_files = None`
        // (matches the resumed-Claude pattern at app.rs:5512).
        let pending = if cloned.is_some() {
            None
        } else {
            Some(Self::list_jsonl_files(&worktree_path))
        };

        // Slice 10c-e-3: pre-generate the workspace id so the
        // daemon spawn branch can auto-register it on
        // `start_session`. The Workspace struct below picks this
        // same id up — that's what makes the daemon's view of the
        // workspace and the TUI's view share an identity.
        let workspace_id_pre = new_workspace_id();

        // migrate-tui-local Issue 3: seeded clones already
        // have a transcript_id at construction (the clone's
        // id); the JSONL is on disk and claude --resume keeps
        // writing to it, so the path is deterministic and we
        // can hand it to the daemon up front. Plain A-n spawns
        // (no seed) leave transcript_path None — the post-spawn
        // detector handles the fresh transcript.
        let pre_spawn_transcript = cloned_transcript_id.as_deref().and_then(|sid| {
            pre_spawn_transcript_path("claude", &worktree_path, sid)
        });
        let s = match self.try_spawn_via_daemon(
            &session_uid,
            &workspace_id_pre,
            &worktree_path,
            "claude",
            "claude",
            cloned_transcript_id.as_deref(),
            cols,
            rows,
            // A-n / create_local_session is taskless by design
            // (the user creates the workspace before any task
            // is bound). The session is taskless from the
            // daemon's POV too — auth uses the taskless-caller
            // same-workspace branch.
            None,
            // A-n / create_local_session is not a workflow
            // participant at spawn time; if the user later
            // launches a workflow on this session,
            // `rpc_set_workflow_context` updates the daemon copy.
            None,
            None,
            &active_host,
            pre_spawn_transcript.as_deref(),
        ) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                if let Some(c) = cloned.as_ref() {
                    Self::cleanup_failed_clone(c);
                }
                self.set_status_msg(&format!(
                    "Daemon spawn failed: {}",
                    e
                ));
                return;
            }
            None => {
                // Unreachable post-migrate-tui-local: "claude" is
                // daemon-eligible, so try_spawn_via_daemon never
                // returns None here. Surface loudly if it does.
                if let Some(c) = cloned.as_ref() {
                    Self::cleanup_failed_clone(c);
                }
                self.set_status_msg(
                    "Internal: try_spawn_via_daemon returned None for daemon-eligible 'claude'",
                );
                return;
            }
        };

        let ts = TerminalSession {
            color: None,
            uid: session_uid,
            label: "claude".to_string(),
            session_type: "claude".to_string(),
            session: s,
            status: SessionStatus::Running,
            idle_since: None,
            last_write_at: None,
            transcript_id: cloned_transcript_id.clone(),
            generation: 0,
            pending_jsonl_files: pending,
            hidden: false,
            idle_timeout_secs,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            last_delivery: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
            seeded_from_snapshot: seed_from.map(str::to_string),
            preserved_last_exit: None,
            // 12e-r2 F1: use the active_host SNAPSHOT taken at
            // the top of this function — same value the spawn
            // dialed against. Reading `self.active_host` here
            // would race a concurrent cycle and tag the
            // session with the wrong host.
            host_id: active_host.clone(),
        };
        let ws = Workspace {
            color: None,
            pinned: false,
            id: workspace_id_pre,
            name: label.to_string(),
            is_closed: false,
            is_cloud: false,
            repo_url: Some(repo_url.to_string()),
            worktree_path: Some(worktree_path),
            main_repo_path: Some(main_repo),
            worker_vm: None,
            worker_zone: None,
            // Same host the session was spawned on (the snapshot above).
            host_id: active_host.clone(),
            sessions: vec![ts],
            tombstones: Vec::new(),
            is_pushing: false,
        };
        let new_wi = self.workspaces.len();
        self.workspaces.push(ws);
        // Sub-2b-1 review-r#2 #3: seeded sessions have a known
        // transcript_id at construction time (the clone's id —
        // see `cloned_transcript_id`). The discovery loop's
        // `initial_bind` branch won't fire for them
        // (transcript_id is already Some), so push the
        // transcript_path here explicitly. No-op for local-only
        // sessions (the push helper gates on
        // `daemon_session_uid.is_some()`).
        if let Some(ws) = self.workspaces.get(new_wi) {
            if let Some(ts) = ws.sessions.get(0) {
                Self::push_transcript_path_to_daemon_if_attached(&self.host_pool, ts, ws);
            }
        }
        self.cursor = Cursor::Session(new_wi, 0);
        self.save_session_manifest();
        // Sub-2b-3 review-5 #2: A-n added a new workspace with
        // a fresh worktree_path. The daemon needs that mapping
        // BEFORE a freshly-spawned agent calls mcp_start_session
        // (descendant-task resolution looks up the workspace
        // via state.workspaces). Without this push, the agent
        // would race the next API-driven reconcile.
        self.push_state_to_daemon();
        self.set_status_msg("Workspace created");
    }

    /// Phase 3 (remote-session-execution): A-n on a REMOTE daemon host.
    /// The daemon resolves the repo, creates the worktree, and builds
    /// argv/env on its OWN filesystem (`create_session` RPC); the TUI sends
    /// only the high-level request — NO local argv/env/working_dir/MCP-path/
    /// cgroup_prefix — then attaches over the host's socket via
    /// `try_attach_via_daemon_with_deps` and builds a `TerminalSession`
    /// pinned to that host.
    ///
    /// `in_place` and `seed_from` are rejected up front (Non-goals): both
    /// need cross-host machinery (spawning in the repo root / materializing
    /// + resuming a snapshot on the remote) out of scope here. Rejected with
    /// a status message and NO RPC — never silently downgraded.
    fn create_remote_session(
        &mut self,
        host: &cm_daemon::host_id::HostId,
        repo_url: &str,
        label: &str,
        start_branch: Option<&str>,
        idle_timeout_secs: u16,
        seed_from: Option<&str>,
        in_place: bool,
    ) {
        // Non-goals: reject (no RPC) before any work.
        if in_place {
            self.set_status_msg(
                "Remote A-n: in-place (repo-root) sessions aren't supported on a remote host",
            );
            return;
        }
        if seed_from.is_some() {
            self.set_status_msg(
                "Remote A-n: seeding from a snapshot isn't supported on a remote host",
            );
            return;
        }

        let slug = worktree::slugify(label);
        if slug.is_empty() {
            self.set_status_msg("Invalid name");
            return;
        }

        let socket = match self
            .host_pool
            .for_host(host)
            .ok()
            .and_then(|h| h.socket_path())
        {
            Some(s) => s,
            None => {
                self.set_status_msg(&format!(
                    "Remote host `{}` not reachable (no live socket)",
                    host.as_str()
                ));
                return;
            }
        };

        let (cols, rows) = self.last_term_size;
        // The TUI is the source of truth for uid + workspace identity (same
        // as the local path); the daemon auto-registers the workspace from
        // the worktree it creates.
        let session_uid = new_session_uid();
        let workspace_id_pre = new_workspace_id();
        let op_token = crate::daemon_launch::operator_token();

        // create_session: daemon resolves repo → worktree → argv/env. Engine
        // travels in the daemon's WIRE vocabulary ("claude-code"). A-n is
        // taskless.
        let res = match crate::client_session::rpc_create_session(
            &socket,
            op_token,
            &session_uid,
            &workspace_id_pre,
            "claude",
            "claude-code",
            repo_url,
            start_branch,
            &slug,
            None,
            cols,
            rows,
        ) {
            Ok(r) => r,
            Err(e) => {
                self.set_status_msg(&format!("Remote create_session failed: {}", e));
                return;
            }
        };

        // Attach to the just-created remote session over the host's socket.
        let worktree_path = PathBuf::from(&res.worktree_path);
        let session = match try_attach_via_daemon_with_deps(
            &self.host_pool,
            &res.session_uid,
            &res.workspace_id,
            &worktree_path,
            "claude",
            "claude",
            cols,
            rows,
            None,
            None,
            None,
            host,
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                // The daemon already started the session (and created its
                // worktree). Mirror `ClientSession::new`'s cleanup contract:
                // best-effort kill before bubbling so we don't leak a live,
                // headless, unattached session on the remote host. Log the
                // cleanup error separately; the original attach error wins.
                if let Err(cleanup_err) = crate::client_session::rpc_kill_session(
                    &socket,
                    op_token,
                    &res.session_uid,
                ) {
                    eprintln!(
                        "create_remote_session cleanup: kill_session({}) failed \
                         after attach error: {}",
                        res.session_uid, cleanup_err,
                    );
                }
                self.set_status_msg(&format!("Remote attach failed: {}", e));
                return;
            }
        };

        let ts = TerminalSession {
            color: None,
            uid: res.session_uid,
            label: "claude".to_string(),
            session_type: "claude".to_string(),
            session,
            status: SessionStatus::Running,
            idle_since: None,
            last_write_at: None,
            transcript_id: None,
            generation: 0,
            // Remote: the worktree lives on the daemon's filesystem, so the
            // TUI can't run local JSONL detection. Transcript-path resolution
            // for remote sessions is a follow-on (out of Phase 3 scope); the
            // interactive PTY attach above works regardless.
            pending_jsonl_files: None,
            hidden: false,
            idle_timeout_secs,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            last_delivery: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
            seeded_from_snapshot: None,
            preserved_last_exit: None,
            host_id: host.clone(),
        };
        let ws = Workspace {
            color: None,
            pinned: false,
            id: res.workspace_id,
            name: label.to_string(),
            is_closed: false,
            is_cloud: false,
            repo_url: Some(repo_url.to_string()),
            worktree_path: Some(worktree_path),
            // The main checkout lives on the remote host; there is no local
            // main repo path for a remote workspace.
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: host.clone(),
            sessions: vec![ts],
            tombstones: Vec::new(),
            is_pushing: false,
        };
        let new_wi = self.workspaces.len();
        self.workspaces.push(ws);
        self.cursor = Cursor::Session(new_wi, 0);
        self.save_session_manifest();
        self.set_status_msg(&format!("Workspace created on `{}`", host.as_str()));
    }

    /// Attach to the active workspace (SSH for cloud, claude for local, bash fallback).
    /// A-R: revive the focused DEAD session in place, keeping its uid,
    /// label, task binding, and conversation — the recovery for an
    /// accidental ctrl-c / crashed agent that previously required a full
    /// TUI restart (whose manifest restore is the only other path that
    /// resurrects dead sessions).
    ///
    /// Local sessions re-run that startup-restore primitive for ONE slot
    /// (`spawn_restored_session`): re-attach if the daemon still holds the
    /// uid live (the TUI merely lost its stream), else re-spawn at the
    /// SAME uid with the transcript resumed (claude `--resume` / codex
    /// `resume`; bash respawns fresh). Remote sessions can't spawn
    /// TUI-side (`guard_local_host_only` — local argv/paths are wrong on
    /// the remote), so they ask the session's host daemon to
    /// `session.revive` (same-uid resumed respawn composed daemon-side)
    /// and then ride the existing deferred-reattach machinery to rebind
    /// the slot once the spawn lands.
    pub(super) fn revive_active_session(&mut self) {
        let (wi, si) = match self.cursor.clone() {
            Cursor::Session(wi, si) => (wi, si),
            Cursor::Workspace(wi)
                if self
                    .workspaces
                    .get(wi)
                    .is_some_and(|w| w.sessions.len() == 1) =>
            {
                (wi, 0)
            }
            _ => {
                self.set_status_msg("Focus a session to revive");
                return;
            }
        };
        let Some(ws) = self.workspaces.get(wi) else { return };
        let Some(ts) = ws.sessions.get(si) else { return };
        if !ts.session.exited {
            self.set_status_msg(
                "Session is not dead — A-R revives exited sessions",
            );
            return;
        }
        if ts.workflow_run_id.is_some() {
            self.set_status_msg(
                "Workflow participant — the workflow owns its lifecycle (A-u resumes the run)",
            );
            return;
        }
        if ts.continuous_task_id.is_some() {
            self.set_status_msg(
                "Continuous session — the scheduler owns its respawn (supervision / trigger)",
            );
            return;
        }
        let Some(wt_path) = ws.worktree_path.clone() else {
            self.set_status_msg("Workspace has no worktree path — can't revive");
            return;
        };

        let local = cm_daemon::host_id::HostId::local();
        if ts.host_id != local {
            // Remote: daemon-side same-uid revive, then the standard
            // deferred-reattach flow rebinds the slot.
            let ws_id = ws.id.clone();
            let host_id = ts.host_id.clone();
            let mut entry = ts.to_manifest_entry();
            entry.last_exit = None;
            let socket = self
                .host_pool
                .for_host(&host_id)
                .ok()
                .and_then(|h| h.socket_path());
            let Some(socket) = socket else {
                self.set_status_msg(&format!(
                    "Host `{}` unreachable — can't revive",
                    host_id.as_str(),
                ));
                return;
            };
            if let Err(e) = crate::client_session::rpc_session_revive(
                &socket,
                crate::daemon_launch::operator_token(),
                &entry,
                &ws_id,
                &wt_path.to_string_lossy(),
            ) {
                self.set_status_msg(&format!("Revive failed: {e}"));
                return;
            }
            {
                let ts = &mut self.workspaces[wi].sessions[si];
                ts.session.exited = false;
                ts.set_status(SessionStatus::Idle);
                ts.preserved_last_exit = None;
            }
            self.requeue_remote_reconnect(wi, si, "A-R revive");
            self.save_session_manifest();
            self.set_status_msg(&format!(
                "Session revived on `{}` — reattaching…",
                host_id.as_str(),
            ));
            return;
        }

        // Local: one-slot rerun of the startup restore.
        let mut entry = ts.to_manifest_entry();
        entry.last_exit = None;
        let (cols, rows) = self.last_term_size;
        // Live-uid probe decides attach-vs-spawn, exactly like startup
        // restore. An unreachable daemon yields an empty set → the spawn
        // path, whose own RPC then surfaces the real error.
        let live_uids = self
            .host_pool
            .for_host(&local)
            .ok()
            .and_then(|h| h.socket_path())
            .and_then(|socket| {
                crate::client_session::rpc_list_session_uids(
                    &socket,
                    crate::daemon_launch::operator_token(),
                )
                .ok()
            })
            .unwrap_or_default();
        let spawned = {
            let ws = &self.workspaces[wi];
            self.spawn_restored_session(&entry, ws, (cols, rows), &live_uids)
        };
        match spawned {
            Some((mut fresh, outcome)) => {
                // Same post-swap kick as the workflow respawn path: force
                // the daemon PTY to the pane size so the pane repaints.
                fresh.session.resize(cols, rows);
                self.workspaces[wi].sessions[si] = fresh;
                self.save_session_manifest();
                self.set_status_msg(match outcome {
                    RestoreOutcome::Attached => {
                        "Session revived — reattached to the still-live daemon session"
                    }
                    RestoreOutcome::Spawned => {
                        "Session revived — respawned with its conversation resumed"
                    }
                });
            }
            None => {
                self.set_status_msg(
                    "Revive failed — could not respawn the session (see stderr log)",
                );
            }
        }
    }

    pub(super) fn attach_active(&mut self) {
        let wi = match self.active_workspace_index() {
            Some(wi) => wi,
            None => return,
        };
        let (cols, rows) = self.last_term_size;

        {
            let ws = &self.workspaces[wi];
            if !ws.sessions.is_empty() {
                self.set_status_msg("Workspace already has sessions");
                return;
            }
        }

        // Host comes from the WORKSPACE (global-host removal). attach_active
        // runs on an EMPTY workspace, so for a REMOTE workspace A-a means
        // "reconnect the daemon-owned remote session", NOT "spawn a fresh
        // local one": the worktree is a path on the remote host that doesn't
        // exist locally, so a local spawn would land claude in $HOME and
        // orphan the real session (the reported bug). Re-arm the
        // deferred-reattach worklist (picking up any entry stranded in
        // `skipped_manifest_entries`) and let `drain_deferred_remote_reattach`
        // resolve it — it reattaches if the session is still alive on the
        // daemon, or the row stays a ghost (close with A-w) if it ended.
        let spawn_host = self.workspaces[wi].host_id.clone();
        if spawn_host != crate::hosts::HostId::local() {
            let ws_id = self.workspaces[wi].id.clone();
            let n = self.rearm_remote_reattach_for_workspace(&ws_id);
            if n > 0 {
                self.set_status_msg(&format!(
                    "Reconnecting remote session on `{}`…",
                    spawn_host,
                ));
            } else {
                self.set_status_msg(&format!(
                    "Nothing to reconnect on `{}` — the remote session ended; A-w to close this row",
                    spawn_host,
                ));
            }
            return;
        }

        let ws = &self.workspaces[wi];
        if ws.is_cloud && ws.worker_vm.is_none() {
            self.set_status_msg("Waiting for cloud VM assignment...");
            return;
        }

        // Borrow `ws` only for the snapshots needed below the
        // dispatch — `try_spawn_via_daemon` reborrows `self`.
        let worker_vm = ws.worker_vm.clone().filter(|s| !s.is_empty());
        let worker_zone = ws.worker_zone.clone();
        let worktree_path = ws.worktree_path.clone();
        let workspace_id = ws.id.clone();

        let ts = if let Some(vm) = worker_vm {
            let zone = worker_zone
                .unwrap_or_else(|| self.config.gcp_zone.clone());
            let args = vec![
                "compute".to_string(),
                "ssh".to_string(),
                vm,
                format!("--zone={}", zone),
                format!("--project={}", self.config.gcp_project),
                "--".to_string(),
                "-t".to_string(),
                "TERM=xterm-256color sudo su - worker -c 'tmux attach -t claude'".to_string(),
            ];
            Session::new("gcloud", &args, cols, rows, None, Default::default(), None)
                .ok()
                .map(|s| make_simple_session("ssh", "bash", s, None))
        } else if let Some(wt) = worktree_path {
            // migrate-tui-local Issue C: the local-claude branch
            // sends the workspace's local-filesystem worktree
            // (and per-session MCP config under `~/.cm/mcp/...`)
            // to the daemon. `spawn_host` is already proven `local`
            // by the remote early-return above, so this guard is now
            // defensive (a future caller that reaches here with a
            // non-local host still fail-fasts rather than mistagging
            // a session). The cloud-VM branch above and the
            // bash-fallback below don't talk to the daemon.
            if let Err(e) = guard_local_host_only(&spawn_host, "A-a attach-active") {
                self.set_status_msg(&format!("{}", e));
                None
            } else {
                let session_uid = new_session_uid();
                let pending = Self::list_jsonl_files(&wt);
                match self.try_spawn_via_daemon(
                    &session_uid,
                    &workspace_id,
                    &wt,
                    "claude",
                    "claude",
                    None,
                    cols,
                    rows,
                    None,
                    None,
                    None,
                    &spawn_host,
                    // attach_active is a fresh spawn — post-spawn
                    // detector will discover the transcript path.
                    None,
                ) {
                    Some(Ok(s)) => Some(make_simple_session_with_uid(
                        session_uid,
                        "claude",
                        "claude",
                        s,
                        Some(pending),
                    )),
                    Some(Err(e)) => {
                        self.set_status_msg(&format!("Attach (daemon spawn): {}", e));
                        None
                    }
                    None => {
                        self.set_status_msg(
                            "Internal: try_spawn_via_daemon returned None for daemon-eligible 'claude'",
                        );
                        None
                    }
                }
            }
        } else {
            // No worktree + no VM: bash-only fallback for orphan
            // workspaces. Stays local because the daemon needs a
            // worktree-bound workspace to auto-register on
            // start_session.
            Session::new("/bin/bash", &[], cols, rows, None, Default::default(), None)
                .ok()
                .map(|s| make_simple_session("bash", "bash", s, None))
        };

        if let Some(ts) = ts {
            // 10e-d: release any stale cap-kill de-dup entry for
            // this uid before publishing the new session into the
            // workspace. In production `new_session_uid` is
            // monotonic so the set never holds this uid yet — this
            // is defensive and exists for test paths that reuse
            // uids (and to keep the spawn-path contract explicit:
            // "fresh session → fresh toast window").
            self.clear_cap_kill_toast_state(&ts.uid);
            let si = self.workspaces[wi].sessions.len();
            self.workspaces[wi].sessions.push(ts);
            self.cursor = Cursor::Session(wi, si);
        }
    }

    /// `A-w` in the planning panel: attach a live, READ-ONLY terminal
    /// view of a cloud backtest's worker tmux, rendered like any other
    /// CM session. Spawns a LOCAL `gcloud compute ssh …` session (the
    /// operator's laptop gcloud auth reaches the backtest project) that
    /// runs `tmux attach -r -t backtest` on the worker VM.
    ///
    /// Graceful cases:
    ///   - non-backtest task → status message, no spawn.
    ///   - backtest not yet dispatched (no `worker_vm`) → status
    ///     message, no spawn.
    ///   - VM already gone → the `gcloud ssh` child fails and the
    ///     session exits cleanly (surfaced as a normal exited session),
    ///     no zombie.
    pub(super) fn watch_backtest(
        &mut self,
        task_id: &str,
        kind: &str,
        worker_vm: Option<String>,
        vm_project: Option<String>,
        vm_zone: Option<String>,
        title: &str,
    ) {
        if kind != "backtest" {
            self.set_status_msg(&format!(
                "A-w watches cloud backtest runs — '{}' is kind={}",
                title, kind
            ));
            return;
        }
        let vm = match worker_vm.filter(|s| !s.is_empty()) {
            Some(vm) => vm,
            None => {
                self.set_status_msg(
                    "Backtest not dispatched yet — no worker VM assigned. Retry once it's running.",
                );
                return;
            }
        };
        // metadata.vm.project is authoritative: backtest VMs run in a
        // DIFFERENT GCP project than the CM default (config.gcp_project),
        // so falling back to the config default is a last-resort guard,
        // not the expected path.
        let project = vm_project
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.config.gcp_project.clone());
        let zone = vm_zone
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.config.gcp_zone.clone());

        let args = backtest_watch_ssh_args(&vm, &project, &zone, backtest_watch_use_iap());
        let (cols, rows) = self.last_term_size;
        let session = match Session::new(
            "gcloud",
            &args,
            cols,
            rows,
            None,
            Default::default(),
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.set_status_msg(&format!("watch backtest: spawn failed: {}", e));
                return;
            }
        };
        let mut ts = make_simple_session(&format!("watch {}", title), "bash", session, None);
        ts.task_id = Some(task_id.to_string());

        // Home the watch session in the backtest task's own cloud
        // workspace (auto-provisioned during reconcile once worker_vm
        // lands). Fall back to matching by VM, then to provisioning a
        // minimal cloud workspace so the session always has a sidebar
        // row.
        let ws_idx = self
            .tasks
            .iter()
            .find(|t| t.task_id.as_deref() == Some(task_id))
            .and_then(|t| t.workspace_id.clone())
            .and_then(|wid| resolve_workspace_by_id(&self.workspaces, &wid))
            .or_else(|| {
                self.workspaces
                    .iter()
                    .position(|w| w.is_cloud && w.worker_vm.as_deref() == Some(vm.as_str()))
            });
        let ws_idx = match ws_idx {
            Some(i) => i,
            None => {
                let ws = Workspace {
                    color: None,
                    pinned: false,
                    id: new_workspace_id(),
                    name: title.to_string(),
                    is_closed: false,
                    is_cloud: true,
                    repo_url: None,
                    worktree_path: None,
                    main_repo_path: None,
                    worker_vm: Some(vm.clone()),
                    worker_zone: Some(zone.clone()),
                    host_id: cm_daemon::host_id::HostId::local(),
                    sessions: vec![],
                    tombstones: Vec::new(),
                    is_pushing: false,
                };
                self.workspaces.push(ws);
                self.workspaces.len() - 1
            }
        };

        self.clear_cap_kill_toast_state(&ts.uid);
        let si = self.workspaces[ws_idx].sessions.len();
        self.workspaces[ws_idx].sessions.push(ts);
        self.cursor = Cursor::Session(ws_idx, si);
        self.view_mode = ViewMode::Sessions;
        self.set_status_msg(&format!("Watching backtest {} (read-only) on {}", title, vm));
    }

    /// Spawn a session on an existing workspace by type ("claude" / "codex" / "bash").
    /// If `task_id` is Some, the new session is tagged with that task so it
    /// appears under the corresponding task subheader.
    pub(super) fn spawn_session_on_workspace(
        &mut self,
        workspace_id: &str,
        session_type: &str,
        task_id: Option<String>,
        seed_from: Option<&str>,
    ) {
        // Host is a property of the WORKSPACE, not the global active_host
        // (which only seeds NEW workspaces). The spawn host is resolved and
        // guarded from the target workspace below, once ws_index is known.
        // Resolve workspace_id → current index. If the workspace
        // disappeared while the form was open (delete, reconcile drop,
        // etc.), bail cleanly rather than spawning into an unrelated
        // workspace at whatever happens to sit at the stale index now.
        let ws_index = match resolve_workspace_by_id(&self.workspaces, workspace_id) {
            Some(i) => i,
            None => {
                self.set_status_msg(
                    "Workspace no longer exists — session not started",
                );
                return;
            }
        };
        if self.workspaces[ws_index].is_cloud && self.workspaces[ws_index].worker_vm.is_none() {
            self.set_status_msg("Waiting for cloud VM assignment...");
            return;
        }

        let (cols, rows) = self.last_term_size;

        // Cloud workspace + bash session type → SSH into the VM.
        if let Some(vm) = self.workspaces[ws_index].worker_vm.clone().filter(|s| !s.is_empty()) {
            if session_type == "bash" {
                let zone = self.workspaces[ws_index]
                    .worker_zone
                    .clone()
                    .unwrap_or_else(|| self.config.gcp_zone.clone());
                let si = self.workspaces[ws_index].sessions.len();
                let tmux_name = format!("bash-{}", si);
                let args = vec![
                    "compute".to_string(),
                    "ssh".to_string(),
                    vm,
                    format!("--zone={}", zone),
                    format!("--project={}", self.config.gcp_project),
                    "--".to_string(),
                    "-t".to_string(),
                    format!(
                        "TERM=xterm-256color sudo su - worker -c 'cd /workspace && tmux new-session -As {}'",
                        tmux_name
                    ),
                ];
                match Session::new("gcloud", &args, cols, rows, None, Default::default(), None) {
                    Ok(s) => {
                        let mut ts = make_simple_session(&tmux_name, "bash", s, None);
                        ts.task_id = task_id.clone();
                        let si = self.workspaces[ws_index].sessions.len();
                        self.workspaces[ws_index].sessions.push(ts);
                        self.cursor = Cursor::Session(ws_index, si);
                        self.save_session_manifest();
                        self.set_status_msg("Started SSH bash session");
                    }
                    Err(e) => self.set_status_msg(&format!("Spawn: {}", e)),
                }
                return;
            }
        }

        // The new session runs on the workspace's host (its worktree lives
        // there) — inherit from an existing session, else local. NOT the
        // global active_host (which only seeds NEW workspaces). Remote spawns
        // from the TUI aren't supported yet (doc/remote-session-execution.md);
        // guard with a clear message rather than mistag/misroute.
        let spawn_host = self.workspaces[ws_index]
            .sessions
            .first()
            .map(|s| s.host_id.clone())
            .unwrap_or_else(|| crate::hosts::HostId::local());
        // Phase 3 (remote-session-execution): a remote-hosted workspace routes
        // A-s to the daemon-resolved `add_session` path (reuses the remote
        // worktree). Local A-s runs the existing path below, unchanged (it
        // always passed the old `guard_local_host_only`).
        if spawn_host != crate::hosts::HostId::local() {
            self.add_remote_session(
                &spawn_host,
                ws_index,
                session_type,
                task_id,
                seed_from,
            );
            return;
        }
        let wt = self.workspaces[ws_index].worktree_path.clone();

        // Clone the seed snapshot BEFORE computing the baseline (Codex)
        // or building args (both engines). For Codex, the cloned seed
        // file must be in the baseline so the post-spawn detector picks
        // the new rollout id rather than rebinding to the seed file.
        let cloned: Option<agent_memory::ClonedSession> =
            match (seed_from, session_type, wt.as_ref()) {
                (Some(name), "claude", Some(p)) => {
                    match self.clone_snapshot_for_spawn(name, Engine::ClaudeCode, p) {
                        Some(c) => Some(c),
                        None => return,
                    }
                }
                (Some(name), "codex", Some(p)) => {
                    match self.clone_snapshot_for_spawn(name, Engine::Codex, p) {
                        Some(c) => Some(c),
                        None => return,
                    }
                }
                (Some(_), _, _) => {
                    // Bash or no worktree — seed_from is meaningless.
                    // The form prevents this combination but defend
                    // against it.
                    self.set_status_msg(
                        "Snapshots only apply to claude / codex sessions",
                    );
                    return;
                }
                (None, _, _) => None,
            };

        // For Claude with a clone, the JSONL is already on disk and
        // --resume keeps writing to it, so pending_jsonl_files = None
        // (detector path isn't used). For Codex, baseline is taken AFTER
        // the clone so the seed file is excluded and the detector picks
        // the freshly-minted rollout id post-resume.
        let pending = match (session_type, cloned.is_some()) {
            ("claude", true) => None,
            ("claude", false) => wt.as_ref().map(|p| Self::list_jsonl_files(p)),
            ("codex", _) => wt.as_ref().map(|p| Self::list_codex_sessions(p)),
            _ => None,
        };
        // Pre-generate uid so MCP env carries the same CM_TUI_SESSION_ID
        // the TerminalSession will hold. Sessions added on a workspace
        // are taskless from MCP's POV (they inherit a task_id below for
        // sidebar grouping but no workflow context).
        let session_uid_pre = new_session_uid();
        let cloned_transcript_id = cloned.as_ref().map(|c| c.transcript_id.clone());
        // migrate-tui-local: A-s spawns route through the daemon
        // for all three engines (claude / codex / bash). Workspaces
        // here always have a worktree (the cloud / VM branch above
        // already early-returned), so the `wt.is_none()` arm
        // surfaces a developer-visible error rather than fall back
        // to local PTY spawn.
        let workspace_id = self.workspaces[ws_index].id.clone();
        let Some(wt_path) = wt.as_deref() else {
            if let Some(c) = cloned.as_ref() {
                Self::cleanup_failed_clone(c);
            }
            self.set_status_msg(
                "A-s spawn: workspace has no worktree — daemon spawn requires one",
            );
            return;
        };
        // migrate-tui-local Issue 3: seeded Claude A-s spawns
        // know the transcript_id (cloned id) at construction;
        // hand its deterministic path to the daemon so
        // resolve_authorized_session resolves immediately. The
        // codex seed produces a fresh rollout id post-resume —
        // pre_spawn_transcript_path correctly returns None for
        // codex, so the post-spawn detector continues to
        // handle that case.
        let pre_spawn_transcript = cloned_transcript_id.as_deref().and_then(|sid| {
            pre_spawn_transcript_path(session_type, wt_path, sid)
        });
        let result: anyhow::Result<Session> = match self.try_spawn_via_daemon(
            &session_uid_pre,
            &workspace_id,
            wt_path,
            session_type,
            session_type,
            cloned_transcript_id.as_deref(),
            cols,
            rows,
            // Sub-2a Finding #1: A-s spawns a session under
            // an existing task — pass the task_id through
            // so the daemon's DaemonSession.task_id is
            // populated at spawn time, not left None.
            task_id.as_deref(),
            // A-s spawns a session under an existing task,
            // but workflow membership is decided later (when
            // the user runs A-f on it). No workflow context
            // at spawn time.
            None,
            None,
            &spawn_host,
            pre_spawn_transcript.as_deref(),
        ) {
            Some(Ok(s)) => Ok(s),
            Some(Err(e)) => {
                if let Some(c) = cloned.as_ref() {
                    Self::cleanup_failed_clone(c);
                }
                self.set_status_msg(&format!(
                    "Daemon spawn failed: {}",
                    e
                ));
                return;
            }
            None => {
                if let Some(c) = cloned.as_ref() {
                    Self::cleanup_failed_clone(c);
                }
                self.set_status_msg(&format!(
                    "Unsupported session_type '{}' for daemon spawn",
                    session_type
                ));
                return;
            }
        };
        match result {
            Ok(s) => {
                // Use the same uid we baked into MCP env for claude/codex.
                // bash sessions don't have MCP config and the uid is just
                // for sidebar tracking — but we still use the pre-gen one
                // for consistency.
                let mut ts = make_simple_session_with_uid(
                    session_uid_pre,
                    session_type,
                    session_type,
                    s,
                    pending,
                );
                ts.task_id = task_id;
                ts.seeded_from_snapshot = seed_from.map(str::to_string);
                // The new session inherits the WORKSPACE's host (the worktree
                // lives there) — the same value the spawn dialed against —
                // NOT the global active_host (which only seeds new workspaces).
                ts.host_id = spawn_host.clone();
                // Engine-asymmetric transcript_id wiring — see
                // `ClonedSession` rustdoc. For Claude the cloned id IS
                // the live transcript id; for Codex it's a seed-file id
                // and the live id is filled in by detection.
                if session_type == "claude" {
                    ts.transcript_id = cloned_transcript_id;
                }
                let si = self.workspaces[ws_index].sessions.len();
                self.workspaces[ws_index].sessions.push(ts);
                // Sub-2b-1 review-r#2 #3: same as
                // `create_local_session` — seeded Claude
                // sessions know their transcript_id at
                // construction (cloned id), so push to the
                // daemon now. The discovery loop's
                // `initial_bind` arm won't fire because
                // transcript_id is already Some. Codex's id is
                // a seed-file id; the discovery loop's
                // codex_resume_rebind window catches that and
                // pushes when the actual rollout file appears.
                if let Some(ws) = self.workspaces.get(ws_index) {
                    if let Some(ts) = ws.sessions.get(si) {
                        Self::push_transcript_path_to_daemon_if_attached(&self.host_pool, ts, ws);
                    }
                }
                self.cursor = Cursor::Session(ws_index, si);
                self.save_session_manifest();
                self.set_status_msg(&format!("Started {} session", session_type));
            }
            Err(e) => {
                if let Some(c) = cloned.as_ref() {
                    Self::cleanup_failed_clone(c);
                }
                self.set_status_msg(&format!("Spawn: {}", e));
            }
        }
    }

    /// Phase 3 (remote-session-execution): A-s on a REMOTE-hosted workspace.
    /// Adds another session to the workspace's EXISTING remote worktree via
    /// the daemon's `add_session` RPC (no `repo_url`/`slug`/`start_branch` —
    /// the daemon looks up the workspace's worktree), then attaches over the
    /// host's socket and builds a `TerminalSession` pinned to that host.
    ///
    /// `seed_from` is rejected up front (Non-goals): resuming an agent-memory
    /// snapshot on a remote host needs cross-host materialization out of
    /// scope here. Rejected with a status message and NO RPC.
    pub(super) fn add_remote_session(
        &mut self,
        host: &cm_daemon::host_id::HostId,
        ws_index: usize,
        session_type: &str,
        task_id: Option<String>,
        seed_from: Option<&str>,
    ) {
        if seed_from.is_some() {
            self.set_status_msg(
                "Remote A-s: seeding from a snapshot isn't supported on a remote host",
            );
            return;
        }

        let socket = match self
            .host_pool
            .for_host(host)
            .ok()
            .and_then(|h| h.socket_path())
        {
            Some(s) => s,
            None => {
                self.set_status_msg(&format!(
                    "Remote host `{}` not reachable (no live socket)",
                    host.as_str()
                ));
                return;
            }
        };

        let (cols, rows) = self.last_term_size;
        let workspace_id = self.workspaces[ws_index].id.clone();
        let session_uid = new_session_uid();
        let op_token = crate::daemon_launch::operator_token();
        // Daemon WIRE engine ("claude" → "claude-code").
        let wire_engine = match session_type {
            "claude" => "claude-code",
            other => other,
        };

        let res = match crate::client_session::rpc_add_session(
            &socket,
            op_token,
            &session_uid,
            &workspace_id,
            session_type,
            wire_engine,
            task_id.as_deref(),
            cols,
            rows,
        ) {
            Ok(r) => r,
            Err(e) => {
                self.set_status_msg(&format!("Remote add_session failed: {}", e));
                return;
            }
        };

        let worktree_path = PathBuf::from(&res.worktree_path);
        let session = match try_attach_via_daemon_with_deps(
            &self.host_pool,
            &res.session_uid,
            &workspace_id,
            &worktree_path,
            session_type,
            session_type,
            cols,
            rows,
            task_id.as_deref(),
            None,
            None,
            host,
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                // The daemon already started the session into the existing
                // worktree. Mirror `ClientSession::new`'s cleanup contract:
                // best-effort kill before bubbling so we don't leak a live,
                // headless, unattached session on the remote host. Log the
                // cleanup error separately; the original attach error wins.
                if let Err(cleanup_err) = crate::client_session::rpc_kill_session(
                    &socket,
                    op_token,
                    &res.session_uid,
                ) {
                    eprintln!(
                        "add_remote_session cleanup: kill_session({}) failed \
                         after attach error: {}",
                        res.session_uid, cleanup_err,
                    );
                }
                self.set_status_msg(&format!("Remote attach failed: {}", e));
                return;
            }
        };

        let mut ts = make_simple_session_with_uid(
            res.session_uid,
            session_type,
            session_type,
            session,
            None,
        );
        ts.task_id = task_id;
        ts.host_id = host.clone();
        let si = self.workspaces[ws_index].sessions.len();
        self.workspaces[ws_index].sessions.push(ts);
        self.cursor = Cursor::Session(ws_index, si);
        self.save_session_manifest();
        self.set_status_msg(&format!("Started {} session on `{}`", session_type, host.as_str()));
    }

    /// Spawn a local claude --resume session after a pull completes.
    pub(super) fn spawn_resumed_session(
        &mut self,
        task_id: Option<String>,
        worktree_path: PathBuf,
        main_repo: PathBuf,
        session_id: String,
        repo_url: String,
        prompt: String,
    ) {
        let (cols, rows) = self.last_term_size;
        // migrate-tui-local Issue B: cloud-pull A-l always
        // materializes the resumed workspace from a locally-
        // pulled worktree onto the local filesystem. Pin the
        // host snapshot to `HostId::local()` — NOT
        // `self.active_host` — so a concurrent A-H cycle between
        // pull-start and PullComplete can't send the local
        // filesystem path to a remote daemon (and tag the new
        // workspace with the wrong host_id).
        let host_snapshot = cm_daemon::host_id::HostId::local();
        let workspace_id_pre = new_workspace_id();
        // Pre-generate the session UID so the per-session MCP config
        // bakes the matching CM_TUI_SESSION_ID. Without this, a pulled
        // session can spawn but its agent has no MCP config and any
        // tool call would fail auth as `not_found`.
        let session_uid = new_session_uid();

        // migrate-tui-local: route the resume through
        // `try_spawn_via_daemon` with `--resume <session_id>`
        // threaded as `resume_session_id`. The daemon then spawns
        // `claude --resume <id>` and registers the session in
        // `state.sessions` with the resumed transcript bound at
        // spawn time — no post-spawn `/resume` workaround.
        //
        // migrate-tui-local Issue 3: we already know the
        // transcript_id (session_id) AND the worktree, so the
        // claude transcript path is deterministic — hand it to
        // the daemon up front so `resolve_authorized_session`
        // resolves immediately for MCP `read_session_output`.
        let pre_spawn_transcript =
            pre_spawn_transcript_path("claude", &worktree_path, session_id.as_str());
        let new_sess = match self.try_spawn_via_daemon(
            &session_uid,
            &workspace_id_pre,
            &worktree_path,
            "claude",
            "claude",
            Some(session_id.as_str()),
            cols,
            rows,
            task_id.as_deref(),
            None,
            None,
            &host_snapshot,
            pre_spawn_transcript.as_deref(),
        ) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                self.set_status_msg(&format!("Resume (daemon spawn): {}", e));
                return;
            }
            None => {
                self.set_status_msg(
                    "Internal: try_spawn_via_daemon returned None for daemon-eligible 'claude'",
                );
                return;
            }
        };

        {
            let mut ts = make_simple_session_with_uid(
                session_uid,
                "claude",
                "claude",
                new_sess,
                None,
            );
            ts.transcript_id = Some(session_id.clone());
            ts.task_id = task_id.clone();
            // migrate-tui-local Issue B: tag with the local host
            // snapshot (same value used in the daemon dial above).
            // A concurrent A-H cycle MUST NOT influence the new
            // local workspace's host_id.
            ts.host_id = host_snapshot.clone();

                // If we have a task_id, find the TaskEntry and its (cloud)
                // workspace; replace that workspace with a local one.
                let target_ti = task_id
                    .as_ref()
                    .and_then(|id| {
                        self.tasks
                            .iter()
                            .position(|t| t.task_id.as_deref() == Some(id))
                    });

            // migrate-tui-local: workspace id was pre-generated
            // above so the daemon auto-registered it on
            // start_session; carry the same value through here.
            let local_ws = Workspace {
                color: None,
                pinned: false,
                id: workspace_id_pre,
                name: task_id
                    .as_deref()
                    .and_then(|id| {
                        self.tasks
                            .iter()
                            .find(|t| t.task_id.as_deref() == Some(id))
                            .map(|t| t.name.clone())
                    })
                    .unwrap_or_else(|| prompt.chars().take(60).collect()),
                is_closed: false,
                is_cloud: false,
                repo_url: Some(repo_url.clone()),
                worktree_path: Some(worktree_path.clone()),
                main_repo_path: Some(main_repo.clone()),
                worker_vm: None,
                worker_zone: None,
                // The cloud-pull replacement worktree is always local.
                host_id: cm_daemon::host_id::HostId::local(),
                sessions: vec![ts],
                tombstones: Vec::new(),
                is_pushing: false,
            };
            let ws_id = local_ws.id.clone();

            if let Some(ti) = target_ti {
                // Remove the old (cloud) workspace if one was linked.
                if let Some(old_id) = self.tasks[ti].workspace_id.clone() {
                    self.workspaces.retain(|w| w.id != old_id);
                }
                self.tasks[ti].is_cloud = false;
                self.tasks[ti].session_id = Some(session_id);
                self.tasks[ti].workspace_id = Some(ws_id.clone());
            } else {
                // No matching task — create one.
                self.tasks.push(TaskEntry {
                    task_id,
                    name: local_ws.name.clone(),
                    api_status: TaskStatus::Running,
                    repo_url: Some(repo_url),
                    prompt: Some(prompt),
                    wip_branch: None,
                    session_id: Some(session_id),
                    blocked_at: None,
                    is_cloud: false,
                    workspace_id: Some(ws_id.clone()),
                    project: None,
                    parent_task_id: None,
                    worktree_mode: WorktreeMode::Inherit,
                    metadata: None,
                });
            }
            self.workspaces.push(local_ws);
            let new_wi = self.workspaces.len() - 1;
            self.cursor = Cursor::Session(new_wi, 0);
            self.save_session_manifest();
            // Sub-2a Finding #1: a resume_locally may have
            // inserted a new TaskEntry above.
            self.push_state_to_daemon();
            self.set_status_msg("Resumed locally");
        }
    }

    /// Mark the first task bound to the active workspace as done via the API.
    /// Does nothing if the workspace has zero or multiple bound tasks (ambiguous).
    pub(super) fn mark_active_done(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };

        // Task-scoped: if the cursor is on a task header, or on a session
        // tagged with a task, mark THAT task done (and close only its
        // sessions). If the cursor is workspace-scoped, fall back to the
        // old "single bound task" logic.
        let scoped_tid = self.cursor_task_id();

        let ws_id = self.workspaces[wi].id.clone();
        let tid = match scoped_tid {
            Some(t) => t,
            None => {
                let bound: Vec<String> = self
                    .tasks
                    .iter()
                    .filter(|t| t.workspace_id.as_deref() == Some(&ws_id))
                    .filter_map(|t| t.task_id.clone())
                    .collect();
                if bound.len() > 1 {
                    self.set_status_msg("Multiple tasks bound — pick one (A-d on its header)");
                    return;
                }
                match bound.into_iter().next() {
                    Some(t) => t,
                    None => {
                        // No task — soft-close every session in the
                        // workspace. Tombstone each so the resolver
                        // can still answer `read_session_output`.
                        // Helper persists the manifest internally.
                        self.tombstone_and_remove(wi, |_| true);
                        self.cursor = Cursor::Workspace(wi);
                        self.clamp_cursor();
                        self.set_status_msg("Cleared sessions");
                        return;
                    }
                }
            }
        };

        let mut fields = HashMap::new();
        fields.insert(
            "status".to_string(),
            serde_json::Value::String("done".to_string()),
        );
        self.backend.update_task(tid.clone(), fields);
        self.planning.mark_task_done_by_id(&tid);
        if let Some(task) = self
            .tasks
            .iter_mut()
            .find(|t| t.task_id.as_deref() == Some(&tid))
        {
            task.api_status = TaskStatus::Done;
        }
        // Drop sessions tagged with this task. Other task-scoped and
        // workspace-level sessions in the same workspace stay running.
        // Tombstone each so post-done `read_session_output` keeps working.
        // Helper persists the manifest before returning.
        let target = tid.clone();
        self.tombstone_and_remove(wi, |ts| {
            ts.task_id.as_deref() == Some(target.as_str())
        });
        // P1 (Feature 3): reap the workspace if that was its last live work.
        // Without this the workspace is hidden only until `reconcile_tasks`
        // drops the now-Done task, after which `is_past_workspace` no longer
        // matches (no bound task) and the empty workspace reappears as an
        // unbound header — the leftover the operator has to A-W by hand.
        self.maybe_reap_spent_workspace(wi);
        self.cursor = Cursor::Workspace(wi);
        self.clamp_cursor();
        self.set_status_msg("Marked done");
    }

    /// P1 (Feature 3): soft-close a "spent" workspace so it stops lingering
    /// after its last subtask finishes. A workspace is spent when it is a
    /// local (non-cloud), not-already-closed workspace that has **no live
    /// sessions**, **no active (non-Done) bound task**, and **at least one
    /// tombstone** (i.e. it was actually used — this excludes a freshly
    /// created empty slot waiting for its first session). Soft-close means
    /// `is_closed = true` (same as `close_active_workspace` / A-W), which
    /// keeps it hidden via `is_past_workspace`'s `is_closed` branch even after
    /// its Done task is reconciled away. Deliberately NOT a hard delete: the
    /// tombstones (post-done `read_session_output`) and the git worktree are
    /// preserved — only A-x (`delete_active`) force-removes those. Idempotent
    /// and cheap; safe to call from any teardown site.
    pub(super) fn maybe_reap_spent_workspace(&mut self, wi: usize) {
        let reap = self
            .workspaces
            .get(wi)
            .is_some_and(|ws| Self::workspace_is_spent(ws, &self.tasks));
        if reap {
            if let Some(ws) = self.workspaces.get_mut(wi) {
                ws.is_closed = true;
            }
            self.save_session_manifest();
        }
    }

    /// Pure predicate behind [`Self::maybe_reap_spent_workspace`]: true when
    /// `ws` is a local workspace that has finished its work and should be
    /// hidden. Split out so the decision is side-effect-free and unit-testable
    /// (the mutation + manifest write live in the caller). Spent ⟺ local
    /// (non-cloud), not already closed, **no live sessions**, **at least one
    /// tombstone** (it was used — excludes a freshly-created empty slot), and
    /// **no bound task that isn't Done** (another active task keeps it open).
    fn workspace_is_spent(ws: &Workspace, tasks: &[TaskEntry]) -> bool {
        !ws.is_cloud
            && !ws.is_closed
            && ws.sessions.is_empty()
            && !ws.tombstones.is_empty()
            && !tasks.iter().any(|t| {
                t.workspace_id.as_deref() == Some(&ws.id)
                    && !matches!(t.api_status, TaskStatus::Done)
            })
    }

    /// Delete whatever the cursor resolves to:
    ///   - Cursor::Task → delete just that task (close its sessions, remove
    ///     from backend + local TaskEntry). The workspace, worktree, and any
    ///     other tasks / workspace-level sessions survive.
    ///   - Cursor::Session on a task-tagged session → same as Cursor::Task.
    ///   - Otherwise → delete the whole workspace: close sessions, remove
    ///     worktree + branch, delete any bound tasks from the API.
    pub(super) fn delete_active(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };

        // Task-scoped delete path.
        if let Some(tid) = self.cursor_task_id() {
            // Tombstone-then-drop the task's sessions so the resolver
            // can still answer for them post-delete. Helper persists.
            let target = tid.clone();
            self.tombstone_and_remove(wi, |ts| {
                ts.task_id.as_deref() == Some(target.as_str())
            });
            self.backend.delete_task(tid.clone());
            self.tasks.retain(|t| t.task_id.as_deref() != Some(tid.as_str()));
            self.cursor = Cursor::Workspace(wi);
            self.clamp_cursor();
            self.set_status_msg("Task deleted");
            self.save_session_manifest();
            // Sub-2a Finding #1: task removal — refresh tree.
            self.push_state_to_daemon();
            return;
        }

        let ws_id = self.workspaces[wi].id.clone();
        // In-place workspaces have NO dedicated worktree or branch — their
        // `worktree_path` IS the main repo. Skipping git teardown for them
        // is the whole point of this guard: `git worktree remove` would
        // target the main checkout, and `git branch -D` would delete the
        // repo's live branch (e.g. `main`).
        let in_place = self.workspaces[wi].is_in_place();
        let worktree_path = self.workspaces[wi].worktree_path.clone();
        let main_repo_path = self.workspaces[wi].main_repo_path.clone();
        let bound_task_ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| t.workspace_id.as_deref() == Some(&ws_id))
            .filter_map(|t| t.task_id.clone())
            .collect();

        // Determine the branch to delete from any bound task's wip_branch.
        let wip_branch = self
            .tasks
            .iter()
            .find(|t| t.workspace_id.as_deref() == Some(&ws_id))
            .and_then(|t| t.wip_branch.clone());

        // Worktree removal is the gate to the rest of the destructive
        // cleanup. If `git worktree remove` fails, branches and API
        // tasks should NOT get deleted — better to leave the user
        // with a recoverable state (worktree still on disk, branches
        // intact, API rows intact) than to half-cleanup. The status
        // bar shows the git error so the user knows what's wrong.
        //
        // In-place workspaces skip this entirely: there's no worktree to
        // remove (it's the main repo). The rest of the cleanup (API task
        // deletion, session kills, row removal) still runs unconditionally
        // below — we just never touch git.
        if !in_place {
            if let (Some(ref wt), Some(ref repo)) = (&worktree_path, &main_repo_path) {
                if let Err(e) = worktree::remove_worktree(repo, wt) {
                    self.set_status_msg(&format!(
                        "Workspace delete aborted: git worktree remove failed: {}",
                        e
                    ));
                    return;
                }
            }
        }
        // Branch deletion: skipped for in-place. An in-place task's
        // `wip_branch` is the repo's real current branch (e.g. `main`),
        // not a `cm/<slug>` throwaway — deleting it would be catastrophic.
        if !in_place {
            if let (Some(ref branch), Some(ref repo)) = (&wip_branch, &main_repo_path) {
                let _ = std::process::Command::new("git")
                    .arg("-C")
                    .arg(repo)
                    .args(["branch", "-D", branch])
                    .output();
                if !bound_task_ids.is_empty() {
                    let _ = std::process::Command::new("git")
                        .arg("-C")
                        .arg(repo)
                        .args(["push", "origin", "--delete", branch])
                        .output();
                }
            }
        }

        for tid in &bound_task_ids {
            self.backend.delete_task(tid.clone());
        }
        self.tasks.retain(|t| !bound_task_ids.iter().any(|id| t.task_id.as_deref() == Some(id)));
        // Slice 10c-e-3b-fix2: workspace teardown is the bulkiest
        // operator-driven cleanup path. Issue daemon kill_session
        // for every daemon-attached session in this workspace
        // BEFORE the Workspace (and its Sessions) drop — Drop is
        // detach-only by design.
        let pool = std::sync::Arc::clone(&self.host_pool);
        let removed_uids: Vec<String> = self.workspaces[wi]
            .sessions
            .iter()
            .map(|ts| ts.uid.clone())
            .collect();
        for ts in &self.workspaces[wi].sessions {
            Self::kill_daemon_session_if_attached(&pool, ts);
        }
        self.workspaces.remove(wi);
        // Cancel reconnect/reattach for every session in the deleted
        // workspace — otherwise a reconnecting entry would stay queued in
        // `pending_remote_reattach` forever (its workspace is gone).
        for uid in &removed_uids {
            self.forget_reconnect_state(uid);
        }
        self.cursor = Cursor::Workspace(wi.min(self.workspaces.len().saturating_sub(1)));
        // Sub-2a Finding #1: workspace delete removed all bound
        // tasks from `self.tasks` — refresh tree.
        self.push_state_to_daemon();
        self.set_status_msg("Deleted");
    }

    /// Push the active local workspace to the cloud. If a task is bound to
    /// the workspace, its id is included so the cloud side can reuse it;
    /// otherwise a new cloud task is created from the workspace's name.
    ///
    /// **Invariant**: this function does NOT mutate local workspace state
    /// (no tombstones, no clearing `worktree_path`, no flipping
    /// `is_cloud`). All destructive cleanup is deferred to
    /// `BackendEvent::PushComplete` in `drain_backend_events`. A failed
    /// push (`PushFailed`) just clears `is_pushing` and surfaces the
    /// error — the user can retry without reconstructing the worktree.
    pub(super) fn push_active(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };
        if self.workspaces[wi].is_cloud {
            self.set_status_msg("Can only push local workspaces");
            return;
        }
        if self.workspaces[wi].is_pushing {
            self.set_status_msg("Push already in progress");
            return;
        }
        // In-place workspaces have no dedicated worktree to upload — their
        // path IS the main repo. Pushing would convert the main checkout
        // into a cloud workspace (clearing the local row), which is
        // confusing and almost never intended. Block it explicitly.
        if self.workspaces[wi].is_in_place() {
            self.set_status_msg("Can't push an in-place workspace (no worktree to upload)");
            return;
        }
        let worktree_path = match &self.workspaces[wi].worktree_path {
            Some(p) => p.clone(),
            None => {
                self.set_status_msg("No worktree to push");
                return;
            }
        };
        let repo_url = match &self.workspaces[wi].repo_url {
            Some(u) => u.clone(),
            None => {
                self.set_status_msg("No repo URL");
                return;
            }
        };
        let ws_id = self.workspaces[wi].id.clone();
        let ws_name = self.workspaces[wi].name.clone();
        let first = self.first_task_for_ws(&ws_id);
        let name = first.and_then(|t| t.prompt.clone()).unwrap_or(ws_name);
        let task_id = first.and_then(|t| t.task_id.clone());

        self.workspaces[wi].is_pushing = true;
        self.backend.push(worktree_path, repo_url, name, task_id, ws_id);
        self.cursor = Cursor::Workspace(wi);
        self.set_status_msg("Pushing to cloud...");
    }

    /// Apply the destructive local-cleanup half of a push, gated on a
    /// `PushComplete` event from the backend. Tombstones live sessions,
    /// drops `worktree_path`, flips `is_cloud` on the workspace and any
    /// bound task, and persists the new state. If `cloud_task_id` was
    /// returned (always set for now, but kept Optional in the event),
    /// no extra task binding work is done — `do_refresh` will pull the
    /// authoritative cloud row in the next refresh tick.
    pub(super) fn finish_push(&mut self, workspace_id: &str, _cloud_task_id: Option<String>) {
        let Some(wi) = self.workspaces.iter().position(|w| w.id == workspace_id) else {
            return;
        };
        // Tombstone first — the helper saves the manifest with each
        // tombstone's `worktree_path` snapshotted at the current value.
        // We then mutate workspace + task state and save AGAIN so the
        // post-push state (no worktree, is_cloud=true) is durable too.
        // Without the second save, a crash here would leave the manifest
        // with valid tombstones but the workspace still flagged local
        // with a stale `worktree_path` — the worst kind of half-state
        // because it looks valid on restart.
        self.tombstone_and_remove(wi, |_| true);
        let ws_id = self.workspaces[wi].id.clone();
        self.workspaces[wi].worktree_path = None;
        self.workspaces[wi].is_cloud = true;
        self.workspaces[wi].is_pushing = false;
        if let Some(task) = self
            .tasks
            .iter_mut()
            .find(|t| t.workspace_id.as_deref() == Some(&ws_id))
        {
            task.is_cloud = true;
        }
        self.save_session_manifest();
        // Sub-2b-3 review-5 #2: push the cleared worktree_path
        // to the daemon. Without this, the daemon retains the
        // stale local path until the next reconcile_tasks
        // (which could be seconds later, or never if the API
        // isn't refreshing), and a concurrent `mcp_start_session`
        // would spawn into the deleted worktree.
        self.push_state_to_daemon();
    }

    /// Pull the active cloud workspace to local (uses the first bound task).
    pub(super) fn pull_active(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };
        let ws_id = self.workspaces[wi].id.clone();
        let Some(task) = self
            .tasks
            .iter()
            .find(|t| t.workspace_id.as_deref() == Some(&ws_id))
        else {
            self.set_status_msg("No task bound to pull");
            return;
        };
        let task_id = match task.task_id.clone() {
            Some(id) => id,
            None => {
                self.set_status_msg("Task has no id");
                return;
            }
        };
        let repo_url = match task.repo_url.clone() {
            Some(u) => u,
            None => {
                self.set_status_msg("No repo URL on task");
                return;
            }
        };
        let main_repo = match worktree::find_local_repo(&repo_url) {
            Some(p) => p,
            None => {
                self.set_status_msg("Repo not found locally");
                return;
            }
        };
        self.backend.pull(task_id, main_repo);
        self.set_status_msg("Pulling to local...");
    }

    /// Launch a task from the planning view.
    pub(super) fn launch_from_plan(
        &mut self,
        project: &str,
        slug: &str,
        prompt: &str,
        start_branch: Option<&str>,
        _autostart: bool,
        task_id: &str,
        // Sub-2a Finding #2: parent edge from the planning row,
        // used to initialize the local TaskEntry stub before the
        // first `push_task_tree_to_daemon` fires. Without it, the
        // first push publishes the subtask as top-level and the
        // daemon's auth walk can't authorize parent → subtask
        // until the next reconcile patches it.
        parent_task_id: Option<&str>,
        // `true` when the launch form's branch field held the `.`
        // sentinel: run in the main repo in-place (no worktree, no
        // `cm/<slug>` branch).
        in_place: bool,
    ) {
        // Planning A-l creates a local worktree + spawns a session into it, so
        // the host is local (the global active_host is retired —
        // DESIGN_REMOVE_GLOBAL_HOST.md). The guard is now a structural
        // invariant (always local) kept for symmetry with the other spawn
        // paths until remote TUI-launch lands.
        let active_host = cm_daemon::host_id::HostId::local();
        if let Err(e) = guard_local_host_only(
            &active_host,
            "A-l launch-from-plan",
        ) {
            self.set_status_msg(&format!("{}", e));
            return;
        }
        let repo_url = match self.config.repos.get(project) {
            Some(url) => url.clone(),
            None => {
                self.set_status_msg(&format!("No repo configured for '{}'", project));
                return;
            }
        };

        let main_repo = match worktree::find_local_repo(&repo_url) {
            Some(p) => p,
            None => {
                self.set_status_msg("Repo not found locally");
                return;
            }
        };

        // In-place: cwd IS the main checkout — skip worktree + setup (see
        // `create_local_session` for the rationale). Cloning keeps
        // `worktree_path` byte-equal to `main_repo_path`.
        let worktree_path = if in_place {
            main_repo.clone()
        } else {
            match worktree::create_worktree(&main_repo, slug, start_branch) {
                // `_created` flag (created-vs-reused) is unused on this path.
                Ok((p, _created)) => p,
                Err(e) => {
                    self.set_status_msg(&format!("Worktree: {}", e));
                    return;
                }
            }
        };

        if !in_place {
            worktree::setup_worktree(&main_repo, &worktree_path);
        }

        let (cols, rows) = self.last_term_size;
        // migrate-tui-local: pre-generate UID + workspace id so the
        // daemon can auto-register the workspace at start_session.
        // Route the spawn through the daemon RPC so the planning-
        // launched agent's session lands in state.sessions, not
        // state.tui_sessions.
        let session_uid = new_session_uid();
        let workspace_id_pre = new_workspace_id();
        let pending = Self::list_jsonl_files(&worktree_path);

        let new_sess = match self.try_spawn_via_daemon(
            &session_uid,
            &workspace_id_pre,
            &worktree_path,
            "claude",
            slug,
            None,
            cols,
            rows,
            Some(task_id),
            None,
            None,
            &active_host,
            // launch_from_plan is a fresh spawn — post-spawn
            // detector will discover the transcript path.
            None,
        ) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                self.set_status_msg(&format!("Launch: {}", e));
                return;
            }
            None => {
                self.set_status_msg(
                    "Internal: try_spawn_via_daemon returned None for daemon-eligible 'claude'",
                );
                return;
            }
        };

        // For a normal launch the WIP branch is the freshly-created
        // `cm/<slug>`. For in-place there's no new branch — record the main
        // repo's ACTUAL current branch (e.g. `main`), or `None` on detached
        // HEAD. This value is never a `cm/*` name, so `recover_worktree_path`
        // returns `None` for it and reconcile can't mis-map an in-place task
        // onto a `<repo>-<slug>` worktree dir.
        let branch: Option<String> = if in_place {
            worktree::worktree_current_branch(&main_repo)
        } else {
            Some(format!("cm/{}", slug))
        };
        let mut ts = make_simple_session_with_uid(
            session_uid,
            slug,
            "claude",
            new_sess,
            Some(pending),
        );
        ts.task_id = Some(task_id.to_string());
        ts.host_id = active_host.clone();
        if !prompt.trim().is_empty() {
            ts.pending_prompt = Some(PendingWrite::wait_for_quiet(
                prompt.to_string(),
                false,
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(60),
            ));
        }

        let ws = Workspace {
            color: None,
            pinned: false,
            id: workspace_id_pre,
            name: slug.to_string(),
            is_closed: false,
            is_cloud: false,
            repo_url: Some(repo_url.clone()),
            worktree_path: Some(worktree_path),
            main_repo_path: Some(main_repo),
            worker_vm: None,
            worker_zone: None,
            // Same host the subtask session was spawned on.
            host_id: active_host.clone(),
            sessions: vec![ts],
            tombstones: Vec::new(),
            is_pushing: false,
        };
        let ws_id = ws.id.clone();
        self.workspaces.push(ws);
        let new_wi = self.workspaces.len() - 1;

        self.tasks.push(TaskEntry {
            task_id: Some(task_id.to_string()),
            name: slug.to_string(),
            api_status: TaskStatus::Running,
            repo_url: Some(repo_url.clone()),
            prompt: Some(prompt.to_string()),
            wip_branch: branch.clone(),
            session_id: None,
            blocked_at: None,
            is_cloud: false,
            workspace_id: Some(ws_id),
            // Pin project synchronously so subtask inheritance
            // works before the next reconcile pass — without
            // this, an agent calling `create_subtask` in the
            // first second sees `project: None` on the parent
            // and writes `project = NULL` to the API, which
            // the planning refresh then filters out.
            project: Some(project.to_string()),
            // Sub-2a Finding #2: pin the parent edge from
            // the launch action so the first
            // `push_task_tree_to_daemon` publishes the
            // correct subtask edge — pre-fix this was
            // `None` and the daemon saw the subtask as
            // top-level until reconcile patched it.
            parent_task_id: parent_task_id.map(str::to_string),
            worktree_mode: if in_place {
                WorktreeMode::InPlace
            } else {
                WorktreeMode::Inherit
            },
            metadata: None,
        });

        self.cursor = Cursor::Session(new_wi, 0);
        self.view_mode = ViewMode::Sessions;

        let mut fields = std::collections::HashMap::new();
        fields.insert("status".to_string(), serde_json::Value::String("running".to_string()));
        // Only write `wip_branch` when there's a real branch. In-place on a
        // detached HEAD yields `None` — writing a bogus value would confuse
        // reconcile and cleanup.
        if let Some(b) = branch {
            fields.insert("wip_branch".to_string(), serde_json::Value::String(b));
        }
        // Persist `worktree_mode = "in-place"` so the API row matches the
        // local TaskEntry. Without this, reconcile (which copies the API's
        // `worktree_mode` back into the local entry at `reconcile_tasks`)
        // would overwrite the local `InPlace` with whatever the row was
        // created with — and a stale `"branch"` would make
        // `mark_subtask_done` treat the in-place workspace (whose
        // `worktree_path == main_repo_path`) as a removable worktree. Only
        // written for in-place launches: a normal launch leaves the row's
        // mode untouched (it created a real `cm/<slug>` worktree, and the
        // `mark_subtask_done` is_in_place() guard handles any drift anyway).
        if in_place {
            fields.insert(
                "worktree_mode".to_string(),
                serde_json::Value::String(WorktreeMode::InPlace.as_wire().to_string()),
            );
        }
        self.backend.update_plan_task(task_id.to_string(), fields);
        self.save_session_manifest();
        // Sub-2a Finding #1: launch added a TaskEntry —
        // refresh daemon's tree so any agent that spawns
        // off this task immediately authorizes correctly.
        self.push_state_to_daemon();
        self.set_status_msg("Task launched");
    }

    /// Open workspaces the planning picker can target. Skips closed workspaces
    /// and cloud workspaces (those have no worktree to share).
    pub(super) fn collect_workspace_candidates(&self) -> Vec<WorkspaceCandidate> {
        self.workspaces
            .iter()
            .filter(|w| !w.is_closed && w.worktree_path.is_some())
            .map(|w| WorkspaceCandidate {
                workspace_id: w.id.clone(),
                name: w.name.clone(),
                repo_url: w.repo_url.clone(),
            })
            .collect()
    }

    /// Spawn a new Claude session in an existing workspace and bind the
    /// given task to it. No new worktree — the workspace already has one.
    pub(super) fn launch_into_workspace(
        &mut self,
        workspace_id: &str,
        task_id: &str,
        task_title: &str,
        task_repo_url: &str,
        project: &str,
        prompt: &str,
        // Sub-2a Finding #2: parent edge from the planning row.
        // See `launch_from_plan` for the full backstory.
        parent_task_id: Option<&str>,
    ) {
        // Planning A-l into an existing workspace → use THAT workspace's host
        // (not the global active_host — the old read mistagged a session when
        // the global differed from the workspace's worktree host). The guard
        // then correctly fail-fasts on a remote workspace (TUI-initiated remote
        // launch isn't supported yet — local-only paths can't be sent remote).
        let active_host = self
            .workspaces
            .iter()
            .find(|w| w.id == workspace_id)
            .map(|w| w.host_id.clone())
            .unwrap_or_else(cm_daemon::host_id::HostId::local);
        if let Err(e) = guard_local_host_only(
            &active_host,
            "A-l launch-into-workspace",
        ) {
            self.set_status_msg(&format!("{}", e));
            return;
        }
        let Some(wi) = self.workspace_index_by_id(workspace_id) else {
            self.set_status_msg("Workspace no longer exists");
            return;
        };
        let Some(worktree_path) = self.workspaces[wi].worktree_path.clone() else {
            self.set_status_msg("Workspace has no worktree");
            return;
        };

        let (cols, rows) = self.last_term_size;
        // Pre-generate UID so the per-session MCP config carries the
        // matching CM_TUI_SESSION_ID. Phase 1 "MCP-everywhere" — without
        // this, a session launched into an existing workspace can't call
        // any orchestration tool.
        let session_uid = new_session_uid();
        let pending = Self::list_jsonl_files(&worktree_path);
        let workspace_id_owned = workspace_id.to_string();
        let new_sess = match self.try_spawn_via_daemon(
            &session_uid,
            &workspace_id_owned,
            &worktree_path,
            "claude",
            task_title,
            None,
            cols,
            rows,
            Some(task_id),
            None,
            None,
            &active_host,
            // launch_into_workspace is a fresh spawn — post-
            // spawn detector handles the transcript path.
            None,
        ) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                self.set_status_msg(&format!("Launch: {}", e));
                return;
            }
            None => {
                self.set_status_msg(
                    "Internal: try_spawn_via_daemon returned None for daemon-eligible 'claude'",
                );
                return;
            }
        };
        let mut ts = make_simple_session_with_uid(
            session_uid,
            task_title,
            "claude",
            new_sess,
            Some(pending),
        );
        ts.task_id = Some(task_id.to_string());
        ts.host_id = active_host.clone();
        if !prompt.trim().is_empty() {
            ts.pending_prompt = Some(PendingWrite::wait_for_quiet(
                prompt.to_string(),
                false,
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(60),
            ));
        }
        let si = self.workspaces[wi].sessions.len();
        self.workspaces[wi].sessions.push(ts);

        // The task may be in backlog (not yet in self.tasks because
        // reconcile only pulls running/blocked). Upsert a stub with
        // the workspace binding set; a later reconcile will fill in
        // the remaining API fields without clobbering workspace_id.
        if let Some(task) = self
            .tasks
            .iter_mut()
            .find(|t| t.task_id.as_deref() == Some(task_id))
        {
            task.workspace_id = Some(workspace_id.to_string());
        } else {
            self.tasks.push(TaskEntry {
                task_id: Some(task_id.to_string()),
                name: task_title.to_string(),
                api_status: TaskStatus::Running,
                repo_url: Some(task_repo_url.to_string()),
                prompt: Some(prompt.to_string()),
                wip_branch: None,
                session_id: None,
                blocked_at: None,
                is_cloud: false,
                workspace_id: Some(workspace_id.to_string()),
                // Same race fix as `launch_from_plan` — pin the
                // project synchronously from the planning row so
                // an early `create_subtask` inherits it.
                project: Some(project.to_string()),
                // Sub-2a Finding #2: pin the parent edge so the
                // first `push_task_tree_to_daemon` publishes the
                // correct subtask edge — pre-fix `None` here
                // showed the subtask as top-level on the daemon
                // until reconcile patched it.
                parent_task_id: parent_task_id.map(str::to_string),
                worktree_mode: WorktreeMode::Inherit,
                metadata: None,
            });
        }
        self.cursor = Cursor::Session(wi, si);
        self.view_mode = ViewMode::Sessions;

        let mut fields = std::collections::HashMap::new();
        fields.insert(
            "status".to_string(),
            serde_json::Value::String("running".to_string()),
        );
        self.backend
            .update_plan_task(task_id.to_string(), fields);
        self.save_session_manifest();
        // Sub-2a Finding #1: same as `launch_from_plan`,
        // a launch may have inserted a new TaskEntry.
        self.push_state_to_daemon();
        self.set_status_msg("Task launched into workspace");
    }

    /// Clear a task's workspace binding. Task status is left alone.
    pub(super) fn unbind_task_from_workspace(&mut self, task_id: &str) {
        if let Some(task) = self
            .tasks
            .iter_mut()
            .find(|t| t.task_id.as_deref() == Some(task_id))
        {
            if task.workspace_id.is_some() {
                task.workspace_id = None;
                self.save_session_manifest();
                self.set_status_msg("Task unbound from workspace");
            }
        }
    }

    /// Planning-view counterpart to `reopen_active_workspace`. Resolves the
    /// task's worktree via either its existing workspace binding, an in-memory
    /// workspace matching by recovered path, or a filesystem scan. Refuses
    /// when the worktree directory is gone. Otherwise PATCHes the task back
    /// to `running`, un-archives (or provisions) the workspace, switches to
    /// the Sessions view, and leaves the cursor on the reopened workspace.
    pub(super) fn reopen_task_from_planning(&mut self, task_id: &str) {
        let entry = self
            .tasks
            .iter()
            .find(|t| t.task_id.as_deref() == Some(task_id));
        let (repo_url, wip_branch, workspace_id, is_cloud, name) = match entry {
            Some(t) => (
                t.repo_url.clone(),
                t.wip_branch.clone(),
                t.workspace_id.clone(),
                t.is_cloud,
                t.name.clone(),
            ),
            None => {
                self.set_status_msg("Task not found locally");
                return;
            }
        };
        if is_cloud {
            self.set_status_msg("Cloud tasks aren't reopenable from past");
            return;
        }

        // Locate an existing workspace: by id, else by recovered worktree_path.
        let recovered_path = wip_branch
            .as_deref()
            .zip(repo_url.as_deref())
            .and_then(|(b, r)| worktree::recover_worktree_path(r, b));
        let mut wi: Option<usize> = workspace_id
            .as_deref()
            .and_then(|id| self.workspaces.iter().position(|w| w.id == id));
        if wi.is_none() {
            if let Some(ref wt) = recovered_path {
                wi = self
                    .workspaces
                    .iter()
                    .position(|w| w.worktree_path.as_deref() == Some(wt.as_path()));
            }
        }

        // Resolve the worktree path we'll validate. Prefer the existing
        // workspace's path (might differ from recovered if branch was renamed),
        // fall back to filesystem recovery.
        let worktree_path = wi
            .and_then(|i| self.workspaces[i].worktree_path.clone())
            .or(recovered_path);
        let worktree_path = match worktree_path {
            Some(p) if p.exists() => p,
            Some(p) => {
                self.set_status_msg(&format!(
                    "Worktree gone: {} — task can't be reopened",
                    p.display()
                ));
                return;
            }
            None => {
                self.set_status_msg("Worktree not found on disk — task can't be reopened");
                return;
            }
        };

        // PATCH task → running and update optimistic in-memory state.
        let mut fields = HashMap::new();
        fields.insert(
            "status".to_string(),
            serde_json::Value::String("running".to_string()),
        );
        self.backend.update_task(task_id.to_string(), fields);
        if let Some(entry) = self
            .tasks
            .iter_mut()
            .find(|t| t.task_id.as_deref() == Some(task_id))
        {
            entry.api_status = TaskStatus::Running;
        }
        self.planning.mark_task_running_by_id(task_id);

        // Provision a workspace if none exists yet — the manifest may have
        // dropped the binding when the task last reconciled as done (the
        // reconcile loop skips non-running/blocked tasks).
        let final_wi = match wi {
            Some(i) => i,
            None => {
                let main_repo = repo_url.as_deref().and_then(worktree::find_local_repo);
                let ws = Workspace {
                    color: None,
                    pinned: false,
                    id: new_workspace_id(),
                    name,
                    is_closed: false,
                    is_cloud: false,
                    repo_url: repo_url.clone(),
                    worktree_path: Some(worktree_path),
                    main_repo_path: main_repo,
                    worker_vm: None,
                    worker_zone: None,
                    // API task-sync provisions a local worktree.
                    host_id: cm_daemon::host_id::HostId::local(),
                    sessions: vec![],
                    tombstones: Vec::new(),
                    is_pushing: false,
                };
                let new_ws_id = ws.id.clone();
                self.workspaces.push(ws);
                let idx = self.workspaces.len() - 1;
                if let Some(entry) = self
                    .tasks
                    .iter_mut()
                    .find(|t| t.task_id.as_deref() == Some(task_id))
                {
                    entry.workspace_id = Some(new_ws_id);
                }
                idx
            }
        };

        self.workspaces[final_wi].is_closed = false;
        let respawned = self.resurrect_designer_sessions_for_workspace(final_wi);
        let final_ws_id = self.workspaces[final_wi].id.clone();
        self.cursor = Cursor::Workspace(final_wi);
        self.save_session_manifest();
        // Sub-2b-3 review-5 #2: reopen may have provisioned a
        // new workspace (the `None => { ... }` arm above
        // pushes a fresh Workspace with worktree_path). Push
        // so the daemon knows the path BEFORE the user can
        // A-s an agent into it.
        self.push_state_to_daemon();
        self.view_mode = ViewMode::Sessions;
        self.clamp_cursor();
        let tombstone_count = self.workspaces[final_wi].tombstones.len();
        if tombstone_count > 0 {
            self.input_mode = InputMode::Confirm {
                prompt: format!(
                    "Restore {} closed session{} in this workspace?",
                    tombstone_count,
                    if tombstone_count == 1 { "" } else { "s" },
                ),
                action: ConfirmAction::RestoreTombstones {
                    ws_id: final_ws_id,
                },
            };
        } else if respawned > 0 {
            self.set_status_msg(&format!(
                "Reopened — resurrected {} designer session{}",
                respawned,
                if respawned == 1 { "" } else { "s" },
            ));
        } else {
            self.set_status_msg("Reopened — A-s to add session");
        }
    }

    pub(super) fn unlaunch_task(&mut self, task_id: &str) {
        let mut fields = std::collections::HashMap::new();
        fields.insert("status".to_string(), serde_json::Value::String("backlog".to_string()));
        self.backend.update_plan_task(task_id.to_string(), fields);

        let ws_id = self
            .tasks
            .iter_mut()
            .find(|t| t.task_id.as_deref() == Some(task_id))
            .and_then(|t| {
                t.api_status = TaskStatus::Backlog;
                t.workspace_id.take()
            });

        if let Some(ws_id) = ws_id {
            if let Some(wi) = self.workspaces.iter().position(|w| w.id == ws_id) {
                // Tombstone each session before dropping so post-unlaunch
                // `read_session_output` works for the closed sessions.
                // Helper persists the manifest internally.
                self.tombstone_and_remove(wi, |_| true);
                if let Some(ws) = self.workspaces.get_mut(wi) {
                    ws.is_closed = true;
                }
            }
        }
        self.save_session_manifest();
        self.clamp_cursor();
        self.set_status_msg("Task unlaunched \u{2192} backlog");
    }
}

#[cfg(test)]
mod backtest_watch_tests {
    use super::*;

    #[test]
    fn direct_ssh_args_shape() {
        let args = backtest_watch_ssh_args(
            "cm-bt-abc123",
            "prediction-market-scalper",
            "us-east4-a",
            false,
        );
        assert_eq!(
            &args[..7],
            &[
                "compute".to_string(),
                "ssh".to_string(),
                "cm-bt-abc123".to_string(),
                "--project=prediction-market-scalper".to_string(),
                "--zone=us-east4-a".to_string(),
                "--".to_string(),
                "-t".to_string(),
            ]
        );
        assert_eq!(args.last().unwrap(), BACKTEST_WATCH_REMOTE_CMD);
        // Direct SSH must NOT tunnel through IAP.
        assert!(!args.iter().any(|a| a == "--tunnel-through-iap"));
    }

    #[test]
    fn iap_inserts_tunnel_flag_before_separator() {
        let args = backtest_watch_ssh_args("vm1", "proj", "zone-b", true);
        let tunnel_pos = args
            .iter()
            .position(|a| a == "--tunnel-through-iap")
            .expect("iap flag present");
        let sep_pos = args.iter().position(|a| a == "--").expect("separator present");
        // The IAP flag is a gcloud flag, so it must precede the `--`
        // that hands the rest to ssh.
        assert!(tunnel_pos < sep_pos);
    }

    #[test]
    fn attach_is_read_only_and_targets_root_backtest_tmux() {
        // Read-only (`-r`) is the core safety guarantee — a watcher must
        // never be able to send keystrokes into a live run. Root's tmux
        // server needs `sudo`. The session name is the literal `backtest`.
        let args = backtest_watch_ssh_args("vm", "p", "z", false);
        let remote = args.last().expect("remote command present");
        assert!(remote.contains("tmux attach -r -t backtest"), "got: {remote}");
        assert!(remote.contains("sudo "), "root tmux needs sudo: {remote}");
        // The wrapper waits for the session (VM-create races tmux-create)
        // and never sends keystrokes.
        assert!(remote.contains("has-session -t backtest"), "waits for session: {remote}");
        assert!(!remote.contains("send-keys"), "must never send keys: {remote}");
    }

    #[test]
    fn vm_zone_project_are_discrete_argv_not_shell_interpolated() {
        // Values with shell metacharacters land as isolated argv
        // elements (no shell parses them), and the remote command is a
        // fixed literal — so a hostile VM name cannot inject.
        let args = backtest_watch_ssh_args("a b; rm -rf /", "p'x", "z\"y", false);
        assert!(args.iter().any(|a| a == "a b; rm -rf /"));
        assert!(args.iter().any(|a| a == "--project=p'x"));
        assert!(args.iter().any(|a| a == "--zone=z\"y"));
        // The remote command is a fixed literal — none of the injected
        // content appears in it.
        let remote = args.last().unwrap();
        assert_eq!(remote, BACKTEST_WATCH_REMOTE_CMD);
        assert!(!remote.contains("rm -rf"));
    }

    #[test]
    fn use_iap_env_toggle() {
        // Test the pure parser directly — no process-global env mutation,
        // so this can't race sibling tests in the same binary's threads.
        // Default (unset) = direct SSH.
        assert!(!iap_flag_enabled(None));
        // Recognized truthy tokens enable IAP, including surrounding
        // whitespace (the env read trims via the same code path).
        for truthy in ["1", "true", "yes", "on", " 1 ", "on\n", "\tyes"] {
            assert!(
                iap_flag_enabled(Some(truthy)),
                "‘{truthy}’ should enable IAP"
            );
        }
        // Anything else = direct SSH.
        for falsy in ["0", "false", "no", "off", "", "  ", "maybe", "TRUE"] {
            assert!(
                !iap_flag_enabled(Some(falsy)),
                "‘{falsy}’ should NOT enable IAP"
            );
        }
    }
}

/// Slice 12e: A-H active-host cycling + new-session inheritance
/// + multi-host sidebar grouping + per-session-host routing for
/// the three `_if_attached` helpers.
#[cfg(test)]
mod slice_12e_tests {
    use super::*;
    use cm_daemon::host_id::HostId;

    /// Build an isolated HOME + write a `hosts.toml` listing the
    /// given (name, default) entries. Returns the App with
    /// hosts loaded from that file.
    fn build_app_with_hosts(
        entries: &[(&str, bool)],
        guard: &std::sync::MutexGuard<'static, ()>,
    ) -> (App, tempfile::TempDir) {
        let _ = guard; // keep guard alive
        let tmp = tempfile::tempdir().expect("tempdir");
        let cm_dir = tmp.path().join(".cm");
        std::fs::create_dir_all(&cm_dir).expect("create .cm");
        let mut toml = String::new();
        for (name, default) in entries {
            toml.push_str("[[host]]\n");
            toml.push_str(&format!("name = \"{}\"\n", name));
            if *name == "local" {
                toml.push_str("transport = \"unix\"\n");
                toml.push_str("socket = \"/tmp/local-test.sock\"\n");
            } else {
                toml.push_str("transport = \"ssh-unix\"\n");
                toml.push_str(&format!("ssh_host = \"{}-host\"\n", name));
                toml.push_str(&format!(
                    "remote_socket = \"/remote/{}.sock\"\n",
                    name,
                ));
            }
            if *default {
                toml.push_str("default = true\n");
            }
            toml.push('\n');
        }
        std::fs::write(cm_dir.join("hosts.toml"), &toml)
            .expect("write hosts.toml");
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
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        (app, tmp)
    }

    // (Retired with the global active_host: `t_g3e_active_host_cycle` and
    // `cycle_active_host_single_host_shows_hint` exercised `A-H` /
    // `cycle_active_host`, both removed in DESIGN_REMOVE_GLOBAL_HOST.md Phase D.
    // Host is now a per-workspace attribute — see
    // `launch_into_workspace_guards_on_workspace_host_not_global` and
    // `a_n_form_defaults_host_to_local`.)

    /// Global-host removal: `start_new_session` seeds the form's `host_id` to
    /// `local` — the overwhelmingly common case — rather than a global mode the
    /// operator must remember to set. A non-local host is a per-task pick via
    /// ←/→ on the host field.
    #[test]
    fn a_n_form_defaults_host_to_local() {
        let guard = crate::test_support::home_lock();
        let (mut app, _tmp) = build_app_with_hosts(
            &[("local", true), ("manager", false)],
            &guard,
        );
        // start_new_session bails without a repo; give it one.
        app.config
            .repos
            .insert("r".into(), "https://github.com/a/b".into());
        app.start_new_session();
        match &app.input_mode {
            InputMode::NewSession { host_id, .. } => {
                assert_eq!(
                    *host_id,
                    HostId::local(),
                    "A-n form defaults the host to local (no global mode)",
                );
            }
            _ => panic!("expected NewSession form open"),
        }
    }

    /// Host picker: the offered host list the dispatcher feeds the form is
    /// sourced from the CONFIGURED hosts (`self.hosts.hosts`). Driving the
    /// real `handle_input_event` path, ←/→ on the host field cycles to the
    /// configured `manager` entry.
    #[test]
    fn a_n_form_host_picker_sourced_from_configured_hosts() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |c: KeyCode| {
            CrosstermEvent::Key(KeyEvent::new(c, KeyModifiers::empty()))
        };
        let guard = crate::test_support::home_lock();
        let (mut app, _tmp) = build_app_with_hosts(
            &[("local", true), ("manager", false)],
            &guard,
        );
        app.config
            .repos
            .insert("r".into(), "https://github.com/a/b".into());
        app.start_new_session();
        // Tab from field 0 (repo) to field 5 (host).
        for _ in 0..5 {
            app.handle_input_event(&key(KeyCode::Tab));
        }
        // ←/→ cycles through the configured hosts: local → manager.
        app.handle_input_event(&key(KeyCode::Right));
        match &app.input_mode {
            InputMode::NewSession { host_id, active_field, .. } => {
                assert_eq!(*active_field, 5, "host field should be active");
                assert_eq!(
                    *host_id,
                    HostId::new("manager"),
                    "host picker must cycle through the configured hosts",
                );
            }
            _ => panic!("expected NewSession form open"),
        }
    }

    // (Retired with the global active_host: `t_g3e_new_session_inherits`
    // pinned the `ts.host_id = self.active_host.clone()` injection at the A-n /
    // A-s / A-f call sites. New sessions now inherit host from their WORKSPACE
    // (DESIGN_REMOVE_GLOBAL_HOST.md Phases A–C); see
    // `launch_into_workspace_guards_on_workspace_host_not_global` and
    // `a_n_submit_routes_by_chosen_host`.)

    /// T_g3e_sidebar_groups_per_host (named acceptance test).
    ///
    /// `visual_items_status` emits a `HostHeader` per host
    /// when `hosts.toml` has >1 entry, and groups sessions by
    /// host. Single-host setups render unchanged.
    #[test]
    fn t_g3e_sidebar_groups_per_host() {
        let guard = crate::test_support::home_lock();
        let (mut app, _tmp) = build_app_with_hosts(
            &[("local", true), ("manager", false)],
            &guard,
        );
        // Construct two workspaces, each with one session,
        // pinned to different hosts.
        for (i, host_name) in [(0, "local"), (1, "manager")].iter() {
            let mut ts = make_simple_session_with_uid(
                format!("uid-{}", i),
                &format!("label-{}", i),
                "bash",
                crate::session::Session::new(
                    "/bin/true",
                    &[],
                    80,
                    24,
                    None,
                    HashMap::new(),
                    None,
                )
                .expect("spawn"),
                None,
            );
            ts.host_id = HostId::new(*host_name);
            ts.status = SessionStatus::Idle;
            let ws = Workspace {
                color: None,
                pinned: false,
                id: format!("ws-{}", i),
                name: format!("ws-{}", i),
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
            app.workspaces.push(ws);
        }
        let items = app.visual_items_status();
        let host_headers: Vec<_> = items
            .iter()
            .filter_map(|i| {
                if let VisualItem::HostHeader(h) = i {
                    Some(h.clone())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            host_headers,
            vec![HostId::local(), HostId::new("manager")],
            "multi-host sidebar MUST emit one HostHeader per \
             configured host (in hosts.toml order); got items: {:?}",
            items,
        );

        // Single-host case: no HostHeader.
        // Drop the first guard BEFORE acquiring a second —
        // std::sync::Mutex isn't reentrant and `home_lock()`
        // uses one static mutex.
        drop(_tmp);
        drop(app);
        drop(guard);
        let guard2 = crate::test_support::home_lock();
        let (mut single_app, _tmp2) =
            build_app_with_hosts(&[("local", true)], &guard2);
        single_app.workspaces.push(Workspace {
            color: None,
            pinned: false,
            id: "ws-only".into(),
            name: "ws-only".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: None,
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            is_pushing: false,
            sessions: vec![{
                let mut ts = make_simple_session_with_uid(
                    "uid-only".into(),
                    "label",
                    "bash",
                    crate::session::Session::new(
                        "/bin/true",
                        &[],
                        80,
                        24,
                        None,
                        HashMap::new(),
                        None,
                    )
                    .expect("spawn"),
                    None,
                );
                ts.status = SessionStatus::Idle;
                ts
            }],
            tombstones: Vec::new(),
        });
        let single_items = single_app.visual_items_status();
        let has_host_header = single_items
            .iter()
            .any(|i| matches!(i, VisualItem::HostHeader(_)));
        assert!(
            !has_host_header,
            "single-host setup MUST NOT emit HostHeader rows; \
             got: {:?}",
            single_items,
        );
    }

    /// Continuous members (orchestrators + their subtasks) are excluded from
    /// ALL three main sidebar builders — they live only in the dedicated
    /// continuous column. The normal (non-continuous) session stays. Holds
    /// regardless of `continuous_column_on` (the column's render gate is
    /// separate; the main builders never carry continuous sessions). Replaces
    /// the retired `continuous_sessions_sort_below_under_a_header` /
    /// `hide_continuous_removes_group_from_all_builders` — continuous tasks no
    /// longer render in the main sidebar at all (no `ContinuousHeader` there).
    #[test]
    fn continuous_members_excluded_from_all_main_builders() {
        let guard = crate::test_support::home_lock();
        let mk_ws = || {
            let mk = |uid: &str, label: &str, cont: Option<&str>, mgr: Option<&str>| {
                let mut ts = make_simple_session_with_uid(
                    uid.into(),
                    label,
                    "bash",
                    crate::session::Session::new(
                        "/bin/true",
                        &[],
                        80,
                        24,
                        None,
                        HashMap::new(),
                        None,
                    )
                    .expect("spawn"),
                    None,
                );
                ts.status = SessionStatus::Idle;
                ts.continuous_task_id = cont.map(|s| s.to_string());
                ts.managed_by_uid = mgr.map(|s| s.to_string());
                ts
            };
            Workspace {
                color: None,
                pinned: false,
                id: "ws-0".into(),
                name: "ws-0".into(),
                is_closed: false,
                is_cloud: false,
                repo_url: None,
                worktree_path: None,
                main_repo_path: None,
                worker_vm: None,
                worker_zone: None,
                host_id: cm_daemon::host_id::HostId::local(),
                is_pushing: false,
                sessions: vec![
                    mk("uid-normal", "normal", None, None), // Session(0,0) — stays
                    mk("uid-orch", "orch", Some("ct-1"), None), // Session(0,1) — orchestrator
                    mk("uid-sub", "sub", None, Some("uid-orch")), // Session(0,2) — its subtask
                ],
                tombstones: Vec::new(),
            }
        };
        let assert_excluded = |items: &[VisualItem], builder: &str| {
            assert!(
                !items.iter().any(|i| matches!(i, VisualItem::ContinuousHeader)),
                "{builder}: no ContinuousHeader in the main sidebar; got: {items:?}",
            );
            assert!(
                !items.iter().any(|i| matches!(i, VisualItem::Session(0, 1))),
                "{builder}: the orchestrator MUST be excluded; got: {items:?}",
            );
            assert!(
                !items.iter().any(|i| matches!(i, VisualItem::Session(0, 2))),
                "{builder}: the subtask MUST be excluded; got: {items:?}",
            );
            assert!(
                items.iter().any(|i| matches!(i, VisualItem::Session(0, 0))),
                "{builder}: the normal session MUST survive; got: {items:?}",
            );
        };

        // Single-host: status + task builders, both column states.
        for on in [false, true] {
            let (mut app, _tmp) = build_app_with_hosts(&[("local", true)], &guard);
            app.workspaces.push(mk_ws());
            app.continuous_column_on = on;
            assert_excluded(&app.visual_items_status(), "visual_items_status");
            assert_excluded(&app.visual_items_task(), "visual_items_task");
        }
        // Multi-host status builder (>1 host routes through the multihost path).
        for on in [false, true] {
            let (mut app, _tmp) = build_app_with_hosts(
                &[("local", true), ("manager", false)],
                &guard,
            );
            app.workspaces.push(mk_ws());
            app.continuous_column_on = on;
            assert_excluded(
                &app.visual_items_status_multihost(),
                "visual_items_status_multihost",
            );
        }
    }

    // ---------------------------------------------------------
    // Orchestrator-added tests: route the 3 helpers through the
    // session's pinned host.
    // ---------------------------------------------------------

    /// `kill_daemon_session_routes_to_session_host`: a session
    /// pinned to a non-default host fires its kill RPC against
    /// THAT host's socket, not the default host's.
    ///
    /// We can't directly observe the socket dialed by
    /// `rpc_kill_session` without standing up a daemon, so the
    /// test asserts the host_pool routing decision by reading
    /// the path the helper picks. Verified via inspection of
    /// `host_pool.for_host(&ts.host_id)`'s socket_path output
    /// for both the default and non-default host: they MUST
    /// differ.
    #[test]
    fn kill_daemon_session_routes_to_session_host() {
        let guard = crate::test_support::home_lock();
        let (app, _tmp) = build_app_with_hosts(
            &[("local", true), ("manager", false)],
            &guard,
        );
        let local_path = app
            .host_pool
            .get_handle_for_test(&HostId::local())
            .expect("local handle")
            .socket_path();
        let manager_handle = app
            .host_pool
            .get_handle_for_test(&HostId::new("manager"))
            .expect("manager handle");
        // Pre-spawn the SshUnix handle has no path; build_handle
        // didn't trigger SshTunnel::spawn. The PROOF that
        // routing differs by host is: the two handles are
        // distinct ConnectionHandle instances, and the Unix
        // handle returns Some(local_path) while the SshUnix
        // handle returns None (pre-spawn). Together: the
        // 3-helper routing through `pool.for_host(host_id)`
        // produces DIFFERENT outputs for the two hosts.
        assert!(local_path.is_some());
        assert_eq!(
            manager_handle.socket_path(),
            None,
            "SshUnix handle's path is None pre-first-spawn; \
             after spawn it's a per-spawn random path under \
             the tunnel dir — never equal to the local path",
        );

        // Pin via source-text: each of the 3 helpers calls
        // `host_pool.for_host(&ts.host_id)`.
        let src = crate::app::APP_SRC_FOR_SCAN;
        for helper in &[
            "kill_daemon_session_if_attached",
            "push_transcript_path_to_daemon_if_attached",
        ] {
            // Locate the helper's body via its `pub(crate) fn`
            // signature.
            let sig = format!("pub(crate) fn {}(", helper);
            let start = src.find(&sig).unwrap_or_else(|| {
                panic!("must find `{}` signature", sig)
            });
            // Bound the search to the next `pub(crate) fn` or
            // EOF — close enough heuristic for the body.
            let rest = &src[start..];
            let end = rest[1..]
                .find("\n    pub(crate) fn ")
                .map(|i| 1 + i)
                .unwrap_or(rest.len());
            let body = &rest[..end];
            assert!(
                body.contains("host_pool.for_host(&ts.host_id)"),
                "{}'s body must route through \
                 `host_pool.for_host(&ts.host_id)` (12e); \
                 found:\n{}",
                helper,
                body,
            );
        }
    }

    /// `transcript_path_push_routes_to_session_host`: alias for
    /// the second helper covered by the structural assertion
    /// above. Kept as a separate test entry to match the
    /// prompt's enumeration and surface the failure clearly.
    #[test]
    fn transcript_path_push_routes_to_session_host() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        let sig = "pub(crate) fn push_transcript_path_to_daemon_if_attached(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    pub(crate) fn ")
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("host_pool.for_host(&ts.host_id)"),
            "push_transcript_path_to_daemon_if_attached must \
             route via host_pool.for_host(&ts.host_id); body:\n{}",
            body,
        );
        // Pin: the function takes a `host_pool` parameter so
        // multi-host callers can fan out per session.
        assert!(
            body.contains(
                "host_pool: &crate::host_pool::HostPool"
            ),
            "signature must accept `host_pool` parameter",
        );
    }

    /// 12e (F2 fix): watch consumer reconnects to the NEW
    /// random socket path after an SSH tunnel respawn. Pre-12e
    /// the consumer captured a `PathBuf` at App::new time and
    /// would forever retry the stale path. 12e changed the
    /// signature to a `SocketPathProvider` closure that's
    /// re-invoked on each reconnect.
    #[test]
    fn manifest_watch_reconnects_to_new_socket_after_tunnel_respawn() {
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let tmp = tempfile::tempdir().expect("tempdir");
        let socket1 = tmp.path().join("path-a.sock");
        let socket2 = tmp.path().join("path-b.sock");

        // Build a path_provider that returns `path-a` for the
        // first N calls, then `path-b` for the rest. Mirrors
        // the production case where an SSH tunnel respawn
        // moves the socket to a new random path.
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = Arc::clone(&call_count);
        let socket1_clone = socket1.clone();
        let socket2_clone = socket2.clone();
        let provider: crate::host_pool::SocketPathProvider =
            Arc::new(move || {
                let n = call_count_clone.fetch_add(1, Ordering::SeqCst);
                // First 2 calls → path-a (the original).
                // 3rd+ calls → path-b (post-respawn).
                if n < 2 {
                    Some(socket1_clone.clone())
                } else {
                    Some(socket2_clone.clone())
                }
            });

        // No listener bound at either path → consumer's dial
        // fails on both. Wait until the provider has been
        // called at least 3 times (enough to have switched to
        // path-b at least once).
        let (event_tx, _event_rx) =
            std::sync::mpsc::channel::<crate::manifest_watch::ManifestEvent>();

        let provider_for_thread = Arc::clone(&provider);
        let _thread = std::thread::spawn(move || {
            // Run the consumer for ~3s then exit (the channel
            // disconnect will end it).
            let _ = std::thread::Builder::new()
                .name("test-watch".to_string())
                .spawn(move || {
                    crate::manifest_watch::run_consumer_with_provider(
                        &provider_for_thread,
                        cm_daemon::host_id::HostId::local(),
                        event_tx,
                    );
                });
        });

        // Wait for the provider to be called enough times to
        // have moved past path-a — proves the consumer
        // re-invokes the provider on each reconnect attempt.
        // The 1s backoff means waiting up to ~5s should
        // exercise 3-4 reconnect attempts.
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(8);
        let mut saw_path_b_query = false;
        while std::time::Instant::now() < deadline {
            if call_count.load(Ordering::SeqCst) >= 3 {
                saw_path_b_query = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            saw_path_b_query,
            "consumer must have called the path_provider at \
             least 3 times within 8s (proof that it re-fetches \
             the path on each reconnect, not just at startup); \
             actual calls: {}",
            call_count.load(Ordering::SeqCst),
        );

        // Also pin (structural) that the swap from path-a to
        // path-b actually flows through to the dial: the
        // provider's return value is what the consumer uses
        // for its connect attempt. We've already proven the
        // provider IS being re-called; the connect side is a
        // direct read of that return value (see
        // `run_consumer_with_provider`'s loop body). Bind
        // path-b on the SUT side as a final sanity check:
        // a real listener appears + we let the consumer's
        // next attempt succeed.
        let _listener_b = UnixListener::bind(&socket2).expect("bind b");
        // Drop the listener immediately — we're not actually
        // exchanging frames here. The point: the consumer
        // dialed it (or would; failure is also OK), without
        // crashing.
        let _ = _listener_b;
    }

    // ---------------------------------------------------------
    // 12e Round 2 reviewer findings: regression tests.
    // ---------------------------------------------------------

    /// 12e-r2 F1: when `active_host` is a non-default host,
    /// the daemon-spawn path MUST dial that host's socket, not
    /// the default host's. Pre-r2 `try_spawn_via_daemon` went
    /// through `default_handle()` while the resulting
    /// `TerminalSession.host_id` was tagged with `active_host`
    /// — every subsequent per-session RPC would then route to
    /// a daemon that had no record of the UID.
    ///
    /// Structural pin: assert `try_spawn_via_daemon`'s body
    /// routes through `host_pool.for_host(host_id)` (the new
    /// parameter), NOT `host_pool.default_handle()`. The
    /// `host_id` parameter must exist in the signature.
    #[test]
    fn spawn_via_daemon_routes_to_host_id_not_default() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        // Locate the function body bounded by the signature
        // line and the closing `}`.
        let sig = "pub fn try_spawn_via_daemon(";
        let start = src.find(sig).expect("must find sig");
        // Bound by the next `pub fn` / `fn ` at indent 4.
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    pub fn ")
            .or_else(|| rest[1..].find("\n    fn "))
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // The function must accept `host_id: &cm_daemon::host_id::HostId`.
        assert!(
            body.contains("host_id: &cm_daemon::host_id::HostId"),
            "try_spawn_via_daemon signature MUST accept a \
             `host_id: &HostId` parameter so callers can pin \
             the spawn to a specific host; body:\n{}",
            body,
        );

        // The dial site must use for_host(host_id), NOT
        // default_handle().
        assert!(
            body.contains("host_pool.for_host(host_id)"),
            "try_spawn_via_daemon MUST route through \
             `host_pool.for_host(host_id)`; body:\n{}",
            body,
        );
        assert!(
            !body.contains("host_pool.default_handle()"),
            "try_spawn_via_daemon MUST NOT call \
             `host_pool.default_handle()` — that ignores the \
             caller's active_host choice (12e-r2 F1); body:\n{}",
            body,
        );

        // The two production callers (create_local_session,
        // spawn_session_on_workspace) must SNAPSHOT the host ONCE
        // at the top of the function and pass that snapshot through
        // to BOTH the spawn call AND the TerminalSession.host_id
        // assignment. Pin structurally — the markers are the
        // comments and the snapshot variable name.
        // create_local_session (A-n) snapshots the CHOSEN host param
        // (the host-picker choice, defaulting to active_host) — NOT
        // self.active_host directly.
        assert!(
            src.contains("passed in as `chosen_host`")
                && src.contains("Snapshot it ONCE here"),
            "create_local_session must snapshot the chosen host param once \
             for a new workspace",
        );
        // spawn_session_on_workspace (A-s) resolves the spawn host from the
        // WORKSPACE (its worktree's host), NOT the global active_host — host is
        // a workspace property; active_host only seeds new workspaces.
        assert!(
            src.contains(
                "Host is a property of the WORKSPACE, not the global active_host",
            ),
            "spawn_session_on_workspace must resolve the spawn host from the \
             workspace, not the global active_host",
        );
        // And the call sites pass &active_host (the snapshot).
        // migrate-tui-local Issue 3: a `transcript_path` arg
        // follows `&active_host,` now, so the needle is the
        // snapshot reference alone (with trailing comma).
        assert!(
            src.contains("&active_host,"),
            "create_local_session must pass &active_host to \
             try_spawn_via_daemon",
        );
    }

    /// 12e-r2 F2 (Option A): one watch consumer per
    /// configured host. A multi-host setup spawns N
    /// manifest.watch + N events.subscribe consumers, all live
    /// from startup, so sessions on any host get streaming
    /// events — independent of which host new sessions target
    /// (host is now a per-workspace attribute, not a global
    /// switch; DESIGN_REMOVE_GLOBAL_HOST.md).
    #[test]
    fn watch_consumers_one_per_configured_host() {
        let guard = crate::test_support::home_lock();
        let (app, _tmp) = build_app_with_hosts(
            &[("local", true), ("manager", false)],
            &guard,
        );
        // 2 hosts → 2 manifest + 2 workflow consumers.
        let manifest_n = app._manifest_watch_threads.len();
        let workflow_n = app._workflow_watch_threads.len();
        assert_eq!(
            manifest_n, 2,
            "multi-host setup MUST spawn one manifest.watch \
             consumer per host (12e-r2 F2 Option A); got {} \
             for {} hosts",
            manifest_n,
            app.hosts.hosts.len(),
        );
        assert_eq!(
            workflow_n, 2,
            "multi-host setup MUST spawn one events.subscribe \
             consumer per host",
        );

        // None of the threads should have died on their own
        // (consumers loop forever; they only exit on channel
        // disconnect or process exit). `is_finished` returns
        // true only if the thread has exited.
        for (i, t) in app._manifest_watch_threads.iter().enumerate() {
            assert!(
                !t.is_finished(),
                "manifest watch consumer #{} should be alive",
                i,
            );
        }
        for (i, t) in app._workflow_watch_threads.iter().enumerate() {
            assert!(
                !t.is_finished(),
                "workflow watch consumer #{} should be alive",
                i,
            );
        }
    }

    /// 12e-r2 F2: single-host setup still gets exactly one
    /// consumer of each type. Pre-r2 it was also one
    /// consumer, so this is a no-overhead regression-pin.
    #[test]
    fn watch_consumers_single_host_count_is_one() {
        let guard = crate::test_support::home_lock();
        let (app, _tmp) = build_app_with_hosts(
            &[("local", true)],
            &guard,
        );
        assert_eq!(app._manifest_watch_threads.len(), 1);
        assert_eq!(app._workflow_watch_threads.len(), 1);
    }

    // ---------------------------------------------------------
    // 12e Round 3 reviewer findings.
    // ---------------------------------------------------------

    /// 12e-r8 F1 (named acceptance): `spawn_managed_session`
    /// MUST derive its target host from the CALLER's
    /// `host_id` (resolved via `resolve_caller_host`). An
    /// agent's spawn rights belong to the agent's context —
    /// same shape as Unix `fork()` inheriting the parent's cwd.
    ///
    /// Historically the guard gated on the (now-removed) global
    /// `active_host`, so an operator viewing a non-local host
    /// could fail-fast a local-rooted agent's `start_session`
    /// even though `spawn on local where caller lives` was the
    /// right answer. With the global host retired
    /// (DESIGN_REMOVE_GLOBAL_HOST.md) the host is derived purely
    /// from the caller's session — this pins that.
    #[test]
    fn mcp_start_session_inherits_caller_host() {
        use cm_daemon::host_id::HostId;

        let src = crate::app::APP_SRC_FOR_SCAN;

        // Structural: resolve_caller_host helper exists +
        // walks self.workspaces.sessions to find the caller.
        let resolver_sig =
            "pub(crate) fn resolve_caller_host(\n        &self,\n        caller_uid: &str,\n    ) -> std::io::Result<cm_daemon::host_id::HostId>";
        assert!(
            src.contains(resolver_sig),
            "resolve_caller_host helper must exist with the \
             expected signature (12e-r8 F1)",
        );

        // Structural: spawn_managed_session uses caller_host,
        // NOT active_host.
        let sig = "pub fn spawn_managed_session(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    pub fn ")
            .or_else(|| rest[1..].find("\n    pub(crate) fn "))
            .or_else(|| rest[1..].find("\n    fn "))
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("let caller_host = self.resolve_caller_host(caller_uid)?"),
            "spawn_managed_session must resolve caller_host \
             via resolve_caller_host (12e-r8 F1); body:\n{}",
            body,
        );
        assert!(
            body.contains("guard_local_host_only(&caller_host, \"MCP `start_session`\")"),
            "guard MUST check caller_host, not active_host",
        );
        assert!(
            body.contains("&caller_host,") &&
            body.contains("host_id: caller_host.clone()"),
            "spawn must pass &caller_host to try_spawn_via_daemon \
             AND tag the new session with caller_host.clone()",
        );
        // Regression pin: the active_host snapshot is gone.
        assert!(
            !body.contains("let active_host = self.active_host.clone();"),
            "spawn_managed_session MUST NOT snapshot \
             self.active_host (12e-r8 F1 — pre-r8 the guard \
             gated on UI state, rejecting local-rooted agent \
             spawns when operator was viewing manager)",
        );

        // Runtime: a local-rooted caller resolves to local and
        // the spawn routes there — derived purely from the
        // caller's own session, no global host in the picture.
        let guard = crate::test_support::home_lock();
        let (mut app, _tmp) = build_app_with_hosts(
            &[("local", true), ("manager", false)],
            &guard,
        );

        // Plant a TUI session with host_id=local as the
        // caller. Use a workspace with a worktree so
        // spawn_managed_session gets past the worktree
        // lookup before we observe the guard outcome.
        let tmpdir = tempfile::tempdir().expect("worktree tempdir");
        let mut caller_ts = make_simple_session_with_uid(
            "caller-local-uid".into(),
            "caller",
            "bash",
            crate::session::Session::new(
                "/bin/true",
                &[],
                80,
                24,
                None,
                HashMap::new(),
                None,
            )
            .expect("dummy session"),
            None,
        );
        caller_ts.host_id = HostId::local();
        app.workspaces.push(Workspace {
            color: None,
            pinned: false,
            id: "ws-local-caller".into(),
            name: "ws-local-caller".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: Some(tmpdir.path().to_path_buf()),
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            is_pushing: false,
            sessions: vec![caller_ts],
            tombstones: Vec::new(),
        });

        // Sanity: resolver picks up the planted caller's
        // host (local), not active_host (manager).
        let resolved = app
            .resolve_caller_host("caller-local-uid")
            .expect("resolver must find planted caller");
        assert_eq!(
            resolved,
            HostId::local(),
            "resolve_caller_host MUST return the caller's \
             pinned host_id, NOT active_host",
        );

        // Symmetric: unknown caller → NotFound error.
        let missing = app.resolve_caller_host("ghost-uid");
        assert!(
            missing.is_err(),
            "resolve_caller_host on a missing uid MUST error",
        );

        // End-to-end: spawn_managed_session WOULD NOT
        // fail-fast on a local-rooted caller despite
        // active_host=manager. Calling the real spawn
        // requires a working PTY + daemon socket — out of
        // scope for a unit test. The structural pins above
        // cover that the function consults caller_host, not
        // active_host.
        drop(guard);
    }


    /// 12e-r4 F1.1 / 12e-r5 F2: `try_spawn_via_daemon` MUST
    /// accept the design-doc-correct wire vocabulary
    /// `"claude-code"` as an alias for internal-legacy
    /// `"claude"`. Round 4 added the alias inline in TWO
    /// match sites; round 5 collapsed it to a single
    /// `normalize_session_type_to_internal` helper called
    /// ONCE at the top of `try_spawn_via_daemon` — so all
    /// downstream consumers (argv match, wire_session_type
    /// mapping, memory_cap_for lookup) consult the same
    /// internal form.
    #[test]
    fn mcp_start_session_with_claude_code_type_routes_to_daemon() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        // The single-helper normalize function must exist
        // and explicitly map "claude-code" → "claude".
        let norm_sig = "fn normalize_session_type_to_internal(";
        let norm_start = src.find(norm_sig).expect(
            "normalize_session_type_to_internal helper must exist",
        );
        let norm_rest = &src[norm_start..];
        let norm_end = norm_rest[1..]
            .find("\n}\n")
            .map(|i| 1 + i + 2)
            .unwrap_or(norm_rest.len());
        let norm_body = &norm_rest[..norm_end];
        assert!(
            norm_body.contains("\"claude-code\" => \"claude\""),
            "normalize_session_type_to_internal must map \
             \"claude-code\" to \"claude\" (12e-r5 F2); \
             body:\n{}",
            norm_body,
        );

        // migrate-tui-local: the daemon-routing body moved
        // from `App::try_spawn_via_daemon` (which is now a
        // thin wrapper) to the free helper
        // `try_spawn_via_daemon_with_deps`. Inspect the free
        // helper's body for the normalize-once / consult-
        // internal-everywhere invariants.
        let sig = "pub(crate) fn try_spawn_via_daemon_with_deps(";
        let start = src.find(sig).expect(
            "try_spawn_via_daemon_with_deps free helper must exist",
        );
        let rest = &src[start..];
        let end = rest[1..]
            .find("\npub ")
            .or_else(|| rest[1..].find("\nfn "))
            .or_else(|| rest[1..].find("\nimpl "))
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // The function MUST call the normalize helper once
        // and capture as `internal_session_type`.
        assert!(
            body.contains(
                "let internal_session_type = normalize_session_type_to_internal(session_type)",
            ),
            "try_spawn_via_daemon_with_deps must call \
             normalize_session_type_to_internal ONCE at the \
             top and capture as `internal_session_type` \
             (12e-r5 F2); body:\n{}",
            body,
        );

        // All three downstream sites — argv match,
        // wire_session_type match, and memory_cap_for lookup
        // — MUST consult `internal_session_type`, NOT the raw
        // `session_type`.
        assert!(
            body.contains("let argv_result = match internal_session_type"),
            "argv match must consult internal_session_type",
        );
        assert!(
            body.contains("let wire_session_type = match internal_session_type"),
            "wire_session_type match must consult internal_session_type",
        );
        assert!(
            body.contains("memory_cap_for(internal_session_type)"),
            "memory_cap_for lookup MUST use internal_session_type \
             — pre-r5 it consulted raw session_type, producing \
             a bogus `CM_SESSION_MEM_SOFT_CLAUDE-CODE` env var \
             lookup that always missed",
        );

        // Regression pin: no raw-`session_type` consumer
        // remains for the cap-lookup.
        assert!(
            !body.contains("memory_cap_for(session_type)"),
            "memory_cap_for MUST NOT use the raw session_type \
             (12e-r5 F2 regression pin)",
        );
    }

    /// 12e-r5 F2: structural pin on the per-engine
    /// `memory_cap_for` env-var lookup using the normalized
    /// (internal) vocabulary. This is the regression-pin for
    /// "claude-code" → `CM_SESSION_MEM_SOFT_CLAUDE` (not
    /// `CM_SESSION_MEM_SOFT_CLAUDE-CODE`). The actual env-var
    /// resolution is covered by existing `config` cap tests;
    /// what this pins is that the call site here uses the
    /// internal form.
    #[test]
    fn mcp_start_session_claude_code_uses_claude_memory_cap_env_vars() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        // migrate-tui-local: inspect the free helper, not the
        // App method (which is now a thin wrapper).
        let sig = "pub(crate) fn try_spawn_via_daemon_with_deps(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\npub ")
            .or_else(|| rest[1..].find("\nfn "))
            .or_else(|| rest[1..].find("\nimpl "))
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("memory_cap_for(internal_session_type)"),
            "cap lookup must use the normalized internal \
             vocabulary so an MCP `start_session(type=\
             \"claude-code\")` resolves the same cap env vars \
             as `type=\"claude\"`",
        );
    }

    /// 12e-r5 F1 + r6 (shared helper): `spawn_managed_session`
    /// MUST refuse to spawn when `active_host != HostId::local()`.
    /// Round 5 inlined the guard; round 6 collapsed it into
    /// the shared `guard_local_host_only` helper so every
    /// spawn site uses the same message format. Test pins the
    /// helper-call shape and the runtime behavior.
    /// 12e-r8 F1 (renamed from r5's `..._for_non_local_active_host`):
    /// `spawn_managed_session` MUST refuse the spawn when
    /// the CALLER's host_id is non-local. The pre-r8 form
    /// gated on `active_host`; round 8 routes through the
    /// caller's pinned host (resolved via
    /// `resolve_caller_host`).
    #[test]
    fn mcp_start_session_fails_fast_for_non_local_caller_host() {
        use cm_daemon::host_id::HostId;

        let src = crate::app::APP_SRC_FOR_SCAN;

        let sig = "pub fn spawn_managed_session(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    pub fn ")
            .or_else(|| rest[1..].find("\n    pub(crate) fn "))
            .or_else(|| rest[1..].find("\n    fn "))
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // Structural pin: guard on caller_host.
        assert!(
            body.contains(
                "guard_local_host_only(&caller_host, \"MCP `start_session`\")",
            ),
            "spawn_managed_session must call \
             guard_local_host_only(&caller_host, ...) — \
             route through the CALLER's host, not active_host \
             (12e-r8 F1); body:\n{}",
            body,
        );

        // Ordering: the resolve + guard MUST precede
        // `try_spawn_via_daemon`.
        let resolve_idx = body
            .find("let caller_host = self.resolve_caller_host(caller_uid)?")
            .expect("resolve_caller_host call not found");
        let guard_idx = body
            .find("guard_local_host_only(&caller_host,")
            .expect("guard call not found");
        let spawn_idx = body
            .find("self.try_spawn_via_daemon(")
            .expect("try_spawn_via_daemon call not found");
        assert!(
            resolve_idx < guard_idx && guard_idx < spawn_idx,
            "resolve_caller_host MUST precede guard MUST \
             precede try_spawn_via_daemon — got resolve={}, \
             guard={}, spawn={}",
            resolve_idx,
            guard_idx,
            spawn_idx,
        );

        // Runtime: plant a CALLER pinned to manager; the
        // spawn MUST fail-fast — the guard keys off the
        // caller's own host_id.
        let guard = crate::test_support::home_lock();
        let (mut app, _tmp) = build_app_with_hosts(
            &[("local", true), ("manager", false)],
            &guard,
        );

        let tmpdir = tempfile::tempdir().expect("worktree tempdir");
        let mut caller_ts = make_simple_session_with_uid(
            "caller-on-manager".into(),
            "caller",
            "bash",
            crate::session::Session::new(
                "/bin/true",
                &[],
                80,
                24,
                None,
                HashMap::new(),
                None,
            )
            .expect("dummy session"),
            None,
        );
        caller_ts.host_id = HostId::new("manager");
        app.workspaces.push(Workspace {
            color: None,
            pinned: false,
            id: "ws-mgr-caller".into(),
            name: "ws-mgr-caller".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: Some(tmpdir.path().to_path_buf()),
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            is_pushing: false,
            sessions: vec![caller_ts],
            tombstones: Vec::new(),
        });
        let result = app.spawn_managed_session(
            app.workspaces.len() - 1,
            "caller-on-manager",
            "claude",
            "test-label",
            None,
            None,
            false,
        );
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("manager"),
                    "error must name the offending host; got: {}",
                    msg,
                );
                assert!(
                    msg.contains("MCP `start_session`"),
                    "error must name the op; got: {}",
                    msg,
                );
            }
            Ok(uid) => panic!(
                "spawn_managed_session MUST fail when caller's \
                 host_id is non-local; got Ok({})",
                uid,
            ),
        }
        drop(guard);
    }

    /// 12e-r6: the shared `guard_local_host_only` helper
    /// MUST exist with the expected signature + error
    /// content. Every fail-fast spawn site uses it; this
    /// pins the helper itself so a refactor that renames or
    /// inlines it back trips clearly.
    #[test]
    fn guard_local_host_only_helper_exists() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        assert!(
            src.contains(
                "pub(crate) fn guard_local_host_only(\n    host_id: &cm_daemon::host_id::HostId,\n    op_name: &str,\n) -> std::io::Result<()>",
            ),
            "shared `guard_local_host_only` helper must exist \
             with the expected signature (12e-r6)",
        );
        // The body must surface Phase 3 / slice 12g in the
        // error message so operators have a clear pointer
        // when the guard fires.
        let sig = "pub(crate) fn guard_local_host_only(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n}\n")
            .map(|i| 1 + i + 2)
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("daemon/NOTES.md slice 12g")
                || body.contains("Phase 3"),
            "guard helper's error message must point at the \
             follow-up slice; body:\n{}",
            body,
        );
        // Local host MUST pass the guard.
        assert!(
            body.contains(
                "if host_id == &cm_daemon::host_id::HostId::local()",
            ),
            "guard must allow local host (return Ok) and \
             only fail on non-local",
        );
    }

    /// 12e-r6 F1: `create_local_session` (A-n) MUST call the
    /// shared guard BEFORE worktree creation. Pre-r6 a
    /// non-local active_host would proceed through worktree
    /// creation, then fail at the daemon spawn — leaving an
    /// orphan worktree on disk.
    #[test]
    fn create_local_session_fails_fast_before_worktree() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        let sig = "fn create_local_session(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // Phase 3 (remote-session-execution): the local-only guard was
        // replaced by a remote-host DISPATCH. A non-local active host MUST
        // route to `create_remote_session` BEFORE any local worktree / spawn
        // work — otherwise a remote A-n would build a local worktree (orphan
        // dir) and a local spawn it can't use.
        assert!(
            body.contains("self.create_remote_session("),
            "create_local_session must dispatch a non-local active host to \
             create_remote_session; body:\n{}",
            body,
        );
        let dispatch_idx = body
            .find("if active_host != crate::hosts::HostId::local()")
            .expect("remote-host dispatch not found");
        let worktree_idx = body
            .find("worktree::create_worktree(")
            .expect("worktree::create_worktree call not found");
        assert!(
            dispatch_idx < worktree_idx,
            "remote dispatch (at byte {}) MUST precede \
             worktree::create_worktree (at byte {}) — otherwise a remote-host \
             A-n leaves an orphan local worktree dir",
            dispatch_idx,
            worktree_idx,
        );
        let spawn_idx = body
            .find("self.try_spawn_via_daemon(")
            .expect("try_spawn_via_daemon call not found");
        assert!(
            dispatch_idx < spawn_idx,
            "remote dispatch must precede try_spawn_via_daemon (the local path)",
        );
    }

    /// 12e-r6 F1: `spawn_session_on_workspace` (A-s) MUST
    /// call the shared guard before any spawn work.
    #[test]
    fn spawn_session_on_workspace_fails_fast_for_non_local_host() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        let sig = "fn spawn_session_on_workspace(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // Phase 3 (remote-session-execution): the local-only guard was
        // replaced by a remote-host DISPATCH on the WORKSPACE's resolved spawn
        // host (host is a workspace property, not the global active_host). A
        // remote-hosted workspace MUST route to `add_remote_session` BEFORE the
        // local spawn path.
        assert!(
            body.contains("self.add_remote_session("),
            "spawn_session_on_workspace must dispatch a remote-hosted workspace \
             to add_remote_session; body:\n{}",
            body,
        );
        let dispatch_idx = body
            .find("if spawn_host != crate::hosts::HostId::local()")
            .expect("remote-host dispatch not found");
        let spawn_idx = body
            .find("self.try_spawn_via_daemon(")
            .expect("try_spawn_via_daemon call not found");
        assert!(
            dispatch_idx < spawn_idx,
            "remote dispatch must precede try_spawn_via_daemon (the local path)",
        );
    }

    /// Phase 4 (remote-session-execution): the 12e-r7 local-only
    /// `guard_local_host_only(&entry.host_id, ...)` SKIP in
    /// `restore_sessions` is replaced by a remote REATTACH branch:
    /// a non-local entry routes to `try_reattach_remote_session`
    /// (which attaches over its host's socket, ungated) and, on
    /// failure, PRESERVES the raw entry in `skipped_manifest_entries`
    /// (the 12e-r7 F1 data-loss protection is retained). The local
    /// path (`spawn_restored_session`) is untouched.
    #[test]
    fn restore_sessions_reattaches_remote_else_preserves() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        let sig = "fn restore_sessions(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // The local-only SKIP guard is GONE — a non-local entry is no
        // longer fail-fast-skipped; it reattaches.
        assert!(
            !body.contains("guard_local_host_only(&entry.host_id")
                && !body.contains(
                    "guard_local_host_only(\n                        &entry.host_id,",
                ),
            "Phase 4 removes the local-only restore skip guard on \
             entry.host_id (replaced by a remote reattach branch); body:\n{}",
            body,
        );

        // Structural pin: a non-local entry routes to the remote reattach
        // helper.
        assert!(
            body.contains("self.try_reattach_remote_session("),
            "restore_sessions must reattach a non-local entry via \
             try_reattach_remote_session; body:\n{}",
            body,
        );
        let remote_branch_idx = body
            .find("if entry.host_id != cm_daemon::host_id::HostId::local()")
            .expect("remote-entry branch not found");
        let spawn_idx = body
            .find("self.spawn_restored_session(")
            .expect("spawn_restored_session (local path) call not found");
        assert!(
            remote_branch_idx < spawn_idx,
            "the remote reattach branch must precede the local \
             spawn_restored_session path",
        );

        // The reattach-failure path MUST preserve the raw entry in
        // `skipped_manifest_entries` — the 12e-r7 F1 data-loss protection,
        // retained (a remote session that can't be reattached is preserved,
        // never dropped, since remote re-spawn from restore is out of scope).
        assert!(
            body.contains("self.skipped_manifest_entries"),
            "restore_sessions must preserve a non-reattachable remote entry \
             in skipped_manifest_entries (no data-loss regression); body:\n{}",
            body,
        );

        // Phase 4 startup-freeze fix — local-path-unchanged structural pin:
        // the deferral that moves blocking remote dials off the main thread is
        // gated on `self.host_pool.dial_may_block(...)` and lives INSIDE the
        // non-local remote branch (after `remote_branch_idx`, before the local
        // `spawn_restored_session`). So a LOCAL entry — which never enters the
        // remote branch — can never be deferred or queued: the local restore
        // path is byte-for-byte unchanged.
        let defer_idx = body
            .find("self.host_pool.dial_may_block(&entry.host_id)")
            .expect(
                "restore_sessions must gate the off-main-thread deferral on \
                 host_pool.dial_may_block",
            );
        assert!(
            remote_branch_idx < defer_idx && defer_idx < spawn_idx,
            "the dial_may_block deferral must sit inside the non-local remote \
             branch (so local entries never reach it)",
        );
        assert!(
            body.contains("self.pending_remote_reattach.push("),
            "the deferral must queue the entry in pending_remote_reattach for \
             the background-warmed reattach",
        );
    }

    /// 12e-r7 F1 (named acceptance): the round-6 restore
    /// guard caused a data-loss regression — entries with
    /// non-local `host_id` were filtered out at restore
    /// AND `save_session_manifest` only serialized live
    /// `ws.sessions`, so the next save dropped them from
    /// disk forever. Round 7 fixes this by preserving the
    /// raw ManifestEntry in `App.skipped_manifest_entries`
    /// and appending those entries during save.
    ///
    /// End-to-end: write a manifest with a remote-pinned
    /// entry, restore, save, re-load, assert the remote-
    /// pinned entry is still there with all fields intact.
    #[test]
    fn spawn_restored_session_skip_preserves_manifest_entry_across_save_cycle() {
        use cm_daemon::host_id::HostId;
        use cm_daemon::manifest::{
            Manifest, ManifestEntry, ManifestWorkspace,
        };

        let guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let cm_dir = tmp.path().join(".cm");
        std::fs::create_dir_all(&cm_dir).expect("create .cm");

        // Build a manifest containing a workspace with TWO
        // sessions: one local (will restore via fail-fast-OK
        // path → live state), one pinned to `manager` (will
        // be skipped + preserved).
        let mut workspaces = HashMap::new();
        let local_entry = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: "uid-local".into(),
            managed_by_uid: None,
            generation: 0,
            label: "local-sess".into(),
            session_type: "bash".into(),
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
            host_id: HostId::local(),
        };
        let remote_entry = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: "uid-remote".into(),
            managed_by_uid: Some("agent-X".into()),
            generation: 7,
            label: "manager-sess".into(),
            session_type: "claude".into(),
            transcript_id: Some("sid-deadbeef".into()),
            hidden: true,
            idle_timeout_secs: 90,
            burst_threshold: 3,
            workflow_run_id: Some("wf_abc".into()),
            workflow_role: Some("worker".into()),
            continuous_task_id: None,
            task_id: Some("task_xyz".into()),
            notify_on_idle: true,
            seeded_from_snapshot: Some("snap1".into()),
            last_exit: None,
            host_id: HostId::new("manager"),
            global_perms: false,
        };
        let ws = ManifestWorkspace {
            color: None,
            pinned: false,
            id: "ws-acc".into(),
            name: "ws-acc".into(),
            is_closed: false,
            is_cloud: false,
            worktree_path: None,
            main_repo_path: None,
            repo_url: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![local_entry.clone(), remote_entry.clone()],
            tombstones: Vec::new(),
        };
        workspaces.insert(ws.id.clone(), ws);
        let manifest = Manifest {
            task_colors: Default::default(),
            workspaces,
            bindings: HashMap::new(),
            view: Some("status".to_string()),
            hide_continuous: false,
            continuous_column_on: false,
        };
        std::fs::write(
            cm_dir.join("tui-sessions.json"),
            serde_json::to_string(&manifest).expect("ser"),
        )
        .expect("write manifest");

        // Also write a hosts.toml so `manager` is a valid
        // host id in the App's pool (otherwise the App might
        // fall back to local-only and the guard wouldn't
        // distinguish the two entries).
        std::fs::write(
            cm_dir.join("hosts.toml"),
            r#"
[[host]]
name = "local"
transport = "unix"
socket = "/tmp/local-test.sock"
default = true

[[host]]
name = "manager"
transport = "ssh-unix"
ssh_host = "manager-host"
remote_socket = "/remote/manager.sock"
"#,
        )
        .expect("write hosts.toml");

        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        // App::new doesn't call restore_sessions (it's
        // triggered later by `drain_backend_events` on the
        // first `TasksUpdated` event). For this test we
        // invoke it directly.
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        app.restore_sessions();

        // Verify the skipped entry was preserved in App state.
        let skipped = app
            .skipped_manifest_entries
            .get("ws-acc")
            .expect("ws-acc entry must have skipped sessions");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].uid, "uid-remote");
        assert_eq!(skipped[0].host_id, HostId::new("manager"));

        // Now drive a save_session_manifest. The remote entry
        // MUST round-trip back to disk.
        //
        // `save_session_manifest` no-ops until `sessions_restored` is set
        // (the guard that prevents the "lost sessions on restart" clobber).
        // Production reaches restore via `maybe_restore_sessions`, which sets
        // the flag; this test calls `restore_sessions` directly, so set it
        // explicitly here — otherwise the save is skipped and this test would
        // pass spuriously off the still-present original file.
        app.sessions_restored = true;
        app.save_session_manifest();

        // Re-load and verify the remote entry is intact.
        let reloaded_bytes = std::fs::read_to_string(
            cm_dir.join("tui-sessions.json"),
        )
        .expect("read after save");
        let reloaded: Manifest = serde_json::from_str(&reloaded_bytes)
            .expect("parse manifest after save");
        let reloaded_ws = reloaded
            .workspaces
            .get("ws-acc")
            .expect("ws-acc must be in the reloaded manifest");
        let reloaded_remote = reloaded_ws
            .sessions
            .iter()
            .find(|e| e.uid == "uid-remote")
            .expect(
                "remote-pinned entry uid-remote MUST round-trip \
                 through the save — round-6's drop-and-overwrite \
                 was the data-loss regression this test pins",
            );
        // Pin every load-bearing field — full structural
        // equality on the entry that was skipped + saved.
        assert_eq!(reloaded_remote.uid, remote_entry.uid);
        assert_eq!(
            reloaded_remote.managed_by_uid,
            remote_entry.managed_by_uid,
        );
        assert_eq!(reloaded_remote.generation, remote_entry.generation);
        assert_eq!(reloaded_remote.label, remote_entry.label);
        assert_eq!(
            reloaded_remote.session_type,
            remote_entry.session_type,
        );
        assert_eq!(
            reloaded_remote.transcript_id,
            remote_entry.transcript_id,
        );
        assert_eq!(reloaded_remote.hidden, remote_entry.hidden);
        assert_eq!(
            reloaded_remote.idle_timeout_secs,
            remote_entry.idle_timeout_secs,
        );
        assert_eq!(
            reloaded_remote.burst_threshold,
            remote_entry.burst_threshold,
        );
        assert_eq!(
            reloaded_remote.workflow_run_id,
            remote_entry.workflow_run_id,
        );
        assert_eq!(
            reloaded_remote.workflow_role,
            remote_entry.workflow_role,
        );
        assert_eq!(reloaded_remote.task_id, remote_entry.task_id);
        assert_eq!(
            reloaded_remote.notify_on_idle,
            remote_entry.notify_on_idle,
        );
        assert_eq!(
            reloaded_remote.seeded_from_snapshot,
            remote_entry.seeded_from_snapshot,
        );
        assert_eq!(reloaded_remote.host_id, remote_entry.host_id);

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        drop(guard);
    }

    /// Regression: `save_session_manifest` MUST NOT overwrite the on-disk
    /// manifest before `restore_sessions` has hydrated `self.workspaces`.
    ///
    /// This is the "lost sessions / lost workspaces on restart" bug. On
    /// startup `self.workspaces` is empty (or holds only the live agent
    /// sessions adoption surfaced); because the writer does a FULL REPLACE,
    /// any save before restore clobbered the real manifest down to that
    /// partial view. The guard turns such a save into a no-op until
    /// `sessions_restored` is set (by `maybe_restore_sessions`).
    #[test]
    fn save_session_manifest_noops_before_restore_so_manifest_is_not_clobbered() {
        use cm_daemon::manifest::{Manifest, ManifestWorkspace};

        let guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let cm_dir = tmp.path().join(".cm");
        std::fs::create_dir_all(&cm_dir).expect("create .cm");

        // A real, multi-workspace manifest already on disk — the state a
        // restart must preserve.
        let mut workspaces = HashMap::new();
        for id in ["ws-keep-1", "ws-keep-2"] {
            workspaces.insert(
                id.to_string(),
                ManifestWorkspace {
                    color: None,
                    pinned: false,
                    id: id.to_string(),
                    name: id.to_string(),
                    is_closed: false,
                    is_cloud: false,
                    worktree_path: None,
                    main_repo_path: None,
                    repo_url: None,
                    worker_vm: None,
                    worker_zone: None,
                    host_id: cm_daemon::host_id::HostId::local(),
                    sessions: vec![],
                    tombstones: vec![],
                },
            );
        }
        let on_disk = Manifest {
            task_colors: Default::default(),
            workspaces,
            bindings: HashMap::new(),
            view: Some("task".to_string()),
            hide_continuous: false,
            continuous_column_on: false,
        };
        std::fs::write(
            cm_dir.join("tui-sessions.json"),
            serde_json::to_string(&on_disk).expect("ser"),
        )
        .expect("write manifest");

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
        // App::new must NOT have restored yet — restore is driven from the
        // main loop via `maybe_restore_sessions`.
        assert!(!app.sessions_restored, "fresh App must not be pre-restored");

        // Mimic the clobber trigger: a single fresh workspace lands in live
        // state (as adoption would mint) and a save fires BEFORE restore.
        app.workspaces.push(Workspace {
            color: None,
            pinned: false,
            id: "ws-fresh-adopted".into(),
            name: "agent: phantom".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: None,
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![],
            tombstones: vec![],
            is_pushing: false,
        });
        app.save_session_manifest();

        // The on-disk manifest MUST be untouched — the two real workspaces
        // survive, the phantom did NOT replace them.
        let reloaded: Manifest = serde_json::from_str(
            &std::fs::read_to_string(cm_dir.join("tui-sessions.json")).expect("read"),
        )
        .expect("parse");
        assert!(
            reloaded.workspaces.contains_key("ws-keep-1")
                && reloaded.workspaces.contains_key("ws-keep-2"),
            "pre-restore save clobbered the manifest: {:?}",
            reloaded.workspaces.keys().collect::<Vec<_>>(),
        );
        assert!(
            !reloaded.workspaces.contains_key("ws-fresh-adopted"),
            "pre-restore save must be a no-op, not a partial write",
        );

        // After the flag flips (what `maybe_restore_sessions` does), saves
        // resume and persist live state.
        app.sessions_restored = true;
        app.save_session_manifest();
        let reloaded2: Manifest = serde_json::from_str(
            &std::fs::read_to_string(cm_dir.join("tui-sessions.json")).expect("read2"),
        )
        .expect("parse2");
        assert!(
            reloaded2.workspaces.contains_key("ws-fresh-adopted"),
            "post-restore save must persist live workspaces",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        drop(guard);
    }

    /// 12e-r7 F2: `launch_from_plan` (planning A-l, creating
    /// a new workspace + worktree) MUST call the shared
    /// guard BEFORE worktree creation. Same orphan-disk
    /// rationale as round-6 F1 for `create_local_session`.
    #[test]
    fn launch_from_plan_fails_fast_before_worktree_for_non_local_active_host() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        let sig = "fn launch_from_plan(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // Structural pin: shared-helper call.
        assert!(
            body.contains(
                "guard_local_host_only(\n            &active_host,\n            \"A-l launch-from-plan\",\n        )",
            ) || body.contains(
                "guard_local_host_only(&active_host, \"A-l launch-from-plan\")",
            ),
            "launch_from_plan must call guard_local_host_only \
             at the top (12e-r7 F2); body:\n{}",
            body,
        );

        // Ordering: guard MUST precede worktree creation +
        // try_spawn_via_daemon (the function uses
        // `spawn_agent_session` directly, not
        // `try_spawn_via_daemon`, but the orphan-dir concern
        // is the same).
        let guard_idx = body
            .find("guard_local_host_only(")
            .expect("guard call not found");
        let worktree_idx = body
            .find("worktree::create_worktree(")
            .expect("create_worktree call not found");
        assert!(
            guard_idx < worktree_idx,
            "guard (at byte {}) MUST precede \
             worktree::create_worktree (at byte {}) — \
             otherwise a remote-host A-l leaves an orphan dir",
            guard_idx,
            worktree_idx,
        );
    }

    /// 12e-r7 F2: `launch_into_workspace` (planning A-l,
    /// reusing an existing workspace) MUST call the shared
    /// guard at the top. Same shape as round-6 F1 for
    /// `spawn_session_on_workspace`.
    #[test]
    fn launch_into_workspace_fails_fast_for_non_local_active_host() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        let sig = "fn launch_into_workspace(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        assert!(
            body.contains(
                "guard_local_host_only(\n            &active_host,\n            \"A-l launch-into-workspace\",\n        )",
            ) || body.contains(
                "guard_local_host_only(&active_host, \"A-l launch-into-workspace\")",
            ),
            "launch_into_workspace must call \
             guard_local_host_only at the top (12e-r7 F2); \
             body:\n{}",
            body,
        );

        // Ordering: guard MUST precede the workspace lookup
        // + any spawn work. The guard short-circuits the
        // function on non-local active_host.
        // migrate-tui-local: the spawn call shape moved from
        // `self.spawn_agent_session(` (local PTY) to
        // `self.try_spawn_via_daemon(` (daemon RPC).
        let guard_idx = body
            .find("guard_local_host_only(")
            .expect("guard call not found");
        let spawn_idx = body
            .find("self.try_spawn_via_daemon(")
            .expect("try_spawn_via_daemon call not found");
        assert!(
            guard_idx < spawn_idx,
            "guard must precede try_spawn_via_daemon",
        );
    }

    /// 12e-r4 F1.2: `spawn_managed_session` MUST take the
    /// transcript-baseline snapshot (`pending_jsonl_files`)
    /// BEFORE the call to `try_spawn_via_daemon`. Pre-r4 the
    /// snapshot ran AFTER the spawn — the new agent's
    /// transcript JSONL (created within ms of spawn) was
    /// already in the baseline, so the detector treated it
    /// as preexisting and never bound transcript_id;
    /// `resolve_authorized_session` would then return
    /// `pending` forever.
    #[test]
    fn mcp_start_session_takes_transcript_baseline_before_spawn() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        let sig = "pub fn spawn_managed_session(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    pub fn ")
            .or_else(|| rest[1..].find("\n    pub(crate) fn "))
            .or_else(|| rest[1..].find("\n    fn "))
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // Find the offsets of the baseline snapshot (where
        // the engine-keyed Self::list_*_files call lives) and
        // the daemon-spawn call site. Snapshot MUST precede
        // spawn.
        let baseline_marker = "Self::list_jsonl_files(&worktree_path)";
        let spawn_marker = "self.try_spawn_via_daemon(";
        let baseline_idx = body
            .find(baseline_marker)
            .expect("baseline snapshot site not found");
        let spawn_idx = body
            .find(spawn_marker)
            .expect("try_spawn_via_daemon call site not found");
        assert!(
            baseline_idx < spawn_idx,
            "spawn_managed_session: `pending_jsonl_files` \
             snapshot (at byte {}) MUST precede the \
             `try_spawn_via_daemon` call (at byte {}). \
             Pre-r4 the order was reversed and the new \
             agent's transcript file was captured as \
             preexisting (12e-r4 F1.2).",
            baseline_idx,
            spawn_idx,
        );

        // Also pin the comment naming the bug so a future
        // refactor that reverts the order trips this test
        // with a clear failure reason.
        assert!(
            body.contains(
                "12e-r4 F1.2: snapshot transcript baseline BEFORE",
            ),
            "spawn_managed_session must carry the round-4 \
             F1.2 narrative comment",
        );
    }

    /// 12e-r8 F2 (named acceptance): `push_tui_sessions_to_host`
    /// MUST filter the session snapshot to only entries
    /// where `ts.host_id == target_host_id`. Pre-r8 every
    /// daemon received every session — a "state lie" that
    /// confuses lookup-by-uid, auth walks, and the eventual
    /// merged list_sessions view.
    ///
    /// Structural pin: the filter is present in the helper's
    /// body. Runtime verification would require standing up
    /// daemons + intercepting wire frames, which is overkill;
    /// the filter expression is small and direct.
    #[test]
    fn push_tui_sessions_to_daemon_buckets_by_host_id() {
        // Post-push-worker refactor: per-host bucketing happens
        // in `push_tui_sessions_to_daemon` when building the
        // owned snapshot, not in a `push_tui_sessions_to_host`
        // helper (the helper was removed). Pin the bucketing
        // pattern so a session pinned to host X is only
        // delivered to host X's daemon, never to the others
        // (12e-r8 F2 invariant preserved post-async).
        let src = crate::app::APP_SRC_FOR_SCAN;
        let sig = "pub(crate) fn push_tui_sessions_to_daemon(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("per_host.get_mut(&ts.host_id)"),
            "push_tui_sessions_to_daemon MUST route each session \
             into its pinned host's bucket (12e-r8 F2 — preserved \
             post-async); body:\n{}",
            body,
        );
        // The other two pushers (task_tree, workflow_defs) carry
        // host-agnostic payloads — pin that they don't sprout a
        // per-host filter accidentally.
        for sig in &[
            "pub(crate) fn push_workflow_definitions_to_daemon(",
            "pub(crate) fn push_task_tree_to_daemon(",
        ] {
            let start = src.find(sig).expect("must find sig");
            let rest = &src[start..];
            let end = rest[1..]
                .find("\n    fn ")
                .into_iter()
                .chain(rest[1..].find("\n    pub fn "))
                .chain(rest[1..].find("\n    pub(crate) fn "))
                .chain(rest[1..].find("\n    pub(super) fn "))
                .chain(rest[1..].find("\n#[cfg(test)]"))
                .min()
                .map(|i| 1 + i)
                .unwrap_or(rest.len());
            let body = &rest[..end];
            assert!(
                !body.contains("ts.host_id"),
                "{} payload is host-agnostic — MUST NOT filter \
                 by per-session host_id; body:\n{}",
                sig,
                body,
            );
        }
    }

    /// 12e-r3 F2 preserved post-async: each `_to_daemon` method
    /// MUST enumerate every host in `hosts.toml` when building
    /// its payload (task tree / workflow defs collect a
    /// `Vec<HostId>`; tui sessions seeds a per-host bucket map),
    /// and MUST hand off to `self.push_worker`. The per-host
    /// fanout itself lives in `push_worker.rs`; this test pins
    /// the main-thread half (everyone-enumerated + worker-routed).
    #[test]
    fn push_state_fanouts_to_all_hosts() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        for (sig, host_iter_marker, worker_marker) in &[
            (
                "pub(crate) fn push_tui_sessions_to_daemon(",
                "for host in &self.hosts.hosts",
                "self.push_worker.push_tui_sessions",
            ),
            (
                "pub(crate) fn push_workflow_definitions_to_daemon(",
                "self.hosts.hosts.iter()",
                "self.push_worker.push_workflow_defs",
            ),
            (
                "pub(crate) fn push_task_tree_to_daemon(",
                "self.hosts.hosts.iter()",
                "self.push_worker.push_task_tree",
            ),
        ] {
            let start = src.find(sig).expect("must find sig");
            let rest = &src[start..];
            let end = rest[1..]
                .find("\n    fn ")
                .into_iter()
                .chain(rest[1..].find("\n    pub fn "))
                .chain(rest[1..].find("\n    pub(crate) fn "))
                .chain(rest[1..].find("\n    pub(super) fn "))
                .chain(rest[1..].find("\n#[cfg(test)]"))
                .min()
                .map(|i| 1 + i)
                .unwrap_or(rest.len());
            let body = &rest[..end];
            assert!(
                body.contains(host_iter_marker),
                "{} body MUST enumerate `{}` to include every \
                 host; body:\n{}",
                sig,
                host_iter_marker,
                body,
            );
            assert!(
                body.contains(worker_marker),
                "{} body MUST hand off to `{}` (async push \
                 worker — keeps the main thread off network RTT); \
                 body:\n{}",
                sig,
                worker_marker,
                body,
            );
        }

        // The push helpers in app.rs MUST NOT do their own
        // direct RPC dialing — that's the whole point of the
        // worker. No `rpc_*`, no `for_host(`, no
        // `default_handle()` call sites in the push path of
        // app.rs.
        // Scan ALL production code (per-file test-stripped): the rpc_*
        // wrappers live on the push_worker thread, so no app production
        // code anywhere may dial them (or default_handle) directly.
        let push_section = crate::app::app_prod_src();
        let push_section = push_section.as_str();
        assert!(
            !push_section.contains("self.host_pool.default_handle()"),
            "push helpers MUST NOT call \
             self.host_pool.default_handle() — fanout lives in \
             the push worker (12e-r3 F2 preserved)",
        );
        assert!(
            !push_section.contains("rpc_task_update_tree(")
                && !push_section
                    .contains("rpc_tui_update_sessions_snapshot(")
                && !push_section
                    .contains("rpc_workflow_update_definitions("),
            "push helpers in app.rs MUST NOT call rpc_* directly \
             — those run on the push_worker thread",
        );
    }

    /// 12e-r3 F3: when `HostPool::from_config` fails, the
    /// App's `hosts` field MUST also fall back to the
    /// synthesized local default. Pre-r3 only the pool fell
    /// back; `hosts` retained the multi-host config, so a
    /// workspace pinned to a host not in the pool would have
    /// every `host_pool.for_host(...)` fail with `NotFound`.
    /// Keeping `hosts` and `host_pool` in lockstep is the
    /// invariant this pins.
    ///
    /// To deliberately fail `from_config`, write a hosts.toml
    /// that flips on the dir-resolution failure: unset
    /// XDG_RUNTIME_DIR AND set HOME to an empty path so
    /// `tunnel_dir_under_home` errors. But that breaks too
    /// much App-init machinery. Instead, drive the fix's
    /// presence structurally + drive a runtime check that the
    /// FALLBACK path (already in place even pre-r3) results
    /// in a consistent App when triggered by a malformed
    /// hosts.toml.
    ///
    /// Runtime approach: write a hosts.toml that REFERENCES
    /// `transport = "tcp-tls"` — the loader rejects TLS in
    /// validate() before pool construction, so we get a
    /// HostsConfig::load Err. The existing App::new path
    /// already falls back to synthesized local default in
    /// that case (see line ~2810). After fallback, `hosts`
    /// has the single synthesized local entry and the pool
    /// agrees. This test pins that consistency.
    ///
    /// The r3 F3 fix is for the SEPARATE failure mode where
    /// HostsConfig::load succeeds but HostPool::from_config
    /// errors. Pin that case structurally — the source must
    /// re-bind `hosts` in the `Err(...)` branch.
    #[test]
    fn pool_construction_failure_falls_back_to_local_only_hosts() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        // Structural pin: the from_config error branch must
        // re-bind `hosts` AND build a fresh pool from the
        // local default. The marker is the 12e-r3 F3
        // comment plus the tuple-rebind pattern.
        assert!(
            src.contains(
                "12e-r3 F3: when pool construction fails",
            ),
            "App::new must carry the 12e-r3 F3 fallback \
             comment",
        );
        assert!(
            src.contains("(local, pool)"),
            "App::new's from_config Err branch must produce \
             (HostsConfig, HostPool) as a tuple — both \
             rebound to the synthesized local default so \
             `hosts` and `host_pool` stay consistent",
        );
        // Make sure the rebinding actually returns BOTH from
        // the match.
        let from_config_start = src
            .find("HostPool::from_config(&hosts)")
            .expect("must find from_config call site");
        let from_config_section = &src[from_config_start
            ..from_config_start
                + 2000
                    .min(src.len() - from_config_start)];
        assert!(
            from_config_section.contains("Ok(pool) => (hosts, pool)"),
            "Ok branch must yield (hosts, pool) tuple"
        );
        assert!(
            from_config_section.contains("(local, pool)"),
            "Err branch must yield (local, pool) — local being \
             the synthesized HostsConfig",
        );

        // Runtime sanity: a load with a TLS-transport host
        // makes HostsConfig::load fail, App::new falls back
        // → hosts.hosts.len() == 1 AND active_host == local.
        let guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let cm_dir = tmp.path().join(".cm");
        std::fs::create_dir_all(&cm_dir).expect("create .cm");
        std::fs::write(
            cm_dir.join("hosts.toml"),
            r#"
[[host]]
name = "local"
transport = "unix"
socket = "/tmp/local-test.sock"
default = true

[[host]]
name = "future"
transport = "tcp-tls"
addr = "1.2.3.4:443"
tls_fingerprint = "deadbeef"
"#,
        )
        .expect("write hosts.toml");
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
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(
            app.hosts.hosts.len(),
            1,
            "TLS-transport host triggers HostsConfig::load \
             Err → App falls back to synthesized local default",
        );
        // The pool must contain the local host (consistency
        // check that `hosts` and `host_pool` agree).
        assert!(
            app.host_pool.for_host(&cm_daemon::host_id::HostId::local()).is_ok(),
            "host_pool MUST contain the local host that \
             active_host points to",
        );
    }
}

/// migrate-tui-local acceptance tests. Source-text pins for the
/// named criteria in `doc/migrate-tui-local-spawn-decision.md`:
/// every TuiLocal call site in production code is gone; spawn
/// sites that previously called `spawn_agent_session` (the
/// local-PTY path) now call `try_spawn_via_daemon`
/// (or `try_spawn_via_daemon_with_deps` for the controller /
/// free-function callers).
#[cfg(test)]
mod migrate_tui_local_tests {
    /// T_migrate_no_tuilocal_sites_remain: no non-test code in
    /// `tui/src/app.rs` references `SpawnTarget::TuiLocal`. Test
    /// fixtures may still use the variant; the enum value stays in
    /// place for future cloud-worker use. (The former
    /// `tui/src/workflow/controller.rs` half of this pin was dropped
    /// when that file was deleted — the TUI owns no workflow logic.)
    #[test]
    fn t_migrate_no_tuilocal_sites_remain() {
        // Production-only per-file scan: the concatenated corpus cannot be
        // depth-stripped reliably (test string literals unbalance the brace
        // count and swallow every later file), so the stripper runs per
        // source file via crate::app::app_prod_src().
        let app_prod = crate::app::app_prod_src();

        // The doc-comment + decision-doc references to
        // `SpawnTarget::TuiLocal` ARE allowed (we keep them
        // as historical context). The pin is on actual call
        // sites — lines that pass the variant to a function.
        let bad_pattern = "crate::mcp_config::SpawnTarget::TuiLocal,";
        assert!(
            !app_prod.contains(bad_pattern),
            "tui/src/app.rs production code MUST NOT pass \
             `crate::mcp_config::SpawnTarget::TuiLocal,` to \
             any build_args call site — migrate-tui-local \
             routes every spawn through SpawnTarget::Daemon.",
        );
    }

    /// T_migrate_a_n_session_is_daemon_owned: the A-n /
    /// `create_local_session` flow routes the spawn through
    /// `try_spawn_via_daemon` (which produces a daemon-owned
    /// session registered in `state.sessions`). Pin the
    /// structural shape — the call exists and is reached on
    /// the happy path.
    #[test]
    fn t_migrate_a_n_session_is_daemon_owned() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        let sig = "fn create_local_session(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("self.try_spawn_via_daemon("),
            "create_local_session MUST call \
             self.try_spawn_via_daemon to spawn the A-n \
             session daemon-side; body:\n{}",
            body,
        );
        // And the local-PTY fallback (spawn_agent_session) is
        // either gone or only the "unreachable" branch.
        assert!(
            !body.contains("self.spawn_agent_session("),
            "create_local_session MUST NOT call \
             self.spawn_agent_session — migrate-tui-local \
             removed the local-PTY fallback for the A-n \
             daemon-eligible 'claude' path; body:\n{}",
            body,
        );
    }

    /// T_migrate_manifest_restore_produces_daemon_owned:
    /// `spawn_restored_session` (the manifest-restore path)
    /// routes claude/codex restores through the daemon RPC.
    #[test]
    fn t_migrate_manifest_restore_produces_daemon_owned() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        let sig = "fn spawn_restored_session(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("self.try_spawn_via_daemon("),
            "spawn_restored_session MUST route claude/codex \
             through self.try_spawn_via_daemon so restored \
             sessions are daemon-owned; body:\n{}",
            body,
        );
    }

    /// T_migrate_spawn_resumed_session_passes_resume_arg:
    /// `spawn_resumed_session` (A-l cloud-pull resume) calls
    /// `try_spawn_via_daemon` with the session_id passed as
    /// the `resume_session_id` argument. The daemon then
    /// spawns `claude --resume <id>` so the daemon-registered
    /// session's transcript_id matches the resumed id at
    /// registration time.
    #[test]
    fn t_migrate_spawn_resumed_session_passes_resume_arg() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        let sig = "fn spawn_resumed_session(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];
        // Daemon-routed.
        assert!(
            body.contains("self.try_spawn_via_daemon("),
            "spawn_resumed_session MUST call \
             self.try_spawn_via_daemon (migrate-tui-local); \
             body:\n{}",
            body,
        );
        // Threads `Some(session_id.as_str())` as the resume
        // argument.
        assert!(
            body.contains("Some(session_id.as_str())"),
            "spawn_resumed_session MUST thread \
             `Some(session_id.as_str())` to \
             try_spawn_via_daemon's resume_session_id slot so \
             the daemon spawns `claude --resume <id>` (no \
             post-spawn /resume workaround); body:\n{}",
            body,
        );
    }

    /// T_migrate_bash_session_is_daemon_owned: A-s spawning a
    /// `bash` session also routes through the daemon (the
    /// daemon-side `is_valid_session_type` already accepts
    /// "bash"; daemon's `mcp_config::build_args` maps
    /// "bash" → ("/bin/bash", [])). `spawn_session_on_workspace`
    /// passes session_type unchanged to try_spawn_via_daemon,
    /// so bash is daemon-eligible too.
    #[test]
    fn t_migrate_bash_session_is_daemon_owned() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        // try_spawn_via_daemon_with_deps explicitly handles
        // "bash" in its argv match — that's where bash gets
        // its daemon eligibility.
        let sig = "pub(crate) fn try_spawn_via_daemon_with_deps(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\npub ")
            .or_else(|| rest[1..].find("\nfn "))
            .or_else(|| rest[1..].find("\nimpl "))
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("\"bash\" => Ok((\"/bin/bash\".to_string(), Vec::new())),"),
            "try_spawn_via_daemon_with_deps must accept \
             session_type=\"bash\" and produce \
             (\"/bin/bash\", []) argv so A-s bash sessions \
             route through the daemon; body:\n{}",
            body,
        );

        // And `spawn_session_on_workspace` passes
        // session_type verbatim — i.e. it routes ALL types
        // through try_spawn_via_daemon (no special-case for
        // bash).
        let sig2 = "fn spawn_session_on_workspace(";
        let start2 = src.find(sig2).expect("must find sig2");
        let rest2 = &src[start2..];
        let end2 = rest2[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest2[1..].find("\n    pub fn "))
            .chain(rest2[1..].find("\n    pub(crate) fn "))
            .chain(rest2[1..].find("\n    pub(super) fn "))
            .chain(rest2[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest2.len());
        let body2 = &rest2[..end2];
        assert!(
            body2.contains("self.try_spawn_via_daemon("),
            "spawn_session_on_workspace MUST call \
             self.try_spawn_via_daemon for every session_type \
             (claude / codex / bash); body:\n{}",
            body2,
        );
    }

    /// T_migrate_cross_route_refusal_unreachable_or_removed:
    /// the daemon's "TUI-owned, can't be proxied" Conflict
    /// refusal in `return_auth_error_if_denied_with_state` is
    /// gone. The associated "post-Phase-1 cross-route work"
    /// doc-comment is also removed.
    #[test]
    fn t_migrate_cross_route_refusal_unreachable_or_removed() {
        let methods_src = include_str!("../../../daemon/src/control/methods.rs");
        assert!(
            !methods_src.contains("is TUI-owned; the daemon does not"),
            "daemon's cross-route refusal Conflict error MUST \
             be removed post-migrate-tui-local — every \
             session is now daemon-owned so the branch is \
             unreachable",
        );
        assert!(
            !methods_src.contains("post-Phase-1 cross-route"),
            "daemon's `post-Phase-1 cross-route` doc-comment \
             MUST be removed post-migrate-tui-local",
        );
    }

    // ============================================================
    // Reviewer-surfaced regressions
    //
    // These pins close the three correctness gaps the reviewer
    // flagged after the initial migration landed:
    //   1. manifest restore conflicting with surviving daemon
    //      sessions — restore now attaches to live UIDs.
    //   2. workflow env block dropped in the shared spawn helper
    //      — `CM_WORKFLOW_RUN_ID` + `CM_ROLE` now land in the
    //      spawned MCP server's env.
    //   3. `transcript_path` hardcoded to None for resume/restore
    //      — the helper accepts and forwards a caller-supplied
    //      path so `resolve_authorized_session` resolves
    //      immediately for already-known transcripts.
    // ============================================================

    /// `spawn_restored_session` must consult the live-daemon-UID
    /// set: when the entry's UID is already in the daemon's
    /// registry, route through the attach helper (no
    /// `start_session`); otherwise fall through to the daemon-
    /// spawn path. Pre-fix the restore unconditionally called
    /// `start_session` and the daemon's collision guard dropped
    /// the survivor.
    #[test]
    fn t_migrate_manifest_restore_attaches_to_live_daemon_session() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        // The function signature MUST accept the live UID set.
        let sig = "fn spawn_restored_session(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let body_open = rest.find('{').expect("body open brace");
        let after_sig = &rest[..body_open];
        assert!(
            after_sig.contains("live_daemon_uids: &std::collections::HashSet<String>")
                || after_sig.contains("live_daemon_uids: &HashSet<String>"),
            "spawn_restored_session must accept the live-daemon-UID \
             set so it can dispatch attach-vs-spawn (Issue 1); \
             signature header:\n{}",
            after_sig,
        );

        // Bound the body and verify both branches exist.
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // Attach branch: `live_daemon_uids.contains(...)` gate
        // followed by a call to the attach helper.
        let gate_idx = body
            .find("live_daemon_uids.contains(")
            .expect("missing live_daemon_uids.contains() guard");
        let attach_idx = body
            .find("try_attach_via_daemon_with_deps(")
            .expect("missing try_attach_via_daemon_with_deps() call");
        let spawn_idx = body
            .find("self.try_spawn_via_daemon(")
            .expect("missing self.try_spawn_via_daemon() fallback");
        assert!(
            gate_idx < attach_idx,
            "the live-UID gate MUST precede the attach call (Issue 1); \
             gate at {}, attach at {}, body:\n{}",
            gate_idx,
            attach_idx,
            body,
        );
        assert!(
            attach_idx < spawn_idx,
            "the attach branch MUST appear before the spawn fallback so \
             the attach path wins when the UID is live (Issue 1); attach at \
             {}, spawn at {}, body:\n{}",
            attach_idx,
            spawn_idx,
            body,
        );

        // restore_sessions probes the daemon ONCE up front via
        // `rpc_list_session_uids` and threads the set through.
        let restore_src_idx = src.find("fn restore_sessions(").expect("restore_sessions exists");
        let restore_rest = &src[restore_src_idx..];
        let restore_end = restore_rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(restore_rest[1..].find("\n    pub fn "))
            .chain(restore_rest[1..].find("\n    pub(crate) fn "))
            .chain(restore_rest[1..].find("\n    pub(super) fn "))
            .chain(restore_rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(restore_rest.len());
        let restore_body = &restore_rest[..restore_end];
        assert!(
            restore_body.contains("rpc_list_session_uids("),
            "restore_sessions MUST probe the daemon via \
             rpc_list_session_uids before the spawn loop \
             (Issue 1); restore_sessions body excerpt:\n{}",
            &restore_body[..restore_body.len().min(2000)],
        );
        assert!(
            restore_body.contains("let live_daemon_uids:"),
            "restore_sessions MUST bind the probe result as \
             `live_daemon_uids` (Issue 1); restore_sessions body \
             excerpt:\n{}",
            &restore_body[..restore_body.len().min(2000)],
        );

        // And `attach_existing` exists on ClientSession with the
        // expected shape.
        let cs_src = include_str!("../client_session.rs");
        assert!(
            cs_src.contains("pub fn attach_existing("),
            "ClientSession::attach_existing must exist so the \
             restore path can skip start_session (Issue 1)",
        );
        // And the daemon-list probe helper exists on the wire.
        assert!(
            cs_src.contains("pub fn rpc_list_session_uids("),
            "rpc_list_session_uids must exist so restore can \
             probe the daemon (Issue 1)",
        );
    }

    /// The shared `try_spawn_via_daemon_with_deps` helper must
    /// wire `workflow_run_id` / `workflow_role` into the
    /// `mcp_config::build_args` workflow argument so the spawned
    /// MCP server receives `CM_WORKFLOW_RUN_ID` + `CM_ROLE` in its
    /// env. Pre-fix the helper passed `None` to build_args and
    /// `workflow_transition` / `workflow_done` from a respawned
    /// workflow participant fell through unknown.
    #[test]
    fn t_migrate_workflow_respawn_passes_workflow_env() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        // Locate the helper body.
        let sig = "pub(crate) fn try_spawn_via_daemon_with_deps(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\npub ")
            .or_else(|| rest[1..].find("\nfn "))
            .or_else(|| rest[1..].find("\nimpl "))
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // The helper MUST construct a WorkflowMeta from the
        // (workflow_run_id, workflow_role) tuple and pass it to
        // build_args.
        assert!(
            body.contains("crate::mcp_config::WorkflowMeta { run_id, role }"),
            "try_spawn_via_daemon_with_deps MUST construct a \
             WorkflowMeta from the workflow_run_id/workflow_role \
             pair (Issue 2); body excerpt:\n{}",
            &body[..body.len().min(2000)],
        );
        // And the build_args calls MUST forward `workflow_meta`
        // (NOT `None`) so build_env puts CM_WORKFLOW_RUN_ID +
        // CM_ROLE into the MCP server's env.
        let claude_call_idx = body
            .find("Engine::ClaudeCode,")
            .expect("missing Engine::ClaudeCode arm");
        let codex_call_idx = body
            .find("Engine::Codex,")
            .expect("missing Engine::Codex arm");
        // After each engine line, check that `workflow_meta.clone()` (NOT `None`) appears.
        let claude_slice = &body[claude_call_idx..claude_call_idx + 300];
        let codex_slice = &body[codex_call_idx..codex_call_idx + 300];
        assert!(
            claude_slice.contains("workflow_meta.clone()"),
            "Engine::ClaudeCode build_args call MUST pass \
             `workflow_meta.clone()` to build_args (Issue 2); \
             pre-fix it passed `None` and CM_WORKFLOW_RUN_ID was \
             dropped from the env; slice:\n{}",
            claude_slice,
        );
        assert!(
            codex_slice.contains("workflow_meta.clone()"),
            "Engine::Codex build_args call MUST pass \
             `workflow_meta.clone()` to build_args (Issue 2); \
             slice:\n{}",
            codex_slice,
        );

        // build_env (in mcp_config) sets CM_WORKFLOW_RUN_ID +
        // CM_ROLE when WorkflowMeta is Some. Pin that contract
        // hasn't drifted.
        let mcp_src = include_str!("../mcp_config.rs");
        assert!(
            mcp_src.contains("CM_WORKFLOW_RUN_ID"),
            "mcp_config::build_env must set CM_WORKFLOW_RUN_ID \
             so the spawned MCP server reads it from env",
        );
        assert!(
            mcp_src.contains("CM_ROLE"),
            "mcp_config::build_env must set CM_ROLE so the \
             spawned MCP server reads it from env",
        );
    }

    /// `spawn_resumed_session` (A-l cloud-pull resume) and
    /// `spawn_restored_session` (manifest restore for known
    /// transcript_ids) MUST thread the pre-known transcript path
    /// to the daemon (not `None`), so the daemon's
    /// `resolve_authorized_session` resolves immediately and MCP
    /// `read_session_output` can read the restored transcript.
    #[test]
    fn t_migrate_resume_path_registers_transcript_with_daemon() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        // pre_spawn_transcript_path helper exists with the
        // claude-deterministic-path semantics.
        assert!(
            src.contains("fn pre_spawn_transcript_path("),
            "pre_spawn_transcript_path helper must exist so \
             resume/restore sites can pass the daemon a known \
             path at spawn time (Issue 3)",
        );

        // spawn_resumed_session body computes the path and
        // threads it.
        let sig = "fn spawn_resumed_session(";
        let start = src.find(sig).expect("must find spawn_resumed_session");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("let pre_spawn_transcript ="),
            "spawn_resumed_session MUST compute `pre_spawn_transcript` \
             before try_spawn_via_daemon (Issue 3); body excerpt:\n{}",
            &body[..body.len().min(1500)],
        );
        assert!(
            body.contains("pre_spawn_transcript_path(\"claude\", &worktree_path,"),
            "spawn_resumed_session MUST call pre_spawn_transcript_path \
             with \"claude\" + worktree + session_id (Issue 3); body \
             excerpt:\n{}",
            &body[..body.len().min(1500)],
        );
        assert!(
            body.contains("pre_spawn_transcript.as_deref()"),
            "spawn_resumed_session MUST thread the computed \
             transcript path through to try_spawn_via_daemon \
             (Issue 3); body excerpt:\n{}",
            &body[..body.len().min(1500)],
        );

        // spawn_restored_session for known transcript_ids does
        // the same.
        let sig2 = "fn spawn_restored_session(";
        let start2 = src.find(sig2).expect("must find spawn_restored_session");
        let rest2 = &src[start2..];
        let end2 = rest2[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest2[1..].find("\n    pub fn "))
            .chain(rest2[1..].find("\n    pub(crate) fn "))
            .chain(rest2[1..].find("\n    pub(super) fn "))
            .chain(rest2[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest2.len());
        let body2 = &rest2[..end2];
        assert!(
            body2.contains("pre_spawn_transcript_path(&entry.session_type"),
            "spawn_restored_session MUST call pre_spawn_transcript_path \
             with the entry's session_type when the manifest entry has a \
             known transcript_id (Issue 3); body excerpt:\n{}",
            &body2[..body2.len().min(1500)],
        );
        assert!(
            body2.contains("pre_spawn_transcript.as_deref()"),
            "spawn_restored_session MUST thread the computed path \
             through to BOTH the attach and spawn branches (Issue \
             3); body excerpt:\n{}",
            &body2[..body2.len().min(1500)],
        );

        // The helper's signature MUST end with the transcript_path arg.
        let helper_sig_idx = src
            .find("pub(crate) fn try_spawn_via_daemon_with_deps(")
            .expect("helper must exist");
        let helper_rest = &src[helper_sig_idx..];
        let helper_open = helper_rest.find('{').expect("helper body open");
        let helper_header = &helper_rest[..helper_open];
        assert!(
            helper_header.contains("transcript_path: Option<&str>"),
            "try_spawn_via_daemon_with_deps MUST accept a \
             `transcript_path: Option<&str>` argument (Issue 3); \
             signature header:\n{}",
            helper_header,
        );

        // And the helper forwards it (not `None`) into the
        // ClientSessionConfig.
        let helper_end = helper_rest[1..]
            .find("\npub ")
            .or_else(|| helper_rest[1..].find("\nfn "))
            .or_else(|| helper_rest[1..].find("\nimpl "))
            .map(|i| 1 + i)
            .unwrap_or(helper_rest.len());
        let helper_body = &helper_rest[..helper_end];
        assert!(
            !helper_body.contains("transcript_path: None,"),
            "try_spawn_via_daemon_with_deps MUST NOT hardcode \
             `transcript_path: None` in the ClientSessionConfig \
             — that was the Issue 3 regression. The helper must \
             forward the caller-supplied `transcript_path` arg.",
        );
        assert!(
            helper_body.contains("transcript_path,\n        workflow_run_id,")
                || helper_body.contains("transcript_path,"),
            "try_spawn_via_daemon_with_deps MUST forward \
             `transcript_path` into the ClientSessionConfig \
             (Issue 3)",
        );
    }

    /// migrate-tui-local Issue A: the live-daemon-UID probe MUST
    /// filter out rows backed only by `state.tui_sessions`. Pre-
    /// fix the probe took the unfiltered `list_sessions` array
    /// (which mixes daemon-owned + TUI-pushed snapshot rows), so
    /// a stale snapshot row from a previous TUI process tricked
    /// `spawn_restored_session` into the attach branch; the
    /// attach RPC then failed (no live PTY behind the snapshot)
    /// and the manifest entry was silently dropped.
    ///
    /// Pinned via two complementary checks:
    ///   1. `rpc_list_session_uids` passes
    ///      `{ "daemon_owned_only": true }` so the daemon-side
    ///      filter fires.
    ///   2. The daemon's `list_sessions` handler honors
    ///      `daemon_owned_only` and short-circuits before the
    ///      TUI-snapshot loop when set.
    #[test]
    fn t_migrate_restore_probe_excludes_tui_session_rows() {
        // 1. TUI-side: rpc_list_session_uids passes the filter
        // flag.
        let cs_src = include_str!("../client_session.rs");
        let helper_idx = cs_src
            .find("pub fn rpc_list_session_uids(")
            .expect("rpc_list_session_uids must exist");
        let helper_rest = &cs_src[helper_idx..];
        let helper_end = helper_rest[1..]
            .find("\npub ")
            .or_else(|| helper_rest[1..].find("\nfn "))
            .map(|i| 1 + i)
            .unwrap_or(helper_rest.len());
        let helper_body = &helper_rest[..helper_end];
        assert!(
            helper_body.contains("\"daemon_owned_only\": true"),
            "rpc_list_session_uids MUST pass \
             `{{ \"daemon_owned_only\": true }}` to list_sessions \
             (Issue A) so stale tui_sessions snapshot rows from a \
             prior TUI process don't show up as attachable. \
             body:\n{}",
            helper_body,
        );

        // 2. Daemon-side: list_sessions handler honors the flag.
        let methods_src = include_str!("../../../daemon/src/control/methods.rs");
        // The flag MUST exist on the params struct.
        assert!(
            methods_src.contains("daemon_owned_only: bool"),
            "daemon's ListSessionsParams MUST carry a \
             `daemon_owned_only: bool` field (Issue A)",
        );
        // And the handler MUST short-circuit before the
        // state.tui_sessions loop when the flag is set.
        let handler_idx = methods_src
            .find("pub fn list_sessions(")
            .expect("daemon list_sessions handler must exist");
        let handler_rest = &methods_src[handler_idx..];
        let handler_end = handler_rest[1..]
            .find("\npub fn ")
            .or_else(|| handler_rest[1..].find("\nfn "))
            .map(|i| 1 + i)
            .unwrap_or(handler_rest.len());
        let handler_body = &handler_rest[..handler_end];
        // Find the short-circuit gate AND the tui_sessions loop.
        let gate_idx = handler_body
            .find("if p.daemon_owned_only")
            .expect("missing `if p.daemon_owned_only` gate");
        let tui_loop_idx = handler_body
            .find("for (uid, snap) in state.tui_sessions.iter()")
            .expect("missing tui_sessions loop");
        assert!(
            gate_idx < tui_loop_idx,
            "the `daemon_owned_only` short-circuit MUST precede \
             the state.tui_sessions loop (Issue A); gate at {}, \
             loop at {}",
            gate_idx,
            tui_loop_idx,
        );
        // And the short-circuit MUST early-return.
        let gate_slice = &handler_body[gate_idx..tui_loop_idx];
        assert!(
            gate_slice.contains("return Ok(Value::Array(sessions))"),
            "the daemon_owned_only branch MUST early-return the \
             daemon-owned rows before falling into the \
             state.tui_sessions loop; gate-to-loop slice:\n{}",
            gate_slice,
        );
    }

    /// migrate-tui-local Issue B: `spawn_resumed_session`
    /// always materializes a local replacement workspace from a
    /// locally-pulled worktree. The daemon spawn + TerminalSession
    /// host tag MUST be pinned to `HostId::local()` — NOT
    /// `self.active_host`. Otherwise a concurrent A-H cycle
    /// between pull-start and PullComplete sends the local
    /// filesystem path to a remote daemon and mistags the new
    /// workspace.
    #[test]
    fn t_migrate_spawn_resumed_session_pins_local_host() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        let sig = "fn spawn_resumed_session(";
        let start = src.find(sig).expect("must find spawn_resumed_session");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // The host snapshot MUST be HostId::local(), not
        // self.active_host.
        assert!(
            body.contains("let host_snapshot = cm_daemon::host_id::HostId::local();"),
            "spawn_resumed_session MUST pin its host snapshot to \
             `HostId::local()` because the cloud-pull replacement \
             workspace is always local-side (Issue B); body \
             excerpt:\n{}",
            &body[..body.len().min(2000)],
        );
        // And MUST NOT bind `self.active_host` to the host
        // snapshot — that's the pre-fix race-prone form.
        assert!(
            !body.contains("let active_host = self.active_host.clone();"),
            "spawn_resumed_session MUST NOT snapshot \
             `self.active_host` (Issue B regression pin) — a \
             concurrent A-H cycle between pull-start and \
             PullComplete would otherwise mistag the new local \
             workspace. body excerpt:\n{}",
            &body[..body.len().min(2000)],
        );
        // And the daemon dial + ts.host_id assignment MUST use
        // host_snapshot.
        assert!(
            body.contains("&host_snapshot,"),
            "spawn_resumed_session MUST pass `&host_snapshot` to \
             try_spawn_via_daemon (Issue B); body excerpt:\n{}",
            &body[..body.len().min(2000)],
        );
        assert!(
            body.contains("ts.host_id = host_snapshot.clone();"),
            "spawn_resumed_session MUST tag the new \
             TerminalSession with `host_snapshot` (Issue B); body \
             excerpt:\n{}",
            &body[..body.len().min(2000)],
        );
        // Doc-comment pin: the why is on-record so a future
        // refactor doesn't reintroduce the active_host form.
        assert!(
            body.contains("cloud-pull"),
            "spawn_resumed_session MUST document why the host is \
             pinned local (cloud-pull replacement workspace is \
             always local-side, Issue B); body excerpt:\n{}",
            &body[..body.len().min(2000)],
        );
    }

    /// migrate-tui-local Issue C: every daemon-routed spawn site
    /// that builds local-only paths (worktree from local
    /// filesystem, `~/.cm/mcp/...` config) MUST gate on
    /// `guard_local_host_only` BEFORE the
    /// `try_spawn_via_daemon` / `try_spawn_via_daemon_with_deps`
    /// call. Pre-fix four sites slipped through the migration
    /// and silently sent local paths to remote daemons when the
    /// user cycled `A-H` away from `local`.
    ///
    /// Pin: in each function's body, `guard_local_host_only(`
    /// must appear AND precede the daemon-spawn call.
    #[test]
    fn t_migrate_local_only_paths_guard_active_host() {
        let app_src = crate::app::APP_SRC_FOR_SCAN;

        // (source, function name, spawn-call needle, src for
        // error messages).
        let sites: &[(&str, &str, &str)] = &[
            // app.rs: restore_tombstones_for_workspace.
            ("fn restore_tombstones_for_workspace(", "restore_tombstones_for_workspace", "self.try_spawn_via_daemon("),
            // app.rs: resurrect_designer_sessions_for_workspace.
            ("fn resurrect_designer_sessions_for_workspace(", "resurrect_designer_sessions_for_workspace", "self.try_spawn_via_daemon("),
            // app.rs: attach_active.
            ("fn attach_active(&mut self)", "attach_active", "self.try_spawn_via_daemon("),
        ];
        for (sig, name, spawn_needle) in sites {
            let start = app_src.find(sig).unwrap_or_else(|| {
                panic!("{}: function signature {:?} not found", name, sig)
            });
            let rest = &app_src[start..];
            let end = rest[1..]
                .find("\n    fn ")
                .into_iter()
                .chain(rest[1..].find("\n    pub fn "))
                .chain(rest[1..].find("\n    pub(crate) fn "))
                .chain(rest[1..].find("\n    pub(super) fn "))
                .chain(rest[1..].find("\n#[cfg(test)]"))
                .min()
                .map(|i| 1 + i)
                .unwrap_or(rest.len());
            let body = &rest[..end];
            let guard_idx = body.find("guard_local_host_only(").unwrap_or_else(|| {
                panic!(
                    "{} MUST call guard_local_host_only(...) (Issue C); \
                     body excerpt:\n{}",
                    name,
                    &body[..body.len().min(1500)],
                )
            });
            let spawn_idx = body.find(spawn_needle).unwrap_or_else(|| {
                panic!(
                    "{} expected to contain {:?}; body excerpt:\n{}",
                    name,
                    spawn_needle,
                    &body[..body.len().min(1500)],
                )
            });
            assert!(
                guard_idx < spawn_idx,
                "{}: guard_local_host_only at byte {} MUST precede \
                 the daemon-spawn call at byte {} (Issue C); body \
                 excerpt:\n{}",
                name,
                guard_idx,
                spawn_idx,
                &body[..body.len().min(1500)],
            );
        }

        // Phase 4 §E: controller's spawn_workflow_session is deleted — the
        // daemon spawns workflow participants now (see daemon start_workflow).
    }

    /// migrate-tui-local Issue D: when `spawn_restored_session`
    /// takes the ATTACH branch (daemon already has the UID), the
    /// daemon-side `DaemonSession` may be missing the manifest's
    /// `transcript_path` / workflow tags (e.g. the post-spawn
    /// detector hadn't fired before the original TUI exited).
    /// The shared `try_attach_via_daemon_with_deps` helper MUST
    /// push those fields via the existing setter RPCs after the
    /// attach succeeds — otherwise `resolve_authorized_session`
    /// keeps returning `pending` and MCP `read_session_output`
    /// fails.
    ///
    /// `task_id` has no setter and is intentionally not pushed
    /// (the daemon preserves it across TUI restart since
    /// `state.sessions` is daemon-owned).
    #[test]
    fn t_migrate_attach_branch_pushes_manifest_metadata() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        let sig = "pub(crate) fn try_attach_via_daemon_with_deps(";
        let start = src.find(sig).expect("must find try_attach_via_daemon_with_deps");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\npub ")
            .or_else(|| rest[1..].find("\nfn "))
            .or_else(|| rest[1..].find("\nimpl "))
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // After the attach call, the helper must invoke the
        // existing setter RPCs.
        let attach_idx = body
            .find("Session::new_attached_existing(")
            .expect("attach RPC call site missing");
        let set_transcript_idx = body
            .find("rpc_set_transcript_path(")
            .expect(
                "try_attach_via_daemon_with_deps MUST call \
                 rpc_set_transcript_path after attach (Issue D)",
            );
        let set_workflow_idx = body
            .find("rpc_set_workflow_context(")
            .expect(
                "try_attach_via_daemon_with_deps MUST call \
                 rpc_set_workflow_context after attach (Issue D)",
            );
        assert!(
            attach_idx < set_transcript_idx,
            "rpc_set_transcript_path MUST be invoked AFTER the \
             attach call (Issue D); attach at {}, push at {}",
            attach_idx,
            set_transcript_idx,
        );
        assert!(
            attach_idx < set_workflow_idx,
            "rpc_set_workflow_context MUST be invoked AFTER the \
             attach call (Issue D); attach at {}, push at {}",
            attach_idx,
            set_workflow_idx,
        );

        // Transcript-path push: conditional, gated on Some so
        // the daemon's existing value isn't clobbered (Issue D).
        let set_tp_slice =
            &body[set_transcript_idx.saturating_sub(200)..set_transcript_idx];
        assert!(
            set_tp_slice.contains("if let Some(tp) = transcript_path"),
            "rpc_set_transcript_path MUST be gated on \
             `transcript_path.is_some()` so the daemon's existing \
             value isn't clobbered with None (Issue D); slice:\n{}",
            set_tp_slice,
        );

        // migrate-tui-local Issue E: workflow-context push must
        // cover BOTH the set direction `(Some, Some)` AND the
        // clear direction `(None, None)`. Pre-fix only `(Some,
        // Some)` pushed and stale daemon-side workflow tags
        // kept authorizing workflow operations against the
        // wrong (Detached/Done) run. Half-tagged inputs MUST
        // early-skip and log — daemon contract rejects partial
        // tuples.
        //
        // Pin shape: the push lives inside a `match
        // (workflow_run_id, workflow_role)` whose set/clear
        // arms call `rpc_set_workflow_context` and whose `_`
        // arm logs + skips.
        let wf_match_idx = body
            .find("match (workflow_run_id, workflow_role)")
            .expect(
                "try_attach_via_daemon_with_deps MUST dispatch \
                 the workflow push via `match (workflow_run_id, \
                 workflow_role)` (Issue E)",
            );
        // Slice from the match through the rpc_set call (gives
        // us all four arms — they're packed tight).
        let wf_match_end = body[wf_match_idx..]
            .find("}\n    }\n")
            .map(|i| wf_match_idx + i + 8)
            .unwrap_or(body.len());
        let wf_match_slice = &body[wf_match_idx..wf_match_end];
        assert!(
            wf_match_slice.contains("(Some(_), Some(_)) | (None, None)"),
            "the workflow match MUST include both the set arm \
             `(Some(_), Some(_))` AND the clear arm `(None, \
             None)` so a `restore_sessions` untag (manifest \
             None, daemon Some) clears the stale tags (Issue \
             E); slice:\n{}",
            wf_match_slice,
        );
        // Half-tagged arm: must NOT push, must log.
        assert!(
            wf_match_slice.contains("_ =>"),
            "the workflow match MUST have a half-tagged catch-\
             all arm that skips the push (Issue E); slice:\n{}",
            wf_match_slice,
        );
        // Count: rpc_set_workflow_context must appear ONCE
        // inside the match (in the set/clear arm). The catch-
        // all arm must NOT call the setter.
        let push_count = wf_match_slice.matches("rpc_set_workflow_context(").count();
        assert_eq!(
            push_count, 1,
            "rpc_set_workflow_context MUST be invoked exactly \
             once inside the workflow match — only the \
             (Some,Some)|(None,None) arm pushes; the half-tagged \
             arm logs and skips (Issue E). got count: {}",
            push_count,
        );
        // The half-tagged arm carries a skip-and-log message —
        // pin its presence so future refactors can't silently
        // swallow the half-tagged signal.
        let half_marker = body[wf_match_idx..]
            .find("half-tagged workflow state");
        assert!(
            half_marker.is_some(),
            "the half-tagged catch-all arm MUST log \
             `half-tagged workflow state` so an operator can \
             diagnose a corrupted manifest entry (Issue E); \
             slice:\n{}",
            wf_match_slice,
        );
    }

    /// migrate-tui-local Issue F: the live-daemon-UID set
    /// returned by `rpc_list_session_uids` is a snapshot. The
    /// daemon's session can exit between the probe and the
    /// `session.attach` call. Pre-fix the attach branch used
    /// `result.ok()?` on the attach result, so an attach Err
    /// would `return None` and silently drop the manifest entry
    /// from `ws.sessions` — a subsequent save then lost it
    /// permanently.
    ///
    /// Required shape: in `spawn_restored_session`, the
    /// attach-Err case must fall through to the spawn helper
    /// with the same args (the daemon raced into a "no live
    /// session for this UID" state, so respawn is the right
    /// next step). And that Err case must be STRUCTURALLY
    /// DISTINCT from the "UID not in live set" arm — the test
    /// pin counts both arms separately so a future refactor
    /// that collapses them into the bare `result.ok()?` form
    /// trips this test.
    #[test]
    fn t_migrate_attach_failure_falls_back_to_spawn() {
        let src = crate::app::APP_SRC_FOR_SCAN;
        let sig = "fn spawn_restored_session(";
        let start = src.find(sig).expect("must find spawn_restored_session");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(rest[1..].find("\n    pub fn "))
            .chain(rest[1..].find("\n    pub(crate) fn "))
            .chain(rest[1..].find("\n    pub(super) fn "))
            .chain(rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // The attach result must be bound (not stored directly
        // as the function-level `result` via the previous
        // `if/else`-yielded shape — that shape can't fall
        // through to spawn on attach Err).
        let attach_call_idx = body
            .find("Some(try_attach_via_daemon_with_deps(")
            .expect(
                "spawn_restored_session MUST wrap the attach \
                 call in `Some(try_attach_via_daemon_with_deps(\
                 ...))` so the attach Err can be matched and \
                 fallen-through (Issue F)",
            );
        let match_idx = body
            .find("match attach_result {")
            .expect(
                "spawn_restored_session MUST dispatch attach \
                 outcome via `match attach_result {` so the Err \
                 arm can fall back to spawn (Issue F)",
            );
        assert!(
            attach_call_idx < match_idx,
            "the attach call MUST precede the match on its \
             result (Issue F); attach at {}, match at {}",
            attach_call_idx,
            match_idx,
        );

        // The attach-failure arm (`Some(Err(...))`) MUST call
        // `self.try_spawn_via_daemon(` — not just propagate
        // Err. The "UID not in live set" arm (`None`) also
        // calls the same helper, but in a separate match arm.
        // Count: spawn helper called TWICE inside the match —
        // once per non-Ok arm.
        let match_end = body[match_idx..]
            .find("\n        } else {")
            .map(|i| match_idx + i)
            .unwrap_or(body.len());
        let match_slice = &body[match_idx..match_end];
        let spawn_calls = match_slice.matches("self.try_spawn_via_daemon(").count();
        assert_eq!(
            spawn_calls, 2,
            "spawn_restored_session's attach-result match MUST \
             call `self.try_spawn_via_daemon(` in TWO separate \
             arms — `Some(Err(...))` (attach raced) and `None` \
             (UID not in live set). Pre-fix the Err arm used \
             `result.ok()?` and dropped the manifest entry. \
             got count: {}\n\nmatch slice:\n{}",
            spawn_calls,
            match_slice,
        );

        // Both non-Ok arms must be structurally present.
        assert!(
            match_slice.contains("Some(Err(attach_err)) =>")
                || match_slice.contains("Some(Err(e)) =>"),
            "spawn_restored_session MUST have an explicit \
             `Some(Err(...))` arm that falls back to spawn \
             (Issue F); match slice:\n{}",
            match_slice,
        );
        assert!(
            match_slice.contains("None =>"),
            "spawn_restored_session MUST keep the explicit \
             `None =>` arm so the 'UID not in live set' path \
             stays structurally distinct from the attach-Err \
             arm (Issue F); match slice:\n{}",
            match_slice,
        );

        // The attach-Err arm logs the race — a future
        // refactor that silently swallows the Err would trip
        // this pin. Doc-comment pin: 'fall back' or
        // 'fall-through' phrasing on the rationale.
        assert!(
            match_slice.contains("falling back to start_session")
                || match_slice.contains("fallback to spawn")
                || match_slice.contains("retrying as spawn"),
            "the attach-Err arm MUST log the race so an \
             operator can see the recovery happened (Issue F); \
             match slice:\n{}",
            match_slice,
        );
    }


    /// migrate-tui-local Issue H: A-s spawns bash sessions
    /// daemon-owned, so the restore paths MUST also route bash
    /// through the daemon. Pre-fix the restore paths gated the
    /// daemon branch on `"claude" | "codex"` and fell back to
    /// `Session::new("/bin/bash", ...)` for bash — producing a
    /// local TUI-owned session and, when the daemon survived
    /// the restart, a UID collision against the still-live
    /// daemon bash session.
    ///
    /// Pinned at both restore sites: the match arm INCLUDES
    /// `"bash"` alongside `"claude" | "codex"`, and there is NO
    /// `Session::new("/bin/bash"` inside either restore function
    /// for the daemon-eligible branch (the cloud-VM bash SSH
    /// case at the top of `spawn_restored_session` is a
    /// distinct branch that stays — `gcloud compute ssh ... -t
    /// '... tmux ...'`).
    #[test]
    fn t_migrate_bash_restore_routes_through_daemon() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        // 1. spawn_restored_session: the daemon-routed `else if`
        // includes "bash".
        let spawn_idx = src
            .find("fn spawn_restored_session(")
            .expect("spawn_restored_session must exist");
        let spawn_rest = &src[spawn_idx..];
        let spawn_end = spawn_rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(spawn_rest[1..].find("\n    pub fn "))
            .chain(spawn_rest[1..].find("\n    pub(crate) fn "))
            .chain(spawn_rest[1..].find("\n    pub(super) fn "))
            .chain(spawn_rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(spawn_rest.len());
        let spawn_body = &spawn_rest[..spawn_end];
        assert!(
            spawn_body.contains(
                "matches!(entry.session_type.as_str(), \"claude\" | \"codex\" | \"bash\")",
            ),
            "spawn_restored_session's daemon-routed arm MUST \
             include `\"bash\"` alongside `\"claude\" | \
             \"codex\"` (Issue H); body excerpt:\n{}",
            &spawn_body[..spawn_body.len().min(2000)],
        );

        // 2. restore_tombstones_for_workspace: the match arm
        // includes "bash".
        let tomb_idx = src
            .find("fn restore_tombstones_for_workspace(")
            .expect("restore_tombstones_for_workspace must exist");
        let tomb_rest = &src[tomb_idx..];
        let tomb_end = tomb_rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(tomb_rest[1..].find("\n    pub fn "))
            .chain(tomb_rest[1..].find("\n    pub(crate) fn "))
            .chain(tomb_rest[1..].find("\n    pub(super) fn "))
            .chain(tomb_rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(tomb_rest.len());
        let tomb_body = &tomb_rest[..tomb_end];
        assert!(
            tomb_body.contains("\"claude\" | \"codex\" | \"bash\""),
            "restore_tombstones_for_workspace's match arm MUST \
             include `\"bash\"` alongside `\"claude\" | \
             \"codex\"` (Issue H); body excerpt:\n{}",
            &tomb_body[..tomb_body.len().min(2000)],
        );

        // 3. Neither restore function spawns local bash for the
        // daemon-eligible branch. The cloud-VM SSH branch at
        // the top of spawn_restored_session uses
        // `Session::new("gcloud", ...)` (not "/bin/bash"), so
        // searching for `Session::new("/bin/bash"` catches only
        // the regression form. The lingering `_ =>
        // Session::new("/bin/bash", ...)` defensive fallback in
        // restore_tombstones_for_workspace is reachable ONLY
        // for unknown session_types (claude/codex/bash all
        // route through the daemon arm above); leave it as
        // defense-in-depth but pin that bash itself doesn't
        // reach it.
        //
        // Use a co-location check: every `Session::new(\"/bin/bash\"`
        // in a restore function must be the `_ =>` defensive
        // catch-all (not the bash arm). We assert via an
        // arm-ordering pin — `"bash"` matches the daemon arm
        // BEFORE any `_ =>` falls through.
        let bash_arm_idx = tomb_body
            .find("\"claude\" | \"codex\" | \"bash\"")
            .expect("daemon arm must mention bash");
        let catchall_idx = tomb_body.find("_ => Session::new(");
        if let Some(co) = catchall_idx {
            assert!(
                bash_arm_idx < co,
                "the bash daemon arm MUST precede the `_ => \
                 Session::new(...)` catch-all so bash routes \
                 through the daemon (Issue H); bash arm at {}, \
                 catch-all at {}",
                bash_arm_idx,
                co,
            );
        }

        // spawn_restored_session: post-Issue-H the only
        // bash-via-Session::new path inside the function is the
        // explicit cloud-VM SSH branch (which uses "gcloud",
        // not "/bin/bash"). Confirm that no `Session::new("/bin/bash"`
        // is reached for entries whose session_type would match
        // the daemon arm — the surviving `else { ... }` falls
        // through to Session::new("/bin/bash", ...) but is only
        // reachable for unknown types since claude/codex/bash
        // all match the daemon arm above. Pin the arm-ordering.
        let bash_arm_in_spawn = spawn_body
            .find("matches!(entry.session_type.as_str(), \"claude\" | \"codex\" | \"bash\")")
            .expect("daemon arm in spawn_restored_session");
        if let Some(local_bash_idx) = spawn_body.find("Session::new(\"/bin/bash\"") {
            assert!(
                bash_arm_in_spawn < local_bash_idx,
                "the bash daemon arm MUST precede any local \
                 /bin/bash fallback in spawn_restored_session \
                 (Issue H); daemon arm at {}, local-bash at {}",
                bash_arm_in_spawn,
                local_bash_idx,
            );
        }
    }

    /// migrate-tui-local Issue I: UI A-f launches MUST thread
    /// the cursor's task scope through to `App::launch_workflow`
    /// so the daemon records `DaemonSession.task_id` for fresh
    /// participants. Pre-fix the handler hardcoded `None`, so
    /// an A-f on a `Cursor::Task` (tasked planning workspace,
    /// or tasked focused session) silently lost the task scope
    /// and reproduced the Issue G failure for the UI path.
    ///
    /// Pinned at four points:
    ///   1. `InputMode::WorkflowLaunchConfirm` carries a
    ///      `cursor_task_id` field.
    ///   2. `InputMode::WorkflowPicker` carries a
    ///      `cursor_task_id` field.
    ///   3. `open_workflow_launch` captures
    ///      `cursor_task_id` from `Cursor::Task { task_id, .. }`.
    ///   4. The `SubmitAction::LaunchWorkflow` handler at the
    ///      App level passes the captured value to
    ///      `self.launch_workflow(...)` — NOT a hardcoded
    ///      `None`.
    #[test]
    fn t_migrate_workflow_ui_launch_threads_cursor_task_id() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        // 1. & 2. InputMode variants carry the field.
        assert!(
            src.contains(
                "    /// Picking which workflow to launch when more than one is defined.\n    WorkflowPicker {",
            ),
            "WorkflowPicker variant doc-comment marker must exist",
        );
        // Both variants should be followed by a `cursor_task_id`
        // field declaration. Search both variant bodies.
        let picker_idx = src
            .find("    /// Picking which workflow to launch when more than one is defined.\n    WorkflowPicker {")
            .expect("WorkflowPicker variant must exist");
        let picker_end = src[picker_idx..]
            .find("    },\n")
            .map(|i| picker_idx + i + 6)
            .unwrap_or(src.len());
        let picker_body = &src[picker_idx..picker_end];
        assert!(
            picker_body.contains("cursor_task_id: Option<String>"),
            "InputMode::WorkflowPicker MUST carry \
             `cursor_task_id: Option<String>` (Issue I); \
             variant body:\n{}",
            picker_body,
        );

        let confirm_idx = src
            .find("    /// Confirming launch of a workflow on a workspace.\n    WorkflowLaunchConfirm {")
            .expect("WorkflowLaunchConfirm variant must exist");
        let confirm_end = src[confirm_idx..]
            .find("    },\n")
            .map(|i| confirm_idx + i + 6)
            .unwrap_or(src.len());
        let confirm_body = &src[confirm_idx..confirm_end];
        assert!(
            confirm_body.contains("cursor_task_id: Option<String>"),
            "InputMode::WorkflowLaunchConfirm MUST carry \
             `cursor_task_id: Option<String>` (Issue I); \
             variant body:\n{}",
            confirm_body,
        );

        // 3. open_workflow_launch captures cursor_task_id from
        // Cursor::Task.
        let open_idx = src
            .find("fn open_workflow_launch(&mut self)")
            .expect("open_workflow_launch must exist");
        let open_rest = &src[open_idx..];
        let open_end = open_rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(open_rest[1..].find("\n    pub fn "))
            .chain(open_rest[1..].find("\n    pub(crate) fn "))
            .chain(open_rest[1..].find("\n    pub(super) fn "))
            .chain(open_rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(open_rest.len());
        let open_body = &open_rest[..open_end];
        // Three-arm destructure (Session/Workspace/Task) — all
        // three branches must yield a (wi, focused_si,
        // cursor_task_id) triple.
        assert!(
            open_body.contains("let (wi, focused_si, cursor_task_id)"),
            "open_workflow_launch MUST bind a three-arm \
             `(wi, focused_si, cursor_task_id)` tuple from the \
             cursor (Issue I); body excerpt:\n{}",
            &open_body[..open_body.len().min(2000)],
        );
        // The Cursor::Task arm yields Some(task_id) for the
        // task scope.
        assert!(
            open_body.contains("Cursor::Task { ws_idx, task_id }")
                && open_body.contains("(ws_idx, si, Some(task_id))"),
            "the Cursor::Task arm of open_workflow_launch MUST \
             yield `Some(task_id)` so the daemon records the \
             task scope (Issue I); body excerpt:\n{}",
            &open_body[..open_body.len().min(2000)],
        );

        // 4. SubmitAction::LaunchWorkflow handler forwards
        // cursor_task_id (not None).
        //
        // Find the App-level handler block for
        // SubmitAction::LaunchWorkflow.
        let handler_idx = src
            .find("SubmitAction::LaunchWorkflow {\n                ws_id,")
            .expect("LaunchWorkflow handler must exist");
        let handler_rest = &src[handler_idx..];
        let handler_end = handler_rest
            .find("\n            SubmitAction::MarkActiveDone")
            .map(|i| i)
            .unwrap_or(handler_rest.len().min(2000));
        let handler_body = &handler_rest[..handler_end];
        assert!(
            handler_body.contains("cursor_task_id,"),
            "the SubmitAction::LaunchWorkflow destructure MUST \
             pull out `cursor_task_id` (Issue I); handler \
             body:\n{}",
            handler_body,
        );
        // Phase 4 §E: the handler routes to the daemon
        // (`launch_workflow_via_daemon`) and MUST still forward
        // `cursor_task_id` (NOT a hardcoded None).
        assert!(
            handler_body.contains(
                "self.launch_workflow_via_daemon(\n                    &ws_id,\n                    &workflow_name,\n                    &slots,\n                    goal,\n                    cursor_task_id,",
            ),
            "the SubmitAction::LaunchWorkflow handler MUST \
             forward the launch `slots` (Phase 3) AND \
             `cursor_task_id` (NOT a hardcoded `None`) \
             to self.launch_workflow_via_daemon (Phase 4); handler \
             body:\n{}",
            handler_body,
        );
        // Regression pin: the pre-fix `None` hardcode is gone.
        // Assemble the needle dynamically so the test file's
        // include_str!(...) read doesn't self-match the literal.
        let pre_fix_needle = format!(
            "{}({}, &workflow_name, goal, None)",
            "self.launch_workflow_via_daemon", "ws_index",
        );
        assert!(
            !src.contains(&pre_fix_needle),
            "the pre-fix hardcoded-None call MUST be removed (Issue I / Phase 4)",
        );

        // SubmitAction::EnterWorkflowLaunchConfirm + LaunchWorkflow
        // variants carry the cursor_task_id field on the wire.
        assert!(
            src.contains("EnterWorkflowLaunchConfirm {\n        ws_id: String,\n        focused_si: Option<usize>,\n        workflow_name: String,\n        /// migrate-tui-local Issue I"),
            "SubmitAction::EnterWorkflowLaunchConfirm MUST \
             carry a `cursor_task_id` field (Issue I)",
        );
        assert!(
            src.contains("LaunchWorkflow {\n        ws_id: String,\n        workflow_name: String,\n        slots: Vec<WorkflowSlotChoice>,\n        goal: Option<String>,\n        /// migrate-tui-local Issue I"),
            "SubmitAction::LaunchWorkflow MUST carry a \
             `cursor_task_id` field (Issue I)",
        );
    }

    /// migrate-tui-local Issue J: the attach branch of
    /// `spawn_restored_session` MUST NOT prime
    /// `pending_jsonl_files` for the restored session. The
    /// daemon's transcript binding survived the TUI restart, so
    /// the Codex rebind detector at `app.rs::6203` would
    /// otherwise treat an unrelated rollout JSONL created
    /// elsewhere on the system as a rebind candidate and
    /// overwrite the legitimate transcript_id.
    ///
    /// Pinned by four properties:
    ///   1. A `RestoreOutcome` enum exists with `Attached` and
    ///      `Spawned` variants.
    ///   2. `spawn_restored_session` returns
    ///      `Option<(TerminalSession, RestoreOutcome)>` so the
    ///      caller can observe the outcome.
    ///   3. The attach-success arm sets
    ///      `outcome = RestoreOutcome::Attached` BEFORE
    ///      yielding the Session.
    ///   4. The `pending` computation is gated on the outcome:
    ///      Attached → `None` (no priming); Spawned → the
    ///      pre-fix Codex baseline / empty-Vec logic applies.
    #[test]
    fn t_migrate_attach_branch_skips_codex_jsonl_rebind() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        // 1. RestoreOutcome enum exists with both variants.
        let enum_idx = src
            .find("pub(crate) enum RestoreOutcome {")
            .expect(
                "RestoreOutcome enum MUST exist with pub(crate) \
                 visibility (Issue J)",
            );
        let enum_rest = &src[enum_idx..];
        let enum_end = enum_rest
            .find('}')
            .map(|i| i + 1)
            .unwrap_or(enum_rest.len());
        let enum_body = &enum_rest[..enum_end];
        assert!(
            enum_body.contains("Attached,"),
            "RestoreOutcome MUST declare `Attached` variant \
             (Issue J); body:\n{}",
            enum_body,
        );
        assert!(
            enum_body.contains("Spawned,"),
            "RestoreOutcome MUST declare `Spawned` variant \
             (Issue J); body:\n{}",
            enum_body,
        );

        // 2. spawn_restored_session signature returns the
        // tuple.
        let fn_idx = src
            .find("fn spawn_restored_session(")
            .expect("spawn_restored_session must exist");
        let fn_rest = &src[fn_idx..];
        let body_open = fn_rest.find('{').expect("body open brace");
        let header = &fn_rest[..body_open];
        assert!(
            header.contains("-> Option<(TerminalSession, RestoreOutcome)>"),
            "spawn_restored_session MUST return \
             `Option<(TerminalSession, RestoreOutcome)>` so the \
             caller observes the attach-vs-spawn outcome (Issue \
             J); signature header:\n{}",
            header,
        );

        // 3. The attach-success arm sets outcome = Attached.
        let body_end = fn_rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(fn_rest[1..].find("\n    pub fn "))
            .chain(fn_rest[1..].find("\n    pub(crate) fn "))
            .chain(fn_rest[1..].find("\n    pub(super) fn "))
            .chain(fn_rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(fn_rest.len());
        let body = &fn_rest[..body_end];
        assert!(
            body.contains("outcome = RestoreOutcome::Attached;"),
            "the attach-success arm of spawn_restored_session \
             MUST set `outcome = RestoreOutcome::Attached;` \
             (Issue J); body excerpt:\n{}",
            &body[..body.len().min(2500)],
        );
        // Ordering: the outcome flip must precede the Codex
        // primer evaluation. Find the attach-success block and
        // the primer match.
        let attach_set_idx = body
            .find("outcome = RestoreOutcome::Attached;")
            .expect("outcome flip presence already asserted");
        let primer_idx = body
            .find("let pending = match outcome {")
            .expect(
                "the `pending` computation MUST be a `match \
                 outcome` block so the attach arm can return \
                 None (Issue J)",
            );
        assert!(
            attach_set_idx < primer_idx,
            "the attach-success outcome flip MUST execute \
             BEFORE the `pending` computation reads it (Issue \
             J); attach-set at {}, primer at {}",
            attach_set_idx,
            primer_idx,
        );

        // 4. The Attached arm yields None, the Spawned arm
        // keeps the pre-fix logic.
        let primer_rest = &body[primer_idx..];
        let primer_end = primer_rest
            .find("};\n")
            .map(|i| i + 3)
            .unwrap_or(primer_rest.len());
        let primer_block = &primer_rest[..primer_end];
        assert!(
            primer_block.contains("RestoreOutcome::Attached => None,"),
            "the `pending` match's Attached arm MUST yield \
             `None` so no JSONL rebind priming happens on the \
             attach path (Issue J); primer block:\n{}",
            primer_block,
        );
        assert!(
            primer_block.contains("RestoreOutcome::Spawned =>"),
            "the `pending` match MUST keep the Spawned arm so \
             fresh start_session restores still prime the \
             rebind window (Issue J); primer block:\n{}",
            primer_block,
        );
        // The Spawned arm preserves the pre-fix behaviors:
        // codex_resume_baseline for transcript_id-Some, and
        // Some(Vec::new()) for claude|codex without a
        // transcript yet.
        assert!(
            primer_block.contains("codex_resume_baseline"),
            "the Spawned arm MUST preserve the \
             `codex_resume_baseline` priming for fresh codex \
             resumes (Issue J); primer block:\n{}",
            primer_block,
        );
        assert!(
            primer_block.contains("Some(Vec::new())"),
            "the Spawned arm MUST preserve the empty-Vec \
             priming for fresh claude/codex spawns (Issue J); \
             primer block:\n{}",
            primer_block,
        );

        // Caller-side: restore_sessions destructures the tuple.
        let restore_idx = src
            .find("fn restore_sessions(")
            .expect("restore_sessions must exist");
        let restore_rest = &src[restore_idx..];
        let restore_end = restore_rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(restore_rest[1..].find("\n    pub fn "))
            .chain(restore_rest[1..].find("\n    pub(crate) fn "))
            .chain(restore_rest[1..].find("\n    pub(super) fn "))
            .chain(restore_rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(restore_rest.len());
        let restore_body = &restore_rest[..restore_end];
        assert!(
            restore_body
                .contains("if let Some((ts, _outcome)) = self.spawn_restored_session("),
            "restore_sessions MUST destructure the \
             `(TerminalSession, RestoreOutcome)` tuple returned \
             by spawn_restored_session (Issue J); restore_sessions \
             body excerpt:\n{}",
            &restore_body[..restore_body.len().min(2000)],
        );
    }

    /// migrate-tui-local Issue K: the tombstone-restore daemon-
    /// spawn call MUST pass the tombstone's user-visible label
    /// (`&tomb.label`) as the daemon label arg — NOT
    /// `session_type`. The daemon stores its own copy from
    /// `start_session`'s `label` field and surfaces it via MCP
    /// `list_sessions`. Pre-fix the same `session_type` value
    /// went into both the type slot AND the label slot, so
    /// restored tombstones showed up daemon-side labelled
    /// `claude` / `codex` / `bash` instead of `reviewer` /
    /// `planner` / etc.
    #[test]
    fn t_migrate_tombstone_restore_uses_label_not_session_type() {
        let src = crate::app::APP_SRC_FOR_SCAN;

        // Bound the tombstone-restore function body.
        let fn_idx = src
            .find("fn restore_tombstones_for_workspace(")
            .expect("restore_tombstones_for_workspace must exist");
        let fn_rest = &src[fn_idx..];
        let fn_end = fn_rest[1..]
            .find("\n    fn ")
            .into_iter()
            .chain(fn_rest[1..].find("\n    pub fn "))
            .chain(fn_rest[1..].find("\n    pub(crate) fn "))
            .chain(fn_rest[1..].find("\n    pub(super) fn "))
            .chain(fn_rest[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| 1 + i)
            .unwrap_or(fn_rest.len());
        let body = &fn_rest[..fn_end];

        // Locate the daemon-spawn call inside the function and
        // inspect its argv list.
        let call_idx = body
            .find("self.try_spawn_via_daemon(")
            .expect(
                "restore_tombstones_for_workspace MUST call \
                 self.try_spawn_via_daemon (post-migrate-tui-local)",
            );
        let call_end = body[call_idx..]
            .find(") {")
            .map(|i| call_idx + i)
            .unwrap_or(body.len());
        let call_args = &body[call_idx..call_end];

        // The label arg MUST be `&tomb.label` — the tombstone's
        // user-visible label, not the session_type discriminator.
        assert!(
            call_args.contains("&tomb.label,"),
            "the tombstone-restore daemon spawn MUST pass \
             `&tomb.label` as the label arg so the daemon's \
             stored title matches the user-visible label (Issue \
             K); call args:\n{}",
            call_args,
        );

        // Regression: the pre-fix shape passed `session_type` in
        // both consecutive slots (type AND label). The label
        // arg must NOT appear immediately after the type arg as
        // a duplicated `session_type,` line.
        //
        // Slice just past the type arg (the first `session_type,`
        // line) and check the next non-whitespace token is NOT
        // another `session_type,`.
        let type_slot_idx = call_args
            .find("session_type,\n")
            .expect("call must pass session_type as the type arg");
        let post_type = &call_args[type_slot_idx + "session_type,\n".len()..];
        // The next non-whitespace token must be the label arg
        // — currently `&tomb.label,`. A regression that puts
        // `session_type,` back in would fail this pin.
        let trimmed = post_type.trim_start();
        assert!(
            !trimmed.starts_with("session_type,"),
            "regression pin: the label slot in the tombstone-\
             restore daemon spawn MUST NOT be `session_type,` \
             (Issue K — that was the pre-fix bug that made \
             restored sessions show up as `claude`/`codex`/\
             `bash` daemon-side); call args:\n{}",
            call_args,
        );
        assert!(
            trimmed.starts_with("&tomb.label,"),
            "the label slot MUST be `&tomb.label,` immediately \
             after the type slot (Issue K); post-type slice \
             head:\n{}",
            &trimmed[..trimmed.len().min(200)],
        );
    }
}

#[cfg(test)]
mod revive_session_tests {
    //! A-R (`revive_active_session`) behavior pins: the guards that keep
    //! revive scoped to genuinely-dead, non-workflow sessions, and the
    //! remote branch's fail-closed handling when the host daemon can't be
    //! reached. The daemon-side respawn mechanics are pinned in
    //! `cm-daemon`'s `revive_*` tests; the local spawn path reuses
    //! `spawn_restored_session`, pinned by the startup-restore suite.
    use super::*;
    use std::collections::HashMap;

    fn dummy_ts(uid: &str, exited: bool, host: cm_daemon::host_id::HostId) -> TerminalSession {
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
        session.exited = exited;
        TerminalSession {
            color: None,
            uid: uid.into(),
            label: "claude".into(),
            session_type: "claude".into(),
            session,
            status: SessionStatus::Idle,
            idle_since: None,
            last_write_at: None,
            transcript_id: Some("11111111-2222-3333-4444-555555555555".into()),
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
        }
    }

    fn app_with_session(ts: TerminalSession) -> App {
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        app.workspaces.push(Workspace {
            color: None,
            pinned: false,
            id: "ws-revive".into(),
            name: "revive".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: Some(std::env::temp_dir()),
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![ts],
            tombstones: Vec::new(),
            is_pushing: false,
        });
        app.cursor = Cursor::Session(0, 0);
        // Manifest saves no-op (no disk writes) — in-memory asserts only.
        app.sessions_restored = false;
        app
    }

    fn status_text(app: &App) -> String {
        app.status_msg
            .as_ref()
            .map(|(m, _)| m.clone())
            .unwrap_or_default()
    }

    /// Revive only applies to a DEAD session: a live one is refused with
    /// a status hint and the slot is untouched.
    #[test]
    fn revive_refuses_non_exited_session() {
        let ts = dummy_ts("ts-aaaaaaaaaaaaaa01-0", false, cm_daemon::host_id::HostId::local());
        let mut app = app_with_session(ts);
        app.revive_active_session();
        assert!(
            status_text(&app).contains("not dead"),
            "live session refused with a hint, got: {}",
            status_text(&app),
        );
        assert_eq!(app.workspaces[0].sessions[0].uid, "ts-aaaaaaaaaaaaaa01-0");
        assert!(!app.workspaces[0].sessions[0].session.exited);
    }

    /// Workflow participants are workflow-engine-owned; revive refuses
    /// them and points at A-u (resume the run) instead.
    #[test]
    fn revive_refuses_workflow_participant() {
        let mut ts =
            dummy_ts("ts-aaaaaaaaaaaaaa02-0", true, cm_daemon::host_id::HostId::local());
        ts.workflow_run_id = Some("wf-1".into());
        let mut app = app_with_session(ts);
        app.revive_active_session();
        assert!(
            status_text(&app).contains("Workflow participant"),
            "workflow participant refused, got: {}",
            status_text(&app),
        );
        assert!(
            app.workspaces[0].sessions[0].session.exited,
            "slot left as-is",
        );
    }

    /// Continuous sessions are scheduler-owned; revive refuses them (an
    /// on-demand revive could double-spawn against supervision).
    #[test]
    fn revive_refuses_continuous_session() {
        let mut ts =
            dummy_ts("ts-aaaaaaaaaaaaaa04-0", true, cm_daemon::host_id::HostId::local());
        ts.continuous_task_id = Some("ct-1".into());
        let mut app = app_with_session(ts);
        app.revive_active_session();
        assert!(
            status_text(&app).contains("Continuous session"),
            "continuous session refused, got: {}",
            status_text(&app),
        );
        assert!(
            app.workspaces[0].sessions[0].session.exited,
            "slot left as-is",
        );
    }

    /// Remote branch, fail-closed: when the `session.revive` RPC can't
    /// reach the host daemon, the slot stays dead and nothing is queued
    /// for reattach — no half-revived state.
    #[test]
    fn revive_remote_rpc_failure_leaves_slot_dead() {
        let ghost = cm_daemon::host_id::HostId::new("ghost");
        let ts = dummy_ts("ts-aaaaaaaaaaaaaa03-0", true, ghost.clone());
        let mut app = app_with_session(ts);
        // A unix host whose socket doesn't exist: `for_host` resolves a
        // path, the RPC dial fails immediately (no ssh, no timeout).
        let hosts = crate::hosts::HostsConfig {
            hosts: vec![
                crate::hosts::HostConfig {
                    id: cm_daemon::host_id::HostId::local(),
                    transport: crate::hosts::HostTransport::Unix {
                        socket: std::env::temp_dir()
                            .join("cm-revive-test-nonexistent-local.sock"),
                    },
                    default: true,
                },
                crate::hosts::HostConfig {
                    id: ghost,
                    transport: crate::hosts::HostTransport::Unix {
                        socket: std::env::temp_dir()
                            .join("cm-revive-test-nonexistent-ghost.sock"),
                    },
                    default: false,
                },
            ],
        };
        app.host_pool = std::sync::Arc::new(
            crate::host_pool::HostPool::from_config(&hosts).expect("pool"),
        );
        app.revive_active_session();
        assert!(
            status_text(&app).starts_with("Revive failed"),
            "RPC failure surfaced, got: {}",
            status_text(&app),
        );
        let ts = &app.workspaces[0].sessions[0];
        assert!(ts.session.exited, "slot stays dead on RPC failure");
        assert!(
            app.pending_remote_reattach.is_empty(),
            "nothing queued for reattach on RPC failure",
        );
        assert!(
            !app.reconnecting_sessions.contains(&ts.uid),
            "not marked reconnecting on RPC failure",
        );
    }
}
