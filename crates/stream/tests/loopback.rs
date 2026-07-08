// ADR T6: Loopback stream test.
// Synthetic BGRA frames → encode → gRPC → receive → verify sequence number.
//
// Run with: cargo test --features nexus-stream/ffmpeg -p nexus-stream --test loopback

#![cfg(feature = "ffmpeg")]

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{transport::Server, Request, Response, Status};

use nexus_proto::stream::v1::{
    stream_service_client::StreamServiceClient,
    stream_service_server::{StreamService, StreamServiceServer},
    InputEvent, VideoFrame,
};
use nexus_stream::capture::CapturedFrame;
use nexus_stream::encode::Encoder;

/// A minimal gRPC StreamService that feeds synthetic BGRA frames through
/// the real H.264 Encoder and streams the resulting VideoFrames.
struct SyntheticStreamHost {
    encoder: Arc<Mutex<Encoder>>,
}

#[tonic::async_trait]
impl StreamService for SyntheticStreamHost {
    type RemoteControlStream =
        tokio_stream::wrappers::ReceiverStream<Result<VideoFrame, Status>>;

    async fn remote_control(
        &self,
        _req: Request<tonic::Streaming<InputEvent>>,
    ) -> Result<Response<Self::RemoteControlStream>, Status> {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let encoder = self.encoder.clone();

        tokio::spawn(async move {
            let mut encoder = encoder.lock().await;
            let mut seq = 0u64;

            for _ in 0..10 {
                let frame = CapturedFrame {
                    data: vec![128u8; 1920 * 1080 * 4],
                    width: 1920,
                    height: 1080,
                };

                match encoder.encode(frame) {
                    Ok(encoded) => {
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
                    Err(e) => {
                        let _ = tx
                            .send(Err(Status::internal(format!("encode: {e}"))))
                            .await;
                        break;
                    }
                }

                tokio::time::sleep(Duration::from_millis(33)).await;
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }
}

#[tokio::test]
async fn t6_loopback_stream_synthetic_bgra_to_videoframe() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_test_writer()
        .try_init();

    let encoder = Arc::new(Mutex::new(
        Encoder::new(1920, 1080).expect("Encoder::new should succeed"),
    ));
    let host = SyntheticStreamHost { encoder };

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let local_addr = listener.local_addr().expect("local_addr should succeed");
    let incoming = TcpListenerStream::new(listener);

    tokio::spawn(async move {
        Server::builder()
            .add_service(StreamServiceServer::new(host))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    let endpoint = format!("http://{}", local_addr);
    let mut client = StreamServiceClient::connect(endpoint)
        .await
        .expect("client should connect");

    let (input_tx, input_rx) = tokio::sync::mpsc::channel::<InputEvent>(256);
    let _ = input_tx;
    let input_stream = tokio_stream::wrappers::ReceiverStream::new(input_rx);

    let response = client
        .remote_control(input_stream)
        .await
        .expect("remote_control should succeed");
    let mut stream = response.into_inner();

    let mut prev_seq = 0u64;
    let mut frame_count = 0u64;
    let timeout_dur = Duration::from_secs(10);

    loop {
        let msg = tokio::time::timeout(timeout_dur, stream.message()).await;

        match msg {
            Ok(Ok(Some(vf))) => {
                frame_count += 1;
                assert!(
                    vf.sequence > prev_seq,
                    "sequence must monotonically increase: prev={prev_seq}, cur={}",
                    vf.sequence,
                );
                assert!(
                    vf.width > 0 && vf.height > 0,
                    "dimensions must be valid: {}x{}",
                    vf.width,
                    vf.height,
                );
                assert!(vf.timestamp_ms > 0, "timestamp_ms must be non-zero");
                prev_seq = vf.sequence;

                if frame_count >= 5 {
                    break;
                }
            }
            Ok(Ok(None)) => {
                panic!("server closed stream after {frame_count} frames");
            }
            Ok(Err(status)) => {
                panic!("server returned error: {status}");
            }
            Err(_elapsed) => {
                panic!(
                    "timed out waiting for frame {} (prev_seq={})",
                    frame_count, prev_seq,
                );
            }
        }
    }

    assert!(
        frame_count >= 5,
        "expected at least 5 frames, got {frame_count}",
    );
    tracing::info!(
        "T6 loopback test passed: {} frames received, seq up to {}",
        frame_count,
        prev_seq,
    );
}
