"""pytest plugin for `scripts/cm-gate` (doc/gate-receipts.md §9).

Loaded with `-p cm_gate_pytest` when CM_GATE_REPORT names a JSON file. It
records what actually happened to every node: outcome, the exception type of
a failure, skip reasons, deselections and collection errors, so the
receipt's node set comes from the run rather than from a summary line.
"""

from __future__ import annotations

import json
import os

import pytest

# Popped so subprocesses the suite starts (a nested pytest) cannot inherit
# the path and overwrite this run's report.
_PATH = os.environ.pop("CM_GATE_REPORT", None)
_nodes: dict[str, dict] = {}
_collected: list[str] = []
_deselected: list[str] = []


def _skip_reason(report) -> str:
    longrepr = report.longrepr
    if isinstance(longrepr, tuple) and len(longrepr) == 3:
        text = str(longrepr[2])
    else:
        text = str(longrepr or "")
    for prefix in ("Skipped: ", "skipped: "):
        if text.startswith(prefix):
            text = text[len(prefix):]
    return text.strip()


def pytest_collectreport(report):
    if report.failed:
        _nodes[report.nodeid or "<collection>"] = {
            "id": report.nodeid or "<collection>", "outcome": "error",
            "exc_type": "CollectionError", "phase": "collect"}


def pytest_deselected(items):
    _deselected.extend(item.nodeid for item in items)


def pytest_collection_finish(session):
    _collected.extend(item.nodeid for item in session.items)


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item, call):
    # The exception type travels in user_properties, which xdist forwards
    # from its workers to the controller.
    outcome = yield
    report = outcome.get_result()
    if call.excinfo is not None:
        report.user_properties.append(("cm_gate_exc", call.excinfo.typename))


def pytest_runtest_logreport(report):
    node = _nodes.setdefault(report.nodeid, {"id": report.nodeid, "outcome": "passed"})
    wasxfail = hasattr(report, "wasxfail")
    if report.when == "call":
        if report.passed:
            node["outcome"] = "xpassed" if wasxfail else "passed"
        elif report.skipped:
            node["outcome"] = "xfailed" if wasxfail else "skipped"
            if not wasxfail:
                node["reason"] = _skip_reason(report)
        elif report.failed:
            node["outcome"] = "failed"
            node["exc_type"] = _exc_type(report)
    elif report.failed:
        # setup/teardown errors are errors, never ordinary failures
        node["outcome"] = "error"
        node["phase"] = report.when
        node["exc_type"] = _exc_type(report)
    elif report.when == "setup" and report.skipped:
        node["outcome"] = "xfailed" if wasxfail else "skipped"
        if not wasxfail:
            node["reason"] = _skip_reason(report)


def _exc_type(report) -> str | None:
    for key, value in getattr(report, "user_properties", []) or []:
        if key == "cm_gate_exc":
            return value
    crash = getattr(report.longrepr, "reprcrash", None)
    message = getattr(crash, "message", "") or ""
    if ":" in message:
        head = message.split(":", 1)[0].strip()
        if head and " " not in head:
            return head.rsplit(".", 1)[-1]
    if message and " " not in message.strip():
        return message.strip().rsplit(".", 1)[-1]
    return None


def pytest_unconfigure(config):
    if not _PATH:
        return
    # Under xdist only the controller writes; workers have no session items.
    if hasattr(config, "workerinput"):
        return
    with open(_PATH, "w") as fh:
        json.dump({"nodes": list(_nodes.values()), "collected": _collected,
                   "deselected": _deselected}, fh)
