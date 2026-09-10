# Backtest machine selection and recorded provenance

Initiative-Bridge source review for P3/P4/P5, 2026-09-10. CM revision inspected:
`ecd41d6af29df656fb4ee883b9240543c46d1cd3`. No backtest was submitted, and no
deployed service, environment setting, existing run, or GCE instance was queried.

**Finding:** machine selection is already recorded on the CM task. The two
submission paths have different source defaults, but both preserve an explicit
nonempty `machine_type`. The benchmark receipt's CPU count alone is insufficient;
that does not mean the wider task/run provenance lacks a machine-class record.

## Selection in the inspected source

| Path | Default when caller omits the type | Explicit caller value | Record |
|---|---|---|---|
| Python MCP with a PlanningClient | `CM_BACKTEST_MACHINE_TYPE`, falling back to `n2-standard-4` | Overrides the default | New task `metadata.vm.machine_type` |
| MCP fallback through daemon `backtest.submit` | `c2-standard-4` in the Rust handler | Forwarded by MCP and selected by the handler | New task `metadata.vm.machine_type` |
| Dispatcher launch | Dispatcher defaults, including environment override with `n2-standard-4` fallback | Task `metadata.vm` merges over dispatcher defaults | Resolved VM settings written back after successful launch |

Sources: `mcp_server/server.py:3367–3373,3513–3585`;
`daemon/src/control/methods.rs:16283–16312`;
`dispatch/config.py:31–37`;
`api/dispatch_daemon.py:214–263`.

The MCP schema exposes `machine_type`. In the fallback route it is forwarded
only when nonempty; the daemon then uses that value instead of its default.
The dispatcher computes
`vm_over = {**BACKTEST_VM_DEFAULTS, **(meta.get("vm") or {})}` and passes it to
`launch_worker`. The worker constructs the GCE instance's machine type from the
override (`dispatch/vm.py:32–52,81–84`). Thus the inspected code does not support
the claim that an explicit c2 request is silently replaced by the n2 default.

The Rust comment says its defaults mirror Python/dispatcher; the literal values
disagree. That is a source inconsistency worth recording as a candidate issue,
not evidence of which route or revision handled a particular deployed run.
Do not treat a historical partial-deploy note as today's runtime configuration.

## What to inspect for a particular comparison, if later authorized

`get_task(task_id)` exposes task metadata. After successful launch the dispatcher
stores resolved `metadata.vm`, `metadata.backtest.run_key`, `launched_at`,
`worker_vm`, and `worker_zone`. Link the artifact to its task and run before
concluding that machine class cannot be disambiguated. The compact
`get_backtest_result` view intentionally omits task VM metadata; inspect the
task as well rather than relying on the result summary or CPU count alone.

This is recorded configuration, not an independent observation of physical CPU
model or every runtime property. It is also mutable task state: a spot-preemption
relaunch updates launch time and VM identity, and a run can have several partial
and final artifacts. A latest task record must not be silently assigned to every
historical attempt. Existing attempt-specific records may suffice; if they do
not, label that gap precisely before proposing new storage.

For a future proposal, distinguish requested class, resolved configuration,
launch/attempt identity, and observed host properties. Follow existing scoped
benchmark/panel rules for explicit class selection. Whether a particular pair
is comparable remains a domain-evaluation question; this source review supplies
the CM provenance path, not a verdict on any run.

## Status

Research input only. No default changed, new task filed, implementation selected,
or deployment performed. P3/P4/P5 should incorporate the existing metadata path
and keep runtime verification as a separately scoped future action.

## Related P3 question: default runner module

The same inspected CM source defaults to
`analysis.backtests.backtest_actrader_grid` in Python
(`mcp_server/server.py:3378`) and the daemon
(`daemon/src/control/methods.rs:16057–16059`). The local `origin/main` snapshot
`0f1f47a` also retains the Python default. The callable tool's description says
"canonical grid runner" without identifying that module. This is not evidence
of the loaded server's effective default.

At predictionTrading source `91365cc40`, `git ls-tree` shows
`analysis/backtests/backtest_production_grid.py` and no
`analysis/backtests/backtest_actrader_grid.py`. Thus a submission that relies on
the inspected CM default while checking out that PT revision names an absent
module. An explicit `script` is already supported and recorded in task
`metadata.backtest`; this is a concrete compatibility risk for omitted scripts,
not evidence that existing submissions failed. No submission was performed.

## Launch timestamp boundary (source addendum, 2026-09-10)

Inspected `api/dispatch_daemon.py:244-255` and `dispatch/vm.py:120-127`,
blob-identical between source pin `ecd41d6` and review branch `ff392bf`.
`_launch_backtest_vm` assigns `metadata.backtest.launched_at` **after** awaiting
`_launch_backtest_vm_sync`. That helper calls `launch_worker`, which waits for
GCE instance creation with `op.result()`, then fetches the external IP and returns.

Thus `launched_at` is a dispatcher wall-clock observation after VM creation and
lookup, not queue admission, creation start, worker-startup-script start, or
replay start. `launched_at - task.created_at` can include queueing and creation;
`artifact.created_at - launched_at` excludes the already completed creation
interval and may overlap differently with startup work. Relabel those intervals
by their actual endpoints before calling them queue wait or inclusive worker
wall. Establish task/run/attempt and clock provenance before subtracting replay
wall from them. A small remainder is not proof that provisioning was cheap.
No GCE, worker, database, or deployed-state read was performed for this addendum.
