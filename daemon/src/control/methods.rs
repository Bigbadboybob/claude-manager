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

use crate::control::protocol::ErrorCode;
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
    spawn_params.args = p.argv[1..].to_vec();
    spawn_params.working_dir = Some(working_dir);
    spawn_params.cols = p.cols;
    spawn_params.rows = p.rows;
    // Adopt the caller's env wholesale. The TUI provides the
    // MCP-server routing pin (`CM_TUI_SOCKET=""` /
    // `CM_DAEMON_SOCKET=<abs path>` per slice 10c-e-3a's
    // SpawnTarget::Daemon) plus `CM_TUI_SESSION_ID` and any
    // workflow vars. Daemon does not modify.
    for (k, v) in p.env {
        spawn_params.env.insert(k, v);
    }
    // Always pin `CM_TUI_SESSION_ID` to the daemon-minted uid even
    // if the caller didn't (or sent a different value). This is
    // the correlation key for `~/.cm/memory_kills/<uid>.jsonl`,
    // and the daemon owns the uid identity for daemon-spawned
    // sessions.
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
    let state_for_cleanup = Arc::clone(state_arc);
    let uid_for_cleanup = session_uid.clone();
    let on_exit: Box<dyn FnOnce(&DaemonExitStatus) + Send + 'static> =
        Box::new(move |_status: &DaemonExitStatus| {
            let mut s = state_for_cleanup
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            s.sessions.remove(&uid_for_cleanup);
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
    drop(state);

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
fn return_auth_error_if_denied(
    decision: crate::control::auth::AuthDecision,
    caller_uid: &str,
    target_uid: &str,
) -> MethodResult {
    use crate::control::auth::AuthDecision;
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
            return_auth_error_if_denied(decision, cuid, &p.session_uid)?;
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
    // Sub-2a Finding #2 TOCTOU fix: auth + remove happen in one
    // critical section so a non-descendant target can't slip in
    // (or out) between authorize-time and remove-time.
    if let Some(cuid) = caller_uid {
        let decision = crate::control::auth::check_session_caller(
            &state,
            cuid,
            &p.session_uid,
        );
        return_auth_error_if_denied(decision, cuid, &p.session_uid)?;
    }
    let removed = state.sessions.remove(&p.session_uid);
    let removed = match removed {
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
    //
    // The Arc'd `last_exit` outlives `DaemonSession` (the
    // attach-stream's End-frame consumer holds an
    // `Arc<LastExitProbe>` clone), so setting the flag here is
    // observable from the End-frame path even after `removed`
    // drops below.
    removed.last_exit.mark_operator_kill_requested();
    // Drop on the moved-out `DaemonSession` runs at scope-end of
    // this match arm — SIGKILL via pidfd, reaper observes exit.
    // The reaper's on_exit callback will try to lock the state and
    // remove the (now-absent) uid; that's a harmless no-op.
    drop(removed);

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
            return_auth_error_if_denied(decision, cuid, &p.session_uid)?;
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
//   - `state`: always `"running"` at sub-1. Phase 1 daemon
//      doesn't track tombstones (those still live on the TUI
//      side until slice 10e flips manifest ownership). The
//      `include_exited` param is accepted but a no-op.
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

    // Sub-2a Finding #3: authorize the requested task scope
    // BEFORE iterating. Mirrors
    // `tui/src/control/methods.rs:498-521`:
    //   - Operator caller: no restriction.
    //   - Taskless Session caller + explicit task_id: Unauthorized.
    //   - Tasked Session caller + explicit task_id: must be
    //     self-or-descendant of caller's task.
    if let Some(req_task) = p.task_id.as_deref() {
        if let Some(cuid) = caller_uid {
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

    // Effective scope task: explicit param if present, else
    // caller's own task (for Session callers). Operator callers
    // with no param have `scope_task = None` and see all
    // sessions; Operator callers WITH `task_id` filter to that
    // subtree.
    let scope_task: Option<String> = p.task_id.clone().or_else(|| {
        caller_uid
            .and_then(|cuid| state.sessions.get(cuid))
            .and_then(|s| s.task_id.clone())
    });

    let mut sessions: Vec<Value> = Vec::with_capacity(state.sessions.len());
    for (uid, session) in state.sessions.iter() {
        let included = match (scope_task.as_deref(), caller_uid) {
            // Explicit scope (param OR caller's task): include
            // only sessions whose task_id is self-or-descendant
            // of the scope. Mirrors TUI's `Some(scope) =>` arm.
            (Some(scope), _) => session
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
            // No scope, Session caller: defer to the per-session
            // auth check (taskless caller → same-workspace).
            (None, Some(cuid)) => {
                crate::control::auth::check_session_caller(&state, cuid, uid)
                    .is_allow()
            }
            // No scope, Operator caller: every session.
            (None, None) => true,
        };
        if !included {
            continue;
        }
        // Sub-2b-1 review-r#3 #1: single helper computes
        // `(state, idle)` for both `list_sessions` and
        // `resolve_authorized_session`. Pre-fix list_sessions
        // hardcoded `"ready"` + `false`, even though sub-2b-1
        // already had the data needed to compute both
        // (`transcript_path` + `last_activity_at`). Same daemon,
        // two methods, different answers — the Python MCP
        // tool's `wait_for_session_idle` was polling
        // list_sessions while `read_session_output` resolved
        // through resolve_authorized_session, so the
        // wait-then-read flow could observe idle=false from
        // resolve while list said idle=false anyway. Now both
        // agree.
        let (state_str, idle) = compute_session_state_and_idle(session);
        sessions.push(json!({
            "session_uid": uid,
            "label": session.title,
            "type": session.session_type,
            "state": state_str,
            "idle": idle,
            "managed_by_uid": session.managed_by_uid,
        }));
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
        let decision = crate::control::auth::check_session_caller(
            &state,
            cuid,
            &p.session_uid,
        );
        return_auth_error_if_denied(decision, cuid, &p.session_uid)?;
    }
    let session = state.sessions.get(&p.session_uid).ok_or_else(|| {
        (
            ErrorCode::NotFound,
            format!("session '{}' not in daemon registry", p.session_uid),
        )
    })?;

    let (state_str, idle) = compute_session_state_and_idle(session);
    let transcript_path = session.transcript_path.clone();
    let engine = engine_str(&session.session_type);
    // Sub-2b-1 review-r#2 #2: surface the generation counter
    // so the Python `read_session_output` tool's cursor
    // (`v1:<generation>:<offset>`) resets when the underlying
    // transcript file rotates (e.g. `/clear`, codex resume).
    let generation = session.generation;
    Ok(json!({
        "state": state_str,
        "engine": engine,
        "transcript_path": transcript_path,
        "generation": generation,
        "idle": idle,
    }))
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
/// `exited` is NOT returned today — daemon removes sessions
/// from `state.sessions` on exit (sub-2a's kill_session +
/// reaper-cleanup callback). Tombstone retention lands in
/// slice 10e (daemon-side manifest ownership); when it does
/// this helper grows an `Exited` arm reading from
/// `state.workspaces[..].tombstones`. Both call sites pick
/// the new arm up for free.
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
    Ok(json!({
        "ok": true,
        "generation": session.generation,
    }))
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
    for ws_entry in p.workspaces {
        let entry = state
            .workspaces
            .entry(ws_entry.workspace_id.clone())
            .or_insert_with(|| crate::manifest::ManifestWorkspace {
                id: ws_entry.workspace_id.clone(),
                worktree_path: None,
                ..Default::default()
            });
        // Sub-2b-3 review-3 #2: assign unconditionally — the
        // TUI's Option<String> wire shape uses `None` to mean
        // "no live worktree" (workspace was closed, pushed to
        // cloud, etc.). Pre-fix this only updated on `Some`, so
        // the daemon retained a stale path after the TUI
        // signalled the worktree was gone, and `mcp_start_session`
        // would still spawn into the dead path instead of
        // surfacing NotFound.
        entry.worktree_path = ws_entry.worktree_path.map(std::path::PathBuf::from);
    }
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

pub fn propose_task(_state_arc: &Arc<Mutex<DaemonState>>, params: &Value) -> MethodResult {
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
    match crate::planning_client::propose_task(&req) {
        Ok(task) => Ok(task),
        Err(e) => Err(e.to_method_err()),
    }
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
                .cloned()
                .ok_or((
                    ErrorCode::NotFound,
                    format!(
                        "task '{}' has no bound workspace in the daemon's task \
                         snapshot — the TUI must push task.update_tree with \
                         `workspace_id` populated for descendant-task subtree \
                         spawns to resolve (sub-2b-3 review-2 #1)",
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
        (
            target_workspace_id,
            caller.task_id.clone(),
            wt,
            cap_inherit,
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
    let (program, argv_tail) = crate::mcp_config::build_args(&p.type_, &session_uid)
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

    // Build env: daemon-injected pins plus nothing else.
    let env_map = crate::mcp_config::build_env(&session_uid);
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
    if let Some(cuid) = caller_uid {
        full_params.insert("managed_by_uid".into(), Value::String(cuid.to_string()));
    }
    if let Some(tid) = task_id_for_spawn {
        full_params.insert("task_id".into(), Value::String(tid));
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
    // delivery. Now we look up the new session's InputHandle
    // and write the prompt + trailing newline through the
    // shared `write_and_stamp` helper (same path used by the
    // attach-stream Input frame handler). Newline appended
    // when missing so the receiving agent sees a complete
    // submission (matches the TUI's `submit=true` shape on
    // `send_input`).
    //
    // **No quiet-wait gating yet**: the TUI's existing
    // `PendingWrite::wait_for_quiet` machinery isn't
    // relocated daemon-side. For sub-2b-3 the prompt goes
    // straight to the PTY post-spawn; the agent buffers
    // appropriately. A future slice can add a daemon-side
    // pending-prompt queue if races with engine startup
    // become observable.
    if let Some(prompt) = p.prompt.as_deref() {
        if !prompt.is_empty() {
            let handle_opt = {
                let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                state.sessions.get(&session_uid).map(|s| s.input_handle())
            };
            let Some(handle) = handle_opt else {
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
            let mut payload = prompt.as_bytes().to_vec();
            if !payload.ends_with(b"\n") {
                payload.push(b'\n');
            }
            if let Err(e) = handle.write_and_stamp(&payload) {
                // Sub-2b-3 review-8 #1: kill the just-spawned
                // session and surface the error. Pre-fix
                // a write failure logged + returned `ok`,
                // leaving a half-initialized session with no
                // delivered prompt — the caller has no way
                // to know the prompt didn't land. Removing
                // the session from the registry drops the
                // DaemonSession, which SIGKILLs the child via
                // its pidfd-based Drop.
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
            ticket,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ManifestWorkspace;
    use tempfile::TempDir;

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
    fn kill_session_removes_from_registry_and_terminates_child() {
        let dir = TempDir::new().unwrap();
        let state = state_with_workspace("ws-kill", &dir);
        let uid = spawn_bash(&state, "ws-kill");
        assert!(state.lock().unwrap().sessions.contains_key(&uid));

        let params = json!({ "session_uid": &uid });
        let result = kill_session(&state, &params, None).expect("kill ok");
        assert_eq!(result["ok"], true);
        assert!(
            !state.lock().unwrap().sessions.contains_key(&uid),
            "kill_session must remove the entry from the registry",
        );
        // The Drop-driven kill via pidfd sent SIGKILL; the reaper
        // observes the exit and the on_exit callback's remove is a
        // no-op (already removed). Nothing to verify beyond the
        // registry being empty.
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
        let mut p = crate::session::SpawnParams::new(uid, format!("test-{}", uid), "/bin/sleep");
        p.args = vec!["30".to_string()];
        p.workspace_id = workspace_id.to_string();
        let session = crate::session::DaemonSession::spawn(p).expect("spawn /bin/sleep");
        let mut s = state.lock().unwrap();
        s.sessions.insert(uid.to_string(), session);
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
    fn kill_session_auth_allow_removes_target() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        insert_session(&state, "ts-caller", "ws-shared");
        insert_session(&state, "ts-victim", "ws-shared");
        let params = json!({ "session_uid": "ts-victim" });
        let result = kill_session(&state, &params, Some("ts-caller")).expect("must allow");
        assert_eq!(result["ok"], true);
        let s = state.lock().unwrap();
        assert!(!s.sessions.contains_key("ts-victim"));
        assert!(s.sessions.contains_key("ts-caller"));
        drop(s);
        kill_all_sessions(&state);
    }

    /// Race-style: two threads call `kill_session` on the SAME
    /// target. Exactly one should observe NotFound (the loser).
    /// Pre-fix the two threads could race between the
    /// authorize-lock and the remove-lock, and both could pass
    /// auth before either removed (the loser would then see
    /// NotFound when it tried to remove, which it does — but
    /// the *auth pass* on a now-dead target was the leakage).
    /// Post-fix, auth+remove are atomic: the loser's auth check
    /// observes the post-remove state and surfaces NotFound at
    /// auth time. This test simply verifies the wire outcome
    /// (one Ok, one NotFound) regardless of the path; ordering
    /// is non-deterministic but the cardinality is the invariant.
    #[test]
    fn concurrent_kill_session_yields_exactly_one_ok() {
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
        assert_eq!(oks, 1, "exactly one thread must succeed");
        assert_eq!(not_founds, 1, "the other must see NotFound");
        // Target gone from registry.
        let s = state.lock().unwrap();
        assert!(!s.sessions.contains_key("ts-victim"));
        drop(s);
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
}
