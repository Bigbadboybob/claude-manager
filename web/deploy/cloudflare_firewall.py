#!/usr/bin/env python3
"""Print a reviewable GCP firewall script or nginx allowlist. Never apply it."""
import argparse
import ipaddress
import json
from pathlib import Path
import shlex
import subprocess
from urllib.request import Request, urlopen


def ranges(version, fixture=None):
    if fixture:
        body = Path(fixture).read_text()
    else:
        request = Request(f"https://www.cloudflare.com/ips-v{version}/",
                          headers={"User-Agent": "cm-board-web-firewall/1.0", "Accept": "text/plain"})
        with urlopen(request, timeout=15) as response:
            body = response.read(65537).decode("ascii")
    if len(body) > 65536:
        raise ValueError("Unexpected Cloudflare range response size")
    networks = [ipaddress.ip_network(line.strip()) for line in body.splitlines() if line.strip()]
    if not networks or any(n.version != version or not n.is_global or n.prefixlen < (8 if version == 4 else 16)
                           for n in networks):
        raise ValueError("Refusing empty, mixed, private or overly broad Cloudflare ranges")
    return [str(n) for n in networks]


def allows_443(rule):
    if rule.get("disabled") or rule.get("direction", "INGRESS") != "INGRESS" or int(rule.get("priority", 1000)) > 901:
        return False
    for allow in rule.get("allowed", []):
        protocol = str(allow.get("IPProtocol", ""))
        if protocol == "all":
            return True
        if protocol not in ("tcp", "6"):
            continue
        if not allow.get("ports"):
            return True
        for port in allow["ports"]:
            bounds = str(port).split("-", 1)
            if int(bounds[0]) <= 443 <= int(bounds[-1]):
                return True
    return False


def audit(rules, nets, tag, network, update):
    if not isinstance(rules, list):
        raise ValueError("Expected a GCP firewall rule list")
    conflicts = []
    for rule in rules:
        if not allows_443(rule):
            continue
        # Updates may retain exactly our restricted allow rules. No broad
        # sources, extra targets, protocols or ports qualify for this exception.
        managed = any(rule.get("name") == f"cm-board-cloudflare-v{version}"
                      and rule.get("priority") == 900
                      and rule.get("targetTags") == [tag]
                      and rule.get("network", "").rsplit("/", 1)[-1] == network
                      and set(rule.get("sourceRanges", [])) <= set(nets[version])
                      and bool(rule.get("sourceRanges"))
                      and not any(rule.get(k) for k in ("sourceTags", "sourceServiceAccounts", "targetServiceAccounts"))
                      and rule.get("allowed") == [{"IPProtocol": "tcp", "ports": ["443"]}]
                      for version in (4, 6))
        if not (update and managed):
            conflicts.append(rule.get("name", "<unnamed>"))
    if conflicts:
        raise ValueError("Existing ingress allow rules can override the board deny (priority <=901): " + ", ".join(conflicts))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--format", choices=["gcp", "nginx"], default="gcp")
    parser.add_argument("--project", default="claude-manager-prod")
    parser.add_argument("--network", default="default")
    parser.add_argument("--tag", default="cm-board-origin")
    parser.add_argument("--update", action="store_true", help="Generate updates to existing rules")
    parser.add_argument("--ipv4-file", help="Use a saved published range list for offline review")
    parser.add_argument("--ipv6-file")
    parser.add_argument("--rules-file", help="Read an exported GCP firewall JSON list instead of invoking gcloud")
    args = parser.parse_args()
    nets = {4: ranges(4, args.ipv4_file), 6: ranges(6, args.ipv6_file)}
    if args.format == "nginx":
        print("# Cloudflare peer ranges; regenerated with web/deploy/cloudflare_firewall.py")
        for version in (4, 6):
            for net in nets[version]:
                print(f"allow {net};")
        print("deny all;")
        return
    if args.rules_file:
        rules = json.loads(Path(args.rules_file).read_text())
    else:
        result = subprocess.run(["gcloud", "compute", "firewall-rules", "list", "--project", args.project, "--format=json"],
                                check=True, capture_output=True, text=True, timeout=30)
        rules = json.loads(result.stdout)
    audit(rules, nets, args.tag, args.network, args.update)
    print("#!/bin/sh\nset -eu\n# REVIEW before executing. Only TCP 443 on tagged instances is affected.")
    print("# Add cm-board-origin to cm-manager only. Audit pre-existing/hierarchical rules first.")
    # A specific deny prevents a broader lower-priority default HTTPS allow
    # from exposing the origin. Separate rules are required for IPv4/IPv6.
    for version in (4, 6):
        for allow in (False, True):
            name = f"cm-board-{'cloudflare' if allow else 'deny-other'}-v{version}"
            cmd = ["gcloud", "compute", "firewall-rules", "update" if args.update else "create", name,
                   "--project", args.project, "--priority", "900" if allow else "901",
                   "--target-tags", args.tag, "--source-ranges",
                   ",".join(nets[version]) if allow else ("0.0.0.0/0" if version == 4 else "::/0")]
            if args.update:
                # update preserves the existing action; gcloud has no --deny.
                cmd += ["--rules", "tcp:443"]
            else:
                cmd += ["--network", args.network, "--direction", "INGRESS", "--action", "ALLOW" if allow else "DENY",
                        "--rules", "tcp:443"]
            print(shlex.join(cmd))


if __name__ == "__main__":
    main()
