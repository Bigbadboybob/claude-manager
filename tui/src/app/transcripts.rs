//! proper-resume: the transcript picker's data source.
//!
//! Lists the conversations an engine could `--resume` (claude) or
//! `codex resume` for a worktree — the same set the agents' own
//! `/resume` pickers show, since both scope their session lists to the
//! working directory. Two consumers:
//!
//! - **A-e → Transcript → Enter** binds an EXISTING row to a transcript
//!   (repair a mis-detected binding — the ESmisc codex incident — or
//!   point a dead row at the right conversation before A-R revives it).
//! - **A-s → Resume → Enter** spawns a NEW session resumed from a
//!   transcript, with the binding correct from the start — the native
//!   replacement for "A-s, then `/resume` inside the pane".
//!
//! Previews read only the head of each file (the JSONL files grow into
//! the megabytes and there are hundreds of them), so opening the picker
//! on a busy worktree stays sub-second.

use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

/// One resumable conversation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TranscriptCandidate {
    /// The resume key: claude's file stem / codex's thread uuid.
    pub id: String,
    /// Last modification of the transcript file.
    pub modified: SystemTime,
    /// File size in bytes — a cheap proxy for conversation length.
    pub size_bytes: u64,
    /// The first real user message, single-line, truncated. Empty when
    /// the head of the file carried nothing human-typed.
    pub preview: String,
    /// Label of the sidebar row currently bound to this transcript
    /// (filled in by the caller — the lister has no sidebar view).
    pub bound_to: Option<String>,
}

/// How many lines of a transcript's head to scan for a preview.
const PREVIEW_SCAN_LINES: usize = 80;
/// Preview width in chars.
const PREVIEW_CHARS: usize = 90;
/// Upper bound on candidates returned (newest first).
const MAX_CANDIDATES: usize = 60;

/// List the resumable transcripts for `engine` (`"claude"` / `"codex"`)
/// in `worktree`, newest first. Anything else (bash) has no transcripts
/// and yields an empty list.
pub(crate) fn list_transcript_candidates(
    worktree: &Path,
    engine: &str,
) -> Vec<TranscriptCandidate> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    list_transcript_candidates_in(&home, worktree, engine)
}

/// `HOME`-parameterized body of [`list_transcript_candidates`] so tests
/// can point it at a fixture tree.
pub(crate) fn list_transcript_candidates_in(
    home: &Path,
    worktree: &Path,
    engine: &str,
) -> Vec<TranscriptCandidate> {
    let mut out = match engine {
        "claude" => list_claude(home, worktree),
        "codex" => list_codex(home, worktree),
        _ => Vec::new(),
    };
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out.truncate(MAX_CANDIDATES);
    out
}

/// `~/.claude/projects/<encoded worktree>/*.jsonl` — the same encoding
/// `App::detect_session_id` / `list_jsonl_files` use.
fn list_claude(home: &Path, worktree: &Path) -> Vec<TranscriptCandidate> {
    let Some(path_str) = worktree.to_str() else {
        return Vec::new();
    };
    let encoded = path_str.replace('/', "-").replace('.', "-");
    let dir = home.join(".claude/projects").join(encoded);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        // Claude writes a zero-byte or metadata-only file for a session
        // that never got a message; nothing to resume there.
        if meta.len() == 0 {
            continue;
        }
        out.push(TranscriptCandidate {
            id: stem.to_string(),
            modified: meta.modified().unwrap_or(UNIX_EPOCH),
            size_bytes: meta.len(),
            preview: claude_preview(&path),
            bound_to: None,
        });
    }
    out
}

/// `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` whose first-line
/// `payload.cwd` is the worktree — mirrors `App::walk_codex_sessions`,
/// plus a preview.
fn list_codex(home: &Path, worktree: &Path) -> Vec<TranscriptCandidate> {
    let Some(wt_str) = worktree.to_str() else {
        return Vec::new();
    };
    let root = home.join(".codex/sessions");
    let mut out = Vec::new();
    walk_codex(&root, wt_str, &mut out);
    out
}

fn walk_codex(dir: &Path, wt_str: &str, out: &mut Vec<TranscriptCandidate>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_codex(&path, wt_str, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(first) = workflow::transcript::read_first_line(&path) else { continue };
        let Ok(head) = serde_json::from_str::<serde_json::Value>(first.trim()) else {
            continue;
        };
        if head.pointer("/payload/cwd").and_then(|v| v.as_str()) != Some(wt_str) {
            continue;
        }
        let Some(id) = head.pointer("/payload/id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        out.push(TranscriptCandidate {
            id: id.to_string(),
            modified: meta.modified().unwrap_or(UNIX_EPOCH),
            size_bytes: meta.len(),
            preview: codex_preview(&path),
            bound_to: None,
        });
    }
}

/// Read up to `PREVIEW_SCAN_LINES` lines of `path`.
fn head_lines(path: &Path) -> Vec<String> {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .take(PREVIEW_SCAN_LINES)
        .map_while(Result::ok)
        .collect()
}

/// First human-typed user message in a claude transcript. Skips tool
/// results (array content with `tool_result` blocks), meta/system
/// injections, and anything that starts with a tag (`<command-name>`,
/// `<system-reminder>`, `<local-command-stdout>`…), which is how claude
/// marks non-human user turns.
fn claude_preview(path: &Path) -> String {
    for line in head_lines(path) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        if v.get("isMeta").and_then(|b| b.as_bool()) == Some(true) {
            continue;
        }
        let Some(content) = v.pointer("/message/content") else { continue };
        let text = match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(blocks) => {
                let mut acc = String::new();
                for b in blocks {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            acc.push_str(t);
                            acc.push(' ');
                        }
                    }
                }
                acc
            }
            _ => continue,
        };
        if let Some(p) = as_preview(&text) {
            return p;
        }
    }
    String::new()
}

/// First human-typed user message in a codex rollout. Codex records the
/// prompt twice — as an `event_msg` / `user_message` and as a
/// `response_item` user message — and wraps environment context /
/// AGENTS.md in tagged blocks; either shape works, tagged text is
/// skipped.
fn codex_preview(path: &Path) -> String {
    for line in head_lines(path) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let text: Option<String> = match v.get("type").and_then(|t| t.as_str()) {
            Some("event_msg")
                if v.pointer("/payload/type").and_then(|t| t.as_str())
                    == Some("user_message") =>
            {
                v.pointer("/payload/message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            }
            Some("response_item")
                if v.pointer("/payload/role").and_then(|r| r.as_str()) == Some("user") =>
            {
                v.pointer("/payload/content")
                    .and_then(|c| c.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
            }
            _ => None,
        };
        if let Some(p) = text.as_deref().and_then(as_preview) {
            return p;
        }
    }
    String::new()
}

/// Collapse whitespace, reject tag-led / empty text, truncate. `None`
/// means "not a human message, keep looking".
fn as_preview(text: &str) -> Option<String> {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() || collapsed.starts_with('<') {
        return None;
    }
    let cleaned = sanitize_for_display(&collapsed);
    let mut out: String = cleaned.chars().take(PREVIEW_CHARS).collect();
    if cleaned.chars().count() > PREVIEW_CHARS {
        out.push('\u{2026}');
    }
    Some(out)
}

/// `2m` / `3h` / `5d` style age for the picker row.
pub(crate) fn age_label(modified: SystemTime, now: SystemTime) -> String {
    let secs = now
        .duration_since(modified)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// `12K` / `1.4M` style size for the picker row.
pub(crate) fn size_label(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{}K", bytes / 1024)
    } else {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// The first 8 chars of a transcript id — how the picker and the A-e
/// form refer to one.
pub(crate) fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

impl App {
    /// Open the transcript picker for `target`. Candidates are the
    /// target worktree's transcripts for the target engine, newest
    /// first, each tagged with the sidebar row that already holds it
    /// (`bound_to = Some("")` for the row being rebound itself).
    pub(super) fn open_transcript_picker(&mut self, target: TranscriptPickTarget) {
        let (workspace_id, engine, self_uid): (&str, String, Option<&str>) = match &target {
            TranscriptPickTarget::BindSession { workspace_id, session_uid } => {
                let engine = self
                    .workspaces
                    .iter()
                    .find(|w| w.id == *workspace_id)
                    .and_then(|w| w.sessions.iter().find(|s| s.uid == *session_uid))
                    .map(|s| s.session_type.clone());
                let Some(engine) = engine else {
                    self.set_status_msg("Session no longer in the sidebar");
                    return;
                };
                (workspace_id.as_str(), engine, Some(session_uid.as_str()))
            }
            TranscriptPickTarget::NewTerminalSession { workspace_id, session_type, .. } => {
                (workspace_id.as_str(), session_type.clone(), None)
            }
        };
        let Some(ws) = self.workspaces.iter().find(|w| w.id == workspace_id) else {
            self.set_status_msg("Workspace no longer exists");
            return;
        };
        let Some(wt) = ws.worktree_path.clone() else {
            self.set_status_msg("Workspace has no worktree — nothing to resume");
            return;
        };
        let mut candidates = list_transcript_candidates(&wt, &engine);
        for cand in &mut candidates {
            cand.bound_to = ws
                .sessions
                .iter()
                .find(|s| s.transcript_id.as_deref() == Some(cand.id.as_str()))
                .map(|s| if Some(s.uid.as_str()) == self_uid { String::new() } else { s.label.clone() });
        }
        // Start on the row's current binding when rebinding, so Enter
        // without moving is a no-op rather than a surprise.
        let selected = self_uid
            .and_then(|_| candidates.iter().position(|c| c.bound_to.as_deref() == Some("")))
            .unwrap_or(0);
        self.input_mode = InputMode::TranscriptPicker { candidates, selected, target };
    }

    /// A-e → Transcript → Enter: resolve the form's indices to stable
    /// ids and open the picker in bind mode.
    pub(super) fn open_transcript_picker_for_session(
        &mut self,
        ws_index: usize,
        session_index: usize,
    ) {
        let Some((workspace_id, session_uid, session_type)) = self
            .workspaces
            .get(ws_index)
            .and_then(|ws| {
                ws.sessions
                    .get(session_index)
                    .map(|ts| (ws.id.clone(), ts.uid.clone(), ts.session_type.clone()))
            })
        else {
            self.set_status_msg("Session no longer in the sidebar");
            return;
        };
        if !matches!(session_type.as_str(), "claude" | "codex") {
            self.set_status_msg("Only claude / codex sessions have a transcript");
            return;
        }
        self.open_transcript_picker(TranscriptPickTarget::BindSession {
            workspace_id,
            session_uid,
        });
    }

    /// Rebind the row `session_uid` in `workspace_id` to transcript
    /// `id`: the manifest row (what A-R and a user-owned startup restore
    /// resume from) and, for a live daemon-attached session, the
    /// daemon's registry (what a daemon-side restore / `session.revive`
    /// resumes from, and what `read_session_output` reads).
    ///
    /// For a LIVE session this is a metadata repair, not a conversation
    /// switch — the agent keeps writing wherever it is, and a claude
    /// pane's Stop hook re-stamps the daemon with the real path on its
    /// next turn (which then flows back here via the manifest
    /// broadcast). To actually move the pane, run `/resume <id>` in it;
    /// the binding follows on its own. For a DEAD row the bind is the
    /// whole point: A-R revives it from the chosen conversation.
    pub(super) fn bind_session_transcript(
        &mut self,
        workspace_id: &str,
        session_uid: &str,
        id: &str,
    ) {
        let Some(wi) = self.workspaces.iter().position(|w| w.id == workspace_id) else {
            self.set_status_msg("Workspace no longer exists — not bound");
            return;
        };
        let Some(si) = self.workspaces[wi]
            .sessions
            .iter()
            .position(|s| s.uid == session_uid)
        else {
            self.set_status_msg("Session no longer in the sidebar — not bound");
            return;
        };
        let short = short_id(id);
        {
            let ts = &mut self.workspaces[wi].sessions[si];
            if ts.transcript_id.as_deref() == Some(id) {
                self.set_status_msg(&format!("Already bound to {}\u{2026}", short));
                return;
            }
            if ts.transcript_id.is_some() {
                ts.rebind_transcript(Some(id.to_string()));
            } else {
                ts.transcript_id = Some(id.to_string());
            }
            ts.pending_jsonl_files = None;
        }
        self.save_session_manifest();
        self.needs_redraw = true;
        let (live, daemon_attached) = {
            let ts = &self.workspaces[wi].sessions[si];
            (!ts.session.exited, ts.session.daemon_session_uid.is_some())
        };
        if live && daemon_attached {
            let ws = &self.workspaces[wi];
            let ts = &ws.sessions[si];
            Self::push_transcript_path_to_daemon_if_attached(&self.host_pool, ts, ws);
        }
        if live {
            self.set_status_msg(&format!(
                "Bound to {}\u{2026} — run /resume {} in the pane to switch its conversation",
                short, id,
            ));
        } else {
            self.set_status_msg(&format!(
                "Bound to {}\u{2026} — A-R revives this session from it",
                short,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn write(path: &Path, lines: &[&str]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, lines.join("\n")).unwrap();
    }

    #[test]
    fn claude_candidates_come_from_the_encoded_project_dir_newest_first() {
        let home = tempfile::tempdir().unwrap();
        let wt = Path::new("/home/u/code/proj.x");
        let dir = home.path().join(".claude/projects/-home-u-code-proj-x");
        let old = dir.join("11111111-aaaa-bbbb-cccc-000000000001.jsonl");
        let new = dir.join("22222222-aaaa-bbbb-cccc-000000000002.jsonl");
        write(
            &old,
            &[
                r#"{"type":"user","isMeta":true,"message":{"content":"<command-name>/clear</command-name>"}}"#,
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
                r#"{"type":"user","message":{"content":"fix the   flaky\nrestore test"}}"#,
            ],
        );
        write(&new, &[r#"{"type":"user","message":{"content":[{"type":"text","text":"hello there"}]}}"#]);
        // An empty file is a never-used session: not resumable.
        write(&dir.join("33333333-empty.jsonl"), &[]);
        let t0 = SystemTime::now() - Duration::from_secs(3600);
        filetime_set(&old, t0);
        filetime_set(&new, t0 + Duration::from_secs(60));

        let got = list_transcript_candidates_in(home.path(), wt, "claude");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "22222222-aaaa-bbbb-cccc-000000000002");
        assert_eq!(got[0].preview, "hello there");
        assert_eq!(got[1].id, "11111111-aaaa-bbbb-cccc-000000000001");
        assert_eq!(got[1].preview, "fix the flaky restore test", "tag-led + tool_result turns are skipped");
        assert!(got.iter().all(|c| c.bound_to.is_none()));
    }

    #[test]
    fn codex_candidates_filter_on_cwd_and_read_the_first_prompt() {
        let home = tempfile::tempdir().unwrap();
        let wt = Path::new("/home/u/code/proj");
        let day = home.path().join(".codex/sessions/2026/09/06");
        write(
            &day.join("rollout-2026-09-06T13-15-34-01a077ee-d12c-7611-a855-440c37783e88.jsonl"),
            &[
                r#"{"type":"session_meta","payload":{"id":"01a077ee-d12c-7611-a855-440c37783e88","cwd":"/home/u/code/proj"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"forecast the path budget"}}"#,
            ],
        );
        write(
            &day.join("rollout-2026-09-06T14-00-00-01a07903-5604-76a2-8626-ecff6c64c2b8.jsonl"),
            &[r#"{"type":"session_meta","payload":{"id":"01a07903-5604-76a2-8626-ecff6c64c2b8","cwd":"/somewhere/else"}}"#],
        );
        let got = list_transcript_candidates_in(home.path(), wt, "codex");
        assert_eq!(got.len(), 1, "the other-cwd rollout is not offered");
        assert_eq!(got[0].id, "01a077ee-d12c-7611-a855-440c37783e88");
        assert_eq!(got[0].preview, "forecast the path budget");
    }

    #[test]
    fn bash_and_missing_dirs_yield_nothing() {
        let home = tempfile::tempdir().unwrap();
        let wt = Path::new("/nope");
        assert!(list_transcript_candidates_in(home.path(), wt, "bash").is_empty());
        assert!(list_transcript_candidates_in(home.path(), wt, "claude").is_empty());
        assert!(list_transcript_candidates_in(home.path(), wt, "codex").is_empty());
    }

    #[test]
    fn labels_are_compact() {
        let now = SystemTime::now();
        assert_eq!(age_label(now - Duration::from_secs(5), now), "now");
        assert_eq!(age_label(now - Duration::from_secs(120), now), "2m");
        assert_eq!(age_label(now - Duration::from_secs(7200), now), "2h");
        assert_eq!(age_label(now - Duration::from_secs(3 * 86_400), now), "3d");
        assert_eq!(size_label(512), "512B");
        assert_eq!(size_label(20 * 1024), "20K");
        assert_eq!(size_label(1_500_000), "1.4M");
        assert_eq!(short_id("01a077ee-d12c-7611-a855-440c37783e88"), "01a077ee");
        assert!(as_preview("   ").is_none());
        assert!(as_preview("<system-reminder>x</system-reminder>").is_none());
        let long = "x".repeat(200);
        assert_eq!(as_preview(&long).unwrap().chars().count(), PREVIEW_CHARS + 1);
    }

    fn filetime_set(path: &Path, t: SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }
}
