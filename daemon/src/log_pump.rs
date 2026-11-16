//! Timestamped `daemon.log`.
//!
//! Daemon diagnostics are plain `eprintln!`s. They used to go straight to
//! `log_path` (stdout/stderr dup2'd onto it) with no time on them, so on
//! 2026-10-15, when Owner saw keypresses arrive minutes late, the log could
//! not say when anything happened.
//!
//! The brain now points stdout/stderr at a pipe read by a small child
//! process (this binary run as `--log-pump <path>`) that prefixes every line
//! with an RFC3339 UTC timestamp (ms) and appends it to the file. A child,
//! not a thread, so a crashing brain's last lines — often the panic message
//! — are still drained and written; the pump exits when every writer is
//! gone. If the pump cannot start or dies, the brain falls back to writing
//! the file directly, unstamped, as before.

use std::io::{BufRead, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// The prefix a line gets: `2026-10-15T23:42:01.123Z `.
pub fn stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ ").to_string()
}

/// `--log-pump <path>`: copy stdin to `path` line by line, each line
/// prefixed with [`stamp`]. Returns at EOF (every writer closed).
pub fn run_pump(path: &str) -> i32 {
    // Drop every descriptor inherited from the brain except stdio: one
    // without CLOEXEC (a socket or PTY master handed over by the holder)
    // must not stay open in here for as long as the pump runs.
    close_inherited();
    // A reload signal meant for the brain must not end the log.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
    let mut file = match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cm-daemon log pump: open {path}: {e}");
            return 1;
        }
    };
    pump(std::io::stdin().lock(), &mut file);
    0
}

fn close_inherited() {
    let fds: Vec<i32> = std::fs::read_dir("/proc/self/fd")
        .map(|dir| {
            dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .filter(|&fd| fd > 2)
                .collect()
        })
        .unwrap_or_default();
    for fd in fds {
        unsafe {
            libc::close(fd);
        }
    }
}

fn pump(mut input: impl BufRead, out: &mut impl Write) {
    let mut line = Vec::new();
    loop {
        line.clear();
        match input.read_until(b'\n', &mut line) {
            Ok(0) => return,
            Ok(_) => {
                if !line.ends_with(b"\n") {
                    line.push(b'\n');
                }
                let mut stamped = stamp().into_bytes();
                stamped.extend_from_slice(&line);
                // One write per line, so lines from several writers never
                // interleave mid-line in the file.
                let _ = out.write_all(&stamped);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Point this process's stdout/stderr at `path`, through a timestamping
/// pump when one can be started.
pub fn install(path: &str) {
    match start_pump(path) {
        Ok(()) => eprintln!("cm-daemon: stdout/stderr → {path} (timestamped; daemon.toml log_path)"),
        Err(e) => {
            direct(path);
            eprintln!("cm-daemon: stdout/stderr → {path} (daemon.toml log_path; no timestamps: {e})");
        }
    }
}

/// The old behavior: the file itself on fds 1 and 2.
fn direct(path: &str) {
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => unsafe {
            libc::dup2(f.as_raw_fd(), 1);
            libc::dup2(f.as_raw_fd(), 2);
        },
        Err(e) => eprintln!("cm-daemon: could not open log_path {path}: {e} — keeping current stderr"),
    }
}

fn start_pump(path: &str) -> std::io::Result<()> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let log = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    // /proc/self/exe, not current_exe(): a deploy may have replaced the
    // binary on disk.
    let mut child = std::process::Command::new("/proc/self/exe")
        .arg("--log-pump")
        .arg(path)
        .stdin(std::process::Stdio::from(read.try_clone()?))
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    unsafe {
        libc::dup2(write.as_raw_fd(), 1);
        libc::dup2(write.as_raw_fd(), 2);
    }
    drop(write);
    // Keep a read end here: if the pump dies, writes then fill the pipe
    // instead of failing (`eprintln!` panics on a failed write), and the
    // watcher below switches to the file before it fills.
    let path = path.to_owned();
    std::thread::Builder::new()
        .name("cm-log-pump-watch".into())
        .spawn(move || {
            let status = child.wait();
            direct(&path);
            drop(read);
            eprintln!("cm-daemon: log pump exited ({status:?}); logging to {path} without timestamps");
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_line_gets_a_utc_millisecond_stamp_and_a_final_partial_line_is_kept() {
        let mut out = Vec::new();
        pump(&b"first\nsecond line\n\nno newline"[..], &mut out);
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        for (line, body) in lines.iter().zip(["first", "second line", "", "no newline"]) {
            let (ts, rest) = line.split_at(25);
            assert_eq!(rest, body);
            assert!(chrono::DateTime::parse_from_rfc3339(ts.trim_end()).is_ok(), "{ts}");
            assert!(ts.ends_with("Z ") && ts.as_bytes()[19] == b'.', "{ts}");
        }
    }
}
