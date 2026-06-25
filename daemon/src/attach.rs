//! `AttachTicket` allocator. Single-use, short-TTL tokens that bind a
//! pending PTY-attach connection to the session uid and caller identity
//! that requested it.
//!
//! ## Flow (slice 5 of doc/persistent-host-daemon.md)
//!
//! 1. The TUI sends `session.attach { uid }` on the control connection.
//! 2. The daemon's session-attach method handler validates that the
//!    caller has scope over `uid`, then calls
//!    [`TicketAllocator::allocate`] to mint a fresh ticket bound to
//!    `(uid, caller)`. The ticket id is a v4 UUID; it expires 30
//!    seconds after issue.
//! 3. The handler returns `{ attach_ticket, attach_addr }` to the TUI.
//! 4. The TUI dials a fresh connection to `attach_addr`, completes any
//!    transport-level auth (TLS + `auth.hello` on TCP; nothing on Unix),
//!    and sends `attach.open { ticket }` as the first JSON-RPC frame.
//! 5. The daemon's `attach.open` handler calls [`TicketAllocator::consume`]
//!    with the presented ticket and the connection's caller identity. On
//!    success it binds the connection to that session's PTY-byte fanout
//!    and the frame stream begins.
//!
//! ## Why this exists as its own primitive
//!
//! The listening address is shared across all attaches and the control
//! connection — a fresh dial alone doesn't tell the daemon which session
//! the new connection wants. The ticket is the routing token. Single-use
//! and identity-bound semantics close two specific attacks:
//!
//!   - Replay: a leaked ticket can't be reused by a different connection
//!     because [`consume`](TicketAllocator::consume) removes it on
//!     success.
//!   - Cross-session redirection: a ticket issued for session A presented
//!     by a caller whose identity differs from the issuer's is rejected.
//!     This matters when multiple operators share a daemon (future
//!     work — today there's a single operator, but the design is
//!     future-proofed).
//!
//! Expired tickets are reclaimed lazily on consume, plus by an explicit
//! [`TicketAllocator::gc`] sweep the accept loop can call when it likes.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::control::protocol::Caller;

/// How long a freshly-allocated ticket stays usable. Long enough to
/// cover the TUI dialing a fresh connection and shipping `attach.open`;
/// short enough that a leaked-ticket window is bounded.
pub const TICKET_TTL: Duration = Duration::from_secs(30);

/// A single-use, short-TTL token that authorizes a fresh connection to
/// attach to a specific session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachTicket {
    /// Opaque, unique id. Returned to the TUI as the `attach_ticket`
    /// field of the `session.attach` response. The TUI echoes it back
    /// on its `attach.open` frame.
    pub id: String,
    /// The session this ticket authorizes attachment to. The daemon's
    /// `attach.open` handler routes the connection to this session's
    /// PTY fanout once the ticket consumes successfully.
    pub session_uid: String,
}

/// Reasons [`TicketAllocator::consume`] can fail. Each maps to a
/// well-defined behavior on the daemon's accept loop: unknown / expired
/// → close with `NotFound` (don't leak which case it was); identity
/// mismatch → close with `Unauthorized`.
#[derive(Debug, PartialEq, Eq)]
pub enum ConsumeError {
    /// No ticket with this id exists. Either it was never allocated,
    /// or it was already consumed (single-use), or it expired and was
    /// reclaimed by an earlier gc / consume.
    NotFound,
    /// The ticket exists and is unexpired, but the caller presenting
    /// it is not the caller that asked for it. Treat as a hostile
    /// attempt to redirect another operator's pending attach.
    IdentityMismatch,
    /// The ticket exists in the table but is past its TTL. Returned
    /// when we choose to surface the "was once valid" state separately
    /// from "never existed" — usually you'll fold this into `NotFound`
    /// before responding to the client to avoid leaking timing info.
    Expired,
}

struct Entry {
    session_uid: String,
    /// The caller that asked for the ticket. Compared by value against
    /// the caller presenting `attach.open` — the comparison is on the
    /// payload (`session_uid` or `token_id`), not on object identity.
    issued_to: Caller,
    expires_at: Instant,
}

/// Thread-safe pending-ticket store.
///
/// One allocator lives in the daemon for the process lifetime. The
/// inner `Mutex<HashMap<...>>` is fine because ticket operations are
/// O(1) and very rare relative to PTY-byte traffic (one allocate per
/// `session.attach`, one consume per `attach.open`). A reader/writer
/// lock would be premature.
pub struct TicketAllocator {
    inner: Mutex<HashMap<String, Entry>>,
}

impl Default for TicketAllocator {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl TicketAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a fresh ticket bound to `session_uid` and `issued_to`.
    /// Returns the public-facing `AttachTicket` (the daemon hands this
    /// back through the `session.attach` response).
    pub fn allocate(&self, session_uid: impl Into<String>, issued_to: Caller) -> AttachTicket {
        self.allocate_with_clock(session_uid, issued_to, Instant::now())
    }

    /// Like [`allocate`](Self::allocate) but lets the caller pin the
    /// "now" instant. Used by tests to drive expiry behavior without
    /// sleeping.
    pub fn allocate_with_clock(
        &self,
        session_uid: impl Into<String>,
        issued_to: Caller,
        now: Instant,
    ) -> AttachTicket {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let session_uid = session_uid.into();
        let entry = Entry {
            session_uid: session_uid.clone(),
            issued_to,
            expires_at: now + TICKET_TTL,
        };
        let mut store = self
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        store.insert(id.clone(), entry);
        AttachTicket {
            id,
            session_uid,
        }
    }

    /// Consume a ticket. On success returns the bound session uid;
    /// the entry is removed from the store (single-use). On failure
    /// the entry is left alone unless it's expired, in which case it
    /// is removed and the caller gets [`ConsumeError::Expired`].
    ///
    /// `presented_by` is the caller identity attached to the connection
    /// sending the `attach.open` frame. It must match the identity the
    /// ticket was issued to.
    pub fn consume(
        &self,
        ticket_id: &str,
        presented_by: &Caller,
    ) -> Result<String, ConsumeError> {
        self.consume_with_clock(ticket_id, presented_by, Instant::now())
    }

    /// Like [`consume`](Self::consume) but lets the caller pin "now".
    pub fn consume_with_clock(
        &self,
        ticket_id: &str,
        presented_by: &Caller,
        now: Instant,
    ) -> Result<String, ConsumeError> {
        let mut store = self
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let entry = store.get(ticket_id).ok_or(ConsumeError::NotFound)?;
        if entry.expires_at <= now {
            // Reclaim the expired entry while we're here so a follow-up
            // gc doesn't have to.
            store.remove(ticket_id);
            return Err(ConsumeError::Expired);
        }
        if !callers_match(&entry.issued_to, presented_by) {
            // Don't remove on identity mismatch — the legitimate caller
            // may still come along and consume.
            return Err(ConsumeError::IdentityMismatch);
        }
        // Single-use: remove on successful consume.
        let entry = store.remove(ticket_id).expect("just-checked entry");
        Ok(entry.session_uid)
    }

    /// Sweep expired entries. Cheap because the table is small (rarely
    /// more than a handful of in-flight tickets). Returns the number
    /// of entries reclaimed, primarily for tests / metrics.
    pub fn gc(&self) -> usize {
        self.gc_with_clock(Instant::now())
    }

    /// Like [`gc`](Self::gc) but with a pinned clock.
    pub fn gc_with_clock(&self, now: Instant) -> usize {
        let mut store = self
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let before = store.len();
        store.retain(|_, e| e.expires_at > now);
        before - store.len()
    }

    /// Test helper: number of pending tickets. Not part of the public
    /// allocator contract; surface only for assertions.
    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }
}

/// Compare callers by their identifying payload, not by enum
/// discriminator alone — two `Caller::Session` values with different
/// `session_uid`s must not match.
fn callers_match(a: &Caller, b: &Caller) -> bool {
    match (a, b) {
        (Caller::Session(x), Caller::Session(y)) => x.session_uid == y.session_uid,
        (Caller::Operator(x), Caller::Operator(y)) => x.token_id == y.token_id,
        _ => false,
    }
}

// ===================================================================
// Method-handler pure functions
// ===================================================================
//
// `session.attach` and `attach.open` as plain functions over a
// `&TicketAllocator` + `&Caller` + typed request — no implicit
// dispatcher, no `App`, no globals. They land in the daemon-side
// methods dispatcher (task #17, blocked on slice 10's App-state
// migration) as one-line wrappers, but landing them now freezes the
// semantics so the rewire is plug-in not redesign.

/// Parameters for the `session.attach` JSON-RPC method.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionAttachParams {
    /// Session UID the caller wants to attach to. The daemon
    /// validates that the caller has scope over this UID before
    /// allocating a ticket.
    pub uid: String,
}

/// Response shape for `session.attach`. The TUI dials a fresh
/// connection to `attach_addr` and sends `attach.open { ticket }`
/// as the first frame on it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionAttachResponse {
    pub attach_ticket: String,
    pub attach_addr: String,
}

/// Parameters for the `attach.open` JSON-RPC method.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttachOpenParams {
    pub ticket: String,
    /// Client terminal size at attach time. `Option` (with
    /// `#[serde(default)]`) for backward-compat: older TUIs send
    /// only `ticket` and rely solely on a best-effort post-attach
    /// `Resize` data frame. That frame silently drops when the
    /// attach socket is dead/replaced at the moment it fires
    /// (`Broken pipe`), leaving the PTY stuck at its spawn size —
    /// the "session renders tiny" bug. When these are present the
    /// daemon resizes the PTY server-side at stream bind
    /// (`dispatch_attach_open`), which runs in-process over the
    /// freshly-consumed ticket and cannot hit a dead socket, so
    /// the size is correct regardless of the data socket's health.
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

/// Response shape for `attach.open`. Returns the bound session uid
/// so the dispatch caller (in the daemon's accept loop) can route
/// the connection to the matching `DaemonSession`'s PTY-byte
/// fanout.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttachOpenResponse {
    pub session_uid: String,
}

/// Method-handler error. Maps onto `ErrorCode` at the dispatch
/// layer (see `protocol.rs`): the dispatcher does
/// `Response::err(req.id, error.code(), error.message())`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachError {
    /// Caller doesn't have authority for the requested action. Used
    /// for: Session callers reaching session.attach (in Phase 1
    /// only Operator callers can issue tickets); presenting a
    /// ticket that was issued to a different caller (defends
    /// against a leaked ticket being replayed by a different
    /// process).
    Unauthorized(String),
    /// Ticket is unknown, already consumed, or expired. The three
    /// cases collapse into one error code so a probe-style caller
    /// can't distinguish "never issued" from "already consumed"
    /// from "expired" by timing or response.
    NotFound(String),
}

impl AttachError {
    /// Maps onto `protocol::ErrorCode` for the dispatch layer's
    /// final `Response::err`. Centralised so the mapping doesn't
    /// drift when methods evolve.
    pub fn code(&self) -> crate::control::protocol::ErrorCode {
        match self {
            AttachError::Unauthorized(_) => {
                crate::control::protocol::ErrorCode::Unauthorized
            }
            AttachError::NotFound(_) => {
                crate::control::protocol::ErrorCode::NotFound
            }
        }
    }

    pub fn message(&self) -> &str {
        match self {
            AttachError::Unauthorized(m) | AttachError::NotFound(m) => m,
        }
    }
}

/// `session.attach` method handler. Allocates an `AttachTicket`
/// bound to `caller`'s identity and the requested session uid, then
/// returns the ticket id and the address the TUI should dial for
/// the dedicated attach connection.
///
/// **Scope check (Phase 1):** only `Operator` callers may issue
/// tickets. The doc spec calls for scope checking against the
/// caller's session-tree for Session callers, but no Phase-1 use
/// case needs sessions attaching to other sessions; the strict
/// Operator-only rule is what matches the integration we're
/// building toward. When the cross-session use case lands later,
/// the check extends to "Operator OR Session-with-descendant-of-uid".
///
/// **`attach_addr` parameter:** passed in by the dispatch layer
/// from daemon config (`~/.cm/daemon.sock` for Unix; `host:port`
/// for the TCP transport in Phase 3). Threading it as a parameter
/// rather than reading from globals keeps the function testable
/// and explicit.
///
/// TODO(slice-17): wire this into the relocated `control/methods.rs`
/// dispatch table. The wrapper is one line: `dispatch.attach.session_attach(&alloc, &req.caller, &params, &cfg.attach_addr)`.
pub fn session_attach(
    alloc: &TicketAllocator,
    caller: &Caller,
    params: &SessionAttachParams,
    attach_addr: &str,
) -> Result<SessionAttachResponse, AttachError> {
    match caller {
        Caller::Operator(_) => {}
        Caller::Session(_) => {
            return Err(AttachError::Unauthorized(
                "session.attach requires an Operator caller".into(),
            ));
        }
    }
    let ticket = alloc.allocate(&params.uid, caller.clone());
    Ok(SessionAttachResponse {
        attach_ticket: ticket.id,
        attach_addr: attach_addr.to_string(),
    })
}

/// `attach.open` method handler. Consumes a ticket previously
/// issued by `session_attach` and returns the bound session uid on
/// success. The dispatch caller then transitions the connection
/// from RPC mode into the PTY-byte stream attached to that
/// session's fanout (see [`crate::session::PtyByteFanout`]).
///
/// **Caller identity check:** delegates to
/// [`TicketAllocator::consume`], which verifies the presenting
/// caller's identity matches the issuer's. A leaked ticket cannot
/// be replayed by a different process — the consume call fails
/// with `IdentityMismatch`, which maps to `Unauthorized` here.
///
/// **NotFound conflation:** unknown / expired / already-consumed
/// all return `NotFound("ticket not valid")` so a probe-style
/// caller can't distinguish them.
///
/// TODO(slice-17): wire into `control/methods.rs`. Wrapper is one
/// line; the dedicated-connection state transition lives at the
/// dispatch layer.
pub fn attach_open(
    alloc: &TicketAllocator,
    caller: &Caller,
    params: &AttachOpenParams,
) -> Result<AttachOpenResponse, AttachError> {
    match alloc.consume(&params.ticket, caller) {
        Ok(uid) => Ok(AttachOpenResponse { session_uid: uid }),
        Err(ConsumeError::IdentityMismatch) => Err(AttachError::Unauthorized(
            "ticket was issued to a different caller".into(),
        )),
        Err(ConsumeError::NotFound) | Err(ConsumeError::Expired) => {
            Err(AttachError::NotFound("ticket not valid".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn operator(t: &str) -> Caller {
        Caller::operator(t)
    }

    fn session(uid: &str) -> Caller {
        Caller::session(uid)
    }

    #[test]
    fn allocate_produces_unique_ids() {
        let alloc = TicketAllocator::new();
        let a = alloc.allocate("ts-1", operator("op-default"));
        let b = alloc.allocate("ts-1", operator("op-default"));
        assert_ne!(a.id, b.id, "ticket ids must be unique");
        assert_eq!(a.session_uid, "ts-1");
        assert_eq!(b.session_uid, "ts-1");
    }

    #[test]
    fn consume_round_trip_returns_session_uid_and_removes() {
        let alloc = TicketAllocator::new();
        let ticket = alloc.allocate("ts-7", operator("op-default"));
        assert_eq!(alloc.pending_count(), 1);

        let uid = alloc
            .consume(&ticket.id, &operator("op-default"))
            .expect("consume");
        assert_eq!(uid, "ts-7");
        assert_eq!(alloc.pending_count(), 0, "consume must remove entry");
    }

    #[test]
    fn second_consume_fails_not_found() {
        let alloc = TicketAllocator::new();
        let ticket = alloc.allocate("ts-7", operator("op-default"));

        alloc.consume(&ticket.id, &operator("op-default")).unwrap();
        let err = alloc
            .consume(&ticket.id, &operator("op-default"))
            .unwrap_err();
        assert_eq!(err, ConsumeError::NotFound);
    }

    #[test]
    fn unknown_ticket_id_fails_not_found() {
        let alloc = TicketAllocator::new();
        let err = alloc
            .consume("never-issued", &operator("op-default"))
            .unwrap_err();
        assert_eq!(err, ConsumeError::NotFound);
    }

    #[test]
    fn identity_mismatch_between_operator_callers_fails() {
        let alloc = TicketAllocator::new();
        let ticket = alloc.allocate("ts-7", operator("op-alice"));

        // Same caller variant, different token id.
        let err = alloc
            .consume(&ticket.id, &operator("op-bob"))
            .unwrap_err();
        assert_eq!(err, ConsumeError::IdentityMismatch);

        // Ticket must still be consumable by the legitimate caller —
        // identity mismatch is non-destructive.
        let uid = alloc.consume(&ticket.id, &operator("op-alice")).unwrap();
        assert_eq!(uid, "ts-7");
    }

    #[test]
    fn identity_mismatch_across_caller_kinds_fails() {
        // A ticket issued to an Operator is not consumable by a
        // Session caller even if some payload happened to be similar.
        let alloc = TicketAllocator::new();
        let ticket = alloc.allocate("ts-7", operator("matching"));

        let err = alloc
            .consume(&ticket.id, &session("matching"))
            .unwrap_err();
        assert_eq!(err, ConsumeError::IdentityMismatch);
    }

    #[test]
    fn expired_ticket_fails_expired_and_is_reclaimed() {
        let alloc = TicketAllocator::new();
        // Pin a "now" sufficiently into the past that the TTL has
        // elapsed by the time we consume.
        let issued_at = Instant::now() - Duration::from_secs(60);
        let ticket = alloc.allocate_with_clock("ts-9", operator("op-default"), issued_at);
        assert_eq!(alloc.pending_count(), 1);

        let err = alloc
            .consume(&ticket.id, &operator("op-default"))
            .unwrap_err();
        assert_eq!(err, ConsumeError::Expired);

        // Reclaimed eagerly so subsequent consumes see NotFound, not
        // Expired — the table doesn't grow without bound.
        assert_eq!(alloc.pending_count(), 0);
        let err2 = alloc
            .consume(&ticket.id, &operator("op-default"))
            .unwrap_err();
        assert_eq!(err2, ConsumeError::NotFound);
    }

    #[test]
    fn gc_removes_expired_entries_only() {
        let alloc = TicketAllocator::new();
        let now = Instant::now();
        let stale = now - Duration::from_secs(60);

        let _ = alloc.allocate_with_clock("ts-1", operator("op"), stale);
        let _ = alloc.allocate_with_clock("ts-2", operator("op"), stale);
        let fresh = alloc.allocate_with_clock("ts-3", operator("op"), now);

        assert_eq!(alloc.pending_count(), 3);
        let reclaimed = alloc.gc_with_clock(now);
        assert_eq!(reclaimed, 2);
        assert_eq!(alloc.pending_count(), 1);

        // The fresh one is still consumable.
        let uid = alloc.consume(&fresh.id, &operator("op")).unwrap();
        assert_eq!(uid, "ts-3");
    }

    // ===============================================================
    // session_attach / attach_open method-handler tests
    // ===============================================================

    use crate::control::protocol::ErrorCode;

    const TEST_ATTACH_ADDR: &str = "/tmp/cm-daemon-test.sock";

    #[test]
    fn session_attach_with_operator_caller_returns_ticket() {
        let alloc = TicketAllocator::new();
        let caller = operator("op-default");
        let resp = session_attach(
            &alloc,
            &caller,
            &SessionAttachParams { uid: "ts-7".into() },
            TEST_ATTACH_ADDR,
        )
        .expect("operator can attach");
        assert!(!resp.attach_ticket.is_empty());
        assert_eq!(resp.attach_addr, TEST_ATTACH_ADDR);
        assert_eq!(alloc.pending_count(), 1);
    }

    #[test]
    fn session_attach_rejects_session_caller_in_phase_1() {
        // Session callers attaching to other sessions is a future
        // use case; Phase 1 keeps the rule strict.
        let alloc = TicketAllocator::new();
        let caller = session("ts-self");
        let err = session_attach(
            &alloc,
            &caller,
            &SessionAttachParams { uid: "ts-7".into() },
            TEST_ATTACH_ADDR,
        )
        .expect_err("session caller must be Unauthorized");
        assert_eq!(err.code(), ErrorCode::Unauthorized);
        assert!(err.message().contains("Operator"));
        assert_eq!(
            alloc.pending_count(),
            0,
            "rejected requests must not produce side effects",
        );
    }

    #[test]
    fn attach_open_happy_path_consumes_ticket() {
        let alloc = TicketAllocator::new();
        let caller = operator("op-default");
        let issued = session_attach(
            &alloc,
            &caller,
            &SessionAttachParams { uid: "ts-7".into() },
            TEST_ATTACH_ADDR,
        )
        .unwrap();

        let resp = attach_open(
            &alloc,
            &caller,
            &AttachOpenParams {
                ticket: issued.attach_ticket.clone(),
                cols: None,
                rows: None,
            },
        )
        .expect("consume succeeds");
        assert_eq!(resp.session_uid, "ts-7");
        assert_eq!(alloc.pending_count(), 0, "single-use: ticket consumed");
    }

    #[test]
    fn attach_open_with_unknown_ticket_is_not_found() {
        let alloc = TicketAllocator::new();
        let err = attach_open(
            &alloc,
            &operator("op-default"),
            &AttachOpenParams {
                ticket: "never-issued".into(),
                cols: None,
                rows: None,
            },
        )
        .expect_err("unknown ticket must be rejected");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.message().contains("not valid"));
    }

    #[test]
    fn attach_open_with_expired_ticket_collapses_to_not_found() {
        // The reviewer-specified scope check: expired + unknown
        // both surface as NotFound so a probe-style caller can't
        // distinguish them. Here we use the with_clock allocator
        // form to drive expiry without sleeping.
        let alloc = TicketAllocator::new();
        let caller = operator("op-default");
        let issued_at = Instant::now() - Duration::from_secs(60);
        let ticket = alloc.allocate_with_clock("ts-9", caller.clone(), issued_at);

        let err = attach_open(
            &alloc,
            &caller,
            &AttachOpenParams {
                ticket: ticket.id.clone(),
                cols: None,
                rows: None,
            },
        )
        .expect_err("expired ticket must be rejected");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(
            err.message().contains("not valid"),
            "must not leak 'expired' in the message: {}",
            err.message(),
        );
    }

    #[test]
    fn attach_open_with_wrong_caller_is_unauthorized() {
        // Ticket issued to operator A; operator B tries to consume.
        let alloc = TicketAllocator::new();
        let issued = session_attach(
            &alloc,
            &operator("op-alice"),
            &SessionAttachParams { uid: "ts-7".into() },
            TEST_ATTACH_ADDR,
        )
        .unwrap();

        let err = attach_open(
            &alloc,
            &operator("op-bob"),
            &AttachOpenParams {
                ticket: issued.attach_ticket.clone(),
                cols: None,
                rows: None,
            },
        )
        .expect_err("wrong caller must be rejected");
        assert_eq!(err.code(), ErrorCode::Unauthorized);
        assert!(
            err.message().contains("issued to a different caller"),
            "message should hint at the identity mismatch: {}",
            err.message(),
        );

        // The legitimate caller can still consume — identity
        // mismatch is non-destructive.
        let resp = attach_open(
            &alloc,
            &operator("op-alice"),
            &AttachOpenParams {
                ticket: issued.attach_ticket,
                cols: None,
                rows: None,
            },
        )
        .expect("legitimate caller still succeeds");
        assert_eq!(resp.session_uid, "ts-7");
    }

    #[test]
    fn attach_open_double_consume_is_not_found_on_second_call() {
        // First consume succeeds; second collapses to NotFound
        // because the allocator removed the entry on the first
        // success (single-use).
        let alloc = TicketAllocator::new();
        let caller = operator("op-default");
        let issued = session_attach(
            &alloc,
            &caller,
            &SessionAttachParams { uid: "ts-7".into() },
            TEST_ATTACH_ADDR,
        )
        .unwrap();

        let first = attach_open(
            &alloc,
            &caller,
            &AttachOpenParams {
                ticket: issued.attach_ticket.clone(),
                cols: None,
                rows: None,
            },
        )
        .expect("first consume succeeds");
        assert_eq!(first.session_uid, "ts-7");

        let err = attach_open(
            &alloc,
            &caller,
            &AttachOpenParams {
                ticket: issued.attach_ticket,
                cols: None,
                rows: None,
            },
        )
        .expect_err("second consume must fail");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }

    #[test]
    fn session_attach_response_is_json_serializable() {
        // The dispatcher serializes the handler's response into the
        // `Response::result` JSON value, so the response type must
        // round-trip through serde cleanly.
        let resp = SessionAttachResponse {
            attach_ticket: "tk-x".into(),
            attach_addr: TEST_ATTACH_ADDR.into(),
        };
        let s = serde_json::to_string(&resp).unwrap();
        let back: SessionAttachResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(back.attach_ticket, "tk-x");
        assert_eq!(back.attach_addr, TEST_ATTACH_ADDR);
    }

    #[test]
    fn attach_open_request_is_json_deserializable() {
        // The dispatcher deserializes `req.params` into the handler's
        // request type; this test pins the JSON shape so a future
        // schema drift fails noisily.
        //
        // Legacy shape (older TUI, ticket only): cols/rows default to
        // None so the daemon falls back to the post-attach Resize frame.
        let s = r#"{ "ticket": "tk-xyz" }"#;
        let req: AttachOpenParams = serde_json::from_str(s).unwrap();
        assert_eq!(req.ticket, "tk-xyz");
        assert_eq!(req.cols, None);
        assert_eq!(req.rows, None);

        // Size-bearing shape (current TUI): cols/rows present so the
        // daemon resizes the PTY server-side at stream bind.
        let s = r#"{ "ticket": "tk-xyz", "cols": 203, "rows": 51 }"#;
        let req: AttachOpenParams = serde_json::from_str(s).unwrap();
        assert_eq!(req.ticket, "tk-xyz");
        assert_eq!(req.cols, Some(203));
        assert_eq!(req.rows, Some(51));
    }

    #[test]
    fn allocator_is_thread_safe() {
        // Drive a few hundred allocate+consume cycles from multiple
        // threads to catch any obvious Mutex / map-ordering breakage.
        // Not a stress test — just a smoke check that the shared
        // Mutex<HashMap> compiles and doesn't panic under contention.
        use std::sync::Arc;
        use std::thread;

        let alloc = Arc::new(TicketAllocator::new());
        let mut handles = Vec::new();
        for t in 0..8 {
            let a = alloc.clone();
            handles.push(thread::spawn(move || {
                for i in 0..50 {
                    let caller = operator(&format!("op-{}", t));
                    let ticket = a.allocate(format!("ts-{}-{}", t, i), caller.clone());
                    let uid = a.consume(&ticket.id, &caller).unwrap();
                    assert_eq!(uid, format!("ts-{}-{}", t, i));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(alloc.pending_count(), 0);
    }
}
