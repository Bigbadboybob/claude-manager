//! Bounded parallel disk preparation; reducers and error ordering stay serial.
use super::*;
use std::sync::mpsc;

pub(super) fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map_or(2, usize::from)
        .clamp(1, 8)
}

pub(super) struct Prepared {
    pub journal: Value,
    // Defer event errors until after serial journal validation, just as before.
    pub event: Option<Result<(String, Value)>>,
}

fn prepare(root: &Path, path: &Path) -> Result<Prepared> {
    let journal = load(path)?;
    let event = (journal["kind"] == "publish").then(|| {
        let rel = required(&journal["data"], "event_path")?;
        if Path::new(&rel).is_absolute()
            || Path::new(&rel)
                .components()
                .any(|x| !matches!(x, std::path::Component::Normal(_)))
        {
            return Err(err("invalid_record", "Unsafe event path"));
        }
        let bytes = fs::read(root.join(&rel))?;
        if hash(&bytes) != strv(&journal, "event_sha256") {
            return Err(err("corrupt_event", "Event digest mismatch"));
        }
        Ok((rel, serde_json::from_slice(&bytes)?))
    });
    Ok(Prepared { journal, event })
}

pub(super) fn replay(
    store: &mut Store,
    paths: &[PathBuf],
    workers: usize,
    scan: &mut replication::JournalScan,
) -> Result<()> {
    let workers = workers.clamp(1, 8).min(paths.len());
    if workers <= 1 {
        for path in paths {
            store.apply_rebuilt(path, prepare(&store.root, path)?, scan)?;
        }
        return Ok(());
    }
    let root = &store.root.clone();
    std::thread::scope(|scope| {
        let mut readers = Vec::with_capacity(workers);
        for worker in 0..workers {
            // At most eight prepared records queued per worker, rather than
            // materializing a second copy of the entire history in memory.
            let (send, receive) = mpsc::sync_channel(8);
            readers.push(receive);
            std::thread::Builder::new()
                .name(format!("cm-msg-read-{worker}"))
                .spawn_scoped(scope, move || {
                    for path in paths.iter().skip(worker).step_by(workers) {
                        if send.send(prepare(root, path)).is_err() {
                            break; // Serial replay failed; cancel prefetched work.
                        }
                    }
                })?;
        }
        for (i, path) in paths.iter().enumerate() {
            let prepared = readers[i % workers]
                .recv()
                .map_err(|_| err("storage_error", "Messaging journal reader stopped"))??;
            store.apply_rebuilt(path, prepared, scan)?;
        }
        // On error these receivers drop before scope joins the workers, so
        // no producer remains stuck in a bounded send.
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reopen(root: &Path, workers: usize) -> Store {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("messaging.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        Store::open_locked_with_workers(root, lock, workers).unwrap()
    }

    fn fingerprint(s: &Store) -> Value {
        json!({
            "events": s.events.iter().map(|e| json!([e.event, e.position, e.received_at, e.event_sha256])).collect::<Vec<_>>(),
            "index": s.event_index, "position": s.position, "clock": s.clock,
            "names": s.names, "channels": s.channels(), "memberships": s.memberships,
            "membership_revisions": s.membership_revisions, "enrolled": s.enrolled,
            "conversations": s.conversations, "requests": s.requests,
            "norms": s.norms, "channel_norms": s.channel_norms,
            "availability": s.owner_availability, "receipts": s.replication.receipts,
            "rejections": s.replication.rejections, "coverage": s.replication.coverage,
            "marks": s.replication.marks, "degraded": s.degraded,
        })
    }

    fn fixture(root: &Path) -> Store {
        let mut s = Store::open(root).unwrap();
        s.create_channel(
            "owner",
            &json!({"path":"work/rebuild","request_id":"channel"}),
        )
        .unwrap();
        let person = Person {
            id: s.participant_id("scout"),
            name: "Scout".into(),
            session_uid: "scout".into(),
            task: None,
            present: true,
            kind: "agent".into(),
        };
        s.send(
            &person.id,
            &person.session_uid,
            "agent",
            &json!({"channel":"general","name":"Scout","body":"Ready","request_id":"claim"}),
            &[person.clone()],
        )
        .unwrap();
        s.send(
            "owner",
            "",
            "owner",
            &json!({"dm":person.id,"body":"Private reply","request_id":"dm"}),
            &[person],
        )
        .unwrap();
        s.availability(
            "owner",
            &json!({"action":"set","level":"around","request_id":"available"}),
        )
        .unwrap();
        // More records than the whole prefetch window, with metadata and
        // interleaved replication decisions that depend on earlier publishes.
        for i in 0..96 {
            let reply = s
                .send(
                    "owner",
                    "",
                    "owner",
                    &json!({
                        "channel":if i % 2 == 0 { "general" } else { "work/rebuild" },
                        "body":format!("Message {i}"),"request_id":format!("message-{i}")
                    }),
                    &[],
                )
                .unwrap();
            if i % 7 == 0 {
                let id = reply["event_id"].as_str().unwrap();
                let receipt = s.replication.receipts[id].clone();
                s.replication.receipts.remove(id);
                s.record_receipt(&receipt).unwrap();
            }
        }
        s
    }

    #[test]
    fn parallel_rebuild_equals_live_state_and_serial_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let s = fixture(tmp.path());
        let expected = fingerprint(&s);
        drop(s);
        for workers in [1, 2, 8] {
            let s = reopen(tmp.path(), workers);
            assert!(s.degraded.is_none(), "{:?}", s.degraded);
            assert_eq!(fingerprint(&s), expected, "{workers} readers");
        }
    }

    #[test]
    fn parallel_rebuild_keeps_first_error_and_partial_state() {
        for failure in ["digest", "journal", "path"] {
            let tmp = tempfile::tempdir().unwrap();
            let s = fixture(tmp.path());
            let first = &s.events[3];
            let journal_path = s.journal_dir().join(format!("{:020}.json", first.position));
            let mut j = load(&journal_path).unwrap();
            let expected_error = match failure {
                "digest" => "corrupt_event: Event digest mismatch",
                "journal" => {
                    j["protocol"] = json!(99);
                    "invalid_record: Conflicting or unsupported journal record"
                }
                _ => {
                    j["data"]["event_path"] = json!("../outside.json");
                    "invalid_record: Unsafe event path"
                }
            };
            // Even a bad journal must win over its own corrupt event. A later
            // fast JSON error must not win over the earlier ordered failure.
            fs::write(s.event_path(&first.event).unwrap(), b"broken event").unwrap();
            atomic_replace(&journal_path, &j).unwrap();
            fs::write(
                s.journal_dir().join(format!("{:020}.json", s.position)),
                b"{",
            )
            .unwrap();
            drop(s);
            let serial = reopen(tmp.path(), 1);
            assert_eq!(serial.degraded.as_deref(), Some(expected_error));
            let expected = fingerprint(&serial);
            drop(serial);
            let parallel = reopen(tmp.path(), 8);
            assert_eq!(fingerprint(&parallel), expected, "{failure}");
        }
    }

    #[test]
    fn replication_decisions_use_cached_verified_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let s = fixture(tmp.path());
        drop(s);
        let mut s = reopen(tmp.path(), 4);
        let event = s.events.last().unwrap().clone();
        let id = strv(&event.event, "id");
        let receipt = s.replication.receipts[id].clone();
        assert_eq!(
            event.event_sha256,
            hash(&fs::read(s.event_path(&event.event).unwrap()).unwrap())
        );
        fs::remove_file(s.event_path(&event.event).unwrap()).unwrap();
        s.record_receipt(&receipt).unwrap();
        let mut journal = json!({"kind":"replication.status","event_id":id,
            "event_sha256":event.event_sha256,"data":{"status":"replicated","receipt":receipt}});
        s.apply_replication_journal(&journal).unwrap();
        journal["event_sha256"] = json!("0".repeat(64));
        assert_eq!(
            s.apply_replication_journal(&journal).unwrap_err().code,
            "invalid_receipt"
        );
        drop(s);
        // Caching does not mask loss/corruption on the next rebuild.
        assert!(reopen(tmp.path(), 4).degraded.is_some());
    }
}
