//! Per-message, per-actor reaction sets; no message-create event or wake.
use super::*;
const EMOJI: [&str; 5] = ["✅", "👀", "👍", "❌", "🎉"];
impl Store {
    pub(super) fn validate_reaction(&self, e: &Value) -> Result<()> {
        let data = &e["data"];
        let actor = required(&e["actor"], "id")?;
        if !EMOJI.contains(&strv(data, "emoji"))
            || !data["remove"].is_boolean()
            || e["actor"]["kind"] != if actor == "owner" { "owner" } else { "agent" }
        {
            return Err(err(
                "invalid_reaction",
                "Use one of ✅ 👀 👍 ❌ 🎉 and a boolean remove",
            ));
        }
        if actor != "owner" && !self.names.contains_key(&actor) {
            return Err(err(
                "unauthorized",
                "Reaction actor has no retained identity",
            ));
        }
        let target = self
            .published(&required(data, "message_id")?)
            .ok_or_else(|| err("dependency_missing", "Reaction message not retained"))?;
        if target.event["type"] != "message.create"
            || target.event["conversation_id"] != e["conversation_id"]
            || !self.visible(&actor, strv(&target.event, "conversation_id"))
        {
            return Err(err(
                "unauthorized",
                "Reaction target is not visible in this conversation",
            ));
        }
        Ok(())
    }
    pub(super) fn reduce_reaction(&mut self, e: &Value) -> Result<()> {
        if e["type"] != "reaction" {
            return Ok(());
        }
        self.validate_reaction(e)?;
        let current = self
            .read_model
            .reactions
            .entry(required(&e["data"], "message_id")?)
            .or_default()
            .entry(required(&e["data"], "emoji")?)
            .or_default()
            .entry(required(&e["actor"], "id")?)
            .or_default();
        let order = ReadCursor::of(e);
        if order > current.0 {
            *current = (
                order,
                e["data"]["remove"] != true,
                required(&e["actor"], "name")?,
            );
        }
        Ok(())
    }
    pub(super) fn reaction_summary(&self, actor: &str, id: &str) -> Value {
        let mut out = serde_json::Map::new();
        for (emoji, actors) in self.read_model.reactions.get(id).into_iter().flatten() {
            let mut names = vec![];
            let mut mine = false;
            for (id, (_, active, old_name)) in actors {
                if !active {
                    continue;
                }
                names.push(if id == "owner" {
                    "Owner".to_owned()
                } else {
                    self.names
                        .get(id)
                        .map(|n| n.name.clone())
                        .unwrap_or_else(|| old_name.clone())
                });
                mine |= id == actor;
            }
            if !names.is_empty() {
                out.insert(
                    emoji.clone(),
                    json!({"count":names.len(),"names":names,"mine":mine}),
                );
            }
        }
        Value::Object(out)
    }
    pub fn react(&mut self, actor: &str, p: &Value) -> Result<Value> {
        let id = required(p, "message_id")?;
        let emoji = required(p, "emoji")?;
        if !EMOJI.contains(&emoji.as_str()) || p.get("remove").is_some_and(|v| !v.is_boolean()) {
            return Err(err(
                "invalid_reaction",
                "Use one of ✅ 👀 👍 ❌ 🎉 and a boolean remove",
            ));
        }
        let target = self
            .published(&id)
            .filter(|e| {
                e.event["type"] == "message.create"
                    && self.visible(actor, strv(&e.event, "conversation_id"))
            })
            .ok_or_else(|| err("not_found", "Reaction message not found"))?
            .event
            .clone();
        let (key, digest, prior) = self.request(actor, p, "reaction")?;
        let event = if let Some(prior) = prior {
            prior
        } else {
            let name = if actor == "owner" {
                "Owner"
            } else {
                self.names
                    .get(actor)
                    .map(|n| n.name.as_str())
                    .ok_or_else(|| err("name_required", "Claim a name with chat_send first"))?
            }
            .to_owned();
            self.publish(
                "reaction",
                target["conversation_id"].as_str(),
                "Reaction updated",
                json!({"message_id":id,"emoji":emoji,"remove":p["remove"]==true}),
                actor,
                &name,
                if actor == "owner" { "owner" } else { "agent" },
                &key,
                &digest,
            )?
        };
        if p["remove"] != true && mention_recipients(&target).contains(&actor) {
            self.advance_cursors(
                actor,
                BTreeMap::from([(
                    required(&target, "conversation_id")?,
                    ReadCursor::of(&target),
                )]),
            )?;
        }
        Ok(
            json!({"event_id":event["id"],"message_id":id,"reactions":self.reaction_summary(actor,&id),"counts":self.counts(actor)?,"replication":self.event_replication(strv(&event,"id"))}),
        )
    }
}
