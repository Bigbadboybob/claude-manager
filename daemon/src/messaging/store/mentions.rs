//! Mention resolution for `messaging.send`: names (not just participant IDs)
//! in `mentions[]`, and `@Name` tokens in the body promoted to real mentions
//! when they name exactly one current member of the conversation. Anything
//! that cannot be promoted comes back as a warning, so a sender never again
//! believes a body `@Name` woke someone when it did not.
//!
//! The stored event is unchanged in shape: `data.mentions` holds participant
//! IDs only, so replication validation and recipient fan-out see exactly what
//! a structured-mention send produced before.
use super::*;

/// How a participant reference (an ID, a current name or an alias) resolved.
pub(super) enum ParticipantRef {
    Found(String),
    NotFound,
    Ambiguous,
}

/// Body words that look like broadcast mentions in other chat tools. They
/// are never promoted; `mention_here=true` is the explicit form.
const BROADCAST_TOKENS: [&str; 4] = ["here", "channel", "everyone", "all"];

/// Body mention outcome for one send: the final structured mention set plus
/// what the sender is told about it.
pub(super) struct MentionPlan {
    pub mentions: BTreeSet<String>,
    pub resolved: Vec<Value>,
    pub warnings: Vec<Value>,
}

/// `@token`s in a message body, in order of first appearance and without the
/// `@`. Skips fenced code blocks, inline code spans, email addresses and
/// path/URL segments (`a@b.c`, `https://x/@y`): an `@` only starts a token
/// when the character before it is not part of a word, path or address.
pub(super) fn body_mention_tokens(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    for line in body.split('\n') {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut in_code = false;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '`' {
                in_code = !in_code;
                i += 1;
                continue;
            }
            if in_code || c != '@' {
                i += 1;
                continue;
            }
            let prev = i.checked_sub(1).map(|j| chars[j]);
            if prev.is_some_and(|p| p.is_alphanumeric() || "_.-/@:+=\\".contains(p)) {
                i += 1;
                continue;
            }
            let start = i + 1;
            let mut end = start;
            while end < chars.len()
                && (chars[end].is_alphanumeric() || "-_.".contains(chars[end]))
            {
                end += 1;
            }
            let mut token: String = chars[start..end].iter().collect();
            while token.ends_with(['.', '-', '_']) {
                token.pop();
            }
            if !token.is_empty() && !out.iter().any(|t| name_key(t) == name_key(&token)) {
                out.push(token);
            }
            i = end.max(i + 1);
        }
    }
    out
}

impl Store {
    /// Resolve a participant reference the way DM recipients always have: an
    /// exact participant ID (or `owner`), else a current name or alias by
    /// normalized spelling, then by dashed key. A released name (a task's
    /// superseded orchestrator) yields to the participant holding it now.
    pub(super) fn resolve_participant_ref(&self, s: &str, people: &[Person]) -> ParticipantRef {
        if s == "owner" || self.names.contains_key(s) || people.iter().any(|x| x.id == s) {
            return ParticipantRef::Found(s.to_owned());
        }
        let mut hits: Vec<_> = self
            .names
            .iter()
            .filter(|(_, n)| {
                normalize(&n.name) == normalize(s)
                    || n.aliases.iter().any(|a| normalize(a) == normalize(s))
            })
            .collect();
        // Honor exact legacy names/aliases first: before this policy,
        // "Build Scout" and "Build-Scout" could belong to different IDs.
        if hits.is_empty() {
            hits = self
                .names
                .iter()
                .filter(|(_, n)| {
                    name_key(&n.name) == name_key(s)
                        || n.aliases.iter().any(|a| name_key(a) == name_key(s))
                })
                .collect();
        }
        if hits.len() > 1 && hits.iter().any(|(_, n)| !n.released) {
            hits.retain(|(_, n)| !n.released);
        }
        match hits.len() {
            0 => ParticipantRef::NotFound,
            1 => ParticipantRef::Found(hits[0].0.clone()),
            _ => ParticipantRef::Ambiguous,
        }
    }

    pub(super) fn participant_display_name(&self, id: &str, people: &[Person]) -> String {
        if id == "owner" {
            return "Owner".into();
        }
        self.names
            .get(id)
            .map(|n| n.name.clone())
            .or_else(|| people.iter().find(|p| p.id == id).map(|p| p.name.clone()))
            .unwrap_or_else(|| id.to_owned())
    }

    /// Build the structured mention set for a send. `dm_members` is the DM's
    /// fixed membership (None for a channel, whose current members come from
    /// the membership map). Errors only for the explicit `mentions[]`
    /// parameter, as before; body tokens never fail a send.
    pub(super) fn plan_mentions(
        &self,
        actor: &str,
        conv: &str,
        dm_members: Option<&Vec<String>>,
        p: &Value,
        body: &str,
        people: &[Person],
    ) -> Result<MentionPlan> {
        let mut mentions = BTreeSet::new();
        let mut resolved = Vec::new();
        let mut warnings = Vec::new();
        for raw in p["mentions"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            let reference = raw.strip_prefix('@').unwrap_or(raw);
            let id = match self.resolve_participant_ref(reference, people) {
                ParticipantRef::Found(id) => id,
                ParticipantRef::NotFound => {
                    return Err(err("not_found", "Mention participant not found"))
                }
                ParticipantRef::Ambiguous => {
                    return Err(err(
                        "ambiguous_mention",
                        format!("'{raw}' names more than one participant; use chat_people and pass the participant ID"),
                    ))
                }
            };
            if dm_members.is_some_and(|v| !v.iter().any(|x| *x == id)) {
                return Err(err(
                    "invalid_mention",
                    "DM mentions are restricted to its members",
                ));
            }
            if mentions.insert(id.clone()) {
                resolved.push(json!({"token":raw,"id":id,"name":self.participant_display_name(&id, people),"source":"param"}));
            }
        }
        let members: Option<BTreeSet<String>> = match dm_members {
            Some(m) => Some(m.iter().cloned().collect()),
            None => self.memberships.get(conv).cloned(),
        };
        for token in body_mention_tokens(body) {
            let at = format!("@{token}");
            if BROADCAST_TOKENS.iter().any(|b| b.eq_ignore_ascii_case(&token)) {
                warnings.push(json!({"code":"body_broadcast_not_promoted","token":at,
                    "hint":if dm_members.is_some(){"DMs already notify their members"}else{"Body text does not notify the channel; pass mention_here=true to notify current members"}}));
                continue;
            }
            if name_key(&token) == "owner" {
                warnings.push(json!({"code":"owner_not_promoted","token":at,
                    "hint":"Body text never notifies Owner; use notify_user when Owner action is needed"}));
                continue;
            }
            let id = match self.resolve_participant_ref(&token, people) {
                ParticipantRef::Found(id) => id,
                ParticipantRef::NotFound => {
                    warnings.push(json!({"code":"unresolved_body_mention","token":at,
                        "hint":"No participant has this name, so nobody was notified; resolve the recipient with chat_people and pass mentions=[\"<participant id>\"]"}));
                    continue;
                }
                ParticipantRef::Ambiguous => {
                    warnings.push(json!({"code":"ambiguous_body_mention","token":at,
                        "hint":"This name matches more than one participant, so nobody was notified; resolve with chat_people and pass mentions=[\"<participant id>\"]"}));
                    continue;
                }
            };
            if id == actor || mentions.contains(&id) {
                continue;
            }
            let name = self.participant_display_name(&id, people);
            if !members.as_ref().is_some_and(|m| m.contains(&id)) {
                warnings.push(json!({"code":"body_mention_not_member","token":at,"id":id,"name":name,
                    "hint":if dm_members.is_some(){"Not a member of this DM, so not notified; message them directly"}else{"Not a member of this channel, so not notified; pass mentions=[\"<participant id>\"] to notify them anyway"}}));
                continue;
            }
            if mentions.len() >= 32 {
                warnings.push(json!({"code":"too_many_mentions","token":at,"id":id,"name":name,
                    "hint":"A message can mention at most 32 participants; this one was not notified"}));
                continue;
            }
            mentions.insert(id.clone());
            resolved.push(json!({"token":at,"id":id,"name":name,"source":"body"}));
        }
        Ok(MentionPlan {
            mentions,
            resolved,
            warnings,
        })
    }
}

impl Store {
    /// A retry returns the stored event unchanged; re-derive its mention
    /// report (best effort) so the sender sees the same answer. Only IDs that
    /// the stored event actually mentions are reported as resolved.
    pub(super) fn annotate_retry_mentions(
        &self,
        actor: &str,
        p: &Value,
        people: &[Person],
        e: &Value,
        result: &mut Value,
    ) {
        let conv = strv(e, "conversation_id");
        let stored: BTreeSet<&str> = e["data"]["mentions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let dm_members = self.conversations.get(conv);
        if let Ok(plan) = self.plan_mentions(actor, conv, dm_members, p, strv(e, "body"), people) {
            let resolved = plan
                .resolved
                .into_iter()
                .filter(|r| r["id"].as_str().is_some_and(|id| stored.contains(id)))
                .collect();
            annotate(result, resolved, plan.warnings);
        }
    }
}

/// Attach the mention report to a send response (omitted when empty, so
/// ordinary sends keep their shape).
pub(super) fn annotate(result: &mut Value, resolved: Vec<Value>, warnings: Vec<Value>) {
    if !resolved.is_empty() {
        result["mentions_resolved"] = json!(resolved);
    }
    if !warnings.is_empty() {
        result["warnings"] = json!(warnings);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(s: &Store, uid: &str) -> Person {
        Person {
            id: s.participant_id(uid),
            name: uid.into(),
            session_uid: uid.into(),
            task: None,
            present: true,
            kind: "agent".into(),
        }
    }
    /// Enroll every person under a chosen name with a first #general post.
    fn enrolled(s: &mut Store, people: &[Person], names: &[&str]) {
        for (p, name) in people.iter().zip(names) {
            s.send(&p.id, &p.session_uid, "agent",
                &json!({"channel":"general","body":"ready","request_id":format!("hello-{}",p.session_uid),"name":name}),
                people).unwrap();
        }
    }
    fn say(s: &mut Store, from: &Person, target: Value, body: &str, key: &str, people: &[Person]) -> Value {
        let mut q = target;
        q["body"] = json!(body);
        q["request_id"] = json!(key);
        s.send(&from.id, &from.session_uid, "agent", &q, people).unwrap()
    }
    fn codes(v: &Value) -> Vec<String> {
        v["warnings"].as_array().into_iter().flatten()
            .map(|w| format!("{} {}", w["code"].as_str().unwrap(), w["token"].as_str().unwrap()))
            .collect()
    }

    #[test]
    fn body_name_promotes_a_channel_member_and_notifies_them() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let people = vec![person(&s, "a"), person(&s, "b")];
        enrolled(&mut s, &people, &["Lead", "lane"]);
        let sent = say(&mut s, &people[0], json!({"channel":"general"}), "@lane please rebase", "m1", &people);
        let b = &people[1].id;
        assert_eq!(sent["event"]["data"]["mentions"], json!([b]));
        assert_eq!(sent["event"]["data"]["mention_recipients"], json!([b]));
        assert_eq!(sent["mentions_resolved"], json!([{"token":"@lane","id":b,"name":"lane","source":"body"}]));
        assert!(sent.get("warnings").is_none());
        assert_eq!(sent["created_at"], sent["event"]["created_at"]);
        assert!(s.wake_intents()["b"].iter().any(|w| w.event_id == sent["event_id"].as_str().unwrap()));
        // Normalized spelling resolves too; the sender's own name is ignored.
        let again = say(&mut s, &people[0], json!({"channel":"general"}), "@LANE and @Lead", "m2", &people);
        assert_eq!(again["event"]["data"]["mentions"], json!([b]));
        assert!(again.get("warnings").is_none());
    }

    #[test]
    fn unpromotable_body_tokens_warn_and_code_spans_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let people = vec![person(&s, "a"), person(&s, "b"), person(&s, "c")];
        enrolled(&mut s, &people, &["Lead", "lane", "outsider"]);
        s.channel_action(&people[0].id, &json!({"action":"create","path":"work","request_id":"mk"}), &people).unwrap();
        s.channel_action(&people[1].id, &json!({"action":"join","path":"work","request_id":"j"}), &people).unwrap();
        // Two participants holding the same name: ambiguous.
        let twin = s.names[&people[1].id].clone();
        s.names.insert("agent:x:twin".into(), Name { session_uid: "twin".into(), ..twin });
        let sent = say(&mut s, &people[0], json!({"channel":"work"}),
            "@outsider @nobody @here @Owner `@lane` and @lane", "w1", &people);
        assert_eq!(sent["event"]["data"]["mentions"], json!([]));
        assert_eq!(codes(&sent), vec![
            "body_mention_not_member @outsider",
            "unresolved_body_mention @nobody",
            "body_broadcast_not_promoted @here",
            "owner_not_promoted @Owner",
            "ambiguous_body_mention @lane",
        ]);
        assert_eq!(sent["warnings"][0]["id"], json!(people[2].id));
    }

    #[test]
    fn names_resolve_in_mentions_param_and_released_aliases_yield() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let people = vec![person(&s, "a"), person(&s, "b"), person(&s, "c")];
        enrolled(&mut s, &people, &["Lead", "lane", "next"]);
        let sent = say(&mut s, &people[0], json!({"channel":"general","mentions":["@lane", people[2].id]}), "two", "p1", &people);
        assert_eq!(sent["event"]["data"]["mentions"], json!(BTreeSet::from([people[1].id.clone(), people[2].id.clone()])));
        assert_eq!(sent["mentions_resolved"][0], json!({"token":"@lane","id":people[1].id,"name":"lane","source":"param"}));
        let missing = s.send(&people[0].id, "a", "agent",
            &json!({"channel":"general","body":"x","request_id":"p2","mentions":["ghost"]}), &people).unwrap_err();
        assert_eq!(missing.code, "not_found");
        // A superseded holder's released name yields to the current holder.
        let mut old = s.names[&people[1].id].clone();
        old.released = true;
        s.names.insert(people[1].id.clone(), old);
        let mut cur = s.names[&people[2].id].clone();
        cur.aliases.push("lane".into());
        s.names.insert(people[2].id.clone(), cur);
        let routed = say(&mut s, &people[0], json!({"channel":"general"}), "@lane over to you", "p3", &people);
        assert_eq!(routed["event"]["data"]["mentions"], json!([people[2].id]));
    }

    #[test]
    fn dm_body_mentions_stay_inside_the_dm_and_retries_repeat_the_report() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let people = vec![person(&s, "a"), person(&s, "b"), person(&s, "c")];
        enrolled(&mut s, &people, &["Lead", "lane", "outsider"]);
        let first = say(&mut s, &people[0], json!({"dm":people[1].id}), "@lane fyi, cc @outsider", "d1", &people);
        assert_eq!(first["event"]["data"]["mentions"], json!([people[1].id]));
        assert_eq!(codes(&first), vec!["body_mention_not_member @outsider"]);
        let retry = say(&mut s, &people[0], json!({"dm":people[1].id}), "@lane fyi, cc @outsider", "d1", &people);
        assert_eq!(retry["event_id"], first["event_id"]);
        assert_eq!(retry["mentions_resolved"], first["mentions_resolved"]);
        assert_eq!(retry["warnings"], first["warnings"]);
        assert_eq!(retry["created_at"], first["created_at"]);
    }

    #[test]
    fn body_tokens_skip_code_emails_urls_and_trailing_punctuation() {
        let body = "Ping @lane and @Swarm-Coord.\n`@not-me` mail a@b.com see https://x.io/@user\n```\n@fenced\n```\n(@paren), @lane again, @here";
        assert_eq!(
            body_mention_tokens(body),
            vec!["lane", "Swarm-Coord", "paren", "here"]
        );
    }
}
