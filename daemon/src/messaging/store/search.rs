//! Lazy per-conversation indexes for sender and word/phrase search. Cold reads
//! build from the existing timeline; publication/ingest only update warm indexes.
use super::*;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

mod build;
mod cache;
mod page;
mod scope;
pub(super) use scope::SearchScope;

#[cfg(test)]
mod tests;

#[derive(Default)]
struct Posting {
    // Both contain local document IDs. Canonical order can differ from append
    // order when a replica receives old history after live traffic.
    canonical: Vec<usize>,
    arrivals: Vec<usize>,
}
impl Posting {
    fn bytes(&self) -> usize {
        (self.canonical.capacity() + self.arrivals.capacity()) * std::mem::size_of::<usize>()
    }
    fn insert(&mut self, index: usize, documents: &[(ReadCursor, usize)]) {
        if self
            .canonical
            .last()
            .is_none_or(|i| documents[*i].0 < documents[index].0)
        {
            self.canonical.push(index);
        } else {
            let at = self
                .canonical
                .partition_point(|i| documents[*i].0 < documents[index].0);
            self.canonical.insert(at, index);
        }
        if self
            .arrivals
            .last()
            .is_none_or(|i| documents[*i].1 < documents[index].1)
        {
            self.arrivals.push(index);
        } else {
            let at = self
                .arrivals
                .partition_point(|i| documents[*i].1 < documents[index].1);
            self.arrivals.insert(at, index);
        }
    }
}
#[derive(Default)]
struct ConversationSearch {
    // Shared keys plus global event indices; postings use local document IDs.
    documents: Vec<(ReadCursor, usize)>,
    senders: HashMap<String, Posting>,
    words: HashMap<String, Posting>,
    bytes: usize,
    last_search: u64,
}
pub(super) struct SearchIndex {
    conversations: HashMap<String, ConversationSearch>,
    pending: std::collections::VecDeque<String>,
    worker: Option<String>,
    active: Option<String>,
    failed: HashMap<String, cache::Failure>,
    oversized: HashMap<String, cache::Oversized>,
    requested: HashMap<String, u64>,
    budget: usize,
    total_bytes: usize,
    building_bytes: usize,
    clock: u64,
    retired: Vec<ConversationSearch>,
    disposer: Option<std::sync::mpsc::Sender<ConversationSearch>>,
    handle: Weak<Mutex<Option<Store>>>,
    #[cfg(test)]
    probe: build::Probe,
}
fn words(text: &str) -> Vec<String> {
    normalize(text).unicode_words().map(str::to_owned).collect()
}
impl ConversationSearch {
    fn header_bytes(&self) -> usize {
        self.documents.capacity() * std::mem::size_of::<(ReadCursor, usize)>()
            + (self.words.capacity() + self.senders.capacity())
                * 2
                * std::mem::size_of::<(String, Posting)>()
    }
    fn index(&mut self, e: &Published, event_index: usize) {
        self.document(
            ReadCursor::of(&e.event),
            event_index,
            strv(&e.event["actor"], "id"),
            strv(&e.event, "body"),
            true,
        );
    }
    fn document(
        &mut self,
        key: ReadCursor,
        event_index: usize,
        sender: &str,
        body: &str,
        sorted: bool,
    ) {
        let before_header = self.header_bytes();
        let mut added = key.event_id.capacity();
        let index = self.documents.len();
        self.documents.push((key, event_index));
        let mut posting_added = 0;
        let mut insert = |posting: &mut Posting| {
            let before = posting.bytes();
            if sorted {
                posting.insert(index, &self.documents);
            } else {
                posting.canonical.push(index);
                posting.arrivals.push(index);
            }
            posting_added += posting.bytes() - before;
        };
        if !self.senders.contains_key(sender) {
            added += sender.len();
        }
        insert(self.senders.entry(sender.into()).or_default());
        // Borrow words from one normalized body. Only new dictionary entries
        // allocate strings; sorting/dedup replaces a per-message BTreeSet.
        let normalized = normalize(body);
        let mut terms: Vec<_> = normalized.unicode_words().collect();
        terms.sort_unstable();
        terms.dedup();
        for word in terms {
            if let Some(posting) = self.words.get_mut(word) {
                insert(posting);
            } else {
                added += word.len();
                insert(self.words.entry(word.into()).or_default());
            }
        }
        self.bytes += added + posting_added + self.header_bytes() - before_header;
    }
    fn sort(&mut self) {
        for p in self.senders.values_mut().chain(self.words.values_mut()) {
            p.canonical
                .sort_unstable_by(|a, b| self.documents[*a].0.cmp(&self.documents[*b].0));
            p.arrivals.sort_unstable_by_key(|i| self.documents[*i].1);
        }
    }
}
impl SearchIndex {
    /// O(1) accounting only. The mpsc disposer queue has no depth accessor.
    pub(super) fn memory_counts(&self) -> (usize, usize, usize, usize) {
        (self.total_bytes, self.building_bytes, self.conversations.len(), self.retired.len())
    }

    pub(super) fn index(&mut self, e: &Published, index: usize) {
        // Cold at startup: no keys, body tokenization, postings or extra replay.
        if self.conversations.is_empty() {
            return;
        }
        if let Some(c) = self
            .conversations
            .get_mut(strv(&e.event, "conversation_id"))
        {
            if Store::readable_event(&e.event) {
                let before = c.bytes;
                c.index(e, index);
                self.total_bytes += c.bytes - before;
            }
        }
        self.enforce_budget();
    }
}

#[cfg(test)]
pub(super) fn skip_index_for_test() -> bool {
    tests::skip_hook()
}

pub(super) struct SearchFilter {
    pub sender: Option<String>,
    words: Vec<String>,
}
impl SearchFilter {
    pub fn active(&self) -> bool {
        self.sender.is_some() || !self.words.is_empty()
    }
}

impl Store {
    pub(super) fn prepare_search(
        &mut self,
        actor: &str,
        conv: Option<&str>,
        public: bool,
        dms: bool,
    ) -> Result<()> {
        self.search_scope(actor, conv, public, dms, None)
            .map(|_| ())
    }

    pub(super) fn search_filter(&self, p: &Value, people: &[Person]) -> Result<SearchFilter> {
        let text = |key: &str| -> Result<Option<&str>> {
            p.get(key)
                .filter(|v| !v.is_null())
                .map(|v| {
                    v.as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            err("invalid_params", format!("{key} must be a nonempty string"))
                        })
                })
                .transpose()
        };
        let sender = if let Some(reference) = text("sender")? {
            // Pin name resolution across pages: a rename/released name must not
            // silently switch the author halfway through a frozen history read.
            if let Some(cursor) = p.get("cursor").filter(|c| !c.is_null()) {
                Some(
                    required(cursor, "search_sender")
                        .map_err(|_| err("invalid_cursor", "Missing search sender"))?,
                )
            } else if ["owner", "system"].contains(&normalize(reference).as_str()) {
                Some(normalize(reference))
            } else if reference.starts_with("agent:") {
                Some(reference.into())
            } else {
                match self.resolve_participant_ref(reference, people) {
                    mentions::ParticipantRef::Found(id) => Some(id),
                    mentions::ParticipantRef::NotFound => {
                        return Err(err(
                            "not_found",
                            "Sender not found; use chat_people to resolve a participant ID",
                        ))
                    }
                    mentions::ParticipantRef::Ambiguous => {
                        return Err(err(
                            "ambiguous_sender",
                            "Sender name is ambiguous; use a participant ID",
                        ))
                    }
                }
            }
        } else {
            None
        };
        let words = if let Some(query) = text("query")? {
            let terms = words(query);
            if terms.is_empty() {
                return Err(err(
                    "invalid_params",
                    "query must contain at least one word",
                ));
            }
            terms
        } else {
            Vec::new()
        };
        Ok(SearchFilter { sender, words })
    }

    /// Seek the smallest posting and expose every candidate before filtering.
    /// The page merger budgets visits across all conversations, including
    /// rejected intersections, phrase checks, time windows and other filters.
    pub(super) fn search_candidates<'a>(
        &'a self,
        cid: &str,
        search: &'a SearchFilter,
        anchor: Option<&ReadCursor>,
        newest: bool,
        after: Option<u64>,
        high: u64,
    ) -> Box<dyn Iterator<Item = (&'a Published, bool)> + 'a> {
        let index = &self.read_model.search;
        let Some(c) = index.conversations.get(cid) else {
            return Box::new(std::iter::empty());
        };
        let mut lists = Vec::new();
        if let Some(sender) = &search.sender {
            let Some(list) = c.senders.get(sender) else {
                return Box::new(std::iter::empty());
            };
            lists.push(list);
        }
        for word in search.words.iter().collect::<BTreeSet<_>>() {
            let Some(list) = c.words.get(word) else {
                return Box::new(std::iter::empty());
            };
            lists.push(list);
        }
        let Some((smallest, _)) = lists
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| p.arrivals.len())
        else {
            return Box::new(std::iter::empty());
        };
        let primary = lists.swap_remove(smallest);
        let candidates: Box<dyn Iterator<Item = &usize>> = if let Some(after) = after {
            let lo = primary
                .arrivals
                .partition_point(|i| self.events[c.documents[*i].1].position <= after);
            let hi = primary
                .arrivals
                .partition_point(|i| self.events[c.documents[*i].1].position <= high);
            Box::new(primary.arrivals[lo.min(hi)..hi].iter())
        } else if newest {
            let hi = anchor.map_or(primary.canonical.len(), |key| {
                primary
                    .canonical
                    .partition_point(|i| c.documents[*i].0 < *key)
            });
            Box::new(primary.canonical[..hi].iter().rev())
        } else {
            let lo = anchor.map_or(0, |key| {
                primary
                    .canonical
                    .partition_point(|i| c.documents[*i].0 <= *key)
            });
            Box::new(primary.canonical[lo..].iter())
        };
        Box::new(candidates.map(move |&i| {
            let eligible = self.events[c.documents[i].1].position <= high
                && lists.iter().all(|p| {
                    p.arrivals
                        .binary_search_by_key(&c.documents[i].1, |j| c.documents[*j].1)
                        .is_ok()
                });
            (&self.events[c.documents[i].1], eligible)
        }))
    }
}
