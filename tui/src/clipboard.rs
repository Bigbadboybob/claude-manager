//! Read the viewing machine's clipboard without blocking terminal rendering.

use std::borrow::Cow;
use std::io::{self, Read, Write};
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
    let mut command = Command::new(args[0]);
    command.args(&args[1..]);
    String::from_utf8(run_command(command, &[], budget, 1024 * 1024)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Bound reads AND writes: a disconnected SSH child must not leave clipboard
/// input blocked in a pipe indefinitely. Only the background worker calls this.
fn run_command(
    mut command: Command,
    input: &[u8],
    budget: Duration,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stdin = child.stdin.take();
        let nonblocking = |fd| -> io::Result<()> {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        };
        nonblocking(stdout.as_raw_fd())?;
        nonblocking(stdin.as_ref().unwrap().as_raw_fd())?;
        let deadline = Instant::now() + budget;
        let mut bytes = Vec::new();
        let mut buf = [0; 8192];
        let mut eof = false;
        let mut written = 0;
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "clipboard operation timed out",
                ));
            }
            if written == input.len() {
                stdin.take();
            }
            if let Some(pipe) = stdin.as_mut() {
                match pipe.write(&input[written..]) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(n) => written += n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => return Err(e),
                }
            }
            if !eof {
                match stdout.read(&mut buf) {
                    Ok(0) => eof = true,
                    Ok(n) => {
                        if bytes.len() + n > max_bytes {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "clipboard exceeds size limit",
                            ));
                        }
                        bytes.extend_from_slice(&buf[..n]);
                        continue;
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => return Err(e),
                }
            }
            if eof {
                if let Some(status) = child.try_wait()? {
                    if !status.success() {
                        return Err(io::Error::other("clipboard command failed"));
                    }
                    return Ok(bytes);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

pub(crate) struct PasteResult {
    pub text: String,
    pub image: bool,
}

const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Called only on an explicit paste shortcut, on the viewing machine.
pub(crate) fn prepare_paste(
    transport: &crate::hosts::HostTransport,
) -> io::Result<Option<PasteResult>> {
    let mut providers: Vec<Vec<&str>> = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        providers.push(vec!["wl-paste", "--no-newline", "--type", "image/png"]);
    }
    providers.push(vec![
        "xclip",
        "-selection",
        "clipboard",
        "-target",
        "image/png",
        "-o",
    ]);
    for args in providers {
        let mut command = Command::new(args[0]);
        command.args(&args[1..]);
        match run_command(command, &[], Duration::from_millis(1500), MAX_IMAGE_BYTES) {
            Ok(bytes) if bytes.starts_with(PNG_MAGIC) => {
                return save_image(transport, &bytes).map(|path| {
                    Some(PasteResult {
                        text: path,
                        image: true,
                    })
                });
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidData => return Err(e),
            _ => {}
        }
    }
    Ok(read_clipboard(ClipboardType::Clipboard)
        .filter(|s| !s.is_empty())
        .map(|text| PasteResult { text, image: false }))
}

// All executable text is constant. Clipboard bytes travel only over stdin;
// neither their contents nor a clipboard filename can become shell syntax.
const SAVE_IMAGE_SCRIPT: &str = r#"
import os, sys, tempfile
expected = int(sys.stdin.buffer.readline())
assert 0 < expected <= 32 * 1024 * 1024
image = sys.stdin.buffer.read(expected + 1)
assert len(image) == expected and image.startswith(b'\x89PNG\r\n\x1a\n')
os.umask(0o077)
directory = os.path.expanduser('~/.cm/attachments')
os.makedirs(directory, mode=0o700, exist_ok=True)
fd, path = tempfile.mkstemp(prefix='paste-', suffix='.png', dir=directory)
with os.fdopen(fd, 'wb') as out:
    out.write(image)
print(path)
"#;

fn save_image(transport: &crate::hosts::HostTransport, bytes: &[u8]) -> io::Result<String> {
    use crate::hosts::HostTransport;
    let command = match transport {
        HostTransport::Unix { .. } => {
            let mut c = Command::new("python3");
            c.args(["-c", SAVE_IMAGE_SCRIPT]);
            c
        }
        HostTransport::SshUnix {
            ssh_host, ssh_user, ..
        } => {
            let mut c = Command::new("ssh");
            c.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8"]);
            if let Some(user) = ssh_user {
                c.arg("-l").arg(user);
            }
            c.arg("--").arg(ssh_host);
            c.arg(format!(
                "python3 -c '{}'",
                SAVE_IMAGE_SCRIPT.replace('\'', "'\"'\"'")
            ));
            c
        }
        HostTransport::TcpTls { .. } => {
            return Err(io::Error::other(
                "Image paste currently requires an SSH or local host",
            ))
        }
    };
    // Prefixing the exact length prevents a broken upload from publishing a
    // truncated image. Files persist across reconnect/resume, like transcripts.
    let mut input = format!("{}\n", bytes.len()).into_bytes();
    input.extend_from_slice(bytes);
    let output = run_command(command, &input, Duration::from_secs(30), 16 * 1024)?;
    let path =
        String::from_utf8(output).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = path.trim();
    if !path.starts_with('/') || path.chars().any(char::is_control) {
        return Err(io::Error::other("Image upload returned an invalid path"));
    }
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_accepts_neovim_clipboard_queries() {
        use alacritty_terminal::{event::Event, vte::ansi::Handler, Term};
        let (tx, rx) = mpsc::channel();
        let size = crate::session::TermSize {
            columns: 80,
            screen_lines: 24,
        };
        let mut term = Term::new(
            crate::session::terminal_config(),
            &size,
            crate::session::EventProxy::new(tx),
        );
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

    #[test]
    fn stalled_upload_times_out_instead_of_blocking_on_stdin() {
        let mut command = Command::new("sleep");
        command.arg("20");
        let started = Instant::now();
        let err = run_command(
            command,
            &vec![0; 1024 * 1024],
            Duration::from_millis(40),
            1024,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn image_upload_is_exact_private_unique_and_rejects_truncation() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        // Binary bytes and shell metacharacters are payload, never executable text.
        let bytes = [PNG_MAGIC, b"\0\xff\n'$(touch unwanted)`touch unwanted`"].concat();
        let mut input = format!("{}\n", bytes.len()).into_bytes();
        input.extend_from_slice(&bytes);
        let upload = |input: &[u8]| {
            let mut command = Command::new("python3");
            command
                .args(["-c", SAVE_IMAGE_SCRIPT])
                .env("HOME", home.path());
            run_command(command, input, Duration::from_secs(3), 16384)
        };
        let path = String::from_utf8(upload(&input).unwrap()).unwrap();
        let path = std::path::Path::new(path.trim());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let second = String::from_utf8(upload(&input).unwrap()).unwrap();
        assert_ne!(path, std::path::Path::new(second.trim()));
        assert!(upload(&input[..input.len() - 1]).is_err());
        assert_eq!(
            std::fs::read_dir(home.path().join(".cm/attachments"))
                .unwrap()
                .count(),
            2
        );
    }
}
