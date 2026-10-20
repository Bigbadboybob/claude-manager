//! Copy bounded batches under the store lock; tokenize, sort and free them off it.
use super::*;

type Handle = Mutex<Option<Store>>;
const BATCH_MESSAGES: usize = 128;
const BATCH_BYTES: usize = 64 * 1024;
const COPY_BUDGET: Duration = Duration::from_millis(2);

struct Document {
    key: ReadCursor,
    event_index: usize,
    sender: String,
    body: String,
}
impl Document {
    fn copy(store: &Store, i: usize) -> Self {
        let e = &store.events[i].event;
        Self {
            key: ReadCursor::of(e),
            event_index: i,
            sender: strv(&e["actor"], "id").into(),
            body: strv(e, "body").into(),
        }
    }
    fn index(self, index: &mut ConversationSearch, sorted: bool) {
        index.document(self.key, self.event_index, &self.sender, &self.body, sorted);
    }
}

struct Build {
    cid: String,
    fence: usize,
    upper: Option<ReadCursor>,
    after: Option<ReadCursor>,
    delta: usize,
    index: ConversationSearch,
}
impl Build {
    fn new(store: &Store, cid: String) -> Self {
        let fence = store.events.len();
        let upper = store.read_model.conversations[&cid]
            .timeline
            .last()
            .map(|(k, _)| k.clone());
        Self {
            cid,
            fence,
            upper,
            after: None,
            delta: fence,
            index: ConversationSearch::default(),
        }
    }
    fn snapshot(&mut self, store: &Store) -> (Vec<Document>, bool) {
        let start = Instant::now();
        let timeline = &store.read_model.conversations[&self.cid].timeline;
        let lo = self
            .after
            .as_ref()
            .map_or(0, |key| timeline.partition_point(|(k, _)| k <= key));
        let mut batch = Vec::new();
        let mut bytes = 0;
        let mut next = lo;
        for (key, i) in timeline[lo..].iter().take(BATCH_MESSAGES) {
            if self.upper.as_ref().is_none_or(|upper| key > upper) {
                return (batch, true);
            }
            self.after = Some(key.clone());
            next += 1;
            // Backfill can insert behind our canonical cursor. Every arrival
            // after this fence is picked up by delta(), independent of its key.
            if *i < self.fence {
                let doc = Document::copy(store, *i);
                bytes += doc.body.len();
                batch.push(doc);
            }
            if bytes >= BATCH_BYTES || start.elapsed() >= COPY_BUDGET {
                break;
            }
        }
        let done = timeline
            .get(next)
            .is_none_or(|(k, _)| self.upper.as_ref().is_none_or(|upper| k > upper));
        (batch, done)
    }
    fn delta(&mut self, store: &Store) -> Vec<Document> {
        let start = Instant::now();
        let end = (self.delta + BATCH_MESSAGES).min(store.events.len());
        let mut batch = Vec::new();
        let mut bytes = 0;
        while self.delta < end {
            let i = self.delta;
            self.delta += 1;
            let e = &store.events[i].event;
            if strv(e, "conversation_id") == self.cid && Store::readable_event(e) {
                let doc = Document::copy(store, i);
                bytes += doc.body.len();
                batch.push(doc);
            }
            if bytes >= BATCH_BYTES || start.elapsed() >= COPY_BUDGET {
                break;
            }
        }
        batch
    }
    fn install(&mut self, store: &mut Store) {
        store
            .read_model
            .search
            .conversations
            .insert(self.cid.clone(), std::mem::take(&mut self.index));
        store.read_model.search.pending.remove(&self.cid);
    }
}

#[derive(Default, Clone)]
pub(super) struct Probe {
    #[cfg(test)]
    pub max_lock_ns: Arc<std::sync::atomic::AtomicU64>,
    #[cfg(test)]
    entered: Option<std::sync::mpsc::Sender<()>>,
    #[cfg(test)]
    release: Option<Arc<Mutex<std::sync::mpsc::Receiver<()>>>>,
}
impl Probe {
    fn record(&self, _elapsed: Duration) {
        #[cfg(test)]
        self.max_lock_ns.fetch_max(
            _elapsed.as_nanos() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    fn pause(&mut self) {
        #[cfg(test)]
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
            self.release
                .take()
                .unwrap()
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
    }
}

fn with_store<T>(
    handle: &Weak<Handle>,
    token: &str,
    probe: &Probe,
    f: impl FnOnce(&mut Store) -> T,
) -> Option<T> {
    let handle = handle.upgrade()?;
    let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
    let start = Instant::now();
    let store = slot.as_mut()?;
    if store.read_model.search.worker.as_deref() != Some(token) {
        return None;
    }
    let result = f(store);
    let elapsed = start.elapsed();
    drop(slot);
    probe.record(elapsed);
    Some(result)
}

fn run(handle: &Weak<Handle>, token: &str, mut probe: Probe) {
    loop {
        let Some(Some(mut build)) = with_store(handle, token, &probe, |s| {
            s.read_model
                .search
                .pending
                .first()
                .cloned()
                .map(|cid| Build::new(s, cid))
        }) else {
            return;
        };
        loop {
            let Some((batch, done)) = with_store(handle, token, &probe, |s| build.snapshot(s))
            else {
                return;
            };
            probe.pause(); // test barrier: deliberately outside every store/delivery lock
            for doc in batch {
                doc.index(&mut build.index, false);
            }
            if done {
                break;
            }
        }
        // Cold construction appends unsorted postings and sorts each once.
        build.index.sort();
        loop {
            let Some(batch) = with_store(handle, token, &probe, |s| {
                if build.delta == s.events.len() {
                    build.install(s);
                    None
                } else {
                    Some(build.delta(s))
                }
            }) else {
                return;
            };
            let Some(batch) = batch else {
                break;
            };
            for doc in batch {
                doc.index(&mut build.index, true);
            }
        }
    }
}

impl Store {
    pub(crate) fn start_search_worker(&mut self, handle: &Arc<Handle>) -> Result<()> {
        let search = &mut self.read_model.search;
        if search.worker.is_some() || search.pending.is_empty() {
            return Ok(());
        }
        let token = uuid();
        search.worker = Some(token.clone());
        let handle = Arc::downgrade(handle);
        #[cfg(test)]
        let probe = search.probe.clone();
        #[cfg(not(test))]
        let probe = Probe::default();
        if let Err(e) = std::thread::Builder::new()
            .name("cm-chat-index".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&handle, &token, probe)
                }));
                if result.is_err() {
                    eprintln!("cm messaging: search worker failed; next search will retry");
                }
                // A new Store can replace this one during recovery. An old worker
                // must neither install into it nor clear its new worker token.
                if let Some(handle) = handle.upgrade() {
                    let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(s) = slot.as_mut() {
                        if s.read_model.search.worker.as_deref() == Some(&token) {
                            s.read_model.search.worker = None;
                        }
                    }
                }
            })
        {
            search.worker = None;
            return Err(e.into());
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn pause_search_for_test(
        &mut self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        self.read_model.search.probe.entered = Some(entered);
        self.read_model.search.probe.release = Some(Arc::new(Mutex::new(blocked)));
        (waiting, release)
    }

    // Direct Store correctness fixtures have no shared handle or dispatcher.
    // They drive the same snapshot/delta builder synchronously; RPC tests cover
    // production scheduling and concurrent sends.
    #[cfg(test)]
    pub(super) fn finish_search_for_test(&mut self) {
        while let Some(cid) = self.read_model.search.pending.first().cloned() {
            let mut build = Build::new(self, cid);
            loop {
                let (batch, done) = build.snapshot(self);
                for doc in batch {
                    doc.index(&mut build.index, false);
                }
                if done {
                    break;
                }
            }
            build.index.sort();
            while build.delta < self.events.len() {
                for doc in build.delta(self) {
                    doc.index(&mut build.index, true);
                }
            }
            build.install(self);
        }
    }
}
