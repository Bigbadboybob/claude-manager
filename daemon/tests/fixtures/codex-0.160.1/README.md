Captured from the installed Codex 0.160.1 app-server on 2026-10-06 with
`mcp_server/tests/integration_native_codex_protocol.py`. The model is a local
deterministic HTTP fixture, using a fresh HOME/CODEX_HOME with no credentials.

`success-background.jsonl` contains normal completion, a yielded `sleep 4`
command, and a reconnect/native wake. `approval-success.jsonl` contains a
command approval followed by successful completion. Row order and runtime
record shapes are preserved; generated UUIDs and fixture paths are replaced,
and instruction strings longer than 512 characters are omitted. Timestamps
and protocol fields remain as recorded. Additional bookkeeping and unknown
record cases in the Rust tests are explicitly synthetic.

`snapshot-background.json` and `snapshot-waiting.json` are normalized snapshots
captured from that same relay run and deserialized/applied by the Rust state
tests to verify the Python-to-daemon contract. The corpus research confirms that
`thread_settings_applied` and `thread_goal_updated` are `event_msg` payload kinds,
not top-level rollout kinds.
