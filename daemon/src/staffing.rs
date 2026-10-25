//! Owner staffing guidance. Selection is local and never waits for the API.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod runtime;
pub(crate) use runtime::dispatch;
pub use runtime::{exposure, record_spawn, start, Cache};

pub const KINDS: [&str; 6] = [
    "math",
    "design",
    "architecture",
    "metric",
    "coding",
    "fanout",
];
pub const PRECEDENCE: &str =
    "explicit type/model > strong preference > work rule > lean preference > default";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Engine {
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "claude-code")]
    Claude,
}
impl Engine {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude-code",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "claude" | "claude-code" => Some(Self::Claude),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strength {
    Strong,
    Lean,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preference {
    pub engine: Engine,
    pub strength: Strength,
    pub reason: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub default_engine: Engine,
    pub rules: BTreeMap<String, Engine>,
    pub preference: Option<Preference>,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            default_engine: Engine::Codex, // today's launch default
            rules: KINDS
                .into_iter()
                .map(|k| {
                    (
                        k.into(),
                        if matches!(k, "coding" | "fanout") {
                            Engine::Claude
                        } else {
                            Engine::Codex
                        },
                    )
                })
                .collect(),
            preference: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u64,
    pub policy: Policy,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            version: 0,
            policy: Policy {
                default_engine: Engine::Codex,
                rules: BTreeMap::new(),
                preference: None,
            },
            updated_at: None,
        }
    }
}

pub struct Decision {
    pub engine: String,
    pub note: Option<String>,
}

impl Policy {
    pub fn validate(&self) -> Result<(), String> {
        if self.rules.keys().any(|key| !KINDS.contains(&key.as_str())) {
            return Err(
                "unknown work kind; use math, design, architecture, metric, coding or fanout"
                    .into(),
            );
        }
        if let Some(p) = &self.preference {
            if p.reason.trim().is_empty()
                || p.reason.chars().count() > 500
                || p.reason.chars().any(char::is_control)
            {
                return Err(
                    "preference reason must be a nonempty single line of at most 500 characters"
                        .into(),
                );
            }
        }
        Ok(())
    }
    pub fn active_preference(&self, now: DateTime<Utc>) -> Option<&Preference> {
        self.preference.as_ref().filter(|p| p.expires_at > now)
    }
    pub fn recommend(&self, kind: Option<&str>, now: DateTime<Utc>) -> (Engine, String) {
        let preference = self.active_preference(now);
        if let Some(p) = preference.filter(|p| p.strength == Strength::Strong) {
            return (
                p.engine,
                format!("strong preference: {} (until {})", p.reason, p.expires_at),
            );
        }
        if let Some((kind, engine)) = kind.and_then(|k| self.rules.get(k).map(|e| (k, *e))) {
            return (engine, format!("{kind} rule"));
        }
        if let Some(p) = preference {
            return (
                p.engine,
                format!("lean preference: {} (until {})", p.reason, p.expires_at),
            );
        }
        (self.default_engine, "default".into())
    }
    pub fn select(
        &self,
        explicit_type: Option<&str>,
        model: Option<&str>,
        kind: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Decision, String> {
        if kind.is_some_and(|k| !KINDS.contains(&k)) {
            return Err("invalid work_kind".into());
        }
        let (recommended, _) = self.recommend(kind, now);
        let engine = if let Some(explicit) = explicit_type {
            if explicit != "bash" && Engine::parse(explicit).is_none() {
                return Err("invalid session type".into());
            }
            Engine::parse(explicit).map_or_else(|| explicit.to_owned(), |e| e.wire().into())
        } else if let Some(model) = model {
            crate::models::infer_engine(model)
                .and_then(Engine::parse)
                .ok_or_else(|| {
                    "cannot infer engine for explicit model; pass type as well".to_owned()
                })?
                .wire()
                .into()
        } else {
            recommended.wire().into()
        };
        let note = self.note(&engine, kind, now);
        Ok(Decision { engine, note })
    }
    /// Native forks retain their source engine. They comment on policy without
    /// switching engines or pretending that conversations are portable.
    pub fn note(&self, chosen: &str, kind: Option<&str>, now: DateTime<Utc>) -> Option<String> {
        let engine = Engine::parse(chosen)?;
        let (recommended, reason) = self.recommend(kind, now);
        let preference = self.active_preference(now);
        if engine == recommended && preference.is_none_or(|p| p.engine == engine) {
            return None;
        }
        let preference = preference
            .filter(|p| p.engine != engine)
            .map(|p| format!("; current preference {}: {}", p.engine.wire(), p.reason))
            .unwrap_or_default();
        Some(format!("Staffing: chosen {}; policy recommends {} ({reason}){preference}. Precedence: {PRECEDENCE}.", engine.wire(), recommended.wire()))
    }
    pub fn digest(&self, now: DateTime<Utc>) -> String {
        let (engine, reason) = self.recommend(None, now);
        format!(
            "{} ({reason}); rules: {}. Precedence: {PRECEDENCE}.",
            engine.wire(),
            self.rules
                .iter()
                .map(|(k, v)| format!("{k}={}", v.wire()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staffing_precedence_expiry_and_explicit_model() {
        let now = Utc::now();
        let mut p = Policy::default();
        assert_eq!(
            p.select(None, None, Some("fanout"), now).unwrap().engine,
            "claude-code"
        );
        p.preference = Some(Preference {
            engine: Engine::Codex,
            strength: Strength::Strong,
            reason: "higher limits".into(),
            expires_at: now + chrono::Duration::days(1),
        });
        assert_eq!(
            p.select(None, None, Some("fanout"), now).unwrap().engine,
            "codex"
        );
        let explicit = p
            .select(Some("claude-code"), None, Some("fanout"), now)
            .unwrap();
        assert_eq!(explicit.engine, "claude-code");
        assert!(explicit.note.unwrap().contains(PRECEDENCE));
        assert_eq!(
            p.select(None, Some("claude-sonnet-5-5"), None, now)
                .unwrap()
                .engine,
            "claude-code"
        );
        assert!(p
            .select(None, Some("unknown-custom-model"), None, now)
            .is_err());
        p.preference.as_mut().unwrap().strength = Strength::Lean;
        let decision = p.select(None, None, Some("fanout"), now).unwrap();
        assert_eq!(decision.engine, "claude-code");
        assert!(decision.note.unwrap().contains("current preference codex"));
        p.default_engine = Engine::Claude;
        assert_eq!(p.recommend(None, now).0, Engine::Codex);
        p.preference.as_mut().unwrap().expires_at = now;
        assert_eq!(p.recommend(None, now).0, Engine::Claude);
        assert!(p.note("claude-code", Some("fanout"), now).is_none());
        assert!(p.note("bash", None, now).is_none());
    }
}
