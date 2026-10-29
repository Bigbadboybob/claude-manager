# Claude account-limit recovery

The daemon can resume an ordinary managed Claude session after a synthetic
`rate_limit` failure when the account UUID changes or the failure's structured
`quotaLimits.resetsAt` has passed. It checks the current native session and
transcript, requires no later user input and an empty composer, and sends a
single continuation prompt per session and error-record UUID. Sessions are
processed sequentially. Codex, workflow participants, continuous-task runs,
and sessions with no manager are excluded; the continuous scheduler owns its
own recovery. Unmanaged sessions are logged for the operator.

Set `claude_limit_recovery = false` in `daemon.toml` and run
`daemon.reload_config` to disable new admissions. The reload is serialized
with an admitted terminal write. An account fingerprint is retained when a
failure is observed; token refresh alone is not an account switch. When the
brain was down across a switch, or the initial account identity was unavailable,
the switch cannot be proven. Recovery then waits for a valid structured reset;
without one it stays unarmed.

An attempt is persisted before terminal input. A crash or an uncertain write
is never replayed automatically. A `[cm-recovery]` system notice aggregates
the result for each managing session; it reports bytes written without claiming
the agent started a new turn. Transcript identity ambiguity is reported as
`could not check`. Retained record IDs prevent duplicate attempts and have no
time-based expiry. Input serialization holds the writer, lifecycle and native
notification guards through the paste and its 1.5-second submit gap. A busy
composer, changed transcript or human input defers recovery.

The empty-composer check deliberately fails closed when terminal evidence is
missing. A fixture from the deployed Claude limit screen has not yet been
captured; isolated synthetic screen tests do not prove that all deployed CLI
versions render an accepted composer.
