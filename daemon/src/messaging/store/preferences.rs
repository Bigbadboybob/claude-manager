use super::watches::Scope;
use super::*;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Preferences {
    pub revision: u64,
    pub dnd: bool,
    pub bell: bool,
    pub rules: BTreeMap<String, Follow>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Follow {
    pub scope: Scope,
    pub inbox: bool,
    pub wake: bool,
    pub muted: bool,
    pub since: u64,
    #[serde(default)]
    pub hub_since: Option<u64>,
    #[serde(default)]
    pub wake_since: Option<u64>,
}

impl Scope {
    fn specificity(&self) -> (usize, usize) {
        if self.thread.is_some() {
            (4, 0)
        } else if self.peer.is_some() || self.conversation.is_some() {
            (3, 0)
        } else if let Some(path) = &self.channel {
            (
                2,
                if path == "*" {
                    0
                } else {
                    path.split('/').count()
                },
            )
        } else {
            (1, 0)
        }
    }
}

impl Store {
    pub(super) fn preference_for_event(
        &self,
        actor: &str,
        event: &Published,
    ) -> (bool, bool, bool) {
        let v = &event.event;
        if v["type"] != "message.create" || !self.visible(actor, strv(v, "conversation_id")) {
            return (false, false, false);
        }
        let dm = self.conversations.contains_key(strv(v, "conversation_id"));
        let mention = mention_recipients(v).contains(&actor);
        let mut inbox = dm || mention;
        let mut wake = actor != "owner" && inbox;
        let mut muted = false;
        if let Some(state) = self.personal.get(actor) {
            let prefs = &state.preferences;
            muted = prefs.dnd;
            let mut rules: Vec<_> = prefs
                .rules
                .values()
                .filter(|r| self.scope_matches(actor, &r.scope, v))
                .collect();
            rules.sort_by_key(|r| r.scope.specificity());
            for rule in rules {
                muted |= rule.muted;
                // New follows start at their commit boundary; enabling a
                // channel never floods an inbox with its entire old history.
                let later = rule
                    .hub_since
                    .and_then(|boundary| {
                        self.replication
                            .receipts
                            .get(strv(v, "id"))
                            .and_then(|r| r["position"].as_str())
                            .and_then(|p| p.parse::<u64>().ok())
                            .map(|p| p > boundary)
                    })
                    .unwrap_or(event.position > rule.since);
                if later || !rule.inbox {
                    inbox = rule.inbox;
                }
                if event.position > rule.wake_since.unwrap_or(rule.since) || !rule.wake {
                    wake = rule.wake;
                }
            }
        }
        (
            inbox && v["actor"]["id"] != actor,
            wake && !muted && v["actor"]["id"] != actor,
            muted,
        )
    }

    fn scope_contains(&self, actor: &str, parent: &Scope, child: &Scope) -> bool {
        if parent == child {
            return true;
        }
        if parent.thread.is_some() {
            return false;
        }
        if let Some(thread) = &child.thread {
            return self
                .events
                .iter()
                .find(|e| e.event["id"] == *thread)
                .is_some_and(|e| self.scope_matches(actor, parent, &e.event));
        }
        if let Some(cid) = &child.conversation {
            if let Some(e) = self
                .events
                .iter()
                .find(|e| e.event["conversation_id"] == *cid)
            {
                return self.scope_matches(actor, parent, &e.event);
            }
        }
        if parent.dms && (child.dms || child.peer.is_some()) {
            return true;
        }
        if let (Some(base), Some(path)) = (&parent.channel, &child.channel) {
            return base == "*"
                || base == path
                || parent.include_children && path.starts_with(&format!("{base}/"));
        }
        false
    }

    fn preference_response(
        &self,
        actor: &str,
        prefs: &Preferences,
        scope: Option<&Scope>,
        p: &Value,
    ) -> Result<Value> {
        let mut rules: Vec<_> = prefs
            .rules
            .values()
            .filter(|r| scope.is_none_or(|s| self.scope_contains(actor, &r.scope, s)))
            .collect();
        rules.sort_by_key(|r| r.scope.specificity());
        let direct = scope.is_some_and(|s| {
            s.dms
                || s.peer.is_some()
                || s.conversation
                    .as_ref()
                    .is_some_and(|id| self.conversations.contains_key(id))
        });
        let (mut inbox, mut wake, mut muted) = (direct, direct && actor != "owner", prefs.dnd);
        if scope.is_some() {
            for rule in &rules {
                inbox = rule.inbox;
                wake = rule.wake;
                muted |= rule.muted;
            }
        }
        let entries = rules
            .iter()
            .map(|r| {
                let mut v = serde_json::to_value(r).unwrap();
                v["id"] = json!(hash(&serde_json::to_vec(&r.scope).unwrap()));
                v
            })
            .collect();
        let page = self.directory_page(actor,&json!({"directory":"preferences","scope":scope,"revision":prefs.revision,"cursor":p["cursor"],"limit":p["limit"]}),entries)?;
        Ok(
            json!({"revision":prefs.revision,"dnd":prefs.dnd,"bell":prefs.bell,"scope":scope,"rules":page["items"],"next_cursor":page["next_cursor"],"effective":if scope.is_some(){json!({"inbox":inbox,"wake":wake && !muted && actor != "owner","muted":muted})}else{Value::Null},"defaults":{"incoming_dms":{"inbox":true,"wake":actor != "owner"},"mentions":{"inbox":true,"wake":actor != "owner"},"channels":{"inbox":false,"wake":false}},"position":self.position_token(self.position)}),
        )
    }

    pub fn follow(&mut self, actor: &str, p: &Value, people: &[Person]) -> Result<Value> {
        let action = p["action"].as_str().unwrap_or("get");
        if !["get", "set", "remove"].contains(&action) {
            return Err(err(
                "invalid_params",
                "Follow actions are get, set and remove",
            ));
        }
        let op = if action != "get" {
            let op = self.personal_request(actor, p, &format!("follow.{action}"))?;
            if let Some(prior) = op.2 {
                return Ok(prior);
            }
            Some((op.0, op.1))
        } else {
            None
        };
        let scope = p
            .get("scope")
            .filter(|v| !v.is_null())
            .map(|v| self.scope(actor, v, people, true))
            .transpose()?;
        let mut state = self.personal_state(actor);
        if action == "get" {
            return self.preference_response(actor, &state.preferences, scope.as_ref(), p);
        }
        let expected = p["expected_revision"].as_u64();
        if expected != Some(state.preferences.revision)
            && !(expected.is_none() && state.preferences.revision == 0)
        {
            let mut result =
                self.preference_response(actor, &state.preferences, scope.as_ref(), p)?;
            result["status"] = json!("conflict");
            return Ok(result);
        }
        for key in ["inbox", "wake", "muted", "dnd", "bell"] {
            if p.get(key).is_some_and(|v| !v.is_null() && !v.is_boolean()) {
                return Err(err("invalid_params", format!("{key} must be a boolean")));
            }
        }
        if let Some(scope) = &scope {
            if p.get("dnd").is_some_and(|v| !v.is_null())
                || p.get("bell").is_some_and(|v| !v.is_null())
            {
                return Err(err(
                    "invalid_params",
                    "DND and bell are global preferences; omit scope",
                ));
            }
            let id = hash(&serde_json::to_vec(scope)?);
            if action == "remove" {
                state.preferences.rules.remove(&id);
            } else {
                if !state.preferences.rules.contains_key(&id)
                    && state.preferences.rules.len() >= 256
                {
                    return Err(err(
                        "preference_limit",
                        "At most 256 follow overrides; remove unused overrides",
                    ));
                }
                let previous = state.preferences.rules.get(&id);
                let inbox = p["inbox"]
                    .as_bool()
                    .unwrap_or(previous.map(|r| r.inbox).unwrap_or(true));
                let wake = p["wake"]
                    .as_bool()
                    .unwrap_or(previous.map(|r| r.wake).unwrap_or(false));
                let muted = p["muted"]
                    .as_bool()
                    .unwrap_or(previous.map(|r| r.muted).unwrap_or(false));
                let since = previous
                    .filter(|r| r.inbox && inbox)
                    .map(|r| r.since)
                    .unwrap_or(self.position);
                let wake_since = Some(
                    previous
                        .filter(|r| r.wake && wake)
                        .map(|r| r.wake_since.unwrap_or(r.since))
                        .unwrap_or(self.position),
                );
                state.preferences.rules.insert(
                    id,
                    Follow {
                        scope: scope.clone(),
                        inbox,
                        wake,
                        muted,
                        since,
                        hub_since: None,
                        wake_since,
                    },
                );
            }
        } else {
            if action == "remove"
                || ["inbox", "wake", "muted"]
                    .iter()
                    .any(|k| p.get(*k).is_some_and(|v| !v.is_null()))
            {
                return Err(err(
                    "invalid_scope",
                    "Scope is required for follow/mute settings",
                ));
            }
            if let Some(dnd) = p["dnd"].as_bool() {
                state.preferences.dnd = dnd;
            }
            if let Some(bell) = p["bell"].as_bool() {
                if actor != "owner" {
                    return Err(err("invalid_params", "Bell is an Owner TUI preference"));
                }
                if bell && !state.preferences.bell {
                    state.bell_position = self.position;
                    for m in state.monitors.values() {
                        state.bell_monitors.insert(m.id.clone(), m.hit_high);
                    }
                }
                state.preferences.bell = bell;
            }
        }
        state.preferences.revision = state
            .preferences
            .revision
            .checked_add(1)
            .ok_or_else(|| err("counter_overflow", "Preference revision exhausted"))?;
        let mut result = self.preference_response(actor, &state.preferences, scope.as_ref(), p)?;
        result["status"] = json!("saved");
        let (key, digest) = op.unwrap();
        self.commit_personal_operation(state, key, digest, result)
    }
}
