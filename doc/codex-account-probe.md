# Codex account probe and continuous-task recovery

Status: producer, strict reader, guarded reconciliation/recovery and pool support deployed on cm-manager, 2026-09-07 UTC. The external probe runs every 30 minutes. Companion to the [Codex migration plan](continuous-codex-migration.md), which retains the separate live lifecycle gate before production cutover.

## Deployed pool support

The producer accepts the built-in `openai` provider or the explicitly validated `cm_pool` route at `http://127.0.0.1:2455/backend-api/codex`, using Responses, WebSockets and an absolute executable command-auth helper. Arbitrary custom providers and overrides of built-in OpenAI still fail closed. Additive optional schema-1 fields `provider_config` and `auth_command_hash` bind the nonsecret route definition and helper contents. The daemon independently checks the actual host TOML and helper hash against both configuration and successful state. Missing or changed pool evidence cannot release a hold. Credentials are excluded.

Isolated probe overrides must follow the `exec` subcommand with CLI 0.153.4; root overrides preceding `exec --ignore-user-config` were observed to be ignored. The installed producer completed a real pooled `gpt-5.6-sol` request and published `OK` at epoch `1788754076.024022`, with bound rollout provider/model/version evidence.

An actual CLI 503 `No available accounts` error produces `POOL_UNAVAILABLE`, distinct from an individual account limit. The scheduler closes only the matching ended run and records a durable recovery hold, preserving workers and staged work. This error cannot distinguish exhausted capacity from unavailable continuation ownership. Unknown proxy failures remain conservative errors. Recovery requires post-hold pool proof, explicit work reconciliation and settled obligations before guarded retirement/replay; successful inference alone cannot authorize replay. Drain blocks automatic retirement, replay and admission.

The full daemon suite passed 1,241 tests with four existing ignores; ten producer tests include provider/helper drift and the installed CLI's pool-error fixture. Backups and deployed hashes are recorded in `~/.local/share/cm-codex-lb/migration-support.json`. The following initial implementation checkpoint is retained as historical context; its statements about pending deployment/recovery are superseded by this section.

## Implementation checkpoint

[`scripts/codex-usage-probe`](../scripts/codex-usage-probe) now runs the pinned native executable against the saved CLI account, with a fixed response and bound rollout evidence. The implementation uses a nonblocking `fcntl.flock` internally for both cron and manual entry, holds it through atomic publication, and kills only the disposable request's process group on timeout. A second external flock around the same lock file is unnecessary and would prevent it running. Its cron entry should invoke the script directly.

Initialize a host pin with `--configure --executable /absolute/path/to/native/codex --model gpt-5.6-sol`. Configuration lives in `~/.cm/codex-probe-config.json`; it binds the native binary SHA-256 (not npm's JS launcher), CLI version, target model, Codex home, current host model and built-in OpenAI provider. Custom providers currently fail closed. Runtime/configuration changes invalidate prior evidence and require an explicit new pin.

The script ignores user config/rules for its isolated request, disables project instructions, shell and multi-agent tools, strips CM attribution and per-invocation API-key/base-URL overrides, and uses the configured Codex home's saved authentication. It verifies the exact reply, completed JSON event, matching thread/rollout, CLI/provider and `turn_context.model`; a tool invocation or missing model evidence cannot produce `OK`. Rollouts are retained because `--ephemeral` omits the model evidence needed by this contract. The invocation follows the [official noninteractive CLI documentation](https://learn.chatgpt.com/docs/non-interactive-mode) and was checked against the installed CLI help.

[`codex_account.rs`](../daemon/src/continuous/codex_account.rs) validates schema, explicit times, 61-minute freshness, post-hold start, runtime hash, provider/host-model configuration and observed model. Claude's reader remains unchanged. [`codex_probe.rs`](../daemon/src/continuous/codex_probe.rs) and [0.153.4 fixtures](../daemon/tests/fixtures/codex-0.153.4/README.md) cover actual success and loopback-generated runtime errors. Codex account failures now remain held and report the Codex probe path. Unknown tails do not authorize a close. A Codex consumer wedge can close only after known worker/monitor obligations settle, and records a durable `recovery_hold` that blocks scheduled/manual claims and supervision.

**Recovery and deployment remain incomplete.** The scheduler can read fresh Codex account evidence but deliberately does not yet replay or retire a Codex run. Per-item reconciliation, release of `recovery_hold`, guarded retirement, and their race tests remain required; this checkpoint cannot authorize production migration. The standalone script was staged under `/home/lucas/.cm/migrations/continuous-codex/probe-check-4yb2cjea/` on cm-manager and completed a real `gpt-5.6-sol` request with CLI 0.153.4. Its configuration/state are confined to that review directory. The final staged invocation returned `OK` at epoch `1788741188.5185492`, with native SHA-256 `56ef98ab4032d317ab26e9b5e5a175650717351edb16ed9cde0cb6d1734d62da` and matching host/observed model `gpt-5.6-sol`. No cron entry, default probe state/configuration, daemon deployment or production-task change was made.

Validation: eight script tests cover success, missing/mismatched model/completion, unexpected tools, structured errors/redaction, changed runtime/provider/host model, process-group timeout cleanup, lock contention, newer failure superseding success, and failed atomic replacement. Rust reader/parser/scheduler tests cover stale/wrong-runtime evidence, explicit account failures, unknown tails, normal Claude behavior, and consumer holds. Full CM lifecycle validation remains a separate canary gate.

## Architecture

Follow the existing Claude architecture: an **external scheduled probe produces a state file; the daemon consumes it**. The read-only cm-manager check on 2026-09-06 found:

```cron
*/30 * * * * flock -n /home/lucas/.cm/claude-probe.lock /home/lucas/.cm/bin/claude-usage-probe >/dev/null 2>&1
```

The daemon's [`account_recovery_proven`](../daemon/src/continuous/scheduler.rs) reads `~/.cm/claude-probe-state.json`; [`usage_probe_ok_after`](../daemon/src/continuous/probe.rs) requires an `OK` result newer than the hold. It does not schedule that producer. The script's absolute path matters because the installed script is not on the inspected login shell's PATH.

Add a versioned `scripts/codex-usage-probe`, install it as `/home/lucas/.cm/bin/codex-usage-probe`, and add a cron entry invoking the script, which acquires the separate `codex-probe.lock` internally. The proposed initial cadence is the same **30 minutes** as Claude. An explicit operator check can run sooner under that same lock. Automatic recovery waits for the next successful scheduled check and the daemon's next recovery poll.

The daemon reads the result during its existing throttled account-recovery pass. It owns no probe worker, timer, subprocess registry or restart/orphan protocol. A brain restart does not create another probe or restart the external schedule. Preserve the existing Claude script, cron entry and reader semantics throughout mixed-engine migration.

## Producer behavior

Run one bounded noninteractive Codex request using the task host's user, pinned executable, authentication/provider configuration and expected model. On cm-manager the expected model is **`gpt-5.6-sol`**. This is host deployment configuration, not a new Rust enum default or local model pin.

Use a scratch directory without task/project instructions or CM MCP tools, ask for a fixed response, and capture structured completion/model evidence. Validate the exact invocation and configuration-isolation flags against the CLI pinned by migration Phase A. Do not run a production task prompt as a probe. A model-list entry, valid-looking credential file, CLI start or zero exit code alone is insufficient evidence of an authenticated model completion.

Both scheduled and manual checks use the script’s same nonblocking `fcntl.flock`. Keep the lock through the entire request and atomic result write. A competing invocation skips without overwriting the last result. Apply a proposed 60-second timeout to the disposable probe and its child process group; an unhealthy model must not hold the lock indefinitely. Timeout handling never signals a CM task session.

The cron cadence bounds ordinary retries; the daemon does not launch an immediate retry on each held task or failed read. An external probe may continue through a brain restart. If the host or script fails before publishing a result, the reader treats missing or stale evidence as no recovery proof.

## State-file contract

Write `~/.cm/codex-probe-state.json` atomically using a temporary file in the same directory, file sync and rename, restricted to the owning user. Keep the previous complete file visible until replacement succeeds. Publish failure outcomes as well as successes; a failed newer check must supersede an older success.

Proposed version-1 fields:

| Field | Meaning |
|---|---|
| `schema_version` | Reader-supported schema version, initially `1`. |
| `engine` | `codex`; prevents accidental acceptance of Claude evidence. |
| `status` | `OK`, `AUTH_EXPIRED`, `USAGE_LIMITED`, `POOL_UNAVAILABLE`, `MODEL_UNAVAILABLE`, `TIMEOUT`, or `ERROR`. |
| `started_at`, `checked_at` | Probe start and completed check time as finite UTC epoch seconds. |
| `executable`, `cli_version`, `executable_hash` | Runtime actually invoked, matched to the deployment manifest. |
| `requested_model`, `observed_model` | Configured target and model established by runtime evidence; absent observed model cannot yield `OK`. |
| `codex_home` | Saved CLI authentication home bound by the host pin; no credential contents are copied. |
| `configuration_id` | Nonsecret identity of the intended provider/runtime configuration, excluding credential contents. |
| `detail` | Bounded, redacted diagnostic sufficient to distinguish timeout, auth failure and unsupported model. |

Return `OK` only after a completed real request with the expected model and validated response. Use actual structured Codex errors from the tested version to classify authentication/usage failures. Model-unavailable errors require a configuration fix and must not masquerade as a successful login. Ordinary assistant prose is not an account-error signal. Never serialize credentials or raw sensitive request headers into the state file or logs.

Use a Codex-specific strict reader for this schema. The inspected installed Claude state has no numeric `checked_at` and relies on its reader's legacy mtime fallback; preserve that compatibility for Claude and require explicit timestamps for the new Codex contract. Missing fields, unsupported schemas, malformed or nonfinite timestamps, future timestamps, mismatched model/runtime/configuration, and non-`OK` outcomes fail closed. As a proposed freshness bound, reject a result older than two scheduled intervals plus the request timeout (61 minutes at the initial settings); record the deployed interval and bound together. These proposed timing values are deployment choices, not existing scheduler settings.

## Scheduler recovery

Implement engine-aware account-error detection and Codex rollout-tail/wedge parsing in migration Phase B4. This probe supplies positive recovery evidence for a detected hold; it does not detect missing `report_done` itself.

For an active Codex hold, accept only a successful Codex check **started after** the block was detected, matching the current expected executable/model/provider configuration. Revalidate task engine, run sequence, session UID and hold identity under the task lock before any mutation. A Claude success, old Codex success, configuration change or stale run snapshot cannot release the hold. Keep loading existing Claude hold/probe records with their current compatibility rules.

Before releasing a consumer hold, reconcile the staged batch with processed and unfinished item outcomes. Delivery-time acknowledgment is not proof of processing. Recover only unfinished work using original deduplication keys; preserve completed outcomes. Missing, corrupt or ambiguous evidence keeps the hold in place. Do not requeue completed work simply because an account check succeeded.

Integrate the [graceful-drain contract](continuous-task-drain.md): `auth_wedge_pass` must observe draining runs despite `paused`, but successful probe evidence cannot unpause, spawn, claim another batch or retire an active draining session. Expose recovery readiness/blockers and finish work reconciliation under that contract. Outside drain, use the guarded existing retirement/recovery lifecycle only for the matched poisoned run, preserving normal completion if it wins a race.

Alerts identify Codex, the outcome, the evidence time and whether the external check is missing/stale or the account remains unavailable. Do not direct a Codex operator to `claude-probe-state.json` or imply that a daemon restart runs another probe.

## Validation and deployment

Test the script with a controlled CLI executable for success, structured account/model errors, timeout and child cleanup, lock contention, missing metadata and a failed atomic write. Verify a failure replaces earlier success and no credentials appear in emitted state. Test the reader and recovery transitions with stale/wrong-engine proof, timestamps, runtime/model mismatch, existing Claude records, concurrent completion/drain, and partially processed batches.

Record the actual Codex CLI pin, script revision, cron line and state schema in the migration host manifest. Install the script and consumer together; verify cron ownership/environment and one explicit locked invocation. Phase C must show a real successful `gpt-5.6-sol` request with the pinned remote CLI and acceptance of its published result. Do not manufacture account-failure fixtures by exhausting or invalidating production credentials.

This producer and its reader/recovery integration are required before production migration, with an explicit hard gate before Wave 3 consumers. A configured model line or a parser-only implementation does not satisfy that gate.
