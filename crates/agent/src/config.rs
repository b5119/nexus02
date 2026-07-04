//! Persists this device's identity (DeviceId), its shared-secret auth token,
//! and its self-signed TLS material across restarts.
//!
//! On Linux/macOS this lives under `$HOME/.config/nexus/`. On Android,
//! "home directory" needs to be the app's own files dir (Context.getFilesDir()
//! equivalent) — handled by whatever wraps this binary in a foreground
//! service, which should set NEXUS_CONFIG_DIR before exec'ing this agent.
//! (See ADR 0003 for why $HOME alone isn't enough, and ADR 0004 for the
//! auth/TLS model these tokens and certs implement.)

use anyhow::{Context, Result};
use nexus_common::DeviceId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub device_id: DeviceId,

    /// Shared secret the client must present (as gRPC metadata) on every call.
    /// Generated once on first run and persisted here. `#[serde(default)]` lets
    /// configs written before auth existed deserialize cleanly — they get a
    /// token backfilled and re-saved on next load. See ADR 0004.
    #[serde(default)]
    pub auth_token: String,
}

impl AgentConfig {
    pub fn load_or_create() -> Result<Self> {
        let path = config_path()?;

        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config at {path:?}"))?;
            let mut cfg: AgentConfig =
                serde_json::from_str(&raw).with_context(|| "parsing existing agent config")?;

            // Migrate pre-auth configs: backfill a token and persist it so it
            // stays stable across restarts (a client paired once keeps working).
            if cfg.auth_token.is_empty() {
                cfg.auth_token = generate_token();
                write_config(&path, &cfg)?;
            }
            Ok(cfg)
        } else {
            let cfg = AgentConfig {
                device_id: DeviceId::new(),
                auth_token: generate_token(),
            };
            write_config(&path, &cfg)?;
            Ok(cfg)
        }
    }
}

fn write_config(path: &Path, cfg: &AgentConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(cfg)?)
        .with_context(|| format!("writing config to {path:?}"))?;
    Ok(())
}

/// 128-bit random token rendered as 32 hex chars. Plenty as a shared secret
/// for the LAN-trust threat model; not a replacement for real pairing (ADR 0004).
fn generate_token() -> String {
    Uuid::new_v4().simple().to_string()
}

/// The directory all per-device state lives in (config + TLS material).
/// NEXUS_CONFIG_DIR overrides the default, which is essential on Android where
/// $HOME is `/` and read-only (ADR 0003).
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("NEXUS_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME not set and NEXUS_CONFIG_DIR not provided")?;
    Ok(PathBuf::from(home).join(".config/nexus"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agent.json"))
}

/// PEM-encoded self-signed cert + key the host serves TLS with. The client
/// must be handed `cert_pem` (the cert acts as its own CA) to verify the
/// connection — for the loopback test that's the same file on disk; for a real
/// phone→Dell run you copy cert.pem to the client. See ADR 0004.
pub struct TlsIdentity {
    pub cert_pem: String,
    pub key_pem: String,
    pub cert_path: PathBuf,
}

pub fn load_or_create_tls_identity(device_id: &DeviceId) -> Result<TlsIdentity> {
    let dir = config_dir()?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    let device_id_str = device_id.to_string();

    // Check if existing cert has the DeviceId embedded as a SAN.
    // If not — the cert was generated before ADR 0013 and is missing the
    // identity SAN that the mTLS interceptor needs to extract the peer's
    // device_id from incoming connections. Regenerate it with the SAN.
    let needs_regen = || {
        let cert_pem = match std::fs::read_to_string(&cert_path) {
            Ok(s) => s,
            Err(_) => return true,
        };
        let key_ok = key_path.exists();
        if !key_ok {
            return true;
        }
        // Parse the existing self-signed cert and check for the DeviceId SAN.
        let params = match rcgen::CertificateParams::from_ca_cert_pem(&cert_pem) {
            Ok(p) => p,
            Err(_) => return true,
        };
        let found = params.subject_alt_names.iter().any(
            |san| matches!(san, rcgen::SanType::DnsName(name) if name.as_str() == device_id_str),
        );
        !found
    };

    if cert_path.exists() && key_path.exists() && !needs_regen() {
        let cert_pem = std::fs::read_to_string(&cert_path)
            .with_context(|| format!("reading TLS cert at {cert_path:?}"))?;
        let key_pem = std::fs::read_to_string(&key_path)
            .with_context(|| format!("reading TLS key at {key_path:?}"))?;
        return Ok(TlsIdentity {
            cert_pem,
            key_pem,
            cert_path,
        });
    }

    if cert_path.exists() {
        tracing::info!(
            "existing cert.pem is missing DeviceId SAN (pre-ADR-0013); regenerating with device_id {}",
            device_id_str
        );
    }

    // Generate a fresh self-signed cert valid for localhost / 127.0.0.1.
    // The DeviceId SAN is required for the mTLS interceptor to extract the
    // peer's identity from incoming connections (see ADR 0013 §6).
    // LAN-trust only — see ADR 0004 for what this does and does not cover.
    let san = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        device_id_str.clone(),
    ];
    let rcgen::CertifiedKey { cert, key_pair } = rcgen::generate_simple_self_signed(san)
        .context("generating self-signed TLS certificate")?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    std::fs::create_dir_all(&dir)?;
    // Write cert+key atomically: write to temp files, then rename.
    // This prevents a partial write from leaving the config dir in an
    // inconsistent state if the process is killed mid-write.
    let cert_tmp = dir.join("cert.pem.tmp");
    let key_tmp = dir.join("key.pem.tmp");
    std::fs::write(&cert_tmp, &cert_pem)
        .with_context(|| format!("writing TLS cert to {cert_tmp:?}"))?;
    std::fs::write(&key_tmp, &key_pem)
        .with_context(|| format!("writing TLS key to {key_tmp:?}"))?;
    std::fs::rename(&cert_tmp, &cert_path)?;
    std::fs::rename(&key_tmp, &key_path)?;

    Ok(TlsIdentity {
        cert_pem,
        key_pem,
        cert_path,
    })
}
