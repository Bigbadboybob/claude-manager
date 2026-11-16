# Codex update notifications on cm-manager

The operator requested automatic update checks and Telegram alerts while keeping Codex **0.153.4** fixed for migration validation. [`scripts/codex-update-check`](../scripts/codex-update-check) reads the installed CLI version and the public npm registry's stable `@openai/codex` release. It has no installation, rollback, model-request or session-control operation.

Installed on cm-manager on **2026-09-06 at approximately 22:21 UTC**. Two real checks found installed/latest/held version 0.153.4 with no failures. A setup notification was accepted by Telegram, the daily cron entry is active, and Codex startup update checks are disabled. Existing configuration/crontab backups are in `~/.cm/backups/codex-update-check-20260906T222126Z/`. No daemon or task session was restarted.

## Installation and schedule

Install the executable as `/home/lucas/.cm/bin/codex-update-check` and configure `/home/lucas/.cm/codex-update-config.json`:

```json
{"hold_version": "0.153.4"}
```

The managed block in `lucas`'s crontab is:

```cron
# BEGIN cm-codex-update-check
0 13 * * * /usr/bin/python3 /home/lucas/.cm/bin/codex-update-check >> /home/lucas/.cm/codex-update-check.log 2>&1
# END cm-codex-update-check
```

The host uses UTC, so this runs at **13:00 UTC daily** (08:00 Chicago during daylight saving time, 07:00 during standard time). Existing cron entries are preserved. A process-local file descriptor holds an exclusive host file lock for the check and state write, so manual and scheduled invocations cannot overlap.

Check immediately using the same path:

```bash
/home/lucas/.cm/bin/codex-update-check
```

Inspect `~/.cm/codex-update-state.json` for installed/latest versions, the hold, check and last-success timestamps, failure count and notification history. State is written atomically at mode 0600. Output and failures go to `~/.cm/codex-update-check.log` under cron.

## Notification behavior

- A newer stable release sends installed/latest versions, the official changelog link, hold information and a command for a deliberate future update. Numeric version ordering handles releases such as `0.9.0` versus `0.10.0`; prerelease or unexpected package metadata is rejected.
- A successful update alert is remembered per release across subsequent checks. Failed notifications remain retryable. Delivery and local persistence cannot form a transaction: a crash after Telegram accepts a message but before state is saved can cause one repeat.
- Installed-version drift from `hold_version` sends a separate alert once per hold/installed-version pair. The checker reports drift and never automatically fixes it.
- Two consecutive failed checks produce an alert. Continuing failure is reminded at most weekly; recovery produces a recovery message. A missing/invalid configuration is a failure, not an implicit removal of the hold. Corrupt or unwritable state is preserved and reported immediately.
- All messages use the existing `~/.cm/bin/cm-notify` and its configured Telegram destination, tagged `codex-updates`. Notifier output is withheld from state/logs to avoid copying credential-bearing diagnostics.

An ordinary successful check with no new release or hold mismatch sends nothing. Checks do not invoke the Codex model or affect tasks. Failure alerts depend on the host, cron and Telegram remaining available; this checker does not provide an external heartbeat monitor. Notification transport failures are logged and retried on a later run.

## Hold and startup checks

After migration validation, change `hold_version` to the next approved baseline or set it to `null` to remove the hold. Removing the hold still does **not** enable automatic installation. Update the baseline deliberately alongside the package update.

Once the installed checker and Telegram delivery have been verified, set root-level `check_for_update_on_startup = false` in cm-manager's `~/.codex/config.toml`, preserving its `gpt-5.6-sol` model setting. This prevents headless sessions from relying on the interactive update path that cannot modify the root-owned npm installation. Codex documents this setting for centrally managed updates: [configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).

To remove the service, delete only the marked cron block and restore Codex startup checks. Keep or archive the state and log as needed. The installer should back up the existing crontab, checker configuration and Codex configuration before changing them. No daemon or session restart is required.

## Verification

Run `python3 -m unittest discover -s tests -p test_codex_update_check.py -v` for release ordering, deduplication, hold drift, error/recovery alerts, retryable delivery, file locking, corrupt/atomic state behavior, bounded registry requests and safe notifier errors. Deployment also requires a real registry check, a second check proving stable behavior, and one setup notification through the real notifier. Keep fake-release tests local; do not inject a fabricated available-version alert into production.

### CM native launch protection

The CM native Codex launcher forces `check_for_update_on_startup=false` on both
its app-server and remote terminal frontend, including fresh, resume, fork and
frontend relaunch paths. This disables fetching; it does **not** dismiss a
previously cached update menu. Immediately before every frontend process is
created, CM reads the effective `$CODEX_HOME/version.json` (literal nonempty
`CODEX_HOME`, otherwise `~/.codex/version.json`) and sets only `dismissed_version`
to the cached `latest_version`. Other cache keys are preserved. No menu-choice
keystrokes, package updates, auth/trust/approval bypasses, or user config edits
are part of this protection.

A missing home/cache, empty cache or object without a latest version starts
normally, with no cache creation or write access required. An already dismissed
notice also needs no write. Malformed, oversized (>64 KiB), suspicious/nonregular
or unreadable entries refuse startup visibly; an actual notice that cannot be
written/dismissed also refuses. The error names `version.json` and the
`dismissed_version` repair. Inspect that file in the session's effective home,
repair invalid data if needed, and set `dismissed_version` to `latest_version`
after verifying the cached version. An unwritable lock/cache is an operator
repair, never a reason to send Enter to a menu. The existing updater remains the
only update-check/installation policy; this launcher change installs nothing.

CM launches serialize updates with `.cm-version.lock` in that home (one-second
lock deadline), use regular-file/owner checks and atomic replacement, and refuse
detected concurrent rewrites. This lock coordinates CM only. An external Codex
process/updater can still overwrite the cache after verification because upstream
does not honor this lock. That residual race cannot be eliminated by a cache-only
scheme; the flags prevent CM's own processes from fetching a new notice. The
launcher does not claim that successful suppression proves the composer ready.

If a Codex launch exits nonzero within 120 seconds without a positively observed
turn start or confirmed initial prompt, the daemon captures **content-free**
`startup_failure` diagnostics before removing the session. Existing authorized
`resolve_authorized_session` and `list_sessions(include_exited=true)` reads retain
PID/start, generation, exit time/code, optional prompt receipt ID, up to 4096 bytes
inspected from the existing PTY tail, truncation/closed markers, and fixed labels
for update notice/choice, EACCES, suppression or backend-start failure. A repair
hint is included for observed update markers. No raw PTY text, prompt, credentials
or transcript is persisted. Initial-prompt confirmation carries these diagnostics
only when the receipt ID matches; `session_gone` still means unconfirmed.
MCP `read_last_turn` and `read_session_output` expose the retained diagnostic on
authorized readback too, even when no transcript was created.

Clean exits, known operator/cap kills, established turns and later exits do not
produce this startup diagnostic. A signal-only or lost exit status cannot prove
a nonzero startup failure. The tail may be empty, partial or lack the final
reader bytes; its markers are observations, not a root-cause claim. Retention is
that of existing bounded exit tombstones; diagnostics do not promise permanent
logs or a full terminal capture. CM's release operator owns live canary testing.
