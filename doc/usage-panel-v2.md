# Usage panel v2: full-screen, colour, resets, projections (design, 2026-11-23 ~20:05Z)

Owner installed tui-cb19226 and found the F5 overlay hard to read. Owner's words (11-23 ~20:00Z):

> make it its own screen like the f6 org chart … use colors … wider bars for usage … I assume 100% means 100% used? Definitely a colored icon and count for resets, both types. Codex LB has a very readable interface. … the weekly usage bar should be the main thing I see and then 5 hour can be a smaller visual that could be expanded … navigate with j/k and when you focus that account it shows the full details. … Overall projections for the week and colors for different usage levels is good too.

Reference: Codex LB's Accounts page (list on the left with a weekly bar, reset countdown and status badge per account; the selected account's detail on the right: usage bars, "Usage resets: N available", request totals, a 7-day trend chart of 5h and weekly).

Lead: Claude-Accounts. Contract between the hub worker (usage-p1, Python, `scripts/cm-claude-accounts`) and the TUI worker (usage-p23, `tui/src/app/usage.rs`). Schema stays `1`: every addition below is optional and additive; the TUI must render without any of them (older hub). Nothing existing is renamed or removed. `hub status --json` is unchanged.

## A. Snapshot additions (hub worker)

All percents are **used** (0-100). The TUI labels them "used".

1. `accounts[].limits[]` gain:
   - `window_minutes` (int|null): Claude `five_hour` → 300, `weekly_*` → 10080. Codex from codex-lb `usage_history.window_minutes` (host-side query change: select that column too). On a Pro Codex plan the `primary` window is 10080 (weekly), so the TUI must label Codex limits by `window_minutes`, never by primary/secondary.
   - `switch_at` (float): the hub's switch threshold for that limit and account (98, or the account override such as 94 for bigbadbossbob). Codex: 100 unless a policy exists.
2. `accounts[].resets`: list of `{type, count, expires_at, as_of}`; Claude entries also have `usable_now`. Absent or `null` = unknown, `[]` = none.
   - `type` `claude_fable` (grant `gives_back_weekly` true or `fable` in its ID), `claude_full` (ID begins `full-reset`), `codex` (Codex LB reset credits). Fable takes precedence. Claude `count` sums remaining credits regardless of usability; `usable_now` is true if any grant with credits is usable, and `expires_at` is the nearest expiry among grants with credits.
   - Claude: hub-side only. A cache file `hub/reset-grants.json` filled by a read-only sweep of **pool** accounts (the same read as `~/.local/share/cm-claude-accounts/claim-reset.py status EMAIL`: `GET /api/oauth/usage?cedar_ember=1&skip_spend=1` with the CLI User-Agent; parse `cedar_ember.grants[]`: `id`, `gives_back_weekly`, `resets_left`, `ends_at`, `usable_now`). At most once per 6 h per account, from the hub check-in path but never blocking it (five-second request timeout, 15-second sweep budget, errors ignored), and never refreshing a login just for this; skip accounts whose access token is expired. Accounts lent to a host keep their last cached value; `as_of` says how old. Add `hub resets [--refresh] [--json]` to show/refresh it, and make `claim-reset.py claim` refresh that account's cache entry afterwards (small edit there). Never log or print tokens.
   - Codex: host side. The collector reads the host's local Codex LB API `GET http://127.0.0.1:2455/api/accounts` (1 s timeout, optional, configurable/disable-able like `codex_lb_store`), and copies **only** `accountId`, `availableResetCredits`, `resetCreditNearestExpiresAt` per account (the response also carries `auth.*` objects: never copy, store or log them). The hub emits `{type:"codex", count:availableResetCredits, expires_at:<nearest expiry>, as_of:<check-in time>}`.
3. `accounts[].projection` (or null): from the hub's own `limit_readings` history for the account's binding **weekly** limit in its current reset window:
   `{kind, rate_per_hour, basis_hours, at_switch (epoch|null: when it reaches switch_at at that rate, null if not before reset or rate≈0), at_reset_percent (projected used % at that limit's reset, capped at 100)}`. Use the last ~6 h of readings in the same window (fewer if the window is younger); null with fewer than 2 readings or under 30 min of history. Rate is only meaningful while the account is in use; that's fine.
4. Top-level `outlook`: one entry per engine:
   `{engine, accounts, accounts_with_room, room_weeks, burn_per_hour, dry_at, refills}`
   - `room_weeks`: sum over accounts with room of `max(switch_at - weekly_used, 0) / 100` (how many full account-weeks are left).
   - `burn_per_hour`: sum of `rate_per_hour / 100` over accounts currently active on a host (Claude: `active_on` non-empty; Codex: status active with a rate).
   - `dry_at`: when `room_weeks` runs out at `burn_per_hour`, counting the refills below as they happen (simple step simulation over the next 7 days), or null if it never does within 7 days.
   - `refills`: the next 7 days of weekly resets, `[{at, email, frees}]` where `frees` = used share returned (percent/100), sorted by time.

Tests for each, fixture JSON updated (`tests/fixtures/usage-dashboard.json` and `tui/tests/fixtures/usage-snapshot.json` must stay in sync), docs in `doc/usage-dashboard.md`.

## B. Full-screen Usage view (TUI worker)

F5 opens a **full-screen view** drawn like the F6 org chart (its own screen, not an overlay box; Esc/F5/q return to where you were). Same `usage.snapshot` RPC and 60 s refresh while open; the header shows data age and any error.

Layout (wide terminal; degrade sensibly below ~100 columns by hiding the detail pane, which Enter then opens full-width):

```
 Usage · Claude 7/19 with room · Codex 2/4 with room                 updated 40s ago · ? keys
 ┌ Week outlook ───────────────────────────────────────────────────────────────────────────────┐
 │ Claude  2.6 account-weeks left · burning 0.45/h on 3 hosts · runs dry Tue 14:00Z (in 1d 18h)│
 │         refills: Mon 11:00Z outlook +98% · Tue 08:00Z bender4 +96% · …                       │
 │ Codex   1.9 left · burning 0.04/h · lasts past next refill                                   │
 └─────────────────────────────────────────────────────────────────────────────────────────────┘
 CLAUDE                                    used │ bender0@kleinatc.com          ● f-sessions
▸ bender0     ██████████░░░░░░░░░░░░░░  35% ▂5h │ Week   ███████████░░░░░░░░░░░░░░░░░░░░░  35% used
  bender2     ███████░░░░░░░░░░░░░░░░░  27%     │        resets Sat 08:00Z (in 4d 12h) · switch at 98%
  bender5     ████████████████████░░░░  70% ▅5h │ 5h     ██████░░░░░░░░░░░░ 30% · resets 21:10Z
  outlook     ████████████████████████  98% ✕   │ Fable  █░░░ 7%
  …                                             │ Resets ⚡ Fable ×1 (until 2027-03-09) · ⟲ Full ×1 (until 12-10)
 CODEX                                          │ Pace   2.1%/h → 98% Wed 04:00Z, before its reset ⚠
  bender2     ░░░░░░░░░░░░░░░░░░░░░░░░   2%     │ Hosts  active on f-sessions · reserved: —
  …                                             │ 7-day  [chart: weekly line + 5h line + 98% switch line]
                                                │ Tokens 24h 1.2M in / 0.3M out · 7d … · 30d … · all …
```

1. **List (left):** sections CLAUDE and CODEX. One row per account: short name (alias or email local part), a **wide weekly bar** (the main visual; as wide as the pane allows, at least 20 cells), weekly % used, a small 5h indicator (one-cell level glyph `▁▂▃▄▅▆▇█` plus % only when the 5h window is live), reset-credit icons with counts (see 4), and markers: `●` active on a host, `✕` no room, `◌` stale/unread. Sort: in use first, then most weekly room. `j/k`/arrows move; `g/G` top/bottom; `x` expands every row to show a full 5h bar under the weekly bar (toggle, remembered while open).
2. **Detail (right):** for the focused account everything we know: weekly bar with "used", reset time + countdown, switch threshold; 5h bar; scoped limits (Fable etc.) as smaller bars; resets with expiry; pace/projection; hosts using it and reservation/note; Codex status/plan/pool host; a 7-day chart of weekly and 5h used % with the switch line (reuse the History chart); token totals for that account.
3. **Colours by level** (used %, relative to that limit's `switch_at`, default 98): `< 50` green, `50–80` yellow, `80–switch_at` orange/red-ish, `≥ switch_at` red bold; unknown/stale grey. Use the same colours for bars, % text and chart lines. Active-host marker in the theme's accent colour.
4. **Reset icons:** `⚡N` Claude Fable resets (magenta), `⟲N` Claude full resets (cyan), `⟲N` Codex reset credits (cyan); hidden when the count is 0 in the list, shown as `0` dim in the detail; `?` when unknown. Add a one-line legend under the list or in `?` help.
5. **Outlook box (top):** from `outlook`; red when `dry_at` is within 24 h, yellow within 72 h, green otherwise. Omit when absent.
6. **Tokens** stays as a second tab (`Tab`/`2`), full width, same chart controls as today (`h`/`d`, `e`, `g`); `1` back to Accounts. History becomes part of the detail pane (drop the separate tab).
7. Keep it fast: render from the cached snapshot only; no blocking I/O on the UI thread.

Tests: render tests at 200×50, 120×40 and 80×24 from the fixture (with and without the new fields), colour thresholds, j/k focus + detail content, `x` expansion, reset icon rendering. Update `doc/usage-panel.md` and the CLAUDE.md F5 line.

## C. Rollout (lead)

Hub worker and TUI worker in parallel on fresh branches from origin/main. Lead reviews, runs the isolated suites, merges, installs the hub tool on cm-manager, builds and stages `tui-<commit>` for Owner's laptop install, and asks Swarm-Coord to reinstall the host tool on cm-sessions (host-side change in A.1/A.2). F needs Owner (F-Lead's rule). Laptop collection stays off.
