//! Slice 12f: `daemon.toml` schema, loader, and security
//! validation.
//!
//! Phase 3's host model splits "where the TUI runs" from "where
//! the agent process runs". A remote-host cm-daemon (on the
//! cm-manager VM, a Mac mini, etc.) can't borrow the TUI's
//! local env vars (`CM_API_URL`, `CM_API_TOKEN`,
//! `CM_MCP_SERVER` paths) because those refer to TUI-side
//! files. The daemon needs its own config source.
//!
//! `daemon.toml` lives next to the daemon binary (or at
//! `~/.cm/daemon.toml` on a workstation) and is read once at
//! startup. Every spawned agent process inherits env from this
//! config, overriding whatever the TUI's `start_session` RPC
//! sent.
//!
//! ## Security: 0o600 + api_token-leak guard
//!
//! daemon.toml carries `api_token` (planning-API HTTP bearer).
//! Operators sometimes drop it into `/etc/` with world-readable
//! perms — that would leak the bearer to any local user. The
//! loader refuses to start when the file is world-readable AND
//! contains a non-empty `api_token`. Loud failure beats silent
//! leak.
//!
//! ## Missing file: inline defaults
//!
//! For the local workstation case (no daemon.toml on disk), the
//! loader returns sensible defaults that match what the TUI
//! injects today: empty api_url / api_token (planning API
//! disabled), mcp_server_path discovered via the existing
//! `crate::workflow::spawn::mcp_server_path()` resolver, etc.
//! This means existing local deployments keep working without
//! a config-file change.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Per-transport auth model. `daemon.toml`'s `[auth] mode` field.
///
/// - `SshTrust`: SSH session IS the auth boundary. Daemon
///   accepts any Operator-tagged frame on connections from
///   its listen socket. Used by slice 12d (SSH-unix tunnel
///   transport).
/// - `Token`: an `auth.hello` frame is required as the first
///   frame after TLS handshake. Token compared in constant
///   time against `CM_DAEMON_TOKEN`. Used by slice 12h
///   (TLS-TCP transport).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    SshTrust,
    Token,
}

impl Default for AuthMode {
    fn default() -> Self {
        // 12d ships first; default to its model so a config
        // file with `[auth]` omitted doesn't accidentally
        // require token-mode handshakes.
        AuthMode::SshTrust
    }
}

/// `[auth]` section.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub mode: AuthMode,
}

/// `[tls]` section. Optional — when absent, the daemon binds only
/// its Unix control socket. When present, the daemon ALSO binds a
/// rustls TCP listener on `listen_addr` and requires every
/// connection's first frame to be `auth.hello` carrying the
/// `CM_DAEMON_TOKEN` value (slice 12h).
///
/// All three fields are required. The loader does NOT auto-create
/// or auto-generate certs; operators run the documented openssl
/// invocation on the VM (see PR description / `doc/`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsConfig {
    /// PEM-encoded server certificate (or chain). Absolute path;
    /// the daemon reads it at startup. Self-signed certs are
    /// expected — the TUI pins SHA-256 of the leaf, not a chain
    /// of trust.
    pub cert_path: String,
    /// PEM-encoded private key matching `cert_path`. Accepts
    /// `PRIVATE KEY` (PKCS#8), `RSA PRIVATE KEY`, or
    /// `EC PRIVATE KEY` (SEC1) blocks. Mode 0o600 is the
    /// operator's responsibility — the daemon does NOT validate
    /// it here because key/cert perms are usually file-system-
    /// owner enforced (sudo / systemd User=).
    pub key_path: String,
    /// `host:port` to bind. Production: `0.0.0.0:8443` behind a
    /// firewall rule scoped to the operator's IP (NOT
    /// `0.0.0.0/0`).
    pub listen_addr: String,
}

/// `[[repo]]` allowlist entry — a repo the daemon is permitted to clone
/// on demand (Phase 2, remote-session-execution). `name` is matched
/// against the requested repo's shortname and `url` against the requested
/// URL; `url` is also the actual `git clone` source. The allowlist is the
/// default-deny complement to `allow_clone`: cloning arbitrary URLs runs
/// code-fetch on the host, so it's opt-in per-repo (or globally via
/// `allow_clone`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RepoAllowEntry {
    pub name: String,
    pub url: String,
}

/// `[scheduler]` section — Continuous Tasks Phase 3. Tunables for the daemon's
/// periodic-fire driver (`continuous::scheduler::ContinuousScheduler`, the
/// structural twin of `workflow::poller::WorkflowPoller`). Every field is
/// `#[serde(default)]`, and the section itself is `#[serde(default)]` on
/// `DaemonConfig`, so an existing `daemon.toml` with no `[scheduler]` section
/// still loads (the local-workstation + cm-manager configs predate it).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerConfig {
    /// Master on/off for the scheduler. `false` makes the scheduler's
    /// `tick_once` a no-op (lib.rs still constructs+starts the thread) so no
    /// Periodic continuous task ever fires. Default `true`.
    #[serde(default = "default_scheduler_enabled")]
    pub enabled: bool,
    /// Tick cadence in MICROSECONDS — same vocab as the poller's `tick_micros`
    /// (`workflow::poller::DEFAULT_TICK_INTERVAL_MICROS = 250_000`). The
    /// scheduler seeds its own `tick_micros` from this value (clamped to a
    /// 1 ms floor, falling back to the default when `0`); keeping it in micros
    /// avoids diverging from the twin and the clamp math. Default `250_000`
    /// (250 ms).
    #[serde(default = "default_scheduler_tick_micros")]
    pub tick_interval: u64,
    /// Optional disk guard: the maximum number of live continuous worktrees the
    /// scheduler permits. `None` (default) = unguarded. Enforcement lives in the
    /// scheduler's due-check (Phase 3), NOT in this config struct.
    #[serde(default)]
    pub max_worktrees: Option<u32>,
    /// Default per-fire memory cap in BYTES, applied to continuous spawns as the
    /// memory-cap triple (`memory_cap_bytes` / `memory_cap_hard_bytes` /
    /// `cgroup_prefix`) unless a task overrides it via
    /// `ContinuousTask::mem_cap_bytes`. `0` opts out (uncapped). Default `0`
    /// (uncapped) — a non-zero cap needs a usable `systemd-run --user --scope`
    /// (a reachable user manager bus); opt in per-task or set this on a host
    /// where the capped path works.
    #[serde(default = "default_scheduler_default_cap")]
    pub default_cap: u64,
    /// Continuous Tasks Phase 3b (stuck-story watchdog): how many investigators
    /// the watchdog may spawn for one stuck fresh run before it gives up and
    /// auto-escalates (last_run → Stuck). Counted in
    /// `ContinuousTask::investigation_count` (reset on every fresh fire).
    /// Default `2`.
    #[serde(default = "default_scheduler_max_investigations")]
    pub max_investigations: u32,
    /// Continuous Tasks Phase 3b (stuck-story watchdog): the spawned
    /// investigator's OWN runtime budget in SECONDS — so a wedged investigator
    /// is itself bounded and eventually auto-escalates. The per-task worker
    /// budget is `ContinuousTask::max_runtime_secs` (`None` = watchdog off);
    /// this is the investigator-session analogue. Default `600` (10 min).
    #[serde(default = "default_scheduler_investigator_runtime_secs")]
    pub default_investigator_runtime_secs: u32,
    /// Continuous Tasks: persistent-orchestrator stall detection. A persistent
    /// (long-lived) orchestrator that received a fire but produced NO transcript
    /// growth within this many SECONDS is wedged (e.g. parked at an interactive
    /// rate-limit / trust modal a headless `send_input` can't drive). The
    /// scheduler SURFACES it (daemon-log alert + a `"stalled"` runs.jsonl line),
    /// one alert per fire episode — it does NOT auto-kill or auto-spawn, because
    /// the dominant cause (shared-account rate-limit) would also block any
    /// investigator we spawned; recovery stays the operator's call. `None`
    /// (default) = OFF (opt-in — avoids false positives on genuinely-long tool
    /// calls until the operator has calibrated a budget). A sane value is `1800`
    /// (30 min). Applies to `RunMode::Persistent` only; the Fresh analogue is
    /// per-task `max_runtime_secs` (the watchdog).
    #[serde(default)]
    pub persistent_max_stall_secs: Option<u32>,
    /// Consumer-wedge watchdog (2026-08-03 incident): a Consumer task whose
    /// active run is still `Running` while its live session's transcript ends
    /// in a COMPLETED turn (or a delivered-but-unanswered prompt) and has been
    /// quiet for this many SECONDS is wedged — the agent finished (or died)
    /// without `report_done`, and the run-active due-gate would otherwise skip
    /// every future fire forever. The scheduler auto-closes such a run
    /// (`Running → Failed`, a `"wedge_closed"` runs.jsonl line, an operator
    /// alert) so the due-gate refires naturally — bounded by
    /// [`SchedulerConfig::wedge_close_limit`]. Must comfortably exceed the
    /// longest legitimate "turn ended, waiting for a monitor wake" gap (a
    /// worker the orchestrator parked on can run ~25 min). `0` = OFF.
    /// Default `3600` (1 h — the incident wedge lasted 3.5 DAYS).
    #[serde(default = "default_scheduler_wedge_grace_secs")]
    pub consumer_wedge_grace_secs: u64,
    /// Consecutive wedge auto-closes (no intervening clean completion) after
    /// which the scheduler STOPS closing and escalates instead — a repeatedly
    /// wedging consumer means something systemic, and every auto-close+refire
    /// claims (and acks) real queue items into a broken orchestrator. The
    /// counter is `ContinuousTask::consecutive_wedge_closes`, reset by
    /// `report_done` / a clean exit / an operator `force_done`. Default `3`.
    #[serde(default = "default_scheduler_wedge_close_limit")]
    pub wedge_close_limit: u32,
}

fn default_scheduler_enabled() -> bool {
    true
}

fn default_scheduler_tick_micros() -> u64 {
    // Poller-class cadence — mirrors `workflow::poller::DEFAULT_TICK_INTERVAL_MICROS`.
    250_000
}

fn default_scheduler_default_cap() -> u64 {
    // 0 — UNCAPPED by default. A per-fire memory cap requires a usable
    // `systemd-run --user --scope` (a reachable user manager bus); when the
    // daemon runs as a system service without one, a non-zero default only
    // creates a boot trap for new tasks (they degrade to uncapped anyway, or —
    // before the capability probe — failed the fire outright). Every live
    // continuous task opts out (`mem_cap_bytes: 0`), so the safe, honest
    // default matches usage: no cap unless a task explicitly sets one, or the
    // operator sets `[scheduler] default_cap` on a host where caps work.
    0
}

fn default_scheduler_max_investigations() -> u32 {
    // Two investigators per stuck run before the watchdog auto-escalates
    // (DESIGN_CONTINUOUS_TASKS.md §11).
    2
}

fn default_scheduler_investigator_runtime_secs() -> u32 {
    // 10 minutes — the investigator session's own runtime budget so a wedged
    // investigator is itself bounded and auto-escalates.
    600
}

fn default_scheduler_wedge_grace_secs() -> u64 {
    // 1 hour of post-turn silence before a Running consumer run is judged
    // wedged. Longest legitimate silent gap is a monitor-parked worker
    // (~25 min budget) + delivery slack; 1 h leaves >2× headroom while
    // detecting in 1 h what the 2026-08-03 incident left invisible for 3.5 d.
    3600
}

fn default_scheduler_wedge_close_limit() -> u32 {
    // Three consecutive auto-closes, then escalate: enough to self-heal a
    // flaky missed report_done, few enough that a systemically-broken
    // consumer doesn't burn its queue via close→refire churn.
    3
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: default_scheduler_enabled(),
            tick_interval: default_scheduler_tick_micros(),
            max_worktrees: None,
            default_cap: default_scheduler_default_cap(),
            max_investigations: default_scheduler_max_investigations(),
            default_investigator_runtime_secs: default_scheduler_investigator_runtime_secs(),
            persistent_max_stall_secs: None,
            consumer_wedge_grace_secs: default_scheduler_wedge_grace_secs(),
            wedge_close_limit: default_scheduler_wedge_close_limit(),
        }
    }
}

/// Parsed `daemon.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonConfig {
    /// Absolute path to `mcp_server/server.py`. The daemon
    /// injects this into every agent's `CM_MCP_SERVER` env so
    /// the agent's MCP runtime knows where to find the
    /// claude-manager server. Empty string means "let the
    /// agent fall back to the existing
    /// `crate::workflow::spawn::mcp_server_path()` resolver."
    #[serde(default)]
    pub mcp_server_path: String,
    /// Planning API base URL (typically
    /// `http://localhost:8000` on a local workstation,
    /// `http://10.150.0.x:8000` for a cm-manager-hosted
    /// daemon). Injected as `CM_API_URL` so MCP tools like
    /// `propose_task` reach the right server.
    #[serde(default)]
    pub api_url: String,
    /// Planning API bearer token. Injected as `CM_API_TOKEN`.
    /// **Sensitive**: world-readable daemon.toml + non-empty
    /// `api_token` = loud-fail at load.
    #[serde(default)]
    pub api_token: String,
    /// Where the daemon writes its log. Informational —
    /// the daemon's stderr already lands at the systemd
    /// journal in production. Future slices may consult this
    /// to add a file-tailer.
    #[serde(default)]
    pub log_path: String,
    /// Workflows directory the daemon's controller reads
    /// TOML files from. Defaults to today's value when empty.
    #[serde(default)]
    pub workflows_dir: String,
    /// `[auth]` section. Defaults to `mode = "ssh-trust"`
    /// when the section is omitted.
    #[serde(default)]
    pub auth: AuthConfig,
    /// `[tls]` section. `Some` enables the rustls TCP listener
    /// (slice 12h); `None` leaves it disabled — the daemon binds
    /// only its Unix socket. Omitting the section in `daemon.toml`
    /// is the local-workstation default.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// Directory where the daemon clones repos not already on disk
    /// (Phase 2 clone-on-demand). Empty → the `~/.cm/repos` default
    /// (see [`DaemonConfig::repos_dir_or_default`]). Only consulted when
    /// a clone is actually permitted (an allowlist match or
    /// `allow_clone`); with neither, no clone ever happens and this is
    /// unused.
    #[serde(default)]
    pub repos_dir: String,
    /// Open-cloning flag. `false` (default) → only allowlisted URLs may
    /// be cloned. Cloning an arbitrary URL runs code-fetch on the host,
    /// so open cloning is opt-in. `true` → any requested URL may be
    /// cloned on demand.
    #[serde(default)]
    pub allow_clone: bool,
    /// Clone allowlist. Each `[[repo]]` entry permits cloning one repo by
    /// `name` / `url` even when `allow_clone` is false.
    #[serde(default, rename = "repo")]
    pub repos: Vec<RepoAllowEntry>,
    /// `[scheduler]` section — Continuous Tasks Phase 3 tunables (the
    /// periodic-fire driver + per-fire memory cap). Additive +
    /// `#[serde(default)]` so a `daemon.toml` predating it still loads.
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    /// Operator push-alert command (see `crate::notify::notify_operator`).
    /// When set, daemon escalations (auth expiry, wedged consumer runs, stuck
    /// escalations, circuit-breaker pauses, persistent stalls) exec this with
    /// the alert message as the single argument and `CM_NOTIFY_TAG` in the
    /// env — on cm-manager, `/home/lucas/.cm/bin/cm-notify` (the Telegram
    /// script). Use an ABSOLUTE path: the systemd unit's PATH is minimal.
    /// `None`/empty (default) = alerts land on stderr (journal) only. Born
    /// from the 2026-08-03 auth-expiry incident, where every failure was
    /// logged and nothing was pushed.
    #[serde(default)]
    pub notify_command: Option<String>,
}

impl DaemonConfig {
    /// Effective clone directory: the configured `repos_dir`, or the
    /// `~/.cm/repos` default when unset. Daemon-cloned repos live here —
    /// separate from the operator's hand-managed `~/code/projects` (which
    /// `find_local_repo` checks first), alongside the daemon's other
    /// `~/.cm/` state (worktrees, mcp configs, workflow-runs).
    pub fn repos_dir_or_default(&self) -> PathBuf {
        if self.repos_dir.trim().is_empty() {
            crate::path::dot_cm_dir().join("repos")
        } else {
            PathBuf::from(&self.repos_dir)
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            mcp_server_path: String::new(),
            api_url: String::new(),
            api_token: String::new(),
            log_path: String::new(),
            workflows_dir: String::new(),
            auth: AuthConfig::default(),
            tls: None,
            repos_dir: String::new(),
            allow_clone: false,
            repos: Vec::new(),
            scheduler: SchedulerConfig::default(),
            notify_command: None,
        }
    }
}

/// Errors surfaced by `DaemonConfig::load_or_default`.
#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    /// 12f security gate: file exists, contains a non-empty
    /// `api_token`, and is world-readable (group or other
    /// has read perm). Loud failure to keep the bearer out
    /// of a multi-user system's casual reach.
    InsecurePermsWithSecret {
        path: PathBuf,
        mode: u32,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "daemon.toml I/O error: {}", e),
            ConfigError::Parse(e) => write!(f, "daemon.toml parse error: {}", e),
            ConfigError::InsecurePermsWithSecret { path, mode } => write!(
                f,
                "daemon.toml at {} is world- or group-readable \
                 (mode = 0o{:o}) AND contains a non-empty `api_token`. \
                 Refusing to start to prevent bearer-token leak. \
                 Fix: `chmod 0600 {}`",
                path.display(),
                mode,
                path.display(),
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Load `daemon.toml` from `path`. Missing file → inline
/// defaults (the local-workstation case keeps working without
/// a config-file change). Present file + 0o600 + valid TOML →
/// parsed config. Present file + world/group-readable +
/// non-empty `api_token` → `InsecurePermsWithSecret` error.
pub fn load_or_default(path: &Path) -> Result<DaemonConfig, ConfigError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Missing file is a NORMAL case (workstation with
            // no daemon.toml). Inline defaults: empty
            // api_url/api_token (planning API disabled —
            // matches today's behavior on the local box),
            // empty mcp_server_path (agent's fallback
            // resolver runs).
            return Ok(DaemonConfig::default());
        }
        Err(e) => return Err(ConfigError::Io(e)),
    };

    let cfg: DaemonConfig =
        toml::from_str(&contents).map_err(ConfigError::Parse)?;

    // Security gate: world- or group-readable file + non-empty
    // api_token = bearer-leak risk. Refuse to load.
    if !cfg.api_token.is_empty() {
        validate_secret_perms(path)?;
    }

    Ok(cfg)
}

/// Permission check on `path`. Returns Err if the file is
/// readable by group OR other. Only called when there's a
/// secret to protect — a config with no `api_token` can be
/// world-readable safely (the rest of the fields are paths
/// and URLs, not credentials).
fn validate_secret_perms(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;
    let meta =
        std::fs::metadata(path).map_err(ConfigError::Io)?;
    let mode = meta.permissions().mode() & 0o777;
    // 0o077 covers group + other read/write/execute. Any bit
    // set in those groups means non-owner can see the bearer.
    if mode & 0o077 != 0 {
        return Err(ConfigError::InsecurePermsWithSecret {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

/// Default config path. Mirrors `path::dot_cm_dir()` for the
/// workstation case; production deployments (cm-manager VM)
/// typically symlink `/etc/cm-daemon/daemon.toml` here or set
/// `CM_DAEMON_CONFIG=<path>`.
pub fn default_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("CM_DAEMON_CONFIG") {
        return PathBuf::from(p);
    }
    crate::path::dot_cm_dir().join("daemon.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// T_g3f_daemon_config_load (named acceptance): a valid
    /// daemon.toml with 0o600 perms parses correctly,
    /// round-trips every field, and resolves the auth mode.
    #[test]
    fn t_g3f_daemon_config_load() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        let content = r#"
mcp_server_path = "/opt/cm-daemon/mcp_server/server.py"
api_url = "http://localhost:8000"
api_token = "secret-token-abc"
log_path = "/var/log/cm-daemon.log"
workflows_dir = "/opt/cm-daemon/workflows/"

[auth]
mode = "ssh-trust"
"#;
        std::fs::write(&path, content).expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod 600");

        let cfg = load_or_default(&path).expect("load");
        assert_eq!(
            cfg.mcp_server_path,
            "/opt/cm-daemon/mcp_server/server.py",
        );
        assert_eq!(cfg.api_url, "http://localhost:8000");
        assert_eq!(cfg.api_token, "secret-token-abc");
        assert_eq!(cfg.log_path, "/var/log/cm-daemon.log");
        assert_eq!(cfg.workflows_dir, "/opt/cm-daemon/workflows/");
        assert_eq!(cfg.auth.mode, AuthMode::SshTrust);
    }

    /// 12f: token-mode auth alternative is accepted.
    #[test]
    fn auth_token_mode_parses() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        std::fs::write(
            &path,
            "mcp_server_path = \"\"\n[auth]\nmode = \"token\"\n",
        )
        .expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
        let cfg = load_or_default(&path).expect("load");
        assert_eq!(cfg.auth.mode, AuthMode::Token);
    }

    /// 12f: missing daemon.toml → inline defaults (the
    /// local-workstation case). The daemon starts cleanly
    /// without any config file on disk.
    #[test]
    fn missing_file_returns_defaults() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist.toml");
        let cfg = load_or_default(&missing).expect("missing file is OK");
        assert_eq!(cfg.mcp_server_path, "");
        assert_eq!(cfg.api_url, "");
        assert_eq!(cfg.api_token, "");
        assert_eq!(cfg.auth.mode, AuthMode::SshTrust);
    }

    /// 12f: world-readable file + non-empty api_token =
    /// refuse-to-load. Bearer leak prevention.
    #[test]
    fn world_readable_with_secret_refuses_to_load() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        std::fs::write(&path, "api_token = \"sekrit\"\n")
            .expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("chmod 644");
        match load_or_default(&path) {
            Err(ConfigError::InsecurePermsWithSecret { mode, .. }) => {
                assert_eq!(mode & 0o077, 0o044, "mode preserves the offending bits");
            }
            other => panic!(
                "expected InsecurePermsWithSecret; got {:?}",
                other.map(|c| c.api_token),
            ),
        }
    }

    /// 12f: world-readable file is OK as long as api_token
    /// is empty. (mcp_server_path / api_url / workflows_dir
    /// aren't secrets.)
    #[test]
    fn world_readable_without_secret_is_fine() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        std::fs::write(
            &path,
            "mcp_server_path = \"/opt/x.py\"\napi_url = \"http://x\"\n",
        )
        .expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("chmod 644");
        let cfg = load_or_default(&path).expect("no secret, no problem");
        assert_eq!(cfg.api_url, "http://x");
    }

    /// 12f: group-readable file + non-empty api_token is also
    /// rejected (any non-owner read bit triggers the gate).
    #[test]
    fn group_readable_with_secret_refuses_to_load() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        std::fs::write(&path, "api_token = \"sekrit\"\n")
            .expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o640),
        )
        .expect("chmod 640");
        match load_or_default(&path) {
            Err(ConfigError::InsecurePermsWithSecret { .. }) => {}
            other => panic!(
                "expected InsecurePermsWithSecret on group-readable + secret; \
                 got {:?}",
                other.map(|c| c.api_url),
            ),
        }
    }

    /// 12f: malformed TOML surfaces as `Parse` error rather
    /// than a silent default-fill.
    #[test]
    fn malformed_toml_surfaces_parse_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        std::fs::write(&path, "this is not = valid = toml\n")
            .expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
        match load_or_default(&path) {
            Err(ConfigError::Parse(_)) => {}
            other => panic!(
                "expected Parse error; got {:?}",
                other.map(|c| c.api_url),
            ),
        }
    }

    /// 12f: serde round-trip works (Serialize + Deserialize)
    /// — the doc-prescribed shape parses back to byte-equal
    /// values across the field set.
    #[test]
    fn serde_round_trip() {
        let original = DaemonConfig {
            mcp_server_path: "/opt/x.py".into(),
            api_url: "http://h:8000".into(),
            api_token: "tok".into(),
            log_path: "/var/log/d.log".into(),
            workflows_dir: "/opt/wf/".into(),
            auth: AuthConfig {
                mode: AuthMode::Token,
            },
            tls: None,
            repos_dir: "/home/lucas/.cm/repos".into(),
            allow_clone: true,
            repos: vec![RepoAllowEntry {
                name: "claude-manager".into(),
                url: "https://github.com/u/claude-manager.git".into(),
            }],
            scheduler: SchedulerConfig::default(),
            notify_command: None,
        };
        let toml_text = toml::to_string(&original).expect("ser");
        let reparsed: DaemonConfig =
            toml::from_str(&toml_text).expect("de");
        assert_eq!(reparsed.mcp_server_path, original.mcp_server_path);
        assert_eq!(reparsed.api_url, original.api_url);
        assert_eq!(reparsed.api_token, original.api_token);
        assert_eq!(reparsed.log_path, original.log_path);
        assert_eq!(reparsed.workflows_dir, original.workflows_dir);
        assert_eq!(reparsed.auth.mode, original.auth.mode);
        assert!(reparsed.tls.is_none());
        assert_eq!(reparsed.repos_dir, original.repos_dir);
        assert_eq!(reparsed.allow_clone, original.allow_clone);
        assert_eq!(reparsed.repos.len(), 1);
        assert_eq!(reparsed.repos[0].name, "claude-manager");
        assert_eq!(reparsed.repos[0].url, "https://github.com/u/claude-manager.git");
    }

    /// Phase 2: the repos section (`repos_dir`, `allow_clone`, and
    /// `[[repo]]` allowlist entries) parses from daemon.toml.
    #[test]
    fn repos_section_parses() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        std::fs::write(
            &path,
            r#"
mcp_server_path = ""
repos_dir = "/home/lucas/.cm/repos"
allow_clone = false

[[repo]]
name = "claude-manager"
url = "https://github.com/u/claude-manager.git"

[[repo]]
name = "other"
url = "git@github.com:u/other.git"
"#,
        )
        .expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
        let cfg = load_or_default(&path).expect("load");
        assert_eq!(cfg.repos_dir, "/home/lucas/.cm/repos");
        assert!(!cfg.allow_clone);
        assert_eq!(cfg.repos.len(), 2);
        assert_eq!(cfg.repos[0].name, "claude-manager");
        assert_eq!(cfg.repos[1].url, "git@github.com:u/other.git");
    }

    /// Phase 2: no repos section → empty defaults (allow_clone=false,
    /// empty allowlist) so no clone ever happens — today's behavior.
    /// `repos_dir_or_default` falls back to `~/.cm/repos`.
    #[test]
    fn repos_section_absent_defaults_to_no_clone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        std::fs::write(&path, "mcp_server_path = \"\"\n").expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
        let cfg = load_or_default(&path).expect("load");
        assert!(!cfg.allow_clone, "default-deny: no open cloning");
        assert!(cfg.repos.is_empty(), "no allowlist entries");
        assert_eq!(cfg.repos_dir, "", "repos_dir unset");
        assert!(
            cfg.repos_dir_or_default().ends_with(".cm/repos"),
            "default clone dir is ~/.cm/repos, got {:?}",
            cfg.repos_dir_or_default(),
        );
    }

    /// Phase 2: a non-empty `repos_dir` is honored verbatim by
    /// `repos_dir_or_default`.
    #[test]
    fn repos_dir_or_default_honors_configured_value() {
        let cfg = DaemonConfig {
            repos_dir: "/srv/clones".into(),
            ..DaemonConfig::default()
        };
        assert_eq!(cfg.repos_dir_or_default(), PathBuf::from("/srv/clones"));
    }

    /// Phase 3: no `[scheduler]` section → every field falls back to its
    /// serde default (enabled, 250_000 µs / poller-class tick, no worktree
    /// guard, 1 GiB cap) so a `daemon.toml` predating Continuous Tasks Phase 3
    /// still loads.
    #[test]
    fn scheduler_section_absent_defaults() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        std::fs::write(&path, "mcp_server_path = \"\"\n").expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
        let cfg = load_or_default(&path).expect("load");
        assert!(cfg.scheduler.enabled, "scheduler on by default");
        assert_eq!(cfg.scheduler.tick_interval, 250_000, "poller-class µs tick");
        assert!(cfg.scheduler.max_worktrees.is_none(), "unguarded by default");
        assert_eq!(cfg.scheduler.default_cap, 0, "uncapped by default (opt-in caps)");
        // Phase 3b watchdog tunables fall back to their serde defaults too.
        assert_eq!(cfg.scheduler.max_investigations, 2, "two investigators by default");
        assert_eq!(
            cfg.scheduler.default_investigator_runtime_secs, 600,
            "10-minute investigator budget default",
        );
    }

    /// Phase 3: `SchedulerConfig::default()` (used by the missing-file path and
    /// the `DaemonConfig` default) carries the same values as an absent section.
    #[test]
    fn scheduler_config_default_values() {
        let s = SchedulerConfig::default();
        assert!(s.enabled);
        assert_eq!(s.tick_interval, 250_000);
        assert_eq!(s.max_worktrees, None);
        assert_eq!(s.default_cap, 0);
        assert_eq!(s.max_investigations, 2);
        assert_eq!(s.default_investigator_runtime_secs, 600);
    }

    /// Phase 3: an explicit `[scheduler]` section overrides each field, and a
    /// partial section leaves the unspecified fields at their serde defaults.
    #[test]
    fn scheduler_section_parses_overrides() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        std::fs::write(
            &path,
            r#"
mcp_server_path = ""

[scheduler]
enabled = false
tick_interval = 500000
max_worktrees = 32
"#,
        )
        .expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
        let cfg = load_or_default(&path).expect("load");
        assert!(!cfg.scheduler.enabled);
        assert_eq!(cfg.scheduler.tick_interval, 500_000);
        assert_eq!(cfg.scheduler.max_worktrees, Some(32));
        // `default_cap` was omitted → its serde default still applies.
        assert_eq!(cfg.scheduler.default_cap, 0);
        // The Phase 3b keys were omitted → their serde defaults still apply.
        assert_eq!(cfg.scheduler.max_investigations, 2);
        assert_eq!(cfg.scheduler.default_investigator_runtime_secs, 600);
    }

    /// Phase 3b: an explicit `[scheduler]` section overrides the watchdog
    /// tunables (`max_investigations`, `default_investigator_runtime_secs`).
    #[test]
    fn scheduler_section_parses_watchdog_overrides() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        std::fs::write(
            &path,
            r#"
mcp_server_path = ""

[scheduler]
max_investigations = 5
default_investigator_runtime_secs = 1200
"#,
        )
        .expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
        let cfg = load_or_default(&path).expect("load");
        assert_eq!(cfg.scheduler.max_investigations, 5);
        assert_eq!(cfg.scheduler.default_investigator_runtime_secs, 1200);
        // Untouched keys keep their serde defaults.
        assert!(cfg.scheduler.enabled);
        assert_eq!(cfg.scheduler.default_cap, 0);
    }

    /// 12h: `[tls]` section parses when present. All three fields
    /// (cert_path, key_path, listen_addr) are required — the
    /// loader doesn't auto-fill any defaults because there's no
    /// safe production default for a cert path.
    #[test]
    fn tls_section_parses_when_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        std::fs::write(
            &path,
            r#"
mcp_server_path = ""
[auth]
mode = "token"

[tls]
cert_path = "/etc/cm-daemon/cert.pem"
key_path = "/etc/cm-daemon/key.pem"
listen_addr = "0.0.0.0:8443"
"#,
        )
        .expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
        let cfg = load_or_default(&path).expect("load");
        let tls = cfg.tls.expect("tls section");
        assert_eq!(tls.cert_path, "/etc/cm-daemon/cert.pem");
        assert_eq!(tls.key_path, "/etc/cm-daemon/key.pem");
        assert_eq!(tls.listen_addr, "0.0.0.0:8443");
        // The token-mode auth pair is the natural companion of
        // a TLS listener (auth.hello frame validates the token).
        assert_eq!(cfg.auth.mode, AuthMode::Token);
    }

    /// 12h: omitting `[tls]` keeps the field `None` — the
    /// listener is opt-in.
    #[test]
    fn tls_section_defaults_to_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.toml");
        std::fs::write(&path, "mcp_server_path = \"\"\n")
            .expect("write");
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
        let cfg = load_or_default(&path).expect("load");
        assert!(cfg.tls.is_none());
    }
}
