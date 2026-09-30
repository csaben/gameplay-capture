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
    /// Open whatever `encode(frame)` would open lazily, ahead of time, so the
    /// first encode of a segment doesn't stall the queue (and drop frames).
    fn prepare(&mut self, _frame: &CapturedFrame) -> EncResult<()> {
        Ok(())
    }
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
///
/// Windows: WGC frames live on the capture session's D3D11 device, and the
/// encoder must run on that same device. So on Windows [`FfmpegEncoder::open`]
/// only *probes* (opens and drops an encoder, so "no hardware encoder" still
/// fails up front) and the real encoder is opened on the first frame with
/// `Encoder::open_with_d3d11_device(frame's device)` when the payload is D3D11
/// (plain `Encoder::open` otherwise). Other platforms open eagerly.
/// Re-opens (after a flush) are pinned to the probed encoder name and follow
/// the same rule.
pub struct FfmpegEncoder {
    cfg: EncoderConfig,
    inner: Option<Encoder>,
    params: StreamParams,
}

impl FfmpegEncoder {
    /// Probes and opens the hardware encoder now, so failures surface before recording.
    pub fn open(cfg: &EncoderConfig) -> EncResult<Self> {
        let probe = Encoder::open(cfg)?;
        let params = probe.params().clone();
        // Pin later (re-)opens to the encoder the probe picked.
        let mut cfg = cfg.clone();
        cfg.force_encoder = Some(params.encoder_name.clone());
        let inner = if cfg!(windows) {
            drop(probe); // reopened on the first frame's D3D11 device
            None
        } else {
            Some(probe)
        };
        Ok(Self { cfg, inner, params })
    }

    fn open_for(&self, frame: &CapturedFrame) -> EncResult<Encoder> {
        #[cfg(windows)]
        if let cap_capture::FramePayload::D3D11 { device, .. } = &frame.payload {
            return Encoder::open_with_d3d11_device(&self.cfg, device);
        }
        let _ = frame;
        Encoder::open(&self.cfg)
    }
}

impl FrameEncoder for FfmpegEncoder {
    fn params(&self) -> StreamParams {
        self.params.clone()
    }
    fn encode(&mut self, frame: &CapturedFrame, pts: i64, force_keyframe: bool) -> EncResult<Vec<EncodedPacket>> {
        self.prepare(frame)?;
        self.inner.as_mut().unwrap().encode(frame, pts, force_keyframe)
    }
    fn prepare(&mut self, frame: &CapturedFrame) -> EncResult<()> {
        if self.inner.is_none() {
            let enc = self.open_for(frame)?;
            if enc.params().extradata != self.params.extradata {
                tracing::debug!("re-opened encoder has different extradata; using the new parameters");
            }
            self.params = enc.params().clone();
            self.inner = Some(enc);
        }
        Ok(())
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
