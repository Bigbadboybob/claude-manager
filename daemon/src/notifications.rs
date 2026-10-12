//! Durable native agent notifications. Shared wire format/locks with
//! mcp_server/notifications.py; neither producer performs terminal I/O.
use crate::messaging::atomic_replace;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    os::fd::AsRawFd,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

fn digest(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
pub fn directory(root: &Path, uid: &str) -> PathBuf {
    root.join("notifications").join(digest(uid))
}
fn path(root: &Path, uid: &str, id: &str) -> PathBuf {
    directory(root, uid).join(format!("{}.json", digest(id)))
}
fn lock(root: &Path, uid: &str) -> io::Result<fs::File> {
    let dir = directory(root, uid);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("queue.lock"))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(f)
}
fn read(path: &Path) -> io::Result<Option<Value>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
pub fn get(root: &Path, uid: &str, id: &str) -> io::Result<Option<Value>> {
    let _guard = lock(root, uid)?;
    read(&path(root, uid, id))
}
pub fn publish(
    root: &Path,
    uid: &str,
    id: &str,
    source: &str,
    text: &str,
    marker: &str,
) -> io::Result<Value> {
    if text.is_empty() || text.len() > 65536 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "notification must contain 1–65536 UTF-8 bytes",
        ));
    }
    // The consumer confirms delivery by finding the marker in the agent's
    // transcript; a text without it can never be observed and would sit
    // `submitted` until the stall alarm pages Owner.
    if marker.is_empty() || !text.contains(marker) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "notification text must contain its marker",
        ));
    }
    let _guard = lock(root, uid)?;
    let path = path(root, uid, id);
    if let Some(old) = read(&path)? {
        if old["text"] != text || old["source"] != source || old["marker"] != marker {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "notification ID content conflict",
            ));
        }
        return Ok(old);
    }
    let mut events = Vec::new();
    for entry in fs::read_dir(directory(root, uid))? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|x| x == "json")
            && entry.file_name() != "transport.json"
        {
            if let Some(value) = read(&entry.path())? {
                events.push((entry.path(), value));
            }
        }
    }
    if events.len() >= 512 {
        events.sort_by(|a, b| {
            a.1["created_at"]
                .as_f64()
                .partial_cmp(&b.1["created_at"].as_f64())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some((path, _)) = events
            .iter()
            .find(|(_, v)| matches!(v["status"].as_str(), Some("observed" | "cancelled")))
        {
            fs::remove_file(path)?;
        } else {
            return Err(io::Error::other(
                "notification queue full; inspect notification_status",
            ));
        }
    }
    let event = json!({"version":1,"id":id,"recipient":uid,"source":source,"text":text,"marker":marker,
        "status":"pending","created_at":chrono::Utc::now().timestamp_millis() as f64 / 1000.0});
    if let Err(e) = atomic_replace(&path, &event) {
        // Still under the queue lock: failed publication cannot be consumed.
        let _ = fs::remove_file(&path);
        return Err(e);
    }
    Ok(event)
}
/// All retained events plus the consumer's `transport.json` heartbeat, read
/// under the queue lock. A missing queue directory is an empty queue.
pub fn snapshot(root: &Path, uid: &str) -> io::Result<(Vec<Value>, Option<Value>)> {
    let dir = directory(root, uid);
    if !dir.is_dir() {
        return Ok((Vec::new(), None));
    }
    let _guard = lock(root, uid)?;
    let mut events = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|x| x == "json")
            && entry.file_name() != "transport.json"
        {
            if let Ok(Some(value)) = read(&entry.path()) {
                events.push(value);
            }
        }
    }
    let transport = read(&dir.join("transport.json")).ok().flatten();
    Ok((events, transport))
}
/// Retire this recipient's submitted/uncertain events whose text lacks their
/// marker (published before `publish` enforced it): they were delivered but
/// can never be observed, so they would keep a stall-alarm episode open
/// forever. Pending ones are left alone (delivery does not need the marker,
/// so retiring them would drop a wake); once submitted they are retired on a
/// later pass. Marks them cancelled with `retired: "marker_missing"`; returns
/// how many.
pub fn retire_unconfirmable(root: &Path, uid: &str) -> io::Result<usize> {
    let dir = directory(root, uid);
    if !dir.is_dir() {
        return Ok(0);
    }
    let _guard = lock(root, uid)?;
    let mut retired = 0;
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|x| x != "json") || path.file_name().is_some_and(|n| n == "transport.json") {
            continue;
        }
        let Ok(Some(mut event)) = read(&path) else { continue };
        let open = matches!(event["status"].as_str(), Some("submitted" | "uncertain"));
        let marker = event["marker"].as_str().unwrap_or("");
        let confirmable = !marker.is_empty() && event["text"].as_str().is_some_and(|t| t.contains(marker));
        if open && !confirmable {
            event["status"] = json!("cancelled");
            event["retired"] = json!("marker_missing");
            atomic_replace(&path, &event)?;
            retired += 1;
        }
    }
    Ok(retired)
}

/// Only unclaimed events can be rewritten or retracted. A submitted native
/// frame cannot be recalled; leave the receipt state intact.
pub fn update_pending(
    root: &Path,
    uid: &str,
    id: &str,
    text: Option<&str>,
) -> io::Result<Option<Value>> {
    let _guard = lock(root, uid)?;
    let path = path(root, uid, id);
    let Some(mut event) = read(&path)? else {
        return Ok(None);
    };
    if event["status"] == "pending" {
        if let Some(text) = text {
            if event["text"] == text {
                return Ok(Some(event));
            }
            event["text"] = json!(text);
        } else {
            event["status"] = json!("cancelled");
        }
        atomic_replace(&path, &event)?;
    }
    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notifications_idempotency_cancellation_and_no_retraction_after_claim() {
        let _env = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let event = publish(root, "uid", "event", "chat", "wake marker", "marker").unwrap();
        assert_eq!(
            publish(root, "uid", "event", "chat", "wake marker", "marker").unwrap(),
            event
        );
        assert!(publish(root, "uid", "event", "chat", "different marker", "marker").is_err());
        assert!(get(root, "other", "event").unwrap().is_none());
        assert_eq!(
            update_pending(root, "uid", "event", None).unwrap().unwrap()["status"],
            "cancelled"
        );
        let mut claimed = publish(root, "uid", "second", "chat", "wake marker", "marker").unwrap();
        claimed["status"] = json!("submitting");
        atomic_replace(&path(root, "uid", "second"), &claimed).unwrap();
        assert_eq!(
            update_pending(root, "uid", "second", None)
                .unwrap()
                .unwrap()["status"],
            "submitting"
        );
    }
    #[test]
    fn notifications_require_the_marker_in_the_text_and_retire_old_unconfirmable_events() {
        let _env = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let err = publish(root, "uid", "x", "owner", "no marker here", "[cm-x 1]").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(publish(root, "uid", "x", "owner", "text", "").is_err(), "an empty marker is refused");
        publish(root, "uid", "ok", "owner", "[cm-x 2] fine", "[cm-x 2]").unwrap();
        // An event written before the check (marker missing), stuck submitted.
        let mut stuck = get(root, "uid", "ok").unwrap().unwrap();
        stuck["id"] = json!("old");
        stuck["text"] = json!("Owner availability: away→focused");
        stuck["marker"] = json!("[cm-owner-availability r1]");
        stuck["status"] = json!("submitted");
        atomic_replace(&path(root, "uid", "old"), &stuck).unwrap();
        assert_eq!(retire_unconfirmable(root, "uid").unwrap(), 1);
        let old = get(root, "uid", "old").unwrap().unwrap();
        assert_eq!((old["status"].as_str(), old["retired"].as_str()), (Some("cancelled"), Some("marker_missing")));
        assert_eq!(get(root, "uid", "ok").unwrap().unwrap()["status"], "pending", "confirmable events untouched");
        assert_eq!(retire_unconfirmable(root, "uid").unwrap(), 0, "idempotent");
        // A pending marker-less event can still be delivered: left alone
        // until it is submitted.
        let mut pending = old.clone();
        pending["id"] = json!("old-pending");
        pending["status"] = json!("pending");
        atomic_replace(&path(root, "uid", "old-pending"), &pending).unwrap();
        assert_eq!(retire_unconfirmable(root, "uid").unwrap(), 0);
        assert_eq!(get(root, "uid", "old-pending").unwrap().unwrap()["status"], "pending");
        pending["status"] = json!("submitted");
        atomic_replace(&path(root, "uid", "old-pending"), &pending).unwrap();
        assert_eq!(retire_unconfirmable(root, "uid").unwrap(), 1);
        assert_eq!(retire_unconfirmable(root, "nobody").unwrap(), 0);
    }
    #[test]
    fn notifications_python_and_rust_share_queue_and_receipts() {
        let _env = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        publish(tmp.path(), "uid", "from-rust", "chat", "wake marker", "marker").unwrap();
        let output = std::process::Command::new("python3")
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap())
            .args(["-c", "from pathlib import Path; import sys; from mcp_server.notifications import Queue; q=Queue('uid',Path(sys.argv[1])); assert q.get('from-rust')['status']=='pending'; q.cancel('from-rust'); q.publish('from-python','session_monitor','worker done marker','marker')"])
            .arg(tmp.path()).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            get(tmp.path(), "uid", "from-rust").unwrap().unwrap()["status"],
            "cancelled"
        );
        assert_eq!(
            get(tmp.path(), "uid", "from-python").unwrap().unwrap()["source"],
            "session_monitor"
        );
    }
}
