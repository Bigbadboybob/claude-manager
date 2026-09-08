//! Drive buffered attach input even when the remote terminal produces no output.
//!
//! Alacritty does not call Write::flush after its input buffer is consumed.
//! A StreamWriter can still have a partial frame at that point. The output
//! worker sleeps when empty and polls socket writability under backpressure;
//! it never needs a keystroke or an inbound terminal event to finish a frame.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};

use crate::term_shim::{StreamWriter, MAX_INPUT_FRAME_BYTES};

struct State {
    writer: StreamWriter<UnixStream>,
    stopped: bool,
    error: Option<io::ErrorKind>,
    background_update: Option<bool>,
}

pub struct AttachWriter {
    state: Arc<(Mutex<State>, Condvar)>,
    socket: UnixStream,
}

/// UI-side flow updates are coalesced in the writer state and sent by its
/// worker. Never write a socket or wait for backpressure on the UI thread.
pub struct OutputControl {
    state: Arc<(Mutex<State>, Condvar)>,
    last_visible: std::time::Instant,
    background: bool,
}

impl OutputControl {
    pub fn set_visible(&mut self, visible: bool) {
        if visible { self.last_visible = std::time::Instant::now(); }
        let background = !visible && self.last_visible.elapsed() >= std::time::Duration::from_secs(2);
        if self.background == background { return; }
        // The critical section contains only nonblocking writes of one frame.
        // Contention simply retries next tick, never stalls keyboard handling.
        let Ok(mut s) = self.state.0.try_lock() else { return; };
        if s.stopped || s.error.is_some() { return; }
        s.background_update = Some(background);
        self.background = background;
        self.state.1.notify_one();
    }
}

impl AttachWriter {
    pub fn new(socket: UnixStream, stream_id: impl Into<String>) -> io::Result<Self> {
        let worker_socket = socket.try_clone()?;
        let writer = StreamWriter::new(socket.try_clone()?, stream_id);
        let state = Arc::new((
            Mutex::new(State {
                writer,
                stopped: false,
                error: None,
                background_update: None,
            }),
            Condvar::new(),
        ));
        let shared = Arc::clone(&state);
        std::thread::Builder::new()
            .name("cm-attach-input".into())
            .spawn(move || {
                let (lock, ready) = &*shared;
                loop {
                    let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
                    while !state.stopped && state.writer.pending_bytes() == 0 && state.background_update.is_none() {
                        state = ready.wait(state).unwrap_or_else(|p| p.into_inner());
                    }
                    if state.stopped {
                        return;
                    }
                    if state.writer.pending_bytes() == 0 {
                        if let Some(background) = state.background_update.take() {
                            if let Err(e) = state.writer.send_output_flow(background) {
                                state.error = Some(e.kind());
                                let _ = worker_socket.shutdown(std::net::Shutdown::Both);
                                return;
                            }
                        }
                    }
                    match state.writer.flush_pending() {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(e) => {
                            state.error = Some(e.kind());
                            // Wake the reader too, so a failed writer causes normal
                            // transport reconnect even when the agent is silent.
                            let _ = worker_socket.shutdown(std::net::Shutdown::Both);
                            return;
                        }
                    }
                    drop(state);
                    let mut fd = libc::pollfd {
                        fd: worker_socket.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // Bounded wait only while congested. Drop also shuts down the
                    // socket, waking poll immediately; an empty queue uses Condvar.
                    unsafe {
                        libc::poll(&mut fd, 1, 100);
                    }
                }
            })?;
        Ok(Self { state, socket })
    }

    pub fn output_control(&self) -> OutputControl {
        OutputControl { state: self.state.clone(), last_visible: std::time::Instant::now(), background: false }
    }

    fn with_writer<T>(
        &self,
        f: impl FnOnce(&mut StreamWriter<UnixStream>) -> io::Result<T>,
    ) -> io::Result<T> {
        let (lock, ready) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(kind) = state.error {
            return Err(io::Error::from(kind));
        }
        let result = f(&mut state.writer);
        if state.writer.pending_bytes() > 0 {
            ready.notify_one();
        }
        result
    }

    pub fn send_resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.with_writer(|w| w.send_resize(cols, rows))
    }

    pub fn flush_pending(&mut self) -> io::Result<bool> {
        self.with_writer(|w| w.flush_pending())
    }

    #[cfg(test)]
    pub fn pending_bytes(&self) -> usize {
        self.state
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .writer
            .pending_bytes()
    }
}

impl Write for AttachWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // At most one input frame is buffered. Alacritty retains a large
        // paste's remainder until the socket makes progress.
        self.with_writer(|w| w.write(&buf[..buf.len().min(MAX_INPUT_FRAME_BYTES)]))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.with_writer(|w| w.flush())
    }
}

impl Drop for AttachWriter {
    fn drop(&mut self) {
        let (lock, ready) = &*self.state;
        lock.lock().unwrap_or_else(|p| p.into_inner()).stopped = true;
        ready.notify_one();
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}
