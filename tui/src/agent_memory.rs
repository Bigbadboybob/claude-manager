//! Snapshot storage shared with the host daemon.
pub use cm_daemon::agent_memory::*;

/// The catalog stays pinned to the host selected when it was opened.
#[derive(Clone, Default)]
pub enum SnapshotStore {
    #[default]
    Local,
    Remote {
        socket: std::path::PathBuf,
        token: String,
    },
    Unavailable(String),
}

impl SnapshotStore {
    pub fn request<T: serde::de::DeserializeOwned>(&self, params: serde_json::Value) -> Result<T> {
        let value = match self {
            Self::Local => catalog(&params)?,
            Self::Remote { socket, token } => crate::client_session::rpc_catalog_control(
                socket,
                token,
                "snapshot.control",
                params,
            )
            .map_err(|e| SnapshotError::Io(std::io::Error::other(e.to_string())))?,
            Self::Unavailable(host) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "Host {host} is unavailable"
                ))))
            }
        };
        Ok(serde_json::from_value(value)?)
    }
    pub fn list(&self) -> Result<Vec<Snapshot>> {
        self.request(serde_json::json!({"action": "list"}))
    }
    pub fn rename(&self, name: &str, new_name: &str) -> Result<()> {
        self.request(serde_json::json!({"action":"rename", "name":name, "new_name":new_name}))
    }
    pub fn delete(&self, name: &str) -> Result<()> {
        self.request(serde_json::json!({"action":"delete", "name":name}))
    }
    pub fn preview(&self, name: &str) -> Result<(Vec<String>, Vec<String>)> {
        self.request(serde_json::json!({"action":"preview", "name":name}))
    }
}
