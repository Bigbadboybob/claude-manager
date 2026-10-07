# Claude hook fixtures

Synthetic main-thread payloads matching the hook schemas inspected in the installed
Claude Code 2.1.291 bundle on 2026-10-07. These are not live session captures.
The schema source and background task mappings are documented in
`~/.local/share/swarm-coord/idle-research/claude-code.md` (hook appendix).

`Stop.normalized.json` is the expected producer RPC body, without session UID,
at Unix time 1000. Both Python normalization and Rust state application use it.
Private marker fields check that prompts, tool arguments, cron instructions and
notification text are omitted from reports. Descriptions, bounded assistant text
and error details are intentionally included.
