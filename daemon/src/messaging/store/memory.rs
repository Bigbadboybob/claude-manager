//! Aggregate diagnostic reads. No payload traversal, mutation or cache warming.
use super::{Personal, Store};
use serde::Serialize;
use std::time::Instant;

#[derive(Debug, Serialize)]
pub(crate) struct ParticipantCounts {
    pub complete: bool,
    pub reason: Option<&'static str>,
    pub total: usize,
    pub visited: usize,
    /// Sums only over visited participants; never extrapolated to the total.
    pub operations_visited_sum: Option<usize>,
    pub monitors_visited_sum: Option<usize>,
}

#[derive(Debug, Serialize)]
pub(crate) struct StoreMemorySnapshot {
    pub events_len: usize,
    pub events_capacity: usize,
    pub shared_requests: usize,
    pub replication_receipts: usize,
    pub replication_rejections: usize,
    pub coverage_scopes: usize,
    pub coverage_marks: usize,
    pub participants: ParticipantCounts,
    pub search_active_estimated_bytes: usize,
    pub search_building_estimated_bytes: usize,
    pub search_warm_conversations: usize,
    /// Only the fallback Vec, NOT the separate mpsc disposal queue.
    pub search_retired_local_count: usize,
    pub search_disposal_queue_count: Option<usize>,
    pub search_disposal_queue_reason: &'static str,
}

impl Store {
    pub(crate) fn memory_snapshot(&self, deadline: Instant, limit: usize) -> StoreMemorySnapshot {
        let (active, building, warm, retired) = self.read_model.search.memory_counts();
        StoreMemorySnapshot {
            events_len: self.events.len(),
            events_capacity: self.events.capacity(),
            shared_requests: self.requests.len(),
            replication_receipts: self.replication.receipts.len(),
            replication_rejections: self.replication.rejections.len(),
            coverage_scopes: self.replication.coverage.len(),
            coverage_marks: self.replication.marks.len(),
            participants: participant_counts(
                self.personal.values(),
                self.personal.len(),
                deadline,
                limit,
            ),
            search_active_estimated_bytes: active,
            search_building_estimated_bytes: building,
            search_warm_conversations: warm,
            search_retired_local_count: retired,
            search_disposal_queue_count: None,
            search_disposal_queue_reason: "unavailable: mpsc disposer has no depth accessor",
        }
    }
}

fn participant_counts<'a>(
    mut values: impl Iterator<Item = &'a Personal>,
    total: usize,
    deadline: Instant,
    limit: usize,
) -> ParticipantCounts {
    let mut result = ParticipantCounts {
        complete: false,
        reason: None,
        total,
        visited: 0,
        operations_visited_sum: None,
        monitors_visited_sum: None,
    };
    let (mut operations, mut monitors) = (0, 0);
    while result.visited < total {
        if Instant::now() >= deadline {
            result.reason = Some("deadline");
            break;
        }
        if result.visited >= limit {
            result.reason = Some("participant_limit");
            break;
        }
        let Some(personal) = values.next() else {
            result.reason = Some("incomplete_iterator");
            break;
        };
        operations += personal.operations.len();
        monitors += personal.monitors.len();
        result.visited += 1;
    }
    result.complete = result.visited == total;
    if result.complete || result.visited > 0 {
        result.operations_visited_sum = Some(operations);
        result.monitors_visited_sum = Some(monitors);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn memory_snapshot_participant_limit_and_deadline_are_explicit() {
        let mut a = Personal::default();
        a.operations.insert(
            "request".into(),
            super::super::personal::Operation {
                digest: "digest".into(),
                result: serde_json::json!({"secret":"must not be serialized"}),
            },
        );
        a.monitors.insert(
            "monitor".into(),
            serde_json::from_value(serde_json::json!({
                "id":"monitor", "scope":{}, "mode":"once", "notify":"wake",
                "include_self":false, "created_at":"2026-10-28T00:00:00Z",
                "state":"active", "start":0, "scanned":0, "hit_high":0,
                "hits":0, "acknowledged":0
            }))
            .unwrap(),
        );
        let b = Personal::default();
        let full = participant_counts(
            [&a, &b].into_iter(),
            2,
            Instant::now() + Duration::from_secs(1),
            2,
        );
        assert!(full.complete);
        assert_eq!(full.operations_visited_sum, Some(1));
        assert_eq!(full.monitors_visited_sum, Some(1));
        let part = participant_counts(
            [&a, &b].into_iter(),
            2,
            Instant::now() + Duration::from_secs(1),
            1,
        );
        assert!(!part.complete);
        assert_eq!(part.reason, Some("participant_limit"));
        assert_eq!(
            (part.total, part.visited, part.operations_visited_sum),
            (2, 1, Some(1))
        );
        let expired = participant_counts([&a].into_iter(), 1, Instant::now(), 1);
        assert_eq!(expired.reason, Some("deadline"));
        assert_eq!(expired.operations_visited_sum, None);
        assert!(!serde_json::to_string(&part).unwrap().contains("secret"));
        let empty = participant_counts([].into_iter(), 0, Instant::now(), 0);
        assert!(empty.complete);
        assert_eq!(empty.operations_visited_sum, Some(0));
    }

    #[test]
    fn memory_snapshot_store_counts_are_read_only_and_do_not_warm_search() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(tmp.path()).unwrap();
        store.events.reserve(32);
        store
            .replication
            .marks
            .insert(1, serde_json::json!({"secret":"cursor payload"}));
        let before = store.publication_position();
        let snapshot = store.memory_snapshot(Instant::now() + Duration::from_secs(1), 4096);
        assert_eq!(snapshot.events_len, store.events.len());
        assert_eq!(snapshot.events_capacity, store.events.capacity());
        assert_eq!(snapshot.coverage_marks, 1);
        assert_eq!(snapshot.search_active_estimated_bytes, 0);
        assert_eq!(snapshot.search_warm_conversations, 0);
        assert_eq!(snapshot.search_disposal_queue_count, None);
        assert_eq!(store.publication_position(), before);
        let wire = serde_json::to_value(snapshot).unwrap();
        assert!(wire["search_disposal_queue_count"].is_null());
        assert!(!wire.to_string().contains("cursor payload"));
    }
}
