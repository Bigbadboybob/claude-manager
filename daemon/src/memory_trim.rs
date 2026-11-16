//! One allocator maintenance thread. No CM lock is held across allocator calls.
//! The C calls cannot be cancelled and have no hard latency bound. A single
//! shared permit excludes periodic/manual maintenance and memory_snapshot.
use crate::{
    config::MemoryConfig,
    control::{
        operator,
        protocol::{ErrorCode, Request, Response},
    },
    memory_snapshot::{self, Admission, Allocator, Section, IN_FLIGHT},
};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    sync::{atomic::AtomicBool, Arc, Condvar, Mutex, OnceLock},
    time::{Duration, Instant},
};

static SERVICE: OnceLock<Arc<Service<'static>>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Identity {
    brain_pid: u32,
    process_start_ticks: Option<u64>,
    holder_epoch: Option<u64>,
    build_id: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Params {
    request_id: String,
    brain_pid: u32,
    process_start_ticks: u64,
    holder_epoch: Option<u64>,
}

#[derive(Clone, Serialize)]
struct Receipt {
    request: Params,
    status: &'static str,
    reason: Option<&'static str>,
    result: Option<RunResult>,
}

struct Work<'a> {
    _admission: Admission<'a>,
    source: &'static str,
    min_free_bytes: Option<u64>,
    schedule_version: u64,
}

struct Control<'a> {
    config: MemoryConfig,
    due: Option<Instant>,
    receipt: Option<Receipt>,
    pending: Option<Work<'a>>,
    worker_available: bool,
    schedule_version: u64,
}

struct Service<'a> {
    identity: Identity,
    gate: &'a AtomicBool,
    control: Mutex<Control<'a>>,
    wake: Condvar,
}

fn next_due(config: &MemoryConfig, now: Instant) -> Option<Instant> {
    (config.trim_interval_secs != 0)
        .then(|| now.checked_add(Duration::from_secs(config.trim_interval_secs.into())))
        .flatten()
}

impl<'a> Service<'a> {
    fn new(config: MemoryConfig, identity: Identity, gate: &'a AtomicBool, now: Instant) -> Self {
        Self {
            identity,
            gate,
            control: Mutex::new(Control {
                due: next_due(&config, now),
                config,
                receipt: None,
                pending: None,
                worker_available: true,
                schedule_version: 0,
            }),
            wake: Condvar::new(),
        }
    }

    fn reload(&self, config: MemoryConfig, now: Instant) {
        let mut c = self.control.lock().unwrap_or_else(|p| p.into_inner());
        if c.config != config {
            if c.config.trim_interval_secs != config.trim_interval_secs {
                c.due = next_due(&config, now);
                c.schedule_version = c.schedule_version.wrapping_add(1);
            }
            c.config = config;
            self.wake.notify_one();
        }
    }

    fn request(&self, params: Params) -> Result<serde_json::Value, (ErrorCode, &'static str)> {
        // Monolith execveat can preserve PID/start and reset generation after
        // terminal fallback, while discarding the receipt. Fail closed before
        // any lock/admission: this tuple cannot fence a different process image.
        if self.identity.holder_epoch.is_none() {
            return Ok(
                serde_json::json!({"status":"unavailable", "reason":"process_image_binding_unavailable", "accepted":false}),
            );
        }
        if self.identity.process_start_ticks.is_none() {
            return Ok(
                serde_json::json!({"status":"unavailable", "reason":"process_identity_unavailable"}),
            );
        }
        if params.brain_pid != self.identity.brain_pid
            || Some(params.process_start_ticks) != self.identity.process_start_ticks
            || params.holder_epoch != self.identity.holder_epoch
        {
            return Err((
                ErrorCode::Conflict,
                "brain PID/start/epoch does not match; do not retry against a replacement brain",
            ));
        }
        let receipt = {
            let mut c = self.control.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(receipt) = &c.receipt {
                if receipt.request != params {
                    return Err((ErrorCode::Conflict, "one explicit trim attempt per brain epoch; retry the original request exactly"));
                }
                receipt.clone()
            } else {
                if !c.worker_available {
                    return Ok(
                        serde_json::json!({"status":"unavailable", "reason":"trim_worker_unavailable"}),
                    );
                }
                let Some(admission) = Admission::enter(self.gate) else {
                    return Ok(
                        serde_json::json!({"status":"busy", "reason":"allocator_in_flight", "accepted":false}),
                    );
                };
                let receipt = Receipt {
                    request: params,
                    status: "pending",
                    reason: None,
                    result: None,
                };
                c.receipt = Some(receipt.clone());
                c.pending = Some(Work {
                    _admission: admission,
                    source: "operator",
                    min_free_bytes: None,
                    schedule_version: c.schedule_version,
                });
                self.wake.notify_one();
                receipt
            }
        }; // Bounded receipt clone only; serialization happens without locks.
        Ok(
            serde_json::json!({"version":1, "identity":self.identity, "accepted":true, "attempt":receipt}),
        )
    }

    /// Caller holds the control lock: config reload and admission have a single
    /// ordering point. Wakes are hints; always recheck the authoritative due time.
    fn take_work(&self, c: &mut Control<'a>, now: Instant) -> Option<Work<'a>> {
        if let Some(work) = c.pending.take() {
            c.receipt.as_mut().expect("pending receipt").status = "running";
            return Some(work);
        }
        if !c.worker_available || !c.due.is_some_and(|due| now >= due) {
            return None;
        }
        // Busy checks skip this slot rather than queue/catch up. Successful
        // work resets from completion below, so long calls never cause bursts.
        c.due = next_due(&c.config, now);
        let admission = Admission::enter(self.gate)?;
        Some(Work {
            _admission: admission,
            source: "periodic",
            min_free_bytes: Some(c.config.trim_min_free_bytes),
            schedule_version: c.schedule_version,
        })
    }

    fn finish(&self, work: Work<'a>, result: &Result<RunResult, ()>, now: Instant) {
        let mut c = self.control.lock().unwrap_or_else(|p| p.into_inner());
        if work.source == "operator" {
            let receipt = c.receipt.as_mut().expect("admitted operator receipt");
            match result {
                Ok(result) => {
                    receipt.status = "complete";
                    receipt.result = Some(result.clone());
                }
                Err(()) => {
                    receipt.status = "unknown";
                    receipt.reason = Some("worker_panicked_result_unknown");
                }
            }
        }
        if c.schedule_version == work.schedule_version || c.due.is_some_and(|due| due <= now) {
            c.due = next_due(&c.config, now);
        }
        // Release only after recording completion/unknown. No handler or
        // disconnect can release a permit belonging to the dedicated worker.
        drop(work);
    }
}

pub(crate) fn start(config: MemoryConfig) {
    SERVICE.get_or_init(|| {
        let service = Arc::new(Service::new(
            config,
            Identity {
                brain_pid: std::process::id(),
                process_start_ticks: memory_snapshot::process_start_ticks(),
                holder_epoch: crate::holder_mode::global().map(|c| c.epoch),
                build_id: crate::build_id(),
            },
            &IN_FLIGHT,
            Instant::now(),
        ));
        let worker = service.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("cm-memory-trim".into())
            .spawn(move || worker_loop(&worker))
        {
            service
                .control
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .worker_available = false;
            eprintln!("cm-daemon: memory trim worker unavailable: {error}");
        }
        service
    });
}

pub(crate) fn reload(config: MemoryConfig) {
    if let Some(service) = SERVICE.get() {
        service.reload(config, Instant::now());
    }
}

fn worker_loop(service: &Service<'_>) {
    loop {
        let work = {
            let mut c = service.control.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if let Some(work) = service.take_work(&mut c, Instant::now()) {
                    break work;
                }
                c = match c.due {
                    Some(due) => {
                        service
                            .wake
                            .wait_timeout(c, due.saturating_duration_since(Instant::now()))
                            .unwrap_or_else(|p| p.into_inner())
                            .0
                    }
                    None => service.wake.wait(c).unwrap_or_else(|p| p.into_inner()),
                };
            }
        };
        // No daemon/Store/control lock is held across metrics, trim, or logs.
        // Panic preserves an explicit unknown receipt; it never retries work.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(&service.identity, &work, metrics, trim)
        }))
        .map_err(|_| ());
        service.finish(work, &result, Instant::now());
        // Receipt is published before logging. Logging is fallible and cannot
        // turn a completed attempt into a duplicate after an uncertain reply.
        let log = match result {
            Ok(result) => serde_json::to_string(&result),
            Err(()) => serde_json::to_string(
                &serde_json::json!({"identity":service.identity, "status":"unknown", "reason":"worker_panicked_result_unknown"}),
            ),
        };
        if let Ok(log) = log {
            use std::io::Write;
            let _ = writeln!(std::io::stderr(), "cm-daemon: memory_trim {log}");
        }
    }
}

pub(crate) fn dispatch(req: &Request) -> Response {
    dispatch_with(
        req,
        SERVICE.get().map(Arc::as_ref),
        cfg!(target_env = "gnu"),
    )
}

fn dispatch_with(req: &Request, service: Option<&Service<'_>>, supported: bool) -> Response {
    if let Err((code, _)) = operator::require_strong_operator(&req.caller) {
        return Response::err(
            req.id.clone(),
            code,
            "daemon.memory_trim requires a configured strong operator token",
        );
    }
    let params = serde_json::from_value::<Params>(req.params.clone());
    let params = match params {
        Ok(p) if !p.request_id.is_empty() && p.request_id.len() <= 128
            && p.request_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_:.".contains(&b))
            && req.params.get("holder_epoch").is_some() => p,
        _ => return Response::err(req.id.clone(), ErrorCode::InvalidParams,
            "expected request_id (1..128 ASCII letters/digits/-_:.), brain_pid, process_start_ticks, holder_epoch (number or null)"),
    };
    if !supported {
        return Response::ok(
            req.id.clone(),
            serde_json::json!({"status":"unsupported", "reason":"malloc_trim_requires_glibc", "accepted":false}),
        );
    }
    let result = match service {
        Some(service) => service.request(params),
        None => Ok(serde_json::json!({"status":"unavailable", "reason":"trim_worker_not_started"})),
    };
    match result {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, reason)) => Response::err(req.id.clone(), code, reason),
    }
}

#[derive(Clone, Serialize)]
struct ProcessMemory {
    units: &'static str,
    rss_bytes: Option<u64>,
    swap_bytes: Option<u64>,
}

#[derive(Clone, Serialize)]
struct Metrics {
    allocator: Section<Allocator>,
    process: Section<ProcessMemory>,
}

#[derive(Clone, Serialize)]
struct RunResult {
    identity: Identity,
    source: &'static str,
    status: &'static str,
    reason: Option<&'static str>,
    started_at: String,
    finished_at: String,
    elapsed_us: u64,
    threshold_decision: &'static str,
    min_free_bytes: Option<u64>,
    malloc_trim_return: Option<i32>,
    trim_elapsed_us: Option<u64>,
    before: Metrics,
    after: Option<Metrics>,
}

fn run(
    identity: &Identity,
    work: &Work<'_>,
    mut read: impl FnMut() -> Metrics,
    trim: impl FnOnce() -> Option<i32>,
) -> RunResult {
    let began = Instant::now();
    let mut out = RunResult {
        identity: identity.clone(),
        source: work.source,
        status: "skipped",
        reason: None,
        started_at: crate::messaging::now(),
        finished_at: String::new(),
        elapsed_us: 0,
        threshold_decision: "explicit_operator_bypass",
        min_free_bytes: work.min_free_bytes,
        malloc_trim_return: None,
        trim_elapsed_us: None,
        before: read(),
        after: None,
    };
    let should_trim = if out.before.allocator.status == "unsupported" {
        out.status = "unsupported";
        out.reason = Some("malloc_trim_requires_glibc");
        out.threshold_decision = "unavailable";
        false
    } else if let Some(min) = work.min_free_bytes {
        match out.before.allocator.data.as_ref() {
            Some(allocator) if allocator.free_arena_bytes as u64 >= min => {
                out.threshold_decision = "at_or_above_threshold";
                true
            }
            Some(_) => {
                out.threshold_decision = "below_threshold";
                false
            }
            None => {
                out.threshold_decision = "unavailable";
                out.reason = Some("free_bytes_unavailable");
                false
            }
        }
    } else {
        true
    };
    if should_trim {
        let trim_start = Instant::now();
        out.malloc_trim_return = trim();
        out.trim_elapsed_us = Some(trim_start.elapsed().as_micros() as u64);
        out.status = if out.malloc_trim_return.is_some() {
            "complete"
        } else {
            "unsupported"
        };
        if out.malloc_trim_return.is_none() {
            out.reason = Some("malloc_trim_requires_glibc");
        }
        out.after = Some(read());
    }
    out.finished_at = crate::messaging::now();
    out.elapsed_us = began.elapsed().as_micros() as u64;
    out
}

fn trim() -> Option<i32> {
    #[cfg(target_env = "gnu")]
    {
        Some(unsafe { libc::malloc_trim(0) })
    }
    #[cfg(not(target_env = "gnu"))]
    {
        None
    }
}

fn metrics() -> Metrics {
    let began = Instant::now();
    let mut allocator = memory_snapshot::allocator();
    allocator.elapsed_us = began.elapsed().as_micros() as u64;
    let began = Instant::now();
    let mut process = process_memory();
    process.elapsed_us = began.elapsed().as_micros() as u64;
    Metrics { allocator, process }
}

fn process_memory() -> Section<ProcessMemory> {
    let mut text = String::new();
    let read = std::fs::File::open("/proc/self/status")
        .and_then(|f| f.take(65537).read_to_string(&mut text));
    if read.is_err() {
        return Section::unavailable("proc_status_unreadable");
    }
    if text.len() > 65536 {
        return Section::unavailable("proc_status_too_large");
    }
    parse_process_memory(&text)
}

fn parse_process_memory(text: &str) -> Section<ProcessMemory> {
    let bytes = |key: &str| -> Option<u64> {
        let mut parts = text
            .lines()
            .find_map(|line| line.strip_prefix(key))?
            .split_whitespace();
        let kib = parts.next()?.parse::<u64>().ok()?;
        if parts.next()? != "kB" || parts.next().is_some() {
            return None;
        }
        kib.checked_mul(1024)
    };
    let data = ProcessMemory {
        units: "bytes",
        rss_bytes: bytes("VmRSS:"),
        swap_bytes: bytes("VmSwap:"),
    };
    let complete = data.rss_bytes.is_some() && data.swap_bytes.is_some();
    Section {
        status: if complete { "ok" } else { "partial" },
        reason: (!complete).then_some("proc_status_fields_unavailable"),
        elapsed_us: 0,
        data: Some(data),
    }
}

#[cfg(test)]
mod tests;
