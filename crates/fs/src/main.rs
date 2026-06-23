//! nexus-mount: mounts a remote device's filesystem (served by a
//! running nexus-agent host) onto a local directory via FUSE.
//!
//! This only builds on Linux/macOS — see Cargo.toml's target-gated
//! fuser dependency. Android has no equivalent for a third-party
//! FUSE client; that direction is handled by a custom in-app file
//! browser instead, which is a separate, much simpler piece (no FUSE
//! involved at all — just gRPC calls rendered into a list view).

mod filesystem;
mod grpc_client;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "nexus-mount", about = "Mount a remote nexus-agent host's files locally")]
struct Args {
    /// Address of the remote agent to mount, e.g. http://192.168.1.50:50051
    /// For milestone 1 this is typed in manually; the control-plane
    /// registry (device discovery) replaces this once it exists.
    #[arg(long)]
    remote: String,

    /// Local directory to mount onto. Must already exist and be empty.
    #[arg(long)]
    mountpoint: String,
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

    tracing::info!(remote = %args.remote, mountpoint = %args.mountpoint, "mounting");

    let client = grpc_client::RemoteFs::connect(args.remote).await?;
    let fs = filesystem::NexusFuse::new(client);

    // mount2 blocks the current thread until unmount; run it on a
    // dedicated blocking thread so we don't tie up the tokio runtime
    // that grpc_client needs for its async calls underneath.
    let mountpoint = args.mountpoint.clone();
    tokio::task::spawn_blocking(move || {
        let options = vec![fuser::MountOption::RO, fuser::MountOption::FSName("nexus".into())];
        fuser::mount2(fs, &mountpoint, &options)
    })
    .await??;

    Ok(())
}
