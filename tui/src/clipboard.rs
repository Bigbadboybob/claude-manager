//! Read the viewing machine's clipboard without blocking terminal rendering.

use std::borrow::Cow;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use alacritty_terminal::event_loop::{EventLoopSender, Msg};
use alacritty_terminal::term::ClipboardType;

type Formatter = Arc<dyn Fn(&str) -> String + Sync + Send>;
struct Request {
    selection: ClipboardType,
    formatter: Formatter,
    // Replies belong to this attachment, even if the session is restarted while
    // the desktop clipboard provider is answering.
    target: EventLoopSender,
}

pub fn request(selection: ClipboardType, formatter: Formatter, target: EventLoopSender) {
    static WORKER: OnceLock<Option<mpsc::SyncSender<Request>>> = OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Request>(16);
        std::thread::Builder::new()
            .name("cm-clipboard".into())
            .spawn(move || {
                for request in rx {
                    let text = read_clipboard(request.selection).unwrap_or_default();
                    let response = (request.formatter)(&text);
                    let _ = request
                        .target
                        .send(Msg::Input(Cow::Owned(response.into_bytes())));
                }
            })
            .ok()
            .map(|_| tx)
    });
    if let Some(worker) = worker {
        let _ = worker.try_send(Request {
            selection,
            formatter,
            target,
        });
    }
}

fn readers(selection: ClipboardType, wayland: bool) -> Vec<Vec<&'static str>> {
    let primary = matches!(selection, ClipboardType::Selection);
    let mut result = Vec::new();
    if wayland {
        let mut args = vec!["wl-paste", "--no-newline"];
        if primary {
            args.push("--primary");
        }
        result.push(args);
    }
    result.push(vec![
        "xclip",
        "-selection",
        if primary { "primary" } else { "clipboard" },
        "-o",
    ]);
    result.push(vec![
        "xsel",
        if primary { "--primary" } else { "--clipboard" },
        "--output",
    ]);
    #[cfg(target_os = "macos")]
    result.push(vec!["pbpaste"]);
    result
}

fn read_clipboard(selection: ClipboardType) -> Option<String> {
    for args in readers(selection, std::env::var_os("WAYLAND_DISPLAY").is_some()) {
        if let Ok(text) = read_command(&args, Duration::from_millis(700)) {
            return Some(text);
        }
    }
    None
}

fn read_command(args: &[&str], budget: Duration) -> io::Result<String> {
    const MAX_BYTES: usize = 1024 * 1024;
    let mut child = Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        let mut stdout = child.stdout.take().expect("piped stdout");
        let fd = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let deadline = Instant::now() + budget;
        let mut bytes = Vec::new();
        let mut buf = [0; 8192];
        let mut eof = false;
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "clipboard provider timed out",
                ));
            }
            if !eof {
                match stdout.read(&mut buf) {
                    Ok(0) => eof = true,
                    Ok(n) => {
                        if bytes.len() + n > MAX_BYTES {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "clipboard exceeds 1 MiB",
                            ));
                        }
                        bytes.extend_from_slice(&buf[..n]);
                        continue;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            if eof {
                if let Some(status) = child.try_wait()? {
                    if !status.success() {
                        return Err(io::Error::other("clipboard provider failed"));
                    }
                    return String::from_utf8(bytes)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    // A stuck provider must not accumulate children or monopolize this worker.
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_accepts_neovim_clipboard_queries() {
        use alacritty_terminal::{event::Event, vte::ansi::Handler, Term};
        let (tx, rx) = mpsc::channel();
        let size = crate::session::TermSize { columns: 80, screen_lines: 24 };
        let mut term = Term::new(crate::session::terminal_config(), &size, crate::session::EventProxy::new(tx));
        term.clipboard_load(b'c', "\x07");
        match rx.try_recv().expect("terminal must forward OSC 52 reads") {
            Event::ClipboardLoad(ClipboardType::Clipboard, formatter) => {
                assert_eq!(formatter("hello"), "\x1b]52;c;aGVsbG8=\x07");
            }
            event => panic!("unexpected clipboard event: {event:?}"),
        }
    }

    #[test]
    fn wayland_clipboard_and_primary_use_the_matching_selection() {
        assert_eq!(
            readers(ClipboardType::Clipboard, true)[0],
            ["wl-paste", "--no-newline"]
        );
        let primary = readers(ClipboardType::Selection, true);
        assert_eq!(primary[0], ["wl-paste", "--no-newline", "--primary"]);
        assert_eq!(primary[1], ["xclip", "-selection", "primary", "-o"]);
        assert_eq!(readers(ClipboardType::Clipboard, false)[0][0], "xclip");
    }

    #[test]
    fn provider_preserves_unicode_and_trailing_newlines() {
        assert_eq!(
            read_command(
                &["sh", "-c", "printf 'héllo\\n\\n'"],
                Duration::from_secs(2)
            )
            .unwrap(),
            "héllo\n\n"
        );
    }

    #[test]
    fn provider_timeout_and_failed_exit_do_not_return_partial_text() {
        let start = Instant::now();
        let error = read_command(&["sleep", "20"], Duration::from_millis(40)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(read_command(
            &["sh", "-c", "printf partial; exit 1"],
            Duration::from_secs(2)
        )
        .is_err());
    }

    #[test]
    fn runaway_provider_output_is_bounded() {
        let error = read_command(
            &["head", "-c", "1048577", "/dev/zero"],
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
