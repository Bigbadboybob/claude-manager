//! Effect-time scheduler authority for task monitors. Reconciliation is only
//! maintenance: a skipped pass or a later scheduler handover proves nothing.
use super::Store;
use crate::continuous::{adoption, task};
use std::{
    collections::BTreeMap,
    ops::{Deref, DerefMut},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Authority {
    Current,
    Stale,
    Deferred,
}

struct TaskGuard {
    read: task::ReadGuard,
    _lifecycle: task::LifecycleGuard,
}

/// Opaque ownership held through local publication. No blocking state/store
/// acquisition or network I/O is permitted while these file guards are held.
pub(crate) struct Guards {
    _tasks: Vec<TaskGuard>,
}

pub(crate) struct View<'a> {
    store: &'a mut Store,
    previous: Option<BTreeMap<String, Authority>>,
    guards: Guards,
}

impl<'a> View<'a> {
    pub(crate) fn new(store: &'a mut Store) -> Self {
        let bindings = store.active_task_bindings();
        let mut tasks = BTreeMap::new();
        for binding in &bindings {
            tasks.entry(binding.task_id.clone()).or_insert_with(|| {
                // Both acquisitions are nonblocking: the caller already owns
                // the store (and sometimes delivery) lock. Adoption takes the
                // lifecycle lock before its durable intent; scheduler task
                // writes take the state-file lock, even without the global gate.
                let lifecycle = task::try_lock_lifecycle(&binding.task_id).ok()?;
                let read = task::try_read_guard(&binding.task_id).ok()?;
                adoption::ensure_available(&binding.task_id).ok()?;
                Some(TaskGuard {
                    read,
                    _lifecycle: lifecycle,
                })
            });
        }
        let mut authority = BTreeMap::new();
        for binding in bindings {
            let current = tasks.get(&binding.task_id).and_then(Option::as_ref);
            let status = match current {
                None => Authority::Deferred,
                Some(guard) => {
                    let t = &guard.read.task;
                    let matches = t.task_id == binding.task_id
                        && t.enabled
                        && t.adoption_pending.is_none()
                        && t.task_channel != task::TaskChannelPolicy::Off
                        && t.messaging.as_ref().is_some_and(|m| {
                            m.space_id == store.space_id
                                && m.channel_id == binding.channel_id
                                && m.subscription_id == binding.id
                                && m.revision == binding.revision
                                && m.session_uid == t.current_session_uid
                                && t.current_session_uid
                                    .as_ref()
                                    .is_some_and(|uid| store.participant_id(uid) == binding.actor)
                        });
                    if matches {
                        Authority::Current
                    } else {
                        Authority::Stale
                    }
                }
            };
            authority.insert(binding.id, status);
        }
        let previous = store.task_authority.replace(authority);
        Self {
            store,
            previous,
            guards: Guards {
                _tasks: tasks.into_values().flatten().collect(),
            },
        }
    }

    /// Transfer guards to the delivery caller before releasing the store. That
    /// caller uses precomputed previews and nonblocking session checks only.
    pub(crate) fn take_guards(&mut self) -> Guards {
        std::mem::replace(&mut self.guards, Guards { _tasks: Vec::new() })
    }
}
impl Deref for View<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        self.store
    }
}
impl DerefMut for View<'_> {
    fn deref_mut(&mut self) -> &mut Store {
        self.store
    }
}
impl Drop for View<'_> {
    fn drop(&mut self) {
        self.store.task_authority = self.previous.take();
    }
}
