"""Conservative idle classification; inconclusive samples never reclaim."""
from dataclasses import dataclass
from datetime import datetime, timedelta
from typing import Sequence


@dataclass(frozen=True)
class Sample:
    at: datetime
    usage_usec: int


def idle_average(samples: Sequence[Sample], now: datetime, cpus: int,
                 window_seconds: int, max_gap_seconds: int) -> float | None:
    if cpus <= 0 or window_seconds <= 0 or max_gap_seconds <= 0:
        raise ValueError('invalid idle configuration')
    points = sorted(samples, key=lambda s: s.at)
    start = now - timedelta(seconds=window_seconds)
    prior = [p for p in points if p.at <= start]
    recent = [p for p in points if start < p.at <= now]
    if not prior or not recent:
        return None
    selected = [prior[-1], *recent]
    if (start - selected[0].at).total_seconds() > max_gap_seconds or (now - selected[-1].at).total_seconds() > max_gap_seconds:
        return None
    if any((b.at - a.at).total_seconds() > max_gap_seconds or b.usage_usec < a.usage_usec
           for a, b in zip(selected, selected[1:])):
        return None
    elapsed = (selected[-1].at - selected[0].at).total_seconds()
    if elapsed <= 0:
        return None
    return 100 * (selected[-1].usage_usec - selected[0].usage_usec) / (elapsed * 1_000_000 * cpus)


def next_idle_action(*, now: datetime, average: float | None, threshold: float,
                     last_activity: datetime, notices: Sequence[datetime],
                     idle_seconds: int, answer_seconds: int, notice_limit: int,
                     expires_at: datetime, expiry_guard_seconds: int,
                     queued_fit: bool) -> str | None:
    if average is None or average >= threshold:
        return None
    sent = sorted(t for t in notices if t > last_activity)
    if (expires_at-now).total_seconds() <= expiry_guard_seconds:
        return None
    if len(sent) >= 2 and all((now-t).total_seconds() >= answer_seconds for t in sent[-2:]) and queued_fit:
        return 'reclaim'
    if (now-last_activity).total_seconds() < idle_seconds:
        return None
    if len(sent) >= notice_limit:
        return None
    if sent and (now-sent[-1]).total_seconds() < answer_seconds:
        return None
    return 'notice'
