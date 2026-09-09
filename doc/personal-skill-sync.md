# Personal skill sync

`~/.claude/skills` is the source of truth for personal skills. `~/.agents/skills` contains Codex adapters or symlinks to those Claude skills; `~/.codex/skills` is included when present. `scripts/cm-sync-skills` transfers only skill directories (a directory is eligible when it contains `SKILL.md`) over an explicit SSH/GCloud inventory. It never copies `.env`, credentials, private keys, or agent configuration, refuses links outside the three skill roots, verifies a SHA-256 manifest on the destination, and keeps remote-only skills. Changed destination files are backed up under `~/.cm/skill-sync/backups/` before replacement.

Create `~/.cm/skill-sync.json` on the source host. Set `source_hostname` to the output of `hostname`; this prevents an accidentally copied config from making another machine overwrite the fleet. Each target is either an SSH alias or a GCloud instance:

```json
{
  "source_hostname": "cm-sessions.us-east4-a.c.claude-manager-prod.internal",
  "targets": [
    {"name": "cm-manager", "ssh": "cm-manager"},
    {"name": "cashflow-vm", "gcloud": {"instance": "cashflow-vm", "project": "lucas-finance-automation", "zone": "us-central1-a"}}
  ]
}
```

Run `scripts/cm-sync-skills --dry-run` to inspect changes, then `scripts/cm-sync-skills` to sync all targets. Pass `--host cm-manager` to sync one target. The process is serialized by `~/.cm/skill-sync/sync.lock`; status and per-target errors are recorded in `status.json`, so an offline host does not prevent other hosts from being updated and is retried on the next run.

The source machine installs a user systemd timer with `scripts/install-cm-skill-sync`. It runs every five minutes and starts automatically after reboot when the user systemd manager is enabled. Existing Claude/Codex processes may need a restart to refresh their skill list; future continuous-task sessions inherit the normal user skill directories automatically.
