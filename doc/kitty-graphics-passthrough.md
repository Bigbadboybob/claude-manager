# Kitty graphics in session panes

Programs in a CM pane (snacks.nvim, image.nvim, `kitten icat
--unicode-placeholder`, yazi) can show images when the laptop's terminal is
kitty or ghostty. CM supports the kitty graphics protocol's **Unicode
placeholder** mode only. Direct placements (`a=p`/`a=T` without `U=1`) are not
drawn; see [Limits](#limits).

## Why panes needed work

Each pane is parsed by `alacritty_terminal` and repainted as a text grid by
Ratatui. Alacritty drops APC strings (`ESC _ G … ESC \`), never answers
`CSI > q` (XTVERSION) or `CSI 16 t`, and the TUI discarded `CSI 14 t`. The
daemon also set the pane PTY's pixel size to zero. snacks.nvim therefore saw
an unknown terminal with a 0×0-pixel window.

## Design

Placeholder mode puts each image into ordinary text cells: U+10EEEE, with
combining diacritics for row and column and the image id in the foreground
colour. The outer terminal draws the image into whichever cells carry that
id. The TUI therefore does not track pixel positions. Scrolling, occlusion by
the sidebar, pane switching and scrollback follow the grid automatically.

1. **Outer terminal detection** (`tui/src/graphics/outer.rs`). At startup,
   while raw mode is on and before the alternate screen is entered, the TUI
   probes only when the environment hints at kitty or ghostty (`TERM`,
   `KITTY_WINDOW_ID`, `TERM_PROGRAM`, `GHOSTTY_RESOURCES_DIR`). It sends a
   graphics query (`a=q`), XTVERSION, `CSI 16 t` and a DA1 sentinel,
   then reads the replies for up to 300 ms. Passthrough is enabled only when
   the graphics query answers `OK` and XTVERSION names kitty or ghostty,
   because only those terminals implement placeholders. It stays off inside
   tmux or screen. `CM_KITTY_GRAPHICS=0` disables detection; `=1` forces
   passthrough on. The cell pixel size comes from `TIOCGWINSZ` on stdout and
   falls back to the `CSI 16 t` reply. It is re-read on every resize.
2. **Stream interception** (`tui/src/graphics/filter.rs`). A byte-level state
   machine in the attach reader (`attached_pty.rs::ReaderHalf`) removes
   `ESC _ G … ESC \` before Alacritty's parser sees it. It handles sequences
   split across reads and caps a single command at 64 MiB. It also observes
   the queries `CSI 14 t`, `CSI 16 t` and `CSI > q` and passes their bytes
   through unchanged. Commands and queries go over a channel to the pane's
   `PaneGraphics` on the UI thread. Each attach starts with the daemon
   replaying its output ring, and the attach reply's `replay_bytes` says how
   long that replay is. Events inside it are marked `replay`. Replayed
   image commands are re-sent, which restores images after a reconnect when
   they still fit in the ring. Replayed queries are not answered: an answer
   typed into a shell prompt would appear as garbage.
3. **Rewriting** (`tui/src/graphics/pane.rs`). The outer terminal has a single
   id space. Each pane therefore maps inner image ids (`i=`, plus `I=` image
   numbers and `P=` parent ids) to TUI-allocated outer ids. Outer ids are
   24-bit, so the foreground colour alone can encode them. Every forwarded
   command gets `q=2`, because the outer terminal's replies would arrive on
   the TUI's stdin, and crossterm cannot parse APC. The pane synthesises the
   replies the program expects (`OK` or an error) using its original ids and
   honouring its `q=` setting. Chunked uploads (`m=1`) are accumulated per
   pane and forwarded contiguously when the last chunk arrives. This keeps
   one pane's upload from interleaving with another pane's commands.
   A new command other than a bare continuation (`m`/`q` only) abandons an
   unfinished upload whose program died mid-transfer. `d=a`/`d=A` deletes expand to per-image deletes of this pane's images;
   positional deletes are dropped. All outer output goes through one queue.
   The main loop flushes it to stdout between frames, never during a draw.
4. **File-based transmission.** A session on a cloud host names files on
   that host (`t=f`, `t=t`, `t=s`), which the laptop's terminal cannot read.
   snacks.nvim uses `t=f` unless it sees `SSH_CONNECTION`, and daemon
   sessions do not have that variable. The pane fetches the bytes through the
   operator RPC `graphics.read_file` on the session's own daemon. The RPC
   accepts regular files only, honours `O=`/`S=`, is capped at 32 MiB, and
   replies in 2 MiB pages, because control frames are capped at 4 MiB. It
   deletes temporary files and shared memory, as kitty does, once the last
   page has been read. The pane then
   forwards the bytes in-band (`t=d`). Later commands from that pane wait
   behind the fetch, so their order is preserved.
5. **Query replies.** With passthrough enabled, the pane answers `CSI 14 t`,
   `CSI 16 t`, XTVERSION (the outer terminal's own reply, so snacks detects
   kitty or ghostty) and the graphics `a=q` probe. The TUI sends the cell
   pixel size in each resize (`WindowSize.cell_width/height`). The daemon
   applies `pixel_width`/`pixel_height` to the PTY's `TIOCSWINSZ`. When a
   resize carries no pixel size (older viewers, the `session.resize` repair
   RPC), the daemon reuses the last cell size it received for that session.
6. **Placeholder cells** (`tui/src/terminal_widget.rs`). For a U+10EEEE cell,
   the widget emits the character with its row and column diacritics, which
   Alacritty stores as zero-width characters. It rewrites the colour-encoded
   inner id (24-bit RGB or a 256-colour index, plus an optional third
   diacritic for the high byte) to the outer id as truecolour. It keeps the
   underline colour, which carries snacks' placement id. A placeholder whose
   id this pane never transmitted renders as a blank cell. It can never
   display another pane's image. With passthrough on, panes that have no
   image map (local PTYs, the planning editor) blank every placeholder.
7. **Cleanup.** Dropping a pane's `PaneGraphics` queues `a=d,d=I` for each of
   its images. The queue is flushed again when the TUI quits. This happens
   when the pane is closed, the session is removed,
   or a reconnect replaces the attach stream. Hidden panes keep their images
   in the outer terminal's memory, but no cells reference them, so nothing is
   drawn.

Without passthrough (a non-graphics terminal, tmux, or `CM_KITTY_GRAPHICS=0`),
no filter is installed and no queries are answered. Behaviour is unchanged.

## Limits

- Direct placements (`a=p` or `a=T` without `U=1`) are not drawn: `a=T` is
  reduced to a transmit, and `a=p` is refused with `ENOTSUP`. Drawing them
  would need cursor-to-pane coordinate translation and clipping against the
  sidebar and other panes. Positional deletes (`d=c/p/q/x/y/z`) are dropped
  for the same reason. wezterm lacks placeholders, so it is not enabled.
- Errors from the outer terminal (for example a corrupt PNG) are not
  reported back to the program, because its replies are suppressed.
- After a reconnect, only images whose upload is still in the daemon's 1 MiB
  replay ring come back. Others reappear when the program redraws them. In
  nvim, `:e` does this.
- Truly local PTYs (`Session::new`: the backtest watch pane and the planning
  editor) are not intercepted.
- The session's daemon needs a brain with this change: it sets the pixel
  size, reports `replay_bytes`, and serves `graphics.read_file`. An older
  daemon ignores the extra resize fields, so programs see 0×0 pixels. Its
  attach reply has no replay length, so the viewer answers no queries at
  all (it cannot tell replay from live output), and snacks.nvim does not
  detect kitty. Image commands are still forwarded.

## Verification

Tests (run only these targets):

```bash
export CARGO_TARGET_DIR="$HOME/.cm/builds/kitty-graphics"
# Filter state machine, id rewriting, replies, probe parsing, reader tap.
scripts/cm-test-isolated cargo test -p claude-manager-tui graphics::
# Placeholder cells through Alacritty, Ratatui and crossterm.
scripts/cm-test-isolated cargo test -p claude-manager-tui kitty_placeholder
# A program in a real daemon session probes, reads TIOCGWINSZ, sends t=f.
scripts/cm-test-isolated cargo test -p claude-manager-tui kitty_graphics_end_to_end
# Daemon: pixel sizes, replay length, graphics.read_file.
scripts/cm-test-isolated cargo test -p cm-daemon --lib graphics_
```

Manual check in kitty or ghostty: open `~/nvim-image-test/test.md` on
cm-sessions in `nvim`. `:checkhealth snacks` should show non-zero pixel
dimensions and a detected kitty or ghostty. Run
`kitten icat --unicode-placeholder <png>` in a second pane, and confirm that
closing either pane removes only its own images.
