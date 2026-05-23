//! Accept loop on `~/.cm/tui.sock`. One request per connection. Bind on
//! TUI startup; tear the socket file on shutdown.
//!
//! Connection lifecycle:
//!   1. Read 4-byte big-endian length prefix.
//!   2. Read that many bytes (UTF-8 JSON Request envelope).
//!   3. Submit (Request, reply_tx) to the shared queue.
//!   4. Block on reply_rx with REPLY_TIMEOUT.
//!   5. Write 4-byte length + JSON Response.
//!   6. Close.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use super::protocol::{ErrorCode, Request, Response};
use super::queue::{Queue, REPLY_TIMEOUT};

/// Default path. Override via `CM_TUI_SOCKET` for tests / multi-TUI setups.
///
/// `CM_TUI_SOCKET=""` (literal empty / whitespace-only) counts as
/// "unset" — slice-10c-b review fix, parity with the Python
/// resolver and with `cm_daemon::default_socket_path`. Without this,
/// `mcp_config::build_env`'s authoritative-empty entry would be
/// re-read by this fn as a present-but-empty pin, leading
/// downstream code to absolutize `""` to cwd.
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = cm_daemon::path::env_socket_override("CM_TUI_SOCKET") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".cm/tui.sock")
}

/// Bind the socket and spawn the accept loop on a new thread. Returns
/// the queue handle (for the main loop to drain) and the socket path
/// (for cleanup on shutdown). Errors propagate to the caller; the TUI
/// is expected to log and continue without a control socket if binding
/// fails (an old socket file from a crashed TUI gets cleaned up first).
pub fn start(queue: Queue) -> std::io::Result<PathBuf> {
    let path = default_socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Don't blindly unlink an existing socket — that would silently
    // steal MCP traffic from a still-running TUI. Probe first: if a
    // connect succeeds, somebody's home and we abort. Only when the
    // path exists but connect refuses (stale inode from a crashed
    // TUI) do we remove it.
    if path.exists() {
        match UnixStream::connect(&path) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    ErrorKind::AddrInUse,
                    format!(
                        "another TUI appears to be running at {} — \
                         set CM_TUI_SOCKET to a different path or close \
                         the other instance",
                        path.display()
                    ),
                ));
            }
            Err(e) if e.kind() == ErrorKind::ConnectionRefused
                || e.kind() == ErrorKind::NotFound =>
            {
                // Stale socket — safe to remove.
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => {
                // Other connect error (e.g. EACCES) — leave the file
                // alone and let bind error out below with a useful
                // message.
            }
        }
    }

    let listener = UnixListener::bind(&path)?;
    let path_for_thread = path.clone();
    thread::Builder::new()
        .name("tui-control-socket".into())
        .spawn(move || accept_loop(listener, queue, path_for_thread))?;
    Ok(path)
}

fn accept_loop(listener: UnixListener, queue: Queue, _path: PathBuf) {
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let q = queue.clone();
                // Per-connection handling on a fresh thread so a slow
                // request doesn't head-of-line block accept.
                let _ = thread::Builder::new()
                    .name("tui-control-conn".into())
                    .spawn(move || handle_connection(stream, q));
            }
            Err(_) => {
                // EBADF on shutdown — listener gone, exit cleanly.
                break;
            }
        }
    }
}

fn handle_connection(mut stream: UnixStream, queue: Queue) {
    // Bound the connection's read time so a misbehaving client can't
    // hold a connection slot forever. Generous to allow large request
    // bodies (start_session prompts can be a few KB).
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let req_bytes = match read_frame(&mut stream) {
        Ok(b) => b,
        Err(_) => return, // io error or oversized — drop quietly
    };

    let req: Request = match serde_json::from_slice(&req_bytes) {
        Ok(r) => r,
        Err(e) => {
            // Best-effort error response; if we can't even parse the
            // envelope we don't know the request id, so use empty.
            let resp = Response::err(
                String::new(),
                ErrorCode::InvalidParams,
                format!("malformed request: {}", e),
            );
            let _ = write_frame(&mut stream, &resp);
            return;
        }
    };

    let req_id = req.id.clone();
    let rx = queue.submit(req);
    let resp = match rx.recv_timeout(REPLY_TIMEOUT) {
        Ok(r) => r,
        Err(_) => Response::err(
            req_id,
            ErrorCode::Internal,
            "main loop did not reply within timeout",
        ),
    };
    let _ = write_frame(&mut stream, &resp);
}

/// Maximum request body size. Larger requests are rejected to bound
/// memory under a hostile client. 4 MiB easily covers prompt bodies.
const MAX_FRAME_BYTES: u32 = 4 * 1024 * 1024;

pub fn read_frame(stream: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body)?;
    Ok(body)
}

pub fn write_frame(stream: &mut impl Write, resp: &Response) -> std::io::Result<()> {
    let body = serde_json::to_vec(resp)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let len = body.len() as u32;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

/// Best-effort socket cleanup at shutdown.
pub fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}
