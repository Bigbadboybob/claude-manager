//! Offline-only S5 journal. Nothing here is connected to authorization or a worker.
//!
//! One exclusively locked, preallocated append-only file is both index and
//! journal: replay scans it with bounded metadata. The caller provides a 0700
//! directory; components are opened without following symlinks. Frames are
//! length + SHA-256 + strict typed JSON. A successful append means frame fsync
//! completed. It does not prove who supplied facts, remote custody, or S7
//! coverage. Unknown provenance remains unknown. No evidence is reclaimed.
//!
//! Recovery deliberately quarantines a torn tail instead of truncating it:
//! the valid prefix can be visited but no more writes are allowed. A separate
//! crash-safe gap-marker/reclamation protocol is needed before repair. This is
//! a conservative deviation from the contract's future automatic tail repair.
//! See `doc/org-model-s5-spool.md` for bounds and failure windows.

use crate::shadow_observation::{Envelope, EventId, Id, MAX_EVENT_BYTES};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

const MAGIC: &[u8; 8] = b"S5SPL001";
const HEADER: u64 = 24;
const FRAME_HEADER: usize = 36;
const MAX_RECORD_BYTES: usize = 8192;
const MAX_RECORDS: usize = 4096;
const RESERVED_CONTROL_RECORDS: usize = 256;
const MAX_CAPACITY: u64 = 64 * 1024 * 1024;
const BLOCK: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Preallocated file data blocks, not filesystem/inode/journal metadata.
    pub capacity: u64,
    /// Logical bytes withheld from events for interval/ack control frames.
    pub control_reserve: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    pub attempted: u64,
    pub admitted: u64,
    pub loss: u64,
    pub oversize: u64,
    pub invalid: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health { Unopened, Open, Clean, Incomplete, UncleanSuffix, PartialTail, Corrupt, IoUncertain }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Append { Stored, Duplicate }

#[derive(Debug)]
pub enum Error {
    Io(io::Error), Invalid, Bounds, Full, Busy, Conflict, NoOpen, Quarantined,
    UnknownEvent, Unacknowledged, Uncheckpointed,
}
impl From<io::Error> for Error { fn from(value: io::Error) -> Self { Self::Io(value) } }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Record {
    Open { producer: Id, boot: [u8; 16] },
    Event { envelope: Envelope },
    Checkpoint { producer: Id, boot: [u8; 16], next_sequence: u64, counts: Counts },
    /// A known lower bound; an unknown suffix is never turned into zero loss.
    Gap { producer: Id, boot: [u8; 16], known_loss: u64, unknown_suffix: bool },
    Ack { id: EventId, digest: [u8; 32], receipt: Id },
    Close { producer: Id, boot: [u8; 16], next_sequence: u64 },
}

#[derive(Clone, Copy)]
struct EventMeta { id: EventId, digest: [u8; 32], ack: Option<Id> }
#[derive(Clone, Copy)]
struct Epoch {
    producer: Id, boot: [u8; 16], checkpoint: Option<(u64, Counts)>,
    closed: bool, event_count: u64, last_sequence: u64,
}
struct State {
    epochs: Vec<Epoch>, events: Vec<EventMeta>, used: u64, event_bytes: u64,
    records: usize, health: Health, known_loss: u64, unknown_suffix: bool,
}
impl State {
    fn new() -> Self {
        Self { epochs: Vec::new(), events: Vec::new(), used: HEADER, event_bytes: 0,
            records: 0, health: Health::Clean, known_loss: 0, unknown_suffix: false }
    }
    fn active(&self) -> Option<&Epoch> { self.epochs.last().filter(|e| !e.closed) }
}

/// The lock is held for the handle's lifetime. Dropping the handle does not
/// imply clean close. `journal` and `lock` are the only files this creates.
pub struct Spool {
    _directory: File, _lock: File, journal: File, limits: Limits, state: State,
    unrecorded_loss: bool, restart_uncertain: bool,
    #[cfg(test)] fail_next_sync: bool,
}

fn digest(data: &[u8]) -> [u8; 32] { Sha256::digest(data).into() }
fn checked_event(envelope: &Envelope) -> Result<Vec<u8>, Error> {
    if envelope.id.boot == [0; 16] || envelope.id.sequence == 0
        || envelope.id.child != envelope.facts.child || envelope.id.component != envelope.facts.component {
        return Err(Error::Invalid);
    }
    // Fixed-size typed input, serialized under the accepted event limit.
    let mut writer = Bounded::new(MAX_EVENT_BYTES);
    serde_json::to_writer(&mut writer, envelope).map_err(|_| Error::Bounds)?;
    Ok(writer.bytes)
}
struct Bounded { bytes: Vec<u8>, limit: usize }
impl Bounded { fn new(limit: usize) -> Self { Self { bytes: Vec::with_capacity(limit), limit } } }
impl Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bounded record"));
        }
        self.bytes.extend_from_slice(buf); Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn open_component(dir: &File, name: &std::ffi::OsStr) -> io::Result<File> {
    let name = CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL"))?;
    // Each component is opened relative to the previous directory descriptor.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn open_directory(path: &Path) -> Result<File, Error> {
    if !path.is_absolute() { return Err(Error::Invalid); }
    let mut dir = File::open("/")?;
    for part in path.components() {
        match part { Component::RootDir => {}, Component::Normal(name) => dir = open_component(&dir, name)?, _ => return Err(Error::Invalid) }
    }
    let m = dir.metadata()?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o7777 != 0o700 { return Err(Error::Invalid); }
    Ok(dir)
}
fn open_file(dir: &File, name: &str, create: bool) -> Result<File, Error> {
    let name = CString::new(name).map_err(|_| Error::Invalid)?;
    let mut flags = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    if create { flags |= libc::O_CREAT | libc::O_EXCL; }
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 { return Err(Error::Io(io::Error::last_os_error())); }
    let f = unsafe { File::from_raw_fd(fd) };
    let m = f.metadata()?;
    if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o7777 != 0o600 || m.nlink() != 1 { return Err(Error::Invalid); }
    Ok(f)
}
fn open_or_create(dir: &File, name: &str) -> Result<(File, bool), Error> {
    match open_file(dir, name, true) {
        Ok(f) => Ok((f, true)),
        Err(Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => Ok((open_file(dir, name, false)?, false)),
        Err(e) => Err(e),
    }
}
fn validate_lock(lock: &File) -> Result<(), Error> {
    let m = lock.metadata()?;
    if m.len() != 0 || m.blocks() != 0 || m.nlink() != 1 || !m.is_file()
        || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o7777 != 0o600 { return Err(Error::Bounds); }
    Ok(())
}
fn valid_limits(l: Limits) -> bool {
    l.capacity >= BLOCK * 4 && l.capacity <= MAX_CAPACITY && l.capacity % BLOCK == 0
        && l.control_reserve >= BLOCK && l.control_reserve < l.capacity - HEADER
}
fn allocated(f: &File) -> Result<u64, Error> {
    f.metadata()?.blocks().checked_mul(512).ok_or(Error::Bounds)
}

impl State {
    fn apply(&mut self, record: &Record, frame_bytes: u64, limits: Limits) -> Result<(), Error> {
        if self.records >= MAX_RECORDS { return Err(Error::Full); }
        let total = self.used.checked_add(frame_bytes).ok_or(Error::Bounds)?;
        if total > limits.capacity { return Err(Error::Full); }
        match record {
            Record::Open { producer, boot } => {
                if *boot == [0; 16] || self.epochs.iter().any(|e| e.producer == *producer && e.boot == *boot) {
                    return Err(Error::Conflict);
                }
                if self.active().is_some() { self.unknown_suffix = true; }
                self.epochs.push(Epoch { producer: *producer, boot: *boot, checkpoint: None, closed: false, event_count: 0, last_sequence: 0 });
            }
            Record::Event { envelope } => {
                if self.events.len() >= MAX_RECORDS - RESERVED_CONTROL_RECORDS { return Err(Error::Full); }
                let payload = checked_event(envelope)?;
                let id = envelope.id;
                let epoch = self.epochs.last_mut().filter(|e| !e.closed).ok_or(Error::NoOpen)?;
                if id.producer != epoch.producer || id.boot != epoch.boot || id.sequence <= epoch.last_sequence
                    || epoch.checkpoint.is_some_and(|(next, _)| id.sequence < next) { return Err(Error::Invalid); }
                let d = digest(&payload);
                if self.events.iter().any(|e| e.id.producer == id.producer && e.id.boot == id.boot
                    && e.id.sequence == id.sequence && e.id != id) { return Err(Error::Conflict); }
                if let Some(previous) = self.events.iter().find(|e| e.id == id) {
                    return Err(if previous.digest == d { Error::Invalid } else { Error::Conflict });
                }
                let event_bytes = self.event_bytes.checked_add(frame_bytes).ok_or(Error::Bounds)?;
                if event_bytes > limits.capacity - limits.control_reserve - HEADER { return Err(Error::Full); }
                self.events.push(EventMeta { id, digest: d, ack: None });
                self.event_bytes = event_bytes;
                epoch.event_count = epoch.event_count.checked_add(1).ok_or(Error::Bounds)?;
                epoch.last_sequence = id.sequence;
            }
            Record::Checkpoint { producer, boot, next_sequence, counts } => {
                if self.events.iter().any(|e| e.id.producer == *producer && e.id.boot == *boot && e.id.sequence >= *next_sequence) { return Err(Error::Invalid); }
                let epoch = self.epochs.last_mut().filter(|e| !e.closed && e.producer == *producer && e.boot == *boot).ok_or(Error::NoOpen)?;
                if counts.attempted.checked_add(1) != Some(*next_sequence)
                    || counts.admitted.checked_add(counts.loss) != Some(counts.attempted)
                    || counts.oversize > counts.loss || counts.invalid > counts.loss
                    || counts.oversize.checked_add(counts.invalid).is_none_or(|v| v > counts.loss)
                    || counts.admitted != epoch.event_count { return Err(Error::Invalid); }
                if let Some((old_seq, old)) = epoch.checkpoint {
                    if *next_sequence < old_seq || counts.attempted < old.attempted || counts.admitted < old.admitted
                        || counts.loss < old.loss || counts.oversize < old.oversize || counts.invalid < old.invalid {
                        return Err(Error::Invalid);
                    }
                }
                epoch.checkpoint = Some((*next_sequence, *counts));
            }
            Record::Gap { producer, boot, known_loss, unknown_suffix } => {
                if !self.epochs.iter().any(|e| e.producer == *producer && e.boot == *boot) { return Err(Error::Invalid); }
                // Gap reports are cumulative lower bounds, not deltas: a
                // retried marker must never double-count the same loss.
                self.known_loss = self.known_loss.max(*known_loss);
                self.unknown_suffix |= *unknown_suffix;
            }
            Record::Ack { id, digest, receipt } => {
                let event = self.events.iter_mut().find(|e| e.id == *id).ok_or(Error::UnknownEvent)?;
                if event.digest != *digest { return Err(Error::Conflict); }
                if let Some(old) = event.ack { return Err(if old == *receipt { Error::Invalid } else { Error::Conflict }); }
                event.ack = Some(*receipt);
            }
            Record::Close { producer, boot, next_sequence } => {
                let epoch = self.epochs.last_mut().filter(|e| !e.closed && e.producer == *producer && e.boot == *boot).ok_or(Error::NoOpen)?;
                let (seq, counts) = epoch.checkpoint.ok_or(Error::Uncheckpointed)?;
                if seq != *next_sequence || counts.admitted != epoch.event_count { return Err(Error::Uncheckpointed); }
                if self.events.iter().any(|e| e.id.producer == *producer && e.id.boot == *boot && e.ack.is_none()) {
                    return Err(Error::Unacknowledged);
                }
                epoch.closed = true;
            }
        }
        self.used = total; self.records += 1;
        Ok(())
    }
}
impl Clone for State {
    fn clone(&self) -> Self {
        Self { epochs: self.epochs.clone(), events: self.events.clone(), used: self.used,
            event_bytes: self.event_bytes, records: self.records, health: self.health,
            known_loss: self.known_loss, unknown_suffix: self.unknown_suffix }
    }
}

fn frame(record: &Record) -> Result<Vec<u8>, Error> {
    let mut payload = Bounded::new(MAX_RECORD_BYTES);
    serde_json::to_writer(&mut payload, record).map_err(|_| Error::Bounds)?;
    let len = u32::try_from(payload.bytes.len()).map_err(|_| Error::Bounds)?;
    let mut out = Vec::with_capacity(FRAME_HEADER + payload.bytes.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&digest(&payload.bytes));
    out.extend_from_slice(&payload.bytes);
    Ok(out)
}

impl Spool {
    /// No fallback from failed preallocation. The directory is fixture/operator
    /// supplied and must be private; existing files are never replaced.
    pub fn open(path: &Path, limits: Limits) -> Result<Self, Error> {
        Self::open_internal(path, limits, false, false)
    }
    fn open_internal(path: &Path, limits: Limits, inject_recovery_sync_failure: bool, inject_directory_sync_failure: bool) -> Result<Self, Error> {
        #[cfg(not(test))] let _ = (inject_recovery_sync_failure, inject_directory_sync_failure);
        if !valid_limits(limits) { return Err(Error::Bounds); }
        let directory = open_directory(path)?;
        let (lock, _lock_created) = open_or_create(&directory, "lock")?;
        let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::WouldBlock { Err(Error::Busy) } else { Err(Error::Io(e)) };
        }
        validate_lock(&lock)?;
        lock.sync_all()?;
        let (mut journal, created) = open_or_create(&directory, "journal")?;
        if created {
            // KEEP_SIZE reserves all journal blocks without making unwritten
            // capacity look like records; failed allocation disables the spool.
            let rc = unsafe { libc::fallocate(journal.as_raw_fd(), libc::FALLOC_FL_KEEP_SIZE, 0, limits.capacity as libc::off_t) };
            if rc != 0 { return Err(Error::Io(io::Error::last_os_error())); }
            if allocated(&journal)? != limits.capacity { return Err(Error::Bounds); }
            journal.write_all(MAGIC)?;
            journal.write_all(&limits.capacity.to_be_bytes())?;
            journal.write_all(&limits.control_reserve.to_be_bytes())?;
            journal.sync_all()?;
        }
        if allocated(&journal)? != limits.capacity || journal.metadata()?.len() > limits.capacity { return Err(Error::Bounds); }
        let mut head = [0u8; HEADER as usize];
        journal.seek(SeekFrom::Start(0))?;
        journal.read_exact(&mut head)?;
        if &head[..8] != MAGIC || u64::from_be_bytes(head[8..16].try_into().unwrap()) != limits.capacity
            || u64::from_be_bytes(head[16..24].try_into().unwrap()) != limits.control_reserve { return Err(Error::Invalid); }
        // Sync the parent even when adopting visible entries from an earlier
        // interrupted initialization; visibility alone is not durable naming.
        #[cfg(test)]
        if inject_directory_sync_failure { return Err(Error::Io(io::Error::other("injected directory fsync failure"))); }
        directory.sync_all()?;
        let mut spool = Self { _directory: directory, _lock: lock, journal, limits, state: State::new(),
            unrecorded_loss: false, restart_uncertain: false,
            #[cfg(test)] fail_next_sync: inject_recovery_sync_failure };
        spool.recover()?;
        Ok(spool)
    }

    fn recover(&mut self) -> Result<(), Error> {
        let length = self.journal.metadata()?.len();
        self.journal.seek(SeekFrom::Start(HEADER))?;
        while self.state.used < length {
            let remain = length - self.state.used;
            if remain < FRAME_HEADER as u64 { self.state.health = Health::PartialTail; self.state.unknown_suffix = true; break; }
            let mut hdr = [0u8; FRAME_HEADER];
            self.journal.read_exact(&mut hdr)?;
            let n = u32::from_be_bytes(hdr[..4].try_into().unwrap()) as usize;
            if n == 0 || n > MAX_RECORD_BYTES { self.state.health = Health::Corrupt; self.state.unknown_suffix = true; break; }
            if remain < (FRAME_HEADER + n) as u64 { self.state.health = Health::PartialTail; self.state.unknown_suffix = true; break; }
            let mut bytes = vec![0; n];
            self.journal.read_exact(&mut bytes)?;
            if digest(&bytes).as_slice() != &hdr[4..] { self.state.health = Health::Corrupt; self.state.unknown_suffix = true; break; }
            let record: Record = match serde_json::from_slice(&bytes) { Ok(v) => v, Err(_) => {
                self.state.health = Health::Corrupt; self.state.unknown_suffix = true; break;
            }};
            if frame(&record).map_or(true, |canonical| &canonical[FRAME_HEADER..] != bytes.as_slice())
                || self.state.apply(&record, (FRAME_HEADER + n) as u64, self.limits).is_err() {
                self.state.health = Health::Corrupt; self.state.unknown_suffix = true; break;
            }
        }
        // A complete close visible after a failed fsync is not yet durable.
        // Re-sync the validated complete prefix before exposing clean status.
        if self.state.health == Health::Clean && self.sync_journal().is_err() {
            self.state.health = Health::IoUncertain; self.state.unknown_suffix = true;
        }
        if self.state.epochs.is_empty() && self.state.health == Health::Clean { self.state.health = Health::Unopened; }
        if self.state.active().is_some() { self.state.unknown_suffix = true; self.restart_uncertain = true; }
        if self.state.health == Health::Clean && self.state.unknown_suffix { self.state.health = Health::UncleanSuffix; }
        Ok(())
    }

    /// Health describes the whole journal, not just the current epoch.
    pub fn health(&self) -> Health {
        if self.state.health == Health::Clean && self.state.unknown_suffix { Health::UncleanSuffix }
        else if self.state.health == Health::Clean && self.known_loss_lower_bound() > 0 { Health::Incomplete }
        else if self.state.health == Health::Clean && self.state.active().is_some() { Health::Open }
        else { self.state.health }
    }
    pub fn unknown_suffix(&self) -> bool { self.state.unknown_suffix || self.state.epochs.is_empty() }
    pub fn known_loss_lower_bound(&self) -> u64 {
        let checkpoints = self.state.epochs.iter().filter_map(|e| e.checkpoint.map(|(_, c)| c.loss))
            .fold(0u64, u64::saturating_add);
        checkpoints.max(self.state.known_loss)
    }
    pub fn allocated_data_bytes(&self) -> Result<u64, Error> { validate_lock(&self._lock)?; allocated(&self.journal) }
    pub fn logical_bytes(&self) -> u64 { self.state.used }

    fn sync_journal(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_next_sync {
            self.fail_next_sync = false;
            return Err(io::Error::other("injected journal fsync failure"));
        }
        self.journal.sync_all()
    }

    // A complete frame may have survived a failed fsync. Re-sync before an
    // idempotent success, so a retry never mistakes visibility for durability.
    fn sync_existing(&mut self) -> Result<(), Error> {
        if matches!(self.state.health, Health::PartialTail | Health::Corrupt | Health::IoUncertain) { return Err(Error::Quarantined); }
        let check = (|| -> Result<(), Error> {
            validate_lock(&self._lock)?;
            if allocated(&self.journal)? != self.limits.capacity || self.journal.metadata()?.len() != self.state.used { return Err(Error::Bounds); }
            self.sync_journal()?;
            Ok(())
        })();
        if check.is_err() { self.state.health = Health::IoUncertain; self.state.unknown_suffix = true; }
        check
    }

    fn append(&mut self, record: Record) -> Result<(), Error> {
        if matches!(self.state.health, Health::PartialTail | Health::Corrupt | Health::IoUncertain) { return Err(Error::Quarantined); }
        let bytes = frame(&record)?;
        let mut next = self.state.clone();
        next.apply(&record, bytes.len() as u64, self.limits)?;
        if validate_lock(&self._lock).is_err() || allocated(&self.journal)? != self.limits.capacity || self.journal.metadata()?.len() != self.state.used {
            self.state.health = Health::IoUncertain; self.state.unknown_suffix = true; return Err(Error::Bounds);
        }
        let written = self.journal.seek(SeekFrom::End(0)).and_then(|_| self.journal.write_all(&bytes));
        let persisted = written.and_then(|_| self.sync_journal());
        if let Err(e) = persisted {
            self.state.health = Health::IoUncertain;
            self.state.unknown_suffix = true;
            return Err(Error::Io(e));
        }
        self.state = next;
        Ok(())
    }

    /// Start a never-before-used producer/boot pair. A previous unclosed epoch
    /// remains unknown even when a new open is successfully persisted.
    pub fn begin(&mut self, producer: Id, boot: [u8; 16]) -> Result<(), Error> {
        self.append(Record::Open { producer, boot })?;
        if self.state.health == Health::Unopened { self.state.health = Health::Clean; }
        self.restart_uncertain = false;
        Ok(())
    }
    /// Canonical digest used to match an event with an external receipt.
    pub fn event_digest(envelope: &Envelope) -> Result<[u8; 32], Error> {
        Ok(digest(&checked_event(envelope)?))
    }
    /// The exact typed envelope is preserved; this is not origin authentication.
    pub fn append_event(&mut self, envelope: &Envelope) -> Result<Append, Error> {
        if matches!(self.state.health, Health::PartialTail | Health::Corrupt | Health::IoUncertain) { return Err(Error::Quarantined); }
        let payload = match checked_event(envelope) {
            Ok(p) => p,
            Err(e) => { self.unrecorded_loss = true; self.state.unknown_suffix = true; return Err(e); }
        };
        let d = digest(&payload);
        if let Some(old) = self.state.events.iter().find(|e| e.id == envelope.id) {
            if old.digest == d { self.sync_existing()?; return Ok(Append::Duplicate); }
            self.unrecorded_loss = true; self.state.unknown_suffix = true;
            if self.state.health == Health::Clean { self.state.health = Health::UncleanSuffix; }
            return Err(Error::Conflict);
        }
        if self.restart_uncertain { return Err(Error::NoOpen); }
        match self.append(Record::Event { envelope: *envelope }) {
            Ok(()) => Ok(Append::Stored),
            Err(e) => {
                self.unrecorded_loss = true; self.state.unknown_suffix = true;
                if self.state.health == Health::Clean { self.state.health = Health::UncleanSuffix; }
                Err(e)
            }
        }
    }
    pub fn checkpoint(&mut self, next_sequence: u64, counts: Counts) -> Result<(), Error> {
        if self.restart_uncertain { return Err(Error::NoOpen); }
        let e = *self.state.active().ok_or(Error::NoOpen)?;
        self.append(Record::Checkpoint { producer: e.producer, boot: e.boot, next_sequence, counts })
    }
    /// `known_loss` is a cumulative journal-wide lower bound, not a delta.
    pub fn record_gap(&mut self, known_loss: u64, unknown_suffix: bool) -> Result<(), Error> {
        if self.unrecorded_loss && !unknown_suffix { return Err(Error::Invalid); }
        let e = *self.state.active().ok_or(Error::NoOpen)?;
        let result = self.append(Record::Gap { producer: e.producer, boot: e.boot, known_loss, unknown_suffix });
        // Failure to persist the gap must itself make coverage unknown.
        if result.is_err() { self.state.unknown_suffix = true; self.state.health = Health::IoUncertain; }
        else { self.unrecorded_loss = false; }
        result
    }
    /// This records a caller-supplied receipt, not proof that a remote system
    /// committed it. No record is removed, regardless of acknowledgment.
    pub fn acknowledge(&mut self, id: EventId, d: [u8; 32], receipt: Id) -> Result<Append, Error> {
        if matches!(self.state.health, Health::PartialTail | Health::Corrupt | Health::IoUncertain) { return Err(Error::Quarantined); }
        let old = self.state.events.iter().find(|e| e.id == id).ok_or(Error::UnknownEvent)?;
        if old.digest != d { return Err(Error::Conflict); }
        if let Some(existing) = old.ack {
            if existing != receipt { return Err(Error::Conflict); }
            self.sync_existing()?; return Ok(Append::Duplicate);
        }
        self.append(Record::Ack { id, digest: d, receipt })?;
        Ok(Append::Stored)
    }
    /// Caller asserts it has drained the queue. Persist only if checkpoint
    /// counts and local acknowledged events match; review holds are retained.
    pub fn close(&mut self, next_sequence: u64) -> Result<(), Error> {
        if self.unrecorded_loss || self.restart_uncertain { return Err(Error::Uncheckpointed); }
        if let Some(e) = self.state.epochs.last() {
            if e.closed && e.checkpoint.map(|(seq, _)| seq) == Some(next_sequence) {
                return self.sync_existing();
            }
        }
        let e = *self.state.active().ok_or(Error::NoOpen)?;
        self.append(Record::Close { producer: e.producer, boot: e.boot, next_sequence })
    }
    /// Inclusive sequences with a recorded receipt. Holes, unknown suffixes,
    /// and absent producers are not implied to be acknowledged.
    pub fn acknowledged_ranges(&self, producer: Id, boot: [u8; 16]) -> Vec<(u64, u64)> {
        let mut seqs: Vec<u64> = self.state.events.iter()
            .filter(|e| e.id.producer == producer && e.id.boot == boot && e.ack.is_some())
            .map(|e| e.id.sequence).collect();
        seqs.sort_unstable();
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for n in seqs {
            if let Some(last) = ranges.last_mut() {
                if last.1.checked_add(1) == Some(n) { last.1 = n; continue; }
            }
            ranges.push((n, n));
        }
        ranges
    }
    /// Replays the valid prefix even when a damaged suffix is quarantined.
    /// The visitor receives a detached typed snapshot and canonical digest.
    pub fn replay<F: FnMut(Envelope, [u8; 32], bool)>(&mut self, mut visit: F) -> Result<(), Error> {
        let result = self.replay_inner(&mut visit);
        if let Err(ref e) = result {
            self.state.health = if matches!(e, Error::Io(_)) { Health::IoUncertain } else { Health::Corrupt };
            self.state.unknown_suffix = true;
        }
        result
    }
    fn replay_inner<F: FnMut(Envelope, [u8; 32], bool)>(&mut self, visit: &mut F) -> Result<(), Error> {
        self.journal.seek(SeekFrom::Start(HEADER))?;
        let mut offset = HEADER;
        while offset < self.state.used {
            let mut hdr = [0u8; FRAME_HEADER]; self.journal.read_exact(&mut hdr)?;
            let n = u32::from_be_bytes(hdr[..4].try_into().unwrap()) as usize;
            if n == 0 || n > MAX_RECORD_BYTES { return Err(Error::Invalid); }
            let mut bytes = vec![0; n]; self.journal.read_exact(&mut bytes)?;
            if digest(&bytes).as_slice() != &hdr[4..] { return Err(Error::Invalid); }
            let r: Record = serde_json::from_slice(&bytes).map_err(|_| Error::Invalid)?;
            if frame(&r).map_or(true, |canonical| &canonical[FRAME_HEADER..] != bytes.as_slice()) { return Err(Error::Invalid); }
            if let Record::Event { envelope } = r {
                let d = digest(&checked_event(&envelope)?);
                let m = self.state.events.iter().find(|m| m.id == envelope.id && m.digest == d).ok_or(Error::Invalid)?;
                visit(envelope, d, m.ack.is_some());
            }
            offset += (FRAME_HEADER + n) as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shadow_observation::{Authorization, Execution};
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn fixture_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
    fn limits() -> Limits { Limits { capacity: 128 * 1024, control_reserve: 32 * 1024 } }
    fn id(s: &str) -> Id { Id::new(s).unwrap() }
    fn envelope(n: u64) -> Envelope {
        let fixtures: Vec<serde_json::Value> = serde_json::from_str(include_str!("../../tests/fixtures/s5-shadow/cases.json")).unwrap();
        let mut e: Envelope = serde_json::from_value(fixtures[0]["wire"].clone()).unwrap();
        e.id.sequence = n; e
    }
    fn opened(dir: &Path) -> Spool {
        let mut s = Spool::open(dir, limits()).unwrap();
        s.begin(id("producer"), [1; 16]).unwrap(); s
    }
    fn counts(n: u64) -> Counts { Counts { attempted: n, admitted: n, ..Counts::default() } }

    #[test]
    fn durable_roundtrip_dedupe_receipt_and_close() {
        let dir = fixture_dir();
        let mut s = Spool::open(dir.path(), limits()).unwrap();
        assert_eq!(s.health(), Health::Unopened);
        assert!(s.unknown_suffix());
        assert_eq!(s.allocated_data_bytes().unwrap(), limits().capacity);
        s.begin(id("producer"), [1; 16]).unwrap();
        assert_eq!(s.health(), Health::Open);
        let e = envelope(1);
        let original = e;
        let d = Spool::event_digest(&e).unwrap();
        assert_eq!(s.append_event(&e).unwrap(), Append::Stored);
        assert_eq!(s.append_event(&e).unwrap(), Append::Duplicate);
        s.checkpoint(2, counts(1)).unwrap();
        assert!(matches!(s.close(2), Err(Error::Unacknowledged)));
        assert!(matches!(s.acknowledge(e.id, [9; 32], id("receipt")), Err(Error::Conflict)));
        assert_eq!(s.acknowledge(e.id, d, id("receipt")).unwrap(), Append::Stored);
        assert_eq!(s.acknowledge(e.id, d, id("receipt")).unwrap(), Append::Duplicate);
        s.close(2).unwrap(); drop(s);
        let mut s = Spool::open(dir.path(), limits()).unwrap();
        assert_eq!(s.health(), Health::Clean);
        let mut replay = Vec::new(); s.replay(|e, d, ack| replay.push((e, d, ack))).unwrap();
        assert_eq!(replay, vec![(original, d, true)]);
        assert_eq!(s.acknowledged_ranges(id("producer"), [1; 16]), vec![(1, 1)]);
        assert_eq!(replay[0].0.facts.actual_authorization, Authorization::Allow);
        assert_eq!(replay[0].0.facts.execution, Execution::Completed);
        assert!(matches!(s.begin(id("producer"), [1; 16]), Err(Error::Conflict)));
        s.begin(id("producer"), [2; 16]).unwrap();
    }

    #[test]
    fn conflicting_event_cannot_become_clean_coverage() {
        let dir = fixture_dir(); let mut s = opened(dir.path());
        let e = envelope(1); s.append_event(&e).unwrap();
        let mut changed = e; changed.facts.actual_authorization = Authorization::Deny;
        assert!(matches!(s.append_event(&changed), Err(Error::Conflict)));
        assert!(s.unknown_suffix());
        assert!(matches!(s.close(2), Err(Error::Uncheckpointed)));
        s.record_gap(0, true).unwrap(); s.checkpoint(2, counts(1)).unwrap();
        s.acknowledge(e.id, Spool::event_digest(&e).unwrap(), id("receipt")).unwrap();
        s.close(2).unwrap(); drop(s);
        let s = Spool::open(dir.path(), limits()).unwrap(); assert!(s.unknown_suffix());
    }

    #[test]
    fn unclean_boot_replay_and_stable_receipt() {
        let dir = fixture_dir();
        let mut s = opened(dir.path());
        let e = envelope(1); let d = Spool::event_digest(&e).unwrap();
        s.append_event(&e).unwrap(); s.checkpoint(2, counts(1)).unwrap();
        drop(s); // no close: checkpoint is a lower bound, never a clean suffix
        let mut s = Spool::open(dir.path(), limits()).unwrap();
        assert_eq!(s.health(), Health::UncleanSuffix);
        assert!(s.unknown_suffix());
        assert!(matches!(s.checkpoint(2, counts(1)), Err(Error::NoOpen)));
        assert!(matches!(s.close(2), Err(Error::Uncheckpointed)));
        assert!(matches!(s.append_event(&envelope(2)), Err(Error::NoOpen)));
        let mut seen = Vec::new(); s.replay(|e, d, ack| seen.push((e, d, ack))).unwrap();
        assert_eq!(seen, vec![(e, d, false)]);
        assert!(matches!(s.begin(id("producer"), [1; 16]), Err(Error::Conflict)));
        s.begin(id("producer"), [2; 16]).unwrap();
        assert_eq!(s.acknowledge(e.id, d, id("receipt")).unwrap(), Append::Stored);
        drop(s);
        let mut s = Spool::open(dir.path(), limits()).unwrap();
        let mut ack = false; s.replay(|_, _, a| ack = a).unwrap(); assert!(ack);
        assert!(s.unknown_suffix());
    }

    #[test]
    fn partial_tail_and_middle_corruption_quarantine_prefix() {
        let dir = fixture_dir();
        let mut s = opened(dir.path()); let e = envelope(1);
        s.append_event(&e).unwrap(); let prefix = s.logical_bytes(); drop(s);
        let path = dir.path().join("journal");
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::End(0)).unwrap(); file.write_all(&[0, 0, 0]).unwrap(); file.sync_all().unwrap(); drop(file);
        let mut s = Spool::open(dir.path(), limits()).unwrap();
        assert_eq!(s.health(), Health::PartialTail); assert!(s.unknown_suffix());
        assert_eq!(s.logical_bytes(), prefix);
        let mut seen = Vec::new(); s.replay(|e, _, _| seen.push(e)).unwrap(); assert_eq!(seen, vec![e]);
        assert!(matches!(s.append_event(&e), Err(Error::Quarantined))); drop(s);
        // Corrupt an already complete frame, preserving the torn tail.
        let mut file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(HEADER + FRAME_HEADER as u64 + 1)).unwrap();
        let mut b = [0]; file.read_exact(&mut b).unwrap(); b[0] ^= 1;
        file.seek(SeekFrom::Current(-1)).unwrap(); file.write_all(&b).unwrap(); file.sync_all().unwrap(); drop(file);
        let mut s = Spool::open(dir.path(), limits()).unwrap();
        assert_eq!(s.health(), Health::Corrupt); assert!(s.unknown_suffix());
        let mut visited = 0; s.replay(|_, _, _| visited += 1).unwrap(); assert_eq!(visited, 0);
    }

    #[test]
    fn capacity_gap_and_control_exhaustion_are_never_clean() {
        let dir = fixture_dir(); let small = Limits { capacity: 16 * 1024, control_reserve: 4096 };
        let mut s = Spool::open(dir.path(), small).unwrap(); s.begin(id("producer"), [1; 16]).unwrap();
        let mut n = 1;
        loop {
            match s.append_event(&envelope(n)) { Ok(Append::Stored) => n += 1, Err(Error::Full) => break, other => panic!("unexpected {other:?}") }
            assert!(n < 100);
        }
        assert!(s.unknown_suffix());
        assert!(matches!(s.close(n), Err(Error::Uncheckpointed)));
        assert!(matches!(s.record_gap(0, false), Err(Error::Invalid)));
        s.record_gap(1, true).unwrap();
        assert_eq!(s.known_loss_lower_bound(), 1);
        loop {
            match s.record_gap(1, true) { Ok(()) => {}, Err(Error::Full) => break, other => panic!("unexpected {other:?}") }
        }
        assert_eq!(s.health(), Health::IoUncertain); drop(s);
        let s = Spool::open(dir.path(), small).unwrap();
        assert!(s.unknown_suffix()); assert_eq!(s.known_loss_lower_bound(), 1);
    }

    #[test]
    fn sequences_require_accounted_holes_and_no_late_events() {
        let dir = fixture_dir(); let mut s = opened(dir.path());
        assert_eq!(s.health(), Health::Open);
        let e = envelope(2); s.append_event(&e).unwrap();
        assert!(matches!(s.checkpoint(3, counts(1)), Err(Error::Invalid)));
        // Two claimed admissions with only one flushed record are unsupported.
        assert!(matches!(s.checkpoint(3, counts(2)), Err(Error::Invalid)));
        let c = Counts { attempted: 2, admitted: 1, loss: 1, ..Counts::default() };
        s.checkpoint(3, c).unwrap();
        assert_eq!(s.health(), Health::Incomplete);
        s.acknowledge(e.id, Spool::event_digest(&e).unwrap(), id("receipt")).unwrap();
        s.close(3).unwrap(); drop(s);
        let s = Spool::open(dir.path(), limits()).unwrap();
        assert_eq!(s.health(), Health::Incomplete);

        let dir2 = fixture_dir(); let mut s = opened(dir2.path());
        s.append_event(&e).unwrap(); s.checkpoint(3, c).unwrap();
        assert!(matches!(s.append_event(&envelope(1)), Err(Error::Invalid)));
        assert!(s.unknown_suffix());
        assert!(matches!(s.close(3), Err(Error::Uncheckpointed)));
        s.record_gap(1, true).unwrap();
    }

    #[test]
    fn repeated_loss_bound_cannot_clear_new_failures() {
        let dir = fixture_dir(); let mut s = opened(dir.path());
        let e = envelope(1); s.append_event(&e).unwrap();
        s.record_gap(1, false).unwrap();
        let mut conflict = e; conflict.facts.actual_authorization = Authorization::Deny;
        assert!(matches!(s.append_event(&conflict), Err(Error::Conflict)));
        assert!(matches!(s.append_event(&conflict), Err(Error::Conflict)));
        assert!(matches!(s.record_gap(1, false), Err(Error::Invalid)));
        assert!(matches!(s.close(2), Err(Error::Uncheckpointed)));
        s.record_gap(1, true).unwrap();
        // A repeated/lost-response marker is a lower bound, never additive.
        s.record_gap(1, true).unwrap(); assert_eq!(s.known_loss_lower_bound(), 1);
        s.checkpoint(2, counts(1)).unwrap();
        s.acknowledge(e.id, Spool::event_digest(&e).unwrap(), id("receipt")).unwrap();
        s.close(2).unwrap(); drop(s);
        let s = Spool::open(dir.path(), limits()).unwrap(); assert!(s.unknown_suffix());
    }

    #[test]
    fn close_sync_failure_and_failed_recovery_sync_remain_uncertain() {
        let dir = fixture_dir(); let mut s = opened(dir.path());
        let e = envelope(1); s.append_event(&e).unwrap(); s.checkpoint(2, counts(1)).unwrap();
        s.acknowledge(e.id, Spool::event_digest(&e).unwrap(), id("receipt")).unwrap();
        s.fail_next_sync = true;
        assert!(matches!(s.close(2), Err(Error::Io(_))));
        assert_eq!(s.health(), Health::IoUncertain); drop(s);
        let mut s = Spool::open_internal(dir.path(), limits(), true, false).unwrap();
        assert_eq!(s.health(), Health::IoUncertain); assert!(s.unknown_suffix());
        assert!(matches!(s.begin(id("producer"), [2; 16]), Err(Error::Quarantined)));
        let mut seen = 0; s.replay(|_, _, _| seen += 1).unwrap(); assert_eq!(seen, 1);
        drop(s);
        // A successful recovery fsync establishes durable visible close bytes.
        let s = Spool::open(dir.path(), limits()).unwrap(); assert_eq!(s.health(), Health::Clean);
    }

    #[test]
    fn interrupted_directory_sync_is_retried_on_adoption() {
        let dir = fixture_dir();
        assert!(matches!(Spool::open_internal(dir.path(), limits(), false, true), Err(Error::Io(_))));
        let len = std::fs::metadata(dir.path().join("journal")).unwrap().len();
        assert_eq!(len, HEADER);
        assert!(matches!(Spool::open_internal(dir.path(), limits(), false, true), Err(Error::Io(_))));
        assert_eq!(std::fs::metadata(dir.path().join("journal")).unwrap().len(), len);
        let mut s = Spool::open(dir.path(), limits()).unwrap();
        assert_eq!(s.health(), Health::Unopened); s.begin(id("producer"), [1; 16]).unwrap();
        assert_eq!(s.health(), Health::Open);
    }

    #[test]
    fn refuses_allocated_or_sparse_lock_without_overwriting() {
        for sparse in [false, true] {
            let dir = fixture_dir(); let path = dir.path().join("lock");
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            if sparse { f.set_len(4096).unwrap(); } else { f.write_all(&[42]).unwrap(); }
            f.sync_all().unwrap(); let before = f.metadata().unwrap().len(); drop(f);
            assert!(matches!(Spool::open(dir.path(), limits()), Err(Error::Bounds)));
            assert_eq!(std::fs::metadata(path).unwrap().len(), before);
        }
    }

    #[test]
    fn receipt_ranges_preserve_holes_and_held_events() {
        let dir = fixture_dir(); let mut s = opened(dir.path());
        let events = [envelope(1), envelope(2), envelope(3)];
        for e in &events { s.append_event(e).unwrap(); }
        for e in [&events[0], &events[2]] {
            s.acknowledge(e.id, Spool::event_digest(e).unwrap(), id("receipt")).unwrap();
        }
        assert_eq!(s.acknowledged_ranges(id("producer"), [1; 16]), vec![(1, 1), (3, 3)]);
        let mut seen = Vec::new(); s.replay(|_, _, ack| seen.push(ack)).unwrap();
        assert_eq!(seen, vec![true, false, true]);
        s.acknowledge(events[1].id, Spool::event_digest(&events[1]).unwrap(), id("receipt")).unwrap();
        assert_eq!(s.acknowledged_ranges(id("producer"), [1; 16]), vec![(1, 3)]);
        s.checkpoint(4, counts(3)).unwrap(); s.close(4).unwrap(); drop(s);
        let mut s = Spool::open(dir.path(), limits()).unwrap();
        let mut seen = 0; s.replay(|_, _, _| seen += 1).unwrap(); assert_eq!(seen, 3);
    }

    #[test]
    fn persisted_known_loss_cannot_appear_clean() {
        let dir = fixture_dir(); let mut s = opened(dir.path());
        let e = envelope(1); s.append_event(&e).unwrap();
        let c = Counts { attempted: 2, admitted: 1, loss: 1, oversize: 1, invalid: 0 };
        s.checkpoint(3, c).unwrap();
        s.acknowledge(e.id, Spool::event_digest(&e).unwrap(), id("receipt")).unwrap();
        s.close(3).unwrap(); drop(s);
        let s = Spool::open(dir.path(), limits()).unwrap();
        assert_eq!(s.health(), Health::Incomplete);
        assert_eq!(s.known_loss_lower_bound(), 1);
    }

    #[test]
    fn unsynced_receipt_frame_is_never_a_clean_interval() {
        let dir = fixture_dir();
        let mut s = opened(dir.path()); let e = envelope(1);
        s.append_event(&e).unwrap(); s.checkpoint(2, counts(1)).unwrap(); drop(s);
        let d = Spool::event_digest(&e).unwrap();
        // Simulate a write reaching the file but no successful fsync/return.
        // This models visibility, not physical power-loss durability.
        let raw = frame(&Record::Ack { id: e.id, digest: d, receipt: id("receipt") }).unwrap();
        let mut f = std::fs::OpenOptions::new().write(true).open(dir.path().join("journal")).unwrap();
        f.seek(SeekFrom::End(0)).unwrap(); f.write_all(&raw).unwrap(); drop(f);
        let mut reopened = Spool::open(dir.path(), limits()).unwrap();
        assert!(reopened.unknown_suffix());
        let mut receipts = Vec::new(); reopened.replay(|_, _, ack| receipts.push(ack)).unwrap();
        assert_eq!(receipts, vec![true]);
        assert_eq!(reopened.acknowledge(e.id, d, id("receipt")).unwrap(), Append::Duplicate);
    }

    #[test]
    fn typed_reader_rejects_oversize_and_unknown_fields() {
        let fixture: Vec<serde_json::Value> = serde_json::from_str(include_str!("../../tests/fixtures/s5-shadow/cases.json")).unwrap();
        let mut wire = fixture[0]["wire"].clone();
        wire["id"]["producer"] = serde_json::json!("x".repeat(129));
        assert!(serde_json::from_value::<Envelope>(wire).is_err());
        let mut wire = fixture[0]["wire"].clone();
        wire["facts"]["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Envelope>(wire).is_err());
    }

    #[test]
    fn rejects_symlinks_bad_modes_and_concurrent_owners() {
        let dir = fixture_dir(); let s = opened(dir.path());
        assert!(matches!(Spool::open(dir.path(), limits()), Err(Error::Busy)));
        drop(s);
        let parent = fixture_dir(); let link = parent.path().join("link");
        symlink(dir.path(), &link).unwrap();
        assert!(Spool::open(&link, limits()).is_err());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(Spool::open(dir.path(), limits()), Err(Error::Invalid)));
        let other = fixture_dir();
        symlink("/dev/null", other.path().join("journal")).unwrap();
        assert!(Spool::open(other.path(), limits()).is_err());
    }
}
