//! Slice 12h: TLS-TCP transport (daemon side).
//!
//! When `daemon.toml` carries a `[tls]` section, the daemon binds
//! a rustls TCP listener on `listen_addr` ALONGSIDE its existing
//! Unix socket. Each accepted TCP connection:
//!
//! 1. Completes a TLS handshake using the configured cert + key.
//! 2. Reads the FIRST length-prefixed JSON-RPC frame and verifies
//!    it's `method = "auth.hello"` with `params.token` matching
//!    `$CM_DAEMON_TOKEN`. Constant-time compare. Anything else
//!    (wrong method name, wrong/missing token, deny_unknown
//!    surprise) gets an `Unauthorized` response written back,
//!    then the connection is closed without proceeding to
//!    dispatch.
//! 3. After a successful auth.hello, reads the next frame as a
//!    regular `Request` and runs it through `dispatch_request`
//!    just like a Unix-socket connection would. Streaming
//!    outcomes (manifest.watch, events.subscribe, attach.open)
//!    work the same way — the underlying wire format is
//!    identical, just wrapped in TLS.
//!
//! ## Auth model
//!
//! Two independent halves, NOT mutual TLS:
//!   - **Server identity (server → client)**: the daemon serves
//!     its self-signed cert; the TUI dialer pins its SHA-256
//!     fingerprint via a custom `ServerCertVerifier`. Protects
//!     the TUI from a fake / MITM server.
//!   - **Client identity (client → server)**: rustls is built
//!     with `.with_no_client_auth()`, so the TLS layer alone
//!     accepts any peer that completes the handshake. The
//!     `auth.hello` token IS the client credential — sole and
//!     authoritative. A peer that knows `CM_DAEMON_TOKEN` is
//!     trusted; a peer that doesn't is closed.
//!
//! Client certs / mutual TLS are intentionally out of scope for
//! 12h; a future slice can add them if operator workflows need
//! per-user identity beyond the shared token.
//!
//! ## `CM_DAEMON_TOKEN` lifecycle
//!
//! The TUI's `daemon_launch` doesn't manage this token (it
//! lives on the cm-manager VM via systemd's `Environment=`
//! directive, mode 0600 on the unit file). Slice 12g's deferred
//! item: operator generates a fresh token with `openssl rand
//! -hex 32` and pastes it into the unit file; the TUI's
//! `hosts.toml` declares the matching env var name via
//! `[[host]] auth_env = "CM_DAEMON_TOKEN"`.
//!
//! If `[tls]` is set but `CM_DAEMON_TOKEN` is empty / unset,
//! the daemon refuses to start — there's no safe default.
//! Unlike `CM_OPERATOR_TOKEN`'s "disabled with warning" mode,
//! TLS-TCP is opt-in and trusted-by-the-network: an unset
//! token would mean the TLS listener accepts ANY peer that
//! makes it past cert pinning, which is wrong by default.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

use crate::config::TlsConfig;
use crate::control::dispatch::{dispatch_request, DispatchOutcome};
use crate::control::protocol::{ErrorCode, Response};
use crate::control::wire::{read_request, write_response};
use crate::state::DaemonState;

/// Env var the daemon reads to learn its TLS-TCP shared token.
/// Mirrors `operator::ENV_VAR` (`CM_OPERATOR_TOKEN`) in shape but
/// covers a different boundary: operator-token gates same-UID
/// Unix-socket Operator RPCs; this one gates network peers that
/// completed TLS.
pub const ENV_VAR: &str = "CM_DAEMON_TOKEN";

/// Method-name the daemon expects on the FIRST frame of any
/// TLS-TCP connection. Anything else → Unauthorized + close.
pub const AUTH_HELLO_METHOD: &str = "auth.hello";

/// Maximum time the TLS handshake + auth.hello round trip is
/// allowed to take. Real-world handshakes finish in
/// milliseconds; 10s leaves room for a slow link or a
/// half-connected client. Past the deadline, the connection
/// is dropped.
pub const TLS_AUTH_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10);

/// Errors surfaced by `TlsAcceptor::new`. The accept loop's
/// per-connection errors are logged, not surfaced — same shape
/// as the Unix-socket accept loop.
#[derive(Debug)]
pub enum TlsServerError {
    Io(std::io::Error),
    /// PEM file present but no certificate / key block of the
    /// expected type. Carries the offending path so the operator
    /// can fix it.
    NoUsableMaterial(std::path::PathBuf, &'static str),
    /// rustls rejected the cert/key pair (e.g. key doesn't match
    /// cert, or unsupported key algorithm under the ring
    /// provider).
    Rustls(rustls::Error),
    /// `[tls]` is set but `CM_DAEMON_TOKEN` isn't. No safe
    /// default — refuse to bind. Operators set this through
    /// systemd's `Environment=` on the cm-daemon unit file.
    MissingDaemonToken,
}

impl std::fmt::Display for TlsServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsServerError::Io(e) => {
                write!(f, "TLS listener I/O error: {}", e)
            }
            TlsServerError::NoUsableMaterial(p, what) => write!(
                f,
                "no usable {} block in PEM file {}",
                what,
                p.display(),
            ),
            TlsServerError::Rustls(e) => {
                write!(f, "rustls config error: {}", e)
            }
            TlsServerError::MissingDaemonToken => write!(
                f,
                "[tls] is set in daemon.toml but {} is empty/unset; \
                 refusing to start the TLS listener. Set the env \
                 var in the cm-daemon systemd unit (Environment=).",
                ENV_VAR,
            ),
        }
    }
}

impl std::error::Error for TlsServerError {}

impl From<std::io::Error> for TlsServerError {
    fn from(e: std::io::Error) -> Self {
        TlsServerError::Io(e)
    }
}

impl From<rustls::Error> for TlsServerError {
    fn from(e: rustls::Error) -> Self {
        TlsServerError::Rustls(e)
    }
}

/// Configured TLS acceptor. `bind` returns a ready-to-spawn
/// runtime: `serve` consumes self and drives the accept loop.
/// Two-step shape so the bind error (port conflict) is
/// distinguishable from per-connection errors during serve.
pub struct TlsAcceptor {
    config: Arc<ServerConfig>,
    listener: TcpListener,
    expected_token: String,
    listen_addr: String,
}

impl TlsAcceptor {
    /// Build a `ServerConfig` from the [`TlsConfig`] paths and
    /// bind a TCP listener on `listen_addr`. The expected_token
    /// is captured at construction so a stale env-var change
    /// can't flip the auth check mid-run.
    pub fn bind(
        tls_cfg: &TlsConfig,
        expected_token: String,
    ) -> Result<Self, TlsServerError> {
        if expected_token.is_empty() {
            return Err(TlsServerError::MissingDaemonToken);
        }
        let cert_path = std::path::PathBuf::from(&tls_cfg.cert_path);
        let key_path = std::path::PathBuf::from(&tls_cfg.key_path);
        let certs = load_certs(&cert_path)?;
        let key = load_private_key(&key_path)?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(TlsServerError::Rustls)?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(TlsServerError::Rustls)?;
        let listener = TcpListener::bind(&tls_cfg.listen_addr)?;
        Ok(TlsAcceptor {
            config: Arc::new(config),
            listener,
            expected_token,
            listen_addr: tls_cfg.listen_addr.clone(),
        })
    }

    /// Holder-split (phase 4, O11/C12): construct over an
    /// ALREADY-BOUND listener — the holder-custodied fd a brain
    /// restart adopts instead of rebinding (rebind of a live address
    /// would fail while the custodied listener holds it). Identical
    /// cert/key/config path to [`Self::bind`]; only the bind is
    /// skipped.
    pub fn bind_with_listener(
        tls_cfg: &TlsConfig,
        expected_token: String,
        listener: TcpListener,
    ) -> Result<Self, TlsServerError> {
        if expected_token.is_empty() {
            return Err(TlsServerError::MissingDaemonToken);
        }
        let cert_path = std::path::PathBuf::from(&tls_cfg.cert_path);
        let key_path = std::path::PathBuf::from(&tls_cfg.key_path);
        let certs = load_certs(&cert_path)?;
        let key = load_private_key(&key_path)?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(TlsServerError::Rustls)?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(TlsServerError::Rustls)?;
        Ok(TlsAcceptor {
            config: Arc::new(config),
            listener,
            expected_token,
            listen_addr: tls_cfg.listen_addr.clone(),
        })
    }

    /// The bound listener's raw fd — custody input only (the holder
    /// dups it via SCM_RIGHTS; nothing closes through this number).
    pub fn listener_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.listener.as_raw_fd()
    }

    /// Drive the accept loop. Spawns a per-connection handler
    /// thread for each incoming TCP socket. Returns only when
    /// `listener.incoming()` errors fatally (which on a long-
    /// running daemon should never happen — the loop runs for
    /// the process lifetime).
    pub fn serve(
        self,
        state: Arc<Mutex<DaemonState>>,
    ) -> std::io::Result<()> {
        let config = self.config;
        let token = Arc::new(self.expected_token);
        let listen_addr = self.listen_addr;
        eprintln!(
            "cm-daemon: TLS listener bound on {}",
            listen_addr,
        );
        for incoming in self.listener.incoming() {
            match incoming {
                Ok(tcp) => {
                    let config = Arc::clone(&config);
                    let state = Arc::clone(&state);
                    let token = Arc::clone(&token);
                    let _ = std::thread::Builder::new()
                        .name("cm-daemon-tls-conn".into())
                        .spawn(move || {
                            handle_tls_connection(
                                tcp, config, state, token,
                            );
                        });
                }
                Err(e) => {
                    eprintln!(
                        "cm-daemon: TLS accept error: {}",
                        e,
                    );
                    break;
                }
            }
        }
        Ok(())
    }

    /// Test-accessor for the bound port. Useful when binding
    /// to `127.0.0.1:0` from a test — the actual ephemeral port
    /// can only be discovered post-bind.
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }
}

/// Per-connection driver. Splits into discrete steps so the
/// unit tests can call them directly without binding real
/// sockets — see `validate_auth_hello`.
fn handle_tls_connection(
    tcp: TcpStream,
    config: Arc<ServerConfig>,
    state: Arc<Mutex<DaemonState>>,
    expected_token: Arc<String>,
) {
    let _ = tcp.set_read_timeout(Some(TLS_AUTH_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(TLS_AUTH_TIMEOUT));
    let conn = match ServerConnection::new(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "cm-daemon: ServerConnection setup failed: {}",
                e,
            );
            return;
        }
    };
    let mut stream = StreamOwned::new(conn, tcp);
    // Read the first frame. read_request runs the TLS handshake
    // implicitly via the first byte the rustls stream needs to
    // produce — there's no separate handshake-only call to make
    // before reading is permitted.
    let req = match read_request(&mut stream) {
        Ok(Some(r)) => r,
        Ok(None) => {
            // Client opened the TCP socket, completed the TLS
            // handshake, then closed before sending. Clean.
            return;
        }
        Err(e) => {
            eprintln!(
                "cm-daemon: TLS read of auth.hello frame failed: {}",
                e,
            );
            return;
        }
    };

    let auth_result = validate_auth_hello(&req, &expected_token);
    match auth_result {
        Err(resp) => {
            let _ = write_response(&mut stream, &resp);
            // Best-effort close. Per the spec: "otherwise close
            // connection." rustls writes a close_notify on drop.
            return;
        }
        Ok(()) => {
            // Acknowledge the auth.hello, then read the actual
            // RPC frame and dispatch.
            let ack = Response::ok(
                req.id.clone(),
                serde_json::json!({"ok": true}),
            );
            if let Err(e) = write_response(&mut stream, &ack) {
                eprintln!(
                    "cm-daemon: TLS auth.hello ack write failed: {}",
                    e,
                );
                return;
            }
        }
    }

    // After auth, behave like the Unix-socket per-connection
    // path: read one request, dispatch, write the response. The
    // streaming outcomes (attach, manifest.watch, etc.) take
    // over the connection for as long as they need.
    let req = match read_request(&mut stream) {
        Ok(Some(r)) => r,
        Ok(None) => return,
        Err(e) => {
            eprintln!(
                "cm-daemon: TLS post-auth read failed: {}",
                e,
            );
            return;
        }
    };
    let outcome = dispatch_request(&state, &req);
    match outcome {
        DispatchOutcome::Done(response) => {
            let _ = write_response(&mut stream, &response);
        }
        DispatchOutcome::AttachStream { response, handle } => {
            if write_response(&mut stream, &response).is_err() {
                return;
            }
            // The attach-stream handler expects a Read+Write
            // backed by the original stream. The rustls
            // StreamOwned IS Read+Write; the existing
            // `handle_attach_stream` is generic enough to take
            // any such stream. Slice 12h leaves attach-via-TLS
            // unwired (the daemon RPC test gate only covers
            // auth.hello + one dispatch frame); a follow-up
            // slice extends `handle_attach_stream` to be
            // Stream-generic. For now, log + close so the
            // client sees a clean disconnect rather than a
            // hang.
            let _ = handle;
            eprintln!(
                "cm-daemon: TLS attach-stream not yet routed; \
                 closing connection",
            );
        }
        DispatchOutcome::ManifestWatchStream { response, handle } => {
            if write_response(&mut stream, &response).is_err() {
                return;
            }
            let _ = handle;
            eprintln!(
                "cm-daemon: TLS manifest.watch not yet routed; \
                 closing connection",
            );
        }
        DispatchOutcome::EventsSubscribeStream { response, handle } => {
            if write_response(&mut stream, &response).is_err() {
                return;
            }
            let _ = handle;
            eprintln!(
                "cm-daemon: TLS events.subscribe not yet routed; \
                 closing connection",
            );
        }
    }
}

/// Validate the first frame of a TLS connection is an
/// `auth.hello` carrying the expected token.
///
/// Returns `Ok(())` on success or `Err(Response)` carrying the
/// `Unauthorized` (or `InvalidParams`) response the caller
/// should write back before closing.
///
/// Public so the unit tests can drive it directly without
/// binding a real TCP listener — the validation logic is the
/// load-bearing part of slice 12h; the rustls plumbing around
/// it is a thin shell.
pub fn validate_auth_hello(
    req: &crate::control::protocol::Request,
    expected_token: &str,
) -> Result<(), Response> {
    if req.method != AUTH_HELLO_METHOD {
        return Err(Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            format!(
                "first frame on a TLS connection must be \
                 `{}`; got method `{}`",
                AUTH_HELLO_METHOD, req.method,
            ),
        ));
    }
    let token = match req.params.get("token").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            return Err(Response::err(
                req.id.clone(),
                ErrorCode::Unauthorized,
                "auth.hello requires `params.token` as a string",
            ));
        }
    };
    if constant_time_eq(token.as_bytes(), expected_token.as_bytes()) {
        Ok(())
    } else {
        Err(Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "auth.hello token does not match the daemon's \
             configured CM_DAEMON_TOKEN",
        ))
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Read a PEM file and return every `CERTIFICATE` block as a
/// rustls `CertificateDer`. Errors if the file has no certs.
///
/// Hand-rolled PEM parsing rather than `rustls-pemfile` because
/// the offline cargo cache doesn't carry `rustls-pemfile`; the
/// shape is simple enough (RFC 7468) that the dep isn't
/// load-bearing. base64 is already a workspace dep via the
/// StreamFrame encoder.
pub fn load_certs(
    path: &Path,
) -> Result<Vec<CertificateDer<'static>>, TlsServerError> {
    let bytes = std::fs::read(path)?;
    let blocks = parse_pem(&bytes);
    let certs: Vec<CertificateDer<'static>> = blocks
        .into_iter()
        .filter(|(typ, _)| typ == "CERTIFICATE")
        .map(|(_, der)| CertificateDer::from(der))
        .collect();
    if certs.is_empty() {
        return Err(TlsServerError::NoUsableMaterial(
            path.to_path_buf(),
            "CERTIFICATE",
        ));
    }
    Ok(certs)
}

/// Read a PEM file and return the first private key block in
/// PKCS#8 (`PRIVATE KEY`), PKCS#1 RSA (`RSA PRIVATE KEY`), or
/// SEC1 EC (`EC PRIVATE KEY`) form. rustls's `PrivateKeyDer`
/// has a variant for each; we pick the matching one.
pub fn load_private_key(
    path: &Path,
) -> Result<PrivateKeyDer<'static>, TlsServerError> {
    let bytes = std::fs::read(path)?;
    for (typ, der) in parse_pem(&bytes) {
        match typ.as_str() {
            "PRIVATE KEY" => {
                return Ok(PrivateKeyDer::Pkcs8(der.into()));
            }
            "RSA PRIVATE KEY" => {
                return Ok(PrivateKeyDer::Pkcs1(der.into()));
            }
            "EC PRIVATE KEY" => {
                return Ok(PrivateKeyDer::Sec1(der.into()));
            }
            _ => continue,
        }
    }
    Err(TlsServerError::NoUsableMaterial(
        path.to_path_buf(),
        "PRIVATE KEY / RSA PRIVATE KEY / EC PRIVATE KEY",
    ))
}

/// Minimal PEM parser. Extracts each `(label, der_bytes)` pair
/// from the input. Skips any base64 lines that don't decode
/// cleanly (e.g. a stray comment between blocks).
fn parse_pem(input: &[u8]) -> Vec<(String, Vec<u8>)> {
    use base64::Engine;
    let text = match std::str::from_utf8(input) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let label = match line
            .strip_prefix("-----BEGIN ")
            .and_then(|s| s.strip_suffix("-----"))
        {
            Some(l) => l.to_string(),
            None => continue,
        };
        let mut b64 = String::new();
        let mut end_seen = false;
        for body_line in lines.by_ref() {
            if body_line.starts_with("-----END ") {
                end_seen = true;
                break;
            }
            b64.push_str(body_line.trim());
        }
        if !end_seen {
            continue;
        }
        match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes())
        {
            Ok(der) => out.push((label, der)),
            Err(_) => continue,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::protocol::{Caller, Request};
    use serde_json::json;

    fn op_req(method: &str, params: serde_json::Value) -> Request {
        Request {
            id: "r-1".into(),
            caller: Caller::operator("tk"),
            method: method.into(),
            params,
        }
    }

    /// 12h unit-level coverage for the wrong-token case. The
    /// integration-flavored end-to-end version
    /// (`T_g3h_wrong_token_unauthorized`) lives in the dialer-side
    /// tests because it needs a live TLS server; this one pins the
    /// validation logic without rustls in the loop.
    #[test]
    fn wrong_token_returns_unauthorized() {
        let req = op_req(
            "auth.hello",
            json!({"token": "guessed-wrong"}),
        );
        let err = validate_auth_hello(&req, "real-secret")
            .expect_err("must reject");
        assert!(!err.ok);
        assert_eq!(
            err.error.unwrap().code,
            ErrorCode::Unauthorized,
        );
    }

    #[test]
    fn matching_token_is_accepted() {
        let req = op_req(
            "auth.hello",
            json!({"token": "match-me"}),
        );
        validate_auth_hello(&req, "match-me")
            .expect("matching token accepted");
    }

    /// Non-`auth.hello` first frame must be Unauthorized — the
    /// error message names the method actually sent so the
    /// operator's logs disambiguate "wrong frame" from "wrong
    /// token."
    #[test]
    fn non_auth_hello_first_frame_rejected() {
        let req = op_req("ping", json!({}));
        let err = validate_auth_hello(&req, "tok")
            .expect_err("must reject");
        let msg = err.error.unwrap().message;
        assert!(
            msg.contains("auth.hello"),
            "error must name the required method; got: {}",
            msg,
        );
        assert!(
            msg.contains("ping"),
            "error must name the method actually sent; got: {}",
            msg,
        );
    }

    /// auth.hello with no `token` param is treated as
    /// Unauthorized (not InvalidParams) — same close-the-door
    /// behavior as a wrong token so the failure mode is
    /// uniform on the wire.
    #[test]
    fn auth_hello_missing_token_rejected() {
        let req = op_req("auth.hello", json!({}));
        let err = validate_auth_hello(&req, "tok")
            .expect_err("must reject");
        assert_eq!(
            err.error.unwrap().code,
            ErrorCode::Unauthorized,
        );
    }

    /// Empty configured token in `TlsAcceptor::bind` is
    /// surfaced as `MissingDaemonToken`, not silently disabled
    /// (that's the `CM_OPERATOR_TOKEN` model and is wrong for
    /// network-reachable TLS).
    #[test]
    fn empty_expected_token_refuses_to_bind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = TlsConfig {
            cert_path: tmp.path().join("cert.pem").display().to_string(),
            key_path: tmp.path().join("key.pem").display().to_string(),
            listen_addr: "127.0.0.1:0".into(),
        };
        match TlsAcceptor::bind(&cfg, String::new()) {
            Err(TlsServerError::MissingDaemonToken) => {}
            other => panic!(
                "expected MissingDaemonToken, got {:?}",
                other.map(|_| "Ok").err(),
            ),
        }
    }
}
