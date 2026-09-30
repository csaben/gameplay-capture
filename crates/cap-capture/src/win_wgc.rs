//! Windows Graphics Capture (WGC) of a single HWND.
//!
//! - We create our own D3D11 device (BGRA + VIDEO support, multithread
//!   protected) so the encoder can run the D3D11 video processor on the same
//!   device; `WgcSource::device()` exposes it, and every frame carries it too.
//!   A caller-supplied device can be passed in `WgcConfig::device` instead.
//! - `GraphicsCaptureItem` comes from the HWND via `IGraphicsCaptureItemInterop`
//!   (no contact with the game process; DWM hands us its composition surface).
//! - The frame pool is free-threaded: `FrameArrived` fires on a system worker
//!   thread. The handler copies the pool surface into a texture we own
//!   (`CopyResource`, or `CopySubresourceRegion` while a resize is in flight)
//!   and closes the WGC frame immediately, so pool buffers are never held by
//!   the recorder. Then it calls `LatestFrame::publish` and returns; it never
//!   waits on the consumer.
//! - Owned textures are recycled once every `Arc<CapturedFrame>` that
//!   referenced them is gone (COM refcount back to the pool's single ref).
//!   All D3D11 work is on the immediate context of a multithread-protected
//!   device, so the encoder's reads of a texture (issued before it drops the
//!   frame) are ordered before our next copy into it.
//! - Timestamps: `Direct3D11CaptureFrame::SystemRelativeTime` is QPC time
//!   expressed in 100 ns units (`QPC * 10^7 / QPF`). `cap_clock::now_ns()` on
//!   Windows is `QPC * 10^9 / QPF`, so `ns = SystemRelativeTime * 100` is on
//!   the same base (truncation error < 100 ns).
//! - Windows 11: `IsBorderRequired = false` after requesting
//!   `GraphicsCaptureAccessKind::Borderless` (failures ignored on Windows 10);
//!   the cursor is never captured.

use crate::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows::core::{factory, IInspectable, Interface};
use windows::Foundation::{EventRegistrationToken, TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind, GraphicsCaptureItem,
    GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1, IDXGIOutput6};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

/// Pixel format of the capture pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HdrMode {
    /// Always BGRA8. On an HDR monitor DWM tone-maps/clips to SDR for us.
    #[default]
    Sdr,
    /// Always FP16 scRGB (`PixelFormat::Rgba16f`); the encoder must tone-map.
    Hdr,
    /// FP16 if the window's monitor is in HDR (PQ / BT.2020) mode, else BGRA8.
    Auto,
}

#[derive(Clone, Default)]
pub struct WgcConfig {
    pub hdr: HdrMode,
    /// Frame pool depth (WGC buffers). 2 is enough since we copy out at once.
    pub buffers: i32,
    /// Windows 11 24H2+: minimum interval between frames (reduces DWM work for
    /// high-refresh games when we only need ~20 Hz). `None` = every frame.
    pub min_update_interval: Option<std::time::Duration>,
    /// Use this device instead of creating one. It must have been created with
    /// `D3D11_CREATE_DEVICE_BGRA_SUPPORT`; we enable multithread protection on it.
    pub device: Option<ID3D11Device>,
}

impl std::fmt::Debug for WgcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgcConfig")
            .field("hdr", &self.hdr)
            .field("buffers", &self.buffers)
            .field("min_update_interval", &self.min_update_interval)
            .field("device", &self.device.is_some())
            .finish()
    }
}

/// Counters readable while capture runs.
#[derive(Debug, Default)]
pub struct WgcStats {
    /// Frames published.
    pub frames: AtomicU64,
    /// Frames WGC delivered that we dropped because a newer one was queued.
    pub superseded: AtomicU64,
    /// Handler errors (logged, capture continues).
    pub errors: AtomicU64,
    /// Pool recreations due to window resize.
    pub resizes: AtomicU64,
}

/// `SystemRelativeTime` (100 ns units on the QPC base) -> `cap_clock` ns.
pub fn system_relative_time_to_ns(t: TimeSpan) -> Nanos {
    t.Duration.saturating_mul(100)
}

fn werr(what: &str) -> impl Fn(windows::core::Error) -> CaptureError + '_ {
    move |e| CaptureError::Backend(format!("{what}: {e}"))
}

/// Create a D3D11 device suitable for capture + the D3D11 video processor.
pub fn create_device() -> Result<ID3D11Device> {
    let mut device = None;
    // SAFETY: out-pointers are valid for the call.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .map_err(werr("D3D11CreateDevice"))?;
    }
    device.ok_or_else(|| CaptureError::Backend("D3D11CreateDevice returned no device".into()))
}

/// Is the monitor showing `hwnd` in HDR (PQ, BT.2020) mode?
pub fn window_on_hdr_monitor(hwnd: HWND) -> bool {
    // SAFETY: plain DXGI enumeration; all out-values are owned wrappers.
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return false };
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                if let Ok(desc) = output.GetDesc() {
                    if desc.Monitor == mon {
                        return output
                            .cast::<IDXGIOutput6>()
                            .and_then(|o6| o6.GetDesc1())
                            .is_ok_and(|d| d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020);
                    }
                }
                o += 1;
            }
            a += 1;
        }
        false
    }
}

/// Current COM refcount of `obj` (AddRef + Release; exact for D3D11 resources
/// in practice, which is all we need to tell "only the pool holds it").
fn com_refcount<I: Interface>(obj: &I) -> u32 {
    // SAFETY: every COM object starts with a vtable pointer whose first three
    // entries are the IUnknown methods.
    unsafe {
        let raw = obj.as_raw();
        let vt = *(raw as *const *const windows::core::IUnknown_Vtbl);
        ((*vt).AddRef)(raw);
        ((*vt).Release)(raw)
    }
}

/// Maximum owned textures kept for reuse.
const TEXTURE_POOL_MAX: usize = 8;

/// State used by the FrameArrived handler (lives behind a Mutex).
struct FrameCtx {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    d3d_device: IDirect3DDevice,
    sink: Arc<LatestFrame>,
    pool_format: DirectXPixelFormat,
    out_format: PixelFormat,
    buffers: i32,
    last_size: SizeInt32,
    textures: Vec<ID3D11Texture2D>,
    info: Arc<Mutex<Option<SourceInfo>>>,
    stats: Arc<WgcStats>,
}

// SAFETY: the device is multithread-protected, so its immediate context may be
// used from the WGC worker thread; all other members are agile WinRT objects
// or Send + Sync.
unsafe impl Send for FrameCtx {}

impl FrameCtx {
    fn acquire_texture(&mut self, w: u32, h: u32, fmt: DXGI_FORMAT) -> windows::core::Result<ID3D11Texture2D> {
        let matches = |t: &ID3D11Texture2D| {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: valid out-pointer.
            unsafe { t.GetDesc(&mut d) };
            d.Width == w && d.Height == h && d.Format == fmt
        };
        // Forget textures of an old size; they are freed once consumers drop them.
        self.textures.retain(|t| matches(t));
        if let Some(t) = self.textures.iter().find(|t| com_refcount(*t) == 1) {
            return Ok(t.clone());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: fmt,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        // SAFETY: valid descriptor and out-pointer.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        let tex = tex.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER))?;
        if self.textures.len() < TEXTURE_POOL_MAX {
            self.textures.push(tex.clone());
        }
        Ok(tex)
    }

    fn on_frame_arrived(&mut self, pool: &Direct3D11CaptureFramePool) -> windows::core::Result<()> {
        // Take the newest queued frame; release older ones straight back to WGC.
        let mut frame = None;
        while let Ok(f) = pool.TryGetNextFrame() {
            if let Some(old) = frame.replace(f) {
                let _ = old.Close();
                self.stats.superseded.fetch_add(1, Ordering::Relaxed);
            }
        }
        let Some(frame) = frame else { return Ok(()) };

        let capture_ns = system_relative_time_to_ns(frame.SystemRelativeTime()?);
        let content = frame.ContentSize()?;
        if content.Width <= 0 || content.Height <= 0 {
            // Minimised window.
            frame.Close()?;
            return Ok(());
        }
        if content != self.last_size {
            // Window resized: new pool buffers at the new size. This frame is
            // still at the old buffer size; copy the valid region below.
            pool.Recreate(&self.d3d_device, self.pool_format, self.buffers, content)?;
            self.last_size = content;
            self.stats.resizes.fetch_add(1, Ordering::Relaxed);
        }

        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        // SAFETY: WGC surfaces are backed by ID3D11Texture2D.
        let src: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let mut sdesc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid out-pointer.
        unsafe { src.GetDesc(&mut sdesc) };
        let w = (content.Width as u32).min(sdesc.Width);
        let h = (content.Height as u32).min(sdesc.Height);

        let dst = self.acquire_texture(w, h, sdesc.Format)?;
        // SAFETY: both textures live on self.device; formats match.
        unsafe {
            if w == sdesc.Width && h == sdesc.Height {
                self.context.CopyResource(&dst, &src);
            } else {
                let b = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
                self.context.CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, 0, Some(&b));
            }
        }
        drop(src);
        drop(access);
        // Return the pool buffer now; the copy is already queued on the context.
        frame.Close()?;

        self.sink.publish(CapturedFrame {
            capture_ns,
            width: w,
            height: h,
            format: self.out_format,
            payload: FramePayload::D3D11 { texture: dst, device: self.device.clone() },
        });
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        if let Some(i) = self.info.lock().unwrap().as_mut() {
            i.width = w;
            i.height = h;
        }
        Ok(())
    }
}

struct Running {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    frame_token: EventRegistrationToken,
    closed_token: EventRegistrationToken,
}

/// WGC single-window capture backend.
pub struct WgcSource {
    cfg: WgcConfig,
    device: Option<ID3D11Device>,
    running: Option<Running>,
    info: Arc<Mutex<Option<SourceInfo>>>,
    stats: Arc<WgcStats>,
    closed: Arc<AtomicBool>,
}

// SAFETY: WinRT capture objects are agile; the D3D11 device is free-threaded.
unsafe impl Send for WgcSource {}

impl WgcSource {
    pub fn new(cfg: WgcConfig) -> Self {
        Self {
            cfg,
            device: None,
            running: None,
            info: Arc::new(Mutex::new(None)),
            stats: Arc::new(WgcStats::default()),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The D3D11 device frames are delivered on (created lazily; call before
    /// `start` to initialise the encoder early).
    pub fn device(&mut self) -> Result<ID3D11Device> {
        if let Some(d) = &self.device {
            return Ok(d.clone());
        }
        let d = match &self.cfg.device {
            Some(d) => d.clone(),
            None => create_device()?,
        };
        // The WGC worker thread and the encoder thread share the immediate context.
        if let Ok(mt) = d.cast::<ID3D11Multithread>() {
            // SAFETY: plain setter.
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }
        self.device = Some(d.clone());
        Ok(d)
    }

    pub fn stats(&self) -> &WgcStats {
        &self.stats
    }

    /// True once the captured window has closed (WGC `Closed` event).
    pub fn window_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

impl Drop for WgcSource {
    fn drop(&mut self) {
        self.stop();
    }
}

impl FrameSource for WgcSource {
    fn start(&mut self, target: &WindowTarget, sink: Arc<LatestFrame>) -> Result<()> {
        self.stop();
        // SAFETY: may already be initialised (S_FALSE / RPC_E_CHANGED_MODE); either is fine.
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };

        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(CaptureError::Unsupported("Windows Graphics Capture needs Windows 10 1903+".into()));
        }
        let hwnd = HWND(target.native_id as usize as *mut std::ffi::c_void);
        // SAFETY: IsWindow accepts any value.
        if !unsafe { IsWindow(hwnd) }.as_bool() {
            return Err(CaptureError::WindowNotFound(format!("HWND {:#x}", target.native_id)));
        }

        let device = self.device()?;
        // SAFETY: plain getter.
        let context = unsafe { device.GetImmediateContext() }.map_err(werr("GetImmediateContext"))?;
        let dxgi: IDXGIDevice = device.cast().map_err(werr("IDXGIDevice"))?;
        // SAFETY: dxgi is a valid DXGI device.
        let d3d_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|i: IInspectable| i.cast())
            .map_err(werr("CreateDirect3D11DeviceFromDXGIDevice"))?;

        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(werr("capture interop"))?;
        // SAFETY: hwnd was validated above.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd) }
            .map_err(|e| CaptureError::WindowNotFound(format!("HWND {:#x}: {e}", target.native_id)))?;
        let size = item.Size().map_err(werr("item size"))?;

        let hdr = match self.cfg.hdr {
            HdrMode::Sdr => false,
            HdrMode::Hdr => true,
            HdrMode::Auto => window_on_hdr_monitor(hwnd),
        };
        let (pool_format, out_format) = if hdr {
            (DirectXPixelFormat::R16G16B16A16Float, PixelFormat::Rgba16f)
        } else {
            (DirectXPixelFormat::B8G8R8A8UIntNormalized, PixelFormat::Bgra8)
        };
        let buffers = if self.cfg.buffers > 0 { self.cfg.buffers } else { 2 };

        // Ask for borderless capture (Windows 11; unpackaged apps are granted
        // without a prompt). Fails on Windows 10; then the yellow border stays.
        // Note: blocks briefly; call `start` from a non-UI (MTA) thread.
        if let Err(e) =
            GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless).and_then(|op| op.get())
        {
            tracing::debug!("borderless capture access not granted: {e}");
        }

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d_device, pool_format, buffers, size)
            .map_err(werr("CreateFreeThreaded"))?;
        let session = pool.CreateCaptureSession(&item).map_err(werr("CreateCaptureSession"))?;
        let _ = session.SetIsCursorCaptureEnabled(false);
        if let Err(e) = session.SetIsBorderRequired(false) {
            tracing::debug!("IsBorderRequired=false not available: {e}");
        }
        if let Some(iv) = self.cfg.min_update_interval {
            let _ = session.SetMinUpdateInterval(TimeSpan { Duration: (iv.as_nanos() / 100) as i64 });
        }

        *self.info.lock().unwrap() =
            Some(SourceInfo { backend: "wgc", width: size.Width as u32, height: size.Height as u32, hdr });
        self.closed.store(false, Ordering::SeqCst);

        let ctx = Mutex::new(FrameCtx {
            device: device.clone(),
            context,
            d3d_device,
            sink,
            pool_format,
            out_format,
            buffers,
            last_size: size,
            textures: Vec::new(),
            info: self.info.clone(),
            stats: self.stats.clone(),
        });
        let stats = self.stats.clone();
        let frame_token = pool
            .FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
                move |pool, _| {
                    if let Some(pool) = pool {
                        let r = ctx.lock().unwrap().on_frame_arrived(pool);
                        if let Err(e) = r {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!("WGC frame handler: {e}");
                        }
                    }
                    Ok(())
                },
            ))
            .map_err(werr("FrameArrived"))?;
        let closed = self.closed.clone();
        let closed_token = item
            .Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                tracing::info!("captured window closed");
                closed.store(true, Ordering::SeqCst);
                Ok(())
            }))
            .map_err(werr("Closed"))?;

        session.StartCapture().map_err(werr("StartCapture"))?;
        self.running = Some(Running { pool, session, item, frame_token, closed_token });
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            let _ = r.pool.RemoveFrameArrived(r.frame_token);
            let _ = r.item.RemoveClosed(r.closed_token);
            let _ = r.session.Close();
            let _ = r.pool.Close();
        }
    }

    fn info(&self) -> Option<SourceInfo> {
        self.info.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_is_100ns_units() {
        assert_eq!(system_relative_time_to_ns(TimeSpan { Duration: 12_345 }), 1_234_500);
        // Same base as cap_clock: a fresh "now" in 100ns units maps back within 100 ns + call gap.
        let now = cap_clock::now_ns();
        let ts = TimeSpan { Duration: now / 100 };
        assert!((now - system_relative_time_to_ns(ts)) < 100);
    }
}
