# Embedded terminal rendering

CM parses session output into an Alacritty grid, renders that grid into Ratatui
cells, and writes incremental buffer differences through the Crossterm backend.
The grid is already a display model: its cell contents must not be replayed as
terminal commands.

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
