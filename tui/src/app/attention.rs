//! Cloud/local Owner alerts arrive over the existing manifest stream.
use super::*;
use crate::hosts::HostId;
use cm_daemon::owner_attention::Alert;

impl App {
    pub(crate) fn apply_owner_attention_snapshot(
        &mut self,
        host: &HostId,
        alerts: &std::collections::BTreeMap<String, Alert>,
    ) {
        self.owner_alerts
            .retain(|(h, uid), _| h != host || alerts.contains_key(uid));
        for (uid, alert) in alerts {
            self.record_owner_attention(host, uid, Some(alert.clone()));
        }
        self.sync_owner_alert_indicators();
    }

    pub(crate) fn apply_owner_attention(&mut self, host: &HostId, uid: &str, alert: Option<Alert>) {
        self.record_owner_attention(host, uid, alert);
        self.sync_owner_alert_indicators();
    }

    fn record_owner_attention(&mut self, host: &HostId, uid: &str, alert: Option<Alert>) {
        let key = (host.clone(), uid.to_string());
        if let Some(alert) = alert {
            if alert.session_uid != uid {
                return;
            }
            if self.dismissed_owner_alerts.contains(&alert.id) {
                self.push_worker.ack_owner_alert(host.clone(), alert);
                return;
            }
            // The worker deduplicates successful desktop submissions, including
            // across TUI restarts. It also keeps every socket/disk/DBus call off
            // the input thread. A missing sidebar row never suppresses delivery.
            self.push_worker.present_owner_alert(alert.clone());
            self.owner_alert_since.entry(alert.id.clone()).or_insert_with(SystemTime::now);
            self.owner_alerts.insert(key, alert);
        } else {
            self.owner_alerts.remove(&key);
        }
    }

    pub(crate) fn sync_owner_alert_indicators(&mut self) {
        // Remove only the indicators we own; the legacy local TUI handler still
        // works for older MCP clients.
        let mut next = HashMap::new();
        for ((host, _), alert) in &self.owner_alerts {
            let exact = self
                .workspaces
                .iter()
                .flat_map(|w| &w.sessions)
                .find(|s| &s.host_id == host && s.uid == alert.session_uid);
            let row = exact.or_else(|| {
                self.workspaces.iter().flat_map(|w| &w.sessions).find(|s| {
                    &s.host_id == host
                        && alert.continuous_task_id.is_some()
                        && s.continuous_task_id == alert.continuous_task_id
                })
            });
            if let Some(row) = row {
                next.insert(row.uid.clone(), alert.message.clone());
            }
        }
        let changed = self
            .owner_alert_rows
            .iter()
            .any(|uid| !next.contains_key(uid))
            || next
                .iter()
                .any(|(uid, msg)| !self.owner_alert_rows.contains(uid) || self.alerts.get(uid) != Some(msg));
        if changed {
            for uid in self.owner_alert_rows.drain() {
                self.alerts.remove(&uid);
            }
            for (uid, message) in next {
                self.owner_alert_rows.insert(uid.clone());
                self.alerts.insert(uid, message);
            }
            self.needs_redraw = true;
        }
    }

    /// Open Owner alerts, one row per agent session, oldest first, for the
    /// Messages sidebar: daemon alerts from every host plus alerts the TUI
    /// raised itself for older MCP clients.
    pub(crate) fn open_owner_alerts(&mut self) -> Vec<super::messages::OpenAlert> {
        let label_of = |app: &App, host: &HostId, uid: &str| {
            app.workspaces
                .iter()
                .flat_map(|w| &w.sessions)
                .find(|s| &s.host_id == host && s.uid == uid)
                .map(|s| s.label.clone())
        };
        let mut out = Vec::new();
        for ((host, uid), alert) in &self.owner_alerts {
            out.push(super::messages::OpenAlert {
                uid: uid.clone(),
                host: host.clone(),
                label: label_of(self, host, uid).unwrap_or_else(|| alert.label.clone()),
                reason: alert.message.lines().next().unwrap_or("").to_owned(),
                since: self.owner_alert_since.get(&alert.id).copied().unwrap_or_else(SystemTime::now),
            });
        }
        let legacy: Vec<(String, String)> = self
            .alerts
            .iter()
            .filter(|(uid, _)| !self.owner_alert_rows.contains(*uid))
            .map(|(u, m)| (u.clone(), m.clone()))
            .collect();
        for (uid, message) in legacy {
            let since = *self.owner_alert_since.entry(format!("legacy:{uid}")).or_insert_with(SystemTime::now);
            let host = HostId::local();
            out.push(super::messages::OpenAlert {
                label: label_of(self, &host, &uid).unwrap_or_else(|| uid.clone()),
                reason: message.lines().next().unwrap_or("").to_owned(),
                uid,
                host,
                since,
            });
        }
        out.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.label.cmp(&b.label)));
        out
    }

    /// Enter on an "Open alerts" row: go to that session (as A-g does) and
    /// mark its alert seen.
    pub(crate) fn jump_to_owner_alert(&mut self, uid: &str, host: &HostId) {
        let target = self.workspaces.iter().enumerate().find_map(|(wi, w)| {
            (!w.is_closed).then(|| w.sessions.iter().position(|s| s.uid == uid && &s.host_id == host)).flatten()
                .map(|si| (wi, si))
        });
        let Some((wi, si)) = target else {
            self.messages.error = format!("That session is not in the sidebar ({} on `{}`)", uid, host.as_str());
            return;
        };
        let continuous = self.continuous_members().contains(&(wi, si));
        if continuous && !self.continuous_column_on {
            self.messages.error = "That session is in the continuous column: A-c shows it".into();
            return;
        }
        self.cursor_column = if continuous { SidebarColumn::Continuous } else { SidebarColumn::Main };
        self.cursor = Cursor::Session(wi, si);
        self.acknowledge_owner_alerts_for_row(uid);
        self.alerts.remove(uid);
        self.owner_alert_since.remove(&format!("legacy:{uid}"));
        self.messages.visible = false;
        self.needs_redraw = true;
    }

    pub(crate) fn acknowledge_owner_alerts_for_row(&mut self, uid: &str) {
        let row = self
            .workspaces
            .iter()
            .flat_map(|w| &w.sessions)
            .find(|s| s.uid == uid);
        let Some(row) = row else {
            return;
        };
        let keys: Vec<_> = self
            .owner_alerts
            .iter()
            .filter(|((h, _), a)| {
                h == &row.host_id
                    && (a.session_uid == uid
                        || (a.continuous_task_id.is_some()
                            && a.continuous_task_id == row.continuous_task_id))
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            if let Some(alert) = self.owner_alerts.remove(&key) {
                self.dismissed_owner_alerts.insert(alert.id.clone());
                self.push_worker.ack_owner_alert(key.0, alert);
            }
        }
        self.owner_alert_rows.remove(uid);
    }
}
