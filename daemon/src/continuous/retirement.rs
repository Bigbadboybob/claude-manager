//! Per-session input/spawn barrier and durable retirement intents.
//!
//! The barrier is below every production PTY input path. Retiring a drained
//! session holds it exclusively while revalidating completion and recording
//! intent. Old handles, child spawns and restore then refuse the retired UID.
//! Intents live outside the task directory and survive task/session cleanup.
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

pub fn gate(uid: &str) -> Arc<RwLock<()>> {
    static GATES: OnceLock<Mutex<HashMap<PathBuf, Weak<RwLock<()>>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let key = path(uid);
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return gate;
    }
    gates.retain(|_, gate| gate.strong_count() > 0);
    let gate = Arc::new(RwLock::new(()));
    gates.insert(key, Arc::downgrade(&gate));
    gate
}

pub fn path(uid: &str) -> PathBuf {
    // Encode instead of trusting callers to have validated arbitrary UIDs.
    use sha2::{Digest, Sha256};
    crate::path::dot_cm_dir()
        .join("session-retirements")
        .join(format!("{:x}.json", Sha256::digest(uid.as_bytes())))
}

pub fn ensure_open(uid: &str) -> io::Result<()> {
    match std::fs::symlink_metadata(path(uid)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("session {uid} is durably retired; start a fresh session"),
        )),
    }
}

pub fn write_json(path: &Path, value: &serde_json::Value) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    let temp = parent.join(format!(".write-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    let _ = std::fs::remove_file(temp);
    result
}
