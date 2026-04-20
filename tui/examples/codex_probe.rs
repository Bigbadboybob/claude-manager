//! Reproduce the TUI's codex-spawning path using alacritty_terminal directly,
//! send various Enter-byte encodings, and check whether codex actually produced
//! a rollout containing our test prompt. This isolates whether the problem is
//! in the PTY setup (alacritty-specific termios) or elsewhere.
//!
//! Run with:
//!   cargo run --example codex_probe -- [encoding_name]
//!
//! Encoding names: raw_cr raw_lf raw_crlf kitty_enter kitty_alt kitty_meta
//!                 kitty_shift kitty_ctrl ss3_m

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::EventLoop;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::tty;
use alacritty_terminal::Term;

use std::sync::mpsc;

#[derive(Clone)]
struct EventProxy {
    tx: mpsc::Sender<TermEvent>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: TermEvent) {
        let _ = self.tx.send(event);
    }
}

struct TermSize {
    cols: usize,
    rows: usize,
}
impl alacritty_terminal::grid::Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

fn list_codex_sessions() -> Vec<PathBuf> {
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h),
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let root = home.join(".codex/sessions");
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    out.push(p);
                }
            }
        }
    }
    walk(&root, &mut out);
    out
}

fn encoding_bytes(name: &str) -> Option<&'static [u8]> {
    Some(match name {
        "raw_cr" => b"\r".as_slice(),
        "raw_lf" => b"\n".as_slice(),
        "raw_crlf" => b"\r\n".as_slice(),
        "kitty_enter" => b"\x1b[13u".as_slice(),
        "kitty_alt" => b"\x1b[13;3u".as_slice(),
        "kitty_meta" => b"\x1b[13;9u".as_slice(),
        "kitty_shift" => b"\x1b[13;2u".as_slice(),
        "kitty_ctrl" => b"\x1b[13;5u".as_slice(),
        "ss3_m" => b"\x1bOM".as_slice(),
        _ => return None,
    })
}

fn probe(encoding_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let enter = encoding_bytes(encoding_name).ok_or("unknown encoding")?;
    let prompt = "please respond PING123";

    let before: std::collections::HashSet<_> = list_codex_sessions().into_iter().collect();

    let work = tempfile::tempdir()?;
    let cwd = work.path().to_path_buf();

    // Build args mirroring workflow::spawn::codex_args
    let args: Vec<String> = vec![
        "--dangerously-bypass-approvals-and-sandbox".into(),
    ];

    // Set up alacritty Term + PTY, exactly like Session::new
    let cols: u16 = 120;
    let rows: u16 = 40;
    let (tx, rx) = mpsc::channel();
    let proxy = EventProxy { tx };
    let config = TermConfig::default();
    let size = TermSize {
        cols: cols as usize,
        rows: rows as usize,
    };
    let term = Term::new(config, &size, proxy.clone());
    let term = Arc::new(FairMutex::new(term));

    // Inherit parent env so codex sees TERM, PATH, HOME, LOGNAME, etc. —
    // matches how pexpect (and a login shell) would spawn it.
    let env: HashMap<String, String> = std::env::vars().collect();
    let pty_config = tty::Options {
        shell: Some(tty::Shell::new("codex".to_string(), args)),
        working_directory: Some(cwd.clone()),
        drain_on_exit: true,
        env,
    };
    let window_size = WindowSize {
        num_lines: rows,
        num_cols: cols,
        cell_width: 1,
        cell_height: 1,
    };
    tty::setup_env();
    let pty = tty::new(&pty_config, window_size, 0)?;
    let mut pty_writer = pty.file().try_clone()?;
    let event_loop = EventLoop::new(term.clone(), proxy, pty, true, false)?;
    event_loop.spawn();

    // Drain events briefly
    let drain = |how_long: Duration| {
        let deadline = Instant::now() + how_long;
        while Instant::now() < deadline {
            while let Ok(ev) = rx.try_recv() {
                let _ = ev;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    };

    // Give codex time to draw its trust prompt
    drain(Duration::from_secs(6));

    // Dismiss the trust prompt: "1\r" selects "Yes, continue". Skip if
    // PROBE_SKIP_TRUST=1 (to test config-based trust).
    if std::env::var_os("PROBE_SKIP_TRUST").is_none() {
        pty_writer.write_all(b"1\r")?;
        drain(Duration::from_secs(10));
    } else {
        drain(Duration::from_secs(10));
    }

    // Optional: simulate the TUI's fresh-context /clear-first flow.
    if std::env::var_os("PROBE_SEND_CLEAR").is_some() {
        pty_writer.write_all(b"/clear")?;
        std::thread::sleep(Duration::from_millis(50));
        pty_writer.write_all(enter)?;
        drain(Duration::from_secs(5));
    }

    // Now send the test prompt text — body and enter as SEPARATE writes so
    // codex treats them as keystrokes (not a paste). 50ms pause between is
    // generous; empirically even 0ms works with the separate-syscall rule.
    pty_writer.write_all(prompt.as_bytes())?;
    std::thread::sleep(Duration::from_millis(50));
    pty_writer.write_all(enter)?;

    // Wait for codex to respond (produces a rollout)
    drain(Duration::from_secs(35));

    // Check if a new rollout matches
    let after: Vec<PathBuf> = list_codex_sessions();
    let new_rollouts: Vec<&PathBuf> = after.iter().filter(|p| !before.contains(*p)).collect();

    let matching: Vec<&PathBuf> = new_rollouts
        .iter()
        .filter(|p| {
            fs::read_to_string(p)
                .map(|c| c.contains(prompt))
                .unwrap_or(false)
        })
        .copied()
        .collect();

    println!(
        "encoding={} new_rollouts={} matching={}",
        encoding_name,
        new_rollouts.len(),
        matching.len()
    );
    for m in &matching {
        println!("  matched: {}", m.display());
    }
    if matching.is_empty() {
        // Also print last bit of Term screen for debugging
        let term = term.lock();
        use alacritty_terminal::grid::Dimensions;
        let rows = term.screen_lines();
        let cols = term.columns();
        let mut screen = String::new();
        for r in (rows.saturating_sub(10))..rows {
            for c in 0..cols {
                let cell = &term.grid()[alacritty_terminal::index::Point::new(
                    alacritty_terminal::index::Line(r as i32),
                    alacritty_terminal::index::Column(c),
                )];
                screen.push(cell.c);
            }
            screen.push('\n');
        }
        println!("--- last 10 rows of term screen ---");
        println!("{}", screen);
    }

    if matching.is_empty() {
        Err("no rollout containing prompt".into())
    } else {
        Ok(())
    }
}

fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "raw_cr".into());
    match probe(&name) {
        Ok(()) => {
            println!("OK");
            std::process::exit(0);
        }
        Err(e) => {
            println!("FAIL: {}", e);
            std::process::exit(1);
        }
    }
}
