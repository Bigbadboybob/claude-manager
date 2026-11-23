# Account usage panel

F5 opens Usage from any view; F5, Esc or q closes it. The panel contacts the
`manager` host only while open and refreshes every 60 seconds. Set the top-level
`usage_host = "manager"` in `~/.cm/tui-settings.toml` to use another configured
host, then restart the TUI. The target must run the account hub and the new daemon
RPC; host connection and hub errors appear in the panel. The displayed age is
based on the hub's `generated_at` timestamp.

Tab or 1–3 selects Accounts, Tokens or History. Accounts lists each limit and
reset, host, reservation and weekly history; j/k scrolls. Codex room is inferred
from an active status and all available limits being below 98%; a missing status
or limits displays unknown. Tokens shows the four totals windows, then a chart:
h/d selects hourly or daily buckets, e cycles engines, g groups by account or
host, and j/k scrolls. Solid bars show input plus output and shaded bars show
cache read plus write. History uses j/k to choose an account and plots its limit
readings over seven days, with the 98% switch line. Page Up and Page Down scroll on every tab.
