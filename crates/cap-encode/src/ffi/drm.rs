//! Linux DMA-BUF import (UNTESTED: this was written on a headless box with no
//! PipeWire DMA-BUF producer; it compiles and follows FFmpeg's documented
//! DRM PRIME contract, but has never seen a real buffer).
//!
//! A PipeWire DMA-BUF is wrapped as an `AV_PIX_FMT_DRM_PRIME` frame whose
//! `data[0]` is an `AVDRMFrameDescriptor`. From there:
//! - VAAPI: the frame goes into a `hwmap,scale_vaapi=...` graph whose hwmap
//!   filter carries the encoder's VAAPI device (zero copy import).
//! - NVENC: FFmpeg cannot map DRM PRIME to CUDA directly, so the buffer is
//!   mmapped (`av_hwframe_map`, linear modifiers only) and fed through the
//!   CPU-upload path. A zero-copy CUDA import would need
//!   `cuImportExternalMemory` on the dma-buf fd (or a Vulkan hop); TODO.

use super::*;

pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}
pub const DRM_FORMAT_ARGB8888: u32 = fourcc(b'A', b'R', b'2', b'4');
pub const DRM_FORMAT_XRGB8888: u32 = fourcc(b'X', b'R', b'2', b'4');
pub const DRM_FORMAT_ABGR8888: u32 = fourcc(b'A', b'B', b'2', b'4');
pub const DRM_FORMAT_XBGR8888: u32 = fourcc(b'X', b'B', b'2', b'4');
pub const DRM_FORMAT_NV12: u32 = fourcc(b'N', b'V', b'1', b'2');

/// Software pixel format equivalent to a DRM fourcc (alpha ignored).
pub fn sw_format_for_fourcc(fourcc: u32) -> Option<Px> {
    match fourcc {
        DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 => Some(Px::AV_PIX_FMT_BGR0),
        DRM_FORMAT_ABGR8888 | DRM_FORMAT_XBGR8888 => Some(Px::AV_PIX_FMT_RGB0),
        DRM_FORMAT_NV12 => Some(Px::AV_PIX_FMT_NV12),
        _ => None,
    }
}

pub struct DrmPlaneIn {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

/// DRM device + a frames context matching the incoming buffers.
pub struct DrmImporter {
    device: BufRef,
    frames: Option<(BufRef, u32, u32, Px)>,
}

impl DrmImporter {
    pub fn new() -> Result<DrmImporter> {
        Ok(DrmImporter { device: hw_device(DeviceType::AV_HWDEVICE_TYPE_DRM, None)?, frames: None })
    }

    pub fn frames_ctx(&mut self, w: u32, h: u32, sw: Px) -> Result<&BufRef> {
        let stale = !matches!(&self.frames, Some((_, fw, fh, fs)) if *fw == w && *fh == h && *fs == sw);
        if stale {
            let f = hw_frames(&self.device, Px::AV_PIX_FMT_DRM_PRIME, sw, w, h, 0, |_| {})?;
            self.frames = Some((f, w, h, sw));
        }
        Ok(&self.frames.as_ref().unwrap().0)
    }

    /// Wrap the dma-buf planes as a DRM PRIME frame. The fds stay owned by
    /// the caller (the `CapturedFrame`) and must outlive the import call.
    pub fn wrap(&mut self, w: u32, h: u32, fourcc: u32, modifier: u64, planes: &[DrmPlaneIn]) -> Result<Frame> {
        let sw = sw_format_for_fourcc(fourcc)
            .ok_or_else(|| EncodeError::UnsupportedPayload(format!("dma-buf fourcc {fourcc:#x}")))?;
        if planes.is_empty() || planes.len() > 4 {
            return Err(EncodeError::UnsupportedPayload(format!("dma-buf with {} planes", planes.len())));
        }
        let frames = self.frames_ctx(w, h, sw)?.try_clone()?;
        let frame = Frame::empty()?;
        // SAFETY: descriptor is zero-initialised, filled within its fixed-size
        // arrays (planes.len() <= 4 checked), and owned by an AVBufferRef that
        // the frame holds, so FFmpeg frees it with the frame.
        unsafe {
            let size = std::mem::size_of::<sys::AVDRMFrameDescriptor>();
            let desc = sys::av_mallocz(size) as *mut sys::AVDRMFrameDescriptor;
            if desc.is_null() {
                return Err(EncodeError::Ffmpeg("av_mallocz failed".into()));
            }
            let buf = sys::av_buffer_create(desc as *mut u8, size, Some(sys::av_buffer_default_free), ptr::null_mut(), 0);
            if buf.is_null() {
                sys::av_free(desc as *mut libc::c_void);
                return Err(EncodeError::Ffmpeg("av_buffer_create failed".into()));
            }
            let d = &mut *desc;
            d.nb_objects = planes.len() as i32;
            d.nb_layers = 1;
            d.layers[0].format = fourcc;
            d.layers[0].nb_planes = planes.len() as i32;
            for (i, p) in planes.iter().enumerate() {
                let end = libc::lseek(p.fd, 0, libc::SEEK_END);
                d.objects[i].fd = p.fd;
                d.objects[i].size = if end > 0 { end as usize } else { 0 };
                d.objects[i].format_modifier = modifier;
                d.layers[0].planes[i].object_index = i as i32;
                d.layers[0].planes[i].offset = p.offset as isize;
                d.layers[0].planes[i].pitch = p.stride as isize;
            }
            let f = &mut *frame.as_ptr();
            f.format = Px::AV_PIX_FMT_DRM_PRIME as i32;
            f.width = w as i32;
            f.height = h as i32;
            f.data[0] = desc as *mut u8;
            f.buf[0] = buf;
            f.hw_frames_ctx = frames.as_ptr();
            std::mem::forget(frames); // reference now owned by the frame
        }
        Ok(frame)
    }
}

/// mmap a DRM PRIME frame for CPU reads (linear layouts only).
pub fn map_to_memory(drm: &Frame, modifier: u64) -> Result<Frame> {
    if modifier != DRM_FORMAT_MOD_LINEAR {
        return Err(EncodeError::UnsupportedPayload(format!(
            "dma-buf modifier {modifier:#x} is tiled; CPU mapping needs a linear buffer (use VAAPI or request linear from PipeWire)"
        )));
    }
    let dst = Frame::empty()?;
    // SAFETY: dst is a fresh frame; format NONE lets FFmpeg pick the sw format.
    unsafe {
        (*dst.as_ptr()).format = Px::AV_PIX_FMT_NONE as i32;
        check(sys::av_hwframe_map(dst.as_ptr(), drm.as_ptr(), sys::AV_HWFRAME_MAP_READ as i32), "av_hwframe_map(DRM->memory)")?;
    }
    Ok(dst)
}
