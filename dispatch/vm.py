"""Launch and manage GCP spot VMs for worker tasks."""
import logging
import urllib.request
import uuid
from google.api_core import exceptions as gax
from google.cloud import compute_v1
from dispatch.config import GCP_PROJECT, GCP_ZONE, VM_MACHINE_TYPE, VM_IMAGE_FAMILY, VM_IMAGE_PROJECT, API_TOKEN
from pathlib import Path

logger = logging.getLogger("cm.dispatch")

# GCE caps instance metadata at 256KB/value and 512KB total. The backtest
# payload (inline YAML config + fields) plus the startup script sit far
# under that, but reject pathological values with a clear error instead of
# letting the insert fail opaquely.
_METADATA_VALUE_MAX_BYTES = 250_000

# ---------------------------------------------------------------------------
# Machine-type / boot-disk compatibility
# ---------------------------------------------------------------------------
# Machine families whose instances CANNOT boot from a pd-standard disk: GCE
# rejects the insert outright (400, "Invalid value for field
# '...initializeParams.diskType'" / UNSUPPORTED_OPERATION). The backtest lane
# defaults to pd-standard (SSD_TOTAL_GB quota pressure in the PMS region —
# see BACKTEST_VM_DEFAULTS), so a bare `machine_type="c3-standard-16"`
# submission would fail VM creation forever without the auto-correction in
# `resolve_disk_type`.
#
# Deliberately mirrored, NOT imported, in two places that ship without the
# `dispatch` package: `mcp_server/server.py::_NO_PD_STANDARD_FAMILIES`
# (deployed standalone to /opt/cm-daemon/mcp_server) and
# `daemon/src/control/methods.rs::NO_PD_STANDARD_FAMILIES` (Rust). Keep the
# three in sync; this one is authoritative because it is the last gate before
# the insert.
NO_PD_STANDARD_FAMILIES = frozenset({
    "c3", "c3d", "c4", "c4a", "c4d", "n4", "h3", "z3", "m4", "x4",
    "a3", "a4", "g2",
})
# What a pd-standard request degrades to on those families. pd-balanced is
# the cheapest universally-supported alternative — note it draws on
# SSD_TOTAL_GB quota, which pd-standard does not.
PD_STANDARD_FALLBACK_DISK_TYPE = "pd-balanced"
DEFAULT_DISK_TYPE = "pd-balanced"


def machine_family(machine_type: str) -> str:
    """The GCE machine FAMILY of a machine type ('c3-standard-16' -> 'c3')."""
    return (machine_type or "").strip().split("-", 1)[0].lower()


def supports_pd_standard(machine_type: str) -> bool:
    """Whether `machine_type`'s family can boot from a pd-standard disk.

    Unknown/empty families are assumed compatible: a stale allow-list must
    never silently rewrite a working submission's disk type.
    """
    return machine_family(machine_type) not in NO_PD_STANDARD_FAMILIES


def resolve_disk_type(machine_type: str, disk_type: str | None, *,
                      explicit: bool = False) -> str:
    """Boot-disk type for `machine_type`, auto-corrected for pd-standard gaps.

    `disk_type` is the value the caller's merge produced (config default +
    per-task metadata.vm); `explicit=True` means the SUBMITTER named it, and
    it is then returned untouched — an operator override is never silently
    rewritten. An explicit unsupported pair still fails the insert, but that
    failure is now a deterministic spec error (see `classify_launch_error`)
    which the dispatcher blocks on instead of retrying forever.

    Only pd-standard is corrected: it is the one lane default that some
    families reject. pd-ssd / pd-balanced / hyperdisk-* pass through.
    """
    resolved = (disk_type or "").strip() or DEFAULT_DISK_TYPE
    if explicit or resolved != "pd-standard":
        return resolved
    if supports_pd_standard(machine_type):
        return resolved
    return PD_STANDARD_FALLBACK_DISK_TYPE


# Substrings that mark a TRANSIENT / capacity failure — quota, stockout,
# throttling, backend hiccups. These keep the historical retry behaviour
# (requeue to backlog): the same request may well succeed later.
_CAPACITY_ERROR_MARKERS = (
    "quota", "exhaust", "stockout", "does not have enough resources",
    "resource pool", "try a different zone", "rate limit", "ratelimitexceeded",
    "backend error", "internal error", "try again", "unavailable",
    "deadline exceeded", "timeout", "timed out", "connection reset",
    "service_unavailable", "resource_availability",
)
# Substrings that mark a DETERMINISTIC spec rejection — the request is
# malformed for this machine type / disk type / image and will fail
# identically on every retry.
_SPEC_ERROR_MARKERS = (
    "not supported", "unsupported", "does not support",
    "invalid value for field", "unknown disk type", "invalid machine type",
    "was not found", "no such object", "invalid resource usage",
    "must be a valid",
)


def classify_launch_error(exc: BaseException) -> str:
    """Classify a VM-creation failure as ``"spec"`` or ``"capacity"``.

    ``"spec"`` = deterministic: the instance resource is invalid for the
    requested machine type / disk type / image, so retrying re-fails
    identically. The backtest lane BLOCKS such a task instead of requeueing
    it (a requeued row lands back at the head of its priority band and
    head-of-line-blocks the whole lane — cm bug 24e3d6ff, first hit by a
    c3 machine type against the pd-standard boot-disk default).

    ``"capacity"`` = everything else, including anything unrecognised. The
    default is deliberately the historical behaviour (requeue + retry): a
    misfiled capacity error only costs a retry, while a misfiled spec error
    would silently kill a runnable submission.
    """
    # Local payload/programming errors raised before we ever reach GCE
    # (e.g. the metadata-size guard above) are deterministic by definition.
    if isinstance(exc, (ValueError, TypeError, KeyError)):
        return "spec"

    text = f"{type(exc).__name__}: {exc}".lower()
    if any(marker in text for marker in _CAPACITY_ERROR_MARKERS):
        return "capacity"

    # google-api-core maps the compute LRO's http_error_status_code through
    # from_http_status, so a rejected insert arrives as BadRequest(400) /
    # InvalidArgument(400) / NotFound(404).
    if isinstance(exc, (gax.BadRequest, gax.InvalidArgument, gax.NotFound)):
        return "spec"
    if getattr(exc, "code", None) in (400, 404):
        return "spec"
    if any(marker in text for marker in _SPEC_ERROR_MARKERS):
        return "spec"
    return "capacity"


def read_worker_script(filename: str = "startup.sh") -> str:
    """Read a worker VM startup script from worker/ by filename."""
    return (Path(__file__).parent.parent / "worker" / filename).read_text()


def launch_worker(task_id: str, repo_url: str, repo_branch: str,
                  prompt: str, manager_callback_url: str, *,
                  overrides: dict | None = None,
                  startup_script: str | None = None,
                  extra_metadata: dict[str, str] | None = None,
                  network_tags: list[str] | None = None) -> tuple[str, str]:
    """Create a spot VM for a task. Returns (instance_name, external_ip).

    `overrides` may carry per-task VM settings (metadata.vm on backtest
    tasks): project, zone, machine_type, image_family, image_project,
    service_account, disk_size_gb, disk_type. Anything absent falls back to
    the process-global config values, so existing callers are unchanged.

    `disk_type` is passed through VERBATIM — machine-type compatibility is
    the caller's call (see `resolve_disk_type`, applied by the backtest lane
    before it gets here), because only the caller knows whether the value was
    an operator override or a lane default.

    `network_tags` replaces the default tag set. The default keeps the
    legacy `allow-ttyd` tag for the general worker lane; the backtest lane
    passes its dedicated scoped tag instead (see ensure_ttyd_firewall) so
    its ttyd is never matched by a 0.0.0.0/0 rule.

    The instance name carries a random suffix: SPOT preemption STOPs the
    instance (instance_termination_action="STOP"), so a preempt-requeued
    task's relaunch would collide with its still-existing old instance if
    names were derived from the task id alone.
    """
    client = compute_v1.InstancesClient()

    o = overrides or {}
    project = o.get("project") or GCP_PROJECT
    zone = o.get("zone") or GCP_ZONE
    machine_type = o.get("machine_type") or VM_MACHINE_TYPE
    image_family = o.get("image_family") or VM_IMAGE_FAMILY
    image_project = o.get("image_project") or VM_IMAGE_PROJECT
    sa_email = o.get("service_account") or "default"
    disk_size_gb = int(o.get("disk_size_gb") or 50)
    disk_type = o.get("disk_type") or DEFAULT_DISK_TYPE

    instance_name = f"cm-worker-{task_id[:8]}-{uuid.uuid4().hex[:6]}"

    if startup_script is None:
        startup_script = read_worker_script("startup.sh")

    items = [
        compute_v1.Items(key="startup-script", value=startup_script),
        compute_v1.Items(key="task-id", value=task_id),
        compute_v1.Items(key="repo-url", value=repo_url),
        compute_v1.Items(key="repo-branch", value=repo_branch),
        compute_v1.Items(key="task-prompt", value=prompt),
        compute_v1.Items(key="manager-callback-url", value=manager_callback_url),
        compute_v1.Items(key="api-token", value=API_TOKEN),
    ]
    for key, value in (extra_metadata or {}).items():
        if len(value.encode("utf-8")) > _METADATA_VALUE_MAX_BYTES:
            raise ValueError(
                f"instance metadata value for {key!r} exceeds "
                f"{_METADATA_VALUE_MAX_BYTES} bytes (GCE limit)"
            )
        items.append(compute_v1.Items(key=key, value=value))

    instance = compute_v1.Instance(
        name=instance_name,
        machine_type=f"zones/{zone}/machineTypes/{machine_type}",
        scheduling=compute_v1.Scheduling(
            provisioning_model="SPOT",
            instance_termination_action="STOP",
            on_host_maintenance="TERMINATE",
        ),
        disks=[
            compute_v1.AttachedDisk(
                auto_delete=True,
                boot=True,
                initialize_params=compute_v1.AttachedDiskInitializeParams(
                    source_image=f"projects/{image_project}/global/images/family/{image_family}",
                    disk_size_gb=disk_size_gb,
                    disk_type=f"zones/{zone}/diskTypes/{disk_type}",
                ),
            ),
        ],
        network_interfaces=[
            compute_v1.NetworkInterface(
                access_configs=[
                    compute_v1.AccessConfig(name="External NAT"),
                ],
            ),
        ],
        metadata=compute_v1.Metadata(items=items),
        service_accounts=[
            compute_v1.ServiceAccount(
                # "default" resolves to the default compute SA of the project
                # the instance is created IN — backtest workers automatically
                # get the prediction-market-scalper SA.
                email=sa_email,
                scopes=["https://www.googleapis.com/auth/cloud-platform"],
            ),
        ],
        tags=compute_v1.Tags(items=list(network_tags) if network_tags else ["cm-worker", "allow-ttyd"]),
    )

    op = client.insert(project=project, zone=zone, instance_resource=instance)
    op.result()  # Wait for creation

    # Get the external IP
    inst = client.get(project=project, zone=zone, instance=instance_name)
    external_ip = inst.network_interfaces[0].access_configs[0].nat_i_p

    return instance_name, external_ip


def _self_external_ip() -> str | None:
    """External IP of the host this dispatcher runs on (None off-GCE).

    Keeps cm-manager itself inside the backtest-ttyd allowlist without
    hardcoding its address anywhere.
    """
    req = urllib.request.Request(
        "http://metadata.google.internal/computeMetadata/v1/instance/"
        "network-interfaces/0/access-configs/0/external-ip",
        headers={"Metadata-Flavor": "Google"},
    )
    try:
        with urllib.request.urlopen(req, timeout=2) as resp:
            return resp.read().decode().strip() or None
    except Exception:
        return None


_FIREWALL_PERM_WARNED: set[str] = set()


def ensure_ttyd_firewall(project: str, tag: str, source_ranges: list[str]) -> None:
    """Idempotently align the scoped ttyd ingress rule for backtest workers.

    One rule named after ``tag`` in ``project``'s default network: tcp:8080
    from ``source_ranges`` plus this host's own external IP, to instances
    carrying ``tag``. Replaces the old ``allow-ttyd`` 0.0.0.0/0 rule on this
    lane — the workers' ttyd (a ROOT web terminal) was internet-reachable
    and actively probed by scanners within hours of boot.

    Best-effort by design, and every failure mode fails CLOSED (no rule /
    stale rule -> ingress denied, never widened):
      - The cm-manager SA holds only instanceAdmin on the backtest project,
        so firewall calls may 403; the rule is then managed manually (the
        warning carries the exact gcloud command) and this degrades to a
        warn-once no-op.
      - No reachable source ranges at all -> skip, warn, leave denied.
    Callers must also wrap this in try/except: a firewall hiccup must never
    stop a backtest launch (it only costs ttyd reachability).
    """
    self_ip = _self_external_ip()
    ranges = list(dict.fromkeys(
        source_ranges + ([f"{self_ip}/32"] if self_ip else [])
    ))
    if not ranges:
        logger.warning(
            f"ttyd firewall {tag}@{project}: no source ranges "
            f"(CM_BACKTEST_TTYD_SOURCE_RANGES unset and no GCE self-IP) — "
            f"leaving :8080 ingress denied; ttyd reachable via ssh tunnel only"
        )
        return

    client = compute_v1.FirewallsClient()
    desired = compute_v1.Firewall(
        name=tag,
        network="global/networks/default",
        direction="INGRESS",
        allowed=[compute_v1.Allowed(I_p_protocol="tcp", ports=["8080"])],
        source_ranges=ranges,
        target_tags=[tag],
        description=(
            "ttyd on cm backtest workers — operator + cm-manager IPs only. "
            "Managed by claude-manager dispatch/vm.py::ensure_ttyd_firewall."
        ),
    )
    try:
        try:
            existing = client.get(project=project, firewall=tag)
        except gax.NotFound:
            existing = None
        if existing is not None:
            if (sorted(existing.source_ranges) == sorted(ranges)
                    and list(existing.target_tags) == [tag]
                    and [(a.I_p_protocol, sorted(a.ports)) for a in existing.allowed]
                    == [("tcp", ["8080"])]):
                return
            client.update(project=project, firewall=tag,
                          firewall_resource=desired).result()
            logger.info(f"ttyd firewall {tag}@{project}: updated, sources={ranges}")
        else:
            client.insert(project=project, firewall_resource=desired).result()
            logger.info(f"ttyd firewall {tag}@{project}: created, sources={ranges}")
    except gax.PermissionDenied:
        if project not in _FIREWALL_PERM_WARNED:
            _FIREWALL_PERM_WARNED.add(project)
            logger.warning(
                f"ttyd firewall {tag}@{project}: SA lacks firewall perms — "
                f"managing the rule manually. Align it with: gcloud compute "
                f"firewall-rules update {tag} --project={project} "
                f"--source-ranges={','.join(ranges)} (create with the same "
                f"ranges, --allow=tcp:8080 --target-tags={tag}, if missing)"
            )


def delete_worker(instance_name: str, project: str = GCP_PROJECT,
                  zone: str = GCP_ZONE):
    """Delete a worker VM. Backtest workers live in another project/zone —
    callers with a task row should resolve the location from metadata.vm."""
    client = compute_v1.InstancesClient()
    try:
        op = client.delete(project=project, zone=zone, instance=instance_name)
        op.result()
    except Exception:
        pass  # Already deleted


def get_worker_ip(instance_name: str, project: str = GCP_PROJECT,
                  zone: str = GCP_ZONE) -> str | None:
    """Get the external IP of a worker VM."""
    client = compute_v1.InstancesClient()
    try:
        inst = client.get(project=project, zone=zone, instance=instance_name)
        return inst.network_interfaces[0].access_configs[0].nat_i_p
    except Exception:
        return None
