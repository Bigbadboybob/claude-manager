# cm-manager Codex account pool

Installed 2026-09-07 UTC at the operator's request, following the [account-rotation evaluation](codex-account-rotation-evaluation.md).

## Current state

- codex-lb **1.24.0**, user service `cm-codex-lb.service`, enabled with restart-on-failure. User lingering was already enabled, so the service runs independently of SSH logins.
- Dashboard/proxy bound to **127.0.0.1:2455**; OAuth callback configured for **127.0.0.1:1455** and started when a browser login begins. Access through SSH forwarding; no public listener or firewall change.
- Standard dashboard authentication. Local access through the tunnel needs no bootstrap token. Proxy routes require a separate API key.
- Capacity-weighted routing, sticky threads enabled, anonymous telemetry disabled.
- The existing cm-manager CLI account was imported as **Existing CLI account**. Its usage response showed `quota_exceeded`, with 0% secondary quota remaining. The original `~/.codex/auth.json` was unchanged by the import.
- The operator enrolled the second account. A successful request on 2026-09-07 found 86% secondary quota remaining; this is a point-in-time reading.
- cm-manager's user Codex configuration selects `cm_pool` and retains `gpt-5.6-sol` for new Codex sessions. All ten production continuous-task definitions have migrated to Codex; see the [execution record](continuous-codex-migration-execution-20260907.md) for remaining production observations. The laptop uses a separately installed [local pool](local-codex-account-pool.md), with its own enrollment and activation status.
- The external migration probe and daemon reader now verify this exact pool route, model and credential-helper hash. Pool-unavailable runs receive a durable hold and require work reconciliation before recovery.

## Dashboard access from the laptop

Bookmark **<http://localhost:2456>** for this remote pool. The laptop's enabled user service `cm-codex-pool-tunnel.service` forwards that port to cm-manager and reconnects after connection failures. The application-menu shortcut is **Codex Accounts - cm-manager**. This connection is for dashboard access; local model traffic uses the independent local-pool design. The old manual dashboard tunnel may be closed.

The local pool owns ports 2455 and, during login, 1455 on the laptop. For a future **remote** account enrollment, use the dashboard's **Paste callback URL (for remote server)** flow. Do not forward the remote callback over the local pool's active callback port. Do not send callback URLs through chat or record them in this repository.

## Historical initial login steps

These were the first cm-manager enrollment steps, before the local pool existed. Use the access instructions above now.

On the operator's computer, keep this tunnel running:

```bash
ssh -N -o ExitOnForwardFailure=yes \
  -L 2455:127.0.0.1:2455 \
  -L 1455:127.0.0.1:1455 cm-manager
```

Open <http://localhost:2455>. In Accounts, choose **Add account → OAuth → Browser (PKCE) → Start sign-in**, then **Open sign-in page**. Sign in to the nearly full account. If the browser selects the depleted account automatically, copy the authorization link into a private/incognito window and use the other login. Keep the dashboard open until it reports **Account added**. The depleted account is already enrolled and does not need a second login.

If callback forwarding fails, the OAuth dialog also has **Paste callback URL (for remote server)**. Paste the browser's final localhost callback URL into that dashboard field; it contains temporary login credentials and should not be sent through chat or saved in this repository.

After enrollment, verify that the dashboard shows two distinct accounts and fresh quota for the new account. Then run the real model and CM canary checks before production cutover.

## Runtime and maintenance

| Item | Path on cm-manager |
|---|---|
| User unit | `~/.config/systemd/user/cm-codex-lb.service` |
| Environment, including bootstrap secret | `~/.config/cm-codex-lb/service.env` |
| Proxy client key | `~/.config/cm-codex-lb/proxy.key` |
| Application data and encryption key | `~/.local/share/cm-codex-lb/data/` |
| Pinned runtime | `~/.local/share/cm-codex-lb/runtime/1.24.0/` |
| Private installation/verification records | `~/.local/share/cm-codex-lb/{setup-state,verification}.json` |
| Opt-in CLI launcher | `~/.local/bin/cm-codex-pool` |
| Credential helper | `~/.local/bin/cm-codex-pool-token` |

The provider uses command authentication through the private credential helper. The helper reads `proxy.key` only when Codex requests a credential; never run it directly in captured tool output. The launcher also supplies the provider definition and default model explicitly. For isolated `exec` invocations, these overrides follow the subcommand: flags before `exec --ignore-user-config` were observed to be ignored by CLI 0.153.4. A caller can still override the model explicitly.

```bash
# Run on cm-manager
systemctl --user status cm-codex-lb
curl --fail http://127.0.0.1:2455/health/ready
~/.local/bin/cm-codex-pool
```

Service restarts interrupt requests routed through this proxy: finish/drain those requests before intentional maintenance. The data directory contains credentials and conversation data; keep it private. A recoverable backup needs both its database and encryption key. Updates are manual and pinned pending the migration pilot.

## Verified at installation

Readiness OK; dashboard and all 21 referenced assets served successfully; unauthenticated proxy access returned 401; authenticated model-catalog access returned 200 and included `gpt-5.6-sol`; the launcher reported Codex CLI 0.153.4. No service crash/restart was observed at installation.

After enrollment, a real request returned `CM_CODEX_POOL_OK`. Its bound rollout records provider `cm_pool`, CLI 0.153.4 and model `gpt-5.6-sol` (thread `01a07a00-b9f9-7003-b685-4a5811e160f6`). The installed probe returned `OK` at epoch `1788754076.024022` using the same route and model. The disposable CM canary completed its first orchestrator/worker/final-monitor cycle and reused both sessions for its second artifact. Drain validation found missing MCP journal-coverage propagation; it was repaired and a subsequent natural drain/checkpoint passed. See the [execution record](continuous-codex-migration-execution-20260907.md) for later lifecycle checks, production status and remaining workflow gates.

Support deployment backup: `~/.local/share/cm-codex-lb/migration-support-backup/`. Its manifest records the daemon/probe hashes and prior private Codex configuration. A brain restart preserved existing sessions; epoch 4/PID 2754625 remained healthy beyond ten minutes. Full daemon validation passed 1,241 tests, with four existing ignores; the probe suite passed ten tests. These results do not yet authorize declaring the production fleet migrated.

See the project's [getting-started guide](https://soju06.github.io/codex-lb/getting-started/) for dashboard onboarding and the [routing contract](https://github.com/Soju06/codex-lb/blob/v1.24.0/docs/routing.md) for account-bound continuation limits.
