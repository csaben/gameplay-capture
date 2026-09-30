//! Window capture: `trait FrameSource` plus per-platform backends.
//!
//! Every backend captures a single window, hands out GPU-resident frames where
//! the platform allows, and stamps each frame with `cap_clock` time. None of
//! them touch the game process.

use cap_types::Nanos;
use std::sync::{Arc, Mutex};

pub mod synthetic;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("window not found: {0}")]
    WindowNotFound(String),
    #[error("capture not supported on this platform/session: {0}")]
    Unsupported(String),
    #[error("user denied capture permission")]
    PermissionDenied,
    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, CaptureError>;

/// Which window to capture. Produced by `cap-focus` window enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowTarget {
    /// HWND (Windows), X11 window id (Linux/X11), CGWindowID (macOS).
    /// Ignored on Wayland, where the portal picker chooses the window.
    pub native_id: u64,
    pub title: String,
    pub game_id: String,
    pub pid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra8,
    Rgba8,
    /// Half-float RGBA (HDR on Windows); must be tone-mapped in the scale step.
    Rgba16f,
    Nv12,
}

/// A DMA-BUF plane exported by PipeWire (Linux).
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct DmaBufPlane {
    pub fd: std::os::fd::OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// Where the frame's pixels live.
pub enum FramePayload {
    /// CPU memory (X11 XShm, PipeWire SHM fallback, synthetic).
    Cpu { data: Vec<u8>, stride: usize },
    /// Linux DMA-BUF (fourcc + modifier describe the layout).
    #[cfg(target_os = "linux")]
    DmaBuf { fourcc: u32, modifier: u64, planes: Vec<DmaBufPlane> },
    /// Windows D3D11 texture from WGC. The device is the one the capture
    /// session was created on; the encoder must use the same device.
    #[cfg(windows)]
    D3D11 {
        texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    },
    /// macOS IOSurface-backed CVPixelBuffer (retained raw `CVPixelBufferRef`).
    #[cfg(target_os = "macos")]
    CvPixelBuffer(macos::RetainedPixelBuffer),
}

impl std::fmt::Debug for FramePayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FramePayload::Cpu { data, stride } => write!(f, "Cpu({} bytes, stride {stride})", data.len()),
            #[cfg(target_os = "linux")]
            FramePayload::DmaBuf { fourcc, modifier, planes } => {
                write!(f, "DmaBuf(fourcc {fourcc:#x}, modifier {modifier:#x}, {} planes)", planes.len())
            }
            #[cfg(windows)]
            FramePayload::D3D11 { .. } => write!(f, "D3D11"),
            #[cfg(target_os = "macos")]
            FramePayload::CvPixelBuffer(_) => write!(f, "CvPixelBuffer"),
        }
    }
}

// SAFETY: COM texture/device pointers are free-threaded for D3D11 (we only use
// them on the encode thread after handoff); CVPixelBuffer is refcounted and
// thread-safe.
unsafe impl Send for FramePayload {}
unsafe impl Sync for FramePayload {}

#[derive(Debug)]
pub struct CapturedFrame {
    /// Capture time on the `cap_clock` monotonic clock.
    pub capture_ns: Nanos,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub payload: FramePayload,
}

/// Single-slot "latest frame" mailbox. Capture callbacks overwrite it and never
/// block; the fixed-rate ticker samples it.
#[derive(Default)]
pub struct LatestFrame {
    inner: Mutex<(u64, Option<Arc<CapturedFrame>>)>,
}

impl LatestFrame {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    /// Called from capture callbacks.
    pub fn publish(&self, frame: CapturedFrame) {
        let mut g = self.inner.lock().unwrap();
        g.0 += 1;
        g.1 = Some(Arc::new(frame));
    }
    /// Returns (sequence number, frame). Sequence increases on every publish,
    /// so the ticker can tell repeated frames apart.
    pub fn latest(&self) -> (u64, Option<Arc<CapturedFrame>>) {
        let g = self.inner.lock().unwrap();
        (g.0, g.1.clone())
    }
}

#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub backend: &'static str,
    pub width: u32,
    pub height: u32,
    pub hdr: bool,
}

pub trait FrameSource: Send {
    /// Begin capturing `target`; frames are published into `sink`.
    fn start(&mut self, target: &WindowTarget, sink: Arc<LatestFrame>) -> Result<()>;
    fn stop(&mut self);
    fn info(&self) -> Option<SourceInfo>;
}

/// Pick the right backend for this OS / session.
pub fn default_source() -> Result<Box<dyn FrameSource>> {
    Err(CaptureError::Unsupported("no backend implemented yet".into()))
}

#[cfg(target_os = "macos")]
pub mod macos {
    /// Retained `CVPixelBufferRef`; released on drop.
    pub struct RetainedPixelBuffer(pub *mut std::ffi::c_void);
}
