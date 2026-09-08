# Cloud-first interactive sessions

Fresh sessions and planning launches use the default in `~/.cm/hosts.toml`.
The migration sets this to `sessions`. The Host field remains available for
explicit local execution; an unavailable cloud host produces an error.

Existing workspaces retain their host when adding sessions, reopening, resuming,
or starting workflows. Add Session and Workflow dialogs show Host; `Alt+h`
changes it. Choosing another host creates a separate checkout. It does not move
files or an existing conversation. Transcript resumes use the existing workspace
host; saved snapshots can seed a new workspace on their own host.

Cloud tasks use the ordinary sidebar layout. Local work has a dim inline `⌂`.
Status ordering is shared across hosts. Continuous tasks keep their dedicated
column and grouping. Session push/pull controls and `Alt+9` / `Alt+0` are removed;
Git operations and Spot backtests keep their existing controls.

Transcript and snapshot catalogs read from the owning host. Snapshot save,
preview, rename, delete and seeding work through operator-authenticated daemon
RPCs. A failed seed launch rolls back its materialized transcript/memory files.
Remote in-place launches use the main checkout without changing its branch.
A missing explicitly selected resume transcript is an error, not a fresh launch.

Deploy matching daemons before the TUI. Brain-only deployment and the ten-minute
stability gate follow [the holder guide](../HOWTO_HOLDER_BRAIN_SPLIT.md).

Validation: 852 TUI tests and 1,364 daemon library tests passed (four daemon tests
ignored), including remote wire options, host defaults, host switching, catalog
authorization, in-place preservation, seed rollback and resume composition.
Snapshot storage's existing 32 tests now run in the daemon library.
The remote catalog client has its own method gate; the actual cloud snapshot
picker was verified in Kitty after correcting the continuous-control gate it
previously called.

This implementation record does not certify the data/session migration. Its
cutover and verification are tracked separately in
[the migration plan](cloud-migration-plan.md) and private operator artifacts.
