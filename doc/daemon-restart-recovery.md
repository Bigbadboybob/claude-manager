# Recovering continuous tasks after a host-service restart

On 2026-09-12, unattended upgrades on cm-manager invoked needrestart after
updating libc/Python. Restarting `cm-daemon.service` stopped the holder and all
44 agent sessions. Normal brain deployments do not have this effect.

## Keep package upgrades from restarting the holder

Install the committed service exclusions on each Ubuntu session host:

```bash
sudo install -D -m0644 deploy/needrestart/50-claude-manager.conf \
  /etc/needrestart/conf.d/50-claude-manager.conf
```

This takes effect on the next needrestart invocation; no service restart is
needed. Packages still update on disk. Schedule holder/OS maintenance separately
when agents can stop, so running processes can load updated libraries.
Use [brain-only deployment](../HOWTO_HOLDER_BRAIN_SPLIT.md#3-routine-deploys-brain-code--the-weekly-case)
for routine daemon changes. Never restart the systemd unit to refresh its brain.

## A fire can be pasted without being submitted

A resumed Codex process can enable terminal modes while MCP startup is still
running. Its first Enter may be swallowed, leaving the batch in the composer.
A nonempty rollout from an earlier turn is not evidence of this fire's delivery.

CM now captures a rollout baseline immediately before the prompt body is
written and looks for a new matching user-message receipt. This covers fresh
launches, resumed/persistent fires, and `send_input`. Imported history and
startup metadata do not acknowledge a prompt. New runtime activity without an
exact receipt suppresses recovery without claiming a confirmed delivery.

If the prompt remains unconfirmed, CM retries **Enter only**, at bounded
intervals, and alerts after the confirmation window. It never clears the
composer or re-pastes the batch based on missing transcript evidence. Any new
Owner input, replacement process, retirement, or daemon quiescence ends the
recovery writes. Empty submissions and slash commands retain their own semantics.

For an existing parked prompt, inspect `read_session_output` on its owning
host and confirm the composer contains the intended batch before sending an
empty `send_input` with `submit: true`. This submits existing composer text;
do not paste another copy on top of it.

## A bridge cooldown can require a different Codex thread

The runtime error `HTTP responses session bridge is cooling down after repeated
upstream timeouts` now creates an explicit recovery hold and notification.
It is classified only from a runtime error record, never from agent prose.
Token-usage bookkeeping does not conceal the last substantive runtime event.
Unknown-tail diagnostics are limited to one per task/run per five minutes.

Inspect codex-lb for `previous_response_not_found` and continuation ownership.
A thread-bound bridge can keep rejecting retries even when other threads work.
Preserve the old rollout, reconcile the staged batch and live workers, then
replace the failed thread if needed and deliver only the work still owed.
Do not use `continuous.force_done` to clear an alert: it does not complete or
redeliver the claimed items. CM's existing reconciliation/retirement gates stay
in force; detection of a bridge error is not authorization for a blind reset.

## Sessionless workspaces and exit records

Exit tombstones preserve `continuous_task_id` through normal exits, brain
handoffs and persistence. Existing older records without the field remain
readable. The viewer also fills missing task/continuous/parent tags from the
owning daemon's existing session poll, preserving the session UID and PTY.

An open workspace whose tasks descend from a visible continuous orchestrator
stays in the Continuous column when its last session exits. It renders as a dim
`(no session)` row and supports normal workspace navigation, inspection and
closure. Shared or ambiguous workspaces stay in the main sidebar; unfinished
work is never hidden merely because its worker died. Closed workspaces stay
closed. This display change needs a laptop TUI install/relaunch; agent sessions
keep running.

## Incident evidence and verification

The original operational handoff and subsequent receipts are under
`~/.cm/handoffs/daemon-restart-recovery-20260912/` on cm-sessions. The two scraper
orchestrators were recovered before these code changes; source fixes prevent
the identified failure modes from remaining silent on future deliveries.
Nightly predictionTrading test failures predated the holder restart and are a
separate application-test issue. Use the saved logs and targeted reproductions;
do not rerun the full suite as an incident-recovery or deployment prerequisite.

Focused coverage includes a real PTY with an old rollout, a large pasted batch
and a deliberately swallowed Enter through all three delivery entry points;
operator draft protection; bridge holds across all schedule types; tombstone
round trips including legacy records; and sessionless sidebar ownership,
navigation, filtering, closure and host boundaries.
