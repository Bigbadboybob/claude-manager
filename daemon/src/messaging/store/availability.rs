//! `messaging.availability`: Owner's availability level as a replicated,
//! Owner-only event. See `crate::owner_availability` for the model.
use super::*;
use crate::owner_availability::{self as availability, Level};

impl Store {
    /// Shape check shared by local reduce and replica ingest preflight.
    pub(super) fn validate_owner_availability(e: &Value) -> Result<()> {
        if e["actor"]["id"] != "owner" || !e["conversation_id"].is_null() {
            return Err(err(
                "invalid_record",
                "Owner availability is Owner-authored and conversation-free",
            ));
        }
        if !availability::valid_level(&e["data"]["level"]) {
            return Err(err("invalid_record", "Unknown availability level"));
        }
        Ok(())
    }

    pub(super) fn reduce_owner_availability(&mut self, e: &Value) -> Result<()> {
        Self::validate_owner_availability(e)?;
        let mut record = e["data"].clone();
        record["changed_at"] = e["created_at"].clone();
        record["event_id"] = e["id"].clone();
        // Projected for lock-free readers (`ping`). A failed projection never
        // fails the event: the in-memory value stays authoritative and the
        // next change or restart rewrites the file.
        let path = availability::projection_path(self.cm_root());
        if let Err(error) = serde_json::to_vec_pretty(&record)
            .map_err(io::Error::other)
            .and_then(|bytes| project_file(&path, &bytes))
        {
            eprintln!("cm messaging: owner availability projection: {error}");
        }
        self.owner_availability = record;
        Ok(())
    }

    /// `{action:"get"}` for anyone; `{action:"set", level, note?, source?,
    /// request_id}` for Owner only. `level:"unset"` clears it (legacy
    /// deliver-everything behavior).
    pub fn availability(&mut self, actor: &str, p: &Value) -> Result<Value> {
        let exposure = |s: &Store| availability::exposure_at(&s.owner_availability, Utc::now());
        match p["action"].as_str().unwrap_or("get") {
            "get" => Ok(json!({"owner_availability": exposure(self)})),
            "set" => {
                if actor != "owner" {
                    return Err(err("unauthorized", "Only Owner sets availability"));
                }
                let (key, digest, prior) = self.request(actor, p, availability::EVENT_TYPE)?;
                if let Some(e) = prior {
                    return Ok(json!({"owner_availability": exposure(self), "event_id": e["id"]}));
                }
                let raw = required(p, "level")?;
                let level = if raw == "unset" {
                    None
                } else {
                    Some(Level::parse(&raw).ok_or_else(|| {
                        err(
                            "invalid_params",
                            "level must be away, around, focused, on-call or unset",
                        )
                    })?)
                };
                let note = p["note"].as_str().unwrap_or("").trim().to_owned();
                if note.chars().count() > availability::NOTE_MAX_CHARS || note.chars().any(char::is_control) {
                    return Err(err("invalid_params", "note must be one line of at most 200 characters"));
                }
                let source = match p["source"].as_str().unwrap_or("cli") {
                    s @ ("tui" | "cli") => s,
                    _ => return Err(err("invalid_params", "source must be tui or cli")),
                };
                let level_str = level.map(Level::as_str);
                let data = json!({
                    "level": level_str,
                    "previous": self.owner_availability["level"],
                    "source": source,
                    "note": note,
                });
                let body = format!("Owner availability: {}", level_str.unwrap_or("unset"));
                let e = self.publish(
                    availability::EVENT_TYPE,
                    None,
                    &body,
                    data,
                    "owner",
                    "Owner",
                    "owner",
                    &key,
                    &digest,
                )?;
                Ok(json!({"owner_availability": exposure(self), "event_id": e["id"]}))
            }
            _ => Err(err("invalid_params", "action must be get or set")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(s: &mut Store, actor: &str, level: &str, key: &str) -> Result<Value> {
        s.availability(actor, &json!({"action":"set","level":level,"request_id":key,"source":"tui"}))
    }

    #[test]
    fn owner_sets_level_agents_read_it_and_it_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        assert_eq!(s.availability("agent:x:a", &json!({})).unwrap()["owner_availability"]["set"], false);
        assert_eq!(set(&mut s, "agent:x:a", "away", "a1").unwrap_err().code, "unauthorized");
        let first = set(&mut s, "owner", "focused", "o1").unwrap();
        assert_eq!(first["owner_availability"]["level"], "focused");
        assert!(first["owner_availability"]["previous"].is_null());
        // Retrying the same request returns the same event and publishes nothing.
        let count = s.events.len();
        assert_eq!(set(&mut s, "owner", "focused", "o1").unwrap()["event_id"], first["event_id"]);
        assert_eq!(s.events.len(), count);
        set(&mut s, "owner", "away", "o2").unwrap();
        let read = s.availability("agent:x:a", &json!({"action":"get"})).unwrap();
        assert_eq!(read["owner_availability"]["level"], "away");
        assert_eq!(read["owner_availability"]["previous"], "focused");
        assert_eq!(set(&mut s, "owner", "busy", "o3").unwrap_err().code, "invalid_params");
        // The projection serves lock-free readers, and both survive a reopen.
        assert_eq!(crate::owner_availability::exposure(tmp.path())["level"], "away");
        drop(s);
        let s = Store::open(tmp.path()).unwrap();
        assert_eq!(s.owner_availability["level"], "away");
        // Unset returns to deliver-everything.
        let mut s = s;
        set(&mut s, "owner", "unset", "o4").unwrap();
        assert_eq!(crate::owner_availability::exposure(tmp.path())["set"], false);
    }

    #[test]
    fn forged_or_malformed_availability_records_are_rejected() {
        let ok = json!({"type":"owner.availability","actor":{"id":"owner"},"conversation_id":null,"data":{"level":"around"}});
        assert!(Store::validate_owner_availability(&ok).is_ok());
        let mut forged = ok.clone();
        forged["actor"]["id"] = json!("agent:x:a");
        assert!(Store::validate_owner_availability(&forged).is_err());
        let mut bad = ok.clone();
        bad["data"]["level"] = json!("asleep");
        assert!(Store::validate_owner_availability(&bad).is_err());
    }
}
