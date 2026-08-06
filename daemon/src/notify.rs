//! Operator push alerts — the daemon's out-of-band "wake the human" channel.
//!
//! Born from the 2026-08-03 auth-expiry incident: two consumer orchestrators
//! sat wedged for 3.5 days with the failure fully *surfaced* (daemon log
//! lines, runs.jsonl audit rows) but never *pushed* — the operator found out
//! by asking why no output had appeared. Surfacing is not alerting.
//!
//! The push mechanism is deliberately not baked in: the daemon shells out to
//! a configured command (`notify_command` in `daemon.toml`), passing the
//! message as the single argument. On cm-manager that is the existing
//! `~/.cm/bin/cm-notify` Telegram script (`scripts/cm-notify` in this repo);
//! any executable with the same contract works. `CM_NOTIFY_TAG` carries the
//! source tag so the operator can tell which subsystem alerted.
//!
//! Default is UNSET (`None`): every alert still lands on stderr (the systemd
//! journal), but nothing external fires — so tests and dev daemons on
//! machines that DO have Telegram creds can never send real messages. Enable
//! per host:
//!
//! ```toml
//! notify_command = "/home/lucas/.cm/bin/cm-notify"
//! ```

use std::process::Stdio;

/// Emit an operator alert: always to stderr, and — when `command` is
/// configured — via the external notifier, detached (a slow/hung notifier
/// must never stall the scheduler tick that fired it; `cm-notify`'s curl
/// alone can block 15s). The child is reaped by a short-lived thread so it
/// doesn't linger as a zombie. Best-effort everywhere: a missing/broken
/// command logs and moves on.
pub fn notify_operator(command: Option<&str>, tag: &str, message: &str) {
    eprintln!("cm-daemon: ALERT [{}] {}", tag, message);
    let Some(cmd) = command.map(str::trim).filter(|c| !c.is_empty()) else {
        return;
    };
    match std::process::Command::new(cmd)
        .arg(message)
        .env("CM_NOTIFY_TAG", tag)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::Builder::new()
                .name("cm-notify-reap".into())
                .spawn(move || {
                    let _ = child.wait();
                })
                .ok();
        }
        Err(e) => {
            eprintln!(
                "cm-daemon: notify_command '{}' failed to spawn: {} (alert delivered \
                 to stderr only)",
                cmd, e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The configured command receives the message as its single argument and
    /// the tag via `CM_NOTIFY_TAG`.
    #[test]
    fn notify_runs_configured_command_with_message_and_tag() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        let script = dir.path().join("notify.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"$CM_NOTIFY_TAG|$1\" > '{}'\n", out.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        notify_operator(Some(script.to_str().unwrap()), "auth", "creds expired");

        // Detached spawn — poll briefly for the side effect.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(s) = std::fs::read_to_string(&out) {
                assert_eq!(s.trim(), "auth|creds expired");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "notifier never ran");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// No command configured / a missing binary: stderr-only, no panic.
    #[test]
    fn notify_tolerates_unset_and_broken_commands() {
        notify_operator(None, "t", "m");
        notify_operator(Some("   "), "t", "m");
        notify_operator(Some("/nonexistent/cm-notify"), "t", "m");
    }
}
