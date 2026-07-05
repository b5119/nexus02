use anyhow::Result;

use crate::encode::EncodedFrame;

/// Decoded raw BGRA frame ready for display.
pub struct DecodedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// H.264 decoder backed by FFmpeg.
///
/// When the `ffmpeg` feature is disabled, returns empty frames (scaffold).
pub struct Decoder {
    #[cfg(not(feature = "ffmpeg"))]
    width: u32,
    #[cfg(not(feature = "ffmpeg"))]
    height: u32,
    #[cfg(feature = "ffmpeg")]
    inner: FfmpegDecoder,
}

#[cfg(feature = "ffmpeg")]
struct FfmpegDecoder {
    video: ffmpeg_next::decoder::Video,
    scaler: Option<ffmpeg_next::software::scaling::Context>,
    width: u32,
    height: u32,
}

#[cfg(feature = "ffmpeg")]
unsafe impl Send for FfmpegDecoder {}

#[cfg(feature = "ffmpeg")]
impl FfmpegDecoder {
    fn new(width: u32, height: u32) -> Result<Self> {
        use ffmpeg_next as ffmpeg;
        use ffmpeg::{codec, decoder};

        let codec = decoder::find_by_name("h264")
            .ok_or_else(|| anyhow::anyhow!("H.264 decoder not found"))?;

        let video = codec::context::Context::new_with_codec(codec)
            .decoder()
            .open()?
            .video()?;

        Ok(Self {
            video,
            scaler: None,
            width,
            height,
        })
    }

    fn decode(&mut self, frame: &EncodedFrame) -> Result<DecodedFrame> {
        use ffmpeg_next as ffmpeg;
        use ffmpeg::{error, packet, Error};

        let mut pkt = packet::Packet::copy(&frame.data);
        if frame.keyframe {
            pkt.set_flags(packet::Flags::KEY);
        }

        self.video.send_packet(&pkt)?;

        let mut decoded = ffmpeg::frame::Video::empty();
        match self.video.receive_frame(&mut decoded) {
            Ok(()) => {
                let scaler = self.scaler.get_or_insert_with(|| {
                    ffmpeg::software::scaling::Context::get(
                        decoded.format(),
                        decoded.width(),
                        decoded.height(),
                        ffmpeg::format::Pixel::BGRA,
                        self.width,
                        self.height,
                        ffmpeg::software::scaling::flag::Flags::BILINEAR,
                    )
                    .expect("failed to create scaler")
                });

                let mut bgra = ffmpeg::frame::Video::new(
                    ffmpeg::format::Pixel::BGRA,
                    self.width,
                    self.height,
                );
                scaler.run(&decoded, &mut bgra)?;

                let size = (self.width * self.height * 4) as usize;
                let mut data = vec![0u8; size];
                let src = bgra.data(0);
                let copy_len = src.len().min(size);
                data[..copy_len].copy_from_slice(&src[..copy_len]);

                Ok(DecodedFrame {
                    data,
                    width: self.width,
                    height: self.height,
                })
            }
            Err(Error::Other { errno: error::EAGAIN }) => {
                let size = (self.width * self.height * 4) as usize;
                Ok(DecodedFrame {
                    data: vec![0u8; size],
                    width: self.width,
                    height: self.height,
                })
            }
            Err(e) => anyhow::bail!("decode receive_frame: {e}"),
        }
    }
}

impl Decoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        #[cfg(feature = "ffmpeg")]
        {
            ffmpeg_next::init()?;
            let inner = FfmpegDecoder::new(width, height)?;
            tracing::info!("decoder: using libavcodec H.264 decoder");
            Ok(Self { inner })
        }

        #[cfg(not(feature = "ffmpeg"))]
        {
            tracing::info!("decoder: ffmpeg feature disabled, using scaffold (no-op)");
            Ok(Self { width, height })
        }
    }

    pub fn decode(&mut self, frame: &EncodedFrame) -> Result<DecodedFrame> {
        #[cfg(feature = "ffmpeg")]
        {
            self.inner.decode(frame)
        }

        #[cfg(not(feature = "ffmpeg"))]
        {
            let _ = frame;
            let size = (self.width * self.height * 4) as usize;
            Ok(DecodedFrame {
                data: vec![0u8; size],
                width: self.width,
                height: self.height,
            })
        }
    }
}
