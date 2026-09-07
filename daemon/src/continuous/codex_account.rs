//! Strict reader for the external Codex probe. Claude's legacy reader remains
//! separate. A valid-looking credential file is never recovery evidence.
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

const FRESH_SECS: f64 = 3660.0;

#[derive(Deserialize)]
struct Config {
    schema_version: u32,
    executable: String,
    executable_hash: String,
    cli_version: String,
    requested_model: String,
    model_provider: String,
    configuration_id: String,
    codex_home: String,
    host_model: Option<String>,
    provider_config: Option<serde_json::Value>,
    auth_command_hash: Option<String>,
}

#[derive(Deserialize)]
struct ProbeState {
    schema_version: u32,
    engine: String,
    status: String,
    started_at: f64,
    checked_at: f64,
    executable: String,
    executable_hash: String,
    cli_version: String,
    requested_model: String,
    observed_model: String,
    model_provider: String,
    configuration_id: String,
    codex_home: String,
    provider_config: Option<serde_json::Value>,
    auth_command_hash: Option<String>,
}

fn json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let mut data = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(128 * 1024 + 1)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() > 128 * 1024 {
        return None;
    }
    serde_json::from_slice(&data).ok()
}

fn runtime_hash(path: &Path) -> Option<String> {
    // Hash the native CLI selected by the deployment, not npm's JS launcher.
    // A state-file pin alone must not survive an executable update.
    let mut file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > 512 * 1024 * 1024 {
        return None;
    }
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Some(format!("{:x}", digest.finalize()))
}

pub fn ok_after(config_path: &Path, state_path: &Path, blocked_at: u64, now: f64) -> bool {
    let validate = || -> Option<bool> {
        let config: Config = json(config_path)?;
        let state: ProbeState = json(state_path)?;
        if config.schema_version != 1 || state.schema_version != 1 || state.engine != "codex" || state.status != "OK"
            || !now.is_finite() || !state.started_at.is_finite() || !state.checked_at.is_finite()
            // detected_at has whole-second precision. Conservatively require
            // a start beyond the entire second that contains the detection.
            || state.started_at < blocked_at as f64 + 1.0 || state.checked_at < state.started_at
            || state.checked_at > now || now - state.checked_at > FRESH_SECS
            || state.checked_at - state.started_at > 90.0
            || config.requested_model.is_empty() || config.cli_version.is_empty() || config.configuration_id.is_empty()
            || !matches!(config.model_provider.as_str(), "openai" | "cm_pool") || !Path::new(&config.executable).is_absolute()
            || state.executable != config.executable || state.executable_hash != config.executable_hash
            || state.cli_version != config.cli_version || state.model_provider != config.model_provider
            || state.requested_model != config.requested_model || state.observed_model != config.requested_model
            || state.configuration_id != config.configuration_id
            || state.provider_config != config.provider_config || state.auth_command_hash != config.auth_command_hash
            || state.codex_home != config.codex_home || !Path::new(&config.codex_home).is_absolute()
        {
            return Some(false);
        }
        let user_config_path = Path::new(&config.codex_home).join("config.toml");
        let user_config: toml::Value = match std::fs::read_to_string(user_config_path) {
            Ok(text) => toml::from_str(&text).ok()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                toml::Value::Table(Default::default())
            }
            Err(_) => return None,
        };
        if user_config
            .get("model_provider")
            .and_then(toml::Value::as_str)
            .unwrap_or("openai")
            != config.model_provider
            || user_config.get("model").and_then(toml::Value::as_str)
                != config.host_model.as_deref()
            || user_config
                .get("model_providers")
                .and_then(|p| p.get("openai"))
                .is_some()
        {
            return Some(false);
        }
        if config.model_provider == "cm_pool" {
            let definition = user_config.get("model_providers")?.get("cm_pool")?;
            let definition = serde_json::to_value(definition).ok()?;
            let command = definition.pointer("/auth/command")?.as_str()?;
            let expected = serde_json::json!({"name":"openai", "base_url":"http://127.0.0.1:2455/backend-api/codex",
                "wire_api":"responses", "supports_websockets":true,
                "auth":{"command":command,"timeout_ms":5000,"refresh_interval_ms":300000}});
            if definition != expected || config.provider_config.as_ref() != Some(&definition)
                || !Path::new(command).is_absolute()
                || config.auth_command_hash.as_ref() != Some(&runtime_hash(Path::new(command))?) {
                return Some(false);
            }
        } else if config.provider_config.is_some() || config.auth_command_hash.is_some() {
            return Some(false);
        }
        Some(runtime_hash(Path::new(&config.executable))? == config.executable_hash)
    };
    validate() == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pool_probe_requires_matching_provider_and_unchanged_credential_helper() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("codex");
        let helper = dir.path().join("helper");
        std::fs::write(&binary, "pinned-native-runtime").unwrap();
        std::fs::write(&helper, "pinned-helper").unwrap();
        let definition = json!({"name":"openai","base_url":"http://127.0.0.1:2455/backend-api/codex",
            "wire_api":"responses","supports_websockets":true,
            "auth":{"command":helper,"timeout_ms":5000,"refresh_interval_ms":300000}});
        let host = json!({"model":"gpt-5.6-sol","model_provider":"cm_pool","model_providers":{"cm_pool":definition}});
        let hp = dir.path().join("config.toml");
        std::fs::write(&hp, toml::to_string(&host).unwrap()).unwrap();
        let config = json!({"schema_version":1,"executable":binary,"executable_hash":runtime_hash(&binary).unwrap(),
            "cli_version":"0.153.4","requested_model":"gpt-5.6-sol","host_model":"gpt-5.6-sol","model_provider":"cm_pool",
            "configuration_id":"pool-pin","codex_home":dir.path(),"provider_config":definition,"auth_command_hash":runtime_hash(&helper).unwrap()});
        let mut state = config.clone();
        state.as_object_mut().unwrap().extend(json!({"engine":"codex","status":"OK","started_at":101.0,"checked_at":110.0,"observed_model":"gpt-5.6-sol"}).as_object().unwrap().clone());
        let cp = dir.path().join("probe-config.json");
        let sp = dir.path().join("probe-state.json");
        std::fs::write(&cp, config.to_string()).unwrap();
        std::fs::write(&sp, state.to_string()).unwrap();
        assert!(ok_after(&cp, &sp, 100, 111.0));
        for key in ["provider_config", "auth_command_hash"] {
            let mut invalid = state.clone();
            invalid.as_object_mut().unwrap().remove(key);
            std::fs::write(&sp, invalid.to_string()).unwrap();
            assert!(!ok_after(&cp, &sp, 100, 111.0), "missing {key}");
        }
        std::fs::write(&sp, state.to_string()).unwrap();
        for pointer in ["/model_providers/cm_pool/base_url", "/model_providers/cm_pool/auth/command", "/model_provider"] {
            let mut changed = host.clone();
            *changed.pointer_mut(pointer).unwrap() = json!("changed");
            std::fs::write(&hp, toml::to_string(&changed).unwrap()).unwrap();
            assert!(!ok_after(&cp, &sp, 100, 111.0), "changed {pointer}");
        }
        std::fs::write(&hp, toml::to_string(&host).unwrap()).unwrap();
        std::fs::write(&helper, "changed-helper").unwrap();
        assert!(!ok_after(&cp, &sp, 100, 111.0));
    }

    #[test]
    fn codex_account_requires_fresh_matching_post_hold_runtime_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("codex");
        std::fs::write(&binary, "pinned-native-runtime").unwrap();
        let config = json!({"schema_version":1,"executable":binary,"executable_hash":runtime_hash(&binary).unwrap(),"cli_version":"0.153.4","requested_model":"gpt-5.6-sol","model_provider":"openai","configuration_id":"host-openai-pin", "codex_home":dir.path().join(".codex")});
        let mut state = config.clone();
        state.as_object_mut().unwrap().extend(json!({"engine":"codex","status":"OK","started_at":101.0,"checked_at":110.0,"observed_model":"gpt-5.6-sol"}).as_object().unwrap().clone());
        let cp = dir.path().join("config.json");
        let sp = dir.path().join("state.json");
        std::fs::write(&cp, config.to_string()).unwrap();
        std::fs::write(&sp, state.to_string()).unwrap();
        assert!(ok_after(&cp, &sp, 100, 111.0));
        assert!(!ok_after(&cp, &sp, 101, 111.0));
        assert!(!ok_after(&cp, &sp, 100, 4000.0));
        for (key, invalid) in [
            ("schema_version", json!(2)),
            ("engine", json!("claude")),
            ("status", json!("AUTH_EXPIRED")),
            ("started_at", json!(100.5)),
            ("started_at", json!(120.0)),
            ("checked_at", json!(112.0)),
            ("executable_hash", json!("old-runtime")),
            ("requested_model", json!("other-model")),
            ("observed_model", json!("other-model")),
            ("cli_version", json!("0.134.0")),
            ("configuration_id", json!("another-host")),
            ("model_provider", json!("another-provider")),
        ] {
            let mut wrong = state.clone();
            wrong[key] = invalid;
            std::fs::write(&sp, wrong.to_string()).unwrap();
            assert!(!ok_after(&cp, &sp, 100, 111.0), "{key}");
        }
        for key in [
            "started_at",
            "checked_at",
            "observed_model",
            "configuration_id",
        ] {
            let mut missing = state.clone();
            missing.as_object_mut().unwrap().remove(key);
            std::fs::write(&sp, missing.to_string()).unwrap();
            assert!(
                !ok_after(&cp, &sp, 100, 111.0),
                "no mtime fallback for {key}"
            );
        }
        std::fs::write(&sp, state.to_string()).unwrap();
        std::fs::write(&binary, "updated-runtime").unwrap();
        assert!(!ok_after(&cp, &sp, 100, 111.0));
    }
}
