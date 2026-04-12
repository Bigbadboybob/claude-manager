use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::tty;
use alacritty_terminal::Term;

use std::sync::mpsc;

/// Proxy that forwards alacritty terminal events to a channel.
#[derive(Clone)]
pub struct EventProxy {
    tx: mpsc::Sender<TermEvent>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: TermEvent) {
        let _ = self.tx.send(event);
    }
}

/// A terminal session wrapping alacritty's Term + PTY + EventLoop.
pub struct Session {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub sender: EventLoopSender,
    /// Direct fd to PTY master for low-latency input writes.
    pty_writer: File,
    pub event_rx: mpsc::Receiver<TermEvent>,
    pub title: String,
    pub exited: bool,
    /// Rolling window of recent Wakeup timestamps for burst detection.
    pub wakeup_times: Vec<Instant>,
}

impl Session {
    /// Spawn a new terminal session running the given shell command.
    pub fn new(
        shell: &str,
        args: &[String],
        cols: u16,
        rows: u16,
        working_dir: Option<PathBuf>,
        env: HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();
        let event_proxy = EventProxy { tx: event_tx };

        let config = TermConfig::default();

        let size = TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        let term = Term::new(config, &size, event_proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let pty_config = tty::Options {
            shell: Some(tty::Shell::new(shell.to_string(), args.to_vec())),
            working_directory: working_dir,
            drain_on_exit: true,
            env,
        };

        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 1,
            cell_height: 1,
        };

        // Setup terminal environment (TERM, COLORTERM).
        tty::setup_env();

        let pty = tty::new(&pty_config, window_size, 0)?;

        // Dup the PTY master fd so we can write input directly,
        // bypassing the event loop channel for lower latency.
        let pty_writer = pty.file().try_clone()?;

        let event_loop = EventLoop::new(
            term.clone(),
            event_proxy,
            pty,
            true,  // drain_on_exit
            false, // ref_test
        )?;

        let sender = event_loop.channel();

        // Spawn the PTY I/O thread.
        event_loop.spawn();

        Ok(Session {
            term,
            sender,
            pty_writer,
            event_rx,
            title: format!("{} {}", shell, args.join(" ")),
            exited: false,
            wakeup_times: Vec::new(),
        })
    }

    /// Send raw bytes to the PTY (keyboard input).
    /// Writes directly to the PTY fd for minimal latency.
    ///
    /// The PTY fd is non-blocking (set by alacritty), so we must loop on
    /// WouldBlock to avoid silently dropping data on large writes (e.g. pastes).
    pub fn write(&mut self, data: &[u8]) {
        let mut pos = 0;
        while pos < data.len() {
            match (&self.pty_writer).write(&data[pos..]) {
                Ok(n) => pos += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // PTY buffer full — brief yield then retry.
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }

    /// Notify the PTY of a terminal resize.
    pub fn resize(&self, cols: u16, rows: u16) {
        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 1,
            cell_height: 1,
        };
        let _ = self.sender.send(Msg::Resize(window_size));
        self.term.lock().resize(TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        });
    }
}

/// Simple dimensions struct implementing alacritty's Dimensions trait.
pub struct TermSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl alacritty_terminal::grid::Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}
