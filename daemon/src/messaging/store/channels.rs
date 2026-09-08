//! Channel administration and attributed pins are retained metadata events.
use super::*;

#[cfg(test)]
mod tests;

impl Store {
    pub(super) fn channel_at(&self, path: &str, id: &str, high: u64) -> Value {
        let mut channel = json!({"path":path,"id":id,"name":path,"revision":id,
            "kind":"channel","description":"","created_by":"owner","admins":[],"allow_agent_edits":false,"default_join":false,"archived":false});
        for published in self.events.iter().filter(|e| e.position <= high) {
            let e = &published.event;
            if let Some(created) = e["data"]["channels"]
                .as_array()
                .and_then(|a| a.iter().find(|c| c["id"] == id))
            {
                let creator = if e["actor"]["id"] == "system" {
                    "owner"
                } else {
                    strv(&e["actor"], "id")
                };
                channel["created_by"] = json!(creator);
                channel["admins"] = if creator == "owner" {
                    json!([])
                } else {
                    json!([creator])
                };
                for key in ["name", "description", "admins", "allow_agent_edits", "default_join", "archived"] {
                    if let Some(value) = created.get(key) {
                        channel[key] = value.clone();
                    }
                }
            }
            if e["type"] == "channel.update" && e["data"]["channel"]["id"] == id {
                channel = e["data"]["channel"].clone();
            }
            if e["type"] == "channel.membership.initialize" && e["data"]["channel_id"] == id {
                channel["default_join"] = e["data"]["default_join"].clone();
            }
        }
        channel
    }
    fn channel_admin(&self, actor: &str, c: &Value) -> bool {
        actor == "owner"
            || c["admins"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == actor))
    }
    pub(super) fn channel_permissions(&self, actor: &str, c: &mut Value) {
        let admin = self.channel_admin(actor, c);
        c["can_manage"] = json!(admin);
        c["can_edit"] = json!(admin || c["allow_agent_edits"] == true);
        c["joined"] = json!(self.joined(actor, strv(c, "id")));
    }
    fn validate_channel_fields(
        &self,
        p: &Value,
        creator: &str,
        people: &[Person],
        existing_admins: &[Value],
    ) -> Result<Value> {
        let mut fields = json!({});
        for (key, max) in [("name", 100), ("description", 1000)] {
            if let Some(value) = p.get(key) {
                let text = value
                    .as_str()
                    .ok_or_else(|| err("invalid_params", format!("{key} must be text")))?;
                if text.chars().count() > max
                    || (key == "name"
                        && (text.trim().is_empty() || text.chars().any(char::is_control)))
                {
                    return Err(err(
                        "invalid_params",
                        format!("{key} exceeds {max} characters or is invalid"),
                    ));
                }
                fields[key] = json!(if key == "name" { text.trim() } else { text });
            }
        }
        for key in ["allow_agent_edits", "default_join", "archived"] {
        if let Some(value) = p.get(key) {
            if !value.is_boolean() {
                return Err(err("invalid_params", format!("{key} must be a boolean")));
            }
            fields[key] = value.clone();
        }
        }
        if let Some(value) = p.get("admins") {
            let admins = value
                .as_array()
                .ok_or_else(|| err("invalid_params", "admins must be participant IDs"))?;
            if admins.len() > 32 {
                return Err(err("invalid_params", "At most 32 named channel admins"));
            }
            let mut ids = BTreeSet::new();
            for value in admins {
                let id = value
                    .as_str()
                    .ok_or_else(|| err("invalid_params", "admins must be participant IDs"))?;
                if id != "owner"
                    && id != creator
                    && !self.names.contains_key(id)
                    && !people.iter().any(|p| p.id == id)
                    && !existing_admins.iter().any(|v| v == id)
                {
                    return Err(err(
                        "not_found",
                        "Admin not found; resolve participant IDs with chat_people",
                    ));
                }
                if id != "owner" {
                    ids.insert(id.to_owned());
                }
            }
            if ids.len() > 32 {
                return Err(err("invalid_params", "At most 32 named admins"));
            }
            fields["admins"] = json!(ids);
        }
        Ok(fields)
    }
    fn channel_actor_name(&self, actor: &str) -> String {
        self.names
            .get(actor)
            .map(|n| n.name.clone())
            .unwrap_or_else(|| {
                if actor == "owner" {
                    "Owner".into()
                } else {
                    "Agent".into()
                }
            })
    }
    fn channel_result(&self, actor: &str, e: &Value) -> Value {
        let mut value = self.send_response(e);
        let mut channel = if e["type"] == "channel.update" {
            e["data"]["channel"].clone()
        } else {
            let c = e["data"]["channels"].as_array().unwrap().last().unwrap();
            self.channel_at(
                strv(c, "path"),
                strv(c, "id"),
                self.events
                    .iter()
                    .find(|p| p.event["id"] == e["id"])
                    .unwrap()
                    .position,
            )
        };
        self.channel_permissions(actor, &mut channel);
        value["channel"] = channel;
        value["status"] = json!("saved");
        value
    }
    pub fn create_channel(&mut self, actor: &str, p: &Value) -> Result<Value> {
        self.create_channel_with_people(actor, p, &[])
    }
    fn create_channel_with_people(
        &mut self,
        actor: &str,
        p: &Value,
        people: &[Person],
    ) -> Result<Value> {
        let (key, digest, prior) = self.request(actor, p, "channel.create")?;
        if let Some(e) = prior {
            return Ok(self.channel_result(actor, &e));
        }
        let path = required(p, "path")?;
        channel_path(&path)?;
        if self.channels.contains_key(&path) {
            return Err(err("channel_exists", "Channel already exists"));
        }
        let fields = self.validate_channel_fields(p, actor, people, &[])?;
        let mut items = vec![];
        let mut part = String::new();
        for segment in path.split('/') {
            if !part.is_empty() {
                part.push('/');
            }
            part.push_str(segment);
            if !self.channels.contains_key(&part) {
                let mut c = json!({"path":part,"id":uuid(),"name":part,"description":"",
                    "admins":if actor == "owner" { vec![] } else { vec![actor] },"allow_agent_edits":false});
                if part == path {
                    for (k, v) in fields.as_object().unwrap() {
                        c[k] = v.clone();
                    }
                }
                if actor != "owner" && !c["admins"].as_array().unwrap().iter().any(|id| id == actor)
                {
                    c["admins"].as_array_mut().unwrap().push(json!(actor));
                }
                if c["admins"].as_array().unwrap().len() > 32 {
                    return Err(err(
                        "invalid_params",
                        "At most 32 named admins including the creator on creation",
                    ));
                }
                items.push(c);
            }
        }
        let e = self.publish(
            "channel.create",
            None,
            &format!("Created #{path}"),
            json!({"channels":items,"membership_version":1}),
            actor,
            &self.channel_actor_name(actor),
            if actor == "owner" { "owner" } else { "agent" },
            &key,
            &digest,
        )?;
        let _ = self.project();
        Ok(self.channel_result(actor, &e))
    }
    pub fn channel_action(&mut self, actor: &str, p: &Value, people: &[Person]) -> Result<Value> {
        if self.degraded.is_none() { self.enroll_participant(actor)?; }
        let action = p["action"].as_str().unwrap_or("list");
        if action == "list" {
            if p.get("joined_only").is_some_and(|v| !v.is_boolean())
                || p.get("query").is_some_and(|v| !v.is_string()) {
                return Err(err("invalid_params", "joined_only must be a boolean and query must be text"));
            }
            let channels = self.channels_for(actor)?.into_iter().filter(|c| {
                (p["joined_only"] != true || c["joined"] == true)
                    && p["query"].as_str().is_none_or(|q| ["path","name","description"].iter()
                        .any(|k| normalize(strv(c,k)).contains(&normalize(q))))
            }).collect();
            return self.directory_page(actor, p, channels);
        }
        if ["join", "leave"].contains(&action) { return self.change_membership(actor, p, people); }
        if action == "create" {
            return self.create_channel_with_people(actor, p, people);
        }
        if !["get", "update", "members"].contains(&action) {
            return Err(err(
                "unsupported_feature",
                "Channel actions: list, get, create, update, join, leave, members",
            ));
        }
        let request = if action == "update" {
            Some(self.request(actor, p, "channel.update")?)
        } else {
            None
        };
        if let Some((_, _, Some(e))) = &request {
            return Ok(self.channel_result(actor, e));
        }
        let mut selector = json!({});
        if let Some(path) = p.get("path") {
            selector["channel"] = path.clone();
        }
        if let Some(id) = p.get("conversation") {
            selector["conversation"] = id.clone();
        }
        let (id, _) = self.resolve(actor, &selector, people, false)?;
        let (path, _) = self
            .channels
            .iter()
            .find(|(_, cid)| **cid == id)
            .ok_or_else(|| err("not_found", "Channel not found"))?;
        let mut current = self.channel_info(path, &id);
        self.channel_permissions(actor, &mut current);
        if action == "members" {
            let roster = self.people(people).as_array().cloned().unwrap_or_default();
            let members = self.memberships.get(&id).into_iter().flatten().map(|member| {
                roster.iter().find(|p| p["id"] == *member).cloned().unwrap_or_else(||
                    json!({"id":member,"name":member,"present":false}))
            }).collect();
            return self.directory_page(actor,p,members);
        }
        if action == "get" {
            return Ok(current);
        }
        if current["can_edit"] != true
            || ((p.get("admins").is_some() || p.get("allow_agent_edits").is_some() || p.get("default_join").is_some() || p.get("archived").is_some())
                && current["can_manage"] != true)
        {
            return Err(err("unauthorized", "Only channel admins can change access; channel editing is restricted by its policy"));
        }
        let expected = required(p, "expected_revision")?;
        if current["revision"] != expected {
            return Ok(
                json!({"status":"conflict","current_revision":current["revision"],"channel":current}),
            );
        }
        let fields = self.validate_channel_fields(
            p,
            strv(&current, "created_by"),
            people,
            current["admins"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )?;
        if fields.as_object().unwrap().is_empty() {
            return Err(err(
                "invalid_params",
                "Supply name, description, admins, allow_agent_edits, default_join or archived",
            ));
        }
        for (k, v) in fields.as_object().unwrap() {
            current[k] = v.clone();
        }
        current.as_object_mut().unwrap().remove("can_edit");
        current.as_object_mut().unwrap().remove("can_manage");
        for key in ["joined","member_count","membership_revision"] { current.as_object_mut().unwrap().remove(key); }
        current["revision"] = json!(uuid());
        let (key, digest, _) = request.unwrap();
        let e = self.publish(
            "channel.update",
            None,
            &format!(
                "Updated #{}: {}",
                current["path"].as_str().unwrap(),
                current["name"].as_str().unwrap()
            ),
            json!({"channel":current,"expected_revision":expected}),
            actor,
            &self.channel_actor_name(actor),
            if actor == "owner" { "owner" } else { "agent" },
            &key,
            &digest,
        )?;
        let _ = self.project();
        Ok(self.channel_result(actor, &e))
    }
    pub(super) fn pin_states(&self, high: u64) -> BTreeMap<String, Value> {
        let mut pins = BTreeMap::new();
        for p in self
            .events
            .iter()
            .filter(|p| p.position <= high && p.event["type"] == "conversation.pin")
        {
            let e = &p.event;
            let id = strv(&e["data"], "target_id");
            if e["data"]["pinned"] == true {
                pins.insert(
                    id.into(),
                    json!({"actor":e["actor"],"created_at":e["created_at"],"revision":e["id"]}),
                );
            } else {
                pins.remove(id);
            }
        }
        pins
    }
    pub(super) fn pins_revision(&self, cid: &str, high: u64) -> String {
        self.events
            .iter()
            .rev()
            .find(|e| {
                e.position <= high
                    && e.event["conversation_id"] == cid
                    && e.event["type"] == "conversation.pin"
            })
            .map(|e| strv(&e.event, "id"))
            .unwrap_or(cid)
            .into()
    }
    pub fn pins(&mut self, actor: &str, p: &Value, people: &[Person]) -> Result<Value> {
        let action = p["action"].as_str().unwrap_or("list");
        if action == "list" {
            let mut query = p.clone();
            query["pinned_only"] = json!(true);
            let (cid, _) = self.resolve(actor, &query, people, false)?;
            let mut result = self.read(actor, &query, people)?;
            result["revision"] = result["pins_revision"].clone();
            result["conversation_id"] = json!(cid);
            return Ok(result);
        }
        if !["set", "remove"].contains(&action) {
            return Err(err("unsupported_feature", "Pin actions: list, set, remove"));
        }
        let (key, digest, prior) = self.request(actor, p, "conversation.pin")?;
        if let Some(e) = prior {
            return Ok(
                json!({"status":"saved","revision":e["id"],"event_id":e["id"],"pinned":e["data"]["pinned"]}),
            );
        }
        let (cid, _) = self.resolve(actor, p, people, false)?;
        if let Some((path, _)) = self.channels.iter().find(|(_, id)| **id == cid) {
            let c = self.channel_info(path, &cid);
            if !self.channel_admin(actor, &c) && c["allow_agent_edits"] != true {
                return Err(err(
                    "unauthorized",
                    "Only channel admins can pin messages unless agent editing is enabled",
                ));
            }
        }
        let target = required(p, "message_id")?;
        if !self.events.iter().any(|e| {
            e.event["id"] == target
                && e.event["type"] == "message.create"
                && e.event["conversation_id"] == cid
        }) {
            return Err(err("not_found", "Message not found in this conversation"));
        }
        let revision = self.pins_revision(&cid, self.position);
        if required(p, "expected_revision")? != revision {
            return Ok(json!({"status":"conflict","current_revision":revision}));
        }
        let pinned = action == "set";
        let active = self.pin_states(self.position);
        if pinned
            && !active.contains_key(&target)
            && self
                .events
                .iter()
                .filter(|e| {
                    e.event["conversation_id"] == cid && active.contains_key(strv(&e.event, "id"))
                })
                .count()
                >= 100
        {
            return Err(err(
                "pin_limit",
                "At most 100 pinned messages per conversation; unpin one first",
            ));
        }
        let e = self.publish(
            "conversation.pin",
            Some(&cid),
            &format!(
                "{} message {target}",
                if pinned { "Pinned" } else { "Unpinned" }
            ),
            json!({"target_id":target,"pinned":pinned,"expected_revision":revision}),
            actor,
            &self.channel_actor_name(actor),
            if actor == "owner" { "owner" } else { "agent" },
            &key,
            &digest,
        )?;
        Ok(json!({"status":"saved","revision":e["id"],"event_id":e["id"],"pinned":pinned}))
    }
}
