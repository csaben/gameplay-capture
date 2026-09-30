//! GPU scale + colour convert, hardware encode, and fragmented-MP4 muxing.
//!
//! The encoder runs continuously across segments; the recorder forces a
//! keyframe at each segment boundary and opens a new `SegmentMuxer`.

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
    _private: (),
}

impl Encoder {
    /// Probe hardware encoders in platform order and open the first that works.
    /// Refuses (never falls back to software) unless `EncoderConfig::force_encoder`
    /// names a software encoder explicitly.
    pub fn open(_cfg: &EncoderConfig) -> Result<Self> {
        unimplemented!()
    }
    pub fn params(&self) -> &StreamParams {
        unimplemented!()
    }
    /// Scale/convert and encode one frame. `pts` is the output frame index.
    /// Returns zero or more packets (no B-frames, so normally exactly one).
    pub fn encode(&mut self, _frame: &CapturedFrame, _pts: i64, _force_keyframe: bool) -> Result<Vec<EncodedPacket>> {
        unimplemented!()
    }
    pub fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        unimplemented!()
    }
}

/// Writes one segment's `video.mp4` as fragmented MP4
/// (`movflags=frag_keyframe+empty_moov`) so a crash leaves a playable file.
pub struct SegmentMuxer {
    _private: (),
}

impl SegmentMuxer {
    /// `pts_offset` is subtracted from packet timestamps so each file starts at 0.
    pub fn create(_path: &Path, _params: &StreamParams, _pts_offset: i64) -> Result<Self> {
        unimplemented!()
    }
    pub fn write(&mut self, _pkt: &EncodedPacket) -> Result<()> {
        unimplemented!()
    }
    pub fn finish(self) -> Result<()> {
        unimplemented!()
    }
}

/// GPU name for the manifest (best effort).
pub fn gpu_name() -> String {
    "unknown".into()
}

#[allow(dead_code)]
fn _assert_send() {
    fn s<T: Send>() {}
    let _ = s::<EncodedPacket>;
    let _: Nanos = 0;
}
