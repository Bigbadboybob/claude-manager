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
    }
}
