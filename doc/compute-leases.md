# Compute leases

The pool configuration starts every box off. The deployer must verify node
systemd behavior on a drained, approved box before enabling the rest of the
pool; the implementation tests do not contact lane boxes.

Agents use `compute_request(cpus, mem_gb, duration, priority, purpose, class,
pinned)` with duration in seconds, then `compute_status(lease_id)` while
waiting. Priorities are `v221-gate`, `normal`, and `low`; classes are `burst`
and `long`. A granted lease identifies its box, CPU slots, expiry, SSH alias,
workdir, and command prefix. Pinned requests restrict CPU placement; loose
requests have memory and CPU quota limits. Release with `compute_release`.
Only work run under the returned prefix is accounted to the lease.
Use `v221-gate` only for work named by EP or the deploy lead, and put the
authorizing ruling ID in the purpose. Requests above the normal per-lease slot
limit should explain their size in the purpose; the session limit still applies.
An ETA is the earliest known capacity change, not a promised start; it can be
unknown. A queue position of zero means a grant is being installed.

The planning API persists reservations and queues node actions. The separate
user service on cm-sessions invokes the node script through configured gcloud
IAP routes. The SSH aliases must use the approved IAP route and be available
on the calling cm-sessions host. This does not install keys or authorize a new
route from cm-manager. Reservations remain occupied until release is confirmed.

For the eventual cutover, Compute-Manager stops new `lq` work and lets old
runs drain. Set `cutover_ready` only after that freeze. The executor must
confirm each box is drained before enabling it; leave the pool and executor
examples disabled until those checks and the deployment review pass. Keep
`lq` unchanged during this implementation and retire it separately after the
agreed week.

Extensions are currently refused while any other session is queued, including
requests for other boxes. The configured total-duration cap is strict in v1;
longer work must request a fresh lease. The `lucas` Unix account is shared: anyone who can
use the privileged script can run in or release another known lease ID.
Agents must use the prefix for work to be accounted and protected by the slice.

The planned installation surface is SQL/API, daemon and MCP, the standalone
executor service, and on each box the root-owned `/usr/local/bin/cm-lease`,
`/etc/cm-lease.json`, and the reap service/timer. Copy
`deploy/cm-lease.example.json` and set the capacity for each box, create the
workdir root, install the root-owned script and config, and install/enable the
reap timer only as part of the separately coordinated deployment. Sudoers
must restrict access to this one script; review the shared-user trust boundary
before installing it. Configure the executor's environment and route file from
`deploy/cm-compute-executor.example.json`; keep credentials out of the repo.

For the deployer, after reviewing each box's limits and creating the parent
workdir, the node installation commands are:

```sh
sudo install -o root -g root -m 0755 scripts/cm-lease /usr/local/bin/cm-lease
sudo install -o root -g root -m 0644 /path/to/reviewed-node.json /etc/cm-lease.json
sudo install -o root -g root -m 0644 deploy/cm-lease-reap.service /etc/systemd/system/
sudo install -o root -g root -m 0644 deploy/cm-lease-reap.timer /etc/systemd/system/
sudo visudo -f /etc/sudoers.d/cm-lease
sudo systemctl daemon-reload
sudo systemctl enable --now cm-lease-reap.timer
```

The sudoers entry is `lucas ALL=(root) NOPASSWD: /usr/local/bin/cm-lease *`.
This grants that shared Unix account access to every `cm-lease` subcommand;
it does not distinguish individual agents. The deployer must create the
workdir root first and set each node's `max_slots` to its reviewed pool limit.
Install `scripts/cm-compute-executor` as `~/.local/bin/cm-compute-executor`
and `deploy/cm-compute-executor.service` as a user unit. Its private
environment file supplies `CM_API_URL`, `CM_API_TOKEN`, and
`CM_COMPUTE_EXECUTOR_CONFIG`. Install the pool JSON on the API host and set
`CM_COMPUTE_POOL_CONFIG`. Leave `cutover_ready` false and boxes disabled until
the freeze and drain checks above have been completed.

Idle, reclaim and expiry notices use the native board-push delivery channel.
The API counts an idle notice only after the recipient daemon reports that the
session observed it; failed or ambiguous delivery cannot authorize reclaim.
