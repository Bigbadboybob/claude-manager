#!/usr/bin/env python3
"""Release the continuous-task documentation bundle and runtime guidance to a host.

Reproduces the layout at `~/.cm/docs/continuous-tasks/` (repo docs + doc/ +
doc/messaging/ + mcp_server/AGENT_GUIDE.md + scripts/cm-op) with a hash-verified
`documentation-release.json`, and installs the runtime guidance the agents
actually read: `~/.cm/policies/continuous-review-routing.md` and, when
`--mcp-guide` is given, `/opt/cm-daemon/mcp_server/AGENT_GUIDE.md` (sudo).
Previous copies are backed up under `~/.cm/audits/docs-release-<stamp>/before/`
on the target host.

    scripts/release-continuous-docs.py --host cm-manager --dry-run
    scripts/release-continuous-docs.py --host cm-manager --mcp-guide
    scripts/release-continuous-docs.py --host local
"""
import argparse
import datetime as dt
import hashlib
import io
import json
import pathlib
import subprocess
import sys
import tarfile

REPO = pathlib.Path(__file__).resolve().parents[1]
BUNDLE = "~/.cm/docs/continuous-tasks"
POLICY_SRC = "doc/continuous-review-routing.md"
POLICY_DST = "~/.cm/policies/continuous-review-routing.md"
GUIDE_SRC = "mcp_server/AGENT_GUIDE.md"
GUIDE_DST = "/opt/cm-daemon/mcp_server/AGENT_GUIDE.md"


def bundle_files():
    files = sorted(p for p in REPO.glob("*.md"))
    files += sorted(REPO.glob("doc/*.md"))
    files += sorted(p for p in REPO.glob("doc/messaging/**/*") if p.is_file())
    files += [REPO / GUIDE_SRC, REPO / "scripts/cm-op"]
    out = []
    for p in files:
        rel = p.relative_to(REPO).as_posix()
        if rel.startswith("doc/messaging/research/") and p.suffix not in {".py", ".json", ".md"}:
            continue
        out.append(rel)
    return out


def sha(path):
    return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default="cm-manager")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--mcp-guide", action="store_true", help="also install AGENT_GUIDE.md under /opt/cm-daemon (sudo)")
    args = ap.parse_args()
    rev = subprocess.run(["git", "-C", str(REPO), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    dirty = subprocess.run(["git", "-C", str(REPO), "status", "--porcelain"], capture_output=True, text=True).stdout.strip() != ""
    files = bundle_files()
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%d-%H%M%S")
    manifest = {
        "at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "source_revision": rev + ("+dirty" if dirty else ""),
        "status": "installed_and_hash_verified",
        "bundle": BUNDLE.replace("~", "/home/lucas"),
        "bundle_files": {f: sha(REPO / f) for f in files},
        "runtime_guidance": {POLICY_DST.replace("~", "/home/lucas"): sha(REPO / POLICY_SRC)},
        "backups": f"/home/lucas/.cm/audits/docs-release-{stamp}/before",
        "activation": "No daemon/session restart for docs and policy. Existing MCP connections keep the guide text they loaded; new connections load the new guide.",
    }
    if args.mcp_guide:
        manifest["runtime_guidance"][GUIDE_DST] = sha(REPO / GUIDE_SRC)
    print(f"{len(files)} bundle files, source {manifest['source_revision']}, host {args.host}")
    if args.dry_run:
        print(json.dumps({k: v for k, v in manifest.items() if k != "bundle_files"}, indent=1))
        return
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        for f in files:
            tar.add(REPO / f, arcname=f"bundle/{f}")
        tar.add(REPO / POLICY_SRC, arcname="policy/continuous-review-routing.md")
        tar.add(REPO / GUIDE_SRC, arcname="guide/AGENT_GUIDE.md")
        info = tarfile.TarInfo("manifest.json")
        data = json.dumps(manifest, indent=1).encode()
        info.size = len(data)
        tar.addfile(info, io.BytesIO(data))
    installer = f"""
set -euo pipefail
STAMP={stamp}
AUD=~/.cm/audits/docs-release-$STAMP; mkdir -p $AUD/before $AUD/stage
tar -xzf - -C $AUD/stage
[ -d {BUNDLE} ] && cp -a {BUNDLE} $AUD/before/continuous-tasks || true
[ -f {POLICY_DST} ] && cp -a {POLICY_DST} $AUD/before/continuous-review-routing.md || true
mkdir -p {BUNDLE} ~/.cm/policies
cp -a $AUD/stage/bundle/. {BUNDLE}/
cp $AUD/stage/manifest.json {BUNDLE}/documentation-release.json
cp $AUD/stage/policy/continuous-review-routing.md {POLICY_DST}
chmod +x {BUNDLE}/scripts/cm-op
python3 - <<'EOF'
import json,hashlib,os
m=json.load(open(os.path.expanduser('{BUNDLE}/documentation-release.json')))
bad=[f for f,h in m['bundle_files'].items() if hashlib.sha256(open(os.path.expanduser('{BUNDLE}/'+f),'rb').read()).hexdigest()!=h]
p=os.path.expanduser('{POLICY_DST}')
if hashlib.sha256(open(p,'rb').read()).hexdigest()!=m['runtime_guidance'][p]: bad.append(p)
print('hash-verified' if not bad else 'MISMATCH '+str(bad)); raise SystemExit(1 if bad else 0)
EOF
"""
    if args.mcp_guide:
        installer += f"""
[ -f {GUIDE_DST} ] && cp -a {GUIDE_DST} $AUD/before/AGENT_GUIDE.md || true
sudo cp $AUD/stage/guide/AGENT_GUIDE.md {GUIDE_DST}
python3 -c "import hashlib,json;m=json.load(open('/home/lucas/.cm/docs/continuous-tasks/documentation-release.json'));h=hashlib.sha256(open('{GUIDE_DST}','rb').read()).hexdigest();print('guide-verified' if h==m['runtime_guidance']['{GUIDE_DST}'] else 'GUIDE MISMATCH');raise SystemExit(0 if h==m['runtime_guidance']['{GUIDE_DST}'] else 1)"
"""
    installer += "echo installed $AUD\n"
    if args.host == "local":
        out = subprocess.run(["bash", "-c", installer], input=buf.getvalue(), capture_output=True, timeout=300)
    else:
        ssh = ["ssh", "-o", "ConnectTimeout=15", args.host]
        up = subprocess.run(ssh + ["cat > /tmp/cm-docs-release.sh"], input=installer.encode(), capture_output=True, timeout=60)
        if up.returncode != 0:
            raise SystemExit(f"stage installer failed: {up.stderr.decode()[:300]}")
        out = subprocess.run(ssh + ["bash /tmp/cm-docs-release.sh"], input=buf.getvalue(), capture_output=True, timeout=300)
    sys.stdout.write(out.stdout.decode())
    sys.stderr.write(out.stderr.decode())
    if out.returncode != 0:
        raise SystemExit(f"install failed rc={out.returncode}")


if __name__ == "__main__":
    main()
