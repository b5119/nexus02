use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tonic::transport::{Channel, ClientTlsConfig};
use tonic::Request;

use nexus_common::DeviceId;

// ---------------------------------------------------------------------------
// TrustedCertsStore — initiator-side (trusted-certs.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrustedHostEntry {
    pub cert_pem: String,
    pub host_display_name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TrustedCertsFile {
    hosts: HashMap<String, TrustedHostEntry>,
}

pub struct TrustedCertsStore {
    path: PathBuf,
    inner: Mutex<TrustedCertsFile>,
}

impl TrustedCertsStore {
    pub fn open() -> Result<Self> {
        let path = crate::config::config_dir()?.join("trusted-certs.json");
        let inner = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading trusted-certs.json at {path:?}"))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing trusted-certs.json at {path:?}"))?
        } else {
            TrustedCertsFile {
                hosts: HashMap::new(),
            }
        };
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    pub fn get(&self, device_id: &DeviceId) -> Option<TrustedHostEntry> {
        let map = self.inner.lock().unwrap();
        map.hosts.get(&device_id.to_string()).cloned()
    }

    pub fn add(
        &self,
        device_id: &DeviceId,
        cert_pem: String,
        host_display_name: String,
    ) -> Result<()> {
        let mut map = self.inner.lock().unwrap();
        map.hosts.insert(
            device_id.to_string(),
            TrustedHostEntry {
                cert_pem,
                host_display_name,
            },
        );
        persist_json(&self.path, &*map)
    }
}

fn persist_json<T: serde::Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| anyhow::anyhow!("serialization error: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)
        .with_context(|| format!("writing {tmp:?}"))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {tmp:?} -> {path:?}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pairing client
// ---------------------------------------------------------------------------

pub async fn pair_with_host(
    host: &str,
    port: u16,
    code: &str,
    cert_pem: &str,
    initiator_device_id: &DeviceId,
    display_name: &str,
) -> Result<(String, String)> {
    let addr = format!("https://{host}:{port}");
    let uri: tonic::transport::Uri = addr.parse()?;

    let tls = ClientTlsConfig::new().with_enabled_roots();

    let channel = Channel::builder(uri)
        .tls_config(tls)
        .map_err(|e| anyhow::anyhow!("TLS config error: {e}"))?
        .connect()
        .await?;

    let mut client =
        nexus_proto::pair::v1::pair_service_client::PairServiceClient::new(channel);

    let req = nexus_proto::pair::v1::PairRequest {
        code: code.to_string(),
        initiator_device_id: initiator_device_id.to_string(),
        initiator_cert_pem: cert_pem.to_string(),
        initiator_display_name: display_name.to_string(),
    };

    let resp = client.request_pair(Request::new(req)).await?.into_inner();

    if !resp.accepted {
        anyhow::bail!(
            "pairing rejected: {}",
            if resp.error_message.is_empty() {
                "unknown error"
            } else {
                &resp.error_message
            }
        );
    }

    tracing::info!(
        host_device_id = %resp.host_device_id,
        "paired successfully with host"
    );
    println!("Paired successfully with {}", resp.host_device_id);

    Ok((resp.host_device_id, resp.host_cert_pem))
}
