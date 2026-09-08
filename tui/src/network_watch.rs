//! Notice route changes and resume without waiting for SSH's dead-peer timer.
//! Only CM's own forwarding processes are replaced; agents stay daemon-owned.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub fn spawn(pool: &Arc<crate::host_pool::HostPool>) {
    #[cfg(target_os = "linux")]
    {
        let pool = Arc::downgrade(pool);
        let _ = std::thread::Builder::new()
            .name("cm-network-watch".into())
            .spawn(move || {
                let mut previous = default_routes();
                let mut last_poll = SystemTime::now();
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let Some(pool) = pool.upgrade() else {
                        return;
                    };
                    let current = default_routes();
                    let now = SystemTime::now();
                    let resumed = now
                        .duration_since(last_poll)
                        .is_ok_and(|d| d > Duration::from_secs(10));
                    let changed = matches!((&previous, &current), (Some(a), Some(b)) if a != b);
                    if resumed || changed {
                        pool.reset_ssh_after_network_change();
                    }
                    if current.is_some() {
                        previous = current;
                    }
                    last_poll = now;
                }
            });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pool;
}

#[cfg(target_os = "linux")]
fn default_routes() -> Option<Vec<String>> {
    let v4 = std::fs::read_to_string("/proc/net/route").ok()?;
    let v6 = std::fs::read_to_string("/proc/net/ipv6_route").unwrap_or_default();
    Some(route_fingerprint(&v4, &v6))
}

#[cfg(any(target_os = "linux", test))]
fn route_fingerprint(v4: &str, v6: &str) -> Vec<String> {
    let mut routes = Vec::new();
    for line in v4.lines() {
        let f: Vec<_> = line.split_whitespace().collect();
        if f.len() >= 8 && f[1] == "00000000" && f[7] == "00000000" {
            // Interface, next hop, flags and metric; counters are excluded.
            routes.push(format!("4:{}:{}:{}:{}", f[0], f[2], f[3], f[6]));
        }
    }
    for line in v6.lines() {
        let f: Vec<_> = line.split_whitespace().collect();
        if f.len() >= 10 && f[0] == "00000000000000000000000000000000" && f[1] == "00" {
            routes.push(format!("6:{}:{}:{}:{}", f[9], f[4], f[5], f[8]));
        }
    }
    routes.sort();
    routes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_changes_ignore_counters_and_non_default_routes() {
        let initial = "wlan0 00000000 0100000A 0003 0 0 600 00000000\nwlan0 0000000A 00000000 0001 0 0 0 00FFFFFF";
        let counters = "wlan0 00000000 0100000A 0003 15 90 600 00000000\ndocker0 000011AC 00000000 0001 0 0 0 0000FFFF";
        assert_eq!(
            route_fingerprint(initial, ""),
            route_fingerprint(counters, "")
        );
        assert_ne!(
            route_fingerprint(initial, ""),
            route_fingerprint(&initial.replace("wlan0", "eth0"), "")
        );
        assert_ne!(route_fingerprint(initial, ""), route_fingerprint("", ""));
    }
}
