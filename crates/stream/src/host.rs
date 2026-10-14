use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

use nexus_proto::stream::v1::{stream_service_server::StreamService, InputEvent, VideoFrame};

use crate::capture::{CapturedFrame, ScreenCapture};
use crate::encode::Encoder;
use crate::inject::Injector;

/// Frames the send queue may hold (about 130 ms at 30 fps).
const SEND_QUEUE_FRAMES: usize = 4;

/// After this many consecutive idle ticks (about a second) a still screen is re-sent only
/// every third tick instead of every tick, so an idle desktop costs almost nothing.
const IDLE_FULL_RATE_TICKS: u32 = 30;

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
        // Small bounded queue: at 30 fps, 4 frames is ~130 ms. A deep queue (it was 32, about a
        // second) only turns a slow network into a second of lag; when it is full the sender
        // drops and recovers at a keyframe instead (see the loop below).
        let (tx, rx) = tokio::sync::mpsc::channel(SEND_QUEUE_FRAMES);

        let encoder = self.host.encoder.clone();
        let capture = self.host.capture.clone();
        let injector = self.host.injector.clone();

        tokio::spawn(async move {
            // Metrics counters for observability (shared via Arc for cross-task access)
            let frames_captured = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let frames_encoded = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let frames_sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let frames_repeated = Arc::new(std::sync::atomic::AtomicU64::new(0));
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
            let log_frames_repeated = Arc::clone(&frames_repeated);
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
                        "stream metrics: captured={}, encoded={}, sent={}, repeated={}, dropped_lag={}, dropped_channel_full={}, encode_err={}, capture_err={}, avg_capture_latency_us={}, avg_encode_latency_us={}",
                        log_frames_captured.load(Ordering::Relaxed),
                        log_frames_encoded.load(Ordering::Relaxed),
                        log_frames_sent.load(Ordering::Relaxed),
                        log_frames_repeated.load(Ordering::Relaxed),
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

            // Capture and encode are CPU-heavy and blocking (x264, polling PipeWire), so the loop runs
            // on a dedicated blocking thread. Running it on an async worker stalled input handling
            // and network I/O for tens of milliseconds per frame.
            let loop_task = tokio::task::spawn_blocking(move || {
                use tokio::sync::mpsc::error::TrySendError;

                let mut encoder = encoder.blocking_lock();
                let mut capture = capture.blocking_lock();
                let frame_interval = std::time::Duration::from_secs_f64(1.0 / capture.fps());

                // Monotonic start instant: frame timestamps are small, increasing values (ms since
                // stream start) that map cleanly to MediaCodec's presentationTimeUs.
                let start = std::time::Instant::now();
                let mut next_deadline = start + frame_interval;
                let mut seq = 0u64;
                let mut last_frame: Option<CapturedFrame> = None;
                let mut idle_ticks = 0u32;

                // A viewer that just connected can only start on a keyframe.
                encoder.request_keyframe();
                // After a frame is dropped every later delta frame is undecodable for the viewer
                // (it shows blocky artifacts until the next keyframe). So after a drop we do not
                // send delta frames at all, ask the encoder for a keyframe and resume at it.
                let mut awaiting_keyframe = false;

                loop {
                    // Drift-free pacing: sleep until the next frame is due.
                    let now = std::time::Instant::now();
                    if now < next_deadline {
                        std::thread::sleep(next_deadline - now);
                    }
                    next_deadline += frame_interval;

                    // More than two frames behind: skip this tick BEFORE encoding, which never
                    // corrupts the stream (the encoder's reference chain is untouched).
                    let now = std::time::Instant::now();
                    if now > next_deadline + frame_interval * 2 {
                        frames_dropped_lag.fetch_add(1, Ordering::Relaxed);
                        next_deadline = now + frame_interval;
                        continue;
                    }
                    if tx.is_closed() {
                        break;
                    }

                    let capture_start = std::time::Instant::now();
                    match capture.capture_frame() {
                        Ok(Some(f)) => {
                            last_frame = Some(f);
                            idle_ticks = 0;
                            frames_captured.fetch_add(1, Ordering::Relaxed);
                        }
                        // Unchanged screen: keep showing the previous picture (never black).
                        Ok(None) => idle_ticks = idle_ticks.saturating_add(1),
                        Err(e) => {
                            capture_errors.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!("capture failed: {e:#}");
                            continue;
                        }
                    }
                    let capture_latency_us = capture_start.elapsed().as_micros() as u64;
                    total_capture_latency_us.fetch_add(capture_latency_us, Ordering::Relaxed);

                    // Nothing captured yet (screen still before its first frame): nothing to send.
                    let Some(frame) = last_frame.as_ref() else {
                        continue;
                    };
                    // A long-idle screen is re-sent at a third of the rate.
                    if idle_ticks >= IDLE_FULL_RATE_TICKS && !idle_ticks.is_multiple_of(3) {
                        continue;
                    }
                    if idle_ticks > 0 {
                        frames_repeated.fetch_add(1, Ordering::Relaxed);
                    }

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

                    // The encoder can buffer a frame and return nothing yet.
                    if encoded.data.is_empty() {
                        continue;
                    }
                    if awaiting_keyframe {
                        if !encoded.keyframe {
                            frames_dropped_lag.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        awaiting_keyframe = false;
                    }

                    seq += 1;
                    let vf = VideoFrame {
                        sequence: seq,
                        timestamp_ms: start.elapsed().as_millis() as u64,
                        width: encoded.width,
                        height: encoded.height,
                        data: encoded.data,
                        keyframe: encoded.keyframe,
                        capture_latency_us,
                        encode_latency_us,
                    };

                    // Never wait for the network: a full queue means the viewer is behind.
                    match tx.try_send(Ok(vf)) {
                        Ok(()) => {
                            frames_sent.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(TrySendError::Full(_)) => {
                            frames_dropped_channel_full.fetch_add(1, Ordering::Relaxed);
                            awaiting_keyframe = true;
                            encoder.request_keyframe();
                        }
                        Err(TrySendError::Closed(_)) => {
                            tracing::info!("viewer disconnected, ending stream");
                            break;
                        }
                    }
                }
            });
            let _ = loop_task.await;
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
