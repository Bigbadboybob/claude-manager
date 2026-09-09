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
    /// Channel IDs, rather than display names, are the durable scope keys.
    pub(super) fn norms_scope(&self, p: &Value) -> Result<String> {
        if p.get("scope")
            .is_some_and(|s| !s.is_null() && !s.is_string())
        {
            return Err(err("invalid_scope", "scope must be a string"));
        }
        let scope = p["scope"].as_str().unwrap_or("global");
        if let Some(path) = p.get("channel").filter(|v| !v.is_null()) {
            let path = path
                .as_str()
                .ok_or_else(|| err("invalid_scope", "channel must be a path"))?;
            let id = self
                .channels
                .get(path.trim_start_matches('#'))
                .ok_or_else(|| err("not_found", "Channel not found"))?;
            let resolved = format!("channel:{id}");
            if scope != "global" && scope != resolved {
                return Err(err(
                    "invalid_scope",
                    "scope and channel refer to different documents",
                ));
            }
            return Ok(resolved);
        }
        if scope == "global"
            || scope
                .strip_prefix("channel:")
                .is_some_and(|id| self.channels.values().any(|c| c == id))
        {
            return Ok(scope.into());
        }
        Err(err(
            "invalid_scope",
            "Use global, channel:<channel-id>, or the channel path parameter",
        ))
    }

    fn norm_event(&self, scope: &str, revision: &str) -> Option<&Published> {
        self.events.iter().find(|e| {
            e.event["type"] == "norms.update"
                && e.event["data"]["scope"] == scope
                && e.event["data"]["revision"] == revision
        })
    }

    fn current_norms(&self, scope: &str) -> Value {
        if scope == "global" {
            return self.norms.clone();
        }
        self.channel_norms.get(scope).cloned().unwrap_or_else(|| {
            // The channel's immutable ID represents its initial empty document.
            json!({"scope":scope,"revision":scope.strip_prefix("channel:").unwrap(),"text":"","parent_revision":null})
        })
    }

    fn norm_revision(&self, scope: &str, revision: &str) -> Result<Value> {
        if let Some(event) = self.norm_event(scope, revision) {
            return Ok(event.event.clone());
        }
        if scope.strip_prefix("channel:") == Some(revision) {
            return Ok(
                json!({"data":{"scope":scope,"revision":revision,"text":"","summary":"No channel norms yet"}}),
            );
        }
        Err(err("not_found", "Norms revision not found in this scope"))
    }

    pub fn acknowledge_norms(&mut self, actor: &str, revision: &str) -> Result<()> {
        self.acknowledge_scoped_norms(actor, "global", revision)
    }

    fn acknowledge_scoped_norms(&mut self, actor: &str, scope: &str, revision: &str) -> Result<()> {
        self.norm_revision(scope, revision)?;
        let mut state = self.personal_state(actor);
        if !state.norms_supplied.contains(revision) {
            return Err(err(
                "context_not_supplied",
                "Read the complete norms text or diff before acknowledging this revision",
            ));
        }
        let position = self
            .norm_event(scope, revision)
            .map(|e| e.position)
            .unwrap_or(0);
        let previous = state
            .norms_ack_for(scope)
            .and_then(|r| self.norm_event(scope, r))
            .map(|e| e.position)
            .unwrap_or(0);
        if state.norms_ack_for(scope).is_none() || position > previous {
            if scope == "global" {
                state.norms_ack = Some(revision.into());
            } else {
                state
                    .channel_norms_ack
                    .insert(scope.into(), revision.into());
            }
            self.save_personal(state)?;
        }
        Ok(())
    }

    pub fn norms_status(&self, actor: &str) -> Value {
        self.scoped_norms_status(actor, &["global".into()])
    }

    fn scoped_norms_status(&self, actor: &str, scopes: &[String]) -> Value {
        let state = self.personal_state(actor);
        let mut current = json!({});
        let mut acknowledged = json!({});
        let mut stale = Vec::new();
        for scope in scopes {
            current[scope] = self.current_norms(scope)["revision"].clone();
            acknowledged[scope] = json!(state.norms_ack_for(scope));
            if current[scope] != acknowledged[scope] {
                stale.push(scope.clone());
            }
        }
        json!({"current":current,"acknowledged":acknowledged,"changed":!stale.is_empty(),"stale_scopes":stale,"tool":"chat_norms","scope":scopes.last()})
    }

    /// Messaging context never rejects the primary operation. Global norms and
    /// the exact selected channel apply; there is no implicit parent inheritance.
    pub fn context_response(
        &mut self,
        actor: &str,
        p: &Value,
        mut result: Value,
        force: bool,
    ) -> Value {
        let mut scopes = vec!["global".to_owned()];
        let channel_id = p["channel"]
            .as_str()
            .and_then(|path| self.channels.get(path))
            .map(String::as_str)
            .or_else(|| p["conversation"].as_str())
            .or_else(|| result["target"]["id"].as_str())
            .or_else(|| result["event"]["conversation_id"].as_str());
        if let Some(id) = channel_id.filter(|id| self.channels.values().any(|c| c == id)) {
            scopes.push(format!("channel:{id}"));
        }
        let selected_scope = result["scope"].as_str().or_else(|| p["scope"].as_str());
        if let Some(scope) = selected_scope.filter(|s| s.starts_with("channel:")) {
            if let Ok(scope) = self.norms_scope(&json!({"scope":scope})) {
                if !scopes.contains(&scope) {
                    scopes.push(scope);
                }
            }
        }
        let mut context_error = None;
        if let Some(seen) = p["norms_seen"].as_object() {
            for (scope, revision) in seen {
                let ack = self.norms_scope(&json!({"scope":scope})).and_then(|scope| {
                    let revision = revision
                        .as_str()
                        .ok_or_else(|| err("invalid_params", "Norms revisions must be strings"))?;
                    self.acknowledge_scoped_norms(actor, &scope, revision)
                });
                if let Err(e) = ack {
                    context_error = Some(e.to_string());
                }
            }
        }
        if let Some(object) = result.as_object_mut() {
            object.remove("norms");
        }
        if let Some(recent) = result.get_mut("recent").and_then(Value::as_object_mut) {
            recent.remove("norms");
        }
        let mut state = self.personal_state(actor);
        let mut dirty = false;
        for scope in &scopes {
            let doc = self.current_norms(scope);
            let revision = strv(&doc, "revision").to_owned();
            let offered = state.norms_offered.contains(&revision);
            if force || !offered {
                let key = if scope == "global" {
                    "norms"
                } else {
                    "channel_norms"
                };
                let chars = result.to_string().chars().count();
                if strv(&doc, "text").chars().count() + chars < 15_000 {
                    result[key] = doc;
                    result[key]["complete"] = json!(true);
                    dirty |= state.norms_supplied.insert(revision.clone());
                } else {
                    result[key] = json!({"revision":revision,"scope":scope,"complete":false,"read_with":{"tool":"chat_norms","action":"read","scope":scope}});
                }
                dirty |= state.norms_offered.insert(revision);
            }
        }
        if dirty {
            if let Err(e) = self.save_personal(state) {
                context_error = Some(e.to_string());
            }
        }
        result["context_status"] = self.scoped_norms_status(actor, &scopes);
        if let Some(error) = context_error {
            result["context_status"]["acknowledgement_note"] = json!(error);
        }
        result
    }

    pub fn norms_document(&mut self, actor: &str, p: &Value) -> Result<Value> {
        let scope = self.norms_scope(p)?;
        let current = self.current_norms(&scope);
        if let Some(revision) = p["ack_revision"].as_str() {
            self.acknowledge_scoped_norms(actor, &scope, revision)?;
        }
        let action = p["action"].as_str().unwrap_or("read");
        if action == "history" {
            let cursor = p.get("cursor").filter(|v| !v.is_null());
            let high = cursor
                .map(|c| self.check_position(&c["snapshot"]))
                .transpose()?
                .unwrap_or(self.position);
            if cursor.is_some_and(|c| {
                c["kind"] != "norms_history" || c["scope"].as_str().unwrap_or("global") != scope
            }) {
                return Err(err("invalid_cursor", "Not a norms history cursor"));
            }
            let last = cursor
                .and_then(|c| c["last_position"].as_u64())
                .unwrap_or(u64::MAX);
            let items: Vec<Value> = self.events.iter().rev().filter(|e| e.position <= high && e.position < last && e.event["type"] == "norms.update" && e.event["data"]["scope"] == scope).map(|e| {
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
                json!({"kind":"norms_history","scope":scope,"snapshot":self.position_token(high),"last_position":out.last().map(|i|i["position"].clone())})
            } else {
                Value::Null
            };
            return Ok(
                json!({"scope":scope,"items":out,"next_cursor":next,"position":self.position_token(high)}),
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
            if page.scope != scope || page.action != action || offset > page.through {
                return Err(err(
                    "invalid_cursor",
                    "Norms pages cannot skip unseen content",
                ));
            }
            (token, page, offset)
        } else {
            let revision = p["revision"]
                .as_str()
                .unwrap_or(strv(&current, "revision"))
                .to_owned();
            let since = if action == "diff" {
                p["since"][&scope]
                    .as_str()
                    .or_else(|| p["since"].as_str())
                    .map(str::to_owned)
                    .or_else(|| state.norms_ack_for(&scope).map(str::to_owned))
            } else {
                None
            };
            (
                uuid(),
                NormPage {
                    scope: scope.clone(),
                    revision,
                    since,
                    action: action.into(),
                    digest: String::new(),
                    through: 0,
                },
                0,
            )
        };
        let target = self.norm_revision(&scope, &page.revision)?;
        let base = page
            .since
            .as_deref()
            .and_then(|r| self.norm_revision(&scope, r).ok());
        let fallback = action == "diff" && base.is_none();
        let text = if action == "diff" && !fallback {
            textual_diff(
                strv(&base.as_ref().unwrap()["data"], "text"),
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
            json!({"scope":scope,"revision":page.revision,"since":page.since,"format":if action == "diff" && !fallback {"diff"}else{"markdown"},"base_unavailable":fallback,"text":body,"offset":offset,"total_chars":chars.len(),"complete":complete,"next_cursor":if complete {Value::Null}else{json!({"snapshot":self.position_token(self.position),"token":token,"offset":end})},"actor":target["actor"],"summary":target["data"]["summary"],"created_at":target["created_at"],"ack_revision":if complete {json!(page.revision)}else{Value::Null}}),
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
                json!({"scope":event["data"]["scope"],"status":"published","revision":event["data"]["revision"],"event_id":event["id"]}),
            );
        }
        let scope = self.norms_scope(p)?;
        let current = self.current_norms(&scope);
        let channel_id = scope.strip_prefix("channel:");
        if let Some(id) = channel_id {
            let (path, _) = self
                .channels
                .iter()
                .find(|(_, c)| c.as_str() == id)
                .unwrap();
            let mut channel = self.channel_info(path, id);
            self.channel_permissions(actor, &mut channel);
            if channel["can_edit"] != true {
                return Err(err(
                    "unauthorized",
                    "Only channel admins can edit norms unless agent editing is enabled",
                ));
            }
        }
        let expected = required(p, "expected_revision")?;
        let summary = required(p, "summary")?;
        if summary.trim().is_empty() || summary.chars().count() > 500 {
            return Err(err("invalid_params", "Give a summary of 1–500 characters"));
        }
        let text = if action == "revert" {
            let revision = required(p, "revision")?;
            strv(&self.norm_revision(&scope, &revision)?["data"], "text").to_owned()
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
        if current["revision"] != expected {
            let mut conflict = self.norms_document(
                actor,
                &json!({"scope":scope,"action":"diff","since":expected}),
            )?;
            conflict["status"] = json!("conflict");
            conflict["current_revision"] = current["revision"].clone();
            conflict["draft_preserved"] = json!(true);
            return Ok(conflict);
        }
        let revision = uuid();
        let data = json!({"scope":scope,"revision":revision,"parent_revision":expected,"text":text,"summary":summary,"reverted_from":if action == "revert" {p["revision"].clone()}else{Value::Null}});
        let label = channel_id
            .and_then(|id| self.channels.iter().find(|(_, c)| c.as_str() == id))
            .map(|(path, _)| format!("#{path}"))
            .unwrap_or_else(|| "global".into());
        let event = self.publish(
            "norms.update",
            channel_id,
            &format!("{name} updated {label} norms: {summary}"),
            data,
            actor,
            name,
            kind,
            &key,
            &digest,
        )?;
        let path = if let Some(id) = channel_id {
            let (path, _) = self
                .channels
                .iter()
                .find(|(_, c)| c.as_str() == id)
                .unwrap();
            self.root.join("channels").join(path).join("NORMS.md")
        } else {
            self.root.join("NORMS.md")
        };
        let projection = write_file(
            &path,
            strv(&self.current_norms(&scope), "text").as_bytes(),
            true,
        )
        .err()
        .map(|e| e.to_string());
        Ok(
            json!({"scope":scope,"status":"published","revision":revision,"event_id":event["id"],"projection_error":projection}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn channel(s: &mut Store, actor: &str, path: &str) -> Value {
        s.channel_action(
            actor,
            &json!({"action":"create","path":path,"request_id":format!("create-{path}")}),
            &[],
        )
        .unwrap()["channel"]
            .clone()
    }
    fn publish(s: &mut Store, actor: &str, scope: &str, text: &str, key: &str) -> Value {
        let expected = s.current_norms(scope)["revision"].clone();
        s.change_norms(actor, "agent", actor, &json!({"action":"publish","scope":scope,"text":text,"expected_revision":expected,"summary":key,"request_id":key})).unwrap()
    }
    #[test]
    fn messaging_channel_norms_permissions_empty_initial_revision_and_replay() {
        let t = tempfile::tempdir().unwrap();
        let mut s = Store::open(t.path()).unwrap();
        let global = s.norms.clone();
        let c = channel(&mut s, "creator", "work/parser");
        let scope = format!("channel:{}", c["id"].as_str().unwrap());
        let empty = s
            .norms_document("reader", &json!({"channel":"work/parser"}))
            .unwrap();
        assert_eq!(empty["revision"], c["id"]);
        assert_eq!(empty["text"], "");
        let mut p = json!({"action":"publish","channel":"work/parser","text":"Use short review claims.\n","expected_revision":empty["revision"],"summary":"Define claims","request_id":"write"});
        assert_eq!(
            s.change_norms("stranger", "agent", "Stranger", &p)
                .unwrap_err()
                .code,
            "unauthorized"
        );
        let first = s.change_norms("creator", "agent", "Creator", &p).unwrap();
        assert_eq!(
            s.change_norms("creator", "agent", "Creator", &p).unwrap()["revision"],
            first["revision"]
        );
        assert_eq!(s.norms, global);
        p["request_id"] = json!("competing");
        assert_eq!(
            s.change_norms("owner", "owner", "Owner", &p).unwrap()["status"],
            "conflict"
        );
        let edit = s.channel_action("owner", &json!({"action":"update","conversation":c["id"],"name":"Review room","allow_agent_edits":true,"expected_revision":c["revision"],"request_id":"allow"}), &[]).unwrap();
        assert_eq!(edit["channel"]["name"], "Review room");
        let second = publish(&mut s, "stranger", &scope, "Second rule.\n", "second");
        let cleared = s.change_norms("owner","owner","Owner",&json!({"action":"revert","scope":scope,"revision":empty["revision"],"expected_revision":second["revision"],"summary":"Clear channel norms","request_id":"clear"})).unwrap();
        assert_eq!(s.current_norms(&scope)["text"], "");
        assert_ne!(cleared["revision"], empty["revision"]);
        drop(s);
        let mut s = Store::open(t.path()).unwrap();
        assert_eq!(s.norms, global);
        assert_eq!(s.current_norms(&scope)["revision"], cleared["revision"]);
        assert_eq!(
            fs::read_to_string(s.root.join("channels/work/parser/NORMS.md")).unwrap(),
            ""
        );
        assert_eq!(
            s.norms_document("reader", &json!({"scope":scope,"action":"history"}))
                .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            s.norms_document("reader", &json!({"channel":"work"}))
                .unwrap()["text"],
            ""
        );
    }
    #[test]
    fn messaging_channel_norms_cursor_and_ack_are_isolated_and_survive_restart() {
        let t = tempfile::tempdir().unwrap();
        let mut s = Store::open(t.path()).unwrap();
        let a = channel(&mut s, "owner", "a");
        let b = channel(&mut s, "owner", "b");
        let sa = format!("channel:{}", a["id"].as_str().unwrap());
        let sb = format!("channel:{}", b["id"].as_str().unwrap());
        let first = publish(&mut s, "owner", &sa, "Longer channel A norms.\n", "first");
        publish(&mut s, "owner", &sb, "B convention.", "b");
        let mut page = s
            .norms_document("reader", &json!({"scope":sa,"limit_chars":5}))
            .unwrap();
        assert_eq!(
            s.norms_document("reader", &json!({"scope":sb,"cursor":page["next_cursor"]}))
                .unwrap_err()
                .code,
            "invalid_cursor"
        );
        assert!(s
            .acknowledge_scoped_norms("reader", &sa, first["revision"].as_str().unwrap())
            .is_err());
        let cursor = page["next_cursor"].clone();
        drop(s);
        let mut s = Store::open(t.path()).unwrap();
        page = s
            .norms_document("reader", &json!({"scope":sa,"cursor":cursor}))
            .unwrap();
        assert_eq!(page["complete"], true);
        s.acknowledge_scoped_norms("reader", &sa, first["revision"].as_str().unwrap())
            .unwrap();
        assert!(s
            .acknowledge_scoped_norms("reader", &sb, first["revision"].as_str().unwrap())
            .is_err());
        assert_eq!(
            s.scoped_norms_status("reader", &[sa.clone()])["changed"],
            false
        );
        assert_eq!(s.norms_status("reader")["changed"], true);
        assert_eq!(
            s.scoped_norms_status("reader", &[sb.clone()])["changed"],
            true
        );
        publish(&mut s, "owner", &sa, "A changed.\n", "second");
        let diff = s
            .norms_document("reader", &json!({"scope":sa,"action":"diff"}))
            .unwrap();
        assert_eq!(diff["since"], first["revision"]);
        assert!(diff["text"].as_str().unwrap().contains("+A changed."));
        let history = s
            .norms_document("reader", &json!({"scope":sa,"action":"history","limit":1}))
            .unwrap();
        assert_eq!(
            s.norms_document(
                "reader",
                &json!({"scope":sb,"action":"history","cursor":history["next_cursor"]})
            )
            .unwrap_err()
            .code,
            "invalid_cursor"
        );
        assert!(s.change_norms("owner","owner","Owner",&json!({"action":"revert","scope":sb,"revision":first["revision"],"expected_revision":s.current_norms(&sb)["revision"],"summary":"Wrong scope","request_id":"bad-revert"})).is_err());
    }
    #[test]
    fn messaging_channel_norms_context_budget_no_inheritance_and_exact_acknowledgement() {
        let t = tempfile::tempdir().unwrap();
        let mut s = Store::open(t.path()).unwrap();
        channel(&mut s, "owner", "parent/child");
        let sp = s.norms_scope(&json!({"channel":"parent"})).unwrap();
        let sc = s.norms_scope(&json!({"channel":"parent/child"})).unwrap();
        let parent = publish(&mut s, "owner", &sp, "Parent convention.", "parent");
        let first = s.context_response(
            "reader",
            &json!({"channel":"parent/child"}),
            json!({}),
            false,
        );
        assert_eq!(first["channel_norms"]["text"], "");
        assert!(first["context_status"]["current"].get(&sp).is_none());
        assert_eq!(first["norms"]["scope"], "global");
        let ack = json!({"channel":"parent/child","norms_seen":{"global":first["norms"]["revision"],(sc.clone()):first["channel_norms"]["revision"]}});
        assert_eq!(
            s.context_response("reader", &ack, json!({}), false)["context_status"]["changed"],
            false
        );
        let next = publish(&mut s, "owner", &sc, &"x".repeat(20000), "long");
        let notice = s.context_response(
            "reader",
            &json!({"conversation":s.channels["parent/child"]}),
            json!({}),
            false,
        );
        assert_eq!(notice["channel_norms"]["complete"], false);
        assert_eq!(notice["context_status"]["stale_scopes"], json!([sc]));
        assert!(s
            .acknowledge_scoped_norms("reader", &sp, parent["revision"].as_str().unwrap())
            .is_err());
        assert!(s
            .acknowledge_scoped_norms("reader", &sc, next["revision"].as_str().unwrap())
            .is_err());
        let stale = s.context_response(
            "reader",
            &json!({"channel":"parent/child","norms_seen":{(sc):"missing"}}),
            json!({"status":"sent"}),
            false,
        );
        assert_eq!(stale["status"], "sent");
        assert!(stale["context_status"]["acknowledgement_note"].is_string());
    }
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
