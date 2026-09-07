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
