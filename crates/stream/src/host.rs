use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

use nexus_proto::stream::v1::{stream_service_server::StreamService, InputEvent, VideoFrame};

use crate::capture::ScreenCapture;
use crate::encode::Encoder;
use crate::inject::Injector;

/// Stream host: captures the screen, encodes H.264, and serves
/// the StreamService RPC to connected viewers.
pub struct StreamHost {
    capture: Arc<Mutex<ScreenCapture>>,
    encoder: Arc<Mutex<Encoder>>,
    injector: Arc<Mutex<Injector>>,
}

/// The gRPC service implementation for the streaming host.
pub struct StreamHostService {
    host: Arc<StreamHost>,
}

impl StreamHostService {
    pub fn new(host: Arc<StreamHost>) -> Self {
        Self { host }
    }
}

#[tonic::async_trait]
impl StreamService for StreamHostService {
    type RemoteControlStream = tokio_stream::wrappers::ReceiverStream<Result<VideoFrame, Status>>;

    async fn remote_control(
        &self,
        req: Request<tonic::Streaming<InputEvent>>,
    ) -> Result<Response<Self::RemoteControlStream>, Status> {
        let mut input_stream = req.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(32);

        let encoder = self.host.encoder.clone();
        let capture = self.host.capture.clone();
        let injector = self.host.injector.clone();

        tokio::spawn(async move {
            // Input injection handler
            let injector_clone = injector.clone();
            tokio::spawn(async move {
                use tokio_stream::StreamExt;
                while let Some(ev_result) = input_stream.next().await {
                    match ev_result {
                        Ok(ev) => {
                            if let Err(e) = injector_clone.blocking_lock().inject(&ev) {
                                tracing::warn!("input injection failed: {e:#}");
                            }
                        }
                        Err(e) => {
                            tracing::warn!("input stream error: {e}");
                            break;
                        }
                    }
                }
            });

            // Capture/encode/send loop
            let mut seq = 0u64;
            let mut encoder = encoder.lock().await;
            let mut capture = capture.lock().await;
            let fps = capture.fps();
            let mut interval = tokio::time::interval(std::time::Duration::from_secs_f64(1.0 / fps));

            loop {
                interval.tick().await;

                let frame = match capture.capture_frame() {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!("capture failed: {e:#}");
                        continue;
                    }
                };

                let encoded = match encoder.encode(frame) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("encode failed: {e:#}");
                        continue;
                    }
                };

                seq += 1;

                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;

                let vf = VideoFrame {
                    sequence: seq,
                    timestamp_ms: ts,
                    width: encoded.width,
                    height: encoded.height,
                    data: encoded.data,
                    keyframe: encoded.keyframe,
                };

                if tx.send(Ok(vf)).await.is_err() {
                    break;
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }
}

/// Start the stream host: initialize capture, encoder, and injector,
/// then register the StreamService on the given tonic server builder.
pub async fn run_stream_host(
    capture: ScreenCapture,
    encoder: Encoder,
    injector: Injector,
) -> Result<StreamHost> {
    tracing::info!(
        "stream host started: {}x{} @ {} FPS, encoder: {}",
        capture.dimensions().0,
        capture.dimensions().1,
        capture.fps(),
        encoder.codec_name(),
    );

    Ok(StreamHost {
        capture: Arc::new(Mutex::new(capture)),
        encoder: Arc::new(Mutex::new(encoder)),
        injector: Arc::new(Mutex::new(injector)),
    })
}
