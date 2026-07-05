use std::collections::HashMap;
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

/// Load the paired host's cert for mTLS trusted connection.
fn load_trusted_cert(device_id: &str) -> Result<Option<String>> {
    let store_path = config_dir()?.join("trusted-certs.json");
    if !store_path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&store_path)
        .with_context(|| format!("reading {}", store_path.display()))?;
    let store: HashMap<String, serde_json::Value> = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", store_path.display()))?;
    if let Some(entry) = store.get(device_id) {
        if let Some(cert) = entry.get("cert_pem").and_then(|c| c.as_str()) {
            return Ok(Some(cert.to_string()));
        }
    }
    Ok(None)
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

    /// Environment variable for the trusted host device ID.
    /// Must be set when using --trusted (same convention as nexus-mount).
    #[arg(long, env = "NEXUS_TRUSTED_HOST_ID")]
    device_id: Option<String>,
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

    let host_device_id = if args.trusted {
        args.device_id
            .as_ref()
            .context("NEXUS_TRUSTED_HOST_ID must be set when using --trusted")?
            .clone()
    } else {
        "remote".to_string()
    };

    let ca_pem = if args.trusted {
        load_trusted_cert(&host_device_id)?
    } else {
        None
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
    )
    .await?;

    viewer.run().await?;

    Ok(())
}
