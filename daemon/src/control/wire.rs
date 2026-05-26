//! Per-connection length-prefixed JSON framing for the daemon's
//! accept loop. Slice 10a-shell.
//!
//! Mirrors the wire format `tui/src/control/server.rs` already uses
//! on the TUI side: 4-byte big-endian length prefix, then a UTF-8
//! JSON body. One request-response round trip per connection in v1
//! (streaming subscriptions are a later phase).
//!
//! Lives in a separate module from `dispatch.rs` so the framing
//! layer can be unit-tested against `Cursor`s without spawning real
//! sockets. The accept loop in `lib.rs::run()` glues these helpers
//! together with `UnixStream` I/O.

use std::io::{Read, Write};

use crate::control::protocol::{Request, Response, StreamFrame};

/// Same cap as the Python `control_client.py` uses on its outbound
/// request body: 4 MiB. The wire format permits up to `u32::MAX` but
/// any legitimate request is well under this — the cap defends
/// against a corrupted/malicious peer driving the daemon to allocate
/// a multi-gigabyte buffer.
pub const MAX_REQUEST_BYTES: u32 = 4 * 1024 * 1024;

/// Read one length-prefixed JSON `Request` from `reader`. Returns
/// `Ok(None)` on clean EOF at the frame boundary (the client closed
/// before sending anything), `Err` on any partial read / oversize /
/// malformed payload.
pub fn read_request<R: Read>(reader: &mut R) -> std::io::Result<Option<Request>> {
    let mut len_buf = [0u8; 4];
    let mut filled = 0;
    while filled < len_buf.len() {
        match reader.read(&mut len_buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    // Clean inter-frame close — caller dialed and
                    // hung up without sending.
                    return Ok(None);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "torn frame: got {} of 4 length-prefix bytes before EOF",
                        filled
                    ),
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_REQUEST_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "request body length {} exceeds cap of {} bytes",
                len, MAX_REQUEST_BYTES
            ),
        ));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body)?;
    let req: Request = serde_json::from_slice(&body).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    Ok(Some(req))
}

/// Write a length-prefixed JSON `Response` to `writer`.
pub fn write_response<W: Write>(
    writer: &mut W,
    resp: &Response,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(resp).map_err(std::io::Error::other)?;
    if body.len() > MAX_REQUEST_BYTES as usize {
        // The dispatcher shouldn't produce responses this large,
        // but defend against the case rather than silently truncating.
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "response body length {} exceeds cap of {} bytes",
                body.len(),
                MAX_REQUEST_BYTES
            ),
        ));
    }
    let len = (body.len() as u32).to_be_bytes();
    writer.write_all(&len)?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Write a length-prefixed JSON `Request` to `writer`. The client-
/// side mirror of [`read_request`]. Slice 10c-e-2 introduces this
/// for `tui/src/client_session.rs::ClientSession::new` to drive the
/// daemon RPC dance (`start_session` / `session.attach` /
/// `attach.open`) without re-implementing the framing inline.
pub fn write_request<W: Write>(
    writer: &mut W,
    req: &Request,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(req).map_err(std::io::Error::other)?;
    if body.len() > MAX_REQUEST_BYTES as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "request body length {} exceeds cap of {} bytes",
                body.len(),
                MAX_REQUEST_BYTES
            ),
        ));
    }
    let len = (body.len() as u32).to_be_bytes();
    writer.write_all(&len)?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Read a length-prefixed JSON `Response` from `reader`. The client-
/// side mirror of [`write_response`]. Slice 10c-e-2 introduces this
/// alongside [`write_request`] for the TUI's daemon-RPC helpers.
///
/// Returns `Ok(None)` only on clean EOF before any prefix bytes
/// arrived (the daemon closed the connection without responding —
/// e.g. it crashed mid-handler). Partial reads surface as
/// `UnexpectedEof`. Malformed JSON surfaces as `InvalidData`.
pub fn read_response<R: Read>(reader: &mut R) -> std::io::Result<Option<Response>> {
    let mut len_buf = [0u8; 4];
    let mut filled = 0;
    while filled < len_buf.len() {
        match reader.read(&mut len_buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "torn response prefix: {} of 4 bytes before EOF",
                        filled
                    ),
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_REQUEST_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "response body length {} exceeds cap of {} bytes",
                len, MAX_REQUEST_BYTES
            ),
        ));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body)?;
    let resp: Response = serde_json::from_slice(&body).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    Ok(Some(resp))
}

/// Read a length-prefixed JSON `StreamFrame` from `reader`. The
/// counterpart to [`write_stream_frame`], introduced for the
/// inbound side of `crate::control::stream::handle_attach_stream`
/// (slice 10c-e-2 review fix) and for the TUI's `term_shim::StreamReader`
/// at the wire-edge — both decode frames in the same shape.
///
/// `Ok(None)` is reserved for clean inter-frame EOF; a partial
/// prefix surfaces as `UnexpectedEof`. Frame bodies above
/// [`MAX_REQUEST_BYTES`] are rejected before allocation, same as
/// the request reader.
pub fn read_stream_frame<R: Read>(
    reader: &mut R,
) -> std::io::Result<Option<StreamFrame>> {
    let mut len_buf = [0u8; 4];
    let mut filled = 0;
    while filled < len_buf.len() {
        match reader.read(&mut len_buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "torn stream-frame prefix: {} of 4 bytes before EOF",
                        filled
                    ),
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_REQUEST_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "stream-frame body length {} exceeds cap of {} bytes",
                len, MAX_REQUEST_BYTES
            ),
        ));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body)?;
    let frame: StreamFrame = serde_json::from_slice(&body).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    Ok(Some(frame))
}

/// Write a length-prefixed JSON `StreamFrame` to `writer`. Same wire
/// shape as [`write_response`] (4-byte BE length + JSON body), but
/// the body parses as `StreamFrame` rather than `Response` —
/// `kind` field is the discriminator (see
/// [`crate::control::protocol::StreamFrame`]).
///
/// Used by the attach-stream path (slice 10c-c) to relay PTY bytes
/// after the `attach.open` RPC response has been written. Subsequent
/// frames on the same connection are stream frames until the daemon
/// writes `kind = "end"` or the client closes.
pub fn write_stream_frame<W: Write>(
    writer: &mut W,
    frame: &StreamFrame,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(frame).map_err(std::io::Error::other)?;
    if body.len() > MAX_REQUEST_BYTES as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "stream frame body length {} exceeds cap of {} bytes",
                body.len(),
                MAX_REQUEST_BYTES
            ),
        ));
    }
    let len = (body.len() as u32).to_be_bytes();
    writer.write_all(&len)?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::protocol::{Caller, ErrorCode};
    use std::io::Cursor;

    fn frame_bytes_for_request(req: &Request) -> Vec<u8> {
        let body = serde_json::to_vec(req).unwrap();
        let mut out = (body.len() as u32).to_be_bytes().to_vec();
        out.extend(body);
        out
    }

    #[test]
    fn read_request_round_trips_a_full_request() {
        let original = Request {
            id: "r-1".into(),
            caller: Caller::session("ts-x"),
            method: "ping".into(),
            params: serde_json::json!({"key": "value"}),
        };
        let wire = frame_bytes_for_request(&original);
        let mut cursor = Cursor::new(wire);
        let parsed = read_request(&mut cursor)
            .expect("io ok")
            .expect("frame present");
        assert_eq!(parsed.id, "r-1");
        assert_eq!(parsed.method, "ping");
        assert_eq!(parsed.caller.session_uid(), Some("ts-x"));
        assert_eq!(parsed.params, serde_json::json!({"key": "value"}));
    }

    #[test]
    fn read_request_clean_eof_returns_none() {
        // Empty input — the client closed without sending. Must be
        // Ok(None), not an error.
        let mut cursor = Cursor::new(Vec::<u8>::new());
        assert!(read_request(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn read_request_partial_prefix_is_torn_frame_error() {
        // 2 of 4 length bytes then EOF — the client crashed
        // mid-write. Must surface as UnexpectedEof so the daemon
        // logs it; conflating with "clean close" would hide real
        // connection failures.
        let mut wire = (42u32).to_be_bytes().to_vec();
        wire.truncate(2);
        let mut cursor = Cursor::new(wire);
        let err = read_request(&mut cursor)
            .expect_err("partial prefix must error");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains("2 of 4"));
    }

    #[test]
    fn read_request_oversize_length_errors_before_allocate() {
        // Length prefix advertising > 4 MiB. Must error without
        // doing the allocation — otherwise a hostile peer can
        // force the daemon into multi-gigabyte allocations.
        let len = MAX_REQUEST_BYTES + 1;
        let wire = len.to_be_bytes().to_vec();
        let mut cursor = Cursor::new(wire);
        let err = read_request(&mut cursor)
            .expect_err("oversize must error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds cap"));
    }

    #[test]
    fn read_request_truncated_body_is_torn_frame() {
        // Length prefix says 50, body is 5 bytes. UnexpectedEof.
        let prefix = (50u32).to_be_bytes();
        let mut wire = prefix.to_vec();
        wire.extend(b"short");
        let mut cursor = Cursor::new(wire);
        let err = read_request(&mut cursor)
            .expect_err("truncated body must error");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn read_request_malformed_json_is_invalid_data() {
        let body = b"not valid json {";
        let mut wire = (body.len() as u32).to_be_bytes().to_vec();
        wire.extend(body);
        let mut cursor = Cursor::new(wire);
        let err = read_request(&mut cursor)
            .expect_err("malformed body must error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn write_response_round_trips_through_a_cursor() {
        let resp = Response::err("r-1".into(), ErrorCode::Unauthorized, "no");
        let mut sink = Vec::new();
        write_response(&mut sink, &resp).expect("write ok");
        // Decode by hand and confirm.
        let len = u32::from_be_bytes(sink[..4].try_into().unwrap()) as usize;
        let body = &sink[4..4 + len];
        let parsed: Response = serde_json::from_slice(body).unwrap();
        assert_eq!(parsed.id, "r-1");
        assert!(!parsed.ok);
        assert_eq!(
            parsed.error.unwrap().code,
            ErrorCode::Unauthorized
        );
    }

    #[test]
    fn round_trip_request_then_response_via_helpers() {
        // Belt-and-suspenders: write a response, read it back as a
        // body, parse, confirm. Exercises both write_response and
        // the corresponding read shape.
        let resp = Response::ok(
            "r-1".into(),
            serde_json::json!({"pong": true}),
        );
        let mut sink = Vec::new();
        write_response(&mut sink, &resp).expect("write ok");

        let len = u32::from_be_bytes(sink[..4].try_into().unwrap()) as usize;
        let body = &sink[4..4 + len];
        let parsed: Response = serde_json::from_slice(body).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.result.unwrap()["pong"], true);
    }
}
