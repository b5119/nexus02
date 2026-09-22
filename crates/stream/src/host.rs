use std::sync::atomic::Ordering;
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
        // Bounded channel for backpressure handling (capacity 32 frames ~1 second at 30fps)
        let (tx, rx) = tokio::sync::mpsc::channel(32);

        let encoder = self.host.encoder.clone();
        let capture = self.host.capture.clone();
        let injector = self.host.injector.clone();

        tokio::spawn(async move {
            // Metrics counters for observability (shared via Arc for cross-task access)
            let frames_captured = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let frames_encoded = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let frames_sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let frames_dropped_blank = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let frames_dropped_lag = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let frames_dropped_channel_full = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let encode_errors = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let capture_errors = Arc::new(std::sync::atomic::AtomicU64::new(0));
            // Latency sum counters for average calculation
            let total_capture_latency_us = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let total_encode_latency_us = Arc::new(std::sync::atomic::AtomicU64::new(0));

            // Input injection handler
            let injector_clone = injector.clone();
            tokio::spawn(async move {
                use tokio_stream::StreamExt;
                while let Some(ev_result) = input_stream.next().await {
                    match ev_result {
                        Ok(ev) => {
                            if let Err(e) = injector_clone.lock().await.inject(&ev) {
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

            // Spawn periodic metrics logging task
            let log_frames_captured = Arc::clone(&frames_captured);
            let log_frames_encoded = Arc::clone(&frames_encoded);
            let log_frames_sent = Arc::clone(&frames_sent);
            let log_frames_dropped_blank = Arc::clone(&frames_dropped_blank);
            let log_frames_dropped_lag = Arc::clone(&frames_dropped_lag);
            let log_frames_dropped_channel_full = Arc::clone(&frames_dropped_channel_full);
            let log_encode_errors = Arc::clone(&encode_errors);
            let log_capture_errors = Arc::clone(&capture_errors);
            let log_total_capture_latency_us = Arc::clone(&total_capture_latency_us);
            let log_total_encode_latency_us = Arc::clone(&total_encode_latency_us);

            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
                loop {
                    interval.tick().await;
                    tracing::info!(
                        "stream metrics: captured={}, encoded={}, sent={}, dropped_blank={}, dropped_lag={}, dropped_channel_full={}, encode_err={}, capture_err={}, avg_capture_latency_us={}, avg_encode_latency_us={}",
                        log_frames_captured.load(Ordering::Relaxed),
                        log_frames_encoded.load(Ordering::Relaxed),
                        log_frames_sent.load(Ordering::Relaxed),
                        log_frames_dropped_blank.load(Ordering::Relaxed),
                        log_frames_dropped_lag.load(Ordering::Relaxed),
                        log_frames_dropped_channel_full.load(Ordering::Relaxed),
                        log_encode_errors.load(Ordering::Relaxed),
                        log_capture_errors.load(Ordering::Relaxed),
                        if log_frames_captured.load(Ordering::Relaxed) > 0 {
                            log_total_capture_latency_us.load(Ordering::Relaxed) / log_frames_captured.load(Ordering::Relaxed)
                        } else { 0 },
                        if log_frames_encoded.load(Ordering::Relaxed) > 0 {
                            log_total_encode_latency_us.load(Ordering::Relaxed) / log_frames_encoded.load(Ordering::Relaxed)
                        } else { 0 },
                    );
                }
            });

            // Capture/encode/send loop with proper frame pacing and drift correction
            let mut seq = 0u64;
            let mut encoder = encoder.lock().await;
            let mut capture = capture.lock().await;
            let fps = capture.fps();
            let frame_interval_ns = (1_000_000_000.0 / fps) as u64;

            // Monotonic start instant so frame timestamps are small, increasing
            // values (ms since stream start) that map cleanly to MediaCodec's
            // presentationTimeUs (µs), instead of huge Unix-epoch wall-clock
            // values that make hardware decoders reset and flicker.
            let start = std::time::Instant::now();

            // Frame pacing: track when the NEXT frame should be sent to prevent drift
            let mut next_frame_deadline =
                start + std::time::Duration::from_nanos(frame_interval_ns);

            loop {
                // Wait until it's time for the next frame (drift-free pacing)
                let now = std::time::Instant::now();
                if now < next_frame_deadline {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(next_frame_deadline))
                        .await;
                }

                // Schedule next frame deadline (adds frame_interval_ns, self-correcting)
                next_frame_deadline += std::time::Duration::from_nanos(frame_interval_ns);

                // If we're behind by more than 2 frames, skip this frame entirely
                let now = std::time::Instant::now();
                if now
                    > next_frame_deadline + std::time::Duration::from_nanos(frame_interval_ns * 2)
                {
                    // Drop frame to catch up - don't encode, just reschedule
                    frames_dropped_lag.fetch_add(1, Ordering::Relaxed);
                    next_frame_deadline = now + std::time::Duration::from_nanos(frame_interval_ns);
                    continue;
                }

                let capture_start = std::time::Instant::now();
                let frame = match capture.capture_frame() {
                    Ok(f) => f,
                    Err(e) => {
                        capture_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("capture failed: {e:#}");
                        continue;
                    }
                };
                let capture_latency_us = capture_start.elapsed().as_micros() as u64;
                frames_captured.fetch_add(1, Ordering::Relaxed);
                total_capture_latency_us.fetch_add(capture_latency_us, Ordering::Relaxed);

                let encode_start = std::time::Instant::now();
                let encoded = match encoder.encode(frame) {
                    Ok(e) => e,
                    Err(e) => {
                        encode_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("encode failed: {e:#}");
                        continue;
                    }
                };
                let encode_latency_us = encode_start.elapsed().as_micros() as u64;
                frames_encoded.fetch_add(1, Ordering::Relaxed);
                total_encode_latency_us.fetch_add(encode_latency_us, Ordering::Relaxed);

                seq += 1;

                let ts = start.elapsed().as_millis() as u64;

                let vf = VideoFrame {
                    sequence: seq,
                    timestamp_ms: ts,
                    width: encoded.width,
                    height: encoded.height,
                    data: encoded.data,
                    keyframe: encoded.keyframe,
                    capture_latency_us,
                    encode_latency_us,
                };

                // Non-blocking send with timeout and backpressure handling
                match tokio::time::timeout(std::time::Duration::from_millis(50), tx.send(Ok(vf)))
                    .await
                {
                    Ok(Ok(())) => {
                        frames_sent.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Err(_)) => {
                        tracing::warn!("send failed, breaking stream");
                        break;
                    }
                    Err(_) => {
                        // Timeout - channel full, drop frame and count
                        frames_dropped_channel_full.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!("channel full, dropping frame");
                        continue;
                    }
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
