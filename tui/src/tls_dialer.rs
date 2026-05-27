//! Slice 12h: TLS-TCP transport (TUI side).
//!
//! Dials a daemon that's bound a rustls TCP listener (see
//! `cm_daemon::control::tls`). For each connection:
//!
//! 1. `TcpStream::connect(addr)` to open the underlying socket.
//! 2. Complete the TLS handshake. Server cert validation uses a
//!    custom [`rustls::client::danger::ServerCertVerifier`] that
//!    ignores the chain-of-trust path and SAN/CN checks, and
//!    instead pins the leaf cert's SHA-256 fingerprint against
//!    `TlsDialerSpec::fingerprint`. This is the right shape for
//!    self-signed certs deployed by an operator who already
//!    knows the public key out-of-band.
//! 3. Send an `auth.hello` length-prefixed JSON-RPC frame carrying
//!    the token resolved from `$auth_env` (default
//!    `CM_DAEMON_TOKEN`). Read the daemon's response. Any
//!    non-`ok` response → bubble up an io::Error with the
//!    daemon's error message so the operator sees "Unauthorized"
//!    rather than "connection closed."
//! 4. Return the rustls `StreamOwned` ready for subsequent RPC
//!    frames.
//!
//! The dialer is NOT wired through every TUI call site in 12h
//! proper — slice 12h's acceptance gate is the four named tests
//! (handshake-ok / fingerprint-mismatch / auth.hello-required /
//! wrong-token). Full TUI wiring (manifest.watch / attach /
//! session-touching RPCs over TLS) is a follow-up; the existing
//! `host_pool::ConnectionHandle::socket_path()` returns `None` for
//! `HandleState::TcpTls` and the consumer call sites fall back
//! to their no-socket error path until then.

use std::io;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::SystemTime;

use rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme,
    StreamOwned,
};

use cm_daemon::control::protocol::{Caller, Request};
use cm_daemon::control::wire::{read_response, write_request};

/// Method name expected by the daemon on the first frame of a
/// TLS connection. Mirrors `cm_daemon::control::tls::AUTH_HELLO_METHOD`.
pub const AUTH_HELLO_METHOD: &str = "auth.hello";

/// Default env var the dialer reads to learn the daemon token.
/// Operators can override per host via
/// `[[host]] auth_env = "MY_VAR"` in `hosts.toml`; the default
/// matches the daemon's documented `CM_DAEMON_TOKEN`.
pub const DEFAULT_AUTH_ENV: &str = "CM_DAEMON_TOKEN";

/// How long the dialer waits on the connect + TLS handshake
/// + auth.hello round trip. Real-world: handshake fits in a few
/// hundred ms. 10s leaves room for a slow / distant link.
pub const DIAL_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10);

/// Per-address `connect_timeout` budget. Half of `DIAL_TIMEOUT`
/// is spent on TCP connect; the remainder goes to the post-
/// connect handshake + auth.hello round trip. With a multi-A-
/// record host the worst case is `N * CONNECT_BUDGET` for
/// connect alone — bounded by the number of resolved addrs,
/// still well under the OS-level connect ceiling.
pub const CONNECT_BUDGET: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Immutable spec describing one TLS-TCP daemon endpoint. Stored
/// on a [`crate::host_pool::HandleState::TcpTls`] entry; the
/// dialer clones it per dial.
#[derive(Clone, Debug)]
pub struct TlsDialerSpec {
    /// `host:port` to connect to.
    pub addr: String,
    /// 32 raw bytes — the expected SHA-256 of the leaf cert's
    /// DER encoding. Parsed at config-load time from the
    /// `tls_fingerprint` field of `hosts.toml` (see
    /// `crate::hosts::parse_tls_fingerprint`).
    pub fingerprint: [u8; 32],
    /// Name of the env var the dialer reads at dial time to
    /// obtain the daemon token. Empty / unset env value =>
    /// `Err` from `dial()` (the operator forgot to set it).
    pub auth_env: String,
}

/// The dialer surface. Stateless except for the cloned spec —
/// each `dial` call is independent.
pub struct TlsDialer {
    spec: TlsDialerSpec,
}

impl TlsDialer {
    pub fn new(spec: TlsDialerSpec) -> Self {
        Self { spec }
    }

    /// Open one TLS-TCP connection and complete the auth.hello
    /// handshake. On success returns a rustls `StreamOwned`
    /// ready for further RPC frames; on any failure (TCP
    /// connect, TLS handshake, fingerprint mismatch, missing
    /// env, auth.hello rejected) returns an `io::Error` whose
    /// message is operator-actionable.
    pub fn dial(
        &self,
    ) -> io::Result<StreamOwned<ClientConnection, TcpStream>> {
        // 1. Resolve the token before doing any I/O so a missing
        //    env var fails fast.
        let token = self.resolve_token()?;

        // 2. Build the client config. The custom verifier pins
        //    fingerprint; the server name we pass into
        //    ClientConnection::new is irrelevant for our
        //    verifier but rustls still requires SOMETHING.
        let verifier = Arc::new(FingerprintVerifier::new(
            self.spec.fingerprint,
        ));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| {
                io::Error::other(format!(
                    "rustls protocol-version setup failed: {}",
                    e,
                ))
            })?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();

        // 3. Connect TCP + run TLS handshake + auth.hello.
        //
        // Reviewer round 1: bare `TcpStream::connect` doesn't honor
        // any of our `DIAL_TIMEOUT` plumbing — the read/write
        // timeouts only install AFTER the connect resolves, so an
        // unreachable peer (network black-holed, firewall drop,
        // wrong IP) would block on the OS-level connect timeout
        // (Linux: ~63s for tcp_syn_retries=6). Resolve the address
        // ourselves and use `TcpStream::connect_timeout` so the
        // connect step is bounded too.
        //
        // Reviewer round 2: `connect_timeout` only takes a single
        // `SocketAddr`, so naive `addrs.next()` regressed the
        // multi-A-record fallback that bare `connect` did
        // implicitly. Loop over the full address list, returning
        // the FIRST successful connect or the LAST error if all
        // attempts fail. Each attempt gets a fixed
        // `connect_budget = DIAL_TIMEOUT / 2`; multi-addr hosts
        // (rare in practice — usually 1-2 records) trade some
        // worst-case latency for the ability to fall through to
        // a healthy backup. The post-connect handshake/auth
        // deadline reserves the remaining `DIAL_TIMEOUT / 2`.
        let tcp = self.connect_with_fallback()?;
        tcp.set_read_timeout(Some(DIAL_TIMEOUT - CONNECT_BUDGET))?;
        tcp.set_write_timeout(Some(DIAL_TIMEOUT - CONNECT_BUDGET))?;

        // ServerName: with our custom verifier, the name doesn't
        // gate verification — but rustls API still requires a
        // valid ServerName. Use a fixed DNS literal ("daemon")
        // so the value is deterministic and doesn't depend on
        // whether `addr` parses as IP or DNS.
        let server_name = ServerName::try_from("daemon")
            .map_err(|e| {
                io::Error::other(format!(
                    "rustls ServerName build failed: {}",
                    e,
                ))
            })?;

        let conn = ClientConnection::new(Arc::new(config), server_name)
            .map_err(|e| {
                io::Error::other(format!(
                    "rustls ClientConnection setup failed: {}",
                    e,
                ))
            })?;
        let mut stream = StreamOwned::new(conn, tcp);

        // 4. Write auth.hello frame + read the daemon's response.
        let auth_req = Request {
            id: "auth-hello-1".to_string(),
            caller: Caller::operator("tls-dialer"),
            method: AUTH_HELLO_METHOD.to_string(),
            params: serde_json::json!({"token": token}),
        };
        // write_request runs the TLS handshake implicitly on the
        // first byte the rustls stream needs to produce.
        if let Err(e) = write_request(&mut stream, &auth_req) {
            return Err(classify_dial_error(e));
        }
        let resp = match read_response(&mut stream) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "daemon closed the TLS connection without \
                     responding to auth.hello",
                ));
            }
            Err(e) => return Err(classify_dial_error(e)),
        };

        if !resp.ok {
            let (code, message) = match resp.error {
                Some(eb) => (Some(eb.code), eb.message),
                None => (None, String::from("(no error body)")),
            };
            let kind = match code {
                Some(
                    cm_daemon::control::protocol::ErrorCode::Unauthorized,
                ) => io::ErrorKind::PermissionDenied,
                _ => io::ErrorKind::Other,
            };
            return Err(io::Error::new(
                kind,
                format!(
                    "daemon rejected auth.hello (code={:?}): {}",
                    code, message,
                ),
            ));
        }

        Ok(stream)
    }

    /// Resolve `self.spec.addr` and call
    /// [`connect_each_with_fallback`] across every returned
    /// `SocketAddr`. Round 2 reviewer fix: the previous
    /// `addrs.next()` form regressed bare `TcpStream::connect`'s
    /// implicit multi-A-record fallback. Returns the first
    /// successful connect or the last error (with all attempted
    /// addrs surfaced in the message so an operator can tell
    /// which leg of a multi-record DNS lookup actually failed).
    fn connect_with_fallback(&self) -> io::Result<TcpStream> {
        use std::net::ToSocketAddrs;
        let addrs: Vec<std::net::SocketAddr> = self
            .spec
            .addr
            .to_socket_addrs()
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "TLS dial: cannot resolve `{}`: {}",
                        self.spec.addr, e,
                    ),
                )
            })?
            .collect();
        connect_each_with_fallback(
            &self.spec.addr,
            &addrs,
            CONNECT_BUDGET,
        )
    }

    fn resolve_token(&self) -> io::Result<String> {
        let name = if self.spec.auth_env.is_empty() {
            DEFAULT_AUTH_ENV
        } else {
            self.spec.auth_env.as_str()
        };
        let raw = std::env::var(name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "TLS dial requires env var `{}` to carry the \
                     daemon token; set it before launching the \
                     TUI (production: in the operator's shell rc \
                     / launchd plist / systemd-user unit).",
                    name,
                ),
            )
        })?;
        if raw.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "env var `{}` is set but empty; expected the \
                     daemon's CM_DAEMON_TOKEN value.",
                    name,
                ),
            ));
        }
        Ok(raw)
    }
}

/// Iterate `addrs`, calling [`TcpStream::connect_timeout`] on
/// each with `per_addr_budget`. Returns the first success or
/// the last failure. Public so the multi-addr-fallback test in
/// this module's `tests` submodule can drive it without
/// synthesizing DNS — pass a hand-built `[blackhole, real]`
/// vec and assert the loop reaches the live entry.
///
/// Error message includes:
///   - the human-readable `display_addr` (the operator's typed
///     `host:port`, which may be a DNS name we just resolved),
///   - the COUNT of attempts (so a long-tail DNS lookup that
///     racked up several SYN drops is obvious),
///   - the last attempted addr + underlying `io::Error` (the
///     usually-actionable bit),
/// so the operator can tell "single black-hole IP" apart from
/// "DNS resolves to 4 unreachable IPs" without re-running with
/// trace logging.
pub fn connect_each_with_fallback(
    display_addr: &str,
    addrs: &[std::net::SocketAddr],
    per_addr_budget: std::time::Duration,
) -> io::Result<TcpStream> {
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!(
                "TLS dial: `{}` resolved to zero addresses",
                display_addr,
            ),
        ));
    }
    let mut last_err: Option<(std::net::SocketAddr, io::Error)> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(addr, per_addr_budget) {
            Ok(tcp) => return Ok(tcp),
            Err(e) => last_err = Some((*addr, e)),
        }
    }
    let (last_addr, last_inner) = last_err.expect(
        "addrs non-empty implies at least one attempt was made",
    );
    Err(io::Error::new(
        last_inner.kind(),
        format!(
            "TCP connect to {} failed after {} attempt(s); last \
             addr {} (timeout {:?}): {}",
            display_addr,
            addrs.len(),
            last_addr,
            per_addr_budget,
            last_inner,
        ),
    ))
}

/// Cosmetic remap of rustls' fingerprint-mismatch error into a
/// PermissionDenied so the operator-facing message reads as
/// auth-style ("cert fingerprint mismatch") rather than I/O-style
/// ("invalid data").
fn classify_dial_error(e: io::Error) -> io::Error {
    let msg = e.to_string();
    if msg.contains("CertFingerprintMismatch")
        || msg.contains("fingerprint mismatch")
    {
        io::Error::new(io::ErrorKind::PermissionDenied, msg)
    } else {
        e
    }
}

/// Custom `ServerCertVerifier` that ignores chain-of-trust and
/// SAN/CN checks and instead pins the leaf cert's SHA-256
/// fingerprint. Right shape for self-signed certs deployed by an
/// operator who controls both ends.
///
/// Signature verification (TLS 1.2 + 1.3) delegates to the
/// default rustls WebPki algorithms via the ring provider. The
/// pin happens only in `verify_server_cert`; if the cert
/// matches, downstream signature checks proceed normally so an
/// attacker who somehow served the pinned cert without holding
/// the private key still can't complete the handshake.
#[derive(Debug)]
struct FingerprintVerifier {
    expected: [u8; 32],
}

impl FingerprintVerifier {
    fn new(expected: [u8; 32]) -> Self {
        Self { expected }
    }
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        use sha2::Digest;
        let actual = sha2::Sha256::digest(end_entity.as_ref());
        if actual.as_slice() != self.expected.as_slice() {
            return Err(rustls::Error::General(format!(
                "TLS cert fingerprint mismatch: expected {}, got {}. \
                 Update `tls_fingerprint` in hosts.toml after \
                 verifying the daemon's cert with \
                 `openssl x509 -in <cert.pem> -fingerprint -sha256 -noout`.",
                hex32(&self.expected),
                hex32(&actual),
            )));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Format a 32-byte digest as a colon-separated lowercase hex
/// string — matches `openssl x509 -fingerprint -sha256` output
/// so the operator can paste the daemon's openssl line into
/// `tls_fingerprint` in hosts.toml.
fn hex32(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(95);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Compute the SHA-256 fingerprint of a DER-encoded certificate.
/// Public so tests can derive the expected fingerprint from
/// freshly-generated certs without re-implementing the digest.
pub fn cert_sha256(der: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    let mut out = [0u8; 32];
    out.copy_from_slice(&sha2::Sha256::digest(der));
    out
}

/// Reach into `SystemTime` for tests that want to assert no
/// real-time-clock-dependent state ends up in the verifier.
/// Unused at present but kept available for future regression
/// tests.
#[allow(dead_code)]
fn now_unix() -> UnixTime {
    UnixTime::since_unix_epoch(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Mutex;
    use std::time::Duration;

    use rustls::{ServerConfig, ServerConnection};

    // ------------------------------------------------------------------
    // Helpers — cert generation + in-process rustls test server.
    //
    // The four T_g3h_* tests drive a real TLS handshake against an
    // in-process rustls server thread. Certs are generated via the
    // local `openssl` binary in a tempdir per test. This avoids
    // baking static crypto material into the source while keeping
    // each test hermetic (no shared cert state across tests).
    // ------------------------------------------------------------------

    /// Generate a fresh self-signed P-256 cert + PKCS#8 key with
    /// CN=localhost. Returns the (cert_pem_path, key_pem_path,
    /// fingerprint_bytes) tuple. Requires `openssl` on `$PATH`
    /// (production-Linux dev box has it; the test cfg-gates the
    /// dial path to Linux/Unix and we assume openssl exists).
    fn generate_cert_pair(
        dir: &Path,
    ) -> (PathBuf, PathBuf, [u8; 32]) {
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        // Single-command form generates BOTH cert and a PKCS#8 key
        // file. The `-nodes` (NO DES) flag asks for an unencrypted
        // key — the daemon side never types a passphrase.
        // ec_paramgen_curve:P-256 chooses an ECDSA P-256 keypair,
        // which is supported by rustls's ring provider and is the
        // smallest cert size of the algorithms we'd reach for.
        let status = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-nodes",
                "-keyout",
                key_path.to_str().unwrap(),
                "-out",
                cert_path.to_str().unwrap(),
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
            ])
            .output()
            .expect("invoke openssl");
        assert!(
            status.status.success(),
            "openssl cert generation failed: {}",
            String::from_utf8_lossy(&status.stderr),
        );
        // Compute SHA-256 of the DER form so the fingerprint
        // matches what rustls hashes on the client side. Easier
        // to round-trip through `openssl x509 -outform DER`.
        let der_out = Command::new("openssl")
            .args([
                "x509",
                "-in",
                cert_path.to_str().unwrap(),
                "-outform",
                "DER",
            ])
            .output()
            .expect("openssl der");
        assert!(der_out.status.success());
        let fp = cert_sha256(&der_out.stdout);
        (cert_path, key_path, fp)
    }

    /// In-process rustls test server. Binds 127.0.0.1:0, returns
    /// the (acceptor, addr, captured_messages) triple. The
    /// `accept_one` method spawns a handler thread for ONE
    /// connection — sufficient for each test case.
    struct TestServer {
        listener: TcpListener,
        config: Arc<ServerConfig>,
        expected_token: String,
        captured: Arc<Mutex<Vec<TestServerOutcome>>>,
    }

    #[derive(Debug, Clone)]
    enum TestServerOutcome {
        AuthOk { req_id: String },
        AuthRejected { reason: String },
        HandshakeFailed { reason: String },
    }

    impl TestServer {
        fn bind(
            cert_path: &Path,
            key_path: &Path,
            expected_token: String,
        ) -> Self {
            let certs =
                cm_daemon::control::tls::load_certs(cert_path)
                    .expect("load certs");
            let key =
                cm_daemon::control::tls::load_private_key(key_path)
                    .expect("load key");
            let provider = Arc::new(
                rustls::crypto::ring::default_provider(),
            );
            let config = ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("protocol versions")
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .expect("server cert/key pair");
            let listener =
                TcpListener::bind("127.0.0.1:0").expect("bind");
            TestServer {
                listener,
                config: Arc::new(config),
                expected_token,
                captured: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn local_addr(&self) -> std::net::SocketAddr {
            self.listener.local_addr().unwrap()
        }

        fn captured(&self) -> Vec<TestServerOutcome> {
            self.captured
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }

        /// Spawn a handler thread that accepts ONE connection,
        /// completes the TLS handshake, validates auth.hello,
        /// writes either an OK ack or an Unauthorized response,
        /// then closes. Returns the JoinHandle so the test can
        /// wait for the connection to complete.
        fn accept_one(
            &self,
        ) -> std::thread::JoinHandle<()> {
            let listener =
                self.listener.try_clone().expect("clone listener");
            let config = Arc::clone(&self.config);
            let expected_token = self.expected_token.clone();
            let captured = Arc::clone(&self.captured);
            std::thread::spawn(move || {
                let (tcp, _peer) =
                    listener.accept().expect("accept one");
                let _ = tcp.set_read_timeout(Some(
                    Duration::from_secs(5),
                ));
                let _ = tcp.set_write_timeout(Some(
                    Duration::from_secs(5),
                ));
                let conn = match ServerConnection::new(config) {
                    Ok(c) => c,
                    Err(e) => {
                        captured
                            .lock()
                            .unwrap()
                            .push(TestServerOutcome::HandshakeFailed {
                                reason: e.to_string(),
                            });
                        return;
                    }
                };
                let mut stream =
                    rustls::StreamOwned::new(conn, tcp);
                let req = match cm_daemon::control::wire::read_request(
                    &mut stream,
                ) {
                    Ok(Some(r)) => r,
                    Ok(None) => return,
                    Err(e) => {
                        captured
                            .lock()
                            .unwrap()
                            .push(TestServerOutcome::HandshakeFailed {
                                reason: e.to_string(),
                            });
                        return;
                    }
                };
                let outcome =
                    cm_daemon::control::tls::validate_auth_hello(
                        &req,
                        &expected_token,
                    );
                match outcome {
                    Ok(()) => {
                        let ack =
                            cm_daemon::control::protocol::Response::ok(
                                req.id.clone(),
                                serde_json::json!({"ok": true}),
                            );
                        let _ = cm_daemon::control::wire::write_response(
                            &mut stream,
                            &ack,
                        );
                        captured
                            .lock()
                            .unwrap()
                            .push(TestServerOutcome::AuthOk {
                                req_id: req.id,
                            });
                    }
                    Err(resp) => {
                        let reason = resp
                            .error
                            .as_ref()
                            .map(|e| e.message.clone())
                            .unwrap_or_default();
                        let _ = cm_daemon::control::wire::write_response(
                            &mut stream,
                            &resp,
                        );
                        captured
                            .lock()
                            .unwrap()
                            .push(TestServerOutcome::AuthRejected {
                                reason,
                            });
                    }
                }
            })
        }
    }

    fn guard_auth_env(name: &str, value: Option<&str>) -> EnvGuard {
        let prev = std::env::var_os(name);
        match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
        EnvGuard {
            name: name.to_string(),
            prev,
        }
    }

    struct EnvGuard {
        name: String,
        prev: Option<std::ffi::OsString>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(&self.name, v),
                None => std::env::remove_var(&self.name),
            }
        }
    }

    /// T_g3h_tls_handshake_ok — legitimate handshake works
    /// end-to-end against an in-process rustls test server.
    /// Pins:
    ///   - TLS handshake completes (no cert error)
    ///   - auth.hello frame is sent + accepted
    ///   - server captures the request id (proof it actually
    ///     read the frame, not just got TCP bytes)
    #[test]
    fn t_g3h_tls_handshake_ok() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cert, key, fp) = generate_cert_pair(tmp.path());
        let token = "ok-token-12h";
        let server =
            TestServer::bind(&cert, &key, token.to_string());
        let addr = server.local_addr();
        let handle = server.accept_one();

        let _env =
            guard_auth_env("CM_DAEMON_TOKEN_T_OK", Some(token));
        let dialer = TlsDialer::new(TlsDialerSpec {
            addr: addr.to_string(),
            fingerprint: fp,
            auth_env: "CM_DAEMON_TOKEN_T_OK".to_string(),
        });
        let stream = dialer
            .dial()
            .expect("legitimate TLS handshake must succeed");

        handle.join().expect("server handler joined");
        let outcomes = server.captured();
        assert_eq!(
            outcomes.len(),
            1,
            "exactly one outcome captured; got {:?}",
            outcomes,
        );
        match &outcomes[0] {
            TestServerOutcome::AuthOk { req_id } => {
                assert_eq!(req_id, "auth-hello-1");
            }
            other => panic!(
                "expected AuthOk; got {:?}",
                other,
            ),
        }
        drop(stream);
    }

    /// T_g3h_fingerprint_mismatch_clear_error — when the
    /// configured fingerprint doesn't match the server's cert,
    /// the dial surfaces a CLEAR error (not a generic "TCP
    /// connection reset"). The message must:
    ///   - mention "fingerprint"
    ///   - include both expected and actual digests so the
    ///     operator can paste the actual one into hosts.toml.
    #[test]
    fn t_g3h_fingerprint_mismatch_clear_error() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cert, key, _real_fp) = generate_cert_pair(tmp.path());
        let token = "ok-token-12h";
        let server =
            TestServer::bind(&cert, &key, token.to_string());
        let addr = server.local_addr();
        let handle = server.accept_one();

        // Wrong fingerprint: all zeros. Distinct from any
        // legitimately-generated digest.
        let wrong_fp = [0u8; 32];
        let _env = guard_auth_env(
            "CM_DAEMON_TOKEN_T_FPMISMATCH",
            Some(token),
        );
        let dialer = TlsDialer::new(TlsDialerSpec {
            addr: addr.to_string(),
            fingerprint: wrong_fp,
            auth_env: "CM_DAEMON_TOKEN_T_FPMISMATCH".to_string(),
        });
        let err = dialer
            .dial()
            .expect_err("fingerprint mismatch must fail dial");
        let msg = err.to_string();
        assert!(
            msg.contains("fingerprint")
                || msg.contains("Fingerprint"),
            "error must mention 'fingerprint'; got: {}",
            msg,
        );
        assert!(
            msg.contains("expected") && msg.contains("got"),
            "error must surface both expected + actual digests \
             so the operator can paste the right value into \
             hosts.toml; got: {}",
            msg,
        );

        // Best-effort: let the server handler finish + reap.
        let _ = handle.join();
    }

    /// T_g3h_auth_hello_required_first — the daemon's
    /// `validate_auth_hello` rejects a non-`auth.hello` first
    /// frame. This is a daemon-side invariant; the test drives
    /// the validator directly (the rustls plumbing around it
    /// is exercised by `t_g3h_tls_handshake_ok`).
    #[test]
    fn t_g3h_auth_hello_required_first() {
        let req = Request {
            id: "r-not-hello".to_string(),
            caller: Caller::operator("tls-dialer"),
            method: "ping".to_string(),
            params: serde_json::json!({}),
        };
        let resp = cm_daemon::control::tls::validate_auth_hello(
            &req, "any-token",
        )
        .expect_err("non-auth.hello first frame must be rejected");
        assert!(!resp.ok);
        let err = resp.error.expect("error body");
        assert_eq!(
            err.code,
            cm_daemon::control::protocol::ErrorCode::Unauthorized,
        );
        assert!(
            err.message.contains("auth.hello"),
            "error must name the required method; got: {}",
            err.message,
        );
        assert!(
            err.message.contains("ping"),
            "error must name the method actually sent so the \
             operator can disambiguate from a wrong-token \
             failure; got: {}",
            err.message,
        );
    }

    /// T_g3h_wrong_token_unauthorized — when the dialer sends
    /// auth.hello with a token that doesn't match the daemon's
    /// configured value, the daemon writes back Unauthorized
    /// and closes. The dialer surfaces this as a
    /// `PermissionDenied`-classified io::Error so the operator
    /// sees an auth failure, not a generic transport error.
    #[test]
    fn t_g3h_wrong_token_unauthorized() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cert, key, fp) = generate_cert_pair(tmp.path());
        let server = TestServer::bind(
            &cert,
            &key,
            "the-real-token".to_string(),
        );
        let addr = server.local_addr();
        let handle = server.accept_one();

        let _env = guard_auth_env(
            "CM_DAEMON_TOKEN_T_WRONG",
            Some("guessed-wrong"),
        );
        let dialer = TlsDialer::new(TlsDialerSpec {
            addr: addr.to_string(),
            fingerprint: fp,
            auth_env: "CM_DAEMON_TOKEN_T_WRONG".to_string(),
        });
        let err = dialer
            .dial()
            .expect_err("wrong token must fail dial");
        assert_eq!(
            err.kind(),
            io::ErrorKind::PermissionDenied,
            "wrong-token failure must classify as \
             PermissionDenied so operator tooling can tell it \
             apart from transport errors; got: {:?}",
            err.kind(),
        );
        let msg = err.to_string();
        assert!(
            msg.contains("Unauthorized")
                || msg.contains("unauthorized")
                || msg.contains("token"),
            "error must surface the auth-failure cause; got: {}",
            msg,
        );

        handle.join().expect("server handler joined");
        let outcomes = server.captured();
        match outcomes.first() {
            Some(TestServerOutcome::AuthRejected { reason }) => {
                assert!(
                    reason.contains("token")
                        || reason.contains("match"),
                    "server-side rejection reason must mention \
                     the token; got: {}",
                    reason,
                );
            }
            other => panic!(
                "expected AuthRejected outcome; got {:?}",
                other,
            ),
        }
    }

    /// Token-missing case: env var unset means the dialer
    /// fails BEFORE doing any I/O, with a NotFound-classified
    /// error naming the missing env var. Operator-actionable
    /// failure mode.
    #[test]
    fn dial_fails_fast_when_env_var_missing() {
        let _env =
            guard_auth_env("CM_DAEMON_TOKEN_T_MISSING", None);
        let dialer = TlsDialer::new(TlsDialerSpec {
            addr: "127.0.0.1:1".to_string(),
            fingerprint: [0u8; 32],
            auth_env: "CM_DAEMON_TOKEN_T_MISSING".to_string(),
        });
        let err = dialer.dial().expect_err("missing env must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let msg = err.to_string();
        assert!(
            msg.contains("CM_DAEMON_TOKEN_T_MISSING"),
            "error must name the missing env var; got: {}",
            msg,
        );
    }

    /// Reviewer round 2 (12h): `connect_each_with_fallback`
    /// iterates the address list and returns the first success.
    /// Drive it directly with `[blackhole, live_listener]` and
    /// confirm the loop falls through to the second address.
    /// Avoids synthesizing multi-A-record DNS — the loop logic
    /// is the bit under test, not the resolver step.
    #[test]
    fn connect_each_with_fallback_skips_dead_addr() {
        // Live listener at an ephemeral 127.0.0.1 port.
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = listener.local_addr().unwrap();
        // Unreachable: TEST-NET-1 (RFC 5737) — SYNs go void.
        let dead: std::net::SocketAddr =
            "192.0.2.2:8443".parse().unwrap();

        // Order: dead first, live second. A successful connect
        // proves the loop fell through.
        let start = std::time::Instant::now();
        let tcp = connect_each_with_fallback(
            "test:8443",
            &[dead, live],
            std::time::Duration::from_secs(1),
        )
        .expect("loop must fall through to the live addr");
        let elapsed = start.elapsed();
        // Fell through after one timed-out attempt (~1s); the
        // live connect should resolve in microseconds. Cap at
        // 2s to absorb scheduler slack.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "fall-through took {:?}; the dead-addr SYN drop \
             should bound at ~1s and the live connect is \
             instant",
            elapsed,
        );
        // Sanity: the TCP we got connects to the live listener.
        assert_eq!(tcp.peer_addr().unwrap(), live);
        drop(tcp);
        drop(listener);
    }

    /// Reviewer round 2 (12h): when every resolved addr fails,
    /// the surfaced error names the COUNT of attempts and the
    /// last attempted addr — both bits an operator needs to
    /// disambiguate "single unreachable IP" from "DNS resolves
    /// to a list of dead IPs."
    #[test]
    fn connect_each_with_fallback_returns_last_error_when_all_fail() {
        let dead1: std::net::SocketAddr =
            "192.0.2.3:8443".parse().unwrap();
        let dead2: std::net::SocketAddr =
            "192.0.2.4:8443".parse().unwrap();
        let err = connect_each_with_fallback(
            "all-dead:8443",
            &[dead1, dead2],
            std::time::Duration::from_millis(500),
        )
        .expect_err("all-dead must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("2 attempt"),
            "error must name the attempt count; got: {}",
            msg,
        );
        assert!(
            msg.contains("192.0.2.4"),
            "error must name the LAST attempted addr so the \
             operator sees what they're effectively retrying; \
             got: {}",
            msg,
        );
        assert!(
            msg.contains("all-dead:8443"),
            "error must echo the operator's typed `host:port`; \
             got: {}",
            msg,
        );
    }

    /// Reviewer round 1 (12h): a TCP connect to an unreachable /
    /// black-holed peer MUST fail within the dialer's
    /// `DIAL_TIMEOUT` budget — NOT the OS-level connect timeout
    /// (Linux: ~63s for tcp_syn_retries=6). Pre-fix the dialer
    /// called `TcpStream::connect` directly and the read/write
    /// timeouts only installed AFTER it returned, so a SYN-drop
    /// firewall would hang the dial for a minute despite
    /// DIAL_TIMEOUT being 10s.
    ///
    /// Test uses 192.0.2.1 (TEST-NET-1 per RFC 5737) — guaranteed
    /// non-routable so the SYNs go into the void. We assert the
    /// dial errors well under DIAL_TIMEOUT * 1.5 (allowing for
    /// some scheduling slack) and that the error message names
    /// the bounded connect-timeout step so the operator can tell
    /// "no route" apart from "wrong cert."
    #[test]
    fn connect_to_blackhole_is_bounded_by_dial_timeout() {
        // Burn a token env var so the dial proceeds past
        // `resolve_token` and actually reaches the TCP step.
        let _env = guard_auth_env(
            "CM_DAEMON_TOKEN_T_BLACKHOLE",
            Some("any"),
        );
        let dialer = TlsDialer::new(TlsDialerSpec {
            addr: "192.0.2.1:8443".to_string(),
            fingerprint: [0u8; 32],
            auth_env: "CM_DAEMON_TOKEN_T_BLACKHOLE".to_string(),
        });
        let start = std::time::Instant::now();
        let err =
            dialer.dial().expect_err("black-holed addr must fail");
        let elapsed = start.elapsed();
        // Connect budget is DIAL_TIMEOUT/2 = 5s. Allow generous
        // headroom (2× over budget) to absorb scheduler / CI
        // jitter, but cap WELL under the OS-level ~63s ceiling
        // so a regression that re-introduces bare
        // `TcpStream::connect` trips this test.
        assert!(
            elapsed < DIAL_TIMEOUT,
            "blackhole connect MUST be bounded by DIAL_TIMEOUT \
             ({:?}); took {:?} — looks like the connect-timeout \
             fix regressed and we're hitting the OS connect \
             ceiling instead",
            DIAL_TIMEOUT,
            elapsed,
        );
        let msg = err.to_string();
        assert!(
            msg.contains("TCP connect"),
            "error must name the connect step; got: {}",
            msg,
        );
        assert!(
            msg.contains("192.0.2.1:8443"),
            "error must echo the unreachable addr so the \
             operator sees what they typed wrong; got: {}",
            msg,
        );
    }
}
