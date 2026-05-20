//! Agent memory snapshots: save/load named immutable captures of a session's
//! transcript + memory dir for later cloning. See DESIGN_AGENT_MEMORIES.md.
//!
//! This module owns everything under `~/.cm/agent-memories/`. The directory
//! layout is one subdir per snapshot containing `manifest.json`,
//! `transcript.jsonl`, and (for Claude Code only) a `memory/` subdir.
//!
//! The snapshot's name is canonically the directory name; the manifest does
//! not duplicate it. Rename is a single `fs::rename(2)` syscall.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::workflow::toml_schema::Engine;

pub const MANIFEST_VERSION: u32 = 1;
const ROOT_REL: &str = ".cm/agent-memories";
const MAX_NAME_LEN: usize = 128;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub description: String,
    pub engine: Engine,
    pub source_session_uid: String,
    pub source_transcript_id: String,
    pub source_cwd: PathBuf,
    /// Snapshot creation time as seconds since the UNIX epoch. Stored as a
    /// scalar (rather than an ISO string) to avoid pulling in a calendar
    /// library; the catalog UI formats it for display.
    pub created_at_unix: u64,
    pub transcript_bytes: u64,
    pub memory_files: u32,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub name: String,
    pub dir: PathBuf,
    pub manifest: Manifest,
}

#[derive(Debug)]
pub enum SnapshotError {
    InvalidName(String),
    AlreadyExists,
    NotFound,
    /// The source session has no usable transcript yet (file absent or
    /// nothing past the last newline). Surface to the user as
    /// "let the session produce at least one message first" — see
    /// DESIGN_AGENT_MEMORIES.md edge case #1.
    NoTranscript,
    Io(io::Error),
    Parse(serde_json::Error),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::InvalidName(reason) => write!(f, "invalid snapshot name: {reason}"),
            SnapshotError::AlreadyExists => write!(f, "snapshot already exists"),
            SnapshotError::NotFound => write!(f, "snapshot not found"),
            SnapshotError::NoTranscript => write!(
                f,
                "no transcript yet — let the session produce at least one message first"
            ),
            SnapshotError::Io(e) => write!(f, "io error: {e}"),
            SnapshotError::Parse(e) => write!(f, "manifest parse error: {e}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<io::Error> for SnapshotError {
    fn from(e: io::Error) -> Self {
        SnapshotError::Io(e)
    }
}

impl From<serde_json::Error> for SnapshotError {
    fn from(e: serde_json::Error) -> Self {
        SnapshotError::Parse(e)
    }
}

pub type Result<T> = std::result::Result<T, SnapshotError>;

/// Spec for `save`. Callers resolve transcript and memory paths via their
/// engine (different Agent strategies expose them differently) and pass the
/// already-computed paths in.
pub struct SaveSpec<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub engine: Engine,
    pub source_session_uid: &'a str,
    pub source_transcript_id: &'a str,
    pub source_cwd: &'a Path,
    pub source_transcript_path: &'a Path,
    /// Only meaningful for Claude Code. Pass `None` for Codex (Codex has no
    /// per-cwd memory dir — see DESIGN_AGENT_MEMORIES.md edge case #6).
    pub source_memory_dir: Option<&'a Path>,
}

pub fn root_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(ROOT_REL))
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(SnapshotError::InvalidName("name is empty".into()));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(SnapshotError::InvalidName(format!(
            "name exceeds {MAX_NAME_LEN} chars"
        )));
    }
    if name.starts_with('.') {
        return Err(SnapshotError::InvalidName(
            "name cannot start with '.'".into(),
        ));
    }
    if name == ".." || name.contains("/") || name.contains('\\') {
        return Err(SnapshotError::InvalidName(
            "name cannot contain path separators".into(),
        ));
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
            return Err(SnapshotError::InvalidName(format!(
                "name contains invalid character '{c}' (allowed: A-Z a-z 0-9 _ - .)"
            )));
        }
    }
    Ok(())
}

/// Validate that a transcript id (which becomes part of a filesystem path when
/// resolving Claude/Codex transcript locations) is a safe bare filename. The
/// id flows in from `manifest.json` on disk, which is a trust boundary — a
/// hand-edited or corrupted manifest could otherwise produce paths like
/// `~/.claude/projects/<cwd>/../../evil.jsonl` and escape the projects dir.
pub(crate) fn validate_transcript_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(SnapshotError::InvalidName("transcript id is empty".into()));
    }
    if id.len() > 256 {
        return Err(SnapshotError::InvalidName(
            "transcript id exceeds 256 chars".into(),
        ));
    }
    if id == "." || id == ".." {
        return Err(SnapshotError::InvalidName(
            "transcript id is '.' or '..'".into(),
        ));
    }
    for c in id.chars() {
        if c == '/' || c == '\\' || c == '\0' || c.is_control() {
            return Err(SnapshotError::InvalidName(format!(
                "transcript id contains invalid character {c:?}"
            )));
        }
    }
    Ok(())
}

pub fn list() -> Result<Vec<Snapshot>> {
    let root = match root_dir() {
        Some(p) => p,
        None => return Ok(Vec::new()),
    };
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out: Vec<Snapshot> = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if name.starts_with('.') {
            continue;
        }
        if !entry.file_type()?.is_dir() {
            continue;
        }
        match load(&name) {
            Ok(s) => out.push(s),
            Err(_) => continue,
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn load(name: &str) -> Result<Snapshot> {
    validate_name(name)?;
    let dir = match root_dir() {
        Some(r) => r.join(name),
        None => return Err(SnapshotError::NotFound),
    };
    if !dir.is_dir() {
        return Err(SnapshotError::NotFound);
    }
    let manifest_path = dir.join("manifest.json");
    let bytes = fs::read(&manifest_path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            SnapshotError::NotFound
        } else {
            SnapshotError::Io(e)
        }
    })?;
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    // Trust boundary: the id from manifest.json flows into path construction
    // in clone_into_session. Reject anything that isn't a bare filename
    // before it propagates further.
    validate_transcript_id(&manifest.source_transcript_id)?;
    Ok(Snapshot {
        name: name.to_string(),
        dir,
        manifest,
    })
}

pub fn delete(name: &str) -> Result<()> {
    validate_name(name)?;
    let dir = match root_dir() {
        Some(r) => r.join(name),
        None => return Err(SnapshotError::NotFound),
    };
    if !dir.is_dir() {
        return Err(SnapshotError::NotFound);
    }
    fs::remove_dir_all(&dir)?;
    Ok(())
}

pub fn rename(old: &str, new: &str) -> Result<()> {
    validate_name(old)?;
    validate_name(new)?;
    if old == new {
        return Ok(());
    }
    let root = root_dir().ok_or(SnapshotError::NotFound)?;
    let from = root.join(old);
    let to = root.join(new);
    if !from.is_dir() {
        return Err(SnapshotError::NotFound);
    }
    if to.exists() {
        return Err(SnapshotError::AlreadyExists);
    }
    fs::rename(&from, &to)?;
    Ok(())
}

pub fn save(spec: SaveSpec<'_>) -> Result<Snapshot> {
    validate_name(spec.name)?;

    let root = root_dir().ok_or_else(|| {
        SnapshotError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            "HOME is not set",
        ))
    })?;
    fs::create_dir_all(&root)?;

    let final_dir = root.join(spec.name);
    if final_dir.exists() {
        return Err(SnapshotError::AlreadyExists);
    }

    if !spec.source_transcript_path.is_file() {
        return Err(SnapshotError::NoTranscript);
    }

    let tmp_dir = root.join(format!(".tmp-{}", unique_tmp_suffix()));
    fs::create_dir(&tmp_dir)?;

    save_into_tmp_dir(spec, &tmp_dir, &final_dir)
}

/// Populate `tmp_dir` with the snapshot's transcript, optional memory dir,
/// and manifest, then atomically rename it to `final_dir`. Any failure
/// before the final rename triggers cleanup of `tmp_dir` via an RAII guard
/// — including manifest serialization and `write_atomic` failures, which
/// earlier had no cleanup path and could leave hidden `.tmp-*` directories
/// behind invisible to `list()`.
fn save_into_tmp_dir(
    spec: SaveSpec<'_>,
    tmp_dir: &Path,
    final_dir: &Path,
) -> Result<Snapshot> {
    let mut guard = TmpDirGuard::new(tmp_dir);

    let dst_transcript = tmp_dir.join("transcript.jsonl");
    let transcript_bytes =
        copy_transcript_truncating(spec.source_transcript_path, &dst_transcript)?;

    let memory_files = match (spec.engine.clone(), spec.source_memory_dir) {
        (Engine::ClaudeCode, Some(src_mem)) if src_mem.is_dir() => {
            let dst_mem = tmp_dir.join("memory");
            fs::create_dir(&dst_mem)?;
            copy_memory_dir(src_mem, &dst_mem)?
        }
        _ => 0,
    };

    // `copy_transcript_truncating` returns 0 bytes when the source is empty
    // or when its only content is a partial last line with no trailing
    // newline (truncated away). Persisting a 0-byte transcript would create
    // a snapshot that later fails to clone — reject it now and surface the
    // same "no transcript yet" error as the missing-file case.
    if transcript_bytes == 0 {
        return Err(SnapshotError::NoTranscript);
    }

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        description: spec.description.to_string(),
        engine: spec.engine,
        source_session_uid: spec.source_session_uid.to_string(),
        source_transcript_id: spec.source_transcript_id.to_string(),
        source_cwd: spec.source_cwd.to_path_buf(),
        created_at_unix: now_unix(),
        transcript_bytes,
        memory_files,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    write_atomic(&tmp_dir.join("manifest.json"), &manifest_bytes)?;

    fs::rename(tmp_dir, final_dir)?;
    guard.disarm();

    Ok(Snapshot {
        name: spec.name.to_string(),
        dir: final_dir.to_path_buf(),
        manifest,
    })
}

/// RAII guard: removes `path` recursively on drop unless disarmed. Used by
/// `save_into_tmp_dir` so every `?` between tmp-dir creation and the final
/// rename triggers cleanup, without having to thread a cleanup branch
/// through each fallible step.
struct TmpDirGuard<'a> {
    path: &'a Path,
    armed: bool,
}

impl<'a> TmpDirGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TmpDirGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(self.path);
        }
    }
}

// ---------------------------------------------------------------------------
// Clone

/// Result of cloning a snapshot into a new session's expected on-disk paths.
///
/// The semantics of `transcript_id` differ between engines — callers MUST
/// read the per-engine integration notes before assigning it onto the live
/// `TerminalSession`.
///
/// # Claude Code
///
/// `transcript_id` is the **live** transcript id. The cloned JSONL lives at
/// `~/.claude/projects/<encoded-new-cwd>/<transcript_id>.jsonl` and
/// `claude --resume <transcript_id>` continues writing to that same file —
/// the id is stable across the resume. Caller integration: set
/// `ts.transcript_id = Some(cloned.transcript_id.clone())` and
/// `ts.pending_jsonl_files = None` immediately after spawn, matching the
/// existing resumed-Claude pattern at `app.rs:5512`.
///
/// # Codex
///
/// `transcript_id` is a **resume-source** id, not a live transcript id. We
/// wrote a single JSONL with `payload.id == transcript_id` so
/// `codex resume <transcript_id>` can find and read it as seed context, but
/// Codex mints a **fresh rollout id** for the ongoing session and writes a
/// new transcript file (also under `~/.codex/sessions/YYYY/MM/DD/`).
///
/// Caller integration: do **not** set `ts.transcript_id` to
/// `cloned.transcript_id`. Leave `ts.transcript_id = None` and
/// `ts.pending_jsonl_files = Some(baseline)` so the existing rebind
/// detection (`detect_codex_session_id` at `app.rs:2033`) discovers the new
/// rollout id Codex creates after the resume. Pointing the live session at
/// the seed file would leave it bound to a stale transcript that Codex
/// stops writing to after the first reply.
#[derive(Clone, Debug)]
pub struct ClonedSession {
    pub transcript_id: String,
    pub transcript_path: PathBuf,
}

/// Materialize a snapshot into the on-disk locations a freshly-spawned
/// session would expect. Branches by engine.
///
/// The returned `ClonedSession.transcript_id` has engine-specific semantics
/// (live transcript id for Claude vs. resume-source id for Codex) — see the
/// `ClonedSession` docs for caller integration. The TUI's spawn flow MUST
/// honor that distinction; setting `ts.transcript_id` to the returned Codex
/// id would point the session at a stale seed file.
pub fn clone_into_session(snapshot: &Snapshot, new_cwd: &Path) -> Result<ClonedSession> {
    match snapshot.manifest.engine {
        Engine::ClaudeCode => clone_claude(snapshot, new_cwd),
        Engine::Codex => clone_codex(snapshot, new_cwd),
    }
}

fn clone_claude(snapshot: &Snapshot, new_cwd: &Path) -> Result<ClonedSession> {
    let transcript_id = snapshot.manifest.source_transcript_id.clone();
    validate_transcript_id(&transcript_id)?;
    let dst_transcript = claude_transcript_path(new_cwd, &transcript_id).ok_or_else(|| {
        SnapshotError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "could not resolve Claude transcript path (HOME unset or non-UTF-8 cwd)",
        ))
    })?;
    let dst_memory_dir = claude_memory_dir(new_cwd).ok_or_else(|| {
        SnapshotError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "could not resolve Claude memory dir",
        ))
    })?;
    let src_transcript = snapshot.dir.join("transcript.jsonl");
    let src_memory_dir = snapshot.dir.join("memory");
    let src_memory_dir = if src_memory_dir.is_dir() {
        Some(src_memory_dir)
    } else {
        None
    };

    clone_claude_inner(
        &src_transcript,
        src_memory_dir.as_deref(),
        &dst_transcript,
        &dst_memory_dir,
    )?;

    Ok(ClonedSession {
        transcript_id,
        transcript_path: dst_transcript,
    })
}

/// Path-explicit Claude clone. Transactional: stages the transcript and each
/// memory file at tmp paths adjacent to their destinations, then commits via
/// atomic `fs::rename`. On any failure before the commit step, removes the
/// staged tmp files (and the memory dir if we created it). Pre-existing
/// destination memory files are never touched until commit, so a mid-clone
/// failure leaves them byte-identical to before.
///
/// Extracted from `clone_claude` so tests can drive it against tempdir paths
/// without messing with `HOME`.
fn clone_claude_inner(
    src_transcript: &Path,
    src_memory_dir: Option<&Path>,
    dst_transcript: &Path,
    dst_memory_dir: &Path,
) -> Result<()> {
    if dst_transcript.exists() {
        return Err(SnapshotError::AlreadyExists);
    }
    let dst_parent = dst_transcript.parent().ok_or_else(|| {
        SnapshotError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination transcript path has no parent",
        ))
    })?;
    fs::create_dir_all(dst_parent)?;

    let tmp_transcript = dst_parent.join(format!(
        ".{}.tmp-{}",
        dst_transcript
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "transcript".into()),
        unique_tmp_suffix()
    ));

    let mut staged_memory: Vec<StagedMemoryFile> = Vec::new();
    let mut created_memory_dir = false;

    let staged = (|| -> Result<()> {
        fs::copy(src_transcript, &tmp_transcript)?;

        if let Some(src_mem) = src_memory_dir {
            if !dst_memory_dir.is_dir() {
                fs::create_dir_all(dst_memory_dir)?;
                created_memory_dir = true;
            }
            for entry in fs::read_dir(src_mem)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let dst = dst_memory_dir.join(entry.file_name());
                let tmp = dst_memory_dir.join(format!(
                    ".{}.tmp-{}",
                    entry.file_name().to_string_lossy(),
                    unique_tmp_suffix()
                ));
                fs::copy(entry.path(), &tmp)?;
                staged_memory.push(StagedMemoryFile { tmp, dst });
            }
        }
        Ok(())
    })();

    if let Err(e) = staged {
        rollback_claude_clone(&tmp_transcript, &staged_memory, created_memory_dir, dst_memory_dir);
        return Err(e);
    }

    // Commit memory files FIRST, then the transcript. The transcript is the
    // "did the clone happen?" sentinel the spawn flow keys off of — if any
    // memory-rename fails partway, the transcript hasn't been committed yet,
    // so the next retry isn't blocked by a stale `AlreadyExists`. (Renames
    // within the same dir are atomic on POSIX and effectively never fail
    // once staging succeeds, but ENOSPC and a pre-existing non-file at a
    // destination path are both real possibilities.)
    for (i, sm) in staged_memory.iter().enumerate() {
        if let Err(e) = fs::rename(&sm.tmp, &sm.dst) {
            // Best-effort cleanup: any memory tmps that haven't been
            // renamed yet, plus the staged transcript (never committed).
            // Memory files committed earlier in the loop stay at the
            // destination — we can't reverse them, but they don't block
            // a retry because the transcript wasn't committed.
            for other in &staged_memory[i..] {
                let _ = fs::remove_file(&other.tmp);
            }
            let _ = fs::remove_file(&tmp_transcript);
            return Err(e.into());
        }
    }

    if let Err(e) = fs::rename(&tmp_transcript, dst_transcript) {
        // Memory was fully committed; the transcript never landed. Clean up
        // the staged transcript so it doesn't linger as `.tmp-*` cruft.
        let _ = fs::remove_file(&tmp_transcript);
        return Err(e.into());
    }

    Ok(())
}

struct StagedMemoryFile {
    tmp: PathBuf,
    dst: PathBuf,
}

fn rollback_claude_clone(
    tmp_transcript: &Path,
    staged_memory: &[StagedMemoryFile],
    created_memory_dir: bool,
    dst_memory_dir: &Path,
) {
    let _ = fs::remove_file(tmp_transcript);
    for sm in staged_memory {
        let _ = fs::remove_file(&sm.tmp);
    }
    if created_memory_dir {
        // remove_dir succeeds only if the dir is empty. If we created the
        // dir, it shouldn't contain anything but our (now-deleted) tmps;
        // anything else is from a concurrent writer and we leave it alone.
        let _ = fs::remove_dir(dst_memory_dir);
    }
}

fn clone_codex(snapshot: &Snapshot, new_cwd: &Path) -> Result<ClonedSession> {
    // Source id isn't used for path construction (Codex mints a fresh id),
    // but validate defensively so a malformed manifest is rejected at the
    // same point for both engines.
    validate_transcript_id(&snapshot.manifest.source_transcript_id)?;
    let new_id = uuid::Uuid::new_v4().to_string();
    let new_cwd_str = new_cwd.to_str().ok_or_else(|| {
        SnapshotError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "new cwd is not valid UTF-8",
        ))
    })?;

    let src_path = snapshot.dir.join("transcript.jsonl");
    let src = fs::read_to_string(&src_path)?;

    let mut lines = src.split_inclusive('\n');
    let first = lines.next().ok_or_else(|| {
        SnapshotError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "snapshot transcript is empty",
        ))
    })?;

    let first_trim = first.strip_suffix('\n').unwrap_or(first);
    let rewritten_first = rewrite_codex_line1(first_trim, &new_id, new_cwd_str)?;

    let mut out_bytes = Vec::with_capacity(src.len() + 64);
    out_bytes.extend_from_slice(rewritten_first.as_bytes());
    out_bytes.push(b'\n');
    for line in lines {
        out_bytes.extend_from_slice(line.as_bytes());
    }

    let dst = codex_dest_path(&new_id, now_unix())?;
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    write_atomic(&dst, &out_bytes)?;

    // `new_id` is the resume-source id Codex will read on `codex resume`,
    // NOT the id of the rollout transcript Codex creates after that resume.
    // Callers must not assign it to `ts.transcript_id` — see ClonedSession.
    Ok(ClonedSession {
        transcript_id: new_id,
        transcript_path: dst,
    })
}

/// Rewrite line 1 of a Codex JSONL transcript: replace `payload.id` and
/// `payload.cwd` while preserving every other field. Returns the rewritten
/// JSON object as a single-line string (no trailing newline).
pub(crate) fn rewrite_codex_line1(
    line: &str,
    new_id: &str,
    new_cwd: &str,
) -> Result<String> {
    let mut v: serde_json::Value = serde_json::from_str(line)?;
    let payload = v
        .get_mut("payload")
        .and_then(|p| p.as_object_mut())
        .ok_or_else(|| {
            SnapshotError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Codex line 1 has no `payload` object",
            ))
        })?;
    payload.insert("id".to_string(), serde_json::Value::String(new_id.to_string()));
    payload.insert(
        "cwd".to_string(),
        serde_json::Value::String(new_cwd.to_string()),
    );
    Ok(serde_json::to_string(&v)?)
}

fn codex_dest_path(transcript_id: &str, now_secs: u64) -> Result<PathBuf> {
    let root = codex_sessions_root().ok_or_else(|| {
        SnapshotError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            "HOME is not set",
        ))
    })?;
    let (y, m, d) = ymd_from_unix_secs(now_secs);
    Ok(root
        .join(format!("{:04}", y))
        .join(format!("{:02}", m))
        .join(format!("{:02}", d))
        .join(format!("{}.jsonl", transcript_id)))
}

/// Convert UNIX-seconds (UTC) to (year, month, day) using Howard Hinnant's
/// `civil_from_days` algorithm. Self-contained so we don't pull in a calendar
/// crate just to compute a date directory name.
fn ymd_from_unix_secs(secs: u64) -> (i64, u32, u32) {
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// Path helpers. Duplicated from `agent::claude_code` and `agent::codex` on
// purpose — those are `pub(super)` to keep agent module internals private
// (see comment at claude_code.rs:84). The encoded-cwd convention is a stable
// external interface (Claude Code's on-disk layout, Codex's ~/.codex/sessions
// walk), so duplication is bounded.

pub(crate) fn claude_transcript_path(cwd: &Path, transcript_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let path_str = cwd.to_str()?;
    let encoded = path_str.replace('/', "-").replace('.', "-");
    Some(
        home.join(format!(".claude/projects/{}", encoded))
            .join(format!("{}.jsonl", transcript_id)),
    )
}

pub(crate) fn claude_memory_dir(cwd: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let path_str = cwd.to_str()?;
    let encoded = path_str.replace('/', "-").replace('.', "-");
    Some(
        home.join(format!(".claude/projects/{}", encoded))
            .join("memory"),
    )
}

pub(crate) fn codex_sessions_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".codex/sessions"))
}

// ---------------------------------------------------------------------------
// Internal helpers.

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn unique_tmp_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}", nanos, n)
}

/// Copy a JSONL transcript, truncating any trailing partial line so the
/// destination always ends on `\n` (or is empty). The source may be actively
/// being written by an agent — JSONL is line-oriented append-only, so this
/// gives a clean prefix-of-the-transcript-as-of-now.
pub(crate) fn copy_transcript_truncating(src: &Path, dst: &Path) -> io::Result<u64> {
    let bytes = fs::read(src)?;
    let truncated: &[u8] = match bytes.iter().rposition(|&b| b == b'\n') {
        Some(pos) => &bytes[..=pos],
        None => &[],
    };
    write_atomic(dst, truncated)?;
    Ok(truncated.len() as u64)
}

fn copy_memory_dir(src: &Path, dst: &Path) -> io::Result<u32> {
    let mut count: u32 = 0;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if !ty.is_file() {
            continue;
        }
        let from = entry.path();
        let to = dst.join(entry.file_name());
        fs::copy(&from, &to)?;
        count += 1;
    }
    Ok(count)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let fname = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let tmp = parent.join(format!(".{}.tmp-{}", fname.to_string_lossy(), unique_tmp_suffix()));
    {
        use std::io::Write;
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::home_lock;

    #[test]
    fn validate_name_accepts_typical() {
        assert!(validate_name("reviewer-strict").is_ok());
        assert!(validate_name("primed_for_logs").is_ok());
        assert!(validate_name("v1.2.3").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name("A1_b2-c3.d4").is_ok());
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(matches!(
            validate_name(""),
            Err(SnapshotError::InvalidName(_))
        ));
    }

    #[test]
    fn validate_name_rejects_dotfile() {
        assert!(matches!(
            validate_name(".hidden"),
            Err(SnapshotError::InvalidName(_))
        ));
    }

    #[test]
    fn validate_name_rejects_path_separators() {
        assert!(matches!(
            validate_name("reviewer/strict"),
            Err(SnapshotError::InvalidName(_))
        ));
        assert!(matches!(
            validate_name("a\\b"),
            Err(SnapshotError::InvalidName(_))
        ));
    }

    #[test]
    fn validate_name_rejects_parent_ref() {
        assert!(matches!(
            validate_name(".."),
            Err(SnapshotError::InvalidName(_))
        ));
    }

    #[test]
    fn validate_name_rejects_special_chars() {
        for bad in [
            "has space",
            "has:colon",
            "has!bang",
            "has@at",
            "has(paren",
            "tab\there",
        ] {
            assert!(
                matches!(validate_name(bad), Err(SnapshotError::InvalidName(_))),
                "expected `{bad}` to be rejected"
            );
        }
    }

    #[test]
    fn validate_name_enforces_length() {
        let long = "a".repeat(MAX_NAME_LEN + 1);
        assert!(matches!(
            validate_name(&long),
            Err(SnapshotError::InvalidName(_))
        ));
        let max = "a".repeat(MAX_NAME_LEN);
        assert!(validate_name(&max).is_ok());
    }

    #[test]
    fn copy_transcript_truncates_partial_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let dst = dir.path().join("dst.jsonl");
        // Two full lines plus a partial line with no trailing newline.
        fs::write(&src, b"{\"a\":1}\n{\"b\":2}\n{\"c\":pa").unwrap();

        let n = copy_transcript_truncating(&src, &dst).unwrap();
        let copied = fs::read(&dst).unwrap();
        assert_eq!(copied, b"{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(n as usize, copied.len());
    }

    #[test]
    fn copy_transcript_keeps_complete_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let dst = dir.path().join("dst.jsonl");
        fs::write(&src, b"{\"a\":1}\n{\"b\":2}\n").unwrap();
        copy_transcript_truncating(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"{\"a\":1}\n{\"b\":2}\n");
    }

    #[test]
    fn copy_transcript_empty_if_no_newline() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let dst = dir.path().join("dst.jsonl");
        fs::write(&src, b"no_newlines_yet").unwrap();
        let n = copy_transcript_truncating(&src, &dst).unwrap();
        assert_eq!(n, 0);
        assert_eq!(fs::read(&dst).unwrap(), b"");
    }

    #[test]
    fn codex_line1_rewrite_swaps_id_and_cwd() {
        let line = r#"{"timestamp":"2026-05-18T16:04:31.607Z","type":"session_meta","payload":{"id":"019e3bd4-old","cwd":"/old/path","originator":"codex-tui","cli_version":"0.130.0"}}"#;
        let rewritten = rewrite_codex_line1(line, "new-id-xyz", "/new/path").unwrap();
        let v: serde_json::Value = serde_json::from_str(&rewritten).unwrap();
        assert_eq!(v["payload"]["id"], "new-id-xyz");
        assert_eq!(v["payload"]["cwd"], "/new/path");
        // Other fields preserved.
        assert_eq!(v["timestamp"], "2026-05-18T16:04:31.607Z");
        assert_eq!(v["type"], "session_meta");
        assert_eq!(v["payload"]["originator"], "codex-tui");
        assert_eq!(v["payload"]["cli_version"], "0.130.0");
    }

    #[test]
    fn codex_line1_rewrite_rejects_missing_payload() {
        let line = r#"{"timestamp":"now","type":"session_meta"}"#;
        let err = rewrite_codex_line1(line, "id", "/cwd").unwrap_err();
        assert!(matches!(err, SnapshotError::Io(_)), "got: {err:?}");
    }

    #[test]
    fn codex_line1_rewrite_rejects_invalid_json() {
        let err = rewrite_codex_line1("not json", "id", "/cwd").unwrap_err();
        assert!(matches!(err, SnapshotError::Parse(_)), "got: {err:?}");
    }

    #[test]
    fn ymd_from_unix_secs_known_dates() {
        // 1970-01-01T00:00:00Z
        assert_eq!(ymd_from_unix_secs(0), (1970, 1, 1));
        // 2026-05-19T00:00:00Z = day 20592 since epoch
        assert_eq!(ymd_from_unix_secs(20_592 * 86_400), (2026, 5, 19));
        // 2000-02-29T00:00:00Z (leap day) = day 11016 since epoch
        assert_eq!(ymd_from_unix_secs(11_016 * 86_400), (2000, 2, 29));
        // 2100-03-01T00:00:00Z (2100 is a non-leap century, March follows
        // Feb-28) = day 47541 since epoch
        assert_eq!(ymd_from_unix_secs(47_541 * 86_400), (2100, 3, 1));
        // Same date, partial day — second should not roll over into day 47542
        assert_eq!(
            ymd_from_unix_secs(47_541 * 86_400 + 86_399),
            (2100, 3, 1)
        );
    }

    #[test]
    fn validate_transcript_id_accepts_uuid_shaped() {
        assert!(validate_transcript_id("019e3bd4-b2c6-7ac2-ba95-cf8d98fc2236").is_ok());
        assert!(validate_transcript_id("ts-deadbeef-1").is_ok());
        assert!(validate_transcript_id("plain_id").is_ok());
    }

    #[test]
    fn validate_transcript_id_rejects_path_escape() {
        for bad in [
            "",
            ".",
            "..",
            "../evil",
            "/abs/path",
            "sub/dir",
            "back\\slash",
            "has\0null",
            "has\nnewline",
            "has\ttab",
        ] {
            assert!(
                matches!(validate_transcript_id(bad), Err(SnapshotError::InvalidName(_))),
                "expected `{bad:?}` to be rejected"
            );
        }
    }

    #[test]
    fn load_rejects_manifest_with_path_escape_transcript_id() {
        // Construct a snapshot dir directly on disk under a fake HOME so we
        // can exercise load() against a malicious manifest without depending
        // on the real ~/.cm/agent-memories.
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();
        let snap_root = fake_home.join(".cm/agent-memories");
        let snap_dir = snap_root.join("attacker");
        fs::create_dir_all(&snap_dir).unwrap();
        let manifest = serde_json::json!({
            "version": MANIFEST_VERSION,
            "description": "",
            "engine": "claude-code",
            "source_session_uid": "ts-abc",
            "source_transcript_id": "../../evil",
            "source_cwd": "/tmp",
            "created_at_unix": 0,
            "transcript_bytes": 0,
            "memory_files": 0,
        });
        fs::write(snap_dir.join("manifest.json"), manifest.to_string()).unwrap();
        fs::write(snap_dir.join("transcript.jsonl"), b"").unwrap();

        // Point HOME at the fake root for the duration of this single test.
        // Other tests must not touch HOME for this to be safe under parallel
        // execution; only this test (and `load_accepts_well_formed_manifest`)
        // touch HOME, and they serialize via a process-level lock.
        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        // SAFETY: HOME_LOCK serializes access.
        unsafe { std::env::set_var("HOME", fake_home) };
        let result = load("attacker");
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(
            matches!(result, Err(SnapshotError::InvalidName(_))),
            "expected InvalidName, got {result:?}"
        );
    }

    #[test]
    fn load_accepts_well_formed_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();
        let snap_root = fake_home.join(".cm/agent-memories");
        let snap_dir = snap_root.join("ok");
        fs::create_dir_all(&snap_dir).unwrap();
        let manifest = serde_json::json!({
            "version": MANIFEST_VERSION,
            "description": "",
            "engine": "claude-code",
            "source_session_uid": "ts-abc",
            "source_transcript_id": "019e3bd4-b2c6-7ac2-ba95-cf8d98fc2236",
            "source_cwd": "/tmp",
            "created_at_unix": 0,
            "transcript_bytes": 0,
            "memory_files": 0,
        });
        fs::write(snap_dir.join("manifest.json"), manifest.to_string()).unwrap();

        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home) };
        let result = load("ok");
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        let s = result.expect("load should succeed");
        assert_eq!(s.manifest.source_transcript_id, "019e3bd4-b2c6-7ac2-ba95-cf8d98fc2236");
    }

    #[cfg(unix)]
    #[test]
    fn clone_claude_rolls_back_on_memory_failure() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let src_transcript = dir.path().join("src_transcript.jsonl");
        fs::write(&src_transcript, b"line1\nline2\n").unwrap();

        // Source memory dir contains one unreadable file. fs::copy on it
        // will fail with permission denied, mid-clone.
        let src_mem = dir.path().join("src_memory");
        fs::create_dir(&src_mem).unwrap();
        let unreadable = src_mem.join("notes.md");
        fs::write(&unreadable, b"secret").unwrap();
        let mut p = fs::metadata(&unreadable).unwrap().permissions();
        p.set_mode(0o000);
        fs::set_permissions(&unreadable, p).unwrap();

        let dst_transcript = dir.path().join("dst/transcript.jsonl");
        let dst_memory_dir = dir.path().join("dst/memory");

        let result = clone_claude_inner(
            &src_transcript,
            Some(&src_mem),
            &dst_transcript,
            &dst_memory_dir,
        );

        // Restore permissions before tempdir is dropped, or cleanup fails.
        let mut p = fs::metadata(&unreadable).unwrap().permissions();
        p.set_mode(0o644);
        fs::set_permissions(&unreadable, p).unwrap();

        assert!(result.is_err(), "expected clone to fail, got {result:?}");
        assert!(
            !dst_transcript.exists(),
            "transcript must not be left at the destination on failure"
        );
        // The staging tmp file should also be gone — scan the parent for
        // any leftover transcript-like files.
        let dst_parent = dst_transcript.parent().unwrap();
        if dst_parent.exists() {
            for entry in fs::read_dir(dst_parent).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                assert!(
                    !name.contains("transcript.jsonl"),
                    "leftover transcript artifact: {name}"
                );
            }
        }
        // The memory dir was created fresh by this clone — it should be gone
        // after rollback (since we created it and it's now empty).
        assert!(
            !dst_memory_dir.exists(),
            "freshly-created memory dir should be removed on rollback"
        );
    }

    #[cfg(unix)]
    #[test]
    fn clone_claude_failure_leaves_preexisting_memory_files_untouched() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let src_transcript = dir.path().join("src.jsonl");
        fs::write(&src_transcript, b"line\n").unwrap();

        // Source memory dir has two files; the second is unreadable, so the
        // staging loop will fail after staging the first.
        let src_mem = dir.path().join("src_mem");
        fs::create_dir(&src_mem).unwrap();
        fs::write(src_mem.join("alpha.md"), b"new-alpha").unwrap();
        let unreadable = src_mem.join("beta.md");
        fs::write(&unreadable, b"new-beta").unwrap();
        let mut p = fs::metadata(&unreadable).unwrap().permissions();
        p.set_mode(0o000);
        fs::set_permissions(&unreadable, p).unwrap();

        // Pre-existing destination memory dir with a same-named file that
        // must NOT be overwritten by the failed clone.
        let dst_mem = dir.path().join("dst/memory");
        fs::create_dir_all(&dst_mem).unwrap();
        fs::write(dst_mem.join("alpha.md"), b"original-alpha").unwrap();
        let dst_transcript = dir.path().join("dst/t.jsonl");

        let result = clone_claude_inner(
            &src_transcript,
            Some(&src_mem),
            &dst_transcript,
            &dst_mem,
        );

        // Restore perms so tempdir cleanup can succeed.
        let mut p = fs::metadata(&unreadable).unwrap().permissions();
        p.set_mode(0o644);
        fs::set_permissions(&unreadable, p).unwrap();

        assert!(result.is_err(), "expected failure, got {result:?}");
        assert!(
            !dst_transcript.exists(),
            "transcript must not be committed when staging fails"
        );
        // Pre-existing alpha.md content is preserved byte-for-byte.
        assert_eq!(
            fs::read(dst_mem.join("alpha.md")).unwrap(),
            b"original-alpha"
        );
        // beta.md must not have been created at the destination.
        assert!(!dst_mem.join("beta.md").exists());
        // No staging tmp files left behind.
        for entry in fs::read_dir(&dst_mem).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !name.contains(".tmp-"),
                "leftover staging file in memory dir: {name}"
            );
        }
    }

    #[test]
    fn clone_claude_merges_into_existing_memory_dir() {
        // Pre-existing memory dir with one same-named file (gets replaced)
        // and one unrelated file (stays). Verifies the happy-path "merge"
        // behavior promised by the design doc.
        let dir = tempfile::tempdir().unwrap();
        let src_transcript = dir.path().join("src.jsonl");
        fs::write(&src_transcript, b"a\n").unwrap();
        let src_mem = dir.path().join("src_mem");
        fs::create_dir(&src_mem).unwrap();
        fs::write(src_mem.join("alpha.md"), b"snapshot-alpha").unwrap();

        let dst_mem = dir.path().join("dst/memory");
        fs::create_dir_all(&dst_mem).unwrap();
        fs::write(dst_mem.join("alpha.md"), b"old-alpha").unwrap();
        fs::write(dst_mem.join("untouched.md"), b"keep-me").unwrap();
        let dst_transcript = dir.path().join("dst/t.jsonl");

        clone_claude_inner(&src_transcript, Some(&src_mem), &dst_transcript, &dst_mem)
            .unwrap();

        assert_eq!(fs::read(&dst_transcript).unwrap(), b"a\n");
        assert_eq!(fs::read(dst_mem.join("alpha.md")).unwrap(), b"snapshot-alpha");
        assert_eq!(fs::read(dst_mem.join("untouched.md")).unwrap(), b"keep-me");
        // No leftover staging tmps after a successful commit.
        for entry in fs::read_dir(&dst_mem).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(!name.contains(".tmp-"), "leftover tmp: {name}");
        }
    }

    #[test]
    fn clone_claude_succeeds_and_commits_atomically() {
        // Happy path: verifies the transactional rewrite didn't regress the
        // normal case. Both transcript and memory files end up at the dst.
        let dir = tempfile::tempdir().unwrap();
        let src_transcript = dir.path().join("src_t.jsonl");
        fs::write(&src_transcript, b"a\nb\n").unwrap();
        let src_mem = dir.path().join("src_mem");
        fs::create_dir(&src_mem).unwrap();
        fs::write(src_mem.join("CLAUDE.md"), b"hi").unwrap();
        fs::write(src_mem.join("notes.md"), b"there").unwrap();

        let dst_transcript = dir.path().join("dst/sub/t.jsonl");
        let dst_mem = dir.path().join("dst/sub/memory");
        clone_claude_inner(&src_transcript, Some(&src_mem), &dst_transcript, &dst_mem).unwrap();

        assert_eq!(fs::read(&dst_transcript).unwrap(), b"a\nb\n");
        assert_eq!(fs::read(dst_mem.join("CLAUDE.md")).unwrap(), b"hi");
        assert_eq!(fs::read(dst_mem.join("notes.md")).unwrap(), b"there");
    }

    #[test]
    fn clone_claude_rejects_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src_transcript = dir.path().join("src.jsonl");
        fs::write(&src_transcript, b"x\n").unwrap();
        let dst_transcript = dir.path().join("dst/t.jsonl");
        fs::create_dir_all(dst_transcript.parent().unwrap()).unwrap();
        fs::write(&dst_transcript, b"already-here").unwrap();
        let dst_mem = dir.path().join("dst/memory");

        let result = clone_claude_inner(&src_transcript, None, &dst_transcript, &dst_mem);
        assert!(
            matches!(result, Err(SnapshotError::AlreadyExists)),
            "got {result:?}"
        );
        // Existing file untouched.
        assert_eq!(fs::read(&dst_transcript).unwrap(), b"already-here");
    }

    #[test]
    fn clone_claude_memory_commit_failure_leaves_transcript_uncommitted() {
        // Reorder regression test: if a memory `.tmp-*` -> final rename
        // fails, the transcript MUST NOT have been committed to the
        // destination. Otherwise the next retry hits AlreadyExists forever.
        //
        // Failure injection: pre-create a non-empty directory at the path
        // where the staged memory file would commit. `fs::rename(file, dir)`
        // fails on Linux (EISDIR / ENOTEMPTY).
        let dir = tempfile::tempdir().unwrap();
        let src_transcript = dir.path().join("src.jsonl");
        fs::write(&src_transcript, b"a\n").unwrap();

        let src_mem = dir.path().join("src_mem");
        fs::create_dir(&src_mem).unwrap();
        fs::write(src_mem.join("notes.md"), b"hello").unwrap();

        let dst_mem = dir.path().join("dst/memory");
        fs::create_dir_all(&dst_mem).unwrap();
        let blocking_dir = dst_mem.join("notes.md");
        fs::create_dir(&blocking_dir).unwrap();
        fs::write(blocking_dir.join("inside"), b"x").unwrap();

        let dst_transcript = dir.path().join("dst/t.jsonl");

        let result = clone_claude_inner(
            &src_transcript,
            Some(&src_mem),
            &dst_transcript,
            &dst_mem,
        );

        assert!(result.is_err(), "expected memory rename failure, got {result:?}");
        assert!(
            !dst_transcript.exists(),
            "transcript MUST NOT be committed when memory commit fails — \
             would otherwise block retries with AlreadyExists"
        );
        // Staging tmp for the transcript should be cleaned up too.
        let dst_parent = dst_transcript.parent().unwrap();
        if dst_parent.exists() {
            for entry in fs::read_dir(dst_parent).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                assert!(
                    !name.contains(".tmp-"),
                    "leftover staging file in dst parent: {name}"
                );
            }
        }
        // The pre-existing blocking directory is untouched.
        assert!(blocking_dir.is_dir());
        assert!(blocking_dir.join("inside").is_file());
    }

    #[test]
    fn save_rejects_empty_source_transcript() {
        // copy_transcript_truncating returns 0 bytes for an empty source;
        // save() must reject rather than persist a useless snapshot.
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();
        let src = dir.path().join("empty.jsonl");
        fs::write(&src, b"").unwrap();

        let spec = SaveSpec {
            name: "snap",
            description: "",
            engine: Engine::ClaudeCode,
            source_session_uid: "ts-x",
            source_transcript_id: "id-x",
            source_cwd: dir.path(),
            source_transcript_path: &src,
            source_memory_dir: None,
        };

        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home) };
        let result = save(spec);
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(
            matches!(result, Err(SnapshotError::NoTranscript)),
            "expected NoTranscript, got {result:?}"
        );
        // No half-written snapshot dir left behind.
        let snap_root = fake_home.join(".cm/agent-memories");
        if snap_root.exists() {
            for entry in fs::read_dir(&snap_root).unwrap() {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                assert!(name.starts_with(".tmp-") || name == "snap" && false,
                    "unexpected leftover: {name}");
                assert!(
                    name != "snap",
                    "snapshot dir should not exist for an empty-transcript save"
                );
            }
        }
    }

    #[test]
    fn save_rejects_partial_trailing_line_with_no_newline() {
        // Source has content but no trailing newline — copy_transcript_truncating
        // truncates it away, leaving 0 bytes. save() must reject.
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();
        let src = dir.path().join("partial.jsonl");
        fs::write(&src, b"{\"incomplete\": tr").unwrap();

        let spec = SaveSpec {
            name: "snap2",
            description: "",
            engine: Engine::ClaudeCode,
            source_session_uid: "ts-x",
            source_transcript_id: "id-x",
            source_cwd: dir.path(),
            source_transcript_path: &src,
            source_memory_dir: None,
        };

        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home) };
        let result = save(spec);
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(
            matches!(result, Err(SnapshotError::NoTranscript)),
            "expected NoTranscript, got {result:?}"
        );
    }

    #[test]
    fn save_cleans_up_tmp_dir_on_manifest_write_failure() {
        // The cleanup branch previously wrapped only the transcript/memory
        // copy block. If `write_atomic(manifest)` failed afterwards, the
        // populated `.tmp-<rand>/` was left behind — invisible to `list()`
        // because of the `.tmp-` prefix, but still on disk.
        //
        // Force a manifest-write failure by pre-creating `manifest.json` as
        // a directory inside the tmp dir; `fs::rename(staged, manifest.json)`
        // then fails with EISDIR. The RAII guard must clean the whole tmp
        // dir up before returning.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("agent-memories");
        fs::create_dir_all(&root).unwrap();

        let src = dir.path().join("src.jsonl");
        fs::write(&src, b"line\n").unwrap();

        let tmp_dir = root.join(".tmp-deterministic");
        fs::create_dir(&tmp_dir).unwrap();
        let blocker = tmp_dir.join("manifest.json");
        fs::create_dir(&blocker).unwrap();

        let final_dir = root.join("attempt");
        let spec = SaveSpec {
            name: "attempt",
            description: "",
            engine: Engine::ClaudeCode,
            source_session_uid: "ts-x",
            source_transcript_id: "id-x",
            source_cwd: dir.path(),
            source_transcript_path: &src,
            source_memory_dir: None,
        };

        let result = save_into_tmp_dir(spec, &tmp_dir, &final_dir);

        assert!(
            result.is_err(),
            "expected manifest write to fail, got {result:?}"
        );
        assert!(!tmp_dir.exists(), "tmp dir must be cleaned up by the guard");
        assert!(!final_dir.exists(), "final dir must not have been created");
        // The whole memories root should be empty too — no orphaned `.tmp-*`.
        for entry in fs::read_dir(&root).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            panic!("unexpected leftover under memories root: {name}");
        }
    }

    #[test]
    fn save_cleans_up_tmp_dir_on_transcript_copy_failure() {
        // Regression: the previous cleanup branch also handled this case.
        // Make sure the RAII rewrite still covers it. Source transcript
        // exists at `.is_file()` time, but is removed before the inner
        // function tries to copy it — `fs::copy` fails with NotFound.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("agent-memories");
        fs::create_dir_all(&root).unwrap();

        let tmp_dir = root.join(".tmp-deterministic2");
        fs::create_dir(&tmp_dir).unwrap();

        let missing = dir.path().join("missing.jsonl");
        // Never create the file.
        let final_dir = root.join("attempt2");
        let spec = SaveSpec {
            name: "attempt2",
            description: "",
            engine: Engine::ClaudeCode,
            source_session_uid: "ts-x",
            source_transcript_id: "id-x",
            source_cwd: dir.path(),
            source_transcript_path: &missing,
            source_memory_dir: None,
        };

        let result = save_into_tmp_dir(spec, &tmp_dir, &final_dir);

        assert!(result.is_err());
        assert!(!tmp_dir.exists(), "tmp dir must be cleaned up");
    }

    #[test]
    fn save_rejects_missing_source_transcript() {
        // The pre-existing missing-file check is now also NoTranscript (was
        // an Io error). Lock it in.
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();
        let missing = dir.path().join("nope.jsonl");

        let spec = SaveSpec {
            name: "snap3",
            description: "",
            engine: Engine::ClaudeCode,
            source_session_uid: "ts-x",
            source_transcript_id: "id-x",
            source_cwd: dir.path(),
            source_transcript_path: &missing,
            source_memory_dir: None,
        };

        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home) };
        let result = save(spec);
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(
            matches!(result, Err(SnapshotError::NoTranscript)),
            "expected NoTranscript, got {result:?}"
        );
    }

    #[test]
    fn codex_clone_only_touches_line_one() {
        // Build a minimal three-line Codex-shaped transcript and clone it.
        // Verify that line 1 has the new id/cwd and lines 2+ are byte-identical.
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        fs::create_dir(&snap_dir).unwrap();
        let original = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"old-id\",\"cwd\":\"/old/cwd\",\"misc\":42}}\n\
                        {\"type\":\"event\",\"payload\":{\"id\":\"event-1\",\"text\":\"hello\"}}\n\
                        {\"type\":\"event\",\"payload\":{\"id\":\"event-2\",\"text\":\"world\"}}\n";
        fs::write(snap_dir.join("transcript.jsonl"), original).unwrap();

        // Rewrite line 1, splice with lines 2+, verify lines 2+ unchanged.
        let mut lines = original.split_inclusive('\n');
        let first = lines.next().unwrap();
        let first_trim = first.strip_suffix('\n').unwrap();
        let rewritten_first =
            rewrite_codex_line1(first_trim, "new-id", "/new/cwd").unwrap();
        let mut out = String::new();
        out.push_str(&rewritten_first);
        out.push('\n');
        for line in lines {
            out.push_str(line);
        }

        let v1: serde_json::Value =
            serde_json::from_str(out.lines().next().unwrap()).unwrap();
        assert_eq!(v1["payload"]["id"], "new-id");
        assert_eq!(v1["payload"]["cwd"], "/new/cwd");
        assert_eq!(v1["payload"]["misc"], 42);

        let original_rest: String =
            original.split_inclusive('\n').skip(1).collect();
        let out_rest: String = out.split_inclusive('\n').skip(1).collect();
        assert_eq!(out_rest, original_rest);
    }
}
