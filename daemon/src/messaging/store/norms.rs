use super::personal::NormPage;
use super::*;

const PAGE_CHARS: usize = 12_000;

/// A valid unified diff with a single changed hunk. Keeping unchanged prefixes
/// and suffixes as context bounds work even for adversarial 32 KiB documents;
/// distant edits may share a hunk, but no text is invented or omitted.
pub fn textual_diff(old: &str, new: &str, base: &str, revision: &str) -> String {
    if old == new {
        return String::new();
    }
    let a: Vec<_> = old.split_inclusive('\n').collect();
    let b: Vec<_> = new.split_inclusive('\n').collect();
    let prefix = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let start = prefix.saturating_sub(3);
    let a_end = a.len() - suffix;
    let b_end = b.len() - suffix;
    let tail = suffix.min(3);
    let mut out = format!(
        "--- {base}\n+++ {revision}\n@@ -{},{} +{},{} @@\n",
        if a.is_empty() { 0 } else { start + 1 },
        a_end + tail - start,
        if b.is_empty() { 0 } else { start + 1 },
        b_end + tail - start
    );
    let mut push = |prefix: char, line: &str| {
        out.push(prefix);
        out.push_str(line);
        if !line.ends_with('\n') {
            out.push_str("\n\\ No newline at end of file\n");
        }
    };
    for line in &a[start..prefix] {
        push(' ', line);
    }
    for line in &a[prefix..a_end] {
        push('-', line);
    }
    for line in &b[prefix..b_end] {
        push('+', line);
    }
    for line in &b[b_end..b_end + tail] {
        push(' ', line);
    }
    out
}

impl Store {
    fn norm_event(&self, revision: &str) -> Option<&Published> {
        self.events.iter().find(|e| {
            e.event["type"] == "norms.update"
                && e.event["data"]["scope"] == "global"
                && e.event["data"]["revision"] == revision
        })
    }

    pub fn acknowledge_norms(&mut self, actor: &str, revision: &str) -> Result<()> {
        let mut state = self.personal_state(actor);
        if !state.norms_supplied.contains(revision) {
            return Err(err(
                "context_not_supplied",
                "Read the complete norms text or diff before acknowledging this revision",
            ));
        }
        let position = self
            .norm_event(revision)
            .ok_or_else(|| err("not_found", "Norms revision not found"))?
            .position;
        let previous = state
            .norms_ack
            .as_deref()
            .and_then(|r| self.norm_event(r))
            .map(|e| e.position)
            .unwrap_or(0);
        if position > previous {
            state.norms_ack = Some(revision.into());
            self.save_personal(state)?;
        }
        Ok(())
    }

    pub fn norms_status(&self, actor: &str) -> Value {
        let ack = self
            .personal
            .get(actor)
            .and_then(|s| s.norms_ack.as_deref());
        json!({"current":{"global":self.norms["revision"]},"acknowledged":{"global":ack},"changed":ack != self.norms["revision"].as_str(),"tool":"chat_norms","scope":"global"})
    }

    /// Called only by messaging RPCs, after their primary outcome is known.
    /// Missing context is information, never a reason to reject a durable send.
    pub fn context_response(
        &mut self,
        actor: &str,
        p: &Value,
        mut result: Value,
        force: bool,
    ) -> Value {
        let mut context_error = None;
        if let Some(revision) = p["norms_seen"]["global"].as_str() {
            if let Err(e) = self.acknowledge_norms(actor, revision) {
                context_error = Some(e.to_string());
            }
        }
        let mut state = self.personal_state(actor);
        let revision = strv(&self.norms, "revision").to_owned();
        // A used to duplicate the whole document in nested preview responses.
        if let Some(object) = result.as_object_mut() {
            object.remove("norms");
        }
        if let Some(recent) = result.get_mut("recent").and_then(Value::as_object_mut) {
            recent.remove("norms");
        }
        let offered = state.norms_offered.contains(&revision);
        if force || !offered {
            let text = strv(&self.norms, "text");
            let current_chars = serde_json::to_string(&result)
                .unwrap_or_default()
                .chars()
                .count();
            if text.chars().count() + current_chars < 15_000 {
                result["norms"] = self.norms.clone();
                result["norms"]["complete"] = json!(true);
                state.norms_supplied.insert(revision.clone());
            } else {
                result["norms"] = json!({"revision":revision,"scope":"global","complete":false,"read_with":"chat_norms(action=read)"});
            }
            if !offered {
                state.norms_offered.insert(revision);
                if let Err(e) = self.save_personal(state) {
                    context_error = Some(e.to_string());
                }
            } else if force
                && !self
                    .personal
                    .get(actor)
                    .is_some_and(|s| s.norms_supplied.contains(&revision))
                && result["norms"]["complete"] == true
            {
                if let Err(e) = self.save_personal(state) {
                    context_error = Some(e.to_string());
                }
            }
        }
        result["context_status"] = self.norms_status(actor);
        if let Some(error) = context_error {
            result["context_status"]["acknowledgement_note"] = json!(error);
        }
        result
    }

    pub fn norms_document(&mut self, actor: &str, p: &Value) -> Result<Value> {
        if p["scope"].as_str().is_some_and(|s| s != "global") {
            return Err(err("unsupported_feature", "Only global norms are enabled"));
        }
        if let Some(revision) = p["ack_revision"].as_str() {
            self.acknowledge_norms(actor, revision)?;
        }
        let action = p["action"].as_str().unwrap_or("read");
        if action == "history" {
            let cursor = p.get("cursor").filter(|v| !v.is_null());
            let high = cursor
                .map(|c| self.check_position(&c["snapshot"]))
                .transpose()?
                .unwrap_or(self.position);
            if cursor.is_some_and(|c| c["kind"] != "norms_history") {
                return Err(err("invalid_cursor", "Not a norms history cursor"));
            }
            let last = cursor
                .and_then(|c| c["last_position"].as_u64())
                .unwrap_or(u64::MAX);
            let items: Vec<Value> = self.events.iter().rev().filter(|e| e.position <= high && e.position < last && e.event["type"] == "norms.update" && e.event["data"]["scope"] == "global").map(|e| {
                json!({"id":e.event["data"]["revision"],"revision":e.event["data"]["revision"],"parent_revision":e.event["data"]["parent_revision"],"summary":e.event["data"]["summary"].as_str().unwrap_or("Initial shared norms"),"actor":e.event["actor"],"created_at":e.event["created_at"],"position":e.position})
            }).collect();
            let limit = p["limit"].as_u64().unwrap_or(50).clamp(1, 200) as usize;
            let mut out = Vec::new();
            let mut chars = 0;
            for item in &items {
                let size = item.to_string().chars().count();
                if out.len() == limit || chars + size > 14000 {
                    break;
                }
                chars += size;
                out.push(item.clone());
            }
            let next = if out.len() < items.len() {
                json!({"kind":"norms_history","snapshot":self.position_token(high),"last_position":out.last().map(|i|i["position"].clone())})
            } else {
                Value::Null
            };
            return Ok(
                json!({"items":out,"next_cursor":next,"position":self.position_token(high)}),
            );
        }
        if !["read", "diff"].contains(&action) {
            return Err(err("invalid_params", "Expected read, diff or history"));
        }
        let cursor = p.get("cursor").filter(|v| !v.is_null());
        let mut state = self.personal_state(actor);
        let (token, mut page, offset) = if let Some(cursor) = cursor {
            self.check_position(&cursor["snapshot"])?;
            let token = required(cursor, "token")?;
            let page = state
                .norm_pages
                .get(&token)
                .cloned()
                .ok_or_else(|| err("invalid_cursor", "Unknown norms page or wrong reader"))?;
            let offset = cursor["offset"]
                .as_u64()
                .ok_or_else(|| err("invalid_cursor", "Missing page offset"))?
                as usize;
            if page.action != action || offset > page.through {
                return Err(err(
                    "invalid_cursor",
                    "Norms pages cannot skip unseen content",
                ));
            }
            (token, page, offset)
        } else {
            let revision = p["revision"]
                .as_str()
                .unwrap_or(strv(&self.norms, "revision"))
                .to_owned();
            let since = if action == "diff" {
                p["since"]["global"]
                    .as_str()
                    .or_else(|| p["since"].as_str())
                    .map(str::to_owned)
                    .or_else(|| state.norms_ack.clone())
            } else {
                None
            };
            (
                uuid(),
                NormPage {
                    revision,
                    since,
                    action: action.into(),
                    digest: String::new(),
                    through: 0,
                },
                0,
            )
        };
        let target = self
            .norm_event(&page.revision)
            .ok_or_else(|| err("not_found", "Norms revision not found"))?
            .event
            .clone();
        let base = page.since.as_deref().and_then(|r| self.norm_event(r));
        let fallback = action == "diff" && base.is_none();
        let text = if action == "diff" && !fallback {
            textual_diff(
                strv(&base.unwrap().event["data"], "text"),
                strv(&target["data"], "text"),
                page.since.as_deref().unwrap(),
                &page.revision,
            )
        } else {
            strv(&target["data"], "text").into()
        };
        let digest = hash(text.as_bytes());
        if !page.digest.is_empty() && page.digest != digest {
            return Err(err("invalid_cursor", "Norms document changed unexpectedly"));
        }
        page.digest = digest;
        let chars: Vec<char> = text.chars().collect();
        if offset > chars.len() {
            return Err(err("invalid_cursor", "Page offset out of bounds"));
        }
        let limit = p["limit_chars"]
            .as_u64()
            .unwrap_or(PAGE_CHARS as u64)
            .clamp(1, PAGE_CHARS as u64) as usize;
        let end = (offset + limit).min(chars.len());
        let body: String = chars[offset..end].iter().collect();
        page.through = page.through.max(end);
        let complete = end == chars.len() && page.through == chars.len();
        if complete {
            state.norms_supplied.insert(page.revision.clone());
        }
        state.norm_pages.insert(token.clone(), page.clone());
        self.save_personal(state)?;
        Ok(
            json!({"scope":"global","revision":page.revision,"since":page.since,"format":if action == "diff" && !fallback {"diff"}else{"markdown"},"base_unavailable":fallback,"text":body,"offset":offset,"total_chars":chars.len(),"complete":complete,"next_cursor":if complete {Value::Null}else{json!({"snapshot":self.position_token(self.position),"token":token,"offset":end})},"actor":target["actor"],"summary":target["data"]["summary"],"created_at":target["created_at"],"ack_revision":if complete {json!(page.revision)}else{Value::Null}}),
        )
    }

    pub fn change_norms(
        &mut self,
        actor: &str,
        kind: &str,
        name: &str,
        p: &Value,
    ) -> Result<Value> {
        let action = required(p, "action")?;
        if !["publish", "revert"].contains(&action.as_str()) {
            return Err(err("invalid_params", "Expected publish or revert"));
        }
        let (key, digest, prior) = self.request(actor, p, &format!("norms.{action}"))?;
        if let Some(event) = prior {
            return Ok(
                json!({"status":"published","revision":event["data"]["revision"],"event_id":event["id"]}),
            );
        }
        if p["scope"].as_str().is_some_and(|s| s != "global") {
            return Err(err("unsupported_feature", "Only global norms are enabled"));
        }
        let expected = required(p, "expected_revision")?;
        let summary = required(p, "summary")?;
        if summary.trim().is_empty() || summary.chars().count() > 500 {
            return Err(err("invalid_params", "Give a summary of 1–500 characters"));
        }
        let text = if action == "revert" {
            let revision = required(p, "revision")?;
            self.norm_event(&revision)
                .map(|e| strv(&e.event["data"], "text").to_owned())
                .ok_or_else(|| err("not_found", "Revert revision not found"))?
        } else {
            p["text"]
                .as_str()
                .ok_or_else(|| err("invalid_params", "text must be a string"))?
                .replace("\r\n", "\n")
        };
        if text.len() > 32768 {
            return Err(err(
                "norms_too_long",
                "Norms are limited to 32 KiB UTF-8; retain the draft and reference a longer file",
            ));
        }
        if self.norms["revision"] != expected {
            let mut conflict =
                self.norms_document(actor, &json!({"action":"diff","since":{"global":expected}}))?;
            conflict["status"] = json!("conflict");
            conflict["current_revision"] = self.norms["revision"].clone();
            conflict["draft_preserved"] = json!(true);
            return Ok(conflict);
        }
        let revision = uuid();
        let data = json!({"scope":"global","revision":revision,"parent_revision":expected,"text":text,"summary":summary,"reverted_from":if action == "revert" {p["revision"].clone()}else{Value::Null}});
        let event = self.publish(
            "norms.update",
            None,
            &format!("{name} updated global norms: {summary}"),
            data,
            actor,
            name,
            kind,
            &key,
            &digest,
        )?;
        let projection = write_file(
            &self.root.join("NORMS.md"),
            strv(&self.norms, "text").as_bytes(),
            true,
        )
        .err()
        .map(|e| e.to_string());
        Ok(
            json!({"status":"published","revision":revision,"event_id":event["id"],"projection_error":projection}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn messaging_norms_conflict_revert_and_partial_ack_survive_restart() {
        let t = tempfile::tempdir().unwrap();
        let mut s = Store::open(t.path()).unwrap();
        let initial = s.norms["revision"].clone();
        let p = json!({"action":"publish","text":"New shared convention.\n","expected_revision":initial,"summary":"Define a convention","request_id":"norms-change"});
        let changed = s.change_norms("owner", "owner", "Owner", &p).unwrap();
        assert_eq!(
            s.change_norms("owner", "owner", "Owner", &p).unwrap()["revision"],
            changed["revision"]
        );
        let mut competing = p.clone();
        competing["request_id"] = json!("competing");
        let conflict = s
            .change_norms("owner", "owner", "Owner", &competing)
            .unwrap();
        assert_eq!(conflict["status"], "conflict");
        assert!(conflict["text"]
            .as_str()
            .unwrap()
            .contains("+New shared convention."));
        let mut request = json!({"action":"read","limit_chars":5});
        let mut page = s.norms_document("reader", &request).unwrap();
        assert!(!page["complete"].as_bool().unwrap());
        assert!(s
            .acknowledge_norms("reader", changed["revision"].as_str().unwrap())
            .is_err());
        while !page["next_cursor"].is_null() {
            request["cursor"] = page["next_cursor"].clone();
            page = s.norms_document("reader", &request).unwrap();
        }
        s.acknowledge_norms("reader", changed["revision"].as_str().unwrap())
            .unwrap();
        assert_eq!(s.norms_status("reader")["changed"], false);
        drop(s);
        let mut s = Store::open(t.path()).unwrap();
        assert_eq!(s.norms_status("reader")["changed"], false);
        let reverted = s.change_norms("owner","owner","Owner",&json!({"action":"revert","revision":initial,"expected_revision":changed["revision"],"summary":"Restore original conventions","request_id":"revert"})).unwrap();
        assert_ne!(reverted["revision"], initial);
        assert_eq!(s.norms["text"], super::super::super::NORMS);
    }
}
