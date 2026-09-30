//! Windows: FFmpeg D3D11VA device/frames glue around the WGC capture device.
//!
//! UNTESTED on hardware (written on Linux). The FFmpeg-facing half could not
//! be cross-checked because ffmpeg-sys-next needs Windows FFmpeg headers and
//! a C compiler for the target; the video-processor half (`vp`) has no FFmpeg
//! dependency and was `cargo check`ed for x86_64-pc-windows-msvc.
//!
//! Flow per frame:
//! 1. `AVHWDeviceContext(D3D11VA)` wraps the *capture* device (same device as
//!    WGC, so the captured texture can be used without copies).
//! 2. The encoder's `AVHWFramesContext` (NV12, BindFlags = RENDER_TARGET)
//!    hands out textures; the Video Processor blits the WGC BGRA (or FP16)
//!    texture into one, scaling + converting on the GPU.
//! 3. The resulting AV_PIX_FMT_D3D11 frame goes to hevc_nvenc / hevc_amf /
//!    hevc_qsv.
//!
//! FFmpeg's `AVD3D11VADeviceContext` / `AVD3D11VAFramesContext` are not in
//! ffmpeg-sys-next's generated bindings, so their (stable, public) layouts are
//! declared here.

pub mod vp;

use super::*;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;

/// `libavutil/hwcontext_d3d11va.h: AVD3D11VADeviceContext`
#[repr(C)]
struct AVD3D11VADeviceContext {
    device: *mut libc::c_void,
    device_context: *mut libc::c_void,
    video_device: *mut libc::c_void,
    video_context: *mut libc::c_void,
    lock: Option<unsafe extern "C" fn(*mut libc::c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut libc::c_void)>,
    lock_ctx: *mut libc::c_void,
}

/// `libavutil/hwcontext_d3d11va.h: AVD3D11VAFramesContext`
#[repr(C)]
struct AVD3D11VAFramesContext {
    texture: *mut libc::c_void,
    bind_flags: u32,
    misc_flags: u32,
    texture_infos: *mut libc::c_void,
}

fn win_err(what: &str, e: windows::core::Error) -> EncodeError {
    EncodeError::Ffmpeg(format!("{what}: {e}"))
}

/// Device on the default hardware adapter (used for the startup probe when
/// the capture device is not known yet).
pub fn create_default_device() -> Result<ID3D11Device> {
    let mut dev: Option<ID3D11Device> = None;
    // SAFETY: out-params are valid; no adapter/software module.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            None,
        )
        .map_err(|e| win_err("D3D11CreateDevice", e))?;
    }
    dev.ok_or_else(|| EncodeError::Ffmpeg("D3D11CreateDevice returned no device".into()))
}

/// Name of the first DXGI adapter (for `gpu_name`).
pub fn adapter_name() -> Option<String> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    // SAFETY: plain COM calls.
    unsafe {
        let f: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let a = f.EnumAdapters1(0).ok()?;
        let d = a.GetDesc1().ok()?;
        let n = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
        Some(String::from_utf16_lossy(&d.Description[..n]))
    }
}

pub struct D3d11State {
    pub device: ID3D11Device,
    pub device_ref: BufRef,
    pub frames_ref: BufRef,
    hwctx: *mut AVD3D11VADeviceContext,
    vp: Option<vp::VideoProcessor>,
    out: (u32, u32),
    rate_hz: u32,
}

// SAFETY: see VideoProcessor; hwctx points into device_ref, which we own.
unsafe impl Send for D3d11State {}

impl D3d11State {
    pub fn new(device: &ID3D11Device, out_w: u32, out_h: u32, rate_hz: u32) -> Result<Self> {
        // SAFETY: av_hwdevice_ctx_alloc returns a D3D11VA device whose hwctx
        // is an AVD3D11VADeviceContext. FFmpeg Release()s `device` on free,
        // so it gets its own reference (into_raw of a clone == AddRef).
        let (device_ref, hwctx) = unsafe {
            let r = sys::av_hwdevice_ctx_alloc(DeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if r.is_null() {
                return Err(EncodeError::Ffmpeg("av_hwdevice_ctx_alloc(D3D11VA) failed".into()));
            }
            let owned = BufRef(r);
            let dc = (*r).data as *mut sys::AVHWDeviceContext;
            let hw = (*dc).hwctx as *mut AVD3D11VADeviceContext;
            (*hw).device = device.clone().into_raw();
            // device_context / video_* / lock are filled in by init (and it
            // turns on ID3D10Multithread protection for the device).
            check(sys::av_hwdevice_ctx_init(r), "av_hwdevice_ctx_init(D3D11VA)")?;
            (owned, hw)
        };
        let frames_ref = hw_frames(&device_ref, Px::AV_PIX_FMT_D3D11, Px::AV_PIX_FMT_NV12, out_w, out_h, 0, |hwctx| {
            // SAFETY: hwctx of a D3D11VA frames context.
            unsafe { (*(hwctx as *mut AVD3D11VAFramesContext)).bind_flags = D3D11_BIND_RENDER_TARGET.0 as u32 };
        })?;
        Ok(Self { device: device.clone(), device_ref, frames_ref, hwctx, vp: None, out: (out_w, out_h), rate_hz })
    }

    pub fn same_device(&self, d: &ID3D11Device) -> bool {
        self.device.as_raw() == d.as_raw()
    }

    /// Blit `src` into a fresh NV12 texture from the encoder pool.
    pub fn process(&mut self, src: &ID3D11Texture2D, in_w: u32, in_h: u32, hdr: bool) -> Result<Frame> {
        let stale = match &self.vp {
            Some(v) => v.in_size != (in_w, in_h) || v.hdr != hdr,
            None => true,
        };
        if stale {
            self.vp = None;
            self.lock();
            let r = vp::VideoProcessor::new(&self.device, in_w, in_h, self.out.0, self.out.1, self.rate_hz, hdr);
            self.unlock();
            self.vp = Some(r.map_err(|e| win_err("D3D11 video processor", e))?);
        }
        let frame = Frame::from_hw_pool(&self.frames_ref)?;
        // SAFETY: D3D11 frames carry the texture in data[0] and the array
        // slice in data[1] (hwcontext_d3d11va.h); borrowed for the blit only.
        let (tex_ptr, index) = unsafe { ((*frame.as_ptr()).data[0] as *mut libc::c_void, (*frame.as_ptr()).data[1] as usize as u32) };
        let dst = unsafe { ID3D11Texture2D::from_raw_borrowed(&tex_ptr) }
            .ok_or_else(|| EncodeError::Ffmpeg("D3D11 pool frame without texture".into()))?;
        self.lock();
        let r = self.vp.as_ref().unwrap().blt(src, 0, dst, index);
        self.unlock();
        r.map_err(|e| win_err("VideoProcessorBlt", e))?;
        Ok(frame)
    }

    fn lock(&self) {
        // SAFETY: lock/lock_ctx were set by av_hwdevice_ctx_init.
        unsafe {
            if let Some(l) = (*self.hwctx).lock {
                l((*self.hwctx).lock_ctx)
            }
        }
    }
    fn unlock(&self) {
        // SAFETY: as above.
        unsafe {
            if let Some(u) = (*self.hwctx).unlock {
                u((*self.hwctx).lock_ctx)
            }
        }
    }
}
