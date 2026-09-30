//! D3D11 Video Processor: scale + BGRA/FP16 -> NV12 on the GPU
//! (`ID3D11VideoContext::VideoProcessorBlt`).
//!
//! UNTESTED on hardware: written and `cargo check`ed for
//! x86_64-pc-windows-msvc from Linux. Deliberately free of FFmpeg types so it
//! can be type-checked (and later unit-tested on Windows) on its own.
//!
//! Colour: RGB full range in, YCbCr BT.601 studio range out, matching the
//! NVENC-internal and swscale conversions used on the other paths. HDR
//! (R16G16B16A16_FLOAT, scRGB linear) input is declared as
//! `RGB_FULL_G10_NONE_P709` via ID3D11VideoContext1 so the VP applies the
//! gamma encode; values above 1.0 are clamped (no real tone-mapping yet).

use std::mem::ManuallyDrop;
use windows::core::{Interface, Result};
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

pub struct VideoProcessor {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    enumr: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    pub in_size: (u32, u32),
    pub hdr: bool,
}

// SAFETY: D3D11 video interfaces are free-threaded when the device has
// multithread protection on (FFmpeg's d3d11va device init enables it) and we
// serialise immediate-context use with FFmpeg's device lock.
unsafe impl Send for VideoProcessor {}

/// D3D11_VIDEO_PROCESSOR_COLOR_SPACE bitfield:
/// bit0 Usage, bit1 RGB_Range (0 = 0-255), bit2 YCbCr_Matrix (0 = BT.601),
/// bit3 xvYCC, bits4-5 Nominal_Range (1 = 16-235).
const CS_RGB_FULL: u32 = 0;
const CS_YCBCR_601_STUDIO: u32 = 1 << 4;

impl VideoProcessor {
    pub fn new(device: &ID3D11Device, in_w: u32, in_h: u32, out_w: u32, out_h: u32, rate_hz: u32, hdr: bool) -> Result<Self> {
        // SAFETY: plain COM calls on valid interfaces; all out-params are owned.
        unsafe {
            let vdev: ID3D11VideoDevice = device.cast()?;
            let ctx = device.GetImmediateContext()?;
            let vctx: ID3D11VideoContext = ctx.cast()?;
            let rate = DXGI_RATIONAL { Numerator: rate_hz.max(1), Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: rate,
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumr = vdev.CreateVideoProcessorEnumerator(&desc)?;
            let in_fmt = if hdr { DXGI_FORMAT_R16G16B16A16_FLOAT } else { DXGI_FORMAT_B8G8R8A8_UNORM };
            let in_ok = enumr.CheckVideoProcessorFormat(in_fmt)? & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 != 0;
            let out_ok = enumr.CheckVideoProcessorFormat(DXGI_FORMAT_NV12)? & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 != 0;
            if !in_ok || !out_ok {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_NOTIMPL,
                    format!("video processor cannot convert {in_fmt:?} -> NV12 (input ok {in_ok}, output ok {out_ok})"),
                ));
            }
            let vp = vdev.CreateVideoProcessor(&enumr, 0)?;

            let cs_in = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: CS_RGB_FULL };
            let cs_out = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: CS_YCBCR_601_STUDIO };
            vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &cs_in);
            vctx.VideoProcessorSetOutputColorSpace(&vp, &cs_out);
            if let Ok(vctx1) = vctx.cast::<ID3D11VideoContext1>() {
                let in_cs = if hdr { DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709 } else { DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 };
                vctx1.VideoProcessorSetStreamColorSpace1(&vp, 0, in_cs);
                vctx1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601);
            }
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, BOOL::from(false));
            // Stretch the whole source onto the whole target (same as swscale/scale_cuda).
            let src = RECT { left: 0, top: 0, right: in_w as i32, bottom: in_h as i32 };
            let dst = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&vp, 0, BOOL::from(true), Some(&src));
            vctx.VideoProcessorSetStreamDestRect(&vp, 0, BOOL::from(true), Some(&dst));
            vctx.VideoProcessorSetOutputTargetRect(&vp, BOOL::from(true), Some(&dst));

            Ok(Self { vdev, vctx, enumr, vp, in_size: (in_w, in_h), hdr })
        }
    }

    /// Scale/convert `src` (slice `src_index` of a BGRA/FP16 texture) into
    /// slice `dst_index` of the NV12 texture `dst` (which must have
    /// D3D11_BIND_RENDER_TARGET). Caller holds the device-context lock.
    pub fn blt(&self, src: &ID3D11Texture2D, src_index: u32, dst: &ID3D11Texture2D, dst_index: u32) -> Result<()> {
        // SAFETY: COM calls on valid interfaces; views are released on return.
        unsafe {
            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: src_index } },
            };
            let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
            self.vdev.CreateVideoProcessorInputView(src, &self.enumr, &in_desc, Some(&mut in_view))?;

            let mut td = D3D11_TEXTURE2D_DESC::default();
            dst.GetDesc(&mut td);
            let out_desc = if td.ArraySize > 1 {
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2DArray: D3D11_TEX2D_ARRAY_VPOV { MipSlice: 0, FirstArraySlice: dst_index, ArraySize: 1 },
                    },
                }
            } else {
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
                }
            };
            let mut out_view: Option<ID3D11VideoProcessorOutputView> = None;
            self.vdev.CreateVideoProcessorOutputView(dst, &self.enumr, &out_desc, Some(&mut out_view))?;
            let out_view = out_view.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER))?;

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: BOOL::from(true),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: ManuallyDrop::new(in_view),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };
            let streams = [stream];
            let r = self.vctx.VideoProcessorBlt(&self.vp, &out_view, 0, &streams);
            let [mut stream] = streams;
            ManuallyDrop::drop(&mut stream.pInputSurface);
            r
        }
    }
}
