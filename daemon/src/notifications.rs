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
        let event = publish(root, "uid", "event", "chat", "wake", "marker").unwrap();
        assert_eq!(
            publish(root, "uid", "event", "chat", "wake", "marker").unwrap(),
            event
        );
        assert!(publish(root, "uid", "event", "chat", "different", "marker").is_err());
        assert!(get(root, "other", "event").unwrap().is_none());
        assert_eq!(
            update_pending(root, "uid", "event", None).unwrap().unwrap()["status"],
            "cancelled"
        );
        let mut claimed = publish(root, "uid", "second", "chat", "wake", "marker").unwrap();
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
    fn notifications_python_and_rust_share_queue_and_receipts() {
        let _env = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        publish(tmp.path(), "uid", "from-rust", "chat", "wake", "marker").unwrap();
        let output = std::process::Command::new("python3")
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap())
            .args(["-c", "from pathlib import Path; import sys; from mcp_server.notifications import Queue; q=Queue('uid',Path(sys.argv[1])); assert q.get('from-rust')['status']=='pending'; q.cancel('from-rust'); q.publish('from-python','session_monitor','worker done','marker')"])
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
