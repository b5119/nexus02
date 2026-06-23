//! nexus-agent: the daemon that runs on every device in the mesh.
//!
//! Roles:
//! - HOST: exposes this device's filesystem over gRPC (FileService).
//!   Every device kind can be a host, including Android (within
//!   scoped-storage limits — see fs::android_storage for details
//!   once that module exists).
//! - CLIENT: consumes a remote device's FileService and (on Linux/
//!   macOS/Windows) mounts it via FUSE/WinFsp. Android cannot be a
//!   FUSE client — see docs/adr/0001-android-fuse-limitation.md.
//!
//! For milestone 1, this binary only implements the HOST role.
//! The CLIENT/mount side lives in the separate `nexus-fs` crate,
//! which calls into a running agent over gRPC rather than embedding
//! the serving logic itself.

mod config;
mod host;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "nexus-agent", about = "Nexus device mesh agent")]
struct Args {
    /// Directory this agent will serve when acting as a host.
    /// On Android this should be a SAF-accessible path — plain
    /// arbitrary paths outside the app sandbox will fail to read
    /// on Android 10+ regardless of permissions granted.
    #[arg(long, default_value = "/tmp/nexus-share")]
    serve_dir: String,

    /// Port to listen on for incoming gRPC connections from client agents.
    #[arg(long, default_value_t = 50051)]
    port: u16,
}

fn init_logging() {
    #[cfg(target_os = "android")]
    {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Info),
        );
    }

    #[cfg(not(target_os = "android"))]
    {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "nexus_agent=info".into()),
            )
            .init();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let args = Args::parse();
    let cfg = config::AgentConfig::load_or_create()?;

    tracing::info!(
        device_id = %cfg.device_id,
        serve_dir = %args.serve_dir,
        port = args.port,
        "starting nexus-agent"
    );

    host::run(args.serve_dir, args.port).await
}
