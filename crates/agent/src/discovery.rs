use std::fmt::Write as FmtWrite;
use std::time::Duration;

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use nexus_common::DeviceId;

use crate::pairing::PeersStore;

/// Instance name format: `<display_name> (<device_id_short>)`
fn instance_name(display_name: &str, device_id: &DeviceId) -> String {
    let short: String = device_id.to_string().chars().take(8).collect();
    format!("{display_name} ({short})")
}

/// RAII guard that registers an mDNS service on creation and deregisters on drop.
pub struct DiscoveryService {
    daemon: ServiceDaemon,
    fullname: String,
}

impl DiscoveryService {
    pub fn register(
        device_id: &DeviceId,
        port: u16,
        display_name: &str,
        peers: &PeersStore,
    ) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let name = instance_name(display_name, device_id);
        let hostname = format!("{}.local.", device_id);
        let device_id_str = device_id.to_string();

        let paired_list: Vec<String> = peers
            .list()
            .iter()
            .map(|(id, _entry)| id.clone())
            .collect();
        let paired_str = paired_list.join(",");

        let properties: &[(&str, &str)] = &[
            ("device_id", &device_id_str),
            ("nexus_version", "1"),
            ("paired", &paired_str),
        ];

        let info = ServiceInfo::new(
            "_nexus._tcp.local.",
            &name,
            &hostname,
            "0.0.0.0",
            port,
            properties,
        )
        .context("creating mDNS service info")?;

        // Fullname for deregistration: <instance>._nexus._tcp.local.
        let fullname = format!("{name}._nexus._tcp.local.");
        daemon
            .register(info)
            .context("registering mDNS service")?;

        tracing::info!(%fullname, %port, "mDNS service registered");

        Ok(Self { daemon, fullname })
    }
}

impl Drop for DiscoveryService {
    fn drop(&mut self) {
        self.daemon.unregister(&self.fullname).ok();
    }
}

/// Result from a single discovered service.
#[derive(Debug, Clone)]
pub struct DiscoveredService {
    pub device_id: String,
    pub display_name: String,
    pub address: String,
    pub port: u16,
    pub is_paired: bool,
}

/// Scan the LAN for `_nexus._tcp.local` services for up to `timeout_secs`.
pub fn discover(timeout_secs: u64) -> Result<Vec<DiscoveredService>> {
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon
        .browse("_nexus._tcp.local.")
        .context("starting mDNS browse")?;

    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    let mut services: Vec<DiscoveredService> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(std::cmp::min(remaining, Duration::from_secs(1))) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let device_id = info
                    .get_property_val_str("device_id")
                    .unwrap_or_default()
                    .to_string();
                if device_id.is_empty() || !seen.insert(device_id.clone()) {
                    continue;
                }
                let display_name = info
                    .get_hostname()
                    .trim_end_matches(".local.")
                    .to_string();
                let is_paired = PeersStore::open()
                    .map(|store| {
                        uuid::Uuid::parse_str(&device_id)
                            .ok()
                            .map(DeviceId)
                            .map(|did| store.contains(&did))
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);

                let addr = info
                    .get_addresses()
                    .iter()
                    .next()
                    .copied()
                    .unwrap_or_else(|| std::net::Ipv4Addr::new(0, 0, 0, 0).into());
                services.push(DiscoveredService {
                    device_id,
                    display_name,
                    address: addr.to_string(),
                    port: info.get_port(),
                    is_paired,
                });
            }
            Ok(_) => {}
            Err(_) if std::time::Instant::now() < deadline => {}
            Err(e) => {
                tracing::warn!(error = %e, "mDNS recv error");
                break;
            }
        }
    }

    Ok(services)
}

/// Print discovered services as a table to stdout.
pub fn print_discover_table(services: &[DiscoveredService]) {
    if services.is_empty() {
        println!("No nexus devices found on the LAN.");
        return;
    }

    let mut table = String::new();
    let _ = writeln!(
        table,
        "{:<40} {:<30} {:<21} {:<6} paired?",
        "device_id", "display_name", "address", "port"
    );
    let _ = writeln!(
        table,
        "{} {} {} {} {}",
        "-".repeat(40),
        "-".repeat(30),
        "-".repeat(21),
        "-".repeat(6),
        "-".repeat(6)
    );
    for s in services {
        let paired = if s.is_paired { "yes" } else { "no" };
        let _ = writeln!(
            table,
            "{:<40} {:<30} {:<21} {:<6} {}",
            s.device_id, s.display_name, s.address, s.port, paired
        );
    }
    print!("{table}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    static INIT: Once = Once::new();

    fn init() {
        INIT.call_once(|| {
            let _ = tracing_subscriber::fmt()
                .with_env_filter("nexus_agent=debug")
                .with_test_writer()
                .try_init();
        });
    }

    fn make_peers_store() -> PeersStore {
        let dir = std::env::temp_dir()
            .join(format!("nexus-discovery-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        PeersStore::open_in(&dir).unwrap()
    }

    fn short_id(device_id: &DeviceId) -> String {
        device_id.to_string().chars().take(8).collect()
    }

    #[test]
    fn t3_instance_name_format() {
        let id = DeviceId::new();
        let short = short_id(&id);
        let name = instance_name("My Laptop", &id);
        assert_eq!(name, format!("My Laptop ({short})"));
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "mdns_test"), ignore)]
    async fn t1_register_and_discover() {
        init();
        let device_id = DeviceId::new();
        let peers = make_peers_store();

        let _service = DiscoveryService::register(&device_id, 0, "T1 Test Device", &peers)
            .expect("register should succeed");

        tokio::time::sleep(Duration::from_millis(500)).await;

        let results = discover(3).expect("discover should succeed");
        assert!(
            results.iter().any(|s| s.device_id == device_id.to_string()),
            "discovered services should include the registered device; got {results:?}"
        );
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "mdns_test"), ignore)]
    async fn t2_txt_record_fields() {
        init();
        let device_id = DeviceId::new();
        let peers = make_peers_store();
        let device_id_str = device_id.to_string();

        let _service = DiscoveryService::register(&device_id, 0, "T2 Device", &peers)
            .expect("register should succeed");

        tokio::time::sleep(Duration::from_millis(500)).await;

        let results = discover(3).expect("discover should succeed");
        let ours = results
            .iter()
            .find(|s| s.device_id == device_id_str)
            .expect("our device should be in results");

        assert_eq!(ours.device_id, device_id_str, "device_id TXT field");
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "mdns_test"), ignore)]
    async fn t4_cleanup_on_drop() {
        init();
        let device_id = DeviceId::new();
        let peers = make_peers_store();
        let device_id_str = device_id.to_string();

        let fullname = {
            let service = DiscoveryService::register(&device_id, 0, "T4 Device", &peers)
                .expect("register should succeed");
            tokio::time::sleep(Duration::from_millis(500)).await;

            let before = discover(2).expect("discover should succeed");
            assert!(
                before.iter().any(|s| s.device_id == device_id_str),
                "service should be discoverable before drop"
            );
            service.fullname.clone()
        };
        let _ = fullname;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let after = discover(3).expect("discover should succeed");
        assert!(
            !after.iter().any(|s| s.device_id == device_id_str),
            "service should NOT be discoverable after drop; got {after:?}"
        );
    }
}
