# Attach stream progress

The TUI requests `heartbeat: true` on `attach.direct`. A daemon that supports
it replies with `heartbeat_interval_secs: 10` and sends a heartbeat on a quiet
attach stream about every 10 seconds. Output frames also establish progress.
The TUI tracks inbound bytes, including partial frames, and after 45 seconds
without progress uses the existing reconnect indicator, reattach queue and
backoff. This is a transport suspicion, not evidence that the agent exited or
that its work failed. A long scheduling pause or congested connection can also
cause a reconnect. Sleep and wake do not cause repeated concurrent attaches:
the existing queue deduplicates by session and backs off repeated failures.

The silence watchdog is armed only when the direct attach response explicitly
acknowledges the expected interval. An old daemon's direct response, and the
legacy ticket/`attach.open` fallback, leave it disabled. An old TUI does not
request the capability, so a new daemon sends it no heartbeat frames. Both
updated daemon and TUI are needed for this detection; older peers retain the
existing EOF, socket hangup and tunnel-generation recovery. SSH keepalives and
an open socket alone do not prove attach-stream data is reaching the TUI.
