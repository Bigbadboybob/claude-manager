//! JSON-RPC method bodies that take `&mut DaemonState`. Slice 10c-b
//! seeds this module with `start_session`; slice 10c-d will add the
//! other session-mutation methods (`send_input`, `kill_session`,
//! `read_session_output`) as the TUI's `tui/src/control/methods.rs`
//! relocates here function-by-function.
//!
//! ## Why pure functions
//!
//! Method bodies live as standalone functions (not impl methods on
//! `DaemonState`) for the same reason `crate::attach`'s handlers do:
//! they're unit-testable without instantiating the dispatcher, the
//! wire layer, or a real accept loop. The dispatcher in
//! `crate::control::dispatch` is the only wrapper that knows about
//! `Caller` / `Request` / `Response` shapes; everything below it
//! takes plain Rust values.
//!
//! ## Authorization disposition at 10c-b
//!
//! `start_session` is **Operator-callable only** at this slice. The
//! reasoning:
//!
//! - The TUI path that will use it (slice 10c-e's `A-n` / `A-s` rewire)
//!   talks to the daemon as an Operator.
//! - The MCP-agent path (`Caller::Session` reaching `start_session`)
//!   needs descendant-task-tree validation against a *live* task list,
//!   which `DaemonState` doesn't have yet (the manifest snapshot is
//!   read-only and stale; the `tasks` field arrives with 10c-d when
//!   `send_input` and friends relocate). Rather than admit Session
//!   callers without validation, return `Unauthorized` with a pointer
//!   to the slice that lights this up — same pattern slice-10b used
//!   to punt `session.attach`.
//!
//! This is intentionally tighter than necessary for 10c-b's working-set
//! check ("TUI behavior unchanged") and graduates with 10c-d.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{json, Value};

use base64::Engine;

use crate::control::protocol::{Caller, ErrorCode};
use crate::session::{DaemonExitStatus, PendingSession, SpawnParams};
use crate::state::DaemonState;

/// Standard-padded base64. Matches `control::stream`'s encoding so
/// `read_session_output` bytes decode the same way TUI-side as
/// streamed attach bytes.
const BASE64: base64::engine::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Cap on the `text` field of `send_input`. The wire frame is
/// already capped at 4 MiB; this tighter cap on the actual input
/// is a sanity check against an agent sending pasted-in megabytes
/// straight at a PTY. 64 KiB covers any realistic single keystroke
/// + clipboard paste while leaving plenty of slack under the wire
/// cap.
/// Maximum per-frame payload accepted by the daemon's
/// `send_input` JSON-RPC path and the attach-stream Input
/// branch. Both surfaces reject (RPC: InvalidParams + error /
/// stream: log + skip + continue) any frame whose decoded
/// payload exceeds this cap.
///
/// Public so the TUI's `StreamWriter::write` can mirror it via
/// `tui::term_shim::MAX_INPUT_FRAME_BYTES` and chunk client-side
/// (slice 10c-e-3b-fix5). The `client_and_daemon_input_caps_agree`
/// unit test in `tui/src/term_shim.rs::tests` pins the
/// constants in lockstep; if this value moves, that test fails
/// at build time and points at the mirror.
pub const MAX_SEND_INPUT_BYTES: usize = 64 * 1024;

/// Sub-2b-3 review-9: default bound on how long
/// `mcp_start_session` waits for the per-worktree slot
/// before returning `Conflict`. 20s leaves ~10s of headroom
/// under the Python `control_client.call()` 30s default
/// timeout — see [`set_slot_wait_timeout_for_test`] for the
/// test override.
pub const SLOT_WAIT_TIMEOUT_DEFAULT_MS: u64 = 20_000;

/// Atomic override (milliseconds) for the
/// `mcp_start_session` per-worktree slot-wait timeout. `0`
/// means "use the default of [`SLOT_WAIT_TIMEOUT_DEFAULT_MS`]".
/// Tests set this to a short value (e.g. 1 second) so the
/// timeout path is exercised without making the test suite
/// wait the full 20s. Production never touches this — only
/// the `set_slot_wait_timeout_for_test` helper does, and it's
/// only callable inside `#[cfg(test)]`.
static SLOT_WAIT_TIMEOUT_OVERRIDE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn slot_wait_timeout() -> std::time::Duration {
    let override_ms = SLOT_WAIT_TIMEOUT_OVERRIDE_MS
        .load(std::sync::atomic::Ordering::Relaxed);
    let ms = if override_ms == 0 {
        SLOT_WAIT_TIMEOUT_DEFAULT_MS
    } else {
        override_ms
    };
    std::time::Duration::from_millis(ms)
}

/// Test-only: override the slot-wait timeout for the
/// duration of a test. Returns a guard that restores the
/// previous value on drop. Sub-2b-3 review-9 — exercised by
/// the bounded-wait timeout test without making the suite
/// wait the full 20s.
#[cfg(test)]
pub fn set_slot_wait_timeout_for_test(timeout: std::time::Duration) -> SlotWaitTimeoutGuard {
    let ms = timeout.as_millis().min(u64::MAX as u128) as u64;
    let prev = SLOT_WAIT_TIMEOUT_OVERRIDE_MS.swap(ms, std::sync::atomic::Ordering::Relaxed);
    SlotWaitTimeoutGuard { prev }
}

#[cfg(test)]
pub struct SlotWaitTimeoutGuard {
    prev: u64,
}

#[cfg(test)]
impl Drop for SlotWaitTimeoutGuard {
    fn drop(&mut self) {
        SLOT_WAIT_TIMEOUT_OVERRIDE_MS.store(self.prev, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Sub-2b-3 review-10: resolve the `hard_cap_bytes` value to
/// hand to `spawn_watcher` from the wire-supplied
/// `memory_cap_hard_bytes`.
///
/// Pre-review-10 the watcher-startup site passed literal `0`
/// even when the wire carried the real cap (review-4 #1 made
/// `memory_cap_hard_bytes` mandatory alongside
/// `memory_cap_bytes`), so the daemon-spawned path emitted
/// JSONL records with `"hard_cap_bytes": 0` — diverging from
/// the TUI-local watcher which emits the configured value.
/// The named acceptance criterion is "indistinguishable from
/// today's signal 9 toast"; divergent diagnostics break
/// that.
///
/// Defensive `None → 0`: review-4 #1's entry-point
/// validation rejects partial cap tuples, so on the validated
/// path `memory_cap_hard_bytes` MUST be `Some` when
/// `memory_cap_bytes` is. The `0` fallback guards against a
/// future wire-shape regression without changing observable
/// behavior on the validated path.
pub(crate) fn resolve_watcher_hard_cap_bytes(
    memory_cap_hard_bytes_wire: Option<u64>,
) -> u64 {
    memory_cap_hard_bytes_wire.unwrap_or(0)
}

/// Return type for every method handler in this module. Mirrors the
/// shape `tui/src/control/methods.rs::MethodResult` uses today, so
/// the eventual 10c-d relocation is mechanical.
pub type MethodResult = Result<Value, (ErrorCode, String)>;

/// `start_session` request params.
///
/// ## Wire shape (slice 10c-e-3b)
///
/// Pre-10c-e-3b this struct carried `type: "claude-code"|"codex"|"bash"`
/// and the daemon reconstructed argv via a hardcoded
/// `shell_for_type` map. That dropped everything the TUI's local
/// `Session::new` adds — `--mcp-config`, Codex MCP overrides,
/// `--resume` tokens, the systemd-run memory cap wrapper. The
/// daemon-spawned child would have no MCP servers configured,
/// resume tokens wouldn't connect to seeded transcripts, and cap
/// kills wouldn't fire. That's a material divergence from the
/// "no operator-visible behavior change" Phase 1 acceptance
/// criterion.
///
/// Post-10c-e-3b the daemon doesn't interpret an engine name. The
/// caller (TUI's `ClientSession::new`) builds full `argv`, full
/// `env`, the working directory, and the optional memory-cap
/// wrapper using the SAME code path the local `Session::new` uses
/// (`crate::mcp_config::build_args(SpawnTarget::Daemon, ...)` +
/// `tui::session::wrap_with_systemd_run`). The daemon's job
/// becomes "exec this argv with this env in this cwd" — agent-
/// specific knowledge stays TUI-side where the config files live.
#[derive(Deserialize)]
struct StartSessionParams {
    /// Stable session uid, pre-generated by the TUI before the
    /// MCP config file is written. Slice 10c-e-3b-fix: the TUI is
    /// the source of truth for uid identity — without this the
    /// daemon would mint its own uid inside `start_session`, but
    /// the TUI's `~/.cm/mcp/<uid>/claude.json` was already
    /// written with the pre-generated uid (because the env-block
    /// bakes `CM_TUI_SESSION_ID` at config-write time). Every
    /// downstream lookup (A-w kill, manifest binding, agent self-
    /// identification) keys on the same uid; a daemon-minted
    /// alternative would break all of those silently.
    ///
    /// The daemon validates the uid format (sanity, not security
    /// — the uid is forgeable by any caller with daemon-socket
    /// access) and checks for collision against
    /// `state.sessions`.
    uid: String,
    /// Stable workspace id to look up in `DaemonState.workspaces`.
    /// Used for manifest binding only — `working_dir` carries the
    /// actual cwd for the spawned child.
    workspace_id: String,
    /// Human-readable label shown in the sidebar. No semantic use.
    label: String,
    /// Full argv to exec. `argv[0]` is the program; any wrappers
    /// (e.g. `systemd-run --user --scope -- claude ...` for a
    /// memory-capped session) are baked in by the caller.
    argv: Vec<String>,
    /// Working directory for the spawned child. Pre-resolved to an
    /// absolute path on the TUI side so the daemon doesn't reach
    /// for `worktree_path` from its manifest snapshot — the caller
    /// is the source of truth.
    working_dir: String,
    /// Process env for the spawned child. The caller (TUI) populates
    /// this via `mcp_config::build_env(SpawnTarget::Daemon, ...)`
    /// so the spawned MCP server (if any) routes its callbacks to
    /// the daemon socket. The daemon doesn't inject anything beyond
    /// what's here.
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
    /// Initial PTY width (columns). Defaults to 80 — the
    /// `SpawnParams::new` baseline, used by non-TUI callers or
    /// older clients that don't yet send the field. The TUI's
    /// `rpc_start_session` always sends the live operator
    /// terminal size so the daemon-spawned PTY matches what the
    /// user is looking at (slice-10c-e-2 review-3 fix: prior to
    /// this the daemon always opened 80x24 and full-screen apps
    /// misrendered until the first reactive Resize frame).
    #[serde(default = "default_cols")]
    cols: u16,
    /// Initial PTY height (rows). Same backstory as `cols`.
    #[serde(default = "default_rows")]
    rows: u16,
    /// Auto-register fallback for workspaces created mid-session
    /// before slice 10e's `manifest.watch` ships. The daemon's
    /// workspace map is loaded once at startup from
    /// `~/.cm/tui-sessions.json`; any workspace the TUI creates via
    /// `A-n` after the daemon started won't be in that map. When the
    /// TUI passes this field alongside an unknown `workspace_id`, the
    /// daemon registers a minimal workspace entry on the fly and
    /// proceeds with the spawn. Once 10e lands the bidirectional
    /// manifest path, this field becomes redundant but stays
    /// backwards-compatible (the daemon will just ignore it if the
    /// workspace_id is already present).
    ///
    /// `None` preserves the prior "NotFound for unknown workspace_id"
    /// behavior so non-daemon-aware callers see no change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worktree_path: Option<String>,
    /// Soft cap byte count for the memory-cap wrapper (slice
    /// 10c-e-3b-fix2). When `Some`, the spawned child is assumed
    /// to be wrapped via `systemd-run --user --scope ...
    /// MemoryHigh=<this>` (the TUI builds the argv that way
    /// in `app.rs::try_spawn_via_daemon`). The daemon uses the
    /// presence of this field as the signal to populate
    /// `SpawnParams.kills_dir` so the End-frame cap-kill
    /// attribution path can scan for records past its baseline.
    ///
    /// The actual cgroup-OOM watcher that *writes* those records
    /// hasn't relocated to the daemon yet — that's slice
    /// 10d-memory-cap-relocation. Until then, no kill records
    /// land for daemon-spawned sessions, so the End-frame's
    /// `memory_cap_kill` will always be `false`. The plumbing is
    /// in place; the producer is the missing piece.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_cap_bytes: Option<u64>,
    /// Sub-2b-3 review-fix #1: hard cap byte count. Pre-fix
    /// only the soft byte count rode the wire (as a
    /// kills_dir signal); without the hard count, the
    /// daemon couldn't re-wrap argv for child spawns via
    /// `mcp_start_session`. The TUI now sends both so a
    /// capped agent's MCP-driven subtask spawn inherits the
    /// same (soft, hard) pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_cap_hard_bytes: Option<u64>,
    /// Sub-2b-3 review-fix #1: cgroup directory prefix where
    /// systemd-run lands the scope unit. Daemon's
    /// `mcp_start_session` reads this off the caller's
    /// session to build the child's scope path. `None` for
    /// uncapped sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cgroup_prefix: Option<String>,
    /// Session-type discriminator (`"claude-code"` / `"codex"` /
    /// `"bash"`). Slice 10d-mcp-surface-1 surfaces this on
    /// `list_sessions`; future slices (workflow controller
    /// engine selection) may branch on it. Defaults to
    /// `"claude-code"` for backwards compat with older TUI
    /// builds that don't yet send the field.
    #[serde(default = "default_session_type")]
    session_type: String,
    /// Parent session uid for sessions spawned via MCP
    /// `start_session` from an agent. Surfaced on
    /// `list_sessions`. `None` for operator-started sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    managed_by_uid: Option<String>,
    /// Planning task uid this session is bound to. Surfaced on
    /// `list_sessions`; slice 10d-mcp-surface-2 will use it for
    /// the Session-caller descendant-task-tree auth check.
    /// `None` for taskless sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    /// Sub-2b-1: transcript file path for this session, when the
    /// TUI knows it at spawn time. The daemon stores this on
    /// `DaemonSession.transcript_path` and surfaces it via the
    /// new `resolve_authorized_session` method so the Python
    /// MCP `read_session_output` tool can locate the file
    /// without round-tripping through the TUI's control socket.
    ///
    /// `None` is the common case at first spawn (Claude/Codex
    /// transcript file is born after the agent starts writing —
    /// the daemon doesn't know the path until a detection event
    /// fires). With `None` the resolver returns
    /// `state: "pending"` and the Python tool short-circuits to
    /// empty messages + poll-again behavior. A post-detection
    /// update RPC (out of 2b-1 scope) will let the TUI push the
    /// path once it's discovered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transcript_path: Option<String>,
    /// 10d-2c-1 review round-5 (F1): workflow run id this session
    /// is a participant of, when the spawn happens with workflow
    /// context already known. Stored on `DaemonSession` so
    /// `lookup_session_any` returns it for the auth check in
    /// `workflow_transition` / `workflow_done`. `None` for non-
    /// workflow spawns; the daemon's auth check then refuses any
    /// `workflow_transition` from this session, which is correct
    /// (a non-workflow daemon session has no business firing a
    /// transition).
    ///
    /// Note: this covers spawn-time tagging only. When the TUI
    /// launches a workflow on an already-spawned daemon session
    /// (the Existing-slot path in the former TUI
    /// controller's launch), it uses
    /// `session.set_workflow_context` to update the field
    /// after-the-fact — same DaemonSession field, different
    /// write path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workflow_run_id: Option<String>,
    /// Role name within the workflow run. See [`workflow_run_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workflow_role: Option<String>,
    /// Phase-1 continuous wire field: the continuous-task this
    /// spawn is a tick of. `None` for ordinary spawns; the trigger
    /// funnel that sets it lands in Phase 2. Plumbed onto the
    /// `DaemonSession` (and broadcast in the `Added` diff) beside
    /// the workflow tags. See DESIGN_CONTINUOUS_TASKS.md §6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    continuous_task_id: Option<String>,
    /// Global-permissions grant for the spawned session. The TUI
    /// (operator caller) sets this when the human marks a session
    /// global; `mcp_start_session` (agent caller) gates it behind
    /// the caller's own grant. Plumbed onto `DaemonSession.global_perms`
    /// and persisted in the manifest. Defaults to `false`.
    #[serde(default)]
    global_perms: bool,
}

fn default_session_type() -> String {
    "claude-code".to_string()
}

// Slice 10d-memory-cap-relocation review fix #1: the
// previously-existing `cgroup_path: Option<String>` field
// was REMOVED from this struct. Pre-fix the daemon trusted
// the operator-supplied path and the watcher would signal
// PIDs from it — a malicious caller could pre-populate a
// path with PIDs from unrelated processes (shell, other
// worker, etc.) and the daemon would SIGKILL them on the
// first breach. Post-fix the daemon DISCOVERS the cgroup
// from `/proc/<spawn-pid>/cgroup` after Phase 1 spawn (see
// `crate::path::discover_session_cgroup_path`) — the path
// is bound to *this* child by the kernel, not by the
// caller's word.
//
// Serde's default behavior is to silently ignore unknown
// fields (we do NOT use `#[serde(deny_unknown_fields)]`),
// so old TUI builds that still emit `cgroup_path` in
// `rpc_start_session` will continue to work; their value
// is simply dropped on the floor. A behavioral test pins
// this: `caller_supplied_cgroup_path_is_ignored`.

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

/// Spawn a new daemon-owned session.
///
/// ## What this does at 10c-b
///
/// - Validates `type_` against the same `{"claude-code","codex","bash"}`
///   allowlist `tui/src/control/methods.rs::start_session` enforces.
/// - Resolves `workspace_id` against the daemon's read-only manifest
///   snapshot (`DaemonState.workspaces`). Returns `NotFound` if the
///   workspace isn't in the snapshot; returns `Conflict` if it lacks
///   a `worktree_path` (the TUI's equivalent error path).
/// - Allocates a fresh session uid via [`new_session_uid`] using the
///   same `ts-<nanos>-<counter>` format as
///   `tui/src/app.rs::new_session_uid`. Daemon-minted uids are
///   indistinguishable from TUI-minted ones on the wire.
/// - Calls [`DaemonSession::spawn`] with the resolved working
///   directory, an env that injects `CM_TUI_SESSION_ID`, and the
///   binary name for the engine.
/// - Inserts the live session into `state.sessions` keyed by uid.
/// - Returns `{"session_uid": "<new uid>"}`.
///
/// ## What this does NOT do at 10c-b
///
/// - **Memory caps**: `DaemonSession` doesn't carry a `memory_cap`
///   field yet; that's slice 10c-d alongside the cap watcher
///   relocation.
/// - **MCP arg injection**: `claude-code` and `codex` get plain
///   `claude` / `codex` invocations. The TUI's `mcp_config::build_args`
///   adds `--mcp-config` and other flags that wire the MCP socket;
///   relocating that helper to the daemon is a 10c-e prerequisite.
/// - **Pending prompt delivery**: see [`StartSessionParams::prompt`].
/// - **Manifest write**: the daemon doesn't write
///   `~/.cm/tui-sessions.json` until slice 10e flips ownership. The
///   spawned session lives daemon-only at this slice; TUI sidebar
///   only learns about it when 10e's `manifest.watch` broadcasts a
///   diff.
pub fn start_session(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    start_session_with_spawn_fn(
        state_arc,
        params,
        crate::session_watch::default_watcher_spawn_fn(),
    )
}

/// Inner form that takes an injectable watcher-spawn factory.
/// Slice 10d-memory-cap-relocation review fix.
///
/// Tests pass a factory that returns `Err(io::Error)` to exercise
/// the spawn-failure path; production calls `start_session` which
/// supplies the real `Builder::new().name().spawn()` factory.
/// Surface kept `pub(crate)` so the focused failure-injection test
/// in this file's `tests` module can reach it without exposing the
/// injection to external callers.
pub(crate) fn start_session_with_spawn_fn(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
    watcher_spawn_fn: crate::session_watch::WatcherSpawnFn,
) -> MethodResult {
    let p: StartSessionParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;

    if p.argv.is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "argv must be non-empty (argv[0] is the program to exec)".into(),
        ));
    }

    // Uid sanity check (10c-e-3b-fix): the TUI is the source of
    // truth for uid identity. We sanity-check the format here so a
    // malformed uid fails fast rather than landing in the registry
    // as a corruption hazard. This is NOT a security boundary —
    // the daemon socket is local + 0600 and any caller could
    // forge a uid; the check is "did the producer match the
    // documented format" so a typo / off-by-one bug surfaces
    // loudly. Format: `ts-<hex>-<hex>` (see
    // `tui/src/app.rs::new_session_uid`).
    if !is_valid_session_type(&p.session_type) {
        return Err((
            ErrorCode::InvalidParams,
            format!(
                "session_type must be one of \"claude-code\", \"codex\", \"bash\"; got '{}'",
                p.session_type
            ),
        ));
    }

    if !is_valid_session_uid(&p.uid) {
        return Err((
            ErrorCode::InvalidParams,
            format!(
                "uid must match TUI format ts-<hex>-<hex>, got '{}'",
                p.uid
            ),
        ));
    }

    // Sub-2b-3 review-4 #1: reject partial cap tuples at the
    // entry point. The cap triple `(memory_cap_bytes,
    // memory_cap_hard_bytes, cgroup_prefix)` is either all
    // three (capped session) or all three None (uncapped). A
    // partial tuple means the producer (TUI) sent an
    // inconsistent payload — accepting it would store an
    // incomplete cap on `DaemonSession`, and the downstream
    // inheritance path in `mcp_start_session` would silently
    // fall through to "no cap" because it requires the full
    // triple to wrap argv. That's a cap-bypass via wire-shape
    // confusion. Fail closed here so the invariant
    // `cap_complete_iff_capped` holds for every session in
    // `state.sessions`, which lets the inheritance branch
    // trust its `(Some, Some, Some)` match and surface partial
    // tuples as bugs rather than fall through.
    let cap_soft = p.memory_cap_bytes.is_some();
    let cap_hard = p.memory_cap_hard_bytes.is_some();
    let cap_prefix = p.cgroup_prefix.is_some();
    if (cap_soft || cap_hard || cap_prefix) && !(cap_soft && cap_hard && cap_prefix) {
        return Err((
            ErrorCode::InvalidParams,
            format!(
                "memory_cap fields are all-or-nothing: \
                 memory_cap_bytes={}, memory_cap_hard_bytes={}, cgroup_prefix={}. \
                 Send all three (capped) or none (uncapped) — partial tuples \
                 would silently degrade to an uncapped child via mcp_start_session.",
                cap_soft, cap_hard, cap_prefix,
            ),
        ));
    }

    // Lock just for workspace lookup (with optional auto-register),
    // then drop. We must NOT hold the state lock across
    // `DaemonSession::spawn` (slow — opens PTY, forks child) or
    // across the insert (which acquires the lock itself) — and
    // crucially, we mustn't hold it when the reaper-cleanup callback
    // we set below would try to lock.
    //
    // Auto-register branch (slice 10c-e-3): the daemon snapshots
    // workspaces once at startup; mid-session A-n workspaces won't
    // be present until slice 10e's `manifest.watch` ships. When the
    // caller passes `worktree_path` alongside an unknown
    // `workspace_id`, register a minimal workspace entry on the fly.
    // No-op for callers that don't send the field (preserves the
    // "NotFound on unknown id" behavior). Once 10e lands, this
    // becomes redundant but stays harmless.
    {
        let mut state = state_arc.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.workspaces.contains_key(&p.workspace_id) {
            match p.worktree_path.as_deref() {
                Some(path_str) => {
                    let path = PathBuf::from(path_str);
                    state.workspaces.insert(
                        p.workspace_id.clone(),
                        crate::manifest::ManifestWorkspace {
                            id: p.workspace_id.clone(),
                            worktree_path: Some(path),
                            ..Default::default()
                        },
                    );
                }
                None => {
                    return Err((
                        ErrorCode::NotFound,
                        format!(
                            "workspace '{}' not in daemon's manifest snapshot \
                             (pre-10e: pass worktree_path to auto-register)",
                            p.workspace_id
                        ),
                    ));
                }
            }
        }
    }

    // Use the TUI-supplied uid verbatim (10c-e-3b-fix). The TUI's
    // MCP config already references this uid; minting a new one
    // here would silently desync every downstream consumer.
    let session_uid = p.uid.clone();

    // Slice 10c-e-3b-fix6: the collision check moved to the
    // Phase 2 lock-held insert below. Pre-fix6 the check
    // happened here, then the lock was dropped, then spawn
    // happened, then unconditional insert — two concurrent
    // `start_session` calls with the same uid could both pass
    // the guard, both spawn, both insert (last writer wins,
    // first child orphans). Re-checking under the same lock
    // that does the insert closes the TOCTOU window; if the
    // race is lost, `PendingSession`'s Drop SIGKILLs the
    // child via pidfd and we return Conflict cleanly. Same
    // recovery shape as the fix4a unverified-cgroup arm.

    let working_dir = PathBuf::from(&p.working_dir);

    // Build SpawnParams from the caller-supplied argv directly.
    // `argv[0]` is the program; `argv[1..]` is the args. The TUI's
    // `ClientSession::new` builds these via the same code path
    // `Session::new` uses locally (`mcp_config::build_args` +
    // `wrap_with_systemd_run`), so element-wise parity holds.
    let program = p.argv[0].clone();
    let mut spawn_params = SpawnParams::new(&session_uid, &p.label, &program);
    // Slice 10d-mcp-surface-1: workspace_id + session_type +
    // managed_by_uid + task_id are recorded on the DaemonSession
    // so `list_sessions` can surface them on the wire (the
    // Python MCP tool's contract). Session-caller auth (sub-2)
    // will use workspace_id and task_id for the descendant-task
    // check.
    spawn_params.workspace_id = p.workspace_id.clone();
    spawn_params.session_type = p.session_type.clone();
    spawn_params.managed_by_uid = p.managed_by_uid.clone();
    spawn_params.task_id = p.task_id.clone();
    // Sub-2b-1: thread transcript_path so
    // `resolve_authorized_session` has something to return for
    // sessions where the TUI knew the path at spawn time
    // (clone/resume seed flows). `None` is the common case for
    // fresh spawns and yields `state: "pending"` until a
    // post-detection update RPC lands.
    spawn_params.transcript_path = p.transcript_path.clone();
    // 10d-2c-1 review round-5 (F1): plumb workflow context onto
    // the DaemonSession so `lookup_session_any` returns it for
    // the auth check in `workflow_transition` / `workflow_done`.
    // `None` for non-workflow spawns (A-n / A-s without an
    // active workflow); the after-the-fact tagging path
    // (workflow launched on an already-spawned daemon session)
    // uses `session.set_workflow_context`.
    spawn_params.workflow_run_id = p.workflow_run_id.clone();
    spawn_params.workflow_role = p.workflow_role.clone();
    // Phase-1 continuous wire field: carry the continuous-task tag
    // onto the DaemonSession. `None` for ordinary spawns.
    spawn_params.continuous_task_id = p.continuous_task_id.clone();
    // Global-perms grant. This RPC is operator-only (the TUI), so
    // the value is trusted here — the human (or a TUI flow acting
    // for the human) is the authority on who gets global perms. The
    // agent-facing escalation guard lives in `mcp_start_session`.
    spawn_params.global_perms = p.global_perms;
    spawn_params.args = p.argv[1..].to_vec();
    spawn_params.working_dir = Some(working_dir);
    spawn_params.cols = p.cols;
    spawn_params.rows = p.rows;
    // Adopt the caller's env wholesale FIRST. The TUI
    // provides the MCP-server routing pin (`CM_TUI_SOCKET=""` /
    // `CM_DAEMON_SOCKET=<abs path>` per slice 10c-e-3a's
    // SpawnTarget::Daemon) plus `CM_TUI_SESSION_ID` and any
    // workflow vars.
    for (k, v) in p.env {
        spawn_params.env.insert(k, v);
    }

    // Slice 12f: daemon-side env injection. The daemon's
    // config (loaded from daemon.toml at startup) is the
    // authoritative source for environment that the TUI
    // can't safely supply for a remote daemon — local TUI
    // paths don't exist on the cm-manager VM / Mac mini.
    // These insertions OVERRIDE anything the TUI sent so
    // remote-host correctness wins by default.
    //
    // Spec (daemon/NOTES.md 12f):
    //   - CM_TUI_SOCKET   = daemon's listen socket (path of
    //                       the cm-daemon's own .sock; the
    //                       agent's MCP client dials this
    //                       for TUI-routed methods that the
    //                       daemon serves on the same socket
    //                       in remote-host mode).
    //   - CM_DAEMON_SOCKET = same as CM_TUI_SOCKET for
    //                        daemon-routed methods.
    //   - CM_MCP_SERVER  = absolute path to `mcp_server/server.py`
    //                      on the DAEMON's host (resolved from
    //                      `daemon.toml::mcp_server_path`; empty
    //                      means "let the resolver fall back").
    //   - CM_API_URL / CM_API_TOKEN = planning API ingress.
    //                                 From daemon.toml so the
    //                                 daemon can reach the
    //                                 planning server from the
    //                                 daemon's host rather than
    //                                 borrowing the TUI's local
    //                                 binding.
    //   - CM_WORKFLOW_RUN_ID / CM_ROLE = workflow context for
    //                                    participant spawns; from
    //                                    the RPC params, not env
    //                                    or config.
    //   - CM_TUI_SESSION_ID = daemon-minted uid (the
    //                         correlation key for
    //                         `~/.cm/memory_kills/<uid>.jsonl`;
    //                         daemon owns the uid identity for
    //                         daemon-spawned sessions).
    // 12f F1: absolutize before injection. `default_socket_path()`
    // returns whatever `CM_DAEMON_SOCKET` was set to verbatim
    // (including a relative path like `daemon.sock`). Injecting
    // a relative path into the child's env makes the agent
    // dial relative to ITS own worktree cwd — wrong location,
    // NotFound at dial time. `absolutize_socket_path` joins
    // the daemon's cwd-at-startup when the path is relative;
    // absolute paths pass through unchanged.
    let daemon_socket_abs = crate::path::absolutize_socket_path(
        &crate::default_socket_path(),
    )
    .map_err(|e| (
        ErrorCode::Internal,
        format!("absolutize daemon socket path for env injection: {}", e),
    ))?;
    let daemon_socket_str =
        daemon_socket_abs.to_string_lossy().into_owned();
    spawn_params
        .env
        .insert("CM_TUI_SOCKET".into(), daemon_socket_str.clone());
    spawn_params
        .env
        .insert("CM_DAEMON_SOCKET".into(), daemon_socket_str);
    {
        let st = state_arc.lock().expect("state mutex");
        if !st.config.mcp_server_path.is_empty() {
            spawn_params
                .env
                .insert("CM_MCP_SERVER".into(), st.config.mcp_server_path.clone());
        }
        if !st.config.api_url.is_empty() {
            spawn_params
                .env
                .insert("CM_API_URL".into(), st.config.api_url.clone());
        }
        if !st.config.api_token.is_empty() {
            spawn_params
                .env
                .insert("CM_API_TOKEN".into(), st.config.api_token.clone());
        }
    }
    if let Some(run_id) = p.workflow_run_id.as_deref() {
        spawn_params
            .env
            .insert("CM_WORKFLOW_RUN_ID".into(), run_id.to_string());
    }
    if let Some(role) = p.workflow_role.as_deref() {
        spawn_params
            .env
            .insert("CM_ROLE".into(), role.to_string());
    }
    spawn_params
        .env
        .insert("CM_TUI_SESSION_ID".into(), session_uid.clone());

    // Memory-cap plumbing (slice 10c-e-3b-fix2). When the TUI
    // indicates a cap was applied (via `memory_cap_bytes`), the
    // daemon populates `kills_dir` so the reaper's baseline +
    // probe path runs (`session.rs`'s `LastExitProbe::snapshot`
    // scans this directory at End-frame time). The actual
    // cgroup-OOM watcher hasn't relocated yet — that's slice
    // 10d-memory-cap-relocation — so no kill records will land
    // for daemon sessions; `memory_cap_kill` stays `false` until
    // the relocation. The plumbing is in place; the producer
    // moves over next.
    if p.memory_cap_bytes.is_some() {
        spawn_params.kills_dir = crate::path::default_kills_dir();
    }
    // Sub-2b-3 review-fix #1: thread cap fields onto the
    // spawned session so descendant MCP spawns can inherit.
    spawn_params.memory_cap_soft_bytes = p.memory_cap_bytes;
    spawn_params.memory_cap_hard_bytes = p.memory_cap_hard_bytes;
    spawn_params.cgroup_prefix = p.cgroup_prefix.as_deref().map(PathBuf::from);

    // Stash a copy of kills_dir for the slice-10d watcher-spawn
    // arm below: `spawn_params` is moved into
    // `PendingSession::spawn` on the next line, so we can't
    // reach back into it after spawn returns. The watcher needs
    // the same directory the daemon's reaper-side classification
    // path reads from — taking the value here ensures they agree.
    let kills_dir_for_watcher: Option<std::path::PathBuf> =
        spawn_params.kills_dir.clone();

    // === Phase 1: spawn child, no reaper yet. ===
    //
    // Done outside the state lock — spawn_command forks + execs
    // which is slow, and we don't want to serialize other RPCs
    // against it. `PendingSession` owns the live child via its
    // Box<dyn Child> handle; if anything below fails, dropping
    // the PendingSession SIGKILLs + waitpid'd the child cleanly.
    let pending = PendingSession::spawn(spawn_params)
        .map_err(|e| (ErrorCode::Internal, format!("spawn: {}", e)))?;

    // Slice 10d watcher-fix #1: cgroup discovery from
    // `/proc/<pid>/cgroup` — never trust a caller-supplied path.
    //
    // Pre-fix the daemon trusted `params.cgroup_path` from the
    // caller and the watcher signalled PIDs based on it. A buggy
    // or malicious caller could pre-populate an existing cgroup
    // with PIDs from unrelated processes (their shell, another
    // worker, etc.), send that path in `start_session`, and have
    // the daemon SIGKILL those processes on the first memory
    // breach. Daemon's cgroup verification only checked the path
    // had procs — not that those procs were *ours*.
    //
    // Post-fix the daemon DISCOVERS the cgroup path by reading
    // `/proc/<spawn-pid>/cgroup` after Phase 1's child spawn. The
    // kernel writes the cgroup of the live PID; systemd-run will
    // have moved the child into a `cm-sess-*.scope` cgroup if the
    // scope setup completed, and the discovery helper verifies
    // the basename matches that pattern. If the pattern doesn't
    // match (systemd-run failed mid-setup, scope ended up
    // elsewhere, caller bypassed the wrapper), we bail with
    // `Internal` — `pending`'s Drop pidfd-SIGKILLs the child.
    //
    // The discovered path is bound to *this specific spawn* by
    // the kernel — caller's hostile path simply cannot reach the
    // watcher's signalling code anymore. The slice's `cgroup_path`
    // wire field is gone (see the module-level comment above
    // `StartSessionParams`).
    //
    // Local-spawn parity in `tui/src/session.rs::Session::new`
    // does the same discovery; both sides obey "never trust a
    // path that wasn't read from /proc".
    let verified_cgroup_path: Option<String> = if p.memory_cap_bytes.is_some() {
        let pid = pending.pid() as u32;
        let discovered = match crate::path::discover_session_cgroup_path(
            pid,
            std::time::Duration::from_millis(500),
        ) {
            Ok(p) => p,
            Err(e) => {
                // pending.Drop pidfd-SIGKILLs the child.
                return Err((
                    ErrorCode::Internal,
                    format!(
                        "cgroup discovery from /proc/{}/cgroup failed: {} \
                         (refusing to return a session whose memory cap \
                         hasn't been verified against this child's actual \
                         cgroup)",
                        pid, e
                    ),
                ));
            }
        };
        // Belt-and-suspenders: also verify the cgroup is active
        // (cgroup.procs non-empty). discover already required a
        // valid pattern + path existence; the active check
        // confirms the kernel has actually moved the child into
        // this cgroup (not just that the directory was created).
        // Matches the local-spawn `wait_for_cgroup_active`
        // semantics for parity.
        if !crate::path::wait_for_cgroup_active(&discovered, std::time::Duration::from_millis(500)) {
            return Err((
                ErrorCode::Internal,
                format!(
                    "discovered cgroup {} has no procs within 500ms \
                     (systemd-run scope is half-created — likely a \
                     transient systemd error)",
                    discovered.display()
                ),
            ));
        }
        Some(discovered.to_string_lossy().into_owned())
    } else {
        // No memory cap requested → no cgroup discovery needed.
        // The caller is responsible for passing `memory_cap_bytes`
        // when they want a cap.
        None
    };

    // === Slice 10d-memory-cap-relocation: spawn the daemon-side
    // cgroup-OOM watcher BEFORE the lock-held Phase 2. ===
    //
    // Pre-10d-fix the watcher spawn happened AFTER the registry
    // insert and used `.expect()` on the thread spawn. A failure
    // (resource exhaustion, RLIMIT_NPROC hit) would panic-unwind
    // the RPC handler *with the session already inserted*: client
    // gets a broken connection, capped session runs with no
    // kill-log producer, memory_cap_kill stays `false` forever —
    // silent half-broken state. Same bug class as the prior
    // fix2 / fix3 / fix5 / fix6 silent-degrade fixes.
    //
    // Post-fix ordering:
    //   1. cgroup verified above ✓
    //   2. NOW: spawn watcher. On `Err`, drop `pending` (Drop
    //      SIGKILLs child via pidfd, waitpids zombie), return
    //      `Internal`. No registry residue.
    //   3. lock state, recheck uid collision, arm_reaper,
    //      insert (with the watcher's `JoinHandle` stashed on
    //      `DaemonSession.watcher_handle` so a future bounded
    //      join could go there). On collision, drop pending +
    //      detach watcher; watcher self-exits via cgroup-vanish
    //      after pending's Drop SIGKILLs the child.
    //
    // Conditions for spawning the watcher:
    //   1. A cap was requested AND verified (`verified_cgroup_path`
    //      is `Some` — fix4a already verified the cgroup is active).
    //   2. `memory_cap_bytes` is set (soft cap; without it the
    //      watcher has no breach threshold).
    //   3. `kills_dir` is set (populated above when cap was
    //      requested, via `default_kills_dir()`).
    let watcher_handle: Option<std::thread::JoinHandle<()>> = match (
        verified_cgroup_path.as_ref(),
        p.memory_cap_bytes,
        kills_dir_for_watcher,
    ) {
        (Some(cgroup_path), Some(soft_cap_bytes), Some(kills_dir)) => {
            // Sub-2b-3 review-10: pass the real
            // `memory_cap_hard_bytes` to the watcher. Pre-fix
            // this passed literal `0` even when the wire
            // carried the real cap (review-4 #1 made
            // `memory_cap_hard_bytes` mandatory alongside
            // `memory_cap_bytes`), so the daemon-spawned
            // path emitted JSONL records with
            // `"hard_cap_bytes": 0` — diverging from the
            // TUI-local watcher which emits the configured
            // value. The named acceptance criterion is
            // "indistinguishable from today's signal 9
            // toast"; divergent diagnostics break that.
            //
            // Routed through `resolve_watcher_hard_cap_bytes`
            // so the resolver is testable in isolation
            // (the watcher-startup code path requires a real
            // cgroup + systemd-run wrapper to reach in tests).
            let hard_cap_bytes: u64 =
                resolve_watcher_hard_cap_bytes(p.memory_cap_hard_bytes);
            // Slice 10d watcher-fix #6: seed the watcher's
            // breach baseline from the `memory.events.high`
            // counter NOW — before `spawn_watcher` returns.
            // Pre-fix #6 the watcher captured `last_high` at
            // first iteration of run_watcher, which gave the
            // ~1s window between cgroup discovery and watcher
            // thread start (pidfd_open + arm_reaper + insert)
            // a chance to silently absorb early breaches.
            // Reading here closes that window down to the
            // microsecond kernel-level "child spawned but not
            // yet in cgroup" sub-window (inherent, inactionable).
            let initial_high = crate::session_watch::read_memory_events_high(
                std::path::Path::new(cgroup_path),
            );
            match crate::session_watch::spawn_watcher(
                session_uid.clone(),
                std::path::PathBuf::from(cgroup_path),
                soft_cap_bytes,
                hard_cap_bytes,
                kills_dir,
                initial_high,
                watcher_spawn_fn,
            ) {
                Ok(h) => Some(h),
                Err(e) => {
                    // No state lock held, no registry insert
                    // yet. Drop `pending` so its Drop pidfd-
                    // SIGKILLs the still-alive child. Surface
                    // the error to the client so they don't
                    // believe a capped session is running.
                    drop(pending);
                    return Err((
                        ErrorCode::Internal,
                        format!(
                            "spawn cgroup-OOM watcher thread failed: {} \
                             (refusing to return a session whose memory \
                             cap has no producer)",
                            e
                        ),
                    ));
                }
            }
        }
        _ => None,
    };

    // Reaper-cleanup callback: remove the session from the
    // registry when the child exits. Fix #1 from the slice-10c-c
    // review-2: a fast-exit child cannot strand a dead entry,
    // because the lock-held arm_reaper-and-insert sequence below
    // forces the reaper's `on_exit` to either (a) run after our
    // insert (callback blocks on the lock we hold), or (b) run
    // never until our insert is visible (reaper thread hasn't
    // been spawned yet at that point).
    //
    // 10e-a addition (BEFORE the existing `state.sessions.remove`):
    // capture the session's `workspace_id` + `LastExitProbe`, build
    // the typed `LastExit` via the probe's helper, mutate the
    // matching `ManifestEntry.last_exit` in
    // `state.workspaces[ws_id].sessions` if the entry is present
    // (it is when the TUI's load_manifest_from_disk has fed us a
    // snapshot containing this uid; absent when the daemon spawned
    // a session into a workspace the manifest hasn't tracked yet —
    // the diff still fires, just with no on-disk landing zone), and
    // broadcast `ManifestDiff::Exited` to live `manifest.watch`
    // subscribers. All three steps run under the same lock as the
    // existing remove so concurrent kills serialize and subscribers
    // see exit diffs in lifecycle-event order (per the §5 R1
    // mitigation in the 10e plan).
    let state_for_cleanup = Arc::clone(state_arc);
    let uid_for_cleanup = session_uid.clone();
    let on_exit: Box<dyn FnOnce(&DaemonExitStatus) + Send + 'static> =
        Box::new(move |_status: &DaemonExitStatus| {
            let mut s = state_for_cleanup
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            handle_session_exit(&mut s, &uid_for_cleanup);
        });

    // === Phase 2: arm reaper + insert, atomic under the lock. ===
    //
    // The lock is acquired BEFORE arm_reaper (which spawns the
    // reaper thread) and held through the insert. Possible
    // schedules:
    //
    //   - Child still alive when arm_reaper returns: reaper's
    //     wait_for_child blocks; on_exit fires asynchronously
    //     after the child eventually exits and we've long since
    //     unlocked. Cleanup runs correctly.
    //
    //   - Child already exited (fast-exit, e.g. `/bin/false`):
    //     reaper's wait_for_child returns immediately, on_exit
    //     fires, tries to lock state — blocks because WE hold
    //     the lock. We finish insert, unlock. on_exit's lock
    //     acquires, finds the entry we just inserted, removes
    //     it. Clean.
    //
    // The critical property: arm_reaper must NOT be called
    // before the lock is held. (Calling it before would let the
    // reaper fire before insert.)
    let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());

    // Slice 10c-e-3b-fix6: lock-held uid collision check.
    // We must re-check the registry under the SAME lock that
    // does the insert; the pre-spawn check is racy because two
    // concurrent calls with the same uid can both pass it
    // (window between lock-drop and lock-reacquire-for-insert).
    // If the race is lost, drop the `PendingSession` — its Drop
    // SIGKILLs the child via pidfd — and return Conflict
    // cleanly. The losing thread doesn't strand a child.
    //
    // Slice 10d watcher-fix addendum: also drop the
    // `watcher_handle` here so the spawned watcher thread can
    // self-terminate via cgroup-vanish after `pending`'s Drop
    // SIGKILLs the child. Detach is correct (not join) — the
    // watcher's poll loop checks `cgroup_path.exists()` each
    // iteration, exiting promptly once systemd cleans up the
    // scope.
    if state.sessions.contains_key(&session_uid) {
        drop(state); // release before dropping `pending` (Drop is fast but explicit)
        // `pending` drops at end-of-scope; its Drop kills the
        // child cleanly via pidfd. Explicit drop here for
        // intent — the kill happens whether we name it or not,
        // but naming it makes the recovery shape obvious to
        // readers.
        drop(pending);
        drop(watcher_handle);
        return Err((
            ErrorCode::Conflict,
            format!(
                "uid '{}' already exists in daemon registry — refusing to clobber \
                 a live session (race with concurrent start_session for same uid)",
                session_uid
            ),
        ));
    }

    let mut session = pending
        .arm_reaper(Some(on_exit))
        .map_err(|e| (ErrorCode::Internal, format!("arm reaper: {}", e)))?;
    // Slice 10d watcher-fix: stash the watcher handle on the
    // registry-resident session at the same instant we insert
    // it. The drop-on-detach contract documented on the field
    // means session-removal paths don't need explicit cleanup.
    session.watcher_handle = watcher_handle;
    state.sessions.insert(session_uid.clone(), session);
    // P0 session durability (S1): persist the registry now that the
    // new session is live, so a daemon restart can restore it. Covers
    // every production spawn — `mcp_start_session` and
    // `create_session` both funnel through here. Best-effort + small;
    // the transcript_id fills in later via the detector's own persist
    // hook once the engine writes its JSONL.
    state.persist_sessions_best_effort();
    // Option B (criterion #4): broadcast a manifest `Added` so live
    // `manifest.watch` subscribers can render the new session — symmetric with
    // the `Exited` broadcast in `handle_session_exit`. The TUI's consumer adopts
    // workflow-participant rows from this (daemon-launched participants have no
    // locally-created TerminalSession otherwise). The `entry` carries exactly
    // what the TUI needs to build + group a row: uid, workspace, label,
    // session_type, and the workflow tags. Clone the watcher Arc out so the
    // broadcast runs after the state lock drops (matches `Exited`'s shape).
    let added_watcher = Arc::clone(&state.manifest_watcher);
    let added_entry = json!({
        "uid": session_uid,
        "workspace_id": p.workspace_id,
        "label": p.label,
        "session_type": p.session_type,
        "workflow_run_id": p.workflow_run_id,
        "workflow_role": p.workflow_role,
        "continuous_task_id": p.continuous_task_id,
        "task_id": p.task_id,
    });
    drop(state);
    added_watcher.broadcast(crate::manifest::ManifestDiff::Added {
        uid: session_uid.clone(),
        entry: added_entry,
    });

    // Echo VERIFIED cgroup_path back to the TUI (slice
    // 10c-e-3b-fix4a). The path comes from the
    // post-spawn verification arm above — if the scope didn't
    // materialize as active, we already returned Internal and
    // never reach this point. The TUI can therefore treat this
    // field as authoritative: present ⇒ cap is in place;
    // absent ⇒ no cap requested.
    let mut response = json!({ "session_uid": session_uid });
    if let Some(cg) = verified_cgroup_path {
        response["cgroup_path"] = Value::String(cg);
    }
    Ok(response)
}

/// 10e-a: handle a session exit by populating the matching
/// `ManifestEntry.last_exit` in the daemon's manifest snapshot and
/// broadcasting `ManifestDiff::Exited` to live `manifest.watch`
/// subscribers. Called from `start_session`'s reaper-cleanup
/// callback while the state lock is held. Removes the session from
/// `state.sessions` as the final step.
///
/// The mutation order is load-bearing for the 10e plan's §5 race
/// surfaces:
///   - R1 (concurrent kills): all three sub-steps (LastExit build,
///     manifest mutation, broadcast) run under the same lock that
///     guards `state.sessions.remove`, so subscribers see exit
///     diffs in lifecycle-event order — the lock IS the ordering
///     point.
///   - R5 (untracked-uid diff): when `state.workspaces[ws_id]`
///     doesn't have a `ManifestEntry` for this uid (the daemon
///     spawned a session into a workspace the TUI's manifest never
///     tracked), the entry mutation no-ops but the diff still
///     fires. Subscribers that don't know the uid will themselves
///     no-op — see the TUI consumer in 10e-c.
///
/// `exited_at` is captured at call time via `now_unix_f64()`. The
/// reaper has already populated `LastExitProbe.kernel` before
/// invoking the on_exit closure (see
/// `session.rs::arm_reaper`), so `build_last_exit` reads a
/// populated kernel slot.
pub(crate) fn handle_session_exit(state: &mut DaemonState, uid: &str) {
    let exited_at = now_unix_f64();
    // Capture a read-after-exit tombstone BEFORE the session is removed from
    // the registry, so `resolve_authorized_session` / `list_sessions` can still
    // serve its transcript + final state for a window (the MCP read-after-exit
    // contract). Resolve the worktree from the workspace (the session itself
    // doesn't carry it). Built here, recorded after the existing last_exit /
    // broadcast steps so it lands under the same lock as the remove.
    let tombstone = state.sessions.get(uid).map(|sess| {
        let worktree_path = state
            .workspaces
            .get(&sess.workspace_id)
            .and_then(|w| w.worktree_path.as_ref())
            .map(|p| p.to_string_lossy().into_owned());
        crate::state::ExitedTombstone {
            session_uid: uid.to_string(),
            transcript_path: sess.transcript_path.clone(),
            generation: sess.generation,
            session_type: sess.session_type.clone(),
            workspace_id: sess.workspace_id.clone(),
            task_id: sess.task_id.clone(),
            managed_by_uid: sess.managed_by_uid.clone(),
            label: sess.title.clone(),
            workflow_run_id: sess.workflow_run_id.clone(),
            workflow_role: sess.workflow_role.clone(),
            worktree_path,
            global_perms: sess.global_perms,
            exited_at,
        }
    });
    // Continuous Tasks Phase 3b completion signal (b): capture this session's
    // continuous-task tag BEFORE the registry remove (after which `get(uid)`
    // sees None) so a CLEAN exit of a continuous-task session can clear its
    // ACTIVE run below.
    let continuous_task_id = state
        .sessions
        .get(uid)
        .and_then(|s| s.continuous_task_id.clone());
    let (workspace_id_opt, last_exit_opt) = match state.sessions.get(uid) {
        Some(sess) => {
            let le = sess.last_exit.build_last_exit(exited_at);
            (Some(sess.workspace_id.clone()), Some(le))
        }
        None => (None, None),
    };
    if let (Some(ws_id), Some(last_exit)) =
        (workspace_id_opt.as_ref(), last_exit_opt.as_ref())
    {
        if let Some(mw) = state.workspaces.get_mut(ws_id) {
            if let Some(entry) = mw.sessions.iter_mut().find(|e| e.uid == uid) {
                entry.last_exit = Some(last_exit.clone());
            }
        }
    }
    if let Some(last_exit) = last_exit_opt {
        let watcher = Arc::clone(&state.manifest_watcher);
        watcher.broadcast(crate::manifest::ManifestDiff::Exited {
            uid: uid.to_string(),
            last_exit,
        });
    }
    if let Some(tomb) = tombstone {
        state.record_exited(tomb);
    }
    // Continuous Tasks Phase 3b completion signal (b): a continuous-task session
    // exiting CLEANLY clears its ACTIVE run (DESIGN_CONTINUOUS_TASKS.md §11). The
    // DOUBLE guard (session_uid match + status==Running) keeps every kill path
    // benign: operator kill → Done (fine); watchdog escalate sets Stuck FIRST so
    // the kill's exit sees status!=Running → Stuck preserved; resolve_stuck
    // restart re-fires a NEW last_run (new uid) so the killed uid mismatches →
    // no-op; the investigator's own exit (its uid != last_run.session_uid) is a
    // no-op too. `task::modify` is per-task flock disk IO with ZERO DaemonState
    // re-entrancy, so it is safe under this reaper-held lock. Best-effort: a
    // failed persist only leaves the run Running (the watchdog/orphan reconciler
    // is the backstop), so log nothing louder than the swallow.
    if let Some(ct_id) = continuous_task_id {
        let _ = crate::continuous::task::modify(&ct_id, |t| {
            if let Some(run) = t.last_run.as_mut() {
                if run.session_uid.as_deref() == Some(uid)
                    && matches!(run.status, crate::continuous::task::RunStatus::Running)
                {
                    run.status = crate::continuous::task::RunStatus::Done;
                    run.finished_at = Some(crate::continuous::task::now_unix());
                }
            }
        });
    }
    state.sessions.remove(uid);
    // P0 session durability (S1): persist the registry now that the
    // session is gone, so the durable file converges to live state and
    // a restart won't try to restore an already-exited session within
    // the staleness window. Best-effort; runs under the reaper/kill
    // lock that guards the remove above.
    state.persist_sessions_best_effort();
}

/// Sanity-check for a TUI-supplied session uid. Slice 10c-e-3b-fix:
/// the daemon no longer mints its own uid; the TUI is the source of
/// truth (see `StartSessionParams::uid`). We validate here so a
/// malformed uid fails fast with `InvalidParams` rather than landing
/// in the registry as a corruption hazard. Pattern:
/// `ts-<hex>-<hex>` — what `tui/src/app.rs::new_session_uid`
/// produces. Not a security check (the daemon socket is local +
/// 0600 and any caller could forge a uid); a typo / off-by-one in
/// the TUI's generator would just surface as InvalidParams instead
/// of weird downstream desync.
fn is_valid_session_uid(uid: &str) -> bool {
    let rest = match uid.strip_prefix("ts-") {
        Some(r) => r,
        None => return false,
    };
    let mut parts = rest.split('-');
    let (a, b, extra) = (parts.next(), parts.next(), parts.next());
    if extra.is_some() {
        return false;
    }
    match (a, b) {
        (Some(a), Some(b))
            if !a.is_empty()
                && !b.is_empty()
                && a.chars().all(|c| c.is_ascii_hexdigit())
                && b.chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            true
        }
        _ => false,
    }
}

/// Slice 10d-mcp-surface-1 fix #1: enforce the canonical
/// `session_type` vocabulary the Python MCP tool's caller code
/// dispatches on. Pre-fix, an unknown value (typo, future drift,
/// caller bug) would have landed on `DaemonSession.session_type`
/// and propagated to `list_sessions`'s `type` field — MCP
/// consumers downstream would either misroute or fail to match.
/// Rejecting at the wire boundary keeps the registry's
/// session_type domain closed.
///
/// The three canonical values are the same set
/// `tui/src/app.rs::try_spawn_via_daemon` maps to from the TUI's
/// internal vocabulary (`"claude"` → `"claude-code"` happens
/// caller-side).
fn is_valid_session_type(session_type: &str) -> bool {
    matches!(session_type, "claude-code" | "codex" | "bash")
}

/// Sub-2a Finding #2: shared helper to convert
/// `crate::control::auth::AuthDecision` into the typed
/// `MethodResult` error shape used by send_input / kill_session /
/// read_session_output. Auth runs INSIDE the same critical
/// section as the target lookup (closes the TOCTOU window the
/// pre-fix dispatcher's separate-lock pattern had); these
/// methods short-circuit on the decision before extracting any
/// per-session handle.
/// Map an `AuthDecision` to a wire `MethodResult`.
///
/// migrate-tui-local: pre-migrate this consulted
/// `state.tui_sessions` to surface a distinct "TUI-owned, can't
/// be proxied" Conflict for callers targeting a TUI-spawned
/// session. The migration moves every spawn site to the daemon
/// (workflow respawn, manifest restore, A-l resume, etc.), so
/// no production session is "TUI-owned" anymore — the branch is
/// unreachable. The `state` parameter is kept (Option) to
/// preserve the call shape; callers may pass `None` if they
/// don't have state at hand.
fn return_auth_error_if_denied_with_state(
    decision: crate::control::auth::AuthDecision,
    caller_uid: &str,
    target_uid: &str,
    state: Option<&DaemonState>,
) -> MethodResult {
    use crate::control::auth::AuthDecision;
    let _ = state;
    match decision {
        AuthDecision::Allow => Ok(Value::Null),
        AuthDecision::CallerNotInRegistry => Err((
            ErrorCode::Unauthorized,
            format!(
                "Session caller '{}' is not in the daemon registry",
                caller_uid
            ),
        )),
        AuthDecision::TargetNotInRegistry => Err((
            ErrorCode::NotFound,
            format!("target session '{}' not in the daemon registry", target_uid),
        )),
        AuthDecision::OutOfScope => Err((
            ErrorCode::Unauthorized,
            format!(
                "Session caller '{}' is not authorized for target '{}' \
                 (outside caller's task subtree / workspace per TUI-mirror rule)",
                caller_uid, target_uid
            ),
        )),
        AuthDecision::TaskTreeNotYetSynced => Err((
            ErrorCode::Conflict,
            format!(
                "Session caller '{}' targeting '{}' cannot be authorized yet — \
                 the TUI's task-tree snapshot hasn't reached the daemon. \
                 This is the startup-window race; retry the RPC after a brief \
                 delay (the TUI pushes the snapshot during its own startup).",
                caller_uid, target_uid
            ),
        )),
    }
}

// ============================================================
// send_input (slice 10c-d)
// ============================================================

/// `send_input` request params. Mirrors the shape
/// `tui/src/control/methods.rs::SendInputParams` uses today.
#[derive(Deserialize)]
struct SendInputParams {
    /// Target session uid in `DaemonState.sessions`.
    session_uid: String,
    /// Bytes to write to the session's PTY input. Capped at
    /// [`MAX_SEND_INPUT_BYTES`].
    text: String,
    /// Append `\n` after writing the text. Defaults to true
    /// (matching the TUI handler's default — most callers want
    /// the agent to receive the prompt as a submission).
    #[serde(default = "default_submit")]
    submit: bool,
}

fn default_submit() -> bool {
    true
}

/// Write `params.text` to the target session's PTY.
///
/// ## At this slice (10c-d)
///
/// Operator-callable only (matches `start_session`'s disposition
/// — Session-caller descendant-task-tree validation lands in
/// 10c-e). Caller-scope authorization is enforced at the
/// dispatcher arm; this body assumes the caller is authorized.
///
/// The TUI's equivalent calls `agent::submit_prompt` which knows
/// about engine-specific submission formatting (pending_clear,
/// pending_jsonl_files tracking). The daemon's version writes raw
/// bytes + optional `\n`. That gap is intentional for the slice's
/// working-set discipline: agent-aware submission stays TUI-side
/// until the agent module relocates (planned alongside
/// `resolve_authorized_session` in a later slice).
pub fn send_input(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
    caller_uid: Option<&str>,
) -> MethodResult {
    let p: SendInputParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("send_input params: {}", e)))?;

    if p.text.len() > MAX_SEND_INPUT_BYTES {
        return Err((
            ErrorCode::InvalidParams,
            format!(
                "text field {} bytes exceeds cap of {} bytes",
                p.text.len(),
                MAX_SEND_INPUT_BYTES
            ),
        ));
    }
    if !p.submit {
        // Phase-1 parity with the TUI: only submit=true is
        // wired. A typing path (no Enter) would need different
        // semantics around buffering / pending_clear and isn't
        // needed by any current MCP caller.
        return Err((
            ErrorCode::InvalidParams,
            "submit=false is not yet supported (matches TUI parity)".into(),
        ));
    }

    // Build the write payload BEFORE the state lock — text +
    // optional newline is just byte ops, no contention.
    let mut payload = p.text.into_bytes();
    if p.submit {
        payload.push(b'\n');
    }

    // Sub-2a Finding #2 TOCTOU fix: auth + Arc clone happen in
    // ONE critical section so the target session can't be
    // removed (or replaced with an entry the caller wouldn't
    // be authorized for) between authorize-time and act-time.
    // Pre-fix the dispatcher locked for auth, dropped the lock,
    // and this method re-locked — leaving a window.
    // Sub-2b-1 review-r#3 #2: extract an `InputHandle` under
    // the state lock, drop the state lock, then write+stamp
    // through the centralized helper. Pre-fix the write +
    // post-write stamp were inlined here and the stream input
    // path didn't stamp at all — the handle pattern keeps the
    // invariant in one place so future input paths can't skip
    // it.
    let handle = {
        let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        // Session-caller auth: under the same lock that does
        // the target lookup. Operator callers (`caller_uid:
        // None`) bypass.
        if let Some(cuid) = caller_uid {
            let decision = crate::control::auth::check_session_caller(
                &state,
                cuid,
                &p.session_uid,
            );
            return_auth_error_if_denied_with_state(decision, cuid, &p.session_uid, Some(&state))?;
        }
        let session = state.sessions.get_mut(&p.session_uid).ok_or_else(|| {
            (
                ErrorCode::NotFound,
                format!("session '{}' not in daemon registry", p.session_uid),
            )
        })?;
        // Liveness gate: a session whose child has exited has a
        // dead PTY. Surface Conflict for TUI parity. `try_wait`
        // drains the reaper's mpsc channel (side-effectful on
        // `cached_exit`), so we keep `get_mut` here for that.
        if session.try_wait().is_some() {
            return Err((
                ErrorCode::Conflict,
                format!(
                    "session '{}' has exited; PTY no longer writable",
                    p.session_uid
                ),
            ));
        }
        session.input_handle()
    };
    // State lock is dropped — write + stamp happen on the
    // handle's cloned Arcs.
    handle.write_and_stamp(&payload).map_err(|e| {
        (
            ErrorCode::Internal,
            format!("send_input write to PTY: {}", e),
        )
    })?;

    Ok(json!({ "ok": true }))
}

// ============================================================
// session.resize — reliable, out-of-band PTY resize
// ============================================================

#[derive(Deserialize)]
struct SessionResizeParams {
    session_uid: String,
    cols: u16,
    rows: u16,
}

/// Resize a daemon-owned session's PTY to `cols`×`rows`.
///
/// This is the **reliable** size-delivery path. The attach data
/// stream already carries `{"resize": {cols, rows}}` frames, but
/// those are best-effort: when the attach socket is dead/replaced
/// at the instant the frame fires the write drops (`Broken pipe`)
/// and the PTY stays at its old size forever — the "session renders
/// skinny" bug. `attach.open` closed that gap for the *initial*
/// attach (it resizes in-process under the bind lock); this method
/// closes it for *every other* resize. The TUI's adopt scan calls
/// it to re-assert the pane size on any session whose daemon PTY
/// drifted (detected via the `cols`/`rows` `list_sessions` now
/// reports). Because every control RPC dials a fresh socket, this
/// call can't ride a dead attach stream.
///
/// Operator-only (enforced at the dispatch arm) — agents have no
/// resize use case. Resizing to the current size is a harmless
/// idempotent ioctl, so the TUI only calls this on detected drift.
pub fn session_resize(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    let p: SessionResizeParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("session.resize params: {}", e)))?;
    let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    let session = state.sessions.get_mut(&p.session_uid).ok_or_else(|| {
        (
            ErrorCode::NotFound,
            format!("session '{}' not in daemon registry", p.session_uid),
        )
    })?;
    // `resize` stamps `last_cols`/`last_rows` even if the underlying
    // TIOCSWINSZ errors (rare; only when the master fd is gone after
    // child exit). Surface that as Internal so the caller can log;
    // the TUI treats it as best-effort and moves on.
    session.resize(p.cols, p.rows).map_err(|e| {
        (
            ErrorCode::Internal,
            format!("PTY resize for '{}': {}", p.session_uid, e),
        )
    })?;
    Ok(json!({ "ok": true }))
}

// ============================================================
// kill_session (slice 10c-d)
// ============================================================

#[derive(Deserialize)]
struct KillSessionParams {
    session_uid: String,
}

/// Terminate a session by removing it from the registry. The
/// `Drop` impl on the moved-out `DaemonSession` sends `SIGKILL`
/// via pidfd (the PID-reuse-safe primitive from slice-10c-b);
/// the reaper thread observes the exit asynchronously and the
/// reaper-cleanup callback's `remove` becomes a no-op (we just
/// removed). No deadlock: the callback's lock acquire blocks
/// briefly on our held mutex, then runs after we unlock.
///
/// ## At this slice (10c-d)
///
/// Operator-callable only. Manifest-write + tombstone-recording
/// the TUI does today are deferred to slice 10e (daemon-side
/// manifest ownership flip); for now `kill_session` simply
/// removes from the in-memory registry, which is sufficient for
/// `session.attach` against a killed UID to return `NotFound`.
pub fn kill_session(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
    caller_uid: Option<&str>,
) -> MethodResult {
    let p: KillSessionParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("kill_session params: {}", e)))?;

    let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    // Sub-2a Finding #2 TOCTOU fix: auth + lookup happen in one
    // critical section so a non-descendant target can't slip in
    // (or out) between authorize-time and signal-time.
    if let Some(cuid) = caller_uid {
        let decision = crate::control::auth::check_session_caller(
            &state,
            cuid,
            &p.session_uid,
        );
        return_auth_error_if_denied_with_state(decision, cuid, &p.session_uid, Some(&state))?;
    }
    // 10e-a r1 F2: do NOT remove the session from the registry
    // here. Pre-r1 we removed-then-Drop'd, which fired SIGKILL
    // via the dropped session's pidfd — but the reaper's on_exit
    // callback then ran against an already-absent uid, so
    // `handle_session_exit` couldn't find the session, couldn't
    // populate `last_exit`, and couldn't broadcast
    // `ManifestDiff::Exited`. Operator-killed sessions emitted no
    // manifest exit diff — the UI saw nothing for A-w.
    //
    // Post-r1: keep the session in the registry, mark the
    // operator-kill flag on the probe, and send SIGKILL via the
    // session's pidfd directly. The reaper's `wait_for_child`
    // sees the exit, fires on_exit, on_exit acquires the state
    // lock (blocking on us until we return from this handler),
    // and `handle_session_exit` finds the session, builds
    // `LastExit`, mutates the manifest entry, broadcasts the
    // diff, and finally removes from `state.sessions`. Now
    // operator-kill and cap-kill share the same exit-diff path —
    // the UI sees a `ManifestDiff::Exited` frame for both.
    let session = match state.sessions.get_mut(&p.session_uid) {
        Some(s) => s,
        None => {
            return Err((
                ErrorCode::NotFound,
                format!("session '{}' not in daemon registry", p.session_uid),
            ));
        }
    };
    // Slice 10d watcher-fix #4: mark the operator-kill flag on
    // the session's `LastExitProbe` BEFORE the SIGKILL goes
    // out. The probe's flag is read at End-frame snapshot time
    // and joined with `kill_status` + signal in `is_cap_kill`
    // so a transient `protected`/`no_pids` record past baseline
    // doesn't render as a cap kill on a user-initiated A-w.
    session.last_exit.mark_operator_kill_requested();
    // SIGKILL via pidfd. Errors are best-effort: ESRCH means the
    // child already exited (race with the watcher / natural
    // exit) — reaper will still see the exit and fire on_exit.
    let _ = session.kill();

    Ok(json!({ "ok": true }))
}

// ============================================================
// read_session_output (slice 10c-d)
// ============================================================
//
// This is a fanout-snapshot method — distinct from the Python MCP
// tool of the same name in `mcp_server/server.py`, which composes
// `resolve_authorized_session` + a Python file-read. The daemon's
// method is the "give me what's currently buffered" surface; the
// Python tool is the "give me parsed transcript messages from disk"
// surface. Both can coexist.
//
// Wire shape returns a base64-encoded byte buffer + cursor /
// eviction flag matching `crate::session::FanoutSnapshot`.

#[derive(Deserialize)]
struct ReadSessionOutputParams {
    session_uid: String,
    /// Caller's cursor from a previous call. `None` on the first
    /// call returns the full current ring.
    #[serde(default)]
    since_cursor: Option<u64>,
}

/// Return a snapshot of the session's PTY-output fanout. See
/// [`crate::session::PtyByteFanout::snapshot_since`] for the
/// cursor semantics.
///
/// ## At this slice (10c-d)
///
/// Operator-callable only. Session-caller descendant-task-tree
/// validation lands in 10c-e (same gate as send_input /
/// kill_session). The response shape matches `FanoutSnapshot`
/// (bytes are base64-encoded for transport).
pub fn read_session_output(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
    caller_uid: Option<&str>,
) -> MethodResult {
    let p: ReadSessionOutputParams = serde_json::from_value(params.clone())
        .map_err(|e| {
            (
                ErrorCode::InvalidParams,
                format!("read_session_output params: {}", e),
            )
        })?;

    // Sub-2a Finding #2 TOCTOU fix: auth + fanout-Arc clone
    // happen in one critical section, then we drop the state
    // lock and call `snapshot_since` on the fanout's own
    // synchronization. Pre-fix the dispatcher locked for auth,
    // dropped, and this method re-locked — TOCTOU window open
    // to a swap-out of the target between authorize-time and
    // snapshot-time.
    let fanout = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(cuid) = caller_uid {
            let decision = crate::control::auth::check_session_caller(
                &state,
                cuid,
                &p.session_uid,
            );
            return_auth_error_if_denied_with_state(decision, cuid, &p.session_uid, Some(&state))?;
        }
        let session = state.sessions.get(&p.session_uid).ok_or_else(|| {
            (
                ErrorCode::NotFound,
                format!("session '{}' not in daemon registry", p.session_uid),
            )
        })?;
        Arc::clone(&session.fanout)
    };
    let snap = fanout.snapshot_since(p.since_cursor);
    Ok(json!({
        "bytes": BASE64.encode(&snap.bytes),
        "start_offset": snap.start_offset,
        "cursor": snap.cursor,
        "evicted_since_cursor": snap.evicted_since_cursor,
        "closed": snap.closed,
    }))
}

// ============================================================
// list_sessions (slice 10d-mcp-surface-1: scaffolding only)
// ============================================================
//
// Returns a snapshot of the daemon's live session registry. For
// sub-1: Operator-only (Session-caller dispatch deferred to
// sub-2 alongside task-subtree auth).
//
// **Wire shape — Python MCP tool contract** (`mcp_server/server.py:319`):
// the response is a TOP-LEVEL JSON ARRAY (not wrapped in
// `{sessions: [...]}`), and each entry carries the fields the
// Python tool's caller code reads at `mcp_server/server.py:660`:
//
//   `[{ session_uid, label, type, state, idle, managed_by_uid }, …]`
//
// Sub-1 review caught the previous `{sessions: [{uid, ...}]}`
// shape as a contract break (Finding #2). Aligning now so the
// Operator-caller path (and sub-2's Session-caller flip) just
// works without a wire-shape churn.
//
// Field semantics (sub-1):
//
//   - `session_uid`: `DaemonSession.uid`.
//   - `label`: `DaemonSession.title`. (TUI calls it "label";
//      daemon calls it "title". The wire uses "label" for
//      Python MCP tool parity.)
//   - `type`: `DaemonSession.session_type` (`"claude-code"` /
//      `"codex"` / `"bash"` etc.).
//   - `state`: `"running"`/`"ready"`/`"pending"` for live
//      sessions (via `compute_session_state_and_idle`), or
//      `"exited"` for recently-exited tombstones when the caller
//      passes `include_exited=true` (read-after-exit). The daemon
//      now retains a bounded `recently_exited` tombstone ring
//      (`DaemonState::recently_exited`); `include_exited` surfaces
//      those rows instead of being a no-op.
//   - `idle`: always `false` at sub-1. Daemon doesn't track PTY
//      idleness yet; future slice can plumb the idle-detection
//      output through the fanout.
//   - `managed_by_uid`: `DaemonSession.managed_by_uid`.
//
// `caller_workspace: Option<&str>` parameter stays for sub-2's
// Session-caller scoping. Sub-1's `dispatch_list_sessions`
// always passes `None` (Operator sees all).

/// Wire params for `list_sessions`. The Python MCP tool sends
/// `{include_exited: bool, task_id?: string}`. Sub-1 accepts both
/// fields for forward-compat with the tool signature but
/// honors them as documented above (no-ops for now).
#[derive(Deserialize, Default)]
struct ListSessionsParams {
    /// Future-proofs the wire signature for slice 10e
    /// (manifest ownership flip). Sub-1 accepts but ignores.
    #[serde(default)]
    include_exited: bool,
    /// Future-proofs the wire signature for slice 10d-mcp-
    /// surface-2 (task-subtree auth). Sub-1 accepts but ignores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    /// migrate-tui-local Issue A: when `true`, the response
    /// omits rows backed only by `state.tui_sessions` (TUI-pushed
    /// snapshot metadata). Only sessions actually live in
    /// `state.sessions` (daemon-owned PTYs) appear in the array.
    ///
    /// Used by the TUI's manifest-restore probe so a stale
    /// snapshot row left over from a previous TUI process doesn't
    /// trick `spawn_restored_session` into the attach branch (the
    /// attach RPC would then fail because there's no live PTY
    /// behind the snapshot row, and the restored entry would be
    /// silently dropped).
    ///
    /// Default `false` preserves the Python MCP tool's contract
    /// (it expects to see TUI-owned sibling sessions in the
    /// listing).
    #[serde(default)]
    daemon_owned_only: bool,
}

pub fn list_sessions(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
    caller_uid: Option<&str>,
) -> MethodResult {
    // `null` is treated as default (no params) — the Python
    // MCP tool calls control_client.call("list_sessions", {…})
    // with the params object, but synthetic / Operator callers
    // may send `Null`.
    let p: ListSessionsParams = if params.is_null() {
        ListSessionsParams::default()
    } else {
        serde_json::from_value(params.clone())
            .map_err(|e| (ErrorCode::InvalidParams, format!("list_sessions params: {}", e)))?
    };
    let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());

    // Global-permissions grant: a global caller sees EVERY session
    // (across all tasks and workspaces), not just its task-tree
    // slice. Computed once here and consulted in the scope-auth
    // gate, the default-scope resolution, and the per-session
    // include filter below.
    let caller_is_global = caller_uid
        .and_then(|cuid| state.sessions.get(cuid))
        .map(|s| s.global_perms)
        .unwrap_or(false);

    // Sub-2a Finding #3: authorize the requested task scope
    // BEFORE iterating. Mirrors
    // `tui/src/control/methods.rs:498-521`:
    //   - Operator caller: no restriction.
    //   - Global Session caller: no restriction (any task scope ok).
    //   - Taskless Session caller + explicit task_id: Unauthorized.
    //   - Tasked Session caller + explicit task_id: must be
    //     self-or-descendant of caller's task.
    if let Some(req_task) = p.task_id.as_deref() {
        if let Some(cuid) = caller_uid {
            if caller_is_global {
                // Global caller may scope to any task — skip the
                // descendant/taskless gates entirely.
            } else {
            let caller = state.sessions.get(cuid).ok_or_else(|| {
                (
                    ErrorCode::Unauthorized,
                    format!("caller session '{}' not in daemon registry", cuid),
                )
            })?;
            match caller.task_id.as_deref() {
                None => {
                    return Err((
                        ErrorCode::Unauthorized,
                        format!(
                            "taskless caller cannot scope list_sessions to task '{}'",
                            req_task
                        ),
                    ));
                }
                Some(own_task) => {
                    if !crate::control::auth::task_is_self_or_descendant_of(
                        &state.task_tree,
                        req_task,
                        own_task,
                    ) {
                        return Err((
                            ErrorCode::Unauthorized,
                            format!(
                                "task '{}' is not the caller's task or a descendant",
                                req_task
                            ),
                        ));
                    }
                }
            }
            }
        }
    }

    // Effective scope task: explicit param if present, else
    // caller's own task (for Session callers). Operator callers
    // with no param have `scope_task = None` and see all
    // sessions; Operator callers WITH `task_id` filter to that
    // subtree. Global Session callers behave like Operators here —
    // no implicit own-task scope, so they default to seeing every
    // session.
    let scope_task: Option<String> = p.task_id.clone().or_else(|| {
        if caller_is_global {
            return None;
        }
        caller_uid
            .and_then(|cuid| state.sessions.get(cuid))
            .and_then(|s| s.task_id.clone())
    });

    let mut sessions: Vec<Value> =
        Vec::with_capacity(state.sessions.len() + state.tui_sessions.len());

    // Helper: should the given (uid, task_id) be included given
    // the current scope_task / caller_uid context? Used by both
    // the daemon-owned and TUI-owned loops below.
    let should_include = |uid: &str, task_id: Option<&str>| -> bool {
        // Global caller: see everything. Honor an explicit task
        // scope if one was passed, otherwise include unconditionally.
        // (Routing a global caller through `check_session_caller`
        // below would wrongly exclude TUI-owned targets that aren't
        // in `state.sessions` — TargetNotInRegistry.)
        if caller_is_global {
            return match scope_task.as_deref() {
                Some(scope) => task_id
                    .map(|t| {
                        crate::control::auth::task_is_self_or_descendant_of(
                            &state.task_tree,
                            t,
                            scope,
                        )
                    })
                    .unwrap_or(false),
                None => true,
            };
        }
        match (scope_task.as_deref(), caller_uid) {
            // Explicit scope (param OR caller's task): include
            // only sessions whose task_id is self-or-descendant
            // of the scope. Mirrors TUI's `Some(scope) =>` arm.
            (Some(scope), _) => task_id
                .map(|t| {
                    crate::control::auth::task_is_self_or_descendant_of(
                        &state.task_tree,
                        t,
                        scope,
                    )
                })
                .unwrap_or(false),
            // No scope, Session caller: defer to the per-session
            // auth check (taskless caller → same-workspace).
            (None, Some(cuid)) => {
                crate::control::auth::check_session_caller(&state, cuid, uid)
                    .is_allow()
            }
            // No scope, Operator caller: every session.
            (None, None) => true,
        }
    };

    // Daemon-owned sessions (live PTY in `state.sessions`).
    for (uid, session) in state.sessions.iter() {
        if !should_include(uid, session.task_id.as_deref()) {
            continue;
        }
        // Sub-2b-1 review-r#3 #1: single helper computes
        // `(state, idle)` for both `list_sessions` and
        // `resolve_authorized_session` so the Python MCP tool's
        // `wait_for_session_idle` (which polls list_sessions) and
        // `read_session_output` (which resolves via
        // resolve_authorized_session) agree.
        let (state_str, idle) = compute_session_state_and_idle(session);
        sessions.push(json!({
            "session_uid": uid,
            "label": session.title,
            "type": session.session_type,
            "state": state_str,
            "idle": idle,
            "managed_by_uid": session.managed_by_uid,
            // Global-permissions grant — surfaced so an agent (and
            // the grouped MCP view) can tell which sessions are
            // privileged orchestrators.
            "global_perms": session.global_perms,
            // Adoption metadata (Part 1): lets the TUI surface
            // agent-spawned sessions in the sidebar, grouped under their
            // task/workspace. All already live on `DaemonSession`;
            // `worktree_path` is joined from the daemon manifest so the
            // TUI can build/restore a workspace for the adopted session.
            // Additive — older TUI consumers ignore unknown fields, and a
            // newer TUI treats them as Optional.
            "workspace_id": session.workspace_id,
            "task_id": session.task_id,
            "workflow_run_id": session.workflow_run_id,
            "workflow_role": session.workflow_role,
            "continuous_task_id": session.continuous_task_id,
            "worktree_path": state
                .workspaces
                .get(&session.workspace_id)
                .and_then(|w| w.worktree_path.as_ref())
                .map(|p| p.display().to_string()),
            // Live PTY window size (authoritative — `last_cols`/`last_rows`
            // track every applied resize and equal the kernel winsize). The
            // TUI's adopt scan compares these against its pane size to detect
            // sessions whose daemon PTY drifted (e.g. an MCP-spawned codex
            // that started skinny, or a resize data-frame that dropped on a
            // dead attach socket) and re-asserts the size via `session.resize`.
            // Additive — older TUIs ignore unknown fields.
            "cols": session.last_cols,
            "rows": session.last_rows,
        }));
    }
    // TUI-owned sessions (post-Phase-1 unified view, fixes review
    // finding #2). Daemon-spawned agents need to see sibling
    // sessions that the TUI launched locally. We surface them with
    // best-available metadata from the snapshot:
    //   - `state`: "ready" — the TUI's snapshot only contains
    //     live entries (TUI clears its push on session exit), so
    //     "ready" is accurate at snapshot time. `pending` /
    //     `exited` reach consumers via `manifest.watch` diffs.
    //   - `idle`: false — the daemon doesn't track TUI sessions'
    //     activity timestamps. Conservative default (the Python
    //     MCP tool's `wait_for_session_idle` will block instead of
    //     returning a stale `true`).
    //   - `managed_by_uid`: null — TUI snapshot doesn't carry
    //     parent-session correlation.
    // Daemon-owned takes precedence: if a uid is in BOTH maps
    // (shouldn't happen, but TUI's push filter is best-effort),
    // skip the TUI entry rather than emit duplicates.
    //
    // migrate-tui-local Issue A: when the caller passes
    // `daemon_owned_only: true`, skip this loop entirely. The
    // manifest-restore probe sets it so a stale tui_sessions row
    // (e.g. from a previous TUI process) can't trick the restore
    // into attaching to a UID with no live PTY behind it.
    if p.daemon_owned_only {
        // Stable sort + return — same shape as the no-skip path.
        sessions.sort_by(|a, b| {
            a["session_uid"].as_str().unwrap_or("").cmp(b["session_uid"].as_str().unwrap_or(""))
        });
        return Ok(Value::Array(sessions));
    }
    for (uid, snap) in state.tui_sessions.iter() {
        if state.sessions.contains_key(uid) {
            continue;
        }
        if !should_include(uid, snap.task_id.as_deref()) {
            continue;
        }
        sessions.push(json!({
            "session_uid": uid,
            "label": snap.label.clone().unwrap_or_default(),
            "type": snap.session_type.clone().unwrap_or_else(|| "claude-code".into()),
            "state": "ready",
            "idle": false,
            "managed_by_uid": Value::Null,
            // Task / workflow context from the TUI snapshot so the
            // grouped MCP view can place TUI-owned sessions under
            // their task. `workspace_id` isn't carried in the
            // snapshot (TUI-owned), so it's null here.
            "task_id": snap.task_id,
            "workspace_id": Value::Null,
            "workflow_run_id": snap.workflow_run_id,
            "workflow_role": snap.workflow_role,
            "global_perms": snap.global_perms,
        }));
    }
    // Recently-exited sessions, included only when the caller opts in
    // (`include_exited=true`) — the read-after-exit surface: a caller that lost
    // or killed a session can still find it here as state="exited", and
    // resolve_authorized_session serves its transcript. Skip a uid that was
    // re-spawned (live entry wins) or is already a TUI snapshot row. Scope-
    // filtered like live rows, but via the exited-target auth (the tombstone is
    // gone from state.sessions, so the live check_session_caller would deny it).
    if p.include_exited {
        for tomb in state.recently_exited.iter() {
            let uid = tomb.session_uid.as_str();
            if state.sessions.contains_key(uid) || state.tui_sessions.contains_key(uid) {
                continue;
            }
            let include = match (scope_task.as_deref(), caller_uid) {
                (Some(scope), _) => tomb
                    .task_id
                    .as_deref()
                    .map(|t| {
                        crate::control::auth::task_is_self_or_descendant_of(
                            &state.task_tree,
                            t,
                            scope,
                        )
                    })
                    .unwrap_or(false),
                (None, Some(cuid)) => crate::control::auth::check_session_caller_for_exited(
                    &state,
                    cuid,
                    uid,
                    tomb.task_id.as_deref(),
                    &tomb.workspace_id,
                )
                .is_allow(),
                (None, None) => true,
            };
            if !include {
                continue;
            }
            sessions.push(json!({
                "session_uid": uid,
                "label": tomb.label,
                "type": tomb.session_type,
                "state": "exited",
                "idle": true,
                "managed_by_uid": tomb.managed_by_uid,
                "workspace_id": tomb.workspace_id,
                "task_id": tomb.task_id,
                "workflow_run_id": tomb.workflow_run_id,
                "workflow_role": tomb.workflow_role,
                "worktree_path": tomb.worktree_path,
            }));
        }
    }
    // Stable order for deterministic test assertions and for
    // human-debuggable output.
    sessions.sort_by(|a, b| {
        a["session_uid"].as_str().unwrap_or("").cmp(b["session_uid"].as_str().unwrap_or(""))
    });
    // Top-level array, NOT `{sessions: [...]}` — Python MCP tool
    // contract (`mcp_server/server.py:660` iterates the response
    // as a list).
    Ok(Value::Array(sessions))
}

// ============================================================
// resolve_authorized_session (sub-2b-1)
// ============================================================
//
// The Python MCP `read_session_output` tool (`mcp_server/server.py:400`)
// is a TWO-step pattern:
//   1. Call `resolve_authorized_session` with the target session_uid.
//      Daemon returns `{state, engine, transcript_path, generation, idle}`
//      after the same Session-caller descendant-task-tree auth walk
//      sub-2a wired for `send_input` / `kill_session` /
//      `read_session_output` / `list_sessions`.
//   2. With the resolved `transcript_path`, the Python tool reads
//      the transcript file directly via its own parsers
//      (`mcp_server/transcripts/{claude.py,codex.py}`). No further
//      daemon round-trip for the actual message bytes.
//
// Pre-sub-2b-1 only the TUI served `resolve_authorized_session`. A
// daemon-spawned agent hitting `read_session_output` via the
// daemon socket got `UnknownMethod` on step 1 — even though step 2's
// fanout-snapshot `read_session_output` (sub-2a) is implemented on
// the daemon. This wires step 1 daemon-side so the Python tool's
// two-step works end-to-end against the daemon socket.
//
// ## What the daemon does NOT do
//
// Engine-specific path conventions (Claude's
// `~/.claude/projects/<encoded>/*.jsonl` vs Codex's
// `~/.codex/sessions/YYYY/MM/DD/<id>.jsonl`) stay TUI-side. The TUI
// already has the agent module (`tui/src/agent/`) for path
// resolution and the post-spawn detector for the moment the file
// appears on disk. The TUI sends the resolved `transcript_path` to
// the daemon at spawn time when it knows the value (e.g. clone /
// resume seed flows where the id is supplied upfront). A
// post-detection update RPC (`session.set_transcript_path`) for
// the fresh-spawn case is deferred to a follow-up; until then the
// daemon's stored value stays `None` for fresh spawns and the
// resolver returns `state: "pending"` (the Python tool then
// short-circuits to empty messages + poll-again behavior, matching
// the TUI's `RuntimeStateKind::Pending` arm).
//
// ## Response shape
//
// `{state, engine, transcript_path, generation, idle}` — same keys
// the TUI returns, same wire vocabulary. The Python tool reads
// every key with `.get(..., default)` (see `server.py:433-437`), so
// fields the daemon can't compute yet (`generation`, `idle`) carry
// safe defaults (0, false). `engine` is derived from
// `DaemonSession.session_type` via the same `engine_str`
// transformation the TUI uses.

#[derive(Deserialize)]
struct ResolveAuthorizedSessionParams {
    session_uid: String,
}

/// Resolve a session's transcript metadata for the Python MCP
/// tool's two-step read pattern. See module-level doc above for
/// the why; this is just the wire shape implementation.
pub fn resolve_authorized_session(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
    caller_uid: Option<&str>,
) -> MethodResult {
    let p: ResolveAuthorizedSessionParams = serde_json::from_value(params.clone())
        .map_err(|e| {
            (
                ErrorCode::InvalidParams,
                format!("resolve_authorized_session params: {}", e),
            )
        })?;

    // Sub-2a Finding #2 TOCTOU shape: auth + read-target happen
    // in one critical section. Target lookup happens AFTER auth
    // passes so a deny decision doesn't leak metadata via timing
    // on the existence check.
    let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(cuid) = caller_uid {
        // Auth against the LIVE session if present, else its read-after-exit
        // TOMBSTONE (same descendant-task/workspace scope rule applies to a dead
        // target), else the live path which yields TargetNotInRegistry for a
        // genuinely-unknown uid.
        let decision = if state.sessions.contains_key(&p.session_uid) {
            crate::control::auth::check_session_caller(&state, cuid, &p.session_uid)
        } else if let Some(tomb) = state.exited_tombstone(&p.session_uid) {
            crate::control::auth::check_session_caller_for_exited(
                &state,
                cuid,
                &p.session_uid,
                tomb.task_id.as_deref(),
                &tomb.workspace_id,
            )
        } else {
            crate::control::auth::check_session_caller(&state, cuid, &p.session_uid)
        };
        return_auth_error_if_denied_with_state(decision, cuid, &p.session_uid, Some(&state))?;
    }
    // Auth passed (or Operator caller). Resolve the target: live session first,
    // then a recently-exited tombstone (read-after-exit), else NotFound.
    if let Some(session) = state.sessions.get(&p.session_uid) {
        let (state_str, idle) = compute_session_state_and_idle(session);
        // Sub-2b-1 review-r#2 #2: surface the generation counter
        // so the Python `read_session_output` tool's cursor
        // (`v1:<generation>:<offset>`) resets when the underlying
        // transcript file rotates (e.g. `/clear`, codex resume).
        Ok(json!({
            "state": state_str,
            "engine": engine_str(&session.session_type),
            "transcript_path": session.transcript_path.clone(),
            "generation": session.generation,
            "idle": idle,
        }))
    } else if let Some(tomb) = state.exited_tombstone(&p.session_uid) {
        // Read-after-exit: the session left the registry but its transcript
        // file is still on disk. Report state="exited" + the final transcript
        // path/generation so `read_session_output` serves the last output. An
        // exited session has no live PTY, so it is trivially idle.
        Ok(json!({
            "state": "exited",
            "engine": engine_str(&tomb.session_type),
            "transcript_path": tomb.transcript_path.clone(),
            "generation": tomb.generation,
            "idle": true,
        }))
    } else {
        Err((
            ErrorCode::NotFound,
            format!("session '{}' not in daemon registry", p.session_uid),
        ))
    }
}

/// Sub-2b-1 (review #2): PTY-quiet threshold for daemon-side
/// idle computation. Mirrors the TUI's
/// `DEFAULT_IDLE_TIMEOUT_SECS = 2` in `tui/src/app.rs:225` so
/// both surfaces agree on the "how long without output is
/// idle?" answer.
const IDLE_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(2);

/// Sub-2b-1 review-r#3 #1: single source of truth for the
/// `(state, idle)` pair both `resolve_authorized_session` and
/// `list_sessions` report. Pre-fix `list_sessions` hardcoded
/// `state: "ready"` and `idle: false`, so the same daemon
/// would give two different answers for the same session
/// depending on which method the caller used. The Python MCP
/// tool's `wait_for_session_idle` polls `list_sessions` while
/// `read_session_output` calls `resolve_authorized_session` —
/// the divergence broke wait-then-read flows.
///
/// State derivation (matches the TUI's `RuntimeStateKind`):
///   - `transcript_path: Some(_)` → `"ready"`.
///   - `transcript_path: None` → `"pending"` (the TUI's
///     "spawned but no transcript bound yet" gap; Python tool
///     short-circuits to empty messages + poll-again on
///     this).
///
/// This helper handles only LIVE sessions (transcript-bound →
/// `ready`, else `pending`); it never returns `exited`, because the
/// daemon removes a session from `state.sessions` on exit (sub-2a's
/// kill_session + reaper-cleanup callback). The `exited` state for
/// read-after-exit is served separately by `resolve_authorized_session`
/// / `list_sessions` from the `DaemonState::recently_exited` tombstone
/// ring, which `handle_session_exit` records right before the remove.
///
/// Idle derivation: `last_activity_at.elapsed() >=
/// IDLE_THRESHOLD`. Production sessions stamp spawn-time at
/// construction (sub-2b-1 review-r#4 #1), so the `None` arm
/// here is unreachable for live sessions — kept as a defensive
/// fallback that yields `idle: true` for the test-only
/// `PtyByteFanout::new` shortcut (which builds a fanout
/// without an Arc into a parent `DaemonSession`).
///
/// Spawn-time init means a fresh session reports `idle: false`
/// for `IDLE_THRESHOLD` after creation — mirroring TUI's
/// `SessionStatus::Running` default, which only flips to
/// `Idle` on the next event-drain tick if `wakeup_times`
/// has stayed empty.
///
/// Intentionally SLIGHTLY more permissive than TUI's
/// wire-level idle (which ANDs in `pending_prompt` /
/// `pending_clear` checks — TUI-side state the daemon can't
/// see). The window where they diverge is narrow (TUI's
/// `deliver_pending_write` fires on quiet, so pending input
/// is consumed within milliseconds of becoming deliverable);
/// agents that race see at most one premature unblock,
/// self-correcting on the next poll.
pub(super) fn compute_session_state_and_idle(
    session: &crate::session::DaemonSession,
) -> (&'static str, bool) {
    let state_str = if session.transcript_path.is_some() {
        "ready"
    } else {
        "pending"
    };
    let idle = {
        let slot = session
            .last_activity_at
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        match *slot {
            None => true,
            Some(t) => t.elapsed() >= IDLE_THRESHOLD,
        }
    };
    (state_str, idle)
}

// ============================================================
// session.set_transcript_path (sub-2b-1 review #1)
// ============================================================
//
// Claude/Codex transcript files are born AFTER process spawn —
// the path isn't known at `start_session` time for fresh
// sessions. The TUI's existing post-spawn detector (the
// `pending_jsonl_files` → `transcript_id` flow in
// `tui/src/app.rs::drain_terminal_events`) discovers the path
// seconds after spawn by polling the
// `~/.claude/projects/<encoded>/` (Claude) /
// `~/.codex/sessions/YYYY/MM/DD/` (Codex) directories. When the
// TUI binds a fresh `transcript_id`, this RPC fires to push the
// resolved path to the daemon so its
// `resolve_authorized_session` transitions from `pending` to
// `ready`.
//
// **Auth: Operator-only.** The TUI is the authoritative source
// for transcript paths (only the TUI runs the detector that
// resolves engine-specific naming conventions to a concrete
// filesystem path). A Session-caller setting this could lie
// about which file the resolver returns to the Python tool —
// security-equivalent to a Session-caller editing the task tree
// (also Operator-only).
//
// **Idempotent**: re-pushing the same path is a no-op semantically
// (the daemon stores the latest value). The TUI is expected to
// guard against repeat pushes via its own "needs push" flag to
// keep the RPC traffic bounded, but the daemon doesn't reject
// re-pushes — a re-push after `/clear`-driven rebind WOULD
// legitimately update.

#[derive(Deserialize)]
struct SetTranscriptPathParams {
    session_uid: String,
    /// New value for `DaemonSession.transcript_path`. The TUI
    /// sends a non-empty string when its detector resolves a
    /// path; an explicit empty string is rejected as
    /// InvalidParams (callers should not push to clear — let
    /// the session naturally exit instead).
    transcript_path: String,
}

pub fn set_transcript_path(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    let p: SetTranscriptPathParams = serde_json::from_value(params.clone())
        .map_err(|e| {
            (
                ErrorCode::InvalidParams,
                format!("session.set_transcript_path params: {}", e),
            )
        })?;
    if p.transcript_path.is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "transcript_path must be non-empty".into(),
        ));
    }
    let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    let session = state.sessions.get_mut(&p.session_uid).ok_or_else(|| {
        (
            ErrorCode::NotFound,
            format!("session '{}' not in daemon registry", p.session_uid),
        )
    })?;
    // Sub-2b-1 review-r#2 #2: bump generation iff the path
    // actually changed. Same-path re-pushes are idempotent —
    // the TUI's detector might re-push the same path multiple
    // times across polls; bumping every push would invalidate
    // the agent's cursor needlessly. Only true rotations
    // (`/clear`, `/compact`, codex resume that re-detects a new
    // file) bump.
    let changed = session.transcript_path.as_deref() != Some(p.transcript_path.as_str());
    if changed {
        session.generation = session.generation.saturating_add(1);
    }
    session.transcript_path = Some(p.transcript_path);
    let generation = session.generation;
    // P0 session durability (S1): a freshly-resolved transcript is the
    // resume key — persist so a restart can `--resume` this session's
    // history. The read of `generation` above ends the `&mut session`
    // borrow, freeing `state` for the immutable persist call.
    if changed {
        state.persist_sessions_best_effort();
    }
    Ok(json!({
        "ok": true,
        "generation": generation,
    }))
}

// ============================================================
// session.set_workflow_context (10d-2c-1 review round-5 F1)
// ============================================================
//
// After-the-fact tagging: when the TUI launches a workflow on an
// already-spawned daemon-attached session (the Existing-slot path
// in the former TUI controller's launch), it pushes the
// (workflow_run_id, workflow_role) pair to the daemon so the
// `DaemonSession` mirrors what the TUI's `TerminalSession` now
// carries. Without this RPC, `lookup_session_any` would return
// `(None, None)` for daemon-owned workflow participants — round-3's
// `tui_sessions` filter excluded them from the fallback map —
// and the auth check in `workflow_transition` / `workflow_done`
// would reject them. That's the bug the round-5 F1 fix closes
// for the existing-session-becomes-participant case (the typical
// worker shape).
//
// **Caller**: Operator only. Session callers must not be able to
// re-assign their own workflow context (would let an agent
// declare itself part of an arbitrary workflow run and forge
// transitions).
//
// **Idempotent**: pushing the same (run_id, role) is a no-op;
// pushing `(None, None)` clears workflow context (e.g. workflow
// stopped on the session). Daemon doesn't validate that the
// run_id exists in `workflow_runs` — that's the TUI's
// responsibility (the TUI only calls this after a successful
// `launch_workflow`).
//
// Wire shape:
//   { uid: <session_uid>,
//     workflow_run_id: <string | null>,
//     workflow_role: <string | null> }
//
// Response:
//   { ok: true, daemon_owned: bool }
//
// `daemon_owned: false` is a sentinel for TUI callers that
// invoke this on a session the daemon doesn't own (round-3
// snapshot filter would have removed it from `tui_sessions` too);
// no-op success rather than NotFound so the TUI can fire
// this for every workflow participant without branching.

#[derive(Deserialize)]
struct SetWorkflowContextParams {
    /// Session uid to tag. Must be a daemon-owned session
    /// (present in `state.sessions`); see `daemon_owned` in the
    /// response for the no-op case.
    uid: String,
    /// New workflow_run_id. `None` clears the field.
    #[serde(default)]
    workflow_run_id: Option<String>,
    /// New workflow_role. `None` clears the field.
    #[serde(default)]
    workflow_role: Option<String>,
}

pub fn set_workflow_context(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    let p: SetWorkflowContextParams = serde_json::from_value(params.clone())
        .map_err(|e| {
            (
                ErrorCode::InvalidParams,
                format!("session.set_workflow_context params: {}", e),
            )
        })?;
    if p.uid.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "session.set_workflow_context: 'uid' is required".into(),
        ));
    }
    // Defense-in-depth: if exactly one of (run_id, role) is
    // present, that's almost certainly a caller bug — workflow
    // context is meaningful only as a pair. Refuse instead of
    // silently storing a half-tagged session that would later
    // surface as a confusing auth-decline.
    let half_tagged = p.workflow_run_id.is_some() ^ p.workflow_role.is_some();
    if half_tagged {
        return Err((
            ErrorCode::InvalidParams,
            "session.set_workflow_context: workflow_run_id and \
             workflow_role must both be set or both be null"
                .into(),
        ));
    }
    let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    let session = match state.sessions.get_mut(&p.uid) {
        Some(s) => s,
        None => {
            // No-op success for TUI-owned sessions. The TUI calls
            // this for every workflow participant; an Existing
            // slot bound to a TUI-local session legitimately
            // misses the daemon registry.
            return Ok(json!({
                "ok": true,
                "daemon_owned": false,
            }));
        }
    };
    session.workflow_run_id = p.workflow_run_id;
    session.workflow_role = p.workflow_role;
    Ok(json!({
        "ok": true,
        "daemon_owned": true,
    }))
}

// ============================================================
// session.set_global_perms (global-perms feature)
// ============================================================
//
// Operator-only RPC the TUI uses to grant or revoke a live
// session's global-permissions flag — the human grant path (A-e
// session settings toggle, and the post-spawn grant for an
// operator who marks a session global at creation). Mutates
// `DaemonSession.global_perms` so the daemon's Session-caller auth
// (`auth::check_session_caller`) honors the change immediately for
// already-running agents; no respawn needed.
//
// **Caller**: Operator only. A Session caller must NOT be able to
// flip its own (or anyone's) grant — that would be a trivial
// self-escalation, defeating the whole descendant-only model. The
// only agent-side path to global perms is `mcp_start_session`'s
// escalation-guarded spawn param (caller must already be global).
//
// Wire shape:   { uid: <session_uid>, global_perms: <bool> }
// Response:     { ok: true, daemon_owned: bool }
//
// `daemon_owned: false` mirrors `set_workflow_context`: a no-op
// success for a uid the daemon doesn't own (e.g. a TUI-local
// session), so the TUI can fire this without branching.

#[derive(Deserialize)]
struct SetGlobalPermsParams {
    uid: String,
    global_perms: bool,
}

pub fn set_global_perms(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    let p: SetGlobalPermsParams = serde_json::from_value(params.clone())
        .map_err(|e| {
            (
                ErrorCode::InvalidParams,
                format!("session.set_global_perms params: {}", e),
            )
        })?;
    if p.uid.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "session.set_global_perms: 'uid' is required".into(),
        ));
    }
    let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    match state.sessions.get_mut(&p.uid) {
        Some(s) => {
            s.global_perms = p.global_perms;
            Ok(json!({ "ok": true, "daemon_owned": true }))
        }
        None => Ok(json!({ "ok": true, "daemon_owned": false })),
    }
}

/// Map daemon-side `session_type` (`"claude-code"` / `"codex"` /
/// `"bash"`) onto the canonical engine string the Python tool
/// dispatches its parser on. Mirrors the TUI's
/// `tui/src/control/methods.rs::engine_str` exactly so both
/// surfaces agree on the wire vocabulary.
fn engine_str(session_type: &str) -> &'static str {
    match session_type {
        "codex" => "codex",
        // The TUI's helper defaults to "claude-code" for
        // anything not "codex" — `bash` sessions return
        // "claude-code" too (they have no transcript so the
        // engine field is moot; Python tool short-circuits on
        // `transcript_path: None`). Preserve that quirk so
        // wire shapes match byte-for-byte.
        _ => "claude-code",
    }
}

// ============================================================
// task.update_tree (slice 10d-mcp-surface-2a)
// ============================================================
//
// TUI-pushed task-tree snapshot. The Phase 1 source-of-truth
// decision (see `daemon/src/control/auth.rs` module doc and
// `daemon/NOTES.md`'s sub-2a entry): TUI owns the planning task
// tree, daemon caches a snapshot, daemon's Session-caller auth
// reads from the snapshot.
//
// **Snapshot replace semantics**: each call REPLACES
// `state.task_tree` wholesale. The TUI's caller is responsible
// for sending the full tree; partial / diff updates aren't
// supported. This is cheaper to reason about than diff
// semantics (no question about stale ancestors) and the tree is
// small enough that wire size isn't a concern (a few hundred
// tasks at most, each entry is ~50 bytes).
//
// Operator-callable only — Session callers can't update the
// task tree (that'd be a privilege escalation: a Session caller
// could remove their task's parent_task_id chain to escape
// authorization).

#[derive(Deserialize)]
struct TaskUpdateTreeParams {
    /// Full tree snapshot. Each entry pairs a `task_id` with
    /// its `parent_task_id` (`None` for top-level tasks) and,
    /// since sub-2b-3 review-2 #1, an optional `workspace_id`
    /// — the workspace this task is bound to (`None` for tasks
    /// still in backlog).
    tasks: Vec<TaskUpdateTreeEntry>,
    /// Sub-2b-3 review-2 #1: workspace metadata pushed
    /// alongside the task tree. Lets `mcp_start_session`
    /// resolve a descendant task's `working_dir` without
    /// needing a live anchor session in that workspace (the
    /// pre-fix resolver walked `state.sessions` for an
    /// existing session; that fails for first-spawn-into-fresh-
    /// subtask which is the common case `mcp_start_session`
    /// serves).
    ///
    /// Replace-not-merge in lockstep with `tasks`. Field is
    /// `#[serde(default)]` so older TUI builds that don't
    /// know about it can still push trees without
    /// `workspaces`; the daemon falls back to the empty map
    /// and `mcp_start_session` for a descendant task without
    /// a bound workspace surfaces NotFound (matches pre-fix
    /// behavior for tasks not yet anchored).
    #[serde(default)]
    workspaces: Vec<TaskUpdateTreeWorkspaceEntry>,
}

#[derive(Deserialize)]
struct TaskUpdateTreeEntry {
    task_id: String,
    #[serde(default)]
    parent_task_id: Option<String>,
    /// Sub-2b-3 review-2 #1: which workspace this task is
    /// bound to. `None` for backlog tasks; daemon stores
    /// task→workspace mappings only for entries with
    /// `Some(workspace_id)`.
    #[serde(default)]
    workspace_id: Option<String>,
}

#[derive(Deserialize)]
struct TaskUpdateTreeWorkspaceEntry {
    workspace_id: String,
    /// Worktree path — daemon stores on
    /// `state.workspaces[workspace_id].worktree_path`. The
    /// daemon's existing `state.workspaces` map already
    /// carries this (`start_session`'s auto-register branch
    /// populates it); the new wire field extends coverage to
    /// workspaces that haven't been spawned into yet.
    #[serde(default)]
    worktree_path: Option<String>,
}

pub fn task_update_tree(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    let p: TaskUpdateTreeParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("task.update_tree params: {}", e)))?;
    let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    // Flag the first push so `auth::check_session_caller` can
    // distinguish "tree pushed and target genuinely out of scope"
    // from "tree hasn't been pushed yet" — see `DaemonState::task_tree_pushed`.
    state.task_tree_pushed = true;
    // Replace, not merge. The TUI sends the full tree on every
    // update; partial updates aren't a thing (see module doc).
    state.task_tree.clear();
    state.task_workspaces.clear();
    for entry in p.tasks {
        if let Some(ws) = entry.workspace_id.clone() {
            state.task_workspaces.insert(entry.task_id.clone(), ws);
        }
        state.task_tree.insert(entry.task_id, entry.parent_task_id);
    }
    // Sub-2b-3 review-2 #1: merge-not-replace on
    // `state.workspaces`. The daemon's own `start_session`
    // auto-register branch adds workspace entries with the
    // full `ManifestWorkspace` shape (worktree_path + repo_url
    // + worker_vm + ...); the TUI's task.update_tree push
    // carries only `worktree_path` per workspace. Replacing
    // the whole map would lose data already discovered by
    // session-spawn. Instead, upsert worktree_path on a per-
    // workspace basis: existing entries get their
    // worktree_path updated (TUI is authoritative for the
    // mapping); missing entries are inserted with a minimal
    // ManifestWorkspace.
    // Build the set of pushed workspace_ids so we can GC entries
    // that disappeared from the TUI's snapshot AND have no live
    // sessions referencing them (Batch 1B fix for finding #7,
    // stale-workspace accumulation across TUI sessions).
    let pushed_ws_ids: std::collections::HashSet<String> =
        p.workspaces.iter().map(|w| w.workspace_id.clone()).collect();
    for ws_entry in p.workspaces {
        let entry = state
            .workspaces
            .entry(ws_entry.workspace_id.clone())
            .or_insert_with(|| crate::manifest::ManifestWorkspace {
                id: ws_entry.workspace_id.clone(),
                worktree_path: None,
                ..Default::default()
            });
        // Sub-2b-3 review-3 #2: assign unconditionally. The TUI's
        // Option<String> wire shape carries None for both
        // "deliberately deleted" (finish_push after cloud upload)
        // AND "I don't have a path yet" (workspace anchor, cloud
        // workspace, in-flight reconcile). The two interpretations
        // CONFLICT — Batch 1B post-review finding #6 raised the
        // don't-know case but flipping to Some-only broke the
        // finish_push contract (`finish_push_wire_shape_clears_daemon_worktree_path`
        // test) that the daemon clears on TUI's deliberate-delete
        // push. The wire shape needs a sentinel to distinguish
        // the two; until that lands, keep the existing clear-on-
        // None behavior (which matches the user-visible "TUI
        // owns the source of truth" rule) and accept the daemon-
        // discovered-path-cleared edge case. Tracked as a wire-
        // shape follow-up.
        entry.worktree_path = ws_entry.worktree_path.map(std::path::PathBuf::from);
    }
    // GC workspaces that fell out of the TUI's push AND have no
    // live daemon-owned session anchoring them. Without this,
    // closed workspaces accumulate forever in the persistent
    // daemon's state.workspaces map (finding #7). The "no live
    // session" guard prevents removing a workspace the daemon
    // itself just auto-registered from a fresh spawn (the spawn
    // path adds to state.workspaces BEFORE the TUI's next push
    // sees it).
    let live_ws_ids: std::collections::HashSet<String> = state
        .sessions
        .values()
        .map(|s| s.workspace_id.clone())
        .collect();
    // ALSO preserve workspaces bound to a task (`state.bindings`). A daemon-
    // created subtask workspace (`create_subtask`) is awaiting its FIRST agent:
    // it's not in the TUI's push (the TUI doesn't know the daemon-minted subtask
    // yet) AND has no live session yet, so the two guards above would GC it out
    // from under a headless orchestrator's `create_subtask`→`mcp_start_session`
    // — exactly the NotFound this GC's prior comment flagged. `bindings` survives
    // this push (only the daemon's startup manifest-load replaces it; this
    // handler clears `task_workspaces` but never `bindings`), so it's the
    // durable anchor that keeps the workspace alive until its agent spawns.
    let bound_ws_ids: std::collections::HashSet<String> =
        state.bindings.values().cloned().collect();
    state.workspaces.retain(|ws_id, _| {
        pushed_ws_ids.contains(ws_id)
            || live_ws_ids.contains(ws_id)
            || bound_ws_ids.contains(ws_id)
    });
    Ok(json!({
        "ok": true,
        "task_count": state.task_tree.len(),
        "workspace_count": state.task_workspaces.len(),
    }))
}

// ============================================================
// tui.update_sessions_snapshot (10d-1)
// ============================================================
//
// TUI-pushed session snapshot. The TUI is authoritative for
// sessions it spawned locally (`SpawnTarget::TuiLocal`); the
// daemon needs to know about them so 10d-2's workflow-method
// auth can recognize TUI-minted callers. Today the daemon
// only knows about sessions in `state.sessions` (those it
// spawned via `start_session` / `mcp_start_session`).
//
// **Auth: Operator-only.** Same rationale as
// `task.update_tree`: a Session caller rewriting the TUI's
// session map could escape its own auth scope by inserting a
// row that grants it visibility into another task.
//
// **Replace-not-merge** on every push. The TUI sends the full
// snapshot; partial diffs aren't a thing.
//
// **Empty-vs-unset**: explicit empty map is meaningful ("TUI
// has no sessions right now"). The `tui_sessions_pushed`
// flag flips on first push and stays true; future auth
// consumers (10d-2) use the flag to distinguish "TUI
// deliberately reported empty" from "TUI hasn't pushed yet."

#[derive(Deserialize)]
struct TuiUpdateSessionsSnapshotParams {
    sessions: Vec<crate::state::TuiSessionSnapshot>,
}

pub fn tui_update_sessions_snapshot(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    let p: TuiUpdateSessionsSnapshotParams = serde_json::from_value(params.clone())
        .map_err(|e| (
            ErrorCode::InvalidParams,
            format!("tui.update_sessions_snapshot params: {}", e),
        ))?;
    let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    state.tui_sessions.clear();
    for entry in p.sessions {
        state.tui_sessions.insert(entry.uid.clone(), entry);
    }
    state.tui_sessions_pushed = true;
    Ok(json!({
        "ok": true,
        "session_count": state.tui_sessions.len(),
    }))
}

// ============================================================
// workflow.update_definitions (10d-2c-2-1)
// ============================================================
//
// TUI-pushed workflow TOML definitions. Daemon's upcoming
// on_idle driver (2c-2-2) needs them to look up
// `static_transition_on_idle` for the active role, the target
// role's `activation_prompt` template, and per-role engine /
// context knobs. Pushed at TUI startup (right after
// `workflow::toml_schema::load_all(workflows_dir)`) and on any
// later reload.
//
// Replace-not-merge — same shape as `task.update_tree` and
// `tui.update_sessions_snapshot`. Operator-only on the wire:
// a Session caller (i.e. an agent) could otherwise rewrite the
// transition table for the workflow it's a participant of and
// redirect the static-idle gate, breaking the workflow author's
// intent.

#[derive(Deserialize)]
struct WorkflowUpdateDefinitionsParams {
    /// Map keyed by workflow name (`Workflow::name`). Replace
    /// semantics, but only for the OVERRIDE layer: the daemon clears
    /// and repopulates `state.workflow_definitions` (the TUI-pushed
    /// override) — the `workflows_dir` BASE layer
    /// (`state.base_workflow_definitions`, loaded at startup) is left
    /// intact, so a TUI reconnect never clobbers definitions a
    /// no-TUI daemon loaded for itself (Phase 4 two-layer lookup).
    workflows: std::collections::HashMap<
        String,
        crate::workflow::toml_schema::Workflow,
    >,
}

pub fn workflow_update_definitions(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    let p: WorkflowUpdateDefinitionsParams = serde_json::from_value(params.clone())
        .map_err(|e| (
            ErrorCode::InvalidParams,
            format!("workflow.update_definitions params: {}", e),
        ))?;
    let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    state.workflow_definitions.clear();
    for (name, wf) in p.workflows {
        state.workflow_definitions.insert(name, wf);
    }
    Ok(json!({
        "ok": true,
        "workflow_count": state.workflow_definitions.len(),
    }))
}

#[derive(Deserialize)]
struct StartWorkflowParams {
    workflow_name: String,
    #[serde(default)]
    goal: Option<String>,
    /// Explicit worktree path (Operator/TUI path). When absent, resolved from
    /// the Session caller's workspace.
    #[serde(default)]
    worktree: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    task_key: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
    /// Existing-session binding (Phase 1 of `doc/existing-session-binding.md`):
    /// optional `role -> existing daemon_session_uid` map. For each entry the
    /// daemon ADOPTS the already-running live session as that role instead of
    /// fresh-spawning it — preserving its explored context and (for the initial
    /// worker) delivering the goal to the warm agent. Eligibility is enforced
    /// (persistent + `needs_mcp=false` roles only, same-workspace, engine-match,
    /// exclusive); an ineligible/unresolvable entry FAILS the launch rather than
    /// silently fresh-spawning. ABSENT → byte-identical fresh-spawn behavior.
    #[serde(default)]
    role_sessions: Option<std::collections::BTreeMap<String, String>>,
    /// Per-role engine override for FRESH-spawned roles: optional
    /// `role -> "claude-code" | "codex"` map. When present for a role, the
    /// daemon spawns that engine instead of the role's TOML-declared `engine`,
    /// letting the operator pick "new claude" vs "new codex" at launch time.
    /// Only applies to fresh spawns — a role bound via `role_sessions` keeps the
    /// bound session's own engine and ignores any override here. An unknown
    /// value or absent entry falls back to the TOML `engine`. ABSENT → behavior
    /// identical to the TOML default.
    #[serde(default)]
    role_engines: Option<std::collections::BTreeMap<String, String>>,
}

/// Phase 4 §D: daemon-side `start_workflow`. Spawns each role's participant
/// session (fresh) via the in-process `start_session` path, writes the initial
/// `state.json` (`WorkflowRun::new` seeds the worker's iteration-1
/// `TriggerKind::Initial` entry), and records the worker's INITIAL pending
/// activation (`is_initial` — delivery-only, never appends a second worker row).
/// The poller drives every hand-off from there, so the run completes with or
/// without a TUI attached. Worktree resolution: an explicit `worktree` param
/// (TUI `A-f` / Operator) takes precedence, else the Session caller's workspace.
pub fn start_workflow(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    params: &Value,
) -> MethodResult {
    use crate::workflow::run::{MessageBaseline, RoleBinding};
    use crate::workflow::toml_schema::Engine;

    let p: StartWorkflowParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("start_workflow params: {}", e)))?;
    if p.workflow_name.trim().is_empty() {
        return Err((ErrorCode::InvalidParams, "start_workflow: 'workflow_name' is required".into()));
    }

    let caller_uid: Option<String> = match caller {
        Caller::Session(s) => Some(s.session_uid.clone()),
        Caller::Operator(_) => None,
    };

    // Resolve the definition (two-layer) + worktree/workspace under the lock.
    // P-3: also resolve the memory cap each participant should inherit. Caps are
    // a CALLER-supplied policy (the TUI computes soft/hard bytes + cgroup_prefix
    // and they ride a session's start_session params); there is no daemon-side
    // cap config. So:
    //   - Session caller (an agent launching a sub-workflow): inherit the
    //     CALLER session's cap, exactly as `mcp_start_session` does, so workflow
    //     participants are capped like every other descendant spawn.
    //   - Operator/headless caller (no caller session): there is NO cap source
    //     today — participants run UNCAPPED. This is stated explicitly rather
    //     than dropped silently; capping the headless operator-launched case
    //     needs a daemon.toml cap-policy field (a follow-up, out of this phase's
    //     acceptance criteria). See the `participant_cap` resolution below.
    let (wf, worktree, workspace_id, task_id, participant_cap, caller_size) = {
        let state = state_arc.lock().unwrap_or_else(|pp| pp.into_inner());
        let wf = state.workflow_definition(&p.workflow_name).cloned().ok_or((
            ErrorCode::NotFound,
            format!(
                "start_workflow: workflow definition '{}' not loaded (base or override)",
                p.workflow_name
            ),
        ))?;
        // P4 scope: an Operator (TUI) passes an explicit worktree (token already
        // validated at dispatch). A Session caller is CONFINED to its own
        // session's workspace — any client-supplied `worktree`/`workspace_id` is
        // ignored, so an agent can't launch participants in an arbitrary tree.
        let mut participant_cap: Option<(u64, u64, String)> = None;
        // Inherit the caller session's current PTY size for participants when
        // the client didn't pass explicit cols/rows — same rationale as
        // `mcp_start_session`'s caller-size inheritance: a serde-default 80×24
        // participant renders "super narrow" in any full-size attach view.
        // Operator/headless callers have no session to inherit from (the TUI
        // passes its own size explicitly).
        let mut caller_size: Option<(u16, u16)> = None;
        let (worktree, workspace_id, task_id) = match caller_uid.as_deref() {
            None => {
                let wt = p.worktree.clone().ok_or((
                    ErrorCode::InvalidParams,
                    "start_workflow: Operator caller must pass `worktree`".into(),
                ))?;
                // Operator/headless: no caller session → no inherited cap.
                (wt, p.workspace_id.clone().unwrap_or_default(), p.task_id.clone())
            }
            Some(cuid) => {
                let c = state.sessions.get(cuid).ok_or((
                    ErrorCode::Unauthorized,
                    format!("start_workflow: caller session '{}' not in registry", cuid),
                ))?;
                // P-3: inherit the caller session's cap (all-or-nothing triple,
                // like `mcp_start_session`). A capped agent's sub-workflow gets
                // capped participants; an uncapped caller leaves participants
                // uncapped (None).
                participant_cap = match (
                    c.memory_cap_soft_bytes,
                    c.memory_cap_hard_bytes,
                    c.cgroup_prefix.clone(),
                ) {
                    (Some(soft), Some(hard), Some(prefix)) => {
                        Some((soft, hard, prefix.to_string_lossy().into_owned()))
                    }
                    _ => None,
                };
                caller_size = Some((c.last_cols, c.last_rows));
                let own_task = c.task_id.clone();
                // A client-supplied task_id must be self-or-descendant of the
                // caller's own task; otherwise default to the caller's task.
                let task_id = match p.task_id.clone() {
                    Some(req_task) => {
                        let ok = own_task.as_deref().map_or(false, |own| {
                            crate::control::auth::task_is_self_or_descendant_of(
                                &state.task_tree,
                                &req_task,
                                own,
                            )
                        });
                        if !ok {
                            return Err((
                                ErrorCode::Unauthorized,
                                format!(
                                    "start_workflow: task '{}' is not the caller's task or a descendant",
                                    req_task
                                ),
                            ));
                        }
                        Some(req_task)
                    }
                    None => own_task.clone(),
                };
                // P-B: a DESCENDANT task (≠ caller's own) with its own
                // (branch-mode) worktree must spawn THERE, not in the caller's
                // worktree. Resolve via `task_workspaces` exactly like
                // mcp_start_session; own-task / no-task uses the caller's
                // workspace.
                let descendant = task_id
                    .as_deref()
                    .filter(|req| own_task.as_deref() != Some(*req));
                let ws_id = match descendant {
                    Some(req_task) => state.task_workspaces.get(req_task).cloned().ok_or((
                        ErrorCode::NotFound,
                        format!(
                            "start_workflow: descendant task '{}' has no bound workspace \
                             (the TUI must push task.update_tree with workspace_id)",
                            req_task
                        ),
                    ))?,
                    None => c.workspace_id.clone(),
                };
                let wt = state
                    .workspaces
                    .get(&ws_id)
                    .and_then(|w| w.worktree_path.clone())
                    .ok_or((
                        ErrorCode::InvalidParams,
                        format!("start_workflow: no worktree for workspace '{}'", ws_id),
                    ))?;
                (wt.to_string_lossy().into_owned(), ws_id, task_id)
            }
        };
        (wf, worktree, workspace_id, task_id, participant_cap, caller_size)
    };

    // Finding 1: generate the run_id SERVER-SIDE — NEVER honor a caller-supplied
    // id, or a Session RPC could reuse an active run's id and clobber its
    // state.json on `run::save`. Defensively regenerate on the (astronomically
    // unlikely) collision with an existing run rather than overwrite it.
    let run_id = {
        let mut id = crate::workflow::run::new_run_id();
        let mut tries = 0;
        while crate::workflow::run::load_one(&id).is_some() && tries < 8 {
            id = crate::workflow::run::new_run_id();
            tries += 1;
        }
        if crate::workflow::run::load_one(&id).is_some() {
            return Err((
                ErrorCode::Internal,
                "start_workflow: could not allocate a unique run_id".into(),
            ));
        }
        id
    };
    let task_key = p.task_key.clone().unwrap_or_else(|| workspace_id.clone());
    let cols = p.cols.or(caller_size.map(|s| s.0)).unwrap_or(80);
    let rows = p.rows.or(caller_size.map(|s| s.1)).unwrap_or(24);
    let goal = p.goal.clone().unwrap_or_default();

    // Finding 1: track spawned participants so a partial-launch failure cleans
    // them up — no orphaned sessions left running without a valid run.
    let mut spawned_uids: Vec<String> = Vec::new();
    let cleanup_spawned = |uids: &[String]| {
        let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        for uid in uids {
            // DaemonSession's pidfd-based Drop SIGKILLs the child on removal.
            state.sessions.remove(uid);
        }
    };

    // Spawn each role fresh, in role_order, via the in-process start_session.
    let mut role_sessions: std::collections::BTreeMap<String, RoleBinding> = std::collections::BTreeMap::new();
    let mut role_baselines: std::collections::BTreeMap<String, MessageBaseline> = std::collections::BTreeMap::new();

    // ───────────── Existing-session binding (Phase 1) ─────────────
    // Resolve every `role_sessions` entry UP FRONT — validate eligibility,
    // eagerly resolve the bound session's sid, capture its turn/text baselines
    // + any accepted pre-launch plan — BEFORE the spawn loop. Doing it before
    // any spawn means a bind REJECTION leaves zero spawned sessions to clean
    // up; the loop below simply skips a bound role. Bound sessions are tagged
    // with (run_id, role) only AFTER state.json is saved (tag-after-save), so a
    // pre-save failure never orphans a tag on a pre-existing session.
    let role_sessions_param: std::collections::BTreeMap<String, String> =
        p.role_sessions.clone().unwrap_or_default();
    let mut bound_bindings: std::collections::BTreeMap<String, RoleBinding> =
        std::collections::BTreeMap::new();
    let mut bound_baselines: std::collections::BTreeMap<String, MessageBaseline> =
        std::collections::BTreeMap::new();
    let mut bound_text_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut bound_plans: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    // (uid, role) pairs tagged AFTER save (tag-after-save).
    let mut bound_uid_roles: Vec<(String, String)> = Vec::new();
    if !role_sessions_param.is_empty() {
        use crate::workflow::toml_schema::Context;
        // Engine spelling differs across surfaces (the TUI's
        // `Engine::as_session_type()` returns `"claude"`; the daemon spawn path
        // uses `"claude-code"`); compare on a normalized form.
        fn normalize_engine(s: &str) -> &str {
            match s {
                "claude" | "claude-code" => "claude",
                other => other,
            }
        }
        struct BindWork {
            role: String,
            uid: String,
            engine: Engine,
            sid: String,
        }
        // Validate eligibility + eagerly resolve each bound sid under the lock
        // (pure reads). Any failure returns BEFORE the spawn loop runs.
        let binds: Vec<BindWork> = {
            let state = state_arc.lock().unwrap_or_else(|pp| pp.into_inner());
            // Loaded once for the exclusivity check below.
            let all_runs = crate::workflow::run::load_all();
            let mut seen_uids: std::collections::HashMap<&str, &str> =
                std::collections::HashMap::new();
            let mut out: Vec<BindWork> = Vec::new();
            for (role_name, uid) in &role_sessions_param {
                // (1) role exists in the workflow definition.
                let Some(role) = wf.roles.get(role_name) else {
                    return Err((
                        ErrorCode::InvalidParams,
                        format!(
                            "start_workflow: role_sessions names role '{}' which is not \
                             in workflow '{}'",
                            role_name, p.workflow_name
                        ),
                    ));
                };
                // (2) persistent (not fresh) — fresh resets context on every
                // activation; adopting context to KEEP it contradicts that.
                if role.context != Context::Persistent {
                    return Err((
                        ErrorCode::InvalidParams,
                        format!(
                            "start_workflow: role '{}' is Context::Fresh and cannot bind \
                             an existing session (fresh roles reset on every activation; \
                             the adopted context would be wiped)",
                            role_name
                        ),
                    ));
                }
                // (3) does not need the workflow MCP — a needs_mcp role would
                // require a --resume respawn to gain the MCP, defeating the bind.
                if role.needs_mcp {
                    return Err((
                        ErrorCode::InvalidParams,
                        format!(
                            "start_workflow: role '{}' has needs_mcp=true and cannot bind \
                             an existing session (it would need a --resume respawn to gain \
                             the workflow MCP)",
                            role_name
                        ),
                    ));
                }
                // (4) no duplicate uid across two roles in this map.
                if let Some(prev_role) = seen_uids.insert(uid.as_str(), role_name.as_str()) {
                    return Err((
                        ErrorCode::InvalidParams,
                        format!(
                            "start_workflow: uid '{}' bound to two roles ('{}' and '{}') \
                             in role_sessions",
                            uid, prev_role, role_name
                        ),
                    ));
                }
                // (5) uid is a live daemon session on THIS host (no cross-host).
                let Some(session) = state.sessions.get(uid) else {
                    return Err((
                        ErrorCode::InvalidParams,
                        format!(
                            "start_workflow: role_sessions uid '{}' is not a live daemon \
                             session in this host's registry",
                            uid
                        ),
                    ));
                };
                // (6) the bound session's engine matches the role's TOML engine.
                if normalize_engine(&session.session_type)
                    != normalize_engine(role.engine.as_session_type())
                {
                    return Err((
                        ErrorCode::InvalidParams,
                        format!(
                            "start_workflow: bound session '{}' is a '{}' session but role \
                             '{}' is engine '{}'",
                            uid,
                            session.session_type,
                            role_name,
                            role.engine.as_session_type()
                        ),
                    ));
                }
                // (7) the bound session is in the run's RESOLVED workspace, so
                // baseline counting reads ITS OWN transcript dir, not a
                // different worktree's. `check_session_caller` alone is
                // insufficient — it admits any descendant/same-workspace target.
                if session.workspace_id != workspace_id {
                    return Err((
                        ErrorCode::InvalidParams,
                        format!(
                            "start_workflow: bound session '{}' is in workspace '{}', not \
                             the run's workspace '{}'",
                            uid, session.workspace_id, workspace_id
                        ),
                    ));
                }
                // (8) caller is authorized for uid. Session caller passes the
                // descendant/same-workspace scope check; Operator was already
                // token-validated at the dispatch boundary.
                if let Some(cuid) = caller_uid.as_deref() {
                    if !crate::control::auth::check_session_caller(&state, cuid, uid).is_allow() {
                        return Err((
                            ErrorCode::Unauthorized,
                            format!(
                                "start_workflow: caller '{}' is not authorized to bind \
                                 session '{}' (out of descendant/workspace scope)",
                                cuid, uid
                            ),
                        ));
                    }
                }
                // (9) exclusivity: uid not already a participant of another
                // ACTIVE run, and no stale workflow tag pointing at an active
                // run — either would let the OTHER run's poller keep driving
                // this PTY after we overwrote its tags (two pollers, one agent).
                //
                // TOCTOU (deferred — out of Phase 1 scope): this check reads
                // `all_runs` under the lock, but the bound session is tagged only
                // AFTER `save` (the tag-after-save acceptance criterion releases
                // the lock in between). Two concurrent `start_workflow` RPCs both
                // binding the SAME uid could each pass this check before either
                // tags, yielding two active runs whose `daemon_session_uid` point
                // at one PTY. A reservation that closed the window would conflict
                // with tag-after-save, and the realistic trigger (two
                // simultaneous deliberate binds of one live session on a
                // mostly-single-operator daemon) is narrow. The doc's Phase-1 bar
                // is the rejection check itself (implemented + tested); closing
                // the race is left to a follow-up.
                for run in all_runs.iter().filter(|r| r.is_active()) {
                    if run
                        .role_sessions
                        .values()
                        .any(|b| b.daemon_session_uid.as_deref() == Some(uid.as_str()))
                    {
                        return Err((
                            ErrorCode::InvalidParams,
                            format!(
                                "start_workflow: uid '{}' is already a participant of \
                                 active run '{}'",
                                uid, run.run_id
                            ),
                        ));
                    }
                }
                if let Some(tag_run) = session.workflow_run_id.as_deref() {
                    if all_runs.iter().any(|r| r.is_active() && r.run_id == tag_run) {
                        return Err((
                            ErrorCode::InvalidParams,
                            format!(
                                "start_workflow: bound session '{}' still carries a \
                                 workflow tag for active run '{}'",
                                uid, tag_run
                            ),
                        ));
                    }
                }
                // Eagerly resolve the sid (claude file-stem / codex payload.id).
                // A bound agent APPENDS to its EXISTING transcript, so the
                // fresh-spawn listing-diff discovery never fires for it: resolve
                // NOW or reject (do NOT fall through to new-file discovery).
                let Some(tp) = session.transcript_path.as_deref() else {
                    return Err((
                        ErrorCode::InvalidParams,
                        format!(
                            "start_workflow: bound session '{}' has no resolvable transcript \
                             yet; retry once it has run a turn",
                            uid
                        ),
                    ));
                };
                let Some(sid) =
                    crate::workflow::poller::resolve_existing_sid(&session.session_type, tp)
                else {
                    return Err((
                        ErrorCode::InvalidParams,
                        format!(
                            "start_workflow: could not resolve a transcript sid for bound \
                             session '{}' (path '{}')",
                            uid, tp
                        ),
                    ));
                };
                out.push(BindWork {
                    role: role_name.clone(),
                    uid: uid.clone(),
                    engine: role.engine.clone(),
                    sid,
                });
            }
            out
        };
        // Baselines + plan read lock-free against the run's worktree (== the
        // bound session's worktree, enforced by the workspace-match check), so
        // the idle gate counts only turns produced AFTER the goal is delivered.
        let wt_path = std::path::Path::new(&worktree);
        for bw in binds {
            use crate::workflow::transcript::{
                count_messages, latest_plan, list_messages, MessageKind,
            };
            let assistant = count_messages(&bw.engine, wt_path, &bw.sid, MessageKind::Assistant);
            let user = count_messages(&bw.engine, wt_path, &bw.sid, MessageKind::User);
            let text =
                list_messages(&bw.engine, wt_path, &bw.sid, MessageKind::Assistant).len();
            let plan = latest_plan(&bw.engine, wt_path, &bw.sid);
            bound_bindings.insert(
                bw.role.clone(),
                RoleBinding {
                    session_label: bw.role.clone(),
                    current_session_id: Some(bw.sid.clone()),
                    daemon_session_uid: Some(bw.uid.clone()),
                    // The durable bind-time signal the finalize readiness gate
                    // keys off (NOT runtime sid presence — see RoleBinding::bound).
                    bound: true,
                },
            );
            bound_baselines.insert(
                bw.role.clone(),
                MessageBaseline { user_count: user, assistant_count: assistant },
            );
            bound_text_counts.insert(bw.role.clone(), text);
            if let Some(p) = plan {
                bound_plans.insert(bw.role.clone(), p);
            }
            bound_uid_roles.push((bw.uid, bw.role));
        }
    }
    // P-2: the configured `mcp_server_path` (daemon.toml) is the AUTHORITATIVE
    // location for the MCP server on a headless/remote deployment (cm-manager,
    // /opt/cm-daemon) where the repo-relative fallback doesn't resolve. Thread
    // it into each participant's MCP config writer so they can actually start
    // their MCP server and call workflow_transition/workflow_done.
    let configured_mcp_server_path: Option<String> = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let p = st.config.mcp_server_path.clone();
        if p.trim().is_empty() { None } else { Some(p) }
    };
    for role_name in &wf.role_order {
        let Some(role) = wf.roles.get(role_name) else { continue };
        // Existing-session binding: a bound role adopts its pre-resolved binding
        // + baseline (computed up front) instead of fresh-spawning. No spawn, no
        // detector, no spawn-queue ticket — the session is already live and its
        // sid is already resolved.
        if let Some(binding) = bound_bindings.get(role_name) {
            role_sessions.insert(role_name.clone(), binding.clone());
            role_baselines.insert(
                role_name.clone(),
                bound_baselines.get(role_name).cloned().unwrap_or_default(),
            );
            continue;
        }
        // Engine for this fresh spawn: an explicit `role_engines` override (the
        // operator's "new claude" vs "new codex" launch choice) wins; an unknown
        // or absent override falls back to the role's TOML-declared engine.
        let session_type: &'static str = match p
            .role_engines
            .as_ref()
            .and_then(|m| m.get(role_name))
            .map(|s| s.as_str())
        {
            Some("codex") => "codex",
            Some("claude-code") => "claude-code",
            _ => match role.engine {
                Engine::ClaudeCode => "claude-code",
                Engine::Codex => "codex",
            },
        };
        let uid = new_daemon_minted_session_uid();
        // P-3: the cap for THIS participant. Prefer the caller session's
        // inherited cap (Session caller); otherwise fall back to the
        // per-engine CONFIGURED cap (Operator/headless launch — the always-on
        // host this phase targets). Resolved per-role since the configured cap
        // is keyed by engine/session_type.
        let role_cap: Option<(u64, u64, String)> = participant_cap
            .clone()
            .or_else(|| resolve_configured_participant_cap(session_type));
        // P-CRIT: thread the workflow participant identity into BOTH the MCP
        // config writer (so CM_WORKFLOW_RUN_ID/CM_ROLE land in the MCP server's
        // config env block — the only env the MCP child reliably inherits) and
        // the agent's own env below. Setting it on the agent env alone is NOT
        // enough: workflow_transition/workflow_done read it from the MCP
        // server's os.environ, and that child doesn't inherit the agent's env.
        let wf_meta = crate::mcp_config::WorkflowMeta {
            run_id: &run_id,
            role: role_name,
        };
        let (program, argv_tail) =
            match resolve_workflow_spawn_program(
                session_type,
                &uid,
                Some(&wf_meta),
                configured_mcp_server_path.as_deref(),
            ) {
                Ok(v) => v,
                Err(e) => {
                    cleanup_spawned(&spawned_uids);
                    return Err((ErrorCode::Internal, format!("start_workflow build_args({}): {}", role_name, e)));
                }
            };
        // P-3 enforcement: when a cap is inherited, wrap the argv in
        // `systemd-run --scope` with the memory limits — exactly as
        // `mcp_start_session` does — so the participant actually runs under the
        // ceiling, not just records it. Skipped under the test spawn override
        // (deterministic `/bin/sleep`, no user-systemd dependency).
        let (program, argv_tail) = match &role_cap {
            Some((soft, hard, prefix)) if !workflow_spawn_override_active() => {
                let cap_spec = crate::mcp_config::CapSpec {
                    soft_bytes: *soft,
                    hard_bytes: *hard,
                    session_uid: &uid,
                    cgroup_prefix: std::path::Path::new(prefix),
                };
                let (wp, wa, _cgroup) =
                    crate::mcp_config::wrap_with_systemd_run(&program, &argv_tail, Some(&cap_spec));
                (wp, wa)
            }
            _ => (program, argv_tail),
        };
        let mut argv = Vec::with_capacity(argv_tail.len() + 1);
        argv.push(program);
        argv.extend(argv_tail);
        let env_obj: serde_json::Map<String, Value> = crate::mcp_config::build_env(&uid, Some(&wf_meta))
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        let mut full = serde_json::Map::new();
        full.insert("uid".into(), Value::String(uid.clone()));
        full.insert("workspace_id".into(), Value::String(workspace_id.clone()));
        full.insert("label".into(), Value::String(role_name.clone()));
        full.insert("argv".into(), Value::Array(argv.into_iter().map(Value::String).collect()));
        full.insert("working_dir".into(), Value::String(worktree.clone()));
        full.insert("worktree_path".into(), Value::String(worktree.clone()));
        full.insert("env".into(), Value::Object(env_obj));
        full.insert("session_type".into(), Value::String(session_type.to_string()));
        full.insert("workflow_run_id".into(), Value::String(run_id.clone()));
        full.insert("workflow_role".into(), Value::String(role_name.clone()));
        if let Some(tid) = task_id.clone() {
            full.insert("task_id".into(), Value::String(tid));
        }
        // P-3: pass the inherited cap (Session-caller path) so each participant
        // is spawned under the same memory ceiling as every other session.
        // `start_session` then verifies the child landed in the systemd-run
        // `cm-sess-*.scope` we wrapped it into (above). None (Operator/headless)
        // → uncapped, as documented. Under the test spawn override there is no
        // real scope, so we skip the cap KEYS (start_session's cgroup verify
        // would reject the fake `/bin/sleep`) — the threading decision is still
        // captured below for assertions.
        #[cfg(test)]
        CAPTURED_PARTICIPANT_CAP
            .with(|c| c.borrow_mut().push((uid.clone(), role_cap.clone())));
        if !workflow_spawn_override_active() {
            if let Some((soft, hard, ref prefix)) = role_cap {
                full.insert("memory_cap_bytes".into(), Value::Number(soft.into()));
                full.insert("memory_cap_hard_bytes".into(), Value::Number(hard.into()));
                full.insert("cgroup_prefix".into(), Value::String(prefix.clone()));
            }
        }
        full.insert("cols".into(), Value::Number(cols.into()));
        full.insert("rows".into(), Value::Number(rows.into()));
        // P-A: SERIALIZE each participant's snapshot+spawn+detect through the
        // worktree spawn queue (mirrors mcp_start_session). Two claude roles
        // (worker, manager) share one transcript dir; without serialization
        // their detectors each diff against a stale pre-snapshot taken before
        // the other's transcript appears and can cross-bind. By enqueueing +
        // waiting BEFORE the snapshot, role B's snapshot isn't taken until role
        // A's detector has bound (so A's transcript is excluded from B's diff).
        let wt_path = std::path::Path::new(&worktree).to_path_buf();
        // Cross-bind fix: spawn-time detectors are CODEX-ONLY. A Codex agent
        // writes its rollout file at boot, so detection-at-spawn is causal and
        // the spawn-queue serialization above actually serializes. A Claude
        // participant writes NO transcript until its first prompt — its
        // detector window always outlives the slot wait, every role's detector
        // ends up armed concurrently against an empty snapshot, and whichever
        // polls first claims the worker's first transcript (observed on
        // cm-manager: the worker's transcript bound to the idle manager,
        // wedging the run headlessly). Claude roles instead bind causally at
        // activation time via the finalize drainer's deliver-then-discover
        // snapshot diff (`ActivationPhase::RebindPending`).
        let detector_engine = if workflow_detector_disabled() {
            None
        } else {
            match crate::transcript_detect::DetectorEngine::from_session_type(session_type) {
                Some(crate::transcript_detect::DetectorEngine::Codex) => {
                    Some(crate::transcript_detect::DetectorEngine::Codex)
                }
                _ => None,
            }
        };
        let ticket = match detector_engine {
            Some(_) => {
                let queue_arc = workflow_spawn_queue(state_arc, &wt_path);
                let seq = queue_arc.enqueue();
                let ticket = crate::state::WorktreeSpawnTicket::new(queue_arc.clone(), seq);
                // Bounded wait — a slow/non-writing prior detector shouldn't
                // block the launch forever. On timeout, drop the slot and arm
                // this role's detector unserialized (best-effort): correctness
                // for the common (transcript-on-startup) case, liveness for the
                // pathological one.
                if queue_arc.wait_for_turn_timeout(seq, slot_wait_timeout()).is_err() {
                    eprintln!(
                        "cm-daemon: start_workflow: spawn-queue wait timed out for role {} \
                         — arming detector unserialized (best-effort)",
                        role_name
                    );
                    drop(ticket);
                    None
                } else {
                    Some(ticket)
                }
            }
            None => None,
        };
        // Snapshot AFTER the wait so prior roles' bound transcripts are excluded.
        let pre_snapshot: Vec<String> = match detector_engine {
            Some(crate::transcript_detect::DetectorEngine::ClaudeCode) => {
                crate::transcript_detect::snapshot_claude_transcript_ids(&wt_path)
            }
            Some(crate::transcript_detect::DetectorEngine::Codex) => {
                crate::transcript_detect::snapshot_codex_transcript_ids(&wt_path)
            }
            None => Vec::new(),
        };
        #[cfg(test)]
        record_spawn_snapshot_for_test(&uid, &pre_snapshot);
        if let Err((c, m)) = start_session(state_arc, &Value::Object(full)) {
            cleanup_spawned(&spawned_uids);
            return Err((c, format!("start_workflow spawn {}: {}", role_name, m)));
        }
        spawned_uids.push(uid.clone());
        // P-B: arm the detector whenever this engine needs one — INCLUDING the
        // timeout case where `ticket` is None (serialization lost, but the role
        // still needs its transcript bound or the run wedges). Passing the
        // `Option<ticket>` straight through means None → unserialized arm
        // (best-effort), matching the timeout comment's stated intent. The old
        // `if let (Some(engine), Some(ticket))` guard SKIPPED the detector on
        // None, leaving the participant with no transcript_path forever.
        if let Some(engine) = detector_engine {
            // P-B: FAIL CLOSED on detector-thread spawn failure — a participant
            // with no detector never gets `transcript_path`, so
            // `sync_role_session_ids` can't bind `current_session_id` and the
            // run wedges after returning a run_id (forbidden by headless #1/#3).
            // Mirror `start_session`'s fail-closed contract: error + cleanup the
            // sessions spawned so far, rather than returning success with a dead
            // role.
            if let Err(e) = crate::transcript_detect::spawn_queued_detector(
                state_arc.clone(),
                uid.clone(),
                engine,
                wt_path.clone(),
                pre_snapshot,
                ticket,
                workflow_detector_spawn_fn(),
            ) {
                cleanup_spawned(&spawned_uids);
                return Err((
                    ErrorCode::Internal,
                    format!(
                        "start_workflow: transcript detector spawn failed for role '{}' \
                         (uid {}): {} — refusing to return a run with an undetectable \
                         participant that would wedge headlessly",
                        role_name, uid, e
                    ),
                ));
            }
        }
        role_sessions.insert(role_name.clone(), RoleBinding {
            session_label: role_name.clone(),
            current_session_id: None,
            daemon_session_uid: Some(uid),
            bound: false,
        });
        role_baselines.insert(role_name.clone(), MessageBaseline::default());
    }

    let initial_role = wf.role_order.first().cloned().ok_or((
        ErrorCode::InvalidParams,
        "start_workflow: workflow has no roles".into(),
    ))?;

    // Existing-session binding: when the INITIAL role is bound, seed its
    // iteration-1 history entry's `text_messages_at_start` from the live
    // transcript's text-bearing assistant count so `{{ roles.worker.this_turn }}`
    // covers only the post-goal turn (not the worker's pre-launch text). Any
    // bound role's accepted pre-launch plan rides in via `role_plans`.
    let initial_text_count = bound_text_counts.get(&initial_role).copied().unwrap_or(0);
    let mut run = crate::workflow::run::WorkflowRun::new(
        run_id.clone(),
        p.workflow_name.clone(),
        task_key,
        role_sessions,
        initial_role.clone(),
        role_baselines,
        if goal.is_empty() { None } else { Some(goal.clone()) },
        bound_plans,
        initial_text_count,
    );
    if let Some(tid) = task_id.clone() {
        run.task_id = Some(tid);
    }

    // Worker's INITIAL activation: delivery-only. With an `activation_prompt`,
    // raw_prompt is that template (rendered at finalization); otherwise it's the
    // goal delivered VERBATIM (no templating, so literal braces survive) —
    // mirrors the TUI's prepare_initial_prompt.
    // P-4: a WHITESPACE-only `activation_prompt` is treated as absent — the old
    // `prepare_initial_prompt` filtered whitespace before the presence check, so
    // a role with `activation_prompt = "   "` and a real goal must deliver the
    // GOAL (verbatim), not a blank template. Without this filter the
    // whitespace template would be "present", and the empty-`raw_prompt` skip
    // below wouldn't apply (raw_prompt = "   " ≠ empty after the goal fallback
    // is bypassed), so the worker would silently never receive the goal.
    let initial_activation_prompt = wf
        .roles
        .get(&initial_role)
        .and_then(|r| r.activation_prompt.clone())
        .filter(|p| !p.trim().is_empty());
    let verbatim = initial_activation_prompt.is_none();
    let raw_prompt = initial_activation_prompt.unwrap_or_else(|| goal.clone());
    // P-4: if the initial activation would be BLANK — no/whitespace-only
    // activation_prompt AND an empty goal → empty `raw_prompt` — do NOT queue
    // it. `finalize.rs` does `unwrap_or_default()` + presses Enter, so an empty
    // raw_prompt submits a whitespace turn to the fresh worker. The old
    // `prepare_initial_prompt` returned `None` and skipped queuing here; mirror
    // that. The worker still spawns + is active; the user drives it manually.
    // Feedback mode is unaffected (its worker's raw_prompt = the non-empty goal).
    if raw_prompt.trim().is_empty() {
        run.pending_activation = None;
    } else {
        run.pending_activation = Some(crate::workflow::run::PendingActivation {
            activation_id: 1,
            target_role: initial_role.clone(),
            iteration: 1,
            trigger: crate::workflow::run::TriggerKind::Initial,
            raw_prompt,
            verbatim,
            needs_fresh_reset: false,
            is_initial: true,
            phase: crate::workflow::run::ActivationPhase::Queued,
            rendered_prompt: None,
            pre_clear_snapshot: None,
            enter_fire_at_ms: None,
        });
    }
    if let Err(e) = crate::workflow::run::save(&run) {
        cleanup_spawned(&spawned_uids);
        return Err((ErrorCode::Internal, format!("start_workflow save run: {}", e)));
    }
    let (watcher, manifest_watcher, bound_manifest_diffs) = {
        let mut state = state_arc.lock().unwrap_or_else(|pp| pp.into_inner());
        state.workflow_runs.insert(run_id.clone(), run.clone());
        // Tag-after-save (existing-session binding): the bound (pre-existing)
        // sessions get their (run_id, role) workflow tags ONLY now that
        // state.json is durably saved. A failure before this point left them
        // untouched (no orphan tag pointing at a nonexistent run); fresh-spawned
        // participants were tagged at spawn and cleaned up on failure, bound
        // sessions simply kept their prior (untagged) state.
        //
        // Collect a manifest `Updated` diff per freshly-tagged bound session so
        // the TUI learns the (pre-existing) row is now a workflow participant —
        // it re-groups under the workflow header and workflow ops recognize it.
        // Fresh-spawned participants get this via `start_session`'s `Added`
        // broadcast; bound sessions already live in `state.sessions` (and in the
        // TUI's manifest), so they need an in-place `Updated` instead.
        let mut diffs: Vec<crate::manifest::ManifestDiff> = Vec::new();
        for (uid, role) in &bound_uid_roles {
            if let Some(s) = state.sessions.get_mut(uid) {
                s.workflow_run_id = Some(run_id.clone());
                s.workflow_role = Some(role.clone());
                diffs.push(crate::manifest::ManifestDiff::Updated {
                    uid: uid.clone(),
                    entry: json!({
                        "uid": uid,
                        "workspace_id": s.workspace_id,
                        "session_type": s.session_type,
                        "workflow_run_id": s.workflow_run_id,
                        "workflow_role": s.workflow_role,
                        "task_id": s.task_id,
                    }),
                });
            }
        }
        (
            state.workflow_event_watcher.clone(),
            std::sync::Arc::clone(&state.manifest_watcher),
            diffs,
        )
    };
    // Announce the bound sessions' new workflow tags to live `manifest.watch`
    // subscribers, lock-free after the state lock drops (mirrors the `Added` /
    // `Exited` broadcast shape in `start_session` / `handle_session_exit`).
    for diff in bound_manifest_diffs {
        manifest_watcher.broadcast(diff);
    }
    // P-3: broadcast the newly-created run as a state snapshot so clients that
    // subscribed BEFORE this launch (the launching TUI itself, plus any other
    // observer) fold it into their view immediately — `events.subscribe`
    // otherwise only emits snapshots at subscription time, leaving a fresh run
    // invisible/uncontrollable until reconnect (criterion #4). The watcher Arc
    // is cloned out above so this runs lock-free.
    watcher.broadcast_snapshot(run);

    Ok(json!({ "run_id": run_id }))
}

// ============================================================
// 10d-2c-3a: list_workflows + get_workflow_state
// ============================================================
//
// Relocates the two READ-ONLY workflow query methods to daemon
// dispatch. Pre-3a these routed to the TUI socket; agents
// querying workflow state had to roundtrip through the TUI
// process (no value-add — the TUI just read disk and applied
// the same auth check).
//
// Daemon-side reads disk directly via
// `workflow::run::load_all()` / `load_one()`. The TUI's
// pre-3a "in-memory app.workflow_runs first, disk fallback"
// pattern was a TUI reactive-UI optimization; the daemon
// doesn't have an equivalent reactive cache (the
// `state.workflow_runs` map is non-authoritative — the
// 2c-2-2-b round-2 reviewer-fix made the poller read disk
// for the same reason).
//
// **Intentional divergence from TUI** (documented for the
// audit trail):
//
// 1. Daemon reads disk only; no in-memory short-circuit.
//    TUI's optimization was App-bound; daemon's authoritative
//    state lives on disk.
//
// 2. Daemon's `lookup_session_any` does NOT fall through to
//    tombstones. TUI's `caller_ctx_or_tombstone` accepts
//    recently-closed sessions. Tombstones live in the TUI's
//    manifest; the daemon doesn't have a view of them.
//    Pre-existing daemon behavior — matches the existing
//    `workflow_transition` handler's caller-not-found
//    rejection. An agent calling these methods after their
//    session is tombstoned gets `NotFound` daemon-side instead
//    of `Allow + tombstone-flag` TUI-side. Acceptable: the
//    TUI's tombstone fallback was for race windows that don't
//    occur for daemon-routed callers in normal flow.
//
// 3. For Operator callers, daemon skips the auth check
//    (Operator is trusted by convention — same as every
//    other daemon-side method).

#[derive(serde::Deserialize, Default)]
pub struct GetWorkflowStateParams {
    pub run_id: String,
}

pub fn get_workflow_state(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    params: &Value,
) -> MethodResult {
    let p: GetWorkflowStateParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;
    if p.run_id.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "get_workflow_state: 'run_id' is required".into(),
        ));
    }
    // 10d-2c-3a review-r1: auth-ordering. Resolve caller FIRST,
    // load run SECOND, authorize THIRD. Pre-fix the run-load
    // happened before caller resolution, which:
    //   (1) leaked run-existence to probers with bogus
    //       session_uids via differential error messages
    //       ("caller session not found" vs "workflow run X
    //       not found").
    //   (2) wasted disk + flock work on auth-failure paths.
    //
    // Also: for Session callers, "run doesn't exist" and "run
    // exists but caller has no access" return the SAME error
    // code (Unauthorized) so a probe can't distinguish run
    // existence by error code. Operator callers see the
    // legitimate NotFound (they're trusted).
    let caller_view: Option<crate::state::SessionViewAny> = match caller {
        Caller::Session(uid) => {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            match state.lookup_session_any(&uid.session_uid) {
                Some(v) => Some(v),
                None => {
                    return Err((
                        ErrorCode::Unauthorized,
                        "caller session not authorized".into(),
                    ));
                }
            }
        }
        Caller::Operator(_) => None,
    };
    let run = match crate::workflow::run::load_one(&p.run_id) {
        Some(r) => r,
        None => {
            return match caller {
                Caller::Operator(_) => Err((
                    ErrorCode::NotFound,
                    format!("workflow run {} not found", p.run_id),
                )),
                Caller::Session(_) => Err((
                    ErrorCode::Unauthorized,
                    "workflow run is outside caller's scope".into(),
                )),
            };
        }
    };
    if let Some(cv) = caller_view {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        if !workflow_run_authorized_daemon(&cv, &state, &run) {
            return Err((
                ErrorCode::Unauthorized,
                "workflow run is outside caller's scope".into(),
            ));
        }
    }
    Ok(serialize_workflow_run_full(&run))
}

/// 11c: `workflow.get_state` body. Operator-only at the
/// dispatcher; this just validates params, loads disk, and
/// serializes the full `WorkflowRun` (matching 11b's snapshot
/// frame shape so the TUI consumer can deserialize either via
/// the same code path).
pub fn workflow_get_state(params: &Value) -> MethodResult {
    let p: GetWorkflowStateParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;
    if p.run_id.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "workflow.get_state: 'run_id' is required".into(),
        ));
    }
    match crate::workflow::run::load_one(&p.run_id) {
        Some(run) => serde_json::to_value(&run)
            .map_err(|e| (ErrorCode::Internal, format!("serialize WorkflowRun: {}", e))),
        None => Err((
            ErrorCode::NotFound,
            format!("workflow run {} not found", p.run_id),
        )),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct ListWorkflowsParams {
    #[serde(default)]
    pub task_id: Option<String>,
}

pub fn list_workflows(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    params: &Value,
) -> MethodResult {
    let p: ListWorkflowsParams = if params.is_null() {
        ListWorkflowsParams::default()
    } else {
        serde_json::from_value(params.clone())
            .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?
    };
    // 10d-2c-3a review-r1: for Session callers, look up
    // CallerCtx FIRST. Unknown session_uid returns
    // Unauthorized, NOT NotFound — same as
    // `get_workflow_state`. Returning NotFound here would
    // let a prober distinguish "your uid is gibberish" from
    // "no workflows visible to you" (both should be
    // indistinguishable at the auth boundary).
    let caller_view: Option<crate::state::SessionViewAny> =
        if let Caller::Session(uid) = caller {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            match state.lookup_session_any(&uid.session_uid) {
                Some(v) => Some(v),
                None => {
                    return Err((
                        ErrorCode::Unauthorized,
                        "caller session not authorized".into(),
                    ));
                }
            }
        } else {
            None
        };
    // task_id scope filter — Session callers must be authorized
    // for the requested scope. Operators skip this check.
    if let (Some(req), Some(cv)) = (p.task_id.as_deref(), caller_view.as_ref()) {
        match cv.task_id.as_deref() {
            None => {
                return Err((
                    ErrorCode::Unauthorized,
                    format!("taskless caller cannot scope to task {}", req),
                ));
            }
            Some(own) => {
                let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                if !crate::control::auth::task_is_self_or_descendant_of(
                    &state.task_tree,
                    req,
                    own,
                ) {
                    return Err((
                        ErrorCode::Unauthorized,
                        format!(
                            "task {} is not the caller's task or a descendant",
                            req
                        ),
                    ));
                }
            }
        }
    }

    let runs = crate::workflow::run::load_all();
    let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    let mut out: Vec<Value> = Vec::new();
    for run in &runs {
        if !list_workflows_visible_daemon(
            caller_view.as_ref(),
            &state,
            run,
            p.task_id.as_deref(),
        ) {
            continue;
        }
        out.push(serialize_workflow_run_summary(run));
    }
    Ok(Value::Array(out))
}

// ============================================================
// 10d-3: stop_workflow
// ============================================================
//
// Relocates the operator-side stop_workflow MCP method to
// daemon dispatch. Pre-3 this routed to the TUI socket; the
// TUI process owned the state.json mutation. Daemon-side
// canonicalizes the state machine write (the strict-controller
// reading: daemon owns running-state mutations).
//
// Scope decision (greenlit option B): ONLY `stop_workflow`
// relocates in 10d-3. `start_workflow` stays TUI-routed
// because its launch path interleaves session spawning + initial
// prompt delivery — the latter needs 3b's daemon-side prompt
// delivery before a clean relocation. Both deferred.
//
// **TUI's A-o (UI stop) flow continues to write state.json
// directly** via `workflow::run::modify(apply_stop_workflow_status)`.
// Both paths use the SAME `apply_stop_workflow_status` (now in
// `daemon/src/workflow/run.rs`); the parity is canonical by
// construction. Routing A-o through daemon RPC would add a
// roundtrip with no functional benefit for a UI button.

#[derive(serde::Deserialize, Default)]
pub struct StopWorkflowParams {
    pub run_id: String,
}

pub fn stop_workflow(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    params: &Value,
) -> MethodResult {
    let p: StopWorkflowParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;
    if p.run_id.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "stop_workflow: 'run_id' is required".into(),
        ));
    }
    // 10d-2c-3a review-r1 lesson: caller resolution FIRST,
    // load SECOND, authorize THIRD. Session callers get
    // Unauthorized (not NotFound) for both "bogus uid" and
    // "valid uid + nonexistent run" to prevent existence
    // probes via differential error codes. Operator callers
    // see legitimate NotFound (they're trusted).
    let caller_view: Option<crate::state::SessionViewAny> = match caller {
        Caller::Session(uid) => {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            match state.lookup_session_any(&uid.session_uid) {
                Some(v) => Some(v),
                None => {
                    return Err((
                        ErrorCode::Unauthorized,
                        "caller session not authorized".into(),
                    ));
                }
            }
        }
        Caller::Operator(_) => None,
    };
    let run = match crate::workflow::run::load_one(&p.run_id) {
        Some(r) => r,
        None => {
            return match caller {
                Caller::Operator(_) => Err((
                    ErrorCode::NotFound,
                    format!("workflow run {} not found", p.run_id),
                )),
                Caller::Session(_) => Err((
                    ErrorCode::Unauthorized,
                    "workflow run is outside caller's scope".into(),
                )),
            };
        }
    };
    if let Some(cv) = caller_view {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        if !workflow_run_authorized_daemon(&cv, &state, &run) {
            return Err((
                ErrorCode::Unauthorized,
                "workflow run is outside caller's scope".into(),
            ));
        }
    }
    // Apply the canonical mutation via `run::modify` (flock-
    // protected). Same function the TUI A-o flow uses; the
    // fire-output parity test pins this.
    //
    // The terminal-state guard inside
    // `apply_stop_workflow_status` is what makes second-stop
    // idempotent: Detached → no transition needed (mark_detached
    // is a no-op write of the same value); Done → guard
    // returns early.
    crate::workflow::run::modify(
        &p.run_id,
        crate::workflow::run::apply_stop_workflow_status,
    )
    .map_err(|e| {
        (
            ErrorCode::Internal,
            format!("stop_workflow modify failed: {}", e),
        )
    })?;
    Ok(json!({"ok": true}))
}

/// Combined scope+auth filter for `list_workflows` entries.
/// Operator-equivalent: `caller_view = None` always visible.
fn list_workflows_visible_daemon(
    caller_view: Option<&crate::state::SessionViewAny>,
    state: &DaemonState,
    run: &crate::workflow::run::WorkflowRun,
    explicit_scope: Option<&str>,
) -> bool {
    let Some(cv) = caller_view else {
        // Operator caller: see all (no scope filter unless
        // explicit_scope passed, which was already validated
        // above).
        return match explicit_scope {
            Some(req) => {
                let resolved_tid = resolve_run_task_id(state, run);
                match resolved_tid {
                    Some(rid) => crate::control::auth::task_is_self_or_descendant_of(
                        &state.task_tree,
                        &rid,
                        req,
                    ),
                    None => false,
                }
            }
            None => true,
        };
    };
    let Some(own) = cv.task_id.as_deref() else {
        // Taskless Session caller: can't see any workflow runs
        // (matches TUI's gate).
        return false;
    };
    let resolved_tid = resolve_run_task_id(state, run);
    let Some(rid) = resolved_tid else {
        return false;
    };
    if !crate::control::auth::task_is_self_or_descendant_of(
        &state.task_tree,
        &rid,
        own,
    ) {
        return false;
    }
    // If explicit_scope is set, the run's task must also be
    // self-or-descendant of that scope.
    if let Some(req) = explicit_scope {
        return crate::control::auth::task_is_self_or_descendant_of(
            &state.task_tree,
            &rid,
            req,
        );
    }
    true
}

/// Auth check for `get_workflow_state` — same shape as TUI's
/// `workflow_run_authorized`. Operator bypass is handled at the
/// call site (we only call this for Session callers).
fn workflow_run_authorized_daemon(
    caller: &crate::state::SessionViewAny,
    state: &DaemonState,
    run: &crate::workflow::run::WorkflowRun,
) -> bool {
    let Some(own) = caller.task_id.as_deref() else {
        return false;
    };
    let Some(rid) = resolve_run_task_id(state, run) else {
        return false;
    };
    crate::control::auth::task_is_self_or_descendant_of(
        &state.task_tree,
        &rid,
        own,
    )
}

/// Resolve a workflow run's task_id, with the same priority
/// order as TUI's `workflow_run_authorized`:
///   1. `run.task_id` (set by MCP `start_workflow_run`).
///   2. Reverse-walk `state.task_workspaces` (task_id → ws_id)
///      to find a task bound to `run.task_key` (ws_id). Only
///      accept if EXACTLY ONE candidate — avoids leaking
///      across task boundaries in a workspace that hosts
///      multiple tasks.
fn resolve_run_task_id(
    state: &DaemonState,
    run: &crate::workflow::run::WorkflowRun,
) -> Option<String> {
    if let Some(rid) = run.task_id.as_deref() {
        return Some(rid.to_string());
    }
    let candidates: Vec<&str> = state
        .task_workspaces
        .iter()
        .filter(|(_, ws)| ws.as_str() == run.task_key.as_str())
        .map(|(tid, _)| tid.as_str())
        .collect();
    if candidates.len() != 1 {
        return None;
    }
    Some(candidates[0].to_string())
}

fn run_status_str_daemon(status: &crate::workflow::run::RunStatus) -> &'static str {
    use crate::workflow::run::RunStatus;
    match status {
        RunStatus::Running => "running",
        RunStatus::Paused => "paused",
        RunStatus::Done => "done",
        RunStatus::Detached => "detached",
    }
}

fn serialize_workflow_run_summary(run: &crate::workflow::run::WorkflowRun) -> Value {
    json!({
        "run_id": run.run_id,
        "name": run.workflow_name,
        "task_id": run.task_id,
        "workspace_id": run.task_key,
        "active_role": run.active_role,
        "iteration": run.iteration,
        "paused": run.paused,
        "status": run_status_str_daemon(&run.status),
        "started_at": run.started_at,
        "done_reason": run.done_reason,
    })
}

fn serialize_workflow_run_full(run: &crate::workflow::run::WorkflowRun) -> Value {
    let history: Vec<Value> = run
        .history
        .iter()
        .map(|h| {
            json!({
                "iteration": h.iteration,
                "role": h.role,
                "transcript_id": h.session_id,
                "last_message": h.last_message,
                "activated_at": h.activated_at,
                "deactivated_at": h.deactivated_at,
                "trigger": serde_json::to_value(&h.trigger)
                    .unwrap_or(Value::Null),
                "assistant_count_at_start": h.assistant_count_at_start,
            })
        })
        .collect();
    let role_sessions: serde_json::Map<String, Value> = run
        .role_sessions
        .iter()
        .map(|(role, binding)| {
            (
                role.clone(),
                json!({
                    "session_label": binding.session_label,
                    "current_transcript_id": binding.current_session_id,
                }),
            )
        })
        .collect();
    json!({
        "run_id": run.run_id,
        "name": run.workflow_name,
        "task_id": run.task_id,
        "workspace_id": run.task_key,
        "active_role": run.active_role,
        "iteration": run.iteration,
        "paused": run.paused,
        "status": run_status_str_daemon(&run.status),
        "started_at": run.started_at,
        "done_reason": run.done_reason,
        "goal": run.goal,
        "history": history,
        "role_sessions": Value::Object(role_sessions),
    })
}

// ============================================================
// propose_task (sub-2b-2)
// ============================================================
//
// The Python MCP `propose_task` tool (`mcp_server/server.py:124`)
// pre-2b-2 called `cli.planning_client.PlanningClient().propose_task(...)`,
// which POSTs directly to the planning API's `/tasks` endpoint.
// That worked but bypassed every sub-2a auth gate and left the
// daemon unable to log/audit/policy proposals.
//
// Sub-2b-2 moves the API talk daemon-side. Python now routes
// through `control_client.call("propose_task", …)`; the daemon
// owns the `CM_API_URL` + `CM_API_TOKEN` ingress and the
// `POST /tasks` egress. Any caller (Operator or Session) may
// propose — same rule the Python tool enforces today (no
// task-subtree gating; the project owner reviews the queue and
// accepts/rejects manually).
//
// **Auth shape**: both Operator and Session callers proceed.
// We don't gate on `check_session_caller` because there's no
// "target session" — the agent proposes a task into a project
// queue, which it doesn't need to "own" in any session-scoped
// sense. The TUI's existing UX is also unrestricted (any user
// can propose; the project owner decides).

#[derive(Deserialize)]
struct ProposeTaskParams {
    project: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    prompt: String,
    /// Git remote URL the task is for. The Python tool today
    /// auto-detects via `git remote get-url origin` if missing;
    /// the daemon doesn't know the agent's cwd, so we require
    /// the caller to send it. The Python tool wrapper is
    /// responsible for inferring + filling this when the agent
    /// omitted it.
    repo_url: String,
    #[serde(default)]
    difficulty: Option<i32>,
    #[serde(default)]
    depends: Option<Vec<String>>,
}

pub fn propose_task(state_arc: &Arc<Mutex<DaemonState>>, params: &Value) -> MethodResult {
    let p: ProposeTaskParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("propose_task params: {}", e)))?;
    if p.project.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "propose_task: 'project' is required and non-empty".into(),
        ));
    }
    if p.name.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "propose_task: 'name' is required and non-empty".into(),
        ));
    }
    if p.repo_url.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "propose_task: 'repo_url' is required (the Python tool wrapper \
             should auto-detect via `git remote get-url origin` and forward; \
             daemon doesn't know the agent's cwd)".into(),
        ));
    }
    let req = crate::planning_client::ProposeTaskRequest {
        project: &p.project,
        name: &p.name,
        description: &p.description,
        prompt: &p.prompt,
        repo_url: &p.repo_url,
        difficulty: p.difficulty,
        depends: p.depends.as_deref(),
    };
    // 12f F2: pass daemon.toml-sourced credentials as
    // overrides. Empty fields fall through to env in the
    // resolver — preserves the local-workstation case
    // (daemon launched from a shell with CM_API_URL /
    // CM_API_TOKEN exported, no daemon.toml on disk).
    let (api_url_cfg, api_token_cfg) = {
        let st = state_arc.lock().expect("state mutex");
        (
            st.config.api_url.clone(),
            st.config.api_token.clone(),
        )
    };
    let api_url_override = if api_url_cfg.is_empty() {
        None
    } else {
        Some(api_url_cfg.as_str())
    };
    let api_token_override = if api_token_cfg.is_empty() {
        None
    } else {
        Some(api_token_cfg.as_str())
    };
    match crate::planning_client::propose_task(
        &req,
        api_url_override,
        api_token_override,
    ) {
        Ok(task) => Ok(task),
        Err(e) => Err(e.to_method_err()),
    }
}

// ============================================================
// workflow_transition / workflow_done (10d-2b)
// ============================================================
//
// Pre-2b: the Python MCP `workflow_transition` / `workflow_done`
// tools (`mcp_server/server.py::_append_event`) wrote
// `~/.cm/workflow-runs/<id>/events.jsonl` DIRECTLY via file I/O —
// no socket, no caller validation, just a write driven by the
// agent's `CM_WORKFLOW_RUN_ID` + `CM_ROLE` env. The TUI's
// `workflow/controller.rs` tail loop reacted.
//
// 2b flips the WRITER to the daemon: Python tools become
// `control_client.call("workflow_transition", ...)`, the per-
// method resolver routes the call to the daemon socket (added to
// `DAEMON_METHODS`), and the daemon's handlers below assemble the
// event from the RPC params and write via the 2a
// `WorkflowEventsWriter` (Operator-only-by-mistake auth path
// avoided — these are Session-callable AND Operator-callable;
// any participant in a workflow run can call). The TUI's tail
// loop reacts unchanged.
//
// What 2b explicitly does NOT do:
//   - Participant validation (lookup_session_any → match
//     workflow_run_id+role). The event's `role` field still
//     comes from the agent's `CM_ROLE` env via the RPC params,
//     same trust shape as today's `_append_event`. Validation
//     lands with 10d-2c when daemon owns the workflow state.
//   - State mutation. Daemon doesn't keep workflow_runs in sync;
//     that's still TUI-driven from tail observation. 2c relocates.
//
// Auth: any caller — Session OR Operator. Today's `_append_event`
// trusted any caller (just the agent's env vars). Daemon parity
// with that — Session-caller rejection would break TUI-spawned
// participants too. 2c adds the participant check via
// `lookup_session_any` once workflow_runs is daemon-side.

#[derive(serde::Deserialize)]
pub struct WorkflowTransitionParams {
    pub to: String,
    pub prompt: String,
    pub run_id: String,
    pub role: String,
    /// 10d-2c-2-2-b F2: optional precondition guard. When set,
    /// the closure validates `run.active_role == expected_from`
    /// INSIDE the flock and returns Conflict on mismatch. The
    /// daemon's `cm-workflow-poller` passes
    /// `Some(snapshot_active_role)` so a state.json change
    /// between snapshot and apply (e.g., a concurrent MCP
    /// `workflow_transition` from an agent) aborts the
    /// poller's call without mutating state.
    ///
    /// MCP-direct callers (TUI launch, MCP `start_workflow`)
    /// leave this `None` — the existing Session-caller auth
    /// check (`active_role == caller's bound workflow_role`)
    /// is the implicit precondition there. Operators (this is
    /// the only path that bypasses Session-auth) get the
    /// explicit precondition via this field.
    #[serde(default)]
    pub expected_from: Option<String>,
    /// 10d-2c-2-2-b F3: optional trigger discriminator. When
    /// `Some("static_idle")`, the event's `args.trigger` field
    /// carries it; the TUI tail reads `args.trigger` and
    /// constructs `TriggerKind::StaticIdle{from_role}` for the
    /// history entry. Default (None) → TUI uses
    /// `TriggerKind::McpTransition{...}` as before.
    ///
    /// Daemon poller sets `Some("static_idle")`; MCP callers
    /// don't set it.
    #[serde(default)]
    pub trigger: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct WorkflowDoneParams {
    pub reason: String,
    pub run_id: String,
    pub role: String,
}

fn now_unix_f64() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn new_event_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 10d-2c-2-2-b round-4 F2: daemon-internal capture of the
/// outgoing role's last assistant message for the closing
/// history entry. Mirrors the former TUI controller's
/// `fire_transition` capture but reads
/// state via the daemon's session registry instead of
/// `App.workspaces`.
///
/// Returns None if any lookup fails — same shape as TUI's
/// `if let Some((sid, wt))` block. Reasons for None include:
/// run not on disk yet, no active role, no session bound for
/// the active role, no worktree_path on the workspace, or no
/// readable transcript (race with agent startup).
///
/// Single-source contract: this function uses the same
/// `crate::workflow::transcript::last_message` that TUI's
/// resolver and `fire_transition` use. Engine derivation
/// mirrors the TUI rule (`engine_for_session_type(session_type)`).
fn capture_outgoing_last_message(
    state: &DaemonState,
    run_id: &str,
) -> Option<String> {
    let run = crate::workflow::run::load_one(run_id)?;
    let active = run.active_role.as_deref()?;
    let binding = run.role_sessions.get(active)?;
    let sid = binding.current_session_id.as_deref()?;
    // Review-round-5 F2: use the shared three-tier fallback
    // (uid → daemon-tag → TUI-tag with task_id derivation).
    // Pre-r5-r5 this function did tag-based lookup only; a
    // daemon-owned session without `set_workflow_context` tags
    // would fail the lookup and `last_message` would be None on
    // the closing history entry — permanent audit-data loss.
    let ctx = crate::workflow::poller::resolve_role_session_context(
        state, &run, active,
    )?;
    let worktree = state
        .workspaces
        .get(&ctx.workspace_id)
        .and_then(|ws| ws.worktree_path.clone())?;
    let engine = match ctx.session_type.as_str() {
        "codex" => crate::workflow::toml_schema::Engine::Codex,
        _ => crate::workflow::toml_schema::Engine::ClaudeCode,
    };
    crate::workflow::transcript::last_message(&engine, &worktree, sid)
}

pub fn workflow_transition(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    params: &Value,
) -> MethodResult {
    let p: WorkflowTransitionParams = serde_json::from_value(params.clone())
        .map_err(|e| (
            ErrorCode::InvalidParams,
            format!("workflow_transition params: {}", e),
        ))?;
    if p.run_id.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "workflow_transition: 'run_id' is required (set CM_WORKFLOW_RUN_ID \
             on the spawning side)".into(),
        ));
    }
    if p.to.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "workflow_transition: 'to' (target role) is required".into(),
        ));
    }
    let role_for_event = if p.role.trim().is_empty() {
        "unknown".to_string()
    } else {
        p.role.clone()
    };

    // Pre-fetch the caller's workflow_run_id + workflow_role
    // from `lookup_session_any` BEFORE entering the flock.
    // The auth comparison happens INSIDE the flock against the
    // loaded run.active_role to avoid TOCTOU. Operators bypass
    // the check (operator is trusted by definition).
    let session_workflow_info: Option<(Option<String>, Option<String>)> =
        if let Caller::Session(uid) = caller {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            state
                .lookup_session_any(&uid.session_uid)
                .map(|s| (s.workflow_run_id, s.workflow_role))
        } else {
            None
        };
    let is_session_caller = matches!(caller, Caller::Session(_));

    // 10d-2c-2-2-b round-4 F2: capture the outgoing role's
    // last assistant message BEFORE `try_modify` so the closure
    // can pass it to `close_active_role(Some(last_message))`.
    // Pre-fix every daemon-routed transition (both Session-
    // caller MCP and Operator-caller poller) called
    // `close_active_role(None)` and the TUI tail's history
    // append never set `last_message` either — permanent data
    // loss in the audit UI for daemon-routed runs.
    //
    // Daemon-internal capture: no caller param, no parallel-impl
    // drift surface. Reads disk + state.sessions to find the
    // active role's transcript and engine, then calls
    // `transcript::last_message` (same function the TUI's
    // `fire_transition` uses for TUI-direct static fires).
    //
    // If any lookup fails (run not on disk yet, role unbound,
    // worktree unknown), capture is None — same fall-through
    // shape as TUI's `fire_transition` (its `if let Some((sid,
    // wt))` block).
    let captured_last_message: Option<String> = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        capture_outgoing_last_message(&state, &p.run_id)
    };

    // 10d-2c-1 review round-1 (F1) → round-4 → round-5 (F2):
    // event write happens AFTER `try_modify` (round-2 shape) with
    // a bounded retry. Sequence:
    //   1. `try_modify` runs auth + status + target-role + the
    //      round-5 idempotency check, then mutates state.json
    //      under flock.
    //   2. `append_event_with_retry` writes the event with up to
    //      3 attempts (50ms / 100ms / 200ms backoff) before
    //      bubbling Err.
    //
    // Failure mode if `append_event_with_retry` exhausts: state
    // advanced, no event on disk. The TUI tail observes nothing
    // for this transition; the workflow stalls on the daemon's
    // side until either (a) a caller-side retry lands and the
    // round-5 idempotency check skips the mutation while
    // re-appending the event, or (b) operator notices the loud
    // log. Document deflection of this two-file atomicity class
    // lives in `daemon/NOTES.md` ("Rejected findings (10d-2c-1)").
    //
    // The reverse failure mode (round-4 shape: event-inside-closure)
    // had an event-no-state-save corner that wedged the TUI
    // delivery branch when the closure-side append failed; this
    // shape's failure ("state advances + event missing") is
    // strictly more recoverable, since state is the load-bearing
    // record (TUI's tail re-loads `state.json` periodically and
    // its workflow loop's idempotency catches the missing
    // event). Choosing the lesser-evil mode is explicit per the
    // round-5 deflection.
    // 10d-2c-1 review round-7 (F2): `from_role` is captured
    // INSIDE the closure (pre-mutation, under flock) so the TUI
    // tail receives the authoritative outgoing role on the
    // event. Built outside the closure first; the closure
    // writes the captured value via the shared `Arc<Mutex>`
    // below. We assign `event.from_role` AFTER `try_modify`
    // returns Ok but BEFORE `append_event_with_retry`.
    // 10d-2c-2-2-b F3: event's `args.trigger` carries the
    // poller-source discriminator. TUI tail reads it to
    // construct `TriggerKind::StaticIdle` instead of the default
    // `McpTransition`. Absent for MCP-direct callers.
    //
    // Round-4 F1 (security): only honor `trigger` from Operator
    // callers. Pre-fix a Session caller (MCP agent) could send
    // `{"trigger":"static_idle"}` to make their dynamic
    // transition look like a static idle one — dropping
    // prompt/event_id audit fields from the TUI tail's
    // history append. The daemon-poller (the only legitimate
    // source for static_idle) uses Operator caller, so this
    // narrows the override to that path.
    let trigger_str: Option<&str> = match caller {
        Caller::Operator(_) => p.trigger.as_deref(),
        Caller::Session(_) => None,
    };
    let mut args = json!({"to": p.to, "prompt": p.prompt});
    if let Some(t) = trigger_str {
        args["trigger"] = serde_json::Value::String(t.to_string());
    }
    let mut event = crate::workflow::events::Event {
        id: new_event_id(),
        ts: now_unix_f64(),
        run_id: p.run_id.clone(),
        role: role_for_event.clone(),
        tool: "workflow_transition".to_string(),
        args,
        source: "daemon".to_string(),
        from_role: None,
        iteration: 0,
    };

    // 10d-2c-1 + review round-1 (F1 + F3 + P1 #3 + Option A) +
    // round-6 (F2 rollback): state.json RMW under flock(2), with
    // auth + status + target-role validation INSIDE the closure
    // so they see the latest authoritative `active_role` +
    // `status` (no TOCTOU). Mutation is **minimal**: close the
    // outgoing role's history entry, bump iteration, set new
    // active_role. No history.push (Option A — deferred to TUI
    // tail).
    //
    // Round-6 rollback: capture the pre-mutation state INSIDE
    // the closure (under flock, after validation passes) and
    // expose it to the outer scope via an Arc<Mutex<Option<>>>.
    // If `append_event_with_retry` exhausts (state advanced but
    // event missing), the post-call restore writes
    // `rollback_state` back, so an external caller-side retry
    // sees the original pre-mutation state and the daemon's
    // auth check still matches the caller's bound role.
    //
    // Round-5's idempotency short-circuit (`active_role == to`)
    // is REMOVED — rollback restores the pre-mutation state, so
    // there's no "state already advanced" condition the
    // closure needs to absorb. If the daemon ever sees
    // `active_role == to` on entry under non-rollback
    // conditions, some other process advanced it; that's a
    // Conflict, not an idempotent success.
    //
    // `last_message` capture is NOT done here — needs session+
    // worktree lookup. Deferred to 10d-2c-2.
    // Phase 3 (doc/daemon-side-workflow-orchestration.md): record a durable
    // `pending_activation` on the run in the SAME flock mutation that advances
    // `active_role`, so the daemon delivery drainer can finalize the hand-off
    // (render prompt, fresh reset, append history, deliver) WITHOUT re-reading
    // events.jsonl. Captured values for the closure:
    //   - `to_role_is_fresh`: whether the incoming role is `Context::Fresh`
    //     (drives the /clear + sid-rebind path). Looked up from the loaded
    //     workflow definition; default false (persistent) if unavailable.
    //   - `is_static_fire`: poller fires set trigger="static_idle" (Operator
    //     caller); MCP fires don't. Selects StaticIdle vs McpTransition.
    //   - the raw prompt / event id / target role for the record + trigger.
    let to_role_is_fresh: bool = {
        let wf_name = crate::workflow::run::load_one(&p.run_id).map(|r| r.workflow_name);
        let state = state_arc.lock().unwrap_or_else(|pp| pp.into_inner());
        wf_name
            .and_then(|name| state.workflow_definition(&name))
            .and_then(|wf| wf.roles.get(&p.to))
            .map(|r| matches!(r.context, crate::workflow::toml_schema::Context::Fresh))
            .unwrap_or(false)
    };
    let pa_raw_prompt = p.prompt.clone();
    let pa_is_static = matches!(trigger_str, Some("static_idle"));
    let pa_event_id = event.id.clone();
    let pa_target_role = p.to.clone();

    let to_role = p.to.clone();
    let run_id_for_closure = p.run_id.clone();
    // 10d-2c-2-2-b F2: capture for closure use. Operator-callers
    // (currently only the daemon-poller) set this; Session-callers
    // leave it None (their auth check above subsumes it).
    let expected_from = p.expected_from.clone();
    let rollback_state: Arc<Mutex<Option<crate::workflow::run::WorkflowRun>>> =
        Arc::new(Mutex::new(None));
    let rollback_for_closure = rollback_state.clone();
    // 10d-2c-1 review round-7 (F2): captures the
    // pre-mutation `active_role` for the Event's `from_role`
    // field. Same shape as `rollback_state` — exposed to the
    // outer scope via Arc<Mutex<>>.
    let captured_from_role: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_from_role_for_closure = captured_from_role.clone();
    // 10d-2c-1 review round-15: capture POST-mutation iteration
    // inside the closure (under flock, after `iteration += 1`).
    // The TUI's history append uses this per-event value so
    // queued events record their actual activation iteration —
    // pre-r15 it read state.json's current `iteration` which
    // gave the LATEST value to every queued event.
    let captured_iteration: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let captured_iteration_for_closure = captured_iteration.clone();
    // 10d-2c-2-2-b round-4 F2: move pre-captured last_message
    // into closure scope. Cloned because the outer `event`
    // builder doesn't need it (the value lives on the
    // outgoing history entry, not the event payload).
    let captured_last_message_for_closure = captured_last_message.clone();
    let outcome = crate::workflow::run::try_modify(
        &p.run_id,
        move |run| -> Result<(), (ErrorCode, String)> {
            // P1 #3 auth: Session callers must be the
            // active-role's participant. Operators bypass.
            if is_session_caller {
                let active = run.active_role.as_deref();
                let allowed = session_workflow_info
                    .as_ref()
                    .map(|(wf_id, wf_role)| {
                        wf_id.as_deref() == Some(&run_id_for_closure)
                            && wf_role.as_deref() == active
                    })
                    .unwrap_or(false);
                if !allowed {
                    return Err((
                        ErrorCode::Unauthorized,
                        // Message names the run_id but NOT the
                        // active role — don't leak workflow state
                        // to non-participants.
                        format!(
                            "workflow_transition: caller is not a participant of run {}",
                            run_id_for_closure,
                        ),
                    ));
                }
            }
            // 10d-2c-2-2-b F2: expected_from precondition. Operator
            // callers bypass the Session-caller auth check above,
            // which would otherwise serve as the "caller is in
            // the right state" guard. Without this, an Operator
            // (e.g., the daemon's workflow poller) calling with
            // a stale snapshot would mutate state regardless of
            // whether the active_role still matches what the
            // caller observed. Session callers don't set this
            // — their auth check above subsumes the precondition.
            if let Some(expected) = expected_from.as_deref() {
                let actual = run.active_role.as_deref();
                if actual != Some(expected) {
                    return Err((
                        ErrorCode::Conflict,
                        format!(
                            "workflow_transition: expected_from {:?} does not match \
                             current active_role {:?} for run {} — stale snapshot, \
                             retry from current state",
                            expected, actual, run_id_for_closure,
                        ),
                    ));
                }
            }
            // F3 (status): only Running runs accept transitions.
            // Paused/Done/Detached are conflicts — the run is in
            // a state where firing a transition would corrupt
            // semantics (e.g., post-Done activation history).
            if !matches!(run.status, crate::workflow::run::RunStatus::Running) {
                return Err((
                    ErrorCode::Conflict,
                    format!(
                        "workflow_transition: run {} is not Running (status={:?}); \
                         transitions only fire on Running runs",
                        run_id_for_closure, run.status,
                    ),
                ));
            }
            // F3 (target role): target must be a known role in
            // the workflow's role bindings. Catches typos and
            // protects against an agent inventing role names.
            if !run.role_sessions.contains_key(&to_role) {
                let mut valid: Vec<&str> = run
                    .role_sessions
                    .keys()
                    .map(|s| s.as_str())
                    .collect();
                valid.sort();
                return Err((
                    ErrorCode::Conflict,
                    format!(
                        "workflow_transition: target role {:?} not declared in run {} \
                         (valid roles: {:?})",
                        to_role, run_id_for_closure, valid,
                    ),
                ));
            }
            // Round-6 (F2) rollback: snapshot pre-mutation state
            // AFTER validation but BEFORE mutation. The outer
            // scope reads this via `rollback_state.lock()` if
            // append_event_with_retry exhausts and we need to
            // restore.
            *rollback_for_closure.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(run.clone());
            // Round-7 (F2): capture the pre-mutation
            // `active_role` so the Event carries the
            // authoritative outgoing role for the TUI tail.
            *captured_from_role_for_closure
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = run.active_role.clone();
            // 10d-2c-2-2-b round-4 F2: pass the pre-captured
            // last_message so the closing history entry carries
            // the outgoing role's last assistant turn. Pre-fix
            // every daemon-routed transition called
            // `close_active_role(None)` and the audit UI showed
            // `(active)` for what should have been a closed
            // role with its last message recorded.
            // Phase 3: capture the pre-mutation active_role for the
            // pending_activation trigger BEFORE `active_role` is reassigned.
            let pa_from_role = run.active_role.clone().unwrap_or_default();
            run.close_active_role(captured_last_message_for_closure.clone());
            run.iteration += 1;
            run.active_role = Some(to_role);
            // Round-15: capture POST-mutation iteration so the
            // Event carries the per-event activation iteration
            // for the TUI tail's history append.
            *captured_iteration_for_closure
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = run.iteration;
            // Phase 3: record the durable pending-activation alongside the
            // close/iteration/active_role mutation. NO history append, NO
            // render here — finalization (the drainer) does both, reading this
            // record. The TriggerKind is reconstructed from the persisted
            // metadata, so a restart never re-reads events.jsonl.
            let pa_trigger = if pa_is_static {
                crate::workflow::run::TriggerKind::StaticIdle {
                    from_role: pa_from_role.clone(),
                }
            } else {
                crate::workflow::run::TriggerKind::McpTransition {
                    from_role: pa_from_role.clone(),
                    prompt: pa_raw_prompt.clone(),
                    event_id: pa_event_id.clone(),
                }
            };
            run.pending_activation = Some(crate::workflow::run::PendingActivation {
                activation_id: run.iteration as u64,
                target_role: pa_target_role.clone(),
                iteration: run.iteration,
                trigger: pa_trigger,
                raw_prompt: pa_raw_prompt.clone(),
                verbatim: false,
                needs_fresh_reset: to_role_is_fresh,
                is_initial: false,
                phase: crate::workflow::run::ActivationPhase::Queued,
                rendered_prompt: None,
                pre_clear_snapshot: None,
                enter_fire_at_ms: None,
            });
            Ok(())
        },
    );
    let updated = match outcome {
        crate::workflow::run::TryModifyOutcome::Ok(r) => r,
        crate::workflow::run::TryModifyOutcome::Aborted(e) => return Err(e),
        crate::workflow::run::TryModifyOutcome::Persist(
            crate::workflow::run::PersistError::Io(io_err),
        ) if io_err.kind() == std::io::ErrorKind::NotFound => {
            return Err((
                ErrorCode::NotFound,
                format!(
                    "workflow_transition: workflow run {} has no state.json on disk \
                     (was it ever launched?)",
                    p.run_id,
                ),
            ));
        }
        crate::workflow::run::TryModifyOutcome::Persist(
            crate::workflow::run::PersistError::Io(io_err),
        ) if io_err.kind() == std::io::ErrorKind::InvalidInput => {
            // F2 (round 3): run_id validation surfaces as
            // InvalidInput from `run::try_modify` — translate to
            // InvalidParams for the caller.
            return Err((
                ErrorCode::InvalidParams,
                format!("workflow_transition: invalid run_id: {}", io_err),
            ));
        }
        crate::workflow::run::TryModifyOutcome::Persist(other) => {
            return Err((
                ErrorCode::Internal,
                format!("workflow_transition: state.json mutation failed: {}", other),
            ));
        }
    };

    // Round-7 (F2): copy the captured pre-mutation
    // `from_role` onto the event before it lands on disk. The
    // TUI tail's daemon-routed handler reads this field for
    // `McpTransition.from_role`; deriving from the post-mutation
    // in-memory `active_role` would record the WRONG outgoing
    // role (`to`, not the actual previous role).
    event.from_role = captured_from_role
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    // Round-15: copy POST-mutation iteration for the TUI tail's
    // per-event history append.
    event.iteration = *captured_iteration
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    // Round-6 (F2): append event with bounded retry. If
    // exhausted, ROLL BACK the state.json mutation to the
    // pre-mutation snapshot captured inside the closure so the
    // external caller-side retry sees the original state and
    // the daemon's auth check still matches the caller's bound
    // role. Pre-fix (round-5) state stayed advanced; an external
    // retry would hit Unauthorized because active_role had
    // moved past the caller's role. NOTES.md captures the
    // rollback failure mode (rollback-save-also-fails) as
    // unrecoverable.
    //
    // Phase 2 slice 11a: pre-clone the broadcaster Arc so the
    // post-write broadcast runs lock-free.
    let watcher = state_arc
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .workflow_event_watcher
        .clone();
    if let Err(e) = append_event_with_retry(&event, &watcher) {
        eprintln!(
            "cm-daemon: workflow_transition({} → {}): state.json advanced but \
             append_event failed after retries: {} — rolling back state to \
             pre-mutation snapshot so caller retry sees the original state. \
             See NOTES.md \"Rejected findings (10d-2c-1)\" for the rollback \
             failure mode.",
            event.run_id, p.to, e,
        );
        // Take the pre-mutation snapshot out of the Arc; restore
        // it to disk under flock.
        let snapshot = rollback_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(snap) = snapshot {
            // 10d-2c-1 review round-12 (F1): field-targeted
            // rollback. Pre-r12 the rollback did `*r = snap`
            // wholesale, clobbering any TUI-owned field updates
            // that landed in the window between mutation and
            // rollback (sync_role_session_ids, pause/stop,
            // events_offset advance from tail, role_baselines
            // for fresh resets). Round-12 restores ONLY the
            // daemon-owned fields `workflow_transition`
            // actually changed:
            //   - active_role  (mutation flipped to `to`)
            //   - iteration    (mutation bumped by 1)
            // TUI-owned fields stay at whatever value they hold
            // on disk now (which may include TUI updates that
            // landed during the retry window). Symmetric to the
            // round-6 F1 fix on the TUI side ("TUI wholesale
            // save clobbers daemon state") — same principle:
            // each writer touches only its own fields.
            if let Err(re) = crate::workflow::run::modify(&p.run_id, move |r| {
                r.active_role = snap.active_role.clone();
                r.iteration = snap.iteration;
                // Phase 3: the mutation recorded `pending_activation` (a
                // daemon-owned field), so the field-targeted rollback must
                // restore it too — otherwise an exhausted append leaves an
                // orphan record pointing at a role that is no longer active,
                // which the drainer would deliver against. Restores the
                // pre-mutation value (normally None).
                r.pending_activation = snap.pending_activation.clone();
                // 10d-2c-1 review round-13: `close_active_role`
                // (called by the daemon's mutation just before
                // setting `active_role = to`) set the last
                // history entry's `deactivated_at` and
                // `last_message`. Round-12 missed restoring
                // those; post-rollback the run had
                // `active_role = worker` (rolled back) but
                // worker's history entry still showed
                // `deactivated_at: Some(...)` — inconsistent,
                // and `close_active_role` is idempotent so a
                // caller retry couldn't repair it. Restore by
                // matching `(role, iteration)` against
                // `snap.history.last()` — the active-at-
                // snapshot-time entry. Don't restore the whole
                // history Vec (TUI may have appended entries
                // during the retry window).
                if let Some(snap_active) = snap.history.last() {
                    if let Some(disk_entry) = r
                        .history
                        .iter_mut()
                        .find(|h| h.role == snap_active.role
                            && h.iteration == snap_active.iteration)
                    {
                        disk_entry.deactivated_at = snap_active.deactivated_at;
                        disk_entry.last_message = snap_active.last_message.clone();
                    }
                }
            }) {
                // Rollback-save-also-fails. Log loudly; the run
                // is now in an inconsistent state and needs
                // manual recovery (e.g., remove
                // `~/.cm/workflow-runs/<run_id>/state.json.lock`
                // and edit `state.json` by hand). The original
                // event-write failure is the primary error to
                // return to the caller; the rollback failure is
                // a secondary log line.
                eprintln!(
                    "cm-daemon: workflow_transition({}): ROLLBACK ALSO FAILED: \
                     {} — state.json on disk reflects the (uncommitted) \
                     post-mutation shape with no matching event; manual \
                     recovery required",
                    p.run_id, re,
                );
            }
        } else {
            // Closure didn't capture a snapshot (e.g., returned
            // Err before the snapshot statement). Nothing to
            // restore; the on-disk state should already be
            // untouched.
            eprintln!(
                "cm-daemon: workflow_transition({}): no pre-mutation \
                 snapshot captured (closure returned early); on-disk \
                 state should be untouched",
                p.run_id,
            );
        }
        return Err((
            ErrorCode::Internal,
            format!("workflow_transition: failed to append event: {}", e),
        ));
    }

    // Refresh the daemon's in-memory cache.
    {
        let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        state.workflow_runs.insert(updated.run_id.clone(), updated);
    }

    Ok(json!({
        "ok": true,
        "event_id": event.id,
        "run_id": event.run_id,
    }))
}

/// 10d-2c-1 review round-5 (F2): bounded retry around
/// `WorkflowEventsWriter::append_event`. Three attempts with
/// 50ms / 100ms / 200ms backoff between them. The caller (the
/// workflow handlers) uses this AFTER `try_modify` succeeds —
/// state.json is already durable by the time we get here.
///
/// Why 3 attempts: enough to ride out a transient flush/fs hiccup
/// (the kind that the kernel resolves on its own), few enough to
/// not stretch the JSON-RPC round-trip past operator expectations
/// (~350ms total worst case). The backoff is short for the same
/// reason — this is a foreground RPC, not a background job.
///
/// Persistent failures (full disk, read-only fs, permissions
/// drift) won't recover via retry; they surface as Err to the
/// caller and the loud log line documents the inconsistency.
/// See `daemon/NOTES.md` "Rejected findings (10d-2c-1)" for the
/// deflection rationale.
fn append_event_with_retry(
    event: &crate::workflow::events::Event,
    watcher: &Arc<crate::workflow::events::WorkflowEventWatcher>,
) -> std::io::Result<()> {
    const BACKOFFS_MS: &[u64] = &[50, 100, 200];
    let mut attempt: usize = 0;
    loop {
        // 11e (Option B): broadcast is now hooked inside
        // `WorkflowEventsWriter` itself — every successful writer
        // path is automatically covered (including a future
        // daemon-routed `workflow_reject_finding`), without each
        // call site having to remember to broadcast. The post-
        // write ordering invariant still holds: the broadcast
        // fires AFTER the disk fsync inside `append_event_inner`.
        match crate::workflow::events::WorkflowEventsWriter::append_event_and_broadcast(
            event, watcher,
        ) {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempt += 1;
                if attempt >= BACKOFFS_MS.len() + 1 {
                    return Err(e);
                }
                // Log every retry so operators can correlate
                // transient failure clusters even when the
                // final attempt succeeds.
                eprintln!(
                    "cm-daemon: append_event(run={}, event={}) attempt {} \
                     failed: {} (retrying after {}ms)",
                    event.run_id,
                    event.id,
                    attempt,
                    e,
                    BACKOFFS_MS[attempt - 1],
                );
                std::thread::sleep(std::time::Duration::from_millis(
                    BACKOFFS_MS[attempt - 1],
                ));
            }
        }
    }
}

pub fn workflow_done(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    params: &Value,
) -> MethodResult {
    let p: WorkflowDoneParams = serde_json::from_value(params.clone())
        .map_err(|e| (
            ErrorCode::InvalidParams,
            format!("workflow_done params: {}", e),
        ))?;
    if p.run_id.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "workflow_done: 'run_id' is required".into(),
        ));
    }
    let role_for_event = if p.role.trim().is_empty() {
        "unknown".to_string()
    } else {
        p.role.clone()
    };

    // Pre-fetch caller info for the auth check (see
    // `workflow_transition` for the rationale).
    let session_workflow_info: Option<(Option<String>, Option<String>)> =
        if let Caller::Session(uid) = caller {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            state
                .lookup_session_any(&uid.session_uid)
                .map(|s| (s.workflow_run_id, s.workflow_role))
        } else {
            None
        };
    let is_session_caller = matches!(caller, Caller::Session(_));

    // F1 (round-1) + F2 (round-5 revert): event in memory now,
    // appended AFTER try_modify with bounded retry. Same
    // rationale as `workflow_transition`.
    //
    // Round-7 (F2): `workflow_done` events carry `from_role:
    // None` — the active role is being torn down, no "next
    // role" semantics apply. The TUI tail's daemon-routed Done
    // handler doesn't need the field. Pre-round-7 events on
    // disk also have `None` via `#[serde(default)]`.
    let mut event = crate::workflow::events::Event {
        id: new_event_id(),
        ts: now_unix_f64(),
        run_id: p.run_id.clone(),
        role: role_for_event,
        tool: "workflow_done".to_string(),
        args: json!({"reason": p.reason}),
        source: "daemon".to_string(),
        from_role: None,
        iteration: 0,
    };

    // 10d-2c-1 + review round-1 (F1 + F3 + P1 #3) + round-6 (F2
    // rollback): same auth + status validation shape as
    // `workflow_transition`. Round-5's idempotency
    // short-circuit (`status == Done`) is REMOVED — rollback
    // restores pre-mutation state on event-write exhaustion, so
    // a caller retry sees Running again and re-runs the full
    // RMW. If `status == Done` is observed on entry without a
    // rollback, some other process set it; that's a Conflict.
    let reason = p.reason.clone();
    let run_id_for_closure = p.run_id.clone();
    let rollback_state: Arc<Mutex<Option<crate::workflow::run::WorkflowRun>>> =
        Arc::new(Mutex::new(None));
    let rollback_for_closure = rollback_state.clone();
    // Round-15: capture post-mutation iteration (workflow_done
    // doesn't bump iteration, but the field surfaces the value
    // for parity with workflow_transition).
    let captured_iteration: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let captured_iteration_for_closure = captured_iteration.clone();
    let outcome = crate::workflow::run::try_modify(
        &p.run_id,
        move |run| -> Result<(), (ErrorCode, String)> {
            if is_session_caller {
                let active = run.active_role.as_deref();
                let allowed = session_workflow_info
                    .as_ref()
                    .map(|(wf_id, wf_role)| {
                        wf_id.as_deref() == Some(&run_id_for_closure)
                            && wf_role.as_deref() == active
                    })
                    .unwrap_or(false);
                if !allowed {
                    return Err((
                        ErrorCode::Unauthorized,
                        format!(
                            "workflow_done: caller is not a participant of run {}",
                            run_id_for_closure,
                        ),
                    ));
                }
            }
            if !matches!(run.status, crate::workflow::run::RunStatus::Running) {
                return Err((
                    ErrorCode::Conflict,
                    format!(
                        "workflow_done: run {} is not Running (status={:?}); \
                         workflow_done only fires on Running runs",
                        run_id_for_closure, run.status,
                    ),
                ));
            }
            // Round-6 (F2) rollback: snapshot pre-mutation state
            // AFTER validation but BEFORE mutation.
            *rollback_for_closure.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(run.clone());
            run.close_active_role(None);
            run.active_role = None;
            run.status = crate::workflow::run::RunStatus::Done;
            run.done_reason = Some(reason);
            // A terminal run carries no in-flight hand-off: drop any
            // pending_activation so the drainer has nothing to resume and the
            // on-disk record isn't left pointing at a hand-off that can never
            // complete (parity with WorkflowRun::mark_done).
            run.pending_activation = None;
            // Round-15: capture iteration (workflow_done doesn't
            // bump it, but parity with workflow_transition).
            *captured_iteration_for_closure
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = run.iteration;
            Ok(())
        },
    );
    let updated = match outcome {
        crate::workflow::run::TryModifyOutcome::Ok(r) => r,
        crate::workflow::run::TryModifyOutcome::Aborted(e) => return Err(e),
        crate::workflow::run::TryModifyOutcome::Persist(
            crate::workflow::run::PersistError::Io(io_err),
        ) if io_err.kind() == std::io::ErrorKind::NotFound => {
            return Err((
                ErrorCode::NotFound,
                format!(
                    "workflow_done: workflow run {} has no state.json on disk",
                    p.run_id,
                ),
            ));
        }
        crate::workflow::run::TryModifyOutcome::Persist(
            crate::workflow::run::PersistError::Io(io_err),
        ) if io_err.kind() == std::io::ErrorKind::InvalidInput => {
            return Err((
                ErrorCode::InvalidParams,
                format!("workflow_done: invalid run_id: {}", io_err),
            ));
        }
        crate::workflow::run::TryModifyOutcome::Persist(other) => {
            return Err((
                ErrorCode::Internal,
                format!("workflow_done: state.json mutation failed: {}", other),
            ));
        }
    };

    // Round-15: copy post-mutation iteration onto the event.
    event.iteration = *captured_iteration
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    // Round-6 (F2): same rollback shape as
    // `workflow_transition` — restore pre-mutation snapshot if
    // event-write exhausts.
    //
    // Phase 2 slice 11a: broadcaster Arc pre-cloned for lock-free
    // post-write broadcast.
    let watcher = state_arc
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .workflow_event_watcher
        .clone();
    if let Err(e) = append_event_with_retry(&event, &watcher) {
        eprintln!(
            "cm-daemon: workflow_done(run={}): state.json advanced but \
             append_event failed after retries: {} — rolling back state to \
             pre-mutation snapshot",
            event.run_id, e,
        );
        let snapshot = rollback_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(snap) = snapshot {
            // 10d-2c-1 review round-12 (F1): field-targeted
            // rollback. `workflow_done` mutated active_role
            // (→ None), status (→ Done), and done_reason
            // (→ Some(reason)). Restore exactly those three.
            // TUI-owned fields stay at whatever value they
            // currently hold (the TUI may have written
            // events_offset / role_sessions / etc. concurrently
            // during the event-write retry window). See
            // `workflow_transition`'s round-12 commentary for
            // the full rationale.
            if let Err(re) = crate::workflow::run::modify(&p.run_id, move |r| {
                r.active_role = snap.active_role.clone();
                r.status = snap.status.clone();
                r.done_reason = snap.done_reason.clone();
                // 10d-2c-1 review round-13: restore the
                // deactivated-at-snapshot history entry (the
                // one `close_active_role` mutated). Same shape
                // as the `workflow_transition` rollback.
                if let Some(snap_active) = snap.history.last() {
                    if let Some(disk_entry) = r
                        .history
                        .iter_mut()
                        .find(|h| h.role == snap_active.role
                            && h.iteration == snap_active.iteration)
                    {
                        disk_entry.deactivated_at = snap_active.deactivated_at;
                        disk_entry.last_message = snap_active.last_message.clone();
                    }
                }
            }) {
                eprintln!(
                    "cm-daemon: workflow_done({}): ROLLBACK ALSO FAILED: \
                     {} — manual recovery required",
                    p.run_id, re,
                );
            }
        }
        return Err((
            ErrorCode::Internal,
            format!("workflow_done: failed to append event: {}", e),
        ));
    }

    {
        let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        state.workflow_runs.insert(updated.run_id.clone(), updated);
    }

    // GC the per-run writer-lock entry now that the run is done —
    // no more append_event calls expected for this run_id. Bounds
    // the static `WRITER_LOCKS` HashMap that would otherwise grow
    // monotonically for the daemon's lifetime (post-review #10).
    crate::workflow::events::WorkflowEventsWriter::release_writer_lock(&event.run_id);

    Ok(json!({
        "ok": true,
        "event_id": event.id,
        "run_id": event.run_id,
    }))
}

// ============================================================
// workflow_reject_finding (11e prerequisite)
// ============================================================
//
// 11e prerequisite: route Python `_append_event` writes through
// the daemon so Option B's broadcaster-in-WorkflowEventsWriter
// hook covers them. Pre-11e the Python MCP tool wrote directly
// to events.jsonl; post-routing it goes through this RPC which
// uses `append_event_and_broadcast`.
//
// Auth shape mirrors `workflow_transition`/`workflow_done`:
// Session callers must be participants of the run (their
// `workflow_run_id` must match `params.run_id` and their
// `workflow_role` must equal the run's `active_role`). Operator
// callers bypass.

#[derive(serde::Deserialize)]
pub struct WorkflowRejectFindingParams {
    pub run_id: String,
    pub role: String,
    pub text: String,
}

pub fn workflow_reject_finding(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    params: &Value,
) -> MethodResult {
    let p: WorkflowRejectFindingParams = serde_json::from_value(params.clone())
        .map_err(|e| (
            ErrorCode::InvalidParams,
            format!("workflow_reject_finding params: {}", e),
        ))?;
    if p.run_id.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "workflow_reject_finding: 'run_id' is required".into(),
        ));
    }
    let trimmed_text = p.text.trim().to_string();
    if trimmed_text.is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "workflow_reject_finding: 'text' is required (non-empty)".into(),
        ));
    }
    let role_for_event = if p.role.trim().is_empty() {
        "unknown".to_string()
    } else {
        p.role.clone()
    };

    // Pre-fetch caller workflow info for auth.
    let session_workflow_info: Option<(Option<String>, Option<String>)> =
        if let Caller::Session(uid) = caller {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            state
                .lookup_session_any(&uid.session_uid)
                .map(|s| (s.workflow_run_id, s.workflow_role))
        } else {
            None
        };
    let is_session_caller = matches!(caller, Caller::Session(_));

    let mut event = crate::workflow::events::Event {
        id: new_event_id(),
        ts: now_unix_f64(),
        run_id: p.run_id.clone(),
        role: role_for_event,
        tool: "workflow_reject_finding".to_string(),
        args: json!({"text": trimmed_text}),
        source: "daemon".to_string(),
        from_role: None,
        iteration: 0,
    };

    // Apply state mutation under flock. Captures iteration for
    // the event after mutation succeeds. Mirrors workflow_done's
    // capture-after-mutation pattern.
    //
    // Snapshot the pre-mutation `rejected_findings.len()` so an
    // event-write failure below can roll back the push under
    // flock. The mutation is monotone (push-only); restoring the
    // pre-mutation length is sufficient — no need for a full
    // `WorkflowRun` snapshot like `workflow_transition` carries.
    // Mirrors the round-6 F2 rollback pattern from
    // `workflow_transition` / `workflow_done`; we just have less
    // to restore.
    let run_id_for_closure = p.run_id.clone();
    let text_for_closure = trimmed_text.clone();
    let captured_iteration: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let captured_iteration_for_closure = captured_iteration.clone();
    let rollback_len: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
    let rollback_len_for_closure = rollback_len.clone();
    let outcome = crate::workflow::run::try_modify(
        &p.run_id,
        move |run| -> Result<(), (ErrorCode, String)> {
            if is_session_caller {
                let active = run.active_role.as_deref();
                let allowed = session_workflow_info
                    .as_ref()
                    .map(|(wf_id, wf_role)| {
                        wf_id.as_deref() == Some(&run_id_for_closure)
                            && wf_role.as_deref() == active
                    })
                    .unwrap_or(false);
                if !allowed {
                    return Err((
                        ErrorCode::Unauthorized,
                        format!(
                            "workflow_reject_finding: caller is not a participant of run {}",
                            run_id_for_closure,
                        ),
                    ));
                }
            }
            if !matches!(run.status, crate::workflow::run::RunStatus::Running) {
                return Err((
                    ErrorCode::Conflict,
                    format!(
                        "workflow_reject_finding: run {} is not Running (status={:?})",
                        run_id_for_closure, run.status,
                    ),
                ));
            }
            // Pre-mutation len AFTER validation, BEFORE mutation —
            // matches the snapshot point `workflow_transition` /
            // `workflow_done` use for `rollback_state`.
            *rollback_len_for_closure
                .lock()
                .unwrap_or_else(|p| p.into_inner()) =
                Some(run.rejected_findings.len());
            run.rejected_findings.push(crate::workflow::run::RejectedFinding {
                text: text_for_closure.clone(),
                recorded_at: crate::workflow::run::now_unix(),
                iteration: run.iteration,
            });
            *captured_iteration_for_closure
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = run.iteration;
            Ok(())
        },
    );
    let _updated = match outcome {
        crate::workflow::run::TryModifyOutcome::Ok(r) => r,
        crate::workflow::run::TryModifyOutcome::Aborted(e) => return Err(e),
        crate::workflow::run::TryModifyOutcome::Persist(
            crate::workflow::run::PersistError::Io(io_err),
        ) if io_err.kind() == std::io::ErrorKind::NotFound => {
            return Err((
                ErrorCode::NotFound,
                format!(
                    "workflow_reject_finding: workflow run {} has no state.json on disk",
                    p.run_id,
                ),
            ));
        }
        crate::workflow::run::TryModifyOutcome::Persist(other) => {
            return Err((
                ErrorCode::Internal,
                format!("workflow_reject_finding: state.json mutation failed: {}", other),
            ));
        }
    };

    event.iteration = *captured_iteration
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    let watcher = state_arc
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .workflow_event_watcher
        .clone();
    if let Err(e) = append_event_with_retry(&event, &watcher) {
        // Round-6 F2 rollback (mirrored from
        // `workflow_transition` / `workflow_done`): state.json
        // advanced (rejection pushed) but events.jsonl missed
        // the event — a caller-side retry would push another
        // duplicate `RejectedFinding`. Truncate back to the
        // pre-mutation length under flock so the retry re-runs
        // the full RMW cleanly. Field-targeted (only
        // rejected_findings was touched) for the same reason
        // workflow_transition's round-12 rollback is field-
        // targeted: a TUI-owned field could have advanced on
        // disk during the retry window.
        eprintln!(
            "cm-daemon: workflow_reject_finding(run={}): state.json advanced but \
             append_event failed after retries: {} — rolling back \
             rejected_findings push so caller retry sees the original state. \
             See NOTES.md \"Rejected findings (10d-2c-1)\" for the rollback \
             failure mode.",
            event.run_id, e,
        );
        let snapshot_len = rollback_len
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(orig_len) = snapshot_len {
            if let Err(re) = crate::workflow::run::modify(&p.run_id, move |r| {
                if r.rejected_findings.len() > orig_len {
                    r.rejected_findings.truncate(orig_len);
                }
            }) {
                eprintln!(
                    "cm-daemon: workflow_reject_finding({}): ROLLBACK ALSO FAILED: \
                     {} — state.json on disk reflects the (uncommitted) \
                     post-mutation shape with no matching event; manual \
                     recovery required",
                    p.run_id, re,
                );
            }
        } else {
            eprintln!(
                "cm-daemon: workflow_reject_finding({}): no pre-mutation \
                 snapshot captured (closure returned early); on-disk \
                 state should be untouched",
                p.run_id,
            );
        }
        return Err((
            ErrorCode::Internal,
            format!("workflow_reject_finding: failed to append event: {}", e),
        ));
    }

    Ok(json!({
        "ok": true,
        "event_id": event.id,
        "run_id": event.run_id,
    }))
}

// ============================================================
// mcp_start_session (sub-2b-3)
// ============================================================
//
// The Python MCP `start_session` tool (`mcp_server/server.py:361`)
// sends `{type, label, prompt?, task_id?}` — much smaller than
// the daemon's existing `start_session` wire shape (`{uid,
// workspace_id, label, session_type, argv, working_dir, env,
// cols, rows, ...}`) which the TUI's `ClientSession::new` builds.
//
// Two reasons for a separate method (not a discriminated union):
//   1. Security boundary. TUI is trusted to supply argv verbatim
//      (it owns mcp_config::build_args). Session callers cannot
//      be — agents would arbitrary-exec anything through the
//      argv field. Separate methods make the boundary obvious.
//   2. Evolution paths. A future field on one shape (e.g. a
//      TUI-only argv-mod option) shouldn't perturb the other.
//
// **Daemon-local argv mapping**: slice 10c-e-3b deliberately
// removed type→argv mapping from the daemon's general spawn
// path. Sub-2b-3 brings a minimal mapping back — scoped to
// THIS method only — via `crate::mcp_config::build_args`. The
// general `start_session` method keeps its strict "caller
// supplies argv" shape. Documented at
// `daemon/src/mcp_config.rs` module head.
//
// **Auth**: Session callers allowed. When `task_id` is supplied,
// it must be self-or-descendant of the caller's task per
// sub-2a's `task_is_self_or_descendant_of`. Taskless caller +
// explicit task_id → Unauthorized. Operator callers also
// allowed (uniform with other methods).

#[derive(Deserialize)]
struct McpStartSessionParams {
    #[serde(rename = "type")]
    type_: String,
    label: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    /// Grant the spawned child global permissions. **Escalation
    /// guard**: honored only when the CALLER is itself global — a
    /// normal agent requesting `global_perms=true` here gets
    /// `Unauthorized`. This is the one path by which global perms
    /// propagate agent-to-agent; the operator's `start_session`
    /// RPC is the other (human) grant path. Defaults to `false`.
    #[serde(default)]
    global_perms: bool,
}

/// Kitty keyboard Enter (CSI 13 u). codex and claude-code both enable the
/// kitty keyboard protocol at startup, which encodes Enter as this sequence
/// rather than raw `\r`/`\n`. Mirrors the TUI's `enter_bytes_for_mode`
/// kitty arm. (Verified: a bare `\n` submits neither agent's composer.)
const AGENT_KITTY_ENTER: &[u8] = b"\x1b[13u";

/// Wait after spawn before writing the prompt body, so the agent finishes
/// enabling its kitty + bracketed-paste modes (codex enabled them ~1.3-1.8s
/// post-startup in codex-tui.log; claude-code is similar). Held a bit above
/// that for margin; this is the most likely value to need tuning if the
/// prompt still doesn't submit on a slow cold-start — the delivery log line
/// below reports the actual elapsed time to guide it.
const AGENT_PROMPT_SETTLE: std::time::Duration = std::time::Duration::from_millis(2500);

/// Gap between the body and the trailing Enter, so the agent consumes the
/// paste and treats the Enter as a distinct keystroke (not paste tail).
/// Mirrors the TUI's separate deferred-Enter write.
const AGENT_ENTER_GAP: std::time::Duration = std::time::Duration::from_millis(1500);

/// Wrap a prompt body in bracketed-paste markers (`\x1b[200~ … \x1b[201~`)
/// when it spans multiple lines — matches the TUI's
/// `format_body_for_delivery`. Without this, the agent submits at the first
/// newline, mangling a multi-line prompt. Single-line bodies go raw. codex
/// and claude-code always enable BRACKETED_PASTE, so (unlike the TUI) we
/// don't gate on a live terminal mode the daemon can't read.
fn agent_paste_payload(body: &str) -> Vec<u8> {
    if body.contains('\n') {
        let mut out = Vec::with_capacity(body.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        body.as_bytes().to_vec()
    }
}

/// Deliver a `start_session` prompt to a kitty-TUI agent (codex or
/// claude-code) on a detached thread: settle, write the bracketed body,
/// gap, write the kitty Enter. Async so `mcp_start_session` returns
/// promptly (stays under the Python MCP 30s call timeout). The daemon
/// can't read the agent's live terminal mode (no `Term`), so this assumes
/// the modes codex/claude-code always enable; see the constants above.
/// Write failures are logged, not fatal — the session is already
/// registered and the caller has its uid.
fn spawn_agent_prompt_delivery(
    handle: crate::session::InputHandle,
    session_uid: String,
    prompt: String,
) {
    let _ = std::thread::Builder::new()
        .name(format!("cm-daemon-agent-prompt-{}", session_uid))
        .spawn(move || {
            std::thread::sleep(AGENT_PROMPT_SETTLE);
            let body = prompt.trim_end_matches(['\r', '\n']);
            let payload = agent_paste_payload(body);
            let bracketed = payload.len() != body.len();
            if let Err(e) = handle.write_and_stamp(&payload) {
                eprintln!(
                    "cm-daemon: agent prompt body write failed for {}: {}",
                    session_uid, e
                );
                return;
            }
            std::thread::sleep(AGENT_ENTER_GAP);
            if let Err(e) = handle.write_and_stamp(AGENT_KITTY_ENTER) {
                eprintln!(
                    "cm-daemon: agent prompt Enter write failed for {}: {}",
                    session_uid, e
                );
                return;
            }
            // Positive delivery log (the failure mode this fix targets is
            // "writes succeed but the agent never submits" — invisible
            // without this line). If the session stays `pending` after this
            // logs, the bytes landed but the timing/encoding assumption was
            // wrong (tune AGENT_PROMPT_SETTLE / the kitty sequence), rather
            // than a write error or a missing delivery.
            eprintln!(
                "cm-daemon: agent prompt delivered for {}: settle={}ms gap={}ms \
                 body={}B bracketed={} + kitty-Enter(CSI 13 u)",
                session_uid,
                AGENT_PROMPT_SETTLE.as_millis(),
                AGENT_ENTER_GAP.as_millis(),
                body.len(),
                bracketed,
            );
        });
}

/// Deliver a PERSISTENT continuous-task fire's prompt to an EXISTING live
/// session (no respawn — prior context preserved), optionally preceded by a
/// `/clear` auto-compaction. Detached thread, same kitty-TUI mechanics as
/// [`spawn_agent_prompt_delivery`]: settle, then (when `compact`) `/clear` body →
/// gap → kitty-Enter → settle, then the bracketed prompt body → gap → kitty-Enter.
///
/// The `/clear` reuses the SAME hardcoded kitty-Enter + raw single-line body as
/// the prompt delivery, NOT `fresh_reset::send_clear_body` + a `PtyModeTracker`:
/// a tracker attached at fire time has NOT observed the agent's startup
/// kitty/bracketed-paste escapes (those fire once at process start, not per
/// fire), so `term_mode()` would report the default raw-`\r` mode and `/clear`
/// would not submit on a kitty TUI — the exact failure `spawn_agent_prompt_delivery`
/// was written to avoid. The compaction keeps the SAME session/PTY/uid (only the
/// agent's internal transcript sid rotates); `current_session_uid` is unchanged.
/// Write failures are logged, not fatal (the fire already counted).
fn spawn_persistent_prompt_delivery(
    handle: crate::session::InputHandle,
    session_uid: String,
    prompt: String,
    compact: bool,
) {
    let _ = std::thread::Builder::new()
        .name(format!("cm-daemon-persistent-prompt-{}", session_uid))
        .spawn(move || {
            std::thread::sleep(AGENT_PROMPT_SETTLE);
            if compact {
                // `/clear` is a single-line slash command — raw body, no bracket.
                if let Err(e) = handle.write_and_stamp(b"/clear") {
                    eprintln!(
                        "cm-daemon: persistent /clear body write failed for {}: {}",
                        session_uid, e
                    );
                    return;
                }
                std::thread::sleep(AGENT_ENTER_GAP);
                if let Err(e) = handle.write_and_stamp(AGENT_KITTY_ENTER) {
                    eprintln!(
                        "cm-daemon: persistent /clear Enter write failed for {}: {}",
                        session_uid, e
                    );
                    return;
                }
                // Let the agent process /clear (transcript rotation) before the
                // next prompt lands.
                std::thread::sleep(AGENT_PROMPT_SETTLE);
                eprintln!(
                    "cm-daemon: persistent auto-compact /clear delivered for {}",
                    session_uid
                );
            }
            let body = prompt.trim_end_matches(['\r', '\n']);
            let payload = agent_paste_payload(body);
            let bracketed = payload.len() != body.len();
            if let Err(e) = handle.write_and_stamp(&payload) {
                eprintln!(
                    "cm-daemon: persistent prompt body write failed for {}: {}",
                    session_uid, e
                );
                return;
            }
            std::thread::sleep(AGENT_ENTER_GAP);
            if let Err(e) = handle.write_and_stamp(AGENT_KITTY_ENTER) {
                eprintln!(
                    "cm-daemon: persistent prompt Enter write failed for {}: {}",
                    session_uid, e
                );
                return;
            }
            eprintln!(
                "cm-daemon: persistent prompt delivered for {}: compact={} \
                 body={}B bracketed={} + kitty-Enter(CSI 13 u)",
                session_uid,
                compact,
                body.len(),
                bracketed,
            );
        });
}

pub fn mcp_start_session(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
    caller_uid: Option<&str>,
) -> MethodResult {
    let p: McpStartSessionParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("mcp_start_session params: {}", e)))?;
    // Type validation BEFORE the caller lookup so a bogus type
    // surfaces InvalidParams without needing a live caller.
    if !is_valid_session_type(&p.type_) {
        return Err((
            ErrorCode::InvalidParams,
            format!(
                "type must be one of \"claude-code\", \"codex\", \"bash\"; got '{}'",
                p.type_
            ),
        ));
    }
    if p.label.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "label must be non-empty".into(),
        ));
    }
    // Sub-2b-3 review-8 #1: validate prompt length BEFORE
    // spawn. The post-spawn delivery path goes through the
    // same write_and_stamp helper attach-stream Input frames
    // use, but the input-cap check (`send_input` enforces
    // `MAX_SEND_INPUT_BYTES`) was being bypassed for
    // mcp_start_session-delivered prompts. Pre-fix an agent
    // could stuff a huge prompt into `start_session` and
    // bypass the 64 KiB cap. Reject up-front so no orphan
    // session is created.
    if let Some(prompt_text) = p.prompt.as_deref() {
        if prompt_text.len() > MAX_SEND_INPUT_BYTES {
            return Err((
                ErrorCode::InvalidParams,
                format!(
                    "prompt field {} bytes exceeds cap of {} bytes \
                     (same cap as send_input — review-8 #1)",
                    prompt_text.len(),
                    MAX_SEND_INPUT_BYTES,
                ),
            ));
        }
    }

    // Resolve caller context: caller's session, workspace_id,
    // task_id, working_dir, memory_cap inheritance. The lock is
    // held briefly to clone what we need; we then drop it before
    // any spawn work.
    //
    // **Working_dir resolution** (sub-2b-3 review-fix #3):
    // mirrors `tui/src/control/methods.rs::start_session` —
    // when `task_id` is supplied AND it's a DESCENDANT of the
    // caller's task (i.e. not the caller's own task), look up
    // that descendant task's bound workspace and use its
    // `worktree_path`. Branch-mode subtasks live in fresh
    // child worktrees; using the caller's worktree there would
    // spawn the child in the wrong tree while still tagging
    // it with the descendant task.
    //
    // **Cap inheritance** (sub-2b-3 review-fix #1): pull all
    // three cap fields off the caller's `DaemonSession`. None
    // for an uncapped caller; the wrap is a passthrough then.
    let (
        caller_workspace_id,
        caller_task_id,
        working_dir,
        cap_inherit,
        caller_cols,
        caller_rows,
    ) = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let cuid = caller_uid.ok_or((
            ErrorCode::Unauthorized,
            "mcp_start_session is callable only by Session callers \
             (the daemon resolves workspace_id / working_dir from \
             the caller's session). Operator callers should use the \
             full-shape `start_session` instead.".into(),
        ))?;
        let caller = state.sessions.get(cuid).ok_or((
            ErrorCode::Unauthorized,
            format!("caller session '{}' not in daemon registry", cuid),
        ))?;
        // Global-perms escalation guard: a caller may grant the new
        // child global perms ONLY if the caller is itself global.
        // This is the agent-to-agent propagation path the design
        // allows; a normal agent cannot mint a privileged child to
        // escape its own descendant-only scope.
        if p.global_perms && !caller.global_perms {
            return Err((
                ErrorCode::Unauthorized,
                "global_perms requires the caller to itself hold global \
                 permissions; a non-global agent cannot grant global perms \
                 to a child (escalation guard)".into(),
            ));
        }
        // Task auth: if caller supplied a task_id, it must be
        // self-or-descendant of caller's task. Taskless caller +
        // explicit task_id → Unauthorized.
        if let Some(req_task) = p.task_id.as_deref() {
            match caller.task_id.as_deref() {
                None => {
                    return Err((
                        ErrorCode::Unauthorized,
                        format!(
                            "taskless caller cannot bind new session to task '{}'",
                            req_task
                        ),
                    ));
                }
                Some(own) => {
                    if !crate::control::auth::task_is_self_or_descendant_of(
                        &state.task_tree,
                        req_task,
                        own,
                    ) {
                        return Err((
                            ErrorCode::Unauthorized,
                            format!(
                                "task '{}' is not the caller's task or a descendant",
                                req_task
                            ),
                        ));
                    }
                }
            }
        }
        // Sub-2b-3 review-fix #3 + review-2 #1: working_dir +
        // workspace_id resolution. If the caller bound the new
        // session to a descendant task that lives in a
        // DIFFERENT workspace (branch-mode subtask), the child
        // spawns into THAT workspace's worktree, not the
        // caller's.
        //
        // Resolution path:
        //   - No `task_id` supplied OR task_id == caller's own
        //     task → caller's workspace.
        //   - task_id is a descendant → look up the bound
        //     workspace via `state.task_workspaces` (pushed by
        //     the TUI via `task.update_tree`). This works for
        //     fresh descendant tasks with NO live anchor
        //     session yet — the pre-review-2 resolver walked
        //     `state.sessions` for a session tagged with the
        //     task, which failed for first-spawn-into-fresh-
        //     subtask (the common case `mcp_start_session`
        //     serves).
        let descendant_task_target = p.task_id.as_deref()
            .filter(|req| caller.task_id.as_deref() != Some(*req));
        let target_workspace_id: String = match descendant_task_target {
            Some(req_task) => state
                .task_workspaces
                .get(req_task)
                // Headless fallback. `create_subtask` registers the new subtask
                // in BOTH `task_workspaces` AND `bindings`, but the TUI's
                // `task.update_tree` push CLEARS `task_workspaces` (full-replace
                // from the TUI's view, which doesn't yet know the daemon-minted
                // subtask) while leaving `bindings` untouched. So a headless
                // orchestrator that `create_subtask`s then immediately
                // `mcp_start_session`s the fix-agent races the TUI's clear and
                // hits an empty `task_workspaces`. `bindings` survives every
                // runtime push (only the daemon's own startup manifest-load
                // replaces it), as does `state.workspaces` (merge-not-replace),
                // so this fallback resolves the spawn reliably without a TUI.
                .or_else(|| state.bindings.get(req_task))
                .cloned()
                .ok_or((
                    ErrorCode::NotFound,
                    format!(
                        "task '{}' has no bound workspace in the daemon's task \
                         snapshot or bindings — neither the TUI's task.update_tree \
                         nor create_subtask registered it (sub-2b-3 review-2 #1)",
                        req_task
                    ),
                ))?,
            None => caller.workspace_id.clone(),
        };
        let ws = state.workspaces.get(&target_workspace_id).ok_or((
            ErrorCode::NotFound,
            format!(
                "target workspace '{}' not in daemon manifest snapshot",
                target_workspace_id
            ),
        ))?;
        let wt = ws.worktree_path.clone().ok_or((
            ErrorCode::NotFound,
            format!(
                "target workspace '{}' has no worktree_path; \
                 cannot resolve working_dir for new session",
                target_workspace_id
            ),
        ))?;
        // Sub-2b-3 review-fix #1: inherit cap from caller.
        //
        // Sub-2b-3 review-4 #1: fail closed on partial cap
        // tuples. Pre-fix the `_ => None` arm swallowed any
        // incomplete `(soft, hard, prefix)` and produced an
        // uncapped child — a cap-bypass vector via wire-shape
        // inconsistency. With the entry-point validation in
        // `start_session_with_spawn_fn` rejecting partial
        // tuples, this branch should be unreachable in
        // practice; if state somehow grows an inconsistent
        // session (test fixture, manual mutation, future bug)
        // we surface it as Internal rather than silently
        // spawning a cap-stripped child.
        let cap_inherit = match (
            caller.memory_cap_soft_bytes,
            caller.memory_cap_hard_bytes,
            caller.cgroup_prefix.clone(),
        ) {
            (Some(soft), Some(hard), Some(prefix)) => {
                Some(InheritedCap { soft_bytes: soft, hard_bytes: hard, cgroup_prefix: prefix })
            }
            (None, None, None) => None,
            (soft, hard, prefix) => {
                return Err((
                    ErrorCode::Internal,
                    format!(
                        "parent session '{}' has incomplete cap metadata \
                         (soft={:?}, hard={:?}, cgroup_prefix={:?}); refusing \
                         to spawn uncapped child — the (soft, hard, prefix) \
                         triple is all-or-nothing (sub-2b-3 review-4 #1)",
                        cuid, soft, hard, prefix,
                    ),
                ));
            }
        };
        // Inherit the caller's current PTY width so the child opens
        // at the same size the operator is actually looking at, not
        // the 80×24 serde default `start_session` would otherwise
        // apply (the "super narrow window" bug for MCP-spawned
        // claude/codex sessions). `last_cols`/`last_rows` are seeded
        // at the caller's spawn and kept live by attach Resize
        // frames, so even after a terminal resize the child matches.
        (
            target_workspace_id,
            caller.task_id.clone(),
            wt,
            cap_inherit,
            caller.last_cols,
            caller.last_rows,
        )
    };

    // Sub-2b-3 review-7: per-worktree slot wraps
    // {pre-snapshot + spawn + detect}, not just {detect}.
    //
    // Pre-review-7 the slot only serialized the detector
    // polling phase. Two same-worktree spawns could both
    // create transcripts before either detector polled, and
    // detector A would see B's (newer) transcript as the
    // "newest unfamiliar" and cross-bind. The round-3 dedup
    // didn't catch this because at A's poll time, no session
    // had yet bound B's file — dedup only excludes already-
    // claimed paths.
    //
    // Fix: acquire the slot, THEN take the pre-spawn
    // snapshot, THEN spawn the child. The second same-worktree
    // spawn waits at `wait_for_turn` until the first's
    // detector has bound. By the time the second's
    // pre-snapshot runs, the first child's transcript is on
    // disk AND already on its session's `transcript_path`,
    // so the second detector's "newest unfamiliar" lookup
    // unambiguously resolves to the second's own file.
    //
    // The main thread holds the slot through the spawn pipeline
    // and then transfers the ticket into the detector thread,
    // which continues to hold the slot through polling. This
    // preserves the round-5 async-return invariant: spawn-main
    // returns as soon as the spawn pipeline completes;
    // detection runs in the background under the same slot.
    //
    // **Bash spawns continue to skip the queue** (review-6 #2b)
    // — they don't write transcripts, so no serialization is
    // needed.
    let detector_engine = crate::transcript_detect::DetectorEngine::from_session_type(&p.type_);
    let queue_ticket: Option<crate::state::WorktreeSpawnTicket> = if detector_engine.is_some() {
        let queue_arc: Arc<crate::state::WorktreeSpawnQueue> = {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            let registry_arc = state.worktree_spawn_queues.clone();
            drop(state);
            let mut registry = registry_arc.lock().unwrap_or_else(|p| p.into_inner());
            registry
                .entry(working_dir.clone())
                .or_insert_with(|| Arc::new(crate::state::WorktreeSpawnQueue::new()))
                .clone()
        };
        let seq = queue_arc.enqueue();
        let ticket = crate::state::WorktreeSpawnTicket::new(queue_arc.clone(), seq);
        // Sub-2b-3 review-9: bounded wait. Pre-fix this
        // called `wait_for_turn(seq)` unbounded — a slow
        // in-flight detector (up to the 60s detector
        // `MAX_DURATION`) could keep the Python
        // `control_client.call()` 30s default timeout
        // firing while we were still inside the handler;
        // the daemon would later resume, spawn the child,
        // and create an orphan session the client believed
        // had failed.
        //
        // With a 20s bound (well below 30s with headroom),
        // a timeout drops the ticket (Drop fires signal_done
        // → releases the slot for the next waiter) and
        // returns `Conflict`. The agent's retry will
        // eventually succeed once the prior detector
        // completes.
        let slot_wait_timeout = slot_wait_timeout();
        if queue_arc.wait_for_turn_timeout(seq, slot_wait_timeout).is_err() {
            // `ticket` drops here → signal_done(seq) →
            // next waiter in the FIFO can proceed. Without
            // this, a timeout would leave the slot held
            // indefinitely and block all later spawns in
            // this worktree.
            drop(ticket);
            return Err((
                ErrorCode::Conflict,
                format!(
                    "another mcp_start_session is in flight in worktree '{}' \
                     (waited {:?}); retry shortly — the prior detector will \
                     release the slot when it completes or times out \
                     (sub-2b-3 review-9: bounded slot wait)",
                    working_dir.display(),
                    slot_wait_timeout,
                ),
            ));
        }
        Some(ticket)
    } else {
        None
    };

    // Sub-2b-3 review-2 #2 + review-7: snapshot transcript
    // ids AFTER acquiring the slot. Prior in-flight detectors
    // have already bound — the snapshot captures their
    // transcripts (plus any pre-existing ones), so the
    // detector's "new unfamiliar" search after this thread's
    // engine writes its file unambiguously resolves to this
    // session's own transcript.
    //
    // Bash sessions skip both the snapshot and the queue.
    let detector_snapshot: Vec<String> = match detector_engine {
        Some(crate::transcript_detect::DetectorEngine::ClaudeCode) => {
            crate::transcript_detect::snapshot_claude_transcript_ids(&working_dir)
        }
        Some(crate::transcript_detect::DetectorEngine::Codex) => {
            crate::transcript_detect::snapshot_codex_transcript_ids(&working_dir)
        }
        None => Vec::new(),
    };

    // Generate a fresh uid in the TUI format. Same generator
    // shape as `tui/src/app.rs::new_session_uid` —
    // `ts-<nanos>-<counter>`.
    let session_uid = new_daemon_minted_session_uid();

    // Build raw argv via the daemon-local mcp_config helper.
    // Writes the per-session claude.json (claude) or builds
    // codex overrides (codex). Bash gets `/bin/bash` with no
    // args.
    // `mcp_start_session` agents are NOT workflow participants — no workflow
    // meta. (start_workflow is the only path that passes `Some(WorkflowMeta)`.)
    // P-2: still prefer the configured `mcp_server_path` so MCP-spawned agents
    // on a configured remote daemon find server.py too.
    let configured_mcp_server_path: Option<String> = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let p = st.config.mcp_server_path.clone();
        if p.trim().is_empty() { None } else { Some(p) }
    };
    let (program, argv_tail) = crate::mcp_config::build_args(
        &p.type_,
        &session_uid,
        None,
        configured_mcp_server_path.as_deref(),
    )
    .map_err(|e| (ErrorCode::Internal, format!("build_args: {}", e)))?;

    // Sub-2b-3 review-fix #1: wrap argv with systemd-run when
    // the caller carries a cap. Mirrors the TUI's
    // `try_spawn_via_daemon` wrap (see
    // `tui/src/session.rs::wrap_with_systemd_run`) so a capped
    // agent's MCP-driven subtask inherits the same memory
    // ceiling. Pre-fix the daemon spawned the child raw,
    // letting an agent escape its cap via the MCP path.
    let (final_program, final_argv_tail) = match cap_inherit.as_ref() {
        Some(cap) => {
            let cap_spec = crate::mcp_config::CapSpec {
                soft_bytes: cap.soft_bytes,
                hard_bytes: cap.hard_bytes,
                session_uid: &session_uid,
                cgroup_prefix: &cap.cgroup_prefix,
            };
            let (wrapped_program, wrapped_args, _cgroup_path) =
                crate::mcp_config::wrap_with_systemd_run(&program, &argv_tail, Some(&cap_spec));
            (wrapped_program, wrapped_args)
        }
        None => (program, argv_tail),
    };
    let mut argv = Vec::with_capacity(final_argv_tail.len() + 1);
    argv.push(final_program);
    argv.extend(final_argv_tail);

    // Build env: daemon-injected pins plus nothing else (no workflow meta —
    // mcp_start_session agents aren't workflow participants).
    let env_map = crate::mcp_config::build_env(&session_uid, None);
    let env_obj: serde_json::Map<String, Value> = env_map
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();

    // Compose the full StartSessionParams JSON. We delegate to
    // the existing `start_session` method to keep the spawn
    // pipeline in one place (validation, two-phase spawn,
    // reaper, registry insert).
    let task_id_for_spawn = p.task_id.clone().or(caller_task_id);
    let mut full_params = serde_json::Map::new();
    full_params.insert("uid".into(), Value::String(session_uid.clone()));
    full_params.insert("workspace_id".into(), Value::String(caller_workspace_id));
    full_params.insert("label".into(), Value::String(p.label.clone()));
    full_params.insert("argv".into(), Value::Array(
        argv.into_iter().map(Value::String).collect(),
    ));
    full_params.insert(
        "working_dir".into(),
        Value::String(working_dir.to_string_lossy().into_owned()),
    );
    full_params.insert("env".into(), Value::Object(env_obj));
    full_params.insert("session_type".into(), Value::String(p.type_.clone()));
    // Width inheritance (see the caller-resolution block above):
    // pass the caller's live PTY size through so the delegated
    // `start_session` opens the child at that size instead of its
    // 80×24 serde fallback.
    full_params.insert("cols".into(), Value::Number(caller_cols.into()));
    full_params.insert("rows".into(), Value::Number(caller_rows.into()));
    if let Some(cuid) = caller_uid {
        full_params.insert("managed_by_uid".into(), Value::String(cuid.to_string()));
    }
    if let Some(tid) = task_id_for_spawn {
        full_params.insert("task_id".into(), Value::String(tid));
    }
    // Propagate the (guard-approved) global-perms grant to the
    // child. The escalation guard above already verified the caller
    // is global, so `start_session` can trust this value.
    if p.global_perms {
        full_params.insert("global_perms".into(), Value::Bool(true));
    }
    if let Some(cap) = cap_inherit.as_ref() {
        full_params.insert(
            "memory_cap_bytes".into(),
            Value::Number(cap.soft_bytes.into()),
        );
        full_params.insert(
            "memory_cap_hard_bytes".into(),
            Value::Number(cap.hard_bytes.into()),
        );
        full_params.insert(
            "cgroup_prefix".into(),
            Value::String(cap.cgroup_prefix.to_string_lossy().into_owned()),
        );
    }

    let start_result = start_session(state_arc, &Value::Object(full_params))?;

    // Sub-2b-3 review-fix #2: deliver `prompt` if supplied.
    // Pre-fix this was logged-and-dropped — silent contract
    // break with the Python MCP tool which advertises prompt
    // delivery. Look up the new session's InputHandle and
    // write the prompt through the shared `write_and_stamp`
    // helper (same path the attach-stream Input frame handler
    // uses).
    //
    // Engine-specific submission:
    //   - claude-code / bash: body + `\n`, synchronous, with
    //     kill-on-failure (no half-initialized session).
    //   - codex: a bare `\n` does NOT submit codex's
    //     kitty-keyboard TUI, and a multi-line body without
    //     bracketed paste submits at the first newline. The
    //     daemon has no `Term` to read codex's live mode, so it
    //     delivers asynchronously assuming codex's always-on
    //     modes (bracketed paste + kitty Enter) with a settle/
    //     gap delay — see `spawn_agent_prompt_delivery`. This
    //     is the daemon-side stand-in for the TUI's mode-aware
    //     `PendingWrite::wait_for_quiet` drainer, which isn't
    //     relocated daemon-side.
    if let Some(prompt) = p.prompt.as_deref() {
        if !prompt.is_empty() {
            let handle_opt = {
                let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                state
                    .sessions
                    .get(&session_uid)
                    .map(|s| {
                        // codex AND claude-code both run kitty-keyboard
                        // TUIs that need the bracketed-paste + kitty-Enter
                        // treatment (verified: a bare `\n` submits neither).
                        // bash is a real shell — `\n` is the correct submit.
                        let is_tui_agent = matches!(
                            s.session_type.as_str(),
                            "codex" | "claude-code"
                        );
                        (s.input_handle(), is_tui_agent)
                    })
            };
            let Some((handle, is_tui_agent)) = handle_opt else {
                // Session disappeared between spawn and prompt
                // delivery — exceptional but possible (fast-
                // exit reaper removed the registry entry).
                // Sub-2b-3 review-8 #1: surface as Internal
                // rather than silent log. The caller's
                // contract is "prompt was delivered if I get
                // ok"; honoring it means failing closed when
                // the session is gone.
                return Err((
                    ErrorCode::Internal,
                    format!(
                        "mcp_start_session: session '{}' vanished \
                         between spawn and prompt-write",
                        session_uid,
                    ),
                ));
            };
            if is_tui_agent {
                // codex and claude-code both run kitty-keyboard TUIs: a bare
                // newline in the composer does NOT submit, and a multi-line
                // body delivered without bracketed-paste markers splits into
                // premature submissions. The TUI handles this via its
                // mode-aware drainer; the daemon has no `Term` to read the
                // live terminal mode, so we deliver asynchronously assuming
                // the modes these agents always enable (BRACKETED_PASTE +
                // kitty), with a timing gap so they're active before the
                // bytes land. See `spawn_agent_prompt_delivery`. Returns
                // immediately; the transcript detector binds once the agent
                // runs the turn.
                spawn_agent_prompt_delivery(
                    handle,
                    session_uid.clone(),
                    prompt.to_string(),
                );
            } else {
                // claude-code / bash: body + newline, synchronous, with
                // kill-on-failure so no half-initialized session lingers
                // (Sub-2b-3 review-8 #1). A failed write removes the session
                // from the registry, whose Drop SIGKILLs the child.
                let mut payload = prompt.as_bytes().to_vec();
                if !payload.ends_with(b"\n") {
                    payload.push(b'\n');
                }
                if let Err(e) = handle.write_and_stamp(&payload) {
                    let err_msg = format!(
                        "mcp_start_session: prompt-delivery write failed for '{}': {}; \
                         session was killed (review-8 #1 — no half-initialized sessions)",
                        session_uid, e,
                    );
                    let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                    let _ = state.sessions.remove(&session_uid);
                    drop(state);
                    return Err((ErrorCode::Internal, err_msg));
                }
            }
        }
    }

    // Sub-2b-3 review-5 #1 + review-6: dispatch the detector
    // to a background thread, transferring the queue ticket
    // into the closure. Spawn-main returns immediately —
    // necessary to stay under the Python MCP
    // `control_client.call()` 30s default timeout (detector
    // can take up to 60s polling).
    //
    // The ticket's `Drop` releases the queue slot when the
    // detector closure ends (success, timeout, or panic) —
    // see `WorktreeSpawnTicket` for the RAII contract.
    //
    // Bash sessions have `queue_ticket = None` and run no
    // detector — nothing to transfer, nothing to release.
    //
    // Clients that need transcript-bound readiness call
    // `wait_for_session_idle` or poll
    // `resolve_authorized_session` (same shape the TUI uses
    // today).
    //
    // The TUI-spawned-session path is NOT routed through here
    // — TUI sessions have their own detector wired via
    // `App::push_transcript_path_to_daemon_if_attached`.
    if let (Some(engine), Some(ticket)) = (detector_engine, queue_ticket) {
        // Sub-2b-3 review-11: fail-closed on detector-spawn
        // failure. Pre-fix the `Builder::spawn` Err was
        // dropped silently — the session stayed in the
        // registry with no detector, and (with no TUI
        // participant to push `session.set_transcript_path`
        // later) MCP-spawned sessions would stay `pending`
        // forever. Same bug class the watcher path fixed via
        // `WatcherSpawnFn`.
        //
        // On Err here: the ticket was moved into
        // `spawn_queued_detector` and dropped on its failure
        // path (Drop fires `signal_done` → slot released).
        // We must ALSO remove the just-spawned session from
        // the registry so its DaemonSession's pidfd-based
        // Drop SIGKILLs the child — otherwise an orphan
        // session lives without a detector.
        if let Err(e) = crate::transcript_detect::spawn_queued_detector(
            state_arc.clone(),
            session_uid.clone(),
            engine,
            working_dir.clone(),
            detector_snapshot,
            Some(ticket),
            crate::transcript_detect::default_detector_spawn_fn(),
        ) {
            let err_msg = format!(
                "mcp_start_session: failed to spawn transcript detector \
                 thread for '{}': {} (session killed to avoid orphan; \
                 review-11)",
                session_uid, e,
            );
            let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            let _ = state.sessions.remove(&session_uid);
            drop(state);
            return Err((ErrorCode::Internal, err_msg));
        }
    }

    Ok(start_result)
}

// ============================================================
// create_session / add_session (remote-session-execution Phase 1)
//
// Two Operator-only RPCs that let the TUI run its interactive `A-n` /
// `A-s` flows against a REMOTE daemon host. Unlike `start_session`
// (caller supplies argv/env/working_dir as local paths) and unlike
// `mcp_start_session` (Session-callable; derives workspace/task from
// the caller), these accept a high-level request and resolve every
// path on the DAEMON's own filesystem:
//   - `create_session` (A-n): resolves `repo_url`, creates the
//     worktree (`~/.cm/worktrees/<repo>-<slug>` on `cm/<slug>`), and
//     spawns the first session in it.
//   - `add_session` (A-s): looks up an existing workspace's worktree
//     and spawns another session in it — never creates a worktree.
//
// Both build argv/env via the DAEMON's `mcp_config::build_args` /
// `build_env` (so the per-session `~/.cm/mcp/<uid>/claude.json`, the
// sockets, and the venv are the daemon's own) and then delegate to
// `start_session` — the shared spawn core that validates, two-phase-
// spawns, registers, re-injects the daemon's sockets, and broadcasts
// `ManifestDiff::Added`. This mirrors how `mcp_start_session` already
// delegates to `start_session`; the spawn pipeline stays in one place.
//
// They are Operator-only (gated in `dispatch.rs` via `require_operator`)
// because they accept an explicit `workspace_id` (+ `repo_url`/`slug`
// for create) that the Session-callable `mcp_start_session` refuses —
// agents keep using `mcp_start_session`, which enforces descendant-task
// auth and derives the workspace from the caller.
//
// Phase-1 scope: remote sessions spawn UNCAPPED (the memory-cap triple
// is host-local — `cgroup_prefix` is meaningless off-host — so the RPCs
// omit it; daemon-side cap resolution is a later addition). No
// `in_place`/`seed_from` (rejected TUI-side in Phase 3). Repo resolution
// is local-fast-path only; clone-on-demand lands in Phase 2.
// ============================================================

/// `create_session` params (A-n — new workspace, creates a worktree).
#[derive(Deserialize)]
struct CreateSessionParams {
    /// Stable session uid, pre-generated by the caller (same contract
    /// as `start_session`'s `uid`).
    uid: String,
    /// Stable workspace id to bind the session to. Auto-registered in
    /// the daemon's manifest snapshot from the freshly-created worktree.
    workspace_id: String,
    /// Human-readable sidebar label.
    label: String,
    /// Engine: `"claude-code"` | `"codex"` | `"bash"` (same set as
    /// `start_session`'s `session_type`).
    engine: String,
    /// Repo shortname or URL. Resolved to a local checkout on the
    /// daemon host via `find_local_repo` (Phase 2 adds clone-on-demand).
    repo_url: String,
    /// Optional branch to start the worktree from (fetched from origin
    /// first). `None` → new `cm/<slug>` branch off HEAD.
    #[serde(default)]
    start_branch: Option<String>,
    /// Task slug → worktree dir (`<repo>-<slug>`) + branch (`cm/<slug>`).
    slug: String,
    /// Planning task uid this session is bound to. `None` for taskless.
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

/// `add_session` params (A-s — existing workspace, reuses its worktree).
/// No `repo_url`/`slug`/`start_branch` — the workspace already has a
/// worktree.
#[derive(Deserialize)]
struct AddSessionParams {
    uid: String,
    workspace_id: String,
    label: String,
    engine: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

/// Compose the full `start_session` params JSON for a daemon-resolved
/// interactive spawn — the shared resolution behind `create_session`
/// and `add_session`.
///
/// Does the daemon-side path work the TUI used to do locally:
///   - `build_args(engine, uid, None, configured_mcp_server_path)` →
///     argv (and, for `claude-code`, writes the per-session
///     `~/.cm/mcp/<uid>/claude.json` on the DAEMON's filesystem).
///   - `build_env(uid, None)` → env pinned to the DAEMON's sockets.
///   - `working_dir` is the resolved worktree on the daemon's host.
///
/// The result is fed verbatim to `start_session`, which re-injects the
/// daemon's own sockets over `env`, registers the session, and
/// broadcasts `ManifestDiff::Added`. `auto_register_worktree` is
/// `Some(path)` for `create_session` (so the freshly-created worktree's
/// workspace auto-registers via `start_session`'s `worktree_path`
/// branch) and `None` for `add_session` (whose workspace is already
/// known). Remote sessions are uncapped — no memory-cap triple is set.
fn compose_daemon_spawn_params(
    state_arc: &Arc<Mutex<DaemonState>>,
    uid: &str,
    workspace_id: &str,
    label: &str,
    engine: &str,
    working_dir: &std::path::Path,
    task_id: Option<&str>,
    cols: u16,
    rows: u16,
    auto_register_worktree: Option<&std::path::Path>,
) -> MethodResult {
    // P-2: prefer the daemon's configured `mcp_server_path` so the
    // written MCP config points at the server on the daemon's host.
    let configured_mcp_server_path: Option<String> = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let path = st.config.mcp_server_path.clone();
        if path.trim().is_empty() {
            None
        } else {
            Some(path)
        }
    };
    let (program, argv_tail) = crate::mcp_config::build_args(
        engine,
        uid,
        None,
        configured_mcp_server_path.as_deref(),
    )
    .map_err(|e| (ErrorCode::Internal, format!("build_args: {}", e)))?;
    let mut argv = Vec::with_capacity(argv_tail.len() + 1);
    argv.push(program);
    argv.extend(argv_tail);

    let env_map = crate::mcp_config::build_env(uid, None);
    let env_obj: serde_json::Map<String, Value> = env_map
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();

    let mut full = serde_json::Map::new();
    full.insert("uid".into(), Value::String(uid.to_string()));
    full.insert("workspace_id".into(), Value::String(workspace_id.to_string()));
    full.insert("label".into(), Value::String(label.to_string()));
    full.insert(
        "argv".into(),
        Value::Array(argv.into_iter().map(Value::String).collect()),
    );
    full.insert(
        "working_dir".into(),
        Value::String(working_dir.to_string_lossy().into_owned()),
    );
    full.insert("env".into(), Value::Object(env_obj));
    full.insert("session_type".into(), Value::String(engine.to_string()));
    full.insert("cols".into(), Value::Number(cols.into()));
    full.insert("rows".into(), Value::Number(rows.into()));
    if let Some(tid) = task_id {
        full.insert("task_id".into(), Value::String(tid.to_string()));
    }
    if let Some(wt) = auto_register_worktree {
        full.insert(
            "worktree_path".into(),
            Value::String(wt.to_string_lossy().into_owned()),
        );
    }
    Ok(Value::Object(full))
}

// ===================================================================
// Continuous Tasks — Phase 2 trigger funnel (DESIGN_CONTINUOUS_TASKS.md
// §5/§8/§10). MANUAL-ONLY: `trigger` fires a continuous task once, on
// demand, running the FRESH executor (spawn a NEW session per fire, leave
// prior sessions idle). The scheduler / periodic auto-fire (Phase 3), the
// PERSISTENT executor + supervision + watchdog (Phase 3), the queue /
// Consumer layer (Phase 4) and the downstream-allowlist fan-out (Phase 6)
// are later phases and are NOT built here. The continuous record +
// persistence (`crate::continuous::task`) and the runs.jsonl audit
// (`crate::continuous::runlog`) are twins of workflow/run.rs + events.rs and
// ship in the module slice; this slice consumes their public API.
// ===================================================================

/// Thin wrapper over [`compose_daemon_spawn_params`] for a continuous-task
/// fire. Same param shape (argv via `build_args`, env via `build_env`,
/// session_type, cols/rows, task_id, auto-registered worktree) with two
/// continuous-specific bindings:
///   - `engine` is the `session_type` vocab (`"claude-code"`/`"codex"`/
///     `"bash"`) — the caller maps `ContinuousTask::engine` via
///     `Engine::as_session_type` (the wire vocab `"claude"` ≠ the session_type
///     vocab `"claude-code"`).
///   - the resulting params carry the Phase-1 `continuous_task_id` wire field
///     so the spawned session is tagged on `ManifestEntry`/`DaemonSession` and
///     surfaces in the sidebar's Continuous section (the live update rides
///     `manifest.watch`, not a runs.jsonl subscriber).
///
/// `working_dir` is also passed as the `auto_register_worktree` hint, so the
/// task's durable workspace self-heals after a daemon restart cleared the
/// in-memory `state.workspaces` snapshot (idempotent — a no-op when already
/// registered; Phase 2 has no restart reconciliation).
///
/// Phase 3 also threads the memory-cap triple: the per-task `mem_cap_bytes`
/// override (else the daemon's `[scheduler] default_cap`) is resolved via
/// [`resolve_continuous_cap`] and, when backed by a real cgroup prefix, wraps
/// argv in `systemd-run` and sets `memory_cap_bytes`/`memory_cap_hard_bytes`/
/// `cgroup_prefix` (all-or-nothing — `start_session` rejects a partial triple).
/// Absent a user systemd manager the fire runs UNCAPPED rather than failing.
fn compose_continuous_spawn_params(
    state_arc: &Arc<Mutex<DaemonState>>,
    uid: &str,
    workspace_id: &str,
    label: &str,
    engine: &str,
    working_dir: &std::path::Path,
    task_id: Option<&str>,
    continuous_task_id: &str,
    mem_cap_bytes: Option<u64>,
    cols: u16,
    rows: u16,
) -> MethodResult {
    let mut full = compose_daemon_spawn_params(
        state_arc,
        uid,
        workspace_id,
        label,
        engine,
        working_dir,
        task_id,
        cols,
        rows,
        Some(working_dir),
    )?;
    // A continuous fire has no launching caller to inherit a cap from (the
    // scheduler tick / Operator run_now is headless), so derive it from config:
    // the per-task `mem_cap_bytes` override, else `[scheduler] default_cap`.
    let default_cap = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        st.config.scheduler.default_cap
    };
    if let Value::Object(m) = &mut full {
        m.insert(
            "continuous_task_id".into(),
            Value::String(continuous_task_id.to_string()),
        );
        // Memory-cap triple — ALL-OR-NOTHING: wrap argv via systemd-run AND set
        // the three wire keys together (start_session rejects a partial triple
        // and SIGKILLs a child whose scope didn't materialize). The argv built by
        // compose_daemon_spawn_params is the plain program (idx 0) + tail, never a
        // pre-existing wrapper, so re-splitting it is safe. resolve_continuous_cap
        // graceful-degrades to None (uncapped) when the predicted cgroup prefix is
        // absent, so a host with no user systemd manager still fires.
        if let Some((soft, hard, prefix)) =
            resolve_continuous_cap(engine, mem_cap_bytes, default_cap)
        {
            let argv: Vec<String> = m
                .get("argv")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if let Some((program, tail)) = argv.split_first() {
                let cap_spec = crate::mcp_config::CapSpec {
                    soft_bytes: soft,
                    hard_bytes: hard,
                    session_uid: uid,
                    cgroup_prefix: std::path::Path::new(&prefix),
                };
                let (wrapped_program, wrapped_args, _cgroup_path) =
                    crate::mcp_config::wrap_with_systemd_run(program, tail, Some(&cap_spec));
                let mut wrapped_argv = Vec::with_capacity(wrapped_args.len() + 1);
                wrapped_argv.push(Value::String(wrapped_program));
                wrapped_argv.extend(wrapped_args.into_iter().map(Value::String));
                m.insert("argv".into(), Value::Array(wrapped_argv));
                m.insert("memory_cap_bytes".into(), Value::Number(soft.into()));
                m.insert("memory_cap_hard_bytes".into(), Value::Number(hard.into()));
                m.insert("cgroup_prefix".into(), Value::String(prefix));
            }
        }
    }
    Ok(full)
}

/// Resolve the memory-cap triple for a continuous-task spawn (Phase 3,
/// DESIGN_CONTINUOUS_TASKS.md §10 + DESIGN_MEMORY_CAP.md). A continuous fire has
/// no launching caller cap to inherit, so the cap is config-driven: the per-task
/// `mem_cap_bytes` override, else the daemon's `[scheduler] default_cap`.
///
/// A single configured ceiling maps to BOTH `MemoryHigh` and `MemoryMax`
/// (`soft == hard == effective`). `effective == 0` opts out (uncapped). Only
/// `claude-code`/`codex` are capped (bash + unknown → `None`, parity with
/// [`resolve_configured_participant_cap`] and DESIGN_MEMORY_CAP's bash default-off).
///
/// The daemon has NO memory-cap preflight (that lives TUI-side), so the sole gate
/// is the predicted `app.slice` cgroup prefix existing. When it is absent (e.g. a
/// system service with no user systemd manager — no `enable-linger`), return
/// `None` and run the fire UNCAPPED rather than emitting a partial/unbacked triple
/// that `start_session` would reject + SIGKILL. Mirrors
/// [`resolve_configured_participant_cap`]'s `is_dir` graceful-degrade gate and
/// reuses its `CONFIGURED_CAP_PREFIX_OVERRIDE` test seam.
fn resolve_continuous_cap(
    session_type: &str,
    mem_cap_bytes: Option<u64>,
    default_cap: u64,
) -> Option<(u64, u64, String)> {
    // bash/unknown never capped (parity with resolve_configured_participant_cap).
    match session_type {
        "claude-code" | "claude" | "codex" => {}
        _ => return None,
    }
    let effective = mem_cap_bytes.unwrap_or(default_cap);
    if effective == 0 {
        return None;
    }
    let uid = unsafe { libc::getuid() };
    #[allow(unused_mut)]
    let mut prefix = std::path::PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{}.slice/user@{}.service/app.slice",
        uid, uid
    ));
    // In unit tests the cap is OFF unless a test explicitly arms the cgroup prefix
    // override (reuses resolve_configured_participant_cap's seam) — so the
    // spawn-param tests stay deterministic regardless of whether the host has a
    // user systemd slice (where the real `app.slice` would otherwise exist).
    #[cfg(test)]
    match CONFIGURED_CAP_PREFIX_OVERRIDE.with(|c| c.borrow().clone()) {
        Some(ov) => prefix = std::path::PathBuf::from(ov),
        None => return None,
    }
    if !prefix.is_dir() {
        eprintln!(
            "cm-daemon: continuous cap requested ({} bytes) but predicted cgroup \
             prefix {} is absent (no user manager?) — running the fire UNCAPPED \
             rather than failing the spawn",
            effective,
            prefix.display()
        );
        return None;
    }
    Some((effective, effective, prefix.to_string_lossy().into_owned()))
}

/// Mint a fire_token idempotency key (`ft_<hex>-<hex>`) for a `trigger` call
/// that didn't supply one. Same `nanos`+counter recipe as
/// [`new_daemon_minted_session_uid`], distinct prefix. A minted token is fresh
/// by construction, so it never collides with `last_run.fire_token` — the
/// duplicate-fire guard only ever fires for a CALLER-supplied token.
fn new_fire_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ft_{:x}-{:x}", nanos, n)
}

/// Epoch-seconds as f64 for a `runs.jsonl` line's `ts` (mirrors
/// `workflow::events::Event.ts`).
fn runlog_now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
thread_local! {
    /// Test seam for the FRESH executor's spawn boundary. When armed
    /// (`Some(vec)`), [`continuous_fresh_spawn`] RECORDS the composed
    /// `start_session` params and returns a synthetic `{session_uid}` WITHOUT
    /// spawning — so a unit test can assert the params carry
    /// `continuous_task_id` + the pinned worktree without launching a real
    /// claude. Mirrors `SPAWN_PROGRAM_OVERRIDE` / `CAPTURED_WORKFLOW_META` in
    /// `start_workflow`'s spawn loop.
    static CONTINUOUS_SPAWN_SPY: std::cell::RefCell<Option<Vec<Value>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn arm_continuous_spawn_spy_for_test() {
    CONTINUOUS_SPAWN_SPY.with(|c| *c.borrow_mut() = Some(Vec::new()));
}

#[cfg(test)]
pub(crate) fn take_continuous_spawn_spy_for_test() -> Vec<Value> {
    CONTINUOUS_SPAWN_SPY.with(|c| c.borrow_mut().take().unwrap_or_default())
}

#[cfg(test)]
thread_local! {
    /// Test seam for the PERSISTENT executor's live-delivery boundary. When
    /// armed (`Some(vec)`), the persistent executor (1) treats a present
    /// `current_session_uid` as a LIVE session WITHOUT probing `state.sessions`
    /// (so a test needn't stand up a real PTY), and (2) RECORDS each delivery as
    /// `(run_mode, session_uid, compact)` instead of handing off to the detached
    /// delivery thread. Mirrors [`CONTINUOUS_SPAWN_SPY`] for the spawn boundary.
    static CONTINUOUS_DELIVERY_SPY: std::cell::RefCell<Option<Vec<(String, String, bool)>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn arm_continuous_delivery_spy_for_test() {
    CONTINUOUS_DELIVERY_SPY.with(|c| *c.borrow_mut() = Some(Vec::new()));
}

#[cfg(test)]
pub(crate) fn take_continuous_delivery_spy_for_test() -> Vec<(String, String, bool)> {
    CONTINUOUS_DELIVERY_SPY.with(|c| c.borrow_mut().take().unwrap_or_default())
}

/// The FRESH executor's spawn boundary: hand the composed params to the
/// production [`start_session`] choke point — the two-phase race-safe spawn
/// (`PendingSession::spawn` → arm_reaper → lock-held uid-collision recheck →
/// registry insert → `ManifestDiff::Added`) plus claude_trust pretrust and the
/// `CM_*` env injection. Calling `start_session` (NOT `PendingSession::spawn`
/// directly) is load-bearing: it preserves every guarantee
/// create_session/add_session/mcp_start_session/start_workflow rely on.
///
/// Test seam: when [`CONTINUOUS_SPAWN_SPY`] is armed the composed params are
/// recorded and a synthetic `{session_uid}` is returned without spawning.
fn continuous_fresh_spawn(state_arc: &Arc<Mutex<DaemonState>>, full: &Value) -> MethodResult {
    #[cfg(test)]
    {
        let spied = CONTINUOUS_SPAWN_SPY.with(|c| {
            if let Some(captured) = c.borrow_mut().as_mut() {
                captured.push(full.clone());
                true
            } else {
                false
            }
        });
        if spied {
            let uid = full
                .get("uid")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            return Ok(json!({ "session_uid": uid }));
        }
    }
    start_session(state_arc, full)
}

/// Roll back the spawn-window `in_flight` guard and write a `"fired"` failure
/// line after the FRESH executor's spawn boundary errored. Best-effort: the
/// trigger is already returning the error to the caller, so a failed
/// clear/append is logged rather than propagated. Leaving `in_flight` set would
/// wedge the task into permanent `busy` (Phase 3's restart reconciliation
/// clears a leaked guard, but Phase 2 must not leak it on a clean error path).
fn continuous_clear_in_flight_after_failure(
    task_id: &str,
    seq: u64,
    fire_token: &str,
    session_uid: &str,
    run_mode: &str,
    trigger_source: &str,
    detail: &str,
) {
    if let Err(e) = crate::continuous::task::modify(task_id, |t| {
        t.in_flight = None;
    }) {
        eprintln!(
            "cm-daemon: trigger could not clear in_flight for '{}' after spawn failure: {}",
            task_id, e
        );
    }
    if let Err(e) =
        crate::continuous::runlog::ContinuousRunLog::append(&crate::continuous::runlog::RunLogLine {
            seq,
            ts: runlog_now_ts(),
            task_id: task_id.to_string(),
            event: "fired".to_string(),
            fire_token: Some(fire_token.to_string()),
            session_uid: Some(session_uid.to_string()),
            run_mode: Some(run_mode.to_string()),
            trigger_source: Some(trigger_source.to_string()),
            status: Some("failed".to_string()),
            detail: Some(json!({ "error": detail })),
        })
    {
        eprintln!(
            "cm-daemon: trigger failure-runlog append failed for '{}': {}",
            task_id, e
        );
    }
}

/// `trigger` request params (DESIGN_CONTINUOUS_TASKS.md §8). `args` is a
/// free-form blob the daemon does NOT parse (later phases thread it to the
/// agent unchanged); `fire_token` is the Phase-2 idempotency key — absent →
/// the daemon mints `ft_<hex>`.
#[derive(serde::Deserialize)]
struct TriggerParams {
    task_id: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    fire_token: Option<String>,
}

/// Reason a `trigger` fire was abandoned inside the inside-flock check-and-set.
/// Mapped to a clean `{fired:false, reason}` response, NOT an `ErrorCode` error.
enum FireAbort {
    Busy,
    DuplicateFireToken,
}

/// `trigger` — fire a continuous task once (Phase 2 manual fire + Phase 3
/// scheduler/supervision fire; the scheduler calls this in-process).
///
/// Bimodal caller: Operator (TUI / cloud control plane / the daemon's own
/// scheduler — token validated at dispatch / internal) OR Session (an agent
/// fanning out). A Session caller is CONFINED to its own task or a descendant
/// (the downstream-allowlist edge is Phase 6).
///
/// Flow: validate task_id → (Session) self-or-descendant gate → `load_one` →
/// paused guard → resolve the prompt → resolve the per-`run_mode` session uid →
/// inside-flock atomic check-and-set of the `in_flight` spawn-window guard
/// (rejects a concurrent fire as `busy` and a repeat idempotency key as
/// `duplicate_fire_token`) → executor → record `last_run` /
/// `current_session_uid` / `run_count` + CLEAR `in_flight` → append a `"fired"`
/// runs.jsonl line.
///
/// Two executors, branched on `run_mode`:
///   - FRESH — spawn a NEW session per fire (compose params tagged with
///     `continuous_task_id`, pinned to the durable worktree, spawned via the
///     `start_session` choke point), leave prior sessions idle, then
///     `spawn_agent_prompt_delivery`.
///   - PERSISTENT — deliver the prompt to the task's existing live session (no
///     respawn, prior context preserved) via `spawn_persistent_prompt_delivery`,
///     auto-`/clear`-compacting every `compact_every` runs. A dead/absent pinned
///     session promotes to a FRESH respawn (mint a new uid, spawn, rebind
///     `current_session_uid`) rather than writing to a dead PTY.
///
/// Append the completion-signal instruction to a FRESH agent (claude/codex)
/// prompt. A periodic fresh run stays `last_run.status == Running` until it
/// signals done, and the scheduler's due-skip-active will not re-fire a Running
/// task — and a fresh claude/codex session IDLES after its turn (it does not
/// exit), so without an explicit `report_done` a periodic task fires exactly
/// ONCE. Bash runs (run-and-exit → clean exit signals Done) and PERSISTENT
/// sessions (reuse one live PTY, re-delivered each tick — no re-fire gate) are
/// returned unchanged. Verified on cm-manager 2026-06-27.
fn with_completion_instruction(
    prompt: String,
    engine: crate::continuous::task::Engine,
    run_mode: crate::continuous::task::RunMode,
) -> String {
    use crate::continuous::task::{Engine, RunMode};
    if engine == Engine::Bash || run_mode == RunMode::Persistent {
        return prompt;
    }
    format!(
        "{}\n\n---\nWhen you have finished this run, call the `report_done` MCP \
         tool to signal completion — the continuous-task scheduler waits for that \
         signal before starting the next run.",
        prompt
    )
}

/// Returns `{fired:true, fire_token, session_uid, run_mode:"fresh"|"persistent"}`
/// on a fire, else `{fired:false, reason:"busy"|"duplicate_fire_token"|"paused"}`.
pub fn trigger(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    params: &Value,
) -> MethodResult {
    let p: TriggerParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("trigger params: {}", e)))?;

    // Containment-safe task_id allowlist BEFORE any load / path build.
    crate::continuous::task::validate_task_id(&p.task_id)
        .map_err(|e| (ErrorCode::InvalidParams, format!("trigger: {}", e)))?;

    // Session-caller scope gate (Phase 2: self-or-descendant only — the
    // downstream-allowlist fan-out edge is Phase 6). Operator callers bypass:
    // their token was already validated at dispatch. Capture the caller's PTY
    // size in the SAME brief lock so a Session-fired task inherits the caller's
    // width; an Operator/headless fire has no terminal → 80×24 (same default as
    // start_workflow). A taskless Session caller (own_task None) fails the gate,
    // identical to start_workflow.
    let caller_size: Option<(u16, u16)> = match caller {
        Caller::Operator(_) => None,
        Caller::Session(s) => {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            let sess = state.sessions.get(&s.session_uid);
            let own_task = sess.and_then(|x| x.task_id.clone());
            let size = sess.map(|x| (x.last_cols, x.last_rows));
            let ok = own_task.as_deref().map_or(false, |own| {
                crate::control::auth::task_is_self_or_descendant_of(
                    &state.task_tree,
                    &p.task_id,
                    own,
                )
            });
            if !ok {
                return Err((
                    ErrorCode::Unauthorized,
                    format!(
                        "trigger: continuous task '{}' is not the caller's task or a descendant",
                        p.task_id
                    ),
                ));
            }
            size
        }
    };

    // Load the durable record (None → NotFound).
    let task = crate::continuous::task::load_one(&p.task_id).ok_or((
        ErrorCode::NotFound,
        format!("trigger: continuous task '{}' not found", p.task_id),
    ))?;

    // A paused task doesn't fire — a clean skip, not an error.
    if task.paused {
        return Ok(json!({ "fired": false, "reason": "paused" }));
    }

    // Resolve the prompt: explicit `prompt` > `modes[mode].prompt` >
    // `default_prompt`. Continuity across fires is the per-task NOTES.md (the
    // default prompt instructs read-NOTES-first) — Phase 2 spawns a fresh
    // session each fire and relies on that file, not a live process. `args` is
    // accepted but NOT parsed here (reserved for later phases that thread it to
    // the agent).
    let resolved_prompt = if let Some(prompt) = p.prompt.as_deref() {
        prompt.to_string()
    } else if let Some(mode) = p.mode.as_deref() {
        match task.modes.get(mode) {
            Some(preset) => preset.prompt.clone(),
            None => {
                return Err((
                    ErrorCode::InvalidParams,
                    format!(
                        "trigger: mode '{}' is not defined on task '{}'",
                        mode, p.task_id
                    ),
                ));
            }
        }
    } else {
        task.default_prompt.clone()
    };
    // Auto-signal completion for FRESH agent runs so a periodic task re-fires
    // (no per-skill `report_done` footgun). No-op for bash / persistent.
    let resolved_prompt = with_completion_instruction(resolved_prompt, task.engine, task.run_mode);

    // Provenance label for the run record + audit line.
    let trigger_source = match caller {
        Caller::Operator(_) => "operator".to_string(),
        Caller::Session(s) => format!("session:{}", s.session_uid),
    };

    let is_persistent = task.run_mode == crate::continuous::task::RunMode::Persistent;

    // PERSISTENT liveness + delivery-handle resolution (the ONLY DaemonState lock
    // the executor takes before a spawn/PTY write; dropped immediately). For a
    // pinned session that is still LIVE we deliver to its existing PTY and REUSE
    // its uid — so the in_flight guard + last_run identity stay the live uid
    // (Phase-3 restart reconciliation probes in_flight.session_uid for liveness;
    // pointing it at a never-spawned minted uid would make reconciliation orphan
    // a healthy fire). A dead/absent pinned session resolves to None → a FRESH
    // respawn with a freshly-minted uid below.
    //
    //   `Some((uid, Some(handle)))` = live, deliver to handle (production);
    //   `Some((uid, None))`         = live per the delivery test spy (no real PTY);
    //   `None`                      = not persistent, or dead/unset → FRESH spawn.
    let persistent_target: Option<(String, Option<crate::session::InputHandle>)> =
        if is_persistent {
            task.current_session_uid.as_deref().and_then(|uid| {
                #[cfg(test)]
                if CONTINUOUS_DELIVERY_SPY.with(|c| c.borrow().is_some()) {
                    return Some((uid.to_string(), None));
                }
                let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                match state.sessions.get(uid) {
                    // Dead-session predicate: registry-absent OR the reaper has
                    // populated kernel exit (kernel_set flips before on_exit
                    // removes the registry entry — no liveness gap).
                    Some(s) if !s.last_exit.kernel_set() => {
                        Some((uid.to_string(), Some(s.input_handle())))
                    }
                    _ => None,
                }
            })
        } else {
            None
        };

    // Resolve the per-trigger SESSION uid: a PERSISTENT live fire reuses the
    // pinned session's uid (no new spawn); FRESH and the PERSISTENT dead->respawn
    // fallback mint a fresh uid for the new session. It lands in the in_flight
    // guard, the spawn params (respawn), AND last_run.session_uid (one identity).
    let session_uid = match &persistent_target {
        Some((uid, _)) => uid.clone(),
        None => new_daemon_minted_session_uid(),
    };
    // Accept the caller's idempotency key, else mint a fresh one.
    let fire_token = p.fire_token.clone().unwrap_or_else(new_fire_token);

    // Atomic inside-flock check-and-set (no TOCTOU): under the exclusive
    // per-task flock, reject a concurrent fire (`in_flight` already set) or a
    // duplicate idempotency key (== last_run.fire_token), else arm the
    // spawn-window guard. Busy takes precedence over duplicate, matching the
    // pinned contract. A freshly-minted fire_token is unique by construction,
    // so the duplicate branch only ever fires for a CALLER-supplied token.
    let started_at = crate::continuous::task::now_unix();
    let armed = crate::continuous::task::try_modify::<_, FireAbort>(&p.task_id, |t| {
        if t.in_flight.is_some() {
            return Err(FireAbort::Busy);
        }
        if let Some(last) = &t.last_run {
            if last.fire_token == fire_token {
                return Err(FireAbort::DuplicateFireToken);
            }
        }
        t.in_flight = Some(crate::continuous::task::InFlight {
            fire_token: fire_token.clone(),
            session_uid: session_uid.clone(),
            started_at,
        });
        Ok(())
    });
    match armed {
        crate::continuous::task::TryModifyOutcome::Ok(_) => {}
        crate::continuous::task::TryModifyOutcome::Aborted(FireAbort::Busy) => {
            return Ok(json!({ "fired": false, "reason": "busy" }));
        }
        crate::continuous::task::TryModifyOutcome::Aborted(FireAbort::DuplicateFireToken) => {
            return Ok(json!({ "fired": false, "reason": "duplicate_fire_token" }));
        }
        crate::continuous::task::TryModifyOutcome::Persist(e) => {
            return Err((
                ErrorCode::Internal,
                format!("trigger: arm in_flight for '{}': {}", p.task_id, e),
            ));
        }
    }

    // The new run's 1-based sequence number (mirrors run_count+1). Used for both
    // the audit line and the persisted RunRecord. Stable from here: the
    // in_flight guard blocks any concurrent fire from advancing run_count.
    let seq = task.run_count as u64 + 1;

    // run_mode label for the run record, audit line, and response.
    let run_mode_label = if is_persistent { "persistent" } else { "fresh" };

    // ---- Executor (branched on run_mode) -----------------------------------
    // Lock discipline: NO DaemonState mutex and NO continuous-task flock is held
    // across a spawn or a prompt-delivery PTY write — `start_session` re-acquires
    // the state lock internally and the reaper's on_exit callback re-acquires it
    // too; the try_modify above already released the flock.
    match persistent_target {
        // PERSISTENT, pinned session ALIVE: deliver the prompt to the EXISTING
        // PTY (no respawn — prior context preserved). Auto-`/clear`-compact every
        // `compact_every` runs. The handle was already cloned out under the brief
        // liveness-probe lock above, so no lock is held here.
        Some((live_uid, handle_opt)) => {
            // The run just armed is `seq` (== run_count+1). compact-after-N gates
            // on it so the Nth, 2Nth, … fire `/clear`s before delivering.
            let compact =
                matches!(task.compact_every, Some(n) if n > 0 && seq % n as u64 == 0);
            // Under the delivery test spy `handle_opt` is None — record the
            // delivery in lieu of a PTY write (production always carries a handle
            // in the live arm).
            #[cfg(test)]
            if handle_opt.is_none() {
                CONTINUOUS_DELIVERY_SPY.with(|c| {
                    if let Some(v) = c.borrow_mut().as_mut() {
                        v.push(("persistent".to_string(), live_uid.clone(), compact));
                    }
                });
            }
            if let Some(handle) = handle_opt {
                spawn_persistent_prompt_delivery(
                    handle,
                    live_uid.clone(),
                    resolved_prompt.clone(),
                    compact,
                );
            }
        }
        // FRESH, or PERSISTENT dead->respawn: spawn a NEW session (compose params
        // tagged with `continuous_task_id`, worktree-pinned, memory-capped, via
        // the `start_session` choke point), then deliver. FRESH leaves prior idle
        // sessions ALONE; the persistent respawn rebinds `current_session_uid` to
        // the new uid in the record step below.
        None => {
            let engine = task.engine.as_session_type();
            let (cols, rows) = caller_size.unwrap_or((80, 24));
            let working_dir = PathBuf::from(&task.worktree_path);

            let full = match compose_continuous_spawn_params(
                state_arc,
                &session_uid,
                &task.workspace_id,
                &task.label,
                engine,
                &working_dir,
                // Session task_id: the backing planning UUID when set (so a
                // subtask-spawning orchestrator has a real planning parent for
                // create_subtask), else the continuous slug (legacy behavior).
                // The continuous_task_id tag below stays the slug regardless.
                Some(task.planning_task_id.as_deref().unwrap_or(&task.task_id)),
                &task.task_id,
                task.mem_cap_bytes,
                cols,
                rows,
            ) {
                Ok(f) => f,
                Err(e) => {
                    continuous_clear_in_flight_after_failure(
                        &p.task_id,
                        seq,
                        &fire_token,
                        &session_uid,
                        run_mode_label,
                        &trigger_source,
                        &e.1,
                    );
                    return Err(e);
                }
            };

            if let Err(e) = continuous_fresh_spawn(state_arc, &full) {
                continuous_clear_in_flight_after_failure(
                    &p.task_id,
                    seq,
                    &fire_token,
                    &session_uid,
                    run_mode_label,
                    &trigger_source,
                    &e.1,
                );
                return Err(e);
            }

            // Deliver the resolved prompt to the freshly-spawned session. Clone
            // the input handle under a BRIEF state lock, drop it, THEN hand off to
            // the detached delivery thread (settle → bracketed-paste body → gap →
            // kitty Enter). A bare `\n` does NOT submit a claude-code/codex kitty
            // TUI. Best-effort: a vanished session (fast-exit reaper removed the
            // registry entry) skips delivery — the fire already counted and the
            // caller has its uid.
            let handle_opt = {
                let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                state.sessions.get(&session_uid).map(|s| s.input_handle())
            };
            if let Some(handle) = handle_opt {
                spawn_agent_prompt_delivery(handle, session_uid.clone(), resolved_prompt.clone());
            }
        }
    }

    // Record the fire + CLEAR the spawn-window guard in one atomic modify.
    // Phase 2's in_flight is a spawn-window guard ONLY — cleared as the trigger
    // returns (the delivery thread is detached); whole-run tracking is Phase 3.
    if let Err(e) = crate::continuous::task::modify(&p.task_id, |t| {
        t.last_run = Some(crate::continuous::task::RunRecord {
            seq,
            fire_token: fire_token.clone(),
            started_at,
            finished_at: None,
            session_uid: Some(session_uid.clone()),
            status: crate::continuous::task::RunStatus::Running,
            trigger_source: trigger_source.clone(),
        });
        t.current_session_uid = Some(session_uid.clone());
        t.run_count = t.run_count.saturating_add(1);
        t.last_fired_at = started_at;
        t.in_flight = None;
        // Phase 3b: every new FRESH fire starts a clean stuck-story. The
        // watchdog's cap counter and any prior investigator binding belong to
        // the run that just ended, not this one — reset them here (the single
        // record step both fresh and persistent fires funnel through; for a
        // persistent task these are inert no-ops, the watchdog is Fresh-only).
        t.investigation_count = 0;
        t.investigator_uid = None;
        t.investigator_started_at = None;
    }) {
        // The session is already spawned + the prompt delivering; failing to
        // persist last_run only leaks the in_flight guard. Log loudly — the
        // fire DID happen, so we still report fired:true below.
        eprintln!(
            "cm-daemon: trigger spawned '{}' but failed to record last_run / clear in_flight: {}",
            p.task_id, e
        );
    }

    // Append the audit line (best-effort; the fire already happened).
    if let Err(e) =
        crate::continuous::runlog::ContinuousRunLog::append(&crate::continuous::runlog::RunLogLine {
            seq,
            ts: runlog_now_ts(),
            task_id: p.task_id.clone(),
            event: "fired".to_string(),
            fire_token: Some(fire_token.clone()),
            session_uid: Some(session_uid.clone()),
            run_mode: Some(run_mode_label.to_string()),
            trigger_source: Some(trigger_source.clone()),
            status: Some("running".to_string()),
            // Carry the caller's free-form `args` blob into the audit line
            // unparsed — the daemon does NOT interpret it (later phases thread
            // it to the agent). `None` when the caller sent no args.
            detail: p.args.clone(),
        })
    {
        eprintln!(
            "cm-daemon: trigger runlog append failed for '{}': {}",
            p.task_id, e
        );
    }

    Ok(json!({
        "fired": true,
        "fire_token": fire_token,
        "session_uid": session_uid,
        "run_mode": run_mode_label,
    }))
}

// ===================================================================
// Continuous Tasks — Phase 3b (stuck-story state machine: completion
// signal + investigator verdict). DESIGN_CONTINUOUS_TASKS.md §11.
//
// `report_done` (the continuous agent's explicit "my run is done") and
// the clean-exit hook in `handle_session_exit` are the ONLY two signals
// that clear an ACTIVE fresh run (last_run.status Running → Done); there
// is deliberately NO idle-after-output auto-Done heuristic (an
// idle-but-wedged trust-dialog hang must stay Running so the scheduler
// watchdog catches it). `resolve_stuck` is the daemon-spawned
// investigator's verdict (mark_unstuck / restart / escalate), and
// `escalate_stuck` is the shared kill+Stuck+surface helper reused by
// both `resolve_stuck`'s escalate action and the scheduler watchdog's
// cap-reached auto-escalate.
//
// NOTIFY-THE-USER for v1 is the durable failure-surfacing trio
// (DESIGN §11), NOT an active desktop push: there is no daemon-side
// `notify_user` RPC (it is TUI-only, routed to ~/.cm/tui.sock). See
// `escalate_stuck` for the trio.
// ===================================================================

#[derive(Deserialize, Default)]
struct ReportDoneParams {
    #[serde(default)]
    reason: Option<String>,
}

/// `report_done(reason?)` — Session-callable completion signal for a
/// continuous-task agent (DESIGN_CONTINUOUS_TASKS.md §11). The daemon resolves
/// WHICH task/run the caller is from its own session tag (no `task_id` on the
/// wire), then flips the active run `Running → Done` IFF the caller owns it.
///
/// Auth + resolution: Operator callers are rejected (a continuous tick is always
/// a Session). The task is resolved from the CALLER session's
/// `continuous_task_id` read DIRECTLY off `state.sessions` — NOT
/// `lookup_session_any`, whose `SessionViewAny` drops the field. A caller with
/// no `continuous_task_id` is Unauthorized.
///
/// The mark is DOUBLE-guarded: it fires only when
/// `last_run.session_uid == caller_uid` AND `last_run.status == Running`. Any
/// other state (a stale/older run, an already-finished run, a sibling run) is a
/// SOFT no-op that returns `Ok({done:false, …})` with a clear message — NEVER an
/// error, so an agent calling it on a superseded run gets a clean signal. Clears
/// nothing else (no in_flight / current_session_uid / run_count touch).
pub fn report_done(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    params: &Value,
) -> MethodResult {
    let p: ReportDoneParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("report_done params: {}", e)))?;

    // Session-callable only — an Operator frame carries no continuous identity.
    let caller_uid = match caller {
        Caller::Session(s) => s.session_uid.clone(),
        Caller::Operator(_) => {
            return Err((
                ErrorCode::Unauthorized,
                "report_done is Session-callable only (a continuous-task agent reports its own run)"
                    .into(),
            ));
        }
    };

    // Resolve the task from the caller session's continuous_task_id (read the
    // DaemonSession field directly; SessionViewAny omits it). Brief lock, dropped
    // before the disk modify.
    let ct_id = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        state
            .sessions
            .get(&caller_uid)
            .and_then(|s| s.continuous_task_id.clone())
    };
    let ct_id = match ct_id {
        Some(id) => id,
        None => {
            return Err((
                ErrorCode::Unauthorized,
                format!(
                    "report_done: caller session '{}' is not a continuous-task tick \
                     (no continuous_task_id)",
                    caller_uid
                ),
            ));
        }
    };

    // Flip Running → Done IFF the caller owns the active run. `modify` runs the
    // closure under the per-task flock (no DaemonState lock held here).
    let now = crate::continuous::task::now_unix();
    let mut marked = false;
    let updated = match crate::continuous::task::modify(&ct_id, |t| {
        if let Some(run) = t.last_run.as_mut() {
            if run.session_uid.as_deref() == Some(caller_uid.as_str())
                && matches!(run.status, crate::continuous::task::RunStatus::Running)
            {
                run.status = crate::continuous::task::RunStatus::Done;
                run.finished_at = Some(now);
                marked = true;
            }
        }
    }) {
        Ok(t) => t,
        Err(e) => {
            return Err((
                ErrorCode::Internal,
                format!("report_done: persist '{}': {}", ct_id, e),
            ));
        }
    };

    // Audit line only on a real completion (the no-op case is silent).
    if marked {
        let (seq, fire_token, session_uid) = updated
            .last_run
            .as_ref()
            .map(|r| (r.seq, Some(r.fire_token.clone()), r.session_uid.clone()))
            .unwrap_or((0, None, None));
        if let Err(e) = crate::continuous::runlog::ContinuousRunLog::append(
            &crate::continuous::runlog::RunLogLine {
                seq,
                ts: runlog_now_ts(),
                task_id: ct_id.clone(),
                event: "report_done".to_string(),
                fire_token,
                session_uid,
                run_mode: Some("fresh".to_string()),
                trigger_source: Some(format!("session:{}", caller_uid)),
                status: Some("done".to_string()),
                detail: p.reason.clone().map(Value::String),
            },
        ) {
            eprintln!(
                "cm-daemon: report_done runlog append failed for '{}': {}",
                ct_id, e
            );
        }
    }

    let message = if marked {
        "run marked done"
    } else {
        "no-op: caller is not the active run's session, or the run is no longer Running"
    };
    Ok(json!({
        "ok": true,
        "done": marked,
        "task_id": ct_id,
        "message": message,
    }))
}

#[derive(Deserialize)]
struct ResolveStuckParams {
    task_id: String,
    seq: u64,
    action: String,
    #[serde(default)]
    reason: Option<String>,
}

/// `resolve_stuck(task_id, seq, action, reason?)` — the daemon-spawned
/// investigator's verdict on a stuck FRESH run (DESIGN_CONTINUOUS_TASKS.md §11).
/// Session-callable ONLY (the investigator is always a Session).
///
/// Auth (two gates): the caller session's `continuous_task_id` (read directly
/// off `state.sessions`, NOT `lookup_session_any`) MUST equal `task_id`, AND the
/// caller MUST be the task's CURRENT `investigator_uid`. The investigator is a
/// DISTINCT session from the stuck worker, so it cannot self-resolve from its
/// own tag alone — hence the explicit `task_id` + `seq` params.
///
/// Actions (each clears `investigator_uid` so the watchdog can re-engage):
///   * `mark_unstuck` — the run is slow-but-real. Reset `last_run.started_at =
///     now` (extend the watchdog clock); the original stuck session keeps
///     running (NOT killed).
///   * `restart` — wedged but the task is sound. Kill the stuck session
///     (kill_session semantics) → clear `investigator_uid` → re-fire a brand-new
///     FRESH run via `methods::trigger` (Operator caller, fresh fire_token). The
///     kill-then-trigger ORDER is load-bearing: the re-fire mints a new last_run
///     (new uid + seq) so the killed session's clean-exit guard mismatches.
///   * `escalate` — needs a human. Delegates to [`escalate_stuck`] (kill + Stuck
///     + surface), threading the caller's `reason`.
///
/// LOCK DISCIPLINE: no DaemonState lock is held across `kill_session` /
/// `trigger` (both re-acquire it internally).
pub fn resolve_stuck(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    params: &Value,
) -> MethodResult {
    let p: ResolveStuckParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("resolve_stuck params: {}", e)))?;

    // Containment-safe task_id allowlist BEFORE any load / path build.
    crate::continuous::task::validate_task_id(&p.task_id)
        .map_err(|e| (ErrorCode::InvalidParams, format!("resolve_stuck: {}", e)))?;

    // Session-callable only.
    let caller_uid = match caller {
        Caller::Session(s) => s.session_uid.clone(),
        Caller::Operator(_) => {
            return Err((
                ErrorCode::Unauthorized,
                "resolve_stuck is Session-callable only (the investigator renders the verdict)"
                    .into(),
            ));
        }
    };

    // Gate 1: the caller session must be a tick of THIS continuous task. Read the
    // DaemonSession field directly (SessionViewAny omits continuous_task_id).
    let caller_ct_id = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        state
            .sessions
            .get(&caller_uid)
            .and_then(|s| s.continuous_task_id.clone())
    };
    if caller_ct_id.as_deref() != Some(p.task_id.as_str()) {
        return Err((
            ErrorCode::Unauthorized,
            format!(
                "resolve_stuck: caller session '{}' is not a tick of continuous task '{}'",
                caller_uid, p.task_id
            ),
        ));
    }

    // Gate 2: the caller must be the task's CURRENT investigator.
    let task = crate::continuous::task::load_one(&p.task_id).ok_or((
        ErrorCode::NotFound,
        format!("resolve_stuck: continuous task '{}' not found", p.task_id),
    ))?;
    if task.investigator_uid.as_deref() != Some(caller_uid.as_str()) {
        return Err((
            ErrorCode::Unauthorized,
            format!(
                "resolve_stuck: caller '{}' is not the current investigator of task '{}'",
                caller_uid, p.task_id
            ),
        ));
    }

    let reason = p.reason.clone().unwrap_or_default();
    match p.action.as_str() {
        "mark_unstuck" => {
            // Extend the watchdog clock + release the investigation; the stuck
            // session keeps running.
            let now = crate::continuous::task::now_unix();
            if let Err(e) = crate::continuous::task::modify(&p.task_id, |t| {
                if let Some(run) = t.last_run.as_mut() {
                    run.started_at = now;
                }
                t.investigator_uid = None;
            }) {
                return Err((
                    ErrorCode::Internal,
                    format!("resolve_stuck mark_unstuck persist '{}': {}", p.task_id, e),
                ));
            }
            append_resolve_stuck_runlog(&task, p.seq, "unstuck", None);
            Ok(json!({
                "ok": true,
                "action": "mark_unstuck",
                "task_id": p.task_id,
                "seq": p.seq,
            }))
        }
        "restart" => {
            // Kill the stuck session (kill_session semantics; caller_uid=None
            // bypasses the descendant auth gate for this internal kill). A
            // not-found / already-dead session is fine — we still re-fire.
            if let Some(stuck_uid) =
                task.last_run.as_ref().and_then(|r| r.session_uid.clone())
            {
                if let Err((code, msg)) =
                    kill_session(state_arc, &json!({ "session_uid": stuck_uid }), None)
                {
                    eprintln!(
                        "cm-daemon: resolve_stuck restart kill of '{}' for '{}' \
                         failed (continuing to re-fire): {:?}: {}",
                        stuck_uid, p.task_id, code, msg
                    );
                }
            }
            // Clear the investigation BEFORE the re-fire. (The new fresh fire's
            // record step is what resets investigation_count per the new-fire
            // rule — owned by the watchdog slice's trigger edit.)
            if let Err(e) = crate::continuous::task::modify(&p.task_id, |t| {
                t.investigator_uid = None;
            }) {
                return Err((
                    ErrorCode::Internal,
                    format!(
                        "resolve_stuck restart clear investigator '{}': {}",
                        p.task_id, e
                    ),
                ));
            }
            append_resolve_stuck_runlog(&task, p.seq, "restarted", None);
            // Re-fire a brand-new FRESH run. Operator caller (bypasses the
            // self-or-descendant scope gate); None fire_token → trigger mints a
            // fresh one (never trips the duplicate-fire guard).
            let refire = trigger(
                state_arc,
                &Caller::operator("continuous-resolve-stuck"),
                &json!({ "task_id": p.task_id }),
            )?;
            Ok(json!({
                "ok": true,
                "action": "restart",
                "task_id": p.task_id,
                "seq": p.seq,
                "refire": refire,
            }))
        }
        "escalate" => {
            escalate_stuck(state_arc, &task, p.seq, &reason)?;
            Ok(json!({
                "ok": true,
                "action": "escalate",
                "task_id": p.task_id,
                "seq": p.seq,
            }))
        }
        other => Err((
            ErrorCode::InvalidParams,
            format!(
                "resolve_stuck: unknown action '{}' (want mark_unstuck|restart|escalate)",
                other
            ),
        )),
    }
}

/// Shared auto-escalate helper for a stuck FRESH run (DESIGN_CONTINUOUS_TASKS.md
/// §11). Reused by [`resolve_stuck`]'s `escalate` action AND the scheduler
/// watchdog's cap-reached auto-escalate (the scheduler calls this with the
/// loaded task, the run `seq`, and `reason = "max_investigations"`).
///
/// Steps:
///   1. KILL the stuck session via kill_session semantics
///      (`mark_operator_kill_requested` + `session.kill`, LEAVING the registry
///      entry so the reaper broadcasts `ManifestDiff::Exited` + records the
///      tombstone — NEVER a bare `state.sessions.remove`). `caller_uid=None`
///      bypasses the descendant auth gate. A not-found / already-dead session is
///      logged-and-continued (the Stuck flip is the durable surfacing).
///   2. Flip `last_run.status → Stuck` + clear `investigator_uid`
///      UNCONDITIONALLY (escalate is NOT Running-guarded). The kill's async
///      reaper runs `handle_session_exit`, whose clean-exit Done-write IS
///      Running-guarded — so both orderings converge to Stuck regardless of
///      which write wins the per-task flock.
///   3. Append a `runs.jsonl {event:"escalated", detail:{reason}}` audit line.
///
/// NOTIFY-THE-USER: this trio (Stuck status in state.json → continuous.list red
/// glyph; the `ManifestDiff::Exited` broadcast the kill emits → manifest.watch,
/// the session carries `continuous_task_id`; the runs.jsonl escalated line) IS
/// the daemon's "notify" for v1. There is NO daemon-side `notify_user` RPC (it
/// is TUI-only). An ACTIVE push (desktop notification) would be a NEW daemon
/// channel and is out of scope.
///
/// LOCK DISCIPLINE: takes NO DaemonState lock itself — `kill_session` (and the
/// reaper's on_exit) re-lock internally, so a caller (the scheduler tick) MUST
/// NOT hold the DaemonState lock across this call. The disk modify + runlog are
/// flock-only.
pub fn escalate_stuck(
    state_arc: &Arc<Mutex<DaemonState>>,
    task: &crate::continuous::task::ContinuousTask,
    seq: u64,
    reason: &str,
) -> Result<(), (ErrorCode, String)> {
    // 1. Kill the stuck session (best-effort).
    if let Some(stuck_uid) = task.last_run.as_ref().and_then(|r| r.session_uid.clone()) {
        if let Err((code, msg)) =
            kill_session(state_arc, &json!({ "session_uid": stuck_uid }), None)
        {
            eprintln!(
                "cm-daemon: escalate_stuck kill of '{}' for task '{}' \
                 failed (continuing to mark Stuck): {:?}: {}",
                stuck_uid, task.task_id, code, msg
            );
        }
    }

    // 2. Flip last_run → Stuck + clear investigator_uid, UNCONDITIONALLY.
    if let Err(e) = crate::continuous::task::modify(&task.task_id, |t| {
        if let Some(run) = t.last_run.as_mut() {
            run.status = crate::continuous::task::RunStatus::Stuck;
        }
        t.investigator_uid = None;
    }) {
        return Err((
            ErrorCode::Internal,
            format!("escalate_stuck: persist Stuck for '{}': {}", task.task_id, e),
        ));
    }

    // 3. Audit line (best-effort).
    if let Err(e) = crate::continuous::runlog::ContinuousRunLog::append(
        &crate::continuous::runlog::RunLogLine {
            seq,
            ts: runlog_now_ts(),
            task_id: task.task_id.clone(),
            event: "escalated".to_string(),
            fire_token: task.last_run.as_ref().map(|r| r.fire_token.clone()),
            session_uid: task.last_run.as_ref().and_then(|r| r.session_uid.clone()),
            run_mode: Some("fresh".to_string()),
            trigger_source: Some("continuous-watchdog".to_string()),
            status: Some("stuck".to_string()),
            detail: Some(json!({ "reason": reason })),
        },
    ) {
        eprintln!(
            "cm-daemon: escalate_stuck runlog append failed for '{}': {}",
            task.task_id, e
        );
    }

    Ok(())
}

/// Abandon an investigator that has blown its OWN runtime budget
/// (`[scheduler] default_investigator_runtime_secs`). Kill it via `kill_session`
/// semantics (leave-in-registry → the reaper broadcasts `Exited`), clear the
/// investigator binding, and audit. The watchdog then re-evaluates the still-
/// stuck run by `investigation_count` on the next tick — spawning a fresh
/// investigator if under the cap, else auto-escalating. Without this a wedged-
/// but-alive investigator (one that never calls `resolve_stuck` and never exits)
/// would pin the stuck run forever: the watchdog's live-investigator branch
/// would `continue` every tick and the run would never escalate. Best-effort
/// (a failed kill/clear/append is logged, not propagated).
pub(crate) fn abandon_timed_out_investigator(
    state_arc: &Arc<Mutex<DaemonState>>,
    task: &crate::continuous::task::ContinuousTask,
    seq: u64,
    investigator_uid: &str,
) {
    if let Err((code, msg)) =
        kill_session(state_arc, &json!({ "session_uid": investigator_uid }), None)
    {
        eprintln!(
            "cm-daemon: abandon_timed_out_investigator kill of '{}' for task '{}' \
             failed (continuing to clear binding): {:?}: {}",
            investigator_uid, task.task_id, code, msg
        );
    }
    if let Err(e) = crate::continuous::task::modify(&task.task_id, |t| {
        t.investigator_uid = None;
        t.investigator_started_at = None;
    }) {
        eprintln!(
            "cm-daemon: abandon_timed_out_investigator: clear investigator for '{}': {}",
            task.task_id, e
        );
    }
    if let Err(e) = crate::continuous::runlog::ContinuousRunLog::append(
        &crate::continuous::runlog::RunLogLine {
            seq,
            ts: runlog_now_ts(),
            task_id: task.task_id.clone(),
            event: "investigator_timeout".to_string(),
            fire_token: task.last_run.as_ref().map(|r| r.fire_token.clone()),
            session_uid: Some(investigator_uid.to_string()),
            run_mode: Some("fresh".to_string()),
            trigger_source: Some("continuous-watchdog".to_string()),
            status: None,
            detail: None,
        },
    ) {
        eprintln!(
            "cm-daemon: abandon_timed_out_investigator runlog append failed for '{}': {}",
            task.task_id, e
        );
    }
}

/// Append a `runs.jsonl` audit line for a `resolve_stuck` action (`"unstuck"` /
/// `"restarted"`). Best-effort: a failed append is logged, not propagated (the
/// state mutation already landed). Carries the run's fire_token + session_uid
/// from the pre-action task snapshot for correlation.
fn append_resolve_stuck_runlog(
    task: &crate::continuous::task::ContinuousTask,
    seq: u64,
    event: &str,
    detail: Option<Value>,
) {
    if let Err(e) = crate::continuous::runlog::ContinuousRunLog::append(
        &crate::continuous::runlog::RunLogLine {
            seq,
            ts: runlog_now_ts(),
            task_id: task.task_id.clone(),
            event: event.to_string(),
            fire_token: task.last_run.as_ref().map(|r| r.fire_token.clone()),
            session_uid: task.last_run.as_ref().and_then(|r| r.session_uid.clone()),
            run_mode: Some("fresh".to_string()),
            trigger_source: Some("continuous-investigator".to_string()),
            status: None,
            detail,
        },
    ) {
        eprintln!(
            "cm-daemon: resolve_stuck runlog append ({}) failed for '{}': {}",
            event, task.task_id, e
        );
    }
}

/// Copy the evidence for a stuck FRESH run into
/// `~/.cm/continuous-tasks/<task_id>/stuck/<seq>/` so the spawned investigator
/// (and a human) can see what the run was doing when the watchdog declared it
/// stuck (DESIGN_CONTINUOUS_TASKS.md §11). Returns the snapshot dir path — the
/// scheduler watchdog threads it into [`spawn_investigator`].
///
/// Three BEST-EFFORT artifacts (a missing source is fine — a trust-dialog hang
/// may never have produced a transcript, and a task may have no NOTES.md):
///   1. the run's transcript jsonl — copied ONLY when `transcript_path` is
///      `Some(_)` AND the source file exists. Named after the source file (the
///      claude transcript UUID) so snapshots of different runs don't collide;
///      `transcript.jsonl` is the fallback name when the path has no file_name.
///   2. `NOTES.md` from the task's worktree (the cross-fire continuity file).
///   3. `metadata.json` — the RunRecord + `max_runtime_secs` + `elapsed_secs` +
///      `reason` (so the investigator has the run identity + watchdog verdict
///      context without parsing the transcript).
///
/// LOCK DISCIPLINE: takes NO DaemonState lock — the watchdog resolves the stuck
/// session's `transcript_path` under its brief liveness-probe lock and threads
/// it in (collect-then-act; pure fs I/O here). Each copy is logged-and-continued,
/// NOT propagated: a partial snapshot beats none, and the investigator degrades
/// gracefully on a missing file. Writes NEITHER runs.jsonl NOR state.json —
/// those mutations belong to [`spawn_investigator`] / the watchdog.
pub(crate) fn snapshot_stuck_run(
    task: &crate::continuous::task::ContinuousTask,
    seq: u64,
    transcript_path: Option<&std::path::Path>,
    elapsed_secs: u64,
    reason: &str,
) -> std::path::PathBuf {
    let dir = crate::continuous::task::task_dir(&task.task_id)
        .join("stuck")
        .join(seq.to_string());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "cm-daemon: snapshot_stuck_run mkdir {} failed: {}",
            dir.display(),
            e
        );
    }

    // 1. Transcript jsonl (only if a path was resolved AND it exists on disk).
    if let Some(src) = transcript_path {
        if src.exists() {
            let dest = match src.file_name() {
                Some(name) => dir.join(name),
                None => dir.join("transcript.jsonl"),
            };
            if let Err(e) = std::fs::copy(src, &dest) {
                eprintln!(
                    "cm-daemon: snapshot_stuck_run copy transcript {} -> {} failed: {}",
                    src.display(),
                    dest.display(),
                    e
                );
            }
        }
    }

    // 2. The worktree's NOTES.md (cross-fire continuity), if present.
    let notes_src = PathBuf::from(&task.worktree_path).join("NOTES.md");
    if notes_src.exists() {
        let dest = dir.join("NOTES.md");
        if let Err(e) = std::fs::copy(&notes_src, &dest) {
            eprintln!(
                "cm-daemon: snapshot_stuck_run copy NOTES.md {} -> {} failed: {}",
                notes_src.display(),
                dest.display(),
                e
            );
        }
    }

    // 3. metadata.json — the run identity + watchdog verdict context.
    let meta = json!({
        "task_id": task.task_id,
        "seq": seq,
        "run": task.last_run,
        "max_runtime_secs": task.max_runtime_secs,
        "elapsed_secs": elapsed_secs,
        "reason": reason,
    });
    match serde_json::to_string_pretty(&meta) {
        Ok(s) => {
            if let Err(e) = std::fs::write(dir.join("metadata.json"), s) {
                eprintln!(
                    "cm-daemon: snapshot_stuck_run write metadata.json in {} failed: {}",
                    dir.display(),
                    e
                );
            }
        }
        Err(e) => {
            eprintln!("cm-daemon: snapshot_stuck_run serialize metadata failed: {}", e)
        }
    }

    dir
}

/// The daemon-constructed prompt handed to a freshly-spawned investigator. It
/// points the agent at the snapshot dir + the worktree, enumerates the three
/// verdicts, and pins the EXACT `resolve_stuck(...)` call shape — the
/// investigator is a DISTINCT session from the stuck worker, so it must pass
/// `task_id` + `seq` explicitly (it can't self-resolve from its own tag). Built
/// as a multi-line string so [`spawn_agent_prompt_delivery`] wraps it in
/// bracketed-paste (a bare newline would submit the prompt early).
fn investigator_prompt(
    task: &crate::continuous::task::ContinuousTask,
    seq: u64,
    snapshot_dir: &std::path::Path,
) -> String {
    let budget = task
        .max_runtime_secs
        .map(|s| format!("{}s", s))
        .unwrap_or_else(|| "its configured budget".to_string());
    format!(
        "You are the stuck-run investigator for continuous task \"{label}\" (task_id={task_id}).\n\
         \n\
         Run #{seq} of this task has exceeded its runtime budget ({budget}) and looks stuck: it \
         is still alive but has not finished or called report_done.\n\
         \n\
         A snapshot of the run's evidence has been copied to:\n\
         {snapshot}\n\
         Read EVERYTHING in that directory: metadata.json (the run record, how long it has been \
         running, and why it was flagged), the transcript .jsonl (the agent's conversation so far, \
         if one was captured), and NOTES.md (the task's cross-run continuity notes, if present).\n\
         \n\
         Also inspect the live worktree at:\n\
         {worktree}\n\
         Run `git status` and `git diff` there to judge whether the stuck run has made real, sound \
         progress on disk.\n\
         \n\
         Then decide EXACTLY ONE verdict:\n\
         - mark_unstuck: the run is making real progress, just slow. Keep it running and reset its \
         watchdog clock.\n\
         - restart: the run is wedged but the task itself is sound. Kill it and start a fresh run.\n\
         - escalate: the run needs a human. Stop it and alert the operator.\n\
         \n\
         Render your verdict by calling resolve_stuck EXACTLY ONCE:\n\
         resolve_stuck(task_id=\"{task_id}\", seq={seq}, action=\"mark_unstuck\"|\"restart\"|\"escalate\", reason=\"<one line>\")\n\
         \n\
         After that single call you are done — you may report_done or exit.",
        label = task.label,
        task_id = task.task_id,
        seq = seq,
        budget = budget,
        snapshot = snapshot_dir.display(),
        worktree = task.worktree_path,
    )
}

/// Spawn a FRESH claude investigator session for a stuck run (the scheduler
/// watchdog calls this after [`snapshot_stuck_run`]; DESIGN_CONTINUOUS_TASKS.md
/// §11). A near-verbatim restructure of `trigger`'s FRESH executor arm: mint a
/// uid, compose `start_session` params via the SAME choke point
/// (`compose_continuous_spawn_params` → `continuous_fresh_spawn`), then deliver
/// the daemon-constructed verdict prompt on the detached delivery thread.
///
/// The investigator is ALWAYS a `claude-code` session regardless of the task's
/// own engine (it reads snapshot files + git, not a codex/bash workload),
/// labelled `"investigator"`, tagged with the task's `continuous_task_id` (so
/// `list_sessions` groups it under the same task and the watchdog can find it via
/// `investigator_uid`), and pinned to the task's OWN worktree (two claude
/// sessions sharing one worktree is fine — claude transcripts are per-session
/// UUID files). 80×24: headless, no caller terminal.
///
/// On a successful spawn it RECORDS the investigation: `investigator_uid =
/// Some(uid)` + `investigation_count += 1` via `task::modify`, and appends a
/// `runs.jsonl {event:"stuck", detail:{investigation:N}}` audit line. Returns
/// `Ok({session_uid})`. A spawn failure propagates (the `?`) BEFORE any of those
/// mutations, so a failed spawn never records a phantom investigator.
///
/// LOCK DISCIPLINE: NO DaemonState lock is held across the spawn —
/// `compose_continuous_spawn_params` + `continuous_fresh_spawn` re-lock
/// internally, and the input-handle clone is a SEPARATE brief lock dropped before
/// the PTY write. The disk modify + runlog are flock-only. (The investigator's
/// OWN runtime bound is enforced by the watchdog via `[scheduler]
/// default_investigator_runtime_secs`; `compose_continuous_spawn_params` carries
/// no per-session deadline, so the spawn itself is unbounded.)
pub(crate) fn spawn_investigator(
    state_arc: &Arc<Mutex<DaemonState>>,
    task: &crate::continuous::task::ContinuousTask,
    seq: u64,
    snapshot_dir: &std::path::Path,
) -> MethodResult {
    let uid = new_daemon_minted_session_uid();
    // ALWAYS claude — the investigator reads snapshot files + git state,
    // regardless of the task's own engine (which may be codex/bash).
    let engine = crate::continuous::task::Engine::Claude.as_session_type();
    let working_dir = PathBuf::from(&task.worktree_path);

    // Compose via the continuous spawn choke point (tags continuous_task_id +
    // memory-caps + pins the worktree).
    let full = compose_continuous_spawn_params(
        state_arc,
        &uid,
        &task.workspace_id,
        "investigator",
        engine,
        &working_dir,
        Some(&task.task_id),
        &task.task_id,
        task.mem_cap_bytes,
        80,
        24,
    )?;

    // Spawn through the start_session choke point (spied in tests → no real
    // claude). Propagate a spawn error BEFORE recording the investigation.
    continuous_fresh_spawn(state_arc, &full)?;

    // Deliver the daemon-constructed verdict prompt. Clone the input handle under
    // a BRIEF state lock, drop it, THEN hand off to the detached delivery thread.
    // A vanished session (fast-exit reaper removed the entry) skips delivery
    // best-effort — the investigation is already recorded below.
    let prompt = investigator_prompt(task, seq, snapshot_dir);
    let handle_opt = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        state.sessions.get(&uid).map(|s| s.input_handle())
    };
    if let Some(handle) = handle_opt {
        spawn_agent_prompt_delivery(handle, uid.clone(), prompt);
    }

    // Record the investigation: bind investigator_uid + its spawn time (the
    // watchdog bounds the investigator's OWN runtime against this) + bump the
    // count (the watchdog's cap check reads investigation_count next tick). The
    // returned task carries the post-increment count for the audit line.
    let now = crate::continuous::task::now_unix();
    let updated = crate::continuous::task::modify(&task.task_id, |t| {
        t.investigator_uid = Some(uid.clone());
        t.investigator_started_at = Some(now);
        t.investigation_count = t.investigation_count.saturating_add(1);
    })
    .map_err(|e| {
        (
            ErrorCode::Internal,
            format!(
                "spawn_investigator: persist investigator for '{}': {}",
                task.task_id, e
            ),
        )
    })?;

    // Audit line (best-effort): the watchdog flagged run `seq` stuck and spawned
    // investigator N. session_uid carries the STUCK run's uid; the investigator's
    // own uid rides `detail` for correlation.
    if let Err(e) = crate::continuous::runlog::ContinuousRunLog::append(
        &crate::continuous::runlog::RunLogLine {
            seq,
            ts: runlog_now_ts(),
            task_id: task.task_id.clone(),
            event: "stuck".to_string(),
            fire_token: task.last_run.as_ref().map(|r| r.fire_token.clone()),
            session_uid: task.last_run.as_ref().and_then(|r| r.session_uid.clone()),
            run_mode: Some("fresh".to_string()),
            trigger_source: Some("continuous-watchdog".to_string()),
            status: Some("running".to_string()),
            detail: Some(json!({
                "investigation": updated.investigation_count,
                "investigator_uid": uid,
            })),
        },
    ) {
        eprintln!(
            "cm-daemon: spawn_investigator runlog append failed for '{}': {}",
            task.task_id, e
        );
    }

    Ok(json!({ "session_uid": uid }))
}

// ===================================================================
// Continuous CRUD — Operator-only lifecycle management
// (DESIGN_CONTINUOUS_TASKS.md §8). `continuous.create` / `.list` /
// `.pause` / `.run_now` / `.delete`. The authoritative record is daemon
// disk (`crate::continuous::task`); the planning row is a thin mirror.
// These handlers NEVER spawn — firing a task is `trigger`'s job, and
// `continuous.run_now` is a thin forward to it. All are gated
// Operator-only in dispatch.rs (the TUI / cloud control plane manages
// lifecycle; agents fan out via `trigger`).
// ===================================================================

/// `continuous.create` params. The durable worktree is created ONCE here
/// (reused every fire — the disk-growth bound) and the workspace is registered
/// in the daemon's manifest snapshot. Config beyond the `ContinuousTask::new`
/// seed is assigned after construction (mirrors WorkflowRun's all-pub fields);
/// the later-phase fields (downstream/enqueue_to/retention/…) are accepted and
/// persisted but inert in Phase 2.
#[derive(serde::Deserialize)]
struct ContinuousCreateParams {
    /// Durable task id (planning slug, e.g. `"bug-triage"`). Doubles as the
    /// default worktree slug + workspace key. Validated by `validate_task_id`.
    task_id: String,
    /// Optional backing planning-task UUID. When set, the spawned session's
    /// `task_id` is this UUID (not the slug above), so an orchestrator that
    /// spawns subtasks via `create_subtask` has a real planning parent. The
    /// caller creates the planning row (POST /tasks) and passes its `id` here.
    #[serde(default)]
    planning_task_id: Option<String>,
    /// Human-readable sidebar label.
    label: String,
    /// Wire engine: `"claude"`|`"codex"`|`"bash"` (default `claude`).
    #[serde(default)]
    engine: crate::continuous::task::Engine,
    /// `"fresh"`|`"persistent"` (default `fresh`; persistent is a Phase-3 no-op).
    #[serde(default)]
    run_mode: crate::continuous::task::RunMode,
    /// Internally-tagged schedule (default `on_demand`). Phase 2 only ever fires
    /// on demand via `trigger`; periodic/consumer/cron logic is later phases.
    #[serde(default)]
    schedule: crate::continuous::task::Schedule,
    /// The default prompt each fresh fire delivers (the NOTES-first instruction).
    default_prompt: String,
    /// Repo shortname or URL — resolved on the daemon host for the worktree.
    repo_url: String,
    /// Optional branch to start the worktree from. `None` → `cm/<slug>` off HEAD.
    #[serde(default)]
    start_branch: Option<String>,
    /// Worktree dir/branch slug. Defaults to `task_id`.
    #[serde(default)]
    slug: Option<String>,
    /// Workspace id to register. Defaults to `ws-<task_id>`.
    #[serde(default)]
    workspace_id: Option<String>,
    /// Planning project this task belongs to.
    #[serde(default)]
    project: Option<String>,
    /// Host id to pin the task to (e.g. `"cm-manager"`). Defaults to `"local"`
    /// (the `ContinuousTask::new` seed).
    #[serde(default)]
    host: Option<String>,
    /// Optional skill the fresh session loads.
    #[serde(default)]
    skill: Option<String>,
    /// Named prompt presets (`trigger {mode}` selects one).
    #[serde(default)]
    modes: std::collections::BTreeMap<String, crate::continuous::task::ModePreset>,
    // ---- later-phase config: accepted + persisted, but INERT in Phase 2 ----
    #[serde(default)]
    downstream: Vec<String>,
    #[serde(default)]
    enqueue_to: Option<String>,
    #[serde(default)]
    retention: Option<crate::continuous::task::Retention>,
    #[serde(default)]
    review_surface: Option<String>,
    #[serde(default)]
    compact_every: Option<u32>,
    #[serde(default)]
    supervise: bool,
    #[serde(default)]
    max_runtime_secs: Option<u32>,
    /// Phase 3 memory-cap per-task override (bytes). `None` → the daemon's
    /// `[scheduler] default_cap`; `Some(0)` → uncapped.
    #[serde(default)]
    mem_cap_bytes: Option<u64>,
}

/// `continuous.create` — register a continuous task: resolve the repo, create
/// the durable worktree ONCE, register the workspace, and write the
/// authoritative `state.json`. Idempotent: a second create for the same id
/// reuses the existing record (`created=false`) without clobbering it, mirroring
/// `create_worktree`'s reuse semantics. Does NOT spawn — firing is `trigger`'s
/// job. Operator-only (gated in `dispatch.rs`).
///
/// Returns `{ created, task_id, workspace_id, worktree_path }`.
pub fn continuous_create(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    let p: ContinuousCreateParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("continuous.create params: {}", e)))?;

    // Containment-safe task_id allowlist BEFORE any path build — it keys the
    // worktree slug, the workspace, and the `~/.cm/continuous-tasks/<id>` dir.
    crate::continuous::task::validate_task_id(&p.task_id)
        .map_err(|e| (ErrorCode::InvalidParams, format!("continuous.create: {}", e)))?;
    if p.label.trim().is_empty() {
        return Err((ErrorCode::InvalidParams, "label must be non-empty".into()));
    }
    if p.default_prompt.trim().is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            "default_prompt must be non-empty".into(),
        ));
    }
    if p.repo_url.trim().is_empty() {
        return Err((ErrorCode::InvalidParams, "repo_url must be non-empty".into()));
    }

    // Worktree slug defaults to the (already allowlist-validated) task_id. A
    // caller-supplied slug feeds `<repo>-<slug>` + `cm/<slug>`, so guard it
    // against path escape exactly like create_session.
    let slug = p.slug.clone().unwrap_or_else(|| p.task_id.clone());
    if slug.contains('/') || slug.contains('\\') || slug.contains("..") {
        return Err((
            ErrorCode::InvalidParams,
            format!("slug '{}' must not contain path separators or '..'", slug),
        ));
    }
    let workspace_id = p
        .workspace_id
        .clone()
        .unwrap_or_else(|| format!("ws-{}", p.task_id));

    // Resolve the repo on the DAEMON's filesystem: local fast-path, else clone a
    // permitted URL. Snapshot the repos config under the lock, then resolve
    // without holding it (a clone can be slow). Mirrors create_session.
    let (repos_dir, allow_clone, allow_entries): (PathBuf, bool, Vec<(String, String)>) = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        (
            st.config.repos_dir_or_default(),
            st.config.allow_clone,
            st.config
                .repos
                .iter()
                .map(|e| (e.name.clone(), e.url.clone()))
                .collect(),
        )
    };
    let allowlist: Vec<crate::worktree::RepoAllow> = allow_entries
        .iter()
        .map(|(n, u)| crate::worktree::RepoAllow { name: n, url: u })
        .collect();
    let repo = crate::worktree::resolve_repo(&p.repo_url, &repos_dir, allow_clone, &allowlist)
        .map_err(|e| match e {
            crate::worktree::RepoResolveError::NotPermitted(name) => (
                ErrorCode::NotFound,
                format!(
                    "repo '{}' not found on the daemon host and cloning is not \
                     permitted — add a [[repo]] allowlist entry or set \
                     allow_clone in daemon.toml",
                    name
                ),
            ),
            crate::worktree::RepoResolveError::CloneFailed { repo, detail } => (
                ErrorCode::Internal,
                format!("clone of repo '{}' failed: {}", repo, detail),
            ),
        })?;

    // Create the durable worktree ONCE (reused every fire). `created` is `false`
    // when a pre-existing `cm/<slug>` worktree was reused on a slug collision —
    // NEVER delete that on a later failure (it may hold prior work).
    let (worktree_path, created) =
        crate::worktree::create_worktree(&repo, &slug, p.start_branch.as_deref()).map_err(|e| {
            (
                ErrorCode::Internal,
                format!("create_worktree for slug '{}': {}", slug, e),
            )
        })?;
    if created {
        crate::worktree::setup_worktree(&repo, &worktree_path);
    }
    let cleanup_if_created = |worktree_path: &std::path::Path| {
        if created {
            let _ = crate::worktree::remove_worktree(&repo, worktree_path);
        }
    };

    // Register the workspace + task→workspace binding in the daemon's manifest
    // snapshot under the lock, then DROP it. No spawn happens here (that's
    // `trigger`'s job); the durable worktree self-heals via the auto-register
    // hint on each fire (`compose_continuous_spawn_params`).
    {
        let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        state.workspaces.insert(
            workspace_id.clone(),
            crate::manifest::ManifestWorkspace {
                id: workspace_id.clone(),
                worktree_path: Some(worktree_path.clone()),
                ..Default::default()
            },
        );
        state
            .task_workspaces
            .insert(p.task_id.clone(), workspace_id.clone());
    }

    // Build the authoritative record and write it. `create` is the idempotent
    // first-write: `Ok(false)` on a record collision (the handler reuses the
    // existing record, mirroring the worktree's `created=false`).
    let mut task = crate::continuous::task::ContinuousTask::new(
        p.task_id.clone(),
        p.label.clone(),
        workspace_id.clone(),
        worktree_path.to_string_lossy().into_owned(),
        p.engine,
        p.run_mode,
        p.schedule.clone(),
        p.default_prompt.clone(),
    );
    task.planning_task_id = p.planning_task_id.clone();
    task.project = p.project.clone();
    task.repo = Some(p.repo_url.clone());
    if let Some(host) = p.host.clone() {
        task.host_id = host;
    }
    task.skill = p.skill.clone();
    task.modes = p.modes.clone();
    task.downstream = p.downstream.clone();
    task.enqueue_to = p.enqueue_to.clone();
    if let Some(retention) = p.retention.clone() {
        task.retention = retention;
    }
    task.review_surface = p.review_surface.clone();
    task.compact_every = p.compact_every;
    task.supervise = p.supervise;
    task.max_runtime_secs = p.max_runtime_secs;
    task.mem_cap_bytes = p.mem_cap_bytes;

    let record_created = match crate::continuous::task::create(&task) {
        Ok(c) => c,
        Err(e) => {
            cleanup_if_created(&worktree_path);
            return Err((
                ErrorCode::Internal,
                format!("continuous.create persist '{}': {}", p.task_id, e),
            ));
        }
    };

    // NOTE: the `metadata.continuous` mirror onto the planning task row is
    // DEFERRED. `planning_client.rs` has no metadata-PATCH helper today (only
    // `propose_task` POST /tasks), and a `metadata` PATCH replaces the whole
    // object (read-modify-write needed). Daemon disk is authoritative and the
    // planning row is a thin mirror, so the mirror is a best-effort follow-up —
    // `continuous.create` does not depend on it.

    Ok(json!({
        "created": record_created,
        "task_id": p.task_id,
        "workspace_id": workspace_id,
        "worktree_path": worktree_path.to_string_lossy().into_owned(),
    }))
}

/// `continuous.list` — the at-a-glance health read over every persisted
/// continuous task (`load_all` — disk is the authority). No state lock needed;
/// the projection is a snapshot, never a replay (the live edge rides
/// `manifest.watch`). Operator-only (gated in `dispatch.rs`).
///
/// Returns `{ tasks: [ { task_id, label, project, host_id, engine, run_mode,
/// schedule, enabled, paused, run_count, current_session_uid, in_flight,
/// next_fire_at, last_fired_at, last_outcome, last_run }, … ] }`.
pub fn continuous_list(
    _state_arc: &Arc<Mutex<DaemonState>>,
    _params: &Value,
) -> MethodResult {
    let tasks = crate::continuous::task::load_all();
    let items: Vec<Value> = tasks
        .iter()
        .map(|t| {
            json!({
                "task_id": t.task_id,
                "label": t.label,
                "project": t.project,
                "host_id": t.host_id,
                "engine": t.engine,
                "run_mode": t.run_mode,
                "schedule": t.schedule,
                "enabled": t.enabled,
                "paused": t.paused,
                "run_count": t.run_count,
                "current_session_uid": t.current_session_uid,
                "in_flight": t.in_flight.is_some(),
                "next_fire_at": t.next_fire_at,
                "last_fired_at": t.last_fired_at,
                // `last_outcome` surfaces the most-recent run's status glyph
                // (`Running`/`Done`/`Failed`/…); `null` before the first fire.
                "last_outcome": t.last_run.as_ref().map(|r| r.status),
                "last_run": t.last_run,
            })
        })
        .collect();
    Ok(json!({ "tasks": items }))
}

/// `continuous.pause` params.
#[derive(serde::Deserialize)]
struct ContinuousPauseParams {
    task_id: String,
    /// Target paused state (`true` to pause, `false` to resume).
    paused: bool,
}

/// `continuous.pause` — set/clear a task's `paused` flag (a paused task is
/// skipped by `trigger` with `{fired:false, reason:"paused"}`). Operator-only
/// (gated in `dispatch.rs`).
///
/// Returns `{ task_id, paused }`.
pub fn continuous_pause(
    _state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    let p: ContinuousPauseParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("continuous.pause params: {}", e)))?;
    crate::continuous::task::validate_task_id(&p.task_id)
        .map_err(|e| (ErrorCode::InvalidParams, format!("continuous.pause: {}", e)))?;
    // Clean NotFound before the modify (which would otherwise surface a missing
    // `state.json` as an opaque io error).
    if crate::continuous::task::load_one(&p.task_id).is_none() {
        return Err((
            ErrorCode::NotFound,
            format!("continuous.pause: continuous task '{}' not found", p.task_id),
        ));
    }
    let updated = crate::continuous::task::modify(&p.task_id, |t| {
        t.paused = p.paused;
    })
    .map_err(|e| {
        (
            ErrorCode::Internal,
            format!("continuous.pause persist '{}': {}", p.task_id, e),
        )
    })?;
    Ok(json!({ "task_id": p.task_id, "paused": updated.paused }))
}

/// `continuous.run_now` — manual fire. A thin forward to [`trigger`] with the
/// caller threaded through. Operator-only at the dispatch gate, so the validated
/// Operator caller bypasses `trigger`'s Session-caller self-or-descendant scope
/// gate (an Operator is already trusted).
///
/// Returns the same shape as `trigger`
/// (`{fired:true, fire_token, session_uid, run_mode}` or `{fired:false, reason}`).
pub fn continuous_run_now(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    params: &Value,
) -> MethodResult {
    trigger(state_arc, caller, params)
}

/// `continuous.delete` params.
#[derive(serde::Deserialize)]
struct ContinuousDeleteParams {
    task_id: String,
    /// Garbage-collect the durable worktree too (best-effort, default off).
    #[serde(default)]
    gc: bool,
}

/// `continuous.delete` — retire a continuous task: remove its
/// `~/.cm/continuous-tasks/<id>/` record dir (state.json + runs.jsonl + lock) and
/// drop its in-memory manifest registration. With `gc=true`, also best-effort
/// removes the durable worktree (resolved LOCALLY — never cloned on a delete
/// path). Operator-only (gated in `dispatch.rs`).
///
/// Returns `{ deleted: true, task_id, gc }`. An unknown id is `NotFound`.
pub fn continuous_delete(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    let p: ContinuousDeleteParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("continuous.delete params: {}", e)))?;
    crate::continuous::task::validate_task_id(&p.task_id)
        .map_err(|e| (ErrorCode::InvalidParams, format!("continuous.delete: {}", e)))?;
    let task = crate::continuous::task::load_one(&p.task_id).ok_or((
        ErrorCode::NotFound,
        format!(
            "continuous.delete: continuous task '{}' not found",
            p.task_id
        ),
    ))?;

    // Optional worktree GC (default off). Resolve the repo LOCALLY (a delete
    // must never clone) and remove the worktree; any failure is logged, never
    // fatal — the record still retires below.
    if p.gc {
        match task.repo.as_deref().and_then(crate::worktree::find_local_repo) {
            Some(repo) => {
                let wt = PathBuf::from(&task.worktree_path);
                if let Err(e) = crate::worktree::remove_worktree(&repo, &wt) {
                    eprintln!(
                        "cm-daemon: continuous.delete gc could not remove worktree {} \
                         for '{}': {}",
                        task.worktree_path, p.task_id, e
                    );
                }
            }
            None => {
                eprintln!(
                    "cm-daemon: continuous.delete gc could not resolve repo for '{}' \
                     locally — leaving worktree {} in place",
                    p.task_id, task.worktree_path
                );
            }
        }
    }

    // Retire the durable record. The validated task_id keeps this remove_dir_all
    // confined to `~/.cm/continuous-tasks/<id>`.
    let dir = crate::continuous::task::task_dir(&p.task_id);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        return Err((
            ErrorCode::Internal,
            format!("continuous.delete remove {}: {}", dir.display(), e),
        ));
    }

    // Drop the in-memory manifest registration so the sidebar/poller stop
    // referencing the retired task. Disk is authoritative; this just keeps the
    // snapshot tidy (Phase 2 has no restart reconciliation).
    {
        let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        state.task_workspaces.remove(&p.task_id);
        state.workspaces.remove(&task.workspace_id);
    }

    Ok(json!({ "deleted": true, "task_id": p.task_id, "gc": p.gc }))
}

/// `create_session` — A-n on a (possibly remote) daemon host: resolve
/// the repo, create the worktree daemon-side, and spawn the first
/// session in it. Operator-only (gated in `dispatch.rs`).
///
/// Returns `{ session_uid, worktree_path, workspace_id }`.
pub fn create_session(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    let p: CreateSessionParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("create_session params: {}", e)))?;
    // Validate the uid BEFORE any filesystem side effect. `build_args`
    // (via compose_daemon_spawn_params) writes ~/.cm/mcp/<uid>/claude.json,
    // and `Path::join` lets a `../`/absolute uid escape that base — so the
    // uid must be validated before find_local_repo / create_worktree /
    // compose, not only inside the delegated start_session.
    if !is_valid_session_uid(&p.uid) {
        return Err((
            ErrorCode::InvalidParams,
            format!("invalid session uid '{}' (expected ts-<hex>-<hex>)", p.uid),
        ));
    }
    if !is_valid_session_type(&p.engine) {
        return Err((
            ErrorCode::InvalidParams,
            format!(
                "engine must be one of \"claude-code\", \"codex\", \"bash\"; got '{}'",
                p.engine
            ),
        ));
    }
    if p.label.trim().is_empty() {
        return Err((ErrorCode::InvalidParams, "label must be non-empty".into()));
    }
    if p.slug.trim().is_empty() {
        return Err((ErrorCode::InvalidParams, "slug must be non-empty".into()));
    }
    // Guard against path traversal via the slug: it feeds the worktree
    // dir name (`<repo>-<slug>`) and branch (`cm/<slug>`). Legitimate
    // slugs (`slugify` output) are `[a-z0-9-]` only, so this rejects
    // nothing valid while keeping a caller-supplied slug from escaping
    // the worktree base.
    if p.slug.contains('/') || p.slug.contains('\\') || p.slug.contains("..") {
        return Err((
            ErrorCode::InvalidParams,
            format!("slug '{}' must not contain path separators or '..'", p.slug),
        ));
    }

    // Resolve the repo on the DAEMON's filesystem: local fast-path
    // (find_local_repo), else clone a permitted URL into `repos_dir`
    // (Phase 2). Snapshot the repos config under the lock, then resolve
    // without holding it (clone can be slow).
    let (repos_dir, allow_clone, allow_entries): (PathBuf, bool, Vec<(String, String)>) = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        (
            st.config.repos_dir_or_default(),
            st.config.allow_clone,
            st.config
                .repos
                .iter()
                .map(|e| (e.name.clone(), e.url.clone()))
                .collect(),
        )
    };
    let allowlist: Vec<crate::worktree::RepoAllow> = allow_entries
        .iter()
        .map(|(n, u)| crate::worktree::RepoAllow { name: n, url: u })
        .collect();
    let repo = crate::worktree::resolve_repo(
        &p.repo_url,
        &repos_dir,
        allow_clone,
        &allowlist,
    )
    .map_err(|e| match e {
        // Not on disk and cloning isn't permitted → NotFound naming the
        // repo (per the doc; an allowlist entry or allow_clone lifts it).
        crate::worktree::RepoResolveError::NotPermitted(name) => (
            ErrorCode::NotFound,
            format!(
                "repo '{}' not found on the daemon host and cloning is not \
                 permitted — add a [[repo]] allowlist entry or set \
                 allow_clone in daemon.toml",
                name
            ),
        ),
        // Cloning was permitted but git failed (bad URL, network, auth).
        crate::worktree::RepoResolveError::CloneFailed { repo, detail } => (
            ErrorCode::Internal,
            format!("clone of repo '{}' failed: {}", repo, detail),
        ),
    })?;

    // Create the worktree: ~/.cm/worktrees/<repo>-<slug> on cm/<slug>.
    // On `git worktree add` failure `create_worktree` cleans up its
    // partial worktree itself; no session has been spawned yet, so
    // there is no orphan to undo here. `created` is `false` when a
    // pre-existing `cm/<slug>` worktree was reused on a slug collision —
    // we must NEVER delete that on a later failure (it may hold work this
    // call didn't create).
    let (worktree_path, created) =
        crate::worktree::create_worktree(&repo, &p.slug, p.start_branch.as_deref())
            .map_err(|e| {
                (
                    ErrorCode::Internal,
                    format!("create_worktree for slug '{}': {}", p.slug, e),
                )
            })?;

    // Mirror the local A-n path (tui/src/app.rs `create_local_session`):
    // run the repo's `setup_worktree.sh` hook after a FRESH create
    // (best-effort, no-op when the script is absent). NOT on the reuse
    // path — an existing worktree was already initialized.
    if created {
        crate::worktree::setup_worktree(&repo, &worktree_path);
    }

    // On ANY post-create failure (compose OR spawn), remove the worktree
    // ONLY if THIS call created it — never a reused checkout. Without this
    // guard a `build_args`/spawn failure would either orphan a worktree
    // (compose used `?`, leaving it behind) or delete a user's existing
    // checkout (cleanup ran unconditionally on a reused worktree).
    let cleanup_if_created = |worktree_path: &std::path::Path| {
        if created {
            let _ = crate::worktree::remove_worktree(&repo, worktree_path);
        }
    };

    // Compose the daemon-resolved spawn params and delegate to the
    // shared spawn core. Pass the worktree as the auto-register hint so
    // `start_session` registers the workspace if the daemon's manifest
    // snapshot doesn't already know it.
    let full = match compose_daemon_spawn_params(
        state_arc,
        &p.uid,
        &p.workspace_id,
        &p.label,
        &p.engine,
        &worktree_path,
        p.task_id.as_deref(),
        p.cols,
        p.rows,
        Some(&worktree_path),
    ) {
        Ok(v) => v,
        Err(e) => {
            cleanup_if_created(&worktree_path);
            return Err(e);
        }
    };
    let start_result = match start_session(state_arc, &full) {
        Ok(v) => v,
        Err(e) => {
            cleanup_if_created(&worktree_path);
            return Err(e);
        }
    };

    let session_uid = start_result
        .get("session_uid")
        .and_then(Value::as_str)
        .unwrap_or(&p.uid)
        .to_string();
    Ok(json!({
        "session_uid": session_uid,
        "worktree_path": worktree_path.to_string_lossy().into_owned(),
        "workspace_id": p.workspace_id,
    }))
}

/// `add_session` — A-s on a (possibly remote) daemon host: spawn
/// another session in an EXISTING workspace's worktree. Never creates a
/// worktree. Operator-only (gated in `dispatch.rs`).
///
/// Returns `{ session_uid, worktree_path }`. An unknown `workspace_id`
/// returns `NotFound`.
pub fn add_session(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
) -> MethodResult {
    let p: AddSessionParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("add_session params: {}", e)))?;
    // Validate the uid BEFORE any filesystem side effect (compose →
    // build_args writes ~/.cm/mcp/<uid>/claude.json; a `../`/absolute uid
    // would escape that base). See create_session for the full rationale.
    if !is_valid_session_uid(&p.uid) {
        return Err((
            ErrorCode::InvalidParams,
            format!("invalid session uid '{}' (expected ts-<hex>-<hex>)", p.uid),
        ));
    }
    if !is_valid_session_type(&p.engine) {
        return Err((
            ErrorCode::InvalidParams,
            format!(
                "engine must be one of \"claude-code\", \"codex\", \"bash\"; got '{}'",
                p.engine
            ),
        ));
    }
    if p.label.trim().is_empty() {
        return Err((ErrorCode::InvalidParams, "label must be non-empty".into()));
    }

    // Look up the existing workspace's worktree. `add_session` reuses
    // it and NEVER calls `create_worktree`. Unknown workspace →
    // NotFound (the daemon doesn't know this workspace; create one with
    // `create_session` first).
    let worktree_path = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let ws = st.workspaces.get(&p.workspace_id).ok_or((
            ErrorCode::NotFound,
            format!(
                "workspace '{}' is not known to this daemon (add_session reuses an \
                 existing workspace's worktree — create one with create_session first)",
                p.workspace_id
            ),
        ))?;
        ws.worktree_path.clone().ok_or((
            ErrorCode::NotFound,
            format!(
                "workspace '{}' has no worktree_path on this daemon",
                p.workspace_id
            ),
        ))?
    };

    // No auto-register hint: the workspace is already known to the
    // daemon (we just looked it up).
    let full = compose_daemon_spawn_params(
        state_arc,
        &p.uid,
        &p.workspace_id,
        &p.label,
        &p.engine,
        &worktree_path,
        p.task_id.as_deref(),
        p.cols,
        p.rows,
        None,
    )?;
    let start_result = start_session(state_arc, &full)?;

    let session_uid = start_result
        .get("session_uid")
        .and_then(Value::as_str)
        .unwrap_or(&p.uid)
        .to_string();
    Ok(json!({
        "session_uid": session_uid,
        "worktree_path": worktree_path.to_string_lossy().into_owned(),
    }))
}

/// P-3: parse `"6G" | "512M" | "1024K" | "67108864"` into a byte count.
/// Suffixes are powers of 2 (K/M/G/T), same as systemd's `MemoryHigh=`. Mirrors
/// `tui/src/memory_cap.rs::parse_bytes` — configured cap values routinely carry
/// suffixes, so a plain `u64` parse would reject `6G` and fall through to
/// uncapped (P-3a bug).
fn parse_cap_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let last = bytes[bytes.len() - 1];
    let (num_str, multiplier) = match last {
        b'K' | b'k' => (&s[..s.len() - 1], 1024u64),
        b'M' | b'm' => (&s[..s.len() - 1], 1024u64 * 1024),
        b'G' | b'g' => (&s[..s.len() - 1], 1024u64 * 1024 * 1024),
        b'T' | b't' => (&s[..s.len() - 1], 1024u64 * 1024 * 1024 * 1024),
        _ => (s, 1u64),
    };
    let n: u64 = num_str.trim().parse().ok()?;
    n.checked_mul(multiplier)
}

/// P-3: resolve the CONFIGURED per-engine memory cap for a daemon-launched
/// workflow participant, for the case where there is no caller session to
/// inherit from (an Operator/headless launch — the always-on host this phase
/// exists for). Mirrors the TUI's two halves:
///   - soft/hard bytes from `CM_SESSION_MEM_SOFT_<KEY>` /
///     `CM_SESSION_MEM_HARD_<KEY>`, where `<KEY>` is the uppercased INTERNAL cap
///     key — the wire `session_type` is normalized first
///     (`claude-code` → `claude`, `codex` → `codex`), matching
///     `tui/src/app.rs::normalize_session_type_to_internal` +
///     `Config::memory_cap_for`. Values are suffix-aware (`6G`); both must be
///     set with hard >= soft. (P-3a: the prior code looked up the bogus
///     `CM_SESSION_MEM_SOFT_CLAUDE-CODE` and used a plain `u64` parse, so a real
///     `CLAUDE`/`6G` config never matched → silently uncapped.)
///   - `cgroup_prefix` computed from the daemon's uid (same formula as
///     `tui/src/preflight.rs`).
///
/// Graceful degradation: the env vars are the operator's explicit opt-in, but
/// if the predicted `app.slice` cgroup directory doesn't exist (no running user
/// manager — caps genuinely can't apply on this host), return `None` and log,
/// so a misconfigured host runs UNCAPPED rather than failing every workflow
/// launch at `start_session`'s cgroup-scope verification. On a real systemd
/// host (cm-manager) the directory exists and the cap applies.
fn resolve_configured_participant_cap(session_type: &str) -> Option<(u64, u64, String)> {
    // Normalize wire vocabulary → internal cap key (P-3a). bash/unknown → never
    // capped (parity with the TUI: `claude-code`→`claude`, `codex`→`codex`).
    let cap_key = match session_type {
        "claude-code" | "claude" => "claude",
        "codex" => "codex",
        _ => return None,
    };
    let upper = cap_key.to_uppercase();
    let soft = std::env::var(format!("CM_SESSION_MEM_SOFT_{}", upper))
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let hard = std::env::var(format!("CM_SESSION_MEM_HARD_{}", upper))
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let soft_bytes = parse_cap_bytes(&soft)?;
    let hard_bytes = parse_cap_bytes(&hard)?;
    if hard_bytes < soft_bytes {
        eprintln!(
            "cm-daemon: configured cap for '{}' is misconfigured (hard {} < soft {}); \
             running participant uncapped",
            session_type, hard_bytes, soft_bytes
        );
        return None;
    }
    let uid = unsafe { libc::getuid() };
    #[allow(unused_mut)]
    let mut prefix = std::path::PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{}.slice/user@{}.service/app.slice",
        uid, uid
    ));
    // Test seam: point the cgroup prefix at a real temp dir so the `is_dir`
    // graceful-degradation gate can be exercised without /sys/fs/cgroup.
    #[cfg(test)]
    if let Some(ov) = CONFIGURED_CAP_PREFIX_OVERRIDE.with(|c| c.borrow().clone()) {
        prefix = std::path::PathBuf::from(ov);
    }
    if !prefix.is_dir() {
        eprintln!(
            "cm-daemon: CM_SESSION_MEM_*_{} set but predicted cgroup prefix {} is \
             absent (no user manager?) — running participant UNCAPPED rather than \
             failing the launch",
            upper,
            prefix.display()
        );
        return None;
    }
    Some((soft_bytes, hard_bytes, prefix.to_string_lossy().into_owned()))
}

/// Sub-2b-3 review-fix #1: cap-inherit triple cloned out of the
/// caller's `DaemonSession` under the state lock and used after
/// the lock drops to wrap the child's argv. All three fields
/// must be present together — partial caps (e.g. soft only)
/// can't drive a systemd-run wrap.
struct InheritedCap {
    soft_bytes: u64,
    hard_bytes: u64,
    cgroup_prefix: std::path::PathBuf,
}

/// Sub-2b-3: daemon-minted session uid. Mirrors the shape of
/// `tui/src/app.rs::new_session_uid` so the validator in
/// `is_valid_session_uid` accepts it. Per-process counter +
/// monotonic nanos.
#[cfg(test)]
thread_local! {
    /// Test seam: when set, `start_workflow`'s per-role spawn uses this
    /// (program, args) instead of building the real `claude`/`codex` argv, so
    /// tests spawn a lightweight program (e.g. `/bin/sleep`) deterministically
    /// without depending on the agent binaries.
    static SPAWN_PROGRAM_OVERRIDE: std::cell::RefCell<Option<(String, Vec<String>)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_spawn_program_override_for_test(prog: Option<(String, Vec<String>)>) {
    SPAWN_PROGRAM_OVERRIDE.with(|c| *c.borrow_mut() = prog);
}

/// Resolve the (program, argv_tail) for a workflow participant. Honors the
/// test-only spawn override; otherwise builds the real engine argv.
fn resolve_workflow_spawn_program(
    session_type: &str,
    uid: &str,
    workflow: Option<&crate::mcp_config::WorkflowMeta>,
    server_path_override: Option<&str>,
) -> std::io::Result<(String, Vec<String>)> {
    #[cfg(test)]
    {
        // P-CRIT verification seam: record the workflow meta start_workflow
        // threads in here, BEFORE the spawn override short-circuits. This
        // proves the run_id/role reach the MCP-config writer (build_args) for
        // each role — the exact link that was missing — even when a test uses
        // the /bin/sleep override to avoid spawning real agents.
        CAPTURED_WORKFLOW_META.with(|c| {
            c.borrow_mut().push((
                uid.to_string(),
                workflow.map(|w| (w.run_id.to_string(), w.role.to_string())),
            ))
        });
        if let Some(ov) = SPAWN_PROGRAM_OVERRIDE.with(|c| c.borrow().clone()) {
            return Ok(ov);
        }
    }
    crate::mcp_config::build_args(session_type, uid, workflow, server_path_override)
}

#[cfg(test)]
thread_local! {
    /// Per-role `(uid, Option<(run_id, role)>)` captured by
    /// `resolve_workflow_spawn_program`. The P-CRIT test asserts every workflow
    /// participant carries `Some((run_id, role))`.
    static CAPTURED_WORKFLOW_META: std::cell::RefCell<Vec<(String, Option<(String, String)>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn take_captured_workflow_meta_for_test(
) -> Vec<(String, Option<(String, String)>)> {
    CAPTURED_WORKFLOW_META.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

#[cfg(test)]
thread_local! {
    /// P-3 seam: per-role `(uid, inherited cap triple)` captured by
    /// `start_workflow`. Lets a test assert the cap THREADING decision
    /// (which participants get which cap) without depending on a working
    /// user-systemd instance for `start_session`'s cgroup-scope verification.
    static CAPTURED_PARTICIPANT_CAP: std::cell::RefCell<Vec<(String, Option<(u64, u64, String)>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn take_captured_participant_caps_for_test(
) -> Vec<(String, Option<(u64, u64, String)>)> {
    CAPTURED_PARTICIPANT_CAP.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

#[cfg(test)]
thread_local! {
    /// P-3 seam: override the computed cgroup prefix in
    /// `resolve_configured_participant_cap` so tests can exercise the
    /// configured-cap path without a real `/sys/fs/cgroup` hierarchy.
    static CONFIGURED_CAP_PREFIX_OVERRIDE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_configured_cap_prefix_override_for_test(prefix: Option<String>) {
    CONFIGURED_CAP_PREFIX_OVERRIDE.with(|c| *c.borrow_mut() = prefix);
}

/// Get-or-create the per-worktree spawn queue (serializes snapshot+spawn+detect
/// so participants in one worktree don't cross-bind transcripts — P-A).
fn workflow_spawn_queue(
    state_arc: &Arc<Mutex<DaemonState>>,
    working_dir: &std::path::Path,
) -> Arc<crate::state::WorktreeSpawnQueue> {
    let registry_arc = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        state.worktree_spawn_queues.clone()
    };
    let mut registry = registry_arc.lock().unwrap_or_else(|p| p.into_inner());
    registry
        .entry(working_dir.to_path_buf())
        .or_insert_with(|| Arc::new(crate::state::WorktreeSpawnQueue::new()))
        .clone()
}

#[cfg(test)]
thread_local! {
    /// Test seam: pre-snapshots recorded per participant uid by the
    /// `start_workflow` spawn loop, so the serialization invariant (role B's
    /// snapshot taken AFTER role A's detector bound) is observable.
    static SPAWN_SNAPSHOTS: std::cell::RefCell<Vec<(String, Vec<String>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Test seam: when set, `start_workflow` uses [`test_detector_spawn_fn`].
    static USE_TEST_DETECTOR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Test seam: when set, `start_workflow` skips detector arming entirely (for
    /// tests that drive sids manually and don't want the spawn-queue wait).
    static DISABLE_WORKFLOW_DETECTOR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Test seam (P-B): when set, the detector-spawn factory returns a closure
    /// that always fails — so a test can assert `start_workflow` FAILS CLOSED
    /// (errors + cleans up the spawned sessions) on detector-thread spawn
    /// failure rather than returning a run with an undetectable participant.
    static USE_FAILING_DETECTOR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn set_failing_detector_for_test(failing: bool) {
    USE_FAILING_DETECTOR.with(|c| c.set(failing));
}

#[cfg(test)]
pub(crate) fn set_disable_workflow_detector_for_test(disabled: bool) {
    DISABLE_WORKFLOW_DETECTOR.with(|c| c.set(disabled));
}

#[cfg(test)]
fn workflow_detector_disabled() -> bool {
    DISABLE_WORKFLOW_DETECTOR.with(|c| c.get())
}

#[cfg(not(test))]
fn workflow_detector_disabled() -> bool {
    false
}

/// True when a test has installed the spawn-program override (deterministic
/// `/bin/sleep` spawn). P-3: the systemd-run cap wrap is skipped in that case so
/// cap-threading tests don't depend on a working user-systemd instance; the cap
/// METADATA still rides `full` so the spawned session records it.
#[cfg(test)]
fn workflow_spawn_override_active() -> bool {
    SPAWN_PROGRAM_OVERRIDE.with(|c| c.borrow().is_some())
}

#[cfg(not(test))]
fn workflow_spawn_override_active() -> bool {
    false
}

#[cfg(test)]
static TEST_DETECTOR_WORKTREE: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

#[cfg(test)]
pub(crate) fn record_spawn_snapshot_for_test(uid: &str, snapshot: &[String]) {
    SPAWN_SNAPSHOTS.with(|c| c.borrow_mut().push((uid.to_string(), snapshot.to_vec())));
}

#[cfg(test)]
pub(crate) fn take_spawn_snapshots_for_test() -> Vec<(String, Vec<String>)> {
    SPAWN_SNAPSHOTS.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

#[cfg(test)]
pub(crate) fn enable_test_detector(worktree: Option<std::path::PathBuf>) {
    USE_TEST_DETECTOR.with(|c| c.set(worktree.is_some()));
    *TEST_DETECTOR_WORKTREE.lock().unwrap() = worktree;
}

/// Detector-spawn factory used by `start_workflow`. Production uses the real
/// thread spawner; tests can substitute a deterministic detector that writes a
/// transcript (after a delay) so the serialization invariant is observable.
fn workflow_detector_spawn_fn() -> crate::transcript_detect::DetectorSpawnFn {
    #[cfg(test)]
    {
        if USE_FAILING_DETECTOR.with(|c| c.get()) {
            return Box::new(|_name, _body| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "injected detector-thread spawn failure (P-B test)",
                ))
            });
        }
        if USE_TEST_DETECTOR.with(|c| c.get()) {
            return test_detector_spawn_fn();
        }
    }
    crate::transcript_detect::default_detector_spawn_fn()
}

/// Deterministic test detector: spawns a thread that (1) sleeps so a
/// non-serialized next-role snapshot is taken BEFORE this write, (2) writes a
/// `<uid>.jsonl` transcript into the test worktree's claude dir, then (3) runs
/// the real detector body (which binds it + drops the queue ticket). With the
/// serialized fix, role B waits for this to finish, so B's snapshot includes
/// role A's transcript; without it, B's snapshot is empty.
#[cfg(test)]
fn test_detector_spawn_fn() -> crate::transcript_detect::DetectorSpawnFn {
    Box::new(|name: String, body: Box<dyn FnOnce() + Send + 'static>| {
        let uid = name
            .strip_prefix("cm-daemon-transcript-detect-")
            .map(|s| s.to_string())
            .unwrap_or_default();
        let wt = TEST_DETECTOR_WORKTREE.lock().unwrap().clone();
        std::thread::Builder::new().name(name).spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(40));
            if let (Some(wt), Some(home)) = (wt, std::env::var_os("HOME")) {
                let enc = wt.to_str().unwrap().replace('/', "-").replace('.', "-");
                let dir = std::path::PathBuf::from(home).join(format!(".claude/projects/{}", enc));
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(
                    dir.join(format!("{}.jsonl", uid)),
                    "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"x\"}]}}\n",
                );
            }
            body();
        })
    })
}

fn new_daemon_minted_session_uid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ts-{:x}-{:x}", nanos, n)
}

// ============================================================
// Subtask CRUD — create_subtask / list_subtasks / mark_subtask_done
// ============================================================
//
// Port of the TUI's `tui/src/control/methods.rs` subtask handlers,
// re-keyed onto the daemon's headless model so a DAEMON-spawned agent
// (e.g. a continuous-task orchestrator on cm-manager with no TUI) can
// fork + track subtasks. Daemon-spawned sessions get `CM_TUI_SOCKET`
// set to the daemon's own socket, so the MCP client routes
// `create_subtask` / `list_subtasks` / `mark_subtask_done` here.
//
// Differences from the TUI port:
//   - The caller is a `Caller::Session`; its OWN `task_id` is the PARENT.
//   - Parent task metadata (repo_url, project, name, wip_branch) comes
//     from the planning API (GET /tasks/{id}), NOT `app.tasks` /
//     `state.task_tree` — both are empty/absent on a headless daemon.
//   - The parent's local worktree path comes from
//     `DaemonState.workspaces[caller.workspace_id]` (the orchestrator
//     session runs ON this daemon). `main_repo_path` is preferred from
//     that workspace, but production daemon workspaces register only
//     `worktree_path` (the auto-register branch + continuous.create set
//     `main_repo_path = None`), so branch / in-place modes fall back to
//     resolving the repo on disk from the parent's `repo_url` — exactly
//     the `create_session` pattern.
//   - The planning-API HTTP is INLINED here with ureq (mirroring
//     `propose_task`'s config-first / env-fallback credential shape)
//     because `planning_client.rs` only exposes `propose_task` (whose
//     body lacks `parent_task_id` / `slug` / `status` / `wip_branch`)
//     and is outside this slice's edit set.
//
// Lock discipline (mirrors propose_task / create_session): the
// `DaemonState` mutex is NEVER held across a planning-API HTTP call or
// a git/worktree operation. Snapshot what's needed under the lock,
// drop, do HTTP + git, then re-lock only to register the new
// workspace + seed the headless auth edge.

use crate::planning_client::PlanningClientError;

/// Resolved planning-API credentials for one inline call batch.
/// Built from the `DaemonState.config` snapshot (override-first) with
/// `CM_API_URL` / `CM_API_TOKEN` env fallback — an inlined mirror of
/// `planning_client::resolve_api_url` / `resolve_api_token` (those are
/// private to that module).
struct PlanningApiCreds {
    base_url: String,
    token: String,
}

impl PlanningApiCreds {
    /// `api_url_cfg` / `api_token_cfg` are the `state.config` values
    /// snapshotted under the lock (then dropped) by the caller. A
    /// non-empty config value wins; empty falls through to env. The
    /// base URL has any trailing slash trimmed (matches the TUI's
    /// `ApiClient`).
    fn from_config(
        api_url_cfg: &str,
        api_token_cfg: &str,
    ) -> Result<Self, PlanningClientError> {
        Ok(PlanningApiCreds {
            base_url: resolve_planning_api_url(api_url_cfg)?,
            token: resolve_planning_api_token(api_token_cfg)?,
        })
    }

    /// Fresh ureq agent per call batch. 10s global timeout matches
    /// `planning_client::build_agent`.
    fn agent() -> ureq::Agent {
        ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_global(Some(std::time::Duration::from_secs(10)))
                .build(),
        )
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

fn resolve_planning_api_url(override_val: &str) -> Result<String, PlanningClientError> {
    let trimmed = override_val.trim();
    if !trimmed.is_empty() {
        return Ok(trimmed.trim_end_matches('/').to_string());
    }
    let url = std::env::var("CM_API_URL")
        .map_err(|_| PlanningClientError::MissingConfig("CM_API_URL"))?;
    let t = url.trim().to_string();
    if t.is_empty() {
        return Err(PlanningClientError::MissingConfig("CM_API_URL"));
    }
    Ok(t.trim_end_matches('/').to_string())
}

fn resolve_planning_api_token(override_val: &str) -> Result<String, PlanningClientError> {
    let trimmed = override_val.trim();
    if !trimmed.is_empty() {
        return Ok(trimmed.to_string());
    }
    let token = std::env::var("CM_API_TOKEN")
        .map_err(|_| PlanningClientError::MissingConfig("CM_API_TOKEN"))?;
    let t = token.trim().to_string();
    if t.is_empty() {
        return Err(PlanningClientError::MissingConfig("CM_API_TOKEN"));
    }
    Ok(t)
}

/// ureq v3 surfaces a non-2xx response as `Error::StatusCode(u16)` (no
/// body handle in that arm); everything else is transport. Map to the
/// same `PlanningClientError` shape `propose_task` uses so 4xx → caller
/// error, 5xx / transport / missing-config → Internal.
fn map_ureq_err(e: ureq::Error) -> PlanningClientError {
    match e {
        ureq::Error::StatusCode(status) => {
            PlanningClientError::ApiError { status, body: String::new() }
        }
        other => PlanningClientError::Transport(other.to_string()),
    }
}

/// GET /tasks/{id} — the full task row.
fn api_get_task(
    creds: &PlanningApiCreds,
    task_id: &str,
) -> Result<Value, PlanningClientError> {
    let agent = PlanningApiCreds::agent();
    let mut resp = agent
        .get(&creds.url(&format!("/tasks/{}", task_id)))
        .header("Authorization", &creds.auth())
        .call()
        .map_err(map_ureq_err)?;
    resp.body_mut()
        .read_json::<Value>()
        .map_err(|e| PlanningClientError::Transport(format!("decode get_task: {}", e)))
}

/// GET /tasks — the full task universe. The planning API has no
/// `parent_task_id` query filter (only status / project), so
/// `list_subtasks` fetches everything and filters client-side.
fn api_list_tasks(creds: &PlanningApiCreds) -> Result<Vec<Value>, PlanningClientError> {
    let agent = PlanningApiCreds::agent();
    let mut resp = agent
        .get(&creds.url("/tasks"))
        .header("Authorization", &creds.auth())
        .call()
        .map_err(map_ureq_err)?;
    resp.body_mut()
        .read_json::<Vec<Value>>()
        .map_err(|e| PlanningClientError::Transport(format!("decode list_tasks: {}", e)))
}

/// POST /tasks — create a task row; returns the created row (incl. the
/// server-assigned `id`).
fn api_create_task(
    creds: &PlanningApiCreds,
    body: &Value,
) -> Result<Value, PlanningClientError> {
    let agent = PlanningApiCreds::agent();
    let mut resp = agent
        .post(&creds.url("/tasks"))
        .header("Authorization", &creds.auth())
        .send_json(body)
        .map_err(map_ureq_err)?;
    resp.body_mut()
        .read_json::<Value>()
        .map_err(|e| PlanningClientError::Transport(format!("decode create_task: {}", e)))
}

/// PATCH /tasks/{id} — partial update (e.g. `{"status":"done"}`).
fn api_update_task(
    creds: &PlanningApiCreds,
    task_id: &str,
    fields: &Value,
) -> Result<(), PlanningClientError> {
    let agent = PlanningApiCreds::agent();
    agent
        .patch(&creds.url(&format!("/tasks/{}", task_id)))
        .header("Authorization", &creds.auth())
        .send_json(fields)
        .map_err(map_ureq_err)?;
    Ok(())
}

/// DELETE /tasks/{id} — create-rollback for a worktree failure.
fn api_delete_task(
    creds: &PlanningApiCreds,
    task_id: &str,
) -> Result<(), PlanningClientError> {
    let agent = PlanningApiCreds::agent();
    agent
        .delete(&creds.url(&format!("/tasks/{}", task_id)))
        .header("Authorization", &creds.auth())
        .call()
        .map_err(map_ureq_err)?;
    Ok(())
}

/// Generate a 7-hex-char id from nanos + an atomic counter. Ported
/// verbatim from `tui/src/control/methods.rs::make_request_short_id`.
/// Used by `create_subtask` for BOTH the slug suffix and the
/// `cm-sub/<chain>-<short>` branch suffix so they share a consistent
/// suffix without depending on the new task's UUID.
fn make_request_short_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:07x}", (nanos.wrapping_add(n)) & 0x0FFF_FFFF)
}

/// Build the slug chain for a subtask branch name. The TUI walks
/// `app.tasks`; the daemon has no task list, so it walks ancestry via
/// the planning API following `parent_task_id`, joining each ancestor's
/// slugified `name` with `-`, ending in the leaf slug. The immediate
/// parent's already-fetched row is reused to save a GET (the common
/// depth-1 orchestrator→subtask case needs zero extra round-trips).
/// Capped at `MAX_TASK_DEPTH`; ancestor-GET failures break the walk
/// (the chain is cosmetic — the `-<short>` suffix guarantees the slug's
/// uniqueness).
fn build_slug_chain_via_api(
    creds: &PlanningApiCreds,
    parent_id: &str,
    parent_row: &Value,
    leaf_slug: &str,
) -> String {
    let mut chain: Vec<String> = vec![leaf_slug.to_string()];
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cur_id = parent_id.to_string();
    let mut cur_row: Option<Value> = Some(parent_row.clone());
    for _ in 0..crate::control::auth::MAX_TASK_DEPTH {
        if !visited.insert(cur_id.clone()) {
            break;
        }
        let row = match cur_row.take() {
            Some(r) => r,
            None => match api_get_task(creds, &cur_id) {
                Ok(r) => r,
                Err(_) => break,
            },
        };
        let name = row.get("name").and_then(Value::as_str).unwrap_or("");
        chain.push(crate::worktree::slugify(name));
        match row.get("parent_task_id").and_then(Value::as_str) {
            Some(pid) if !pid.is_empty() => cur_id = pid.to_string(),
            _ => break,
        }
    }
    chain.reverse();
    chain.join("-")
}

#[derive(Deserialize)]
struct CreateSubtaskParams {
    name: String,
    #[serde(default)]
    prompt: Option<String>,
    /// One of `"inherit"` (default), `"branch"`, or `"in-place"`.
    #[serde(default = "default_subtask_worktree_mode")]
    worktree_mode: String,
    #[serde(default)]
    project: Option<String>,
}

fn default_subtask_worktree_mode() -> String {
    "inherit".to_string()
}

/// Create a subtask off the CALLER's task (the parent). Session-callable
/// only — the parent is the caller session's own `task_id`. Produces an
/// IDENTICAL subtask to the TUI's `create_subtask`: `parent_task_id`
/// set, `status="running"`, `source="claude"`, `is_cloud=false`,
/// `slug="<chain>-<short>"`, branch mode → a `cm-sub/<chain>-<short>`
/// worktree, with `wip_branch` baked into the API row per mode.
///
/// Returns `{"task_id": <new id>, "worktree_path": <string>}`.
pub fn create_subtask(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
    caller_uid: Option<&str>,
) -> MethodResult {
    let p: CreateSubtaskParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("create_subtask params: {}", e)))?;
    // Validate mode BEFORE resolving the caller (mirror TUI ordering).
    if !matches!(p.worktree_mode.as_str(), "inherit" | "branch" | "in-place") {
        return Err((
            ErrorCode::InvalidParams,
            format!(
                "worktree_mode must be 'inherit', 'branch', or 'in-place', got '{}'",
                p.worktree_mode
            ),
        ));
    }
    // Reject an empty-after-normalization slug before any API/git work.
    let leaf_slug = crate::worktree::slugify(&p.name);
    if leaf_slug.is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            format!("name '{}' produces an empty slug after normalization", p.name),
        ));
    }

    // Resolve the caller + snapshot everything the unlocked HTTP/git
    // phase needs, then DROP the lock.
    let (
        parent_task_id,
        parent_workspace_id,
        parent_worktree_path,
        parent_main_repo_path,
        parent_ws_repo_url,
        repos_dir,
        allow_clone,
        allow_entries,
        api_url_cfg,
        api_token_cfg,
    ): (
        String,
        String,
        Option<PathBuf>,
        Option<PathBuf>,
        Option<String>,
        PathBuf,
        bool,
        Vec<(String, String)>,
        String,
        String,
    ) = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let cuid = caller_uid.ok_or((
            ErrorCode::Unauthorized,
            "create_subtask is callable only by Session callers (the daemon \
             resolves the parent task from the caller's session)"
                .into(),
        ))?;
        let caller = state.sessions.get(cuid).ok_or((
            ErrorCode::Unauthorized,
            format!("caller session '{}' not in daemon registry", cuid),
        ))?;
        // The caller's OWN task is the PARENT. Taskless callers should
        // use propose_task for top-level tasks.
        let parent_task_id = caller.task_id.clone().ok_or((
            ErrorCode::Unauthorized,
            "create_subtask requires a tasked caller; use propose_task for top-level tasks"
                .into(),
        ))?;
        let parent_workspace_id = caller.workspace_id.clone();
        let ws = state.workspaces.get(&parent_workspace_id).ok_or((
            ErrorCode::Conflict,
            format!(
                "caller's workspace '{}' not in daemon manifest snapshot",
                parent_workspace_id
            ),
        ))?;
        (
            parent_task_id,
            parent_workspace_id,
            ws.worktree_path.clone(),
            ws.main_repo_path.clone(),
            ws.repo_url.clone(),
            state.config.repos_dir_or_default(),
            state.config.allow_clone,
            state
                .config
                .repos
                .iter()
                .map(|e| (e.name.clone(), e.url.clone()))
                .collect(),
            state.config.api_url.clone(),
            state.config.api_token.clone(),
        )
    };

    // ---- API + git phase (lock dropped) ----

    let creds =
        PlanningApiCreds::from_config(&api_url_cfg, &api_token_cfg).map_err(|e| e.to_method_err())?;

    // Parent metadata from the planning API (task_tree is empty headless).
    // HARDENING (parent-deleted self-heal): if the parent planning row is
    // GONE — e.g. someone `A-x`'d it off the board, which hard-deletes a
    // non-cloud task — don't 404 the agent. The live caller session's
    // workspace still carries the repo_url + worktree, so everything the
    // subtask needs is in hand; we just create a TOP-LEVEL task
    // (parent_task_id = null) instead, since `tasks.parent_task_id` is an FK
    // that a dangling id would violate anyway. ONLY a clean 404 falls back —
    // a transport/auth/5xx error still propagates (we can't tell if the
    // parent really exists, so retrying is safer than silently orphaning).
    let parent_row: Value = match api_get_task(&creds, &parent_task_id) {
        Ok(row) => row,
        Err(PlanningClientError::ApiError { status: 404, .. }) => {
            eprintln!(
                "cm-daemon: create_subtask parent task {} not found on the \
                 planning API (deleted?); creating '{}' as a TOP-LEVEL task \
                 instead of failing",
                parent_task_id, p.name,
            );
            Value::Null
        }
        Err(e) => return Err(e.to_method_err()),
    };
    let parent_exists = !parent_row.is_null();
    // The FK we actually write into the new row: Some(parent) only when the
    // parent row exists; None (top-level) when it was deleted.
    let effective_parent_id: Option<String> = parent_exists.then(|| parent_task_id.clone());
    let parent_repo_url: String = parent_row
        .get("repo_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| parent_ws_repo_url.clone())
        .filter(|s| !s.is_empty())
        .ok_or((ErrorCode::Conflict, "parent task has no repo_url".into()))?;
    let parent_project = parent_row
        .get("project")
        .and_then(Value::as_str)
        .map(str::to_string);
    let parent_wip_branch = parent_row
        .get("wip_branch")
        .and_then(Value::as_str)
        .map(str::to_string);

    // Project: explicit arg > inherit from parent.
    let project = p.project.clone().or(parent_project);

    // The slug chain walks the parent's ancestry via the API; with no parent
    // row there's nothing to walk, so the chain is just this leaf.
    let slug_chain = if parent_exists {
        build_slug_chain_via_api(&creds, &parent_task_id, &parent_row, &leaf_slug)
    } else {
        leaf_slug.clone()
    };
    let request_short_id = make_request_short_id();
    let unique_slug = format!("{}-{}", slug_chain, request_short_id);

    // `main_repo` for branch / in-place. Prefer the parent workspace's
    // recorded `main_repo_path`; production daemon workspaces register
    // only `worktree_path` (auto-register + continuous.create leave
    // `main_repo_path = None`), so fall back to resolving the repo on
    // the daemon's filesystem from the parent's `repo_url` — the
    // `create_session` pattern.
    let resolve_main_repo = || -> Result<PathBuf, (ErrorCode, String)> {
        if let Some(mr) = parent_main_repo_path.clone() {
            return Ok(mr);
        }
        let allowlist: Vec<crate::worktree::RepoAllow> = allow_entries
            .iter()
            .map(|(n, u)| crate::worktree::RepoAllow { name: n, url: u })
            .collect();
        crate::worktree::resolve_repo(&parent_repo_url, &repos_dir, allow_clone, &allowlist).map_err(
            |e| match e {
                crate::worktree::RepoResolveError::NotPermitted(name) => (
                    ErrorCode::Conflict,
                    format!(
                        "parent workspace has no main_repo_path and repo '{}' is not \
                         resolvable on the daemon host; cannot branch/launch in-place",
                        name
                    ),
                ),
                crate::worktree::RepoResolveError::CloneFailed { repo, detail } => (
                    ErrorCode::Internal,
                    format!("clone of repo '{}' failed: {}", repo, detail),
                ),
            },
        )
    };

    // Step 1 — validate ALL per-mode preconditions BEFORE the POST so
    // a missing path / unresolvable repo can't leak an orphan row.
    let inherit_worktree_path: Option<PathBuf> = if p.worktree_mode == "inherit" {
        Some(parent_worktree_path.clone().ok_or((
            ErrorCode::Conflict,
            "parent workspace has no worktree path (cloud workspace?)".into(),
        ))?)
    } else {
        None
    };
    let branch_main_repo: Option<PathBuf> = if p.worktree_mode == "branch" {
        Some(resolve_main_repo()?)
    } else {
        None
    };
    let in_place_main_repo: Option<PathBuf> = if p.worktree_mode == "in-place" {
        Some(resolve_main_repo()?)
    } else {
        None
    };

    // Parent's base branch: wip_branch, else the worktree's actual HEAD.
    // Branch mode REQUIRES this (never falls back to "main").
    let parent_branch_resolved: Option<String> = parent_wip_branch.clone().or_else(|| {
        parent_worktree_path
            .as_deref()
            .and_then(crate::worktree::worktree_current_branch)
    });
    if p.worktree_mode == "branch" && parent_branch_resolved.is_none() {
        return Err((
            ErrorCode::Conflict,
            "cannot determine parent's base branch (no wip_branch and worktree HEAD is \
             detached or unreadable)"
                .into(),
        ));
    }

    // `wip_branch` baked into the API row UPFRONT, per mode.
    let branch_name_for_new: Option<String> = match p.worktree_mode.as_str() {
        "inherit" => parent_branch_resolved.clone(),
        "branch" => Some(format!("cm-sub/{}-{}", slug_chain, request_short_id)),
        "in-place" => in_place_main_repo
            .as_deref()
            .and_then(crate::worktree::worktree_current_branch),
        _ => None,
    };

    // Step 2 — create the task row. Server assigns the UUID.
    let mut body = json!({
        "repo_url": parent_repo_url,
        "repo_branch": "main",
        "name": p.name,
        "priority": 0,
        "status": "running",
        "slug": unique_slug,
        "source": "claude",
        "is_cloud": false,
        // null when the parent was deleted (top-level fallback) — never a
        // dangling FK that the API insert would reject.
        "parent_task_id": effective_parent_id,
        "worktree_mode": p.worktree_mode,
    });
    if let Some(pr) = &p.prompt {
        body["prompt"] = json!(pr);
    }
    if let Some(pj) = &project {
        body["project"] = json!(pj);
    }
    if let Some(b) = &branch_name_for_new {
        body["wip_branch"] = json!(b);
    }
    let new_task = api_create_task(&creds, &body).map_err(|e| e.to_method_err())?;
    let new_task_id = new_task
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or((ErrorCode::Internal, "api create_task response missing 'id'".into()))?;

    // Step 3 — produce the worktree. A git failure AFTER the row exists
    // triggers a rollback DELETE so we don't leak a `running` row.
    let (worktree_path, new_workspace): (PathBuf, Option<crate::manifest::ManifestWorkspace>) =
        match p.worktree_mode.as_str() {
            "inherit" => {
                // Reuse the parent workspace; no new workspace, no git.
                (inherit_worktree_path.expect("validated above"), None)
            }
            "branch" => {
                let main_repo = branch_main_repo.expect("validated above");
                let parent_branch = parent_branch_resolved.clone().expect("validated above");
                let branch_name = branch_name_for_new
                    .clone()
                    .expect("branch_name_for_new is Some in branch mode");
                let wt = match crate::worktree::create_subtask_worktree(
                    &main_repo,
                    &branch_name,
                    &parent_branch,
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = api_delete_task(&creds, &new_task_id);
                        return Err((
                            ErrorCode::Internal,
                            format!(
                                "create worktree failed for branch '{}'; api task {} rolled back: {}",
                                branch_name, new_task_id, e
                            ),
                        ));
                    }
                };
                crate::worktree::setup_worktree(&main_repo, &wt);
                let ws = crate::manifest::ManifestWorkspace {
                    id: uuid::Uuid::new_v4().simple().to_string(),
                    name: leaf_slug.clone(),
                    repo_url: Some(parent_repo_url.clone()),
                    worktree_path: Some(wt.clone()),
                    main_repo_path: Some(main_repo.clone()),
                    ..Default::default()
                };
                (wt, Some(ws))
            }
            "in-place" => {
                // No worktree, no branch — the subtask runs in the MAIN
                // repo checkout. A SEPARATE workspace whose
                // `worktree_path == main_repo_path` is the in-place
                // marker that gates teardown off git.
                let main_repo = in_place_main_repo.expect("validated above");
                let ws = crate::manifest::ManifestWorkspace {
                    id: uuid::Uuid::new_v4().simple().to_string(),
                    name: leaf_slug.clone(),
                    repo_url: Some(parent_repo_url.clone()),
                    worktree_path: Some(main_repo.clone()),
                    main_repo_path: Some(main_repo.clone()),
                    ..Default::default()
                };
                (main_repo, Some(ws))
            }
            _ => unreachable!(),
        };

    // Step 4 — register the workspace + seed the headless auth edge so
    // a subsequent list_subtasks / mark_subtask_done / mcp_start_session
    // on the subtask resolves the descendant walk (the daemon analog of
    // the TUI's push_task_tree_to_daemon). create_subtask does NOT spawn
    // a session, so there is no ManifestDiff::Added to broadcast — the
    // new workspace reaches a (re)subscribing TUI via the next
    // manifest.watch snapshot (which serializes state.workspaces +
    // state.bindings wholesale), and an Added fires later when an agent
    // spawns a session into the subtask via mcp_start_session.
    let workspace_id_for_new = new_workspace
        .as_ref()
        .map(|w| w.id.clone())
        .unwrap_or_else(|| parent_workspace_id.clone());
    {
        let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(ws) = new_workspace {
            state.workspaces.insert(ws.id.clone(), ws);
        }
        state
            .task_tree
            .insert(new_task_id.clone(), effective_parent_id.clone());
        state
            .task_workspaces
            .insert(new_task_id.clone(), workspace_id_for_new.clone());
        state
            .bindings
            .insert(new_task_id.clone(), workspace_id_for_new);
    }

    Ok(json!({
        "task_id": new_task_id,
        "worktree_path": worktree_path.to_string_lossy(),
    }))
}

#[derive(Deserialize, Default)]
struct ListSubtasksParams {
    #[serde(default)]
    task_id: Option<String>,
}

/// List the children of a task. Read-only: tombstoned (recently-exited)
/// callers may still read (mirrors the TUI's `caller_ctx_or_tombstone`).
/// Scope = explicit `task_id` (must be self-or-descendant of the
/// caller's task) else the caller's own task. The planning API has no
/// `parent_task_id` filter, so this GETs /tasks and filters client-side.
///
/// Returns a JSON array of `{task_id, name, status, worktree_mode,
/// wip_branch, workspace_id}` per child (`workspace_id` from the
/// daemon-local `task_workspaces` binding — the API row has none).
pub fn list_subtasks(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
    caller_uid: Option<&str>,
) -> MethodResult {
    let p: ListSubtasksParams = if params.is_null() {
        ListSubtasksParams::default()
    } else {
        serde_json::from_value(params.clone())
            .map_err(|e| (ErrorCode::InvalidParams, format!("list_subtasks params: {}", e)))?
    };

    let (scope, task_workspaces, api_url_cfg, api_token_cfg): (
        String,
        std::collections::HashMap<String, String>,
        String,
        String,
    ) = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let cuid = caller_uid.ok_or((
            ErrorCode::Unauthorized,
            "list_subtasks is callable only by Session callers".into(),
        ))?;
        // Reads survive caller exit: live registry first, then tombstone.
        let own_task_id: Option<String> = match state.sessions.get(cuid) {
            Some(s) => s.task_id.clone(),
            None => match state.exited_tombstone(cuid) {
                Some(t) => t.task_id.clone(),
                None => {
                    return Err((ErrorCode::NotFound, "caller session not found".into()));
                }
            },
        };
        let scope = match p.task_id.as_deref() {
            Some(req) => {
                let own = own_task_id.as_deref().ok_or((
                    ErrorCode::Unauthorized,
                    format!("taskless caller cannot scope to task {}", req),
                ))?;
                if !crate::control::auth::task_is_self_or_descendant_of(&state.task_tree, req, own) {
                    return Err((
                        ErrorCode::Unauthorized,
                        format!("task {} is not the caller's task or a descendant", req),
                    ));
                }
                req.to_string()
            }
            None => own_task_id.clone().ok_or((
                ErrorCode::Unauthorized,
                "taskless caller cannot list subtasks (no task scope)".into(),
            ))?,
        };
        (
            scope,
            state.task_workspaces.clone(),
            state.config.api_url.clone(),
            state.config.api_token.clone(),
        )
    };

    let creds =
        PlanningApiCreds::from_config(&api_url_cfg, &api_token_cfg).map_err(|e| e.to_method_err())?;
    let all = api_list_tasks(&creds).map_err(|e| e.to_method_err())?;

    let mut out: Vec<Value> = Vec::new();
    for task in &all {
        if task.get("parent_task_id").and_then(Value::as_str) == Some(scope.as_str()) {
            let tid = task.get("id").and_then(Value::as_str);
            let workspace_id = tid.and_then(|id| task_workspaces.get(id)).cloned();
            out.push(json!({
                "task_id": tid,
                "name": task.get("name").cloned().unwrap_or(Value::Null),
                "status": task.get("status").cloned().unwrap_or(Value::Null),
                "worktree_mode": task.get("worktree_mode").cloned().unwrap_or(Value::Null),
                "wip_branch": task.get("wip_branch").cloned().unwrap_or(Value::Null),
                "workspace_id": workspace_id,
            }));
        }
    }
    Ok(Value::Array(out))
}

#[derive(Deserialize)]
struct MarkSubtaskDoneParams {
    task_id: String,
    #[serde(default = "default_close_worktree")]
    close_worktree: bool,
}

fn default_close_worktree() -> bool {
    true
}

/// Allowed planning statuses for `set_subtask_status`. Mirrors the API's task
/// status enum; the API validates too, but pre-checking here gives the agent a
/// crisp error instead of a forwarded 4xx.
const SUBTASK_STATUSES: &[&str] =
    &["draft", "backlog", "running", "blocked", "done", "archived"];

#[derive(Deserialize)]
struct SetSubtaskStatusParams {
    /// Target task. Omitted → the caller's OWN task (the common case: an agent
    /// flagging its own task `blocked` = fix-ready).
    #[serde(default)]
    task_id: Option<String>,
    status: String,
}

/// `set_subtask_status` — Session-callable. Set the planning status of the
/// caller's own task (or a descendant) through the daemon's planning client.
///
/// The HEADLESS-capable analog of the cli-routed `update_task` (status only):
/// on a headless host (e.g. cm-manager, where the `cli` package + `CM_API_URL`
/// are NOT in the agent's MCP env) `update_task` is unavailable, so a bug-fix
/// subtask agent had no way to signal `blocked` (fix-ready). This routes the
/// status PATCH through the daemon, which holds the planning creds — no cli, no
/// creds in agent env. Unlike `mark_subtask_done` it does NO session/worktree
/// teardown: `blocked` means the work is waiting for review, so the session
/// stays alive.
///
/// Returns `{"task_id": <id>, "status": <status>}`.
pub fn set_subtask_status(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
    caller_uid: Option<&str>,
) -> MethodResult {
    let p: SetSubtaskStatusParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("set_subtask_status params: {}", e)))?;
    if !SUBTASK_STATUSES.contains(&p.status.as_str()) {
        return Err((
            ErrorCode::InvalidParams,
            format!("status must be one of {:?}, got '{}'", SUBTASK_STATUSES, p.status),
        ));
    }
    let (target_task_id, api_url_cfg, api_token_cfg): (String, String, String) = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let cuid = caller_uid.ok_or((
            ErrorCode::Unauthorized,
            "set_subtask_status is callable only by Session callers".into(),
        ))?;
        let caller = state.sessions.get(cuid).ok_or((
            ErrorCode::Unauthorized,
            format!("caller session '{}' not in daemon registry", cuid),
        ))?;
        let own = caller.task_id.clone().ok_or((
            ErrorCode::Unauthorized,
            "taskless caller cannot set a task status; use propose_task for top-level tasks".into(),
        ))?;
        let target = p.task_id.clone().unwrap_or_else(|| own.clone());
        if !crate::control::auth::task_is_self_or_descendant_of(&state.task_tree, &target, &own) {
            return Err((
                ErrorCode::Unauthorized,
                format!("task {} is not the caller's task or a descendant", target),
            ));
        }
        (
            target,
            state.config.api_url.clone(),
            state.config.api_token.clone(),
        )
    };
    let creds =
        PlanningApiCreds::from_config(&api_url_cfg, &api_token_cfg).map_err(|e| e.to_method_err())?;
    api_update_task(&creds, &target_task_id, &json!({ "status": p.status }))
        .map_err(|e| e.to_method_err())?;
    Ok(json!({ "task_id": target_task_id, "status": p.status }))
}

/// Best-effort bounded wait until no LIVE session is tagged with
/// `task_id`. `mark_subtask_done` SIGKILLs the subtask's sessions
/// before removing its worktree; the reaper removes them
/// asynchronously, so this gives the reaper a short window so
/// `git worktree remove` doesn't race a still-dying child.
fn wait_for_task_sessions_gone(
    state_arc: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    timeout: std::time::Duration,
) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let any = {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            state
                .sessions
                .values()
                .any(|s| s.task_id.as_deref() == Some(task_id))
        };
        if !any || std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Mark a subtask done. Mutation: requires a LIVE caller (tombstones
/// refused); the target must be self-or-descendant of the caller's
/// task. Ordering matches the TUI: locate + close sessions + remove the
/// worktree (branch mode, `close_worktree`, NOT in-place) BEFORE the API
/// flip — a failed remove leaves the task non-done and retryable. Hard
/// safety net: never `git worktree remove` a workspace whose
/// `worktree_path == main_repo_path` (in-place).
///
/// Returns `{"ok": true, "worktree_removed": <bool>}`.
pub fn mark_subtask_done(
    state_arc: &Arc<Mutex<DaemonState>>,
    params: &Value,
    caller_uid: Option<&str>,
) -> MethodResult {
    let p: MarkSubtaskDoneParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("mark_subtask_done params: {}", e)))?;

    // Auth (LIVE caller) + snapshot the subtask's local workspace paths
    // + creds, then drop the lock.
    let (cleanup, api_url_cfg, api_token_cfg): (
        Option<(String, Option<PathBuf>, Option<PathBuf>)>,
        String,
        String,
    ) = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let cuid = caller_uid.ok_or((
            ErrorCode::Unauthorized,
            "mark_subtask_done is callable only by Session callers".into(),
        ))?;
        let caller = state.sessions.get(cuid).ok_or((
            ErrorCode::Unauthorized,
            format!("caller session '{}' not in daemon registry", cuid),
        ))?;
        let own = caller.task_id.as_deref().ok_or((
            ErrorCode::Unauthorized,
            "taskless caller cannot mark subtasks done".into(),
        ))?;
        if !crate::control::auth::task_is_self_or_descendant_of(&state.task_tree, &p.task_id, own) {
            return Err((
                ErrorCode::Unauthorized,
                format!("task {} is not the caller's task or a descendant", p.task_id),
            ));
        }
        // Resolve the subtask's workspace from the daemon-local maps.
        let cleanup = state
            .task_workspaces
            .get(&p.task_id)
            .cloned()
            .and_then(|ws_id| {
                state
                    .workspaces
                    .get(&ws_id)
                    .map(|ws| (ws_id, ws.worktree_path.clone(), ws.main_repo_path.clone()))
            });
        (
            cleanup,
            state.config.api_url.clone(),
            state.config.api_token.clone(),
        )
    };

    let creds =
        PlanningApiCreds::from_config(&api_url_cfg, &api_token_cfg).map_err(|e| e.to_method_err())?;

    // worktree_mode label is authoritative from the API row.
    let task_row = api_get_task(&creds, &p.task_id).map_err(|e| e.to_method_err())?;
    let was_branch_mode = task_row.get("worktree_mode").and_then(Value::as_str) == Some("branch");

    // Close the subtask's sessions FIRST, for ALL modes — a subtask marked done
    // has finished its work, so its sessions must not linger (git/PTY hygiene).
    // Mirrors the TUI's mark_subtask_done, which closes sessions unconditionally
    // regardless of worktree_mode / close_worktree (the daemon previously only
    // closed them inside the branch-mode + close_worktree path, stranding live
    // sessions on inherit / in-place / close_worktree=false done subtasks).
    {
        let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let targets: Vec<String> = state
            .sessions
            .iter()
            .filter(|(_, s)| s.task_id.as_deref() == Some(p.task_id.as_str()))
            .map(|(uid, _)| uid.clone())
            .collect();
        for uid in targets {
            if let Some(sess) = state.sessions.get_mut(&uid) {
                sess.last_exit.mark_operator_kill_requested();
                let _ = sess.kill();
            }
        }
    }
    wait_for_task_sessions_gone(state_arc, &p.task_id, std::time::Duration::from_secs(2));

    // Phase 1/2 — cleanup BEFORE the API flip. Only branch mode owns a
    // dedicated worktree; inherit shares the parent's, in-place runs in
    // the main repo (worktree_path == main_repo_path).
    let mut worktree_removed = false;
    if p.close_worktree && was_branch_mode {
        let (ws_id, wt_opt, mr_opt) = cleanup.ok_or((
            ErrorCode::Conflict,
            format!(
                "task {} has no bound workspace in this daemon lifetime (a restart \
                 drops in-memory bindings) — pass close_worktree=false to mark it \
                 done without worktree cleanup",
                p.task_id
            ),
        ))?;
        let is_in_place = match (&wt_opt, &mr_opt) {
            (Some(wt), Some(mr)) => wt == mr,
            _ => false,
        };
        if is_in_place {
            // Hard safety net: never git-remove the main checkout.
        } else if wt_opt.is_none() {
            // Already cleaned on a prior call (worktree_path cleared
            // after a successful remove): end-state is "gone" — fall
            // through to (re)try the API flip.
            worktree_removed = true;
        } else {
            let wt = wt_opt.expect("just checked Some");
            let mr = mr_opt.ok_or((
                ErrorCode::Conflict,
                format!(
                    "workspace {} has no main_repo_path; cannot run `git worktree remove`",
                    ws_id
                ),
            ))?;
            // (sessions already closed unconditionally above)
            match crate::worktree::remove_worktree(&mr, &wt) {
                Ok(()) => {
                    worktree_removed = true;
                    let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                        ws.is_closed = true;
                        ws.worktree_path = None;
                    }
                }
                Err(e) => {
                    // Cleanup failure → DO NOT mark the task done (retryable).
                    return Err((
                        ErrorCode::Internal,
                        format!(
                            "worktree remove failed for task {} (sessions closed, but task \
                             NOT marked done — retry once the worktree issue is resolved): {}",
                            p.task_id, e
                        ),
                    ));
                }
            }
        }
    }

    // Phase 3 — only NOW commit the Done status.
    api_update_task(&creds, &p.task_id, &json!({ "status": "done" }))
        .map_err(|e| e.to_method_err())?;

    Ok(json!({ "ok": true, "worktree_removed": worktree_removed }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ManifestWorkspace;
    use tempfile::TempDir;

    /// Multi-line agent prompt bodies must be wrapped in bracketed-paste
    /// markers — otherwise the agent (codex / claude-code) submits at the
    /// first newline and mangles the prompt. Single-line bodies go raw.
    #[test]
    fn agent_paste_payload_wraps_multiline_only() {
        let multi = agent_paste_payload("line one\nline two");
        assert_eq!(multi, b"\x1b[200~line one\nline two\x1b[201~");

        let single = agent_paste_payload("just one line");
        assert_eq!(single, b"just one line");
    }

    /// The agent submit keystroke is the kitty-encoded Enter (CSI 13 u),
    /// not a bare `\n`/`\r` — codex's and claude-code's kitty-keyboard TUIs
    /// ignore the latter, so the prompt never submits.
    #[test]
    fn agent_kitty_enter_is_csi_13u() {
        assert_eq!(AGENT_KITTY_ENTER, b"\x1b[13u");
        assert_ne!(AGENT_KITTY_ENTER, b"\n");
        assert_ne!(AGENT_KITTY_ENTER, b"\r");
    }

    /// Sub-2b-3 review-10: pin the watcher-startup
    /// hard-cap resolution. Pre-fix the watcher-startup
    /// site at methods.rs L735 used a literal `0` regardless
    /// of what was on the wire — so cap-killed sessions
    /// emitted JSONL records with `hard_cap_bytes: 0`,
    /// diverging from the TUI-local watcher. This test
    /// pins the resolver's behavior; combined with the
    /// production call site routing through it AND the
    /// end-to-end JSONL assertion in
    /// `session_watch::tests::producer_end_to_end_kills_and_writes_record`
    /// (which now also asserts the hard_cap_bytes field
    /// matches the spawn_watcher argument), the wire→
    /// watcher→JSONL chain is verified.
    #[test]
    fn resolve_watcher_hard_cap_bytes_passes_through_wire_value() {
        const N: u64 = 128 * 1024 * 1024;
        assert_eq!(
            resolve_watcher_hard_cap_bytes(Some(N)),
            N,
            "wire value must reach the watcher unchanged — \
             a literal 0 here was the review-10 bug",
        );
        // Smaller value, ensure it's not a constant.
        assert_eq!(
            resolve_watcher_hard_cap_bytes(Some(42)),
            42,
        );
    }

    #[test]
    fn resolve_watcher_hard_cap_bytes_defensive_none_yields_zero() {
        // Review-4 #1's entry-point validation rejects
        // partial cap tuples, so on the validated path this
        // branch is unreachable when the watcher is being
        // spawned. The `0` fallback is defense-in-depth
        // against a future wire-shape regression.
        assert_eq!(resolve_watcher_hard_cap_bytes(None), 0);
    }

    /// Construct a `DaemonState` with one workspace whose
    /// `worktree_path` points at a usable temp directory, already
    /// wrapped in `Arc<Mutex<…>>` (the form `start_session` now
    /// requires — slice-10c-c review fix #2).
    fn state_with_workspace(ws_id: &str, dir: &TempDir) -> Arc<Mutex<DaemonState>> {
        let mut s = DaemonState::new();
        s.workspaces.insert(
            ws_id.into(),
            ManifestWorkspace {
                id: ws_id.into(),
                name: "test-ws".into(),
                is_closed: false,
                is_cloud: false,
                worktree_path: Some(dir.path().to_path_buf()),
                main_repo_path: None,
                repo_url: None,
                worker_vm: None,
                worker_zone: None,
                sessions: Vec::new(),
                tombstones: Vec::new(),
            },
        );
        Arc::new(Mutex::new(s))
    }

    /// Drain all registered sessions and kill them. Tests should
    /// call this in their cleanup path even though `Drop` would
    /// SIGKILL via pidfd anyway — explicit cleanup avoids the
    /// reaper-cleanup callback racing with the test's assertions
    /// (a `/bin/bash` that exits because its tty closed would have
    /// the cleanup callback try to remove from the registry,
    /// generating a spurious lock contention).
    fn kill_all_sessions(state: &Arc<Mutex<DaemonState>>) {
        let mut s = state.lock().unwrap();
        for (_, mut sess) in s.sessions.drain() {
            let _ = sess.kill();
        }
    }

    /// Counter for unique TUI-format uids in tests. Slice 10c-e-3b-fix:
    /// the daemon now requires a TUI-supplied uid, so tests must
    /// generate one. Each call returns a freshly-uniquified
    /// `ts-<nanos>-<counter>` string that satisfies
    /// `is_valid_session_uid`.
    fn fresh_test_uid() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("ts-{:x}-{:x}", nanos, n)
    }

    /// Build a `start_session` request body for a bare `/bin/bash`
    /// spawn against the supplied working directory. Helper for
    /// the slice 10c-e-3b argv/env/cwd wire shape — saves repeating
    /// the JSON in every test. Mints a fresh TUI-format uid each
    /// call (slice 10c-e-3b-fix: caller is source of truth for uid).
    fn bash_params(workspace_id: &str, label: &str, dir: &std::path::Path) -> Value {
        json!({
            "uid": fresh_test_uid(),
            "workspace_id": workspace_id,
            "label": label,
            "argv": ["/bin/bash"],
            "working_dir": dir.display().to_string(),
        })
    }

    // --- Param validation -----------------------------------------------------

    #[test]
    fn invalid_params_shape_returns_invalid_params() {
        // Required-field validation: a missing required field
        // (workspace_id here) surfaces as a serde error wrapped in
        // our InvalidParams code so clients get a typed pointer
        // rather than an opaque Internal. Every other required
        // field is supplied so the assertion isolates workspace_id.
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let params = json!({
            "uid": fresh_test_uid(),
            "label": "x",
            "argv": ["/bin/bash"],
            "working_dir": "/tmp",
            // intentionally omitting workspace_id
        });
        let err = start_session(&state, &params).expect_err("missing workspace_id");
        assert_eq!(err.0, ErrorCode::InvalidParams);
        assert!(
            err.1.contains("workspace_id"),
            "error should name the missing field: {}",
            err.1
        );
    }

    #[test]
    fn empty_argv_returns_invalid_params() {
        // Slice 10c-e-3b: the daemon no longer maps session_type to
        // hardcoded argv; the caller's `argv` field is authoritative.
        // An empty argv is a structural error — there's nothing to
        // exec.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-empty-argv", &dir);
        let params = json!({
            "uid": fresh_test_uid(),
            "workspace_id": "ws-empty-argv",
            "label": "x",
            "argv": [],
            "working_dir": dir.path().display().to_string(),
        });
        let err = start_session(&state, &params).expect_err("empty argv");
        assert_eq!(err.0, ErrorCode::InvalidParams);
        assert!(
            err.1.contains("argv"),
            "error should name the empty field: {}",
            err.1
        );
    }

    // --- Workspace resolution -----------------------------------------------

    /// A daemon-created subtask workspace (present in `state.bindings`, with NO
    /// live session and NOT in the TUI's push) must SURVIVE the task.update_tree
    /// GC — else a headless orchestrator's create_subtask→mcp_start_session
    /// races the TUI's push and the workspace is GC'd out from under it (the
    /// documented NotFound). An unbound, sessionless workspace is still GC'd.
    #[test]
    fn task_update_tree_preserves_bound_subtask_workspace() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "ws-subtask".to_string();
            ws.worktree_path = Some(std::path::PathBuf::from("/tmp/sub-wt"));
            s.workspaces.insert("ws-subtask".to_string(), ws);
            // create_subtask registered this binding (survives the push).
            s.bindings.insert("task-sub".to_string(), "ws-subtask".to_string());
            // An unbound, sessionless workspace that SHOULD be GC'd (control).
            let mut orphan = crate::manifest::ManifestWorkspace::default();
            orphan.id = "ws-orphan".to_string();
            s.workspaces.insert("ws-orphan".to_string(), orphan);
        }
        // TUI pushes a tree that knows NEITHER ws-subtask nor ws-orphan.
        let resp = task_update_tree(
            &state,
            &json!({
                "tasks": [{"task_id": "task-other", "parent_task_id": null, "workspace_id": "ws-other"}],
                "workspaces": [{"workspace_id": "ws-other", "worktree_path": "/tmp/other"}],
            }),
        );
        assert!(resp.is_ok(), "update_tree ok: {:?}", resp);
        let s = state.lock().unwrap();
        assert!(
            s.workspaces.contains_key("ws-subtask"),
            "bound subtask workspace must survive the GC",
        );
        assert!(s.workspaces.contains_key("ws-other"), "pushed workspace present");
        assert!(
            !s.workspaces.contains_key("ws-orphan"),
            "unbound sessionless workspace is still GC'd",
        );
    }

    #[test]
    fn missing_workspace_returns_not_found() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let params = json!({
            "uid": fresh_test_uid(),
            "workspace_id": "ws-does-not-exist",
            "label": "x",
            "argv": ["/bin/bash"],
            "working_dir": "/tmp",
        });
        let err = start_session(&state, &params).expect_err("missing ws");
        assert_eq!(err.0, ErrorCode::NotFound);
        assert!(err.1.contains("ws-does-not-exist"));
    }

    #[test]
    fn unknown_workspace_with_worktree_path_auto_registers_and_spawns() {
        // Named acceptance for slice 10c-e-3's auto-register
        // bridge: the daemon snapshots workspaces once at startup,
        // so an A-n workspace created mid-session is unknown to it
        // until 10e's manifest.watch lands. When the TUI passes
        // `worktree_path` alongside an unknown `workspace_id`, the
        // daemon registers a minimal workspace entry on the fly and
        // proceeds with the spawn — preserving the smoke-test
        // workflow without needing 10e first.
        let dir = TempDir::new().unwrap();
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let workspace_id = "ws-fresh-from-a-n";
        let worktree_path = dir.path().display().to_string();

        // Pre-check: the workspace is NOT in the map.
        assert!(
            state.lock().unwrap().workspaces.get(workspace_id).is_none(),
            "workspace must not be pre-registered"
        );

        let params = json!({
            "uid": fresh_test_uid(),
            "workspace_id": workspace_id,
            "label": "shell",
            "argv": ["/bin/bash"],
            "working_dir": worktree_path,
            "worktree_path": worktree_path,
        });
        let result = start_session(&state, &params).expect("auto-register + spawn");
        let uid = result["session_uid"].as_str().unwrap().to_string();

        // The spawn succeeded AND the workspace got registered as a
        // side effect.
        {
            let s = state.lock().unwrap();
            let ws = s
                .workspaces
                .get(workspace_id)
                .expect("workspace auto-registered");
            assert_eq!(ws.id, workspace_id);
            assert_eq!(
                ws.worktree_path.as_ref().map(|p| p.display().to_string()),
                Some(worktree_path)
            );
            assert!(s.sessions.contains_key(&uid));
        }

        kill_all_sessions(&state);
    }

    #[test]
    fn unknown_workspace_without_worktree_path_still_returns_not_found() {
        // The auto-register branch is gated on the caller sending
        // `worktree_path`. Without it, prior "NotFound for unknown
        // id" behavior holds — non-daemon-aware callers see no
        // change.
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let params = json!({
            "uid": fresh_test_uid(),
            "workspace_id": "ws-unknown",
            "label": "x",
            "argv": ["/bin/bash"],
            "working_dir": "/tmp",
            // intentionally no worktree_path
        });
        let err = start_session(&state, &params).expect_err("no auto-register");
        assert_eq!(err.0, ErrorCode::NotFound);
        assert!(
            err.1.contains("worktree_path"),
            "error should hint at the auto-register field: {}",
            err.1
        );
    }

    /// Option B (criterion #4): a successful `start_session` broadcasts a
    /// manifest `Added` carrying the session's uid + workspace + label +
    /// session_type + workflow tags, so a live `manifest.watch` subscriber (the
    /// launching TUI) can adopt a daemon-launched participant row from
    /// broadcasts alone — symmetric with the existing `Exited` broadcast.
    #[test]
    fn start_session_broadcasts_manifest_added_with_workflow_tags() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-add", &dir);
        let (rx, _guard) = {
            let s = state.lock().unwrap();
            s.manifest_watcher.subscribe()
        };
        let uid = fresh_test_uid();
        let params = json!({
            "uid": uid,
            "workspace_id": "ws-add",
            "label": "reviewer",
            "argv": ["/bin/bash"],
            "working_dir": dir.path().display().to_string(),
            "session_type": "claude-code",
            "workflow_run_id": "wf_add",
            "workflow_role": "reviewer",
        });
        start_session(&state, &params).expect("spawn ok");

        let diff = rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .expect("start_session must broadcast a manifest Added");
        match diff {
            crate::manifest::ManifestDiff::Added { uid: u, entry } => {
                assert_eq!(u, uid);
                assert_eq!(entry["workspace_id"], "ws-add");
                assert_eq!(entry["label"], "reviewer");
                assert_eq!(entry["session_type"], "claude-code");
                assert_eq!(entry["workflow_run_id"], "wf_add");
                assert_eq!(entry["workflow_role"], "reviewer");
            }
            other => panic!("expected ManifestDiff::Added, got {:?}", other),
        }
        kill_all_sessions(&state);
    }

    // === create_session / add_session (remote-session-execution Phase 1) ===

    /// Run `git <args>` in `dir`, asserting success. Test helper for the
    /// create_session worktree tests.
    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Set HOME to a tempdir and create a real git repo (one commit, so
    /// HEAD exists) at `$HOME/code/projects/<name>` — where
    /// `find_local_repo` looks. Holds the crate-wide env lock for the
    /// duration of `f` so HOME mutations don't race other tests. `f`
    /// receives `(home_path, repo_name)`.
    fn with_home_and_repo<F: FnOnce(&std::path::Path, &str)>(name: &str, f: F) {
        let _g = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()); }
        let repo = tmp.path().join("code/projects").join(name);
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["config", "user.email", "t@example.com"]);
        run_git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("README.md"), "x").unwrap();
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "-q", "-m", "init"]);
        f(tmp.path(), name);
        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Criterion: `create_session` for a `find_local_repo`-resolvable repo
    /// creates `~/.cm/worktrees/<repo>-<slug>` on `cm/<slug>`, registers
    /// the session, and broadcasts `ManifestDiff::Added`. Uses
    /// `engine=bash` so the spawn is deterministic (no `claude` binary);
    /// the claude-code argv/env/MCP-config resolution is pinned
    /// separately in `create_session_resolves_claude_argv_and_writes_daemon_mcp_config`.
    #[test]
    fn create_session_bash_creates_worktree_registers_and_broadcasts() {
        with_home_and_repo("phase1repo", |home, name| {
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let (rx, _guard) = {
                let s = state.lock().unwrap();
                s.manifest_watcher.subscribe()
            };
            let uid = fresh_test_uid();
            let params = json!({
                "uid": uid,
                "workspace_id": "ws-create-1",
                "label": "remote-bash",
                "engine": "bash",
                "repo_url": name,
                "slug": "phase-one",
            });
            let result = create_session(&state, &params).expect("create_session ok");

            // Response carries identity + resolved worktree + workspace.
            assert_eq!(result["session_uid"], uid);
            let expected_wt = home.join(".cm/worktrees/phase1repo-phase-one");
            assert_eq!(
                result["worktree_path"].as_str().unwrap(),
                expected_wt.to_string_lossy().as_ref(),
            );
            assert_eq!(result["workspace_id"], "ws-create-1");

            // Worktree exists on disk on branch cm/phase-one.
            assert!(expected_wt.join(".git").exists(), "worktree dir must exist");
            let branch = String::from_utf8(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&expected_wt)
                    .args(["rev-parse", "--abbrev-ref", "HEAD"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap();
            assert_eq!(branch.trim(), "cm/phase-one");

            // Session registered.
            assert!(state.lock().unwrap().sessions.contains_key(uid.as_str()));

            // ManifestDiff::Added broadcast (criterion #4).
            let diff = rx
                .recv_timeout(std::time::Duration::from_millis(500))
                .expect("create_session must broadcast ManifestDiff::Added");
            match diff {
                crate::manifest::ManifestDiff::Added { uid: u, entry } => {
                    assert_eq!(u, uid);
                    assert_eq!(entry["workspace_id"], "ws-create-1");
                    assert_eq!(entry["session_type"], "bash");
                }
                other => panic!("expected ManifestDiff::Added, got {:?}", other),
            }
            kill_all_sessions(&state);
        });
    }

    /// Phase 2 wiring: `create_session` resolves a repo that is NOT on
    /// disk by cloning an allowlisted URL into `~/.cm/repos/<name>`, then
    /// builds the worktree from the clone and spawns. Source repo lives
    /// outside `~/code/projects` so `find_local_repo` misses and the
    /// clone path is exercised.
    #[test]
    fn create_session_clones_allowlisted_repo_then_builds_worktree() {
        let _g = crate::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()); }

        // Source repo OUTSIDE ~/code/projects (one commit so it's clonable).
        let src = tempfile::tempdir().unwrap();
        let src_repo = src.path().join("clonerepo");
        std::fs::create_dir_all(&src_repo).unwrap();
        run_git(&src_repo, &["init", "-q"]);
        run_git(&src_repo, &["config", "user.email", "t@example.com"]);
        run_git(&src_repo, &["config", "user.name", "t"]);
        std::fs::write(src_repo.join("README.md"), "x").unwrap();
        run_git(&src_repo, &["add", "-A"]);
        run_git(&src_repo, &["commit", "-q", "-m", "init"]);
        let src_url = src_repo.to_string_lossy().into_owned();

        // State with an allowlist entry mapping clonerepo → src_url.
        let state = {
            let mut s = DaemonState::new();
            s.config.repos.push(crate::config::RepoAllowEntry {
                name: "clonerepo".into(),
                url: src_url.clone(),
            });
            Arc::new(Mutex::new(s))
        };
        let uid = fresh_test_uid();
        let params = json!({
            "uid": uid,
            "workspace_id": "ws-clone",
            "label": "cloned",
            "engine": "bash",
            "repo_url": "clonerepo",
            "slug": "work",
        });
        let result = create_session(&state, &params).expect("create_session via clone ok");
        assert_eq!(result["session_uid"], uid);
        // Cloned into ~/.cm/repos/clonerepo; worktree built from it.
        assert!(
            home.path().join(".cm/repos/clonerepo/.git").exists(),
            "repo cloned into ~/.cm/repos"
        );
        let wt = home.path().join(".cm/worktrees/clonerepo-work");
        assert!(wt.join(".git").exists(), "worktree built from the clone");
        assert_eq!(result["worktree_path"].as_str().unwrap(), wt.to_string_lossy().as_ref());
        kill_all_sessions(&state);

        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Criterion: an unresolvable `repo_url` returns a typed error naming
    /// the repo, and no session is created.
    #[test]
    fn create_session_unresolvable_repo_returns_not_found_naming_repo() {
        with_home_and_repo("realrepo", |_home, _name| {
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let params = json!({
                "uid": fresh_test_uid(),
                "workspace_id": "ws-x",
                "label": "x",
                "engine": "bash",
                "repo_url": "https://github.com/nobody/ghostrepo.git",
                "slug": "y",
            });
            let err = create_session(&state, &params).expect_err("unresolvable repo");
            assert_eq!(err.0, ErrorCode::NotFound);
            assert!(
                err.1.contains("ghostrepo"),
                "error must name the repo: {}",
                err.1
            );
            assert!(
                state.lock().unwrap().sessions.is_empty(),
                "no session on an unresolvable repo"
            );
        });
    }

    /// Criterion: a `git worktree add` failure cleans up (no orphan
    /// session, no orphan worktree). A `start_branch` that exists neither
    /// locally nor on origin (the test repo has no remote) makes every
    /// `git worktree add` attempt fail.
    #[test]
    fn create_session_worktree_add_failure_leaves_no_orphan() {
        with_home_and_repo("addfailrepo", |home, name| {
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let params = json!({
                "uid": fresh_test_uid(),
                "workspace_id": "ws-orphan",
                "label": "x",
                "engine": "bash",
                "repo_url": name,
                "start_branch": "branch-that-does-not-exist-anywhere",
                "slug": "nope",
            });
            let err = create_session(&state, &params)
                .expect_err("worktree add must fail for a nonexistent start_branch");
            assert_eq!(err.0, ErrorCode::Internal);
            assert!(
                state.lock().unwrap().sessions.is_empty(),
                "no orphan session after a worktree-add failure"
            );
            let wt = home.join(".cm/worktrees/addfailrepo-nope");
            assert!(
                !wt.exists(),
                "partial worktree must be cleaned up, found: {:?}",
                wt
            );
        });
    }

    /// Criterion: `add_session` for a daemon-known workspace reuses its
    /// existing worktree (does NOT call `create_worktree`), spawns into
    /// it, and returns it. Asserted by: the returned path is the existing
    /// worktree, and no `~/.cm/worktrees` dir is ever created.
    #[test]
    fn add_session_reuses_existing_worktree_without_creating() {
        let _g = crate::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()); }

        let wt = TempDir::new().unwrap(); // stands in for the existing worktree
        let state = state_with_workspace("ws-add-1", &wt);
        let uid = fresh_test_uid();
        let params = json!({
            "uid": uid,
            "workspace_id": "ws-add-1",
            "label": "second-session",
            "engine": "bash",
        });
        let result = add_session(&state, &params).expect("add_session ok");
        assert_eq!(result["session_uid"], uid);
        assert_eq!(
            result["worktree_path"].as_str().unwrap(),
            wt.path().to_string_lossy().as_ref(),
            "add_session must spawn into the workspace's existing worktree",
        );
        assert!(state.lock().unwrap().sessions.contains_key(uid.as_str()));
        // The load-bearing assertion: add_session never calls
        // create_worktree, so worktree_base() (`~/.cm/worktrees`) is never
        // created.
        assert!(
            !home.path().join(".cm/worktrees").exists(),
            "add_session must reuse the workspace worktree, never create one"
        );
        kill_all_sessions(&state);

        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Criterion: an unknown `workspace_id` returns `NotFound`.
    #[test]
    fn add_session_unknown_workspace_returns_not_found() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let params = json!({
            "uid": fresh_test_uid(),
            "workspace_id": "ws-ghost",
            "label": "x",
            "engine": "bash",
        });
        let err = add_session(&state, &params).expect_err("unknown workspace");
        assert_eq!(err.0, ErrorCode::NotFound);
        assert!(err.1.contains("ws-ghost"), "error must name the workspace: {}", err.1);
        assert!(state.lock().unwrap().sessions.is_empty());
    }

    /// Criterion: with `engine=claude-code`, the daemon-side resolution
    /// (shared by create_session/add_session via
    /// `compose_daemon_spawn_params`) produces `claude` argv from the
    /// daemon's `build_args` and writes `~/.cm/mcp/<uid>/claude.json`
    /// carrying the DAEMON's sockets. Tested at the resolver so it needs
    /// no real `claude` binary.
    #[test]
    fn create_session_resolves_claude_argv_and_writes_daemon_mcp_config() {
        let _g = crate::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()); }

        let state = Arc::new(Mutex::new(DaemonState::new()));
        let uid = fresh_test_uid();
        let wt = home.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();

        let full = compose_daemon_spawn_params(
            &state, &uid, "ws-c", "label", "claude-code", &wt, None, 100, 40, Some(&wt),
        )
        .expect("compose ok");

        // argv comes from the daemon's build_args.
        let argv: Vec<String> = full["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(argv[0], "claude");
        assert!(
            argv.iter().any(|a| a == "--mcp-config"),
            "claude argv must carry --mcp-config: {:?}",
            argv
        );

        // Daemon-written per-session MCP config with the daemon's sockets.
        let cfg = home.path().join(".cm/mcp").join(&uid).join("claude.json");
        assert!(cfg.exists(), "daemon must write ~/.cm/mcp/<uid>/claude.json");
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let env = &parsed["mcpServers"]["claude-manager"]["env"];
        assert!(
            env["CM_DAEMON_SOCKET"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
            "claude.json env must carry the daemon socket: {:?}",
            env
        );
        assert_eq!(env["CM_TUI_SESSION_ID"], uid);

        // Identity / engine / working_dir / cols / auto-register threaded through.
        assert_eq!(full["session_type"], "claude-code");
        assert_eq!(full["working_dir"].as_str().unwrap(), wt.to_string_lossy().as_ref());
        assert_eq!(full["cols"].as_u64(), Some(100));
        assert_eq!(
            full["worktree_path"].as_str().unwrap(),
            wt.to_string_lossy().as_ref(),
            "create passes the new worktree as the auto-register hint",
        );

        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Security: an invalid / path-traversal uid is rejected up front —
    /// BEFORE any filesystem side effect. Pre-fix, `build_args` wrote
    /// `~/.cm/mcp/<uid>/claude.json` (and create_session made the
    /// worktree) before start_session's uid validation ran, so a
    /// `../`/absolute uid could escape `~/.cm/mcp`.
    #[test]
    fn create_session_invalid_uid_rejected_before_side_effects() {
        with_home_and_repo("uidguardrepo", |home, name| {
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let params = json!({
                "uid": "../escape",
                "workspace_id": "ws-uid",
                "label": "x",
                "engine": "claude-code",
                "repo_url": name,
                "slug": "guard",
            });
            let err = create_session(&state, &params).expect_err("invalid uid");
            assert_eq!(err.0, ErrorCode::InvalidParams);
            // Nothing created: no worktree, no mcp dir, no session.
            assert!(
                !home.join(".cm/worktrees/uidguardrepo-guard").exists(),
                "no worktree on invalid uid"
            );
            assert!(
                !home.join(".cm/mcp").exists(),
                "no ~/.cm/mcp dir written on invalid uid"
            );
            assert!(state.lock().unwrap().sessions.is_empty());
        });
    }

    /// `add_session` mirrors the same up-front uid guard: an absolute
    /// uid is rejected before compose → build_args writes any mcp dir.
    #[test]
    fn add_session_invalid_uid_rejected_before_side_effects() {
        let _g = crate::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()); }

        let wt = TempDir::new().unwrap();
        let state = state_with_workspace("ws-uid-add", &wt);
        let params = json!({
            "uid": "/abs/escape",
            "workspace_id": "ws-uid-add",
            "label": "x",
            "engine": "claude-code",
        });
        let err = add_session(&state, &params).expect_err("invalid uid");
        assert_eq!(err.0, ErrorCode::InvalidParams);
        assert!(
            !home.path().join(".cm/mcp").exists(),
            "no ~/.cm/mcp dir written on invalid uid"
        );
        assert!(state.lock().unwrap().sessions.is_empty());

        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Data-loss guard: when `create_worktree` REUSES a pre-existing
    /// `cm/<slug>` worktree (slug/dir collision) and the subsequent spawn
    /// fails, the failure-cleanup must NOT delete that worktree — it may
    /// hold work this call didn't create. The spawn failure is forced via
    /// a uid collision (second create with a still-registered uid →
    /// `start_session` returns Conflict), which exercises the reuse +
    /// cleanup-skip path.
    #[test]
    fn create_session_failed_spawn_does_not_delete_reused_worktree() {
        with_home_and_repo("reuserepo", |home, name| {
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let uid = fresh_test_uid();

            // First create succeeds → worktree created, session registered.
            let p1 = json!({
                "uid": uid,
                "workspace_id": "ws-reuse",
                "label": "first",
                "engine": "bash",
                "repo_url": name,
                "slug": "shared",
            });
            create_session(&state, &p1).expect("first create ok");
            let wt = home.join(".cm/worktrees/reuserepo-shared");
            assert!(wt.join(".git").exists(), "first create makes the worktree");

            // Second create with the SAME uid → create_worktree reuses the
            // existing cm/shared worktree, then start_session rejects the
            // uid collision (Conflict). The reused worktree MUST survive.
            let p2 = json!({
                "uid": uid,
                "workspace_id": "ws-reuse",
                "label": "second",
                "engine": "bash",
                "repo_url": name,
                "slug": "shared",
            });
            let err = create_session(&state, &p2)
                .expect_err("uid collision must fail the spawn");
            assert_eq!(err.0, ErrorCode::Conflict);
            assert!(
                wt.join(".git").exists(),
                "a REUSED worktree must NOT be deleted when the spawn fails"
            );
            // The original session is untouched.
            assert!(state.lock().unwrap().sessions.contains_key(uid.as_str()));
            kill_all_sessions(&state);
        });
    }

    /// Security: a path-traversal slug is rejected before create_worktree
    /// (the slug feeds the worktree dir name + branch).
    #[test]
    fn create_session_traversal_slug_rejected() {
        with_home_and_repo("slugguardrepo", |home, name| {
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let params = json!({
                "uid": fresh_test_uid(),
                "workspace_id": "ws-slug",
                "label": "x",
                "engine": "bash",
                "repo_url": name,
                "slug": "../../etc/evil",
            });
            let err = create_session(&state, &params).expect_err("traversal slug");
            assert_eq!(err.0, ErrorCode::InvalidParams);
            assert!(err.1.contains("path separators"), "error names the cause: {}", err.1);
            assert!(!home.join(".cm/worktrees").exists(), "no worktree on traversal slug");
            assert!(state.lock().unwrap().sessions.is_empty());
        });
    }

    #[test]
    fn worktree_path_for_existing_workspace_is_ignored_not_clobbered() {
        // A pre-existing workspace must not have its worktree_path
        // overwritten by the auto-register fallback. The daemon's
        // logic checks `contains_key` first; a present entry
        // short-circuits.
        let dir = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let state = state_with_workspace("ws-existing", &dir);
        let params = json!({
            "uid": fresh_test_uid(),
            "workspace_id": "ws-existing",
            "label": "x",
            "argv": ["/bin/bash"],
            "working_dir": dir.path().display().to_string(),
            // A different path — must be ignored.
            "worktree_path": other.path().display().to_string(),
        });
        let result = start_session(&state, &params).expect("spawn ok");
        let uid = result["session_uid"].as_str().unwrap().to_string();

        // The original path stays.
        {
            let s = state.lock().unwrap();
            let ws = s.workspaces.get("ws-existing").unwrap();
            assert_eq!(
                ws.worktree_path.as_ref().map(|p| p.display().to_string()),
                Some(dir.path().display().to_string()),
                "pre-existing workspace's worktree_path must not be clobbered \
                 by an auto-register hint"
            );
            assert!(s.sessions.contains_key(&uid));
        }

        kill_all_sessions(&state);
    }

    #[test]
    fn cloud_workspace_can_be_spawned_into_when_caller_supplies_working_dir() {
        // Slice 10c-e-3b: the daemon no longer inspects
        // `workspace.worktree_path` — the caller's `working_dir`
        // is the source of truth for the spawn cwd. Pre-3b this
        // case returned `Conflict` because the daemon needed
        // workspace.worktree_path to populate cwd; now the
        // workspace lookup is purely an existence check.
        //
        // A cloud workspace (worktree_path = None) is fine as long
        // as the caller passes a usable `working_dir`. (The TUI's
        // own cloud path won't route through here in practice —
        // gcloud SSH stays TUI-local — but the daemon shouldn't
        // refuse on workspace shape alone.)
        let dir = TempDir::new().unwrap();
        let state = {
            let mut s = DaemonState::new();
            s.workspaces.insert(
                "ws-cloud".into(),
                ManifestWorkspace {
                    id: "ws-cloud".into(),
                    name: "cloud-ws".into(),
                    is_closed: false,
                    is_cloud: true,
                    worktree_path: None,
                    main_repo_path: None,
                    repo_url: None,
                    worker_vm: None,
                    worker_zone: None,
                    sessions: Vec::new(),
                    tombstones: Vec::new(),
                },
            );
            Arc::new(Mutex::new(s))
        };
        let params = json!({
            "uid": fresh_test_uid(),
            "workspace_id": "ws-cloud",
            "label": "x",
            "argv": ["/bin/bash"],
            "working_dir": dir.path().display().to_string(),
        });
        let result = start_session(&state, &params)
            .expect("spawn into cloud-shaped workspace must work when caller supplies working_dir");
        let uid = result["session_uid"].as_str().unwrap().to_string();
        assert!(state.lock().unwrap().sessions.contains_key(&uid));
        kill_all_sessions(&state);
    }

    // --- Happy path --------------------------------------------------------

    #[test]
    fn spawn_bash_session_inserts_into_registry_and_returns_uid() {
        // The named acceptance criterion: handler runs a real
        // `/bin/bash` against the workspace's worktree_path, the
        // session lands in `state.sessions`, and the returned uid
        // matches the registry key.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-1", &dir);
        let params = bash_params("ws-1", "shell", dir.path());
        let result = start_session(&state, &params).expect("spawn ok");
        let uid = result["session_uid"]
            .as_str()
            .expect("session_uid in response")
            .to_string();
        assert!(uid.starts_with("ts-"), "uid format must be ts-*: {}", uid);

        // Inserted under the same key.
        {
            let s = state.lock().unwrap();
            let session = s.sessions.get(&uid).expect("session registered");
            assert_eq!(session.uid, uid);
            assert_eq!(session.title, "shell");
        }

        kill_all_sessions(&state);
    }

    // ===== Slice 10c-e-3b-fix: TUI-supplied uid passthrough =====

    #[test]
    fn supplied_uid_is_used_verbatim_in_registry_and_response() {
        // Named acceptance: a TUI-supplied uid flows through
        // verbatim — `state.sessions` keys on it AND the response
        // echoes it. Pre-fix the daemon minted a fresh uid inside
        // start_session and the TUI's MCP config (with the
        // pre-generated uid baked in) was desynced from the
        // daemon registry.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-uid-passthrough", &dir);
        let supplied_uid = "ts-feedface-1";
        let params = json!({
            "uid": supplied_uid,
            "workspace_id": "ws-uid-passthrough",
            "label": "passthrough",
            "argv": ["/bin/bash"],
            "working_dir": dir.path().display().to_string(),
        });
        let result = start_session(&state, &params).expect("spawn");
        let returned = result["session_uid"].as_str().expect("session_uid");
        assert_eq!(
            returned, supplied_uid,
            "daemon must echo the TUI-supplied uid"
        );

        {
            let s = state.lock().unwrap();
            let session = s
                .sessions
                .get(supplied_uid)
                .expect("registry must key on supplied uid");
            assert_eq!(session.uid, supplied_uid);
        }

        kill_all_sessions(&state);
    }

    #[test]
    fn duplicate_uid_returns_conflict_without_clobbering_live_session() {
        // Collision guard: starting a session with a uid already
        // in `state.sessions` returns Conflict. Without this guard
        // the duplicate-insert would replace the live session's
        // DaemonSession handle and leak its child PTY.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-uid-dup", &dir);
        let uid = "ts-feedface-2";

        let params = json!({
            "uid": uid,
            "workspace_id": "ws-uid-dup",
            "label": "first",
            "argv": ["/bin/bash"],
            "working_dir": dir.path().display().to_string(),
        });
        let _ok = start_session(&state, &params).expect("first spawn");

        // Second spawn with the same uid — must fail.
        let dup_params = json!({
            "uid": uid,
            "workspace_id": "ws-uid-dup",
            "label": "duplicate",
            "argv": ["/bin/bash"],
            "working_dir": dir.path().display().to_string(),
        });
        let err = start_session(&state, &dup_params).expect_err("duplicate uid");
        assert_eq!(err.0, ErrorCode::Conflict);
        assert!(
            err.1.contains(uid),
            "error should name the colliding uid: {}",
            err.1
        );

        // First session is still in the registry and still alive
        // (not clobbered).
        {
            let s = state.lock().unwrap();
            let session = s
                .sessions
                .get(uid)
                .expect("first session must still be in registry");
            assert_eq!(
                session.title, "first",
                "the live session's metadata must come from the first spawn, not the duplicate"
            );
        }

        kill_all_sessions(&state);
    }

    #[test]
    fn concurrent_start_session_with_same_uid_one_succeeds_other_returns_conflict() {
        // Slice 10c-e-3b-fix6 named acceptance: two concurrent
        // threads call `start_session` with the SAME uid against
        // the same Arc<Mutex<DaemonState>>. The pre-fix6
        // collision check happened outside the insert-lock, so
        // both could pass the guard, both spawn, both insert
        // (last writer wins, first child orphans). The fix moves
        // the check to the same lock-acquisition that does the
        // insert; losing thread drops its PendingSession (Drop
        // SIGKILLs the child via pidfd) and returns Conflict.
        //
        // We hammer the race by spinning up N threads that all
        // race for the same uid. Exactly one must succeed; all
        // others must return Conflict; the registry must contain
        // exactly one session (no orphan).
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-race-uid", &dir);
        let shared_uid = fresh_test_uid();
        let working_dir_str = dir.path().display().to_string();

        let n_threads = 8;
        let mut handles = Vec::with_capacity(n_threads);
        for i in 0..n_threads {
            let state_c = state.clone();
            let uid_c = shared_uid.clone();
            let wd_c = working_dir_str.clone();
            handles.push(std::thread::spawn(move || {
                let params = json!({
                    "uid": uid_c,
                    "workspace_id": "ws-race-uid",
                    "label": format!("race-{}", i),
                    "argv": ["/bin/bash"],
                    "working_dir": wd_c,
                });
                start_session(&state_c, &params)
            }));
        }

        // Collect outcomes.
        let mut ok_count = 0;
        let mut conflict_count = 0;
        for h in handles {
            match h.join().unwrap() {
                Ok(_) => ok_count += 1,
                Err((code, _msg)) if code == ErrorCode::Conflict => conflict_count += 1,
                Err(other) => panic!("unexpected error from racing start_session: {:?}", other),
            }
        }

        assert_eq!(
            ok_count, 1,
            "exactly one start_session should succeed; got {} successes (race fix broken: \
             collision check not under insert lock)",
            ok_count
        );
        assert_eq!(
            conflict_count,
            n_threads - 1,
            "all other start_sessions should return Conflict; got {}",
            conflict_count
        );

        // Registry holds exactly the one winning session — no
        // orphan from a losing thread's spawn.
        {
            let s = state.lock().unwrap();
            assert_eq!(
                s.sessions.len(),
                1,
                "exactly one session in registry; got {} — losing threads stranded their children",
                s.sessions.len()
            );
            assert!(s.sessions.contains_key(&shared_uid));
        }

        kill_all_sessions(&state);
    }

    #[test]
    fn malformed_uid_returns_invalid_params() {
        // Sanity-check on format: the daemon validates the uid
        // matches the TUI's `ts-<hex>-<hex>` pattern. A malformed
        // uid is InvalidParams, not Internal — clients get a typed
        // pointer to the problem.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-bad-uid", &dir);

        for bad in &[
            "",                    // empty
            "no-ts-prefix",        // missing ts- prefix
            "ts-",                 // no segments
            "ts-abc",              // one segment
            "ts-abc-def-ghi",      // three segments
            "ts-XYZ-123",          // non-hex
            "ts-abc-",             // empty second segment
        ] {
            let params = json!({
                "uid": bad,
                "workspace_id": "ws-bad-uid",
                "label": "x",
                "argv": ["/bin/bash"],
                "working_dir": dir.path().display().to_string(),
            });
            let err = start_session(&state, &params).expect_err(
                &format!("malformed uid {:?} must error", bad),
            );
            assert_eq!(
                err.0,
                ErrorCode::InvalidParams,
                "uid {:?} should produce InvalidParams, got {:?}",
                bad,
                err
            );
            assert!(
                err.1.contains("uid") || err.1.contains("ts-"),
                "error should name the uid field: {}",
                err.1
            );
        }
    }

    // ===== Slice 10c-e-3b-fix2: memory-cap plumbing =====
    //
    // The pre-slice-10d `memory_cap_bytes_signals_kills_dir_default`
    // test sent `memory_cap_bytes` against a bare `/bin/bash` argv
    // (no systemd-run wrapper) and asserted the spawn succeeded.
    // Post-slice-10d watcher-fix #1 the daemon performs /proc
    // cgroup discovery + cm-sess-*.scope verification when a cap
    // is requested — a bare `bash` ends up in the test runner's
    // own cgroup, which doesn't match the pattern, so the spawn
    // now (correctly) returns `Internal`. That's the new positive
    // assertion, covered by `cap_request_outside_cm_sess_scope_returns_internal`
    // below. The old test is removed as redundant.

    // Slice 10d watcher-fix #1 changed the cgroup-verification
    // arm from caller-supplied-path verification to /proc-based
    // discovery. Coverage now lives in:
    //   - `cap_request_outside_cm_sess_scope_returns_internal`:
    //     the failure mode when the child isn't in a
    //     `cm-sess-*.scope` cgroup (e.g. systemd-run wasn't
    //     used, or scope setup failed mid-flight).
    //   - `caller_supplied_cgroup_path_is_silently_dropped`:
    //     the security invariant — a forged path in the JSON
    //     does NOT influence what the daemon operates on.
    //   - The /proc parse-layer is exhaustively tested in
    //     `daemon/src/path.rs` (parse_cgroup_v2_line_*).
    //   - The success path requires a real `systemd-run`
    //     scope, which depends on environment; integration
    //     testing tracks via the `#[ignore]`-gated e2e in
    //     `daemon/src/session_watch.rs`.

    #[test]
    fn cap_request_outside_cm_sess_scope_returns_internal() {
        // Slice 10d watcher-fix #1 named acceptance: when a
        // memory cap is requested (`memory_cap_bytes: Some`)
        // but the spawned child isn't in a `cm-sess-*.scope`
        // cgroup, `discover_session_cgroup_path` rejects on
        // basename pattern → `start_session` returns Internal.
        // The PendingSession's Drop SIGKILLs the spawned child
        // via pidfd; no registry residue.
        //
        // In this test the argv is bare `/bin/sleep 30` — no
        // systemd-run wrapper — so the spawned child ends up
        // in the test runner's own cgroup (whatever the test
        // binary is in), which does NOT match `cm-sess-*.scope`.
        // This is the exact threat model: a caller that asks
        // for a cap but spawns the agent outside the wrapper.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-outside-scope", &dir);
        // Sub-2b-3 review-4 #1: the cap triple is now all-or-
        // nothing at the entry point — `memory_cap_bytes` alone
        // is rejected as `InvalidParams`. Send the full triple
        // so the test reaches the cgroup-discovery path it's
        // pinning. The hard byte count and cgroup_prefix
        // values are unimportant here; only the discovery step
        // is being tested.
        let params = json!({
            "uid": fresh_test_uid(),
            "workspace_id": "ws-outside-scope",
            "label": "outside-scope-test",
            "argv": ["/bin/sleep", "30"],
            "working_dir": dir.path().display().to_string(),
            "memory_cap_bytes": 64 * 1024 * 1024u64,
            "memory_cap_hard_bytes": 128 * 1024 * 1024u64,
            "cgroup_prefix": "/sys/fs/cgroup/user.slice",
        });
        let err =
            start_session(&state, &params).expect_err("discovery should fail");
        assert_eq!(err.0, ErrorCode::Internal, "expected Internal, got {:?}", err);
        assert!(
            err.1.contains("cgroup")
                && (err.1.contains("cm-sess") || err.1.contains("/proc")),
            "error should name the discovery / pattern-match failure: {}",
            err.1
        );
        assert!(
            state.lock().unwrap().sessions.is_empty(),
            "discovery failure must NOT leave a session in the registry"
        );
    }

    /// Slice 10d watcher-fix #1 named security invariant:
    /// caller-supplied `cgroup_path` in the JSON request is
    /// silently ignored (the field is no longer in
    /// `StartSessionParams`; serde drops unknown fields by
    /// default). The daemon's behavior must depend only on
    /// what `/proc/<pid>/cgroup` says, not on what the caller
    /// sent.
    ///
    /// Pre-fix the daemon trusted the caller's path: a buggy
    /// or malicious caller could pre-populate a cgroup with
    /// PIDs from unrelated processes (shell, another worker)
    /// and have the daemon's watcher SIGKILL those PIDs on
    /// the first breach. This test pins the new contract.
    ///
    /// We assert by sending an obviously-hostile path
    /// (`/sys/fs/cgroup/this-is-not-a-cm-sess-scope`) and
    /// confirming (a) the daemon doesn't echo it, (b) the
    /// error message — produced by /proc discovery, not by
    /// trying to read the supplied path — doesn't name it.
    /// If the supplied path were still in use, either of
    /// those would surface it.
    #[test]
    fn caller_supplied_cgroup_path_is_silently_dropped() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-forged-path", &dir);
        let hostile = "/sys/fs/cgroup/this-is-not-a-cm-sess-scope".to_string();
        // Sub-2b-3 review-4 #1: send the full cap triple so
        // the test reaches the discovery branch (post-validation).
        // The hostile `cgroup_path` field below is the
        // pre-fix legacy that must STILL be ignored.
        let params = json!({
            "uid": fresh_test_uid(),
            "workspace_id": "ws-forged-path",
            "label": "forged-path-test",
            "argv": ["/bin/sleep", "30"],
            "working_dir": dir.path().display().to_string(),
            "memory_cap_bytes": 64 * 1024 * 1024u64,
            "memory_cap_hard_bytes": 128 * 1024 * 1024u64,
            "cgroup_prefix": "/sys/fs/cgroup/user.slice",
            // The old caller-supplied field. Daemon should
            // ignore it entirely; the JSON deserializer drops
            // unknown fields by default.
            "cgroup_path": hostile.clone(),
        });
        let err = start_session(&state, &params)
            .expect_err("discovery still fails (child is outside cm-sess scope)");
        assert_eq!(err.0, ErrorCode::Internal);
        assert!(
            !err.1.contains(&hostile),
            "error message must NOT contain the hostile caller-supplied path \
             — if it does, the daemon is still observing the field. Got: {}",
            err.1
        );
        // Registry empty (Internal path → pending drop kills + waitpids).
        assert!(state.lock().unwrap().sessions.is_empty());
    }

    /// Slice 10d-memory-cap-relocation review fix #0 (watcher
    /// spawn fallibility): when `spawn_watcher` fails (e.g.
    /// thread-spawn ulimit hit, resource exhaustion),
    /// `start_session` must return `Internal` without leaving
    /// the session in the registry, and the spawned child must
    /// be reaped via `PendingSession`'s Drop.
    ///
    /// **Why `#[ignore]`:** post-watcher-fix #1, the daemon
    /// performs /proc-based cgroup discovery BEFORE reaching
    /// the watcher-spawn step. In a test environment without
    /// a real `systemd-run --user` scope (CI, dev box without
    /// systemd-user, etc.) discovery returns `Err` because the
    /// spawned child isn't in a `cm-sess-*.scope` cgroup. The
    /// watcher-spawn step is unreachable from a test, so
    /// driving the failure-injection at the integration layer
    /// requires a real cm-sess-*.scope.
    ///
    /// The contract is still pinned in two ways:
    ///   1. `spawn_watcher_propagates_thread_spawn_error` (in
    ///      `daemon/src/session_watch.rs::tests`) proves
    ///      `spawn_watcher` returns `Err` rather than panicking
    ///      on `Builder::spawn` failure.
    ///   2. `cap_request_outside_cm_sess_scope_returns_internal`
    ///      exercises the structurally-identical
    ///      `drop(pending); return Internal` cleanup pattern at
    ///      the discovery-failure layer — the same code path
    ///      style the watcher-spawn-failure arm uses.
    ///
    /// To run this test against a real systemd-run environment:
    /// `cargo test -p cm-daemon watcher_spawn_failure -- --ignored`.
    #[test]
    #[ignore]
    fn watcher_spawn_failure_unwinds_with_no_registry_residue() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-watcher-fail", &dir);
        let uid = fresh_test_uid();
        // In a real systemd-run-enabled environment, the
        // caller would set up argv to wrap with
        // `systemd-run --user --scope --unit=cm-sess-...`. We
        // can't reliably do that from this test runner; the
        // assertion shape below is what the test would verify
        // if discovery had succeeded.
        let params = json!({
            "uid": uid,
            "workspace_id": "ws-watcher-fail",
            "label": "watcher-fail-test",
            "argv": ["/bin/sleep", "30"],
            "working_dir": dir.path().display().to_string(),
            "memory_cap_bytes": 64 * 1024 * 1024u64,
        });
        let failing_spawn: crate::session_watch::WatcherSpawnFn =
            Box::new(|_name, _body| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "injected-spawn-failure",
                ))
            });
        let err = start_session_with_spawn_fn(&state, &params, failing_spawn)
            .expect_err("watcher spawn failure must propagate as error, not panic");
        assert_eq!(err.0, ErrorCode::Internal);
        // Either the discovery failure (likely in CI) or the
        // watcher-spawn failure (only reachable in a real
        // systemd-run environment) — both are valid Internal
        // shapes for this test's intent.
        assert!(
            err.1.contains("spawn cgroup-OOM watcher thread failed")
                || err.1.contains("injected-spawn-failure")
                || err.1.contains("cgroup discovery"),
            "expected discovery or spawn failure: {}",
            err.1
        );
        let state_guard = state.lock().unwrap();
        assert!(!state_guard.sessions.contains_key(&uid));
        assert!(state_guard.sessions.is_empty());
    }

    #[test]
    fn no_memory_cap_means_no_cgroup_path_in_response() {
        // Symmetric: a spawn without memory_cap fields should
        // produce a response WITHOUT a `cgroup_path` key (rather
        // than `cgroup_path: null` or an empty string). The
        // local-spawn path mirrors this — Session.cgroup_path =
        // None when memory_cap is None.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-nocap", &dir);
        let params = bash_params("ws-nocap", "no-cap", dir.path());
        let result = start_session(&state, &params).expect("spawn ok");
        assert!(
            result.get("cgroup_path").is_none(),
            "response should omit cgroup_path entirely when no memory cap, got: {}",
            result
        );
        kill_all_sessions(&state);
    }

    #[test]
    fn is_valid_session_uid_unit() {
        // Direct unit test on the validator. Documents the format
        // contract (`ts-<hex>-<hex>`).
        assert!(is_valid_session_uid("ts-abc-1"));
        assert!(is_valid_session_uid("ts-deadbeef-cafe"));
        assert!(is_valid_session_uid("ts-0-0"));

        assert!(!is_valid_session_uid(""));
        assert!(!is_valid_session_uid("ts-"));
        assert!(!is_valid_session_uid("ts-abc"));
        assert!(!is_valid_session_uid("ts-abc-def-ghi"));
        assert!(!is_valid_session_uid("xs-abc-1"));
        assert!(!is_valid_session_uid("ts-XYZ-1")); // non-hex
        assert!(!is_valid_session_uid("ts--1")); // empty first segment
        assert!(!is_valid_session_uid("ts-abc-")); // empty second segment
    }

    #[test]
    fn cm_tui_session_id_env_is_injected_for_child() {
        // The named contract from `spawn_agent_session`: the child's
        // env has `CM_TUI_SESSION_ID=<own uid>`. Daemon-side spawn
        // must do the same so kill-log correlation continues to
        // work the way DESIGN_MEMORY_CAP.md describes.
        //
        // We can't directly inspect `DaemonSession`'s injected env
        // (it's private state inside portable-pty), so test the
        // observable consequence: run `bash -c 'echo
        // $CM_TUI_SESSION_ID'` and confirm the uid comes back
        // through the fanout.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-env", &dir);
        let params = bash_params("ws-env", "env-test", dir.path());
        let result = start_session(&state, &params).expect("ok");
        let uid = result["session_uid"].as_str().unwrap().to_string();

        // Subscribe to the fanout and send input under the state lock.
        let rx = {
            let mut s = state.lock().unwrap();
            let session = s.sessions.get_mut(&uid).expect("registered");
            let rx = session.fanout.subscribe();
            session
                .send_input(b"echo CM_TUI_SESSION_ID=$CM_TUI_SESSION_ID\n")
                .expect("send_input");
            rx
        };

        // Drain bytes until we either see the uid or time out.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut accumulated = Vec::new();
        while std::time::Instant::now() < deadline {
            let remaining =
                deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(chunk) => {
                    accumulated.extend_from_slice(&chunk);
                    let text = String::from_utf8_lossy(&accumulated);
                    if text.contains(&format!("CM_TUI_SESSION_ID={}", uid)) {
                        kill_all_sessions(&state);
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        kill_all_sessions(&state);
        panic!(
            "expected CM_TUI_SESSION_ID={} in child output, got:\n{}",
            uid,
            String::from_utf8_lossy(&accumulated)
        );
    }

    /// T_g3f_env_injection_complete (named acceptance for
    /// slice 12f): daemon-side spawn env contains every var
    /// the doc enumerates — `CM_TUI_SOCKET`,
    /// `CM_TUI_SESSION_ID`, `CM_DAEMON_SOCKET`, `CM_MCP_SERVER`,
    /// `CM_API_URL`, `CM_API_TOKEN`, plus `CM_WORKFLOW_RUN_ID` /
    /// `CM_ROLE` when the spawn carries workflow context.
    ///
    /// We can't dump portable-pty's env hashmap directly, so
    /// we drive a `/bin/bash -c 'env > $OUTFILE; exit 0'`
    /// child and read the dumped env from disk after the
    /// session exits.
    #[test]
    fn t_g3f_env_injection_complete() {
        let dir = TempDir::new().unwrap();
        let env_dump = dir.path().join("env-dump.txt");
        // Pre-populate daemon.toml-like values in state.
        let state = {
            let mut s = DaemonState::new();
            s.workspaces.insert(
                "ws-envinj".into(),
                ManifestWorkspace {
                    id: "ws-envinj".into(),
                    name: "test-ws".into(),
                    is_closed: false,
                    is_cloud: false,
                    worktree_path: Some(dir.path().to_path_buf()),
                    main_repo_path: None,
                    repo_url: None,
                    worker_vm: None,
                    worker_zone: None,
                    sessions: Vec::new(),
                    tombstones: Vec::new(),
                },
            );
            s.config = crate::config::DaemonConfig {
                mcp_server_path:
                    "/opt/cm-daemon/mcp_server/server.py".to_string(),
                api_url: "http://10.150.0.2:8000".to_string(),
                api_token: "test-token-XYZ".to_string(),
                log_path: String::new(),
                workflows_dir: String::new(),
                auth: Default::default(),
                tls: None,
                repos_dir: String::new(),
                allow_clone: false,
                repos: Vec::new(),
                scheduler: Default::default(),
            };
            Arc::new(Mutex::new(s))
        };
        let uid = fresh_test_uid();
        let argv = vec![
            "/bin/bash".to_string(),
            "-c".to_string(),
            format!(
                "env > {} && sleep 30",
                env_dump.display()
            ),
        ];
        let params = json!({
            "uid": uid,
            "workspace_id": "ws-envinj",
            "label": "env-inj",
            "argv": argv,
            "working_dir": dir.path().display().to_string(),
            // Workflow context — exercises the CM_WORKFLOW_RUN_ID /
            // CM_ROLE injection branch.
            "workflow_run_id": "wf-test-123",
            "workflow_role": "worker",
        });
        let _result = start_session(&state, &params).expect("ok");

        // Wait for the env-dump file to exist + be non-empty
        // (the bash child runs `env > ...` then sleeps, so
        // the file appears within milliseconds of spawn).
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(5);
        loop {
            if env_dump.is_file() {
                if let Ok(meta) = std::fs::metadata(&env_dump) {
                    if meta.len() > 0 {
                        break;
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                kill_all_sessions(&state);
                panic!(
                    "env dump file {} did not appear within 5s",
                    env_dump.display(),
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let dumped = std::fs::read_to_string(&env_dump)
            .expect("read env dump");
        kill_all_sessions(&state);

        // Parse `KEY=VALUE\n` lines.
        let env: std::collections::HashMap<&str, &str> = dumped
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();

        // Doc-spec'd vars MUST all be present with the
        // expected values.
        assert_eq!(
            env.get("CM_TUI_SESSION_ID").copied(),
            Some(uid.as_str()),
            "CM_TUI_SESSION_ID must be the daemon-minted uid",
        );
        // Daemon socket var — value is the daemon's own
        // socket path. Verify it's set + non-empty rather
        // than pinning a specific filesystem path (test
        // home_lock setup varies).
        assert!(
            env.contains_key("CM_TUI_SOCKET"),
            "CM_TUI_SOCKET MUST be set",
        );
        assert!(
            env.contains_key("CM_DAEMON_SOCKET"),
            "CM_DAEMON_SOCKET MUST be set",
        );
        let tui_sock = env.get("CM_TUI_SOCKET").copied().unwrap();
        let daemon_sock = env.get("CM_DAEMON_SOCKET").copied().unwrap();
        assert!(
            !tui_sock.is_empty() && !daemon_sock.is_empty(),
            "both socket vars MUST be non-empty",
        );
        assert_eq!(
            tui_sock, daemon_sock,
            "CM_TUI_SOCKET == CM_DAEMON_SOCKET per 12f \
             (both point at daemon's own socket)",
        );

        // Daemon-config-driven vars: MUST take their values
        // from daemon.toml (the pre-populated state.config
        // above), NOT from the caller's env (which was empty
        // in this test).
        assert_eq!(
            env.get("CM_MCP_SERVER").copied(),
            Some("/opt/cm-daemon/mcp_server/server.py"),
            "CM_MCP_SERVER must come from daemon.toml",
        );
        assert_eq!(
            env.get("CM_API_URL").copied(),
            Some("http://10.150.0.2:8000"),
            "CM_API_URL must come from daemon.toml",
        );
        assert_eq!(
            env.get("CM_API_TOKEN").copied(),
            Some("test-token-XYZ"),
            "CM_API_TOKEN must come from daemon.toml",
        );

        // Workflow context vars from the RPC params.
        assert_eq!(
            env.get("CM_WORKFLOW_RUN_ID").copied(),
            Some("wf-test-123"),
        );
        assert_eq!(env.get("CM_ROLE").copied(), Some("worker"));
    }

    /// 12f: when daemon.toml is empty / missing (local
    /// workstation case), the always-on injections still
    /// fire (CM_TUI_SOCKET, CM_DAEMON_SOCKET,
    /// CM_TUI_SESSION_ID), and workflow context isn't
    /// injected if the RPC params don't carry it.
    ///
    /// Honest gap: we can't cleanly assert "CM_API_URL is
    /// NOT injected" from the child's env because the test
    /// runner's parent env already has CM_API_URL set
    /// (developer workstation, CI env, etc.) and the
    /// portable-pty child inherits parent env by default.
    /// The structural pin is that the daemon code only
    /// inserts these vars when the config field is
    /// non-empty (see the `!st.config.api_url.is_empty()`
    /// guards in `start_session`). `t_g3f_env_injection_complete`
    /// proves the positive case (non-empty config → injected
    /// values reach the child).
    #[test]
    fn env_injection_with_empty_config_skips_config_vars() {
        let dir = TempDir::new().unwrap();
        let env_dump = dir.path().join("env-dump.txt");
        let state = {
            let mut s = DaemonState::new();
            s.workspaces.insert(
                "ws-empty-cfg".into(),
                ManifestWorkspace {
                    id: "ws-empty-cfg".into(),
                    name: "test-ws".into(),
                    is_closed: false,
                    is_cloud: false,
                    worktree_path: Some(dir.path().to_path_buf()),
                    main_repo_path: None,
                    repo_url: None,
                    worker_vm: None,
                    worker_zone: None,
                    sessions: Vec::new(),
                    tombstones: Vec::new(),
                },
            );
            // config defaults to empty fields — matches
            // the local-workstation case (no daemon.toml).
            Arc::new(Mutex::new(s))
        };
        let uid = fresh_test_uid();
        let argv = vec![
            "/bin/bash".to_string(),
            "-c".to_string(),
            format!("env > {} && sleep 30", env_dump.display()),
        ];
        let params = json!({
            "uid": uid,
            "workspace_id": "ws-empty-cfg",
            "label": "empty-cfg",
            "argv": argv,
            "working_dir": dir.path().display().to_string(),
        });
        let _r = start_session(&state, &params).expect("ok");

        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(5);
        loop {
            if env_dump.is_file() {
                if let Ok(m) = std::fs::metadata(&env_dump) {
                    if m.len() > 0 {
                        break;
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                kill_all_sessions(&state);
                panic!("env dump did not appear");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let dumped = std::fs::read_to_string(&env_dump).unwrap();
        kill_all_sessions(&state);
        let env: std::collections::HashMap<&str, &str> = dumped
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        // Always-on injections fire regardless of config.
        assert!(env.contains_key("CM_TUI_SOCKET"));
        assert!(env.contains_key("CM_DAEMON_SOCKET"));
        assert_eq!(
            env.get("CM_TUI_SESSION_ID").copied(),
            Some(uid.as_str()),
        );
        // Workflow context vars are PARTICIPANT-ONLY. This spawn carries no
        // workflow params, so the child must have neither var — even when the
        // suite is run from INSIDE a workflow participant session (the daemon
        // process then has CM_WORKFLOW_RUN_ID/CM_ROLE in its env and children
        // would otherwise inherit them). `DaemonSession::spawn` strips both for
        // non-participants (see the participant-only scrub in session.rs), so
        // this negative assertion holds regardless of who runs the suite.
        assert!(
            !env.contains_key("CM_WORKFLOW_RUN_ID"),
            "non-workflow spawn MUST NOT carry CM_WORKFLOW_RUN_ID (no inject, no inherit)",
        );
        assert!(
            !env.contains_key("CM_ROLE"),
            "non-workflow spawn MUST NOT carry CM_ROLE (no inject, no inherit)",
        );
    }

    /// 12f: structural pin on the start_session env-injection
    /// block. Every doc-spec'd var must have its insertion
    /// site in the function's source. Pins what the runtime
    /// test would otherwise miss (the inherited-env-pollution
    /// problem documented in `env_injection_with_empty_config_skips_config_vars`).
    #[test]
    /// 12f F1 (acceptance): the daemon-injected CM_TUI_SOCKET
    /// / CM_DAEMON_SOCKET env values MUST be absolute paths,
    /// even when the daemon was launched with a relative
    /// `$CM_DAEMON_SOCKET`. Pre-F1 the raw `default_socket_path()`
    /// value was injected — a relative parent value would
    /// make the agent dial relative to its own worktree cwd
    /// (wrong location, NotFound at MCP-routing time).
    #[test]
    fn env_injection_absolutizes_relative_socket_paths() {
        // Serialize against other env-mutating tests in this
        // binary that touch CM_DAEMON_SOCKET (the
        // planning_client test_env_lock covers CM_API_*; we
        // reuse the same lock — process-wide env mutation
        // can't safely parallelize across modules anyway).
        let _g = crate::planning_client::test_env_lock();

        let dir = TempDir::new().unwrap();
        let env_dump = dir.path().join("env-dump.txt");

        // Snapshot the pre-test value so we can restore.
        // SAFETY: serialized by the env_lock above.
        let prior_sock = std::env::var_os("CM_DAEMON_SOCKET");
        unsafe {
            std::env::set_var("CM_DAEMON_SOCKET", "daemon.sock");
        }

        let state = {
            let mut s = DaemonState::new();
            s.workspaces.insert(
                "ws-abs".into(),
                ManifestWorkspace {
                    id: "ws-abs".into(),
                    name: "test-ws".into(),
                    is_closed: false,
                    is_cloud: false,
                    worktree_path: Some(dir.path().to_path_buf()),
                    main_repo_path: None,
                    repo_url: None,
                    worker_vm: None,
                    worker_zone: None,
                    sessions: Vec::new(),
                    tombstones: Vec::new(),
                },
            );
            Arc::new(Mutex::new(s))
        };

        let uid = fresh_test_uid();
        let argv = vec![
            "/bin/bash".to_string(),
            "-c".to_string(),
            format!("env > {} && sleep 30", env_dump.display()),
        ];
        let params = json!({
            "uid": uid,
            "workspace_id": "ws-abs",
            "label": "abs-paths",
            "argv": argv,
            "working_dir": dir.path().display().to_string(),
        });
        let _result = start_session(&state, &params).expect("spawn ok");

        // Wait for env dump.
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(5);
        loop {
            if env_dump.is_file() {
                if let Ok(m) = std::fs::metadata(&env_dump) {
                    if m.len() > 0 {
                        break;
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                kill_all_sessions(&state);
                // Restore env before bailing.
                unsafe {
                    match &prior_sock {
                        Some(v) => std::env::set_var("CM_DAEMON_SOCKET", v),
                        None => std::env::remove_var("CM_DAEMON_SOCKET"),
                    }
                }
                panic!("env dump did not appear in 5s");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let dumped = std::fs::read_to_string(&env_dump).unwrap();
        kill_all_sessions(&state);

        // Restore CM_DAEMON_SOCKET before assertions so a
        // panic on assert doesn't leak the test value into
        // subsequent (serialized) tests.
        unsafe {
            match prior_sock {
                Some(v) => std::env::set_var("CM_DAEMON_SOCKET", v),
                None => std::env::remove_var("CM_DAEMON_SOCKET"),
            }
        }

        // Parse env. The daemon's injection MUST have
        // overridden the parent-inherited relative
        // `CM_DAEMON_SOCKET=daemon.sock` with an absolute
        // path (cwd-joined via `absolutize_socket_path`).
        let env: std::collections::HashMap<&str, &str> = dumped
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        let tui_sock = env
            .get("CM_TUI_SOCKET")
            .copied()
            .expect("CM_TUI_SOCKET must be set");
        let daemon_sock = env
            .get("CM_DAEMON_SOCKET")
            .copied()
            .expect("CM_DAEMON_SOCKET must be set");
        assert!(
            tui_sock.starts_with('/'),
            "CM_TUI_SOCKET MUST be absolute (12f F1); got: {:?}",
            tui_sock,
        );
        assert!(
            daemon_sock.starts_with('/'),
            "CM_DAEMON_SOCKET MUST be absolute (12f F1); \
             pre-fix a relative `$CM_DAEMON_SOCKET` parent value \
             would have been injected verbatim, making the \
             agent dial relative to its worktree cwd; got: {:?}",
            daemon_sock,
        );
        // Sanity: the relative form is NOT what landed in
        // the child's env.
        assert_ne!(
            tui_sock, "daemon.sock",
            "raw relative parent value must NOT be injected",
        );
        assert_ne!(daemon_sock, "daemon.sock");
        // Both pin to the same absolute value.
        assert_eq!(
            tui_sock, daemon_sock,
            "CM_TUI_SOCKET == CM_DAEMON_SOCKET (12f invariant)",
        );
        // Ends with the relative filename (cwd was joined,
        // not replaced).
        assert!(
            daemon_sock.ends_with("daemon.sock"),
            "absolutized path should preserve the filename; got: {:?}",
            daemon_sock,
        );
    }

    #[test]
    fn env_injection_source_pins_every_spec_var() {
        let src = include_str!("methods.rs");
        // Locate `pub(crate) fn start_session_with_spawn_fn`
        // body bounds.
        let sig = "pub(crate) fn start_session_with_spawn_fn(";
        let start = src.find(sig).expect("must find sig");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\npub fn ")
            .or_else(|| rest[1..].find("\npub(crate) fn "))
            .or_else(|| rest[1..].find("\nfn "))
            .map(|i| 1 + i)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        // Always-on insertions: CM_TUI_SOCKET, CM_DAEMON_SOCKET,
        // CM_TUI_SESSION_ID. Sockets come from
        // `crate::default_socket_path()`.
        assert!(
            body.contains("\"CM_TUI_SOCKET\".into()"),
            "start_session must insert CM_TUI_SOCKET (12f); body:\n{}",
            body,
        );
        assert!(
            body.contains("\"CM_DAEMON_SOCKET\".into()"),
            "start_session must insert CM_DAEMON_SOCKET (12f)",
        );
        assert!(
            body.contains("\"CM_TUI_SESSION_ID\".into()"),
            "start_session must insert CM_TUI_SESSION_ID (12f)",
        );
        // 12f F1: sockets MUST be absolutized before injection.
        // A relative `$CM_DAEMON_SOCKET` parent value would
        // otherwise be injected verbatim and the agent would
        // dial relative to its own worktree cwd. Pin the
        // helper call.
        assert!(
            body.contains("crate::path::absolutize_socket_path"),
            "start_session must absolutize the daemon socket \
             path before injecting it (12f F1); body:\n{}",
            body,
        );

        // Config-gated insertions: CM_MCP_SERVER / CM_API_URL /
        // CM_API_TOKEN, each guarded by `!st.config.<field>.is_empty()`.
        for var in &["CM_MCP_SERVER", "CM_API_URL", "CM_API_TOKEN"] {
            assert!(
                body.contains(&format!("\"{}\".into()", var)),
                "start_session must insert {} when config \
                 carries it (12f); body:\n{}",
                var,
                body,
            );
        }
        // The config-driven inserts MUST be guarded so empty
        // config doesn't blast over inherited values.
        assert!(
            body.contains("!st.config.mcp_server_path.is_empty()"),
            "CM_MCP_SERVER insertion must be guarded on \
             non-empty config field (12f)",
        );
        assert!(
            body.contains("!st.config.api_url.is_empty()"),
            "CM_API_URL insertion must be guarded",
        );
        assert!(
            body.contains("!st.config.api_token.is_empty()"),
            "CM_API_TOKEN insertion must be guarded",
        );

        // Workflow context insertions: CM_WORKFLOW_RUN_ID /
        // CM_ROLE, gated on the RPC param being Some.
        assert!(
            body.contains("\"CM_WORKFLOW_RUN_ID\".into()"),
            "CM_WORKFLOW_RUN_ID insertion must exist (12f)",
        );
        assert!(
            body.contains("\"CM_ROLE\".into()"),
            "CM_ROLE insertion must exist (12f)",
        );
    }

    #[test]
    fn two_consecutive_spawns_with_distinct_uids_both_land_in_registry() {
        // Slice 10c-e-3b-fix: uids are caller-supplied. The TUI's
        // generator (`tui/src/app.rs::new_session_uid`) mixes a
        // nanos timestamp with an atomic counter, so distinct
        // uids are the steady state. The daemon must accept both
        // without conflict and key the registry on each.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-two", &dir);
        // Two requests with intentionally distinct uids
        // (`bash_params` already mints a fresh uid per call via
        // `fresh_test_uid()`).
        let params1 = bash_params("ws-two", "x", dir.path());
        let params2 = bash_params("ws-two", "x", dir.path());
        let r1 = start_session(&state, &params1).expect("first");
        let r2 = start_session(&state, &params2).expect("second");
        let uid1 = r1["session_uid"].as_str().unwrap();
        let uid2 = r2["session_uid"].as_str().unwrap();
        assert_ne!(uid1, uid2, "two spawns must produce distinct uids");
        assert_eq!(state.lock().unwrap().sessions.len(), 2);
        kill_all_sessions(&state);
    }

    // --- Reaper-driven cleanup (slice-10c-c review fix #2) -----------------

    #[test]
    fn exited_session_is_removed_from_registry_within_bound() {
        // Headline acceptance for fix #2: after a session's child
        // exits, the reaper-installed callback removes the entry
        // from `state.sessions`. Verifiable by spawning a session
        // that exits quickly (use `bash -c 'exit 0'`) and polling
        // `state.sessions.contains_key(&uid)` until it returns
        // false, with a 3s bound.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-exit", &dir);

        // The argv shape is interactive `/bin/bash`. Send `exit\n`
        // via send_input to make the bash session terminate.
        let params = bash_params("ws-exit", "exit-test", dir.path());
        let result = start_session(&state, &params).expect("spawn");
        let uid = result["session_uid"].as_str().unwrap().to_string();
        {
            let mut s = state.lock().unwrap();
            let session = s.sessions.get_mut(&uid).expect("registered");
            session.send_input(b"exit\n").expect("send_input exit");
        }

        // Poll until the session is gone from the registry. The
        // reaper-cleanup callback re-locks state through the same
        // mutex; ms-scale latency is typical.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            {
                let s = state.lock().unwrap();
                if !s.sessions.contains_key(&uid) {
                    return;
                }
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "session {} still in registry 3s after child exited — \
                     reaper-cleanup callback didn't fire",
                    uid
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn fast_exit_child_never_leaks_dead_registry_entry() {
        // Named regression from the slice-10c-c review #2 fix
        // (two-phase spawn). Before the fix, a child that exited
        // between `DaemonSession::spawn`'s return and the
        // `start_session` insert would have its on_exit callback
        // fire against an empty registry — the later insert then
        // stranded a dead entry forever.
        //
        // After the fix, `start_session` uses `PendingSession::spawn`
        // + lock-held `arm_reaper`-and-insert, so the on_exit
        // callback either (a) fires after our unlock (clean), or
        // (b) blocks on our lock until insert completes (clean).
        //
        // We exercise this with a custom workspace whose shell IS
        // a fast-exit binary. The standard claude-code/codex/bash
        // path doesn't admit `/bin/false`, so we test the race at
        // the layer below — via `PendingSession::spawn` +
        // `arm_reaper` directly with the same lock-held pattern
        // `start_session` uses. 50 iterations to amplify any
        // scheduling-dependent flakiness.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-fast", &dir);

        for i in 0..50 {
            let uid = format!("ts-fast-{}", i);

            // PendingSession::spawn against /bin/false — exits
            // immediately with code 1.
            let mut spawn_params =
                crate::session::SpawnParams::new(&uid, "fast-exit", "/bin/false");
            spawn_params.working_dir = Some(dir.path().to_path_buf());
            let pending = crate::session::PendingSession::spawn(spawn_params)
                .expect("phase 1 spawn ok");

            let state_for_cleanup = Arc::clone(&state);
            let uid_for_cleanup = uid.clone();
            let on_exit: Box<dyn FnOnce(&DaemonExitStatus) + Send + 'static> =
                Box::new(move |_status: &DaemonExitStatus| {
                    let mut s = state_for_cleanup
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    s.sessions.remove(&uid_for_cleanup);
                });

            // Lock-held arm + insert (the race-closing pattern).
            {
                let mut s = state.lock().unwrap();
                let session = pending
                    .arm_reaper(Some(on_exit))
                    .expect("phase 2 arm ok");
                s.sessions.insert(uid.clone(), session);
            }

            // Within a bounded window, the registry must either
            // (a) not contain uid (cleanup already ran), or
            // (b) contain a session whose try_wait reports it as
            //     still running. Anything else is a leaked dead
            //     entry — the very bug this slice fixes.
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                let cleaned = {
                    let s = state.lock().unwrap();
                    !s.sessions.contains_key(&uid)
                };
                if cleaned {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    // Liveness check before failing: maybe the
                    // child is genuinely still alive? (Shouldn't
                    // be — /bin/false exits in µs — but be honest
                    // if it is.)
                    let alive = {
                        let mut s = state.lock().unwrap();
                        s.sessions
                            .get_mut(&uid)
                            .and_then(|sess| {
                                if sess.try_wait().is_some() {
                                    None
                                } else {
                                    Some(())
                                }
                            })
                            .is_some()
                    };
                    if alive {
                        panic!(
                            "iter {}: child still alive after 3s (unexpected for /bin/false)",
                            i
                        );
                    } else {
                        panic!(
                            "iter {}: child exited but session {} STILL in registry 3s later — fast-exit race regression",
                            i, uid
                        );
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        // No leftover entries from any iteration.
        assert_eq!(
            state.lock().unwrap().sessions.len(),
            0,
            "registry must be empty after all 50 fast-exit iterations",
        );
    }

    #[test]
    fn registry_remove_races_safely_against_insert() {
        // Fix-2 race contract: even if the child exits very fast
        // (before `start_session` returns), the reaper-cleanup
        // callback's `state.lock()` serializes with the insert
        // via the Arc<Mutex<…>>. By the time the callback's
        // remove runs, the insert has completed → the session
        // briefly appears in the registry, then is removed.
        //
        // We approximate "very fast exit" by using `bash` and
        // exiting immediately. The race window is theoretical
        // (thread-spawn overhead vs. mutex lock latency) but we
        // assert the invariant either way: the registry never
        // ends up with a stranded entry.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-race", &dir);

        for _ in 0..5 {
            let params = bash_params("ws-race", "race-test", dir.path());
            let result = start_session(&state, &params).expect("spawn");
            let uid = result["session_uid"].as_str().unwrap().to_string();
            // Send `exit` immediately to drive a fast exit.
            {
                let mut s = state.lock().unwrap();
                if let Some(session) = s.sessions.get_mut(&uid) {
                    let _ = session.send_input(b"exit\n");
                }
            }
            // Wait for the registry to reflect the exit.
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                if !state.lock().unwrap().sessions.contains_key(&uid) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(
                !state.lock().unwrap().sessions.contains_key(&uid),
                "iteration: session {} not cleaned up within 3s",
                uid
            );
        }
        // No leftover entries.
        assert_eq!(
            state.lock().unwrap().sessions.len(),
            0,
            "registry must be empty after all sessions exit + cleanup",
        );
    }

    // --- 10e-a: on_exit populates manifest entry's last_exit +
    //     broadcasts ManifestDiff::Exited -----------------------------

    /// T1 — when a daemon-spawned session exits, the reaper's
    /// `on_exit` callback (via `handle_session_exit`) populates the
    /// matching `ManifestEntry.last_exit` in the daemon's manifest
    /// snapshot. Exercises the full production wire: real
    /// `PendingSession::spawn(/bin/false)` + lock-held arm_reaper +
    /// insert, then waits for the reaper-cleanup callback to fire.
    ///
    /// `/bin/false` exits with code 1 and no signal; with no
    /// kills_dir configured, `LastExit.memory_cap_kill` is false
    /// and `kills_file_offset` is None. The kernel `code` flows
    /// through from `wait_for_child`.
    #[test]
    fn on_exit_populates_manifest_last_exit_for_fast_exit_child() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-10e-a-t1", &dir);
        let uid = fresh_test_uid();

        // Seed the manifest entry the on_exit will mutate. Mirrors
        // what `state.load_manifest_from_disk` does at daemon
        // startup when the TUI's tui-sessions.json carries this
        // session.
        {
            let mut s = state.lock().unwrap();
            s.workspaces.get_mut("ws-10e-a-t1").unwrap().sessions.push(
                crate::manifest::ManifestEntry {
                    uid: uid.clone(),
                    managed_by_uid: None,
                    generation: 0,
                    label: "fast-exit".to_string(),
                    session_type: "claude-code".to_string(),
                    transcript_id: None,
                    hidden: false,
                    idle_timeout_secs: 0,
                    burst_threshold: 0,
                    workflow_run_id: None,
                    workflow_role: None,
                    continuous_task_id: None,
                    task_id: None,
                    notify_on_idle: false,
                    seeded_from_snapshot: None,
                    last_exit: None,
                    host_id: crate::host_id::HostId::local(),
                    global_perms: false,
                },
            );
        }

        // PendingSession::spawn(/bin/false) — same pattern as
        // `fast_exit_child_never_leaks_dead_registry_entry`.
        let mut spawn_params =
            crate::session::SpawnParams::new(&uid, "fast-exit", "/bin/false");
        spawn_params.working_dir = Some(dir.path().to_path_buf());
        spawn_params.workspace_id = "ws-10e-a-t1".to_string();
        let pending = crate::session::PendingSession::spawn(spawn_params)
            .expect("phase 1 spawn ok");

        // on_exit closure forwards to the extracted helper — same
        // shape as production `start_session`'s on_exit.
        let state_for_cleanup = Arc::clone(&state);
        let uid_for_cleanup = uid.clone();
        let on_exit: Box<dyn FnOnce(&DaemonExitStatus) + Send + 'static> =
            Box::new(move |_status: &DaemonExitStatus| {
                let mut s = state_for_cleanup
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                handle_session_exit(&mut s, &uid_for_cleanup);
            });

        // Lock-held arm + insert (race-closing pattern).
        {
            let mut s = state.lock().unwrap();
            let session = pending
                .arm_reaper(Some(on_exit))
                .expect("phase 2 arm ok");
            s.sessions.insert(uid.clone(), session);
        }

        // Wait for the reaper-cleanup callback to fire — same
        // bounded-deadline polling as the fast-exit test.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let cleaned = !state.lock().unwrap().sessions.contains_key(&uid);
            if cleaned {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("session not cleaned up after 3s — on_exit didn't fire");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Assertion: manifest entry's last_exit populated.
        let s = state.lock().unwrap();
        let entry = s.workspaces["ws-10e-a-t1"]
            .sessions
            .iter()
            .find(|e| e.uid == uid)
            .expect("manifest entry still present");
        let last_exit = entry
            .last_exit
            .as_ref()
            .expect("last_exit must be Some after on_exit");
        assert_eq!(
            last_exit.code,
            Some(1),
            "/bin/false exits with code 1; on_exit must capture it. \
             Got code={:?}",
            last_exit.code,
        );
        assert!(
            !last_exit.memory_cap_kill,
            "no kills_dir configured → memory_cap_kill must be false",
        );
        assert!(
            last_exit.kills_file_offset.is_none(),
            "no kills_dir → kills_file_offset must be None",
        );
        assert!(
            last_exit.exited_at > 0.0,
            "exited_at must be a real wall-clock timestamp",
        );
    }

    /// T2 — companion to T1: subscribers to
    /// `state.manifest_watcher` receive a `ManifestDiff::Exited`
    /// frame with a payload matching the manifest entry's
    /// `last_exit`. Same fast-exit pattern; subscriber attached
    /// BEFORE the spawn.
    #[test]
    fn on_exit_broadcasts_session_exit_diff_to_manifest_watch_subscribers() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-10e-a-t2", &dir);
        let uid = fresh_test_uid();

        // Subscribe BEFORE the spawn so we don't miss the diff.
        // No initial-snapshot replay in this primitive (per
        // `ManifestWatcher` doc) — subscribers see live broadcasts
        // only.
        let (rx, _guard) = {
            let s = state.lock().unwrap();
            s.manifest_watcher.subscribe()
        };

        // Same fast-exit spawn pattern as T1; no manifest entry
        // seeded — T2 only cares about the diff, not the on-disk
        // mutation (which T1 pins). The plan §5 R5 case: even
        // without a matching ManifestEntry the diff still fires.
        let mut spawn_params =
            crate::session::SpawnParams::new(&uid, "fast-exit", "/bin/false");
        spawn_params.working_dir = Some(dir.path().to_path_buf());
        spawn_params.workspace_id = "ws-10e-a-t2".to_string();
        let pending = crate::session::PendingSession::spawn(spawn_params)
            .expect("phase 1 spawn ok");

        let state_for_cleanup = Arc::clone(&state);
        let uid_for_cleanup = uid.clone();
        let on_exit: Box<dyn FnOnce(&DaemonExitStatus) + Send + 'static> =
            Box::new(move |_status: &DaemonExitStatus| {
                let mut s = state_for_cleanup
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                handle_session_exit(&mut s, &uid_for_cleanup);
            });

        {
            let mut s = state.lock().unwrap();
            let session = pending
                .arm_reaper(Some(on_exit))
                .expect("phase 2 arm ok");
            s.sessions.insert(uid.clone(), session);
        }

        // Wait for the diff. recv_timeout is the synchronization
        // point; no need for the polling-cleanup loop because the
        // broadcast happens before the `state.sessions.remove`
        // inside `handle_session_exit`.
        let diff = rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("subscriber must receive Exited diff within 3s");
        match diff {
            crate::manifest::ManifestDiff::Exited {
                uid: diff_uid,
                last_exit,
            } => {
                assert_eq!(
                    diff_uid, uid,
                    "diff uid must match the spawned session's uid",
                );
                assert_eq!(
                    last_exit.code,
                    Some(1),
                    "broadcast last_exit.code must match the kernel exit",
                );
                assert!(
                    !last_exit.memory_cap_kill,
                    "no kills_dir → memory_cap_kill false in broadcast too",
                );
            }
            other => panic!(
                "expected ManifestDiff::Exited, got {:?}",
                other
            ),
        }

        // Clean shutdown: wait for the reaper-cleanup callback to
        // finish so the test's TempDir Drop doesn't race with the
        // child's exit reaping.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(3);
        while state.lock().unwrap().sessions.contains_key(&uid) {
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// T1-cap — the named acceptance criterion's PRIMARY flag:
    /// when the cgroup-OOM watcher kills a process under memory
    /// cap pressure, the reaper's `on_exit` callback must populate
    /// `ManifestEntry.last_exit.memory_cap_kill = true`. End-to-end
    /// through the REAL production path: `handle_breach` writes
    /// the kill_log record + signals → reaper's `waitpid` returns
    /// → `on_exit` runs `handle_session_exit` → manifest mutation
    /// + diff broadcast.
    ///
    /// 10e-a r1 F1 fix: pre-r1 this test simulated the watcher by
    /// calling `write_kill_log_to` BEFORE `libc::kill`, which
    /// accidentally matched production's ORIGINAL order
    /// (kill-then-write) inverted — the test passed for the wrong
    /// reason. Post-r1 the watcher's production order IS
    /// write-then-kill (see `session_watch::handle_breach`), so
    /// this test now drives `handle_breach` directly. Any
    /// regression in the watcher's write-before-kill invariant
    /// surfaces here as a race-flaky `memory_cap_kill: false`.
    ///
    /// Test mechanics:
    /// 1. Configure `kills_dir` + cgroup tempdir; capture baseline.
    /// 2. Spawn `/bin/sleep 30` with `kills_dir` plumbed through.
    /// 3. Seed `cgroup.procs` with the spawned child's PID — this
    ///    is what makes the watcher pick it as the breach victim.
    /// 4. Call `handle_breach` (the production watcher's per-tick
    ///    handler). It writes `KilledByUs` to kill_log BEFORE
    ///    sending SIGTERM/grace/SIGKILL.
    /// 5. Wait for the reaper-cleanup callback; assert
    ///    `memory_cap_kill == true`, `kills_file_offset.is_some()`,
    ///    AND `ManifestDiff::Exited` broadcast received with the
    ///    same flag.
    #[test]
    fn on_exit_flags_memory_cap_kill_via_handle_breach_production_path() {
        let dir = TempDir::new().unwrap();
        let kills_dir = dir.path().join("memory_kills");
        let cgroup_dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-10e-a-cap", &dir);
        let uid = fresh_test_uid();

        // Pre-spawn baseline. The watcher records past this
        // offset are "from this spawn"; everything before is
        // stale from a prior incarnation under the same uid.
        let baseline = crate::reaper::capture_baseline_for_spawn(&kills_dir, &uid)
            .expect("baseline capture");
        assert_eq!(baseline, 0, "fresh uid → baseline 0 (empty file)");

        // Seed the manifest entry the on_exit will mutate.
        {
            let mut s = state.lock().unwrap();
            s.workspaces.get_mut("ws-10e-a-cap").unwrap().sessions.push(
                crate::manifest::ManifestEntry {
                    uid: uid.clone(),
                    managed_by_uid: None,
                    generation: 0,
                    label: "cap-kill-victim".to_string(),
                    session_type: "claude-code".to_string(),
                    transcript_id: None,
                    hidden: false,
                    idle_timeout_secs: 0,
                    burst_threshold: 0,
                    workflow_run_id: None,
                    workflow_role: None,
                    continuous_task_id: None,
                    task_id: None,
                    notify_on_idle: false,
                    seeded_from_snapshot: None,
                    last_exit: None,
                    host_id: crate::host_id::HostId::local(),
                    global_perms: false,
                },
            );
        }

        // Subscribe BEFORE the spawn so we catch the broadcast.
        let (rx, _guard) = {
            let s = state.lock().unwrap();
            s.manifest_watcher.subscribe()
        };

        // /bin/sleep 30 — gives us a stable child the watcher
        // can target. The cap watcher in production picks the
        // highest-RSS unprotected PID from the cgroup; here we
        // seed exactly the spawned child's PID so it's the
        // unambiguous target.
        let mut spawn_params = crate::session::SpawnParams::new(
            &uid,
            "cap-victim",
            "/bin/sleep",
        );
        spawn_params.args = vec!["30".to_string()];
        spawn_params.working_dir = Some(dir.path().to_path_buf());
        spawn_params.workspace_id = "ws-10e-a-cap".to_string();
        spawn_params.kills_dir = Some(kills_dir.clone());
        let pending = crate::session::PendingSession::spawn(spawn_params)
            .expect("phase 1 spawn ok");

        let state_for_cleanup = Arc::clone(&state);
        let uid_for_cleanup = uid.clone();
        let on_exit: Box<dyn FnOnce(&DaemonExitStatus) + Send + 'static> =
            Box::new(move |_status: &DaemonExitStatus| {
                let mut s = state_for_cleanup
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                handle_session_exit(&mut s, &uid_for_cleanup);
            });

        // Lock-held arm + insert. Capture the pid so we can
        // seed cgroup.procs.
        let pid = {
            let mut s = state.lock().unwrap();
            let session = pending
                .arm_reaper(Some(on_exit))
                .expect("phase 2 arm ok");
            let pid = session.pid;
            s.sessions.insert(uid.clone(), session);
            pid
        };

        // Seed the fake cgroup with the child's PID. The
        // watcher's `handle_breach` reads this file to identify
        // its target. Empty `protected` set → the child is fair
        // game.
        std::fs::write(
            cgroup_dir.path().join("cgroup.procs"),
            format!("{}\n", pid).as_bytes(),
        )
        .expect("seed cgroup.procs");

        // Drive the production watcher path — `handle_breach`
        // writes the `KilledByUs` record (10e-a r1 F1 ordering
        // fix: BEFORE signals) and dispatches SIGTERM+grace+SIGKILL
        // via the pidfd helpers. NO manual write_kill_log_to or
        // libc::kill — this is the byte-identical production
        // sequence.
        crate::session_watch::handle_breach(
            &uid,
            cgroup_dir.path(),
            &std::collections::HashSet::new(),
            64 * 1024 * 1024,
            128 * 1024 * 1024,
            &kills_dir,
        );

        // Wait for cleanup.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let cleaned = !state.lock().unwrap().sessions.contains_key(&uid);
            if cleaned {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("session not cleaned up after 5s — on_exit didn't fire");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // Manifest entry assertions.
        {
            let s = state.lock().unwrap();
            let entry = s.workspaces["ws-10e-a-cap"]
                .sessions
                .iter()
                .find(|e| e.uid == uid)
                .expect("manifest entry still present");
            let last_exit = entry
                .last_exit
                .as_ref()
                .expect("last_exit must be Some after on_exit");
            assert!(
                last_exit.memory_cap_kill,
                "killed_by_us record past baseline + SIGKILL exit MUST \
                 flip memory_cap_kill to true; got false (last_exit={:?})",
                last_exit,
            );
            assert!(
                last_exit.kills_file_offset.is_some(),
                "kills_file_offset MUST point at the injected record \
                 (so future scrubbers can locate full kill details). \
                 Got None.",
            );
            // Code is None for signal-kill exits (WIFSIGNALED).
            assert_eq!(
                last_exit.code, None,
                "SIGKILL is a signal-kill → exit code is None",
            );
        }

        // Broadcast assertion — same payload, no drift between
        // the manifest mutation and the diff frame.
        let diff = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("subscriber must receive Exited diff");
        match diff {
            crate::manifest::ManifestDiff::Exited {
                uid: diff_uid,
                last_exit,
            } => {
                assert_eq!(diff_uid, uid);
                assert!(
                    last_exit.memory_cap_kill,
                    "broadcast last_exit.memory_cap_kill MUST also be \
                     true (no drift between manifest mutation and \
                     diff payload)",
                );
                assert!(last_exit.kills_file_offset.is_some());
            }
            other => panic!("expected Exited, got {:?}", other),
        }
    }

    /// 10e-a r1 F2 — operator-kill via `kill_session` RPC now
    /// flows through the reaper's `handle_session_exit` callback,
    /// which means the manifest snapshot's `last_exit` gets
    /// populated AND `ManifestDiff::Exited` fires for
    /// `manifest.watch` subscribers — same path as cap-kill.
    /// Pre-r1 the handler removed-then-Drop'd, so on_exit ran
    /// against an absent uid and emitted no diff; operator-killed
    /// sessions silently bypassed the manifest.watch broadcast.
    ///
    /// Test mechanics:
    /// 1. Insert a daemon-spawned `/bin/sleep` via `insert_session`
    ///    (the helper now installs the on_exit reaper-cleanup
    ///    callback that production `start_session` installs).
    /// 2. Seed the manifest entry so the on_exit mutation has a
    ///    landing zone.
    /// 3. Subscribe to `manifest_watcher` BEFORE the kill.
    /// 4. Call `kill_session` (the RPC handler).
    /// 5. Wait for the reaper-cleanup callback to fire; assert:
    ///    - manifest entry's `last_exit` populated with
    ///      `signal == Some(SIGKILL)`, `code == None`,
    ///      `memory_cap_kill == false` (operator override flips
    ///      the cap classification off per round-7 `is_cap_kill`).
    ///    - subscriber received the `ManifestDiff::Exited` frame.
    #[test]
    fn kill_session_rpc_populates_manifest_last_exit_and_broadcasts_diff() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-10e-a-opkill", &dir);
        let uid = fresh_test_uid();
        insert_session(&state, &uid, "ws-10e-a-opkill");

        // Seed the manifest entry — same shape as T1-clean.
        {
            let mut s = state.lock().unwrap();
            s.workspaces.get_mut("ws-10e-a-opkill").unwrap().sessions.push(
                crate::manifest::ManifestEntry {
                    uid: uid.clone(),
                    managed_by_uid: None,
                    generation: 0,
                    label: "operator-kill-victim".to_string(),
                    session_type: "claude-code".to_string(),
                    transcript_id: None,
                    hidden: false,
                    idle_timeout_secs: 0,
                    burst_threshold: 0,
                    workflow_run_id: None,
                    workflow_role: None,
                    continuous_task_id: None,
                    task_id: None,
                    notify_on_idle: false,
                    seeded_from_snapshot: None,
                    last_exit: None,
                    host_id: crate::host_id::HostId::local(),
                    global_perms: false,
                },
            );
        }

        // Subscribe BEFORE the kill.
        let (rx, _guard) = {
            let s = state.lock().unwrap();
            s.manifest_watcher.subscribe()
        };

        // Operator kill via the RPC handler. Operator caller
        // (None) bypasses session-caller auth.
        let params = json!({ "session_uid": &uid });
        let result = kill_session(&state, &params, None).expect("kill ok");
        assert_eq!(result["ok"], true);

        // Wait for the reaper-cleanup callback.
        poll_until_session_removed(&state, &uid);

        // Manifest entry's last_exit populated.
        {
            let s = state.lock().unwrap();
            let entry = s.workspaces["ws-10e-a-opkill"]
                .sessions
                .iter()
                .find(|e| e.uid == uid)
                .expect("manifest entry still present");
            let last_exit = entry
                .last_exit
                .as_ref()
                .expect("last_exit populated by reaper's on_exit");
            assert!(
                !last_exit.memory_cap_kill,
                "operator-kill MUST NOT flip memory_cap_kill (per \
                 round-7 is_cap_kill: operator_kill_requested=true \
                 disqualifies cap attribution). Got memory_cap_kill=true",
            );
            assert_eq!(
                last_exit.code, None,
                "SIGKILL → signal-kill exit → code is None",
            );
        }

        // Broadcast assertion.
        let diff = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("subscriber MUST receive Exited diff on operator kill");
        match diff {
            crate::manifest::ManifestDiff::Exited {
                uid: diff_uid,
                last_exit,
            } => {
                assert_eq!(diff_uid, uid);
                assert!(
                    !last_exit.memory_cap_kill,
                    "broadcast last_exit.memory_cap_kill must agree \
                     with manifest entry (no drift)",
                );
            }
            other => panic!("expected Exited, got {:?}", other),
        }
    }

    /// T3 — `handle_session_exit` no-ops cleanly when called for
    /// a uid that isn't in `state.sessions`. Pins the R5 path
    /// (untracked-uid call) without needing a real spawn. Subscriber
    /// receives nothing; manifest snapshot stays untouched.
    #[test]
    fn handle_session_exit_noops_when_uid_not_in_sessions() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-10e-a-t3", &dir);
        let uid = fresh_test_uid();

        let (rx, _guard) = {
            let s = state.lock().unwrap();
            s.manifest_watcher.subscribe()
        };

        // Call the helper directly with a uid that has no
        // matching `DaemonSession` in `state.sessions`. Production
        // ordering ensures this can't happen (the on_exit closure
        // runs against a uid the registry contained at insert
        // time), but defense-in-depth.
        {
            let mut s = state.lock().unwrap();
            handle_session_exit(&mut s, &uid);
        }

        // No diff broadcast.
        match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => panic!("unexpected receiver error: {:?}", e),
            Ok(diff) => panic!(
                "expected NO diff for unknown uid, got {:?}",
                diff
            ),
        }
    }

    /// Read-after-exit: `handle_session_exit` records a tombstone before the
    /// registry remove, so `resolve_authorized_session` still serves the exited
    /// session's transcript path + `state="exited"`, and `list_sessions` shows
    /// it only under `include_exited=true`. The bug this guards: the daemon
    /// evicted exited sessions with no tombstone, so the MCP read-after-exit
    /// contract returned not_found.
    #[test]
    fn read_after_exit_serves_transcript_via_tombstone() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-rae", &dir);
        let uid = fresh_test_uid();
        {
            let mut sp =
                crate::session::SpawnParams::new(&uid, "worker", "/bin/sleep");
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "ws-rae".to_string();
            sp.session_type = "claude-code".to_string();
            let mut ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
            ds.transcript_path = Some("/tmp/rae-sid.jsonl".to_string());
            let mut s = state.lock().unwrap();
            s.sessions.insert(uid.clone(), ds);
        }
        // Exit it: tombstone recorded, removed from the live registry (the
        // returned DaemonSession drops here, SIGKILLing the /bin/sleep child).
        {
            let mut s = state.lock().unwrap();
            handle_session_exit(&mut s, &uid);
            assert!(!s.sessions.contains_key(&uid), "removed from live registry");
            assert!(s.exited_tombstone(&uid).is_some(), "tombstone recorded");
        }
        // Operator read-after-exit: exited + the final transcript path.
        let resolved = resolve_authorized_session(
            &state,
            &json!({ "session_uid": uid }),
            None,
        )
        .expect("resolve ok");
        assert_eq!(resolved["state"], "exited");
        assert_eq!(resolved["transcript_path"], "/tmp/rae-sid.jsonl");
        assert_eq!(resolved["idle"], true);
        // list_sessions surfaces the exited row only with include_exited=true.
        let with = list_sessions(&state, &json!({ "include_exited": true }), None)
            .expect("list ok");
        assert!(
            with.as_array().unwrap().iter().any(|r| {
                r["session_uid"] == uid.as_str() && r["state"] == "exited"
            }),
            "include_exited=true must surface the tombstone: {with:?}",
        );
        let without = list_sessions(&state, &json!({ "include_exited": false }), None)
            .expect("list ok");
        assert!(
            !without
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["session_uid"] == uid.as_str()),
            "include_exited=false (default) must omit the tombstone",
        );
    }

    // --- initial PTY size plumbing (slice-10c-e-2 review-3 fix) ----------

    #[test]
    fn start_session_default_cols_rows_used_when_not_provided() {
        // Backwards-compat: a request without cols/rows fields
        // gets the 80/24 default. The bash session's PTY ends up
        // at that size via SpawnParams. We verify by reading
        // `stty size` output through the fanout.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-defsize", &dir);
        // No cols/rows in params — serde defaults kick in.
        let params = json!({
            "uid": fresh_test_uid(),
            "workspace_id": "ws-defsize",
            "label": "x",
            "argv": ["/bin/bash"],
            "working_dir": dir.path().display().to_string(),
        });
        let result = start_session(&state, &params).expect("spawn");
        let uid = result["session_uid"].as_str().unwrap().to_string();

        let rx = {
            let mut s = state.lock().unwrap();
            let session = s.sessions.get_mut(&uid).unwrap();
            let rx = session.fanout.subscribe();
            session.send_input(b"stty size\n").expect("send_input");
            rx
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut accumulated = Vec::new();
        while std::time::Instant::now() < deadline {
            let remaining =
                deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(chunk) => {
                    accumulated.extend_from_slice(&chunk);
                    let text = String::from_utf8_lossy(&accumulated);
                    if text.contains("24 80") {
                        kill_all_sessions(&state);
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        kill_all_sessions(&state);
        panic!(
            "default 80x24 PTY size not observable via stty (looking for '24 80'): {}",
            String::from_utf8_lossy(&accumulated)
        );
    }

    #[test]
    fn start_session_with_explicit_cols_rows_sizes_pty_accordingly() {
        // Named acceptance for the slice-10c-e-2 review-3 fix:
        // the TUI's live operator terminal size must reach the
        // daemon-spawned PTY. Verify by spawning with non-default
        // 120x40, sending `stty size`, and observing the output
        // is "40 120".
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-bigsize", &dir);
        let params = json!({
            "uid": fresh_test_uid(),
            "workspace_id": "ws-bigsize",
            "label": "x",
            "argv": ["/bin/bash"],
            "working_dir": dir.path().display().to_string(),
            "cols": 120u16,
            "rows": 40u16,
        });
        let result = start_session(&state, &params).expect("spawn");
        let uid = result["session_uid"].as_str().unwrap().to_string();

        let rx = {
            let mut s = state.lock().unwrap();
            let session = s.sessions.get_mut(&uid).unwrap();
            let rx = session.fanout.subscribe();
            session.send_input(b"stty size\n").expect("send_input");
            rx
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut accumulated = Vec::new();
        while std::time::Instant::now() < deadline {
            let remaining =
                deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(chunk) => {
                    accumulated.extend_from_slice(&chunk);
                    let text = String::from_utf8_lossy(&accumulated);
                    if text.contains("40 120") {
                        kill_all_sessions(&state);
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        kill_all_sessions(&state);
        panic!(
            "expected '40 120' from stty size, got:\n{}",
            String::from_utf8_lossy(&accumulated)
        );
    }

    // ===========================================================
    // send_input (slice 10c-d)
    // ===========================================================

    /// Helper: spawn a bash session via the registered start_session
    /// path so the registry, reaper-cleanup callback, and Drop
    /// machinery are all live. Returns the session uid. The working
    /// directory is read from the pre-registered workspace's
    /// `worktree_path` — callers stage that via
    /// `state_with_workspace`.
    fn spawn_bash(state: &Arc<Mutex<DaemonState>>, ws_id: &str) -> String {
        let working_dir = state
            .lock()
            .unwrap()
            .workspaces
            .get(ws_id)
            .and_then(|w| w.worktree_path.clone())
            .expect("workspace must have a worktree_path for the test")
            .display()
            .to_string();
        let params = json!({
            "uid": fresh_test_uid(),
            "workspace_id": ws_id,
            "label": "test-bash",
            "argv": ["/bin/bash"],
            "working_dir": working_dir,
        });
        let result = start_session(state, &params).expect("spawn bash");
        result["session_uid"].as_str().unwrap().to_string()
    }

    #[test]
    fn send_input_writes_text_to_pty_and_returns_ok() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-si", &dir);
        let uid = spawn_bash(&state, "ws-si");

        // Subscribe so we can observe what bash echoes back.
        let rx = {
            let mut s = state.lock().unwrap();
            s.sessions.get_mut(&uid).unwrap().fanout.subscribe()
        };

        let params = json!({
            "session_uid": &uid,
            "text": "echo hello-send-input-test",
        });
        let result = send_input(&state, &params, None).expect("send_input ok");
        assert_eq!(result["ok"], true);

        // Observable consequence: bash echoes the line back through
        // the PTY → fanout. Look for the substring within a bounded
        // window.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut accumulated = Vec::new();
        while std::time::Instant::now() < deadline {
            let remaining =
                deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(chunk) => {
                    accumulated.extend_from_slice(&chunk);
                    if String::from_utf8_lossy(&accumulated)
                        .contains("hello-send-input-test")
                    {
                        kill_all_sessions(&state);
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        kill_all_sessions(&state);
        panic!(
            "expected 'hello-send-input-test' in PTY output, got:\n{}",
            String::from_utf8_lossy(&accumulated)
        );
    }

    #[test]
    fn send_input_rejects_oversize_text() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-si-big", &dir);
        let uid = spawn_bash(&state, "ws-si-big");

        // 64 KiB + 1 — one byte past the cap.
        let text = "a".repeat(MAX_SEND_INPUT_BYTES + 1);
        let params = json!({ "session_uid": &uid, "text": text });
        let err = send_input(&state, &params, None).expect_err("oversize must reject");
        assert_eq!(err.0, ErrorCode::InvalidParams);
        assert!(err.1.contains("exceeds cap"), "error names the cap: {}", err.1);
        kill_all_sessions(&state);
    }

    #[test]
    fn send_input_at_cap_boundary_is_accepted() {
        // 64 KiB exact must be allowed (cap is "no more than" not
        // "less than"). Regression guard against off-by-one.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-si-cap", &dir);
        let uid = spawn_bash(&state, "ws-si-cap");

        // Use a payload that won't make bash do anything dramatic
        // when it parses — a comment with padding. Cap-sized
        // exactly.
        let mut text = String::with_capacity(MAX_SEND_INPUT_BYTES);
        text.push('#');
        while text.len() < MAX_SEND_INPUT_BYTES {
            text.push('x');
        }
        assert_eq!(text.len(), MAX_SEND_INPUT_BYTES);

        let params = json!({ "session_uid": &uid, "text": text });
        let result = send_input(&state, &params, None).expect("at-cap must succeed");
        assert_eq!(result["ok"], true);
        kill_all_sessions(&state);
    }

    #[test]
    fn send_input_rejects_submit_false_with_parity_message() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-si-sub", &dir);
        let uid = spawn_bash(&state, "ws-si-sub");

        let params = json!({
            "session_uid": &uid,
            "text": "no-submit",
            "submit": false,
        });
        let err = send_input(&state, &params, None).expect_err("submit=false rejected");
        assert_eq!(err.0, ErrorCode::InvalidParams);
        assert!(err.1.contains("submit=false"), "message must name the field");
        kill_all_sessions(&state);
    }

    #[test]
    fn send_input_unknown_uid_returns_not_found() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let params = json!({
            "session_uid": "ts-ghost",
            "text": "anything",
        });
        let err = send_input(&state, &params, None).expect_err("unknown uid");
        assert_eq!(err.0, ErrorCode::NotFound);
        assert!(err.1.contains("ts-ghost"));
    }

    // ===========================================================
    // kill_session (slice 10c-d)
    // ===========================================================

    #[test]
    fn kill_session_signals_target_and_reaper_callback_removes_from_registry() {
        // 10e-a r1 F2: kill_session returns Ok immediately after
        // signaling via pidfd. The reaper's on_exit callback
        // (running `handle_session_exit`) does the actual removal
        // — that path also populates `last_exit` and broadcasts
        // `ManifestDiff::Exited` for the manifest.watch consumer.
        // Pre-r1 the handler removed-then-Drop'd, which bypassed
        // the on_exit consumer.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-kill", &dir);
        let uid = spawn_bash(&state, "ws-kill");
        assert!(state.lock().unwrap().sessions.contains_key(&uid));

        let params = json!({ "session_uid": &uid });
        let result = kill_session(&state, &params, None).expect("kill ok");
        assert_eq!(result["ok"], true);

        // Removal is now asynchronous via the reaper-cleanup
        // callback. Poll within a bounded window.
        poll_until_session_removed(&state, &uid);
    }

    #[test]
    fn kill_session_unknown_uid_returns_not_found() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let params = json!({ "session_uid": "ts-never-existed" });
        let err = kill_session(&state, &params, None).expect_err("unknown uid");
        assert_eq!(err.0, ErrorCode::NotFound);
    }

    // ===========================================================
    // read_session_output (slice 10c-d)
    // ===========================================================

    #[test]
    fn read_session_output_first_call_returns_full_ring() {
        // Spawn bash, send a marker, snapshot the fanout from the
        // start. First call with `since_cursor = None` returns the
        // full ring.
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-rso", &dir);
        let uid = spawn_bash(&state, "ws-rso");

        // Drive some output and wait briefly for it to land in the
        // fanout.
        let _ = send_input(
            &state,
            &json!({ "session_uid": &uid, "text": "echo marker-first-snap" }),
            None,
        )
        .expect("send_input");

        // Poll until snapshot.bytes contains the marker — bounded
        // window. The fanout receives bytes asynchronously through
        // the reader thread.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let params = json!({ "session_uid": &uid });
            let result =
                read_session_output(&state, &params, None).expect("rso ok");
            let b64 = result["bytes"].as_str().unwrap();
            let bytes = BASE64.decode(b64).unwrap();
            if String::from_utf8_lossy(&bytes).contains("marker-first-snap") {
                assert_eq!(result["start_offset"], 0);
                assert!(result["cursor"].as_u64().unwrap() > 0);
                assert_eq!(result["evicted_since_cursor"], false);
                assert_eq!(result["closed"], false);
                kill_all_sessions(&state);
                return;
            }
            if std::time::Instant::now() >= deadline {
                kill_all_sessions(&state);
                panic!(
                    "marker-first-snap not in fanout after 3s; last bytes:\n{}",
                    String::from_utf8_lossy(&bytes)
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn read_session_output_with_cursor_returns_only_new_bytes() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-rso-2", &dir);
        let uid = spawn_bash(&state, "ws-rso-2");

        // First marker.
        let _ = send_input(
            &state,
            &json!({ "session_uid": &uid, "text": "echo CURSOR-A" }),
            None,
        )
        .unwrap();
        // Wait for first marker to land.
        let cursor_after_a = wait_for_marker(&state, &uid, "CURSOR-A");

        // Second marker, then snapshot since the first cursor.
        let _ = send_input(
            &state,
            &json!({ "session_uid": &uid, "text": "echo CURSOR-B-marker" }),
            None,
        )
        .unwrap();
        // Poll: read_session_output(since=cursor_after_a) must
        // contain CURSOR-B-marker but NOT CURSOR-A.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let params = json!({
                "session_uid": &uid,
                "since_cursor": cursor_after_a,
            });
            let result =
                read_session_output(&state, &params, None).expect("rso ok");
            let b64 = result["bytes"].as_str().unwrap();
            let bytes = BASE64.decode(b64).unwrap();
            let text = String::from_utf8_lossy(&bytes).to_string();
            if text.contains("CURSOR-B-marker") {
                assert!(
                    !text.contains("CURSOR-A"),
                    "since-cursor must exclude CURSOR-A from before; got:\n{}",
                    text
                );
                assert_eq!(result["start_offset"], cursor_after_a);
                kill_all_sessions(&state);
                return;
            }
            if std::time::Instant::now() >= deadline {
                kill_all_sessions(&state);
                panic!(
                    "CURSOR-B-marker not after cursor {} within 3s; got:\n{}",
                    cursor_after_a, text
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn read_session_output_after_kill_reports_closed_true() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-rso-closed", &dir);
        let uid = spawn_bash(&state, "ws-rso-closed");
        // Kill it (Drop sends SIGKILL via pidfd; reader thread
        // sees EOF, fanout.close() fires).
        // But: kill_session REMOVES the session from the registry,
        // so a follow-up read_session_output would get NotFound,
        // not closed=true. So we test the closed signal via the
        // pre-removal window: send_input then directly close the
        // fanout to simulate child exit while session is still in
        // the registry.
        {
            let mut s = state.lock().unwrap();
            let session = s.sessions.get_mut(&uid).unwrap();
            session.fanout.close();
        }
        let params = json!({ "session_uid": &uid });
        let result =
            read_session_output(&state, &params, None).expect("rso ok");
        assert_eq!(
            result["closed"], true,
            "closed flag must propagate from FanoutSnapshot",
        );
        kill_all_sessions(&state);
    }

    #[test]
    fn read_session_output_unknown_uid_returns_not_found() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let params = json!({ "session_uid": "ts-ghost" });
        let err = read_session_output(&state, &params, None).expect_err("unknown uid");
        assert_eq!(err.0, ErrorCode::NotFound);
    }

    // ===========================================================
    // Sub-2a Finding #2: auth + state-mutation atomicity
    //
    // Pre-fix, the dispatcher locked-for-auth then dropped the
    // lock, and the method body re-locked to mutate. That window
    // let an Allow decision act on a target that had been swapped
    // out in the interim. The fix moves auth INTO the method body
    // (same critical section as the target lookup + Arc-clone).
    //
    // These tests pin the *observable* consequence: when auth
    // fails the method must NOT have performed the side effect.
    // The "race-style" framing is that auth is co-located with the
    // mutation under one lock; tests below assert the property by
    // verifying that a deny decision leaves state untouched even
    // when the target is still live in the registry (the
    // pre-existing target wasn't the issue; the window was).
    // ===========================================================

    /// Helper: insert a stubbed `/bin/sleep` session at a specific
    /// uid + workspace_id, returning fast (no start_session
    /// roundtrip + no fresh_test_uid randomization, since these
    /// tests reference uids by hand).
    fn insert_session(
        state: &Arc<Mutex<DaemonState>>,
        uid: &str,
        workspace_id: &str,
    ) {
        // 10e-a r1 F2: install the same on_exit reaper-cleanup
        // callback that production `start_session` installs.
        // Without it the reaper just consumes the kernel exit
        // and the session lingers in `state.sessions` forever
        // — which masks the post-r1 async-removal behavior
        // `kill_session` now depends on.
        let mut p =
            crate::session::SpawnParams::new(uid, format!("test-{}", uid), "/bin/sleep");
        p.args = vec!["30".to_string()];
        p.workspace_id = workspace_id.to_string();
        let pending = crate::session::PendingSession::spawn(p)
            .expect("phase 1 spawn /bin/sleep");
        let state_for_cleanup = Arc::clone(state);
        let uid_for_cleanup = uid.to_string();
        let on_exit: Box<dyn FnOnce(&DaemonExitStatus) + Send + 'static> =
            Box::new(move |_status| {
                let mut s = state_for_cleanup
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                handle_session_exit(&mut s, &uid_for_cleanup);
            });
        let session = pending
            .arm_reaper(Some(on_exit))
            .expect("phase 2 arm_reaper");
        state.lock().unwrap().sessions.insert(uid.to_string(), session);
    }

    /// 10e-a r1 F2: post-fix `kill_session` returns Ok before the
    /// reaper-cleanup callback removes the session from the
    /// registry. Tests that previously asserted immediate removal
    /// now poll within a bounded window. Default 3s matches
    /// `fast_exit_child_never_leaks_dead_registry_entry`'s
    /// established polling pattern.
    fn poll_until_session_removed(
        state: &Arc<Mutex<DaemonState>>,
        uid: &str,
    ) {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let gone = !state.lock().unwrap().sessions.contains_key(uid);
            if gone {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "session '{}' still in registry 3s after kill_session — \
                     reaper-cleanup callback didn't fire (post-r1 F2 the \
                     reaper is the removal trigger, not kill_session itself)",
                    uid,
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Auth-fail on `kill_session` must NOT remove the target. The
    /// pre-fix dispatcher's split lock would have removed if any
    /// thread swapped the registry between auth and act. Post-fix
    /// the auth+remove sit under one lock, so the only way for
    /// auth to fail and the remove to still occur would be a
    /// logic bug — pin against that.
    #[test]
    fn kill_session_auth_failure_leaves_target_in_registry() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        insert_session(&state, "ts-caller", "ws-1");
        insert_session(&state, "ts-victim", "ws-2");
        let params = json!({ "session_uid": "ts-victim" });
        let err = kill_session(&state, &params, Some("ts-caller")).expect_err("must deny");
        assert_eq!(err.0, ErrorCode::Unauthorized);
        // Target STILL present — auth failure aborted before
        // the registry mutation.
        let s = state.lock().unwrap();
        assert!(
            s.sessions.contains_key("ts-victim"),
            "auth failure must NOT have removed the target",
        );
        assert!(s.sessions.contains_key("ts-caller"));
        drop(s);
        kill_all_sessions(&state);
    }

    /// Auth-fail on `send_input` must NOT clone the writer or
    /// attempt the PTY write. Hard to observe "didn't write" on
    /// a real PTY, but the auth-failure error is the
    /// short-circuit signal — verify the error code AND that
    /// the target's writer Arc strong count is unchanged (no
    /// rogue clone leaked from the method body).
    #[test]
    fn send_input_auth_failure_does_not_clone_writer() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        insert_session(&state, "ts-caller", "ws-1");
        insert_session(&state, "ts-victim", "ws-2");
        let initial_writer_refcount = {
            let s = state.lock().unwrap();
            Arc::strong_count(&s.sessions["ts-victim"].writer)
        };
        let params = json!({
            "session_uid": "ts-victim",
            "text": "should-never-arrive",
        });
        let err = send_input(&state, &params, Some("ts-caller")).expect_err("must deny");
        assert_eq!(err.0, ErrorCode::Unauthorized);
        let after_writer_refcount = {
            let s = state.lock().unwrap();
            Arc::strong_count(&s.sessions["ts-victim"].writer)
        };
        assert_eq!(
            initial_writer_refcount, after_writer_refcount,
            "auth failure must NOT have cloned the writer Arc \
             (clone-then-deny would mean the method body's lookup \
             ran AFTER the auth decision)",
        );
        kill_all_sessions(&state);
    }

    /// Auth-fail on `read_session_output` must NOT clone the
    /// fanout. Mirror of the send_input refcount assertion above.
    #[test]
    fn read_session_output_auth_failure_does_not_clone_fanout() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        insert_session(&state, "ts-caller", "ws-1");
        insert_session(&state, "ts-victim", "ws-2");
        let initial_refcount = {
            let s = state.lock().unwrap();
            Arc::strong_count(&s.sessions["ts-victim"].fanout)
        };
        let params = json!({ "session_uid": "ts-victim" });
        let err = read_session_output(&state, &params, Some("ts-caller"))
            .expect_err("must deny");
        assert_eq!(err.0, ErrorCode::Unauthorized);
        let after_refcount = {
            let s = state.lock().unwrap();
            Arc::strong_count(&s.sessions["ts-victim"].fanout)
        };
        assert_eq!(
            initial_refcount, after_refcount,
            "auth failure must NOT have cloned the fanout Arc",
        );
        kill_all_sessions(&state);
    }

    /// Auth-pass on `kill_session` from a same-workspace
    /// taskless caller DOES remove the target — proves the
    /// auth+act pair atomically commits when the decision is
    /// Allow. Pairs with the deny tests above to bracket the
    /// "auth gates the act" invariant.
    #[test]
    fn kill_session_auth_allow_signals_target_and_reaper_removes() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        insert_session(&state, "ts-caller", "ws-shared");
        insert_session(&state, "ts-victim", "ws-shared");
        let params = json!({ "session_uid": "ts-victim" });
        let result = kill_session(&state, &params, Some("ts-caller")).expect("must allow");
        assert_eq!(result["ok"], true);
        // 10e-a r1 F2: removal is asynchronous via the reaper-
        // cleanup callback. Pre-r1 the handler removed inline.
        poll_until_session_removed(&state, "ts-victim");
        // Caller wasn't touched.
        assert!(state.lock().unwrap().sessions.contains_key("ts-caller"));
        kill_all_sessions(&state);
    }

    /// Race-style: two threads call `kill_session` on the SAME
    /// target. Post-10e-a-r1-F2 the registry is mutated by the
    /// reaper-cleanup callback (asynchronously), not by
    /// `kill_session` itself. The auth+lookup are still atomic
    /// under one lock, so both threads' auth checks observe a
    /// consistent state — but BOTH can succeed if both lock
    /// before the reaper fires (the typical case: kernel signal
    /// delivery + waitpid + on_exit-lock-attempt is microseconds,
    /// while the second thread acquires the state lock as soon as
    /// the first releases). The signal is idempotent (second
    /// call returns ESRCH silently OK via pidfd_send_signal).
    ///
    /// Accepted wire outcomes:
    ///   - (Ok, Ok) — both threads ran before reaper fired.
    ///     The session is still in the registry when both
    ///     return; reaper removes it shortly after.
    ///   - (Ok, NotFound) — the reaper-cleanup callback fired
    ///     between the two threads' lock acquisitions, so the
    ///     second thread's lookup found the session already
    ///     gone. The auth+lookup remain TOCTOU-clean: the second
    ///     thread's `state.sessions.get_mut(target)` returns
    ///     None → `NotFound` with no signal sent.
    ///
    /// Pre-r1 only the second outcome was possible (because
    /// kill_session removed inline). Post-r1 both are valid; the
    /// invariant the test pins is "no double-kill leak AND target
    /// ends up removed."
    #[test]
    fn concurrent_kill_session_yields_consistent_outcomes() {
        use std::sync::Barrier;
        let state = Arc::new(Mutex::new(DaemonState::new()));
        insert_session(&state, "ts-caller-a", "ws-shared");
        insert_session(&state, "ts-caller-b", "ws-shared");
        insert_session(&state, "ts-victim", "ws-shared");
        let barrier = Arc::new(Barrier::new(2));
        let s1 = state.clone();
        let b1 = barrier.clone();
        let t1 = std::thread::spawn(move || {
            b1.wait();
            kill_session(
                &s1,
                &json!({ "session_uid": "ts-victim" }),
                Some("ts-caller-a"),
            )
        });
        let s2 = state.clone();
        let b2 = barrier.clone();
        let t2 = std::thread::spawn(move || {
            b2.wait();
            kill_session(
                &s2,
                &json!({ "session_uid": "ts-victim" }),
                Some("ts-caller-b"),
            )
        });
        let r1 = t1.join().expect("t1 panic");
        let r2 = t2.join().expect("t2 panic");
        let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        let not_founds = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, Err((ErrorCode::NotFound, _))))
            .count();
        let unauthorizeds = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, Err((ErrorCode::Unauthorized, _))))
            .count();
        assert_eq!(
            oks + not_founds,
            2,
            "both threads must terminate as Ok-or-NotFound (no \
             Internal / Unauthorized / Conflict allowed); got \
             oks={} not_founds={} unauthorizeds={}",
            oks, not_founds, unauthorizeds,
        );
        assert!(
            oks >= 1,
            "at least one thread must succeed (the one that locked \
             first; the second may also succeed if the reaper \
             hadn't fired yet, or NotFound if it had)",
        );
        // Target eventually gone from registry (reaper cleanup).
        poll_until_session_removed(&state, "ts-victim");
        kill_all_sessions(&state);
    }

    /// Test helper: drive `read_session_output` polls until the
    /// marker substring appears in the decoded bytes, then return
    /// the cursor at that snapshot. Caps at 3s; panics on timeout.
    fn wait_for_marker(state: &Arc<Mutex<DaemonState>>, uid: &str, marker: &str) -> u64 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let params = json!({ "session_uid": uid });
            let result =
                read_session_output(state, &params, None).expect("rso ok");
            let b64 = result["bytes"].as_str().unwrap();
            let bytes = BASE64.decode(b64).unwrap();
            if String::from_utf8_lossy(&bytes).contains(marker) {
                return result["cursor"].as_u64().unwrap();
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "marker '{}' not in fanout within 3s; got:\n{}",
                    marker,
                    String::from_utf8_lossy(&bytes)
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    // ============================================================
    // 10d-2b: workflow_transition / workflow_done tests
    // ============================================================

    /// Spin up an isolated HOME for the test so events.jsonl
    /// writes land in a tempdir, not the operator's real
    /// `~/.cm/workflow-runs`. Mirrors `events.rs`'s
    /// `with_temp_home`.
    fn with_temp_home<F: FnOnce()>(f: F) -> tempfile::TempDir {
        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let orig = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()); }
        f();
        if let Some(o) = orig {
            unsafe { std::env::set_var("HOME", o); }
        }
        tmp
    }

    fn make_state_arc() -> Arc<Mutex<DaemonState>> {
        Arc::new(Mutex::new(DaemonState::new()))
    }

    /// P0 session durability (S1): the `session.set_transcript_path`
    /// RPC must persist the freshly-resolved `transcript_id` to the
    /// daemon's durable file — that id is the `--resume` key a restart
    /// needs. Drives the full RPC so it proves the lifecycle hook
    /// actually fires (not merely the underlying writer the state-mod
    /// tests already cover). The same hook sits on the headless
    /// daemon-side detector path, which is where it matters most.
    #[test]
    fn set_transcript_path_persists_resume_key_to_daemon_sessions_file() {
        use crate::session::{DaemonSession, SpawnParams};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon-sessions.json");
        let state = make_state_arc();
        {
            let mut s = state.lock().unwrap();
            s.daemon_sessions_path = Some(path.clone());
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "ws-tp".to_string();
            ws.worktree_path = Some(std::env::temp_dir());
            s.workspaces.insert("ws-tp".to_string(), ws);
            let mut sp = SpawnParams::new("ts-eeee-ffff", "tp", "/bin/sleep");
            sp.args = vec!["120".to_string()];
            sp.workspace_id = "ws-tp".to_string();
            s.sessions.insert(
                "ts-eeee-ffff".to_string(),
                DaemonSession::spawn(sp).expect("spawn"),
            );
        }
        let tpath =
            "/home/u/.claude/projects/enc/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl";
        set_transcript_path(
            &state,
            &json!({ "session_uid": "ts-eeee-ffff", "transcript_path": tpath }),
        )
        .expect("set_transcript_path ok");

        let m: crate::manifest::Manifest =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let ws = m.workspaces.get("ws-tp").expect("workspace persisted");
        let e = ws
            .sessions
            .iter()
            .find(|e| e.uid == "ts-eeee-ffff")
            .expect("session persisted");
        assert_eq!(
            e.transcript_id.as_deref(),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
            "the transcript-bind hook persisted the resume key",
        );
        // DaemonSession Drop SIGKILLs the sleep child when `state` drops.
    }

    /// P0 session durability (S1): the PRIMARY hook — a real
    /// `start_session` RPC must persist the freshly-spawned session to
    /// the durable file, so a restart can restore it. Exercises the
    /// same spawn funnel `mcp_start_session` / `create_session` use,
    /// through the real method (not a hand-built `DaemonSession`).
    #[test]
    fn start_session_persists_new_session_to_daemon_sessions_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon-sessions.json");
        let state = make_state_arc();
        {
            let mut s = state.lock().unwrap();
            s.daemon_sessions_path = Some(path.clone());
        }
        let wt = std::env::temp_dir();
        start_session(
            &state,
            &json!({
                "uid": "ts-1a2b3c4d5e6f0011-0",
                "session_type": "bash",
                "workspace_id": "ws-ss",
                "working_dir": wt.to_str().unwrap(),
                "worktree_path": wt.to_str().unwrap(),
                "label": "persist-me",
                "argv": ["/bin/sleep", "120"],
            }),
        )
        .expect("start_session ok");

        let m: crate::manifest::Manifest =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let ws = m.workspaces.get("ws-ss").expect("workspace persisted");
        assert!(
            ws.sessions.iter().any(|e| {
                e.uid == "ts-1a2b3c4d5e6f0011-0" && e.session_type == "bash"
            }),
            "the spawn hook persisted the new session",
        );

        // Drop the live session so its Drop SIGKILLs the sleep child.
        state.lock().unwrap().sessions.clear();
    }

    /// 10d-2c-1 test helper: seed a minimal-but-valid
    /// `state.json` on disk for `run_id` so the handler's
    /// load-modify-write under flock has something to load.
    /// Returns the seeded run so callers can compare pre/post
    /// fields without re-loading.
    ///
    /// 10d-2c-1 review round-1 (F3): seeds the standard
    /// `feedback`-shaped role binding map (`worker`, `reviewer`,
    /// `manager`) so transitions to any of these pass the
    /// daemon's target-role validation. Tests that need a
    /// non-feedback role shape should construct their own.
    fn seed_workflow_run(run_id: &str, initial_role: &str) -> crate::workflow::run::WorkflowRun {
        use std::collections::BTreeMap;
        let mut role_sessions = BTreeMap::new();
        for role in ["worker", "reviewer", "manager"] {
            role_sessions.insert(
                role.to_string(),
                crate::workflow::run::RoleBinding {
                    session_label: role.to_string(),
                    current_session_id: None,
                    daemon_session_uid: None,
                    bound: false,
                },
            );
        }
        let run = crate::workflow::run::WorkflowRun::new(
            run_id.to_string(),
            "feedback".to_string(),
            "/tmp/seed-task-key".to_string(),
            role_sessions,
            initial_role.to_string(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        );
        crate::workflow::run::save(&run).expect("seed save ok");
        run
    }

    /// 10d-2b acceptance: a `workflow_transition` call lands an
    /// event in `~/.cm/workflow-runs/<run_id>/events.jsonl` via
    /// the 10d-2a `WorkflowEventsWriter`. Wire shape pinned so
    /// the TUI's existing tail loop reads the new event exactly
    /// like the MCP-server-side `_append_event` produced.
    #[test]
    fn workflow_transition_appends_event_with_expected_shape() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            // 10d-2c-1: seed state.json so the handler's
            // load-modify-write has something to load.
            seed_workflow_run("wf_transition_test", "worker");
            let params = json!({
                "to": "reviewer",
                "prompt": "diff lgtm?",
                "run_id": "wf_transition_test",
                "role": "worker",
            });
            let result = workflow_transition(&state, &Caller::operator("op-test"), &params).expect("ok");
            assert_eq!(result["ok"], true);
            assert_eq!(result["run_id"], "wf_transition_test");
            let event_id = result["event_id"].as_str().expect("event_id present");
            assert!(!event_id.is_empty());

            // Read via the existing tailer — the TUI's read path.
            let (events, _offset) =
                crate::workflow::events::read_new("wf_transition_test", 0);
            assert_eq!(events.len(), 1);
            let ev = &events[0];
            assert_eq!(ev.id, event_id);
            assert_eq!(ev.run_id, "wf_transition_test");
            assert_eq!(ev.role, "worker");
            assert_eq!(ev.tool, "workflow_transition");
            match ev.kind() {
                crate::workflow::events::EventKind::Transition { to, prompt } => {
                    assert_eq!(to, "reviewer");
                    assert_eq!(prompt, "diff lgtm?");
                }
                _ => panic!("expected Transition kind"),
            }
        });
    }

    /// 10d-2c-1 review round-7 (F2): the event carries the
    /// PRE-MUTATION `from_role` (captured by the daemon's
    /// closure under flock). Pre-fix the TUI's tail derived
    /// `from_role` from in-memory `active_role` AFTER the
    /// daemon's mutation, recording `from_role = to` (wrong).
    /// Post-fix the event itself is authoritative.
    #[test]
    fn workflow_transition_event_carries_pre_mutation_from_role() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_from_role_pin", "worker");
            // Daemon's mutation will flip active_role from
            // "worker" to "reviewer"; the event's from_role
            // must be the PRE-mutation value ("worker").
            let result = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_from_role_pin",
                    "role": "worker",
                }),
            )
            .expect("ok");
            assert_eq!(result["ok"], true);

            // State.json now has post-mutation active_role.
            let post = crate::workflow::run::load_one("wf_from_role_pin").unwrap();
            assert_eq!(
                post.active_role.as_deref(),
                Some("reviewer"),
                "daemon mutation must advance active_role to `to`",
            );

            // The event carries the PRE-mutation outgoing role.
            let (events, _) =
                crate::workflow::events::read_new("wf_from_role_pin", 0);
            assert_eq!(events.len(), 1);
            assert_eq!(
                events[0].from_role.as_deref(),
                Some("worker"),
                "event.from_role must be the PRE-mutation active_role \
                 (the outgoing role), not the post-mutation `to`",
            );
        });
    }

    /// 10d-2c-1 review round-7 (F2): `workflow_done` events
    /// carry `from_role: None` — the active role is being torn
    /// down, no "next role" semantics apply.
    #[test]
    fn workflow_done_event_carries_none_from_role() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_done_no_from", "manager");
            workflow_done(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "reason": "approved",
                    "run_id": "wf_done_no_from",
                    "role": "manager",
                }),
            )
            .expect("ok");
            let (events, _) =
                crate::workflow::events::read_new("wf_done_no_from", 0);
            assert_eq!(events.len(), 1);
            assert!(
                events[0].from_role.is_none(),
                "workflow_done events must have from_role=None; \
                 got {:?}",
                events[0].from_role,
            );
        });
    }

    /// 10d-2b: `workflow_done` is the same path as transition,
    /// different tool tag + args. Pin the wire shape.
    #[test]
    fn workflow_done_appends_event_with_expected_shape() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_done_test", "manager");
            let params = json!({
                "reason": "approved",
                "run_id": "wf_done_test",
                "role": "manager",
            });
            let result = workflow_done(&state, &Caller::operator("op-test"), &params).expect("ok");
            let event_id = result["event_id"].as_str().expect("event_id");

            let (events, _) =
                crate::workflow::events::read_new("wf_done_test", 0);
            assert_eq!(events.len(), 1);
            let ev = &events[0];
            assert_eq!(ev.id, event_id);
            assert_eq!(ev.run_id, "wf_done_test");
            assert_eq!(ev.role, "manager");
            assert_eq!(ev.tool, "workflow_done");
            match ev.kind() {
                crate::workflow::events::EventKind::Done { reason } => {
                    assert_eq!(reason, "approved");
                }
                _ => panic!("expected Done kind"),
            }
        });
    }

    /// 10d-2b: empty `role` collapses to `"unknown"` — parity
    /// with the MCP-server-side `_append_event`'s
    /// `os.environ.get("CM_ROLE", "").strip() or "unknown"`
    /// default. Tests the fallback path explicitly so a future
    /// validator change can't silently break it.
    #[test]
    fn workflow_transition_empty_role_falls_back_to_unknown() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_empty_role", "worker");
            let params = json!({
                "to": "reviewer",
                "prompt": "p",
                "run_id": "wf_empty_role",
                "role": "",
            });
            workflow_transition(&state, &Caller::operator("op-test"), &params).expect("ok");

            let (events, _) = crate::workflow::events::read_new("wf_empty_role", 0);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].role, "unknown");
        });
    }

    /// 10d-2b: missing `run_id` → InvalidParams (loud error,
    /// not silent file write). Pre-fix the file-writer would
    /// have raised `KeyError` on the Python side; daemon path
    /// surfaces it as an RPC error.
    #[test]
    fn workflow_transition_missing_run_id_is_invalid_params() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let params = json!({
                "to": "reviewer",
                "prompt": "p",
                "run_id": "",
                "role": "worker",
            });
            let err = workflow_transition(&state, &Caller::operator("op-test"), &params)
                .expect_err("empty run_id must reject");
            assert_eq!(err.0, ErrorCode::InvalidParams);
            assert!(err.1.contains("run_id"), "msg mentions run_id: {}", err.1);
        });
    }

    /// 10d-2b: missing `to` (transition target) → InvalidParams.
    #[test]
    fn workflow_transition_missing_to_is_invalid_params() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let params = json!({
                "to": "",
                "prompt": "p",
                "run_id": "wf_no_to",
                "role": "worker",
            });
            let err = workflow_transition(&state, &Caller::operator("op-test"), &params)
                .expect_err("empty to must reject");
            assert_eq!(err.0, ErrorCode::InvalidParams);
        });
    }

    /// 10d-2b: malformed params (wrong types) → InvalidParams.
    /// Parity with the existing methods' shape-validation.
    #[test]
    fn workflow_transition_malformed_params_is_invalid_params() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let params = json!({
                // `to` should be a string; passing an int.
                "to": 42,
                "prompt": "p",
                "run_id": "wf_malformed",
                "role": "worker",
            });
            let err = workflow_transition(&state, &Caller::operator("op-test"), &params).expect_err("malformed");
            assert_eq!(err.0, ErrorCode::InvalidParams);
        });
    }

    // ============================================================
    // 10d-2c-1: state.json read-modify-write under flock(2) tests
    // ============================================================

    /// 10d-2c-1 acceptance: a `workflow_transition` call not only
    /// appends the event but ALSO mutates `state.json`. The
    /// outgoing role's history entry gets `deactivated_at` set;
    /// the new history entry has `trigger: McpTransition{event_id}`;
    /// `active_role` is the target; `iteration` is bumped.
    #[test]
    fn workflow_transition_mutates_state_json_on_disk() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let pre = seed_workflow_run("wf_state_mut", "worker");
            assert_eq!(pre.active_role.as_deref(), Some("worker"));
            assert_eq!(pre.iteration, 1);
            assert_eq!(pre.history.len(), 1);

            let result = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "look at this",
                    "run_id": "wf_state_mut",
                    "role": "worker",
                }),
            )
            .expect("ok");
            let event_id = result["event_id"].as_str().unwrap().to_string();

            let post =
                crate::workflow::run::load_one("wf_state_mut").expect("state.json present");
            assert_eq!(
                post.active_role.as_deref(),
                Some("reviewer"),
                "active_role advances to target",
            );
            assert_eq!(post.iteration, 2, "iteration bumps on activate");
            // 10d-2c-1 review round-1 Option A: daemon does NOT
            // append the new history entry — TUI tail observer
            // appends it (with correct `assistant_count_at_start`
            // from transcript-tail visibility). After daemon's
            // mutation only, history.len() stays at 1 (the
            // outgoing role, now closed).
            assert_eq!(
                post.history.len(),
                1,
                "Option A: daemon defers history.push to TUI tail",
            );
            assert!(
                post.history[0].deactivated_at.is_some(),
                "outgoing role's history entry must be closed",
            );
            assert_eq!(post.history[0].role, "worker");
            // The event_id is in the events.jsonl file (already
            // tested via the existing
            // `workflow_transition_appends_event_with_expected_shape`
            // test). The TUI tail will pull it from there when
            // it appends the deferred history entry.
            let _ = event_id;
        });
    }

    /// 10d-2c-1 acceptance: `workflow_done` mutates `state.json`
    /// — closes the active role's history, drops `active_role`
    /// to None, flips `status` to Done, records the reason.
    #[test]
    fn workflow_done_mutates_state_json_on_disk() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_done_mut", "manager");
            workflow_done(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "reason": "ship it",
                    "run_id": "wf_done_mut",
                    "role": "manager",
                }),
            )
            .expect("ok");

            let post =
                crate::workflow::run::load_one("wf_done_mut").expect("state.json present");
            assert!(post.active_role.is_none(), "active_role dropped on Done");
            assert!(
                matches!(post.status, crate::workflow::run::RunStatus::Done),
                "status flips to Done",
            );
            assert_eq!(post.done_reason.as_deref(), Some("ship it"));
            assert!(
                post.history.last().unwrap().deactivated_at.is_some(),
                "the previously-active role's history is closed",
            );
        });
    }

    /// 10d-2c-1: the daemon refreshes its in-memory
    /// `state.workflow_runs` cache on each handler entry — the
    /// post-call cache reflects the on-disk mutation. (Per the
    /// user's "treat the in-memory map as a write-side cache"
    /// rule: we update on write so tests can assert against it
    /// without re-loading the disk file.)
    #[test]
    fn workflow_transition_refreshes_in_memory_cache() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_cache_refresh", "worker");
            // Pre-call: the in-memory map is empty (10d-2a's
            // `workflow_runs` field is the cache; nothing put
            // it there yet for this test).
            {
                let s = state.lock().unwrap();
                assert!(s.workflow_runs.is_empty());
            }
            workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_cache_refresh",
                    "role": "worker",
                }),
            )
            .expect("ok");
            let s = state.lock().unwrap();
            let cached = s
                .workflow_runs
                .get("wf_cache_refresh")
                .expect("cache refreshed on write");
            assert_eq!(cached.active_role.as_deref(), Some("reviewer"));
            assert_eq!(cached.iteration, 2);
        });
    }

    /// 10d-2c-1: if an outside writer mutates `state.json`
    /// between handler calls, the daemon picks up that mutation
    /// on the NEXT handler entry. Validates the user-spec
    /// directive: "don't trust the in-memory copy as
    /// authoritative; re-load from disk on each handler
    /// entry."
    #[test]
    fn workflow_transition_re_reads_disk_on_each_handler_entry() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_re_read", "worker");

            // First call — daemon's mutation: worker → reviewer.
            workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "first",
                    "run_id": "wf_re_read",
                    "role": "worker",
                }),
            )
            .expect("first ok");
            {
                let s = state.lock().unwrap();
                assert_eq!(
                    s.workflow_runs["wf_re_read"].active_role.as_deref(),
                    Some("reviewer"),
                );
            }

            // Outside writer (simulated TUI-static-path) mutates
            // state.json under the same flock-protected helper.
            // Sets active_role back to "worker" and bumps
            // iteration further to simulate a static transition.
            crate::workflow::run::modify("wf_re_read", |run| {
                run.active_role = Some("worker".to_string());
                run.iteration = 99;
            })
            .expect("outside modify");

            // Sanity: daemon's cache is now stale.
            {
                let s = state.lock().unwrap();
                assert_eq!(
                    s.workflow_runs["wf_re_read"].active_role.as_deref(),
                    Some("reviewer"),
                    "in-memory cache is stale (deliberately)",
                );
                assert_eq!(s.workflow_runs["wf_re_read"].iteration, 2);
            }

            // Second call — daemon must re-read the outside
            // mutation. The transition fires from active_role=worker
            // (the outside writer's value) to "manager", picking
            // up the iteration=99 baseline.
            workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "manager",
                    "prompt": "second",
                    "run_id": "wf_re_read",
                    "role": "worker",
                }),
            )
            .expect("second ok");
            let s = state.lock().unwrap();
            let cached = &s.workflow_runs["wf_re_read"];
            assert_eq!(
                cached.active_role.as_deref(),
                Some("manager"),
                "second transition fires from outside-written state",
            );
            assert_eq!(
                cached.iteration, 100,
                "iteration bumps from 99 (outside) to 100",
            );
            // 10d-2c-1 review round-1 Option A: daemon doesn't
            // append the new history entry — that's TUI tail's
            // job. So the "from_role reflects disk state" check
            // moves to a different observable: the outgoing
            // role's `deactivated_at` (closed by close_active_role
            // on the just-read state). The outside-written state
            // had active_role="worker" (with iteration=99); after
            // re-read + close_active_role + iteration+=1, the
            // worker entry should now be closed.
            //
            // (The original entry in seed's history is "worker"
            // initial; after first transition daemon closed it
            // and active_role was "reviewer"; the outside_modify
            // reset active_role to "worker" but DIDN'T re-open
            // the history — `deactivated_at` stayed set. So
            // looking at the worker entry's `deactivated_at`
            // here isn't load-bearing.)
            //
            // What IS load-bearing: the post-transition
            // active_role is "manager" and iteration is 100,
            // both already asserted. That proves the read came
            // from disk, not cache.
            let _ = cached;
        });
    }

    /// 10d-2c-1: `workflow_transition` on a run_id with no
    /// state.json returns `NotFound`, not a panic or silent
    /// success. Pre-fix the file-writer would have written the
    /// event but not detected the missing state — the workflow
    /// would silently stall. Post-fix it loud-fails.
    #[test]
    fn workflow_transition_no_state_json_returns_not_found() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            // Intentionally NOT seeded — state.json missing.
            let err = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_no_state",
                    "role": "worker",
                }),
            )
            .expect_err("must reject");
            assert_eq!(err.0, ErrorCode::NotFound);
            assert!(
                err.1.contains("state.json"),
                "msg mentions state.json: {}",
                err.1,
            );
        });
    }

    /// 10d-2c-1 concurrent write: a daemon-side
    /// `workflow_transition` racing a parallel `run::save`
    /// (simulating TUI's static-path save) under flock leaves
    /// `state.json` parseable — no torn bytes — and ONE of the
    /// two writes wins on the conflicting fields. Tests
    /// flock's mutual-exclusion contract.
    #[test]
    fn workflow_transition_concurrent_with_outside_save_no_corruption() {
        let _tmp = with_temp_home(|| {
            use std::sync::Arc as StdArc;
            use std::thread;

            let state = make_state_arc();
            let seeded = seed_workflow_run("wf_concurrent", "worker");
            let run_for_outside = StdArc::new(seeded);

            // Many parallel writers — N daemon-side transition
            // attempts + N outside `run::save` calls. The flock
            // serializes them; final state must parse and have
            // a consistent shape.
            const N: usize = 16;
            let mut handles = Vec::new();
            for i in 0..N {
                let state_clone = state.clone();
                handles.push(thread::spawn(move || {
                    // 10d-2c-1 review round-1 F3: must use a
                    // role that exists in the seed (worker /
                    // reviewer / manager). Cycle through these
                    // three to exercise concurrent contention.
                    let roles = ["worker", "reviewer", "manager"];
                    workflow_transition(
                        &state_clone,
                        &Caller::operator("op-test"),
                        &json!({
                            "to": roles[i % 3],
                            "prompt": "p",
                            "run_id": "wf_concurrent",
                            "role": "worker",
                        }),
                    )
                }));
                let run_clone = run_for_outside.clone();
                handles.push(thread::spawn(move || {
                    // Touch the disk: load+save preserves
                    // events_offset (simulating TUI's static-
                    // path persisting events_offset). Using
                    // modify to take the same exclusive lock
                    // the daemon takes.
                    let _ = crate::workflow::run::modify("wf_concurrent", |run| {
                        run.events_offset = run.events_offset.saturating_add(1);
                    });
                    let _ = run_clone;
                    Ok::<_, (ErrorCode, String)>(json!({}))
                }));
            }
            for h in handles {
                let _ = h.join();
            }

            // Final state must parse and have a coherent shape.
            let final_run =
                crate::workflow::run::load_one("wf_concurrent").expect("parseable");
            assert_eq!(final_run.run_id, "wf_concurrent");
            // Iteration is at least 1 (the seed). Some
            // transitions may have raced and only one wins per
            // RMW, so the exact final value is non-deterministic.
            assert!(final_run.iteration >= 1);
            // history has at least the initial entry; some
            // number of additional entries from successful
            // transitions.
            assert!(final_run.history.len() >= 1);
            // The first entry is always the initial worker.
            assert_eq!(final_run.history[0].role, "worker");
        });
    }

    // ============================================================
    // 10d-2c-1 review round-1 tests (F1, F3)
    // ============================================================

    /// F1: a rejected Session-caller `workflow_transition` MUST
    /// NOT leave an event on disk. Pre-fix the event was written
    /// before auth ran, so a non-participant could forge prompt
    /// delivery via the TUI's source=daemon tail branch.
    #[test]
    fn workflow_transition_rejected_session_writes_no_event() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_no_leak", "worker");
            // Register an imposter session: matching workflow_role
            // but a DIFFERENT run_id. Auth must reject.
            {
                let mut s = state.lock().unwrap();
                s.tui_sessions.insert(
                    "ts-imposter".to_string(),
                    crate::state::TuiSessionSnapshot {
                        uid: "ts-imposter".to_string(),
                        task_id: None,
                        label: Some("worker".into()),
                        session_type: Some("claude-code".into()),
                        hidden: false,
                        workflow_run_id: Some("wf_different".into()),
                        workflow_role: Some("worker".into()),
                        global_perms: false,
                    },
                );
            }
            let err = workflow_transition(
                &state,
                &Caller::session("ts-imposter"),
                &json!({
                    "to": "reviewer",
                    "prompt": "forged",
                    "run_id": "wf_no_leak",
                    "role": "worker",
                }),
            )
            .expect_err("non-participant must reject");
            assert_eq!(err.0, ErrorCode::Unauthorized);
            // The events.jsonl file must NOT exist (or, if it
            // exists from some prior write, must contain no
            // entries for this rejected call).
            let path = crate::workflow::run::events_path("wf_no_leak");
            let (events, _) = crate::workflow::events::read_new("wf_no_leak", 0);
            assert!(
                events.is_empty(),
                "rejected call must NOT leave an event on disk (path: {:?}, count: {})",
                path,
                events.len(),
            );
        });
    }

    /// F3: transition to an unknown role → `Conflict`. State.json
    /// unchanged. events.jsonl unchanged.
    #[test]
    fn workflow_transition_unknown_target_role_conflict() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_unknown_role", "worker");
            let err = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "typo-role",
                    "prompt": "x",
                    "run_id": "wf_unknown_role",
                    "role": "worker",
                }),
            )
            .expect_err("unknown role must reject");
            assert_eq!(err.0, ErrorCode::Conflict);
            assert!(err.1.contains("typo-role"), "msg names target: {}", err.1);
            // Valid roles enumerated in the message.
            assert!(
                err.1.contains("worker") && err.1.contains("reviewer"),
                "msg should list valid roles: {}",
                err.1,
            );
            let post = crate::workflow::run::load_one("wf_unknown_role").unwrap();
            assert_eq!(post.active_role.as_deref(), Some("worker"));
            assert_eq!(post.iteration, 1);
            let (events, _) = crate::workflow::events::read_new("wf_unknown_role", 0);
            assert!(events.is_empty(), "no event written for unknown-role reject");
        });
    }

    /// F3: transition on a Done run → `Conflict`. State.json
    /// unchanged; events.jsonl unchanged.
    #[test]
    fn workflow_transition_on_done_run_conflict() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_done_state", "worker");
            // Mark the run Done first.
            let _ = crate::workflow::run::modify("wf_done_state", |run| {
                run.status = crate::workflow::run::RunStatus::Done;
                run.active_role = None;
                run.done_reason = Some("test setup".into());
            });
            let err = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "x",
                    "run_id": "wf_done_state",
                    "role": "worker",
                }),
            )
            .expect_err("transition on Done must reject");
            assert_eq!(err.0, ErrorCode::Conflict);
            assert!(err.1.contains("not Running"), "msg cites status: {}", err.1);
            let (events, _) = crate::workflow::events::read_new("wf_done_state", 0);
            assert!(events.is_empty(), "no event written for Done-state reject");
        });
    }

    /// F3: transition on a Paused run → `Conflict`.
    #[test]
    fn workflow_transition_on_paused_run_conflict() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_paused", "worker");
            let _ = crate::workflow::run::modify("wf_paused", |run| {
                run.status = crate::workflow::run::RunStatus::Paused;
            });
            let err = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "x",
                    "run_id": "wf_paused",
                    "role": "worker",
                }),
            )
            .expect_err("paused must reject");
            assert_eq!(err.0, ErrorCode::Conflict);
        });
    }

    /// F3 (workflow_done): done on already-Done run → Conflict.
    #[test]
    /// Round-6 (F2 rollback): workflow_done on an already-Done
    /// run returns Conflict (round-5's idempotency
    /// short-circuit is REMOVED — rollback replaces it). If the
    /// daemon sees `status == Done` on entry, it's because some
    /// other process set it; that's a Conflict, not an
    /// idempotent retry. The round-6 caller-retry recovery path
    /// is via the rollback: a failed call rolls back to
    /// Running, and the next call re-runs the full RMW.
    #[test]
    fn workflow_done_on_done_run_returns_conflict() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_double_done", "manager");
            let _ = crate::workflow::run::modify("wf_double_done", |run| {
                run.status = crate::workflow::run::RunStatus::Done;
                run.active_role = None;
                run.done_reason = Some("first done".into());
            });
            let err = workflow_done(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "reason": "second",
                    "run_id": "wf_double_done",
                    "role": "manager",
                }),
            )
            .expect_err("second done must reject");
            assert_eq!(err.0, ErrorCode::Conflict);
            // done_reason preserved (closure short-circuits at
            // the status check before mutation).
            let post =
                crate::workflow::run::load_one("wf_double_done").unwrap();
            assert_eq!(post.done_reason.as_deref(), Some("first done"));
        });
    }

    /// F2 (round 3): malformed run_ids cannot reach the
    /// filesystem. Each entry point (load_one, save, modify,
    /// try_modify, events::append_event) validates the run_id
    /// before any path is constructed.
    #[test]
    fn workflow_transition_rejects_path_traversal_run_ids() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            // Seed a real run with a normal id so the test
            // shows the malformed call CANNOT touch it.
            seed_workflow_run("wf_normal", "worker");

            let bad_ids = [
                "../etc/passwd",
                "foo/../bar",
                "../../wf_normal",
                ".",
                "..",
                "",
                "with\0null",
                "wf with space",
                // 200-char overflow
                &"x".repeat(200),
            ];
            for bad in bad_ids {
                let result = workflow_transition(
                    &state,
                    &Caller::operator("op-test"),
                    &json!({
                        "to": "reviewer",
                        "prompt": "p",
                        "run_id": bad,
                        "role": "worker",
                    }),
                );
                assert!(
                    result.is_err(),
                    "malformed run_id {:?} must reject",
                    bad,
                );
                let err = result.unwrap_err();
                // run_id="" hits the explicit InvalidParams
                // check in the handler before reaching
                // try_modify; other malformed shapes hit
                // try_modify's validator which surfaces as
                // InvalidParams too.
                assert_eq!(
                    err.0,
                    ErrorCode::InvalidParams,
                    "malformed run_id {:?} must error as InvalidParams (got {:?}: {})",
                    bad,
                    err.0,
                    err.1,
                );
            }

            // Cross-validation: the seeded normal run is
            // untouched.
            let normal = crate::workflow::run::load_one("wf_normal").unwrap();
            assert_eq!(normal.active_role.as_deref(), Some("worker"));
            assert_eq!(normal.iteration, 1);
        });
    }

    /// F2 (round 3): `run::load_one` / `save` / `modify` /
    /// `try_modify` all share one validator. Calling any of
    /// them with a malformed run_id surfaces InvalidInput
    /// before path construction. Pin the contract.
    #[test]
    fn run_helpers_reject_malformed_run_ids() {
        use crate::workflow::run;
        let _tmp = with_temp_home(|| {
            for bad in ["..", "foo/bar", "", "with\0", &"x".repeat(200)] {
                // load_one returns None on validation fail (it
                // returns Option, not Result).
                assert!(run::load_one(bad).is_none(),
                    "load_one({:?}) must return None", bad);

                // modify returns Err.
                let r = run::modify(bad, |_| {});
                assert!(r.is_err(), "modify({:?}) must reject", bad);

                // try_modify returns Persist(Io(InvalidInput)).
                let outcome: run::TryModifyOutcome<()> =
                    run::try_modify(bad, |_| Ok(()));
                assert!(
                    matches!(
                        outcome,
                        run::TryModifyOutcome::Persist(
                            run::PersistError::Io(ref e)
                        ) if e.kind() == std::io::ErrorKind::InvalidInput,
                    ),
                    "try_modify({:?}) must surface InvalidInput",
                    bad,
                );
            }
        });
    }

    /// F1 (workflow_done): rejected workflow_done writes no event.
    #[test]
    fn workflow_done_rejected_writes_no_event() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_done_no_leak", "manager");
            {
                let mut s = state.lock().unwrap();
                s.tui_sessions.insert(
                    "ts-imposter".to_string(),
                    crate::state::TuiSessionSnapshot {
                        uid: "ts-imposter".to_string(),
                        task_id: None,
                        label: Some("manager".into()),
                        session_type: Some("claude-code".into()),
                        hidden: false,
                        workflow_run_id: Some("wf_other_run".into()),
                        workflow_role: Some("manager".into()),
                        global_perms: false,
                    },
                );
            }
            let err = workflow_done(
                &state,
                &Caller::session("ts-imposter"),
                &json!({
                    "reason": "x",
                    "run_id": "wf_done_no_leak",
                    "role": "manager",
                }),
            )
            .expect_err("non-participant must reject");
            assert_eq!(err.0, ErrorCode::Unauthorized);
            let (events, _) =
                crate::workflow::events::read_new("wf_done_no_leak", 0);
            assert!(events.is_empty());
        });
    }

    /// Round-6 (F2 rollback): on persistent `append_event`
    /// failure, state.json is ROLLED BACK to the pre-mutation
    /// snapshot. Caller's external retry sees the original
    /// state (active_role still matches caller's bound role) and
    /// can re-issue cleanly. Pre-fix (round-5) state stayed
    /// advanced and the retry hit Unauthorized because
    /// `active_role` had moved past the caller's role.
    #[test]
    /// 10d-2c-1 review round-12 (F1) — named acceptance test.
    /// During the event-write retry window the TUI can write
    /// concurrent updates to the same run (sync_role_session_ids,
    /// role_baselines, events_offset). The rollback path must
    /// restore ONLY daemon-owned fields (active_role, iteration)
    /// — pre-r12 the wholesale `*r = snap` clobbered the TUI's
    /// concurrent updates.
    ///
    /// Simulates the race by manually applying a TUI-style
    /// update between the daemon's try_modify (which captures
    /// the snapshot) and the rollback. The TUI update is to
    /// `role_sessions[reviewer].current_session_id` — TUI
    /// territory per the round-6 ownership split.
    #[test]
    fn workflow_transition_rollback_preserves_concurrent_tui_role_sessions_update() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_r12_concurrent", "worker");

            // Pre-mutation snapshot for assertions.
            let pre = crate::workflow::run::load_one("wf_r12_concurrent")
                .expect("pre load");
            assert_eq!(pre.active_role.as_deref(), Some("worker"));
            assert_eq!(pre.iteration, 1);

            // Block event write with EISDIR → daemon mutation
            // succeeds, event-write retries, then rolls back.
            let dir = crate::workflow::run::run_dir("wf_r12_concurrent");
            std::fs::create_dir_all(&dir).unwrap();
            let events_path =
                crate::workflow::run::events_path("wf_r12_concurrent");
            std::fs::create_dir(&events_path).expect("events.jsonl as dir");

            // Spawn a thread that polls state.json and, once it
            // sees the daemon's mutation land (active_role
            // flipped to "reviewer"), applies a TUI-style
            // role_sessions update via `run::modify`. This
            // races against the rollback that will restore
            // active_role to "worker"; the assertion below
            // verifies the role_sessions update survives.
            let bg = std::thread::spawn(move || {
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_millis(400);
                loop {
                    if std::time::Instant::now() > deadline {
                        return false;
                    }
                    let Some(run) = crate::workflow::run::load_one("wf_r12_concurrent")
                    else {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    };
                    if run.active_role.as_deref() == Some("reviewer") {
                        // Daemon mutation has landed. Apply the
                        // TUI-style update.
                        let result = crate::workflow::run::modify(
                            "wf_r12_concurrent",
                            |r| {
                                if let Some(b) = r.role_sessions.get_mut("reviewer") {
                                    b.current_session_id = Some("ts-tui-update".into());
                                }
                            },
                        );
                        return result.is_ok();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            });

            // Daemon call: try_modify advances state, retries
            // event_write (each fails ~50/100/200ms), then
            // rolls back active_role + iteration.
            let err = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_r12_concurrent",
                    "role": "worker",
                }),
            )
            .expect_err("event write must fail after retries");
            assert_eq!(err.0, ErrorCode::Internal);

            let bg_applied = bg.join().expect("bg joined");
            assert!(
                bg_applied,
                "background TUI update must have landed during \
                 the retry window",
            );

            // Daemon-owned fields rolled back.
            let post = crate::workflow::run::load_one("wf_r12_concurrent")
                .expect("post load");
            assert_eq!(
                post.active_role,
                pre.active_role,
                "round-12 F1: active_role rolled back to pre-mutation; \
                 got {:?}",
                post.active_role,
            );
            assert_eq!(
                post.iteration, pre.iteration,
                "iteration rolled back",
            );
            // TUI-owned field PRESERVED. Pre-r12 the wholesale
            // `*r = snap` would have clobbered this back to
            // None.
            assert_eq!(
                post.role_sessions
                    .get("reviewer")
                    .and_then(|b| b.current_session_id.clone())
                    .as_deref(),
                Some("ts-tui-update"),
                "round-12 F1: TUI-owned role_sessions update must \
                 survive rollback (pre-fix it was clobbered by the \
                 wholesale snapshot restore)",
            );
        });
    }

    /// Phase 3 (doc/daemon-side-workflow-orchestration.md) — append-exhaustion
    /// rollback must leave NO orphan `pending_activation`. The mutation records
    /// a pending_activation in the same closure that advances active_role; if
    /// the event append exhausts, the field-targeted rollback restores
    /// active_role + iteration AND clears pending_activation. Otherwise the
    /// drainer would deliver against a role that is no longer active.
    #[test]
    fn workflow_transition_rollback_clears_orphan_pending_activation() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_pa_rollback", "worker");

            let pre = crate::workflow::run::load_one("wf_pa_rollback").expect("pre load");
            assert_eq!(pre.active_role.as_deref(), Some("worker"));
            assert!(pre.pending_activation.is_none());

            // Block the event write so the mutation rolls back.
            let dir = crate::workflow::run::run_dir("wf_pa_rollback");
            std::fs::create_dir_all(&dir).unwrap();
            let events_path = crate::workflow::run::events_path("wf_pa_rollback");
            std::fs::create_dir(&events_path).expect("events.jsonl as dir");

            let err = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "do the thing",
                    "run_id": "wf_pa_rollback",
                    "role": "worker",
                }),
            )
            .expect_err("event write must fail after retries");
            assert_eq!(err.0, ErrorCode::Internal);

            let post = crate::workflow::run::load_one("wf_pa_rollback").expect("post load");
            // active_role rolled back...
            assert_eq!(post.active_role.as_deref(), Some("worker"));
            // ...and NO orphan pending_activation left for the target role.
            assert!(
                post.pending_activation.is_none(),
                "exhausted append must leave no orphan pending_activation; got {:?}",
                post.pending_activation,
            );
        });
    }

    /// 10d-2c-1 review round-13 — named acceptance test.
    /// Pre-r13 the rollback restored `active_role` +
    /// `iteration` but left the active history entry's
    /// `deactivated_at` set by `close_active_role`. Post-rollback
    /// the run had `active_role = worker` but worker's history
    /// entry showed `deactivated_at: Some(...)` — an inconsistent
    /// state that `close_active_role`'s idempotency prevented
    /// caller retries from repairing. R13 restores
    /// `deactivated_at` + `last_message` on the matching (role,
    /// iteration) entry.
    #[test]
    fn workflow_transition_rollback_restores_active_history_deactivation() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_r13_history_restore", "worker");

            // Sanity: seed shape has one initial worker history
            // entry with `deactivated_at: None`.
            let pre = crate::workflow::run::load_one("wf_r13_history_restore")
                .expect("pre load");
            assert_eq!(pre.history.len(), 1);
            assert_eq!(pre.history[0].role, "worker");
            assert!(pre.history[0].deactivated_at.is_none());
            assert!(pre.history[0].last_message.is_none());

            // Block event-append → mutation succeeds (close +
            // iteration++ + active_role flip), retries exhaust,
            // rollback fires.
            let dir = crate::workflow::run::run_dir("wf_r13_history_restore");
            std::fs::create_dir_all(&dir).unwrap();
            let events_path =
                crate::workflow::run::events_path("wf_r13_history_restore");
            std::fs::create_dir(&events_path).expect("events.jsonl as dir");

            let err = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_r13_history_restore",
                    "role": "worker",
                }),
            )
            .expect_err("event write must fail after retries");
            assert_eq!(err.0, ErrorCode::Internal);

            // Round-13 assertion: active history entry's
            // deactivated_at/last_message restored to pre-
            // mutation (None/None). Pre-r13 deactivated_at
            // was Some(...) — the `close_active_role` side
            // effect that the wholesale-snapshot rollback used
            // to undo but the round-12 field-targeted rollback
            // missed.
            let post = crate::workflow::run::load_one("wf_r13_history_restore")
                .expect("post load");
            assert_eq!(post.active_role.as_deref(), Some("worker"));
            assert_eq!(post.history.len(), 1);
            assert_eq!(post.history[0].role, "worker");
            assert!(
                post.history[0].deactivated_at.is_none(),
                "round-13: worker history entry's deactivated_at must \
                 be rolled back to None; got {:?}",
                post.history[0].deactivated_at,
            );
            assert!(
                post.history[0].last_message.is_none(),
                "round-13: worker history entry's last_message must be \
                 rolled back to None; got {:?}",
                post.history[0].last_message,
            );

            // Drive a successful retry (heal disk, re-call).
            // Asserts no leftover deactivation issue blocks
            // forward progress.
            std::fs::remove_dir(&events_path).expect("remove dir");
            let resp = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_r13_history_restore",
                    "role": "worker",
                }),
            )
            .expect("retry must succeed after r13 clean rollback");
            assert_eq!(resp["ok"], json!(true));
            let final_ = crate::workflow::run::load_one("wf_r13_history_restore")
                .expect("final load");
            assert_eq!(final_.active_role.as_deref(), Some("reviewer"));
            // The retry's close_active_role now correctly
            // deactivates the worker entry (it was None
            // post-rollback, not stale Some).
            assert!(
                final_.history[0].deactivated_at.is_some(),
                "after successful retry, worker entry is properly \
                 deactivated; pre-r13 the idempotent close_active_role \
                 would have left it untouched because deactivated_at \
                 was already Some from the failed first call",
            );
        });
    }

    #[test]
    fn workflow_transition_persistent_event_failure_rolls_back_state_returns_internal() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_evfail_r6", "worker");
            // Capture pre-call state for the post-call rollback
            // assertion. Includes iteration, active_role.
            let pre = crate::workflow::run::load_one("wf_evfail_r6").unwrap();
            let dir = crate::workflow::run::run_dir("wf_evfail_r6");
            std::fs::create_dir_all(&dir).unwrap();
            // Block append_event with EISDIR — persistent.
            let events_path =
                crate::workflow::run::events_path("wf_evfail_r6");
            std::fs::create_dir(&events_path).expect("events.jsonl as dir");

            let start = std::time::Instant::now();
            let err = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_evfail_r6",
                    "role": "worker",
                }),
            )
            .expect_err("event write must fail after retries");
            let elapsed = start.elapsed();
            assert_eq!(err.0, ErrorCode::Internal);
            assert!(
                err.1.contains("failed to append event"),
                "error msg surfaces event-write failure: {}",
                err.1,
            );
            assert!(
                elapsed >= std::time::Duration::from_millis(300),
                "retry backoff must elapse before final Err; \
                 elapsed={:?}",
                elapsed,
            );

            // Round-6 behavior: state.json ROLLED BACK to
            // pre-mutation snapshot.
            let post = crate::workflow::run::load_one("wf_evfail_r6").unwrap();
            assert_eq!(
                post.active_role,
                pre.active_role,
                "round-6: state rolled back to pre-mutation \
                 active_role={:?}; observed post={:?}",
                pre.active_role,
                post.active_role,
            );
            assert_eq!(
                post.iteration, pre.iteration,
                "round-6: iteration rolled back to pre-mutation \
                 value {}; observed post={}",
                pre.iteration, post.iteration,
            );
            // No event on disk.
            let (events, _) =
                crate::workflow::events::read_new("wf_evfail_r6", 0);
            assert!(events.is_empty(), "no events after rollback");
        });
    }

    /// Round-6 (F2 rollback): caller's external retry after a
    /// rollback succeeds cleanly. Since state was rolled back to
    /// pre-mutation, the second call re-runs full RMW from
    /// scratch — single mutation + single event end state.
    #[test]
    fn workflow_transition_caller_retry_after_rollback_succeeds_cleanly() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_retry_r6", "worker");
            let dir = crate::workflow::run::run_dir("wf_retry_r6");
            std::fs::create_dir_all(&dir).unwrap();

            // Stage 1: block event write → rollback fires.
            let events_path =
                crate::workflow::run::events_path("wf_retry_r6");
            std::fs::create_dir(&events_path).expect("events.jsonl as dir");
            let err = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_retry_r6",
                    "role": "worker",
                }),
            )
            .expect_err("first call: event write fails");
            assert_eq!(err.0, ErrorCode::Internal);
            // Rolled back.
            let mid = crate::workflow::run::load_one("wf_retry_r6").unwrap();
            assert_eq!(mid.active_role.as_deref(), Some("worker"));
            assert_eq!(mid.iteration, 1);

            // Stage 2: heal disk + retry.
            std::fs::remove_dir(&events_path).expect("remove dir");
            let ok = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_retry_r6",
                    "role": "worker",
                }),
            )
            .expect("retry must succeed after rollback");
            assert_eq!(ok["ok"], json!(true));

            // Single end-state mutation: active_role advanced
            // ONCE; iteration bumped ONCE; single event.
            let post = crate::workflow::run::load_one("wf_retry_r6").unwrap();
            assert_eq!(post.active_role.as_deref(), Some("reviewer"));
            assert_eq!(
                post.iteration, 2,
                "iteration bumped exactly once (the retry); \
                 no double-mutation"
            );
            let (events, _) =
                crate::workflow::events::read_new("wf_retry_r6", 0);
            assert_eq!(events.len(), 1, "exactly one event after the retry");
        });
    }

    /// Round-5 (F2): transient `append_event` failure (block,
    /// then unblock) recovers within the retry budget. We model
    /// the transient by removing the blocker via a background
    /// thread between attempts.
    #[test]
    fn workflow_transition_transient_event_failure_recovers_via_retry() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_tx_r5", "worker");
            let dir = crate::workflow::run::run_dir("wf_tx_r5");
            std::fs::create_dir_all(&dir).unwrap();
            let events_path = crate::workflow::run::events_path("wf_tx_r5");
            std::fs::create_dir(&events_path).expect("events.jsonl as dir");

            // Background thread: clears the blocker after first
            // backoff completes. First retry should then succeed.
            let evp = events_path.clone();
            let _bg = std::thread::spawn(move || {
                // 70ms — after the 50ms first backoff but
                // before the 100ms second backoff completes.
                std::thread::sleep(std::time::Duration::from_millis(70));
                let _ = std::fs::remove_dir(&evp);
            });

            let resp = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_tx_r5",
                    "role": "worker",
                }),
            )
            .expect("transient failure must recover via retry");
            assert_eq!(resp["ok"], json!(true));

            // State advanced (would have advanced regardless;
            // here the event ALSO lands within the retry budget).
            let post = crate::workflow::run::load_one("wf_tx_r5").unwrap();
            assert_eq!(post.active_role.as_deref(), Some("reviewer"));
            let (events, _) = crate::workflow::events::read_new("wf_tx_r5", 0);
            assert_eq!(events.len(), 1, "event landed via retry");
        });
    }

    #[test]
    /// 10d-2c-1 review round-5 (F1): a session spawned with
    /// `workflow_run_id` / `workflow_role` in `StartSessionParams`
    /// passes the daemon-side auth check on `workflow_transition`.
    /// Pre-fix the daemon-owned session's workflow fields were
    /// hard-coded to None in `lookup_session_any`, so a daemon-
    /// attached agent could never participate in a workflow.
    #[test]
    fn daemon_attached_workflow_participant_passes_workflow_transition_auth() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_r5_spawn", "worker");
            // Insert a daemon-owned session WITH workflow context
            // at spawn time. Bypass the real PendingSession::spawn
            // by injecting a session directly into state.sessions
            // via the SpawnParams + spawn pair the auth tests use.
            let mut p = crate::session::SpawnParams::new(
                "ts-daemon-worker",
                "test-daemon-worker",
                "/bin/sleep",
            );
            p.args = vec!["3".to_string()];
            p.workspace_id = "ws-test".to_string();
            p.workflow_run_id = Some("wf_r5_spawn".to_string());
            p.workflow_role = Some("worker".to_string());
            let pending =
                crate::session::PendingSession::spawn(p).expect("spawn ok");
            let session = pending.arm_reaper(None).expect("arm ok");
            {
                let mut s = state.lock().unwrap();
                s.sessions.insert("ts-daemon-worker".to_string(), session);
            }

            // Active role on the seeded run is "worker"; caller
            // is bound to "worker". Auth must allow.
            let resp = workflow_transition(
                &state,
                &Caller::session("ts-daemon-worker"),
                &json!({
                    "to": "reviewer",
                    "prompt": "go",
                    "run_id": "wf_r5_spawn",
                    "role": "worker",
                }),
            )
            .expect("daemon-attached workflow participant must pass auth");
            assert_eq!(resp["ok"], json!(true));
        });
    }

    /// F1 round-5 (negative): a daemon-attached session with NO
    /// workflow context still gets Unauthorized — auth must not
    /// widen to "any daemon-owned session in the right
    /// workspace." Defense against a non-participant daemon
    /// session forging a transition.
    #[test]
    fn daemon_attached_without_workflow_context_gets_unauthorized() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_r5_no_ctx", "worker");
            // No workflow_run_id/workflow_role on the spawn —
            // matches the regular A-n / A-s spawn shape.
            let mut p = crate::session::SpawnParams::new(
                "ts-daemon-nonparticipant",
                "test-daemon-nonparticipant",
                "/bin/sleep",
            );
            p.args = vec!["3".to_string()];
            p.workspace_id = "ws-test".to_string();
            let pending =
                crate::session::PendingSession::spawn(p).expect("spawn ok");
            let session = pending.arm_reaper(None).expect("arm ok");
            {
                let mut s = state.lock().unwrap();
                s.sessions.insert("ts-daemon-nonparticipant".to_string(), session);
            }

            let err = workflow_transition(
                &state,
                &Caller::session("ts-daemon-nonparticipant"),
                &json!({
                    "to": "reviewer",
                    "prompt": "forged",
                    "run_id": "wf_r5_no_ctx",
                    "role": "worker",
                }),
            )
            .expect_err("non-participant daemon session must reject");
            assert_eq!(err.0, ErrorCode::Unauthorized);
        });
    }

    /// F1 round-5: `session.set_workflow_context` updates a
    /// daemon-owned session's workflow context, after which the
    /// auth check passes. This is the after-the-fact tagging
    /// path used by `launch_workflow` for Existing-slot bindings
    /// on daemon-attached sessions (the typical worker shape).
    #[test]
    fn set_workflow_context_then_workflow_transition_passes_auth() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_r5_setctx", "worker");
            // Spawn WITHOUT workflow context (simulates an
            // already-spawned A-s daemon session pre-launch).
            let mut p = crate::session::SpawnParams::new(
                "ts-existing",
                "test-existing",
                "/bin/sleep",
            );
            p.args = vec!["3".to_string()];
            p.workspace_id = "ws-test".to_string();
            let pending =
                crate::session::PendingSession::spawn(p).expect("spawn ok");
            let session = pending.arm_reaper(None).expect("arm ok");
            {
                let mut s = state.lock().unwrap();
                s.sessions.insert("ts-existing".to_string(), session);
            }

            // Pre-set: auth fails.
            let pre = workflow_transition(
                &state,
                &Caller::session("ts-existing"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_r5_setctx",
                    "role": "worker",
                }),
            );
            assert!(pre.is_err(), "pre-set must reject");
            assert_eq!(pre.unwrap_err().0, ErrorCode::Unauthorized);

            // Apply set_workflow_context.
            let ok = set_workflow_context(
                &state,
                &json!({
                    "uid": "ts-existing",
                    "workflow_run_id": "wf_r5_setctx",
                    "workflow_role": "worker",
                }),
            )
            .expect("set_workflow_context ok");
            assert_eq!(ok["ok"], json!(true));
            assert_eq!(ok["daemon_owned"], json!(true));

            // Post-set: auth passes.
            let post = workflow_transition(
                &state,
                &Caller::session("ts-existing"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_r5_setctx",
                    "role": "worker",
                }),
            )
            .expect("post-set must pass auth");
            assert_eq!(post["ok"], json!(true));
        });
    }

    /// F1 round-5: `set_workflow_context` on an unknown uid
    /// returns success with `daemon_owned: false` — TUI calls
    /// this helper for every workflow participant; a TUI-local
    /// session legitimately isn't in `state.sessions`.
    #[test]
    fn set_workflow_context_returns_daemon_owned_false_for_unknown_uid() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let resp = set_workflow_context(
                &state,
                &json!({
                    "uid": "ts-not-in-daemon",
                    "workflow_run_id": "wf_x",
                    "workflow_role": "worker",
                }),
            )
            .expect("unknown uid no-ops");
            assert_eq!(resp["daemon_owned"], json!(false));
        });
    }

    /// Global-perms feature: `set_global_perms` flips a live
    /// session's grant so the daemon's Session-caller auth honors it
    /// immediately. A non-global caller that was OutOfScope for a
    /// cross-workspace target becomes Allowed once granted.
    #[test]
    fn set_global_perms_flips_live_grant_and_auth() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            // Caller in ws-1, unrelated target in ws-2.
            for (uid, ws) in [("ts-grant", "ws-1"), ("ts-target", "ws-2")] {
                let mut p = crate::session::SpawnParams::new(uid, uid, "/bin/sleep");
                p.args = vec!["3".to_string()];
                p.workspace_id = ws.to_string();
                let sess = crate::session::PendingSession::spawn(p)
                    .expect("spawn")
                    .arm_reaper(None)
                    .expect("arm");
                state.lock().unwrap().sessions.insert(uid.to_string(), sess);
            }

            // Before the grant: cross-workspace target is out of scope.
            {
                let st = state.lock().unwrap();
                assert_eq!(
                    crate::control::auth::check_session_caller(&st, "ts-grant", "ts-target"),
                    crate::control::auth::AuthDecision::OutOfScope,
                );
            }

            let resp = set_global_perms(
                &state,
                &json!({ "uid": "ts-grant", "global_perms": true }),
            )
            .expect("set_global_perms ok");
            assert_eq!(resp["ok"], json!(true));
            assert_eq!(resp["daemon_owned"], json!(true));

            // After the grant: the caller reaches the unrelated target.
            {
                let st = state.lock().unwrap();
                assert!(st.sessions.get("ts-grant").unwrap().global_perms);
                assert_eq!(
                    crate::control::auth::check_session_caller(&st, "ts-grant", "ts-target"),
                    crate::control::auth::AuthDecision::Allow,
                );
            }

            // Revoke returns the caller to scoped behavior.
            set_global_perms(&state, &json!({ "uid": "ts-grant", "global_perms": false }))
                .expect("revoke ok");
            let st = state.lock().unwrap();
            assert!(!st.sessions.get("ts-grant").unwrap().global_perms);
        });
    }

    /// Unknown uid no-ops with `daemon_owned: false` (mirrors
    /// `set_workflow_context`), so the TUI can fire it without
    /// branching on session ownership.
    #[test]
    fn set_global_perms_unknown_uid_no_ops() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let resp = set_global_perms(
                &state,
                &json!({ "uid": "ts-nope", "global_perms": true }),
            )
            .expect("unknown uid no-ops");
            assert_eq!(resp["daemon_owned"], json!(false));
        });
    }

    /// Escalation guard: a NON-global caller requesting
    /// `global_perms=true` on `mcp_start_session` is rejected
    /// (Unauthorized) BEFORE any spawn happens — a normal agent
    /// can't mint a privileged child to escape its scope.
    #[test]
    fn mcp_start_session_non_global_caller_cannot_grant_global_perms() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let mut p = crate::session::SpawnParams::new("ts-plain", "plain", "/bin/sleep");
            p.args = vec!["3".to_string()];
            p.workspace_id = "ws-1".to_string();
            // Not global.
            let sess = crate::session::PendingSession::spawn(p)
                .expect("spawn")
                .arm_reaper(None)
                .expect("arm");
            state.lock().unwrap().sessions.insert("ts-plain".to_string(), sess);

            let err = mcp_start_session(
                &state,
                &json!({
                    "type": "bash",
                    "label": "child",
                    "global_perms": true,
                }),
                Some("ts-plain"),
            )
            .expect_err("non-global caller must be refused the grant");
            assert_eq!(err.0, ErrorCode::Unauthorized);
            assert!(
                err.1.contains("global"),
                "message should name the escalation guard: {}",
                err.1,
            );
            // No child was spawned — only the caller remains.
            assert_eq!(state.lock().unwrap().sessions.len(), 1);
        });
    }

    /// F1 round-5: half-tagged updates are rejected. Caller-bug
    /// defense — silently storing a session with run_id set but
    /// role None (or vice versa) would surface later as a
    /// confusing Unauthorized.
    #[test]
    fn set_workflow_context_rejects_half_tagged() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let err = set_workflow_context(
                &state,
                &json!({
                    "uid": "any",
                    "workflow_run_id": "wf",
                    "workflow_role": null,
                }),
            )
            .expect_err("half-tagged must reject");
            assert_eq!(err.0, ErrorCode::InvalidParams);
        });
    }

    /// Round-6 (F2): `workflow_done` analog of the rollback +
    /// caller-retry test. After a state-advanced/event-missing
    /// failure, state rolls back to Running; retry re-runs the
    /// full mutation; the retry's `reason` IS the one that
    /// lands (since the first call rolled back, the second call
    /// is the only committed mutation).
    #[test]
    fn workflow_done_caller_retry_after_rollback_succeeds_with_retry_reason() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_done_retry_r6", "manager");
            let dir = crate::workflow::run::run_dir("wf_done_retry_r6");
            std::fs::create_dir_all(&dir).unwrap();
            let events_path =
                crate::workflow::run::events_path("wf_done_retry_r6");
            std::fs::create_dir(&events_path).expect("events.jsonl as dir");

            // Stage 1: blocked event write; rollback fires.
            let err = workflow_done(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "reason": "first",
                    "run_id": "wf_done_retry_r6",
                    "role": "manager",
                }),
            )
            .expect_err("first call: event write must fail");
            assert_eq!(err.0, ErrorCode::Internal);
            // Rolled back to Running, no done_reason.
            let mid =
                crate::workflow::run::load_one("wf_done_retry_r6").unwrap();
            assert!(
                matches!(mid.status, crate::workflow::run::RunStatus::Running),
                "round-6: status rolled back to Running, got {:?}",
                mid.status,
            );
            assert!(
                mid.done_reason.is_none(),
                "round-6: done_reason rolled back to None, got {:?}",
                mid.done_reason,
            );

            // Stage 2: heal disk, retry with NEW reason.
            std::fs::remove_dir(&events_path).expect("remove dir");
            let ok = workflow_done(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "reason": "second",
                    "run_id": "wf_done_retry_r6",
                    "role": "manager",
                }),
            )
            .expect("retry must succeed after rollback");
            assert_eq!(ok["ok"], json!(true));

            let post =
                crate::workflow::run::load_one("wf_done_retry_r6").unwrap();
            // Retry's reason is the one that committed (because
            // the first call rolled back).
            assert_eq!(
                post.done_reason.as_deref(),
                Some("second"),
                "round-6: retry reason commits (first rolled back)",
            );
            let (events, _) =
                crate::workflow::events::read_new("wf_done_retry_r6", 0);
            assert_eq!(events.len(), 1, "exactly one event after the retry");
        });
    }

    // ------------------------------------------------------------
    // 10d-2c-2-1: workflow.update_definitions
    // ------------------------------------------------------------

    /// Sanity: pushing a non-empty map populates
    /// `state.workflow_definitions` keyed by `Workflow::name`.
    /// Replace semantics — a second push overwrites the first.
    #[test]
    fn workflow_update_definitions_populates_and_replaces() {
        use crate::workflow::toml_schema::{
            Context, Engine, Role, Workflow,
        };
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            // Pre-state: empty.
            {
                let s = state.lock().unwrap();
                assert!(s.workflow_definitions.is_empty());
            }

            // First push: one workflow "feedback".
            let mut roles_a = BTreeMap::new();
            roles_a.insert(
                "worker".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("a".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            let wf_a = Workflow {
                name: "feedback".to_string(),
                description: String::new(),
                roles: roles_a,
                role_order: vec!["worker".to_string()],
                transitions: vec![],
            };
            let mut map_a = std::collections::HashMap::new();
            map_a.insert("feedback".to_string(), wf_a);

            let resp = workflow_update_definitions(
                &state,
                &json!({"workflows": map_a}),
            )
            .expect("first push ok");
            assert_eq!(resp["ok"], json!(true));
            assert_eq!(resp["workflow_count"], json!(1));
            {
                let s = state.lock().unwrap();
                let wf = s.workflow_definitions.get("feedback").expect("present");
                assert_eq!(wf.role_order, vec!["worker".to_string()]);
                assert!(wf.roles.contains_key("worker"));
            }

            // Second push: replace with a DIFFERENT workflow
            // ("audit"). Original "feedback" must be gone —
            // replace-not-merge.
            let mut roles_b = BTreeMap::new();
            roles_b.insert(
                "auditor".to_string(),
                Role {
                    engine: Engine::Codex,
                    context: Context::Fresh,
                    activation_prompt: Some("b".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            let wf_b = Workflow {
                name: "audit".to_string(),
                description: String::new(),
                roles: roles_b,
                role_order: vec!["auditor".to_string()],
                transitions: vec![],
            };
            let mut map_b = std::collections::HashMap::new();
            map_b.insert("audit".to_string(), wf_b);

            let resp2 = workflow_update_definitions(
                &state,
                &json!({"workflows": map_b}),
            )
            .expect("second push ok");
            assert_eq!(resp2["workflow_count"], json!(1));
            {
                let s = state.lock().unwrap();
                assert!(
                    !s.workflow_definitions.contains_key("feedback"),
                    "first push's workflow must be gone after replace",
                );
                assert!(s.workflow_definitions.contains_key("audit"));
            }
        });
    }

    /// Empty map push is meaningful — clears prior state. Mirrors
    /// `task.update_tree` / `tui.update_sessions_snapshot` semantics.
    #[test]
    fn workflow_update_definitions_empty_push_clears() {
        use crate::workflow::toml_schema::{
            Context, Engine, Role, Workflow,
        };
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            // Seed one entry.
            {
                let mut s = state.lock().unwrap();
                let mut roles = BTreeMap::new();
                roles.insert(
                    "worker".to_string(),
                    Role {
                        engine: Engine::ClaudeCode,
                        context: Context::Persistent,
                        activation_prompt: None,
                        subsequent_activation_prompt: None,
                    needs_mcp: false,
                    },
                );
                s.workflow_definitions.insert(
                    "feedback".to_string(),
                    Workflow {
                        name: "feedback".to_string(),
                        description: String::new(),
                        roles,
                        role_order: vec!["worker".to_string()],
                        transitions: vec![],
                    },
                );
            }
            let resp = workflow_update_definitions(
                &state,
                &json!({"workflows": std::collections::HashMap::<
                    String,
                    crate::workflow::toml_schema::Workflow,
                >::new()}),
            )
            .expect("empty push ok");
            assert_eq!(resp["workflow_count"], json!(0));
            let s = state.lock().unwrap();
            assert!(s.workflow_definitions.is_empty());
        });
    }

    /// Phase 4 §B2: the TUI's `update_definitions` push only replaces the
    /// OVERRIDE layer — it must never clear the daemon-loaded BASE layer. An
    /// override shadows the base; an empty push (TUI reconnect) leaves the base
    /// intact, so `workflow_definition()` still resolves it headlessly.
    #[test]
    fn update_definitions_replaces_override_only_base_survives() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let mk = |order: Vec<&str>| {
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role {
                    engine: Engine::ClaudeCode, context: Context::Persistent,
                    activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false,
                });
                Workflow {
                    name: "feedback".to_string(), description: String::new(), roles,
                    role_order: order.iter().map(|s| s.to_string()).collect(),
                    transitions: vec![],
                }
            };
            // Seed the BASE layer (as startup would from workflows_dir).
            {
                let mut s = state.lock().unwrap();
                s.base_workflow_definitions.insert("feedback".to_string(), mk(vec!["base"]));
            }
            // Base resolves when no override.
            {
                let s = state.lock().unwrap();
                assert_eq!(s.workflow_definition("feedback").unwrap().role_order, vec!["base".to_string()]);
            }
            // A TUI push (override) shadows the base.
            let mut map = std::collections::HashMap::new();
            map.insert("feedback".to_string(), mk(vec!["override"]));
            workflow_update_definitions(&state, &json!({"workflows": map})).expect("push ok");
            {
                let s = state.lock().unwrap();
                assert_eq!(s.workflow_definition("feedback").unwrap().role_order, vec!["override".to_string()]);
            }
            // An empty push (TUI reconnect) clears ONLY the override — base survives.
            workflow_update_definitions(
                &state,
                &json!({"workflows": std::collections::HashMap::<String, crate::workflow::toml_schema::Workflow>::new()}),
            ).expect("empty push ok");
            {
                let s = state.lock().unwrap();
                assert!(s.workflow_definitions.is_empty(), "override cleared");
                assert_eq!(
                    s.workflow_definition("feedback").unwrap().role_order,
                    vec!["base".to_string()],
                    "base survives the override clear"
                );
            }
        });
    }

    /// Phase 4 §D acceptance #2 + #3 (launch): daemon-side `start_workflow`
    /// spawns participants, writes state.json with EXACTLY ONE worker entry
    /// (iteration 1, `Initial`) + an `is_initial` delivery-only pending
    /// activation, and resolves the definition from the BASE layer (NO TUI
    /// override — headless). Uses a lightweight `/bin/sleep` spawn override so
    /// the test doesn't depend on the `claude`/`codex` binaries.
    #[test]
    fn start_workflow_creates_daemon_driven_run_single_initial_entry() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Transition, TriggerOn, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                roles.insert("reviewer".to_string(), Role { engine: Engine::Codex, context: Context::Fresh, activation_prompt: Some("review".to_string()), subsequent_activation_prompt: None, needs_mcp: false });
                roles.insert("manager".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: Some("manage".to_string()), subsequent_activation_prompt: None, needs_mcp: false });
                // BASE layer only — no TUI override.
                s.base_workflow_definitions.insert("feedback".to_string(), Workflow {
                    name: "feedback".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into(), "reviewer".into(), "manager".into()],
                    transitions: vec![
                        Transition { from: "worker".into(), on: TriggerOn::Idle, to: "reviewer".into() },
                        Transition { from: "reviewer".into(), on: TriggerOn::Idle, to: "manager".into() },
                    ],
                });
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "feedback",
                "goal": "implement the feature",
                "worktree": wt.to_str().unwrap(),
                "workspace_id": "ws-1",
            })).expect("start_workflow ok");
            set_spawn_program_override_for_test(None);
            set_disable_workflow_detector_for_test(false);

            let run_id = resp["run_id"].as_str().unwrap().to_string();
            let run = crate::workflow::run::load_one(&run_id).expect("run saved");

            // Criterion #2: exactly ONE worker history entry, iteration 1, Initial.
            assert_eq!(run.history.len(), 1, "only the seeded initial entry");
            assert_eq!(run.history[0].role, "worker");
            assert_eq!(run.history[0].iteration, 1);
            assert!(matches!(run.history[0].trigger, crate::workflow::run::TriggerKind::Initial));
            // Initial activation is delivery-only.
            let pa = run.pending_activation.as_ref().expect("initial pending activation");
            assert!(pa.is_initial);
            assert_eq!(pa.target_role, "worker");
            assert!(!pa.raw_prompt.is_empty(), "worker gets the goal as its initial prompt");
            assert_eq!(run.active_role.as_deref(), Some("worker"));
            // All three participants spawned + registered, bound by daemon uid.
            {
                let s = state.lock().unwrap();
                for role in ["worker", "reviewer", "manager"] {
                    let uid = run.role_sessions[role].daemon_session_uid.as_ref()
                        .unwrap_or_else(|| panic!("{role} has no daemon_session_uid"));
                    assert!(s.sessions.contains_key(uid), "{role} session registered");
                    assert!(run.role_sessions[role].current_session_id.is_none(), "{role} sid discovered later");
                }
            }

            // Drive the initial delivery headlessly: the poller finalizes the
            // worker's is_initial activation (delivery-only) — it must NOT append
            // a second worker row.
            let poller = crate::workflow::poller::WorkflowPoller::new(std::sync::Arc::clone(&state));
            poller.set_finalize_timing_for_test(0, 0);
            poller.poll_once();
            let run = crate::workflow::run::load_one(&run_id).unwrap();
            assert_eq!(
                run.history.iter().filter(|h| h.role == "worker").count(),
                1,
                "initial delivery patches, never appends a 2nd worker entry"
            );
            // Cross-bind fix: the unbound Claude worker's initial activation
            // delivers, then parks in RebindPending awaiting causal sid
            // discovery (it used to clear to Done and rely on the spawn-time
            // detector — the race that wedged headless runs on cm-manager).
            assert_eq!(
                run.pending_activation.as_ref().map(|p| p.phase.clone()),
                Some(crate::workflow::run::ActivationPhase::RebindPending),
                "initial delivery done; sid awaits deliver-then-discover"
            );
        });
    }

    /// P-CRIT — the daemon must thread each participant's workflow identity
    /// (run_id + role) into the MCP-config writer, so `CM_WORKFLOW_RUN_ID` /
    /// `CM_ROLE` land in the MCP server's config env block. WITHOUT this the
    /// reviewer/manager's `workflow_transition` / `workflow_done` calls
    /// hard-fail ("CM_WORKFLOW_RUN_ID is not set") and the headless run stalls
    /// — a regression the poller tests CANNOT catch because they bypass the
    /// real MCP server. This locks the daemon-side half of the chain: every
    /// role reaches `resolve_workflow_spawn_program` with `Some((run_id,
    /// role))` (the build_args writer then emits the env — see
    /// `mcp_config::build_args_claude_workflow_participant_carries_run_id_and_role`).
    /// Mutation check: passing `None` at the spawn site makes this fail.
    #[test]
    fn start_workflow_threads_workflow_meta_into_mcp_config_for_every_role() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Transition, TriggerOn, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt-meta");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                roles.insert("reviewer".to_string(), Role { engine: Engine::Codex, context: Context::Fresh, activation_prompt: Some("review".to_string()), subsequent_activation_prompt: None, needs_mcp: false });
                roles.insert("manager".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: Some("manage".to_string()), subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("feedback".to_string(), Workflow {
                    name: "feedback".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into(), "reviewer".into(), "manager".into()],
                    transitions: vec![
                        Transition { from: "worker".into(), on: TriggerOn::Idle, to: "reviewer".into() },
                        Transition { from: "reviewer".into(), on: TriggerOn::Idle, to: "manager".into() },
                    ],
                });
            }
            let _ = take_captured_workflow_meta_for_test(); // clear any prior capture
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "feedback",
                "goal": "implement the feature",
                "worktree": wt.to_str().unwrap(),
                "workspace_id": "ws-1",
            })).expect("start_workflow ok");
            set_spawn_program_override_for_test(None);
            set_disable_workflow_detector_for_test(false);
            let run_id = resp["run_id"].as_str().unwrap().to_string();

            let captured = take_captured_workflow_meta_for_test();
            // One capture per role, all carrying Some((this run_id, the role)).
            let mut by_role: std::collections::BTreeMap<String, String> = Default::default();
            for (_uid, meta) in &captured {
                let (rid, role) = meta.as_ref().expect(
                    "every workflow participant MUST carry Some(WorkflowMeta) — \
                     None means CM_WORKFLOW_RUN_ID/CM_ROLE never reach the MCP \
                     server and workflow_transition/workflow_done hard-fail",
                );
                assert_eq!(rid, &run_id, "meta run_id must match the launched run");
                by_role.insert(role.clone(), rid.clone());
            }
            assert_eq!(
                by_role.keys().cloned().collect::<Vec<_>>(),
                vec!["manager".to_string(), "reviewer".to_string(), "worker".to_string()],
                "all three roles threaded their identity into the MCP config writer",
            );
        });
    }

    /// Engine choice ("new claude" vs "new codex"): a `role_engines` override
    /// redirects a FRESH-spawned role's engine away from its TOML default; a role
    /// with no override entry keeps its TOML `engine`. Asserted on each spawned
    /// participant's recorded `session_type`.
    #[test]
    fn start_workflow_role_engines_overrides_spawned_engine() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Transition, TriggerOn, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt-engines");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                // Both roles declare claude-code in the TOML.
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                roles.insert("reviewer".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: Some("review".to_string()), subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("review".to_string(), Workflow {
                    name: "review".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into(), "reviewer".into()],
                    transitions: vec![
                        Transition { from: "worker".into(), on: TriggerOn::Idle, to: "reviewer".into() },
                        Transition { from: "reviewer".into(), on: TriggerOn::Idle, to: "worker".into() },
                    ],
                });
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            // worker → codex override; reviewer → no entry (keeps TOML claude-code).
            let resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "review",
                "goal": "do it",
                "worktree": wt.to_str().unwrap(),
                "workspace_id": "ws-1",
                "role_engines": { "worker": "codex" },
            })).expect("start_workflow ok");
            set_spawn_program_override_for_test(None);
            set_disable_workflow_detector_for_test(false);
            let _run_id = resp["run_id"].as_str().unwrap().to_string();

            // Each fresh-spawned participant is recorded in `s.sessions` carrying
            // its `workflow_role` and the `session_type` it was spawned with.
            let s = state.lock().unwrap();
            let mut by_role: BTreeMap<String, String> = Default::default();
            for sess in s.sessions.values() {
                if let Some(role) = &sess.workflow_role {
                    by_role.insert(role.clone(), sess.session_type.clone());
                }
            }
            assert_eq!(
                by_role.get("worker").map(String::as_str),
                Some("codex"),
                "role_engines override must redirect the worker's spawned engine to codex",
            );
            assert_eq!(
                by_role.get("reviewer").map(String::as_str),
                Some("claude-code"),
                "a role with no override entry keeps its TOML-declared engine",
            );
        });
    }

    /// Phase 4 (P4 scope): a Session caller is CONFINED to its own session's
    /// workspace — a client-supplied `worktree`/`workspace_id` override is
    /// ignored, so an agent can't launch participants in an arbitrary tree.
    #[test]
    fn start_workflow_session_caller_confined_to_own_workspace() {
        use crate::session::{DaemonSession, SpawnParams};
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("caller-wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-own".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-own".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
                // A caller session bound to ws-own.
                let mut sp = SpawnParams::new("ts-caller", "caller", "/bin/sleep");
                sp.args = vec!["120".to_string()];
                sp.workspace_id = "ws-own".to_string();
                s.sessions.insert("ts-caller".to_string(), DaemonSession::spawn(sp).expect("spawn"));
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            // The agent tries to override worktree/workspace — must be ignored.
            let resp = start_workflow(
                &state,
                &Caller::session("ts-caller"),
                &json!({ "workflow_name": "solo", "worktree": "/evil/path", "workspace_id": "evil-ws" }),
            ).expect("ok");
            set_spawn_program_override_for_test(None);
            set_disable_workflow_detector_for_test(false);
            let run_id = resp["run_id"].as_str().unwrap().to_string();
            let run = crate::workflow::run::load_one(&run_id).unwrap();
            assert_eq!(run.task_key, "ws-own", "confined to caller's workspace, not the override");
        });
    }

    /// Phase 4 (P-B): a Session caller launching a workflow on a DESCENDANT
    /// task that has its own (branch-mode) worktree spawns participants THERE,
    /// not in the caller's worktree (resolved via `task_workspaces`).
    #[test]
    fn start_workflow_descendant_task_spawns_in_its_own_worktree() {
        use crate::session::{DaemonSession, SpawnParams};
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt_parent = home.join("parent-wt");
            let wt_child = home.join("child-wt");
            std::fs::create_dir_all(&wt_parent).unwrap();
            std::fs::create_dir_all(&wt_child).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                for (id, wt) in [("ws-parent", &wt_parent), ("ws-child", &wt_child)] {
                    let mut ws = crate::manifest::ManifestWorkspace::default();
                    ws.id = id.to_string();
                    ws.worktree_path = Some(wt.clone());
                    s.workspaces.insert(id.to_string(), ws);
                }
                s.task_tree.insert("task-parent".to_string(), None);
                s.task_tree.insert("task-child".to_string(), Some("task-parent".to_string()));
                s.task_workspaces.insert("task-child".to_string(), "ws-child".to_string());
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
                // Caller bound to the PARENT task/workspace.
                let mut sp = SpawnParams::new("ts-caller", "caller", "/bin/sleep");
                sp.args = vec!["120".to_string()];
                sp.workspace_id = "ws-parent".to_string();
                sp.task_id = Some("task-parent".to_string());
                s.sessions.insert("ts-caller".to_string(), DaemonSession::spawn(sp).expect("spawn"));
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let resp = start_workflow(&state, &Caller::session("ts-caller"), &json!({
                "workflow_name": "solo", "task_id": "task-child",
            })).expect("ok");
            set_disable_workflow_detector_for_test(false);
            set_spawn_program_override_for_test(None);

            let run = crate::workflow::run::load_one(resp["run_id"].as_str().unwrap()).unwrap();
            assert_eq!(run.task_key, "ws-child", "descendant task's own workspace, not the parent's");
            assert_eq!(run.task_id.as_deref(), Some("task-child"));
        });
    }

    /// Cross-bind fix: Claude participants must arm NO spawn-time transcript
    /// detector. A Claude agent writes no transcript until its first prompt,
    /// so a spawn-time detector window is pure timing roulette — every idle
    /// role's detector races for the active role's first transcript (observed
    /// on cm-manager: the worker's transcript bound to the idle manager,
    /// wedging the run). Their sids bind causally at activation delivery via
    /// the finalize drainer instead. Deterministic pin: with the FAILING
    /// detector hook installed, any arming attempt would fail the launch
    /// closed — so a successful two-Claude-role launch proves no detector was
    /// armed for either role.
    #[test]
    fn start_workflow_claude_roles_arm_no_spawn_detectors() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                roles.insert("manager".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: Some("m".into()), subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("two".to_string(), Workflow {
                    name: "two".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into(), "manager".into()], transitions: vec![],
                });
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            // Any detector-arming attempt fails the launch closed under this
            // hook — success below therefore proves Claude roles arm nothing.
            set_failing_detector_for_test(true);

            let result = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "two", "goal": "g", "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
            }));

            set_failing_detector_for_test(false);
            set_spawn_program_override_for_test(None);

            let resp = result.expect(
                "two-Claude-role launch must succeed with the failing-detector \
                 hook installed: Claude participants arm no spawn-time detector",
            );
            let run = crate::workflow::run::load_one(resp["run_id"].as_str().unwrap()).unwrap();
            // Both roles spawned, neither sid bound yet — discovery happens at
            // activation delivery, not at spawn.
            for role in ["worker", "manager"] {
                assert!(run.role_sessions[role].daemon_session_uid.is_some());
                assert!(run.role_sessions[role].current_session_id.is_none());
            }
        });
    }

    /// P-B (timeout branch): when the spawn-queue wait TIMES OUT, the role's
    /// detector is still armed UNSERIALIZED (ticket = None) — it must NOT be
    /// skipped. The old `if let (Some(engine), Some(ticket))` guard skipped the
    /// detector entirely on a None ticket, leaving the participant with no
    /// `transcript_path` so `sync_role_session_ids` could never bind its sid and
    /// the run wedged after returning a run_id. Codex role (the only engine
    /// that still arms a spawn-time detector post-cross-bind-fix). Here we
    /// pre-occupy the worktree queue (never released) so the worker's wait
    /// times out, then prove arming was still ATTEMPTED via the failing
    /// detector hook: fail-closed Err means `spawn_queued_detector` was
    /// reached on the timeout path. Mutation: restoring the `Some(ticket)`
    /// guard skips arming → the launch would succeed → this fails.
    #[test]
    fn start_workflow_timeout_still_arms_detector_unserialized() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::Codex, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
            }
            // Pre-occupy the worktree spawn queue with a seq that is NEVER
            // released, so the worker's `wait_for_turn_timeout` is guaranteed to
            // time out (→ ticket None → unserialized arm path).
            let queue = workflow_spawn_queue(&state, &wt);
            let _blocking_seq = queue.enqueue(); // held forever, never signal_done

            let _wait_guard = set_slot_wait_timeout_for_test(std::time::Duration::from_millis(60));
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_failing_detector_for_test(true);

            let result = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "goal": "g", "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
            }));

            set_failing_detector_for_test(false);
            set_spawn_program_override_for_test(None);

            // Fail-closed Err == spawn_queued_detector was REACHED on the
            // timeout path (arming attempted, unserialized). A skip (the old
            // Some(ticket) guard) would have returned Ok with no detector.
            let (_code, msg) = result.expect_err(
                "P-B: detector arming must still be attempted after a \
                 spawn-queue timeout (old guard skipped it → would wedge)",
            );
            assert!(
                msg.contains("transcript detector spawn failed"),
                "error must name the detector spawn failure: {}",
                msg,
            );
        });
    }

    /// P-B (fail-closed branch): if the detector THREAD fails to spawn,
    /// `start_workflow` must FAIL CLOSED — return an error AND clean up the
    /// sessions spawned so far — rather than returning success with a
    /// participant that has no detector (which wedges headlessly). Mirrors
    /// `mcp_start_session`'s fail-closed contract.
    #[test]
    fn start_workflow_fails_closed_on_detector_thread_spawn_failure() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                // Codex: the only engine that still arms a spawn-time detector
                // (its rollout exists from boot). Claude roles bind at
                // activation via the drainer and never reach this path.
                roles.insert("worker".to_string(), Role { engine: Engine::Codex, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
            }
            let sessions_before = { state.lock().unwrap().sessions.len() };
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_failing_detector_for_test(true);

            let result = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "goal": "g", "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
            }));

            set_failing_detector_for_test(false);
            set_spawn_program_override_for_test(None);

            assert!(result.is_err(), "must FAIL CLOSED on detector spawn failure");
            let (_code, msg) = result.unwrap_err();
            assert!(
                msg.contains("transcript detector spawn failed"),
                "error must name the cause: {}",
                msg,
            );
            // Cleanup: the spawned worker session must be removed (no orphan).
            let sessions_after = { state.lock().unwrap().sessions.len() };
            assert_eq!(
                sessions_after, sessions_before,
                "spawned sessions must be cleaned up on fail-closed (no orphans)",
            );
            // P-3b atomicity: a FAILED launch must leave NO run on disk — the run
            // is saved only AFTER all participants spawn. This is what makes the
            // generous client RPC timeout safe: a client give-up can never
            // correspond to a half-launched, persisted run.
            assert!(
                crate::workflow::run::load_all().is_empty(),
                "fail-closed launch must persist no run (atomic save-at-end)",
            );
        });
    }

    // ───────────────── Existing-session binding (Phase 1) ─────────────────

    /// Write a Claude JSONL transcript where `claude_transcript_path` derives it
    /// for `(wt, sid)`, returning the absolute path string for use as a live
    /// session's `transcript_path`.
    fn bind_write_claude_transcript(wt: &std::path::Path, sid: &str, body: &str) -> String {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
        let enc = wt.to_str().unwrap().replace('/', "-").replace('.', "-");
        let dir = home.join(format!(".claude/projects/{}", enc));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.jsonl", sid));
        std::fs::write(&path, body).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// Spawn a live `/bin/sleep` daemon session for binding tests with the given
    /// engine/workspace and `transcript_path` (None = not-yet-resolvable).
    fn bind_spawn_session(
        s: &mut DaemonState,
        uid: &str,
        session_type: &str,
        workspace_id: &str,
        transcript_path: Option<String>,
    ) {
        use crate::session::{DaemonSession, SpawnParams};
        let mut sp = SpawnParams::new(uid, uid, "/bin/sleep");
        sp.args = vec!["120".to_string()];
        sp.session_type = session_type.to_string();
        sp.workspace_id = workspace_id.to_string();
        sp.transcript_path = transcript_path;
        s.sessions
            .insert(uid.to_string(), DaemonSession::spawn(sp).expect("spawn"));
    }

    /// Acceptance #1 (regression): with NO `role_sessions`, every role is
    /// fresh-spawned exactly as before — bound to a freshly-minted daemon uid,
    /// `current_session_id` discovered later (None at launch), no plans seeded,
    /// the initial entry's `text_messages_at_start` 0. Pins "absent →
    /// byte-identical behavior to today".
    #[test]
    fn start_workflow_absent_role_sessions_fresh_spawns_unchanged() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "goal": "g",
                "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
            })).expect("ok");
            set_disable_workflow_detector_for_test(false);
            set_spawn_program_override_for_test(None);

            let run = crate::workflow::run::load_one(resp["run_id"].as_str().unwrap()).unwrap();
            // Fresh-spawn binding: a NEW minted uid, sid discovered later (None).
            let b = &run.role_sessions["worker"];
            assert!(b.daemon_session_uid.is_some(), "fresh worker bound to a minted uid");
            assert!(b.current_session_id.is_none(), "fresh worker sid discovered later");
            assert!(!b.bound, "fresh spawn leaves the bound flag false");
            // No bind-only state seeded.
            assert!(run.role_plans.is_empty(), "no plans without role_sessions");
            assert_eq!(run.history[0].text_messages_at_start, 0, "no bound text baseline");
            assert_eq!(run.role_baselines["worker"].assistant_count, 0, "fresh baseline 0/0");
        });
    }

    /// Acceptance #2: binding an eligible persistent/non-mcp role to a live
    /// session with an N-assistant-turn / M-text transcript that ends in an
    /// accepted `ExitPlanMode` records the binding (uid + eagerly-resolved sid),
    /// `MessageBaseline.assistant_count == N`, the initial entry's
    /// `text_messages_at_start == M`, and `role_plans[role]` — with NO fresh
    /// spawn for that role.
    #[test]
    fn start_workflow_binds_eligible_worker_with_baseline_text_and_plan() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            // N=3 assistant turns (2 text + 1 ExitPlanMode tool_use tail),
            // M=2 text-bearing. The plan is the LAST assistant line, so
            // `latest_plan` surfaces it.
            let body = [
                r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"explored the code"}]}}"##,
                r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"found the bug"}]}}"##,
                r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"ExitPlanMode","input":{"plan":"# Plan\n1. fix it"}}]}}"##,
            ].join("\n");
            let tp = bind_write_claude_transcript(&wt, "sid-worker-live", &body);
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
                bind_spawn_session(&mut s, "sess-worker", "claude-code", "ws-1", Some(tp));
            }
            let _ = take_spawn_snapshots_for_test(); // clear prior captures
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "goal": "do the thing",
                "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
                "role_sessions": { "worker": "sess-worker" },
            })).expect("eligible bind ok");
            set_disable_workflow_detector_for_test(false);
            set_spawn_program_override_for_test(None);

            let run = crate::workflow::run::load_one(resp["run_id"].as_str().unwrap()).unwrap();
            let b = &run.role_sessions["worker"];
            assert_eq!(b.daemon_session_uid.as_deref(), Some("sess-worker"), "bound to the existing uid");
            assert_eq!(b.current_session_id.as_deref(), Some("sid-worker-live"), "sid resolved eagerly");
            assert!(b.bound, "bind path records the durable bound flag");
            assert_eq!(run.role_baselines["worker"].assistant_count, 3, "assistant baseline == N (turns)");
            assert_eq!(run.history[0].text_messages_at_start, 2, "initial entry text baseline == M");
            assert_eq!(run.role_plans.get("worker").map(String::as_str), Some("# Plan\n1. fix it"), "accepted plan snapshotted");
            // No fresh spawn for the bound role: the only session is the bound
            // one, and the spawn loop recorded no pre-snapshot for the worker.
            assert_eq!(state.lock().unwrap().sessions.len(), 1, "no extra session minted for the bound role");
            assert!(take_spawn_snapshots_for_test().is_empty(), "bound role never enters the spawn/detector path");
            // The bound worker is the initial role: delivery-only initial activation.
            let pa = run.pending_activation.as_ref().expect("initial pending activation");
            assert!(pa.is_initial && pa.target_role == "worker");
        });
    }

    /// Regression (existing-session bind → TUI manifest sync): a successful bind
    /// announces the bound session's new workflow tags to live `manifest.watch`
    /// subscribers via a `ManifestDiff::Updated`, so the TUI re-groups the
    /// pre-existing row under the workflow header. Fresh-spawned participants get
    /// this from `start_session`'s `Added`; a bound row already exists, so it
    /// needs an in-place `Updated`. Before the fix the tag was set in
    /// `state.sessions` only and never broadcast — the bound worker rendered
    /// OUTSIDE its own workflow group.
    #[test]
    fn start_workflow_bind_broadcasts_manifest_update_for_bound_session() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let tp = bind_write_claude_transcript(
                &wt, "sid-worker-live",
                r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"##,
            );
            let state = make_state_arc();
            // Subscribe BEFORE the launch so the bind broadcast is captured.
            let (rx, _guard) = {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
                bind_spawn_session(&mut s, "sess-worker", "claude-code", "ws-1", Some(tp));
                s.manifest_watcher.subscribe()
            };
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "goal": "g",
                "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
                "role_sessions": { "worker": "sess-worker" },
            })).expect("eligible bind ok");
            set_disable_workflow_detector_for_test(false);
            set_spawn_program_override_for_test(None);
            let run_id = resp["run_id"].as_str().unwrap().to_string();

            // The bound session's new tags must reach manifest.watch as Updated.
            let mut saw = false;
            while let Ok(diff) = rx.try_recv() {
                if let crate::manifest::ManifestDiff::Updated { uid, entry } = diff {
                    if uid == "sess-worker" {
                        assert_eq!(entry["workflow_run_id"].as_str(), Some(run_id.as_str()), "diff carries the run id");
                        assert_eq!(entry["workflow_role"].as_str(), Some("worker"), "diff carries the role");
                        saw = true;
                    }
                }
            }
            assert!(saw, "bound session tag change must broadcast a manifest Updated diff");
        });
    }

    /// Acceptance #3: a bound session whose sid is NOT resolvable (no transcript
    /// yet) is REJECTED with `InvalidParams` — it must NOT enter the run with
    /// `current_session_id = None`. No run is persisted.
    #[test]
    fn start_workflow_bind_rejects_unresolvable_sid() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
                // Live session but NO transcript_path → sid unresolvable.
                bind_spawn_session(&mut s, "sess-worker", "claude-code", "ws-1", None);
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let result = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "goal": "g",
                "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
                "role_sessions": { "worker": "sess-worker" },
            }));
            set_disable_workflow_detector_for_test(false);
            set_spawn_program_override_for_test(None);

            let (code, msg) = result.expect_err("unresolvable sid must be rejected");
            assert_eq!(code, ErrorCode::InvalidParams);
            assert!(msg.contains("no resolvable transcript"), "reason named: {msg}");
            assert!(crate::workflow::run::load_all().is_empty(), "no run persisted on rejection");
        });
    }

    /// Acceptance #4: each eligibility rejection returns `InvalidParams` /
    /// `Unauthorized` naming the reason — fresh role, needs_mcp role, unknown
    /// role, non-existent uid, other-workspace uid, engine mismatch, a uid in
    /// another active run, and a duplicate uid across two roles.
    #[test]
    fn start_workflow_bind_eligibility_rejections() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            let wt_other = home.join("wt-other");
            std::fs::create_dir_all(&wt).unwrap();
            std::fs::create_dir_all(&wt_other).unwrap();
            let tp = bind_write_claude_transcript(
                &wt, "sid-worker-live",
                r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"##,
            );
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                for (id, w) in [("ws-1", &wt), ("ws-other", &wt_other)] {
                    let mut ws = crate::manifest::ManifestWorkspace::default();
                    ws.id = id.to_string();
                    ws.worktree_path = Some(w.clone());
                    s.workspaces.insert(id.to_string(), ws);
                }
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                roles.insert("worker2".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: Some("w2".into()), subsequent_activation_prompt: None, needs_mcp: false });
                roles.insert("reviewer".to_string(), Role { engine: Engine::Codex, context: Context::Fresh, activation_prompt: Some("r".into()), subsequent_activation_prompt: None, needs_mcp: false });
                roles.insert("manager".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: Some("m".into()), subsequent_activation_prompt: None, needs_mcp: true });
                s.base_workflow_definitions.insert("multi".to_string(), Workflow {
                    name: "multi".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into(), "worker2".into(), "reviewer".into(), "manager".into()],
                    transitions: vec![],
                });
                bind_spawn_session(&mut s, "sess-worker", "claude-code", "ws-1", Some(tp));
                bind_spawn_session(&mut s, "sess-codex", "codex", "ws-1", None);
                bind_spawn_session(&mut s, "sess-other-ws", "claude-code", "ws-other", None);
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);

            let call = |rs: serde_json::Value| {
                start_workflow(&state, &Caller::operator("op"), &json!({
                    "workflow_name": "multi", "goal": "g",
                    "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
                    "role_sessions": rs,
                }))
            };
            let reject = |rs: serde_json::Value, want_code: ErrorCode, needle: &str| {
                let (code, msg) = call(rs).expect_err(&format!("must reject ({needle})"));
                assert_eq!(code, want_code, "code for {needle}: got msg {msg}");
                assert!(msg.to_lowercase().contains(needle), "reason for {needle} not named: {msg}");
            };

            reject(json!({ "ghostrole": "sess-worker" }), ErrorCode::InvalidParams, "not in workflow");
            reject(json!({ "reviewer": "sess-worker" }), ErrorCode::InvalidParams, "fresh");
            reject(json!({ "manager": "sess-worker" }), ErrorCode::InvalidParams, "needs_mcp");
            reject(json!({ "worker": "ghost-uid" }), ErrorCode::InvalidParams, "not a live daemon session");
            reject(json!({ "worker": "sess-other-ws" }), ErrorCode::InvalidParams, "workspace");
            reject(json!({ "worker": "sess-codex" }), ErrorCode::InvalidParams, "engine");
            // Duplicate uid across two roles (worker + worker2 both → sess-worker).
            reject(json!({ "worker": "sess-worker", "worker2": "sess-worker" }), ErrorCode::InvalidParams, "two roles");

            // uid already a participant of another ACTIVE run.
            {
                use crate::workflow::run::{MessageBaseline, RoleBinding, WorkflowRun};
                let mut rs = BTreeMap::new();
                rs.insert("worker".to_string(), RoleBinding {
                    session_label: "worker".to_string(),
                    current_session_id: Some("sid-worker-live".to_string()),
                    daemon_session_uid: Some("sess-worker".to_string()),
                    bound: false,
                });
                let other = WorkflowRun::new(
                    "wf-other-active".to_string(), "multi".to_string(), "ws-1".to_string(),
                    rs, "worker".to_string(), BTreeMap::new(), None, BTreeMap::new(), 0,
                );
                crate::workflow::run::save(&other).unwrap();
            }
            reject(json!({ "worker": "sess-worker" }), ErrorCode::InvalidParams, "active run");

            set_disable_workflow_detector_for_test(false);
            set_spawn_program_override_for_test(None);
            // Only the seeded conflicting run exists; no rejection persisted a run.
            assert_eq!(crate::workflow::run::load_all().len(), 1, "rejections persist no new run");
        });
    }

    /// Acceptance #4 (Unauthorized branch): a Session caller binding a uid that
    /// is in the run's workspace but OUT OF its descendant/task scope is rejected
    /// with `Unauthorized` (the `check_session_caller` gate), distinct from the
    /// InvalidParams workspace-mismatch branch.
    #[test]
    fn start_workflow_bind_rejects_out_of_scope_caller() {
        use crate::session::{DaemonSession, SpawnParams};
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let tp = bind_write_claude_transcript(
                &wt, "sid-target",
                r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"##,
            );
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                // Sibling tasks: task-B is NOT a descendant of the caller's task-A.
                s.task_tree.insert("task-A".to_string(), None);
                s.task_tree.insert("task-B".to_string(), None);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
                // Caller bound to task-A; target session in the SAME workspace
                // but under the unrelated task-B (out of the caller's scope).
                let mut sp = SpawnParams::new("ts-caller", "caller", "/bin/sleep");
                sp.args = vec!["120".to_string()];
                sp.workspace_id = "ws-1".to_string();
                sp.task_id = Some("task-A".to_string());
                s.sessions.insert("ts-caller".to_string(), DaemonSession::spawn(sp).expect("spawn"));
                let mut spt = SpawnParams::new("sess-target", "target", "/bin/sleep");
                spt.args = vec!["120".to_string()];
                spt.session_type = "claude-code".to_string();
                spt.workspace_id = "ws-1".to_string();
                spt.task_id = Some("task-B".to_string());
                spt.transcript_path = Some(tp);
                s.sessions.insert("sess-target".to_string(), DaemonSession::spawn(spt).expect("spawn"));
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let result = start_workflow(&state, &Caller::session("ts-caller"), &json!({
                "workflow_name": "solo", "goal": "g",
                "role_sessions": { "worker": "sess-target" },
            }));
            set_disable_workflow_detector_for_test(false);
            set_spawn_program_override_for_test(None);

            let (code, msg) = result.expect_err("out-of-scope bind must be rejected");
            assert_eq!(code, ErrorCode::Unauthorized);
            assert!(msg.to_lowercase().contains("not authorized"), "reason named: {msg}");
            assert!(crate::workflow::run::load_all().is_empty(), "no run persisted");
        });
    }

    /// Acceptance #6: a launch that fails AFTER bind eligibility but BEFORE save
    /// leaves the bound session's `workflow_run_id`/`workflow_role` tags
    /// unchanged (tag-after-save). The bound worker passes eligibility; the
    /// fresh Codex reviewer's detector arm then fails the launch closed.
    #[test]
    fn start_workflow_bind_tags_only_after_save() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Transition, TriggerOn, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let tp = bind_write_claude_transcript(
                &wt, "sid-worker-live",
                r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"##,
            );
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                roles.insert("reviewer".to_string(), Role { engine: Engine::Codex, context: Context::Fresh, activation_prompt: Some("r".into()), subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("feedback".to_string(), Workflow {
                    name: "feedback".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into(), "reviewer".into()],
                    transitions: vec![Transition { from: "worker".into(), on: TriggerOn::Idle, to: "reviewer".into() }],
                });
                bind_spawn_session(&mut s, "sess-worker", "claude-code", "ws-1", Some(tp));
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            // The fresh Codex reviewer arms a detector; make it fail so the
            // launch fails AFTER the worker bind passed eligibility.
            set_failing_detector_for_test(true);
            let result = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "feedback", "goal": "g",
                "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
                "role_sessions": { "worker": "sess-worker" },
            }));
            set_failing_detector_for_test(false);
            set_spawn_program_override_for_test(None);

            assert!(result.is_err(), "launch must fail closed on detector failure");
            // tag-after-save: the bound session keeps its prior (untagged) state.
            let s = state.lock().unwrap();
            let sess = s.sessions.get("sess-worker").expect("bound session still live");
            assert!(sess.workflow_run_id.is_none(), "no orphan run tag on the bound session");
            assert!(sess.workflow_role.is_none(), "no orphan role tag on the bound session");
            assert!(crate::workflow::run::load_all().is_empty(), "no run persisted");
        });
    }

    /// P-3a: the directly-tested resolver — a "claude-code" wire type with
    /// `CM_SESSION_MEM_SOFT_CLAUDE=6G` (+ matching hard) must resolve a non-None
    /// cap with the correct suffix-parsed byte value. This is the assertion that
    /// would have caught both P-3a bugs (wrong env-var key + non-suffix parse).
    #[test]
    fn resolve_configured_cap_normalizes_type_and_parses_suffix() {
        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp.path().join("app.slice");
        std::fs::create_dir_all(&prefix).unwrap();
        std::env::set_var("CM_SESSION_MEM_SOFT_CLAUDE", "6G");
        std::env::set_var("CM_SESSION_MEM_HARD_CLAUDE", "8G");
        set_configured_cap_prefix_override_for_test(Some(prefix.to_string_lossy().into_owned()));

        // Wire type "claude-code" → key "claude" → CM_SESSION_MEM_*_CLAUDE.
        let cap = resolve_configured_participant_cap("claude-code");

        set_configured_cap_prefix_override_for_test(None);
        std::env::remove_var("CM_SESSION_MEM_SOFT_CLAUDE");
        std::env::remove_var("CM_SESSION_MEM_HARD_CLAUDE");

        assert_eq!(
            cap,
            Some((
                6u64 * 1024 * 1024 * 1024,
                8u64 * 1024 * 1024 * 1024,
                prefix.to_string_lossy().into_owned(),
            )),
            "claude-code must look up _CLAUDE and parse 6G/8G suffixes",
        );
        // bash is never capped.
        assert_eq!(resolve_configured_participant_cap("bash"), None);
    }

    #[test]
    fn parse_cap_bytes_handles_suffixes_and_plain() {
        assert_eq!(parse_cap_bytes("6G"), Some(6u64 * 1024 * 1024 * 1024));
        assert_eq!(parse_cap_bytes("512M"), Some(512u64 * 1024 * 1024));
        assert_eq!(parse_cap_bytes("1024K"), Some(1024u64 * 1024));
        assert_eq!(parse_cap_bytes("67108864"), Some(67108864));
        assert_eq!(parse_cap_bytes("6g"), Some(6u64 * 1024 * 1024 * 1024));
        assert_eq!(parse_cap_bytes("garbage"), None);
        assert_eq!(parse_cap_bytes(""), None);
    }

    /// P-4: when the initial role has NO activation_prompt AND the goal is
    /// empty, the initial activation must NOT be queued — otherwise finalize
    /// would `unwrap_or_default()` + press Enter, submitting a blank turn to the
    /// fresh worker. The run is still created and active; the user drives it.
    #[test]
    fn start_workflow_empty_prompt_and_goal_skips_initial_activation() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                // No activation_prompt on the initial role.
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            // No "goal" key → empty goal.
            let resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
            })).expect("ok");
            set_spawn_program_override_for_test(None);
            set_disable_workflow_detector_for_test(false);

            let run = crate::workflow::run::load_one(resp["run_id"].as_str().unwrap()).unwrap();
            assert!(
                run.pending_activation.is_none(),
                "P-4: blank prompt + empty goal must NOT queue an initial activation (no blank turn)",
            );
            // Run is still created + active — only the auto-delivery is skipped.
            assert_eq!(run.active_role.as_deref(), Some("worker"));
        });
    }

    /// P-4 companion: a non-empty goal (feedback-mode shape) STILL queues the
    /// initial activation — the skip is narrow to the blank case.
    #[test]
    fn start_workflow_nonempty_goal_still_queues_initial_activation() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "goal": "do the thing", "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
            })).expect("ok");
            set_spawn_program_override_for_test(None);
            set_disable_workflow_detector_for_test(false);

            let run = crate::workflow::run::load_one(resp["run_id"].as_str().unwrap()).unwrap();
            let pa = run.pending_activation.as_ref().expect("non-empty goal queues activation");
            assert!(pa.is_initial);
            assert_eq!(pa.raw_prompt, "do the thing");
        });
    }

    /// P-4 edge (must-fix #2): a WHITESPACE-only `activation_prompt` must be
    /// treated as absent, so with a real goal the worker still receives the
    /// GOAL (verbatim) — NOT a blank template, and NOT a silent no-delivery.
    /// Before the trim-filter, `activation_prompt = "   "` counted as present →
    /// raw_prompt = "   " (non-empty) → queued a whitespace turn, OR if the goal
    /// was the intended ask it never reached the worker.
    #[test]
    fn start_workflow_whitespace_prompt_falls_back_to_goal() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                // Whitespace-only activation_prompt on the initial role.
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: Some("   ".to_string()), subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "goal": "real goal", "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
            })).expect("ok");
            set_spawn_program_override_for_test(None);
            set_disable_workflow_detector_for_test(false);

            let run = crate::workflow::run::load_one(resp["run_id"].as_str().unwrap()).unwrap();
            let pa = run
                .pending_activation
                .as_ref()
                .expect("whitespace prompt + real goal must still queue the GOAL");
            assert!(pa.verbatim, "whitespace prompt → treated as absent → goal delivered verbatim");
            assert_eq!(pa.raw_prompt, "real goal");
        });
    }

    /// P-4 edge: whitespace prompt AND empty goal → still skipped (no blank turn).
    #[test]
    fn start_workflow_whitespace_prompt_and_empty_goal_skips() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: Some("  \n ".to_string()), subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
            }
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
            })).expect("ok");
            set_spawn_program_override_for_test(None);
            set_disable_workflow_detector_for_test(false);

            let run = crate::workflow::run::load_one(resp["run_id"].as_str().unwrap()).unwrap();
            assert!(
                run.pending_activation.is_none(),
                "whitespace prompt + empty goal must skip (no blank turn)",
            );
        });
    }

    /// P-3 (parity): a Session caller that is itself memory-capped launches
    /// participants that INHERIT its cap — they must not run uncapped. Asserts
    /// (via the threading-capture seam) that the worker's spawn carried the
    /// caller's (soft, hard, cgroup_prefix) triple. Enforcement itself
    /// (systemd-run wrap + start_session's cgroup-scope verify) is start_session's
    /// existing, separately-tested job and needs real user-systemd — so this
    /// test verifies the daemon-side THREADING decision, which is the fix.
    #[test]
    fn start_workflow_session_caller_participants_inherit_cap() {
        use crate::session::{DaemonSession, SpawnParams};
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let cgroup = home.join("cg");
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-cap".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-cap".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
                // A capped caller session bound to ws-cap.
                let mut sp = SpawnParams::new("ts-capped-caller", "caller", "/bin/sleep");
                sp.args = vec!["120".to_string()];
                sp.workspace_id = "ws-cap".to_string();
                let mut ds = DaemonSession::spawn(sp).expect("spawn caller");
                ds.memory_cap_soft_bytes = Some(100 * 1024 * 1024);
                ds.memory_cap_hard_bytes = Some(200 * 1024 * 1024);
                ds.cgroup_prefix = Some(cgroup.clone());
                s.sessions.insert("ts-capped-caller".to_string(), ds);
            }
            let _ = take_captured_participant_caps_for_test(); // clear prior
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let _resp = start_workflow(&state, &Caller::session("ts-capped-caller"), &json!({
                "workflow_name": "solo", "goal": "g",
            })).expect("ok");
            set_spawn_program_override_for_test(None);
            set_disable_workflow_detector_for_test(false);

            let caps = take_captured_participant_caps_for_test();
            assert_eq!(caps.len(), 1, "one participant");
            let (_uid, cap) = &caps[0];
            assert_eq!(
                cap.as_ref(),
                Some(&(
                    100 * 1024 * 1024u64,
                    200 * 1024 * 1024u64,
                    cgroup.to_string_lossy().into_owned(),
                )),
                "P-3: participant must inherit the caller's (soft, hard, cgroup) cap",
            );
        });
    }

    /// P-3 (headless config path): an OPERATOR (headless) caller has no caller
    /// session to inherit from, so participants take the per-engine CONFIGURED
    /// cap (`CM_SESSION_MEM_SOFT_/HARD_<TYPE>` + computed cgroup prefix). With
    /// the cap env set and the cgroup prefix present, the participant's spawn
    /// carries the configured cap — this is the always-on-host case the phase
    /// targets. (Capture-seam assertion; enforcement is start_session's job.)
    #[test]
    fn start_workflow_operator_caller_participants_take_configured_cap() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            // A real temp dir standing in for the app.slice cgroup prefix.
            let fake_cgroup = home.join("app.slice");
            std::fs::create_dir_all(&fake_cgroup).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-op".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-op".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
            }
            // P-3a: the wire type "claude-code" must normalize to the internal
            // key "claude" → CM_SESSION_MEM_SOFT_CLAUDE (NOT _CLAUDE-CODE), and
            // the suffix-aware parser must accept "6G". (env_lock held by
            // with_temp_home.)
            std::env::set_var("CM_SESSION_MEM_SOFT_CLAUDE", "6G");
            std::env::set_var("CM_SESSION_MEM_HARD_CLAUDE", "8G");
            set_configured_cap_prefix_override_for_test(Some(fake_cgroup.to_string_lossy().into_owned()));
            let _ = take_captured_participant_caps_for_test();
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);

            let _resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "goal": "g", "worktree": wt.to_str().unwrap(), "workspace_id": "ws-op",
            })).expect("ok");

            set_spawn_program_override_for_test(None);
            set_disable_workflow_detector_for_test(false);
            set_configured_cap_prefix_override_for_test(None);
            std::env::remove_var("CM_SESSION_MEM_SOFT_CLAUDE");
            std::env::remove_var("CM_SESSION_MEM_HARD_CLAUDE");

            let caps = take_captured_participant_caps_for_test();
            assert_eq!(caps.len(), 1);
            let (_uid, cap) = &caps[0];
            assert_eq!(
                cap.as_ref(),
                Some(&(
                    6u64 * 1024 * 1024 * 1024,
                    8u64 * 1024 * 1024 * 1024,
                    fake_cgroup.to_string_lossy().into_owned(),
                )),
                "P-3a: 'claude-code' participant must resolve CM_SESSION_MEM_*_CLAUDE \
                 with suffix-parsed bytes (6G), not the bogus _CLAUDE-CODE / plain parse",
            );
        });
    }

    /// P-3: with NO cap configured (no caller session, no `CM_SESSION_MEM_*`
    /// env), an Operator-launched participant runs UNCAPPED — graceful, not a
    /// failed launch. Pins that the configured path is opt-in.
    #[test]
    fn start_workflow_operator_caller_participants_are_uncapped() {
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-op".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-op".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
            }
            // Hermetic: ensure no configured cap is in scope (env_lock held).
            std::env::remove_var("CM_SESSION_MEM_SOFT_CLAUDE");
            std::env::remove_var("CM_SESSION_MEM_HARD_CLAUDE");
            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "goal": "g", "worktree": wt.to_str().unwrap(), "workspace_id": "ws-op",
            })).expect("ok");
            set_spawn_program_override_for_test(None);
            set_disable_workflow_detector_for_test(false);

            let run = crate::workflow::run::load_one(resp["run_id"].as_str().unwrap()).unwrap();
            let worker_uid = run.role_sessions["worker"].daemon_session_uid.clone().unwrap();
            let s = state.lock().unwrap();
            let worker = s.sessions.get(&worker_uid).expect("worker session");
            assert!(
                worker.memory_cap_soft_bytes.is_none() && worker.cgroup_prefix.is_none(),
                "headless operator participants are uncapped (documented gap; \
                 needs a daemon.toml cap policy to change)",
            );
        });
    }

    /// Phase 4 (finding 1): a caller-supplied `run_id` is IGNORED — the daemon
    /// always allocates server-side, so a Session RPC can't reuse an active
    /// run's id and clobber its state.json.
    #[test]
    fn start_workflow_ignores_caller_run_id_and_never_clobbers() {
        use crate::workflow::run::{RoleBinding, WorkflowRun};
        use crate::workflow::toml_schema::{Context, Engine, Role, Workflow};
        use std::collections::BTreeMap;
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-1".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-1".to_string(), ws);
                let mut roles = BTreeMap::new();
                roles.insert("worker".to_string(), Role { engine: Engine::ClaudeCode, context: Context::Persistent, activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false });
                s.base_workflow_definitions.insert("solo".to_string(), Workflow {
                    name: "solo".to_string(), description: String::new(), roles,
                    role_order: vec!["worker".into()], transitions: vec![],
                });
            }
            // Pre-seed a victim run that must NOT be clobbered.
            let mut victim_roles = BTreeMap::new();
            victim_roles.insert("worker".to_string(), RoleBinding { session_label: "worker".into(), current_session_id: Some("victim-sid".into()), daemon_session_uid: None, bound: false });
            let victim = WorkflowRun::new("wf_victim".into(), "solo".into(), "ws-1".into(), victim_roles, "worker".into(), BTreeMap::new(), Some("victim goal".into()), BTreeMap::new(), 0);
            crate::workflow::run::save(&victim).unwrap();

            set_spawn_program_override_for_test(Some(("/bin/sleep".to_string(), vec!["120".to_string()])));
            set_disable_workflow_detector_for_test(true);
            let resp = start_workflow(&state, &Caller::operator("op"), &json!({
                "workflow_name": "solo", "worktree": wt.to_str().unwrap(), "workspace_id": "ws-1",
                "run_id": "wf_victim", "goal": "new run",
            })).expect("ok");
            set_spawn_program_override_for_test(None);
            set_disable_workflow_detector_for_test(false);

            let new_id = resp["run_id"].as_str().unwrap();
            assert_ne!(new_id, "wf_victim", "caller-supplied run_id ignored");
            // Victim untouched.
            let after = crate::workflow::run::load_one("wf_victim").unwrap();
            assert_eq!(after.goal.as_deref(), Some("victim goal"));
            assert_eq!(after.role_sessions["worker"].current_session_id.as_deref(), Some("victim-sid"));
        });
    }

    /// Malformed params surface as InvalidParams.
    #[test]
    fn workflow_update_definitions_malformed_params_rejected() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let err = workflow_update_definitions(
                &state,
                &json!({"not_workflows": "wrong shape"}),
            )
            .expect_err("malformed params must reject");
            assert_eq!(err.0, ErrorCode::InvalidParams);
        });
    }

    // ------------------------------------------------------------
    // 10d-2c-2-1: transcript::assistant_turn_completed_since
    // ------------------------------------------------------------
    //
    // The combined-gate helper isn't itself a daemon handler, but
    // pin its semantics here so the upcoming 2c-2-2 polling
    // driver can rely on the contract. The full count_messages
    // / role_turn_complete pieces have their own tests in
    // `daemon/src/workflow/transcript.rs::tests` (10d-2a era);
    // these new tests focus on the COMBINATION.

    #[test]
    fn assistant_turn_completed_since_requires_both_count_advance_and_idle() {
        use crate::workflow::toml_schema::Engine;
        let _tmp = with_temp_home(|| {
            // Write a transcript with one COMPLETE assistant
            // turn (stop_reason: end_turn). baseline = 0 should
            // fire; baseline = 1 should NOT fire (count == baseline).
            let home =
                std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let wt = std::path::PathBuf::from("/tmp/r2c1-wt");
            let encoded = "-tmp-r2c1-wt";
            let proj = home.join(format!(".claude/projects/{}", encoded));
            std::fs::create_dir_all(&proj).unwrap();
            let complete = r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"##;
            std::fs::write(proj.join("sid-c1.jsonl"), complete).unwrap();

            assert!(
                crate::workflow::transcript::assistant_turn_completed_since(
                    &Engine::ClaudeCode,
                    &wt,
                    "sid-c1",
                    0,
                ),
                "count=1 > baseline=0 AND idle → fires",
            );
            assert!(
                !crate::workflow::transcript::assistant_turn_completed_since(
                    &Engine::ClaudeCode,
                    &wt,
                    "sid-c1",
                    1,
                ),
                "count=1 == baseline=1 → no advance → doesn't fire",
            );
        });
    }

    #[test]
    fn assistant_turn_completed_since_blocks_on_mid_stream_pending_tool_use() {
        use crate::workflow::toml_schema::Engine;
        let _tmp = with_temp_home(|| {
            // Transcript with one assistant turn whose
            // stop_reason is tool_use (mid-stream, not complete).
            // count advances past baseline=0 BUT role_turn_complete
            // returns false → gate stays shut. Pre-r2c2-2's
            // upcoming on_idle driver MUST not fire here.
            let home =
                std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let wt = std::path::PathBuf::from("/tmp/r2c1-mid");
            let encoded = "-tmp-r2c1-mid";
            let proj = home.join(format!(".claude/projects/{}", encoded));
            std::fs::create_dir_all(&proj).unwrap();
            let pending = r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"x","input":{}}]}}"##;
            std::fs::write(proj.join("sid-mid.jsonl"), pending).unwrap();

            assert!(
                !crate::workflow::transcript::assistant_turn_completed_since(
                    &Engine::ClaudeCode,
                    &wt,
                    "sid-mid",
                    0,
                ),
                "count=1 > baseline=0 BUT not idle (tool_use \
                 pending) → gate must stay shut",
            );
        });
    }

    // ============================================================
    // 10d-2c-2-2-b round-4 F1 + F2 tests
    // ============================================================

    /// F1 — Session caller's `trigger: "static_idle"` param is
    /// IGNORED. Pre-fix the event's `args.trigger` field would
    /// have been populated from a Session-caller's input,
    /// letting an MCP agent forge a static-idle history entry
    /// (dropping prompt/event_id audit fields).
    #[test]
    fn workflow_transition_session_caller_cannot_forge_static_idle_trigger() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            // Seed run + insert a daemon session for the worker
            // role so the Session-caller auth check passes
            // (caller's bound role == active_role).
            seed_workflow_run("wf_f1_forge_attempt", "worker");
            {
                let mut s = state.lock().unwrap();
                let mut sp = crate::session::SpawnParams::new(
                    "ts-forger",
                    "worker",
                    "/bin/sleep",
                );
                sp.args = vec!["60".to_string()];
                sp.workspace_id = "/tmp/seed-task-key".to_string();
                sp.workflow_run_id = Some("wf_f1_forge_attempt".to_string());
                sp.workflow_role = Some("worker".to_string());
                let ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
                s.sessions.insert("ts-forger".to_string(), ds);
            }
            let caller = Caller::session("ts-forger");
            // Session caller attempts to forge static_idle.
            let params = json!({
                "to": "reviewer",
                "prompt": "p",
                "run_id": "wf_f1_forge_attempt",
                "role": "worker",
                "trigger": "static_idle",
            });
            workflow_transition(&state, &caller, &params)
                .expect("transition ok (auth passes)");
            // Event's args.trigger MUST be absent — Session
            // caller's forgery attempt was filtered.
            let (events, _) =
                crate::workflow::events::read_new("wf_f1_forge_attempt", 0);
            assert_eq!(events.len(), 1);
            let trigger = events[0].args.get("trigger");
            assert!(
                trigger.is_none(),
                "Session caller's 'trigger' param must be filtered; \
                 got args.trigger = {:?}",
                trigger,
            );
        });
    }

    /// F1 companion — Operator caller's `trigger: "static_idle"`
    /// param IS honored. The daemon-poller path depends on this.
    #[test]
    fn workflow_transition_operator_caller_can_set_static_idle_trigger() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_f1_operator_trigger", "worker");
            let params = json!({
                "to": "reviewer",
                "prompt": "",
                "run_id": "wf_f1_operator_trigger",
                "role": "worker",
                "trigger": "static_idle",
            });
            workflow_transition(
                &state,
                &Caller::operator("daemon-poller"),
                &params,
            )
            .expect("operator transition ok");
            let (events, _) =
                crate::workflow::events::read_new("wf_f1_operator_trigger", 0);
            assert_eq!(events.len(), 1);
            let trigger = events[0]
                .args
                .get("trigger")
                .and_then(|v| v.as_str());
            assert_eq!(
                trigger,
                Some("static_idle"),
                "Operator caller's 'trigger' param must be honored \
                 so the TUI tail's history append uses \
                 TriggerKind::StaticIdle. Got args.trigger = {:?}",
                trigger,
            );
        });
    }

    /// F2 — Handler captures the outgoing role's last assistant
    /// message and stores it on the closing history entry. Pre-fix
    /// every daemon-routed transition (Session AND Operator
    /// callers) called `close_active_role(None)`, permanently
    /// losing `last_message` from the audit log.
    #[test]
    /// Review-round-5 F2 — capture works for daemon-owned
    /// sessions WITHOUT `workflow_run_id`/`workflow_role` tags
    /// (simulates `set_workflow_context` push that never
    /// landed). The new three-tier fallback uses the uid binding
    /// on `RoleBinding.daemon_session_uid` as the first lookup.
    /// Pre-fix this test would have failed because the
    /// tag-based lookup couldn't find the session.
    #[test]
    fn workflow_transition_captures_last_message_via_uid_without_tags() {
        let _tmp = with_temp_home(|| {
            let home =
                std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let wt = home.join("wt-rr5-f2");
            std::fs::create_dir_all(&wt).unwrap();
            let wt_str = wt.to_str().unwrap();
            let encoded = wt_str.replace('/', "-").replace('.', "-");
            let proj = home.join(format!(".claude/projects/{}", encoded));
            std::fs::create_dir_all(&proj).unwrap();
            std::fs::write(
                proj.join("sid-untagged.jsonl"),
                r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"captured via uid path"}]}}"##,
            )
            .unwrap();

            let state = make_state_arc();
            seed_workflow_run("wf_rr5_f2_uid", "worker");
            // Bind: worker → sid-untagged transcript + daemon
            // uid `ts-untagged-w`. NO workflow_run_id/role tags
            // set on the daemon session (simulates failed
            // set_workflow_context).
            crate::workflow::run::modify("wf_rr5_f2_uid", |r| {
                if let Some(b) = r.role_sessions.get_mut("worker") {
                    b.current_session_id = Some("sid-untagged".to_string());
                    b.daemon_session_uid =
                        Some("ts-untagged-w".to_string());
                }
            })
            .expect("bind worker");

            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-rr5-f2".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-rr5-f2".to_string(), ws);
                let mut sp = crate::session::SpawnParams::new(
                    "ts-untagged-w",
                    "worker",
                    "/bin/sleep",
                );
                sp.args = vec!["60".to_string()];
                sp.workspace_id = "ws-rr5-f2".to_string();
                // NB: NOT setting workflow_run_id/workflow_role.
                let ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
                assert!(
                    ds.workflow_run_id.is_none(),
                    "test precondition: daemon session has no tags",
                );
                s.sessions.insert("ts-untagged-w".to_string(), ds);
            }

            workflow_transition(
                &state,
                &Caller::operator("daemon-poller"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_rr5_f2_uid",
                    "role": "worker",
                }),
            )
            .expect("transition ok");

            let post = crate::workflow::run::load_one("wf_rr5_f2_uid")
                .expect("load post");
            let worker_entry = post
                .history
                .iter()
                .find(|h| h.role == "worker")
                .expect("worker history");
            assert_eq!(
                worker_entry.last_message.as_deref(),
                Some("captured via uid path"),
                "uid-first fallback must capture last_message even \
                 without workflow_run_id/role tags. Got: {:?}",
                worker_entry.last_message,
            );
        });
    }

    fn workflow_transition_captures_outgoing_last_message_on_history() {
        let _tmp = with_temp_home(|| {
            let home =
                std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let wt = home.join("wt-f2");
            std::fs::create_dir_all(&wt).unwrap();
            let wt_str = wt.to_str().unwrap();
            let encoded = wt_str.replace('/', "-").replace('.', "-");
            let proj = home.join(format!(".claude/projects/{}", encoded));
            std::fs::create_dir_all(&proj).unwrap();
            std::fs::write(
                proj.join("sid-worker-f2.jsonl"),
                r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"the captured last message"}]}}"##,
            )
            .unwrap();

            let state = make_state_arc();
            seed_workflow_run("wf_f2_last_msg", "worker");
            // Bind worker's transcript_id so the daemon-side
            // capture helper can find it.
            crate::workflow::run::modify("wf_f2_last_msg", |r| {
                if let Some(b) = r.role_sessions.get_mut("worker") {
                    b.current_session_id =
                        Some("sid-worker-f2".to_string());
                }
            })
            .expect("bind worker sid");
            // Daemon session with workspace_id matching the
            // workspace we'll register; workflow_role tags so
            // capture_outgoing_last_message finds it.
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-f2".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-f2".to_string(), ws);
                let mut sp = crate::session::SpawnParams::new(
                    "ts-worker-f2",
                    "worker",
                    "/bin/sleep",
                );
                sp.args = vec!["60".to_string()];
                sp.workspace_id = "ws-f2".to_string();
                sp.workflow_run_id = Some("wf_f2_last_msg".to_string());
                sp.workflow_role = Some("worker".to_string());
                let ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
                s.sessions.insert("ts-worker-f2".to_string(), ds);
            }

            workflow_transition(
                &state,
                &Caller::operator("daemon-poller"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "run_id": "wf_f2_last_msg",
                    "role": "worker",
                }),
            )
            .expect("transition ok");

            let post = crate::workflow::run::load_one("wf_f2_last_msg")
                .expect("load post-transition run");
            // Worker's history entry (the one we just closed)
            // should have last_message populated.
            let worker_entry = post
                .history
                .iter()
                .find(|h| h.role == "worker")
                .expect("worker history entry");
            assert_eq!(
                worker_entry.last_message.as_deref(),
                Some("the captured last message"),
                "last_message must be captured on the closing \
                 history entry; pre-fix it would be None. Got: {:?}",
                worker_entry.last_message,
            );
        });
    }

    // ============================================================
    // 10d-2c-3a — list_workflows + get_workflow_state
    // ============================================================

    /// Smoke test: `list_workflows` with no params, Operator
    /// caller, reads disk via `load_all()`. Pre-3a this method
    /// routed to TUI; post-3a daemon answers directly.
    #[test]
    fn list_workflows_operator_no_filter_returns_all_active_runs() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_lw_op_a", "worker");
            seed_workflow_run("wf_lw_op_b", "worker");
            let result = list_workflows(
                &state,
                &Caller::operator("op-test"),
                &json!({}),
            )
            .expect("ok");
            let arr = result.as_array().expect("array");
            // Filter to the specific runs we seeded so other
            // tests' on-disk runs don't pollute the count under
            // workspace parallelism (env_lock serializes the
            // HOME setup, so this is safe).
            let mut seen: Vec<&str> = arr
                .iter()
                .filter_map(|v| v["run_id"].as_str())
                .filter(|id| id.starts_with("wf_lw_op_"))
                .collect();
            seen.sort();
            assert_eq!(seen, vec!["wf_lw_op_a", "wf_lw_op_b"]);
            // Each entry has the summary shape.
            for v in arr.iter().filter(|v| {
                v["run_id"]
                    .as_str()
                    .map(|s| s.starts_with("wf_lw_op_"))
                    .unwrap_or(false)
            }) {
                assert_eq!(v["name"], "feedback");
                assert_eq!(v["active_role"], "worker");
                assert_eq!(v["status"], "running");
                // Full shape's history field MUST be absent in
                // summary.
                assert!(
                    v.get("history").is_none(),
                    "summary should not include history; got {:?}",
                    v,
                );
            }
        });
    }

    /// `get_workflow_state` with Operator caller returns the
    /// full shape (history + role_sessions). Reads disk via
    /// `load_one()`.
    #[test]
    fn get_workflow_state_operator_returns_full_shape() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_gws_op", "worker");
            let result = get_workflow_state(
                &state,
                &Caller::operator("op-test"),
                &json!({"run_id": "wf_gws_op"}),
            )
            .expect("ok");
            assert_eq!(result["run_id"], "wf_gws_op");
            assert_eq!(result["name"], "feedback");
            assert_eq!(result["active_role"], "worker");
            assert!(result.get("history").is_some(), "full shape includes history");
            assert!(
                result.get("role_sessions").is_some(),
                "full shape includes role_sessions",
            );
            // The seed has one initial history entry.
            let hist = result["history"].as_array().expect("history array");
            assert_eq!(hist.len(), 1);
            assert_eq!(hist[0]["role"], "worker");
        });
    }

    /// `get_workflow_state` for a missing run returns NotFound
    /// to OPERATOR callers (trusted; can legitimately
    /// distinguish missing-run from auth-fail). Session callers
    /// get Unauthorized for the same input — see the
    /// auth-ordering test below.
    #[test]
    fn get_workflow_state_missing_run_returns_not_found_for_operator() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let err = get_workflow_state(
                &state,
                &Caller::operator("op-test"),
                &json!({"run_id": "wf-does-not-exist"}),
            )
            .expect_err("must be NotFound");
            assert_eq!(err.0, ErrorCode::NotFound);
        });
    }

    /// 10d-2c-3a review-r1: auth-ordering / no-info-leak.
    /// Session caller with bogus session_uid AND Session caller
    /// with valid uid + nonexistent run both return Unauthorized
    /// (NOT NotFound). A probe with a bogus uid can't
    /// distinguish "your uid is invalid" from "the run doesn't
    /// exist" or "the run exists but you can't see it" —
    /// preventing existence-probing via differential errors.
    #[test]
    fn get_workflow_state_no_info_leak_for_session_callers() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            // No run on disk for this test.

            // (a) Bogus session_uid + nonexistent run → Unauthorized.
            let err_bogus_uid = get_workflow_state(
                &state,
                &Caller::session("ts-bogus"),
                &json!({"run_id": "wf-nope"}),
            )
            .expect_err("bogus uid must reject");
            assert_eq!(
                err_bogus_uid.0,
                ErrorCode::Unauthorized,
                "bogus session uid: must be Unauthorized (not NotFound) \
                 — pre-fix returned NotFound which leaked nothing yet \
                 BUT changed code from the next case below.",
            );

            // (b) Valid session_uid + nonexistent run → Unauthorized.
            {
                let mut s = state.lock().unwrap();
                let mut sp = crate::session::SpawnParams::new(
                    "ts-real",
                    "x",
                    "/bin/sleep",
                );
                sp.args = vec!["60".to_string()];
                sp.workspace_id = "ws".to_string();
                sp.task_id = Some("task-a".to_string());
                let ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
                s.sessions.insert("ts-real".to_string(), ds);
                s.task_tree.insert("task-a".to_string(), None);
            }
            let err_real_uid = get_workflow_state(
                &state,
                &Caller::session("ts-real"),
                &json!({"run_id": "wf-nope"}),
            )
            .expect_err("valid uid + missing run must reject");
            assert_eq!(
                err_real_uid.0,
                ErrorCode::Unauthorized,
                "valid uid + missing run must be Unauthorized \
                 (not NotFound) so a probe can't differentiate \
                 from case (c) below.",
            );

            // (c) Valid session_uid + run exists but caller has
            // no access → Unauthorized (covered by the existing
            // descendant-scope test; included here as a parity
            // assertion that ALL THREE cases use the same code).
            seed_workflow_run("wf_other_task", "worker");
            crate::workflow::run::modify("wf_other_task", |r| {
                r.task_id = Some("task-b".to_string());
            })
            .expect("set run.task_id");
            // task-b not in task_tree; auth fails.
            let err_no_access = get_workflow_state(
                &state,
                &Caller::session("ts-real"),
                &json!({"run_id": "wf_other_task"}),
            )
            .expect_err("no-access must reject");
            assert_eq!(
                err_no_access.0,
                ErrorCode::Unauthorized,
                "valid uid + run exists + no access must match \
                 cases (a) and (b) — single Unauthorized code \
                 across all three to prevent existence probes.",
            );

            // All three error codes match: indistinguishable to
            // a Session-caller probe.
            assert_eq!(err_bogus_uid.0, err_real_uid.0);
            assert_eq!(err_real_uid.0, err_no_access.0);
        });
    }

    /// `list_workflows` companion: bogus session_uid →
    /// Unauthorized (was NotFound pre-fix).
    #[test]
    fn list_workflows_bogus_session_uid_returns_unauthorized() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let err = list_workflows(
                &state,
                &Caller::session("ts-bogus"),
                &json!({}),
            )
            .expect_err("bogus uid must reject");
            assert_eq!(
                err.0,
                ErrorCode::Unauthorized,
                "bogus session uid must be Unauthorized for \
                 list_workflows (not NotFound). Same boundary \
                 hygiene as get_workflow_state.",
            );
        });
    }

    /// `get_workflow_state` with empty run_id is InvalidParams
    /// (matches `workflow_transition` shape).
    #[test]
    fn get_workflow_state_empty_run_id_is_invalid_params() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let err = get_workflow_state(
                &state,
                &Caller::operator("op-test"),
                &json!({"run_id": ""}),
            )
            .expect_err("must be InvalidParams");
            assert_eq!(err.0, ErrorCode::InvalidParams);
        });
    }

    // ---------------------------------------------------------------
    // Slice 11c: `workflow.get_state` (Operator-only cold-read)
    // ---------------------------------------------------------------

    /// T11 (happy path) — `workflow.get_state` returns the full
    /// `WorkflowRun` shape (serde-default serialization, matching
    /// 11b's snapshot frame payload).
    #[test]
    fn workflow_get_state_happy_path_returns_full_workflow_run() {
        let _tmp = with_temp_home(|| {
            seed_workflow_run("wf_11c_happy", "worker");
            let result = workflow_get_state(&json!({"run_id": "wf_11c_happy"}))
                .expect("ok");
            assert_eq!(result["run_id"], "wf_11c_happy");
            assert_eq!(result["workflow_name"], "feedback");
            assert_eq!(result["active_role"], "worker");
            // Full WorkflowRun shape — has the raw serde fields
            // (history, role_sessions, role_baselines, etc).
            assert!(result.get("history").is_some());
            assert!(result.get("role_sessions").is_some());
            assert!(result.get("events_offset").is_some());
            let hist = result["history"].as_array().expect("history array");
            assert_eq!(hist.len(), 1);
        });
    }

    /// T12 — unknown run_id surfaces `NotFound`.
    #[test]
    fn workflow_get_state_unknown_run_id_returns_not_found() {
        let _tmp = with_temp_home(|| {
            let err = workflow_get_state(&json!({"run_id": "wf-not-there"}))
                .expect_err("must be NotFound");
            assert_eq!(err.0, ErrorCode::NotFound);
        });
    }

    /// T13 — Session caller rejected with `Unauthorized` at the
    /// dispatcher layer (the method body is auth-agnostic by
    /// design; the dispatcher's `require_operator` guard does
    /// the rejection). Drive the dispatch arm directly so the
    /// guard runs.
    #[test]
    fn workflow_get_state_session_caller_rejected_at_dispatcher() {
        let _tmp = with_temp_home(|| {
            seed_workflow_run("wf_11c_auth", "worker");
            let state = make_state_arc();
            let req = crate::control::protocol::Request {
                id: "req-t13".into(),
                caller: Caller::session("ts-some-agent"),
                method: "workflow.get_state".into(),
                params: json!({"run_id": "wf_11c_auth"}),
            };
            let outcome = crate::control::dispatch::dispatch_request(&state, &req);
            let response = outcome.into_response();
            assert!(!response.ok, "Session caller must be rejected");
            assert_eq!(
                response.error.as_ref().expect("error body").code,
                ErrorCode::Unauthorized,
            );
        });
    }

    /// 11c: empty run_id is `InvalidParams` (same shape as
    /// `get_workflow_state`'s empty-string handling).
    #[test]
    fn workflow_get_state_empty_run_id_is_invalid_params() {
        let _tmp = with_temp_home(|| {
            let err = workflow_get_state(&json!({"run_id": ""}))
                .expect_err("must be InvalidParams");
            assert_eq!(err.0, ErrorCode::InvalidParams);
        });
    }

    // ---------------------------------------------------------------
    // Slice 11e prerequisite: `workflow_reject_finding` daemon-routed
    // ---------------------------------------------------------------

    /// Happy path — `workflow_reject_finding` appends to state.json's
    /// rejected_findings AND writes a daemon-source event to
    /// events.jsonl (which Option B's writer broadcasts).
    #[test]
    fn workflow_reject_finding_appends_to_state_and_writes_event() {
        let _tmp = with_temp_home(|| {
            seed_workflow_run("wf_rf_happy", "worker");
            let state = make_state_arc();
            let result = workflow_reject_finding(
                &state,
                &Caller::operator("op"),
                &json!({
                    "run_id": "wf_rf_happy",
                    "role": "manager",
                    "text": "out of scope nit",
                }),
            )
            .expect("ok");
            assert_eq!(result["ok"], true);
            assert_eq!(result["run_id"], "wf_rf_happy");

            // Disk: rejected_findings populated.
            let reloaded =
                crate::workflow::run::load_one("wf_rf_happy").expect("reload");
            assert_eq!(reloaded.rejected_findings.len(), 1);
            assert_eq!(reloaded.rejected_findings[0].text, "out of scope nit");

            // Event on disk too — read events.jsonl and confirm
            // the daemon-source workflow_reject_finding line is there.
            let events_path = crate::workflow::run::events_path("wf_rf_happy");
            let raw = std::fs::read_to_string(&events_path).expect("read events");
            assert!(
                raw.contains("\"tool\":\"workflow_reject_finding\""),
                "events.jsonl must contain the daemon-source event; got {}",
                raw,
            );
            assert!(raw.contains("\"source\":\"daemon\""));
            assert!(raw.contains("\"text\":\"out of scope nit\""));
        });
    }

    /// Empty `text` is `InvalidParams`.
    #[test]
    fn workflow_reject_finding_empty_text_is_invalid_params() {
        let _tmp = with_temp_home(|| {
            seed_workflow_run("wf_rf_empty", "worker");
            let state = make_state_arc();
            let err = workflow_reject_finding(
                &state,
                &Caller::operator("op"),
                &json!({"run_id": "wf_rf_empty", "role": "manager", "text": "   "}),
            )
            .expect_err("must be InvalidParams");
            assert_eq!(err.0, ErrorCode::InvalidParams);
        });
    }

    /// Empty `run_id` is `InvalidParams`.
    #[test]
    fn workflow_reject_finding_empty_run_id_is_invalid_params() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let err = workflow_reject_finding(
                &state,
                &Caller::operator("op"),
                &json!({"run_id": "", "role": "manager", "text": "x"}),
            )
            .expect_err("must be InvalidParams");
            assert_eq!(err.0, ErrorCode::InvalidParams);
        });
    }

    /// 11e rollback (reviewer round) — mirror of
    /// `workflow_transition_persistent_event_failure_rolls_back_state_returns_internal`.
    /// When `append_event_with_retry` exhausts (events.jsonl path
    /// blocked by a directory in its place — the established
    /// fault-injection trick), the rejection push to state.json
    /// MUST be rolled back. Pre-fix `state.json` retained the
    /// pushed RejectedFinding with no matching event on disk, and
    /// a caller-side retry would push a duplicate.
    #[test]
    fn workflow_reject_finding_persistent_event_failure_rolls_back_state_returns_internal() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_rf_evfail", "worker");
            let pre =
                crate::workflow::run::load_one("wf_rf_evfail").expect("pre load");
            let pre_len = pre.rejected_findings.len();

            // Block append_event with EISDIR — persistent.
            let dir = crate::workflow::run::run_dir("wf_rf_evfail");
            std::fs::create_dir_all(&dir).unwrap();
            let events_path =
                crate::workflow::run::events_path("wf_rf_evfail");
            std::fs::create_dir(&events_path).expect("events.jsonl as dir");

            let err = workflow_reject_finding(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "run_id": "wf_rf_evfail",
                    "role": "manager",
                    "text": "rejected nit A",
                }),
            )
            .expect_err("event write must fail after retries");
            assert_eq!(err.0, ErrorCode::Internal);
            assert!(
                err.1.contains("failed to append event"),
                "error message surfaces event-write failure: {}",
                err.1,
            );

            // state.json on disk is ROLLED BACK: rejected_findings
            // back to pre-call length.
            let post =
                crate::workflow::run::load_one("wf_rf_evfail").expect("post load");
            assert_eq!(
                post.rejected_findings.len(),
                pre_len,
                "rollback: rejected_findings.len() MUST match pre-mutation \
                 length ({}); observed {}. Pre-fix the pushed RejectedFinding \
                 was retained with no matching event on disk.",
                pre_len,
                post.rejected_findings.len(),
            );

            // No event on disk either (the directory-at-events-path
            // blocked every retry attempt).
            std::fs::remove_dir(&events_path).expect("remove dir");
            let (events, _) =
                crate::workflow::events::read_new("wf_rf_evfail", 0);
            assert!(events.is_empty(), "no events after rollback");
        });
    }

    /// 11e rollback (reviewer round) — mirror of
    /// `workflow_transition_caller_retry_after_rollback_succeeds_cleanly`.
    /// After a failed call rolls back, the caller's external
    /// retry runs the full RMW from scratch — exactly ONE
    /// rejection lands in state.json, exactly ONE event in
    /// events.jsonl. Pre-fix the rollback was a no-op and the
    /// retry would have stacked a second RejectedFinding.
    #[test]
    fn workflow_reject_finding_caller_retry_after_rollback_succeeds_cleanly() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_rf_retry", "worker");

            // Stage 1: block event write → rollback fires.
            let dir = crate::workflow::run::run_dir("wf_rf_retry");
            std::fs::create_dir_all(&dir).unwrap();
            let events_path =
                crate::workflow::run::events_path("wf_rf_retry");
            std::fs::create_dir(&events_path).expect("events.jsonl as dir");
            let err = workflow_reject_finding(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "run_id": "wf_rf_retry",
                    "role": "manager",
                    "text": "rejected nit (retry)",
                }),
            )
            .expect_err("first call: event write fails");
            assert_eq!(err.0, ErrorCode::Internal);
            // Rolled back.
            let mid =
                crate::workflow::run::load_one("wf_rf_retry").expect("mid load");
            assert_eq!(
                mid.rejected_findings.len(),
                0,
                "rolled back to pre-mutation length",
            );

            // Stage 2: heal disk + retry with identical args.
            std::fs::remove_dir(&events_path).expect("remove dir");
            let ok = workflow_reject_finding(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "run_id": "wf_rf_retry",
                    "role": "manager",
                    "text": "rejected nit (retry)",
                }),
            )
            .expect("retry must succeed after rollback");
            assert_eq!(ok["ok"], json!(true));

            // Single end-state: exactly ONE rejection on disk and
            // exactly ONE event. Pre-fix the rollback was a no-op
            // and the retry would stack a second RejectedFinding.
            let post =
                crate::workflow::run::load_one("wf_rf_retry").expect("post load");
            assert_eq!(
                post.rejected_findings.len(),
                1,
                "exactly one rejection after the retry; no double-push",
            );
            assert_eq!(
                post.rejected_findings[0].text,
                "rejected nit (retry)",
            );
            let (events, _) =
                crate::workflow::events::read_new("wf_rf_retry", 0);
            assert_eq!(events.len(), 1, "exactly one event after the retry");
        });
    }

    /// Slice 11e Option B mechanical: the broadcaster hook lives
    /// inside `WorkflowEventsWriter::append_event_and_broadcast`.
    /// A successful append delivers the event to subscribers
    /// AFTER the disk fsync. Pins the post-write ordering — a
    /// regression that broadcast BEFORE write would fail this
    /// (subscriber would see the event but disk would be empty
    /// if write then failed).
    #[test]
    fn workflow_events_writer_broadcasts_after_disk_write() {
        use std::sync::Arc;
        let _tmp = with_temp_home(|| {
            let watcher =
                Arc::new(crate::workflow::events::WorkflowEventWatcher::new());
            let (rx, _guard) = watcher.subscribe();
            seed_workflow_run("wf_optb", "worker");
            let ev = crate::workflow::events::Event {
                id: "ev-optb-1".into(),
                ts: 0.0,
                run_id: "wf_optb".into(),
                role: "worker".into(),
                tool: "workflow_transition".into(),
                args: json!({"to": "reviewer", "prompt": ""}),
                source: "daemon".into(),
                from_role: None,
                iteration: 0,
            };
            crate::workflow::events::WorkflowEventsWriter::append_event_and_broadcast(
                &ev, &watcher,
            )
            .expect("append + broadcast ok");

            // Event on disk.
            let raw = std::fs::read_to_string(
                crate::workflow::run::events_path("wf_optb"),
            )
            .expect("read events");
            assert!(raw.contains("ev-optb-1"));

            // Event delivered to subscriber AFTER disk write.
            let received = rx
                .recv_timeout(std::time::Duration::from_millis(200))
                .expect("subscriber must receive after disk write")
                .expect_event();
            assert_eq!(received.id, "ev-optb-1");
        });
    }

    /// Session caller without `task_id` (taskless) cannot see
    /// workflow runs (matches TUI's gate via
    /// `workflow_run_authorized`'s `caller.task_id.as_deref()`
    /// None branch).
    #[test]
    fn get_workflow_state_taskless_session_caller_unauthorized() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_gws_taskless", "worker");
            // Set up a Session caller with task_id=None.
            {
                let mut s = state.lock().unwrap();
                let mut sp = crate::session::SpawnParams::new(
                    "ts-taskless",
                    "x",
                    "/bin/sleep",
                );
                sp.args = vec!["60".to_string()];
                sp.workspace_id = "ws-tl".to_string();
                // NB: task_id NOT set.
                let ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
                s.sessions.insert("ts-taskless".to_string(), ds);
            }
            let err = get_workflow_state(
                &state,
                &Caller::session("ts-taskless"),
                &json!({"run_id": "wf_gws_taskless"}),
            )
            .expect_err("taskless must reject");
            assert_eq!(err.0, ErrorCode::Unauthorized);
        });
    }

    /// Session caller scoped to a specific `task_id` for
    /// `list_workflows` — when caller is in scope, see runs;
    /// when out of scope, Unauthorized at the params level.
    #[test]
    fn list_workflows_session_caller_scope_rejection() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            // Caller bound to task-A.
            {
                let mut s = state.lock().unwrap();
                let mut sp = crate::session::SpawnParams::new(
                    "ts-a",
                    "x",
                    "/bin/sleep",
                );
                sp.args = vec!["60".to_string()];
                sp.workspace_id = "ws-a".to_string();
                sp.task_id = Some("task-a".to_string());
                let ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
                s.sessions.insert("ts-a".to_string(), ds);
                // task tree: task-a is top-level.
                s.task_tree.insert("task-a".to_string(), None);
                s.task_tree.insert("task-b".to_string(), None);
            }
            // Requesting task-b's scope when caller is bound to
            // task-a → Unauthorized.
            let err = list_workflows(
                &state,
                &Caller::session("ts-a"),
                &json!({"task_id": "task-b"}),
            )
            .expect_err("cross-scope must reject");
            assert_eq!(err.0, ErrorCode::Unauthorized);
            // Caller's own scope is fine (returns empty list since
            // we haven't seeded any runs).
            let ok = list_workflows(
                &state,
                &Caller::session("ts-a"),
                &json!({"task_id": "task-a"}),
            )
            .expect("self-scope ok");
            assert!(ok.is_array());
        });
    }

    /// Auth filter: run with `task_id` set; Session caller
    /// authorized for that task sees the run; unrelated Session
    /// caller does not. Mirrors TUI's `workflow_run_authorized`
    /// behavior for the run.task_id-set path.
    #[test]
    fn get_workflow_state_session_caller_descendant_scope_visible_only_to_owner() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            // Two callers: ts-a bound to task-a, ts-b to task-b.
            {
                let mut s = state.lock().unwrap();
                for (uid, tid) in [("ts-a", "task-a"), ("ts-b", "task-b")] {
                    let mut sp = crate::session::SpawnParams::new(
                        uid,
                        "x",
                        "/bin/sleep",
                    );
                    sp.args = vec!["60".to_string()];
                    sp.workspace_id = "ws".to_string();
                    sp.task_id = Some(tid.to_string());
                    let ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
                    s.sessions.insert(uid.to_string(), ds);
                }
                s.task_tree.insert("task-a".to_string(), None);
                s.task_tree.insert("task-b".to_string(), None);
            }
            // Seed a run; set its task_id to task-a.
            seed_workflow_run("wf_gws_owned", "worker");
            crate::workflow::run::modify("wf_gws_owned", |r| {
                r.task_id = Some("task-a".to_string());
            })
            .expect("set run.task_id");

            // Owner sees it.
            let ok = get_workflow_state(
                &state,
                &Caller::session("ts-a"),
                &json!({"run_id": "wf_gws_owned"}),
            )
            .expect("owner ok");
            assert_eq!(ok["run_id"], "wf_gws_owned");
            // Outsider doesn't.
            let err = get_workflow_state(
                &state,
                &Caller::session("ts-b"),
                &json!({"run_id": "wf_gws_owned"}),
            )
            .expect_err("outsider rejected");
            assert_eq!(err.0, ErrorCode::Unauthorized);
        });
    }

    // ============================================================
    // 10d-2c-2-2-c R13 — concurrent daemon-poller + MCP
    // workflow_transition. Both orderings tested explicitly.
    // ============================================================

    /// R13 case A — daemon's `workflow_transition` (Operator,
    /// expected_from + trigger=static_idle) fires FIRST and wins.
    /// MCP's Session-caller call fires second and sees
    /// post-mutation state where `active_role` no longer
    /// matches the caller's bound `workflow_role` → Unauthorized.
    ///
    /// Sequential calls suffice — the flock is acquired/released
    /// between calls, but the OUTCOME (second call sees state
    /// mutated by first call) is the same. The point of R13 is
    /// the precondition asymmetry: daemon uses `expected_from`
    /// (R4 explicit param); MCP uses Session-caller auth (R1
    /// implicit gate). Both must reject the loser.
    #[test]
    fn workflow_transition_r13_daemon_first_mcp_unauthorized() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_r13_a", "worker");
            // MCP agent's bound session: workflow_run_id +
            // workflow_role = worker. After daemon's mutation
            // (active_role → reviewer), the MCP auth check
            // (active_role == caller's bound role) fails.
            {
                let mut s = state.lock().unwrap();
                let mut sp = crate::session::SpawnParams::new(
                    "ts-mcp-worker",
                    "worker",
                    "/bin/sleep",
                );
                sp.args = vec!["60".to_string()];
                sp.workspace_id = "/tmp/seed-task-key".to_string();
                sp.workflow_run_id = Some("wf_r13_a".to_string());
                sp.workflow_role = Some("worker".to_string());
                let ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
                s.sessions.insert("ts-mcp-worker".to_string(), ds);
            }

            // Daemon-poller-shape call (Operator, expected_from,
            // trigger=static_idle). Wins.
            workflow_transition(
                &state,
                &Caller::operator("daemon-poller"),
                &json!({
                    "to": "reviewer",
                    "prompt": "",
                    "role": "worker",
                    "run_id": "wf_r13_a",
                    "expected_from": "worker",
                    "trigger": "static_idle",
                }),
            )
            .expect("daemon fire wins");

            // MCP-shape call (Session caller, no
            // expected_from/trigger). Loses with Unauthorized
            // because the caller's bound role (worker) no longer
            // matches active_role (reviewer).
            let err = workflow_transition(
                &state,
                &Caller::session("ts-mcp-worker"),
                &json!({
                    "to": "manager",
                    "prompt": "p",
                    "role": "worker",
                    "run_id": "wf_r13_a",
                }),
            )
            .expect_err("MCP must lose");
            assert_eq!(
                err.0,
                ErrorCode::Unauthorized,
                "MCP's auth check fails: caller's bound role \
                 (worker) != post-mutation active_role (reviewer). \
                 Got: {:?}",
                err,
            );

            // Final state: daemon's transition committed.
            let post = crate::workflow::run::load_one("wf_r13_a")
                .expect("load");
            assert_eq!(post.active_role.as_deref(), Some("reviewer"));
            // Exactly one event written (daemon's).
            let (events, _) =
                crate::workflow::events::read_new("wf_r13_a", 0);
            assert_eq!(
                events.len(),
                1,
                "exactly one event — daemon's. Got: {:?}",
                events,
            );
            assert_eq!(events[0].source, "daemon");
        });
    }

    /// R13 case B — MCP's Session-caller `workflow_transition`
    /// fires FIRST and wins. Daemon's call fires second; its
    /// `expected_from: "worker"` param no longer matches the
    /// post-mutation `active_role` → Conflict.
    ///
    /// Asymmetry note: the daemon-poller path uses the R4
    /// explicit `expected_from` param to reject. The MCP path
    /// uses the R1 implicit Session-caller auth check. Different
    /// code paths, both must reject the loser. This test pins
    /// the daemon-loses ordering.
    #[test]
    fn workflow_transition_r13_mcp_first_daemon_conflict() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_r13_b", "worker");
            {
                let mut s = state.lock().unwrap();
                let mut sp = crate::session::SpawnParams::new(
                    "ts-mcp-worker-b",
                    "worker",
                    "/bin/sleep",
                );
                sp.args = vec!["60".to_string()];
                sp.workspace_id = "/tmp/seed-task-key".to_string();
                sp.workflow_run_id = Some("wf_r13_b".to_string());
                sp.workflow_role = Some("worker".to_string());
                let ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
                s.sessions.insert("ts-mcp-worker-b".to_string(), ds);
            }

            // MCP-shape call (Session caller, no expected_from).
            // Wins.
            workflow_transition(
                &state,
                &Caller::session("ts-mcp-worker-b"),
                &json!({
                    "to": "reviewer",
                    "prompt": "p",
                    "role": "worker",
                    "run_id": "wf_r13_b",
                }),
            )
            .expect("MCP fire wins");

            // Daemon-poller-shape call (Operator, expected_from
            // still "worker"). Loses with Conflict because
            // active_role is now "reviewer".
            let err = workflow_transition(
                &state,
                &Caller::operator("daemon-poller"),
                &json!({
                    "to": "manager",
                    "prompt": "",
                    "role": "worker",
                    "run_id": "wf_r13_b",
                    "expected_from": "worker",
                    "trigger": "static_idle",
                }),
            )
            .expect_err("daemon must lose");
            assert_eq!(
                err.0,
                ErrorCode::Conflict,
                "daemon's expected_from='worker' mismatches \
                 post-mutation active_role='reviewer'. Got: {:?}",
                err,
            );

            let post = crate::workflow::run::load_one("wf_r13_b")
                .expect("load");
            assert_eq!(post.active_role.as_deref(), Some("reviewer"));
            let (events, _) =
                crate::workflow::events::read_new("wf_r13_b", 0);
            assert_eq!(
                events.len(),
                1,
                "exactly one event — MCP's. Got: {:?}",
                events,
            );
            // MCP-routed event uses source="daemon" too (since
            // workflow_transition handler always sets that for
            // daemon-handled events); the discriminator is the
            // absence of `args.trigger`.
            assert_eq!(events[0].source, "daemon");
            assert!(
                events[0].args.get("trigger").is_none(),
                "MCP caller's trigger param filtered (R3 F1); \
                 event must lack args.trigger. Got: {:?}",
                events[0].args,
            );
        });
    }

    // ============================================================
    // 10d-2c-2-2-c fire-output parity — state.json byte-comparison
    // between TUI-direct path and daemon-poller path.
    // ============================================================

    /// Parity invariant: drive a TUI-direct static fire AND a
    /// daemon-poller static fire with identical inputs, then
    /// assert the resulting state.json mutations are
    /// field-equivalent modulo:
    ///   - timestamps (`history[].activated_at` / `deactivated_at`)
    ///   - assistant_count snapshot timing (both should be the
    ///     same value, but check independently)
    ///
    /// For the daemon-poller path, we ALSO simulate the TUI
    /// tail's `append_history_entry_for_event_target_role` since
    /// daemon-only fire stops at "active_role advanced + outgoing
    /// closed"; the new history entry for the target role is
    /// appended by the TUI tail when it consumes the event.
    /// Without that simulation, the post-states would diverge by
    /// construction (one less history entry on daemon path).
    ///
    /// Reviewer flagged this as the highest-value drift guard.
    /// If a future change introduces real divergence, this
    /// test catches it within a single tick of one workflow.
    #[test]
    fn fire_output_parity_state_json_matches_between_paths() {
        let _tmp = with_temp_home(|| {
            let home =
                std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let wt = home.join("wt-fp");
            std::fs::create_dir_all(&wt).unwrap();
            let wt_str = wt.to_str().unwrap();
            let encoded = wt_str.replace('/', "-").replace('.', "-");
            let proj = home.join(format!(".claude/projects/{}", encoded));
            std::fs::create_dir_all(&proj).unwrap();
            // Identical transcript for both runs.
            std::fs::write(
                proj.join("sid-fp.jsonl"),
                r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"parity message"}]}}"##,
            )
            .unwrap();

            // Seed two runs with identical initial state.
            seed_workflow_run("wf_parity_tui", "worker");
            seed_workflow_run("wf_parity_dae", "worker");
            for run_id in ["wf_parity_tui", "wf_parity_dae"] {
                crate::workflow::run::modify(run_id, |r| {
                    if let Some(b) = r.role_sessions.get_mut("worker") {
                        b.current_session_id = Some("sid-fp".to_string());
                    }
                })
                .expect("bind worker sid");
            }

            // Workspace + daemon session registered so
            // `capture_outgoing_last_message` can find them.
            // Same session_uid for both runs so the daemon-path
            // can find one — but the workflow_run_id tag
            // disambiguates which run's call captures.
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = crate::manifest::ManifestWorkspace::default();
                ws.id = "ws-fp".to_string();
                ws.worktree_path = Some(wt.clone());
                s.workspaces.insert("ws-fp".to_string(), ws);
                // One session for TUI path's capture lookup.
                let mut sp_t = crate::session::SpawnParams::new(
                    "ts-w-fp-tui",
                    "worker",
                    "/bin/sleep",
                );
                sp_t.args = vec!["60".to_string()];
                sp_t.workspace_id = "ws-fp".to_string();
                sp_t.workflow_run_id = Some("wf_parity_tui".to_string());
                sp_t.workflow_role = Some("worker".to_string());
                let ds_t = crate::session::DaemonSession::spawn(sp_t).expect("spawn");
                s.sessions.insert("ts-w-fp-tui".to_string(), ds_t);
                // Separate session for daemon path.
                let mut sp_d = crate::session::SpawnParams::new(
                    "ts-w-fp-dae",
                    "worker",
                    "/bin/sleep",
                );
                sp_d.args = vec!["60".to_string()];
                sp_d.workspace_id = "ws-fp".to_string();
                sp_d.workflow_run_id = Some("wf_parity_dae".to_string());
                sp_d.workflow_role = Some("worker".to_string());
                let ds_d = crate::session::DaemonSession::spawn(sp_d).expect("spawn");
                s.sessions.insert("ts-w-fp-dae".to_string(), ds_d);
            }

            // ============================================
            // TUI-direct path: in-process equivalent of
            // `fire_transition` — close + activate.
            // ============================================
            let captured = crate::workflow::transcript::last_message(
                &crate::workflow::toml_schema::Engine::ClaudeCode,
                &wt,
                "sid-fp",
            );
            // Mirror what TUI's `fire_transition` does inside
            // its `run::modify` closure.
            crate::workflow::run::modify("wf_parity_tui", |r| {
                r.close_active_role(captured.clone());
                r.activate_role(
                    "reviewer".to_string(),
                    crate::workflow::run::TriggerKind::StaticIdle {
                        from_role: "worker".to_string(),
                    },
                    /* start_count */ 1, // post-baseline count
                    /* start_text_count */ 0,
                );
            })
            .expect("tui fire");

            // ============================================
            // Daemon-poller path: workflow_transition + tail
            // simulation.
            // ============================================
            workflow_transition(
                &state,
                &Caller::operator("daemon-poller"),
                &json!({
                    "to": "reviewer",
                    "prompt": "",
                    "role": "worker",
                    "run_id": "wf_parity_dae",
                    "expected_from": "worker",
                    "trigger": "static_idle",
                }),
            )
            .expect("daemon fire");
            // Simulate the TUI tail consuming the event +
            // appending the target role's history entry. Reads
            // the daemon's emitted event for the post-mutation
            // iteration value (`event.iteration`).
            let (events, _) =
                crate::workflow::events::read_new("wf_parity_dae", 0);
            assert_eq!(events.len(), 1, "exactly one daemon event");
            let ev_iter = events[0].iteration;
            crate::workflow::run::modify("wf_parity_dae", |r| {
                r.append_history_entry_for_event_target_role(
                    "reviewer",
                    ev_iter,
                    crate::workflow::run::TriggerKind::StaticIdle {
                        from_role: "worker".to_string(),
                    },
                    /* start_count */ 1,
                    /* start_text_count */ 0,
                );
            })
            .expect("tail-append");

            // ============================================
            // Compare states.
            // ============================================
            let tui = crate::workflow::run::load_one("wf_parity_tui")
                .expect("load tui");
            let dae = crate::workflow::run::load_one("wf_parity_dae")
                .expect("load daemon");

            assert_eq!(
                tui.active_role, dae.active_role,
                "active_role must match",
            );
            assert_eq!(
                tui.iteration, dae.iteration,
                "iteration must match",
            );
            assert_eq!(
                tui.history.len(),
                dae.history.len(),
                "history length must match (initial + worker-closed + reviewer-active = 3 ... actually 2 since `WorkflowRun::new`'s seed entry is for the initial role and stays closed). Got tui={}, dae={}",
                tui.history.len(),
                dae.history.len(),
            );
            // Field-by-field comparison of each history entry,
            // modulo timestamps.
            for (i, (t, d)) in
                tui.history.iter().zip(dae.history.iter()).enumerate()
            {
                assert_eq!(
                    t.role, d.role,
                    "history[{}].role drift",
                    i,
                );
                assert_eq!(
                    t.iteration, d.iteration,
                    "history[{}].iteration drift",
                    i,
                );
                assert_eq!(
                    t.session_id, d.session_id,
                    "history[{}].session_id drift",
                    i,
                );
                assert_eq!(
                    t.last_message, d.last_message,
                    "history[{}].last_message DRIFT — this is the \
                     parity-test high-value assertion. TUI and \
                     daemon paths must capture the same last \
                     message for the closing role.",
                    i,
                );
                assert_eq!(
                    t.assistant_count_at_start,
                    d.assistant_count_at_start,
                    "history[{}].assistant_count_at_start drift",
                    i,
                );
                // Trigger comparison: serialize to JSON value
                // so the comparison is structure-aware (matches
                // the on-wire shape pinned by
                // `serialize_trigger_wire_shape`).
                let t_trig =
                    serde_json::to_value(&t.trigger).expect("ser tui trigger");
                let d_trig =
                    serde_json::to_value(&d.trigger).expect("ser dae trigger");
                assert_eq!(
                    t_trig, d_trig,
                    "history[{}].trigger drift (post-R3 fix both \
                     paths should produce StaticIdle{{from_role}})",
                    i,
                );
                // deactivated_at: closed entries (history[0], the
                // worker entry) should be Some on both; active
                // entries (history[1], the reviewer) should be
                // None on both. Exact timestamps differ across
                // paths so we only check the Some/None shape.
                assert_eq!(
                    t.deactivated_at.is_some(),
                    d.deactivated_at.is_some(),
                    "history[{}].deactivated_at shape drift (Some/None)",
                    i,
                );
            }
        });
    }

    // ============================================================
    // 10d-3 — stop_workflow tests
    // ============================================================

    /// T1: Operator caller flips a Running run to Detached on
    /// disk via daemon's stop_workflow handler.
    #[test]
    fn stop_workflow_operator_running_to_detached() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_sw_running", "worker");
            let result = stop_workflow(
                &state,
                &Caller::operator("op-test"),
                &json!({"run_id": "wf_sw_running"}),
            )
            .expect("ok");
            assert_eq!(result["ok"], true);
            let post = crate::workflow::run::load_one("wf_sw_running")
                .expect("load post");
            assert!(matches!(
                post.status,
                crate::workflow::run::RunStatus::Detached
            ));
        });
    }

    /// T3: terminal-state guard — stop on a Done run is a no-op.
    /// Preserves the `Done` status + `done_reason`. Mirrors
    /// 10d-2c-1 round-9's TUI-side guard.
    #[test]
    fn stop_workflow_on_done_run_is_noop_preserves_done_reason() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_sw_done", "worker");
            // Mark the run Done with a reason.
            crate::workflow::run::modify("wf_sw_done", |r| {
                r.status = crate::workflow::run::RunStatus::Done;
                r.active_role = None;
                r.done_reason = Some("approved".into());
            })
            .expect("set Done");

            let result = stop_workflow(
                &state,
                &Caller::operator("op-test"),
                &json!({"run_id": "wf_sw_done"}),
            )
            .expect("ok");
            assert_eq!(result["ok"], true);

            let post = crate::workflow::run::load_one("wf_sw_done")
                .expect("load post");
            assert!(matches!(
                post.status,
                crate::workflow::run::RunStatus::Done
            ));
            assert_eq!(post.done_reason.as_deref(), Some("approved"));
        });
    }

    /// T4: idempotent — second stop on Detached returns ok and
    /// is a no-op (Detached → Detached is benign).
    #[test]
    fn stop_workflow_on_detached_run_is_idempotent() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_sw_idem", "worker");
            // First stop: Running → Detached.
            stop_workflow(
                &state,
                &Caller::operator("op-test"),
                &json!({"run_id": "wf_sw_idem"}),
            )
            .expect("first stop ok");
            // Second stop: still Detached, no error.
            stop_workflow(
                &state,
                &Caller::operator("op-test"),
                &json!({"run_id": "wf_sw_idem"}),
            )
            .expect("second stop ok (idempotent)");
            let post = crate::workflow::run::load_one("wf_sw_idem")
                .expect("load post");
            assert!(matches!(
                post.status,
                crate::workflow::run::RunStatus::Detached
            ));
        });
    }

    /// Missing-run returns NotFound for Operator, Unauthorized
    /// for Session (no-info-leak invariant from 10d-2c-3a).
    #[test]
    fn stop_workflow_missing_run_operator_vs_session_error_codes() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let err_op = stop_workflow(
                &state,
                &Caller::operator("op-test"),
                &json!({"run_id": "wf-missing"}),
            )
            .expect_err("operator missing");
            assert_eq!(err_op.0, ErrorCode::NotFound);

            // Session caller with valid uid + nonexistent run:
            // Unauthorized (matches the get_workflow_state
            // no-info-leak invariant).
            {
                let mut s = state.lock().unwrap();
                let mut sp = crate::session::SpawnParams::new(
                    "ts-real-sw",
                    "x",
                    "/bin/sleep",
                );
                sp.args = vec!["60".to_string()];
                sp.workspace_id = "ws".to_string();
                sp.task_id = Some("task-a".to_string());
                let ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
                s.sessions.insert("ts-real-sw".to_string(), ds);
                s.task_tree.insert("task-a".to_string(), None);
            }
            let err_session = stop_workflow(
                &state,
                &Caller::session("ts-real-sw"),
                &json!({"run_id": "wf-missing"}),
            )
            .expect_err("session missing");
            assert_eq!(err_session.0, ErrorCode::Unauthorized);
        });
    }

    /// T5: auth-ordering / no-info-leak — bogus session_uid →
    /// Unauthorized for stop_workflow (matches get_workflow_state
    /// shape).
    #[test]
    fn stop_workflow_bogus_session_uid_returns_unauthorized() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            let err = stop_workflow(
                &state,
                &Caller::session("ts-bogus"),
                &json!({"run_id": "wf-whatever"}),
            )
            .expect_err("must reject");
            assert_eq!(err.0, ErrorCode::Unauthorized);
        });
    }

    /// T2: Session caller cross-scope rejection. Caller bound to
    /// task-a; run bound to task-b → Unauthorized.
    #[test]
    fn stop_workflow_session_caller_cross_scope_rejected() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_sw_xs", "worker");
            crate::workflow::run::modify("wf_sw_xs", |r| {
                r.task_id = Some("task-b".to_string());
            })
            .expect("set run.task_id");
            {
                let mut s = state.lock().unwrap();
                let mut sp = crate::session::SpawnParams::new(
                    "ts-a-xs",
                    "x",
                    "/bin/sleep",
                );
                sp.args = vec!["60".to_string()];
                sp.workspace_id = "ws".to_string();
                sp.task_id = Some("task-a".to_string());
                let ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
                s.sessions.insert("ts-a-xs".to_string(), ds);
                s.task_tree.insert("task-a".to_string(), None);
                s.task_tree.insert("task-b".to_string(), None);
            }
            let err = stop_workflow(
                &state,
                &Caller::session("ts-a-xs"),
                &json!({"run_id": "wf_sw_xs"}),
            )
            .expect_err("cross-scope must reject");
            assert_eq!(err.0, ErrorCode::Unauthorized);
            // State unchanged.
            let post = crate::workflow::run::load_one("wf_sw_xs")
                .expect("load post");
            assert!(matches!(
                post.status,
                crate::workflow::run::RunStatus::Running
            ));
        });
    }

    /// T6: race ordering A — `stop_workflow` wins flock first;
    /// concurrent `workflow_transition` then sees status =
    /// Detached and returns Conflict via the existing
    /// terminal-state guard (10d-2c-1 round-9).
    #[test]
    fn stop_workflow_then_transition_returns_conflict() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_sw_race_a", "worker");
            // Stop wins.
            stop_workflow(
                &state,
                &Caller::operator("op-test"),
                &json!({"run_id": "wf_sw_race_a"}),
            )
            .expect("stop ok");
            // Transition loses — run is now Detached.
            let err = workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "",
                    "role": "worker",
                    "run_id": "wf_sw_race_a",
                }),
            )
            .expect_err("transition on Detached must conflict");
            assert_eq!(err.0, ErrorCode::Conflict);
            // No event emitted.
            let (events, _) =
                crate::workflow::events::read_new("wf_sw_race_a", 0);
            assert!(
                events.is_empty(),
                "no event written — transition rejected pre-mutation",
            );
        });
    }

    /// T6 symmetric: race ordering B — `workflow_transition` wins
    /// flock first; concurrent `stop_workflow` then sees the
    /// post-transition state (status still Running, active_role
    /// advanced) and successfully transitions Running → Detached.
    /// Both writes succeed in sequence — flock+try_modify
    /// serializes cleanly.
    #[test]
    fn transition_then_stop_workflow_both_succeed() {
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_sw_race_b", "worker");
            // Transition wins.
            workflow_transition(
                &state,
                &Caller::operator("op-test"),
                &json!({
                    "to": "reviewer",
                    "prompt": "",
                    "role": "worker",
                    "run_id": "wf_sw_race_b",
                }),
            )
            .expect("transition ok");
            // Stop succeeds on the now-active-role=reviewer run.
            stop_workflow(
                &state,
                &Caller::operator("op-test"),
                &json!({"run_id": "wf_sw_race_b"}),
            )
            .expect("stop ok");
            let post = crate::workflow::run::load_one("wf_sw_race_b")
                .expect("load post");
            assert!(matches!(
                post.status,
                crate::workflow::run::RunStatus::Detached
            ));
            assert_eq!(post.active_role.as_deref(), Some("reviewer"));
        });
    }

    /// T7: poller skips Detached runs. After stop, the daemon's
    /// `cm-workflow-poller` filters via `run.is_active()` which
    /// excludes Detached. No fire happens for the stopped run.
    #[test]
    fn poller_skips_detached_runs_after_stop() {
        use std::sync::Arc as StdArc;
        let _tmp = with_temp_home(|| {
            let state = make_state_arc();
            seed_workflow_run("wf_sw_poller_detached", "worker");
            stop_workflow(
                &state,
                &Caller::operator("op-test"),
                &json!({"run_id": "wf_sw_poller_detached"}),
            )
            .expect("stop ok");

            // The poller's `collect_snapshots` walks
            // load_all().filter(is_active()). Detached runs are
            // filtered out — assert by calling poll_once() with
            // apply disabled and checking decisions.
            let poller =
                crate::workflow::poller::WorkflowPoller::new(StdArc::clone(&state));
            poller.set_disable_apply_for_test(true);
            let decisions = poller.poll_once();
            // Our specific run must not appear in decisions.
            let saw_our_run = decisions.iter().any(|d| match d {
                crate::workflow::poller::Decision::Skip { run_id, .. }
                | crate::workflow::poller::Decision::Nudge { run_id, .. }
                | crate::workflow::poller::Decision::ActivateStatic {
                    run_id,
                    ..
                } => run_id == "wf_sw_poller_detached",
            });
            assert!(
                !saw_our_run,
                "poller must not surface a Detached run in decisions. \
                 Got: {:?}",
                decisions,
            );
        });
    }

    /// Fire-output parity: TUI-path stop AND daemon-path stop
    /// produce byte-identical state.json mutations.
    ///
    /// Both paths call `apply_stop_workflow_status` via
    /// `run::modify`. The function is the SAME canonical helper
    /// (relocated to `daemon/src/workflow/run.rs` in 10d-3) so
    /// parity is canonical by construction. This test pins
    /// against a future regression where one path diverges
    /// (e.g., adds a side effect on the WorkflowRun struct).
    #[test]
    fn fire_output_parity_stop_workflow_state_matches_between_paths() {
        let _tmp = with_temp_home(|| {
            // Seed two identical runs.
            seed_workflow_run("wf_sw_parity_tui", "worker");
            seed_workflow_run("wf_sw_parity_dae", "worker");

            // TUI-path: direct call to apply_stop_workflow_status
            // via run::modify (mirrors `App::stop_workflow_run`'s
            // closure).
            crate::workflow::run::modify(
                "wf_sw_parity_tui",
                crate::workflow::run::apply_stop_workflow_status,
            )
            .expect("tui-path");

            // Daemon-path: workflow_transition handler — wait,
            // that's a different handler. Use the stop_workflow
            // handler instead.
            let state = make_state_arc();
            stop_workflow(
                &state,
                &Caller::operator("op-test"),
                &json!({"run_id": "wf_sw_parity_dae"}),
            )
            .expect("daemon-path");

            // Compare. Modulo run_id + started_at, the relevant
            // fields must match.
            let tui = crate::workflow::run::load_one("wf_sw_parity_tui")
                .expect("load tui");
            let dae = crate::workflow::run::load_one("wf_sw_parity_dae")
                .expect("load dae");
            assert_eq!(
                tui.status, dae.status,
                "status drift (both paths must set Detached)",
            );
            assert_eq!(
                tui.active_role, dae.active_role,
                "active_role drift",
            );
            assert_eq!(
                tui.iteration, dae.iteration,
                "iteration drift",
            );
            assert_eq!(
                tui.paused, dae.paused,
                "paused drift",
            );
            assert_eq!(
                tui.history.len(),
                dae.history.len(),
                "history length drift",
            );
            // Stop doesn't touch history; both should still have
            // just the initial seed entry. Pin role + last_message
            // for completeness.
            for (i, (t, d)) in
                tui.history.iter().zip(dae.history.iter()).enumerate()
            {
                assert_eq!(t.role, d.role, "history[{}].role drift", i);
                assert_eq!(
                    t.last_message, d.last_message,
                    "history[{}].last_message drift",
                    i,
                );
            }
        });
    }

    // --- Continuous Tasks Phase 2: trigger funnel + FRESH executor ----------

    /// Run `f` with `$HOME` pointed at a fresh tempdir so the continuous-task
    /// persistence (`~/.cm/continuous-tasks/<id>/…`) and the per-session MCP
    /// config (`~/.cm/mcp/<uid>/…`) land in an isolated tree. Serialized via
    /// `env_lock` (the whole crate shares one `$HOME`). Mirrors the
    /// continuous module's own `with_temp_home`.
    fn with_continuous_home<F: FnOnce(&std::path::Path)>(f: F) {
        let _g = crate::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
        }
        f(home.path());
        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// Build a Phase-2 continuous task pinned to `worktree`. The caller mutates
    /// the returned value (e.g. set `in_flight` / `last_run`) and `save`s it.
    fn continuous_task(
        task_id: &str,
        engine: crate::continuous::task::Engine,
        run_mode: crate::continuous::task::RunMode,
        worktree: &std::path::Path,
    ) -> crate::continuous::task::ContinuousTask {
        crate::continuous::task::ContinuousTask::new(
            task_id.to_string(),
            "Continuous label".to_string(),
            "ws-cont".to_string(),
            worktree.to_string_lossy().into_owned(),
            engine,
            run_mode,
            crate::continuous::task::Schedule::OnDemand,
            "read NOTES.md first, then continue".to_string(),
        )
    }

    /// A PERSISTENT task's FIRST fire has no pinned session yet
    /// (`current_session_uid` = None), so it BOOTSTRAPS one via a fresh respawn:
    /// mint a uid, spawn (tagged + pinned), and bind `current_session_uid`. The
    /// spawn boundary is spied (no real claude).
    #[test]
    fn trigger_persistent_no_session_bootstraps_fresh_spawn() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let task = continuous_task(
                "ct-persistent",
                crate::continuous::task::Engine::Claude,
                crate::continuous::task::RunMode::Persistent,
                &wt,
            );
            crate::continuous::task::save(&task).expect("save");

            arm_continuous_spawn_spy_for_test();
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let resp = trigger(
                &state,
                &Caller::operator("op-token"),
                &json!({ "task_id": "ct-persistent" }),
            )
            .expect("trigger ok");
            assert_eq!(resp["fired"], json!(true));
            assert_eq!(resp["run_mode"], json!("persistent"));
            let new_uid = resp["session_uid"].as_str().expect("session_uid str");
            assert!(is_valid_session_uid(new_uid), "bootstrap minted a fresh uid");

            // One bootstrap spawn, tagged + bound to the new uid.
            let captured = take_continuous_spawn_spy_for_test();
            assert_eq!(captured.len(), 1, "one bootstrap spawn");
            assert_eq!(captured[0]["continuous_task_id"], json!("ct-persistent"));
            assert_eq!(captured[0]["uid"].as_str().unwrap(), new_uid);

            let reloaded = crate::continuous::task::load_one("ct-persistent").unwrap();
            assert_eq!(reloaded.run_count, 1);
            assert!(reloaded.in_flight.is_none());
            assert_eq!(reloaded.current_session_uid.as_deref(), Some(new_uid));
        });
    }

    /// A second concurrent fire while `in_flight` is set is rejected `busy`
    /// (the spawn-window guard) — no second spawn, no mutation.
    #[test]
    fn trigger_in_flight_returns_busy() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let mut task = continuous_task(
                "ct-busy",
                crate::continuous::task::Engine::Claude,
                crate::continuous::task::RunMode::Fresh,
                &wt,
            );
            task.in_flight = Some(crate::continuous::task::InFlight {
                fire_token: "ft-already-firing".into(),
                session_uid: "ts-dead-beef-0".into(),
                started_at: 1,
            });
            crate::continuous::task::save(&task).expect("save");

            let state = Arc::new(Mutex::new(DaemonState::new()));
            let resp = trigger(
                &state,
                &Caller::operator("op-token"),
                &json!({ "task_id": "ct-busy" }),
            )
            .expect("trigger ok");
            assert_eq!(resp["fired"], json!(false));
            assert_eq!(resp["reason"], json!("busy"));

            // The pre-existing guard is untouched (no clobber).
            let reloaded = crate::continuous::task::load_one("ct-busy").unwrap();
            assert_eq!(
                reloaded.in_flight.as_ref().unwrap().fire_token,
                "ft-already-firing"
            );
            assert_eq!(reloaded.run_count, 0);
        });
    }

    /// FRESH agent (claude/codex) prompts get the `report_done` completion
    /// instruction appended (so a periodic task re-fires); bash + persistent are
    /// left unchanged.
    #[test]
    fn completion_instruction_appended_for_fresh_agent_only() {
        use crate::continuous::task::{Engine, RunMode};
        let p = "do the triage".to_string();
        let fresh_claude = with_completion_instruction(p.clone(), Engine::Claude, RunMode::Fresh);
        assert!(fresh_claude.starts_with("do the triage"), "original prompt preserved");
        assert!(fresh_claude.contains("report_done"), "fresh claude gets the signal");
        assert!(
            with_completion_instruction(p.clone(), Engine::Codex, RunMode::Fresh).contains("report_done"),
            "fresh codex gets the signal",
        );
        assert_eq!(
            with_completion_instruction(p.clone(), Engine::Bash, RunMode::Fresh),
            p,
            "bash runs-and-exits — no signal appended",
        );
        assert_eq!(
            with_completion_instruction(p.clone(), Engine::Claude, RunMode::Persistent),
            p,
            "persistent reuses its session — no re-fire gate, no signal appended",
        );
    }

    /// A caller-supplied `fire_token` that equals `last_run.fire_token` is a
    /// no-op (`duplicate_fire_token`) — idempotent re-delivery doesn't spawn
    /// twice.
    #[test]
    fn trigger_duplicate_fire_token_is_no_op() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let mut task = continuous_task(
                "ct-dup",
                crate::continuous::task::Engine::Claude,
                crate::continuous::task::RunMode::Fresh,
                &wt,
            );
            task.last_run = Some(crate::continuous::task::RunRecord {
                seq: 1,
                fire_token: "ft-seen-before".into(),
                started_at: 1,
                finished_at: None,
                session_uid: Some("ts-prior-1".into()),
                status: crate::continuous::task::RunStatus::Running,
                trigger_source: "operator".into(),
            });
            task.run_count = 1;
            crate::continuous::task::save(&task).expect("save");

            let state = Arc::new(Mutex::new(DaemonState::new()));
            let resp = trigger(
                &state,
                &Caller::operator("op-token"),
                &json!({ "task_id": "ct-dup", "fire_token": "ft-seen-before" }),
            )
            .expect("trigger ok");
            assert_eq!(resp["fired"], json!(false));
            assert_eq!(resp["reason"], json!("duplicate_fire_token"));

            // No new run: run_count unchanged, in_flight never set.
            let reloaded = crate::continuous::task::load_one("ct-dup").unwrap();
            assert_eq!(reloaded.run_count, 1);
            assert!(reloaded.in_flight.is_none());
        });
    }

    /// A FRESH fire composes `start_session` params tagged with
    /// `continuous_task_id`, pinned to the task's durable worktree, with the
    /// engine mapped to the `session_type` vocab — and records the run + clears
    /// the spawn-window guard. The spawn boundary is spied (no real claude).
    #[test]
    fn trigger_fresh_composes_params_tagged_with_continuous_task_id() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let task = continuous_task(
                "ct-fresh",
                crate::continuous::task::Engine::Claude,
                crate::continuous::task::RunMode::Fresh,
                &wt,
            );
            crate::continuous::task::save(&task).expect("save");

            arm_continuous_spawn_spy_for_test();
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let resp = trigger(
                &state,
                &Caller::operator("op-token"),
                &json!({ "task_id": "ct-fresh" }),
            )
            .expect("trigger ok");

            assert_eq!(resp["fired"], json!(true));
            assert_eq!(resp["run_mode"], json!("fresh"));
            let session_uid = resp["session_uid"].as_str().expect("session_uid str");
            assert!(is_valid_session_uid(session_uid), "minted uid is valid");
            let fire_token = resp["fire_token"].as_str().expect("fire_token str");
            assert!(fire_token.starts_with("ft_"), "minted fire_token: {}", fire_token);

            // The composed params reached the spawn boundary tagged + pinned.
            let captured = take_continuous_spawn_spy_for_test();
            assert_eq!(captured.len(), 1, "exactly one spawn per fire");
            let full = &captured[0];
            assert_eq!(full["continuous_task_id"], json!("ct-fresh"));
            assert_eq!(full["task_id"], json!("ct-fresh"));
            assert_eq!(full["session_type"], json!("claude-code"));
            assert_eq!(
                full["working_dir"].as_str().unwrap(),
                wt.to_string_lossy().as_ref(),
                "pinned to the task's durable worktree",
            );
            assert_eq!(full["uid"].as_str().unwrap(), session_uid);

            // last_run recorded, current_session_uid set, run_count bumped,
            // in_flight CLEARED (spawn-window guard only).
            let reloaded = crate::continuous::task::load_one("ct-fresh").unwrap();
            assert!(reloaded.in_flight.is_none(), "in_flight cleared on return");
            assert_eq!(reloaded.run_count, 1);
            assert_eq!(reloaded.current_session_uid.as_deref(), Some(session_uid));
            let last = reloaded.last_run.expect("last_run recorded");
            assert_eq!(last.fire_token, fire_token);
            assert_eq!(last.session_uid.as_deref(), Some(session_uid));

            // A `"fired"` audit line landed in runs.jsonl.
            let runs = std::fs::read_to_string(
                crate::continuous::task::runs_log_path("ct-fresh"),
            )
            .expect("runs.jsonl exists");
            let line: crate::continuous::runlog::RunLogLine =
                serde_json::from_str(runs.lines().next().expect("one line")).unwrap();
            assert_eq!(line.event, "fired");
            assert_eq!(line.run_mode.as_deref(), Some("fresh"));
            assert_eq!(line.session_uid.as_deref(), Some(session_uid));
        });
    }

    /// When a continuous task carries a backing `planning_task_id`, the spawned
    /// session's `task_id` is that planning UUID (so `create_subtask` resolves a
    /// real planning parent), while the `continuous_task_id` tag stays the slug
    /// (so sidebar grouping is unaffected). Mirrors the tagging test above.
    #[test]
    fn trigger_uses_planning_task_id_as_session_task_id_when_set() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let mut task = continuous_task(
                "ct-planning",
                crate::continuous::task::Engine::Claude,
                crate::continuous::task::RunMode::Fresh,
                &wt,
            );
            task.planning_task_id = Some("planning-uuid-abc123".to_string());
            crate::continuous::task::save(&task).expect("save");

            arm_continuous_spawn_spy_for_test();
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let resp = trigger(
                &state,
                &Caller::operator("op-token"),
                &json!({ "task_id": "ct-planning" }),
            )
            .expect("trigger ok");
            assert_eq!(resp["fired"], json!(true));

            let captured = take_continuous_spawn_spy_for_test();
            assert_eq!(captured.len(), 1, "exactly one spawn per fire");
            let full = &captured[0];
            // Session task_id is the PLANNING UUID (create_subtask parent).
            assert_eq!(full["task_id"], json!("planning-uuid-abc123"));
            // Continuous tag stays the SLUG (sidebar grouping unchanged).
            assert_eq!(full["continuous_task_id"], json!("ct-planning"));

            // The field round-trips through persistence.
            let reloaded = crate::continuous::task::load_one("ct-planning").unwrap();
            assert_eq!(
                reloaded.planning_task_id.as_deref(),
                Some("planning-uuid-abc123"),
            );
        });
    }

    /// Phase 3b: every new FRESH fire RESETS the watchdog state — a stale
    /// `investigation_count` / `investigator_uid` left over from the prior run
    /// (e.g. one that was escalated or whose investigator never cleared) must not
    /// leak into the new run. The record step zeroes them alongside writing the
    /// new `Running` `last_run`.
    #[test]
    fn trigger_fresh_resets_watchdog_state() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let mut task = continuous_task(
                "ct-reset",
                crate::continuous::task::Engine::Claude,
                crate::continuous::task::RunMode::Fresh,
                &wt,
            );
            // Stale watchdog state from a prior run.
            task.investigation_count = 2;
            task.investigator_uid = Some("ts-old-investigator-0".into());
            crate::continuous::task::save(&task).expect("save");

            arm_continuous_spawn_spy_for_test();
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let resp = trigger(
                &state,
                &Caller::operator("op-token"),
                &json!({ "task_id": "ct-reset" }),
            )
            .expect("trigger ok");
            assert_eq!(resp["fired"], json!(true));
            let _ = take_continuous_spawn_spy_for_test();

            let reloaded = crate::continuous::task::load_one("ct-reset").unwrap();
            assert_eq!(
                reloaded.investigation_count, 0,
                "fresh fire zeroes the investigation count",
            );
            assert!(
                reloaded.investigator_uid.is_none(),
                "fresh fire clears the investigator binding",
            );
            // The new run starts Running (the watchdog's active-run precondition).
            assert_eq!(
                reloaded.last_run.as_ref().unwrap().status,
                crate::continuous::task::RunStatus::Running,
            );
        });
    }

    /// `compose_continuous_spawn_params` is a thin wrapper that injects
    /// `continuous_task_id` and pins the worktree, delegating argv/env/cols to
    /// `compose_daemon_spawn_params` (no real `claude` binary needed).
    #[test]
    fn compose_continuous_spawn_params_tags_continuous_task_id() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let uid = fresh_test_uid();

            let full = compose_continuous_spawn_params(
                &state,
                &uid,
                "ws-cont",
                "Continuous label",
                crate::continuous::task::Engine::Claude.as_session_type(),
                &wt,
                Some("ct-compose"),
                "ct-compose",
                None,
                120,
                30,
            )
            .expect("compose ok");

            assert_eq!(full["continuous_task_id"], json!("ct-compose"));
            assert_eq!(full["session_type"], json!("claude-code"));
            assert_eq!(full["task_id"], json!("ct-compose"));
            assert_eq!(full["cols"].as_u64(), Some(120));
            // working_dir AND the auto-register worktree hint both pin the tree.
            assert_eq!(full["working_dir"].as_str().unwrap(), wt.to_string_lossy().as_ref());
            assert_eq!(full["worktree_path"].as_str().unwrap(), wt.to_string_lossy().as_ref());
            // argv came from the daemon's build_args (claude + --mcp-config).
            let argv: Vec<String> = full["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            assert_eq!(argv[0], "claude");
            assert!(argv.iter().any(|a| a == "--mcp-config"), "argv: {:?}", argv);
        });
    }

    // --- Continuous Tasks Phase 3b: completion signal + stuck resolution ----

    /// Spawn a real `/bin/sleep` DaemonSession tagged with `continuous_task_id`
    /// and insert it under `uid` — the registry entry the Phase-3b auth + kill
    /// paths read (no real claude). `DaemonSession::spawn` arms its reaper with
    /// `on_exit=None`, so a killed session is LEFT in the registry (the
    /// kill_session-semantics assertions rely on that). Caller cleans up via
    /// `kill_all_sessions`.
    fn insert_continuous_session(
        state: &Arc<Mutex<DaemonState>>,
        uid: &str,
        continuous_task_id: &str,
    ) {
        let mut sp = crate::session::SpawnParams::new(uid, "worker", "/bin/sleep");
        sp.args = vec!["60".to_string()];
        sp.session_type = "claude-code".to_string();
        let mut ds = crate::session::DaemonSession::spawn(sp).expect("spawn /bin/sleep");
        ds.continuous_task_id = Some(continuous_task_id.to_string());
        state
            .lock()
            .unwrap()
            .sessions
            .insert(uid.to_string(), ds);
    }

    /// Build a FRESH task with a `Running` `last_run` pinned to `session_uid`.
    fn fresh_task_running(
        task_id: &str,
        worktree: &std::path::Path,
        session_uid: &str,
    ) -> crate::continuous::task::ContinuousTask {
        let mut task = continuous_task(
            task_id,
            crate::continuous::task::Engine::Claude,
            crate::continuous::task::RunMode::Fresh,
            worktree,
        );
        task.last_run = Some(crate::continuous::task::RunRecord {
            seq: 1,
            fire_token: "ft-stuck-1".into(),
            started_at: 1,
            finished_at: None,
            session_uid: Some(session_uid.to_string()),
            status: crate::continuous::task::RunStatus::Running,
            trigger_source: "operator".into(),
        });
        task.run_count = 1;
        task
    }

    /// `report_done` from the run's OWN session flips Running → Done + sets
    /// finished_at.
    #[test]
    fn report_done_marks_active_run_done() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let uid = fresh_test_uid();
            let task = fresh_task_running("ct-rd", &wt, &uid);
            crate::continuous::task::save(&task).expect("save");

            let state = Arc::new(Mutex::new(DaemonState::new()));
            insert_continuous_session(&state, &uid, "ct-rd");

            let resp = report_done(&state, &Caller::session(uid.clone()), &json!({}))
                .expect("report_done ok");
            assert_eq!(resp["done"], json!(true));
            assert_eq!(resp["task_id"], json!("ct-rd"));

            let reloaded = crate::continuous::task::load_one("ct-rd").unwrap();
            let last = reloaded.last_run.expect("last_run");
            assert_eq!(last.status, crate::continuous::task::RunStatus::Done);
            assert!(last.finished_at.is_some(), "finished_at set");

            kill_all_sessions(&state);
        });
    }

    /// `report_done` from a session that does NOT own the active run is a SOFT
    /// no-op (Ok with done:false), leaving the run Running — never an error.
    #[test]
    fn report_done_uid_mismatch_is_soft_no_op() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            // The active run belongs to a DIFFERENT session uid.
            let task = fresh_task_running("ct-rd-mm", &wt, "ts-other-run-0");
            crate::continuous::task::save(&task).expect("save");

            let caller_uid = fresh_test_uid();
            let state = Arc::new(Mutex::new(DaemonState::new()));
            insert_continuous_session(&state, &caller_uid, "ct-rd-mm");

            let resp = report_done(&state, &Caller::session(caller_uid.clone()), &json!({}))
                .expect("report_done ok (soft no-op)");
            assert_eq!(resp["done"], json!(false));

            let reloaded = crate::continuous::task::load_one("ct-rd-mm").unwrap();
            assert_eq!(
                reloaded.last_run.unwrap().status,
                crate::continuous::task::RunStatus::Running,
                "a non-owning caller must not flip the run",
            );
            kill_all_sessions(&state);
        });
    }

    /// `report_done` rejects an Operator caller and a Session caller that is not
    /// a continuous-task tick (no continuous_task_id).
    #[test]
    fn report_done_rejects_operator_and_untagged_caller() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        // Operator caller: Unauthorized (a continuous tick is always a Session).
        let err = report_done(&state, &Caller::operator("op"), &json!({}))
            .expect_err("operator rejected");
        assert_eq!(err.0, ErrorCode::Unauthorized);

        // Session caller with NO continuous_task_id: Unauthorized.
        let uid = fresh_test_uid();
        {
            let mut sp = crate::session::SpawnParams::new(&uid, "worker", "/bin/sleep");
            sp.args = vec!["60".to_string()];
            sp.session_type = "claude-code".to_string();
            let ds = crate::session::DaemonSession::spawn(sp).expect("spawn");
            state.lock().unwrap().sessions.insert(uid.clone(), ds);
        }
        let err = report_done(&state, &Caller::session(uid.clone()), &json!({}))
            .expect_err("untagged rejected");
        assert_eq!(err.0, ErrorCode::Unauthorized);
        kill_all_sessions(&state);
    }

    /// A continuous-task session exiting cleanly clears its ACTIVE run via
    /// `handle_session_exit` (Running → Done + finished_at).
    #[test]
    fn clean_exit_marks_continuous_run_done() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let uid = fresh_test_uid();
            let task = fresh_task_running("ct-exit", &wt, &uid);
            crate::continuous::task::save(&task).expect("save");

            let state = Arc::new(Mutex::new(DaemonState::new()));
            insert_continuous_session(&state, &uid, "ct-exit");

            // handle_session_exit removes from the registry + drops the
            // DaemonSession (SIGKILLing /bin/sleep) — no kill_all_sessions needed.
            {
                let mut s = state.lock().unwrap();
                handle_session_exit(&mut s, &uid);
            }

            let reloaded = crate::continuous::task::load_one("ct-exit").unwrap();
            let last = reloaded.last_run.expect("last_run");
            assert_eq!(last.status, crate::continuous::task::RunStatus::Done);
            assert!(last.finished_at.is_some());
        });
    }

    /// The clean-exit Done write is DOUBLE-guarded: a non-active session's exit
    /// (uid mismatch) leaves the run Running, and an already-Stuck run (escalate
    /// set it) is NOT clobbered back to Done.
    #[test]
    fn clean_exit_no_op_on_uid_mismatch_and_when_not_running() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();

            // (1) uid mismatch: the active run is owned by a DIFFERENT uid.
            let exiting = fresh_test_uid();
            let mut task = fresh_task_running("ct-exit-mm", &wt, "ts-active-run-0");
            crate::continuous::task::save(&task).expect("save");
            let state = Arc::new(Mutex::new(DaemonState::new()));
            insert_continuous_session(&state, &exiting, "ct-exit-mm");
            {
                let mut s = state.lock().unwrap();
                handle_session_exit(&mut s, &exiting);
            }
            assert_eq!(
                crate::continuous::task::load_one("ct-exit-mm")
                    .unwrap()
                    .last_run
                    .unwrap()
                    .status,
                crate::continuous::task::RunStatus::Running,
                "exit of a non-active session must not touch the active run",
            );

            // (2) status already Stuck (escalate set it): the guard preserves it.
            let stuck_uid = fresh_test_uid();
            task.task_id = "ct-exit-stuck".to_string();
            if let Some(r) = task.last_run.as_mut() {
                r.session_uid = Some(stuck_uid.clone());
                r.status = crate::continuous::task::RunStatus::Stuck;
            }
            crate::continuous::task::save(&task).expect("save");
            insert_continuous_session(&state, &stuck_uid, "ct-exit-stuck");
            {
                let mut s = state.lock().unwrap();
                handle_session_exit(&mut s, &stuck_uid);
            }
            assert_eq!(
                crate::continuous::task::load_one("ct-exit-stuck")
                    .unwrap()
                    .last_run
                    .unwrap()
                    .status,
                crate::continuous::task::RunStatus::Stuck,
                "a clean exit must not clobber an escalated Stuck run back to Done",
            );
        });
    }

    /// `resolve_stuck` `mark_unstuck` extends the watchdog clock
    /// (started_at = now) and clears investigator_uid; the stuck session is NOT
    /// killed (stays Running).
    #[test]
    fn resolve_stuck_mark_unstuck_extends_clock_and_clears_investigator() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let stuck_uid = fresh_test_uid();
            let inv_uid = fresh_test_uid();
            let mut task = fresh_task_running("ct-unstuck", &wt, &stuck_uid);
            task.investigator_uid = Some(inv_uid.clone());
            task.investigation_count = 1;
            crate::continuous::task::save(&task).expect("save");

            let state = Arc::new(Mutex::new(DaemonState::new()));
            insert_continuous_session(&state, &inv_uid, "ct-unstuck");

            let resp = resolve_stuck(
                &state,
                &Caller::session(inv_uid.clone()),
                &json!({ "task_id": "ct-unstuck", "seq": 1, "action": "mark_unstuck" }),
            )
            .expect("mark_unstuck ok");
            assert_eq!(resp["action"], json!("mark_unstuck"));

            let reloaded = crate::continuous::task::load_one("ct-unstuck").unwrap();
            assert!(reloaded.investigator_uid.is_none(), "investigator cleared");
            let last = reloaded.last_run.unwrap();
            assert_eq!(
                last.status,
                crate::continuous::task::RunStatus::Running,
                "stuck session keeps running",
            );
            assert!(last.started_at > 1, "watchdog clock extended to now");
            kill_all_sessions(&state);
        });
    }

    /// `resolve_stuck` `restart` kills the stuck session (kill_session
    /// semantics — left in the registry, operator-kill flag set), clears
    /// investigator_uid, and re-fires a brand-new FRESH run via trigger.
    #[test]
    fn resolve_stuck_restart_kills_and_refires() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let stuck_uid = fresh_test_uid();
            let inv_uid = fresh_test_uid();
            let mut task = fresh_task_running("ct-restart", &wt, &stuck_uid);
            task.investigator_uid = Some(inv_uid.clone());
            crate::continuous::task::save(&task).expect("save");

            let state = Arc::new(Mutex::new(DaemonState::new()));
            insert_continuous_session(&state, &stuck_uid, "ct-restart");
            insert_continuous_session(&state, &inv_uid, "ct-restart");

            arm_continuous_spawn_spy_for_test();
            let resp = resolve_stuck(
                &state,
                &Caller::session(inv_uid.clone()),
                &json!({ "task_id": "ct-restart", "seq": 1, "action": "restart" }),
            )
            .expect("restart ok");
            assert_eq!(resp["action"], json!("restart"));
            assert_eq!(resp["refire"]["fired"], json!(true));

            // The re-fire reached the spawn boundary exactly once, tagged.
            let captured = take_continuous_spawn_spy_for_test();
            assert_eq!(captured.len(), 1, "one re-fire spawn");
            assert_eq!(captured[0]["continuous_task_id"], json!("ct-restart"));

            // The stuck session was killed via kill_session semantics: operator
            // kill flag set, entry LEFT in the registry (not removed).
            {
                let s = state.lock().unwrap();
                let stuck = s
                    .sessions
                    .get(&stuck_uid)
                    .expect("stuck session left in registry");
                assert!(
                    stuck.last_exit.operator_kill_requested(),
                    "stuck session killed via kill_session semantics",
                );
            }

            // A brand-new FRESH run replaced the old one.
            let reloaded = crate::continuous::task::load_one("ct-restart").unwrap();
            assert_eq!(reloaded.run_count, 2);
            assert!(reloaded.investigator_uid.is_none(), "investigator cleared");
            let last = reloaded.last_run.unwrap();
            assert_eq!(last.seq, 2);
            assert_eq!(last.status, crate::continuous::task::RunStatus::Running);
            assert_ne!(
                last.session_uid.as_deref(),
                Some(stuck_uid.as_str()),
                "re-fire minted a new session uid",
            );
            kill_all_sessions(&state);
        });
    }

    /// `resolve_stuck` `escalate` kills the stuck session (kill_session
    /// semantics), flips last_run → Stuck, clears investigator_uid, and writes
    /// an `escalated` audit line.
    #[test]
    fn resolve_stuck_escalate_kills_and_marks_stuck() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let stuck_uid = fresh_test_uid();
            let inv_uid = fresh_test_uid();
            let mut task = fresh_task_running("ct-escalate", &wt, &stuck_uid);
            task.investigator_uid = Some(inv_uid.clone());
            crate::continuous::task::save(&task).expect("save");

            let state = Arc::new(Mutex::new(DaemonState::new()));
            insert_continuous_session(&state, &stuck_uid, "ct-escalate");
            insert_continuous_session(&state, &inv_uid, "ct-escalate");

            let resp = resolve_stuck(
                &state,
                &Caller::session(inv_uid.clone()),
                &json!({
                    "task_id": "ct-escalate",
                    "seq": 1,
                    "action": "escalate",
                    "reason": "needs a human",
                }),
            )
            .expect("escalate ok");
            assert_eq!(resp["action"], json!("escalate"));

            {
                let s = state.lock().unwrap();
                let stuck = s
                    .sessions
                    .get(&stuck_uid)
                    .expect("stuck session left in registry");
                assert!(
                    stuck.last_exit.operator_kill_requested(),
                    "stuck session killed via kill_session semantics",
                );
            }
            let reloaded = crate::continuous::task::load_one("ct-escalate").unwrap();
            assert_eq!(
                reloaded.last_run.unwrap().status,
                crate::continuous::task::RunStatus::Stuck,
            );
            assert!(reloaded.investigator_uid.is_none());

            // An "escalated" audit line landed in runs.jsonl.
            let runs = std::fs::read_to_string(
                crate::continuous::task::runs_log_path("ct-escalate"),
            )
            .expect("runs.jsonl exists");
            assert!(
                runs.lines().any(|l| {
                    serde_json::from_str::<crate::continuous::runlog::RunLogLine>(l)
                        .map(|r| r.event == "escalated")
                        .unwrap_or(false)
                }),
                "an 'escalated' line is present: {}",
                runs,
            );
            kill_all_sessions(&state);
        });
    }

    /// `resolve_stuck` auth: an Operator caller, a tagged-but-not-investigator
    /// session, and a session tagged with a DIFFERENT task are all rejected —
    /// and none mutate the task.
    #[test]
    fn resolve_stuck_rejects_non_investigator_caller() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let stuck_uid = fresh_test_uid();
            let real_inv = fresh_test_uid();
            let mut task = fresh_task_running("ct-auth", &wt, &stuck_uid);
            task.investigator_uid = Some(real_inv.clone());
            crate::continuous::task::save(&task).expect("save");

            let state = Arc::new(Mutex::new(DaemonState::new()));

            // Operator caller → Unauthorized.
            let err = resolve_stuck(
                &state,
                &Caller::operator("op"),
                &json!({ "task_id": "ct-auth", "seq": 1, "action": "mark_unstuck" }),
            )
            .expect_err("operator rejected");
            assert_eq!(err.0, ErrorCode::Unauthorized);

            // A tagged-but-not-investigator session → Unauthorized (gate 2).
            let other = fresh_test_uid();
            insert_continuous_session(&state, &other, "ct-auth");
            let err = resolve_stuck(
                &state,
                &Caller::session(other.clone()),
                &json!({ "task_id": "ct-auth", "seq": 1, "action": "mark_unstuck" }),
            )
            .expect_err("non-investigator rejected");
            assert_eq!(err.0, ErrorCode::Unauthorized);

            // A session tagged with a DIFFERENT task → Unauthorized (gate 1).
            let wrong_tag = fresh_test_uid();
            insert_continuous_session(&state, &wrong_tag, "ct-other-task");
            let err = resolve_stuck(
                &state,
                &Caller::session(wrong_tag.clone()),
                &json!({ "task_id": "ct-auth", "seq": 1, "action": "mark_unstuck" }),
            )
            .expect_err("wrong-tag rejected");
            assert_eq!(err.0, ErrorCode::Unauthorized);

            // No rejected call mutated the task.
            let reloaded = crate::continuous::task::load_one("ct-auth").unwrap();
            assert_eq!(reloaded.investigator_uid.as_deref(), Some(real_inv.as_str()));
            assert_eq!(
                reloaded.last_run.unwrap().status,
                crate::continuous::task::RunStatus::Running,
            );
            kill_all_sessions(&state);
        });
    }

    /// `snapshot_stuck_run` copies the resolved transcript jsonl (named after its
    /// source file), the worktree's NOTES.md, and a metadata.json (carrying the
    /// run record + reason + elapsed) into `stuck/<seq>/`.
    #[test]
    fn snapshot_stuck_run_writes_transcript_notes_and_metadata() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();

            // A resolved transcript file somewhere outside the snapshot dir.
            let transcript = home.join("3f8a-claude-uuid.jsonl");
            std::fs::write(&transcript, "{\"role\":\"assistant\"}\n").unwrap();
            // The worktree's NOTES.md (cross-fire continuity).
            std::fs::write(wt.join("NOTES.md"), "carry-over notes\n").unwrap();

            let mut task = fresh_task_running("ct-snap", &wt, "ts-stuck-aaaa-0");
            task.max_runtime_secs = Some(60);

            let dir = snapshot_stuck_run(
                &task,
                1,
                Some(transcript.as_path()),
                120,
                "watchdog: max_runtime exceeded",
            );
            assert_eq!(
                dir,
                crate::continuous::task::task_dir("ct-snap")
                    .join("stuck")
                    .join("1"),
            );

            // Transcript copied under its SOURCE file name.
            let copied =
                std::fs::read_to_string(dir.join("3f8a-claude-uuid.jsonl")).expect("transcript");
            assert_eq!(copied, "{\"role\":\"assistant\"}\n");
            // NOTES.md copied.
            assert_eq!(
                std::fs::read_to_string(dir.join("NOTES.md")).expect("NOTES.md"),
                "carry-over notes\n",
            );
            // metadata.json carries the run record + reason + elapsed.
            let meta: Value = serde_json::from_str(
                &std::fs::read_to_string(dir.join("metadata.json")).expect("metadata.json"),
            )
            .expect("valid json");
            assert_eq!(meta["task_id"], json!("ct-snap"));
            assert_eq!(meta["seq"], json!(1));
            assert_eq!(meta["elapsed_secs"], json!(120));
            assert_eq!(meta["reason"], json!("watchdog: max_runtime exceeded"));
            assert_eq!(meta["max_runtime_secs"], json!(60));
            assert!(meta["run"].is_object(), "run record serialized: {}", meta);
        });
    }

    /// `snapshot_stuck_run` is best-effort per file: a None transcript path and a
    /// worktree with no NOTES.md still produce the dir + metadata.json, with no
    /// transcript/NOTES artifacts (a trust-dialog hang may never have a
    /// transcript).
    #[test]
    fn snapshot_stuck_run_tolerates_missing_transcript_and_notes() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let task = fresh_task_running("ct-snap-min", &wt, "ts-stuck-bbbb-0");

            let dir = snapshot_stuck_run(&task, 2, None, 5, "watchdog");
            assert!(dir.join("metadata.json").exists(), "metadata.json written");
            assert!(!dir.join("NOTES.md").exists(), "no NOTES.md to copy");
            // The dir holds metadata.json only (no transcript, no NOTES.md).
            let entries: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(entries, vec!["metadata.json".to_string()], "only metadata.json");
        });
    }

    /// `spawn_investigator` composes a FRESH claude spawn labelled "investigator",
    /// tagged with the task's continuous_task_id and pinned to its worktree; on a
    /// successful spawn it binds `investigator_uid` + bumps `investigation_count`
    /// and writes a `stuck` audit line. The spawn boundary is spied (no real
    /// claude).
    #[test]
    fn spawn_investigator_tags_sets_investigator_and_logs_stuck() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let mut task = fresh_task_running("ct-inv", &wt, "ts-stuck-cccc-0");
            task.max_runtime_secs = Some(60);
            crate::continuous::task::save(&task).expect("save");

            let snapshot_dir = crate::continuous::task::task_dir("ct-inv")
                .join("stuck")
                .join("1");

            arm_continuous_spawn_spy_for_test();
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let resp = spawn_investigator(&state, &task, 1, &snapshot_dir)
                .expect("spawn_investigator ok");
            let inv_uid = resp["session_uid"].as_str().expect("session_uid str");
            assert!(is_valid_session_uid(inv_uid), "minted investigator uid valid");

            // Composed params: a claude investigator tagged + pinned to the
            // task's worktree, under the minted uid.
            let captured = take_continuous_spawn_spy_for_test();
            assert_eq!(captured.len(), 1, "exactly one investigator spawn");
            let full = &captured[0];
            assert_eq!(full["label"], json!("investigator"));
            assert_eq!(full["session_type"], json!("claude-code"));
            assert_eq!(full["continuous_task_id"], json!("ct-inv"));
            assert_eq!(full["task_id"], json!("ct-inv"));
            assert_eq!(
                full["working_dir"].as_str().unwrap(),
                wt.to_string_lossy().as_ref(),
                "pinned to the task's worktree",
            );
            assert_eq!(full["uid"].as_str().unwrap(), inv_uid);

            // The investigation was recorded on the task.
            let reloaded = crate::continuous::task::load_one("ct-inv").unwrap();
            assert_eq!(reloaded.investigator_uid.as_deref(), Some(inv_uid));
            assert_eq!(reloaded.investigation_count, 1);

            // A `stuck` audit line with investigation:1 landed in runs.jsonl.
            let runs =
                std::fs::read_to_string(crate::continuous::task::runs_log_path("ct-inv"))
                    .expect("runs.jsonl exists");
            let stuck = runs
                .lines()
                .filter_map(|l| {
                    serde_json::from_str::<crate::continuous::runlog::RunLogLine>(l).ok()
                })
                .find(|r| r.event == "stuck")
                .expect("a 'stuck' line is present");
            assert_eq!(stuck.detail.as_ref().unwrap()["investigation"], json!(1));
        });
    }

    /// `investigator_prompt` pins the EXACT `resolve_stuck` call (task_id + seq),
    /// enumerates the three verdicts, and points at the snapshot dir + worktree.
    #[test]
    fn investigator_prompt_pins_resolve_stuck_call() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let mut task = fresh_task_running("ct-prompt", &wt, "ts-stuck-dddd-0");
            task.max_runtime_secs = Some(120);
            let snapshot_dir = crate::continuous::task::task_dir("ct-prompt")
                .join("stuck")
                .join("3");

            let p = investigator_prompt(&task, 3, &snapshot_dir);
            assert!(p.contains("ct-prompt"), "names the task: {}", p);
            assert!(p.contains("resolve_stuck(task_id=\"ct-prompt\", seq=3"), "exact call: {}", p);
            assert!(p.contains("mark_unstuck"), "lists mark_unstuck");
            assert!(p.contains("restart"), "lists restart");
            assert!(p.contains("escalate"), "lists escalate");
            assert!(
                p.contains(&snapshot_dir.display().to_string()),
                "points at the snapshot dir",
            );
            assert!(
                p.contains(wt.to_string_lossy().as_ref()),
                "points at the worktree",
            );
        });
    }

    // --- Continuous Tasks Phase 3: PERSISTENT executor + memory cap ---------

    /// A PERSISTENT fire to a LIVE pinned session delivers the prompt to the
    /// EXISTING session (REUSING its uid — no new spawn) and records the run. The
    /// live-delivery boundary is spied (no real PTY).
    #[test]
    fn trigger_persistent_delivers_to_live_session() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let mut task = continuous_task(
                "ct-pers-live",
                crate::continuous::task::Engine::Claude,
                crate::continuous::task::RunMode::Persistent,
                &wt,
            );
            task.current_session_uid = Some("ts-live-aaaa-0".to_string());
            crate::continuous::task::save(&task).expect("save");

            arm_continuous_delivery_spy_for_test();
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let resp = trigger(
                &state,
                &Caller::operator("op-token"),
                &json!({ "task_id": "ct-pers-live" }),
            )
            .expect("trigger ok");

            assert_eq!(resp["fired"], json!(true));
            assert_eq!(resp["run_mode"], json!("persistent"));
            // The live pinned session's uid is REUSED (not a freshly-minted one).
            assert_eq!(resp["session_uid"], json!("ts-live-aaaa-0"));

            // Delivery went to the existing session via the persistent path with
            // compact=false (no compact_every).
            let deliveries = take_continuous_delivery_spy_for_test();
            assert_eq!(deliveries.len(), 1, "exactly one delivery");
            assert_eq!(
                deliveries[0],
                (
                    "persistent".to_string(),
                    "ts-live-aaaa-0".to_string(),
                    false
                )
            );

            let reloaded = crate::continuous::task::load_one("ct-pers-live").unwrap();
            assert!(reloaded.in_flight.is_none(), "in_flight cleared on return");
            assert_eq!(reloaded.run_count, 1);
            // current_session_uid UNCHANGED (same live session, no respawn).
            assert_eq!(
                reloaded.current_session_uid.as_deref(),
                Some("ts-live-aaaa-0")
            );
            let last = reloaded.last_run.expect("last_run recorded");
            assert_eq!(last.session_uid.as_deref(), Some("ts-live-aaaa-0"));

            // The audit line carries run_mode "persistent".
            let runs =
                std::fs::read_to_string(crate::continuous::task::runs_log_path("ct-pers-live"))
                    .expect("runs.jsonl exists");
            let line: crate::continuous::runlog::RunLogLine =
                serde_json::from_str(runs.lines().next().expect("one line")).unwrap();
            assert_eq!(line.event, "fired");
            assert_eq!(line.run_mode.as_deref(), Some("persistent"));
        });
    }

    /// A PERSISTENT fire whose pinned session is DEAD (absent from the registry)
    /// promotes to a FRESH respawn: mint a NEW uid, spawn a new session (tagged +
    /// pinned), and REBIND `current_session_uid`. The spawn boundary is spied
    /// (no real claude); no delivery spy, so the real liveness probe runs.
    #[test]
    fn trigger_persistent_dead_session_promotes_to_fresh_respawn() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let mut task = continuous_task(
                "ct-pers-dead",
                crate::continuous::task::Engine::Claude,
                crate::continuous::task::RunMode::Persistent,
                &wt,
            );
            // A pinned uid that is NOT in state.sessions => dead => respawn.
            task.current_session_uid = Some("ts-dead-bbbb-0".to_string());
            crate::continuous::task::save(&task).expect("save");

            arm_continuous_spawn_spy_for_test();
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let resp = trigger(
                &state,
                &Caller::operator("op-token"),
                &json!({ "task_id": "ct-pers-dead" }),
            )
            .expect("trigger ok");

            assert_eq!(resp["fired"], json!(true));
            assert_eq!(resp["run_mode"], json!("persistent"));
            let new_uid = resp["session_uid"].as_str().expect("session_uid str");
            assert!(is_valid_session_uid(new_uid), "respawn minted a fresh uid");
            assert_ne!(new_uid, "ts-dead-bbbb-0", "did NOT reuse the dead uid");

            // Exactly one respawn spawn, tagged + bound to the new uid.
            let captured = take_continuous_spawn_spy_for_test();
            assert_eq!(captured.len(), 1, "one respawn spawn");
            let full = &captured[0];
            assert_eq!(full["continuous_task_id"], json!("ct-pers-dead"));
            assert_eq!(full["uid"].as_str().unwrap(), new_uid);

            // current_session_uid REBOUND to the new uid; run recorded.
            let reloaded = crate::continuous::task::load_one("ct-pers-dead").unwrap();
            assert!(reloaded.in_flight.is_none());
            assert_eq!(reloaded.run_count, 1);
            assert_eq!(reloaded.current_session_uid.as_deref(), Some(new_uid));
        });
    }

    /// PERSISTENT auto-compact: with `compact_every = Some(2)`, the 2nd run
    /// (seq 2, 2%2==0) `/clear`s before delivering; the 3rd (seq 3, 3%2==1) does
    /// not. Two consecutive fires exercise the modulo gate.
    #[test]
    fn trigger_persistent_compact_after_n_clears_at_nth() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let mut task = continuous_task(
                "ct-pers-compact",
                crate::continuous::task::Engine::Claude,
                crate::continuous::task::RunMode::Persistent,
                &wt,
            );
            task.current_session_uid = Some("ts-live-cccc-0".to_string());
            task.compact_every = Some(2);
            // Prior run_count=1 => this fire is seq=2 (the Nth).
            task.run_count = 1;
            crate::continuous::task::save(&task).expect("save");

            arm_continuous_delivery_spy_for_test();
            let state = Arc::new(Mutex::new(DaemonState::new()));

            // Fire #1: seq=2 => compact.
            trigger(
                &state,
                &Caller::operator("op"),
                &json!({ "task_id": "ct-pers-compact" }),
            )
            .expect("fire 1");
            // Fire #2: seq=3 => no compact.
            trigger(
                &state,
                &Caller::operator("op"),
                &json!({ "task_id": "ct-pers-compact" }),
            )
            .expect("fire 2");

            let deliveries = take_continuous_delivery_spy_for_test();
            assert_eq!(deliveries.len(), 2);
            assert!(deliveries[0].2, "Nth fire (seq 2) compacts");
            assert!(!deliveries[1].2, "non-Nth fire (seq 3) does not compact");

            let reloaded = crate::continuous::task::load_one("ct-pers-compact").unwrap();
            assert_eq!(reloaded.run_count, 3);
        });
    }

    /// `compose_continuous_spawn_params` wraps argv in `systemd-run` and sets the
    /// all-or-nothing memory-cap triple when the cgroup prefix is backed (test
    /// seam), and runs UNCAPPED (plain argv, no triple) when the prefix is absent
    /// (graceful degrade — the daemon has no preflight, so a missing user manager
    /// must not fail the spawn).
    #[test]
    fn compose_continuous_spawn_params_threads_memory_cap_triple() {
        with_continuous_home(|home| {
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).unwrap();
            let state = Arc::new(Mutex::new(DaemonState::new()));

            // (a) Backed prefix (a real temp dir) => wrapped + triple set.
            let cgroup = home.join("fake-app.slice");
            std::fs::create_dir_all(&cgroup).unwrap();
            set_configured_cap_prefix_override_for_test(Some(
                cgroup.to_string_lossy().into_owned(),
            ));
            let uid = fresh_test_uid();
            let full = compose_continuous_spawn_params(
                &state,
                &uid,
                "ws-cont",
                "Continuous label",
                crate::continuous::task::Engine::Claude.as_session_type(),
                &wt,
                Some("ct-cap"),
                "ct-cap",
                Some(536_870_912), // 512 MiB per-task override
                80,
                24,
            )
            .expect("compose ok");
            let argv: Vec<String> = full["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            assert_eq!(argv[0], "systemd-run", "argv wrapped: {:?}", argv);
            assert!(
                argv.iter().any(|a| a == "MemoryMax=536870912"),
                "argv: {:?}",
                argv
            );
            assert_eq!(full["memory_cap_bytes"].as_u64(), Some(536_870_912));
            assert_eq!(full["memory_cap_hard_bytes"].as_u64(), Some(536_870_912));
            assert_eq!(
                full["cgroup_prefix"].as_str(),
                Some(cgroup.to_string_lossy().as_ref())
            );
            // The tagged continuous_task_id survives the wrap.
            assert_eq!(full["continuous_task_id"], json!("ct-cap"));

            // (b) Absent prefix => graceful degrade => plain argv, no triple.
            let absent = home.join("does-not-exist");
            set_configured_cap_prefix_override_for_test(Some(
                absent.to_string_lossy().into_owned(),
            ));
            let uid2 = fresh_test_uid();
            let full2 = compose_continuous_spawn_params(
                &state,
                &uid2,
                "ws-cont",
                "Continuous label",
                crate::continuous::task::Engine::Claude.as_session_type(),
                &wt,
                Some("ct-cap2"),
                "ct-cap2",
                Some(536_870_912),
                80,
                24,
            )
            .expect("compose ok");
            let argv2: Vec<String> = full2["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            assert_eq!(argv2[0], "claude", "uncapped plain argv: {:?}", argv2);
            assert!(
                full2.get("memory_cap_bytes").is_none(),
                "no cap triple when degraded"
            );

            set_configured_cap_prefix_override_for_test(None);
        });
    }

    // --- Continuous Tasks Phase 2: continuous.* CRUD handlers ---------------

    /// `continuous.create` resolves the repo, creates the durable worktree
    /// ONCE, registers the workspace + task→workspace binding, and writes the
    /// authoritative record — and `continuous.list` then surfaces it in the
    /// health projection. A second create for the same id reuses the record
    /// (`created=false`). Engine `claude` does NOT spawn here, so no real
    /// `claude` binary is needed (firing is `trigger`'s job).
    #[test]
    fn continuous_create_then_list_round_trip() {
        with_home_and_repo("continuousrepo", |home, name| {
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let resp = continuous_create(
                &state,
                &json!({
                    "task_id": "bug-triage",
                    "label": "Bug triage",
                    "engine": "claude",
                    "run_mode": "fresh",
                    "default_prompt": "read NOTES.md first, then continue",
                    "repo_url": name,
                    "slug": "bug-triage",
                }),
            )
            .expect("create ok");

            assert_eq!(resp["created"], json!(true));
            assert_eq!(resp["task_id"], json!("bug-triage"));
            let expected_wt = home.join(".cm/worktrees/continuousrepo-bug-triage");
            assert_eq!(
                resp["worktree_path"].as_str().unwrap(),
                expected_wt.to_string_lossy().as_ref(),
            );
            assert!(expected_wt.join(".git").exists(), "worktree dir must exist");

            // Workspace + task→workspace binding registered in the snapshot.
            let ws_id = resp["workspace_id"].as_str().unwrap().to_string();
            {
                let s = state.lock().unwrap();
                assert!(s.workspaces.contains_key(&ws_id), "workspace registered");
                assert_eq!(
                    s.task_workspaces.get("bug-triage").map(|x| x.as_str()),
                    Some(ws_id.as_str()),
                    "task→workspace bound",
                );
            }

            // Authoritative record on disk carries the config.
            let on_disk = crate::continuous::task::load_one("bug-triage").expect("record");
            assert_eq!(on_disk.engine, crate::continuous::task::Engine::Claude);
            assert_eq!(on_disk.run_mode, crate::continuous::task::RunMode::Fresh);
            assert_eq!(on_disk.repo.as_deref(), Some(name));
            assert_eq!(on_disk.workspace_id, ws_id);

            // Idempotent: a second create reuses the record (created=false).
            let resp2 = continuous_create(
                &state,
                &json!({
                    "task_id": "bug-triage",
                    "label": "Bug triage (2)",
                    "default_prompt": "different prompt",
                    "repo_url": name,
                    "slug": "bug-triage",
                }),
            )
            .expect("second create ok");
            assert_eq!(resp2["created"], json!(false), "record collision reuses");
            // The original record was NOT clobbered.
            let reread = crate::continuous::task::load_one("bug-triage").unwrap();
            assert_eq!(reread.label, "Bug triage", "original label preserved");

            // `continuous.list` surfaces the health projection.
            let list = continuous_list(&state, &json!({})).expect("list ok");
            let tasks = list["tasks"].as_array().expect("tasks array");
            assert_eq!(tasks.len(), 1, "exactly one continuous task");
            let t = &tasks[0];
            assert_eq!(t["task_id"], json!("bug-triage"));
            assert_eq!(t["engine"], json!("claude"));
            assert_eq!(t["run_mode"], json!("fresh"));
            assert_eq!(t["paused"], json!(false));
            assert_eq!(t["run_count"], json!(0));
            assert_eq!(t["in_flight"], json!(false));
            assert_eq!(t["last_outcome"], Value::Null, "no run yet");
            // schedule defaults to on_demand (internally-tagged on `kind`).
            assert_eq!(t["schedule"]["kind"], json!("on_demand"));
        });
    }

    /// `continuous.pause` flips the `paused` flag (persisted), and a pause for
    /// an unknown id is a clean `NotFound` (not an opaque io error).
    #[test]
    fn continuous_pause_toggles_paused_flag() {
        with_home_and_repo("pauserepo", |_home, name| {
            let state = Arc::new(Mutex::new(DaemonState::new()));
            continuous_create(
                &state,
                &json!({
                    "task_id": "nightly",
                    "label": "Nightly",
                    "default_prompt": "go",
                    "repo_url": name,
                }),
            )
            .expect("create ok");

            // Pause.
            let resp = continuous_pause(&state, &json!({ "task_id": "nightly", "paused": true }))
                .expect("pause ok");
            assert_eq!(resp["paused"], json!(true));
            assert!(
                crate::continuous::task::load_one("nightly").unwrap().paused,
                "paused persisted",
            );

            // Resume.
            let resp2 =
                continuous_pause(&state, &json!({ "task_id": "nightly", "paused": false }))
                    .expect("resume ok");
            assert_eq!(resp2["paused"], json!(false));
            assert!(
                !crate::continuous::task::load_one("nightly").unwrap().paused,
                "resume persisted",
            );

            // Unknown id → clean NotFound.
            let err = continuous_pause(&state, &json!({ "task_id": "ghost", "paused": true }))
                .expect_err("missing task is an error");
            assert_eq!(err.0, ErrorCode::NotFound);
        });
    }

    /// `continuous.delete` retires the record (state.json gone, list empty) and
    /// drops the in-memory manifest registration. An unknown id is `NotFound`.
    #[test]
    fn continuous_delete_retires_record_and_unregisters() {
        with_home_and_repo("deleterepo", |_home, name| {
            let state = Arc::new(Mutex::new(DaemonState::new()));
            let created = continuous_create(
                &state,
                &json!({
                    "task_id": "ephemeral",
                    "label": "Ephemeral",
                    "default_prompt": "go",
                    "repo_url": name,
                }),
            )
            .expect("create ok");
            let ws_id = created["workspace_id"].as_str().unwrap().to_string();

            let resp = continuous_delete(&state, &json!({ "task_id": "ephemeral" }))
                .expect("delete ok");
            assert_eq!(resp["deleted"], json!(true));

            // Record gone; list empty.
            assert!(crate::continuous::task::load_one("ephemeral").is_none());
            let list = continuous_list(&state, &json!({})).expect("list ok");
            assert_eq!(list["tasks"].as_array().unwrap().len(), 0);

            // In-memory registration dropped.
            {
                let s = state.lock().unwrap();
                assert!(!s.workspaces.contains_key(&ws_id));
                assert!(!s.task_workspaces.contains_key("ephemeral"));
            }

            // Deleting an unknown id is NotFound.
            let err = continuous_delete(&state, &json!({ "task_id": "ghost" }))
                .expect_err("missing task is an error");
            assert_eq!(err.0, ErrorCode::NotFound);
        });
    }

    // ============================================================
    // Subtask CRUD — create_subtask / list_subtasks / mark_subtask_done
    // ============================================================

    #[derive(Clone, Debug)]
    struct StubReq {
        method: String,
        path: String,
        body: String,
    }

    struct StubApi {
        port: u16,
        requests: Arc<Mutex<Vec<StubReq>>>,
    }

    /// Multi-request loop-accept HTTP stub. Unlike
    /// `planning_client::spawn_stub_api_for_test` (one connection only),
    /// the subtask handlers make several sequential calls (GET parent +
    /// POST; GET list; GET + PATCH); this routes each inbound
    /// `(method, path, body)` through `responder` and records it for
    /// assertions. Each `PlanningApiCreds::agent()` is fresh, so every
    /// call opens a NEW connection — the loop handles them in order.
    fn spawn_routed_stub<F>(responder: F) -> StubApi
    where
        F: Fn(&str, &str, &str) -> (u16, String) + Send + 'static,
    {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::<StubReq>::new()));
        let req_sink = Arc::clone(&requests);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut stream = match conn {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let mut buf = [0u8; 8192];
                let mut acc: Vec<u8> = Vec::new();
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            acc.extend_from_slice(&buf[..n]);
                            if let Some(he) = acc.windows(4).position(|w| w == b"\r\n\r\n") {
                                let hs = std::str::from_utf8(&acc[..he]).unwrap_or("");
                                let cl = hs
                                    .lines()
                                    .find_map(|l| {
                                        let ll = l.to_ascii_lowercase();
                                        ll.strip_prefix("content-length:")
                                            .and_then(|v| v.trim().parse::<usize>().ok())
                                    })
                                    .unwrap_or(0);
                                if acc.len() - (he + 4) >= cl {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                let raw = String::from_utf8_lossy(&acc).into_owned();
                let first = raw.lines().next().unwrap_or("");
                let mut parts = first.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let body = raw
                    .split_once("\r\n\r\n")
                    .map(|(_, b)| b.to_string())
                    .unwrap_or_default();
                req_sink.lock().unwrap().push(StubReq {
                    method: method.clone(),
                    path: path.clone(),
                    body: body.clone(),
                });
                let (status, resp_body) = responder(&method, &path, &body);
                let response = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    resp_body.as_bytes().len(),
                    resp_body,
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        StubApi { port, requests }
    }

    fn set_api_env(port: u16) {
        unsafe {
            std::env::set_var("CM_API_URL", format!("http://127.0.0.1:{}", port));
            std::env::set_var("CM_API_TOKEN", "test-token");
        }
    }

    fn clear_api_env() {
        unsafe {
            std::env::remove_var("CM_API_URL");
            std::env::remove_var("CM_API_TOKEN");
        }
    }

    /// Seed a LIVE, tasked caller session into `state.sessions`.
    fn seed_tasked_caller(
        state: &Arc<Mutex<DaemonState>>,
        uid: &str,
        ws_id: &str,
        task_id: &str,
    ) {
        let mut sp = SpawnParams::new(uid, format!("caller-{}", uid), "/bin/sleep");
        sp.args = vec!["120".to_string()];
        sp.workspace_id = ws_id.to_string();
        sp.task_id = Some(task_id.to_string());
        let sess = crate::session::DaemonSession::spawn(sp).expect("spawn caller");
        state.lock().unwrap().sessions.insert(uid.to_string(), sess);
    }

    /// Branch-mode `create_subtask`: builds the `cm-sub/<chain>-<short>`
    /// worktree on disk, registers a FRESH workspace (worktree_path !=
    /// main_repo_path), seeds the headless auth edge (task_tree +
    /// task_workspaces + bindings), and POSTs the create body carrying
    /// `parent_task_id` + `status=running` + the computed slug/branch.
    #[test]
    fn create_subtask_branch_mode_builds_worktree_registers_and_forwards_parent_id() {
        with_home_and_repo("subtaskrepo", |home, name| {
            let repo = home.join("code/projects").join(name);
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = ManifestWorkspace::default();
                ws.id = "ws-parent".to_string();
                ws.worktree_path = Some(repo.clone());
                ws.main_repo_path = Some(repo.clone());
                ws.repo_url = Some(name.to_string());
                s.workspaces.insert("ws-parent".to_string(), ws);
            }
            seed_tasked_caller(&state, "ts-orch", "ws-parent", "task-parent");

            let name_owned = name.to_string();
            let stub = spawn_routed_stub(move |method, path, _body| {
                if method == "GET" && path == "/tasks/task-parent" {
                    (
                        200,
                        format!(
                            r#"{{"id":"task-parent","name":"Parent Task","repo_url":"{}","project":"proj","status":"running","worktree_mode":"inherit","wip_branch":null,"parent_task_id":null}}"#,
                            name_owned
                        ),
                    )
                } else if method == "POST" && path == "/tasks" {
                    (
                        200,
                        r#"{"id":"task-child-1","name":"branchy","status":"running","worktree_mode":"branch","parent_task_id":"task-parent"}"#
                            .to_string(),
                    )
                } else {
                    (404, r#"{"detail":"unexpected"}"#.to_string())
                }
            });
            set_api_env(stub.port);

            let result = create_subtask(
                &state,
                &json!({"name": "branchy", "worktree_mode": "branch"}),
                Some("ts-orch"),
            )
            .expect("create_subtask ok");

            assert_eq!(result["task_id"], "task-child-1");
            let wt = result["worktree_path"].as_str().unwrap().to_string();
            assert!(
                wt.contains("cm-sub-parent-task-branchy-"),
                "worktree path should encode the cm-sub branch: {}",
                wt
            );
            assert!(
                std::path::Path::new(&wt).exists(),
                "branch-mode worktree must exist on disk: {}",
                wt
            );

            {
                let s = state.lock().unwrap();
                let new_ws = s
                    .workspaces
                    .values()
                    .find(|w| w.id != "ws-parent")
                    .expect("a fresh workspace was registered for the subtask");
                assert_eq!(
                    new_ws
                        .worktree_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string()),
                    Some(wt.clone()),
                );
                assert_ne!(
                    new_ws.worktree_path, new_ws.main_repo_path,
                    "branch-mode worktree must differ from the main repo",
                );
                assert_eq!(
                    s.task_tree.get("task-child-1"),
                    Some(&Some("task-parent".to_string())),
                    "headless auth edge must be seeded",
                );
                assert!(s.task_workspaces.contains_key("task-child-1"));
                assert!(s.bindings.contains_key("task-child-1"));
            }

            let reqs = stub.requests.lock().unwrap();
            let post = reqs
                .iter()
                .find(|r| r.method == "POST" && r.path == "/tasks")
                .expect("POST /tasks captured");
            let body: Value = serde_json::from_str(&post.body).expect("post body json");
            assert_eq!(body["parent_task_id"], "task-parent");
            assert_eq!(body["status"], "running");
            assert_eq!(body["worktree_mode"], "branch");
            assert_eq!(body["source"], "claude");
            assert_eq!(body["is_cloud"], false);
            assert_eq!(body["repo_branch"], "main");
            assert_eq!(body["repo_url"], name);
            let slug = body["slug"].as_str().unwrap();
            assert!(slug.starts_with("parent-task-branchy-"), "slug: {}", slug);
            let wip = body["wip_branch"].as_str().unwrap();
            assert!(
                wip.starts_with("cm-sub/parent-task-branchy-"),
                "wip_branch: {}",
                wip
            );
            drop(reqs);

            kill_all_sessions(&state);
            clear_api_env();
        });
    }

    /// `set_subtask_status` — Session-callable headless status PATCH. Sets the
    /// caller's OWN task (task_id omitted) via PATCH /tasks/{id}, no
    /// worktree/session teardown. Validates the status enum + self-or-
    /// descendant auth, both BEFORE any API call.
    #[test]
    fn set_subtask_status_patches_own_task_no_teardown() {
        with_home_and_repo("ssrepo", |_home, _name| {
            let state = make_state_arc();
            seed_tasked_caller(&state, "ts-self", "ws-self", "task-self");

            let stub = spawn_routed_stub(move |method, path, _body| {
                if method == "PATCH" && path == "/tasks/task-self" {
                    (200, r#"{"id":"task-self","status":"blocked"}"#.to_string())
                } else {
                    (404, r#"{"detail":"unexpected"}"#.to_string())
                }
            });
            set_api_env(stub.port);

            // Default target = own task; status flips to blocked.
            let result =
                set_subtask_status(&state, &json!({ "status": "blocked" }), Some("ts-self"))
                    .expect("set_subtask_status ok");
            assert_eq!(result["task_id"], "task-self");
            assert_eq!(result["status"], "blocked");

            // The PATCH carried EXACTLY {"status":"blocked"} (status-only).
            {
                let reqs = stub.requests.lock().unwrap();
                let patch = reqs
                    .iter()
                    .find(|r| r.method == "PATCH" && r.path == "/tasks/task-self")
                    .expect("PATCH /tasks/task-self captured");
                let body: Value = serde_json::from_str(&patch.body).expect("patch body json");
                assert_eq!(body["status"], "blocked");
                assert_eq!(body.as_object().unwrap().len(), 1, "status-only PATCH");
            }

            // Invalid status → InvalidParams, rejected before any API call.
            let bad = set_subtask_status(&state, &json!({ "status": "bogus" }), Some("ts-self"))
                .unwrap_err();
            assert_eq!(bad.0, ErrorCode::InvalidParams);

            // Operator / taskless caller (None) → Unauthorized.
            let none_caller =
                set_subtask_status(&state, &json!({ "status": "done" }), None).unwrap_err();
            assert_eq!(none_caller.0, ErrorCode::Unauthorized);

            // A target that's neither the caller's task nor a descendant →
            // Unauthorized (cross-task scoping rejected, before any API call).
            let cross = set_subtask_status(
                &state,
                &json!({ "status": "done", "task_id": "task-other" }),
                Some("ts-self"),
            )
            .unwrap_err();
            assert_eq!(cross.0, ErrorCode::Unauthorized);

            kill_all_sessions(&state);
            clear_api_env();
        });
    }

    /// HARDENING: when the parent planning row is GONE (404 on
    /// `GET /tasks/{parent}` — e.g. an `A-x` board delete hard-removed it),
    /// create_subtask must NOT fail. It falls back to creating a TOP-LEVEL
    /// task: `parent_task_id = null` (so the `tasks.parent_task_id` FK can't
    /// reject the insert), repo_url + worktree from the still-live caller
    /// workspace, and a bare-leaf slug (no ancestry to walk).
    #[test]
    fn create_subtask_missing_parent_falls_back_to_top_level() {
        with_home_and_repo("subtaskrepo", |home, name| {
            let repo = home.join("code/projects").join(name);
            let state = make_state_arc();
            {
                let mut s = state.lock().unwrap();
                let mut ws = ManifestWorkspace::default();
                ws.id = "ws-parent".to_string();
                ws.worktree_path = Some(repo.clone());
                ws.main_repo_path = Some(repo.clone());
                ws.repo_url = Some(name.to_string());
                s.workspaces.insert("ws-parent".to_string(), ws);
            }
            seed_tasked_caller(&state, "ts-orch", "ws-parent", "task-parent");

            let stub = spawn_routed_stub(move |method, path, _body| {
                if method == "GET" && path == "/tasks/task-parent" {
                    // Parent row was deleted off the board.
                    (404, r#"{"detail":"Task not found"}"#.to_string())
                } else if method == "POST" && path == "/tasks" {
                    (
                        200,
                        r#"{"id":"task-child-top","name":"topchild","status":"running","worktree_mode":"branch","parent_task_id":null}"#
                            .to_string(),
                    )
                } else {
                    (404, r#"{"detail":"unexpected"}"#.to_string())
                }
            });
            set_api_env(stub.port);

            let result = create_subtask(
                &state,
                &json!({"name": "topchild", "worktree_mode": "branch"}),
                Some("ts-orch"),
            )
            .expect("create_subtask must SUCCEED even when the parent row is gone");
            assert_eq!(result["task_id"], "task-child-top");

            // Registered as TOP-LEVEL (no parent) in the headless auth tree.
            {
                let s = state.lock().unwrap();
                assert_eq!(
                    s.task_tree.get("task-child-top"),
                    Some(&None),
                    "a parentless subtask must register as top-level (None)",
                );
            }

            // The POST body carries a NULL parent_task_id (never a dangling
            // FK) and a bare-leaf slug (no parent chain), with repo_url taken
            // from the live workspace.
            let reqs = stub.requests.lock().unwrap();
            let post = reqs
                .iter()
                .find(|r| r.method == "POST" && r.path == "/tasks")
                .expect("POST /tasks captured");
            let body: Value = serde_json::from_str(&post.body).expect("post body json");
            assert!(
                body["parent_task_id"].is_null(),
                "parent_task_id must be null in the top-level fallback, got {}",
                body["parent_task_id"],
            );
            assert_eq!(body["repo_url"], name, "repo_url falls back to the workspace");
            let slug = body["slug"].as_str().unwrap();
            assert!(slug.starts_with("topchild-"), "bare-leaf slug expected: {}", slug);
            drop(reqs);

            kill_all_sessions(&state);
            clear_api_env();
        });
    }

    /// A taskless Session caller (and an Operator caller, which resolves
    /// to `caller_uid = None`) are both rejected with Unauthorized —
    /// create_subtask needs a parent task to fork off.
    #[test]
    fn create_subtask_taskless_and_operator_callers_rejected() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        insert_session(&state, "ts-taskless", "ws-x");
        // Operator caller (caller_uid = None).
        let err = create_subtask(&state, &json!({"name": "x"}), None)
            .expect_err("operator caller must be rejected");
        assert_eq!(err.0, ErrorCode::Unauthorized);
        // Taskless Session caller.
        let err2 = create_subtask(&state, &json!({"name": "x"}), Some("ts-taskless"))
            .expect_err("taskless caller must be rejected");
        assert_eq!(err2.0, ErrorCode::Unauthorized);
        assert!(
            err2.1.contains("propose_task"),
            "taskless rejection should point at propose_task: {}",
            err2.1
        );
        kill_all_sessions(&state);
    }

    /// list_subtasks (scoped to the caller's own task) + mark_subtask_done
    /// round-trip against the stubbed API. The marked subtask is
    /// inherit-mode so no git runs — mark just GETs the row then PATCHes
    /// `status=done`.
    #[test]
    fn list_and_mark_subtask_round_trip_via_api() {
        let _g = crate::test_support::env_lock();
        let state = make_state_arc();
        seed_tasked_caller(&state, "ts-orch", "ws-1", "task-parent");
        {
            let mut s = state.lock().unwrap();
            s.task_tree.insert("task-parent".to_string(), None);
            s.task_tree
                .insert("child-1".to_string(), Some("task-parent".to_string()));
        }
        let stub = spawn_routed_stub(|method, path, _body| match (method, path) {
            ("GET", "/tasks") => (
                200,
                r#"[
                    {"id":"child-1","name":"c1","status":"running","worktree_mode":"branch","wip_branch":"cm-sub/x","parent_task_id":"task-parent"},
                    {"id":"other","name":"o","status":"running","worktree_mode":"inherit","wip_branch":null,"parent_task_id":"task-zzz"}
                ]"#
                .to_string(),
            ),
            ("GET", "/tasks/child-1") => (
                200,
                r#"{"id":"child-1","worktree_mode":"inherit","status":"running"}"#.to_string(),
            ),
            ("PATCH", "/tasks/child-1") => {
                (200, r#"{"id":"child-1","status":"done"}"#.to_string())
            }
            _ => (404, r#"{"detail":"unexpected"}"#.to_string()),
        });
        set_api_env(stub.port);

        let arr = list_subtasks(&state, &json!({}), Some("ts-orch")).expect("list ok");
        let items = arr.as_array().expect("array");
        assert_eq!(items.len(), 1, "only the child under task-parent is returned");
        assert_eq!(items[0]["task_id"], "child-1");
        assert_eq!(items[0]["status"], "running");
        assert_eq!(items[0]["worktree_mode"], "branch");
        assert_eq!(items[0]["wip_branch"], "cm-sub/x");

        let res = mark_subtask_done(&state, &json!({"task_id": "child-1"}), Some("ts-orch"))
            .expect("mark ok");
        assert_eq!(res["ok"], true);
        assert_eq!(res["worktree_removed"], false);

        let reqs = stub.requests.lock().unwrap();
        let patch = reqs
            .iter()
            .find(|r| r.method == "PATCH" && r.path == "/tasks/child-1")
            .expect("PATCH /tasks/child-1 captured");
        let body: Value = serde_json::from_str(&patch.body).expect("patch body json");
        assert_eq!(body["status"], "done");
        drop(reqs);

        kill_all_sessions(&state);
        clear_api_env();
    }

    /// Auth: list_subtasks / mark_subtask_done targeting a task that is
    /// NOT the caller's task or a descendant → Unauthorized (rejected
    /// under the lock, before any API call).
    #[test]
    fn list_and_mark_reject_non_descendant_target() {
        let state = make_state_arc();
        seed_tasked_caller(&state, "ts-orch", "ws-1", "task-parent");
        {
            let mut s = state.lock().unwrap();
            s.task_tree.insert("task-parent".to_string(), None);
            s.task_tree.insert("unrelated".to_string(), None);
        }
        let le = list_subtasks(&state, &json!({"task_id": "unrelated"}), Some("ts-orch"))
            .expect_err("list must reject a non-descendant target");
        assert_eq!(le.0, ErrorCode::Unauthorized);
        let me = mark_subtask_done(&state, &json!({"task_id": "unrelated"}), Some("ts-orch"))
            .expect_err("mark must reject a non-descendant target");
        assert_eq!(me.0, ErrorCode::Unauthorized);
        kill_all_sessions(&state);
    }
}
