# Resuming a session without changing its identity

Use **Alt+s → Resume** in the workspace on the host that owns the conversation.
Both local and cloud viewers ask that daemon to resolve the selected transcript.
For a known conversation, CM reuses its original session UID, task, parent,
workspace, label, permissions, memory cap and saved session preferences. The
Codex/Claude transcript is resumed in place. Codex's saved/host permission policy
continues to apply; this path does not add a permission override to `codex resume`.
Fresh sessions keep their existing YOLO launch behavior.

Messaging identity is `agent:<daemon-id>:<session-uid>`. Keeping both components
preserves the person's name, channel memberships, DMs, receipts and personal
notification preferences. A resume does not create another messaging person.
The picker’s current task and default label cannot reparent or rename a known
session. An exited viewer row is replaced rather than duplicated.

Only **inactive** sessions can be resumed. If either the conversation or its
original CM UID is running, CM refuses and tells you to attach to it. Concurrent
resume requests reserve both the transcript and the UID until spawning completes.
Workflow and continuous-task identities remain owned by their controllers; use
the workflow or continuous-task controls to resume those sessions.

A transient viewer attach failure leaves the resumed worker running and queues
another attach. Disconnecting the viewer does not stop the worker. For an explicit
resume, a missing transcript or ambiguous identity is an error; CM does not
silently launch a fresh conversation instead.

## Persistence and upgrade behavior

The daemon keeps `~/.cm/daemon-resume-identities.json`, an atomically written
metadata archive separate from the live registry and bounded exit cache. Closing
a row, evicting an old exit tombstone, or restarting the daemon does not discard
the archived identity. Current session data remains authoritative for transcript,
task and permissions. The viewer's background snapshot sends only an allow-listed
set of presentation preferences; it cannot change those authorization fields.
Full viewer entries also accompany new close tombstones so an immediate edit/close
cannot outrun the background preferences push.

Pre-upgrade workspace entries and exit tombstones are used as recovery sources.
CM can only restore metadata those older versions actually recorded. Settings
already discarded before this change cannot be reconstructed. A transcript with
no surviving CM ownership record receives its first known CM identity. Multiple
conflicting ownership records require an explicit repair rather than an arbitrary
choice. Resume is host-local; copying a transcript to another daemon does not
transfer its messaging identity.

Snapshot **seeding** remains separate: it deliberately creates a new session and
identity. Process-local state, such as an MCP-process-resident monitor, must be
rearmed after the agent process restarts; transcript and durable CM identity are
preserved, not arbitrary process memory.

New viewers call the operator-only `session.resume` method. An older daemon
rejects that method without spawning a replacement. Updated daemons also apply
identity recovery to an older viewer's `add_session(resume_id=...)`; install the
new viewer to hydrate all returned metadata correctly.

## Verification

Targeted isolated Rust coverage checks durable archive recovery, simultaneous
requests, live conversation and live UID rejection, ambiguity, scheduler ownership,
corruption handling, authorization, restore metadata and local/cloud viewer
hydration. Existing launch, revive, persistence and background-push tests cover
adjacent behavior.

`mcp_server/tests/integration_session_resume_identity.py --daemon-binary <binary>`
uses a private HOME, isolated daemon and installed Codex against a mock model. It
checks running-session rejection, closure and daemon restart, original UID/task/
permissions/preferences, history restoration and a completed subsequent turn.
It never connects to a live CM daemon or sends a real model request.
