//! Bounded, lossless batching for terminal viewers. A slow viewer never
//! blocks the PTY reader or grows an unbounded per-connection output queue.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const CAPACITY: usize = 2 * 1024 * 1024;
const BATCH_BYTES: usize = 64 * 1024;
const BACKGROUND_INTERVAL: Duration = Duration::from_millis(250);

struct State {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
    dropped: bool,
    overflow: bool,
    background: bool,
    last_sent: Instant,
    interactive_until: Instant,
}

#[derive(Clone)]
pub struct OutputSender(Arc<(Mutex<State>, Condvar)>);

pub struct OutputSubscription(OutputSender);

#[derive(Debug, PartialEq, Eq)]
pub enum RecvError {
    Timeout,
    Closed,
    Overflow,
}

pub fn channel() -> (OutputSender, OutputSubscription) {
    let sender = OutputSender(Arc::new((
        Mutex::new(State {
            chunks: VecDeque::new(),
            bytes: 0,
            closed: false,
            dropped: false,
            overflow: false,
            background: false,
            last_sent: Instant::now(),
            interactive_until: Instant::now(),
        }),
        Condvar::new(),
    )));
    (sender.clone(), OutputSubscription(sender))
}

impl OutputSender {
    pub fn push(&self, bytes: &[u8]) -> bool {
        let mut s = self.0 .0.lock().unwrap_or_else(|p| p.into_inner());
        if s.dropped || s.closed || s.overflow {
            return false;
        }
        if bytes.len() > CAPACITY.saturating_sub(s.bytes) {
            s.overflow = true;
            s.chunks.clear();
            s.bytes = 0;
            self.0 .1.notify_one();
            return false;
        }
        if !bytes.is_empty() {
            let was_empty = s.chunks.is_empty();
            s.chunks
                .extend(bytes.chunks(BATCH_BYTES).map(<[u8]>::to_vec));
            s.bytes += bytes.len();
            // Hidden small writes can share one timer wakeup. The first
            // chunk and the size threshold wake the sleeping consumer.
            if was_empty || s.bytes >= BATCH_BYTES {
                self.0 .1.notify_one();
            }
        }
        true
    }

    pub fn close(&self) {
        self.0 .0.lock().unwrap_or_else(|p| p.into_inner()).closed = true;
        self.0 .1.notify_one();
    }

    pub fn note_input(&self) {
        self.0
             .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .interactive_until = Instant::now() + Duration::from_secs(2);
        self.0 .1.notify_one();
    }

    pub fn set_background(&self, background: bool) {
        self.0
             .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .background = background;
        self.0 .1.notify_one();
    }
}

impl OutputSubscription {
    pub fn flow_control(&self) -> OutputSender {
        self.0.clone()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Vec<u8>, RecvError> {
        let deadline = Instant::now() + timeout;
        let (lock, changed) = &*self.0 .0;
        let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if s.overflow {
                return Err(RecvError::Overflow);
            }
            let now = Instant::now();
            let batch_at = s.last_sent + BACKGROUND_INTERVAL;
            if s.bytes > 0
                && (!s.background
                    || now < s.interactive_until
                    || s.closed
                    || now >= batch_at
                    || s.bytes >= BATCH_BYTES)
            {
                let mut batch = s.chunks.pop_front().expect("buffered output");
                while s
                    .chunks
                    .front()
                    .is_some_and(|c| batch.len() + c.len() <= BATCH_BYTES)
                {
                    batch.extend(s.chunks.pop_front().unwrap());
                }
                s.bytes -= batch.len();
                s.last_sent = now;
                return Ok(batch);
            }
            if s.closed {
                return Err(RecvError::Closed);
            }
            if now >= deadline {
                return Err(RecvError::Timeout);
            }
            let wake_at = if s.bytes > 0 {
                deadline.min(batch_at)
            } else {
                deadline
            };
            s = changed
                .wait_timeout(s, wake_at.saturating_duration_since(now))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    #[cfg(test)]
    pub fn recv(&self) -> Result<Vec<u8>, RecvError> {
        self.recv_timeout(Duration::from_secs(2))
    }
}

impl Drop for OutputSubscription {
    fn drop(&mut self) {
        let mut s = self.0 .0 .0.lock().unwrap_or_else(|p| p.into_inner());
        s.dropped = true;
        s.chunks.clear();
        s.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_preserve_every_byte_and_close_drains_tail() {
        let (tx, rx) = channel();
        tx.set_background(true);
        let expected: Vec<u8> = (0..100_000).map(|n| (n % 251) as u8).collect();
        for part in expected.chunks(137) {
            assert!(tx.push(part));
        }
        tx.close();
        let mut actual = Vec::new();
        loop {
            match rx.recv() {
                Ok(bytes) => actual.extend(bytes),
                Err(RecvError::Closed) => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn focus_change_flushes_hidden_output_immediately() {
        let (tx, rx) = channel();
        tx.set_background(true);
        tx.push(b"draft");
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(10)),
            Err(RecvError::Timeout)
        );
        tx.set_background(false);
        assert_eq!(rx.recv_timeout(Duration::ZERO).unwrap(), b"draft");
    }

    #[test]
    fn slow_viewer_overflow_is_explicit_and_does_not_block_other_viewers() {
        let (slow_tx, slow_rx) = channel();
        let (fast_tx, fast_rx) = channel();
        assert!(slow_tx.push(&vec![0; CAPACITY]));
        assert!(!slow_tx.push(b"overflow"));
        assert!(fast_tx.push(b"still interactive"));
        assert_eq!(fast_rx.recv().unwrap(), b"still interactive");
        assert_eq!(slow_rx.recv(), Err(RecvError::Overflow));
        assert_eq!(slow_tx.0 .0.lock().unwrap().bytes, 0);
    }
}
