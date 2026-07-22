use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use nexus_stream::viewer::StreamViewer;

fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("NEXUS_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME not set and NEXUS_CONFIG_DIR not provided")?;
    Ok(PathBuf::from(home).join(".config/nexus"))
}

#[derive(Parser, Debug)]
#[command(
    name = "nexus-viewer",
    about = "View and control a remote nexus-agent stream host's screen"
)]
struct Args {
    /// Address of the stream host, e.g. https://192.168.1.50:50051
    #[arg(long, required_unless_present = "discover")]
    host: Option<String>,

    /// Use a previously paired host's certificate (mTLS).
    #[arg(long)]
    trusted: bool,

    /// Shared-secret auth token. Falls back to NEXUS_AUTH_TOKEN env var.
    #[arg(long, env = "NEXUS_AUTH_TOKEN")]
    token: Option<String>,

    /// Device ID of a specific paired host (for --trusted with multiple hosts).
    #[arg(long)]
    host_id: Option<String>,

    /// Discover paired hosts on the LAN via mDNS instead of specifying --host.
    #[arg(long, conflicts_with = "host")]
    discover: bool,

    /// mDNS scan timeout in seconds (default 5).
    #[arg(long, default_value_t = 5)]
    discover_timeout: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nexus_stream=info".into()),
        )
        .init();

    let args = Args::parse();

    let (host_str, host_device_id, ca_pem, client_cert_pem, client_key_pem) = if args.discover {
        let (host, device_id, ca, cert, key) =
            resolve_via_mdns_viewer(args.host_id.as_deref(), args.discover_timeout).await?;
        (host, device_id, ca, cert, key)
    } else if args.trusted {
        let store_path = config_dir()?.join("trusted-certs.json");
        if !store_path.exists() {
            anyhow::bail!("No paired hosts found. Run nexus-mount pair first.");
        }
        let raw = std::fs::read_to_string(&store_path)
            .with_context(|| format!("reading {}", store_path.display()))?;
        let parsed: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", store_path.display()))?;
        let hosts = parsed
            .get("hosts")
            .and_then(|v| v.as_object())
            .ok_or_else(|| anyhow::anyhow!("malformed trusted-certs.json: missing 'hosts' key"))?;

        let device_id = match args.host_id.as_deref() {
            Some(hid) => {
                if !hosts.contains_key(*hid) {
                    anyhow::bail!("Host ID {hid} not found among paired devices");
                }
                hid.to_string()
            }
            None if hosts.len() == 1 => {
                let id = hosts.keys().next().expect("hosts.len() == 1").clone();
                tracing::info!("Using paired host: {id}");
                id
            }
            None if hosts.len() > 1 => {
                let keys: Vec<_> = hosts.keys().map(|k| format!("  {k}")).collect();
                anyhow::bail!(
                    "Multiple paired hosts found:\n{}\nUse --host-id <device_id> to specify which one.",
                    keys.join("\n")
                );
            }
            _ => anyhow::bail!("No paired hosts found. Run nexus-mount pair first."),
        };

        let cert = hosts
            .get(&device_id)
            .and_then(|e| e.get("cert_pem"))
            .and_then(|c| c.as_str())
            .map(|c| c.to_string());

        // Load client identity cert+key for mTLS.
        let cfg_dir = config_dir()?;
        let client_cert = std::fs::read_to_string(cfg_dir.join("cert.pem"))
            .with_context(|| format!("reading cert.pem from {}", cfg_dir.display()))?;
        let client_key = std::fs::read_to_string(cfg_dir.join("key.pem"))
            .with_context(|| format!("reading key.pem from {}", cfg_dir.display()))?;

        (
            args.host.clone().unwrap(),
            device_id,
            cert,
            Some(client_cert),
            Some(client_key),
        )
    } else {
        let host = args.host.clone().unwrap();
        (host, "remote".to_string(), None, None, None)
    };

    tracing::info!(
        "nexus-viewer connecting to {} (device_id: {})",
        host_str,
        host_device_id
    );

    let mut viewer = StreamViewer::connect(
        &host_str,
        &host_device_id,
        args.token.as_deref(),
        ca_pem.as_deref(),
        client_cert_pem.as_deref(),
        client_key_pem.as_deref(),
    )
    .await?;

    viewer.run().await?;

    Ok(())
}

/// Resolve a host via mDNS for the viewer client.
async fn resolve_via_mdns_viewer(
    host_id: Option<&str>,
    timeout_secs: u64,
) -> Result<(
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
)> {
    let daemon = mdns_sd::ServiceDaemon::new()?;
    let receiver = daemon
        .browse("_nexus._tcp.local.")
        .context("starting mDNS browse")?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut discovered = Vec::new();
    let mut seen = std::collections::HashSet::new();

    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(std::cmp::min(remaining, std::time::Duration::from_secs(1))) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                let device_id = info
                    .get_property_val_str("device_id")
                    .unwrap_or_default()
                    .to_string();
                if device_id.is_empty() || !seen.insert(device_id.clone()) {
                    continue;
                }
                let addr = info
                    .get_addresses()
                    .iter()
                    .copied()
                    .find(|a| !a.is_unspecified())
                    .ok_or_else(|| anyhow::anyhow!("no usable address for mDNS service"))?;
                discovered.push((device_id, addr, info.get_port()));
            }
            Ok(_) => {}
            Err(_) if std::time::Instant::now() < deadline => {}
            Err(e) => {
                tracing::warn!(error = %e, "mDNS recv error");
                break;
            }
        }
    }

    let store_path = config_dir()?.join("trusted-certs.json");
    let raw = std::fs::read_to_string(&store_path)
        .with_context(|| format!("reading {}", store_path.display()))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", store_path.display()))?;
    let hosts = parsed
        .get("hosts")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("malformed trusted-certs.json: missing 'hosts' key"))?;

    let paired: Vec<_> = discovered
        .into_iter()
        .filter(|(id, _, _)| hosts.contains_key(id))
        .collect();

    if paired.is_empty() {
        anyhow::bail!(
            "No paired nexus devices found on the LAN. Pair first with `nexus-mount pair`."
        );
    }

    let (chosen_id, addr, port) = match args.host_id.as_deref() {
        Some(hid) => paired
            .into_iter()
            .find(|(id, _, _)| id == hid)
            .ok_or_else(|| {
                anyhow::anyhow!("Host ID {hid} not found among discovered paired devices")
            })?,
        None if paired.len() == 1 => paired.into_iter().next().unwrap(),
        _ => {
            let list: String = paired
                .iter()
                .map(|(id, addr, port)| {
                    let sa = SocketAddr::new(*addr, *port);
                    format!("  {id}  https://{sa}")
                })
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!("Multiple paired hosts discovered. Use --host-id to select one:\n{list}");
        }
    };

    let cert = hosts
        .get(&chosen_id)
        .and_then(|e| e.get("cert_pem"))
        .and_then(|c| c.as_str())
        .map(|c| c.to_string());

    let cfg_dir = config_dir()?;
    let client_cert = std::fs::read_to_string(cfg_dir.join("cert.pem"))
        .with_context(|| format!("reading cert.pem from {}", cfg_dir.display()))?;
    let client_key = std::fs::read_to_string(cfg_dir.join("key.pem"))
        .with_context(|| format!("reading key.pem from {}", cfg_dir.display()))?;

    let sa = SocketAddr::new(addr, port);
    let host_str = format!("https://{sa}");
    tracing::info!(%host_str, chosen_id = %chosen_id, "resolved host via mDNS for viewer");

    Ok((
        host_str,
        chosen_id,
        cert,
        Some(client_cert),
        Some(client_key),
    ))
}
