//! Ignored timing harness for replica storage costs. Run explicitly, e.g.
//! `CM_STORE_BENCH_DIR=<dir on a real disk> cargo test -p cm-daemon
//! replica_storage_bench -- --ignored --nocapture`.
use super::*;
use std::time::Instant;

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn bench_root() -> tempfile::TempDir {
    match std::env::var("CM_STORE_BENCH_DIR") {
        Ok(dir) => {
            fs::create_dir_all(&dir).unwrap();
            tempfile::tempdir_in(dir).unwrap()
        }
        Err(_) => tempfile::tempdir().unwrap(),
    }
}

/// Mimics transport.rs's `batch` frame handling for one bulk page.
fn page_checkpoint(replica: &mut Store, generation: &str, next: u64, scopes: &BTreeSet<String>) {
    let list: Vec<&str> = scopes
        .iter()
        .map(String::as_str)
        .chain(["metadata"])
        .collect();
    replica
        .record_coverage_batch(&list, generation, next, Some((next, 1, scopes)))
        .unwrap();
}

#[test]
#[ignore]
fn replica_storage_bench() {
    let channels = env("CM_BENCH_CHANNELS", 150);
    let history = env("CM_BENCH_HISTORY", 1500);
    let legacy_files = env("CM_BENCH_LEGACY_COVERAGE", 200_000);
    let tmp = bench_root();
    let mut hub = Store::open(&tmp.path().join("hub")).unwrap();
    let mut replica =
        Store::provision_replica(&tmp.path().join("replica"), &hub.space_descriptor()).unwrap();
    hub.register_host(&replica.daemon_id, true, true, &[])
        .unwrap();
    for i in 0..channels {
        hub.create_channel(
            "owner",
            &json!({"action":"create","path":format!("bench-{i}"),"request_id":format!("c{i}")}),
        )
        .unwrap();
    }
    let paths: Vec<String> = hub.channels.keys().cloned().collect();
    for i in 0..history {
        hub.send(
            "owner",
            "",
            "owner",
            &json!({"channel":paths[i % paths.len()],"body":format!("history {i}"),"request_id":format!("h{i}")}),
            &[],
        )
        .unwrap();
    }
    let scopes: BTreeSet<String> = hub.channels.values().cloned().collect();
    let generation = hub.generation.clone();
    // Catch up (bulk pages), checkpointing like the transport.
    let mut after = 0;
    let through = hub.position;
    let started = Instant::now();
    let mut pages = 0;
    loop {
        let page = hub
            .export_page(&replica.daemon_id, &BTreeSet::new(), after, through)
            .unwrap();
        replica
            .ingest_replica_page(page["items"].as_array().unwrap())
            .unwrap();
        after = page["cursor"].as_u64().unwrap();
        page_checkpoint(&mut replica, &generation, after, &scopes);
        pages += 1;
        if page["complete"] == true {
            break;
        }
    }
    eprintln!(
        "catch-up: {} events in {pages} pages: {:?}",
        replica.events.len(),
        started.elapsed()
    );
    // Steady state: one new hub publication per page.
    let mut samples = vec![];
    for i in 0..10 {
        hub.send(
            "owner",
            "",
            "owner",
            &json!({"channel":"general","body":format!("live {i}"),"request_id":format!("l{i}")}),
            &[],
        )
        .unwrap();
        let page = hub
            .export_page(&replica.daemon_id, &BTreeSet::new(), after, hub.position)
            .unwrap();
        let t = Instant::now();
        replica
            .ingest_replica_page(page["items"].as_array().unwrap())
            .unwrap();
        let ingest = t.elapsed();
        after = page["cursor"].as_u64().unwrap();
        page_checkpoint(&mut replica, &generation, after, &scopes);
        samples.push((t.elapsed(), ingest));
    }
    samples.sort();
    eprintln!(
        "per-page lock hold (1 event, {} scopes): median {:?} (ingest {:?}), max {:?}",
        scopes.len() + 1,
        samples[5].0,
        samples[5].1,
        samples[9].0
    );
    // Synthetic legacy journal: per-scope + transport coverage entries.
    let replica_root = replica.root.clone();
    let cm_root = replica_root
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut pos = replica.position;
    let journal = replica.journal_dir();
    let daemon_id = replica.daemon_id.clone();
    let local_generation = replica.generation.clone();
    let coordinator = replica.coordinator_id().to_owned();
    drop(replica);
    let t = Instant::now();
    let scope_list: Vec<&String> = scopes.iter().collect();
    let mut written = 0;
    let mut cursor = after;
    while written < legacy_files {
        cursor += 1;
        for (k, scope) in scope_list
            .iter()
            .map(|s| s.as_str())
            .chain(["metadata", "transport"])
            .enumerate()
        {
            if written >= legacy_files {
                break;
            }
            pos += 1;
            written += 1;
            let data = if scope == "transport" {
                json!({"scope":"transport","coordinator_id":coordinator,"generation":generation,"cursor":cursor,"revision":1,"scopes":scopes,"complete":false})
            } else {
                json!({"scope":scope,"coordinator_id":coordinator,"generation":generation,"through":cursor + k as u64 * 0,"complete":true})
            };
            let j = json!({"protocol":1,"replica_id":daemon_id,"generation":local_generation,"position":format!("{pos:020}"),
                "recorded_at":now(),"kind":"coverage","event_id":null,"event_sha256":null,"data":data});
            fs::write(
                journal.join(format!("{pos:020}.json")),
                serde_json::to_vec_pretty(&j).unwrap(),
            )
            .unwrap();
        }
    }
    eprintln!("wrote {written} legacy coverage files in {:?}", t.elapsed());
    for label in ["first open", "second open", "third open"] {
        let t = Instant::now();
        let mut s = Store::open(&cm_root).unwrap();
        assert!(s.degraded.is_none(), "{:?}", s.degraded);
        assert_eq!(s.position, pos);
        let elapsed = t.elapsed();
        let checkpoint = s.cached_coverage(scope_list[0]);
        assert_eq!(checkpoint["checkpoint"]["through"], cursor, "{checkpoint}");
        let t = Instant::now();
        s.wait_for_compaction();
        let compaction = t.elapsed();
        drop(s);
        let remaining = fs::read_dir(&journal).unwrap().count();
        eprintln!("{label}: {elapsed:?}; background compaction {compaction:?} (journal files after: {remaining})");
    }
}
