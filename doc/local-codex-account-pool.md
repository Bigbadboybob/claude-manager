# Local Codex account pool

Installed on `pop-os` on September 7, 2026 at the operator's request. The operator chose an independent local pool so model traffic from the laptop goes directly to OpenAI and remains independent of cm-manager availability. The remote pool continues serving cm-manager's tasks.

## Status

codex-lb **1.24.0** is installed in its own Python 3.13 environment and runs as the enabled user service `cm-codex-lb.service`. Its dashboard and proxy listen only on **127.0.0.1:2455**. The local OAuth callback uses **127.0.0.1:1455** when a login is in progress. Telemetry is disabled; routing is capacity-weighted with sticky threads enabled. Proxy requests require a separate local API key.

Both accounts were enrolled with fresh local sign-ins and report active. Installation checks passed: readiness, dashboard and 21 assets, and rejection of unauthenticated proxy requests. A real Codex request returned `LOCAL_CODEX_POOL_OK`; its rollout verifies **CLI 0.153.4 / local_pool / gpt-6-astra**, with the user's existing `xhigh` reasoning setting. Local key usage records also confirm the request reached this instance.

The `local_pool` provider is now active in `~/.codex/config.toml` for new local sessions, pointing to `http://127.0.0.1:2455/backend-api/codex`. All pre-existing configuration values were preserved exactly, including the user's model and reasoning settings. Setup changes the provider, not the model default. Existing sessions were not interrupted. The original configuration and runtime proof are saved privately under `~/.local/share/cm-codex-lb/`.

## Resuming chats created before the switch

Verified with CLI **0.153.4** on September 7: saved chats retain their provider ID. A normal resume of an `openai` chat still selects `openai`, even when the current default is `local_pool`. The resume picker also filters by provider; `--all` removes the directory filter, not the provider filter. The old transcripts remain under `~/.codex`. The TUI's `/status` endpoint display alone is insufficient evidence of the resumed thread's route: the setup session displayed the local endpoint while its failed requests never reached the local LB.

For a lasting migration, use Codex's native **fork** with the destination provider explicitly selected:

```bash
# Get SESSION_ID from /status in the old chat, then run from its workspace.
codex fork -c 'model_provider="local_pool"' SESSION_ID
```

This creates a new thread with the old conversation history and a saved `local_pool` provider. Send a short connection check; after its first user turn the migrated copy appears in the normal local-pool `/resume` picker. Future `codex resume NEW_SESSION_ID` calls retain the local route. The model and reasoning settings are inherited. The original thread remains available; keep its files, because Codex's paginated forks reference their source history.

To browse the older direct-provider chats, use `codex resume --all -c 'model_provider="openai"'`. This is a discovery/direct-resume command, not a migration. An app-server `thread/resume` with `modelProvider="local_pool"` can route one running instance through the pool, but does **not** change the saved provider; use `thread/fork` or the CLI command above for a persistent migrated copy. No transcript or SQLite edits are necessary.

The setup chat was migrated in CM's existing sibling slot `ts-18d2d5011b02fb7f-e`, now resumed on **`01a07cc9-8448-71e3-a7de-e74d73af1d48`**, named **Codex LB setup (local pool)**. Its source is `01a07863-d327-75b1-a238-58f15ff7a8c6`. Native history pagination returned all **2,550 identical history items** before the new verification turn. The reopened CM session returned `LOCAL_POOL_MIGRATION_OK.` and the local LB recorded successful requests for its new thread ID with `gpt-6-astra` / `xhigh`. The copy appears in the default provider-filtered thread list. Backup and verification evidence are private under `~/.local/share/cm-codex-lb/session-migration-20260907T165020Z/`.

CM UI caveat: a daemon-side `session.revive` can leave the TUI's old terminal attachment marked exited even though the replacement process is live and both manifests hold the new transcript ID. Select that sibling and press **Alt+Shift+R** (A-R). The local revive action probes live UIDs and reattaches to the existing process when the old attachment is exited. Direct daemon input and transcript reads verified the migrated session; this final TUI reattachment requires the UI action.

### Remaining CM sessions recovered September 7

At the owner's request, recovered **SEJD Coordinator, Forecast, Central Overnight, Delivery, Shapes, Sampling, Perf, Messaging Builder**, and the additional **codex session in triage-tasks**. All nine passed real local-pool connection checks. Their original CM UIDs, labels, task/workspace bindings, permissions, and `gpt-6-astra` / `xhigh` settings were preserved. All **11 live local CM Codex sessions** now use `local_pool`; both CM manifests contain the migrated IDs, and all nine copies appear in the default provider-filtered picker. Each restored session is idle after its check.

The fresh-screen cases still had their original histories on disk. Sampling's CM slot pointed to a test-only thread; Perf's pointed to an unrelated August transcript. Recovery selected the original conversation for each slot instead of trusting those bindings or choosing the newest thread in a shared directory.

Shapes and Delivery also contained duplicate rollout ordinals, which made native forks fail and left their history indexes missing later turns. Separate recovery copies received new identities and sequential ordinals; every original record payload and timestamp was retained, with only the recovery copy's metadata identity changed. Native forks of those copies then indexed the latest turns successfully. The original nine transcript files remain byte-for-byte unchanged. Native history comparisons verified **21,586 history items** across the nine migrated chats, including their full history prefixes after the connection checks. Keep the original and recovery-source files: the migrated forks reference them.

Private backups, per-session identity mappings, recovery checks, and final verification are under `~/.local/share/cm-codex-lb/batch-session-migration-20260907T171208Z/`, with a consolidated `summary.json`. Follow-up bugs to investigate separately: CM's fresh-session transcript detection selecting an unrelated history, and Codex's duplicate-ordinal writes on resume. No daemon restart or remote continuous-task changes were needed.

## Dashboards and sign-in

| Pool | Bookmark | Application-menu shortcut |
|---|---|---|
| Laptop | <http://localhost:2455> | Codex Accounts - Local |
| cm-manager | <http://localhost:2456> | Codex Accounts - cm-manager |

The remote dashboard uses the enabled, reconnecting user service `cm-codex-pool-tunnel.service`. The old manually launched SSH tunnel is no longer needed. The laptop pool does not depend on that tunnel.

In the **local** dashboard, choose **Accounts → Add account → OAuth → Browser (PKCE)** and sign into each account separately. Use a private browser window when necessary to select the other account. Do not import cm-manager's stored credentials or clone its database. The installations share the accounts' upstream quota, while their request logs and routing state remain separate.

The local pool has not imported the existing CLI login either. That login was left untouched for direct recovery sessions. Both sign-ins, the real completion and the guarded provider activation are complete. Fresh sessions use the local pool; existing sessions finish normally under their prior configuration. The local service makes its own upstream requests with upstream proxy routing disabled and does not use cm-manager's proxy.

## Files and operations

| Item | Local path |
|---|---|
| Service | `~/.config/systemd/user/cm-codex-lb.service` |
| Private service environment | `~/.config/cm-codex-lb/service.env` |
| Local proxy key | `~/.config/cm-codex-lb/proxy.key` |
| Credential helper | `~/.local/bin/cm-codex-local-pool-token` |
| Runtime | `~/.local/share/cm-codex-lb/runtime/1.24.0/` |
| Database and encryption key | `~/.local/share/cm-codex-lb/data/` |
| Setup state, verification, original/prepared configuration | `~/.local/share/cm-codex-lb/` |

The successful verification thread is `01a07c96-647c-7e50-8085-397c989881e6`, completed at approximately 15:57 UTC on September 7. Its private `verification.json` records the endpoint, observed model/provider/CLI, response and rollout path. This proves a real pooled completion; it is not a claim that every quota-exhaustion or account-failover path was exercised on the laptop.

Never run the credential helper in captured output. Keep the data directory private; a recoverable backup needs the database and encryption key together. Finish active requests before deliberately restarting or updating the pool. Runtime updates are pinned and manual for now.

```bash
systemctl --user status cm-codex-lb
curl --fail http://127.0.0.1:2455/health/ready

# Open a fresh direct Codex session using the retained local login.
codex -c 'model_provider="openai"'
```

The earlier proposal to send all laptop requests through cm-manager was tested with one disposable request and then abandoned before activation. Its prepared configuration is saved as `~/.local/share/cm-codex-pool/remote-config-unapplied.toml`. That directory's `setup-state.json` explicitly marks its role as remote-dashboard access only.

References: [codex-lb onboarding](https://soju06.github.io/codex-lb/getting-started/), [refresh coordination implementation](https://github.com/Soju06/codex-lb/blob/v1.24.0/app/modules/accounts/refresh_claims.py), and [Codex custom-provider authentication](https://learn.chatgpt.com/docs/config-file/config-advanced#custom-model-providers).
