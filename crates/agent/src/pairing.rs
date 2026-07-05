use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rand::rngs::OsRng;
use rand::Rng;
use subtle::ConstantTimeEq;
use tonic::{transport::Server, Request, Response, Status};

use nexus_common::DeviceId;

// ---------------------------------------------------------------------------
// Code generation + verification
// ---------------------------------------------------------------------------

pub struct PairingCode {
    code: String,
    generated_at: SystemTime,
    timeout: Duration,
    used: Mutex<bool>,
    failed_attempts: Mutex<u32>,
    max_attempts: u32,
}

impl PairingCode {
    pub fn with_max_attempts(timeout_secs: u64, max_attempts: u32) -> Self {
        let n: u32 = OsRng.gen_range(0..1_000_000);
        let code = format!("{n:06}");
        tracing::info!(
            "pairing code: {code}  (expires in {timeout_secs}s, max {max_attempts} attempts, one-time-use)"
        );
        println!("🔑 Pairing code: {code}  (expires in {timeout_secs}s, max {max_attempts} attempts, one-time-use)");
        Self {
            code,
            generated_at: SystemTime::now(),
            timeout: Duration::from_secs(timeout_secs),
            used: Mutex::new(false),
            failed_attempts: Mutex::new(0),
            max_attempts,
        }
    }

    pub fn verify(&self, input: &str) -> bool {
        // Gate 1: constant-time comparison to prevent timing side-channel.
        let matched: bool = self.code.as_bytes().ct_eq(input.as_bytes()).into();
        if !matched {
            let mut failed = self.failed_attempts.lock().unwrap();
            *failed = failed.saturating_add(1);
            if *failed >= self.max_attempts {
                // Exhausted allowed attempts — invalidate the code so no
                // further guesses are possible, even within the time window.
                let mut used = self.used.lock().unwrap();
                *used = true;
            }
            return false;
        }
        // Gate 2: expiry.
        let elapsed = SystemTime::now()
            .duration_since(self.generated_at)
            .unwrap_or(Duration::ZERO);
        if elapsed > self.timeout {
            return false;
        }
        // Gate 3: one-time-use.
        let mut used = self.used.lock().unwrap();
        if *used {
            return false;
        }
        *used = true;
        true
    }
}

// ---------------------------------------------------------------------------
// PeersStore — host-side (peers.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerEntry {
    pub cert_pem: String,
    pub paired_at: u64,
    pub display_name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PeersFile {
    peers: HashMap<String, PeerEntry>,
}

pub struct PeersStore {
    path: PathBuf,
    inner: Mutex<PeersFile>,
}

impl PeersStore {
    /// Create an empty store that won't persist (for fallback when
    /// peers.json is unavailable).
    pub fn empty() -> Self {
        Self {
            path: PathBuf::new(),
            inner: Mutex::new(PeersFile {
                peers: HashMap::new(),
            }),
        }
    }

    /// Open a PeersStore backed by `dir/peers.json`.
    /// Useful for testing or non-default config locations.
    #[allow(dead_code)]
    pub fn open_in(dir: &std::path::Path) -> Result<Self> {
        let path = dir.join("peers.json");
        let inner = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading peers.json at {path:?}"))?;
            serde_json::from_str(&raw).with_context(|| format!("parsing peers.json at {path:?}"))?
        } else {
            PeersFile {
                peers: HashMap::new(),
            }
        };
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    pub fn open() -> Result<Self> {
        let path = crate::config::config_dir()?.join("peers.json");
        let inner = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading peers.json at {path:?}"))?;
            serde_json::from_str(&raw).with_context(|| format!("parsing peers.json at {path:?}"))?
        } else {
            PeersFile {
                peers: HashMap::new(),
            }
        };
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    #[allow(dead_code)]
    pub fn get(&self, device_id: &DeviceId) -> Option<PeerEntry> {
        let map = self.inner.lock().unwrap();
        map.peers.get(&device_id.to_string()).cloned()
    }

    #[allow(dead_code)]
    pub fn contains(&self, device_id: &DeviceId) -> bool {
        let map = self.inner.lock().unwrap();
        map.peers.contains_key(&device_id.to_string())
    }

    #[allow(dead_code)]
    pub fn verify_cert(&self, device_id: &DeviceId, cert_pem: &str) -> bool {
        let map = self.inner.lock().unwrap();
        match map.peers.get(&device_id.to_string()) {
            Some(entry) => entry.cert_pem == cert_pem,
            None => false,
        }
    }

    pub fn add(&self, device_id: &DeviceId, cert_pem: String, display_name: String) -> Result<()> {
        let mut map = self.inner.lock().unwrap();
        map.peers.insert(
            device_id.to_string(),
            PeerEntry {
                cert_pem,
                paired_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                display_name,
            },
        );
        persist_json(&self.path, &*map)
    }

    pub fn list(&self) -> Vec<(String, PeerEntry)> {
        let map = self.inner.lock().unwrap();
        let mut entries: Vec<_> = map
            .peers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        entries.sort_by_key(|a| a.1.paired_at);
        entries
    }
}

// ---------------------------------------------------------------------------
// Persistent store helpers
// ---------------------------------------------------------------------------

fn persist_json<T: serde::Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| anyhow::anyhow!("serialization error: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).with_context(|| format!("writing {tmp:?}"))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming {tmp:?} -> {path:?}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// PairingServer — gRPC implementation
// ---------------------------------------------------------------------------

use std::sync::Arc;

pub struct PairingServer {
    pub store: Arc<PeersStore>,
    pub code: Arc<PairingCode>,
    pub host_device_id: DeviceId,
    pub host_cert_pem: String,
    pub shutdown_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

#[tonic::async_trait]
impl nexus_proto::pair::v1::pair_service_server::PairService for PairingServer {
    async fn request_pair(
        &self,
        req: Request<nexus_proto::pair::v1::PairRequest>,
    ) -> Result<Response<nexus_proto::pair::v1::PairResponse>, Status> {
        let inner = req.into_inner();

        // Validate the 6-digit code.
        if !self.code.verify(&inner.code) {
            return Ok(Response::new(nexus_proto::pair::v1::PairResponse {
                accepted: false,
                host_cert_pem: String::new(),
                host_device_id: String::new(),
                error_message: "invalid, expired, or already-used code".to_string(),
            }));
        }

        // Parse initiator device_id.
        let initiator_id = match inner.initiator_device_id.parse::<uuid::Uuid>() {
            Ok(u) => DeviceId(u),
            Err(_) => {
                return Ok(Response::new(nexus_proto::pair::v1::PairResponse {
                    accepted: false,
                    host_cert_pem: String::new(),
                    host_device_id: String::new(),
                    error_message: "invalid initiator_device_id".to_string(),
                }));
            }
        };

        // Store the initiator's cert.
        self.store
            .add(
                &initiator_id,
                inner.initiator_cert_pem,
                inner.initiator_display_name,
            )
            .map_err(|e| Status::internal(format!("failed to persist peer: {e}")))?;

        tracing::info!(
            initiator_id = %initiator_id,
            "device paired successfully"
        );

        // Signal the shutdown channel so the listener exits immediately
        // instead of waiting for the full timeout window.
        if let Some(tx) = self.shutdown_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }

        Ok(Response::new(nexus_proto::pair::v1::PairResponse {
            accepted: true,
            host_cert_pem: self.host_cert_pem.clone(),
            host_device_id: self.host_device_id.to_string(),
            error_message: String::new(),
        }))
    }

    async fn list_peers(
        &self,
        _req: Request<nexus_proto::pair::v1::ListPeersRequest>,
    ) -> Result<Response<nexus_proto::pair::v1::ListPeersResponse>, Status> {
        let peers = self.store.list();
        let proto_peers = peers
            .into_iter()
            .map(|(device_id, entry)| nexus_proto::pair::v1::PeerInfo {
                device_id,
                display_name: entry.display_name,
                paired_at: entry.paired_at as i64,
            })
            .collect();
        Ok(Response::new(nexus_proto::pair::v1::ListPeersResponse {
            peers: proto_peers,
        }))
    }
}

// ---------------------------------------------------------------------------
// Pairing listener runner
// ---------------------------------------------------------------------------

pub async fn run_pairing_listener(port: u16, timeout_secs: u64, display_name: &str) -> Result<()> {
    let cfg = crate::config::AgentConfig::load_or_create()?;
    let tls = crate::config::load_or_create_tls_identity(&cfg.device_id)?;

    let store = Arc::new(PeersStore::open()?);
    let code = Arc::new(PairingCode::with_max_attempts(timeout_secs, 5));
    let host_device_id = cfg.device_id;
    let display_name = display_name.to_string();

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    let server = PairingServer {
        store: store.clone(),
        code: code.clone(),
        host_device_id,
        host_cert_pem: tls.cert_pem.clone(),
        shutdown_tx: Mutex::new(Some(tx)),
    };

    let identity = tonic::transport::Identity::from_pem(&tls.cert_pem, &tls.key_pem);

    let addr = format!("0.0.0.0:{port}").parse()?;
    tracing::info!(%addr, timeout_secs, display_name = %display_name, "pairing listener started");

    // Shutdown after timeout or first successful pair.

    // Shutdown after timeout or first successful pair.
    let shutdown = async move {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
                tracing::info!("pairing window expired");
            }
            _ = rx => {
                tracing::info!("pairing completed, shutting down listener");
            }
        }
    };

    Server::builder()
        .tls_config(tonic::transport::ServerTlsConfig::new().identity(identity))
        .map_err(|e| anyhow::anyhow!("TLS config error: {e}"))?
        .add_service(nexus_proto::pair::v1::pair_service_server::PairServiceServer::new(server))
        .serve_with_shutdown(addr, shutdown)
        .await?;

    Ok(())
}
