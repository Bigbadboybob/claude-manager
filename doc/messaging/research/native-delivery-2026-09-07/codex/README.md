# Codex native-delivery fixtures

See [CODEX_FINDINGS.md](CODEX_FINDINGS.md) for conclusions and [observations.json](observations.json) for sanitized measurements from Codex 0.153.4 on 2026-09-07. Full model prompts, raw configs, and production session data are not included.

The fixtures launch real Codex processes against deterministic **local mock Responses API servers**, using fresh isolated homes, state directories, and work directories. They never connect to production CM sessions or use real model credentials. Environment inheritance into Codex is restricted to a small allowlist.

Requirements: Python 3, `aiohttp`, `websockets`, and `pyte`, plus the Codex CLI. Choose a fresh output root for each repeat and an explicit Codex executable when comparing versions:

```bash
export CM_CODEX_RESEARCH_ROOT=/tmp/cm-codex-notification-repeat
export CM_CODEX_BIN=/absolute/path/to/codex
```

Run scripts from this directory. Suggested order:

```bash
python3 harness.py
python3 tui_fixture.py
python3 queue_latency.py
python3 queue_behavior.py
python3 tui_approval.py
python3 queue_reconnect.py
python3 background_shell.py
python3 tui_background.py
python3 background_mcp.py
python3 mcp_yield_active.py
python3 hooks_behavior.py
python3 ws_appserver.py
python3 ws_appserver_retry.py
python3 ws_appserver_tui.py
python3 structured_input_variants.py
```

`background_mcp.py` intentionally includes a 130-second call. The other scripts generally take seconds to tens of seconds. They generate `results.json`, model-request records, lifecycle-event records, and terminal snapshots in named case directories. Do not reuse production `CODEX_HOME` or point the output root at an existing real home; the harness creates its own fixture homes beneath the chosen root.

`mcp_fixture.py` and `hook_fixture.py` are subprocess helpers, not standalone test entry points. For `pyte` installed into a separate package directory, either use `PYTHONPATH` or set `CM_CODEX_RESEARCH_PYTE_PATH`.

The direct-MCP-tool discovery experiment was invalid and is excluded. All MCP cases here use the actual code-mode tool surface. Explicitly yielding a code-mode cell must not be described as autonomous completion delivery: inspect subsequent recorded model requests to distinguish them.
