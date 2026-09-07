//! Receipt evidence comes only from inbound records in the current transcript.
//! A complete, parseable scan of the original binding is required to retry.
use super::*;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Binding {
    pub path: String,
    pub generation: u64,
    dev: u64,
    ino: u64,
}
#[derive(Default, Serialize, Deserialize)]
pub(super) struct Scan {
    binding: Option<Binding>,
    offset: u64,
    invalid: bool,
    #[serde(default)]
    version: Option<(u64, i64, i64, i64, i64)>,
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Evidence {
    Found,
    Absent,
    Unknown,
}

pub(super) fn binding(path: Option<&str>, generation: u64) -> Option<Binding> {
    let path = fs::canonicalize(path?).ok()?;
    let meta = fs::metadata(&path).ok()?;
    meta.is_file().then(|| Binding {
        path: path.to_string_lossy().into(),
        generation,
        dev: meta.dev(),
        ino: meta.ino(),
    })
}
fn content_has(content: &Value, marker: &str) -> bool {
    content.as_str().is_some_and(|s| s.contains(marker))
        || content.as_array().is_some_and(|parts| {
            parts.iter().any(|v| {
                matches!(v["type"].as_str(), Some("text" | "input_text"))
                    && v["text"].as_str().is_some_and(|s| s.contains(marker))
            })
        })
}
fn inbound_has(engine: &str, v: &Value, marker: &str) -> bool {
    match engine {
        "claude-code" => {
            v["type"] == "user"
                && v["message"]["role"] == "user"
                && content_has(&v["message"]["content"], marker)
        }
        "codex" => {
            (v["type"] == "response_item"
                && v["payload"]["type"] == "message"
                && v["payload"]["role"] == "user"
                && content_has(&v["payload"]["content"], marker))
                || (v["type"] == "event_msg"
                    && v["payload"]["type"] == "user_message"
                    && content_has(&v["payload"]["message"], marker))
        }
        _ => false,
    }
}
pub(super) fn inspect(
    scan: &mut Scan,
    original: Option<&Binding>,
    current: Option<&Binding>,
    engine: &str,
    wake: &str,
) -> Evidence {
    let Some(current) = current else {
        return Evidence::Unknown;
    };
    if scan.binding.as_ref() != Some(current) {
        *scan = Scan {
            binding: Some(current.clone()),
            ..Scan::default()
        };
    }
    let Ok(mut file) = fs::File::open(&current.path) else {
        return Evidence::Unknown;
    };
    let Ok(meta) = file.metadata() else {
        return Evidence::Unknown;
    };
    let version = (
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    );
    if scan.version != Some(version) {
        scan.offset = 0;
        scan.invalid = false;
        scan.version = Some(version);
    }
    if meta.ino() != current.ino || meta.dev() != current.dev || meta.len() < scan.offset {
        scan.invalid = true;
        scan.offset = 0;
        return Evidence::Unknown;
    }
    if file.seek(SeekFrom::Start(scan.offset)).is_err() {
        return Evidence::Unknown;
    }
    // Incremental bounded I/O, with no tail-only negative evidence. Oversized
    // or unfinished records remain unknown instead of authorizing a retry.
    let mut bytes = Vec::new();
    if file.take(2 * 1024 * 1024).read_to_end(&mut bytes).is_err() {
        return Evidence::Unknown;
    }
    let end = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let marker = format!("[cm-chat {wake}]");
    let mut found = false;
    for line in bytes[..end]
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
    {
        match serde_json::from_slice::<Value>(line) {
            Ok(v) if inbound_has(engine, &v, &marker) => found = true,
            Ok(_) => {}
            Err(_) => scan.invalid = true,
        }
    }
    let Ok(after) = fs::metadata(&current.path) else {
        return Evidence::Unknown;
    };
    if after.ino() != meta.ino()
        || after.dev() != meta.dev()
        || (
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
        ) != version
    {
        return Evidence::Unknown;
    }
    if found {
        return Evidence::Found;
    }
    scan.offset += end as u64;
    if scan.offset == meta.len() && !scan.invalid && original == Some(current) {
        Evidence::Absent
    } else {
        Evidence::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn messaging_marker_uses_live_inbound_records_and_never_tail_absence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("live.jsonl");
        fs::write(&path,"{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"[cm-chat test]\"}}\n").unwrap();
        let original = binding(path.to_str(), 1).unwrap();
        let mut scan = Scan::default();
        assert_eq!(
            inspect(
                &mut scan,
                Some(&original),
                Some(&original),
                "claude-code",
                "test"
            ),
            Evidence::Absent
        );
        use std::io::Write;
        writeln!(fs::OpenOptions::new().append(true).open(&path).unwrap(),"{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"[cm-chat test]\"}}}}").unwrap();
        assert_eq!(
            inspect(
                &mut scan,
                Some(&original),
                Some(&original),
                "claude-code",
                "test"
            ),
            Evidence::Found
        );
        let other = tmp.path().join("rotated.jsonl");
        fs::write(&other, "{}\n").unwrap();
        let current = binding(other.to_str(), 2).unwrap();
        assert_eq!(
            inspect(
                &mut scan,
                Some(&original),
                Some(&current),
                "claude-code",
                "test"
            ),
            Evidence::Unknown
        );
        fs::write(&other,"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"[cm-chat test]\"}]}}\n").unwrap();
        assert_eq!(
            inspect(
                &mut Scan::default(),
                Some(&original),
                Some(&current),
                "codex",
                "test"
            ),
            Evidence::Found
        );
        fs::write(&path, "not json\n").unwrap();
        assert_eq!(
            inspect(
                &mut Scan::default(),
                Some(&original),
                Some(&original),
                "claude-code",
                "test"
            ),
            Evidence::Unknown
        );
    }
}
