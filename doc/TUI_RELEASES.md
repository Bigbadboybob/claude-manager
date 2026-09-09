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
