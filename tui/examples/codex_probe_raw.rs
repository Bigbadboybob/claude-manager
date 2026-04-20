//! Raw PTY probe — bypasses alacritty's event loop so we can READ the bytes
//! codex writes and SEE what's happening. Uses libc directly to open a pty
//! and spawn codex. If this works where the alacritty-based probe fails,
//! the issue is alacritty's PTY wrapper. If this ALSO fails with /clear,
//! the issue is more fundamental.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

fn list_codex_sessions_at(sessions_dir: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    fn walk(dir: &Path, out: &mut HashSet<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    out.insert(p);
                }
            }
        }
    }
    walk(sessions_dir, &mut out);
    out
}

/// Open a pty master+slave, set slave to be the controlling tty for child.
fn openpty() -> std::io::Result<(OwnedFd, OwnedFd)> {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::grantpt(master) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::unlockpt(master) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let slave_name_ptr = libc::ptsname(master);
        if slave_name_ptr.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let slave_name = std::ffi::CStr::from_ptr(slave_name_ptr)
            .to_string_lossy()
            .into_owned();
        let slave = libc::open(
            CString::new(slave_name).unwrap().as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY,
        );
        if slave < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Set window size on the slave
        let ws = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(slave, libc::TIOCSWINSZ, &ws);
        Ok((OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)))
    }
}

fn probe() -> Result<(), Box<dyn std::error::Error>> {
    let send_clear = std::env::var_os("PROBE_SEND_CLEAR").is_some();
    let prompt = "please respond PING123";
    // Rollouts will be written under the probe's isolated codex home.
    let before: HashSet<PathBuf> = HashSet::new();

    let work = tempfile::tempdir()?;
    let (master, slave) = openpty()?;
    let master_fd = master.as_raw_fd();
    let slave_fd = slave.as_raw_fd();

    // Spawn codex with the slave as its stdin/stdout/stderr
    let slave_for_child = unsafe { Stdio::from_raw_fd(libc::dup(slave_fd)) };
    let slave_for_child2 = unsafe { Stdio::from_raw_fd(libc::dup(slave_fd)) };
    let slave_for_child3 = unsafe { Stdio::from_raw_fd(libc::dup(slave_fd)) };

    // Use an isolated CODEX_HOME so postgres-local (or any other user MCP
    // server) doesn't block codex's REPL at startup.
    let codex_home = work.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    if let Some(home) = std::env::var_os("HOME") {
        let src = PathBuf::from(home).join(".codex/auth.json");
        let dst = codex_home.join("auth.json");
        if src.exists() {
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink(&src, &dst);
            if !dst.exists() {
                let _ = fs::copy(&src, &dst);
            }
        }
    }
    fs::write(
        codex_home.join("config.toml"),
        "model = \"gpt-5.4\"\nmodel_reasoning_effort = \"xhigh\"\n",
    )?;

    let mut cmd = Command::new("codex");
    cmd.arg("--dangerously-bypass-approvals-and-sandbox");
    cmd.env("CODEX_HOME", &codex_home);
    cmd.current_dir(work.path());
    cmd.stdin(slave_for_child);
    cmd.stdout(slave_for_child2);
    cmd.stderr(slave_for_child3);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child: Child = cmd.spawn()?;
    drop(slave); // child has its own copies

    // Reader thread collects all bytes from master into a shared buffer
    let master_reader = unsafe { std::fs::File::from_raw_fd(libc::dup(master_fd)) };
    let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>();
    let reader_handle = thread::spawn(move || {
        let mut file = master_reader;
        let mut buf = [0u8; 4096];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if bytes_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Collected bytes (printable view for debugging)
    let mut collected: Vec<u8> = Vec::new();
    let mut flush_into = |collected: &mut Vec<u8>, rx: &mpsc::Receiver<Vec<u8>>| {
        while let Ok(chunk) = rx.try_recv() {
            collected.extend_from_slice(&chunk);
        }
    };

    let mut master_writer = unsafe { std::fs::File::from_raw_fd(libc::dup(master_fd)) };

    // Sleep helper that also drains bytes
    let sleep_drain =
        |dur: Duration, collected: &mut Vec<u8>, rx: &mpsc::Receiver<Vec<u8>>| {
            let deadline = Instant::now() + dur;
            while Instant::now() < deadline {
                flush_into(collected, rx);
                thread::sleep(Duration::from_millis(50));
            }
        };

    // Wait for trust prompt to render
    sleep_drain(Duration::from_secs(5), &mut collected, &bytes_rx);

    // Dismiss trust prompt
    master_writer.write_all(b"1\r")?;
    master_writer.flush()?;
    sleep_drain(Duration::from_secs(10), &mut collected, &bytes_rx);
    println!("--- after trust dismiss ({} bytes collected) ---", collected.len());

    if send_clear {
        master_writer.write_all(b"/clear\r")?;
        master_writer.flush()?;
        sleep_drain(Duration::from_secs(5), &mut collected, &bytes_rx);
        println!("--- after /clear ({} bytes collected) ---", collected.len());
    }

    master_writer.write_all(prompt.as_bytes())?;
    master_writer.flush()?;
    sleep_drain(Duration::from_secs(1), &mut collected, &bytes_rx);
    master_writer.write_all(b"\r")?;
    master_writer.flush()?;
    sleep_drain(Duration::from_secs(35), &mut collected, &bytes_rx);
    println!("--- after prompt ({} bytes collected) ---", collected.len());

    // Show last bit of collected output (escaped)
    let tail_start = collected.len().saturating_sub(1500);
    let tail = &collected[tail_start..];
    println!(
        "tail of codex output:\n{}",
        String::from_utf8_lossy(tail)
            .replace('\x1b', "\\x1b")
            .chars()
            .take(2000)
            .collect::<String>()
    );

    let _ = child.kill();
    let _ = child.wait();
    drop(master_writer);
    let _ = reader_handle.join();

    // Check rollouts in the isolated codex home
    let after = list_codex_sessions_at(&codex_home.join("sessions"));
    let new_: Vec<_> = after.difference(&before).collect();
    let matching: Vec<_> = new_
        .iter()
        .filter(|p| {
            fs::read_to_string(p)
                .map(|c| c.contains(prompt))
                .unwrap_or(false)
        })
        .collect();
    println!("new_rollouts={} matching={}", new_.len(), matching.len());
    for m in &matching {
        println!("  matched: {}", m.display());
    }
    if matching.is_empty() {
        Err("no rollout".into())
    } else {
        Ok(())
    }
}

fn main() {
    match probe() {
        Ok(()) => println!("OK"),
        Err(e) => {
            println!("FAIL: {}", e);
            std::process::exit(1);
        }
    }
}
