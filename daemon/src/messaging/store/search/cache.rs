use super::*;

pub(super) struct Failure {
    pub until: Instant,
    pub message: String,
}
pub(super) struct Oversized {
    bytes: usize,
    documents: usize,
    budget: usize,
}

impl Default for SearchIndex {
    fn default() -> Self {
        Self {
            conversations: HashMap::new(),
            pending: Default::default(),
            worker: None,
            active: None,
            failed: HashMap::new(),
            oversized: HashMap::new(),
            requested: HashMap::new(),
            budget: 512 * 1024 * 1024,
            total_bytes: 0,
            building_bytes: 0,
            clock: 0,
            retired: Vec::new(),
            disposer: None,
            handle: Weak::new(),
            #[cfg(test)]
            probe: Default::default(),
        }
    }
}

impl SearchIndex {
    pub(super) fn touch(&mut self, cid: &str) {
        self.clock = self.clock.saturating_add(1);
        if let Some(c) = self.conversations.get_mut(cid) {
            c.last_search = self.clock;
        }
    }
    pub(super) fn enqueue(&mut self, cid: String, direct: bool) {
        self.clock = self.clock.saturating_add(1);
        self.requested.insert(cid.clone(), self.clock);
        self.failed.remove(&cid);
        if self.active.as_ref() == Some(&cid) {
            return;
        }
        if direct {
            self.pending.retain(|c| *c != cid);
            self.pending.push_front(cid);
        } else if !self.pending.contains(&cid) {
            self.pending.push_back(cid);
        }
    }
    pub(super) fn fail(&mut self, cid: String, message: impl Into<String>) {
        self.pending.retain(|c| *c != cid);
        self.requested.remove(&cid);
        self.failed.insert(
            cid,
            Failure {
                until: Instant::now() + Duration::from_secs(30),
                message: message.into(),
            },
        );
    }
    pub(super) fn failure(&self, cid: &str) -> Option<ChatError> {
        self.failed
            .get(cid)
            .filter(|f| Instant::now() < f.until)
            .map(|f| {
                err(
                    "search_index_failed",
                    format!(
                        "{}; retry after {} seconds",
                        f.message,
                        f.until.saturating_duration_since(Instant::now()).as_secs() + 1
                    ),
                )
            })
    }
    pub(super) fn too_large(&mut self, cid: &str, documents: usize) -> Option<ChatError> {
        let size = self.oversized.get(cid)?;
        if size.documents != documents || size.budget != self.budget {
            self.oversized.remove(cid);
            return None;
        }
        Some(err(
            "search_too_large",
            format!(
                "Conversation index requires at least {} bytes; search budget is {} bytes",
                size.bytes, size.budget
            ),
        ))
    }
    pub(super) fn remember_oversize(&mut self, cid: String, bytes: usize, documents: usize) {
        self.pending.retain(|c| *c != cid);
        self.requested.remove(&cid);
        self.oversized.insert(
            cid,
            Oversized {
                bytes,
                documents,
                budget: self.budget,
            },
        );
    }
    fn retire(&mut self, index: ConversationSearch) {
        // The disposer is started by the first search read, never by a send or
        // replica publication. It lives until this store is dropped, so warm
        // growth can free an evicted dictionary off-lock even with no searches.
        if let Some(disposer) = &self.disposer {
            match disposer.send(index) {
                Ok(()) => return,
                Err(error) => self.retired.push(error.0),
            }
        } else {
            self.retired.push(index);
        }
    }
    pub(super) fn start_disposer(&mut self) -> Result<()> {
        if self.disposer.is_none() {
            let (sender, receiver) = std::sync::mpsc::channel::<ConversationSearch>();
            std::thread::Builder::new()
                .name("cm-chat-index-free".into())
                .spawn(move || {
                    for index in receiver {
                        drop(index);
                    }
                })?;
            self.disposer = Some(sender);
        }
        for index in std::mem::take(&mut self.retired) {
            self.retire(index);
        }
        Ok(())
    }
    pub(super) fn enforce_budget(&mut self) {
        if self.total_bytes.saturating_add(self.building_bytes) <= self.budget {
            return;
        }
        // An individually oversized entry cannot stay, regardless of its LRU
        // rank. Remove it first so it does not evict unrelated warm indexes.
        let oversized: Vec<_> = self
            .conversations
            .iter()
            .filter(|(_, c)| c.bytes > self.budget)
            .map(|(cid, _)| cid.clone())
            .collect();
        for cid in oversized {
            let index = self.conversations.remove(&cid).unwrap();
            self.total_bytes -= index.bytes;
            self.remember_oversize(cid, index.bytes, index.documents.len());
            self.retire(index);
        }
        while self.total_bytes.saturating_add(self.building_bytes) > self.budget {
            let Some(cid) = self
                .conversations
                .iter()
                .min_by_key(|(_, c)| c.last_search)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            let retired = self.conversations.remove(&cid).unwrap();
            self.total_bytes -= retired.bytes;
            // Large dictionaries must also be freed outside the slot lock.
            self.retire(retired);
        }
    }
    pub(super) fn reserve(&mut self, bytes: usize) -> bool {
        if bytes > self.budget {
            return false;
        }
        self.building_bytes = bytes;
        self.enforce_budget();
        true
    }
    pub(super) fn install(&mut self, cid: String, mut index: ConversationSearch) {
        self.building_bytes = 0;
        self.clock = self.clock.saturating_add(1);
        index.last_search = self.requested.remove(&cid).unwrap_or(self.clock);
        self.total_bytes += index.bytes;
        self.conversations.insert(cid.clone(), index);
        self.pending.retain(|c| *c != cid);
        self.active = None;
        self.failed.remove(&cid);
        self.oversized.remove(&cid);
        self.enforce_budget();
    }
}

impl Store {
    pub(crate) fn configure_search(&mut self, handle: &Arc<Mutex<Option<Store>>>, budget: usize) {
        let search = &mut self.read_model.search;
        search.handle = Arc::downgrade(handle);
        if search.budget != budget {
            search.budget = budget;
            search.failed.clear();
            search.oversized.clear();
            search.enforce_budget();
        }
    }
}
