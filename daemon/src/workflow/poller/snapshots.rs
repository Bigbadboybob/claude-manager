//! Poller-local, disk-validated workflow snapshots. Authoritative mutation and
//! strict handoff readers continue to use the uncached persistence helpers.
use crate::workflow::run::WorkflowRun;
use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct Snapshot {
    pub run: Arc<WorkflowRun>,
    pub broadcast_hash: Option<u64>,
}

#[derive(PartialEq, Eq)]
struct Version {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
impl Version {
    fn of(meta: &Metadata) -> Self {
        Self {
            dev: meta.dev(), ino: meta.ino(), len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

struct Entry {
    version: Version,
    snapshot: Snapshot,
}

#[derive(Default)]
pub(super) struct RunSnapshots {
    entries: HashMap<PathBuf, Entry>,
    #[cfg(test)]
    pub parses: usize,
    #[cfg(test)]
    pub fingerprints: usize,
    #[cfg(test)]
    pub before_refresh: Option<Box<dyn FnMut() + Send>>,
    #[cfg(test)]
    after_read: Option<Box<dyn FnMut(&Path) + Send>>,
}

impl RunSnapshots {
    /// Refresh at each phase boundary, including after transitions/finalizers.
    /// Caller must not hold the daemon registry lock while checking disk.
    pub fn refresh(&mut self, root: &Path) -> Vec<Snapshot> {
        #[cfg(test)]
        if let Some(hook) = self.before_refresh.as_mut() { hook(); }
        let Ok(entries) = fs::read_dir(root) else {
            self.entries.clear();
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() { continue; }
            let path = path.join("state.json");
            seen.insert(path.clone());
            match self.refresh_file(&path) {
                Ok(snapshot) => out.push(snapshot),
                Err(_) => {
                    // Match load_all's lenient skip behavior, never its old
                    // cached success. Retry failures even if metadata is equal.
                    self.entries.remove(&path);
                }
            }
        }
        // Full paths separate roots; deletion or a root change frees history.
        self.entries.retain(|path, _| seen.contains(path));
        out
    }

    fn refresh_file(&mut self, path: &Path) -> io::Result<Snapshot> {
        let meta = fs::metadata(path)?;
        if !meta.is_file() { return Err(io::Error::other("workflow state is not a file")); }
        let version = Version::of(&meta);
        if let Some(entry) = self.entries.get(path).filter(|e| e.version == version) {
            return Ok(entry.snapshot.clone());
        }

        let mut file = File::open(path)?;
        let opened = Version::of(&file.metadata()?);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        #[cfg(test)]
        if let Some(hook) = self.after_read.as_mut() { hook(path); }
        if Version::of(&file.metadata()?) != opened {
            return Err(io::Error::other("workflow state changed during read"));
        }
        #[cfg(test)]
        { self.parses += 1; }
        let run: WorkflowRun = serde_json::from_slice(&bytes)?;
        // Same semantic fingerprint as the old poller: a formatting-only
        // replacement is reloaded but does not produce an extra broadcast.
        #[cfg(test)]
        { self.fingerprints += 1; }
        let broadcast_hash = serde_json::to_string(&run).ok().map(|serialized| {
            let mut hasher = DefaultHasher::new();
            serialized.hash(&mut hasher);
            hasher.finish()
        });
        if Version::of(&fs::metadata(path)?) != opened {
            // Atomic replacement after open: don't label old bytes as the new
            // file. The next phase retries against the current path.
            return Err(io::Error::other("workflow state replaced during read"));
        }
        let snapshot = Snapshot { run: Arc::new(run), broadcast_hash };
        self.entries.insert(path.into(), Entry { version: opened, snapshot: snapshot.clone() });
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::run::RunStatus;

    fn sample(id: &str) -> WorkflowRun {
        WorkflowRun::new(id.into(), "feedback".into(), "workspace".into(),
            Default::default(), "worker".into(), Default::default(),
            Some("first".into()), Default::default(), 0)
    }

    fn replace(root: &Path, run: &WorkflowRun) -> PathBuf {
        let dir = root.join(&run.run_id);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let temp = dir.join("state.json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(run).unwrap()).unwrap();
        fs::rename(temp, &path).unwrap();
        path
    }

    #[test]
    fn file_snapshots_detect_same_size_replace_and_in_place_edit() {
        let root = tempfile::tempdir().unwrap();
        let mut run = sample("one");
        let path = replace(root.path(), &run);
        let mut cache = RunSnapshots::default();
        let first = cache.refresh(root.path()).pop().unwrap();
        let same = cache.refresh(root.path()).pop().unwrap();
        assert!(Arc::ptr_eq(&first.run, &same.run));
        assert_eq!((cache.parses, cache.fingerprints), (1, 1));

        let meta = fs::metadata(&path).unwrap();
        run.goal = Some("other".into());
        replace(root.path(), &run);
        File::open(&path).unwrap().set_modified(meta.modified().unwrap()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), meta.len());
        assert_ne!(fs::metadata(&path).unwrap().ino(), meta.ino());
        let replaced = cache.refresh(root.path()).pop().unwrap();
        assert_eq!(replaced.run.goal.as_deref(), Some("other"));
        assert!(!Arc::ptr_eq(&first.run, &replaced.run));
        assert_ne!(first.broadcast_hash, replaced.broadcast_hash);

        let inode = fs::metadata(&path).unwrap().ino();
        run.goal = Some("third".into());
        fs::write(&path, serde_json::to_vec_pretty(&run).unwrap()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(cache.refresh(root.path())[0].run.goal.as_deref(), Some("third"));
        assert_eq!((cache.parses, cache.fingerprints), (3, 3));

        fs::write(&path, serde_json::to_vec(&run).unwrap()).unwrap();
        let reformatted = cache.refresh(root.path()).pop().unwrap();
        let stable = cache.refresh(root.path()).pop().unwrap();
        assert_eq!(reformatted.broadcast_hash, stable.broadcast_hash);
        assert!(Arc::ptr_eq(&reformatted.run, &stable.run));
        assert_eq!((cache.parses, cache.fingerprints), (4, 4));
    }

    #[test]
    fn file_snapshots_evict_deleted_failed_and_other_root_entries() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let mut run = sample("one");
        let path = replace(root.path(), &run);
        let mut cache = RunSnapshots::default();
        assert_eq!(cache.refresh(root.path()).len(), 1);
        fs::write(&path, "not json").unwrap();
        assert!(cache.refresh(root.path()).is_empty());
        assert!(cache.entries.is_empty(), "never fall back to a stale parsed success");
        let attempts = cache.parses;
        assert!(cache.refresh(root.path()).is_empty());
        assert_eq!(cache.parses, attempts + 1, "unchanged bad files are retried");
        replace(root.path(), &run);
        assert_eq!(cache.refresh(root.path()).len(), 1);
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(cache.refresh(root.path()).is_empty());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(cache.refresh(root.path()).len(), 1);
        fs::remove_file(&path).unwrap();
        assert!(cache.refresh(root.path()).is_empty());
        replace(root.path(), &run);
        assert_eq!(cache.refresh(root.path()).len(), 1);
        replace(root.path(), &sample("two"));
        assert_eq!(cache.refresh(root.path()).len(), 2);
        run.goal = Some("different root".into());
        replace(other.path(), &run);
        let switched = cache.refresh(other.path());
        assert_eq!(switched.len(), 1);
        assert_eq!(switched[0].run.goal.as_deref(), Some("different root"));
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.refresh(&root.path().join("absent")).is_empty());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn file_snapshots_reject_replacement_during_read_then_retry() {
        let root = tempfile::tempdir().unwrap();
        let mut newer = sample("one");
        replace(root.path(), &newer);
        newer.goal = Some("newer".into());
        let mut cache = RunSnapshots::default();
        let bytes = serde_json::to_vec(&newer).unwrap();
        cache.after_read = Some(Box::new(move |path| {
            let temp = path.with_extension("next");
            fs::write(&temp, &bytes).unwrap();
            fs::rename(temp, path).unwrap();
        }));
        assert!(cache.refresh(root.path()).is_empty());
        assert!(cache.entries.is_empty());
        cache.after_read = None;
        assert_eq!(cache.refresh(root.path())[0].run.goal.as_deref(), Some("newer"));
    }

    #[test]
    fn file_snapshots_unchanged_terminal_inventory_does_not_parse_or_hash_again() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..361 {
            let mut run = sample(&format!("history-{i}"));
            run.status = if i < 210 { RunStatus::Done } else { RunStatus::Detached };
            run.history[0].last_message = Some("x".repeat(24_000));
            replace(root.path(), &run);
        }
        let rss_kib = || fs::read_to_string("/proc/self/status").ok().and_then(|s| {
            s.lines().find(|l| l.starts_with("VmRSS:"))?
                .split_whitespace().nth(1)?.parse::<u64>().ok()
        });
        let rss_before = rss_kib();
        let mut cache = RunSnapshots::default();
        let first = cache.refresh(root.path());
        assert_eq!(first.len(), 361);
        assert!(first.iter().all(|r| !r.run.is_active()));
        let rss_after = rss_kib();
        let payload_bytes: u64 = cache.entries.values().map(|e| e.version.len).sum();
        let mut elapsed = std::time::Duration::ZERO;
        for _ in 0..5 {
            let started = std::time::Instant::now();
            let current = cache.refresh(root.path());
            elapsed += started.elapsed();
            assert_eq!(current.len(), 361);
            assert!(current.iter().all(|r| first.iter().any(|old| Arc::ptr_eq(&old.run, &r.run))));
        }
        eprintln!("P3 fixture: 361 runs, {payload_bytes} file bytes, five warm refreshes {:.3}ms; RSS before/after {rss_before:?}/{rss_after:?}KiB (process/allocator estimate)",
            elapsed.as_secs_f64() * 1000.0);
        assert_eq!((cache.parses, cache.fingerprints), (361, 361));
    }
}
