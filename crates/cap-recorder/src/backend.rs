//! Encoder / muxer abstraction so the recorder can run (and be tested) without
//! a hardware encoder. The real implementation wraps `cap_encode`.

use cap_capture::CapturedFrame;
use cap_encode::{EncodedPacket, Encoder, EncoderConfig, SegmentMuxer, StreamParams};
use std::path::Path;

pub use cap_encode::Result as EncResult;

/// A continuously running video encoder. `pts` values are global (they keep
/// increasing across segments); the recorder forces a keyframe at the first
/// frame of every segment.
pub trait FrameEncoder: Send {
    fn params(&self) -> StreamParams;
    fn encode(&mut self, frame: &CapturedFrame, pts: i64, force_keyframe: bool) -> EncResult<Vec<EncodedPacket>>;
    /// Drain all buffered packets. Afterwards the encoder must still accept new
    /// frames (implementations may re-open internally); the next frame the
    /// recorder sends after a flush is always a forced keyframe.
    fn flush(&mut self) -> EncResult<Vec<EncodedPacket>>;
}

/// Writes one segment's `video.mp4`.
pub trait SegmentSink: Send {
    fn write(&mut self, pkt: &EncodedPacket) -> EncResult<()>;
    fn finish(self: Box<Self>) -> EncResult<()>;
}

/// Creates a `SegmentSink` per segment. `pts_offset` is subtracted from packet
/// timestamps so each file starts at 0.
pub trait SinkFactory: Send {
    fn create(&self, path: &Path, params: &StreamParams, pts_offset: i64) -> EncResult<Box<dyn SegmentSink>>;
}

/// `cap_encode::Encoder`, re-opened lazily after a flush (FFmpeg encoders can't
/// continue after being drained).
pub struct FfmpegEncoder {
    cfg: EncoderConfig,
    inner: Option<Encoder>,
    params: StreamParams,
}

impl FfmpegEncoder {
    /// Probes and opens the hardware encoder now, so failures surface before recording.
    pub fn open(cfg: &EncoderConfig) -> EncResult<Self> {
        let inner = Encoder::open(cfg)?;
        let params = inner.params().clone();
        Ok(Self { cfg: cfg.clone(), inner: Some(inner), params })
    }
}

impl FrameEncoder for FfmpegEncoder {
    fn params(&self) -> StreamParams {
        self.params.clone()
    }
    fn encode(&mut self, frame: &CapturedFrame, pts: i64, force_keyframe: bool) -> EncResult<Vec<EncodedPacket>> {
        if self.inner.is_none() {
            let mut cfg = self.cfg.clone();
            // Pin the re-opened encoder to the one we probed first.
            cfg.force_encoder = Some(self.params.encoder_name.clone());
            self.inner = Some(Encoder::open(&cfg)?);
        }
        self.inner.as_mut().unwrap().encode(frame, pts, force_keyframe)
    }
    fn flush(&mut self) -> EncResult<Vec<EncodedPacket>> {
        match self.inner.take() {
            Some(mut e) => e.flush(),
            None => Ok(Vec::new()),
        }
    }
}

/// `cap_encode::SegmentMuxer` (fragmented MP4).
#[derive(Default)]
pub struct FfmpegSinkFactory;

struct FfmpegSink(SegmentMuxer);

impl SegmentSink for FfmpegSink {
    fn write(&mut self, pkt: &EncodedPacket) -> EncResult<()> {
        self.0.write(pkt)
    }
    fn finish(self: Box<Self>) -> EncResult<()> {
        self.0.finish()
    }
}

impl SinkFactory for FfmpegSinkFactory {
    fn create(&self, path: &Path, params: &StreamParams, pts_offset: i64) -> EncResult<Box<dyn SegmentSink>> {
        Ok(Box::new(FfmpegSink(SegmentMuxer::create(path, params, pts_offset)?)))
    }
}
