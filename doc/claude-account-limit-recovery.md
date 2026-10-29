# Claude account-limit recovery

The daemon can resume an ordinary managed Claude session after a synthetic
`rate_limit` failure when the account UUID changes or the failure's structured
`quotaLimits.resetsAt` has passed. It checks the current native session and
transcript and requires no later user input. A ready, identity-matched native
channel receives one durable continuation event per session and error-record
UUID. The consumer checks the same error and later input immediately before
claiming it; a superseded event is cancelled. Sessions are processed
sequentially. Codex, workflow participants, continuous-task runs,
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
events; submitted and uncertain receipts are retained.

PTY delivery is a fallback only when a fresh, identity-matched transport report
proves the native channel is unavailable and there are no live background jobs
or crons. It requires an empty composer and holds the writer, geometry,
lifecycle and native notification guards through the paste and 1.5-second
submit gap. A newly observed background event during that gap withholds Enter;
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
