//! Client-side persistent state for nexus-mount.
//!
//! Mirrors the agent's `config.rs` pattern (ADR 0003): a DeviceId generated
//! once and persisted, with `NEXUS_CONFIG_DIR` overriding the default
//! `$HOME/.config/nexus/` location. The client needs its own DeviceId so it can
//! stamp its own counter into a file's vector clock on local writes (ADR 0005),
//! and a `ClockStore` to remember the last clock it knew for each path.
//!
//! Stored separately from the agent's files (client.json / client-clocks.json)
//! so a single machine can run both an agent and a mount without collision.

use anyhow::{Context, Result};
use nexus_common::DeviceId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct ClientConfig {
    pub device_id: DeviceId,
}

impl ClientConfig {
    pub fn load_or_create() -> Result<Self> {
        let path = config_dir()?.join("client.json");
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading client config at {path:?}"))?;
            serde_json::from_str(&raw).with_context(|| "parsing client config")
        } else {
            let cfg = ClientConfig {
                device_id: DeviceId::new(),
            };
            write_config(&path, &cfg)?;
            Ok(cfg)
        }
    }
}

fn write_config(path: &Path, cfg: &ClientConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(cfg)?)
        .with_context(|| format!("writing client config to {path:?}"))?;
    Ok(())
}

/// Same resolution as the agent: `NEXUS_CONFIG_DIR` if set, else
/// `$HOME/.config/nexus` (ADR 0003).
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("NEXUS_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME not set and NEXUS_CONFIG_DIR not provided")?;
    Ok(PathBuf::from(home).join(".config/nexus"))
}

/// Path to the client's per-file vector-clock store.
pub fn clock_store_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("client-clocks.json"))
}
