//! Session crons (Claude's `CronCreate` scheduled wakeups) across a restart.
//!
//! A session cron lives only in the Claude process: a transcript rotation,
//! a handoff or an A-R restart loses it. CM learns the live set from the
//! Stop hook (`session_crons`: id, schedule, recurring), but not the prompts.
//! Before such a restart CM captures the set, recovers each prompt from the
//! transcript's recent `CronCreate` / `CronList` rows, saves it to
//! `~/.cm/session-crons/<uid>.json`, and names the crons in the first
//! message of the new process so the agent re-creates them (#114: pending
//! crons used to block every rotation of a busy lead, and A-R refused a
//! session with one).

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::agent_state::Cron;
use crate::state::DaemonState;

/// How much of a transcript's tail is searched for prompts.
const TAIL_BYTES: u64 = 64 * 1024 * 1024;
/// At most this many crons are named in a notice (the rest are in the file).
const MAX_NAMED: usize = 12;

/// One cron to carry over.
#[derive(Clone, Debug, PartialEq)]
pub struct Carried {
    pub id: String,
    pub schedule: String,
    pub recurring: bool,
    /// From the transcript, when it was found there.
    pub prompt: Option<String>,
}

/// The crons the live process of `uid` reported at its last turn end.
pub fn capture(state: &DaemonState, uid: &str) -> Vec<Cron> {
    state
        .sessions
        .get(uid)
        .map(|s| crate::agent_state::current(s).background.crons)
        .unwrap_or_default()
}

/// `crons` with their prompts, recovered from the tail of `transcript`.
pub fn with_prompts(crons: &[Cron], transcript: Option<&Path>) -> Vec<Carried> {
    let found = transcript.map(prompts_in).unwrap_or_default();
    crons
        .iter()
        .map(|c| Carried {
            id: c.id.clone(),
            schedule: c.schedule.clone(),
            recurring: c.recurring,
            prompt: found.get(&c.id).cloned(),
        })
        .collect()
}

/// Cron prompts by id, from the last [`TAIL_BYTES`] of a Claude transcript:
/// `CronList` results name id, cron and prompt; a `CronCreate` result names
/// the id and its `tool_use` carries the prompt. Later rows win.
pub fn prompts_in(path: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(mut f) = std::fs::File::open(path) else { return out };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES))).is_err() {
        return out;
    }
    let mut created: HashMap<String, String> = HashMap::new(); // tool_use id -> prompt
    let mut reader = std::io::BufReader::new(f);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let has = |needle: &[u8]| line.windows(needle.len()).any(|w| w == needle);
        if !(has(b"CronCreate") || has(b"humanSchedule") || has(b"\"jobs\"")) {
            continue;
        }
        let Ok(row) = serde_json::from_slice::<Value>(&line) else { continue };
        for item in row.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
            if item["type"] == "tool_use" && item["name"] == "CronCreate" {
                if let (Some(id), Some(prompt)) = (item["id"].as_str(), item.pointer("/input/prompt").and_then(Value::as_str)) {
                    created.insert(id.to_string(), prompt.to_string());
                }
            }
        }
        let result = &row["toolUseResult"];
        if let Some(jobs) = result["jobs"].as_array() {
            for job in jobs {
                if let (Some(id), Some(prompt)) = (job["id"].as_str(), job["prompt"].as_str()) {
                    out.insert(id.to_string(), prompt.to_string());
                }
            }
        } else if let (Some(id), true) = (result["id"].as_str(), result.get("humanSchedule").is_some()) {
            let use_id = row
                .pointer("/message/content")
                .and_then(Value::as_array)
                .and_then(|c| c.iter().find_map(|i| i["tool_use_id"].as_str()));
            if let Some(prompt) = use_id.and_then(|u| created.get(u)) {
                out.insert(id.to_string(), prompt.clone());
            }
        }
    }
    out
}

/// Save `crons` for `uid` (`~/.cm/session-crons/<uid>.json`, beside the
/// daemon's session file). Returns the path written.
pub fn save(state: &DaemonState, uid: &str, why: &str, crons: &[Carried]) -> Option<PathBuf> {
    let dir = state.daemon_sessions_path.as_ref()?.parent()?.join("session-crons");
    save_in(&dir, uid, why, crons)
}

pub(crate) fn save_in(dir: &Path, uid: &str, why: &str, crons: &[Carried]) -> Option<PathBuf> {
    if crons.is_empty() {
        return None;
    }
    let path = dir.join(format!("{uid}.json"));
    let body = json!({
        "uid": uid,
        "saved_at": crate::control::methods::now_unix_f64(),
        "why": why,
        "crons": crons.iter().map(|c| json!({
            "id": c.id, "cron": c.schedule, "recurring": c.recurring, "prompt": c.prompt,
        })).collect::<Vec<_>>(),
    });
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&body)?)?;
        std::fs::rename(&tmp, &path)
    };
    match write() {
        Ok(()) => Some(path),
        Err(e) => {
            eprintln!("cm-daemon: session crons of {uid} not saved: {e}");
            None
        }
    }
}

/// One line for a notice (typed into the composer, so no newlines): what
/// to re-create. Empty when there is nothing to carry.
pub fn notice(crons: &[Carried], file: Option<&Path>) -> String {
    if crons.is_empty() {
        return String::new();
    }
    let named: Vec<String> = crons
        .iter()
        .take(MAX_NAMED)
        .map(|c| format!("{} `{}` ({})", c.id, c.schedule, if c.recurring { "recurring" } else { "one-shot" }))
        .collect();
    let more = crons.len().saturating_sub(MAX_NAMED);
    let more = if more > 0 { format!(" and {more} more") } else { String::new() };
    let missing = crons.iter().filter(|c| c.prompt.is_none()).count();
    let source = match (file, missing) {
        (Some(f), 0) => format!("Their prompts are saved in {}.", f.display()),
        (Some(f), n) => format!(
            "Their prompts are saved in {} ({n} not found in the transcript: write those from your own notes).",
            f.display()
        ),
        (None, _) => "CM could not save their prompts: re-create them from your own notes.".into(),
    };
    format!(
        " Your {} session cron(s) did not survive the restart: {}{more}. {source} Re-create each with \
         CronCreate (same cron, prompt and recurring), check with CronList, then delete that file.",
        crons.len(),
        named.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cron(id: &str) -> Cron {
        Cron { id: id.into(), schedule: "17 * * * *".into(), recurring: true }
    }

    #[test]
    fn prompts_come_from_cron_list_and_cron_create_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let rows = [
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"CronCreate",
                "input":{"cron":"43 */3 * * *","prompt":"watch the tape","recurring":true}}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]},
                "toolUseResult":{"id":"bbbb","humanSchedule":"Every 3 hours","recurring":true}}),
            json!({"type":"user","message":{"content":[]},
                "toolUseResult":{"jobs":[{"id":"aaaa","cron":"17 * * * *","prompt":"hourly check","recurring":true}]}}),
        ];
        let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let found = prompts_in(&path);
        assert_eq!(found.get("aaaa").map(String::as_str), Some("hourly check"));
        assert_eq!(found.get("bbbb").map(String::as_str), Some("watch the tape"));
        let carried = with_prompts(&[cron("aaaa"), cron("cccc")], Some(&path));
        assert_eq!(carried[0].prompt.as_deref(), Some("hourly check"));
        assert_eq!(carried[1].prompt, None, "an unknown id keeps its schedule, without a prompt");
    }

    #[test]
    fn the_notice_names_each_cron_on_one_line_and_the_file_holds_the_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let crons = vec![
            Carried { id: "aaaa".into(), schedule: "17 * * * *".into(), recurring: true, prompt: Some("hourly".into()) },
            Carried { id: "bbbb".into(), schedule: "5 12 * * *".into(), recurring: false, prompt: None },
        ];
        let file = save_in(dir.path(), "ts-x", "rotation", &crons).unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(saved["crons"][0]["prompt"], "hourly");
        assert_eq!(saved["crons"][1]["cron"], "5 12 * * *");
        let n = notice(&crons, Some(&file));
        assert!(!n.contains('\n'), "typed into the composer: one line");
        assert!(n.contains("aaaa `17 * * * *` (recurring)") && n.contains("bbbb `5 12 * * *` (one-shot)"), "{n}");
        assert!(n.contains("1 not found"), "{n}");
        assert!(n.contains("CronCreate"), "{n}");
        assert_eq!(notice(&[], None), "", "nothing to carry: no sentence");
        assert_eq!(save_in(dir.path(), "ts-y", "x", &[]), None);
    }
}
