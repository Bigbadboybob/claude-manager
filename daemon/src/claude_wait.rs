//! Bounded, identity-bound proof that a particular Claude permission wait ended.
//! File growth, generic assistant output, and another tool's result are not proof.
use crate::agent_state::{HookEdges, PresenceObs, PresenceStatus};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 256 * 1024;
const MAX_LINES: usize = 200;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Key {
    pub hook_at: Option<f64>,
    pub presence_at: Option<f64>,
    pub tool_id: Option<String>,
    pub tool_name: Option<String>,
    pub floor: f64,
}
fn permission(reason: Option<&str>) -> bool {
    matches!(
        reason,
        Some("permission" | "permission_prompt" | "permission prompt")
    )
}
impl Key {
    pub(crate) fn new(hooks: &HookEdges, presence: &PresenceObs, floor: f64) -> Option<Self> {
        let hook_at = hooks
            .waiting_at
            .filter(|at| *at > floor)
            .filter(|_| permission(hooks.waiting_for.as_deref()));
        let presence_at = presence
            .status_updated_at
            .filter(|at| *at > floor)
            .filter(|_| {
                presence.status == PresenceStatus::Waiting
                    && permission(presence.waiting_for.as_deref())
            });
        let at = hook_at
            .into_iter()
            .chain(presence_at)
            .max_by(f64::total_cmp)?;
        let hook_is_latest = hook_at == Some(at);
        Some(Self {
            hook_at,
            presence_at,
            floor,
            tool_id: hook_is_latest
                .then(|| hooks.waiting_tool_id.clone())
                .flatten(),
            tool_name: hook_is_latest.then(|| hooks.open_tool.clone()).flatten(),
        })
    }
    fn at(&self) -> f64 {
        self.hook_at
            .into_iter()
            .chain(self.presence_at)
            .max_by(f64::total_cmp)
            .unwrap()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    pub key: Key,
    pub at: f64,
    pub path: PathBuf,
    identity: (u32, Option<u64>),
    // Persist the witness, never the claim that it was verified in this brain.
    #[serde(skip)]
    pub(crate) verified: bool,
    // A bounded witness is revalidated after append and restart; replacing or
    // truncating the file cannot carry the cached verdict to another transcript.
    dev: u64,
    ino: u64,
    offset: u64,
    len: usize,
    hash: Vec<u8>,
}
impl Resolution {
    fn covers(&self, key: &Key) -> bool {
        self.at > key.at()
            && self.at > key.floor
            && key.hook_at.is_none_or(|at| self.key.hook_at == Some(at))
            && key
                .presence_at
                .is_none_or(|at| self.key.presence_at == Some(at))
            && key
                .tool_id
                .as_ref()
                .is_none_or(|id| self.key.tool_id.as_ref() == Some(id))
    }
}
#[derive(Clone, Debug, PartialEq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
impl Stamp {
    fn of(m: &std::fs::Metadata) -> Option<Self> {
        m.is_file().then(|| Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        })
    }
}
struct Cached {
    identity: (u32, Option<u64>),
    path: PathBuf,
    key: Key,
    stamp: Stamp,
    result: Option<Resolution>,
    retry_at: Option<f64>,
}
#[derive(Default)]
pub(crate) struct Reader {
    cache: BTreeMap<String, Cached>,
}
impl Reader {
    pub(crate) fn retain(&mut self, live: &std::collections::HashSet<&str>) {
        self.cache.retain(|uid, _| live.contains(uid.as_str()));
    }
    pub(crate) fn observe(
        &mut self,
        uid: &str,
        identity: (u32, Option<u64>),
        path: &Path,
        key: Key,
        prior: Option<&Resolution>,
        now: f64,
    ) -> Option<Resolution> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .ok()?;
        let stamp = Stamp::of(&file.metadata().ok()?)?;
        let stable = |f: &File| {
            Stamp::of(&f.metadata().ok()?).filter(|s| {
                *s == stamp
                    && std::fs::symlink_metadata(path)
                        .ok()
                        .and_then(|m| Stamp::of(&m))
                        .as_ref()
                        == Some(s)
            })
        };
        if let Some(c) = self.cache.get(uid).filter(|c| {
            c.identity == identity
                && c.path == path
                && c.key == key
                && c.stamp == stamp
                && c.retry_at.is_none_or(|at| now < at)
        }) {
            return stable(&file).and(c.result.clone());
        }
        // Reuse only the exact saved witness, not a monotonic mtime/size guess.
        let old = prior.filter(|p| {
            p.path == path
                && p.identity == identity
                && p.dev == stamp.dev
                && p.ino == stamp.ino
                && p.covers(&key)
                && p.at <= now
                && p.len as u64 <= MAX_BYTES
                && p.offset
                    .checked_add(p.len as u64)
                    .is_some_and(|end| end <= stamp.len)
        });
        let mut retry_at = None;
        let mut result = old.and_then(|p| {
            let mut bytes = vec![0; p.len];
            file.seek(SeekFrom::Start(p.offset)).ok()?;
            file.read_exact(&mut bytes).ok()?;
            (Sha256::digest(&bytes).as_slice() == p.hash).then(|| {
                let mut r = p.clone();
                r.key = key.clone();
                r.verified = true;
                r
            })
        });
        if result.is_none() {
            let offset = stamp.len.saturating_sub(MAX_BYTES);
            file.seek(SeekFrom::Start(offset)).ok()?;
            let mut bytes = vec![0; (stamp.len - offset) as usize];
            file.read_exact(&mut bytes).ok()?;
            if bytes.last() == Some(&b'\n') {
                let skip = if offset == 0 {
                    0
                } else {
                    bytes.iter().position(|b| *b == b'\n')? + 1
                };
                let text = std::str::from_utf8(&bytes[skip..]).ok()?;
                let lines: Vec<_> = text.split_inclusive('\n').rev().take(MAX_LINES).collect();
                let len: usize = lines.iter().map(|s| s.len()).sum();
                let span = &text[text.len() - len..];
                result = resolved_at(span, path, &key, now, &mut retry_at).map(|at| Resolution {
                    key: key.clone(),
                    at,
                    path: path.to_owned(),
                    identity,
                    verified: true,
                    dev: stamp.dev,
                    ino: stamp.ino,
                    offset: offset + skip as u64 + (text.len() - len) as u64,
                    len,
                    hash: Sha256::digest(span.as_bytes()).to_vec(),
                });
            }
        }
        stable(&file)?; // No verdict from a concurrent rewrite/rename/truncation.
        self.cache.insert(
            uid.into(),
            Cached {
                identity,
                path: path.into(),
                key,
                stamp,
                result: result.clone(),
                retry_at,
            },
        );
        result
    }
}

struct Call {
    name: String,
    foreground: bool,
}
fn short(v: Option<&serde_json::Value>) -> Option<&str> {
    v?.as_str().filter(|s| !s.is_empty() && s.len() <= 256)
}
fn resolved_at(
    text: &str,
    path: &Path,
    key: &Key,
    now: f64,
    retry_at: &mut Option<f64>,
) -> Option<f64> {
    let native = path.file_stem()?.to_str()?;
    let mut calls = BTreeMap::<String, Call>::new();
    let mut anchored = false;
    let mut prior_at = None;
    let mut selected: Option<String> = None;
    let mut chose = false;
    let mut resolved = None;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let row: serde_json::Value = serde_json::from_str(line).ok()?;
        if row.get("isSidechain").and_then(|v| v.as_bool()) == Some(true)
            || ["agentId", "agent_id", "parentToolUseID", "parentToolUseId"]
                .iter()
                .any(|key| row.get(*key).is_some_and(|v| !v.is_null()))
        {
            continue;
        }
        let kind = row.get("type")?.as_str()?;
        match kind {
            "progress" | "file-history-snapshot" | "last-prompt" | "summary" | "attachment" => {
                continue
            }
            "system" if row.get("subtype").and_then(|v| v.as_str()) != Some("turn_duration") => {
                continue
            }
            "assistant" | "user" | "system" => {}
            _ => return None,
        }
        if row.get("sessionId").and_then(|v| v.as_str()) != Some(native) {
            return None;
        }
        let stamp = row.get("timestamp")?.as_str()?;
        if !stamp.ends_with('Z') {
            return None;
        }
        let at = crate::workflow::history::iso8601_to_ms(stamp)? as f64 / 1000.0;
        if at > now {
            // Writers can append just after the tick sampled `now`. Do not
            // permanently cache a negative verdict for that unchanged file.
            *retry_at = Some(at);
            return None;
        }
        if prior_at.is_some_and(|last| at < last) {
            return None;
        }
        // A bounded tail often includes history before this incarnation/turn.
        // Skip only the prefix; an older event after a current event is invalid.
        if at < key.floor {
            continue;
        }
        prior_at = Some(at);
        if at > key.at() && !chose {
            chose = true;
            selected = if let Some(id) = &key.tool_id {
                calls.get(id).filter(|c| c.foreground).map(|_| id.clone())
            } else if anchored && calls.len() == 1 {
                calls
                    .iter()
                    .next()
                    .filter(|(_, c)| c.foreground)
                    .map(|(id, _)| id.clone())
            } else {
                None
            };
            if selected
                .as_ref()
                .and_then(|id| calls.get(id))
                .is_some_and(|c| key.tool_name.as_ref().is_some_and(|name| *name != c.name))
            {
                return None;
            }
        }
        if row.get("isMeta").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        let content = row.pointer("/message/content");
        if kind == "system"
            || (kind == "user"
                && content.is_some_and(|c| {
                    c.is_string()
                        || c.as_array()
                            .is_some_and(|a| a.iter().any(|b| b["type"] == "text"))
                }))
        {
            if at <= key.at() {
                anchored = true;
                calls.clear();
            } else if resolved.is_none() {
                return None;
            }
            continue;
        }
        let Some(blocks) = content.and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            match (kind, block.get("type").and_then(|v| v.as_str())) {
                ("assistant", Some("tool_use")) if at <= key.at() => {
                    let id = short(block.get("id"))?;
                    let name = short(block.get("name"))?;
                    if calls.contains_key(id) {
                        return None;
                    }
                    let foreground = block
                        .pointer("/input/run_in_background")
                        .is_none_or(|v| v == false)
                        && !name.contains("__chat_")
                        && name != "SendMessage";
                    calls.insert(
                        id.into(),
                        Call {
                            name: name.into(),
                            foreground,
                        },
                    );
                }
                ("user", Some("tool_result")) => {
                    let id = short(block.get("tool_use_id"))?;
                    if at <= key.at() {
                        calls.remove(id);
                    } else if selected.as_deref() == Some(id) {
                        resolved.get_or_insert(at);
                    }
                }
                _ => {}
            }
        }
    }
    resolved
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::{json, Value};
    pub(crate) fn row(kind: &str, at: i64, content: Value) -> Value {
        json!({"type": kind, "sessionId":"transcript", "isSidechain":false,
            "timestamp":chrono::DateTime::from_timestamp(at,0).unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis,true),
            "message":{"role":kind,"content":content}})
    }
    pub(crate) fn call(id: &str, name: &str, background: bool) -> Value {
        json!({"type":"tool_use","id":id,"name":name,"input":{"run_in_background":background}})
    }
    pub(crate) fn result(id: &str) -> Value {
        json!({"type":"tool_result","tool_use_id":id,"content":"done"})
    }
    pub(crate) fn log(rows: &[Value]) -> String {
        rows.iter().map(|v| v.to_string() + "\n").collect()
    }
    fn key(exact: bool) -> Key {
        Key {
            hook_at: Some(990.0),
            presence_at: Some(990.0),
            tool_id: exact.then(|| "a".into()),
            tool_name: Some("Agent".into()),
            floor: 950.0,
        }
    }
    pub(crate) fn rows() -> Vec<Value> {
        vec![
            row("user", 950, json!("task")),
            row("assistant", 985, json!([call("a", "Agent", false)])),
            row("user", 991, json!([result("a")])),
        ]
    }
    fn resolve(rows: &[Value], key: &Key) -> Option<f64> {
        resolved_at(
            &log(rows),
            Path::new("transcript.jsonl"),
            key,
            1000.0,
            &mut None,
        )
    }

    #[test]
    fn needs_human81_exact_and_unique_legacy_match_only_the_waiting_tool() {
        for exact in [false, true] {
            assert_eq!(resolve(&rows(), &key(exact)), Some(991.0));
            let mut other = rows();
            other[2] = row("user", 991, json!([result("parallel")]));
            assert_eq!(resolve(&other, &key(exact)), None);
        }
        let mut parallel = rows();
        parallel[1]["message"]["content"]
            .as_array_mut()
            .unwrap()
            .push(call("parallel", "Bash", false));
        assert_eq!(
            resolve(&parallel, &key(false)),
            None,
            "legacy ambiguity stays blocked"
        );
        assert_eq!(
            resolve(&parallel, &key(true)),
            Some(991.0),
            "exact waiting ID disambiguates"
        );
        parallel[2] = row("user", 991, json!([result("parallel")]));
        assert_eq!(
            resolve(&parallel, &key(true)),
            None,
            "other parallel result is not resolution"
        );
        parallel[1]["message"]["content"][1]["input"]["run_in_background"] = true.into();
        parallel[2] = rows()[2].clone();
        assert_eq!(
            resolve(&parallel, &key(false)),
            None,
            "an outstanding background call also makes legacy attribution ambiguous"
        );
        let mut mismatch = key(true);
        mismatch.tool_name = Some("Bash".into());
        assert_eq!(resolve(&rows(), &mismatch), None);
        assert_eq!(
            resolve(&rows()[1..], &key(false)),
            None,
            "legacy requires a visible turn boundary"
        );
        assert_eq!(resolve(&rows()[1..], &key(true)), Some(991.0));
    }

    #[test]
    fn needs_human81_unrelated_subagent_background_chat_and_meta_do_not_resolve() {
        for field in [
            "isSidechain",
            "agentId",
            "agent_id",
            "parentToolUseID",
            "parentToolUseId",
            "isMeta",
        ] {
            let mut other = rows();
            other[2][field] = if matches!(field, "isSidechain" | "isMeta") {
                json!(true)
            } else {
                json!("child")
            };
            assert_eq!(resolve(&other, &key(true)), None, "{field}");
        }
        for (name, bg) in [
            ("Agent", true),
            ("mcp__cm__chat_read", false),
            ("SendMessage", false),
        ] {
            let mut other = rows();
            other[1]["message"]["content"] = json!([call("a", name, bg)]);
            let mut k = key(true);
            k.tool_name = None;
            assert_eq!(resolve(&other, &k), None, "{name} background={bg}");
        }
        for last in [
            row(
                "assistant",
                991,
                json!([{"type":"text","text":"still working"}]),
            ),
            row("assistant", 991, json!([call("next", "Bash", false)])),
            row("user", 991, json!("chat arrived")),
            json!({"type":"progress","timestamp":"1970-01-01T00:16:31Z"}),
        ] {
            let mut other = rows();
            other[2] = last;
            assert_eq!(resolve(&other, &key(true)), None);
        }
        let mut other = rows();
        other[2]["sessionId"] = "foreign".into();
        assert_eq!(resolve(&other, &key(true)), None);
        other = rows();
        other[1]["isSidechain"] = true.into();
        assert_eq!(
            resolve(&other, &key(true)),
            None,
            "sidechain call cannot be correlated to main result"
        );
    }

    #[test]
    fn needs_human81_event_order_incarnation_and_new_wait_are_fenced() {
        let mut history = vec![row("user", 100, json!("previous incarnation"))];
        history.extend(rows());
        assert_eq!(
            resolve(&history, &key(true)),
            Some(991.0),
            "old prefix doesn't poison a current proof"
        );
        for at in [949, 984, 990, 1001] {
            let mut other = rows();
            other[2] = row("user", at, json!([result("a")]));
            assert_eq!(
                resolve(&other, &key(true)),
                None,
                "stale/equal/future result {at}"
            );
        }
        let mut out_of_order = rows();
        out_of_order.push(row("assistant", 986, json!([])));
        assert_eq!(resolve(&out_of_order, &key(true)), None);
        let mut newer = key(true);
        newer.hook_at = Some(995.0);
        assert_eq!(
            resolve(&rows(), &newer),
            None,
            "old result cannot clear newer approval"
        );
        newer = key(true);
        newer.floor = 995.0;
        assert_eq!(
            resolve(&rows(), &newer),
            None,
            "old incarnation/turn cannot resolve"
        );
    }

    #[test]
    fn needs_human81_bounded_witness_survives_append_and_restart_but_not_replacement() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(&path, log(&rows())).unwrap();
        let mut reader = Reader::default();
        let proof = reader
            .observe("uid", (1001, Some(42)), &path, key(true), None, 1000.0)
            .unwrap();
        let padding = (row("progress", 992, json!([])).to_string() + "\n").repeat(4000);
        assert!(padding.len() > MAX_BYTES as usize);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(padding.as_bytes())
            .unwrap();
        // The proof is bounded and contains no transcript payload in the sidecar.
        let saved = serde_json::to_vec(&proof).unwrap();
        assert!(saved.len() < 1024);
        assert!(!String::from_utf8_lossy(&saved).contains("completed"));
        let prior: Resolution = serde_json::from_slice(&saved).unwrap();
        assert!(
            !prior.verified,
            "restart must revalidate before deriving state"
        );
        let mut restarted = Reader::default();
        assert_eq!(
            restarted
                .observe(
                    "uid",
                    (1001, Some(42)),
                    &path,
                    key(true),
                    Some(&prior),
                    1000.0
                )
                .unwrap()
                .at,
            991.0
        );
        assert!(Reader::default()
            .observe(
                "uid",
                (1001, Some(43)),
                &path,
                key(true),
                Some(&prior),
                1000.0
            )
            .is_none());
        let rebound = dir.path().join("other.jsonl");
        std::fs::copy(&path, &rebound).unwrap();
        assert!(reader
            .observe(
                "uid",
                (1001, Some(42)),
                &rebound,
                key(true),
                Some(&prior),
                1000.0
            )
            .is_none());
        // Same path and bytes on a different inode must not reuse the old proof.
        std::fs::rename(&rebound, &path).unwrap();
        assert!(reader
            .observe(
                "uid",
                (1001, Some(42)),
                &path,
                key(true),
                Some(&prior),
                1000.0
            )
            .is_none());
        std::fs::write(&path, "{}\n").unwrap();
        assert!(reader
            .observe(
                "uid",
                (1001, Some(42)),
                &path,
                key(true),
                Some(&prior),
                1000.0
            )
            .is_none());
        std::fs::write(&path, log(&rows())).unwrap();
        let proof = reader
            .observe("uid", (1001, Some(42)), &path, key(true), None, 1000.0)
            .unwrap();
        let mut corrupted = rows();
        corrupted[2]["message"]["content"][0]["tool_use_id"] = "b".into();
        std::fs::write(&path, log(&corrupted)).unwrap();
        assert!(reader
            .observe(
                "uid",
                (1001, Some(42)),
                &path,
                key(true),
                Some(&proof),
                1000.0
            )
            .is_none());
    }

    #[test]
    fn needs_human81_future_result_is_rechecked_without_file_growth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut future = rows();
        future[2] = row("user", 1001, json!([result("a")]));
        std::fs::write(&path, log(&future)).unwrap();
        let mut reader = Reader::default();
        assert!(reader
            .observe("uid", (1001, Some(42)), &path, key(true), None, 1000.0)
            .is_none());
        assert!(reader
            .observe("uid", (1001, Some(42)), &path, key(true), None, 1000.5)
            .is_none());
        assert_eq!(
            reader
                .observe("uid", (1001, Some(42)), &path, key(true), None, 1001.0)
                .unwrap()
                .at,
            1001.0
        );
    }

    #[test]
    fn needs_human81_reader_rejects_symlink_incomplete_tail_and_excess_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut reader = Reader::default();
        let text = log(&rows());
        std::fs::write(&path, text.trim_end()).unwrap();
        assert!(reader
            .observe("uid", (1001, Some(42)), &path, key(true), None, 1000.0)
            .is_none());
        std::fs::write(&path, &text).unwrap();
        let link = dir.path().join("link.jsonl");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(reader
            .observe("uid", (1001, Some(42)), &link, key(true), None, 1000.0)
            .is_none());
        assert!(reader
            .observe("uid", (1001, Some(42)), dir.path(), key(true), None, 1000.0)
            .is_none());
        let mut many = rows();
        many.extend((0..MAX_LINES).map(|_| row("progress", 992, json!([]))));
        std::fs::write(&path, log(&many)).unwrap();
        assert!(
            reader
                .observe("uid", (1001, Some(42)), &path, key(true), None, 1000.0)
                .is_none(),
            "no unbounded scan for old proof"
        );
        reader.retain(&Default::default());
        assert!(reader.cache.is_empty());
    }
}
