# Claude account-limit recovery

The daemon can resume an ordinary managed Claude session after a synthetic
`rate_limit` failure when the account UUID changes or the failure's structured
`quotaLimits.resetsAt` has passed. It checks the current native session and
transcript and requires no later user input. A ready, identity-matched native
channel receives one durable continuation event per session and error-record
UUID. The consumer checks the error, later input, manager, scope and binding at
claim; a superseded event is cancelled. At most one event or PTY write is
admitted per two-second polling pass. Codex, workflow participants, continuous-task runs,
and sessions with no manager are excluded; the continuous scheduler owns its
own recovery. Unmanaged sessions are logged for the operator.

Set `claude_limit_recovery = false` in `daemon.toml` and run
`daemon.reload_config` to disable new admissions. The reload is serialized
with an admitted terminal write. An account fingerprint is retained when a
failure is observed; token refresh alone is not an account switch. When the
brain was down across a switch, or the initial account identity was unavailable,
the switch cannot be proven. Recovery then waits for a valid structured reset;
without one it stays unarmed.

An attempt is persisted before publication or terminal input. A crash or an
uncertain effect is never replayed automatically. A `[cm-recovery]` system
notice aggregates each manager's results after receipts settle or at ten
minutes, when remaining events are labelled pending and unconfirmed. It never
claims that the agent started a new turn. Transcript identity ambiguity is
reported as `could not check`. Retained record IDs prevent duplicate attempts
and have no time-based expiry. Disabling recovery retracts pending native
events; submitted and uncertain receipts are retained. A native send past its
final consumer check may complete after disable returns; its receipt is retained.
Transient admission refusals retry until the ten-minute notice bound, then are
reported as pending and delivery unconfirmed.

Recovery notices use a version-2 envelope, which older MCP consumers leave
pending. After ten minutes, an unclaimed version-2 notice is reported as
`pending (delivery unconfirmed — consumer may need MCP reconnect)`; the new
consumer rechecks the session and record when it claims the notice.

An unreadable recovery journal is quarantined and recovery remains disabled
until a clean journal is supplied. Quarantined data is retained; records dated
at or before quarantine remain ineligible because their old one-shot fences
cannot be trusted. A recovery configuration failure leaves recovery disabled
while other hot settings still apply; reload returns a warning, including when
it cannot confirm that the disabled marker was written.

To restore recovery after quarantine, first disable it in the config and reload;
confirm the disabled marker in the reload result. Preserve the quarantined
journal and inspect any unreadable notification event: one unreadable event can
prevent cancellation and re-enablement. If an event must be moved aside for
repair, keep its contents and receipt for inspection. Supply a clean journal
named `daemon-claude-account-recovery.json` beside `daemon-sessions.json`, with
contents `{"version":1,"episodes":{}}`, then enable recovery and reload again.
Do not delete `daemon-claude-account-recovery.quarantined`: it preserves the
cutoff and causes old pending recovery events to be cancelled before recovery
is enabled. Records at or before that cutoff cannot be retried automatically.

If the journal is corrupt and the disabled marker cannot be written at startup,
a previously pending event can still make its first delivery. No duplicate is
created, but the operator should inspect retained receipts before recovery.

PTY delivery is a fallback only when a fresh, identity-matched transport report
proves the native channel is unavailable and there are no live background jobs
or crons. It requires an empty composer and holds the writer, lifecycle and
native notification guards through the paste and 1.5-second
submit gap. A resize or newly observed background event during that gap withholds Enter;
the partial input is uncertain and is never replayed. A busy composer or failed
safety check is reported to the manager for manual recovery and is not tried
again until another account switch or reset edge.

Claude's own background completion queue is outside CM's locks. A native event
can race a job completion after the final check and be queued immediately after
that completion; the accepted result can be one additional turn. Neither event
modifies a draft composer. The exact outcome is not asserted until receipt
evidence arrives.

The empty-composer check deliberately fails closed when terminal evidence is
missing. A fixture from the deployed Claude limit screen has not yet been
captured; isolated synthetic screen tests do not prove that all deployed CLI
versions render an accepted composer.

The explicit one-admission-per-pass cap applies to both delivery routes. The
worker's older source comment about staggering through the PTY settle gap
predates that cap; it does not describe native delivery's pacing.

## Rollback

Before rolling back to a brain that predates version-2 recovery envelopes,
set `claude_limit_recovery = false` and reload. Confirm the disabled marker and
keep recovery disabled throughout the sweep. Run `scripts/cm-recovery-rollback-sweep`
first without `--apply` to list affected files. Then run it with `--apply
--backup <directory>` to move version-2 recovery events into a labelled backup
outside the notification tree; use the same filesystem as the queues. The
helper takes each queue lock, skips `submitting` events, and exits nonzero if
any file is skipped or cannot be moved. Wait for in-flight submissions to
settle and repeat until the sweep succeeds before rolling back. Keep the backup
and its receipts for review.

On a later roll-forward, do not restore those files into the live queues. A
journal that references a moved event reports `native publication outcome
unknown; no automatic retry`, preserving the one-shot fence.
