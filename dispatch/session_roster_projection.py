"""Content-free projection of heartbeat activity for the roster.

The database query must enforce visibility before calling this module. Host
clock observations are labelled separately from API-clock freshness; neither
the absence of an error nor the absence of recovery evidence means healthy.
"""

from datetime import datetime, timedelta
import math
import re


_SOURCES = frozenset({"presence", "hooks", "relay", "transcript", "pty"})
_TURN_STATUSES = frozenset({"completed", "interrupted", "failed"})
_STATES = frozenset({"working", "working-background", "waiting-on-human", "errored",
                     "idle", "starting", "exited", "unknown"})


def _number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    try:
        return value if math.isfinite(value) and value >= 0 else None
    except OverflowError:
        return None


def _short(value, cap):
    return value[:cap] if isinstance(value, str) and value else None


def project_activity(state, reported_at: datetime, state_since, agent_state, now: datetime):
    """Return only bounded activity/limit fields; timestamps keep clock labels."""
    stale = reported_at < now - timedelta(seconds=90)
    raw = agent_state if isinstance(agent_state, dict) else {}
    detail = raw.get("detail") if isinstance(raw.get("detail"), dict) else {}
    turn = raw.get("last_turn") if isinstance(raw.get("last_turn"), dict) else {}
    source = raw.get("source")
    turn_status = turn.get("status")
    last_state = state if isinstance(state, str) and state in _STATES else "unknown"
    error_kind = _short(detail.get("error_kind"), 80)
    if error_kind and not re.fullmatch(r"[A-Za-z0-9_-]+", error_kind):
        error_kind = None
    return {
        "freshness": "stale" if stale else "fresh",
        "activity": {
            "state": "stale" if stale else last_state,
            "last_known_state": last_state,
            "state_since_api_clock": state_since,
            "since_host_clock": _number(raw.get("since")),
            "observed_at_host_clock": _number(raw.get("observed_at")),
            "source": source if isinstance(source, str) and source in _SOURCES else None,
            "last_turn_ended_at_host_clock": _number(turn.get("ended_at")),
            "last_turn_status": (turn_status if isinstance(turn_status, str)
                                 and turn_status in _TURN_STATUSES else None),
        },
        "limit": {
            "error_kind": error_kind,
            "resumes_at_host_clock": _number(detail.get("resumes_at")),
            "retrying": detail.get("retrying") if isinstance(detail.get("retrying"), bool) else None,
            "recovery": "not_exported",
        },
    }
