mod config;
mod filesystem;
mod grpc_client;
mod pairing;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "nexus-mount",
    about = "Mount a remote nexus-agent host's files locally"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Mount a remote filesystem via FUSE
    Mount {
        /// Address of the remote agent, e.g. https://192.168.1.50:50051
        #[arg(long, required_unless_present = "discover")]
        remote: Option<String>,

        /// Local directory to mount onto. Must already exist and be empty.
        #[arg(long)]
        mountpoint: String,

        /// Shared-secret auth token. Falls back to NEXUS_AUTH_TOKEN env var.
        /// Not needed if --trusted or --discover is used (paired device).
        #[arg(long, env = "NEXUS_AUTH_TOKEN", required_unless_present_any = ["trusted", "discover"])]
        token: Option<String>,

        /// Path to the agent's TLS certificate. Falls back to NEXUS_CA_CERT env var.
        /// Not needed if --trusted or --discover is used (the paired host's cert is in trusted-certs.json).
        #[arg(long, env = "NEXUS_CA_CERT", required_unless_present_any = ["trusted", "discover"])]
        ca_cert: Option<String>,

        /// Use a previously paired host's certificate from trusted-certs.json
        /// instead of providing --token and --ca-cert.
        #[arg(long)]
        trusted: bool,

        /// Discover paired hosts on the LAN via mDNS instead of specifying --remote.
        #[arg(long, conflicts_with = "remote")]
        discover: bool,

        /// Device ID of a specific paired host (for --discover with multiple hosts).
        #[arg(long)]
        host_id: Option<String>,

        /// mDNS scan timeout in seconds (default 5).
        #[arg(long, default_value_t = 5)]
        discover_timeout: u64,
    },

    /// Pair with a remote host using a 6-digit code (ADR 0013)
    Pair {
        /// Address of the host to pair with (IP, hostname, or full URL; port defaults to 50052).
        #[arg(long)]
        host: String,

        /// 6-digit pairing code displayed by the host.
        #[arg(long)]
        code: String,

        /// Human-readable display name for this device (shown on the host).
        #[arg(long, default_value = "")]
        display_name: String,

        /// Path to this device's identity certificate. Defaults to cert.pem
        /// in the NEXUS_CONFIG_DIR.
        #[arg(long)]
        cert_path: Option<String>,

        /// Path to the host's TLS cert (CA cert) for the pairing connection.
        /// Defaults to NEXUS_CA_CERT env var.  Use this when the host uses a
        /// self-signed cert (which is the default for nexus-agent).
        #[arg(long, env = "NEXUS_CA_CERT")]
        ca_cert: Option<String>,
    },
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() -> Result<()> {
    anyhow::bail!(
        "nexus-mount only supports Linux and macOS. \
         Windows support requires a WinFsp-based implementation \
         (not yet written — see docs/adr for status)."
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nexus_fs=info".into()),
        )
        .init();

    let args = Args::parse();

    match args.command {
        Command::Mount {
            remote,
            mountpoint,
            token,
            ca_cert,
            trusted,
            discover,
            host_id,
            discover_timeout,
        } => {
            let (remote_addr, token_val, ca_pem, maybe_client_identity, effective_device_id) =
                if discover {
                    resolve_via_mdns(host_id.as_deref(), discover_timeout).await?
                } else if trusted {
                    let store = pairing::TrustedCertsStore::open()?;
                    let host_id_val = std::env::var("NEXUS_TRUSTED_HOST_ID")
                        .context("NEXUS_TRUSTED_HOST_ID must be set when using --trusted")?;
                    let host_id_parsed: uuid::Uuid = host_id_val
                        .parse()
                        .context("NEXUS_TRUSTED_HOST_ID is not a valid UUID")?;
                    let entry = store
                        .get(&nexus_common::DeviceId(host_id_parsed))
                        .context("host not found in trusted-certs.json; pair with it first using `nexus-mount pair`")?;

                    let cfg_dir = config::config_dir()?;
                    let cert_pem =
                        std::fs::read_to_string(cfg_dir.join("cert.pem")).with_context(|| {
                            format!(
                                "reading client cert from {}",
                                cfg_dir.join("cert.pem").display()
                            )
                        })?;
                    let key_pem =
                        std::fs::read_to_string(cfg_dir.join("key.pem")).with_context(|| {
                            format!(
                                "reading client key from {}",
                                cfg_dir.join("key.pem").display()
                            )
                        })?;

                    let cert_device_id = pairing::extract_device_id_from_cert_pem(&cert_pem)
                        .context("extracting device ID from client cert")?;

                    (
                        remote.clone().unwrap(),
                        String::new(),
                        entry.cert_pem,
                        Some((cert_pem, key_pem)),
                        cert_device_id,
                    )
                } else {
                    let remote_addr = remote.clone().unwrap();
                    let token_val =
                        token.context("--token is required (or use --trusted for a paired device)")?;
                    let ca_path = ca_cert
                        .context("--ca-cert is required (or use --trusted for a paired device)")?;
                    let ca_pem = std::fs::read_to_string(&ca_path)
                        .with_context(|| format!("reading agent TLS cert at {ca_path}"))?;
                    let cfg = config::ClientConfig::load_or_create()?;
                    (remote_addr, token_val, ca_pem, None, cfg.device_id)
                };

            let clocks = nexus_common::ClockStore::open(config::clock_store_path()?)
                .context("opening client clock store")?;
            tracing::info!(device_id = %effective_device_id, "client identity loaded");

            let client = match maybe_client_identity {
                Some((cert_pem, key_pem)) => {
                    grpc_client::RemoteFs::connect_trusted(
                        remote_addr,
                        ca_pem,
                        cert_pem,
                        key_pem,
                    )
                    .await?
                }
                None => grpc_client::RemoteFs::connect(remote_addr, ca_pem, token_val).await?,
            };
            let fs = filesystem::NexusFuse::new(client, effective_device_id.to_string(), clocks);

            let mountpoint_clone = mountpoint.clone();
            tokio::task::spawn_blocking(move || {
                let mut config = fuser::Config::default();
                config.mount_options = vec![fuser::MountOption::FSName("nexus".into())];
                fuser::mount2(fs, &mountpoint_clone, &config)
            })
            .await??;

            Ok(())
        }

        Command::Pair {
            host,
            code,
            display_name,
            cert_path,
            ca_cert,
        } => {
            // Determine the client's identity cert.
            let cert_pem = if let Some(ref path) = cert_path {
                std::fs::read_to_string(path)
                    .with_context(|| format!("reading cert from {path}"))?
            } else {
                let default_path = config::config_dir()?.join("cert.pem");
                if default_path.exists() {
                    std::fs::read_to_string(&default_path)
                        .with_context(|| format!("reading cert from {}", default_path.display()))?
                } else {
                    anyhow::bail!(
                        "no cert.pem found at {}; run `nexus-agent serve` once to generate one, \
                         or provide --cert-path",
                        default_path.display()
                    );
                }
            };

            let ca_cert_pem = match &ca_cert {
                Some(path) => {
                    let pem = std::fs::read_to_string(path)
                        .with_context(|| format!("reading host CA cert at {path}"))?;
                    Some(pem)
                }
                None => None,
            };

            // Use the device_id embedded in the cert's DNS SAN, not the
            // ClientConfig's device_id (they may differ — the cert was
            // generated by a separate agent serve invocation).
            let cert_device_id = pairing::extract_device_id_from_cert_pem(&cert_pem).context(
                "extracting device ID from client cert; is this a valid nexus-agent cert.pem?",
            )?;

            let (host_id, host_cert_pem) = pairing::pair_with_host(
                &host,
                50052,
                &code,
                &cert_pem,
                &cert_device_id,
                &display_name,
                ca_cert_pem.as_deref(),
            )
            .await?;

            // Store the host's cert in trusted-certs.json.
            let store = pairing::TrustedCertsStore::open()?;
            let host_device_id: uuid::Uuid = host_id.parse()?;
            store.add(
                &nexus_common::DeviceId(host_device_id),
                host_cert_pem,
                host_device_id.to_string(),
            )?;

            // Strip any existing port from `host` so the hint URL does not end
            // up with a duplicate port (e.g. "https://127.0.0.1:50052:50051").
            let host_part = host
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .rsplit_once(':')
                .map(|(h, _)| h)
                .unwrap_or(
                    host.trim_start_matches("http://")
                        .trim_start_matches("https://"),
                );
            println!("To mount, run: NEXUS_TRUSTED_HOST_ID={host_id} nexus-mount mount --remote https://{host_part}:50051 --mountpoint <dir> --trusted");

            Ok(())
        }
    }
}

/// Resolve a host address via mDNS discovery, filtering to paired hosts.
async fn resolve_via_mdns(
    host_id: Option<&str>,
    timeout_secs: u64,
) -> Result<(String, String, String, Option<(String, String)>, nexus_common::DeviceId)> {
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

    let store = pairing::TrustedCertsStore::open()?;
    let paired: Vec<_> = discovered
        .into_iter()
        .filter(|(id, _, _)| {
            let parsed = uuid::Uuid::parse_str(id).ok().map(nexus_common::DeviceId);
            parsed.is_some_and(|did| store.get(&did).is_some())
        })
        .collect();

    if paired.is_empty() {
        anyhow::bail!(
            "No paired nexus devices found on the LAN. Pair first with `nexus-mount pair`."
        );
    }

    let (chosen_id, addr, port) = match host_id {
        Some(hid) => paired
            .into_iter()
            .find(|(id, _, _)| id == hid)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Host ID {hid} not found among discovered paired devices"
                )
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

    let entry = store
        .get(&nexus_common::DeviceId(uuid::Uuid::parse_str(&chosen_id).unwrap()))
        .context("paired host entry not found")?;

    let cfg_dir = config::config_dir()?;
    let cert_pem = std::fs::read_to_string(cfg_dir.join("cert.pem"))
        .with_context(|| format!("reading client cert from {}", cfg_dir.join("cert.pem").display()))?;
    let key_pem = std::fs::read_to_string(cfg_dir.join("key.pem"))
        .with_context(|| format!("reading client key from {}", cfg_dir.join("key.pem").display()))?;
    let cert_device_id = pairing::extract_device_id_from_cert_pem(&cert_pem)
        .context("extracting device ID from client cert")?;

    let sa = SocketAddr::new(addr, port);
    let remote = format!("https://{sa}");
    tracing::info!(%remote, chosen_id = %chosen_id, "resolved host via mDNS");

    Ok((
        remote,
        String::new(),
        entry.cert_pem,
        Some((cert_pem, key_pem)),
        cert_device_id,
    ))
}
