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
    lock_with(root, uid, false)
}
fn lock_with(root: &Path, uid: &str, nonblocking: bool) -> io::Result<fs::File> {
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
    let flags = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    if unsafe { libc::flock(f.as_raw_fd(), flags) } != 0 {
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

fn valid_census_event(value: &Value) -> bool {
    let id_ok = value["id"].as_str().is_some_and(|id| !id.trim().is_empty());
    let status_ok = matches!(value["status"].as_str(), Some(
        "pending" | "submitting" | "submitted" | "uncertain" | "observed" | "cancelled"));
    // If updated_at exists it is authoritative: an invalid value cannot
    // safely fall back to an older timestamp.
    let at = value.get("updated_at").unwrap_or(&value["created_at"]);
    let time_ok = at.as_f64().is_some_and(|t| t.is_finite() && t > 0.0);
    let text_ok = value["text"].as_str().is_some();
    let marker_ok = value.get("marker").is_none_or(|marker| marker.as_str().is_some());
    id_ok && status_ok && time_ok && text_ok && marker_ok
}
/// #24: hold `uid`'s new wakes (they stay pending) until `until` (unix
/// seconds): CM is about to restart it for a rotation or handoff, so they
/// go to the new process. Rotation/handoff notices are exempt (the
/// consumer's `Queue.HOLD_EXEMPT`). `hold` is not `*.json`, so it is never
/// read as an event.
pub fn set_hold(root: &Path, uid: &str, until: f64) -> io::Result<()> {
    let dir = directory(root, uid);
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(".hold.tmp");
    fs::write(&tmp, json!({ "until": until }).to_string())?;
    fs::rename(tmp, dir.join("hold"))
}

/// Lift [`set_hold`]; the consumer then delivers what waited.
pub fn clear_hold(root: &Path, uid: &str) {
    let _ = fs::remove_file(directory(root, uid).join("hold"));
}

/// Serialize a recovery command against native event claims. An event already
/// being submitted (or ambiguously submitted after this failure) makes the
/// recovery unsafe. Keep this guard only for the short command delivery, never
/// while awaiting the relay's fork binding (its health reports use this lock).
pub(crate) fn recovery_input_guard(root: &Path, uid: &str, failed_at: f64) -> io::Result<Option<fs::File>> {
    let guard = lock(root, uid)?;
    for entry in fs::read_dir(directory(root, uid))? {
        let path = entry?.path();
        if path.extension().is_none_or(|x| x != "json") || path.file_name().is_some_and(|n| n == "transport.json") { continue; }
        let Some(event) = read(&path)? else { continue; };
        let status = event["status"].as_str();
        let at = event["updated_at"].as_f64().or_else(|| event["created_at"].as_f64()).unwrap_or(f64::INFINITY);
        if status == Some("submitting")
            || (matches!(status, Some("submitted" | "uncertain")) && at >= failed_at)
        { return Ok(None); }
    }
    Ok(Some(guard))
}

/// Remove every hold under `root` (brain start: no hold outlives the brain
/// that set it). Returns how many.
pub fn clear_all_holds(root: &Path) -> usize {
    let Ok(dirs) = fs::read_dir(root.join("notifications")) else { return 0 };
    dirs.flatten().filter(|d| fs::remove_file(d.path().join("hold")).is_ok()).count()
}

/// Whether `uid`'s wakes are held right now.
pub fn held(root: &Path, uid: &str, now: f64) -> bool {
    fs::read_to_string(directory(root, uid).join("hold"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["until"].as_f64())
        .is_some_and(|until| until > now)
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
    if !directory(root, uid).is_dir() {
        return Ok((Vec::new(), None));
    }
    snapshot_impl(root, uid, false)
}

/// A complete census for decisions that would otherwise treat a partial
/// queue as empty. Diagnostic callers may continue using `snapshot`.
pub fn snapshot_strict(root: &Path, uid: &str) -> io::Result<(Vec<Value>, Option<Value>)> {
    snapshot_impl(root, uid, true)
}

fn snapshot_impl(root: &Path, uid: &str, strict: bool) -> io::Result<(Vec<Value>, Option<Value>)> {
    let dir = directory(root, uid);
    match fs::metadata(&dir) {
        Ok(meta) if meta.is_dir() => {},
        Ok(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "notification queue is not a directory")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), None)),
        Err(e) => return Err(e),
    }
    let _guard = lock(root, uid)?;
    snapshot_locked(&dir, strict)
}

fn snapshot_locked(dir: &Path, strict: bool) -> io::Result<(Vec<Value>, Option<Value>)> {
    let mut events = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|x| x == "json")
            && entry.file_name() != "transport.json"
        {
            match read(&entry.path()) {
                Ok(Some(value)) => {
                    if strict && !valid_census_event(&value) {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid notification in census"));
                    }
                    events.push(value);
                }
                Ok(None) if strict => return Err(io::Error::new(io::ErrorKind::NotFound, "notification disappeared during census")),
                Err(e) if strict => return Err(e),
                _ => {},
            }
        }
    }
    let transport = if strict {
        let transport = read(&dir.join("transport.json"))?;
        if let Some(value) = &transport {
            let status_ok = value.is_object() && value["status"].as_str().is_some_and(|s| !s.is_empty());
            let reason_ok = value.get("reason").is_none_or(|v| v.is_null() || v.is_string());
            let reasons_ok = value.get("reasons").is_none_or(|v| v.is_null() || v.as_array().is_some_and(|a| a.iter().all(Value::is_string)));
            if !status_ok || !reason_ok || !reasons_ok {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid notification transport"));
            }
        }
        transport
    } else {
        read(&dir.join("transport.json")).ok().flatten()
    };
    Ok((events, transport))
}

/// One complete alarm pass under a nonblocking queue lock. A busy or corrupt
/// recipient is retried later and cannot delay another recipient's alarm.
pub fn alarm_snapshot(root: &Path, uid: &str) -> io::Result<(Vec<Value>, Option<Value>)> {
    let dir = directory(root, uid);
    match fs::metadata(&dir) {
        Ok(meta) if meta.is_dir() => {},
        Ok(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "notification queue is not a directory")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), None)),
        Err(e) => return Err(e),
    }
    let _guard = lock_with(root, uid, true)?;
    snapshot_locked(&dir, true)?;
    retire_unconfirmable_locked(&dir)?;
    snapshot_locked(&dir, true)
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
    retire_unconfirmable_locked(&dir)
}

fn retire_unconfirmable_locked(dir: &Path) -> io::Result<usize> {
    let mut retired = 0;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|x| x != "json") || path.file_name().is_some_and(|n| n == "transport.json") {
            continue;
        }
        let Ok(Some(mut event)) = read(&path) else { continue };
        if !valid_census_event(&event) { continue; }
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

/// Retire ambiguous receipts (`submitted` / `uncertain`, never observed)
/// older than `max_age` seconds (by `updated_at`, else `created_at`): they
/// will never be confirmed, and must not hold anything back for good (#24:
/// 8–17-day-old receipts blocked a rotation boundary). Marks them cancelled
/// with `retired: "stale_receipt"`; returns how many.
pub fn retire_stale(root: &Path, uid: &str, max_age: f64, now: f64) -> io::Result<usize> {
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
        let at = event["updated_at"].as_f64().or_else(|| event["created_at"].as_f64()).unwrap_or(0.0);
        if open && now - at > max_age {
            event["status"] = json!("cancelled");
            event["retired"] = json!("stale_receipt");
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
    fn strict_census_rejects_corrupt_entries_and_non_directory_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        publish(root, "uid", "good", "chat", "[ok] wake", "[ok]").unwrap();
        let dir = directory(root, "uid");
        let bad = dir.join("broken.json");
        fs::write(&bad, b"{").unwrap();
        assert_eq!(snapshot(root, "uid").unwrap().0.len(), 1);
        assert!(snapshot_strict(root, "uid").is_err());
        fs::write(&bad, b"{}").unwrap();
        assert!(snapshot_strict(root, "uid").is_err());
        fs::write(&bad, br#"{"id":"x","status":"bogus","created_at":1}"#).unwrap();
        assert!(snapshot_strict(root, "uid").is_err());
        fs::write(&bad, br#"{"id":"x","status":"submitted"}"#).unwrap();
        assert!(snapshot_strict(root, "uid").is_err());
        let corrupt_marker = br#"{"id":"x","status":"submitted","created_at":1,"text":"wake","marker":42}"#;
        fs::write(&bad, corrupt_marker).unwrap();
        assert!(alarm_snapshot(root, "uid").is_err());
        assert_eq!(fs::read(&bad).unwrap(), corrupt_marker);
        fs::remove_file(bad).unwrap();
        assert_eq!(snapshot_strict(root, "uid").unwrap().0.len(), 1);
        fs::write(dir.join("transport.json"), b"{").unwrap();
        assert!(snapshot_strict(root, "uid").is_err());
        fs::write(dir.join("transport.json"), b"[]").unwrap();
        assert!(snapshot_strict(root, "uid").is_err());
        assert_eq!(snapshot(root, "uid").unwrap().0.len(), 1);
        let other = directory(root, "file");
        fs::write(&other, b"not a directory").unwrap();
        assert!(snapshot_strict(root, "file").is_err());
    }
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
    fn stale_ambiguous_receipts_are_retired_and_fresh_ones_kept() {
        let _env = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (id, status, age) in [("old-sub", "submitted", 9.0 * 86400.0), ("old-unc", "uncertain", 17.0 * 86400.0),
                                  ("fresh", "submitted", 60.0), ("waiting", "pending", 9.0 * 86400.0)] {
            let marker = format!("[m {id}]");
            publish(root, "uid", id, "chat", &format!("{marker} x"), &marker).unwrap();
            let mut e = get(root, "uid", id).unwrap().unwrap();
            e["status"] = json!(status);
            e["updated_at"] = json!(1_000_000.0 - age);
            atomic_replace(&path(root, "uid", id), &e).unwrap();
        }
        assert_eq!(retire_stale(root, "uid", 86400.0, 1_000_000.0).unwrap(), 2);
        assert_eq!(get(root, "uid", "old-sub").unwrap().unwrap()["retired"], "stale_receipt");
        assert_eq!(get(root, "uid", "fresh").unwrap().unwrap()["status"], "submitted");
        assert_eq!(get(root, "uid", "waiting").unwrap().unwrap()["status"], "pending", "a queued wake is not a receipt");
        // Holds round-trip.
        set_hold(root, "uid", 2e9).unwrap();
        assert!(held(root, "uid", 1e9) && !held(root, "uid", 3e9));
        clear_hold(root, "uid");
        assert!(!held(root, "uid", 1e9));
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
