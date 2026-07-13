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
    #[arg(long)]
    host: String,

    /// Use a previously paired host's certificate (mTLS).
    #[arg(long)]
    trusted: bool,

    /// Shared-secret auth token. Falls back to NEXUS_AUTH_TOKEN env var.
    #[arg(long, env = "NEXUS_AUTH_TOKEN")]
    token: Option<String>,

    /// Device ID of a specific paired host (for --trusted with multiple hosts).
    #[arg(long)]
    host_id: Option<String>,
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

    let (host_device_id, ca_pem, client_cert_pem, client_key_pem) = if args.trusted {
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

        let device_id = match (args.host_id.as_ref(), hosts.len()) {
            (Some(id), _) => id.clone(),
            (None, 1) => {
                let id = hosts.keys().next().expect("hosts.len() == 1").clone();
                tracing::info!("Using paired host: {id}");
                id
            }
            (None, n) if n > 1 => {
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

        (device_id, cert, Some(client_cert), Some(client_key))
    } else {
        ("remote".to_string(), None, None, None)
    };

    tracing::info!(
        "nexus-viewer connecting to {} (trusted: {}, device_id: {})",
        args.host,
        args.trusted,
        host_device_id
    );

    let mut viewer = StreamViewer::connect(
        &args.host,
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
