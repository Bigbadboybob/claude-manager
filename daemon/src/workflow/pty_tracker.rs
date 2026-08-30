//! PTY-quiet detector + terminal keyboard-mode tracker for daemon-owned
//! sessions (Phase 1 of `doc/daemon-side-workflow-orchestration.md`).
//!
//! When the daemon delivers a workflow activation prompt headlessly (Phase 3)
//! it needs the same two signals the TUI drainer derives today:
//!
//!   1. **Is the PTY quiet?** — the inner program has produced no output for a
//!      quiet window, so a queued prompt body can be written without racing a
//!      mid-stream redraw. The TUI tracks this with a `wakeup_times` ring fed
//!      one timestamp per PTY-output batch (`tui/src/app.rs`), coalesced to one
//!      entry per >=50ms; its readiness predicate (`pending_write_ready`) asks
//!      "no wakeup within `require_quiet`".
//!   2. **What terminal mode is the inner program in?** — bracketed-paste body
//!      framing and the kitty-keyboard Enter encoding are chosen from the LIVE
//!      mode at fire time (`enter_bytes_for_mode`, `format_body_for_delivery`
//!      in `tui/src/app.rs`). The TUI reads it from an alacritty `Term` fed by
//!      the PTY stream (`*session.term.lock().mode()`).
//!
//! The daemon owns the same PTY bytes via [`crate::session::PtyByteFanout`], so
//! it reproduces both signals with the SAME parser (`alacritty_terminal`, kitty
//! keyboard tracking enabled, mirroring `tui/src/session.rs`) and the SAME
//! coalesce/quiet algorithm. The two implementations are pinned equal by a
//! cross-crate fixture test in the TUI crate (`tui/src/app.rs`, module
//! `pty_tracker_parity`).
//!
//! Phase 1 is observation only: nothing here writes to a PTY or mutates run
//! state. Fresh respawn (Phase 2) and activation delivery (Phase 3) build on
//! top of this tracker.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config as TermConfig, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use alacritty_terminal::Term;

use crate::session::PtyByteFanout;

/// Coalesce window for output wakeups. Mirrors `tui/src/app.rs`: alacritty
/// fires one `Wakeup` per PTY-output batch (kHz rates under heavy output), so
/// the TUI records at most one timestamp per 50ms. The daemon coalesces
/// identically — coalescing is NOT just a memory optimization: it shifts the
/// boundary of the quiet check, so the two implementations must use the same
/// interval or `quiet_for` can disagree at the edge.
pub const WAKEUP_COALESCE: Duration = Duration::from_millis(50);

/// Retention window for recorded wakeups. The TUI prunes to its per-session
/// idle window (default 2s); the daemon keeps a generous fixed window so a
/// prune never drops an entry a quiet query still needs. As long as the prune
/// window is >= any queried quiet window, the quiet decision is identical
/// regardless of the exact prune window (pruning only removes entries strictly
/// older than the window, which a quiet check would never count anyway).
pub const DEFAULT_PRUNE_WINDOW: Duration = Duration::from_secs(300);

const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;

/// Record an output wakeup observed at `now`, coalescing to one entry per
/// [`WAKEUP_COALESCE`]. Mirrors the inline recorder in `tui/src/app.rs`
/// (`should_record = last.map_or(true, |l| now - l >= 50ms)`).
pub fn record_wakeup(wakeups: &mut Vec<Instant>, now: Instant) {
    let should_record = wakeups
        .last()
        .map_or(true, |last| now.duration_since(*last) >= WAKEUP_COALESCE);
    if should_record {
        wakeups.push(now);
    }
}

/// Drop wakeups older than `window` relative to `now`. Mirrors the TUI's
/// per-tick `wakeup_times.retain(|t| now - t < idle_window)`.
pub fn prune_wakeups(wakeups: &mut Vec<Instant>, now: Instant, window: Duration) {
    wakeups.retain(|t| now.duration_since(*t) < window);
}

/// Quiet iff no recorded wakeup falls within `window` of `now`. This is the
/// bare quiet predicate the TUI's `pending_write_ready` applies once its
/// floor/deadline gates pass (`!wakeups.iter().any(|t| now - t < require_quiet)`).
pub fn is_quiet(wakeups: &[Instant], window: Duration, now: Instant) -> bool {
    !wakeups.iter().any(|t| now.duration_since(*t) < window)
}

/// Return the byte sequence meaning "Enter" for the inner program's current
/// mode. Mirror of `tui/src/app.rs::enter_bytes_for_mode`: kitty keyboard
/// (DISAMBIGUATE_ESC_CODES) encodes Enter as CSI 13 u, not raw `\r`.
pub fn enter_bytes_for_mode(mode: TermMode) -> &'static [u8] {
    if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        b"\x1b[13u"
    } else {
        b"\r"
    }
}

/// The longest SINGLE-LINE body we are willing to deliver raw (unbracketed).
///
/// Raw delivery exists for slash commands (`/compact`, `/clear`): the agent
/// only runs them as commands when they arrive as TYPED input, so wrapping
/// them in paste markers would drop them into the composer as pasted text.
/// Those are tiny, and this bound keeps them on the raw path.
///
/// Above the bound raw delivery is UNSAFE. codex reads its PTY in 1024-byte
/// chunks; a raw body crossing that boundary is torn in two — the first 1024
/// bytes land as a `[Pasted Content N chars]` chip, the remainder types in
/// behind it, and the trailing Enter is absorbed by the paste state instead
/// of submitting. The prompt then sits in the composer forever while the
/// delivery's quiet-window verification reads the repaint as `submitted=true`.
///
/// Measured against codex 0.150.1 (single-line body, kitty Enter after the
/// standard 1.5s gap): raw ≤1024B submits, ≥1025B never does, with the split
/// landing at exactly 1024 bytes; the same body wrapped in bracketed paste
/// submits at every size tested (to 12KB). 512 keeps a 2x margin under the
/// boundary and is orders of magnitude above any slash command.
///
/// The pre-fix predicate was "multi-line" alone, on the reasoning that real
/// activation prompts always span several lines. Agent-composed prompts do
/// not: a one-paragraph charter written by an orchestrator is a single line,
/// and every one of them over 1024 bytes wedged. That is the "codex randomly
/// doesn't get its Enter pressed" bug — deterministic, but only on prompts
/// that happen to carry no newline.
pub const RAW_SINGLE_LINE_MAX: usize = 512;

/// Whether a delivery body has to be framed as a bracketed paste (given a
/// program that supports one). Multi-line bodies always do — otherwise the
/// agent submits at the first newline. Long single-line bodies do too, per
/// [`RAW_SINGLE_LINE_MAX`].
pub fn body_needs_bracketing(body: &str) -> bool {
    body.contains('\n') || body.len() > RAW_SINGLE_LINE_MAX
}

/// Frame a delivery body for the inner program's current mode. Mirror of
/// `tui/src/app/events.rs::format_body_for_delivery`: wrap in bracketed-paste
/// markers iff [`body_needs_bracketing`] AND the program enabled bracketed
/// paste; short single-line bodies (slash commands) stay raw so they're
/// recognised as typed input.
pub fn format_body_for_delivery(body: &str, mode: TermMode) -> Vec<u8> {
    if body_needs_bracketing(body) && mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = Vec::with_capacity(body.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        body.as_bytes().to_vec()
    }
}

/// Dimensions for the mode-sink `Term`. The terminal mode is independent of
/// the grid size, but `Term::new` requires a `Dimensions`. A tiny fixed grid
/// keeps memory negligible (we never render this `Term`).
struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
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

/// Per-session tracker that reproduces the TUI drainer's delivery inputs from
/// the daemon's own copy of the PTY output stream: terminal mode (via an
/// alacritty `Term`) and output-quiet timing (via a coalesced wakeup ring).
///
/// Drive it by feeding observed output batches through [`feed`](Self::feed)
/// (deterministic-time, used by tests and the parity fixture) or by attaching
/// it to a live [`PtyByteFanout`] via [`spawn_for_fanout`].
pub struct PtyModeTracker {
    term: Term<VoidListener>,
    processor: Processor,
    wakeups: Vec<Instant>,
    prune_window: Duration,
}

impl PtyModeTracker {
    /// New tracker with a default grid and prune window.
    pub fn new() -> Self {
        Self::with_size(DEFAULT_COLS, DEFAULT_ROWS)
    }

    /// New tracker sized to the session's PTY. Size only affects the (unused)
    /// grid; mode tracking is size-independent.
    pub fn with_size(cols: usize, rows: usize) -> Self {
        let mut config = TermConfig::default();
        // REQUIRED for `term.mode()` to ever reflect DISAMBIGUATE_ESC_CODES —
        // alacritty ignores kitty `CSI > Nu` push/pop unless this is set. The
        // TUI enables it for the same reason (`tui/src/session.rs`).
        config.kitty_keyboard = true;
        // We keep transcripts on disk and never render this grid; no scrollback
        // is needed for mode tracking.
        config.scrolling_history = 0;
        let term = Term::new(config, &Dims { cols, rows }, VoidListener);
        Self {
            term,
            processor: Processor::new(),
            wakeups: Vec::new(),
            prune_window: DEFAULT_PRUNE_WINDOW,
        }
    }

    /// Feed one observed PTY-output batch (the bytes pushed to the fanout by a
    /// single reader-thread read), timestamped `at`. Advances the terminal
    /// parser (updating mode) and records a coalesced wakeup. Empty batches are
    /// ignored, mirroring `PtyByteFanout::push`'s early return and the TUI's
    /// "one Wakeup per non-empty output batch".
    pub fn feed(&mut self, bytes: &[u8], at: Instant) {
        if bytes.is_empty() {
            return;
        }
        self.processor.advance(&mut self.term, bytes);
        record_wakeup(&mut self.wakeups, at);
        prune_wakeups(&mut self.wakeups, at, self.prune_window);
    }

    /// Current terminal mode (bracketed-paste / kitty-keyboard bits, etc.).
    pub fn term_mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Enter encoding for the current mode (see [`enter_bytes_for_mode`]).
    pub fn enter_bytes(&self) -> &'static [u8] {
        enter_bytes_for_mode(self.term_mode())
    }

    /// True once the inner program has enabled either of the modes a
    /// kitty-TUI agent (claude-code / codex) turns on at startup —
    /// kitty key disambiguation or bracketed paste. This is the
    /// observable "the composer is up and reading input" signal the
    /// fresh-spawn delivery path waits for instead of a fixed sleep
    /// that a slow cold start outruns.
    pub fn composer_ready(&self) -> bool {
        let m = self.term_mode();
        m.contains(TermMode::DISAMBIGUATE_ESC_CODES) || m.contains(TermMode::BRACKETED_PASTE)
    }

    /// Whether the inner program currently has bracketed paste enabled.
    pub fn bracketed_paste(&self) -> bool {
        self.term_mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Quiet iff no output wakeup falls within `window` of `now`.
    pub fn quiet_for(&self, window: Duration, now: Instant) -> bool {
        is_quiet(&self.wakeups, window, now)
    }

    /// The recorded wakeup timestamps (test/inspection accessor).
    pub fn wakeups(&self) -> &[Instant] {
        &self.wakeups
    }
}

impl Default for PtyModeTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Attach a [`PtyModeTracker`] to a live [`PtyByteFanout`]: subscribe to the
/// session's output, SYNCHRONOUSLY seed the tracker with the fanout's replay
/// (the current ring contents), then feed each subsequent chunk on a background
/// thread (stamping `Instant::now()`). The returned handle is shared with the
/// feeder thread; callers read mode/quiet state through it. The thread exits
/// when the fanout closes (child exit), so the handle stops updating but stays
/// readable.
///
/// The synchronous seed is load-bearing for the delivery drainer. `subscribe()`
/// enqueues the replay inline (before it returns), but draining it on the
/// background thread leaves a window where the drainer's FIRST
/// `advance_finalization` read sees a brand-new tracker: an empty wakeup ring
/// (`quiet_for` falsely reports QUIET) and a default `TermMode` (raw `\r`
/// instead of the live kitty `CSI 13u`). Against a codex reviewer in kitty mode
/// that raw `\r` lands as a literal and the prompt/`/clear` never submits — the
/// same non-submission failure the Phase 2 body/Enter split fixed. Draining the
/// replay here, before returning, guarantees the first read sees the real
/// terminal mode AND a correctly-stamped recent wakeup.
pub fn spawn_for_fanout(
    fanout: &Arc<PtyByteFanout>,
    cols: usize,
    rows: usize,
) -> Arc<Mutex<PtyModeTracker>> {
    let rx = fanout.subscribe();
    let tracker = Arc::new(Mutex::new(PtyModeTracker::with_size(cols, rows)));
    // Seed synchronously: drain every chunk already queued (the replay the
    // subscribe enqueued inline, plus any live push that raced in). `try_recv`
    // is non-blocking, so this returns as soon as the channel is momentarily
    // empty; the feeder thread below picks up everything after.
    {
        let mut t = tracker.lock().unwrap_or_else(|p| p.into_inner());
        while let Ok(chunk) = rx.try_recv() {
            t.feed(&chunk, Instant::now());
        }
    }
    let feeder = Arc::clone(&tracker);
    thread::spawn(move || {
        // Live pushes after the synchronous seed. On `Disconnected` (fanout
        // closed) the loop ends.
        while let Ok(chunk) = rx.recv() {
            let now = Instant::now();
            let mut t = feeder.lock().unwrap_or_else(|p| p.into_inner());
            t.feed(&chunk, now);
        }
    });
    tracker
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First-use race regression: a tracker created over a fanout whose replay
    /// buffer ALREADY carries kitty-mode-enabling output must, on its FIRST read
    /// (no separate feed, no sleep), report the kitty Enter encoding and NOT be
    /// falsely quiet — i.e. `spawn_for_fanout` seeded the replay synchronously
    /// before returning. Deterministic: no timing sleeps; the seed happens
    /// inline, so the lock immediately observes the real mode + a recent wakeup.
    #[test]
    fn spawn_for_fanout_seeds_replay_synchronously() {
        let fanout = Arc::new(PtyByteFanout::new(4096));
        // Pre-load the ring as if the codex reviewer had already enabled kitty
        // keyboard + bracketed paste during startup.
        fanout.push(b"\x1b[?2004h\x1b[>1ustartup banner...");

        let tracker = spawn_for_fanout(&fanout, 80, 24);
        let t = tracker.lock().unwrap_or_else(|p| p.into_inner());

        // FIRST read — real mode captured from the replay, NOT the default.
        assert!(
            t.term_mode().contains(TermMode::DISAMBIGUATE_ESC_CODES),
            "kitty mode must be seeded synchronously"
        );
        assert_eq!(t.enter_bytes(), b"\x1b[13u", "live kitty Enter, not raw \\r");
        assert!(t.bracketed_paste(), "bracketed paste seeded synchronously");
        // And the replay stamped a recent wakeup, so it is NOT falsely quiet.
        assert!(
            !t.quiet_for(Duration::from_secs(2), Instant::now()),
            "replay must stamp a recent wakeup — not falsely quiet on first read"
        );
    }

    /// `composer_ready` is the fresh-spawn delivery gate: false on a raw
    /// PTY (nothing enabled yet — the slow-cold-start window where a paste
    /// + Enter would land in the void), true the moment EITHER startup
    /// mode appears, in either order.
    #[test]
    fn composer_ready_flips_on_either_startup_mode() {
        let base = Instant::now();

        let mut t = PtyModeTracker::new();
        assert!(!t.composer_ready(), "raw PTY: composer not ready");
        t.feed(b"codex booting...\r\n", base);
        assert!(!t.composer_ready(), "plain output alone must not arm it");
        t.feed(b"\x1b[?2004h", base + Duration::from_millis(50));
        assert!(t.composer_ready(), "bracketed paste alone suffices");

        let mut t2 = PtyModeTracker::new();
        t2.feed(b"\x1b[>1u", base);
        assert!(t2.composer_ready(), "kitty disambiguation alone suffices");
    }

    /// `\x1b[?2004h` enables bracketed paste; `\x1b[>1u` pushes the kitty
    /// keyboard mode whose flag bit 1 is DISAMBIGUATE_ESC_CODES.
    #[test]
    fn tracks_bracketed_paste_and_kitty_set_reset() {
        let mut t = PtyModeTracker::new();
        let base = Instant::now();

        assert!(!t.bracketed_paste());
        assert_eq!(t.enter_bytes(), b"\r");

        t.feed(b"\x1b[?2004h\x1b[>1uhello", base);
        assert!(t.bracketed_paste());
        assert!(t.term_mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        assert_eq!(t.enter_bytes(), b"\x1b[13u");

        // Pop kitty + disable bracketed paste.
        t.feed(b"\x1b[<u\x1b[?2004l", base + Duration::from_millis(100));
        assert!(!t.bracketed_paste());
        assert!(!t.term_mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        assert_eq!(t.enter_bytes(), b"\r");
    }

    /// A control sequence split across two feeds must still be parsed — the
    /// `Processor` is stateful across `advance` calls, exactly as the TUI's
    /// alacritty `EventLoop` is across PTY reads.
    #[test]
    fn parses_escape_sequence_split_across_batches() {
        let mut t = PtyModeTracker::new();
        let base = Instant::now();
        t.feed(b"\x1b[", base);
        assert!(!t.bracketed_paste());
        t.feed(b"?2004h", base + Duration::from_millis(10));
        assert!(t.bracketed_paste());
    }

    #[test]
    fn quiet_after_window_passes_and_noisy_within_it() {
        let mut t = PtyModeTracker::new();
        let base = Instant::now();
        t.feed(b"output", base);
        // 0.5s later, still within a 1s quiet window -> not quiet.
        assert!(!t.quiet_for(Duration::from_secs(1), base + Duration::from_millis(500)));
        // 1.5s later, outside the 1s window -> quiet.
        assert!(t.quiet_for(Duration::from_secs(1), base + Duration::from_millis(1500)));
    }

    #[test]
    fn coalesces_sub_50ms_bursts() {
        let mut t = PtyModeTracker::new();
        let base = Instant::now();
        t.feed(b"a", base);
        t.feed(b"b", base + Duration::from_millis(20));
        t.feed(b"c", base + Duration::from_millis(40));
        // All three within one 50ms window of the first -> a single wakeup.
        assert_eq!(t.wakeups().len(), 1);
        t.feed(b"d", base + Duration::from_millis(60));
        assert_eq!(t.wakeups().len(), 2);
    }

    #[test]
    fn empty_batch_is_ignored() {
        let mut t = PtyModeTracker::new();
        let base = Instant::now();
        t.feed(b"", base);
        assert!(t.wakeups().is_empty());
        assert!(t.quiet_for(Duration::from_secs(1), base));
    }

    /// The bound must stay clear of codex's 1024-byte PTY read chunk: a raw
    /// single-line body that crosses it is split (first 1024 bytes become a
    /// paste chip) and its trailing Enter is swallowed. Measured on codex
    /// 0.150.1 — raw 1024B submits, raw 1025B does not.
    #[test]
    fn raw_single_line_max_stays_under_the_codex_read_boundary() {
        assert!(
            RAW_SINGLE_LINE_MAX < 1024,
            "a raw single-line body must not be able to reach codex's \
             1024-byte read boundary (got {})",
            RAW_SINGLE_LINE_MAX,
        );
    }

    /// Slash commands are why the raw path exists — they only run as
    /// commands when the agent sees them as TYPED input.
    #[test]
    fn short_single_line_body_stays_raw() {
        assert!(!body_needs_bracketing("/compact"));
        assert!(!body_needs_bracketing("/clear"));
        assert!(!body_needs_bracketing("yes, go ahead with option B"));
        let mode = TermMode::BRACKETED_PASTE;
        assert_eq!(format_body_for_delivery("/compact", mode), b"/compact");
    }

    /// The regression this bound exists for: an agent-composed one-paragraph
    /// prompt carries no newline, so the pre-fix multi-line-only predicate
    /// sent it raw and codex never submitted it.
    #[test]
    fn long_single_line_body_is_bracketed() {
        let body = "x".repeat(RAW_SINGLE_LINE_MAX + 1);
        assert!(
            body_needs_bracketing(&body),
            "a single-line body over the raw bound must be bracketed",
        );
        let out = format_body_for_delivery(&body, TermMode::BRACKETED_PASTE);
        assert!(out.starts_with(b"\x1b[200~") && out.ends_with(b"\x1b[201~"));
        assert_eq!(out, format!("\x1b[200~{}\x1b[201~", body).as_bytes());
        // Exactly at the bound it is still a "typed" body.
        assert!(!body_needs_bracketing(&"x".repeat(RAW_SINGLE_LINE_MAX)));
    }

    /// A body the agent can't accept as a paste still goes raw — we must
    /// never emit literal `[200~` into a composer.
    #[test]
    fn long_single_line_body_raw_when_bracketed_paste_disabled() {
        let body = "x".repeat(RAW_SINGLE_LINE_MAX + 1);
        let out = format_body_for_delivery(&body, TermMode::DISAMBIGUATE_ESC_CODES);
        assert_eq!(out, body.as_bytes());
    }
}
