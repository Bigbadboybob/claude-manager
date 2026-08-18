#!/usr/bin/env python3
"""Re-deliver a cloud backtest's result artifact from GCS to the planning API.

Recovery half of the artifact-POST path. When a backtest worker finishes but
cannot land its `task_artifacts` row, it PATCHes the task `blocked` and leaves
the run's full output — plus the exact POST body it failed to deliver — in the
run's GCS prefix as `backtest_artifact.json` (see `post_artifact` in
worker/backtest_startup.sh). This script fetches that body and POSTs it, so
recovery never means rebuilding the summary by hand.

    python3 scripts/repost_backtest_artifact.py <task_id>
    python3 scripts/repost_backtest_artifact.py <task_id> --gcs-prefix gs://bucket/backtests/<run_key>
    python3 scripts/repost_backtest_artifact.py <task_id> --set-status done

With no --gcs-prefix the prefix is derived from the task's own
`metadata.backtest.run_key` plus --bucket.

Runs on the prod manager box, a worker VM, or a laptop: stdlib only (urllib +
gsutil), no venv required.

Idempotent: if the task already has an artifact row it reports it and exits 0
without POSTing (override with --force). Re-running after a successful POST is
therefore safe.

Older runs predate the parked body. For those, --gcs-prefix still works: the
script falls back to `backtest_summary.json` then `summary.json` under the same
prefix and rebuilds the contract-shaped body itself (identical wrap to the
worker's, NaN/Infinity coerced to null so Postgres JSONB accepts it).
"""

from __future__ import annotations

import argparse
import json
import math
import os
import subprocess
import sys
import urllib.error
import urllib.request

DEFAULT_BUCKET = "gs://prediction-market-scalper-datasets"
ARTIFACT_BODY_NAME = "backtest_artifact.json"
# Same order build_artifact_json uses: the PT publisher's contract-shaped
# summary first, the grid runner's raw summary second.
FALLBACK_SUMMARY_NAMES = ("backtest_summary.json", "summary.json")


# ---------------------------------------------------------------------------
# API
# ---------------------------------------------------------------------------


def _request(method: str, url: str, token: str, body: bytes | None = None,
             timeout: float = 60.0) -> tuple[int, bytes]:
    """Fire one HTTP request; return (status, body). Never raises on 4xx/5xx."""
    req = urllib.request.Request(url, data=body, method=method)
    req.add_header("Authorization", f"Bearer {token}")
    if body is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def _get_json(url: str, token: str):
    status, raw = _request("GET", url, token)
    if status != 200:
        raise SystemExit(f"GET {url} -> HTTP {status}: {raw[:500].decode(errors='replace')}")
    return json.loads(raw)


# ---------------------------------------------------------------------------
# GCS
# ---------------------------------------------------------------------------


def _gsutil_cat(path: str) -> bytes | None:
    """Contents of a GCS object, or None if it does not exist / is unreadable."""
    proc = subprocess.run(["gsutil", "cat", path], capture_output=True)
    if proc.returncode != 0:
        return None
    return proc.stdout


# ---------------------------------------------------------------------------
# Body assembly
# ---------------------------------------------------------------------------


def _finite(o):
    """Coerce NaN/Infinity -> None: invalid JSON, and Postgres JSONB rejects them."""
    if isinstance(o, float):
        return o if math.isfinite(o) else None
    if isinstance(o, dict):
        return {k: _finite(v) for k, v in o.items()}
    if isinstance(o, list):
        return [_finite(v) for v in o]
    return o


def _validate_body(raw: bytes, source: str) -> dict:
    """Parse a candidate POST body and enforce the ArtifactCreate contract.

    Catches the exact shapes the API answers 422 to (empty, non-object,
    non-object `summary`) here, where the message is useful, instead of after
    a round trip.
    """
    if not raw.strip():
        raise SystemExit(f"{source} is EMPTY — nothing to post")
    try:
        body = json.loads(raw)
    except ValueError as e:
        raise SystemExit(f"{source} is not valid JSON: {e}")
    if not isinstance(body, dict):
        raise SystemExit(f"{source} is a {type(body).__name__}, expected a JSON object")
    if not isinstance(body.get("summary"), dict):
        raise SystemExit(f"{source} has no object `summary` — the API would 422 this")
    return body


def _rebuild_from_summary(prefix: str, run_key: str | None) -> dict | None:
    """Reconstruct the artifact body from a published summary (pre-parking runs)."""
    for name in FALLBACK_SUMMARY_NAMES:
        raw = _gsutil_cat(f"{prefix}/{name}")
        if raw is None:
            continue
        try:
            summary = json.loads(raw)
        except ValueError:
            continue
        if not isinstance(summary, dict):
            continue
        print(f"  rebuilt from {prefix}/{name}")
        summary = _finite(summary)
        summary["partial"] = False
        summary["gcs_pointer"] = prefix
        if run_key:
            summary["run_key"] = run_key
        return {
            "kind": "backtest-result",
            "partial": False,
            "gcs_prefix": prefix,
            "summary": summary,
        }
    return None


# ---------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        description="Re-POST a cloud backtest result artifact from GCS to the planning API",
    )
    p.add_argument("task_id", help="Planning task UUID of the backtest run")
    p.add_argument("--gcs-prefix", default=None,
                   help="Run prefix (gs://bucket/backtests/<run_key>); "
                        "derived from the task metadata when omitted")
    p.add_argument("--bucket", default=os.environ.get("CM_RESULTS_BUCKET", DEFAULT_BUCKET),
                   help=f"Results bucket used to derive the prefix (default {DEFAULT_BUCKET})")
    p.add_argument("--body-file", default=None,
                   help="POST this local file instead of fetching from GCS")
    p.add_argument("--api-url", default=os.environ.get("CM_API_URL"),
                   help="Planning API base URL (env CM_API_URL)")
    p.add_argument("--api-token", default=os.environ.get("CM_API_TOKEN"),
                   help="Planning API bearer token (env CM_API_TOKEN)")
    p.add_argument("--force", action="store_true",
                   help="POST even if the task already has artifact rows")
    p.add_argument("--set-status", default=None,
                   help="PATCH the task to this status after a successful POST (e.g. done)")
    p.add_argument("--dry-run", action="store_true",
                   help="Resolve and validate the body, print it, POST nothing")
    return p


def main() -> int:
    args = build_parser().parse_args()
    if not args.api_url or not args.api_token:
        raise SystemExit("CM_API_URL and CM_API_TOKEN must be set (or passed as flags)")
    base = args.api_url.rstrip("/")

    task = _get_json(f"{base}/tasks/{args.task_id}", args.api_token)
    bt = ((task.get("metadata") or {}).get("backtest") or {})
    run_key = bt.get("run_key")
    print(f"task {args.task_id}: status={task.get('status')} name={task.get('name')!r} run_key={run_key}")

    existing = _get_json(f"{base}/tasks/{args.task_id}/artifacts", args.api_token)
    if existing and not args.force:
        print(f"task already has {len(existing)} artifact row(s) "
              f"(newest {existing[0].get('created_at')}) — nothing to do; --force to re-post")
        return 0

    # --- resolve the body ---------------------------------------------------
    if args.body_file:
        with open(args.body_file, "rb") as f:
            raw = f.read()
        body = _validate_body(raw, args.body_file)
    else:
        prefix = args.gcs_prefix
        if not prefix:
            if not run_key:
                raise SystemExit("task has no metadata.backtest.run_key — pass --gcs-prefix")
            prefix = f"{args.bucket.rstrip('/')}/backtests/{run_key}"
        prefix = prefix.rstrip("/")
        print(f"fetching {prefix}/{ARTIFACT_BODY_NAME}")
        raw = _gsutil_cat(f"{prefix}/{ARTIFACT_BODY_NAME}")
        if raw is not None:
            body = _validate_body(raw, f"{prefix}/{ARTIFACT_BODY_NAME}")
        else:
            print(f"  no {ARTIFACT_BODY_NAME} at that prefix (run predates body parking); "
                  "rebuilding from the published summary")
            rebuilt = _rebuild_from_summary(prefix, run_key)
            if rebuilt is None:
                raise SystemExit(
                    f"no {ARTIFACT_BODY_NAME} and no usable summary under {prefix} — "
                    "nothing to recover (check the prefix with `gsutil ls`)"
                )
            body = rebuilt

    payload = json.dumps(body, allow_nan=False).encode()
    print(f"body: {len(payload)} bytes, summary keys="
          f"{sorted(body['summary'])[:8]}{'...' if len(body['summary']) > 8 else ''}")

    if args.dry_run:
        print("--dry-run: not posting")
        return 0

    status, resp = _request("POST", f"{base}/tasks/{args.task_id}/artifacts",
                            args.api_token, body=payload)
    print(f"POST /tasks/{args.task_id}/artifacts -> HTTP {status}: "
          f"{resp[:800].decode(errors='replace')}")
    if not 200 <= status < 300:
        return 1

    if args.set_status:
        status, resp = _request(
            "PATCH", f"{base}/tasks/{args.task_id}", args.api_token,
            body=json.dumps({"status": args.set_status}).encode(),
        )
        print(f"PATCH status={args.set_status} -> HTTP {status}: "
              f"{resp[:400].decode(errors='replace')}")
        if not 200 <= status < 300:
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
