//! Window capture: `trait FrameSource` plus per-platform backends.
//!
//! Every backend captures a single window, hands out GPU-resident frames where
//! the platform allows, and stamps each frame with `cap_clock` time. None of
//! them touch the game process.

use cap_types::Nanos;
use std::sync::{Arc, Mutex};

pub mod synthetic;

#[cfg(windows)]
pub mod win_wgc;

#[cfg(target_os = "linux")]
pub mod linux_x11;

/// XDG ScreenCast portal + PipeWire (Wayland). The portal half is behind the
/// `portal` feature, the PipeWire stream behind `pipewire`.
#[cfg(all(target_os = "linux", feature = "portal"))]
pub mod linux_pipewire;

#[cfg(target_os = "macos")]
pub mod mac_sck;

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

/// Which backend `default_source()` would pick in this process, without
/// creating it. Useful for logs and `--help` style diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    WindowsGraphicsCapture,
    PipeWire,
    X11,
    ScreenCaptureKit,
}

/// Pure selection logic for Linux, split out so it can be unit-tested.
/// `pipewire_built` is whether the `pipewire` feature is compiled in.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn pick_linux_backend(
    pipewire_built: bool,
    wayland_display: Option<&str>,
    display: Option<&str>,
) -> Result<BackendKind> {
    let set = |v: Option<&str>| v.is_some_and(|s| !s.is_empty());
    if pipewire_built && set(wayland_display) {
        return Ok(BackendKind::PipeWire);
    }
    if set(display) {
        return Ok(BackendKind::X11);
    }
    if set(wayland_display) {
        return Err(CaptureError::Unsupported(
            "Wayland session without an X server; rebuild cap-capture with the `pipewire` feature".into(),
        ));
    }
    Err(CaptureError::Unsupported("neither WAYLAND_DISPLAY nor DISPLAY is set".into()))
}

/// The backend `default_source()` selects for this OS / session.
pub fn default_backend() -> Result<BackendKind> {
    #[cfg(windows)]
    {
        Ok(BackendKind::WindowsGraphicsCapture)
    }
    #[cfg(target_os = "macos")]
    {
        Ok(BackendKind::ScreenCaptureKit)
    }
    #[cfg(target_os = "linux")]
    {
        let w = std::env::var("WAYLAND_DISPLAY").ok();
        let d = std::env::var("DISPLAY").ok();
        pick_linux_backend(cfg!(feature = "pipewire"), w.as_deref(), d.as_deref())
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Err(CaptureError::Unsupported(std::env::consts::OS.into()))
    }
}

/// Pick the right backend for this OS / session:
/// Windows -> WGC; macOS -> ScreenCaptureKit; Linux -> PipeWire if built with
/// the `pipewire` feature and `WAYLAND_DISPLAY` is set, else X11 if `DISPLAY`
/// is set, else `Unsupported`. Backends are created with default config; build
/// them directly (e.g. `linux_x11::X11Source::new(cfg)`) to customise.
pub fn default_source() -> Result<Box<dyn FrameSource>> {
    match default_backend()? {
        #[cfg(windows)]
        BackendKind::WindowsGraphicsCapture => Ok(Box::new(win_wgc::WgcSource::new(Default::default()))),
        #[cfg(target_os = "macos")]
        BackendKind::ScreenCaptureKit => Ok(Box::new(mac_sck::SckSource::new(Default::default()))),
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        BackendKind::PipeWire => Ok(Box::new(linux_pipewire::PipeWireSource::new(Default::default()))),
        #[cfg(target_os = "linux")]
        BackendKind::X11 => Ok(Box::new(linux_x11::X11Source::new(Default::default()))),
        #[allow(unreachable_patterns)]
        other => Err(CaptureError::Unsupported(format!("{other:?} not built for this target"))),
    }
}

#[cfg(target_os = "macos")]
pub mod macos {
    use std::ffi::c_void;

    #[link(name = "CoreVideo", kind = "framework")]
    extern "C" {
        fn CVPixelBufferRetain(buffer: *mut c_void) -> *mut c_void;
        fn CVPixelBufferRelease(buffer: *mut c_void);
        fn CVPixelBufferGetWidth(buffer: *mut c_void) -> usize;
        fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
        fn CVPixelBufferGetPixelFormatType(buffer: *mut c_void) -> u32;
    }

    /// Retained `CVPixelBufferRef`; released on drop.
    ///
    /// The field is the raw `CVPixelBufferRef`. Whoever constructs this with
    /// the tuple constructor transfers one +1 reference to it.
    pub struct RetainedPixelBuffer(pub *mut c_void);

    impl RetainedPixelBuffer {
        /// Take an extra reference on `buffer` (which the caller keeps owning).
        ///
        /// # Safety
        /// `buffer` must be a valid, non-null `CVPixelBufferRef`.
        pub unsafe fn retain(buffer: *mut c_void) -> Self {
            Self(CVPixelBufferRetain(buffer))
        }
        /// The raw `CVPixelBufferRef` (borrowed; still owned by `self`).
        pub fn as_ptr(&self) -> *mut c_void {
            self.0
        }
        pub fn width(&self) -> usize {
            // SAFETY: self.0 is a live retained CVPixelBufferRef.
            unsafe { CVPixelBufferGetWidth(self.0) }
        }
        pub fn height(&self) -> usize {
            // SAFETY: as above.
            unsafe { CVPixelBufferGetHeight(self.0) }
        }
        /// CoreVideo FourCC, e.g. `'BGRA'` or `'420v'`.
        pub fn pixel_format(&self) -> u32 {
            // SAFETY: as above.
            unsafe { CVPixelBufferGetPixelFormatType(self.0) }
        }
    }

    impl Clone for RetainedPixelBuffer {
        fn clone(&self) -> Self {
            // SAFETY: self.0 is live; retain returns the same pointer +1.
            unsafe { Self::retain(self.0) }
        }
    }

    impl Drop for RetainedPixelBuffer {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: we own exactly one reference.
                unsafe { CVPixelBufferRelease(self.0) };
            }
        }
    }

    // SAFETY: CVPixelBuffer retain/release are thread-safe.
    unsafe impl Send for RetainedPixelBuffer {}
    unsafe impl Sync for RetainedPixelBuffer {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_backend_selection() {
        use BackendKind::*;
        assert_eq!(pick_linux_backend(true, Some("wayland-0"), Some(":0")).unwrap(), PipeWire);
        assert_eq!(pick_linux_backend(false, Some("wayland-0"), Some(":0")).unwrap(), X11);
        assert_eq!(pick_linux_backend(true, None, Some(":0")).unwrap(), X11);
        assert_eq!(pick_linux_backend(true, Some(""), Some(":1")).unwrap(), X11);
        assert!(matches!(pick_linux_backend(false, Some("wayland-0"), None), Err(CaptureError::Unsupported(_))));
        assert!(matches!(pick_linux_backend(true, None, None), Err(CaptureError::Unsupported(_))));
    }

    #[test]
    fn latest_frame_sequence() {
        let l = LatestFrame::new();
        assert_eq!(l.latest().0, 0);
        l.publish(CapturedFrame {
            capture_ns: 5,
            width: 1,
            height: 1,
            format: PixelFormat::Bgra8,
            payload: FramePayload::Cpu { data: vec![0; 4], stride: 4 },
        });
        let (seq, f) = l.latest();
        assert_eq!(seq, 1);
        assert_eq!(f.unwrap().capture_ns, 5);
    }
}
