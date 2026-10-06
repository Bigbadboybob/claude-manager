//! Reader-side glue: runs a pane's PTY bytes through [`ApcFilter`] on the
//! Alacritty EventLoop thread and ships the extracted events to the UI
//! thread's [`super::PaneGraphics`].

use std::io;
use std::sync::mpsc::Sender;

use super::filter::{ApcFilter, FilterEvent};
use super::pane::StreamEvent;

const SCRATCH_BYTES: usize = 64 * 1024;

pub struct Tap {
    filter: ApcFilter,
    tx: Sender<StreamEvent>,
    /// Bytes of daemon replay still to come; `None` when the daemon did not
    /// say, in which case every event counts as replay (never answered).
    replay_left: Option<u64>,
    scratch: Vec<u8>,
    out: Vec<u8>,
    out_pos: usize,
    events: Vec<FilterEvent>,
}

impl Tap {
    pub fn new(tx: Sender<StreamEvent>, replay_bytes: Option<u64>) -> Self {
        Self {
            filter: ApcFilter::new(),
            tx,
            replay_left: replay_bytes,
            scratch: vec![0; SCRATCH_BYTES],
            out: Vec::new(),
            out_pos: 0,
            events: Vec::new(),
        }
    }

    /// `Read::read` through the filter. `source` is the unfiltered stream.
    /// Never reports `Ok(0)` for a read whose bytes were all graphics: it
    /// keeps reading until there is output, `WouldBlock`, or real EOF.
    pub fn read(
        &mut self,
        buf: &mut [u8],
        mut source: impl FnMut(&mut [u8]) -> io::Result<usize>,
    ) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.out_pos < self.out.len() {
                let n = (self.out.len() - self.out_pos).min(buf.len());
                buf[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                return Ok(n);
            }
            self.out.clear();
            self.out_pos = 0;
            let n = source(&mut self.scratch)?;
            if n == 0 {
                return Ok(0);
            }
            let (replayed, live) = match self.replay_left {
                Some(left) => {
                    let r = (left.min(n as u64)) as usize;
                    self.replay_left = Some(left - r as u64);
                    (r, n - r)
                }
                None => (n, 0),
            };
            for (range, replay) in [(0..replayed, true), (replayed..replayed + live, false)] {
                if range.is_empty() {
                    continue;
                }
                self.filter.feed(&self.scratch[range], &mut self.out, &mut self.events);
                for event in self.events.drain(..) {
                    let _ = self.tx.send(StreamEvent { event, replay });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::filter::Query;
    use std::sync::mpsc;

    /// Feeds `chunks` one per source call, then `WouldBlock`.
    fn source(chunks: Vec<Vec<u8>>) -> impl FnMut(&mut [u8]) -> io::Result<usize> {
        let mut chunks = chunks.into_iter();
        move |buf| match chunks.next() {
            Some(c) => {
                buf[..c.len()].copy_from_slice(&c);
                Ok(c.len())
            }
            None => Err(io::ErrorKind::WouldBlock.into()),
        }
    }

    #[test]
    fn graphics_only_reads_do_not_look_like_eof() {
        let (tx, rx) = mpsc::channel();
        let mut tap = Tap::new(tx, Some(0));
        let mut src = source(vec![b"\x1b_Gi=1;AA\x1b\\".to_vec(), b"text".to_vec()]);
        let mut buf = [0u8; 16];
        let n = tap.read(&mut buf, &mut src).unwrap();
        assert_eq!(&buf[..n], b"text");
        assert!(matches!(rx.try_recv().unwrap().event, FilterEvent::Graphics(_)));
        let err = tap.read(&mut buf, &mut src).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn replay_boundary_splits_a_single_read() {
        let (tx, rx) = mpsc::channel();
        let replayed = b"\x1b[>q".to_vec();
        let mut tap = Tap::new(tx, Some(replayed.len() as u64));
        let mut chunk = replayed.clone();
        chunk.extend_from_slice(b"\x1b[>q");
        let mut src = source(vec![chunk]);
        let mut buf = [0u8; 64];
        let n = tap.read(&mut buf, &mut src).unwrap();
        assert_eq!(&buf[..n], b"\x1b[>q\x1b[>q");
        let first = rx.try_recv().unwrap();
        let second = rx.try_recv().unwrap();
        assert_eq!(first.event, FilterEvent::Query(Query::XtVersion));
        assert!(first.replay);
        assert!(!second.replay);
    }

    #[test]
    fn unknown_replay_length_never_counts_as_live() {
        let (tx, rx) = mpsc::channel();
        let mut tap = Tap::new(tx, None);
        let mut src = source(vec![b"\x1b[16t".to_vec()]);
        let mut buf = [0u8; 16];
        tap.read(&mut buf, &mut src).unwrap();
        assert!(rx.try_recv().unwrap().replay);
    }

    #[test]
    fn small_caller_buffers_drain_filtered_output() {
        let (tx, _rx) = mpsc::channel();
        let mut tap = Tap::new(tx, Some(0));
        let mut src = source(vec![b"ab\x1b_Gi=1\x1b\\cd".to_vec()]);
        let mut got = Vec::new();
        let mut buf = [0u8; 1];
        while let Ok(n) = tap.read(&mut buf, &mut src) {
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"abcd");
    }
}
