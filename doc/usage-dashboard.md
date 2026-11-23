# Usage dashboard: Claude + Codex accounts in the TUI (design, 2026-11-23)

Owner asked (11-23 ~04:30Z) for a TUI panel that shows Claude and Codex account availability together, plus historical usage: total tokens and tokens over time. Lead: Claude-Accounts (account manager, cm-manager). This file is the contract between the phases; the P1 worker commits a copy as `doc/usage-dashboard.md`.

## What exists today

- **Claude:** `cm-claude-accounts` (repo `scripts/cm-claude-accounts`, installed `~/.local/bin/cm-claude-accounts` on every host; installed = `origin/main` dcf1bb1). Hosts (cm-manager, cm-sessions, f-sessions, laptop) `checkin` to the hub on cm-manager over ssh every ~2 min. The hub keeps only the **freshest** reading per account (`hub status --json`); there is no usage history and no token counts.
- **Codex:** each cloud host runs codex-lb (Soju06/codex-lb) as `cm-codex-lb.service`, store at `~/.local/share/cm-codex-lb/data/store.db` (SQLite). It already keeps `usage_history` (account, window primary/secondary, used_percent, reset_at, recorded_at) and `request_usage_hourly_rollups` (bucket_epoch, account_id, model, input/output/reasoning/cached_input tokens, request_count, cost_usd) back to 2026-09-07. The `accounts` table has encrypted token columns: **never select them**; select named columns only.
- **Claude transcripts:** `~/.claude/projects/**/*.jsonl` (incl. `subagents/`). Assistant lines carry `message.usage` (`input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`), `message.id`, `requestId`, `timestamp`, `message.model`. One API response appears on several lines (one per content block) with the same `requestId`/`message.id`: dedupe on that pair, keep the last line's usage.

## Phases

### P1. Collect and keep history (Python, `scripts/cm-claude-accounts`)

Host side, added to the existing check-in export under a new `usage` key (bounded size, no secrets):

1. `usage.codex`: if the local codex-lb store exists (path configurable, default above), open it read-only (`mode=ro` URI). Send: accounts (id, email, alias, status, plan_type); the latest `usage_history` row per account and window; hourly rollups with `bucket_epoch` greater than the hub's acknowledged watermark for this host (the hub returns it in the check-in reply), grouped by bucket, account and model. First run backfills everything codex-lb has, at most N buckets per check-in.
2. `usage.claude_tokens`: incremental scan of this host's Claude transcripts. Keep per-file (inode, offset) in state; read at most ~32 MB per check-in so check-ins stay fast; backfill proceeds over several runs. Bucket by hour and model. Attribute each bucket to the account that was active on this host at that time (from the host's switch history); unknown → `account: null`. Send only buckets newer than the hub's watermark plus the current open bucket (resent until closed).
3. Off switch: config `usage_collect` (default on for cloud hosts, **off on the laptop** until claudelb-local reviews the laptop-side change).

Hub side:

4. Store in SQLite under the hub dir (`usage.db`): `limit_readings(engine, account, limit_kind, percent, resets_at, at, source_host)` from every fresh Claude reading and every Codex usage row (dedupe on (engine, account, limit_kind, at)); `token_hours(engine, host, account, model, bucket, input, output, cache_read, cache_write, reasoning, requests, cost_usd)` upserted on (engine, host, account, model, bucket). Keep raw readings 30 days, then one row per hour (max percent); keep token hours indefinitely (tiny).
5. New command `cm-claude-accounts hub usage --json [--since EPOCH] [--granularity hour|day]`. Output, schema version 1:
   - `generated_at`, `schema: 1`
   - `accounts`: one entry per account across both engines: `engine` (`claude`/`codex`), `email`, `alias`, `limits` (`kind`, `percent`, `resets_at`, `read_at`), `active_on` (hosts), `reserved_for`, `room` (Claude only, as in `hub status`), `status` (Codex: active/paused/...), `pool_host` (Codex: which host's pool).
   - `limit_history`: per account and limit kind, a list of `[at, percent]`, downsampled to at most ~200 points for the window.
   - `tokens`: `totals` (all time, 30d, 7d, 24h) by engine, by host, by account; `series` as a list of `{bucket, engine, host, account, input, output, cache_read, cache_write, reasoning, requests}` at the requested granularity.
   - Never tokens or secrets; emails and aliases only.
   Binding example (field names exact; P1 produces it, P2/P3 consume it; extra fields allowed, none renamed):
   ```json
   {"schema": 1, "generated_at": 1795400000, "since": 1794795200, "granularity": "hour",
    "accounts": [
      {"engine": "claude", "email": "a@example.com", "alias": null, "status": null, "pool_host": null,
       "active_on": ["cm-sessions"], "reserved_for": ["f-sessions"], "room": true,
       "limits": [{"kind": "five_hour", "percent": 38.0, "resets_at": 1795410000, "read_at": 1795399800},
                  {"kind": "weekly_all", "percent": 10.0, "resets_at": 1795580000, "read_at": 1795399800},
                  {"kind": "weekly_fable", "percent": 7.0, "resets_at": 1795580000, "read_at": 1795399800}]},
      {"engine": "codex", "email": "b@example.com", "alias": "802", "status": "active", "pool_host": "cm-sessions",
       "active_on": [], "reserved_for": null, "room": null,
       "limits": [{"kind": "primary", "percent": 4.0, "resets_at": 1795405000, "read_at": 1795399700},
                  {"kind": "secondary", "percent": 30.0, "resets_at": 1795900000, "read_at": 1795399700}]}],
    "limit_history": [{"engine": "claude", "email": "a@example.com", "kind": "weekly_all",
                       "points": [[1795300000, 2.0], [1795399800, 10.0]]}],
    "tokens": {
      "totals": [{"window": "24h", "engine": "claude", "host": null, "email": null,
                  "input": 1, "output": 2, "cache_read": 3, "cache_write": 4, "reasoning": 0, "requests": 5}],
      "series": [{"bucket": 1795392000, "engine": "codex", "host": "cm-sessions", "email": "b@example.com",
                  "input": 1, "output": 2, "cache_read": 3, "cache_write": 0, "reasoning": 1, "requests": 4}]}}
   ```
   `totals` rows: `window` in `24h`/`7d`/`30d`/`all`; `host`/`email` null means "all" (one row per engine with both null, then per-host rows with email null, then per-account rows with host null). Limit kinds: Claude uses the tool's existing kind names (map `session`→`five_hour`, `weekly_all`, a scoped weekly → `weekly_<scope>` lower-case); Codex uses `primary`/`secondary`. Codex `cached_input_tokens` → `cache_read`. Percent is 0-100.
6. `hub status --json` stays unchanged (Swarm-Coord parses it). Tests in `tests/test_cm_claude_accounts.py`, run with `HOME=$(mktemp -d) CM_NOTIFY_ENV=/nonexistent`; fixture transcripts and a fixture codex-lb SQLite (schema columns as listed above).

### P2. Daemon RPC (`daemon/`)

7. `usage.snapshot {since?, granularity?}` on the daemon: runs the fixed argv `python3 ~/.local/bin/cm-claude-accounts hub usage --json [...]` (no shell, validated args, 15 s timeout), caches the result for 30 s, and returns it verbatim. Operator-gated (the TUI's per-host operator token), read-only. Only meaningful on the hub host (manager); elsewhere it answers `not_hub`. Brain-only deploy on cm-manager.

### P3. TUI panel (`tui/`)

8. **F5** opens the Usage overlay in every view (F6/F7/F8/F9 are taken; F10 is often eaten by terminals). Esc/F5/q close. Polls `usage.snapshot` on the `manager` host only while open, every 60 s; shows data age and an error line if the host is unreachable.
9. Tabs (Tab / 1-3):
   - **Accounts**: Claude and Codex sections. Per account: email (or alias), 5h and week bars (Fable and Codex primary/secondary as their own bars), reset countdowns, host(s) using it, reservation, room/NO ROOM, and a 7-day sparkline of the binding limit. Sort: in use first, then most room. Summary line: accounts with room per engine.
   - **Tokens**: totals (24h / 7d / 30d / all) by engine; a bar chart over time (`h`/`d` toggles hour/day; `e` cycles engine all/claude/codex; `g` groups by account or host), input+output stacked, cache shown separately.
   - **History**: select an account (j/k), chart of its limit percentages over the last 7 days with the 98% switch line.
10. Built from a fixture JSON first (unit-tested render), then wired to the RPC. Update `CLAUDE.md` keybinding list and the switcher doc.

### P4. Rollout

11. Install the tool on cm-manager, cm-sessions and f-sessions from one commit (as in `doc/claude-account-switcher.md`); verify `hub usage --json` fills over a few check-ins. Brain-only restart on cm-manager for P2 (HOWTO_HOLDER_BRAIN_SPLIT.md §3). TUI release via the `cm-tui-release` runbook. Laptop collection after claudelb-local's review. Advance and follow-up notices in `#cm-general`; tell Swarm-Coord that the hub now reads codex-lb stores read-only.

## P1 implementation details (2026-11-23)

- `usage_collect` is on by default except for the host named `laptop`; set it
  explicitly to `true` or `false` with `config`. `codex_lb_store` overrides the
  default local SQLite path. A collection error is silent and does not stop a
  check-in. A hub which does not return usage acknowledgements causes retries.
- Claude collection keeps per-file inode/offset state and a local SQLite cache
  of request IDs and counts, never transcript content. A completed bucket can
  be revised by a late transcript line; an acknowledgement includes its local
  revision so that revision is retransmitted. Files predating the retained
  account history have unknown attribution. Lines larger than 32 MiB are skipped
  incrementally; incomplete final lines are retried. At most 32 MiB is read per
  check-in, and the usage payload is capped at 180 KiB.
- Codex backfill uses a bucket watermark and a composite cursor so an hour with
  more than 300 account/model rows can span check-ins. Up to 50 open-hour rows
  are resent; a larger open hour completes through the closed-hour backfill.
  Collection skips the Codex part if there are over 500 accounts or 1,000
  latest limit rows, to keep exports bounded. SQLite is opened with `mode=ro`;
  authentication columns are never selected.
- The snapshot defaults to the last seven days and hourly series. Totals use
  hourly bucket start times for window boundaries, and `all` starts at epoch
  zero. Unattributed token rows use null account/email and contribute to engine and host totals; they do not produce a per-account total, since null there means all accounts. Series include both `account` (as in the binding example) and the additive `email` alias for consumers. Old limit readings are
  compacted to the highest recorded percentage per hour, retaining that
  reading's original timestamp and reset. History is sampled uniformly in
  time-order to at most 200 points per account and kind.
