# Claude account switcher (design, not yet built)

Status: design draft 2026-10-22 from an Owner discussion on cm-sessions
(branch `cm/claudelb`). Nothing is implemented. The laptop validation ran
on 2026-10-22; see [Validation results](#validation-results-laptop-2026-10-22).
Details still marked **verify** below are the ones it did not settle.

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

**Identifying the login in the file.** If refresh tokens rotate, the live
`refreshToken` usually matches no saved copy, so token matching alone
cannot identify the account. Resolve identity in this order:

1. Exact `refreshToken` match with a saved copy.
2. The active account recorded in `state.json`, provided
   `oauthAccount.accountUuid` in `~/.claude.json` still equals that
   account's UUID. A token that differs from the saved copy is then that
   account's own refresh.
3. If `oauthAccount.accountUuid` names a different account than
   `state.json`, Owner ran `/login` outside the tool: treat the file as that
   account's login (capture it, adding it if new) and update `state.json`.
4. Otherwise refuse and ask Owner to `/login` + `capture`. Never overwrite
   or discard a login the tool cannot identify.

This relies on the tool's own switches keeping `oauthAccount` in step with
the credentials (switch step 3). If validation drops step 3, rule 3 is
lost: out-of-band `/login`s become undetectable and the tool must
instead identify the login by asking Anthropic (the profile request Claude
Code makes; see `profileFetchedAt`). **Verify** which request that is, if
needed.

`switch <target>` under the lock:

1. **Take Claude Code's refresh lock** with the identical mechanism (see
   [Refresh lock](#refresh-lock-claude-code-21308)). Under the lock a
   refresher re-reads `.credentials.json` and, if the access token there
   differs from the one it held, adopts the file instead of refreshing.
   Holding the lock for steps 2–3 keeps any session from refreshing the
   old account mid-switch. Keep the hold to a few seconds.
2. **Write back.** Read `.credentials.json`, identify it with the rules
   above, and save its `claudeAiOauth` block as that account's latest copy.
   Keep the account's `oauthAccount` from `capture`; never take it from the
   live `~/.claude.json` here, which can lag the credentials (validation
   saved one account's tokens with another's profile this way).
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
   skipped; billing follows `.credentials.json`). Still needed for identity
   rule 3 and for a correct `/status`.
5. **Expect the target's refresh.** The target's saved access token has
   probably expired while it was inactive, so a session will refresh it
   and rewrite the file within moments. That is the target's own refresh:
   after it, save the file as the target's latest copy. Step 1 is what
   makes this unambiguous; without it a write here could also be the old
   account's late refresh, and the two can't be told apart from the file
   alone. If a write does contradict expectations (e.g. a session reports
   the old account in `/status`), stop, alert Owner, and do not overwrite
   anything.
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
  HTTP 200). The response has `five_hour` and `seven_day`, each with
  `utilization` (whole percent) and `resets_at`, plus model-specific and
  spend fields. Undocumented, so tolerate it changing shape. Never poll an
  inactive account's saved token (decision 3); it only stays valid until
  that account's next refresh anyway.
- **`poll` never refreshes a token.** If the live access token has
  expired (`expiresAt` passed, or the usage request returns 401), skip the
  cycle and record why; the next real request from a session refreshes it.
- **`poll` repairs the profile.** Each cycle, after identifying the live
  login, compare `oauthAccount.accountUuid` in `~/.claude.json` with that
  account and rewrite the `oauthAccount` key (same atomic method as switch
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

### Multiple hosts

Each host (laptop, cm-sessions, cm-manager) has its own `~/.claude`, so each
needs its own store and a one-time `/login` + `capture` per account. Do not
copy saved logins between hosts: each host keeps its own refresh-token
lineage, and sharing one lineage across hosts would make them overwrite
each other. A later CM command can run `switch` on every host and aggregate
`status`.

### Costs and caveats

- Every live session loses its prompt cache when the account changes (the
  same as a manual `/login` today). Switch only when needed.
- Residual policy risk as described under Background. Prefer `suggest` mode
  until Owner is comfortable.
- Saved logins are secrets: never print them, never put them in the repo,
  logs, chat or task records. Mode 0700/0600 throughout.

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
   `tests/test_cm_claude_accounts.py` (12 tests; run
   `python3 -m unittest tests.test_cm_claude_accounts`). `capture` and
   `list` checked on the laptop; a real `switch` has not run yet.
2. `poll` + state + alerts + a systemd user timer (every ~5 min).
3. `suggest` autoswitch, then `auto` behind a config flag.
4. Laptop rollout, then cm-sessions and cm-manager (one host at a time,
   with Owner present for the per-account `/login`s).
5. Optional: TUI dashboard panel; multi-host `switch`/`status` through CM.

Follow the repository workflow (branch, test, merge to `main`; no PRs) and
add a short operator section to this doc once built.
