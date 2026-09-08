//! Continuous-task channel ownership comes from scheduler records, never a
//! caller-supplied task slug. Network membership work runs outside delivery.
use super::{ChatError, Store, TaskBinding};
use crate::{continuous::task, state::DaemonState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskChannel {
    pub subscription_id: String,
    pub space_id: String,
    pub channel_id: String,
    pub revision: u64,
    pub session_uid: Option<String>,
}
impl TaskChannel {
    pub fn handover(&mut self, uid: &str) {
        if self.session_uid.as_deref() != Some(uid) {
            self.revision = self.revision.saturating_add(1);
            self.session_uid = Some(uid.into());
        }
    }
}
pub fn configuration(
    state: &Arc<Mutex<DaemonState>>,
    value: &Value,
) -> Result<Option<TaskChannel>, ChatError> {
    if value.is_null() {
        return Ok(None);
    }
    let id = value["channel_id"].as_str().ok_or_else(|| ChatError {
        code: "invalid_params".into(),
        message: "messaging needs channel_id".into(),
    })?;
    super::rpc::initialize(state)?;
    let store = state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .messaging
        .clone();
    let slot = store.lock().unwrap_or_else(|p| p.into_inner());
    let store = slot.as_ref().unwrap();
    if !store
        .channels()
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == id)
    {
        return Err(ChatError {
            code: "not_found".into(),
            message: "Configure a known public channel for the task".into(),
        });
    }
    Ok(Some(TaskChannel {
        subscription_id: uuid::Uuid::new_v4().to_string(),
        space_id: store.space_id.clone(),
        channel_id: id.into(),
        revision: 0,
        session_uid: None,
    }))
}
/// Refresh the fence synchronously at a messaging RPC boundary. The background
/// loop also runs this so a replacement doesn't need to discover chat first.
pub fn refresh(state: &Arc<Mutex<DaemonState>>) -> Result<Vec<(String, Value)>, ChatError> {
    let (handle, root, ids) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (
            s.messaging.clone(),
            s.messaging_root.clone(),
            s.sessions
                .values()
                .filter_map(|p| p.continuous_task_id.clone())
                .collect::<BTreeSet<_>>(),
        )
    };
    let mut ids = ids;
    {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(store) = slot.as_ref() {
            ids.extend(store.configured_task_ids());
        }
    }
    let records = ids
        .iter()
        .filter_map(|id| task::load_one(id))
        .collect::<Vec<_>>();
    let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_none() {
        *slot = Some(Store::open(&root)?);
    }
    let store = slot.as_mut().unwrap();
    if store.messaging_frozen() {
        return Ok(Vec::new());
    }
    let mut joins = Vec::new();
    let mut active = BTreeSet::new();
    for task in records {
        if !task.enabled {
            continue;
        }
        let Some(binding) = task.messaging else {
            continue;
        };
        let Some(uid) = binding
            .session_uid
            .filter(|uid| task.current_session_uid.as_ref() == Some(uid))
        else {
            continue;
        };
        if binding.space_id != store.space_id {
            continue;
        }
        let actor = store.participant_id(&uid);
        store.bind_task_subscription(TaskBinding {
            id: binding.subscription_id.clone(),
            task_id: task.task_id,
            channel_id: binding.channel_id.clone(),
            actor: actor.clone(),
            revision: binding.revision,
            active: true,
            acknowledged: BTreeSet::new(),
        })?;
        active.insert(binding.subscription_id.clone());
        let params = json!({"action":"join","conversation":binding.channel_id,"request_id":format!("task-join:{}:{}",binding.subscription_id,binding.revision)});
        let joined = store
            .channels_for(&actor)?
            .iter()
            .any(|c| c["id"] == binding.channel_id && c["joined"] == true);
        if !joined {
            if store.is_coordinator() {
                store.channel_action(&actor, &params, &[])?;
            } else {
                joins.push((actor, params));
            }
        }
    }
    store.fence_task_subscriptions(&active)?;
    Ok(joins)
}
pub fn spawn(state: &Arc<Mutex<DaemonState>>) {
    let weak = Arc::downgrade(state);
    std::thread::spawn(move || loop {
        let Some(state) = weak.upgrade() else {
            break;
        };
        match refresh(&state) {
            Ok(joins) => {
                let sync = state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .messaging_sync
                    .clone();
                if let Some(sync) = sync {
                    for (actor, params) in joins {
                        let _ = sync.request(&actor, "messaging.channels", &params);
                    }
                }
            }
            Err(e) => eprintln!("cm task messaging: {e}"),
        }
        drop(state);
        std::thread::sleep(Duration::from_secs(2));
    });
}
