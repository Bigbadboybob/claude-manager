//! Background desktop delivery and retryable, compare-ID Owner acknowledgements.
use crate::{host_pool::HostPool, hosts::HostId};
use cm_daemon::owner_attention::Alert;
use std::{
    collections::VecDeque,
    path::PathBuf,
    time::{Duration, Instant},
};

pub enum Command {
    Present(Alert),
    Acknowledge(HostId, Alert),
}

#[cfg(test)]
mod tests {
    use super::*;
    fn alert(id: &str) -> Alert {
        Alert {
            id: id.into(),
            session_uid: "self".into(),
            label: "Task".into(),
            message: "Review ready".into(),
            task_id: None,
            continuous_task_id: None,
            ..Default::default()
        }
    }
    #[test]
    fn owner_notification_deduplicates_reconnect_and_tui_restart_but_new_alert_delivers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.json");
        let mut worker = Delivery::new(path.clone());
        let mut deliveries = 0;
        worker.present(&alert("1"), |_| {
            deliveries += 1;
            Ok(())
        });
        worker.present(&alert("1"), |_| {
            deliveries += 1;
            Ok(())
        });
        let mut worker = Delivery::new(path);
        worker.present(&alert("1"), |_| {
            deliveries += 1;
            Ok(())
        });
        worker.present(&alert("2"), |_| {
            deliveries += 1;
            Ok(())
        });
        assert_eq!(deliveries, 2);
    }
    #[test]
    fn owner_notification_failure_is_not_recorded_as_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.json");
        let mut worker = Delivery::new(path.clone());
        worker.present(&alert("1"), |_| Err("desktop unavailable".into()));
        assert!(!path.exists());
        worker.present(&alert("1"), |_| Ok(()));
        assert!(path.exists());
    }

    #[test]
    fn released_requests_arriving_together_become_one_digest() {
        let dir = tempfile::tempdir().unwrap();
        let mut worker = Delivery::new(dir.path().join("receipts.json"));
        let released = |id: &str, label: &str| Alert {
            label: label.into(),
            released_at: Some("2026-10-06T21:00:00Z".into()),
            ..alert(id)
        };
        worker.handle(Command::Present(released("r1", "Lane-A")));
        worker.handle(Command::Present(released("r2", "Lane-B")));
        worker.handle(Command::Present(released("r2", "Lane-B")));
        let start = Instant::now();
        let mut shown = Vec::new();
        worker.flush_released(start, |a| { shown.push(a.message.clone()); Ok(()) });
        assert!(shown.is_empty(), "inside the digest window");
        worker.flush_released(start + RELEASE_DIGEST_WINDOW, |a| { shown.push(a.message.clone()); Ok(()) });
        assert_eq!(shown, vec!["2 held requests released: Lane-A, Lane-B".to_string()]);
        // Both are recorded: a reconnect replay shows nothing new.
        worker.handle(Command::Present(released("r1", "Lane-A")));
        assert!(worker.released.is_empty());
        // A single release shows as itself; ordinary alerts are never delayed.
        worker.handle(Command::Present(released("r3", "Lane-C")));
        let mut single = Vec::new();
        worker.flush_released(Instant::now() + RELEASE_DIGEST_WINDOW, |a| { single.push(a.message.clone()); Ok(()) });
        assert_eq!(single, vec!["Review ready".to_string()]);
        let mut direct = 0;
        worker.present(&alert("plain"), |_| { direct += 1; Ok(()) });
        assert_eq!(direct, 1);
    }
    #[test]
    #[ignore = "requires scripts/test-owner-desktop.py private notification bus"]
    fn owner_notification_desktop_transport() {
        assert_eq!(std::env::var("CM_TEST_OWNER_DESKTOP").as_deref(), Ok("1"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.json");
        let mut delivery = Delivery::new(path.clone());
        delivery.handle(Command::Present(alert("desktop-smoke")));
        assert!(
            path.exists(),
            "desktop service must accept before a receipt is recorded"
        );
        let mut reconnected = Delivery::new(path);
        reconnected.handle(Command::Present(alert("desktop-smoke")));
    }
}

/// Released held requests arriving this close together (one availability
/// change releases them all at once) become one desktop popup.
const RELEASE_DIGEST_WINDOW: Duration = Duration::from_secs(3);

pub struct Delivery {
    seen: VecDeque<String>,
    path: PathBuf,
    acknowledgements: Vec<(HostId, Alert)>,
    retry_at: Instant,
    released: Vec<(Alert, Instant)>,
}

impl Delivery {
    pub fn new(path: PathBuf) -> Self {
        let seen = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                eprintln!("cm-tui: cannot read desktop notification receipts: {e}");
                VecDeque::new()
            }),
            Err(_) => VecDeque::new(),
        };
        Self {
            seen,
            path,
            acknowledgements: Vec::new(),
            retry_at: Instant::now(),
            released: Vec::new(),
        }
    }

    pub fn handle(&mut self, command: Command) {
        match command {
            Command::Present(alert) if alert.released_at.is_some() => {
                if !self.seen.contains(&alert.id) && !self.released.iter().any(|(a, _)| a.id == alert.id) {
                    self.released.push((alert, Instant::now()));
                }
            }
            Command::Present(alert) => self.present(&alert, |a| {
                crate::app::show_user_alert(&a.label, &a.message)
            }),
            Command::Acknowledge(host, alert) => {
                if !self
                    .acknowledgements
                    .iter()
                    .any(|(h, a)| h == &host && a.id == alert.id)
                {
                    self.acknowledgements.push((host, alert));
                }
            }
        }
    }

    /// Show buffered released requests once the digest window has passed:
    /// a single one as itself, several as one "N held requests released"
    /// popup. Failures keep them buffered for the next flush.
    fn flush_released(&mut self, now: Instant, show: impl FnOnce(&Alert) -> Result<(), String>) {
        let Some(oldest) = self.released.iter().map(|(_, t)| *t).min() else {
            return;
        };
        if now.duration_since(oldest) < RELEASE_DIGEST_WINDOW {
            return;
        }
        let batch: Vec<Alert> = self.released.drain(..).map(|(a, _)| a).collect();
        if batch.len() == 1 {
            self.present(&batch[0], show);
            return;
        }
        let mut labels: Vec<&str> = batch.iter().map(|a| a.label.as_str()).collect();
        labels.dedup();
        let digest = Alert {
            id: format!("release-digest:{}", batch[0].id),
            label: "Claude Manager".into(),
            message: format!(
                "{} held requests released: {}",
                batch.len(),
                labels.iter().take(5).copied().collect::<Vec<_>>().join(", ")
            ),
            ..Alert::default()
        };
        if let Err(e) = show(&digest) {
            eprintln!("cm-tui: Owner release digest failed (sidebar alerts retained): {e}");
            let now = Instant::now();
            self.released = batch.into_iter().map(|a| (a, now)).collect();
            return;
        }
        let ids: Vec<String> = batch.into_iter().map(|a| a.id).collect();
        self.record_seen(&ids);
    }

    fn present(&mut self, alert: &Alert, show: impl FnOnce(&Alert) -> Result<(), String>) {
        if self.seen.contains(&alert.id) {
            return;
        }
        if let Err(e) = show(alert) {
            eprintln!("cm-tui: Owner desktop notification failed (sidebar alert retained): {e}");
            return;
        }
        self.record_seen(std::slice::from_ref(&alert.id));
    }

    fn record_seen(&mut self, ids: &[String]) {
        self.seen.extend(ids.iter().cloned());
        while self.seen.len() > 4096 {
            self.seen.pop_front();
        }
        let result = (|| -> std::io::Result<()> {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = self
                .path
                .with_extension(format!("tmp.{}", std::process::id()));
            std::fs::write(&tmp, serde_json::to_vec(&self.seen)?)?;
            std::fs::rename(tmp, &self.path)
        })();
        if let Err(e) = result {
            eprintln!("cm-tui: persist desktop notification receipts: {e}");
        }
    }

    pub fn flush(&mut self, pool: &HostPool) {
        self.flush_released(Instant::now(), |a| crate::app::show_user_alert(&a.label, &a.message));
        if Instant::now() < self.retry_at || self.acknowledgements.is_empty() {
            return;
        }
        self.retry_at = Instant::now() + Duration::from_secs(5);
        self.acknowledgements.retain(|(host, alert)| {
            let result = (|| -> anyhow::Result<()> {
                let socket = pool
                    .for_host(host)?
                    .socket_path()
                    .ok_or_else(|| anyhow::anyhow!("no socket for Owner alert acknowledgement"))?;
                crate::client_session::rpc_messaging(
                    &socket,
                    &pool.operator_token_for(host),
                    "owner_attention.ack",
                    serde_json::json!({"session_uid": alert.session_uid, "alert_id": alert.id}),
                )?;
                Ok(())
            })();
            if let Err(e) = &result {
                eprintln!("cm-tui: retrying Owner alert acknowledgement on {host}: {e}");
            }
            result.is_err()
        });
    }
}
