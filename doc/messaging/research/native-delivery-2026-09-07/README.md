# Native delivery research fixtures

See the [main survey and recommendation](../../NATIVE_DELIVERY_RESEARCH.md), [Codex findings](codex/CODEX_FINDINGS.md), and the sanitized [Claude observations](claude/observations.json) / [Codex observations](codex/observations.json).

Recorded 2026-09-07 with Claude Code **2.1.263** and Codex **0.153.4**, using real client binaries and deterministic local mock model endpoints. These are opt-in research fixtures, not CM production code or ordinary CI tests. They create disposable configurations/workspaces and never use a production CM session. Client environments are constructed from an allowlist and use synthetic/no API credentials. Model responses and harmless tool commands are supplied by the fixtures.

The comparison separates model requests from UI completion notices. Claude measurements stop when the next main-conversation model request arrives. Codex queue/control measurements include completion of the synthetic turn. Real inference latency is absent; do not compare millisecond figures as cross-client benchmarks.

## Claude reproduction

Python 3's standard library and the pinned Claude executable are sufficient. Set a new absolute output directory for a clean run. `CM_RESEARCH_CLAUDE_BIN` defaults to the recorded version under the current user's native install; override it if installed elsewhere.

```bash
export CM_RESEARCH_OUTPUT=/tmp/cm-claude-notification-repeat
export CM_RESEARCH_CLAUDE_BIN=/absolute/path/to/claude
python3 claude/claude_harness.py
python3 claude/claude_cases.py bash mcp async rewake channel
python3 claude/claude_socket_cases.py idle active approval default refuse ownchild
python3 claude/claude_child_bridge.py
python3 claude/claude_generic_mcp.py
python3 claude/claude_wait_edges.py
python3 claude/claude_stream_case.py
python3 claude/claude_disconnect_case.py
```

Run these from this directory. `claude_mcp.py` is a subprocess helper, not a standalone experiment. Each case saves `result.json`, mock HTTP request bodies, a debug log, and terminal or stream output in its own output subdirectory. The checked-in observations contain only summaries, no real session content or credentials.

The harness keeps telemetry/feature fetching disabled. Therefore the channel probe is expected to document an availability gate rather than prove positive push delivery. Monitor is absent under that configuration. Consult the survey for official documented behavior and account/provider constraints; do not interpret the negative fixture result as a universal lack of support.

Tests inspect actual main-conversation messages separately from automatic title-generation requests. For Bash completion, the model's notification contains an output-file reference; absence of the printed notification body is expected. A successful MCP completion includes the returned text directly. Approval fixtures use an explicit ask rule for a harmless command, since `printf` alone may be allowed without prompting.

The socket tests send only to endpoints published by their own newly launched disposable clients. The own-child test uses the socket and token inherited by its MCP process; it records only their presence, never the token value. Every client is stopped in a `finally` block, including failed cases.

## Codex reproduction

See [codex/README.md](codex/README.md) for dependencies and individual commands. Tests cover ordinary embedded TUI delivery, the persistent queue, draft/approval preservation, disconnected-client replay, an owned app-server with multiple controllers and the official remote TUI, background shell/MCP completion, hooks, and structured-input variants.

The failed direct-MCP discovery attempt and initial too-short queue observation are explicitly excluded from conclusions. The valid long-MCP test takes 130 seconds. The copied checkpoint fixture and Claude Bash/MCP draft-confirmation tests were also run from their checked-in locations to validate the portable helper paths.

## Scope of the evidence

The mocks test client scheduling and transport, not model judgment, production authentication, CM's future receipt ledger, actual local-LB integration, cloud synchronization, or other OS versions. A fixed observation window showing no idle wake is reported as such. Official behavior is cited in the main report and Codex findings.

`sources.json` records fetched documentation URLs and content hashes plus the tested executable hashes. Full public-page snapshots and raw disposable traces remain in temporary research directories; the report and sanitized observations are the durable result.
