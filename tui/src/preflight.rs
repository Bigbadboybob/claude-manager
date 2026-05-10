//! Preflight probe for the per-session memory cap. Runs once at TUI
//! startup; the result is cached for the lifetime of the run. See
//! DESIGN_MEMORY_CAP.md § Components / Preflight for the full spec.

use crate::memory_cap::MemoryCapAvailability;

/// Run the preflight probe. Returns `Available` only when *all four*
/// prerequisites of a real capped session are verified: env sanity,
/// scope creation with the production property set, cgroup-path
/// resolution, and property readback.
#[cfg(target_os = "linux")]
pub fn probe() -> MemoryCapAvailability {
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // Step 1: env sanity.
    let runtime_dir = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(s) if !s.is_empty() => PathBuf::from(s),
        _ => {
            return MemoryCapAvailability::Unavailable {
                reason: "XDG_RUNTIME_DIR not set".into(),
            };
        }
    };
    if !runtime_dir.join("systemd").is_dir() {
        return MemoryCapAvailability::Unavailable {
            reason: format!(
                "no systemd/ under XDG_RUNTIME_DIR ({})",
                runtime_dir.display()
            ),
        };
    }

    if Command::new("systemd-run")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .filter(|s| s.success())
        .is_none()
    {
        return MemoryCapAvailability::Unavailable {
            reason: "systemd-run not on PATH or non-functional".into(),
        };
    }

    // Step 2: spawn a real scope asynchronously, with the production
    // property set, so the probe matches what real sessions get.
    let unit_suffix = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let unit_name = format!("cm-preflight-{}", unit_suffix);
    let unit = format!("{}.scope", unit_name);

    let mut child = match Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            &format!("--unit={}", unit_name),
            "-p",
            "MemoryHigh=64M",
            "-p",
            "MemoryMax=128M",
            "-p",
            "MemorySwapMax=0",
            "--",
            "/bin/sleep",
            "30",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return MemoryCapAvailability::Unavailable {
                reason: format!("systemd-run spawn failed: {}", e),
            };
        }
    };

    // Compute the predicted cgroup path. For `--user --scope`, systemd
    // places the unit under the calling user's `app.slice`.
    let uid = unsafe { libc::getuid() };
    let cgroup_prefix = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{}.slice/user@{}.service/app.slice",
        uid, uid
    ));
    let scope_dir = cgroup_prefix.join(&unit);
    let cgroup_procs = scope_dir.join("cgroup.procs");

    // Step 3: wait up to ~500 ms for the scope to come up, then verify
    // path + properties.
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut probe_failure: Option<String> = None;
    loop {
        if cgroup_procs.exists() {
            let read = |name: &str| std::fs::read_to_string(scope_dir.join(name));
            let mh = read("memory.high");
            let mm = read("memory.max");
            let ms = read("memory.swap.max");
            match (mh, mm, ms) {
                (Ok(mh), Ok(mm), Ok(ms)) => {
                    let mh_n = mh.trim().parse::<u64>().ok();
                    let mm_n = mm.trim().parse::<u64>().ok();
                    let ms_trim = ms.trim();
                    let ok = mh_n == Some(64 * 1024 * 1024)
                        && mm_n == Some(128 * 1024 * 1024)
                        && (ms_trim == "0");
                    if !ok {
                        probe_failure = Some(format!(
                            "property readback mismatch: memory.high={} memory.max={} memory.swap.max={}",
                            mh.trim(),
                            mm.trim(),
                            ms_trim
                        ));
                    }
                }
                (mh, mm, ms) => {
                    probe_failure = Some(format!(
                        "property readback failed: {:?} {:?} {:?}",
                        mh.err(),
                        mm.err(),
                        ms.err()
                    ));
                }
            }
            break;
        }
        if Instant::now() >= deadline {
            probe_failure = Some(format!(
                "cgroup path did not appear within 500 ms: {}",
                cgroup_procs.display()
            ));
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Step 4: cleanup. Stop the unit and reap the child.
    let _ = Command::new("systemctl")
        .args(["--user", "stop", &unit])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();

    if let Some(reason) = probe_failure {
        return MemoryCapAvailability::Unavailable { reason };
    }
    MemoryCapAvailability::Available { cgroup_prefix }
}

#[cfg(not(target_os = "linux"))]
pub fn probe() -> MemoryCapAvailability {
    MemoryCapAvailability::Unavailable {
        reason: "memory cap requires Linux + systemd".into(),
    }
}
