//! GPU scale + colour convert, hardware encode, and fragmented-MP4 muxing.
//!
//! The encoder runs continuously across segments; the recorder forces a
//! keyframe at each segment boundary and opens a new `SegmentMuxer`.
//!
//! Hardware encoders are configured for zero output delay (NVENC `delay=0`,
//! `zerolatency`), so `encode` normally returns the packet for the frame it
//! was given. Callers should still route packets to segments by `pkt.pts`
//! (not by call order); `SegmentMuxer::write` rejects packets that precede
//! its `pts_offset`.
//!
//! All FFmpeg / D3D11 / VideoToolbox FFI lives in the private `ffi` module.
//! See `README.md` in this crate for platform status and build notes.

mod ffi;
mod gpu;
mod pipeline;
pub mod probe;

use cap_capture::CapturedFrame;
use cap_types::Nanos;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("no working hardware encoder (tried: {0})")]
    NoHardwareEncoder(String),
    #[error("unsupported frame payload for encoder {0}")]
    UnsupportedPayload(String),
    #[error("ffmpeg: {0}")]
    Ffmpeg(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, EncodeError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    Av1,
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub rate_hz: u32,
    pub codec: Codec,
    /// Constant QP (ignored in lossless mode).
    pub qp: u32,
    pub lossless: bool,
    /// Keyframe interval in frames (default = rate_hz, i.e. 1 s).
    pub gop: u32,
    /// Force a specific FFmpeg encoder name (e.g. "hevc_nvenc").
    pub force_encoder: Option<String>,
}

impl EncoderConfig {
    pub fn new(width: u32, height: u32, rate_hz: u32) -> Self {
        Self { width, height, rate_hz, codec: Codec::Hevc, qp: 19, lossless: false, gop: rate_hz, force_encoder: None, }
    }
}

/// One compressed access unit. `pts`/`dts` are in frame units (time base 1/rate_hz).
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub pts: i64,
    pub dts: i64,
    pub keyframe: bool,
}

/// Parameters the muxer needs (codec extradata etc.) and the manifest records.
#[derive(Debug, Clone)]
pub struct StreamParams {
    pub encoder_name: String,
    pub width: u32,
    pub height: u32,
    pub rate_hz: u32,
    pub codec: Codec,
    pub extradata: Vec<u8>,
    /// Goes into `manifest.encoder_params`.
    pub params: BTreeMap<String, String>,
}

pub struct Encoder {
    inner: pipeline::Inner,
}

impl Encoder {
    /// Probe hardware encoders in platform order and open the first that works.
    /// Refuses (never falls back to software) unless `EncoderConfig::force_encoder`
    /// names a software encoder explicitly.
    pub fn open(cfg: &EncoderConfig) -> Result<Self> {
        Ok(Self { inner: pipeline::open(cfg, None)? })
    }

    /// Windows: open on the capture session's D3D11 device (preferred over
    /// `open`, which creates its own device and reopens on the first frame
    /// that comes from a different device).
    #[cfg(windows)]
    pub fn open_with_d3d11_device(
        cfg: &EncoderConfig,
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    ) -> Result<Self> {
        Ok(Self { inner: pipeline::open(cfg, Some(pipeline::PlatformDevice::D3D11(device.clone())))? })
    }

    pub fn params(&self) -> &StreamParams {
        &self.inner.params
    }

    /// Which scale/convert path the last frame took (e.g.
    /// `"gpu:hwupload+scale_cuda"`, `"cpu:swscale+hwupload"`). For logs/tests.
    pub fn last_frame_path(&self) -> Option<&'static str> {
        self.inner.last_path.map(|p| p.as_str())
    }

    /// Scale/convert and encode one frame. `pts` is the output frame index.
    /// Returns zero or more packets (no B-frames, so normally exactly one).
    pub fn encode(&mut self, frame: &CapturedFrame, pts: i64, force_keyframe: bool) -> Result<Vec<EncodedPacket>> {
        self.inner.encode(frame, pts, force_keyframe)
    }

    /// Drain the encoder. Encoding may continue afterwards (the encoder is
    /// transparently reopened with identical settings).
    pub fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        self.inner.flush()
    }
}

/// Writes one segment's `video.mp4` as fragmented MP4
/// (`movflags=frag_keyframe+empty_moov`) so a crash leaves a playable file.
pub struct SegmentMuxer {
    w: ffi::Mp4Writer,
    pts_offset: i64,
    last_dts: Option<i64>,
    frames: u64,
}

/// movflags used for every segment.
pub const MOVFLAGS: &str = "frag_keyframe+empty_moov+default_base_moof";

impl SegmentMuxer {
    /// `pts_offset` is subtracted from packet timestamps so each file starts at 0.
    pub fn create(path: &Path, params: &StreamParams, pts_offset: i64) -> Result<Self> {
        ffi::init();
        let codec = match params.codec {
            Codec::Hevc => ffi::MuxCodec::Hevc,
            Codec::Av1 => ffi::MuxCodec::Av1,
        };
        let w = ffi::Mp4Writer::create(path, codec, params.width, params.height, params.rate_hz, &params.extradata, MOVFLAGS)?;
        Ok(Self { w, pts_offset, last_dts: None, frames: 0 })
    }

    pub fn write(&mut self, pkt: &EncodedPacket) -> Result<()> {
        let pts = pkt.pts - self.pts_offset;
        let dts = pkt.dts - self.pts_offset;
        if pts < 0 || dts < 0 {
            return Err(EncodeError::Ffmpeg(format!(
                "packet pts {} / dts {} precedes segment start {}",
                pkt.pts, pkt.dts, self.pts_offset
            )));
        }
        if self.frames == 0 && !pkt.keyframe {
            tracing::warn!(pts = pkt.pts, "segment does not start with a keyframe");
        }
        if let Some(l) = self.last_dts {
            if dts <= l {
                return Err(EncodeError::Ffmpeg(format!("non-increasing dts {dts} after {l}")));
            }
        }
        self.w.write(&pkt.data, pts, dts, pkt.keyframe, 1)?;
        self.last_dts = Some(dts);
        self.frames += 1;
        Ok(())
    }

    /// Packets written so far.
    pub fn frames_written(&self) -> u64 {
        self.frames
    }

    pub fn finish(mut self) -> Result<()> {
        self.w.finish()
    }
}

/// GPU name for the manifest (best effort).
pub fn gpu_name() -> String {
    gpu::gpu_name()
}

#[allow(dead_code)]
fn _assert_send() {
    fn s<T: Send>() {}
    let _ = s::<EncodedPacket>;
    let _ = s::<Encoder>;
    let _ = s::<SegmentMuxer>;
    let _: Nanos = 0;
}
