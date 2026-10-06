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

pub struct Delivery {
    seen: VecDeque<String>,
    path: PathBuf,
    acknowledgements: Vec<(HostId, Alert)>,
    retry_at: Instant,
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
        }
    }

    pub fn handle(&mut self, command: Command) {
        match command {
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

    fn present(&mut self, alert: &Alert, show: impl FnOnce(&Alert) -> Result<(), String>) {
        if self.seen.contains(&alert.id) {
            return;
        }
        if let Err(e) = show(alert) {
            eprintln!("cm-tui: Owner desktop notification failed (sidebar alert retained): {e}");
            return;
        }
        self.seen.push_back(alert.id.clone());
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
