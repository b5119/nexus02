//! Persists this device's identity (DeviceId) across restarts.
//! On Linux/macOS this writes to a dotfile in the home directory.
//! On Android, "home directory" needs to be the app's own
//! files dir (Context.getFilesDir() equivalent) — handled by
//! whatever wraps this binary in a foreground service, which
//! should set NEXUS_CONFIG_DIR before exec'ing this agent.

use anyhow::{Context, Result};
use nexus_common::DeviceId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub device_id: DeviceId,
}

impl AgentConfig {
    pub fn load_or_create() -> Result<Self> {
        let path = config_path()?;

        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config at {path:?}"))?;
            let cfg: AgentConfig = serde_json::from_str(&raw)
                .with_context(|| "parsing existing agent config")?;
            Ok(cfg)
        } else {
            let cfg = AgentConfig {
                device_id: DeviceId::new(),
            };
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)
                .with_context(|| format!("writing new config to {path:?}"))?;
            Ok(cfg)
        }
    }
}

fn config_path() -> Result<PathBuf> {
    // NEXUS_CONFIG_DIR lets the Android wrapper point this at the
    // app's sandboxed files directory instead of $HOME, which doesn't
    // meaningfully exist for an Android foreground service.
    if let Ok(dir) = std::env::var("NEXUS_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("agent.json"));
    }

    let home = std::env::var("HOME").context("HOME not set and NEXUS_CONFIG_DIR not provided")?;
    Ok(PathBuf::from(home).join(".config/nexus/agent.json"))
}
