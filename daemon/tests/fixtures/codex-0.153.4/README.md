# Codex 0.153.4 fixtures

Captured from `/usr/bin/codex` on cm-manager on 2026-09-06/07 UTC.

`success.jsonl` is a completed real `gpt-5.6-sol` request with the fixed reply
`CM_CODEX_PROBE_OK`. Instructions, ordinary response items and machine paths
were removed; session version/provider, turn model and runtime events remain.

The three error cases used an isolated temporary `CODEX_HOME`, a dummy key and
an HTTP server bound to loopback. No production credentials or model endpoint
were used for failure cases. The fake Responses endpoint returned 401 with
`invalid_api_key`, 429 with `usage_limit_reached`, or 404 with `model_not_found`.
The files retain the pinned CLI's actual output and rollout event shapes.

In this version, rollout errors are nested in `task_complete.payload.error`.
The usage error preserves `codex_error_info=usage_limit_exceeded`; the tested
401 and 404 errors use `other`, with the HTTP status in a structured error's
message. These are deliberately different from app-server camelCase schemas.

`pool_unavailable.jsonl` and `pool_unavailable-exec.jsonl` were captured from
the same CLI against a separate empty codex-lb 1.24.0 instance on loopback
port 12455, without production accounts. The actual structured runtime error
retains the 503 `No available accounts` message but loses the upstream error
code. It means this request cannot be served; it does not establish that every
account is exhausted, since continuation ownership can also restrict routing.
CM records a durable pool hold and requires reconciliation before recovery.
