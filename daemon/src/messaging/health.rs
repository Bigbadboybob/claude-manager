//! Bounded, host-local chat RPC telemetry and sustained degradation alarm.
//! The telemetry mutex is independent of the store and is never held while
//! waiting on it. In-flight waits are sampled by the ticker as well as on exit.
use crate::{
    owner_attention::{self, Escalation, GateOutcome},
    owner_availability::Urgency,
    state::DaemonState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Instant,
};

const BOUNDS: [u64; 14] = [
    1,
    10,
    100,
    500,
    1000,
    2000,
    5000,
    10000,
    15000,
    25000,
    30000,
    60000,
    120000,
    u64::MAX,
];
const KEY: &str = "system:messaging-health";
const MAX_METHODS: usize = 64;
const MAX_IN_FLIGHT: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub window_minutes: u64,
    pub evaluation_minutes: u64,
    pub bad_minutes: u64,
    pub wait_threshold_ms: u64,
    pub slow_total_ms: u64,
    pub recovery_minutes: u64,
    pub realert_minutes: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            window_minutes: 15,
            evaluation_minutes: 5,
            bad_minutes: 3,
            wait_threshold_ms: 10000,
            slow_total_ms: 10000,
            recovery_minutes: 5,
            realert_minutes: 30,
        }
    }
}
impl Config {
    // Bound allocations and avoid zero/invalid windows from disabling the alarm.
    fn normalized(&self) -> Self {
        let mut c = self.clone();
        c.window_minutes = c.window_minutes.clamp(1, 60);
        c.evaluation_minutes = c.evaluation_minutes.clamp(1, c.window_minutes);
        c.bad_minutes = c.bad_minutes.clamp(1, c.evaluation_minutes);
        c.recovery_minutes = c.recovery_minutes.clamp(1, 1440);
        c.realert_minutes = c.realert_minutes.clamp(1, 1440);
        c
    }
}
#[derive(Clone, Default)]
struct Stats {
    waits: [u64; 14],
    count: u64,
    max_wait: u64,
    total_ms: u64,
    max_total: u64,
    slow: u64,
    sheds: u64,
    errors: u64,
}
impl Stats {
    fn add(&mut self, wait: u64, total: u64, slow: bool, shed: bool, error: bool) {
        self.count += 1;
        self.waits[BOUNDS.iter().position(|&b| wait <= b).unwrap()] += 1;
        self.max_wait = self.max_wait.max(wait);
        self.total_ms = self.total_ms.saturating_add(total);
        self.max_total = self.max_total.max(total);
        self.slow += u64::from(slow);
        self.sheds += u64::from(shed);
        self.errors += u64::from(error);
    }
    fn merge(&mut self, other: &Self) {
        for (a, b) in self.waits.iter_mut().zip(other.waits) {
            *a += b;
        }
        self.count += other.count;
        self.max_wait = self.max_wait.max(other.max_wait);
        self.total_ms = self.total_ms.saturating_add(other.total_ms);
        self.max_total = self.max_total.max(other.max_total);
        self.slow += other.slow;
        self.sheds += other.sheds;
        self.errors += other.errors;
    }
    fn percentile(&self, pct: u64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = (self.count * pct).div_ceil(100);
        let mut n = 0;
        for (i, count) in self.waits.iter().enumerate() {
            n += count;
            if n >= target {
                return BOUNDS[i].min(self.max_wait);
            }
        }
        0
    }
    fn value(&self) -> Value {
        json!({"count":self.count,"p50_wait_ms":self.percentile(50),"p95_wait_ms":self.percentile(95),"max_wait_ms":self.max_wait,"total_ms":self.total_ms,"max_total_ms":self.max_total,"slow_count":self.slow,"sheds":self.sheds,"errors":self.errors})
    }
}
#[derive(Default)]
struct Bucket {
    minute: u64,
    stats: Stats,
    methods: BTreeMap<String, Stats>,
    pending_bad: bool,
    pending_max: u64,
}
struct Pending {
    started: Instant,
    done: AtomicBool,
}
#[derive(Default)]
struct Inner {
    buckets: VecDeque<Bucket>,
    pending: BTreeMap<u64, Arc<Pending>>,
    next_id: u64,
    degraded: bool,
    last_bad: Option<u64>,
    next_alert: Option<u64>,
    alert_id: Option<String>,
    initialized: bool,
}
pub struct Health {
    started: Instant,
    inner: Mutex<Inner>,
}
impl Default for Health {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            inner: Mutex::new(Inner::default()),
        }
    }
}
impl Health {
    fn seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn bucket(inner: &mut Inner, minute: u64) -> &mut Bucket {
        let index = match inner.buckets.iter().position(|b| b.minute >= minute) {
            Some(i) if inner.buckets[i].minute == minute => i,
            Some(i) => {
                inner.buckets.insert(
                    i,
                    Bucket {
                        minute,
                        ..Default::default()
                    },
                );
                i
            }
            None => {
                inner.buckets.push_back(Bucket {
                    minute,
                    ..Default::default()
                });
                inner.buckets.len() - 1
            }
        };
        // The ticker and record paths can straddle a minute boundary. Keep the
        // just-requested bucket even if it is the oldest during that race.
        let removed = if inner.buckets.len() > 60 && index > 0 {
            inner.buckets.pop_front();
            1
        } else {
            0
        };
        &mut inner.buckets[index - removed]
    }
    fn record(
        &self,
        method: &str,
        wait: u64,
        total: u64,
        shed: bool,
        error: bool,
        config: &Config,
        finished_waits: &[u64],
    ) {
        let mut inner = self.lock();
        for id in finished_waits {
            inner.pending.remove(id);
        }
        let b = Self::bucket(&mut inner, self.seconds() / 60);
        let slow = total > config.slow_total_ms;
        b.stats.add(wait, total, slow, shed, error);
        let key = if b.methods.contains_key(method) || b.methods.len() < MAX_METHODS {
            method.chars().take(100).collect()
        } else {
            "other".to_string()
        };
        b.methods
            .entry(key)
            .or_default()
            .add(wait, total, slow, shed, error);
    }
    fn snapshot_at(&self, seconds: u64, config: &Config) -> Value {
        let c = config.normalized();
        let inner = self.lock();
        let minute = seconds / 60;
        let mut stats = Stats::default();
        let mut methods = BTreeMap::<String, Stats>::new();
        for b in inner
            .buckets
            .iter()
            .filter(|b| b.minute <= minute && minute - b.minute < c.window_minutes)
        {
            stats.merge(&b.stats);
            for (name, s) in &b.methods {
                methods.entry(name.clone()).or_default().merge(s);
            }
        }
        let max_pending = inner
            .pending
            .values()
            .filter(|p| !p.done.load(Ordering::Relaxed))
            .map(|p| p.started.elapsed().as_millis().min(u64::MAX as u128) as u64)
            .max()
            .unwrap_or(0);
        let mut value = stats.value();
        value["state"] = json!(if inner.degraded { "degraded" } else { "ok" });
        value["window_minutes"] = json!(c.window_minutes);
        value["in_flight_waits"] = json!(inner
            .pending
            .values()
            .filter(|p| !p.done.load(Ordering::Relaxed))
            .count());
        value["max_in_flight_wait_ms"] = json!(max_pending);
        value["by_method"] = json!(methods
            .into_iter()
            .map(|(k, v)| (k, v.value()))
            .collect::<BTreeMap<_, _>>());
        value
    }
    pub fn snapshot(&self, config: &Config) -> Value {
        self.snapshot_at(self.seconds(), config)
    }
    fn tick(&self, seconds: u64, config: &Config) -> (Action, bool, Value) {
        let c = config.normalized();
        let mut inner = self.lock();
        inner.pending.retain(|_, p| !p.done.load(Ordering::Relaxed));
        let pending_max = inner
            .pending
            .values()
            .filter(|p| !p.done.load(Ordering::Relaxed))
            .map(|p| p.started.elapsed().as_millis().min(u64::MAX as u128) as u64)
            .max()
            .unwrap_or(0);
        let minute = seconds / 60;
        let current = Self::bucket(&mut inner, minute);
        current.pending_max = current.pending_max.max(pending_max);
        current.pending_bad |= pending_max > c.wait_threshold_ms;
        let current_bad = current.pending_bad
            || current.stats.sheds > 0
            || current.stats.percentile(95) > c.wait_threshold_ms;
        if current_bad {
            inner.last_bad = Some(seconds);
        }
        let recent: Vec<_> = inner
            .buckets
            .iter()
            .filter(|b| b.minute <= minute && minute - b.minute < c.evaluation_minutes)
            .collect();
        let bad = recent
            .iter()
            .filter(|b| {
                b.pending_bad || b.stats.sheds > 0 || b.stats.percentile(95) > c.wait_threshold_ms
            })
            .count() as u64;
        let mut stats = Stats::default();
        let mut max_pending = 0;
        for b in recent {
            stats.merge(&b.stats);
            max_pending = max_pending.max(b.pending_max);
        }
        let was_degraded = inner.degraded;
        let action = if !inner.degraded && bad >= c.bad_minutes {
            inner.degraded = true;
            Action::Alert
        } else if inner.degraded
            && inner
                .last_bad
                .is_some_and(|t| seconds.saturating_sub(t) >= c.recovery_minutes * 60)
        {
            inner.degraded = false;
            Action::Recover
        } else if inner.degraded && inner.next_alert.is_none_or(|t| seconds >= t) {
            Action::Alert
        } else if !inner.degraded && inner.alert_id.is_some() {
            Action::Recover
        } else {
            Action::None
        };
        let changed = was_degraded != inner.degraded;
        let summary = json!({"p95_wait_ms":stats.percentile(95),"max_wait_ms":stats.max_wait.max(max_pending),"sheds":stats.sheds,"bad_minutes":bad,"evaluation_minutes":c.evaluation_minutes});
        (action, changed, summary)
    }
}
#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Alert,
    Recover,
}
thread_local! { static CURRENT: RefCell<Option<Current>> = const { RefCell::new(None) }; }
struct Current {
    health: Arc<Health>,
    wait_ms: u64,
    pending_ids: Vec<u64>,
}
/// Instrument only store acquisitions on the RPC thread, including refresh.
pub fn store_lock<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    let started = Instant::now();
    let pending = CURRENT.with(|c| {
        c.borrow_mut().as_mut().and_then(|c| {
            let mut i = c.health.lock();
            if i.pending.len() >= MAX_IN_FLIGHT {
                return None;
            }
            i.next_id = i.next_id.wrapping_add(1);
            let id = i.next_id;
            let wait = Arc::new(Pending {
                started,
                done: AtomicBool::new(false),
            });
            i.pending.insert(id, wait.clone());
            c.pending_ids.push(id);
            Some(wait)
        })
    });
    let guard = lock.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(pending) = pending {
        pending.done.store(true, Ordering::Relaxed);
    }
    CURRENT.with(|c| {
        if let Some(c) = c.borrow_mut().as_mut() {
            c.wait_ms = c
                .wait_ms
                .saturating_add(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        }
    });
    guard
}
pub fn measure<F, T>(
    health: Arc<Health>,
    config: Config,
    method: &str,
    start: Instant,
    f: F,
) -> Result<T, super::ChatError>
where
    F: FnOnce() -> Result<T, super::ChatError>,
{
    let previous = CURRENT.with(|c| {
        c.borrow_mut().replace(Current {
            health: health.clone(),
            wait_ms: 0,
            pending_ids: Vec::new(),
        })
    });
    let result = f();
    let current = CURRENT
        .with(|c| std::mem::replace(&mut *c.borrow_mut(), previous))
        .unwrap();
    health.record(
        method,
        current.wait_ms,
        start.elapsed().as_millis().min(u64::MAX as u128) as u64,
        result.as_ref().err().is_some_and(|e| e.code == "busy"),
        result.is_err(),
        &config,
        &current.pending_ids,
    );
    result
}
/// Durable episode metadata preserves dedupe and cooldown across brain restarts.
#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    degraded: bool,
    alert_id: Option<String>,
    next_alert_unix: u64,
}
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn save_episode(root: &std::path::Path, h: &Health, seconds: u64) {
    let inner = h.lock();
    let data = Persisted {
        degraded: inner.degraded,
        alert_id: inner.alert_id.clone(),
        next_alert_unix: unix_now()
            .saturating_add(inner.next_alert.unwrap_or(seconds).saturating_sub(seconds)),
    };
    drop(inner);
    if let Err(e) = super::atomic_replace(&root.join("messaging-health-alarm.json"), &json!(data)) {
        eprintln!("cm-daemon: messaging health episode persistence failed: {e}");
    }
}
/// Run outside the store lock; this can notify even while chat is wedged.
pub fn alarm_pass(state: &Arc<Mutex<DaemonState>>) {
    let (health, config, root) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (
            s.messaging_health.clone(),
            s.config.messaging_health.clone(),
            s.messaging_root.clone(),
        )
    };
    let seconds = health.seconds();
    if !health.lock().initialized {
        let saved = std::fs::read(root.join("messaging-health-alarm.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Persisted>(&b).ok())
            .unwrap_or_default();
        let mut inner = health.lock();
        inner.initialized = true;
        inner.degraded = saved.degraded;
        inner.alert_id = saved.alert_id;
        if saved.degraded {
            inner.last_bad = Some(seconds);
        }
        inner.next_alert =
            Some(seconds.saturating_add(saved.next_alert_unix.saturating_sub(unix_now())));
    }
    let (action, changed, stats) = health.tick(seconds, &config);
    if action == Action::None {
        return;
    }
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_else(|_| "this host".into())
        .trim()
        .to_string();
    let load = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| "unavailable".into());
    let s = state.lock().unwrap_or_else(|p| p.into_inner());
    match action {
        Action::Alert => {
            let msg = format!("{host}: CM chat degraded over {} minutes: p95 wait {} ms, max wait {} ms, {} sheds; load averages (1/5/15 min) {load}.", stats["evaluation_minutes"], stats["p95_wait_ms"], stats["max_wait_ms"], stats["sheds"]);
            let e = Escalation {
                source: "messaging-health".into(),
                dedupe_key: KEY.into(),
                urgency: Urgency::Blocking,
                summary: msg,
                session_uid: None,
                task_id: None,
            };
            match owner_attention::escalate(&s, e) {
                Ok(GateOutcome::Delivered { alert_id } | GateOutcome::Coalesced { alert_id }) => {
                    let mut inner = health.lock();
                    inner.alert_id = Some(alert_id);
                    inner.next_alert =
                        Some(seconds.saturating_add(config.normalized().realert_minutes * 60));
                }
                Err(e) => eprintln!("cm-daemon: messaging health alert failed: {e}"),
            }
            if changed {
                eprintln!(
                    "cm-daemon: messaging_health {}",
                    json!({"state":"degraded", "host":host, "metrics":stats, "load_average":load})
                );
            }
        }
        Action::Recover => {
            let id = health.lock().alert_id.clone();
            if let Some(id) = id {
                if let Err(e) = owner_attention::withdraw(&s, KEY, &id) {
                    eprintln!("cm-daemon: messaging health withdrawal failed: {e}");
                } else {
                    health.lock().alert_id = None;
                }
            }
            health.lock().next_alert = None;
            if changed {
                eprintln!(
                    "cm-daemon: messaging_health {}",
                    json!({"state":"ok", "host":host, "metrics":stats})
                );
            }
        }
        Action::None => {}
    }
    drop(s);
    save_episode(&root, &health, seconds);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample(h: &Health, minute: u64, wait: u64, shed: bool) {
        let mut inner = h.lock();
        Health::bucket(&mut inner, minute)
            .stats
            .add(wait, wait + 7, wait > 10_000, shed, shed);
    }

    #[test]
    fn window_percentiles_counts_and_expiry() {
        let h = Health::default();
        let c = Config::default();
        sample(&h, 0, 10, false);
        sample(&h, 1, 12_000, true);
        let view = h.snapshot_at(60, &c);
        assert_eq!(view["count"], 2);
        assert_eq!(view["p50_wait_ms"], 10);
        assert_eq!(view["p95_wait_ms"], 12_000);
        assert_eq!(view["max_wait_ms"], 12_000);
        assert_eq!(view["sheds"], 1);
        assert_eq!(view["errors"], 1);
        assert_eq!(view["slow_count"], 1);
        assert_eq!(h.snapshot_at(15 * 60, &c)["count"], 1);
        assert_eq!(h.snapshot_at(16 * 60, &c)["count"], 0);
    }

    #[test]
    fn sustained_threshold_dedupe_and_recovery() {
        let h = Health::default();
        let c = Config::default();
        sample(&h, 0, 10_001, false);
        assert_eq!(h.tick(0, &c).0, Action::None);
        sample(&h, 1, 0, true);
        assert_eq!(h.tick(60, &c).0, Action::None);
        sample(&h, 2, 20_000, false);
        assert_eq!(h.tick(120, &c).0, Action::Alert);
        h.lock().next_alert = Some(120 + 30 * 60);
        assert_eq!(h.tick(180, &c).0, Action::None);
        assert_eq!(h.tick(419, &c).0, Action::None);
        assert_eq!(h.tick(420, &c).0, Action::Recover);
        assert_eq!(h.snapshot_at(420, &c)["state"], "ok");
    }

    #[test]
    fn sustained_wait_realerts_only_after_cooldown() {
        let h = Health::default();
        let c = Config::default();
        let pending = Arc::new(Pending {
            started: Instant::now() - Duration::from_secs(20),
            done: AtomicBool::new(false),
        });
        h.lock().pending.insert(1, pending);
        assert_eq!(h.tick(0, &c).0, Action::None);
        assert_eq!(h.tick(60, &c).0, Action::None);
        let (action, _, view) = h.tick(120, &c);
        assert_eq!(action, Action::Alert);
        assert!(view["max_wait_ms"].as_u64().unwrap() >= 20_000);
        h.lock().next_alert = Some(1920);
        assert_eq!(h.tick(1919, &c).0, Action::None);
        assert_eq!(h.tick(1920, &c).0, Action::Alert);
    }

    #[test]
    fn measurement_records_completed_methods_and_errors() {
        let health = Arc::new(Health::default());
        let lock = Mutex::new(());
        let result: Result<(), super::super::ChatError> = measure(
            health.clone(),
            Config::default(),
            "messaging.read",
            Instant::now(),
            || {
                drop(store_lock(&lock));
                Err(super::super::ChatError {
                    code: "busy".into(),
                    message: "test".into(),
                })
            },
        );
        assert!(result.is_err());
        let v = health.snapshot(&Config::default());
        assert_eq!(v["by_method"]["messaging.read"]["count"], 1);
        assert_eq!(v["sheds"], 1);
        assert_eq!(v["errors"], 1);
        assert_eq!(v["in_flight_waits"], 0);
    }

    #[test]
    fn real_blocked_store_wait_is_visible_before_rpc_finishes() {
        let health = Arc::new(Health::default());
        let store = Arc::new(Mutex::new(()));
        let held = store.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_health = health.clone();
        let worker_store = store.clone();
        let worker = std::thread::spawn(move || {
            tx.send(()).unwrap();
            measure(
                worker_health,
                Config::default(),
                "messaging.open",
                Instant::now(),
                || {
                    drop(store_lock(&worker_store));
                    Ok(())
                },
            )
            .unwrap();
        });
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while health.snapshot(&Config::default())["in_flight_waits"] != 1 {
            assert!(Instant::now() < deadline, "wait was not registered");
            std::thread::yield_now();
        }
        assert_eq!(health.snapshot(&Config::default())["count"], 0);
        drop(held);
        worker.join().unwrap();
        let view = health.snapshot(&Config::default());
        assert_eq!(view["in_flight_waits"], 0);
        assert_eq!(view["by_method"]["messaging.open"]["count"], 1);
    }

    #[test]
    fn alarm_delivers_blocking_attention_and_persists_episode() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = DaemonState::new();
        state.daemon_sessions_path = Some(tmp.path().join("daemon-sessions.json"));
        state.messaging_root = tmp.path().to_path_buf();
        let h = Arc::new(Health {
            started: Instant::now() - Duration::from_secs(130),
            inner: Mutex::new(Inner {
                initialized: true,
                ..Inner::default()
            }),
        });
        for minute in 0..3 {
            sample(&h, minute, 20_000, false);
        }
        state.messaging_health = h;
        let state = Arc::new(Mutex::new(state));
        alarm_pass(&state);
        let alerts = owner_attention::snapshot(&state.lock().unwrap()).unwrap();
        let alert = &alerts[KEY];
        assert_eq!(alert.urgency.as_deref(), Some("blocking"));
        assert!(alert.message.contains("p95 wait"));
        assert!(alert.message.contains("load averages"));
        let saved: Persisted = serde_json::from_slice(
            &std::fs::read(tmp.path().join("messaging-health-alarm.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.alert_id.as_deref(), Some(alert.id.as_str()));
        assert!(saved.degraded);
        alarm_pass(&state);
        assert_eq!(
            owner_attention::snapshot(&state.lock().unwrap()).unwrap()[KEY].id,
            alert.id
        );

        // A new brain adopts the episode and its cooldown, then withdraws
        // the same alert after a full healthy recovery interval.
        let restarted = Arc::new(Health {
            started: Instant::now() - Duration::from_secs(301),
            inner: Mutex::new(Inner::default()),
        });
        state.lock().unwrap().messaging_health = restarted.clone();
        alarm_pass(&state);
        assert!(restarted.lock().degraded);
        assert_eq!(
            restarted.lock().alert_id.as_deref(),
            Some(alert.id.as_str())
        );
        assert!(restarted.lock().next_alert.unwrap() > restarted.seconds());
        restarted.lock().last_bad = Some(0);
        alarm_pass(&state);
        assert!(!owner_attention::snapshot(&state.lock().unwrap())
            .unwrap()
            .contains_key(KEY));
        let saved: Persisted = serde_json::from_slice(
            &std::fs::read(tmp.path().join("messaging-health-alarm.json")).unwrap(),
        )
        .unwrap();
        assert!(!saved.degraded);
        assert!(saved.alert_id.is_none());
    }
}
