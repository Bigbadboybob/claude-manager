"""Pure scheduling decisions for the compute lease pool.

All resource claims (including claims being granted or released) must be read
and written under the same database advisory lock by the caller. In particular,
absence of a confirmed node release is never treated as free capacity.
"""
from dataclasses import dataclass
from datetime import datetime
from typing import Mapping, Sequence

PRIORITIES = {"v221-gate": 0, "normal": 1, "low": 2}
OCCUPYING = frozenset({"granting", "active", "releasing"})


@dataclass(frozen=True)
class Box:
    name: str
    ssh_target: str
    slots: tuple[str, ...]
    configured_enabled: bool = True
    run_argv: tuple[str, ...] | None = None


@dataclass(frozen=True)
class Policy:
    cpus_per_slot: int
    mem_gb_per_slot: int
    max_slots_per_session: int
    max_slots_per_lease: int
    max_slots_per_team: int


def slots_needed(lease: Mapping, policy: Policy) -> int:
    cpus = lease["cpus"]
    if not isinstance(cpus, int) or isinstance(cpus, bool) or cpus <= 0:
        raise ValueError("cpus must be a positive integer")
    if policy.cpus_per_slot <= 0:
        raise ValueError("cpus_per_slot must be positive")
    mem = lease["mem_gb"]
    if not isinstance(mem, int) or isinstance(mem, bool) or mem <= 0:
        raise ValueError("mem_gb must be a positive integer")
    if policy.mem_gb_per_slot <= 0:
        raise ValueError("mem_gb_per_slot must be positive")
    return max((cpus + policy.cpus_per_slot - 1) // policy.cpus_per_slot,
               (mem + policy.mem_gb_per_slot - 1) // policy.mem_gb_per_slot)


def queue_order(leases: Sequence[Mapping]) -> list[Mapping]:
    return sorted((x for x in leases if x["status"] == "queued"),
                  key=lambda x: (PRIORITIES[x["priority"]], x["created_at"], str(x["id"])))


def limit_reason(lease: Mapping, leases: Sequence[Mapping], policy: Policy,
                 enabled_boxes: Sequence[Box]) -> str | None:
    """Distinguish the team soft cap from limits that must retain queue order."""
    count = slots_needed(lease, policy)
    if count > policy.max_slots_per_lease and not str(lease.get("purpose") or "").strip():
        return 'hard'
    occupied = [x for x in leases if x["status"] in OCCUPYING]
    if sum(slots_needed(x, policy) for x in occupied if x["owner"] == lease["owner"]) + count > policy.max_slots_per_session:
        return 'hard'
    team = lease.get("team")
    other_waiters = team and any(x["status"] == "queued" and x.get("team") != team and
        any(box.configured_enabled and len(box.slots) >= slots_needed(x, policy) for box in enabled_boxes)
        for x in leases)
    # Gate work bypasses the team soft cap, never the session hard cap above.
    if (lease.get('priority') != 'v221-gate' and other_waiters and
            sum(slots_needed(x, policy) for x in occupied if x.get("team") == team) + count > policy.max_slots_per_team):
        return 'soft_team'
    return None


def select_box(lease: Mapping, leases: Sequence[Mapping], boxes: Sequence[Box],
               policy: Policy, *, enabled_boxes: Sequence[Box] | None = None) -> tuple[Box, tuple[str, ...]] | None:
    """Find capacity without oversubscription. Slot lists are disjoint CPU sets."""
    if limit_reason(lease, leases, policy, boxes if enabled_boxes is None else enabled_boxes):
        return None
    count = slots_needed(lease, policy)
    occupied = [x for x in leases if x['status'] in OCCUPYING]
    for box in boxes:
        used: set[str] = set()
        for current in occupied:
            if current.get("box") == box.name:
                # A malformed reservation must never silently free capacity.
                reservation = current.get("slot_ids")
                if (not reservation or len(reservation) != slots_needed(current, policy)
                        or len(set(reservation)) != len(reservation)
                        or not set(reservation).issubset(set(box.slots))
                        or used.intersection(reservation)):
                    raise ValueError(f"missing slot reservation on {box.name}")
                used.update(reservation)
        free = tuple(slot for slot in box.slots if slot not in used)
        if len(free) >= count:
            return box, free[:count]
    return None


def queue_eta(leases: Sequence[Mapping], now: datetime) -> datetime | None:
    """Return a conservative earliest possible change, not a promised start."""
    expiries = [x["expires_at"] for x in leases
                if x["status"] == "active" and x.get("expires_at") and x["expires_at"] > now]
    return min(expiries) if expiries else None
