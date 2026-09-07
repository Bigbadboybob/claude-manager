# Codex account rotation evaluation

2026-09-07 UTC (September 6 in Chicago). Recommendation: **codex-lb for the cm-manager pilot**, subject to a real two-account lifecycle test. Authmux is useful for selecting logins for CLI launches; its file-switching design is a weaker fit for persistent orchestrators and concurrent workers.

Follow-up: the operator approved setup and enrolled both accounts. The [cm-manager account-pool runbook](codex-account-pool.md) records the installed private service, successful pooled model/probe requests, provider deployment and CM canary progress. Pool-aware probe/recovery support is now deployed. The results and remaining-gates list below describe the earlier isolated evaluation; they are not the current deployment inventory.

## Versions and isolation

| Component | Evaluated version |
|---|---|
| cm-manager Codex CLI | 0.153.4 |
| Requested deployment model | gpt-5.6-sol |
| codex-lb | Stable 1.24.0, commit 84fde5a |
| authmux | Stable npm 0.1.26, commit 423afd8 |

Tests used stable releases, not the newer default branches. Both tools were installed under `/home/lucas/.cm/experiments/account-rotation-20260907/`, with an isolated Python 3.13 environment and local npm prefix. No global installation, shell hook, watcher, production provider change, or real credential import. The temporary proxy listened only on `127.0.0.1:12455` and was stopped after testing.

## Comparison

| Concern | codex-lb | authmux |
|---|---|---|
| Selection | Proxy routes model requests across eligible accounts, with quota/health tracking. | Copies a saved login over active `auth.json`; automatic mode changes snapshots at quota thresholds. |
| Persistent sessions | Eligible failed requests can move accounts without restarting the client. | Replacing the file does not establish that a live Codex process reloads cached authentication. Shell hooks run around CLI launches. |
| Concurrent workers | Central routing suits many daemon clients. | Global auth changes and terminal restoration need coordination with daemon launches. |
| Continuation | Some state remains tied to its account. Only safe replay may cross accounts. | No request replay or conversation-state migration. |
| Operations | Service, database, credential store and routing configuration. | Smaller installation and convenient manual account selection. |

Authmux's stable `activateSnapshot` uses `copyFile` followed by chmod. Registry durability is separate from active-auth replacement. A cached-reader fixture retained account A after the file changed to B; this is **not** a live Codex refresh test.

codex-lb distinguishes soft sticky routing from hard ownership of continuation state, including previous responses, turn state and uploaded files. Disabling stickiness does not remove hard ownership. A healthy second account does not guarantee an old thread can continue. Retain CM's graceful drain and reconciliation recovery path.

## Results on cm-manager

- **67 codex-lb tests passed**, 364 deselected: stable-release load-balancer, integration, WebSocket and compaction tests selected with `usage_limit or quota_loss or precreated or previous_response_usage or forwards_client_tools or owner or rate_limit`. Accounts/upstream responses were simulated. Includes early quota failover and refusal to replay account-bound continuations.
- **Authmux synthetic-account checks passed:** snapshot save/activation, automatic low-quota A-to-B switch, `0600` active-file permissions, and rejection of cross-identity snapshot overwrite. No real accounts or watcher.
- **Actual proxy process:** readiness OK; HTTP request against an empty pool returned 503 with `code=no_accounts`.
- **Installed CLI against actual proxy:** ephemeral, read-only invocation requesting `gpt-5.6-sol`, ignoring user config/rules and using an invocation-only custom provider, reached the loopback endpoint. WebSocket attempt and HTTP fallback returned the expected empty-pool failure. CLI JSON retained the message but omitted the structured `no_accounts` code. This verifies endpoint/configuration compatibility, **not** model inference or real account failover.
- **Production:** holder epoch 3/brain PID 2732636 healthy, matching brain/holder counts, no pending exits, MCP OK. All ten production definitions still Claude. Login-file modification time unchanged during evaluation.

Evidence in the experiment directory: `lb-tests.log`, `authmux-result.json`, `smoke-result.json`, `cli-smoke-result.json`, `cli-smoke.stdout.jsonl`, and `production-state-after.json`. Non-secret copies are retained locally outside the worktree.

## Remaining pilot gates

1. Enroll two operator-owned accounts. Keep credentials in cm-manager's private service store, outside migration manifests/repository. Fresh login was still pending at evaluation time.
2. Verify a real `gpt-5.6-sol` completion through the pool, then a disposable CM orchestrator/worker with final-monitor delivery, resumed turns and compaction. Test a controlled account-unavailable transition after a completed turn, including safe hold/handover when continuation cannot move.
3. Extend [`codex-usage-probe`](../scripts/codex-usage-probe) and its [daemon reader](../daemon/src/continuous/codex_account.rs) for an explicitly pinned provider. The producer currently rejects custom providers and ignores user config during execution. A direct-account completion cannot certify the proxy route. Bind evidence to actual provider configuration and exact model.
4. Distinguish pool exhaustion, unavailable continuation owner, proxy outage and auth failure in CM. Test the installed CLI's actual error/rollout representation, since the empty-pool structured code is lost. A healthy pool must not authorize replay of unresolved work.
5. Apply proxy settings only to newly launched canary sessions. Preserve the migration checkpoint before `api-update`; do not rewrite active production transcripts or interrupt workers.

## Sources

- [codex-lb stable release](https://github.com/Soju06/codex-lb/releases/tag/v1.24.0)
- [Routing and continuation ownership](https://github.com/Soju06/codex-lb/blob/v1.24.0/docs/routing.md)
- [WebSocket quota-retry tests](https://github.com/Soju06/codex-lb/blob/v1.24.0/tests/integration/test_proxy_websocket_responses.py)
- [authmux README](https://github.com/opencue/authmux/blob/v0.1.26/README.md)
- [Active-snapshot implementation](https://github.com/opencue/authmux/blob/v0.1.26/src/lib/accounts/write/use.ts)
- [Automatic-switch policy](https://github.com/opencue/authmux/blob/v0.1.26/src/lib/accounts/auto-switch/policy.ts)
- [Official Codex provider configuration](https://learn.chatgpt.com/docs/config-file/config-advanced#custom-model-providers)
