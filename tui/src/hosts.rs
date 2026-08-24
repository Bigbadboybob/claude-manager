//! Slice 12a: `~/.cm/hosts.toml` schema + loader.
//!
//! Phase 3's host-abstraction layer. This module defines the
//! types every session-bearing piece of TUI state will eventually
//! carry (`HostId`), and the on-disk config file that enumerates
//! which daemons the TUI can talk to. No runtime consumer yet —
//! 12b adds the field to manifest entries; 12c wires the
//! connection pool; 12d adds the SSH-unix transport implementation.
//!
//! ## Wire shape
//!
//! ```toml
//! [[host]]
//! name = "local"
//! transport = "unix"
//! socket = "~/.cm/daemon.sock"
//! default = true
//!
//! [[host]]
//! name = "manager"
//! transport = "ssh-unix"
//! ssh_host = "cm-manager"
//! ssh_user = "lucas"           # optional
//! remote_socket = "/home/lucas/.cm/daemon.sock"
//! ```
//!
//! The `transport` discriminator is rendered as a sibling field on
//! each `[[host]]` table (rather than nested under a `[host.transport]`
//! sub-table) because that's what the design doc spec shows and
//! what reads naturally in a config file. Serde delivers this via
//! `#[serde(tag = "transport", rename_all = "kebab-case")]` on the
//! enum plus `#[serde(flatten)]` on `HostConfig::transport`.
//!
//! ## Local-as-host (A1 from the Phase 3 plan)
//!
//! `HostsConfig::load` synthesizes a single local entry when the
//! file is missing — existing single-user setups don't need to
//! create a config file to keep working. The synthesized entry
//! uses `cm_daemon::default_socket_path()` so it stays in lockstep
//! with the rest of the daemon path resolution.
//!
//! ## TLS-TCP transport (12h — was a 12a placeholder)
//!
//! `HostTransport::TcpTls` is now a real variant. The TUI dialer
//! (`crate::tls_dialer`) opens a TCP connection, completes a
//! rustls handshake while pinning the cert's SHA-256 fingerprint
//! via a custom `ServerCertVerifier`, and sends an `auth.hello`
//! JSON-RPC frame carrying the value of `auth_env` (resolved from
//! the TUI's process env at dial time, NOT at config-load time).
//! The daemon's matching listener (`cm_daemon::control::tls`)
//! validates the `auth.hello` token and forwards the connection to
//! the same dispatch loop the Unix socket uses.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// 12b: `HostId` moved to `cm_daemon::host_id` so the daemon-side
// `ManifestEntry` can carry the field as a first-class type
// without a layering inversion. Re-exported here so existing
// callers continue to write `crate::hosts::HostId`. The host-
// abstraction logic in this module (HostTransport, HostConfig,
// HostsConfig) stays TUI-side; only the typed identifier crossed
// the layer.
pub use cm_daemon::host_id::HostId;

/// Transport over which the TUI dials a daemon. The variant chosen
/// determines which fields are required on the `[[host]]` entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "kebab-case")]
pub enum HostTransport {
    /// Local Unix domain socket. The trust boundary is filesystem
    /// permissions — same model as Phase 1/2's `~/.cm/daemon.sock`.
    /// No `auth.hello` required.
    Unix {
        /// Path to the daemon's Unix socket. `~` is expanded at
        /// load time; relative paths are accepted but rare.
        socket: PathBuf,
    },
    /// SSH-tunneled Unix socket. The TUI invokes `ssh -L
    /// <local-path>:<remote-socket> [user@]host`, binds the local
    /// end, and dials it. The SSH session IS the auth boundary;
    /// daemon-side operator-token check runs in `ssh-trust` mode
    /// (A5 from the Phase 3 plan).
    ///
    /// `socket` (in production resolution) gets computed as
    /// `/tmp/cm-host-<name>.sock`; not stored on the variant
    /// because it's derivable from `HostId`.
    SshUnix {
        /// `[user@]host` SSH destination. Typically a gcloud
        /// SSH config alias (e.g. `cm-manager`) so this matches
        /// what the operator types into `ssh cm-manager` at the
        /// shell.
        ssh_host: String,
        /// Optional SSH user override. When unset, ssh uses the
        /// invoking user (or whatever `~/.ssh/config` declares
        /// for the host).
        #[serde(default)]
        ssh_user: Option<String>,
        /// Path to the daemon's Unix socket on the REMOTE host.
        /// Tilde expansion does NOT happen here (the remote
        /// home directory may differ from the local one — see
        /// the explicit `/home/<daemon-user>/.cm/daemon.sock`
        /// shape in the Phase 3 NOTES).
        remote_socket: PathBuf,
    },
    /// TLS-over-TCP transport (slice 12h). The TUI dialer
    /// connects to `addr`, completes a rustls handshake while
    /// pinning the leaf cert's SHA-256 fingerprint against
    /// `tls_fingerprint`, then sends an `auth.hello` JSON-RPC
    /// frame carrying the value of `$auth_env` from the
    /// invoking process's env (e.g. `CM_DAEMON_TOKEN`).
    ///
    /// All three fields are required at config load — the 12a
    /// placeholder allowed optional fields with a friendly
    /// "not implemented" error; 12h flips that to "real". Use
    /// `transport = "ssh-unix"` instead for setups where you'd
    /// rather lean on SSH for auth + transport.
    TcpTls {
        /// `host:port` of the daemon's TLS listener (matches
        /// `daemon.toml`'s `[tls] listen_addr`).
        addr: String,
        /// Lower-case hex-encoded SHA-256 of the leaf cert's DER
        /// bytes (64 hex chars). Colon-separated `aa:bb:..`
        /// shape is also accepted at parse time so operators can
        /// paste the output of `openssl x509 -fingerprint -sha256`
        /// without reformatting.
        tls_fingerprint: String,
        /// Name of the env var the TUI reads at dial time to
        /// learn the daemon token. Default `CM_DAEMON_TOKEN`.
        /// Carried in the TOML so power users can run two
        /// daemons with different tokens from the same TUI
        /// session without env-var collisions.
        #[serde(default = "default_auth_env")]
        auth_env: String,
    },
}

/// Default `auth_env` for [`HostTransport::TcpTls`] when the
/// operator omits the field. Matches the daemon's documented
/// env var.
fn default_auth_env() -> String {
    "CM_DAEMON_TOKEN".to_string()
}

/// One entry from `[[host]]` in `hosts.toml`. Field order in the
/// struct doesn't match the TOML wire shape because serde flattens
/// the transport's tag + fields into the host table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostConfig {
    /// `name = "..."` field on the TOML table. Renamed via serde
    /// because `HostId` is the Rust-side type name and `name` is
    /// the user-facing one.
    #[serde(rename = "name")]
    pub id: HostId,
    /// Transport variant + its required fields. Flattened so the
    /// TOML reads `transport = "unix" \n socket = "..."` rather
    /// than `[host.transport] kind = "unix" socket = "..."`.
    #[serde(flatten)]
    pub transport: HostTransport,
    /// Whether this entry is the "default host" for new-session
    /// creation. Validation requires exactly one `true` across
    /// the whole file.
    #[serde(default)]
    pub default: bool,
    /// Operator token the TUI presents to THIS host's daemon on every
    /// `Caller::Operator` RPC (`manifest.watch`, `events.subscribe`,
    /// `task.update_tree`, `session.kill`, …), inline. Each daemon
    /// validates against the `CM_OPERATOR_TOKEN` it was started with,
    /// and a remote daemon's token is NOT the local one: cm-manager's
    /// `cm-redeploy` mints its own `~/.cm/operator-token` and hands it
    /// to `cm-daemon.service` via an `EnvironmentFile` drop-in, so a
    /// TUI that presented its local token got `operator token does not
    /// match` on every gated RPC — the watch stream never connected and
    /// remote kills never pruned their rows. Resolution order (see
    /// `HostPool::operator_token_for`): this field → `operator_token_file`
    /// → (ssh-unix only) the remote `<dirname(remote_socket)>/operator-token`
    /// fetched once over the same ssh alias → the local token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_token: Option<String>,
    /// Local file holding the token for this host (one line, whitespace
    /// trimmed). `~` is expanded at load time. Mutually usable with
    /// `operator_token`; the inline value wins when both are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_token_file: Option<PathBuf>,
}

/// Parsed `hosts.toml`. Use `HostsConfig::load(path)` to obtain
/// one — the constructor runs validation and synthesizes the
/// local-default when the file is missing.
#[derive(Clone, Debug)]
pub struct HostsConfig {
    pub hosts: Vec<HostConfig>,
}

impl HostsConfig {
    /// Locate the default host entry. Validation guarantees
    /// exactly one exists post-`load`; callers can `.expect()`
    /// the `Option` if they want to crash on a bug elsewhere.
    pub fn default_host(&self) -> Option<&HostConfig> {
        self.hosts.iter().find(|h| h.default)
    }

    /// Find a host by id. Returns `None` for unknown ids.
    pub fn find(&self, id: &HostId) -> Option<&HostConfig> {
        self.hosts.iter().find(|h| &h.id == id)
    }

    /// Load + validate `~/.cm/hosts.toml`. On file-not-found, returns
    /// the synthesized local default (A1 from the Phase 3 plan) so
    /// existing single-user setups don't need a config file.
    ///
    /// On any other error (malformed TOML, validation failure, I/O
    /// other than NotFound), returns `Err`. The caller is expected
    /// to surface the error and decide whether to fall back to
    /// [`Self::synthesized_local_default`] — `App::new` does that
    /// so a malformed file doesn't lock the operator out of the
    /// TUI.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self::synthesized_local_default());
            }
            Err(e) => return Err(Error::Io(e)),
        };
        let parsed: TomlFile = toml::from_str(&raw).map_err(Error::Toml)?;
        let mut hosts = parsed.host;
        // Tilde-expand Unix socket paths so `socket = "~/.cm/..."`
        // does what the user expects without a manual canonicalize.
        // SshUnix `remote_socket` is left untouched — the remote
        // host's `$HOME` may not match the local one.
        for h in &mut hosts {
            if let HostTransport::Unix { socket } = &mut h.transport {
                *socket = expand_tilde(socket);
            }
            if let Some(f) = &h.operator_token_file {
                h.operator_token_file = Some(expand_tilde(f));
            }
        }
        let cfg = HostsConfig { hosts };
        cfg.validate()?;
        Ok(cfg)
    }

    /// In-memory single-entry `[[host]] name="local" transport="unix"
    /// default=true` config. Doesn't touch the filesystem — `App::new`'s
    /// malformed-config fallback uses this so the synthesis is infallible
    /// by construction.
    ///
    /// Reviewer-round (12a): the previous shape called `load` against a
    /// sentinel path like `/dev/null/hosts.toml-nonexistent` and
    /// `.expect`ed the synthesized branch. On Unix, opening a child
    /// path of `/dev/null` returns `NotADirectory` rather than
    /// `NotFound`, so the sentinel hit the I/O-error branch and the
    /// `.expect` panicked — defeating the exact lockout-prevention the
    /// fallback was designed for. This constructor removes the
    /// filesystem touch entirely.
    pub fn synthesized_local_default() -> Self {
        HostsConfig {
            hosts: vec![HostConfig {
                id: HostId::local(),
                transport: HostTransport::Unix {
                    socket: cm_daemon::default_socket_path(),
                },
                default: true,
                operator_token: None,
                operator_token_file: None,
            }],
        }
    }

    fn validate(&self) -> Result<(), Error> {
        // Reserved empty name. Caught before duplicate-detection
        // so the error message is the more actionable one when
        // multiple entries are misconfigured this way.
        for h in &self.hosts {
            if h.id.0.is_empty() {
                return Err(Error::ReservedHostName);
            }
        }

        // Round 3 F3: filename-safe host-name validation. The
        // host name becomes a substring of the per-spawn
        // tunnel socket path (`<dir>/cm-host-<name>-<rnd>.sock`),
        // so `name = "../bogus"` would escape the tunnel dir
        // and `name = "prod/us"` would nest into a non-existent
        // subdir. Strict allow-list: `[A-Za-z0-9._-]+`,
        // ≤64 chars, no leading dot, no `..` substring.
        for h in &self.hosts {
            validate_host_name(&h.id.0)?;
        }

        // Duplicate names. Case-sensitive — `Local` and `local`
        // count as distinct. Surface ALL collisions in the error
        // (not just the first) so the user fixes them in one pass.
        let mut seen = HashSet::new();
        let mut dupes = Vec::new();
        for h in &self.hosts {
            if !seen.insert(&h.id.0) {
                dupes.push(h.id.0.clone());
            }
        }
        if !dupes.is_empty() {
            dupes.sort();
            dupes.dedup();
            return Err(Error::DuplicateHostName(dupes));
        }

        // 12h: TcpTls is now a real variant. Validate the
        // fingerprint shape here rather than at dial time so a
        // typo in hosts.toml surfaces at TUI startup with the
        // bad host name in the error, not at first-click.
        for h in &self.hosts {
            if let HostTransport::TcpTls {
                tls_fingerprint, ..
            } = &h.transport
            {
                validate_tls_fingerprint(&h.id.0, tls_fingerprint)?;
            }
        }

        // Exactly one default. Zero is a configuration error (the
        // TUI needs SOMETHING to be the default-active host on
        // launch); two is ambiguous and the user must pick.
        let defaults: Vec<String> = self
            .hosts
            .iter()
            .filter(|h| h.default)
            .map(|h| h.id.0.clone())
            .collect();
        match defaults.len() {
            0 => Err(Error::NoDefaultHost),
            1 => Ok(()),
            _ => Err(Error::MultipleDefaultHosts(defaults)),
        }
    }
}

/// 12h: validate `tls_fingerprint` is a SHA-256 hex digest, either
/// 64 lower/upper hex chars or 32 colon-separated bytes (the
/// `openssl x509 -fingerprint -sha256` shape). Conversion to bytes
/// is deferred to dial time — the loader only proves the input
/// could become a 32-byte digest.
fn validate_tls_fingerprint(
    host_name: &str,
    raw: &str,
) -> Result<(), Error> {
    let cleaned: String = raw
        .chars()
        .filter(|c| *c != ':' && !c.is_whitespace())
        .collect();
    if cleaned.len() != 64
        || !cleaned.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(Error::InvalidTlsFingerprint {
            host: host_name.to_string(),
            raw: raw.to_string(),
        });
    }
    Ok(())
}

/// Parse a `tls_fingerprint` value into its 32 raw bytes. Used by
/// the dialer at connection time. Returns an io::Error so the
/// dialer can surface it without an extra error type.
pub fn parse_tls_fingerprint(raw: &str) -> io::Result<[u8; 32]> {
    let cleaned: String = raw
        .chars()
        .filter(|c| *c != ':' && !c.is_whitespace())
        .collect();
    if cleaned.len() != 64
        || !cleaned.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "tls_fingerprint must be 64 hex chars or 32 \
                 colon-separated bytes; got {:?}",
                raw,
            ),
        ));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16)
            .expect("hex validation pre-checked");
    }
    Ok(out)
}

/// Round 3 F3: enforce that a host name is a filename-safe token
/// suitable to embed in a path component. The tunnel socket
/// path is `<tunnel_dir>/cm-host-<name>-<random>.sock`; a
/// name with `/`, `..`, or other path separators would either
/// nest into a subdir we don't own or escape the tunnel dir
/// entirely.
///
/// Allowed: `[A-Za-z0-9._-]+`, 1..=64 chars, no leading dot,
/// no `..` substring. Practical examples that pass: `local`,
/// `cm-manager`, `prod_us.dev`, `worker-01`. Examples that
/// fail: `prod/us` (slash), `..` (dot-dot), `.hidden` (leading
/// dot), the empty string (handled separately upstream).
fn validate_host_name(name: &str) -> Result<(), Error> {
    if name.len() > 64 {
        return Err(Error::InvalidHostName {
            name: name.to_string(),
            reason: "name exceeds 64 characters",
        });
    }
    if name.starts_with('.') {
        return Err(Error::InvalidHostName {
            name: name.to_string(),
            reason: "name must not start with `.` (would be a hidden file in the tunnel dir)",
        });
    }
    if name.contains("..") {
        return Err(Error::InvalidHostName {
            name: name.to_string(),
            reason: "name must not contain `..` (parent-directory traversal)",
        });
    }
    for c in name.chars() {
        let allowed = c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-';
        if !allowed {
            return Err(Error::InvalidHostName {
                name: name.to_string(),
                reason: "name must match [A-Za-z0-9._-]+ (slashes, backslashes, \
                         whitespace, and non-ASCII are not allowed)",
            });
        }
    }
    Ok(())
}

/// Wire-level deserialization wrapper. `[[host]]` in TOML becomes
/// a `Vec<HostConfig>` field named `host` (singular, matching the
/// TOML array name). Not exposed publicly — consumers see
/// `HostsConfig` after validation.
#[derive(Deserialize)]
struct TomlFile {
    #[serde(default)]
    host: Vec<HostConfig>,
}

/// Errors surfaced by `HostsConfig::load`. Each variant carries
/// enough context for an operator-facing error message — the
/// `Display` impl renders them at the actionable level.
#[derive(Debug)]
pub enum Error {
    /// Filesystem error other than `NotFound` (which synthesizes
    /// the local default rather than erroring).
    Io(io::Error),
    /// `toml` parser rejected the file.
    Toml(toml::de::Error),
    /// Two or more `[[host]]` entries share the same `name`.
    /// Carries the list of colliding names, deduplicated +
    /// sorted.
    DuplicateHostName(Vec<String>),
    /// No entry has `default = true`. The TUI needs SOMETHING to
    /// be the default-active host on launch.
    NoDefaultHost,
    /// Two or more entries have `default = true`. Ambiguous —
    /// the user must pick.
    MultipleDefaultHosts(Vec<String>),
    /// An entry has `name = ""` (empty string). Reserved.
    ReservedHostName,
    /// 12h: a [`HostTransport::TcpTls`] entry has a
    /// `tls_fingerprint` field that isn't a 64-hex / 32-byte
    /// SHA-256 digest. Validated at load time so a typo
    /// surfaces at TUI startup, not at first-click.
    InvalidTlsFingerprint {
        host: String,
        raw: String,
    },
    /// Round 3 F3: host name failed the filename-safe regex.
    /// `reason` is a short human-readable string naming the
    /// specific rule that was violated (length, leading dot,
    /// `..`, invalid char).
    InvalidHostName {
        name: String,
        reason: &'static str,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "hosts.toml I/O error: {}", e),
            Error::Toml(e) => write!(f, "hosts.toml parse error: {}", e),
            Error::DuplicateHostName(names) => write!(
                f,
                "hosts.toml: duplicate host name(s): {}. \
                 Host names must be unique (case-sensitive).",
                names.join(", "),
            ),
            Error::NoDefaultHost => write!(
                f,
                "hosts.toml: no entry has `default = true`. Mark \
                 exactly one [[host]] as the default for new-session \
                 creation.",
            ),
            Error::MultipleDefaultHosts(names) => write!(
                f,
                "hosts.toml: multiple entries have `default = true`: {}. \
                 Exactly one [[host]] must be marked default.",
                names.join(", "),
            ),
            Error::ReservedHostName => write!(
                f,
                "hosts.toml: a [[host]] entry has `name = \"\"` \
                 (empty string). The empty name is reserved; pick \
                 a real identifier (e.g. \"local\" or \"manager\").",
            ),
            Error::InvalidTlsFingerprint { host, raw } => write!(
                f,
                "hosts.toml: [[host]] `{}` has invalid \
                 `tls_fingerprint = {:?}`. Expected a SHA-256 \
                 digest (64 hex chars, or 32 colon-separated \
                 bytes — `openssl x509 -fingerprint -sha256` \
                 format).",
                host, raw,
            ),
            Error::InvalidHostName { name, reason } => write!(
                f,
                "hosts.toml: invalid host name `{}`: {}. Host \
                 names must match [A-Za-z0-9._-]+ (1..=64 chars, \
                 no leading dot, no `..`).",
                name, reason,
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Default path: `~/.cm/hosts.toml`. Mirrors `path::dot_cm_dir()`
/// on the daemon side so the resolver is consistent across the
/// repo.
pub fn default_path() -> PathBuf {
    cm_daemon::path::dot_cm_dir().join("hosts.toml")
}

/// Expand `~` and `~/...` to the user's home directory. Other
/// shell-isms (`$VAR`, `..`) are left alone — operators should
/// write absolute paths if they want them.
fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if s == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T_g3a_synthesized — file doesn't exist → loader returns a
    /// single-entry config with the local default. The socket
    /// path matches `cm_daemon::default_socket_path()` so the
    /// rest of the daemon resolver stays consistent.
    #[test]
    fn t_g3a_synthesized() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("does-not-exist.toml");
        let cfg = HostsConfig::load(&path).expect("synthesized default");
        assert_eq!(cfg.hosts.len(), 1, "exactly one entry synthesized");
        let entry = &cfg.hosts[0];
        assert_eq!(entry.id, HostId::local());
        assert!(entry.default, "synthesized entry is the default");
        match &entry.transport {
            HostTransport::Unix { socket } => {
                assert_eq!(
                    socket,
                    &cm_daemon::default_socket_path(),
                    "synthesized socket must match cm_daemon::default_socket_path()",
                );
            }
            other => panic!("expected Unix transport, got {:?}", other),
        }
        assert!(cfg.default_host().is_some());
        assert_eq!(cfg.default_host().unwrap().id, HostId::local());
    }

    /// T_g3a_multi_host — real file with local + manager.
    /// Verifies the full wire shape: tag-named transport variants,
    /// optional ssh_user, default flag, tilde expansion on Unix
    /// sockets.
    #[test]
    fn t_g3a_multi_host() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        // Override HOME so tilde-expansion is deterministic.
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let path = tmp.path().join("hosts.toml");
        std::fs::write(
            &path,
            r#"
[[host]]
name = "local"
transport = "unix"
socket = "~/.cm/daemon.sock"
default = true

[[host]]
name = "manager"
transport = "ssh-unix"
ssh_host = "cm-manager"
ssh_user = "lucas"
remote_socket = "/home/lucas/.cm/daemon.sock"
"#,
        )
        .expect("write hosts.toml");

        let cfg = HostsConfig::load(&path).expect("valid multi-host config");
        assert_eq!(cfg.hosts.len(), 2);

        // Entry 1: local with tilde-expanded socket path.
        let local = &cfg.hosts[0];
        assert_eq!(local.id, HostId::local());
        assert!(local.default);
        match &local.transport {
            HostTransport::Unix { socket } => {
                assert_eq!(
                    socket,
                    &tmp.path().join(".cm/daemon.sock"),
                    "~/ should expand against $HOME",
                );
            }
            other => panic!("expected Unix, got {:?}", other),
        }

        // Entry 2: manager via SSH-unix, with optional ssh_user.
        let manager = &cfg.hosts[1];
        assert_eq!(manager.id, HostId::new("manager"));
        assert!(!manager.default);
        match &manager.transport {
            HostTransport::SshUnix {
                ssh_host,
                ssh_user,
                remote_socket,
            } => {
                assert_eq!(ssh_host, "cm-manager");
                assert_eq!(ssh_user.as_deref(), Some("lucas"));
                assert_eq!(
                    remote_socket,
                    &PathBuf::from("/home/lucas/.cm/daemon.sock"),
                    "remote_socket should NOT be tilde-expanded \
                     (the remote HOME may differ)",
                );
            }
            other => panic!("expected SshUnix, got {:?}", other),
        }

        // default_host / find lookup.
        assert_eq!(cfg.default_host().unwrap().id, HostId::local());
        assert_eq!(
            cfg.find(&HostId::new("manager")).unwrap().id,
            HostId::new("manager"),
        );
        assert!(cfg.find(&HostId::new("nope")).is_none());

        // Restore HOME.
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// T_g3a_validation_failures — every Error variant the loader
    /// can produce.
    #[test]
    fn t_g3a_validation_failures() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");

        // (1) No default host.
        let p = tmp.path().join("no-default.toml");
        std::fs::write(
            &p,
            r#"
[[host]]
name = "local"
transport = "unix"
socket = "/tmp/x.sock"
"#,
        )
        .unwrap();
        match HostsConfig::load(&p) {
            Err(Error::NoDefaultHost) => {}
            other => panic!("expected NoDefaultHost, got {:?}", other),
        }

        // (2) Multiple default hosts.
        let p = tmp.path().join("two-defaults.toml");
        std::fs::write(
            &p,
            r#"
[[host]]
name = "a"
transport = "unix"
socket = "/tmp/a.sock"
default = true

[[host]]
name = "b"
transport = "unix"
socket = "/tmp/b.sock"
default = true
"#,
        )
        .unwrap();
        match HostsConfig::load(&p) {
            Err(Error::MultipleDefaultHosts(names)) => {
                assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected MultipleDefaultHosts, got {:?}", other),
        }

        // (3) Duplicate host name. Both must surface; sort + dedup
        //     in the error means the operator sees one fixable name.
        let p = tmp.path().join("dupes.toml");
        std::fs::write(
            &p,
            r#"
[[host]]
name = "local"
transport = "unix"
socket = "/tmp/1.sock"
default = true

[[host]]
name = "local"
transport = "unix"
socket = "/tmp/2.sock"
"#,
        )
        .unwrap();
        match HostsConfig::load(&p) {
            Err(Error::DuplicateHostName(names)) => {
                assert_eq!(names, vec!["local".to_string()]);
            }
            other => panic!("expected DuplicateHostName, got {:?}", other),
        }

        // (4) Reserved empty name.
        let p = tmp.path().join("empty-name.toml");
        std::fs::write(
            &p,
            r#"
[[host]]
name = ""
transport = "unix"
socket = "/tmp/x.sock"
default = true
"#,
        )
        .unwrap();
        match HostsConfig::load(&p) {
            Err(Error::ReservedHostName) => {}
            other => panic!("expected ReservedHostName, got {:?}", other),
        }

        // (5) 12h: TcpTls is a real variant now. A well-formed
        //     entry loads cleanly; a malformed fingerprint
        //     surfaces InvalidTlsFingerprint with the host name
        //     in the message so the operator fixes it in one
        //     round.
        let p = tmp.path().join("tcp-tls-bad-fp.toml");
        std::fs::write(
            &p,
            r#"
[[host]]
name = "manager"
transport = "tcp-tls"
addr = "34.11.80.141:8443"
tls_fingerprint = "not-a-real-fingerprint"
default = true
"#,
        )
        .unwrap();
        match HostsConfig::load(&p) {
            Err(Error::InvalidTlsFingerprint { host, raw }) => {
                assert_eq!(host, "manager");
                assert_eq!(raw, "not-a-real-fingerprint");
            }
            other => panic!(
                "expected InvalidTlsFingerprint, got {:?}",
                other,
            ),
        }

        // (5b) 12h: a valid TcpTls entry parses + validates. Uses
        //     a 64-hex SHA-256 digest. `auth_env` defaults to
        //     `CM_DAEMON_TOKEN` when omitted.
        let p = tmp.path().join("tcp-tls-ok.toml");
        std::fs::write(
            &p,
            r#"
[[host]]
name = "manager"
transport = "tcp-tls"
addr = "34.11.80.141:8443"
tls_fingerprint = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
default = true
"#,
        )
        .unwrap();
        let cfg = HostsConfig::load(&p).expect("valid tcp-tls config");
        match &cfg.hosts[0].transport {
            HostTransport::TcpTls {
                addr,
                tls_fingerprint,
                auth_env,
            } => {
                assert_eq!(addr, "34.11.80.141:8443");
                assert_eq!(tls_fingerprint.len(), 64);
                assert_eq!(auth_env, "CM_DAEMON_TOKEN");
            }
            other => panic!("expected TcpTls, got {:?}", other),
        }

        // (6) TOML parse error (bare malformed file).
        let p = tmp.path().join("malformed.toml");
        std::fs::write(&p, b"not valid toml = = =").unwrap();
        match HostsConfig::load(&p) {
            Err(Error::Toml(_)) => {}
            other => panic!("expected Toml, got {:?}", other),
        }

        // (7) Empty file (zero [[host]] entries) → NoDefaultHost
        //     because validation runs on an empty Vec and finds
        //     zero defaults.
        let p = tmp.path().join("empty.toml");
        std::fs::write(&p, b"").unwrap();
        match HostsConfig::load(&p) {
            Err(Error::NoDefaultHost) => {}
            other => panic!(
                "expected NoDefaultHost for empty file, got {:?}",
                other,
            ),
        }
    }

    /// Tilde expansion sanity check — verifies the helper directly
    /// in case the multi-host test path ever masks the bug.
    #[test]
    fn tilde_expansion_resolves_against_home() {
        let _guard = crate::test_support::home_lock();
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/test-home");
        }
        assert_eq!(
            expand_tilde(&PathBuf::from("~/.cm/foo")),
            PathBuf::from("/test-home/.cm/foo"),
        );
        assert_eq!(
            expand_tilde(&PathBuf::from("~")),
            PathBuf::from("/test-home"),
        );
        // Non-tilde paths untouched.
        assert_eq!(
            expand_tilde(&PathBuf::from("/absolute/path")),
            PathBuf::from("/absolute/path"),
        );
        match orig {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    // -----------------------------------------------------------
    // Round 3 F3: host-name filename-safety tests.
    // -----------------------------------------------------------

    /// Helper: build a hosts.toml with one named entry plus a
    /// uniquely-named default. Returns the load result so tests
    /// can pin whether validation accepted or rejected the
    /// candidate name. The default uses a synthetic name
    /// (`__test_default__`) to avoid colliding with any
    /// candidate name the tests might pass.
    fn load_with_host_name(name: &str) -> Result<HostsConfig, Error> {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("hosts.toml");
        // Use a TOML literal-string ('...') for the candidate
        // name so backslashes don't get re-interpreted as TOML
        // escape sequences (e.g. `\b` → backspace).
        let content = format!(
            "[[host]]\n\
             name = \"__test_default__\"\n\
             transport = \"unix\"\n\
             socket = \"/tmp/local.sock\"\n\
             default = true\n\
             \n\
             [[host]]\n\
             name = '{}'\n\
             transport = \"ssh-unix\"\n\
             ssh_host = \"irrelevant\"\n\
             remote_socket = \"/irrelevant.sock\"\n",
            name,
        );
        std::fs::write(&path, content).expect("write hosts.toml");
        HostsConfig::load(&path)
    }

    /// `operator_token` (inline) and `operator_token_file` (local path,
    /// tilde-expanded) parse per `[[host]]`; both are optional and absent
    /// entries read back as `None` so existing files keep working.
    #[test]
    fn operator_token_fields_parse_per_host() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let path = tmp.path().join("hosts.toml");
        std::fs::write(
            &path,
            r#"
[[host]]
name = "local"
transport = "unix"
socket = "/tmp/local.sock"
default = true

[[host]]
name = "manager"
transport = "ssh-unix"
ssh_host = "cm-manager"
remote_socket = "/home/lucas/.cm/daemon.sock"
operator_token = "inline-tok"

[[host]]
name = "other"
transport = "ssh-unix"
ssh_host = "cm-other"
remote_socket = "/home/lucas/.cm/daemon.sock"
operator_token_file = "~/secrets/other-token"
"#,
        )
        .expect("write hosts.toml");
        let cfg = HostsConfig::load(&path).expect("load");
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let local = cfg.find(&HostId::local()).expect("local");
        assert!(local.operator_token.is_none() && local.operator_token_file.is_none());
        let manager = cfg.find(&HostId::new("manager")).expect("manager");
        assert_eq!(manager.operator_token.as_deref(), Some("inline-tok"));
        assert!(manager.operator_token_file.is_none());
        let other = cfg.find(&HostId::new("other")).expect("other");
        assert!(other.operator_token.is_none());
        assert_eq!(
            other.operator_token_file.as_deref(),
            Some(tmp.path().join("secrets/other-token").as_path()),
            "operator_token_file must be tilde-expanded at load",
        );
    }

    /// Round 3 F3: host names with path separators (`/`, `\`)
    /// must be rejected.
    #[test]
    fn host_name_rejects_path_separators() {
        for name in ["prod/us", "a\\b", "with/slash"].iter() {
            match load_with_host_name(name) {
                Err(Error::InvalidHostName { name: n, .. }) => {
                    assert_eq!(&n, name);
                }
                other => panic!(
                    "host name {:?} must be rejected with \
                     InvalidHostName; got: {:?}",
                    name, other,
                ),
            }
        }
    }

    /// Round 3 F3: host names with `..` (anywhere) must be
    /// rejected. Examples: `..`, `a..b`, `a/../b`.
    #[test]
    fn host_name_rejects_dot_dot() {
        for name in ["..", "a..b", "x..y..z"].iter() {
            match load_with_host_name(name) {
                Err(Error::InvalidHostName { name: n, reason: _ }) => {
                    assert_eq!(&n, name);
                }
                other => panic!(
                    "host name {:?} must be rejected with \
                     InvalidHostName; got: {:?}",
                    name, other,
                ),
            }
        }
        // `a/../b` is rejected by the path-separator check
        // first; that's also an InvalidHostName error.
        match load_with_host_name("a/../b") {
            Err(Error::InvalidHostName { .. }) => {}
            other => panic!(
                "`a/../b` must be rejected; got: {:?}",
                other,
            ),
        }
    }

    /// Round 3 F3: host names with a leading dot must be
    /// rejected (would be a hidden file in the tunnel dir).
    #[test]
    fn host_name_rejects_leading_dot() {
        for name in [".foo", ".hidden", "."].iter() {
            match load_with_host_name(name) {
                Err(Error::InvalidHostName { name: n, .. }) => {
                    assert_eq!(&n, name);
                }
                other => panic!(
                    "host name {:?} must be rejected with \
                     InvalidHostName; got: {:?}",
                    name, other,
                ),
            }
        }
    }

    /// Round 3 F3: alnum + dash + underscore + dot are accepted.
    /// Pin the positive case so we don't regress the validator
    /// into rejecting realistic operator-chosen names like
    /// `manager-prod_us.dev`.
    #[test]
    fn host_name_accepts_alnum_dash_underscore_dot() {
        for name in [
            "local",
            "cm-manager",
            "worker-01",
            "prod_us.dev",
            "manager-prod_us.dev",
            "A.B-C_D",
            "abc123",
        ]
        .iter()
        {
            let r = load_with_host_name(name);
            assert!(
                r.is_ok(),
                "host name {:?} should be accepted; got: {:?}",
                name,
                r.err(),
            );
        }
    }

    /// Round 3 F3: too-long names (>64 chars) are rejected.
    #[test]
    fn host_name_rejects_too_long() {
        let name = "a".repeat(65);
        match load_with_host_name(&name) {
            Err(Error::InvalidHostName { name: n, reason }) => {
                assert_eq!(n, name);
                assert!(reason.contains("64"));
            }
            other => panic!(
                "65-char name must be rejected; got: {:?}",
                other,
            ),
        }
        // 64-char name is fine.
        let ok_name = "a".repeat(64);
        assert!(load_with_host_name(&ok_name).is_ok());
    }
}
