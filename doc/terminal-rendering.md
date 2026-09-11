# Embedded terminal rendering

CM parses session output into an Alacritty grid, renders that grid into Ratatui
cells, and writes incremental buffer differences through the Crossterm backend.
The grid is already a display model: its cell contents must not be replayed as
terminal commands.

## Cloud sessions appearing monochrome

PTY children receive `TERM=xterm-256color` and `COLORTERM=truecolor` from both
daemon spawn paths, including the holder/brain split. These describe CM's
embedded terminal, independent of the service's environment: systemd normally
supplies neither variable, and noninteractive SSH can supply `TERM=dumb`.
Without them, applications such as Claude can detect no color support even
though the viewer renders colors correctly. The old local Alacritty spawn
path already initialized its terminal environment.

Explicit per-session `TERM`/`COLORTERM` values win over these defaults. `NO_COLOR`
and other application preferences are preserved; CM does not set `FORCE_COLOR`.
This applies to fresh agent/shell/workflow/continuous launches and normal resumes.

Deploy with a brain-only restart; no holder or laptop TUI update is needed.
Running agents keep their existing environment and conversation. They pick up
the defaults on their next normal restart/resume, not by reconnecting the viewer.
Do not interrupt active agents or restart scheduler-owned continuous sessions
just to change colors.

Focused regressions inspect real holder-launched children and exercise `tput`
color detection on both spawn paths, including explicit monochrome overrides:

```bash
CARGO_TARGET_DIR="$HOME/.cm/builds/cloud-terminal-colors" CARGO_BUILD_JOBS=2 \
  scripts/cm-test-isolated cargo test --locked -p cm-daemon \
  --test pty_environment --test holder_mode_e2e pty_environment -- --test-threads=1
```

## Tabs overwriting the sidebar

Alacritty retains a literal tab in the starting cell when advancing to a tab
stop. This preserves tabs for text extraction. Passing that character straight
through `TerminalWidget` makes the laptop terminal advance its cursor again.
Crossterm assumes adjacent buffer updates advance by one column, so the next
changed cells can land beyond the terminal pane and overwrite sidebar borders,
text, and section shading. The unchanged sidebar is absent from later buffer
differences, leaving persistent gaps until it is redrawn.

`TerminalWidget` renders grid control characters as single spaces, keeping
their existing colors and modifiers. The original Alacritty grid, text
selection, PTY output, and agent processes are unaffected. There is no extra
full-screen repaint or network traffic.

The regression tests feed tabbed output into an inner Alacritty terminal,
render actual Ratatui differences through Crossterm, and interpret those bytes
in a second terminal. They check sidebar characters/backgrounds and pane text
across consecutive updates and different pane offsets. A scrollback test checks
tab styling and preservation of the source grid.

Run them with a private target through `scripts/cm-test-isolated`:

```bash
CARGO_TARGET_DIR="$HOME/.cm/builds/terminal-rendering" CARGO_BUILD_JOBS=4 \
  scripts/cm-test-isolated cargo test --locked -p claude-manager-tui terminal_widget::tests
```

Activation requires only the [laptop TUI release](TUI_RELEASES.md) and viewer
relaunch. A viewer window resize repaints existing outer-screen damage as a
temporary workaround. Do not use a session restart to repair sidebar artifacts.

## Missing conversation after reconnect

The daemon retains a 1 MiB tail of raw PTY bytes. A long-lived session can fill
that tail entirely with incremental screen updates, evicting the original
screen those updates depended on. Replaying it into a fresh terminal can leave
the upper conversation blank even while new messages and input work. The agent's
saved transcript is separate from this display buffer.

Existing Codex panes now request a repaint the first time they are viewed after
attachment or reconnect. The viewer briefly changes the PTY width by one column
and restores the current viewport size 400 ms later. Codex rebuilds its history
and composer in response. The viewer ticks drive this asynchronously: no input,
agent restart, blocking delay, or extra control RPC is involved. Hidden panes
defer the work until viewed. A size restoration still completes if focus moves,
and a user window resize during recovery takes precedence over the old size.

**Alt+r** also requests this repaint for the focused Codex session, alongside
the existing planning refresh, reconnect nudge, and outer-screen clear.
**Alt+Shift+r** retains its distinct agent restart/revive behavior.

Shells are not automatically repainted: a shell cannot recreate old command
output on resize. This recovery also does not turn the terminal into an unlimited
transcript viewer; its 1,500-line scrollback limit still applies. Authoritative
terminal snapshots and longer history access remain separate work.

The regression uses a real daemon/PTY and a deterministic repaintable application.
It evicts the first screen with more than 1 MiB of incremental updates, verifies
the missing header after attachment, and recovers it without changing the child
PID or sending input. It also covers hidden panes, focus changes, a concurrent
window resize, repeated focus, and shell attachments. Run the `codex_repaint`
test filter through the isolated runner above.
