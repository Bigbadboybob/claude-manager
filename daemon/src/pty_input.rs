//! Bounded writes to a session's PTY.
//!
//! A child that stops reading its input (wedged, stopped, or busy in a
//! loop that never reads stdin) fills the kernel's PTY input buffer, and a
//! plain blocking `write_all` on the master then never returns. Before
//! 2026-10-16 that parked whichever thread was writing for good, and with
//! it the session's writer mutex: the attach stream's inbound loop (so its
//! resizes too), delivery threads, and RPC handlers piled up behind it, and
//! a brain restart could only abort (`restart_busy`).
//!
//! [`BoundedWriter`] hands each write to a per-session worker thread and
//! waits at most [`WRITE_TIMEOUT`]. On timeout the caller gets `TimedOut`
//! and the daemon logs it; later writes fail at once until the worker's
//! write finishes. Other sessions are untouched: each has its own worker.
//! Bytes already handed to a stuck worker still reach the child if it
//! starts reading again; nothing is dropped silently.

use std::io::{self, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(not(test))]
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

type Job = (Vec<u8>, mpsc::SyncSender<io::Result<()>>);

pub struct BoundedWriter {
    uid: String,
    jobs: mpsc::Sender<Job>,
    /// When the worker's current write started; `None` while idle.
    busy_since: Arc<Mutex<Option<Instant>>>,
    timeout: Duration,
}

impl BoundedWriter {
    pub fn new(uid: &str, inner: Box<dyn Write + Send>) -> io::Result<Self> {
        Self::with_timeout(uid, inner, WRITE_TIMEOUT)
    }

    pub fn with_timeout(uid: &str, mut inner: Box<dyn Write + Send>, timeout: Duration) -> io::Result<Self> {
        let (jobs, rx) = mpsc::channel::<Job>();
        let busy_since = Arc::new(Mutex::new(None));
        let busy = Arc::clone(&busy_since);
        std::thread::Builder::new()
            .name("cm-pty-input".into())
            .spawn(move || {
                for (bytes, done) in rx {
                    *busy.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
                    let result = inner.write_all(&bytes).and_then(|()| inner.flush());
                    *busy.lock().unwrap_or_else(|p| p.into_inner()) = None;
                    let _ = done.send(result);
                }
            })?;
        Ok(Self { uid: uid.to_owned(), jobs, busy_since, timeout })
    }

    fn stalled(&self, since: Instant) -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "PTY input for {} has been blocked for {} s: the child is not reading its \
                 input. Input fails until it reads again; restart the session (A-R) if it is wedged",
                self.uid,
                since.elapsed().as_secs(),
            ),
        )
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(since) = *self.busy_since.lock().unwrap_or_else(|p| p.into_inner()) {
            // An earlier write timed out and is still stuck in the kernel.
            return Err(self.stalled(since));
        }
        let (done, result) = mpsc::sync_channel(1);
        self.jobs
            .send((buf.to_vec(), done))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "PTY input worker is gone"))?;
        match result.recv_timeout(self.timeout) {
            Ok(r) => r.map(|()| buf.len()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let since = self
                    .busy_since
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .unwrap_or_else(|| Instant::now() - self.timeout);
                let e = self.stalled(since);
                eprintln!("cm-daemon: {e}");
                Err(e)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "PTY input worker is gone"))
            }
        }
    }

    /// Every write is flushed by the worker before it reports back.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that blocks until released, like a full PTY input buffer.
    struct Gate(Arc<(Mutex<bool>, std::sync::Condvar)>, Arc<Mutex<Vec<u8>>>);
    impl Write for Gate {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let (open, cv) = &*self.0;
            let mut open = open.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
            self.1.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A real PTY whose child never reads stdin: input to it fails within
    /// the bound instead of hanging, and another session's input is not
    /// held up by it.
    #[test]
    fn a_child_that_stops_reading_fails_its_input_without_blocking_another_session() {
        use crate::session::{DaemonSession, SpawnParams};
        // Raw mode, like claude and codex: in canonical mode the kernel
        // drops input once the line buffer is full, so writes never block.
        let mut params = SpawnParams::new("ts-stuck-pty", "pty-input-test", "/bin/sh");
        params.args = vec!["-c".into(), "stty raw -echo; echo ready; exec sleep 600".into()];
        let mut stuck = DaemonSession::spawn(params).expect("spawn sleep");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(&stuck.fanout.snapshot_since(None).bytes).contains("ready") {
            assert!(Instant::now() < deadline, "the child never switched to raw mode");
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut live = DaemonSession::spawn(SpawnParams::new("ts-live-pty", "pty-input-test", "/bin/cat"))
            .expect("spawn cat");

        let handle = stuck.input_handle();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Far more than the kernel PTY buffers hold.
            let _ = tx.send(handle.write_and_stamp(&vec![b'x'; 1 << 20]));
        });
        let result = rx
            .recv_timeout(WRITE_TIMEOUT + Duration::from_secs(10))
            .expect("a write to a child that never reads must return, not hang");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);

        let started = Instant::now();
        live.input_handle().write_and_stamp(b"still typing\n").expect("other session's input");
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        let started = Instant::now();
        assert!(stuck.input_handle().write_and_stamp(b"y").is_err());
        assert!(started.elapsed() < Duration::from_millis(500), "the stuck session fails fast now");

        let _ = stuck.kill();
        let _ = live.kill();
    }

    #[test]
    fn a_stuck_write_times_out_then_fails_fast_and_lands_once_the_child_reads() {
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let got = Arc::new(Mutex::new(Vec::new()));
        let mut w = BoundedWriter::with_timeout(
            "ts-stuck",
            Box::new(Gate(Arc::clone(&gate), Arc::clone(&got))),
            Duration::from_millis(200),
        )
        .unwrap();
        let started = Instant::now();
        let e = w.write_all(b"hello").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(e.to_string().contains("not reading its input"), "{e}");

        let started = Instant::now();
        assert_eq!(w.write_all(b"more").unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(100), "later writes fail at once");

        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let deadline = Instant::now() + Duration::from_secs(5);
        while w.busy_since.lock().unwrap().is_some() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(&*got.lock().unwrap(), b"hello", "the handed-over bytes still landed");
        w.write_all(b" again").unwrap();
        assert_eq!(&*got.lock().unwrap(), b"hello again");
    }
}
