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
    /// Make the next frame an IDR (set after a dropped frame or when a viewer joins).
    force_keyframe: bool,
}

#[cfg(feature = "ffmpeg")]
unsafe impl Send for FfmpegEncoder {}

#[cfg(feature = "ffmpeg")]
impl FfmpegEncoder {
    const VIDEO_TIME_BASE: (i32, i32) = (1, 1000); // ms
    const NOMINAL_FRAME_MS: i64 = 33; // 30 fps

    fn new(width: u32, height: u32, codec_name: &str, bitrate_kbps: u32) -> Result<Self> {
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

        // Target bitrate, scaled by resolution relative to 1080p. The rate cap (VBV) below
        // uses the same number, so a burst can never exceed it for long.
        let target_bps =
            (scaled_w * scaled_h) as u64 * (bitrate_kbps as u64 * 1000) / (1920 * 1080);
        enc.set_bit_rate(target_bps as usize);
        let vbv_maxrate = bitrate_kbps;
        // About one frame of buffering: a smaller buffer keeps latency low.
        let vbv_bufsize = (bitrate_kbps / 8).max(100);

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
            // forced-idr=1: a frame we mark as an I-frame becomes a real IDR, so a decoder that
            // lost earlier frames can restart from it (see `request_keyframe`).
            let x264_params = format!(
                "keyint=15:min-keyint=15:scenecut=0:bframes=0:repeat-headers=1:forced-idr=1:\
                 vbv-bufsize={vbv_bufsize}:vbv-maxrate={vbv_maxrate}:ref=1:me=dia:subme=0:no-deblock=1"
            );
            d.set("x264-params", &x264_params);
            d
        };

        // Add encoder options via Dictionary (not x264-params)
        // Only apply these for libx264; QSV doesn't support these options
        if codec_name == "libx264" {
            opts.set("preset", "ultrafast");
            opts.set("tune", "zerolatency");
            opts.set("profile", "baseline");
        }

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
            force_keyframe: true, // the first frame of a stream is always a keyframe
        })
    }

    fn encode(&mut self, frame: &CapturedFrame) -> Result<EncodedFrame> {
        use ffmpeg::{error, packet, Error};
        use ffmpeg_next as ffmpeg;

        // Copy row by row: the destination rows may be padded (stride > width * 4, e.g. for
        // widths that are not a multiple of 32 pixels), while captured rows are tight.
        // A single block copy would shear the picture diagonally on such screens.
        let row_bytes = (frame.width as usize) * 4;
        let dst_stride = self.src_frame.stride(0);
        let rows = (frame.height as usize).min(frame.data.len() / row_bytes.max(1));
        let dst = self.src_frame.data_mut(0);
        for y in 0..rows {
            let src_row = &frame.data[y * row_bytes..(y + 1) * row_bytes];
            let start = y * dst_stride;
            dst[start..start + row_bytes].copy_from_slice(src_row);
        }

        self.src_frame.set_pts(Some(self.pts));
        self.pts += Self::NOMINAL_FRAME_MS;

        // Reuse yuv frame
        self.yuv_frame.set_pts(self.src_frame.pts());
        self.scaler.run(&self.src_frame, &mut self.yuv_frame)?;

        // The scaler reuses `yuv_frame`, so set the picture type explicitly every time.
        let kind = if std::mem::take(&mut self.force_keyframe) {
            ffmpeg::util::picture::Type::I
        } else {
            ffmpeg::util::picture::Type::None
        };
        self.yuv_frame.set_kind(kind);

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

/// Default bitrate for a 1080p stream, in kbit/s. 4 Mbps is plenty for desktop content and
/// leaves headroom on Wi-Fi, where 8 Mbps caused stalls.
pub const DEFAULT_BITRATE_KBPS: u32 = 4_000;

/// Bitrate in kbit/s (for 1080p) for the `--quality` preset: `low`, `medium` or `high`.
/// Anything else (including an empty string) falls back to the default.
pub fn bitrate_kbps_for_quality(quality: &str) -> u32 {
    match quality.trim().to_ascii_lowercase().as_str() {
        "low" => 2_000,
        "medium" => DEFAULT_BITRATE_KBPS,
        "high" => 8_000,
        _ => DEFAULT_BITRATE_KBPS,
    }
}

impl Encoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        Self::with_bitrate(width, height, DEFAULT_BITRATE_KBPS)
    }

    /// Like [`Encoder::new`] with the bitrate taken from a `--quality` preset.
    pub fn with_quality(width: u32, height: u32, quality: &str) -> Result<Self> {
        Self::with_bitrate(width, height, bitrate_kbps_for_quality(quality))
    }

    pub fn with_bitrate(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self> {
        #[cfg(feature = "ffmpeg")]
        {
            ffmpeg_next::init()?;

            let (codec_name, inner) = if Self::has_qsv() {
                match FfmpegEncoder::new(width, height, "h264_qsv", bitrate_kbps) {
                    Ok(inner) => ("h264_qsv", inner),
                    Err(e) => {
                        tracing::warn!("QSV encoder failed ({}), falling back to libx264", e);
                        let inner = FfmpegEncoder::new(width, height, "libx264", bitrate_kbps)?;
                        ("libx264", inner)
                    }
                }
            } else {
                let inner = FfmpegEncoder::new(width, height, "libx264", bitrate_kbps)?;
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
            let _ = bitrate_kbps;
            tracing::info!("encoder: ffmpeg feature disabled, using scaffold (no-op)");
            Ok(Self {
                codec_name: "scaffold".to_string(),
                width,
                height,
                seq: 0,
            })
        }
    }

    /// Make the next encoded frame a keyframe (IDR). Call after a frame was dropped (every
    /// later frame depends on it) and when a viewer connects.
    pub fn request_keyframe(&mut self) {
        #[cfg(feature = "ffmpeg")]
        {
            self.inner.force_keyframe = true;
        }
    }

    #[cfg(feature = "ffmpeg")]
    fn has_qsv() -> bool {
        ffmpeg_next::encoder::find_by_name("h264_qsv").is_some()
    }

    pub fn encode(&mut self, frame: &CapturedFrame) -> Result<EncodedFrame> {
        self.seq += 1;
        let seq = self.seq;

        #[cfg(feature = "ffmpeg")]
        {
            let mut ef = self.inner.encode(frame)?;
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

    #[test]
    fn quality_presets_map_to_bitrates() {
        assert_eq!(bitrate_kbps_for_quality("low"), 2_000);
        assert_eq!(bitrate_kbps_for_quality("medium"), 4_000);
        assert_eq!(bitrate_kbps_for_quality("high"), 8_000);
        assert_eq!(
            bitrate_kbps_for_quality(" HIGH "),
            8_000,
            "case and spaces ignored"
        );
        assert_eq!(bitrate_kbps_for_quality(""), DEFAULT_BITRATE_KBPS);
        assert_eq!(bitrate_kbps_for_quality("ultra"), DEFAULT_BITRATE_KBPS);
    }

    #[cfg(feature = "ffmpeg")]
    fn solid(w: u32, h: u32, shade: u8) -> CapturedFrame {
        CapturedFrame {
            data: vec![shade; (w * h * 4) as usize],
            width: w,
            height: h,
        }
    }

    /// Frames until `is_key` is true or `max` frames were encoded; the encoder may buffer a
    /// frame or two, so look at a short window rather than exactly one frame.
    #[cfg(feature = "ffmpeg")]
    fn keyframe_in_next(enc: &mut Encoder, f: &CapturedFrame, max: usize) -> bool {
        (0..max).any(|_| {
            let e = enc.encode(f).unwrap();
            !e.data.is_empty() && e.keyframe
        })
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn the_first_frame_is_a_keyframe() {
        let mut enc = Encoder::new(128, 96).unwrap();
        let f = solid(128, 96, 40);
        assert!(
            keyframe_in_next(&mut enc, &f, 3),
            "stream must start on a keyframe"
        );
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn requesting_a_keyframe_produces_one_before_the_periodic_interval() {
        let mut enc = Encoder::new(128, 96).unwrap();
        let f = solid(128, 96, 40);
        // Settle past the first keyframe and a few delta frames (periodic IDR is every 15).
        for _ in 0..6 {
            enc.encode(&f).unwrap();
        }
        enc.request_keyframe();
        assert!(
            keyframe_in_next(&mut enc, &f, 3),
            "a requested keyframe must arrive within a couple of frames, not at the next periodic IDR"
        );
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn widths_that_are_not_a_multiple_of_32_encode_without_error() {
        // 1366 x 768 has row padding in the BGRA buffer: the old block copy sheared the picture.
        let mut enc = Encoder::new(1366, 768).unwrap();
        let f = solid(1366, 768, 200);
        for _ in 0..3 {
            enc.encode(&f).unwrap();
        }
    }
}
