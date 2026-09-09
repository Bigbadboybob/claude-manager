//! Public channel membership, retained independently of notification preferences.
use super::*;

#[cfg(test)]
mod tests;

/// New messages freeze their complete mention audience at publication. Older
/// messages carry only direct mentions, whose meaning stays unchanged.
pub(super) fn mention_recipients(event: &Value) -> Vec<&str> {
    event["data"]
        .get("mention_recipients")
        .unwrap_or(&event["data"]["mentions"])
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

impl Store {
    // Authority is checked by channel_action at the coordinator. Replicas may
    // receive historical membership after newer access settings; validate the
    // retained operation's shape here, not today's admin list.
    pub(super) fn validate_membership_event(&self, event: &Value) -> Result<()> {
        let id = required(event, "conversation_id")?;
        let actor = required(&event["actor"], "id")?;
        let member = required(&event["data"], "participant_id")?;
        let data = &event["data"];
        let add = data["action"] == "add_member";
        if actor == "system"
            || member == "system"
            || !data["joined"].is_boolean()
            || !self.memberships.contains_key(&id)
            || (add && data["joined"] != true)
            || (!add && actor != member)
        {
            return Err(err("invalid_record", "Invalid channel membership change"));
        }
        Ok(())
    }

    pub(super) fn joined(&self, actor: &str, channel: &str) -> bool {
        self.memberships
            .get(channel)
            .is_some_and(|members| members.contains(actor))
    }

    pub(super) fn reduce_membership(&mut self, event: &Value) -> Result<()> {
        if event["type"] == "membership.enrollment" {
            let actor = required(&event["data"], "participant_id")?;
            if actor == "system"
                || event["actor"]["id"] != "system"
                || !self.enrolled.insert(actor.clone())
            {
                return Err(err("invalid_record", "Invalid participant enrollment"));
            }
            let channels: Vec<String> =
                serde_json::from_value(event["data"]["default_channels"].clone())?;
            for id in channels {
                self.memberships
                    .get_mut(&id)
                    .ok_or_else(|| err("invalid_record", "Unknown default channel"))?
                    .insert(actor.clone());
                self.membership_revisions.insert(id, required(event, "id")?);
            }
        }
        if event["type"] == "channel.create" && event["data"]["membership_version"] == 1 {
            for channel in event["data"]["channels"].as_array().into_iter().flatten() {
                let id = required(channel, "id")?;
                let members = self.memberships.entry(id.clone()).or_default();
                let actor = strv(&event["actor"], "id");
                if actor != "system" {
                    members.insert(actor.into());
                }
                self.membership_revisions.insert(id, required(event, "id")?);
            }
        }
        if event["type"] == "channel.membership.initialize" {
            let id = required(&event["data"], "channel_id")?;
            if !self.channels.values().any(|cid| cid == &id)
                || self.memberships.contains_key(&id)
                || event["actor"]["id"] != "system"
            {
                return Err(err(
                    "invalid_record",
                    "Invalid channel membership initialization",
                ));
            }
            let members: BTreeSet<String> =
                serde_json::from_value(event["data"]["members"].clone())?;
            if members.iter().any(|id| id.is_empty() || id == "system") {
                return Err(err("invalid_record", "Invalid channel member"));
            }
            self.memberships.insert(id.clone(), members);
            self.membership_revisions.insert(id, required(event, "id")?);
        }
        if event["type"] == "channel.membership" {
            self.validate_membership_event(event)?;
            let id = required(event, "conversation_id")?;
            let member = required(&event["data"], "participant_id")?;
            let joined = event["data"]["joined"]
                .as_bool()
                .ok_or_else(|| err("invalid_record", "Membership needs joined boolean"))?;
            let members = self
                .memberships
                .get_mut(&id)
                .ok_or_else(|| err("invalid_record", "Channel membership is uninitialized"))?;
            if joined {
                members.insert(member);
            } else {
                members.remove(&member);
            }
            self.membership_revisions.insert(id, required(event, "id")?);
        }
        Ok(())
    }

    /// One retained initialization per legacy channel, safe to resume after a
    /// crash. Creators, prior posters and explicit positive channel follows
    /// carry over, including their configured descendant scope. Browsing history alone does not enroll a member.
    pub(super) fn initialize_memberships(&mut self) -> Result<()> {
        for (path, id) in self.channels.clone() {
            if self.memberships.contains_key(&id) {
                continue;
            }
            let mut members = BTreeSet::new();
            if let Some(created) = self.events.iter().find(|e| {
                e.event["data"]["channels"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|c| c["id"] == id))
            }) {
                let actor = strv(&created.event["actor"], "id");
                if actor != "system" {
                    members.insert(actor.to_owned());
                }
            }
            for published in self
                .events
                .iter()
                .filter(|e| e.event["type"] == "message.create" && e.event["conversation_id"] == id)
            {
                members.insert(required(&published.event["actor"], "id")?);
            }
            for (actor, state) in &self.personal {
                if state.preferences.rules.values().any(|rule| {
                    (rule.inbox || rule.wake)
                        && rule.scope.thread.is_none()
                        && (rule.scope.channel.as_ref().is_some_and(|base| {
                            base == "*"
                                || base == &path
                                || rule.scope.include_children
                                    && path.starts_with(&format!("{base}/"))
                        }) || rule.scope.conversation.as_deref() == Some(&id))
                }) {
                    members.insert(actor.clone());
                }
            }
            self.publish("channel.membership.initialize", None,
                &format!("Initialized membership for #{path} from creators, posters and explicit follows"),
                json!({"channel_id":id,"members":members,"version":1,"default_join":matches!(path.as_str(), "general" | "cm-general")}),
                "system", "System", "system", &format!("membership-v1:{id}"), "")?;
        }
        Ok(())
    }

    pub(super) fn enroll_participant(&mut self, actor: &str) -> Result<()> {
        if self.enrolled.contains(actor) {
            return Ok(());
        }
        self.shared_mutation_allowed()?;
        let channels: Vec<_> = self
            .channels
            .iter()
            .filter(|(path, id)| self.channel_info(path, id)["default_join"] == true)
            .map(|(_, id)| id.clone())
            .collect();
        self.publish(
            "membership.enrollment",
            None,
            "Enrolled a participant in the default messaging channels",
            json!({"participant_id":actor,"default_channels":channels}),
            "system",
            "System",
            "system",
            &format!("membership-enrollment-v1:{actor}"),
            "",
        )?;
        Ok(())
    }

    pub fn enroll_participants(&mut self, people: &[Person]) -> Result<()> {
        if self.degraded.is_some() || !self.is_coordinator() {
            return Ok(());
        }
        let mut actors: BTreeSet<_> = self
            .names
            .keys()
            .chain(self.personal.keys())
            .cloned()
            .collect();
        actors.extend(self.memberships.values().flatten().cloned());
        actors.extend(self.conversations.values().flatten().cloned());
        actors.extend(people.iter().map(|p| p.id.clone()));
        actors.insert("owner".into());
        for actor in actors {
            self.enroll_participant(&actor)?;
        }
        Ok(())
    }

    pub(super) fn change_membership(
        &mut self,
        actor: &str,
        p: &Value,
        people: &[Person],
    ) -> Result<Value> {
        let (key, digest, prior) = self.request(actor, p, "channel.membership")?;
        if let Some(event) = prior {
            return Ok(self.membership_result(actor, &event));
        }
        let mut selector = json!({});
        if let Some(path) = p.get("path") {
            selector["channel"] = path.clone();
        }
        if let Some(id) = p.get("conversation") {
            selector["conversation"] = id.clone();
        }
        let (id, _) = self.resolve(actor, &selector, people, false)?;
        let path = self
            .channels
            .iter()
            .find(|(_, cid)| **cid == id)
            .map(|(p, _)| p.clone())
            .ok_or_else(|| err("not_found", "Channel not found"))?;
        let add = p["action"] == "add_member";
        let member = if add {
            let mut channel = self.channel_info(&path, &id);
            self.channel_permissions(actor, &mut channel);
            if channel["can_manage"] != true {
                return Err(err(
                    "unauthorized",
                    "Only channel admins and Owner can add members",
                ));
            }
            let member = required(p, "participant_id")?;
            if member != "owner"
                && !self.names.contains_key(&member)
                && !people.iter().any(|person| person.id == member)
            {
                return Err(err(
                    "not_found",
                    "Participant not found; resolve an ID with chat_people",
                ));
            }
            member
        } else {
            if p.get("participant_id")
                .is_some_and(|member| member != actor)
            {
                return Err(err(
                    "invalid_params",
                    "Join/leave are self-only; admins can use add_member",
                ));
            }
            actor.to_owned()
        };
        let joined = add || p["action"] == "join";
        let name = self
            .names
            .get(actor)
            .map(|n| n.name.clone())
            .or_else(|| {
                people
                    .iter()
                    .find(|p| p.id == actor)
                    .map(|p| p.name.clone())
            })
            .unwrap_or_else(|| {
                if actor == "owner" {
                    "Owner".into()
                } else {
                    "Agent".into()
                }
            });
        let body = if add {
            let member_name = self
                .names
                .get(&member)
                .map(|n| n.name.as_str())
                .or_else(|| {
                    people
                        .iter()
                        .find(|p| p.id == member)
                        .map(|p| p.name.as_str())
                })
                .unwrap_or(if member == "owner" { "Owner" } else { &member });
            format!("{name} added {member_name} to #{path}")
        } else {
            format!("{name} {} #{path}", if joined { "joined" } else { "left" })
        };
        let mut data = json!({"participant_id":member,"joined":joined});
        if add {
            data["action"] = json!("add_member");
        }
        let event = self.publish(
            "channel.membership",
            Some(&id),
            &body,
            data,
            actor,
            &name,
            if actor == "owner" { "owner" } else { "agent" },
            &key,
            &digest,
        )?;
        let _ = self.project();
        Ok(self.membership_result(actor, &event))
    }

    fn membership_result(&self, actor: &str, event: &Value) -> Value {
        let mut result = self.send_response(event);
        result["membership"] = json!({"joined":event["data"]["joined"],"revision":event["id"],
            "participant_id":event["data"]["participant_id"],
            "current_joined":self.joined(strv(&event["data"], "participant_id"), strv(event, "conversation_id"))});
        if event["data"]["action"] == "add_member" {
            result["membership"]["added_by"] = event["actor"]["id"].clone();
        }
        if let Some((path, id)) = self
            .channels
            .iter()
            .find(|(_, id)| event["conversation_id"] == **id)
        {
            let mut c = self.channel_info(path, id);
            self.channel_permissions(actor, &mut c);
            result["channel"] = c;
        }
        result["status"] = json!("saved");
        result
    }
}
