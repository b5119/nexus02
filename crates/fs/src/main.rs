mod config;
mod filesystem;
mod grpc_client;
mod pairing;

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
        #[arg(long)]
        remote: String,

        /// Local directory to mount onto. Must already exist and be empty.
        #[arg(long)]
        mountpoint: String,

        /// Shared-secret auth token. Falls back to NEXUS_AUTH_TOKEN env var.
        /// Not needed if --trusted is used (paired device).
        #[arg(long, env = "NEXUS_AUTH_TOKEN", required_unless_present = "trusted")]
        token: Option<String>,

        /// Path to the agent's TLS certificate. Falls back to NEXUS_CA_CERT env var.
        /// Not needed if --trusted is used (the paired host's cert is in trusted-certs.json).
        #[arg(long, env = "NEXUS_CA_CERT", required_unless_present = "trusted")]
        ca_cert: Option<String>,

        /// Use a previously paired host's certificate from trusted-certs.json
        /// instead of providing --token and --ca-cert.
        #[arg(long)]
        trusted: bool,
    },

    /// Pair with a remote host using a 6-digit code (ADR 0013)
    Pair {
        /// Address of the host to pair with (IP or hostname, no port — uses 50052).
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
        } => {
            let (token_val, ca_pem) = if trusted {
                // Read the host's cert from trusted-certs.json by matching the remote host.
                let store = pairing::TrustedCertsStore::open()?;
                // Extract host device_id from the remote address — use an env var or
                // a config. For now, require the user to set NEXUS_TRUSTED_HOST_ID.
                let host_id = std::env::var("NEXUS_TRUSTED_HOST_ID")
                    .context("NEXUS_TRUSTED_HOST_ID must be set when using --trusted")?;
                let host_id_parsed: uuid::Uuid = host_id
                    .parse()
                    .context("NEXUS_TRUSTED_HOST_ID is not a valid UUID")?;
                let entry = store
                    .get(&nexus_common::DeviceId(host_id_parsed))
                    .context("host not found in trusted-certs.json; pair with it first using `nexus-mount pair`")?;
                (String::new(), entry.cert_pem)
            } else {
                let token =
                    token.context("--token is required (or use --trusted for a paired device)")?;
                let ca_path = ca_cert
                    .context("--ca-cert is required (or use --trusted for a paired device)")?;
                let ca_pem = std::fs::read_to_string(&ca_path)
                    .with_context(|| format!("reading agent TLS cert at {ca_path}"))?;
                (token, ca_pem)
            };

            let cfg = config::ClientConfig::load_or_create()?;
            let clocks = nexus_common::ClockStore::open(config::clock_store_path()?)
                .context("opening client clock store")?;
            tracing::info!(device_id = %cfg.device_id, "client identity loaded");

            let client = grpc_client::RemoteFs::connect(remote.clone(), ca_pem, token_val).await?;
            let fs = filesystem::NexusFuse::new(client, cfg.device_id.to_string(), clocks);

            let mountpoint_clone = mountpoint.clone();
            tokio::task::spawn_blocking(move || {
                let options = vec![fuser::MountOption::FSName("nexus".into())];
                fuser::mount2(fs, &mountpoint_clone, &options)
            })
            .await??;

            Ok(())
        }

        Command::Pair {
            host,
            code,
            display_name,
            cert_path,
        } => {
            let cfg = config::ClientConfig::load_or_create()?;

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

            let (host_id, host_cert_pem) = pairing::pair_with_host(
                &host,
                50052,
                &code,
                &cert_pem,
                &cfg.device_id,
                &display_name,
            )
            .await?;

            // Store the host's cert in trusted-certs.json.
            let store = pairing::TrustedCertsStore::open()?;
            let host_device_id: uuid::Uuid = host_id.parse()?;
            store.add(
                &nexus_common::DeviceId(host_device_id),
                host_cert_pem,
                display_name,
            )?;

            println!("To mount, run: NEXUS_TRUSTED_HOST_ID={host_id} nexumount mount --remote https://{host}:50051 --mountpoint <dir> --trusted");

            Ok(())
        }
    }
}
