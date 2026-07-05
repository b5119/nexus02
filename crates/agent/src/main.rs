mod config;
mod host;
mod pairing;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "nexus-agent", about = "Nexus device mesh agent")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the data-plane server (filesystem serving, GC, etc.)
    Serve {
        /// Directory this agent will serve when acting as a host.
        #[arg(long, default_value = "/tmp/nexus-share")]
        serve_dir: String,

        /// Port to listen on for incoming gRPC connections.
        #[arg(long, default_value_t = 50051)]
        port: u16,

        /// GC sweep interval in hours (ADR 0011).
        #[arg(long, default_value_t = 6)]
        gc_interval_hours: u64,

        /// Tombstone TTL in hours (ADR 0011).
        #[arg(long, default_value_t = 24)]
        tombstone_ttl_hours: u64,

        /// Hard cap per store (number of entries) before GC eviction.
        #[arg(long, default_value_t = 50_000)]
        max_store_entries: usize,
    },

    /// Start the pairing listener (port 50052, code-based device pairing)
    PairMode {
        /// Seconds before the pairing code expires (default 60).
        #[arg(long, default_value_t = 60)]
        timeout_secs: u64,

        /// Human-readable display name for this host (shown to the initiator).
        #[arg(long, default_value = "")]
        display_name: String,
    },

    /// List all paired devices from peers.json
    ListPeers,
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

    match args.command {
        Command::Serve {
            serve_dir,
            port,
            gc_interval_hours,
            tombstone_ttl_hours,
            max_store_entries,
        } => {
            let cfg = config::AgentConfig::load_or_create()?;

            tracing::info!(
                device_id = %cfg.device_id,
                %serve_dir,
                port,
                "starting nexus-agent serve"
            );

            host::run(
                serve_dir,
                port,
                cfg.auth_token,
                cfg.device_id,
                gc_interval_hours,
                tombstone_ttl_hours,
                max_store_entries,
            )
            .await
        }

        Command::PairMode {
            timeout_secs,
            display_name,
        } => {
            tracing::info!(timeout_secs, "starting pair-mode listener");
            pairing::run_pairing_listener(50052, timeout_secs, &display_name).await
        }

        Command::ListPeers => {
            let store = pairing::PeersStore::open()?;
            let peers = store.list();
            if peers.is_empty() {
                println!("No paired devices.");
                return Ok(());
            }
            println!("{:<40} {:<30} paired_at", "device_id", "display_name");
            println!("{} {} {}", "-".repeat(40), "-".repeat(30), "-".repeat(10));
            for (device_id, entry) in &peers {
                println!(
                    "{device_id:<40} {:<30} {}",
                    entry.display_name, entry.paired_at
                );
            }
            Ok(())
        }
    }
}
