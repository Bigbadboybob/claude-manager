//! A monitor retains its immutable predicate and arrival interval. Hit pages
//! are reconstructed from retained publications, not an ever-growing ID array.
use super::*;

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Scope {
    pub channel: Option<String>,
    pub conversation: Option<String>,
    pub peer: Option<String>,
    pub dms: bool,
    pub thread: Option<String>,
    pub include_children: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Monitor {
    pub id: String,
    #[serde(default)]
    pub task_subscription: Option<String>,
    pub scope: Scope,
    pub mode: String,
    pub notify: String,
    pub include_self: bool,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub state: String,
    pub start: u64,
    pub scanned: u64,
    pub hit_high: u64,
    pub hits: u64,
    pub acknowledged: u64,
    pub closing_fence: Option<u64>,
}

impl Store {
    pub(super) fn scope(
        &self,
        actor: &str,
        p: &Value,
        people: &[Person],
        allow_all_channels: bool,
    ) -> Result<Scope> {
        if !p.is_object() {
            return Err(err("invalid_scope", "Scope must be an object"));
        }
        for key in p.as_object().unwrap().keys() {
            if ![
                "channel",
                "conversation",
                "dm",
                "dms",
                "thread",
                "include_children",
            ]
            .contains(&key.as_str())
            {
                return Err(err(
                    "invalid_scope",
                    format!("Unsupported scope field: {key}"),
                ));
            }
        }
        for key in ["channel", "conversation", "dm", "thread"] {
            if p.get(key).is_some_and(|v| !v.is_null() && !v.is_string()) {
                return Err(err("invalid_scope", format!("{key} must be a string")));
            }
        }
        for key in ["dms", "include_children"] {
            if p.get(key).is_some_and(|v| !v.is_null() && !v.is_boolean()) {
                return Err(err("invalid_scope", format!("{key} must be a boolean")));
            }
        }
        let selectors = ["channel", "conversation", "dm"]
            .iter()
            .filter(|k| p.get(**k).is_some_and(|v| !v.is_null()))
            .count()
            + usize::from(p["dms"] == true);
        let thread = p["thread"].as_str().map(str::to_owned);
        if selectors != 1 && !(selectors == 0 && thread.is_some()) {
            return Err(err(
                "invalid_scope",
                "Choose a channel, peer DM, incoming DMs, or thread",
            ));
        }
        let mut scope = Scope {
            thread,
            include_children: p["include_children"] == true,
            ..Scope::default()
        };
        if let Some(path) = p["channel"].as_str() {
            if path != "*" || !allow_all_channels {
                channel_path(path)?;
            }
            if path != "*" && !self.channels.contains_key(path) {
                return Err(err("not_found", "Channel not found"));
            }
            scope.channel = Some(path.into());
        } else if p["dms"] == true {
            scope.dms = true;
        } else if p["dm"].is_string() {
            let (id, creation) = self.resolve(actor, &json!({"dm":p["dm"]}), people, true)?;
            let members: Vec<String> = if let Some(c) = creation {
                serde_json::from_value(c["members"].clone())?
            } else {
                self.conversations[&id].clone()
            };
            scope.peer = members.into_iter().find(|id| id != actor);
        } else if let Some(id) = p["conversation"].as_str() {
            if !self.visible(actor, id) {
                return Err(err("not_found", "Conversation not found"));
            }
            if let Some(members) = self.conversations.get(id) {
                if members.len() == 2 {
                    scope.peer = members.iter().find(|m| m.as_str() != actor).cloned();
                } else {
                    scope.conversation = Some(id.into());
                }
            } else if let Some((path, _)) = self.channels.iter().find(|(_, cid)| cid.as_str() == id)
            {
                scope.channel = Some(path.clone());
            }
        }
        if scope.include_children && (scope.channel.is_none() || scope.thread.is_some()) {
            return Err(err(
                "invalid_scope",
                "include_children applies to channels without a thread",
            ));
        }
        if let Some(thread) = scope.thread.clone() {
            let root = self
                .events
                .iter()
                .find(|e| {
                    e.event["id"] == thread
                        && e.event["type"] == "message.create"
                        && self.visible(actor, strv(&e.event, "conversation_id"))
                })
                .ok_or_else(|| err("not_found", "Thread not found"))?;
            let cid = strv(&root.event, "conversation_id");
            let root_id = root.event["data"]["thread_root"]
                .as_str()
                .unwrap_or(&thread)
                .to_owned();
            let saved = scope.thread.take();
            if selectors > 0 && !self.scope_matches(actor, &scope, &root.event) {
                return Err(err("not_found", "Thread not found in that conversation"));
            }
            let _ = saved;
            scope = Scope {
                conversation: Some(cid.into()),
                thread: Some(root_id),
                ..Scope::default()
            };
        }
        Ok(scope)
    }

    pub(super) fn scope_matches(&self, actor: &str, scope: &Scope, event: &Value) -> bool {
        let cid = strv(event, "conversation_id");
        if !self.visible(actor, cid) {
            return false;
        }
        if let Some(path) = &scope.channel {
            let actual = self
                .channels
                .iter()
                .find_map(|(p, id)| (id == cid).then_some(p));
            if !actual.is_some_and(|actual| {
                path == "*"
                    || actual == path
                    || scope.include_children && actual.starts_with(&format!("{path}/"))
            }) {
                return false;
            }
        }
        if scope.conversation.as_deref().is_some_and(|id| id != cid) {
            return false;
        }
        if scope.dms && !self.conversations.contains_key(cid) {
            return false;
        }
        if let Some(peer) = &scope.peer {
            if !self.conversations.get(cid).is_some_and(|members| {
                members.len() == 2 && members.contains(peer) && members.iter().any(|m| m == actor)
            }) {
                return false;
            }
        }
        if let Some(root) = &scope.thread {
            if event["id"] != *root && event["data"]["thread_root"] != *root {
                return false;
            }
        }
        true
    }

    pub(super) fn monitor_matches(&self, actor: &str, monitor: &Monitor, e: &Published) -> bool {
        e.event["type"] == "message.create"
            && self.task_monitor_matches(actor, monitor, strv(&e.event, "id"))
            && (monitor.include_self || e.event["actor"]["id"] != actor)
            && self.scope_matches(actor, &monitor.scope, &e.event)
    }

    pub(super) fn scan_monitor(
        &self,
        actor: &str,
        monitor: &mut Monitor,
        clock: DateTime<Utc>,
    ) -> Result<bool> {
        if monitor.state != "active"
            || actor == "owner" && self.sync_enabled() && !self.is_coordinator()
        {
            return Ok(false);
        }
        let expired = monitor
            .expires_at
            .as_deref()
            .map(parse_time)
            .transpose()?
            .is_some_and(|expiry| clock >= expiry);
        if expired && monitor.closing_fence.is_none() {
            monitor.closing_fence = Some(self.position);
        }
        let high = monitor.closing_fence.unwrap_or(self.position);
        let mut changed = high != monitor.scanned || expired;
        let scanned = monitor.scanned;
        for e in self
            .events
            .iter()
            .filter(|e| e.position > scanned && e.position <= high)
        {
            if self.monitor_matches(actor, monitor, e) {
                monitor.hits = monitor
                    .hits
                    .checked_add(1)
                    .ok_or_else(|| err("counter_overflow", "Monitor count exhausted"))?;
                monitor.hit_high = e.position;
                changed = true;
                if monitor.mode == "once" {
                    monitor.state = "matched".into();
                    monitor.closing_fence = Some(e.position);
                    monitor.scanned = e.position;
                    return Ok(true);
                }
            }
        }
        monitor.scanned = high;
        if expired {
            monitor.state = "expired".into();
        }
        Ok(changed)
    }

    pub fn advance_monitors_at(&mut self, clock: DateTime<Utc>) -> Result<()> {
        let owners: Vec<_> = self.personal.keys().cloned().collect();
        for actor in owners {
            let mut state = self.personal_state(&actor);
            let mut changed = false;
            for monitor in state.monitors.values_mut() {
                changed |= self.scan_monitor(&actor, monitor, clock)?;
            }
            if changed {
                self.save_personal(state)?;
            }
        }
        Ok(())
    }

    fn monitor_summary(&self, actor: &str, m: &Monitor) -> Value {
        let unseen = self
            .events
            .iter()
            .filter(|e| {
                e.position > m.acknowledged
                    && e.position <= m.hit_high
                    && self.monitor_matches(actor, m, e)
            })
            .count();
        let delivery = actor
            .strip_prefix(&format!("agent:{}:", self.daemon_id))
            .map(|uid| super::super::delivery::monitor_status(&self.root, uid, &m.id))
            .unwrap_or_else(|| json!({"passive":true}));
        json!({"id":m.id,"monitor_id":m.id,"delivery":delivery,"scope":m.scope,"mode":m.mode,"notify":m.notify,"include_self":m.include_self,"state":m.state,"created_at":m.created_at,"expires_at":m.expires_at,"hits":m.hits,"unacknowledged":unseen,"start":self.position_token(m.start),"scanned":self.position_token(m.scanned)})
    }

    pub fn register_monitor(&mut self, actor: &str, p: &Value, people: &[Person]) -> Result<Value> {
        let (key, digest, prior) = self.personal_request(actor, p, "monitor.register")?;
        if let Some(prior) = prior {
            return Ok(prior);
        }
        self.advance_monitors_at(Utc::now())?;
        let mut state = self.personal_state(actor);
        if state
            .monitors
            .values()
            .filter(|m| m.state == "active")
            .count()
            >= 64
        {
            return Err(err(
                "monitor_limit",
                "At most 64 active monitors per participant",
            ));
        }
        let scope = self.scope(actor, &p["scope"], people, false)?;
        let mode = p["mode"].as_str().unwrap_or("once");
        if !["once", "continuous"].contains(&mode) {
            return Err(err("invalid_params", "mode must be once or continuous"));
        }
        let notify =
            p["notify"]
                .as_str()
                .unwrap_or(if actor == "owner" { "badge" } else { "wake" });
        if !["wake", "badge", "none"].contains(&notify) {
            return Err(err("invalid_params", "notify must be wake, badge or none"));
        }
        let clock = Utc::now();
        let expires_at = p["expires_in"]
            .as_str()
            .map(|duration| {
                let (start, end) = time_bounds(&json!({"since":duration}))?;
                let delta =
                    parse_time(end.as_deref().unwrap())? - parse_time(start.as_deref().unwrap())?;
                Ok::<_, ChatError>((clock + delta).to_rfc3339())
            })
            .transpose()?;
        if p.get("expires_in")
            .is_some_and(|v| !v.is_null() && !v.is_string())
        {
            return Err(err(
                "invalid_params",
                "expires_in is a duration such as 10m",
            ));
        }
        let start = p
            .get("after")
            .filter(|v| !v.is_null())
            .map(|v| self.check_position(v))
            .transpose()?
            .unwrap_or(self.position);
        let mut monitor = Monitor {
            id: uuid(),
            task_subscription: None,
            scope,
            mode: mode.into(),
            notify: notify.into(),
            include_self: p["include_self"] == true,
            created_at: clock.to_rfc3339(),
            expires_at,
            state: "active".into(),
            start,
            scanned: start,
            hit_high: start,
            hits: 0,
            acknowledged: start,
            closing_fence: None,
        };
        self.scan_monitor(actor, &mut monitor, clock)?;
        let result = self.monitor_summary(actor, &monitor);
        state.monitors.insert(monitor.id.clone(), monitor);
        self.commit_personal_operation(state, key, digest, result)
    }

    pub fn monitors(&mut self, actor: &str, p: &Value) -> Result<Value> {
        let action = p["action"].as_str().unwrap_or("list");
        let mutation = ["ack", "cancel", "cancel_all", "dismiss"].contains(&action);
        let op = if mutation {
            let op = self.personal_request(actor, p, &format!("monitors.{action}"))?;
            if let Some(prior) = op.2 {
                return Ok(prior);
            }
            Some((op.0, op.1))
        } else {
            None
        };
        self.advance_monitors_at(Utc::now())?;
        let mut state = self.personal_state(actor);
        if action == "list" {
            let items = state
                .monitors
                .values()
                .filter(|m| m.state != "dismissed")
                .map(|m| self.monitor_summary(actor, m))
                .collect();
            return self.directory_page(actor, p, items);
        }
        if action == "cancel_all" {
            let mut cancelled = 0;
            for m in state.monitors.values_mut().filter(|m| {
                m.task_subscription.is_none()
                    && !["cancelled", "dismissed"].contains(&m.state.as_str())
            }) {
                m.state = "cancelled".into();
                m.closing_fence.get_or_insert(self.position);
                cancelled += 1;
            }
            let (key, digest) = op.unwrap();
            return self.commit_personal_operation(
                state,
                key,
                digest,
                json!({"cancelled":cancelled}),
            );
        }
        let id = required(p, "monitor_id")?;
        let m = state
            .monitors
            .get_mut(&id)
            .filter(|m| m.state != "dismissed")
            .ok_or_else(|| err("not_found", "Monitor not found"))?;
        if mutation && m.task_subscription.is_some() {
            let binding = m
                .task_subscription
                .as_ref()
                .and_then(|id| self.task_bindings.get(id));
            if binding.is_none_or(|b| {
                !b.active || b.actor != actor || m.id != format!("task-{}-{}", b.id, b.revision)
            }) {
                return Err(err(
                    "stale_binding",
                    "This session no longer owns the task subscription",
                ));
            }
            if action != "ack" {
                return Err(err(
                    "scheduler_owned",
                    "Configure or remove a task subscription through continuous.update",
                ));
            }
        }
        if action == "get" {
            let result = self.monitor_results(actor, m, p)?;
            state
                .monitor_receipts
                .insert(hash(&serde_json::to_vec(&result["receipt"])?));
            self.save_personal(state)?;
            return Ok(result);
        }
        match action {
            "cancel" | "dismiss" => {
                m.state = if action == "cancel" {
                    "cancelled"
                } else {
                    "dismissed"
                }
                .into();
                m.closing_fence.get_or_insert(self.position);
            }
            "ack" => {
                let receipt = &p["receipt"];
                if !state
                    .monitor_receipts
                    .contains(&hash(&serde_json::to_vec(receipt)?))
                {
                    return Err(err(
                        "invalid_receipt",
                        "Read a complete result page before acknowledging it",
                    ));
                }
                if receipt["actor"] != actor
                    || receipt["monitor_id"] != id
                    || receipt["space_id"] != self.space_id
                    || receipt["generation"] != self.generation
                {
                    return Err(err(
                        "invalid_receipt",
                        "Monitor receipt belongs to another owner, monitor or generation",
                    ));
                }
                let from = receipt["from"]
                    .as_u64()
                    .ok_or_else(|| err("invalid_receipt", "Missing receipt start"))?;
                let through = receipt["through"]
                    .as_u64()
                    .ok_or_else(|| err("invalid_receipt", "Missing receipt end"))?;
                if from > m.acknowledged || through < from || through > m.scanned {
                    return Err(err(
                        "invalid_receipt",
                        "Acknowledge complete result pages in order",
                    ));
                }
                m.acknowledged = m.acknowledged.max(through);
            }
            _ => {
                return Err(err(
                    "invalid_params",
                    "Supported actions: list, get, ack, cancel, cancel_all, dismiss",
                ))
            }
        }
        if action == "ack" {
            self.acknowledge_task_monitor(actor, m)?;
        }
        let result = self.monitor_summary(actor, m);
        let (key, digest) = op.unwrap();
        self.commit_personal_operation(state, key, digest, result)
    }

    fn monitor_results(&self, actor: &str, m: &Monitor, p: &Value) -> Result<Value> {
        let cursor = p.get("cursor").filter(|v| !v.is_null());
        let (high, from) = if let Some(c) = cursor {
            if c["kind"] != "monitor_results" || c["monitor_id"] != m.id || c["actor"] != actor {
                return Err(err("invalid_cursor", "Cursor belongs to another monitor"));
            }
            let high = self.check_position(&c["snapshot"])?;
            let from = c["last_position"]
                .as_u64()
                .ok_or_else(|| err("invalid_cursor", "Missing result offset"))?;
            if from < m.start
                || from > high
                || high > m.scanned
                || c["unacknowledged_only"].as_bool().unwrap_or(false)
                    != p["unacknowledged_only"].as_bool().unwrap_or(false)
            {
                return Err(err("invalid_cursor", "Invalid result range"));
            }
            (high, from)
        } else {
            (
                m.scanned,
                if p["unacknowledged_only"] == true {
                    m.acknowledged
                } else {
                    m.start
                },
            )
        };
        let matching: Vec<_> = self
            .events
            .iter()
            .filter(|e| {
                e.position > from
                    && e.position <= high
                    && e.position <= m.hit_high
                    && self.monitor_matches(actor, m, e)
            })
            .collect();
        let limit = p["limit"].as_u64().unwrap_or(50).clamp(1, 200) as usize;
        let mut out = Vec::new();
        let mut size = 0;
        for e in &matching {
            let item = json!({"id":e.event["id"],"conversation_id":e.event["conversation_id"],"actor":e.event["actor"],"created_at":e.event["created_at"],"received_at":e.received_at,"preview":strv(&e.event,"body").chars().take(180).collect::<String>(),"position":e.position});
            let chars = item.to_string().chars().count();
            if out.len() == limit || size + chars > 14000 {
                break;
            }
            size += chars;
            out.push(item);
        }
        let more = out.len() < matching.len();
        let through = if more {
            out.last()
                .and_then(|v| v["position"].as_u64())
                .unwrap_or(from)
        } else {
            high
        };
        Ok(
            json!({"monitor":self.monitor_summary(actor,m),"items":out,"next_cursor":if more {json!({"kind":"monitor_results","actor":actor,"monitor_id":m.id,"snapshot":self.position_token(high),"last_position":through,"unacknowledged_only":p["unacknowledged_only"].as_bool().unwrap_or(false)})}else{Value::Null},"receipt":{"actor":actor,"space_id":self.space_id,"generation":self.generation,"monitor_id":m.id,"from":from,"through":through},"position":self.position_token(high)}),
        )
    }

    pub(super) fn monitor_inbox(&self, actor: &str, e: &Published) -> bool {
        self.personal.get(actor).is_some_and(|p| {
            p.monitors.values().any(|m| {
                m.notify != "none"
                    && m.state != "dismissed"
                    && e.position > m.acknowledged
                    && e.position <= m.hit_high
                    && self.monitor_matches(actor, m, e)
            })
        })
    }

    pub fn monitor_status(&self, actor: &str) -> Value {
        let state = self.personal_state(actor);
        let summaries: Vec<_> = state
            .monitors
            .values()
            .filter(|m| m.state != "dismissed")
            .map(|m| self.monitor_summary(actor, m))
            .collect();
        json!({"active":summaries.iter().filter(|m|m["state"] == "active").count(),"unacknowledged":summaries.iter().filter_map(|m|m["unacknowledged"].as_u64()).sum::<u64>(),"badges":summaries.iter().filter(|m|m["notify"] != "none").filter_map(|m|m["unacknowledged"].as_u64()).sum::<u64>()})
    }
}
