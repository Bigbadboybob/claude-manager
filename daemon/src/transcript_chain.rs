//! Claude `continued-in` chains (#24).
//!
//! When Claude Code moves a conversation to a fresh session file (its
//! "Resume from summary" choice, or a background-session handoff) it appends
//! `{"type":"continued-in","sessionId":<old>,"continuedInSessionId":<new>}`
//! to the old transcript. The old file is then superseded: resuming it
//! reloads the whole history the move was meant to leave behind. Claude's own
//! `/resume` picker hides such files; every CM resume path follows the chain
//! to the newest session instead ([`follow`]).
//!
//! Claude's reader (2.1.310) scans the file backwards and stops at the first
//! line that is either the record or a user/assistant message: a record
//! followed by more conversation means the old session was resumed and
//! continued anyway, and is no longer superseded. [`continued_in`] mirrors
//! that rule.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const RECORD: &str = "\"type\":\"continued-in\"";
const CHUNK: u64 = 64 * 1024;
/// How far back from the end a scan looks before giving up (a superseded
/// file's record is its last line or near it).
const SCAN_LIMIT: u64 = 16 * 1024 * 1024;
/// Longest chain followed (cycles are refused separately).
const MAX_HOPS: usize = 16;

/// The session a transcript says it continued in, if it is superseded.
pub fn continued_in(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut end = len;
    let mut carry: Vec<u8> = Vec::new();
    while end > 0 && len - end < SCAN_LIMIT {
        let start = end.saturating_sub(CHUNK);
        let mut buf = vec![0u8; (end - start) as usize];
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut buf).ok()?;
        buf.extend_from_slice(&carry);
        // Every complete line in `buf` except possibly the first (it may be
        // cut at `start`, unless `start` is the file's beginning).
        let mut lines: Vec<&[u8]> = buf.split(|b| *b == b'\n').collect();
        let head = if start > 0 { Some(lines.remove(0)) } else { None };
        for line in lines.iter().rev() {
            if let Some(decision) = decide(line) {
                return decision;
            }
        }
        carry = head.map(<[u8]>::to_vec).unwrap_or_default();
        end = start;
    }
    if !carry.is_empty() {
        if let Some(decision) = decide(&carry) {
            return decision;
        }
    }
    None
}

/// `Some(Some(id))`: the record; `Some(None)`: conversation after it (or no
/// record before the last message); `None`: a line that decides nothing.
fn decide(line: &[u8]) -> Option<Option<String>> {
    let text = std::str::from_utf8(line).ok()?;
    if text.contains(RECORD) {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let id = value.get("continuedInSessionId")?.as_str()?.trim();
        return Some((!id.is_empty()).then(|| id.to_string()));
    }
    if text.contains("\"type\":\"user\"") || text.contains("\"type\":\"assistant\"") {
        return Some(None);
    }
    None
}

/// Where a session's transcript lives: next to `from` in the same project
/// directory, else in any other Claude project directory (a move can land
/// in the new cwd's directory).
fn locate(from: &Path, id: &str) -> Option<PathBuf> {
    let file = format!("{id}.jsonl");
    if let Some(dir) = from.parent() {
        let here = dir.join(&file);
        if here.is_file() {
            return Some(here);
        }
        if let Some(projects) = dir.parent() {
            for entry in std::fs::read_dir(projects).ok()?.flatten() {
                let candidate = entry.path().join(&file);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// A followed chain: the newest session and the hops taken to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Followed {
    pub id: String,
    pub path: PathBuf,
    /// The superseded ids, oldest first (the starting id included).
    pub superseded: Vec<String>,
}

/// Follow `continued-in` pointers from `path` (a transcript named
/// `<id>.jsonl`). `None` when the file is not superseded, or its successor
/// is missing (resuming the file you have beats resuming nothing). Cycles
/// and over-long chains stop at the last good hop.
pub fn follow(path: &Path) -> Option<Followed> {
    let mut current = path.to_path_buf();
    let mut superseded = Vec::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(stem(&current)?);
    while superseded.len() < MAX_HOPS {
        let Some(next) = continued_in(&current) else { break };
        if !seen.insert(next.clone()) {
            eprintln!(
                "cm-daemon: transcript chain from {} loops at {next}; stopping",
                path.display()
            );
            break;
        }
        let Some(next_path) = locate(&current, &next) else {
            eprintln!(
                "cm-daemon: {} continued in {next}, whose transcript is not on disk; \
                 keeping the last one found",
                current.display()
            );
            break;
        };
        superseded.push(stem(&current)?);
        current = next_path;
    }
    if superseded.is_empty() {
        return None;
    }
    Some(Followed { id: stem(&current)?, path: current, superseded })
}

/// For an adopted Claude session's recorded transcript path: the newest
/// file in its chain, when it moved (a brain restart adopts the live
/// process; this keeps its resume key on the conversation it is in).
pub fn followed_path(session_type: &str, path: Option<&str>) -> Option<String> {
    if session_type != "claude-code" {
        return None;
    }
    follow(Path::new(path?)).map(|f| f.path.to_string_lossy().into_owned())
}

fn stem(path: &Path) -> Option<String> {
    path.file_stem()?.to_str().map(str::to_string)
}

/// For a Claude resume of `id` in `worktree`: the newest session in its
/// chain, when it moved. Logged, so a followed resume is visible.
pub fn resolve_claude_resume(uid: &str, worktree: &Path, id: &str) -> Option<Followed> {
    let path = crate::transcript_detect::claude_transcript_path(worktree, id)?;
    let followed = follow(&path)?;
    eprintln!(
        "cm-daemon: session {uid}: transcript {id} continued in {} ({} hop(s)); resuming that",
        followed.id,
        followed.superseded.len(),
    );
    Some(followed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) fn write(dir: &Path, id: &str, lines: &[serde_json::Value]) -> PathBuf {
        let path = dir.join(format!("{id}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{}", serde_json::to_string(l).unwrap()).unwrap();
        }
        path
    }

    pub(crate) fn user(text: &str) -> serde_json::Value {
        serde_json::json!({"type": "user", "message": {"role": "user", "content": text}})
    }

    pub(crate) fn moved(from: &str, to: &str) -> serde_json::Value {
        serde_json::json!({"type": "continued-in", "timestamp": "2026-10-18T00:00:00Z",
                           "sessionId": from, "continuedInSessionId": to})
    }

    #[test]
    fn a_two_hop_chain_resumes_the_newest_session() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "aaaa", &[user("one"), moved("aaaa", "bbbb")]);
        write(dir.path(), "bbbb", &[user("two"), moved("bbbb", "cccc")]);
        let c = write(dir.path(), "cccc", &[user("three")]);
        let followed = follow(&a).expect("superseded");
        assert_eq!(followed.id, "cccc");
        assert_eq!(followed.path, c);
        assert_eq!(followed.superseded, ["aaaa", "bbbb"]);
        assert!(follow(&c).is_none(), "the newest file is not superseded");
    }

    #[test]
    fn a_record_followed_by_more_conversation_is_not_superseded() {
        // CM resumed the old id after the move and the agent kept working in
        // it (the SEJD-JAX-Opus shape): Claude no longer treats it as moved.
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "aaaa", &[user("one"), moved("aaaa", "bbbb"), user("later")]);
        write(dir.path(), "bbbb", &[user("two")]);
        assert!(follow(&a).is_none());
    }

    #[test]
    fn bookkeeping_after_the_record_does_not_hide_it() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "aaaa", &[
            user("one"),
            moved("aaaa", "bbbb"),
            serde_json::json!({"type": "last-prompt", "lastPrompt": "x"}),
            serde_json::json!({"type": "ai-title", "aiTitle": "t"}),
        ]);
        write(dir.path(), "bbbb", &[user("two")]);
        assert_eq!(follow(&a).unwrap().id, "bbbb");
    }

    #[test]
    fn a_missing_successor_or_a_cycle_keeps_the_last_good_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "aaaa", &[user("one"), moved("aaaa", "gone")]);
        assert!(follow(&a).is_none(), "the successor is not on disk");
        let x = write(dir.path(), "xxxx", &[moved("xxxx", "yyyy")]);
        write(dir.path(), "yyyy", &[moved("yyyy", "xxxx")]);
        assert_eq!(follow(&x).unwrap().id, "yyyy", "the loop back is refused");
    }

    #[test]
    fn the_successor_may_live_in_another_project_directory() {
        let root = tempfile::tempdir().unwrap();
        let one = root.path().join("-proj-one");
        let two = root.path().join("-proj-two");
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        let a = write(&one, "aaaa", &[user("one"), moved("aaaa", "bbbb")]);
        let b = write(&two, "bbbb", &[user("two")]);
        assert_eq!(follow(&a).unwrap().path, b);
    }

    #[test]
    fn the_scan_crosses_chunk_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(200 * 1024);
        let a = write(dir.path(), "aaaa", &[
            user("one"),
            moved("aaaa", "bbbb"),
            serde_json::json!({"type": "attachment", "blob": big}),
        ]);
        write(dir.path(), "bbbb", &[user("two")]);
        assert_eq!(follow(&a).unwrap().id, "bbbb");
    }
}
