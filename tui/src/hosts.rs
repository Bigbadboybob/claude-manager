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
//! ## TLS-TCP placeholder (A2 from the Phase 3 plan)
//!
//! `HostTransport::TcpTls` is a placeholder variant — declaring
//! it on the public schema lets a user write a `transport = "tcp-tls"`
//! entry today, but the loader returns `Error::TlsNotImplemented`
//! pointing them at `transport = "ssh-unix"` for now. Slice 12h
//! flips the loader to accept it. The forward-compat error message
//! is the first thing the user sees, so it has to be actionable.

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
    /// Placeholder for slice 12h (TLS-TCP transport). Declaring
    /// the variant on the public schema lets users write
    /// `transport = "tcp-tls"` today; the loader returns
    /// `Error::TlsNotImplemented` so the message names slice 12h
    /// rather than a confusing "unknown transport" parse error.
    ///
    /// Fields are intentionally untyped (single `addr` string +
    /// pass-through `tls_fingerprint`) so 12h can replace this
    /// without a wire-shape break. The acceptance criterion
    /// "TLS-TCP not yet implemented — use transport=ssh-unix for
    /// now" governs the user-facing error.
    TcpTls {
        #[serde(default)]
        addr: Option<String>,
        #[serde(default)]
        tls_fingerprint: Option<String>,
    },
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

        // TcpTls placeholder — reject at load. A future slice 12h
        // flips this to an accept path with rustls dialing.
        for h in &self.hosts {
            if matches!(h.transport, HostTransport::TcpTls { .. }) {
                return Err(Error::TlsNotImplemented(h.id.0.clone()));
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
    /// An entry has `transport = "tcp-tls"`. Phase 3 plan A2:
    /// SSH-unix ships first; TLS-TCP lands in slice 12h. The
    /// message names the workaround.
    TlsNotImplemented(String),
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
            Error::TlsNotImplemented(name) => write!(
                f,
                "hosts.toml: [[host]] `{}` uses `transport = \
                 \"tcp-tls\"`, which is not yet implemented. Use \
                 `transport = \"ssh-unix\"` for remote daemons \
                 today; TLS-TCP lands in Phase 3 slice 12h.",
                name,
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

        // (5) TcpTls placeholder — forward-compat error pointing
        //     at slice 12h. Message must name the workaround
        //     transport (ssh-unix) so the operator fixes it in
        //     one round.
        let p = tmp.path().join("tcp-tls.toml");
        std::fs::write(
            &p,
            r#"
[[host]]
name = "manager"
transport = "tcp-tls"
addr = "34.11.80.141:8443"
default = true
"#,
        )
        .unwrap();
        match HostsConfig::load(&p) {
            Err(Error::TlsNotImplemented(name)) => {
                assert_eq!(name, "manager");
                let msg = Error::TlsNotImplemented(name).to_string();
                assert!(
                    msg.contains("ssh-unix"),
                    "the error message must name the workaround \
                     transport so the operator fixes it in one round; \
                     got: {}",
                    msg,
                );
                assert!(
                    msg.contains("12h"),
                    "and reference the slice that lands TLS-TCP; got: {}",
                    msg,
                );
            }
            other => panic!("expected TlsNotImplemented, got {:?}", other),
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
}
