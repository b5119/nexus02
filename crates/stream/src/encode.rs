use anyhow::Result;

use crate::capture::CapturedFrame;

/// Encoded H.264 frame ready for transport.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub keyframe: bool,
    pub timestamp_ms: u64,
}

/// H.264 encoder backed by FFmpeg.
///
/// Tries QSV (`h264_qsv`) first, falls back to `libx264`.
/// When the `ffmpeg` feature is disabled, returns empty frames (scaffold).
pub struct Encoder {
    codec_name: String,
    width: u32,
    height: u32,
    seq: u64,
    #[cfg(feature = "ffmpeg")]
    inner: FfmpegEncoder,
}

#[cfg(any(feature = "ffmpeg", test))]
fn scale_dimensions(w: u32, h: u32) -> (u32, u32) {
    if w <= 1920 && h <= 1080 {
        return (w, h);
    }
    let scale = (1920.0_f64 / w as f64).min(1080.0_f64 / h as f64);
    ((w as f64 * scale) as u32, (h as f64 * scale) as u32)
}

#[cfg(feature = "ffmpeg")]
struct FfmpegEncoder {
    ctx: ffmpeg_next::encoder::video::Encoder,
    scaler: ffmpeg_next::software::scaling::Context,
    scaled_w: u32,
    scaled_h: u32,
    pts: i64,
    src_frame: ffmpeg_next::frame::Video,
    yuv_frame: ffmpeg_next::frame::Video,
}

#[cfg(feature = "ffmpeg")]
unsafe impl Send for FfmpegEncoder {}

#[cfg(feature = "ffmpeg")]
impl FfmpegEncoder {
    const VIDEO_TIME_BASE: (i32, i32) = (1, 1000); // ms
    const NOMINAL_FRAME_MS: i64 = 33; // 30 fps

    fn new(width: u32, height: u32, codec_name: &str) -> Result<Self> {
        use ffmpeg::{codec, encoder, format, Dictionary};
        use ffmpeg_next as ffmpeg;

        let (scaled_w, scaled_h) = scale_dimensions(width, height);

        let codec = encoder::find_by_name(codec_name)
            .ok_or_else(|| anyhow::anyhow!("encoder {} not found", codec_name))?;

        let mut enc = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;

        enc.set_width(scaled_w);
        enc.set_height(scaled_h);
        enc.set_format(format::Pixel::YUV420P);
        enc.set_time_base(Self::VIDEO_TIME_BASE);
        enc.set_frame_rate(Some((30, 1)));

        // Target ~10 Mbps for 1080p30 with VBV constraints for network stability
let target_bps = (scaled_w * scaled_h) as u64 * 8_000_000 / (1920 * 1080);
        enc.set_bit_rate(target_bps as usize);

        // x264 tune for low-latency streaming (balanced for stability):
        // - keyint=15: IDR every ~0.5s for fast recovery
        // - min-keyint=15 + scenecut=0: strictly periodic IDRs
        // - bframes=0: no reordering delay
        // - repeat-headers=1: SPS/PPS with every IDR
        // - vbv: moderate constraints for network stability
        // - preset=ultrafast: fastest encoding
        // - tune=zerolatency: optimize for streaming
        // - profile=baseline: maximum decoder compatibility
        let mut opts = {
            let mut d = Dictionary::new();
            d.set(
                "x264-params",
                "keyint=15:min-keyint=15:scenecut=0:bframes=0:repeat-headers=1:vbv-bufsize=1000:vbv-maxrate=8000:ref=1:me=dia:subme=0:no-deblock=1",
            );
            d
        };
        
        // Add encoder options via Dictionary (not x264-params)
        opts.set("preset", "ultrafast");
        opts.set("tune", "zerolatency");
        opts.set("profile", "baseline");

        let opened = enc.open_with(opts)?;

        let scaler = ffmpeg::software::scaling::Context::get(
            format::Pixel::BGRA,
            width,
            height,
            format::Pixel::YUV420P,
            scaled_w,
            scaled_h,
            ffmpeg::software::scaling::flag::Flags::BILINEAR,
        )?;

        // Pre-allocate frames to avoid per-frame allocations
        let src_frame = ffmpeg::frame::Video::new(format::Pixel::BGRA, width, height);
        let yuv_frame = ffmpeg::frame::Video::new(format::Pixel::YUV420P, scaled_w, scaled_h);

        Ok(Self {
            ctx: opened,
            scaler,
            scaled_w,
            scaled_h,
            pts: 0,
            src_frame,
            yuv_frame,
        })
    }

    fn encode(&mut self, frame: &CapturedFrame) -> Result<EncodedFrame> {
        use ffmpeg::{error, packet, Error};
        use ffmpeg_next as ffmpeg;

        // Reuse pre-allocated frames - avoid allocation
        // Clear and copy new data
        let dst = self.src_frame.data_mut(0);
        let copy_len = dst.len().min(frame.data.len());
        dst[..copy_len].copy_from_slice(&frame.data[..copy_len]);

        self.src_frame.set_pts(Some(self.pts));
        self.pts += Self::NOMINAL_FRAME_MS;

        // Reuse yuv frame
        self.yuv_frame.set_pts(self.src_frame.pts());
        self.scaler.run(&self.src_frame, &mut self.yuv_frame)?;

        self.ctx.send_frame(&self.yuv_frame)?;

        let mut pkt = packet::Packet::empty();
        let data = match self.ctx.receive_packet(&mut pkt) {
            Ok(()) => pkt.data().unwrap_or(&[]).to_vec(),
            Err(Error::Other {
                errno: error::EAGAIN,
            }) => vec![],
            Err(e) => anyhow::bail!("encode receive_packet: {e}"),
        };

        let keyframe = pkt.is_key();

        Ok(EncodedFrame {
            data,
            width: self.scaled_w,
            height: self.scaled_h,
            keyframe,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        })
    }
}

impl Encoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        #[cfg(feature = "ffmpeg")]
        {
            ffmpeg_next::init()?;

            let (codec_name, inner) = if Self::has_qsv() {
                match FfmpegEncoder::new(width, height, "h264_qsv") {
                    Ok(inner) => ("h264_qsv", inner),
                    Err(e) => {
                        tracing::warn!("QSV encoder failed ({}), falling back to libx264", e);
                        let inner = FfmpegEncoder::new(width, height, "libx264")?;
                        ("libx264", inner)
                    }
                }
            } else {
                let inner = FfmpegEncoder::new(width, height, "libx264")?;
                ("libx264", inner)
            };

            Ok(Self {
                codec_name: codec_name.to_string(),
                width,
                height,
                seq: 0,
                inner,
            })
        }

        #[cfg(not(feature = "ffmpeg"))]
        {
            tracing::info!("encoder: ffmpeg feature disabled, using scaffold (no-op)");
            Ok(Self {
                codec_name: "scaffold".to_string(),
                width,
                height,
                seq: 0,
            })
        }
    }

    #[cfg(feature = "ffmpeg")]
    fn has_qsv() -> bool {
        ffmpeg_next::encoder::find_by_name("h264_qsv").is_some()
    }

    pub fn encode(&mut self, frame: CapturedFrame) -> Result<EncodedFrame> {
        self.seq += 1;
        let seq = self.seq;

        #[cfg(feature = "ffmpeg")]
        {
            let mut ef = self.inner.encode(&frame)?;
            if ef.data.is_empty() {
                ef.keyframe = seq.is_multiple_of(60);
            }
            Ok(ef)
        }

        #[cfg(not(feature = "ffmpeg"))]
        {
            let _ = frame;
            Ok(EncodedFrame {
                data: vec![],
                width: self.width,
                height: self.height,
                keyframe: seq.is_multiple_of(60),
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            })
        }
    }

    pub fn codec_name(&self) -> &str {
        &self.codec_name
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t3_encoder_initializes() {
        let enc = Encoder::new(1920, 1080).unwrap();
        let name = enc.codec_name();
        assert!(
            name == "libx264" || name == "h264_qsv" || name == "scaffold",
            "codec should be one of the supported encoders: {name}"
        );
    }

    #[test]
    fn scale_dimensions_unchanged_below_1080p() {
        let (w, h) = scale_dimensions(1280, 720);
        assert_eq!(w, 1280);
        assert_eq!(h, 720);
    }

    #[test]
    fn scale_dimensions_scales_4k() {
        let (w, h) = scale_dimensions(3840, 2160);
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }
}
