//! Dispatch-pending detection over an orchestrator's `.<task>/index.yaml`.
//!
//! Closes the operator-visibility gap found 2026-07-18: after `/triage-review`
//! clears an index-level `blocked_reason` and writes a dated
//! `# OPERATOR <date> ...` directive into an issue entry, the issue has no
//! visible state in the TUI's Continuous panel until the orchestrator's next
//! cycle acts on it (hours later). During that window the operator can't tell
//! approved-awaiting-dispatch apart from untracked.
//!
//! This module parses the index and surfaces issues in exactly that window:
//!
//!   - `blocked_reason` is null / cleared / absent (a SET reason means the
//!     ball is with the operator — that state already renders as the blocked
//!     dot via the planning-status convention), AND
//!   - a dated `OPERATOR <YYYY-MM-DD>` comment exists in the entry (the
//!     directive), AND
//!   - no `operator_ack` on/after the directive date (the orchestrator
//!     cycle-start ACK contract — HOWTO_CONTINUOUS_TASKS.md; an acked
//!     directive is "seen", the orchestrator has it), AND
//!   - the issue's `stage` isn't a closed value (`done` etc.).
//!
//! The remaining condition — "no live `subtask_task_id` mapping to a non-done
//! planning task" — is applied by the CONSUMER (the TUI filters against its
//! synced planning rows; the daemon doesn't need an API round-trip here), so
//! parsed candidates carry `subtask_task_id` verbatim.
//!
//! ## Why not a YAML parser
//!
//! The directive lives in a YAML *comment* — strict parsing would drop it.
//! The file is also free-form (folded scalars, long inline commentary, ad-hoc
//! per-issue keys), written by an agent, and must never hard-fail a scan. So
//! this is a tolerant line scanner: it tracks the `issues:` section by
//! indentation, reads `key: value  # comment` fields at the first field
//! indent, and skips everything deeper (folded-scalar content — which can
//! legitimately contain `#` text that is NOT a comment).

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Upper bound on how much of an index file a scan will read. The real file
/// is tens of KB; anything past this is a runaway and gets skipped rather
/// than parsed.
const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;

/// Cap stored/wired titles so one long title line can't bloat the wire.
const MAX_TITLE_CHARS: usize = 160;

/// One index issue in the operator-unblocked, orchestrator-hasn't-acted
/// window. Wire type: serialized inside the `continuous.dispatch_pending`
/// response and deserialized by the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingIssue {
    /// Index key, e.g. `PERF-083`.
    pub issue_id: String,
    /// The issue's `title:` value, when present (truncated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Newest `OPERATOR <date>` comment date found in the entry
    /// (`YYYY-MM-DD`).
    pub directive_date: String,
    /// The entry's `subtask_task_id:`, when present — the consumer drops the
    /// issue if this maps to a live (non-done) planning task, because a live
    /// subtask means the orchestrator already dispatched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtask_task_id: Option<String>,
}

/// Scan one orchestrator's index for dispatch-pending candidates. Missing
/// dir/file, unreadable file, or an oversized file all yield an empty vec —
/// a scan must never error a control-method response.
pub fn scan_task(worktree_path: &str, task_id: &str) -> Vec<PendingIssue> {
    let path = Path::new(worktree_path)
        .join(format!(".{}", task_id))
        .join("index.yaml");
    match std::fs::metadata(&path) {
        Ok(m) if m.len() <= MAX_INDEX_BYTES => {}
        _ => return Vec::new(),
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_index(&text),
        Err(_) => Vec::new(),
    }
}

/// `stage:` values that mean the issue is closed — a lingering OPERATOR
/// comment on one of these must not flag forever.
fn stage_is_closed(stage: &str) -> bool {
    matches!(
        stage,
        "done" | "closed" | "archived" | "wont_fix" | "wontfix" | "rejected" | "dropped"
    )
}

/// A YAML-ish null: absent value, `null`/`Null`/`NULL`, `~`, or empty
/// (including quoted-empty, already stripped by `strip_quotes`).
fn is_nullish(value: &str) -> bool {
    matches!(value, "" | "~" | "null" | "Null" | "NULL")
}

/// Strip one layer of matching surrounding quotes.
fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2 {
        let b = s.as_bytes();
        if (b[0] == b'"' && b[s.len() - 1] == b'"')
            || (b[0] == b'\'' && b[s.len() - 1] == b'\'')
        {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Split a line's value part at the first `#` that sits OUTSIDE single/double
/// quotes → `(value, comment)`. Tolerant: unterminated quotes swallow the
/// rest of the line into the value (no comment).
fn split_value_comment(raw: &str) -> (&str, Option<&str>) {
    let bytes = raw.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'#' if !in_single && !in_double => {
                return (raw[..i].trim_end(), Some(&raw[i + 1..]));
            }
            _ => {}
        }
    }
    (raw.trim_end(), None)
}

/// First `YYYY-MM-DD` in `s`, if any. Hand-rolled so the daemon doesn't grow
/// a regex dependency for one pattern.
fn find_date(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() < 10 {
        return None;
    }
    for i in 0..=b.len() - 10 {
        let w = &b[i..i + 10];
        let digits_at = |idxs: &[usize]| idxs.iter().all(|&j| w[j].is_ascii_digit());
        if digits_at(&[0, 1, 2, 3, 5, 6, 8, 9]) && w[4] == b'-' && w[7] == b'-' {
            // Avoid matching the tail of a longer digit run (e.g. an id).
            let prev_digit = i > 0 && b[i - 1].is_ascii_digit();
            if !prev_digit {
                return Some(String::from_utf8_lossy(w).into_owned());
            }
        }
    }
    None
}

/// `OPERATOR <...> <date>` directive date in a comment, if any: the comment
/// must contain the literal token `OPERATOR` (uppercase — the convention
/// `/triage-review` writes) with a date somewhere after it.
fn operator_directive_date(comment: &str) -> Option<String> {
    let idx = comment.find("OPERATOR")?;
    find_date(&comment[idx..])
}

/// Per-issue accumulator while scanning.
#[derive(Default)]
struct IssueAcc {
    issue_id: String,
    title: Option<String>,
    stage: Option<String>,
    /// `Some(cleared?)` once a `blocked_reason:` field was seen.
    blocked_reason_cleared: Option<bool>,
    directive_date: Option<String>,
    ack_date: Option<String>,
    subtask_task_id: Option<String>,
}

impl IssueAcc {
    fn note_comment(&mut self, comment: &str) {
        if let Some(d) = operator_directive_date(comment) {
            // Keep the NEWEST directive (ISO dates compare lexicographically).
            if self.directive_date.as_deref().map_or(true, |cur| d.as_str() > cur) {
                self.directive_date = Some(d);
            }
        }
    }

    fn finish(self) -> Option<PendingIssue> {
        let directive_date = self.directive_date?;
        if let Some(stage) = &self.stage {
            if stage_is_closed(stage) {
                return None;
            }
        }
        // A SET blocked_reason means operator-action-needed (the blocked dot's
        // domain), not dispatch-pending. Absent counts as cleared.
        if self.blocked_reason_cleared == Some(false) {
            return None;
        }
        // Cycle-start ACK on/after the directive → the orchestrator has seen
        // it; the plain ◇ "orchestrator has it" state applies again.
        if let Some(ack) = &self.ack_date {
            if ack.as_str() >= directive_date.as_str() {
                return None;
            }
        }
        Some(PendingIssue {
            issue_id: self.issue_id,
            title: self.title,
            directive_date,
            subtask_task_id: self.subtask_task_id,
        })
    }
}

/// Parse an index file's text into dispatch-pending candidates. Never fails;
/// anything unrecognized is skipped.
pub fn parse_index(text: &str) -> Vec<PendingIssue> {
    let mut out = Vec::new();
    let mut in_issues = false;
    // Indent (spaces) of issue keys / of issue fields, learned from the first
    // occurrence of each.
    let mut issue_indent: Option<usize> = None;
    let mut field_indent: Option<usize> = None;
    let mut current: Option<IssueAcc> = None;

    let finish_current = |cur: &mut Option<IssueAcc>, out: &mut Vec<PendingIssue>| {
        if let Some(acc) = cur.take() {
            if let Some(p) = acc.finish() {
                out.push(p);
            }
        }
    };

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            continue;
        }
        let indent = trimmed.len() - trimmed.trim_start().len();
        let body = trimmed.trim_start();

        if !in_issues {
            if indent == 0 && (body == "issues:" || body.starts_with("issues: #")) {
                in_issues = true;
            }
            continue;
        }

        // Any other top-level key ends the issues section.
        if indent == 0 && !body.starts_with('#') {
            break;
        }

        // Full-line comment: attribute to the current issue only at (or
        // above) field level — deeper lines can be folded-scalar CONTENT that
        // merely looks like a comment.
        if body.starts_with('#') {
            if let Some(acc) = current.as_mut() {
                if field_indent.map_or(true, |fi| indent <= fi) {
                    acc.note_comment(&body[1..]);
                }
            }
            continue;
        }

        // Key line? (`key: ...` — tabs are treated like any other char; the
        // files are space-indented in practice.)
        let Some(colon) = body.find(':') else {
            continue;
        };
        let key = body[..colon].trim();
        let rest = &body[colon + 1..];
        // A key containing spaces or quotes is folded-scalar content, not a
        // field ("RAG: Found 0 relevant markets ..." inside a block).
        let key_like = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');

        // New issue header: key-only line at the issue indent.
        let (value_part, comment_part) = split_value_comment(rest);
        let header_like = key_like && value_part.trim().is_empty();
        if header_like && issue_indent.map_or(true, |ii| indent == ii) && current.is_none() {
            issue_indent = Some(indent);
            let mut acc = IssueAcc {
                issue_id: key.to_string(),
                ..Default::default()
            };
            if let Some(c) = comment_part {
                acc.note_comment(c);
            }
            current = Some(acc);
            continue;
        }
        if header_like && issue_indent == Some(indent) {
            // Next issue at the same indent: close out the previous one.
            finish_current(&mut current, &mut out);
            field_indent = None;
            let mut acc = IssueAcc {
                issue_id: key.to_string(),
                ..Default::default()
            };
            if let Some(c) = comment_part {
                acc.note_comment(c);
            }
            current = Some(acc);
            continue;
        }

        let Some(acc) = current.as_mut() else {
            continue;
        };
        let ii = issue_indent.unwrap_or(0);
        if indent <= ii {
            continue;
        }
        // Learn the field indent from the first KEY-LIKE sub-line; anything
        // deeper (or prose containing a colon) is block content, skipped
        // wholesale.
        if !key_like {
            continue;
        }
        let fi = *field_indent.get_or_insert(indent);
        if indent != fi {
            continue;
        }

        if let Some(c) = comment_part {
            acc.note_comment(c);
        }
        let value = strip_quotes(value_part);
        match key {
            "title" => {
                if !is_nullish(value) {
                    acc.title = Some(value.chars().take(MAX_TITLE_CHARS).collect());
                }
            }
            "stage" => {
                if !is_nullish(value) {
                    acc.stage = Some(value.to_ascii_lowercase());
                }
            }
            "blocked_reason" => {
                // A folded/literal scalar marker means a non-null value.
                let cleared =
                    is_nullish(value) && !matches!(value, ">" | "|" | ">-" | "|-" | ">+" | "|+");
                acc.blocked_reason_cleared = Some(cleared);
            }
            "subtask_task_id" => {
                if !is_nullish(value) {
                    acc.subtask_task_id = Some(value.to_string());
                }
            }
            "operator_ack" => {
                if let Some(d) = find_date(value) {
                    // Keep the newest ack.
                    if acc.ack_date.as_deref().map_or(true, |cur| d.as_str() > cur) {
                        acc.ack_date = Some(d);
                    }
                }
            }
            _ => {}
        }
    }
    finish_current(&mut current, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Abbreviated real-world shape (perf-triage 2026-07-18): PERF-083 is the
    /// dispatch-pending exemplar — cleared blocked_reason with a dated
    /// OPERATOR comment and no subtask; PERF-088 has an OPERATOR comment but
    /// also a subtask id (the CONSUMER decides via planning liveness);
    /// PERF-087 has a cleared reason with a non-OPERATOR comment.
    const REAL_SHAPE: &str = r#"# Perf-triage orchestrator memory — tracked performance issues.
next_id: 89   # next NEW-discovery id

issues:

  PERF-088:
    title: "Residual news-burst event-loop FREEZE (~8/hr) NOT owned by PERF-087 fix#1"
    stage: fix_ready                 # cycle 58 (2026-07-16): rebase complete
    blocked_reason: null             # OPERATOR 2026-07-18 (/triage-review): A1-A4 MERGED; KEEP SUBTASK OPEN
    subtask_task_id: 94e3b1aa-d5df-4fbf-837a-9385fdb7becd   # cycle 48: INVESTIGATE spawned

  PERF-087:
    title: "News-pipeline article-batch path FREEZES the event loop 26-33s"
    stage: done                       # cycle 63 (2026-07-18): MONITORING COMPLETE
    blocked_reason: null              # cycle 47: MERGED + deployed; monitoring is MY job
    monitoring_days_clean: 5          # cycle 63 (2026-07-18): DAY 5/5 -> DONE

  PERF-083:
    title: "Reconciler 1Hz safety heartbeat does full per-market work on no-op ticks — ~128s CPU/hr"
    adopted_from_trader: true
    stage: proposed               # trader writeup has full root cause + fix space A/B/C
    review_rounds: 0
    blocked_reason: null   # OPERATOR 2026-07-18 (/triage-review): decision EXISTED since 2026-07-13; DISPATCH an IMPLEMENT subtask per Option C
    window_stats_2026-06-30: {near_price_hysteresis_skips_30min: 1903}
    human_decision_needed: >
      The only fix that reduces heartbeat work changes the wake/reconciler CORE.
      # this looks like a comment but is folded-scalar CONTENT: OPERATOR 2000-01-01
    operator_decision_2026_07_13: >
      OPTION C APPROVED by the operator via /triage-review (2026-07-13).

cycles:
  c64:
    note: "not an issue — OPERATOR 2026-07-18 outside the issues section"   # OPERATOR 2026-07-18
"#;

    #[test]
    fn t_real_shape_flags_perf_083_and_088_not_087() {
        let got = parse_index(REAL_SHAPE);
        let ids: Vec<&str> = got.iter().map(|p| p.issue_id.as_str()).collect();
        // 088 is a candidate (its OPERATOR comment is dated + reason cleared);
        // the TUI drops it because its subtask maps to a live planning task.
        assert_eq!(ids, vec!["PERF-088", "PERF-083"]);
        let p88 = &got[0];
        assert_eq!(
            p88.subtask_task_id.as_deref(),
            Some("94e3b1aa-d5df-4fbf-837a-9385fdb7becd")
        );
        let p83 = &got[1];
        assert_eq!(p83.directive_date, "2026-07-18");
        assert_eq!(p83.subtask_task_id, None);
        assert!(p83.title.as_deref().unwrap().starts_with("Reconciler 1Hz"));
    }

    #[test]
    fn t_set_blocked_reason_not_pending() {
        let text = "issues:\n  X-1:\n    blocked_reason: needs_human_decision   # OPERATOR 2026-07-18: look at this\n";
        assert!(parse_index(text).is_empty());
    }

    #[test]
    fn t_undated_operator_comment_not_pending() {
        let text = "issues:\n  X-1:\n    blocked_reason: null   # OPERATOR: do the thing someday\n";
        assert!(parse_index(text).is_empty());
    }

    #[test]
    fn t_absent_blocked_reason_with_directive_is_pending() {
        let text = "issues:\n  X-1:\n    stage: proposed   # OPERATOR 2026-07-18: dispatch it\n";
        let got = parse_index(text);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].issue_id, "X-1");
    }

    #[test]
    fn t_ack_on_or_after_directive_suppresses() {
        let text = "issues:\n  X-1:\n    blocked_reason: null   # OPERATOR 2026-07-18: dispatch\n    operator_ack: 2026-07-18\n";
        assert!(parse_index(text).is_empty());
        let text2 = "issues:\n  X-1:\n    blocked_reason: null   # OPERATOR 2026-07-18: dispatch\n    operator_ack: 2026-07-19\n";
        assert!(parse_index(text2).is_empty());
    }

    #[test]
    fn t_stale_ack_from_older_directive_still_pending() {
        let text = "issues:\n  X-1:\n    blocked_reason: null   # OPERATOR 2026-07-18: new directive\n    operator_ack: 2026-07-10   # acked the OLD 2026-07-09 directive\n";
        let got = parse_index(text);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].directive_date, "2026-07-18");
    }

    #[test]
    fn t_newest_of_multiple_directives_wins() {
        let text = "issues:\n  X-1:\n    blocked_reason: null   # OPERATOR 2026-07-10: first ask\n    note: x   # OPERATOR 2026-07-15: follow-up directive\n";
        let got = parse_index(text);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].directive_date, "2026-07-15");
    }

    #[test]
    fn t_closed_stage_suppresses() {
        for stage in ["done", "wont_fix", "rejected", "closed", "archived"] {
            let text = format!(
                "issues:\n  X-1:\n    stage: {}\n    blocked_reason: null   # OPERATOR 2026-07-18: lingering note\n",
                stage
            );
            assert!(parse_index(&text).is_empty(), "stage {} must suppress", stage);
        }
    }

    #[test]
    fn t_full_line_operator_comment_counts() {
        let text = "issues:\n  X-1:\n    # OPERATOR 2026-07-18: dispatch per option B\n    blocked_reason: null\n";
        let got = parse_index(text);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn t_folded_scalar_content_never_counts_as_comment_or_field() {
        // The block content contains both a '#'-prefixed line with an
        // OPERATOR date and a line that looks like a blocked_reason field —
        // neither may leak out of the block.
        let text = "issues:\n  X-1:\n    decision: >\n      first line of prose\n      # OPERATOR 2026-07-18 inside a folded block\n      blocked_reason: set_by_prose\n";
        assert!(parse_index(text).is_empty());
    }

    #[test]
    fn t_hash_inside_quoted_title_is_not_a_comment() {
        let text = "issues:\n  X-1:\n    title: \"uses #anchors in text\"   # OPERATOR 2026-07-18: go\n    blocked_reason: null\n";
        let got = parse_index(text);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title.as_deref(), Some("uses #anchors in text"));
    }

    #[test]
    fn t_outside_issues_section_ignored() {
        let text = "meta:\n  note: x   # OPERATOR 2026-07-18: not an issue\n";
        assert!(parse_index(text).is_empty());
        // And keys AFTER the issues section (top-level) are ignored too —
        // covered by REAL_SHAPE's `cycles:` block.
    }

    #[test]
    fn t_garbage_and_empty_never_panic() {
        for t in [
            "",
            "issues:",
            "issues:\n  A:\n",
            "just some text\nwith: colons everywhere\n\t\ttabs",
            "issues:\n\tA:\n\t\tblocked_reason null broken",
            "issues:\n  A-1:\n    title: \"unterminated",
        ] {
            let _ = parse_index(t);
        }
    }

    #[test]
    fn t_find_date_edges() {
        assert_eq!(find_date("x 2026-07-18 y"), Some("2026-07-18".into()));
        assert_eq!(find_date("2026-07-1"), None);
        assert_eq!(find_date("12026-07-18"), None); // tail of a longer run
        assert_eq!(find_date("no date"), None);
    }

    #[test]
    fn t_scan_task_missing_paths_empty() {
        assert!(scan_task("/nonexistent/worktree", "perf-triage").is_empty());
    }

    #[test]
    fn t_scan_task_reads_dot_dir_index() {
        let dir = std::env::temp_dir().join(format!(
            "cm-dp-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let idx_dir = dir.join(".perf-triage");
        std::fs::create_dir_all(&idx_dir).unwrap();
        std::fs::write(
            idx_dir.join("index.yaml"),
            "issues:\n  P-1:\n    blocked_reason: null  # OPERATOR 2026-07-18: dispatch\n",
        )
        .unwrap();
        let got = scan_task(dir.to_str().unwrap(), "perf-triage");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].issue_id, "P-1");
        std::fs::remove_dir_all(&dir).ok();
    }
}
