# Account usage view

F5 opens the full-screen Usage view from any view; F5, Esc or q returns to the
previous view. The panel contacts the `manager` host only while open and refreshes
every 60 seconds. Set the top-level `usage_host = "manager"` in
`~/.cm/tui-settings.toml` to use another configured host, then restart the TUI.
The target must run the account hub and the daemon RPC; the header shows the
data age and any host or hub error.

Tab or 1–2 selects Accounts or Tokens. All percentages and bars mean **used**
capacity. In Accounts, j/k or arrows select an account and g/G moves to the
first or last. Weekly bars are the main view; x expands or collapses full 5h
bars. On screens narrower than 100 columns, Enter opens the focused account's
full-width detail and Enter or Backspace returns to the list. Page Up/Down scrolls
the detail, and ? toggles key help. The detail includes limits, reset times,
credits, projections, hosts, a seven-day chart and account token totals. Codex
accounts in multiple pools show ×N in the list and each pool's status in detail.

Green is below 50% used, yellow is 50–80%, orange is 80% to the switch threshold,
and bold red is at or above the threshold. Grey and ◌ mean unknown or stale:
readings older than ten minutes, or past their reset time, are stale. The list
shows the weekly reset countdown (`r:`). A live 5h limit has a small glyph and
percentage in the list. ● means active and ✕ means no room. ⚡ is a Claude Fable reset; ⟲ is a Claude full reset or Codex credit.
The number is the reported count; ? means unknown. For Claude, ! means none of
that type is usable now; a count may include unusable grants even if some are
usable. Zero counts are omitted from the list but shown in details. Reset-credit
timestamps show their age; they are not necessarily live counts. Codex room is inferred from active status and all
available limits being below their switch thresholds. If the hub has not supplied
Codex window durations, those windows are labelled unknown rather than guessed.
The week outlook is shown when the hub supplies it.

Tokens shows four total windows followed by a chart. h/d selects hourly buckets
over seven days or daily buckets over 30 days; e cycles engines, g groups by
account or host, and j/k scrolls. Solid bars show input plus output and shaded
bars show cache read plus write.
