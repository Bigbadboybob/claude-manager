# Releasing the laptop TUI over SSH

Use this workflow for a Claude Manager TUI update built on `cm-sessions` and
installed on Owner's Linux x86-64 laptop. This is the workflow used for releases
such as `tui-51caf88`. Its reusable installer is
[`scripts/install-cm-tui.py`](../scripts/install-cm-tui.py).

The TUI runs on the laptop; agents and their Bash panes usually run on
`cm-sessions`. Installing a binary on the cloud VM does not update the laptop.
Current cloud-to-laptop access is a read-only data export. Unless a writable
deployment connection has actually been established, prepare the release and
give Owner the local-terminal command below. Report it as ready for installation,
not installed. A TUI-only change needs no daemon, holder, MCP, or API rollout.
For changes to those components, follow [the daemon deployment runbook](../HOWTO_HOLDER_BRAIN_SPLIT.md)
and the applicable component/messaging rollout instructions as well.

## Build the approved revision

1. Review the diff and complete the checks appropriate to it. Rust tests must use
   `scripts/cm-test-isolated` with a private `CARGO_TARGET_DIR`; tests can otherwise
   overwrite the live TUI manifest. Keep the test results for the release receipt.
2. Commit the intended changes. If Owner requested a merge, perform that workflow
   first. Select the exact committed revision to ship and use a clean checkout;
   preserve unrelated work. Don't edit sources while building or label an older
   binary with a newer commit.
3. From that checkout, build only the TUI using a private, reusable cache:

   ```bash
   export CM_TUI_TARGET="$HOME/.cm/builds/tui-release"
   CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR="$CM_TUI_TARGET" \
     cargo build --locked --release -p claude-manager-tui
   ```

   Add `--offline` when dependencies are cached. Avoid `~/.cm/shared-target` as a
   build directory: it holds installed binaries. A daemon *library* compilation
   is expected and does not require deploying or restarting a daemon.

## Package without changing an existing release

After the build succeeds, run this from the same checkout and shell. It creates
`~/.cm/releases/tui-<commit>/`, embeds the expected binary hash in the installer,
and refuses an existing release directory. Use a new release label if an earlier
package must be superseded; preserve the earlier binary and receipt.

```bash
python3 - <<'PY'
from pathlib import Path
from datetime import datetime, timezone
import ast, hashlib, json, os, shutil, subprocess

def git(*args):
    return subprocess.check_output(['git', *args], text=True).strip()

assert not git('status', '--porcelain'), 'Use a clean checkout; preserve other work.'
revision = git('rev-parse', 'HEAD')
binary = Path(os.environ['CM_TUI_TARGET']) / 'release/claude-manager-tui'
assert binary.is_file(), 'Build the selected revision first.'
release = Path.home() / '.cm/releases' / ('tui-' + revision[:7])
release.mkdir(exist_ok=False)
shutil.copy2(binary, release / 'claude-manager-tui')
digest = hashlib.sha256(binary.read_bytes()).hexdigest()
data = {'commit': revision, 'sha256': digest,
        'source': 'cm-sessions:' + str(release / 'claude-manager-tui')}
template = Path('scripts/install-cm-tui.py').read_text()
assert template.count('RELEASE = None') == 1
installer = template.replace('RELEASE = None', 'RELEASE = ' + repr(data))
ast.parse(installer)
(release / 'install-laptop.py').write_text(installer)
(release / 'install-laptop.py').chmod(0o755)
files = ['claude-manager-tui', 'install-laptop.py']
(release / 'SHA256SUMS').write_text(''.join(
    hashlib.sha256((release / name).read_bytes()).hexdigest() + '  ' + name + '\n'
    for name in files))
(release / 'release.json').write_text(json.dumps({
    **data, 'created_at': datetime.now(timezone.utc).isoformat(),
    'status': 'ready for laptop installation', 'component': 'tui',
}, indent=2) + '\n')
print(release)
PY
```

Verify `sha256sum -c SHA256SUMS` inside the release directory. Record the actual
checks and their results in `release.json`; do not invent test counts. The
installer template has regression checks in `scripts/test-install-cm-tui.py`:
atomic installation, repeat installation preserving the backup, corrupt/failed
transfers retaining the old binary, and cloud-host refusal. Run those if changing
the template. Check the binary's architecture with `file` when the build host or
laptop changes.

## Install and activate

Give Owner the concrete command with the actual release name, to run in a
**local laptop terminal outside a CM cloud Bash pane**:

```bash
ssh cm-sessions 'cat ~/.cm/releases/tui-<commit>/install-laptop.py' | python3
```

The installer downloads over the existing SSH alias, checks SHA-256 before
replacing anything, saves the previous binary as `claude-manager-tui.before-<commit>`,
and atomically replaces `~/.cm/shared-target/release/claude-manager-tui`. Repeating
the same installation preserves the original rollback copy. It changes no
manifest, terminal opacity, agent process, or daemon configuration.

Owner closes and reopens the TUI to load the new binary. Existing cloud sessions
keep running. Installation success is the installer's output (or a verified
laptop checksum); a cloud build or staging receipt alone does not prove it.
Post the release notice in `#cm-general` as required by `CLAUDE.md`, distinguishing
ready-for-installation from installed and stating the required Owner action.

To roll back on the laptop, copy the named backup to a temporary file beside the
installed binary, preserve executable permissions, atomically rename it over the
installed binary, and reopen the TUI. Do not truncate an executable in place or
restart the holder/agent sessions for a viewer rollback.

## Laptop daemon releases (including Messages changes)

The TUI's Messages view uses the **laptop's local daemon**. Deploying new
messaging RPCs on `cm-sessions` and `cm-manager` alone does not update the
Owner-facing store, counts, or mark-read behavior. Ship the laptop brain as well
when those behaviors change. The laptop must already run the holder/brain split;
this installer does not migrate a monolith or stop agent sessions.

The cloud packager is [`scripts/package-cm-daemon.py`](../scripts/package-cm-daemon.py).
From a clean, committed checkout on `cm-sessions`:

```bash
python3 scripts/test-install-cm-daemon.py
python3 scripts/test-install-cm-tui.py
python3 scripts/package-cm-daemon.py \
  --target-dir "$HOME/.local/share/<lane>/target" --offline --with-tui
```

Label the lane directory with `OWNER.md` before building, as for other private
builds. The packager labels its target and release directories. It builds the
release workspace when `--with-tui` is supplied, otherwise `cm-daemon` and
`cm-holder`, using a private target and two Cargo jobs. Omit `--offline` if
dependencies need downloading. The packager rejects a dirty or changing checkout,
the installed shared target, and an existing release directory. It never calls a
daemon RPC or replaces an installed binary.

The resulting `~/.cm/releases/daemon-<12-character-commit>/` contains:

- `cm-daemon` and `cm-holder` from the selected revision;
- `install-laptop-daemon.py`, with source paths and expected SHA-256 embedded;
- `SHA256SUMS`, `release.json`, and ownership/retention metadata;
- with `--with-tui`, the TUI binary and its existing checksum-embedded installer.

Packaging verifies `SHA256SUMS`. Record actual review/test results alongside the
receipt, keeping its checksums current if you edit it. Remove your private target
after packaging/checks; preserve staged releases and rollback backups.

Give Owner the concrete command printed by the packager. It runs in a **local
laptop terminal**, not a CM cloud Bash pane:

```bash
ssh cm-sessions 'cat ~/.cm/releases/daemon-<commit>/install-laptop-daemon.py' | python3
# Combined package: activate the brain, then install the TUI for its next launch.
ssh cm-sessions 'cat ~/.cm/releases/daemon-<commit>/install-laptop-daemon.py' | python3 - --with-tui
```

The daemon installer authenticates with the laptop's `~/.cm/operator-token` on
`~/.cm/daemon.sock`. It ignores inherited remote socket/token environment
overrides, refuses cloud hosts, and serializes installer invocations with a local
lock. Preflight requires a healthy split, strong operator authentication, matching
brain/holder session counts, and no restart already in flight.

The running brain's `/proc/<brain_pid>/exe` identifies the active pinned image
and its path, including the ` (deleted)` suffix left after an atomic update.
This is more reliable than the holder's original `--brain` command line after
later deploys. The destination is therefore the actual shared-target or `/opt`
path, and the restart always passes that exact `binary_path`. An explicit
`--binary-path /absolute/path/cm-daemon` overrides detection; when changing the
path, also account for the holder's startup configuration on a later full launch.
The ordinary default updates the existing startup location.

For a root-owned `/opt` destination, run `sudo -v` in the laptop terminal first,
then append `--sudo` to the installer command (`python3 - --sudo`). Only file
installation uses `sudo -n`; run the installer itself as the laptop user so it
uses that user's token, socket and state. It does not silently switch to a
writable shared target while the holder still pins `/opt`.

Before replacement it downloads and checks the binary, runs
`--daemon-preflight` against the laptop's durable state, rechecks the generation,
and saves the **running pinned image** and its SHA-256 as
`cm-daemon.before-<commit>` beside the destination. Repeated installs retain the
original backup. A sibling temporary file plus rename atomically replaces the
destination; a running executable is never truncated. Checksum, download or
preflight failures leave the installed brain unchanged.

Activation calls `daemon.restart` once and then verifies:

- `holder_epoch` increased by exactly one and the brain PID changed;
- the running `/proc/<new_brain_pid>/exe` SHA-256 equals the expected artifact;
- the holder PID/start time and executable hash stayed unchanged;
- the breaker is running, MCP preflight passed, and session counts are unchanged;
- those checks remain true during a 90-second initial stability check.

EOF or timeout from the restart call only begins verification; it never counts
as success. A refused or unverified restart reports failure, retains the installed
file and backup, and prints the rollback command. A late crash after the initial
check remains possible: the holder's breaker horizon is ten minutes. Do not call
the initial check a ten-minute soak. `--soak-seconds` can select a different
initial observation period, which the installer reports explicitly.

The printed rollback line reruns the same staged installer with `--rollback`
and the exact destination. It verifies the backup checksum, runs preflight,
atomically restores it, and performs the same restart/hash/epoch checks. It does
not rely on a potentially changed in-memory previous-pin slot. Example:

```bash
ssh cm-sessions 'cat ~/.cm/releases/daemon-<commit>/install-laptop-daemon.py' | python3 - --rollback
```

`cm-holder` is **packaged only**. The routine install neither overwrites nor
upgrades the running holder; a holder change still needs the separate
`daemon.upgrade_holder` procedure in the daemon runbook. The installer also does
not update MCP files or the planning API. The optional TUI install happens after
verified brain activation: Owner still closes and reopens the viewer. If that
second component fails, the brain remains activated and the TUI installer can be
retried separately. Use the TUI's own backup procedure to roll back the viewer.

Report cloud staging, laptop activation, and viewer relaunch separately. A lane
that is authorized only to stage a release must hand it off to its coordinator
without running the laptop install command.
