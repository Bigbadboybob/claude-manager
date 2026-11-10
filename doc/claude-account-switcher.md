# Claude account switcher

Status: design 2026-10-22 from an Owner discussion on cm-sessions (branch
`cm/claudelb`). Built so far: `capture` / `list` / `switch` / `remove`
(real switch passed on the laptop 2026-11-07) and `poll` / `status` /
`test-alert` (real readings on the laptop 2026-11-09; Telegram delivery
not yet proven) and autoswitch `suggest`/`auto` (fakes only); see
[Operating it](#operating-it).
Details still marked **verify** below are the ones validation did not settle.

## Goal

Owner needs more Claude capacity than one Max 20x subscription provides
(individual use, several Max accounts Owner pays for). Today Owner switches
accounts by running `/login` in any one session; every running Claude Code
session on that machine then follows the new account. This tool automates
that workflow:

- keep every account's login on the machine so switching never needs a
  browser sign-in;
- switch the machine's active account with one command (manual or
  automatic);
- show every account's 5-hour and weekly usage in one dashboard and alert
  Owner when the active account runs low.

It is the Claude counterpart to codex-lb ([codex-account-pool.md](codex-account-pool.md))
in purpose, but deliberately **not** in mechanism (see below).

## Decisions already made (Owner, 2026-10-22)

1. **Switch the machine-wide login; do not route requests.** No proxy, no
   `ANTHROPIC_BASE_URL`, no request rewriting, no per-request account
   choice. The unmodified `claude` binary talks to Anthropic directly with
   whatever login is in `~/.claude/.credentials.json`. The tool only does
   what a manual `/login` does, faster.
2. **Running sessions are not restarted.** Owner has observed that after
   `/login` in one session, other running sessions on the same machine
   start using the new account. The design relies on this. Confirmed on the
   laptop for a direct in-place file swap without `/login`.
3. **Inactive accounts show last-known usage (option "a").** The tool does
   not refresh inactive accounts' tokens or poll them. It shows the last
   reading taken while each account was active, plus its known reset times.
4. Test and develop on the **laptop** first, not cm-sessions/cm-manager,
   so active sessions on the cloud hosts are not disturbed.

## Background: why not a proxy

Research summary (October 2026), for context only:

- Community pooling proxies exist: better-ccflare (most mature), TeamClaude
  (rewrites `account_uuid` in request bodies), Maxpool, claude-rotate,
  AIGate (no proxy, launches the official binary per account), CLIProxyAPI.
  All except AIGate store tokens and inject them per request.
- Anthropic's [legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance)
  says subscription OAuth is for "ordinary use of Claude Code", usage
  limits "assume ordinary, individual usage", and developers "may not
  collect, store, or intermediate Claude.ai credentials or session tokens".
  An Anthropic Claude Code team member has reportedly said holding several
  Max accounts is not itself a violation.
- API-key billing is not viable at Owner's volume (SemiAnalysis estimates a
  $200 Max plan delivers up to ~$8,000/month of API-priced usage).
- A machine-wide login switch through Claude Code's own credential file is
  the closest automation of what Owner already does by hand. It carries
  residual risk (one account running low, then traffic continuing on
  another, is visible to Anthropic either way), but it avoids every
  proxy-specific red flag.

## Facts about the credential files (observed on cm-sessions, Claude Code 2.1.308)

Key names only; never print values.

`~/.claude/.credentials.json` (mode 0600):

```
claudeAiOauth:
  accessToken, refreshToken, expiresAt, refreshTokenExpiresAt,
  scopes (list), subscriptionType, rateLimitTier,
  subscriptionTypeSource (laptop)
```

`~/.claude.json` contains `oauthAccount` with `accountUuid`,
`emailAddress`, `organizationUuid`, `organizationName`, `displayName`,
`billingType`, `seatTier`, rate-limit tier fields and others. `/login`
writes both files. `~/.claude.json` is large and is rewritten frequently by
every running Claude Code process.

Documented ([authentication](https://code.claude.com/docs/en/authentication)):
on Linux the login lives in `~/.claude/.credentials.json` (0600);
`CLAUDE_CONFIG_DIR` moves it (and session history, which would break CM's
transcript discovery, so do not use per-account config dirs);
`CLAUDE_CODE_OAUTH_TOKEN` and `apiKeyHelper` outrank the `/login`
credential, so they must **not** be set in sessions this tool manages.

## Design

### Components

One small CLI per host, working name `cm-claude-accounts`, with a state
directory `~/.config/cm-claude-accounts/` (mode 0700):

```
accounts/<accountUuid>.json  saved login: claudeAiOauth block + oauthAccount
                        block (email kept as the display label)
state.json              active account, per-account last usage reading,
                        reset times, thresholds, autoswitch mode, history
lock                    flock held during capture/switch
```

Commands:

| Command | Effect |
|---|---|
| `capture` | Save the currently active login right after Owner ran `/login`. `/login` writes `.credentials.json` and `oauthAccount` together, so at that moment `oauthAccount.accountUuid` reliably names the login; the account is keyed by that UUID. Sets it as the active account in `state.json`. Idempotent. |
| `list` / `status` | Accounts, which is active, last usage per account, reset times, age of each reading. |
| `switch <email>` | Write back the active account's latest credentials, then make `<email>` active (see protocol). |
| `poll` | Read the active account's usage and record it; trigger alerts/autoswitch. Run by a timer. |
| `remove <email>` | Delete a saved account. |

### Switch protocol

The main pitfall is token refresh. Claude Code refreshes the access token
in the background and writes the new tokens back to `.credentials.json`.
The refresh token rotates on every refresh, and the previous access token
is revoked at once (validated). A saved copy therefore goes stale as soon
as Claude Code refreshes it, and switching back to a stale copy would need
a new `/login`.

**Identifying the login in the file.** Refresh tokens rotate, so the live
`refreshToken` usually matches no saved copy. The profile in
`~/.claude.json` is **never** evidence of identity: running sessions can
write a stale `oauthAccount` back (a review of `fb616fd` showed that
trusting it saves one account's tokens under another). Resolve identity in
this order:

1. Exact `refreshToken` match with a saved copy.
2. Otherwise ask Anthropic with the live access token:
   `GET https://api.anthropic.com/api/oauth/profile` (headers
   `Authorization: Bearer`, `Content-Type: application/json`,
   `Cache-Control: no-cache`; the request Claude Code makes). Its
   `account.uuid` equals `oauthAccount.accountUuid` (validated on the
   laptop). Read-only; it never refreshes. A saved account with that UUID
   gets the login; an unsaved one is added only if `~/.claude.json`
   describes that same UUID (otherwise refuse and ask for `/login` +
   `capture`).
3. If the access token has expired, or Anthropic cannot be reached or
   rejects it, refuse and change nothing. Never refresh to find out.

The network request runs **before** taking the refresh lock (holds must
stay short); under the lock the switcher re-reads the file and starts over
if a session refreshed in the meantime. `poll` repairs the profile only
after this confirmation.

`switch <target>` under the lock:

1. **Take Claude Code's refresh lock** with the identical mechanism (see
   [Refresh lock](#refresh-lock-claude-code-21308)). Under the lock a
   refresher re-reads `.credentials.json` and, if the access token there
   differs from the one it held, adopts the file instead of refreshing.
   Holding the lock for steps 2–3 keeps any session from refreshing the
   old account mid-switch. Keep the hold to a few seconds. Identify the
   live login (rules above) before taking it.
2. **Write back.** Read `.credentials.json`, identify it with the rules
   above, and save its `claudeAiOauth` block as that account's latest copy.
   Keep the account's `oauthAccount` from `capture`; never take it from the
   live `~/.claude.json` unless it names the confirmed account.
3. **Swap credentials.** Replace only the `claudeAiOauth` key (the file
   can hold other secrets, such as MCP servers' OAuth tokens) and write
   `.credentials.json` in place (truncate + write, mode 0600), as `/login`
   does (it kept the inode). Running sessions followed an in-place write.
   Claude Code's own refresh replaces the file by rename; whether running
   sessions also follow a rename swap was not tested, so use in-place.
4. **Swap account metadata.** Replace only the `oauthAccount` key in
   `~/.claude.json` with the target's saved block: read, modify, write to a
   temp file, rename, and retry if the file changed underneath. Validated:
   it only affects display (`/status` keeps naming the old account if it is
   skipped; billing follows `.credentials.json`). Still needed for
   a correct `/status`. Record and persist `state.json`'s active account
   immediately after step 3, still under the lock, so a failure here can
   only leave the display stale; a profile-write failure is a warning.
5. **Expect the target's refresh.** The target's saved access token has
   probably expired while it was inactive, so a session will refresh it
   and rewrite the file within moments. Nothing special is needed: the
   next write-back (`switch`, `capture` or `poll`) confirms the new tokens
   with Anthropic and saves them under the right account.
6. Record the switch in `state.json` history and notify Owner.

`capture` and `poll` also perform write-back (step 2) opportunistically,
so the saved copy of the active account stays current even without
switches.

If the target's saved refresh token has expired (`refreshTokenExpiresAt`)
or the first request after a switch fails authentication, mark the account
`needs-login`, switch back, and notify Owner to `/login` that account and
`capture` again.

### Refresh lock (Claude Code 2.1.308)

Read from the binary and confirmed live on the laptop. It is neither
`flock` nor a timestamp file: it is the npm `proper-lockfile` scheme, a
directory created with `mkdir` (atomic; `EEXIST` means held).

- **Locks, in order:** `~/.claude/.oauth_refresh.lock` (current), then
  `<realpath(~/.claude)>.lock`, i.e. `~/.claude.lock` (legacy). Both are
  directories with options `stale: 60000, update: 5000`: the holder
  touches each directory's mtime every 5 s, and a lock whose mtime is
  older than 60 s counts as stale and may be removed by the next taker. If
  the legacy lock is held, Claude Code releases the current one and backs
  off.
- **Owner record:** after acquiring, Claude Code writes
  `~/.claude/.oauth_refresh.lock.owner` (JSON: `pid`, optional
  `procStart`, `pidDomain`, `pidSpace`, `lockBirthtimeMs`, optional
  `legacyLockBirthtimeMs`; behind feature flag `tengu_quiet_marten`) and
  removes it on release. Waiters use it only to take over from a holder
  proven dead; with no owner record they never take over early
  (`no_owner_record`) and fall back to the 60 s staleness.
- **Waiters:** on `ELOCKED` a refresher retries up to 5 times with
  1–2 s random backoff (at least 7.5 s if the lock is unchanged), then
  gives up for that attempt (`lock_busy` / `lock_timeout`) and retries on
  a later request.
- **Refresh is on demand**, triggered by a request with an expired (or
  near-expiry) access token; nothing refreshes while sessions are idle.

The switcher takes both directories in the same order, touches their
mtime while held, writes **no** owner record (so nobody can take over
from it early; if it dies, the 60 s staleness frees the lock), and
releases in reverse order. Holds stay well under the waiters' ~7.5 s
budget.

Live check (laptop): with both directories held by a test script and the
access token marked expired, a running session's refresh waited for the
full 6 s hold and landed 1.5 s after release.

### Usage, dashboard and alerts

- **Active account:** poll the usage data that `/usage` shows with the
  access token already in `.credentials.json`:
  `GET https://api.anthropic.com/api/oauth/usage` with
  `Authorization: Bearer <accessToken>` (no beta header needed; validated,
  HTTP 200). Read the `limits` list, not fixed fields: each entry has
  `kind` (`session` = 5-hour, `weekly_all`, `weekly_scoped` per model),
  `group`, `scope`, `percent`, `is_active` (the binding limit), `severity`
  (`normal` / `warning` / …) and `resets_at`. Plans differ: as of
  2026-11-07 some Max accounts have **no 5-hour limit**, and their
  `five_hour` field is `null`. Thresholds apply per limit present.
  Undocumented, so tolerate it changing shape. Never poll an
  inactive account's saved token (decision 3); it only stays valid until
  that account's next refresh anyway.
- **Rate limit:** the usage endpoint answered HTTP 429 on the laptop
  (2026-11-09) at the 7th call in about 30 minutes (5-minute timer plus a
  manual run). Its budget may be shared with Owner's own `/usage`, so
  `poll` runs every 10 minutes and treats a 429 as a skip: no request for
  15 min, doubling per consecutive 429 up to 2 h, or longer if
  `Retry-After` says so.
- **`poll` never refreshes a token.** If the live access token has
  expired (`expiresAt` passed, or the usage request returns 401), skip the
  cycle and record why; the next real request from a session refreshes it.
- **`poll` repairs the profile.** Each cycle, once the live login's
  identity is confirmed (identity rules above), compare
  `oauthAccount.accountUuid` in `~/.claude.json` with that account and
  rewrite the `oauthAccount` key (same atomic method as switch
  step 4) if it names the wrong account. Running sessions rewrite
  `~/.claude.json` and could restore a stale in-memory profile.
- **Inactive accounts:** last reading taken while active, its timestamp,
  and the reset times from that reading. An account whose weekly reset has
  passed since its last reading is shown as "likely refilled".
- **Alerts:** through CM's existing Owner channels (`cm-notify` /
  Telegram, as the codex update checker uses), when the active account
  crosses a threshold (default: 80% of 5-hour or 90% of weekly), and when
  any account needs a new login.
- **Dashboard:** start with `cm-claude-accounts status` output. A TUI panel
  (or a section in the F9 settings / A-i peek) can come later and read
  `state.json`.

### Autoswitch

Modes, configured in `state.json`:

- `off`: alerts only.
- `suggest` (start here): on threshold, alert with the recommended target
  and the exact `switch` command.
- `auto`: on threshold, switch to the account with the most room (latest
  readings, preferring accounts whose reset has passed), then alert.

No switching back and forth: require a minimum dwell time (e.g. 30 min) and
never switch to an account whose last reading is above threshold and whose
reset has not passed.

Safeguards in `auto` (built 2026-11-09): it never switches to an account
with no reading on any host unless `auto_allow_unread` is set; it never
picks an account Owner has excluded (`exclude <email>`); and it stops after
`max_auto_switches_per_day` automatic switches (default 6) in 24 h. In
each case it falls back to the `suggest` alert and says why.

### Multiple hosts

Each host (laptop, cm-sessions, cm-manager) has its own `~/.claude`, so each
needs its own store and a one-time `/login` + `capture` per account. Do not
copy saved logins between hosts: each host keeps its own refresh-token
lineage, and sharing one lineage across hosts would make them overwrite
each other.

Hosts share **readings**, never logins (built 2026-11-09). `export` prints
this host's accounts, their last readings and the active account as JSON,
with no tokens. A host that lists others as `peers` (ssh aliases) reads
their exports with `ssh -o BatchMode=yes <peer> python3
.local/bin/cm-claude-accounts export`. Then:

- `status` shows each account's freshest reading from any host ("read 5m
  ago on cm-sessions"), which host each account is active on, accounts
  saved only on a peer, and peers that could not be read.
- When `poll` is choosing a target, it reads the peers first. An account's
  freshest reading counts. An account active on another host ranks after
  idle accounts with room, since both hosts would be drawing on it.
- `export` takes no store lock, so two hosts that list each other never
  wait on each other. A peer that fails keeps its last good copy;
  "active on" is trusted for an hour after the last good read.

Running `switch` on several hosts at once is still manual (one `switch` per
host).

### Costs and caveats

- Every live session loses its prompt cache when the account changes (the
  same as a manual `/login` today). Switch only when needed.
- Residual policy risk as described under Background. Prefer `suggest` mode
  until Owner is comfortable.
- Saved logins are secrets: never print them, never put them in the repo,
  logs, chat or task records. Mode 0700/0600 throughout.

## Coordinator control plane (design delta, 2026-11-10)

Owner's direction (brief: `~/.local/share/swarm-coord/account-coordinator/BRIEF.md`
on cm-sessions): not a blind autoswitch but a control plane that an
**account coordinator** agent on cm-manager drives. The coordinator (or
Owner through it) decides which account each host uses and when it moves;
switches stay rare (each drops every live session's prompt cache, and about
1 in 20-30 sessions errors afterwards); accounts can be reserved; near a
limit the coordinator stops non-critical work. Owner's only manual step is
one `/login` per needed account per host, at the end. `auto` stays as a
fallback a host uses only when told to.

### Hub and check-in

- **Hub** on cm-manager: `~/.config/cm-claude-accounts/hub/` (0700) holds
  `desired.json` (per-host assignments, reservations, drains, policy, a
  revision number), `reports/<host>.json` (each host's last check-in) and
  `events.log`. Only hub commands write it, under `hub/lock`, and nothing
  holds that lock across a network call.
- **Check-in, outbound only.** Every host's timer runs `poll`, which ends
  with `ssh -o BatchMode=yes cm-manager cm-claude-accounts hub checkin
  <host>` carrying its `export` (accounts, logins' validity, readings,
  active account, apply results, live sessions; never tokens) on stdin.
  The reply is that host's desired entry plus the merged readings,
  reservations and policy. The laptop needs no inbound SSH; cm-manager
  checks in locally. Hub unreachable: keep the current account and the
  last reply's policy, and say so in `status`.
- **Cadence:** the timer moves to every 2 min so an assignment applies
  within about 2 min everywhere; the usage request itself stays at most once
  per 10 min (the 429 budget). The coordinator can also run a cloud host's
  `poll` over ssh for an immediate apply.
- **Applying.** A host's desired entry is `{account, mode}`:
  - `pin`: converge to `account` (switch if different). If the account is
    not saved on this host or its login expired, don't switch; report
    `blocked: needs /login of <email> on <host>`.
  - `auto`: the host may switch itself at threshold (today's `auto`), but
    only to accounts not reserved for others and not draining.
  - `manual` (default with no hub): never switch; suggest only.
  Each switch keeps today's protocol (refresh lock, write-back), the
  result goes into the next check-in, and the history records
  `reason: hub rev N` / `auto: …`.

### Coordinator commands (on cm-manager; `--json` on each for the agent)

| Command | Effect |
|---|---|
| `hub status` | per host: active vs desired account, mode, last check-in age, apply result, live Claude sessions; per account: freshest reading and its age, reset times, hosts using it, hosts with a saved login, reserved/draining |
| `hub assign <host> <account> [--pin\|--auto\|--manual]` | set a host's desired entry (`none` clears it); refused for an account reserved for another host or not saved on that host, unless `--force` |
| `hub reserve <account> --for <host>[,..] [--note …]` / `hub release <account>` | keep an account for named hosts (or `nobody`); auto and drains never pick it for others |
| `hub drain <account> [--dry-run]` | stop placing work on it and pin each host using it to the best other account (most room, saved on that host, not reserved or draining, fewest hosts already on it); prints the plan |
| `hub undrain <account>` | allow it again |
| `hub policy <key> <value>` | thresholds, default mode, dwell, daily cap, sweep on/off |
| `hub history` | assignment changes, apply results, switches, sweeps |

**Keeping switches rare:** a host moves only when the coordinator assigns
it, a drain moves it, or (in `auto`) it crosses a threshold. Assigning the
current account is a no-op. `hub status` shows each host's live session
count, so the coordinator can move the host where a switch costs least.
Dwell time and the daily cap still bound `auto`.

**Priorities** stay with the coordinator: claudelb reports which hosts (and
so which sessions) draw on each account; the coordinator tells
non-critical sessions to stop through CM messaging. claudelb does not
classify work.

### Post-switch sweep and re-prompt

For 30 minutes after any switch on a host, each check-in also runs a sweep
on that host, through its own CM daemon (local operator socket, so it works
on the laptop):

1. List live Claude sessions (`list_sessions`) and find each transcript
   (`resolve_authorized_session`).
2. Re-prompt a session once if it is idle at its prompt and its last entry
   is an API error message (`isApiErrorMessage`), whether the error came
   after the switch (the 1-in-20-30 bug) or stopped it on the limit that
   triggered the switch (`error: rate_limit`). The prompt is "Continue where
   you left off (the Claude account was switched)", sent the way CM's
   `send_input` submits.
3. Once per session per switch, and every re-prompt is logged (alerts.log,
   history, and a coordinator alert naming the sessions). Billing and
   account-hold errors (`oauth_org_not_allowed`, `account_on_hold`) are
   never re-prompted: they alert Owner.
4. Only sessions an agent manages (`managed_by_uid` set). Never bash
   shells, and never sessions a continuous task or workflow owns, since
   their scheduler does. Owner's own sessions only with `config
   sweep_owner_sessions true` (Swarm-Coord for Owner, 2026-11-10). At most
   20 per run.

Errors seen in cm-sessions transcripts over the last 14 days:
`rate_limit` (429, 432 entries), `authentication_failed` ("Not logged in",
22), `oauth_org_not_allowed` (403, 5), `account_on_hold` (3). The sweep
matches the entry's shape, not the text, so it doesn't depend on the exact
text of the post-switch bug.

### Alerts with a hub (Swarm-Coord for Owner, 2026-11-10)

Per-host alerts (thresholds, poll failures, automatic switches, sweeps) go
to the hub's alert log (`hub alerts`). If `hub policy coordinator_command`
is set, the hub also runs that command once per alert to wake the
coordinator. They queue on the host while the hub is unreachable. Owner's
Telegram gets only what needs Owner: a saved login that expired or an
active login that isn't saved, billing or hold refusals, and one hub alert
when few accounts have room left anywhere (`hub policy low_room_accounts`,
default 1; repeated only if it gets worse, or after 6 h).

### cm-manager's account

Continuous tasks and the coordinator share cm-manager's one fixed account
(Swarm-Coord for Owner, 2026-11-10): the hub pins cm-manager to it and
reserves it for cm-manager, so no other host or drain takes it. No separate
coordinator login. Risk: continuous work could use up the coordinator's
room too. The coordinator watches that account first, and `hub critical`
lists the coordinator as critical work.

### Logins without moving a host

`cm-claude-accounts login` runs `claude` with `CLAUDE_CONFIG_DIR` set to a
private scratch directory. There Owner types `/login` and completes the
browser step, then `/exit`. The tool then saves that account (confirmed with
Anthropic) and deletes the scratch copy. The host's own login is never
touched, so its sessions keep running and keep their cache. Checked on
cm-sessions (Claude Code 2.1.322): with `CLAUDE_CONFIG_DIR` set, Claude
Code reads and writes `.claude.json` and the credentials inside that
directory ("Not logged in" there, home files unchanged). The account each
host is on now needs no login at all: `capture` saves it.

Accounts seen on 2026-11-10: laptop has bender0, bender7, bender192801 and
lklein9900 saved (active bender7); cm-sessions is on bender1@kleinatc.com;
cm-manager is on bender192802@gmail.com. Neither cloud host has anything
saved yet.

So per host: **laptop** none. **cm-manager** none if its account is the
one it's on now (capture), else 1. **cm-sessions** capture its current
account, plus one scratch `login` for each other account it may use.

### Build status

1-2 (hub, check-in, apply, coordinator commands, alert routing): built,
5fd01446. 3 (sweep): built. `login` (scratch-directory capture): built. 109 tests;
the sweep, hub and alert-routing rules each fail a test when broken.
The coordinator's separate config directory: dropped (cm-manager's one
account serves both). Real checks happen at cutover, below.

## Cutover runbook (target 2026-11-10 13:00Z)

Owner's only part is the browser login in step 4. Agents do everything
else. `claudelb` drives the cloud hosts (Owner approval for the cloud
captures and logins is the cutover itself), and `claudelb-local` drives the
laptop. Install everything from one commit, `<SHA>` below (the head of
`cm/claudelb` at cutover).

1. **Hub on cm-manager** (claudelb, over ssh; no login, no switch):
   copy `scripts/cm-claude-accounts` and `deploy/` at `<SHA>` to
   `~/.local/share/cm-claude-accounts/src/` (with an `OWNER.md`), then
   `python3 ~/.local/share/cm-claude-accounts/src/scripts/cm-claude-accounts install --no-timer`,
   `cm-claude-accounts config hub local`, `config host_name cm-manager`,
   `hub policy notify_command ~/.cm/bin/cm-notify`, `hub status`
   (creates the hub store; add an `OWNER.md` to
   `~/.config/cm-claude-accounts/`).
2. **Laptop** (claudelb-local): check out `<SHA>`, `install` (timer
   already running), `config host_name laptop`, `config hub cm-manager`,
   then `status` and confirm `hub cm-manager as laptop: checked in`.
   On cm-manager: `hub assign laptop bender7@kleinatc.com --pin` (it's
   already on it, so no switch).
3. **Capture the cloud hosts' current logins** (claudelb; no switch, no
   login): on cm-sessions and on cm-manager, `cm-claude-accounts capture`.
   Then on each, `config host_name <host>`, `config hub cm-manager` (cm-manager: `local`),
   `install` (starts the 2-min timer). `hub status` should show all three
   hosts on their current accounts.
4. **Owner's logins** (cm-sessions; repeat for each extra account it may
   use): an agent starts a CM bash session on cm-sessions running
   `cm-claude-accounts login`. Owner attaches, types `/login`, signs in as
   that account in the browser, then `/exit`. The tool prints `saved <email>`.
   If cm-manager's chosen account isn't the one it's on, do the same once
   there.
5. **Assignments** (claudelb on cm-manager): `hub reserve <cm-manager's account> --for cm-manager`,
   `hub assign cm-manager <it> --pin`, `hub assign cm-sessions <its current account> --pin`
   (nothing moves), any reservations Owner names. Leave `default_mode`
   manual, so nothing switches until the coordinator decides.
6. **Check:** `hub status` shows every host checked in within 2 min, each
   apply `ok`, accounts saved where expected, no `NO ROOM` surprises.
   `hub history` shows no switch. Each host's `status` names the hub.
7. **Hand over** to the coordinator (Swarm-Coord): set `hub policy
   coordinator_command <wake script>` once the coordinator exists; brief
   it on `hub status --json`, `assign`, `drain`, `reserve`, `critical`,
   `alerts`.

**Rollback** per host: `cm-claude-accounts config hub default` (back to
local suggest mode; the timer keeps reading usage), or `systemctl --user
disable --now cm-claude-accounts-poll.timer`. Saved logins stay; nothing
needs Owner.

## Plan

### 1. Validation on the laptop (do this first)

Use two accounts that both have quota left, so a switch is not explained by
the old account failing.

1. Owner `/login`s to account A, then account B, in a scratch session. After
   each, copy `.credentials.json` and the `oauthAccount` block to a private
   scratch store (do not print them).
2. With two other Claude sessions running, swap the file directly (no
   `/login`) from B to A. In each running session, check `/status` and send
   a short prompt. Confirm the request is billed to A (A's usage moves).
   Try both a `rename` swap and an in-place write.
3. Check whether swapping `oauthAccount` in `~/.claude.json` is needed for
   correct behaviour, or only for display.
4. Force a refresh (wait for `expiresAt` or edit it to the past in a
   scratch copy) and record whether `refreshToken` changes. Switch away and
   back using a pre-refresh saved copy to see whether the old refresh token
   still works.
5. Find how `/usage` gets its numbers.
6. Find how long before `expiresAt` Claude Code refreshes (switch step 1),
   and whether several sessions refreshing at once each write the file.

Write the findings into this doc and resolve each **verify**.

### Validation results (laptop, 2026-10-22)

Laptop (pop-os), Claude Code 2.1.308, no `CLAUDE_CODE_OAUTH_TOKEN`,
`ANTHROPIC_*` or `apiKeyHelper` set. Accounts: A = an account at ~10%
(also active on another machine, so its usage was not a clean signal),
B = an account at 94% five-hour and otherwise idle, used as the clean
signal. Tokens were handled only as 8-hex SHA-256 fingerprints.

| Step | Result |
|---|---|
| 1. Save logins | `/login` wrote `.credentials.json` **in place** (same inode) plus `oauthAccount`. `/login` to B did not invalidate A's saved tokens. |
| 2. Swap without `/login` | **Works.** With two sessions running on A, an in-place swap to B: their next prompts were billed to B (B 94% → 95%, the sessions showed B's 94% warning); swapping back to A stopped B's usage. A session running before an earlier swap A ← B also followed it. No session rewrote the file after a swap. Rename swap not tested. |
| 3. `oauthAccount` | **Display only.** With credentials on A and `oauthAccount` still B, `/status` showed B's email while usage went to A. |
| 4. Refresh rotation | Setting `expiresAt` to the past made a running session refresh within ~1 s, unprompted, holding `~/.claude/.oauth_refresh.lock`. **Both tokens rotated**; the old access token returned 401 immediately. The refresh rewrote the file by **rename** (new inode). Reusing the pre-refresh refresh token was deliberately not tested (reuse detection could revoke the whole login); treat saved copies as single-use after any refresh. |
| 5. Usage source | `GET /api/oauth/usage` (see Usage above). |
| 6. Refresh lead time | **Not measured.** Normal refresh happens some time before `expiresAt` (access tokens last 8 h); an `expiresAt` in the past triggers an immediate refresh. Concurrent refreshes: the lock serialises them and a waiter adopts the winner's file rather than refreshing again (from the binary; only one refresher was observed). |

Implications for the build: write back before every switch, swap in place
under Claude Code's refresh lock, keep each account's `oauthAccount` from
`capture`, and still rewrite `oauthAccount` for `/status` and identity.
Open: whether long-running sessions later rewrite `~/.claude.json` with a
stale in-memory `oauthAccount` (a written value held for 60 s here).

### 2. Build

1. `cm-claude-accounts` CLI with `capture`, `list`, `switch` (including
   refresh lock, write-back and target-refresh handling) and tests against
   fake credential files. **Started:** `scripts/cm-claude-accounts` and
   `tests/test_cm_claude_accounts.py` (16 tests; run
   `python3 -m unittest tests.test_cm_claude_accounts`). `capture` and
   `list` checked on the laptop, including a real Anthropic confirmation.
   **Real switch passed (laptop, 2026-11-07, Claude Code 2.1.316):** with
   two running sessions on account A (42% weekly, 5-hour limit), `switch`
   to B (89% weekly, no 5-hour limit) moved both: `/usage` showed B
   immediately and a prompt succeeded on B; `switch` back moved both
   again. In place (same inode), profile updated, locks released, the
   outgoing login identified by token. The 2.1.316 binary has the same
   refresh lock (paths, `stale:60000,update:5000`, owner record) and the
   same profile and usage endpoints as 2.1.308.
2. `poll` + state + alerts + a systemd user timer (every ~5 min).
   **Built (2026-11-09), fakes only:** `poll`, `status`, `test-alert`,
   `deploy/cm-claude-accounts-poll.{service,timer}` (every 10 min since the
   429 below); 33 tests. Beyond the
   rules above, `poll` also saves the live login whenever a session has
   refreshed it (confirmed with Anthropic, as in `switch`), so the saved
   copy stays current and `switch` rarely refuses after idle time; and a
   `/login` nobody captured is saved when Anthropic confirms it and the
   profile names the same account (otherwise it alerts). Not yet run
   against a real login.
3. `suggest` autoswitch, then `auto` behind a config flag.
   **Laptop run of `poll` (2026-11-09, 1213a47):** real readings for the
   active account (week 96%, 5h 0%, `week:Fable` 0%); the credentials file
   was unchanged across six polls. It found three bugs, fixed in 1213a47:
   reset times jitter by under a second, model-scoped limits have an object
   `scope`, and an undelivered alert was never retried.
   **Built (2026-11-09), fakes only:** both modes inside `poll`. On a limit
   at or over threshold, the target is the saved account with the most
   room by its last reading (a limit whose reset has passed counts as
   empty; accounts never read rank after any with known room; expired
   logins never qualify). `suggest` (default) adds
   `Suggest: cm-claude-accounts switch <email> (<why>)` to the threshold
   alert, or "No other saved account has room"; `status` shows the same.
   `auto` runs `switch` itself, at most once per `min_dwell_minutes`
   (default 30) after any switch, records `reason: auto: …` in history and
   alerts what it did. Only the 5-hour and all-models weekly limits
   trigger a switch or rule out a target; a full model-scoped limit
   (`week:Fable`) alerts without advice unless `"switch_on_scoped": true`.
   50 tests.
   **Safeguards and other hosts (2026-11-09), fakes only:** peers and
   `export` (see Multiple hosts); `auto` needs a reading for its target,
   honours `exclude`, and caps itself per day; `config`, `history`,
   `exclude`/`include` and `install` commands. 74 tests.
   **Laptop run of cf0ff7a/14790a0 (2026-11-09, 18:10-18:58Z):** `install`
   installed the script and enabled the timer; `config`, `history`, `export`
   (4 accounts, no tokens) and a failing peer (tool absent on cm-sessions:
   clean "export failed", poll unaffected) behaved as designed. With
   cm-notify copied over (Owner-approved), `test-alert` and then the
   undelivered 96% alert reached Telegram. Four clean polls.
   **Laptop run of 35ed90c (2026-11-09, 05:03-05:47Z, timer only):** a 429
   on the first call (9 min after the previous) was skipped with a 15-min
   backoff; then three clean calls 10 min apart. The undelivered 96% alert
   was retried after an hour, carrying `Suggest: cm-claude-accounts switch
   <email> (no usage reading yet)`. A session refresh at 05:17:40Z replaced
   `.credentials.json` (new inode, by rename), and the next `poll` saved the
   new login after Anthropic confirmed it; `poll` itself never wrote the
   file. Still unproven: Telegram delivery (no notifier on the laptop yet),
   a match with `/usage`, and `auto` on a real login.
4. Laptop rollout, then cm-sessions and cm-manager (one host at a time,
   with Owner present for the per-account `/login`s).
5. Optional: TUI dashboard panel; multi-host `switch` (multi-host `status`
   is built: peers).

Follow the repository workflow (branch, test, merge to `main`; no PRs).

## Operating it

Per host, as the user who runs Claude Code. Never copy the store between
hosts (see Multiple hosts).

```bash
scripts/cm-claude-accounts install    # from a checkout: ~/.local/bin + poll timer
cm-claude-accounts capture            # right after each /login
cm-claude-accounts login              # add an account without moving this host (scratch /login)
cm-claude-accounts hub status         # on the hub (cm-manager): every host and account
cm-claude-accounts status             # dashboard: every account's freshest reading
cm-claude-accounts switch <email>     # move every session on this host
cm-claude-accounts config             # list settings; `config KEY VALUE` changes one
cm-claude-accounts history            # recent switches, captures, setting changes
cm-claude-accounts test-alert         # check the alert path
journalctl --user -u cm-claude-accounts-poll.service -n 20   # last runs
```

`install --no-timer` copies the script only. Starting the timer also means
the next `poll` saves this host's current login (as `capture` would), so on
a new host run it with Owner present.

- **Store:** `~/.config/cm-claude-accounts/` (0700): `accounts/<uuid>.json`
  (saved logins, 0600), `state.json` (active account, readings, peer
  copies, settings, alert bookkeeping, history; no tokens), `alerts.log`
  (every alert, delivered or not).
- **Alerts** run `~/.cm/bin/cm-notify "<message>"` with
  `CM_NOTIFY_TAG=claude-accounts@<host>` (Telegram; needs `~/.cm/notify.env`).
  Override with `--notify-command` or `config notify_command <path>`.
  A failed delivery is logged and shown by `status`.
  Sent: active account at or over its threshold (once per limit per reset
  period), a saved login that has expired, an active login that is not
  saved, 3 failed polls in a row, and recovery after that.
- **Settings** (`config KEY VALUE`; `default` resets one). `config` takes
  the store lock, so it is safe while the timer runs; don't hand-edit
  `state.json`.

  | Key | Default | Meaning |
  |---|---|---|
  | `autoswitch` | `suggest` | `off`, `suggest` or `auto`. `auto` moves every session on the host on its own: only with Owner's say-so |
  | `thresholds.session` / `thresholds.weekly` | 80 / 90 | alert percent; `thresholds.<kind>` overrides one limit kind |
  | `min_dwell_minutes` | 30 | no automatic switch sooner than this after any switch |
  | `max_auto_switches_per_day` | 6 | automatic switches allowed in 24 h |
  | `auto_allow_unread` | false | let `auto` pick an account never read on any host |
  | `switch_on_scoped` | false | let a full model-scoped limit (`week:Fable`) trigger a switch |
  | `peers` | none | comma-separated ssh aliases to read (see Multiple hosts); keys must work without a passphrase prompt, since the timer has no agent |
  | `notify_command` | `~/.cm/bin/cm-notify` | alert command |

  `exclude <email>` keeps an account out of every suggestion and automatic
  switch (`include` undoes it); `status` marks it.
- `poll` exits 0 when it skips (access token expired or rejected: the next
  prompt in any session renews it; or backing off after HTTP 429) and 1
  when it fails, printing the reason to the journal.
