//! Approval-based pairing: no code to type, no timing window.
//!
//! While `serve` runs, it also listens for "please pair me" requests. When one
//! arrives, the host shows a dialog ("IN-101 wants to pair. Code 0637. Pair / Deny")
//! and the user clicks **Pair** on the machine itself. The tablet shows the same
//! 4-digit code, so a person-in-the-middle is visible to the user:
//!
//! * the tablet computes the code from **the certificate it actually received** in the
//!   TLS handshake plus its own device id;
//! * the host computes it from **its own certificate** plus the claimed device id.
//!
//! If someone relays the connection with their own certificate, the two codes differ.
//!
//! Safeguards: only one request may be pending at a time, requests are rate limited per
//! IP, the dialog times out after 60 s (counts as a denial), and untrusted text (the
//! device name) is stripped of control characters, truncated, and shown without markup.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tonic::{transport::Server, Request, Response, Status};

use nexus_common::DeviceId;
use nexus_proto::pair::v1::{PairRequest, PairResponse};

use crate::pairing::{PairingRateLimiter, PeersStore};

/// Port of the always-on approval listener (the code/QR listener uses 50052).
pub const APPROVAL_PORT: u16 = 50053;

/// How long the user has to answer the dialog.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// 4-digit code both sides display: first 4 bytes of
/// `SHA-256("<initiator device id>|<host certificate fingerprint>")` mod 10,000.
pub fn short_auth_string(initiator_device_id: &str, host_cert_fingerprint_hex: &str) -> String {
    let digest = Sha256::digest(format!("{initiator_device_id}|{host_cert_fingerprint_hex}"));
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 10_000;
    format!("{n:04}")
}

/// Makes an untrusted device name safe to show: no control characters, trimmed, at most
/// 40 characters; empty names become "an unnamed device".
pub fn sanitize_device_name(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "an unnamed device".to_string()
    } else {
        cleaned.chars().take(40).collect()
    }
}

/// Asks the person at this machine whether to allow a device to pair.
// Newer clippy flags `#[must_use]` that `async_trait` adds to a future-returning method.
#[allow(clippy::double_must_use)]
#[tonic::async_trait]
pub trait Approver: Send + Sync {
    /// `true` only if the user explicitly approved. Timeout or failure is `false`.
    async fn approve(&self, device_name: &str, code: &str) -> bool;
}

/// Shows a `zenity` question dialog (present on GNOME). If `zenity` cannot run, the
/// request is denied: pairing never succeeds without a human answering.
pub struct ZenityApprover;

#[tonic::async_trait]
impl Approver for ZenityApprover {
    async fn approve(&self, device_name: &str, code: &str) -> bool {
        let text = format!(
            "{device_name} wants to pair with this computer.\n\nOnly click Pair if the \
             device is showing the code {code}."
        );
        let status = tokio::process::Command::new("zenity")
            .args([
                "--question",
                "--no-markup",
                "--title=Nexus pairing request",
                "--ok-label=Pair",
                "--cancel-label=Deny",
                &format!("--timeout={}", APPROVAL_TIMEOUT.as_secs()),
                &format!("--text={text}"),
            ])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .status()
            .await;
        match status {
            Ok(s) => s.success(), // 0 = Pair; 1 = Deny; 5 = timed out
            Err(e) => {
                tracing::warn!("cannot show pairing dialog (is zenity installed?): {e}");
                false
            }
        }
    }
}

pub struct ApprovalServer {
    pub store: Arc<PeersStore>,
    pub host_device_id: DeviceId,
    pub host_cert_pem: String,
    pub host_fingerprint: String,
    pub auth_token: String,
    pub approver: Arc<dyn Approver>,
    pub rate_limiter: PairingRateLimiter,
    pending: AtomicBool,
}

impl ApprovalServer {
    pub fn new(
        store: Arc<PeersStore>,
        host_device_id: DeviceId,
        host_cert_pem: String,
        auth_token: String,
        approver: Arc<dyn Approver>,
    ) -> Result<Self> {
        let host_fingerprint = crate::pair_link::cert_fingerprint(&host_cert_pem)?;
        Ok(Self {
            store,
            host_device_id,
            host_cert_pem,
            host_fingerprint,
            auth_token,
            approver,
            // Each request interrupts the user with a dialog: allow only a few per minute.
            rate_limiter: PairingRateLimiter::new(3, Duration::from_secs(60)),
            pending: AtomicBool::new(false),
        })
    }
}

fn reject(message: &str) -> Result<Response<PairResponse>, Status> {
    Ok(Response::new(PairResponse {
        accepted: false,
        host_cert_pem: String::new(),
        host_device_id: String::new(),
        error_message: message.to_string(),
        auth_token: String::new(),
    }))
}

/// Clears the "request pending" flag however the request ends (including cancellation).
struct PendingGuard<'a>(&'a AtomicBool);
impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[tonic::async_trait]
impl nexus_proto::pair::v1::pair_service_server::PairService for ApprovalServer {
    async fn request_pair(
        &self,
        req: Request<PairRequest>,
    ) -> Result<Response<PairResponse>, Status> {
        let client_ip = req
            .remote_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let inner = req.into_inner();

        if !inner.request_approval {
            return reject("this listener only accepts approval requests");
        }
        let (allowed, retry) = self.rate_limiter.check(&client_ip);
        if !allowed {
            let secs = retry.map(|d| d.as_secs()).unwrap_or(60);
            return reject(&format!("too many requests, retry in {secs} seconds"));
        }
        let initiator_id = match inner.initiator_device_id.parse::<uuid::Uuid>() {
            Ok(u) => DeviceId(u),
            Err(_) => return reject("invalid initiator_device_id"),
        };

        // One dialog at a time, so a flood cannot bury the user in prompts.
        if self
            .pending
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return reject("another pairing request is waiting for approval");
        }
        let _guard = PendingGuard(&self.pending);

        let name = sanitize_device_name(&inner.initiator_display_name);
        let code = short_auth_string(&inner.initiator_device_id, &self.host_fingerprint);
        tracing::info!(%initiator_id, %name, %code, "pairing approval requested");

        let approved = tokio::time::timeout(
            APPROVAL_TIMEOUT + Duration::from_secs(5),
            self.approver.approve(&name, &code),
        )
        .await
        .unwrap_or(false);
        if !approved {
            tracing::info!(%initiator_id, "pairing denied or timed out");
            return reject("pairing was denied or timed out on the host");
        }

        self.store
            .add(&initiator_id, inner.initiator_cert_pem, name)
            .map_err(|e| Status::internal(format!("failed to persist peer: {e}")))?;
        tracing::info!(%initiator_id, "device paired (approved on host)");
        Ok(Response::new(PairResponse {
            accepted: true,
            host_cert_pem: self.host_cert_pem.clone(),
            host_device_id: self.host_device_id.to_string(),
            error_message: String::new(),
            auth_token: self.auth_token.clone(),
        }))
    }

    async fn list_peers(
        &self,
        _req: Request<nexus_proto::pair::v1::ListPeersRequest>,
    ) -> Result<Response<nexus_proto::pair::v1::ListPeersResponse>, Status> {
        // Peer details are not disclosed over the unauthenticated approval listener.
        Err(Status::permission_denied(
            "not available on the approval listener",
        ))
    }
}

/// Runs the approval listener until the process exits.
pub async fn run_approval_listener(
    port: u16,
    store: Arc<PeersStore>,
    host_device_id: DeviceId,
    cert_pem: String,
    key_pem: String,
    auth_token: String,
) -> Result<()> {
    let server = ApprovalServer::new(
        store,
        host_device_id,
        cert_pem.clone(),
        auth_token,
        Arc::new(ZenityApprover),
    )?;
    let identity = tonic::transport::Identity::from_pem(&cert_pem, &key_pem);
    let addr = format!("0.0.0.0:{port}").parse()?;
    tracing::info!(%addr, "pairing approval listener started");
    Server::builder()
        .tls_config(tonic::transport::ServerTlsConfig::new().identity(identity))?
        .add_service(nexus_proto::pair::v1::pair_service_server::PairServiceServer::new(server))
        .serve(addr)
        .await
        .context("approval listener failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_proto::pair::v1::pair_service_server::PairService;
    use std::sync::atomic::AtomicUsize;

    const DEVICE: &str = "3f9a21bc-0000-4000-8000-000000000001";

    struct Fake {
        answer: bool,
        delay: Duration,
        calls: AtomicUsize,
    }
    #[tonic::async_trait]
    impl Approver for Fake {
        async fn approve(&self, _n: &str, _c: &str) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.answer
        }
    }

    fn server(answer: bool, delay_ms: u64) -> (ApprovalServer, Arc<Fake>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(PeersStore::open_in(dir.path()).unwrap());
        let cert = rcgen::generate_simple_self_signed(vec!["nexus".to_string()]).unwrap();
        let fake = Arc::new(Fake {
            answer,
            delay: Duration::from_millis(delay_ms),
            calls: AtomicUsize::new(0),
        });
        let srv = ApprovalServer::new(
            store,
            DeviceId::new(),
            cert.cert.pem(),
            "token-123".to_string(),
            fake.clone(),
        )
        .unwrap();
        (srv, fake, dir)
    }

    fn request(approval: bool, id: &str) -> Request<PairRequest> {
        Request::new(PairRequest {
            code: String::new(),
            initiator_device_id: id.to_string(),
            initiator_cert_pem: String::new(),
            initiator_display_name: "IN-101".to_string(),
            request_approval: approval,
        })
    }

    #[test]
    fn short_auth_string_matches_the_shared_test_vector() {
        // The same vector is asserted in the Android app's SasTest.
        assert_eq!(short_auth_string(DEVICE, &"ab".repeat(32)), "0637");
    }

    #[test]
    fn short_auth_string_depends_on_both_inputs() {
        let a = short_auth_string(DEVICE, &"ab".repeat(32));
        assert_ne!(
            a,
            short_auth_string(DEVICE, &"cd".repeat(32)),
            "different host cert"
        );
        assert_ne!(
            a,
            short_auth_string("3f9a21bc-0000-4000-8000-000000000002", &"ab".repeat(32)),
            "different device"
        );
        assert_eq!(a.len(), 4);
    }

    #[test]
    fn device_names_are_made_safe_for_display() {
        assert_eq!(sanitize_device_name("  IN-101 \n"), "IN-101");
        assert_eq!(sanitize_device_name("a\u{0007}b\u{001b}[31m"), "ab[31m");
        assert_eq!(sanitize_device_name(&"x".repeat(100)).chars().count(), 40);
        assert_eq!(sanitize_device_name("  \n\t"), "an unnamed device");
    }

    #[tokio::test]
    async fn approved_request_pairs_and_returns_credentials() {
        let (srv, fake, _dir) = server(true, 0);
        let resp = srv
            .request_pair(request(true, DEVICE))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.accepted, "{}", resp.error_message);
        assert_eq!(resp.auth_token, "token-123");
        assert!(!resp.host_cert_pem.is_empty());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        assert!(srv.store.contains(&DeviceId(DEVICE.parse().unwrap())));
    }

    #[tokio::test]
    async fn denied_request_stores_nothing_and_leaks_no_credentials() {
        let (srv, _fake, _dir) = server(false, 0);
        let resp = srv
            .request_pair(request(true, DEVICE))
            .await
            .unwrap()
            .into_inner();
        assert!(!resp.accepted);
        assert!(resp.auth_token.is_empty() && resp.host_cert_pem.is_empty());
        assert!(!srv.store.contains(&DeviceId(DEVICE.parse().unwrap())));
    }

    #[tokio::test]
    async fn requests_without_the_approval_flag_never_prompt() {
        let (srv, fake, _dir) = server(true, 0);
        let resp = srv
            .request_pair(request(false, DEVICE))
            .await
            .unwrap()
            .into_inner();
        assert!(!resp.accepted);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn invalid_device_id_is_rejected_before_prompting() {
        let (srv, fake, _dir) = server(true, 0);
        let resp = srv
            .request_pair(request(true, "not-a-uuid"))
            .await
            .unwrap()
            .into_inner();
        assert!(!resp.accepted);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn only_one_request_can_be_pending_at_a_time() {
        let (srv, fake, _dir) = server(true, 150);
        let srv = Arc::new(srv);
        let first = {
            let srv = srv.clone();
            tokio::spawn(async move { srv.request_pair(request(true, DEVICE)).await })
        };
        tokio::time::sleep(Duration::from_millis(40)).await;
        let second = srv
            .request_pair(request(true, "3f9a21bc-0000-4000-8000-000000000002"))
            .await
            .unwrap()
            .into_inner();
        assert!(!second.accepted);
        assert!(second.error_message.contains("another pairing request"));
        assert!(first.await.unwrap().unwrap().into_inner().accepted);
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            1,
            "second never prompted"
        );
        // The flag is released afterwards.
        let third = srv
            .request_pair(request(true, DEVICE))
            .await
            .unwrap()
            .into_inner();
        assert!(third.accepted);
    }

    #[tokio::test]
    async fn per_ip_rate_limit_applies() {
        let (srv, _fake, _dir) = server(false, 0);
        for _ in 0..3 {
            srv.request_pair(request(true, DEVICE)).await.unwrap();
        }
        let resp = srv
            .request_pair(request(true, DEVICE))
            .await
            .unwrap()
            .into_inner();
        assert!(
            resp.error_message.contains("too many requests"),
            "{}",
            resp.error_message
        );
    }

    #[tokio::test]
    async fn peer_list_is_not_disclosed() {
        let (srv, _fake, _dir) = server(true, 0);
        let err = srv
            .list_peers(Request::new(nexus_proto::pair::v1::ListPeersRequest {}))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }
}
