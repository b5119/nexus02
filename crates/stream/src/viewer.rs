use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::Channel;
use tonic::Request;

use nexus_proto::stream::v1::{stream_service_client::StreamServiceClient, InputEvent};

use crate::decode::Decoder;
use crate::display::ViewerDisplay;
use crate::encode::EncodedFrame;

/// gRPC metadata header carrying the shared-secret auth token.
const AUTH_HEADER: &str = "x-nexus-token";

/// Stream viewer: connects to a host, receives H.264 frames, decodes
/// and displays them, and forwards input events back to the host.
pub struct StreamViewer {
    client: StreamServiceClient<Channel>,
    decoder: Arc<Mutex<Decoder>>,
    display: ViewerDisplay,
    token: MetadataValue<Ascii>,
}

impl StreamViewer {
    pub async fn connect(
        addr: &str,
        host_device_id: &str,
        token: Option<&str>,
        trusted_ca_pem: Option<&str>,
        client_cert_pem: Option<&str>,
        client_key_pem: Option<&str>,
    ) -> Result<Self> {
        let channel =
            connect_channel(addr, trusted_ca_pem, client_cert_pem, client_key_pem).await?;
        let client = StreamServiceClient::new(channel);

        let token_val: MetadataValue<Ascii> = token
            .unwrap_or("")
            .parse()
            .context("auth token must be valid ASCII")?;

        let width = 1920;
        let height = 1080;

        let decoder = Arc::new(Mutex::new(Decoder::new(width, height)?));
        let display = ViewerDisplay::new(width, height, host_device_id)?;

        tracing::info!(%addr, host_device_id, "viewer connected");

        Ok(Self {
            client,
            decoder,
            display,
            token: token_val,
        })
    }

    /// Run the viewer event loop.
    /// Receives frames, decodes, renders, and sends input events.
    pub async fn run(&mut self) -> Result<()> {
        let (input_tx, input_rx) = tokio::sync::mpsc::channel::<InputEvent>(256);

        let input_stream = tokio_stream::wrappers::ReceiverStream::new(input_rx);
        let mut req = Request::new(input_stream);
        req.metadata_mut().insert(AUTH_HEADER, self.token.clone());
        let response = self.client.remote_control(req).await?;
        let mut response_stream = response.into_inner();

        tracing::info!("viewer event loop started");

        while let Some(vf) = response_stream.message().await? {
            let encoded = EncodedFrame {
                data: vf.data,
                width: vf.width,
                height: vf.height,
                keyframe: vf.keyframe,
                timestamp_ms: vf.timestamp_ms,
            };

            let decoded = {
                let mut decoder = self.decoder.lock().await;
                decoder.decode(&encoded)?
            };

            self.display.render(decoded)?;

            while let Some(ev) = self.display.poll_input() {
                if input_tx.send(ev).await.is_err() {
                    return Ok(());
                }
            }

            if !self.display.is_running() {
                break;
            }
        }

        Ok(())
    }
}

async fn connect_channel(
    addr: &str,
    trusted_ca_pem: Option<&str>,
    client_cert_pem: Option<&str>,
    client_key_pem: Option<&str>,
) -> Result<Channel> {
    use tonic::transport::{Certificate, Channel as TonicChannel, ClientTlsConfig, Identity};

    // Normalize to https://host:port — handles "127.0.0.1:50051",
    // "http://127.0.0.1:50051", "https://127.0.0.1:50051".
    let input = addr
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let (host, port) = match input.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, p),
        _ => (input, "50051"),
    };
    let normalized = format!("https://{host}:{port}");
    let uri: tonic::transport::Uri = normalized.parse()?;
    let mut channel_builder = TonicChannel::builder(uri);

    if let Some(ca_pem) = trusted_ca_pem {
        let mut tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(ca_pem))
            .domain_name("localhost");
        if let (Some(cert), Some(key)) = (client_cert_pem, client_key_pem) {
            tls = tls.identity(Identity::from_pem(cert, key));
        }
        channel_builder = channel_builder.tls_config(tls)?;
    }

    let channel = channel_builder.connect().await?;
    Ok(channel)
}
