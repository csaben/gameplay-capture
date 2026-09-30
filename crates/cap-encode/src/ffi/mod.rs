//! The FFmpeg / GPU interop module (spec risk item: "FFmpeg hardware-frame
//! interop in Rust is unsafe-heavy — keep interop in one small module").
//!
//! Every `unsafe` block of the crate lives under `ffi/`. The types exported
//! here are small RAII owners around FFmpeg objects with safe methods, so the
//! pipeline logic in `pipeline.rs` / `mux.rs` is plain safe Rust.
//!
//! Submodules:
//! - `drm`   (Linux): DMA-BUF -> `AV_PIX_FMT_DRM_PRIME` frame import + CPU map.
//! - `d3d11` (Windows): FFmpeg D3D11VA device/frames glue; the D3D11 Video
//!   Processor itself lives in `d3d11::vp` and has no FFmpeg dependency.
//! - `vt`    (macOS): VTPixelTransferSession glue.

#![allow(clippy::missing_safety_doc)]
// Casts on bindgen constants are kept: their Rust types differ across FFmpeg versions.
#![allow(clippy::unnecessary_cast)]

use crate::{EncodeError, EncodedPacket, Result};
use ffmpeg_sys_next as sys;
use std::ffi::{CStr, CString};
use std::ptr;

#[cfg(target_os = "linux")]
pub mod drm;
#[cfg(windows)]
pub mod d3d11;
#[cfg(target_os = "macos")]
pub mod vt;

pub use sys::AVHWDeviceType as DeviceType;
pub use sys::AVPixelFormat as Px;

pub fn err_str(code: i32) -> String {
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: buffer is large enough; av_strerror NUL-terminates.
    unsafe {
        sys::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

pub fn check(code: i32, what: &str) -> Result<i32> {
    if code < 0 {
        Err(EncodeError::Ffmpeg(format!("{what}: {} ({code})", err_str(code))))
    } else {
        Ok(code)
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|_| CString::new("?").unwrap())
}

const EAGAIN: i32 = sys::AVERROR(libc::EAGAIN);

/// One-time library init: quiet logging unless `CAP_FFMPEG_LOG` is set.
pub fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let level = match std::env::var("CAP_FFMPEG_LOG").ok().as_deref() {
            Some("debug") => sys::AV_LOG_DEBUG,
            Some("verbose") => sys::AV_LOG_VERBOSE,
            Some("info") => sys::AV_LOG_INFO,
            Some("warning") => sys::AV_LOG_WARNING,
            _ => sys::AV_LOG_ERROR,
        };
        // SAFETY: plain setter.
        unsafe { sys::av_log_set_level(level as i32) };
    });
}

pub fn version_info() -> String {
    // SAFETY: returns a static string.
    unsafe { CStr::from_ptr(sys::av_version_info()).to_string_lossy().into_owned() }
}

pub fn pix_fmt_name(p: Px) -> String {
    // SAFETY: returns a static string or NULL.
    unsafe {
        let n = sys::av_get_pix_fmt_name(p);
        if n.is_null() {
            format!("{p:?}")
        } else {
            CStr::from_ptr(n).to_string_lossy().into_owned()
        }
    }
}

// ---------------------------------------------------------------------------
// AVBufferRef (hw device / hw frames contexts)
// ---------------------------------------------------------------------------

/// Owned `AVBufferRef`.
pub struct BufRef(*mut sys::AVBufferRef);

// SAFETY: AVBufferRef refcounting is atomic; the referenced hw contexts are
// only used from the thread that owns the Encoder.
unsafe impl Send for BufRef {}

impl BufRef {
    pub fn as_ptr(&self) -> *mut sys::AVBufferRef {
        self.0
    }
    /// New reference (caller-owned raw pointer, for handing to FFmpeg).
    fn new_raw_ref(&self) -> *mut sys::AVBufferRef {
        // SAFETY: self.0 is a valid buffer ref.
        unsafe { sys::av_buffer_ref(self.0) }
    }
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn try_clone(&self) -> Result<BufRef> {
        let r = self.new_raw_ref();
        if r.is_null() {
            return Err(EncodeError::Ffmpeg("av_buffer_ref: out of memory".into()));
        }
        Ok(BufRef(r))
    }
}

impl Drop for BufRef {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { sys::av_buffer_unref(&mut self.0) }
    }
}

/// `av_hwdevice_ctx_create(kind, device)`.
pub fn hw_device(kind: DeviceType, device: Option<&str>) -> Result<BufRef> {
    let dev = device.map(cstr);
    let mut r: *mut sys::AVBufferRef = ptr::null_mut();
    // SAFETY: out-pointer + optional C string.
    let ret = unsafe {
        sys::av_hwdevice_ctx_create(&mut r, kind, dev.as_ref().map_or(ptr::null(), |c| c.as_ptr()), ptr::null_mut(), 0)
    };
    check(ret, &format!("av_hwdevice_ctx_create({kind:?})"))?;
    Ok(BufRef(r))
}

/// Allocate + init a hw frames context. `customize` may touch the
/// API-specific `hwctx` before init (D3D11 BindFlags).
pub fn hw_frames(
    device: &BufRef,
    hw_fmt: Px,
    sw_fmt: Px,
    width: u32,
    height: u32,
    initial_pool_size: i32,
    customize: impl FnOnce(*mut libc::c_void),
) -> Result<BufRef> {
    // SAFETY: standard alloc/init sequence on a valid device ref.
    unsafe {
        let r = sys::av_hwframe_ctx_alloc(device.as_ptr());
        if r.is_null() {
            return Err(EncodeError::Ffmpeg("av_hwframe_ctx_alloc failed".into()));
        }
        let owned = BufRef(r);
        let fc = (*r).data as *mut sys::AVHWFramesContext;
        (*fc).format = hw_fmt;
        (*fc).sw_format = sw_fmt;
        (*fc).width = width as i32;
        (*fc).height = height as i32;
        (*fc).initial_pool_size = initial_pool_size;
        customize((*fc).hwctx);
        check(sys::av_hwframe_ctx_init(r), &format!("av_hwframe_ctx_init({hw_fmt:?}/{sw_fmt:?})"))?;
        Ok(owned)
    }
}

// ---------------------------------------------------------------------------
// AVFrame
// ---------------------------------------------------------------------------

pub struct Frame(*mut sys::AVFrame);

// SAFETY: frames are moved between threads only as a whole.
unsafe impl Send for Frame {}

impl Frame {
    pub fn empty() -> Result<Frame> {
        // SAFETY: plain allocation.
        let f = unsafe { sys::av_frame_alloc() };
        if f.is_null() {
            return Err(EncodeError::Ffmpeg("av_frame_alloc failed".into()));
        }
        Ok(Frame(f))
    }

    pub fn as_ptr(&self) -> *mut sys::AVFrame {
        self.0
    }

    /// Software frame with its own buffers.
    pub fn sw(fmt: Px, width: u32, height: u32) -> Result<Frame> {
        let f = Frame::empty()?;
        // SAFETY: fresh frame; get_buffer allocates planes.
        unsafe {
            (*f.0).format = fmt as i32;
            (*f.0).width = width as i32;
            (*f.0).height = height as i32;
            check(sys::av_frame_get_buffer(f.0, 0), "av_frame_get_buffer")?;
        }
        Ok(f)
    }

    /// Packed single-plane software frame filled from `data` (row copy).
    pub fn sw_packed_from(fmt: Px, width: u32, height: u32, data: &[u8], stride: usize, bpp: usize) -> Result<Frame> {
        let row = width as usize * bpp;
        if stride < row || data.len() < stride * (height as usize - 1) + row {
            return Err(EncodeError::UnsupportedPayload(format!(
                "cpu buffer too small: {} bytes, stride {stride}, {width}x{height}",
                data.len()
            )));
        }
        let f = Frame::sw(fmt, width, height)?;
        // SAFETY: plane 0 has at least linesize*height bytes, rows are in bounds (checked above).
        unsafe {
            let dst = (*f.0).data[0];
            let ls = (*f.0).linesize[0] as usize;
            for y in 0..height as usize {
                ptr::copy_nonoverlapping(data.as_ptr().add(y * stride), dst.add(y * ls), row);
            }
        }
        Ok(f)
    }

    /// NV12 software frame filled from a CPU buffer (Y rows, then UV rows, same stride).
    pub fn sw_nv12_from(width: u32, height: u32, data: &[u8], stride: usize) -> Result<Frame> {
        let (w, h) = (width as usize, height as usize);
        let ch = h.div_ceil(2);
        if stride < w || data.len() < stride * (h + ch - 1) + w {
            return Err(EncodeError::UnsupportedPayload("nv12 buffer too small".into()));
        }
        let f = Frame::sw(Px::AV_PIX_FMT_NV12, width, height)?;
        // SAFETY: both planes are allocated for (w, h) / (w, ceil(h/2)) rows of
        // at least `w` bytes; source rows are in bounds (checked above).
        unsafe {
            let fr = &*f.0;
            for y in 0..h {
                ptr::copy_nonoverlapping(data.as_ptr().add(y * stride), fr.data[0].add(y * fr.linesize[0] as usize), w);
            }
            for y in 0..ch {
                ptr::copy_nonoverlapping(data.as_ptr().add((h + y) * stride), fr.data[1].add(y * fr.linesize[1] as usize), w);
            }
        }
        Ok(f)
    }

    /// Frame from a hw frames pool.
    pub fn from_hw_pool(frames: &BufRef) -> Result<Frame> {
        let f = Frame::empty()?;
        // SAFETY: valid frames ctx.
        check(unsafe { sys::av_hwframe_get_buffer(frames.as_ptr(), f.0, 0) }, "av_hwframe_get_buffer")?;
        Ok(f)
    }

    /// Upload/download: `self` (allocated) <- `src`.
    pub fn transfer_from(&mut self, src: &Frame) -> Result<()> {
        // SAFETY: both frames valid; FFmpeg checks formats.
        check(unsafe { sys::av_hwframe_transfer_data(self.0, src.0, 0) }, "av_hwframe_transfer_data")?;
        Ok(())
    }

    pub fn set_pts(&mut self, pts: i64) {
        // SAFETY: valid frame.
        unsafe { (*self.0).pts = pts }
    }

    /// Request (or clear) a forced keyframe. Encoders map pict_type I to IDR
    /// when their `forced-idr`/`forced_idr` option is set (we always set it).
    pub fn set_force_keyframe(&mut self, force: bool) {
        // SAFETY: valid frame.
        unsafe {
            (*self.0).pict_type =
                if force { sys::AVPictureType::AV_PICTURE_TYPE_I } else { sys::AVPictureType::AV_PICTURE_TYPE_NONE };
        }
    }

    /// Plane pointers/strides of a software (or CPU-mapped) frame.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn planes(&self) -> ([*const u8; 4], [i32; 4]) {
        // SAFETY: valid frame.
        unsafe {
            let f = &*self.0;
            ([f.data[0], f.data[1], f.data[2], f.data[3]].map(|p| p as *const u8), [f.linesize[0], f.linesize[1], f.linesize[2], f.linesize[3]])
        }
    }
    pub fn planes_mut(&mut self) -> ([*mut u8; 4], [i32; 4]) {
        // SAFETY: valid frame.
        unsafe {
            let f = &*self.0;
            ([f.data[0], f.data[1], f.data[2], f.data[3]], [f.linesize[0], f.linesize[1], f.linesize[2], f.linesize[3]])
        }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: we own the frame.
        unsafe { sys::av_frame_free(&mut self.0) }
    }
}

// ---------------------------------------------------------------------------
// swscale (CPU fallback: BGRA/RGBA/RGBAF16 -> encoder format + resize)
// ---------------------------------------------------------------------------

pub struct Sws {
    ctx: *mut sys::SwsContext,
    pub key: (Px, u32, u32, Px, u32, u32),
}

// SAFETY: used from one thread at a time.
unsafe impl Send for Sws {}

pub fn sws_supports_input(p: Px) -> bool {
    // SAFETY: pure query.
    unsafe { sys::sws_isSupportedInput(p) > 0 }
}

impl Sws {
    /// Bicubic resize; RGB -> YUV uses BT.601 limited range (swscale default),
    /// matching what NVENC's internal RGB->YUV conversion signals.
    pub fn new(src: Px, sw: u32, sh: u32, dst: Px, dw: u32, dh: u32) -> Result<Sws> {
        // SAFETY: parameters are plain values.
        let ctx = unsafe {
            sys::sws_getContext(
                sw as i32,
                sh as i32,
                src,
                dw as i32,
                dh as i32,
                dst,
                sys::SWS_BICUBIC as i32,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if ctx.is_null() {
            return Err(EncodeError::Ffmpeg(format!("sws_getContext {src:?} {sw}x{sh} -> {dst:?} {dw}x{dh} unsupported")));
        }
        Ok(Sws { ctx, key: (src, sw, sh, dst, dw, dh) })
    }

    /// Scale a packed single-plane CPU image into `dst`.
    pub fn scale_packed(&self, data: &[u8], stride: usize, dst: &mut Frame) -> Result<()> {
        let (_, sw, sh, ..) = self.key;
        let _ = sw;
        if data.len() < stride * sh as usize {
            return Err(EncodeError::UnsupportedPayload(format!("cpu buffer too small: {} < {}", data.len(), stride * sh as usize)));
        }
        self.scale_planes([data.as_ptr(), ptr::null(), ptr::null(), ptr::null()], [stride as i32, 0, 0, 0], dst)
    }

    /// Scale an NV12 CPU image (Y plane then interleaved UV plane, same stride).
    pub fn scale_nv12(&self, data: &[u8], stride: usize, dst: &mut Frame) -> Result<()> {
        let (_, _, sh, ..) = self.key;
        let y_size = stride * sh as usize;
        if data.len() < y_size + stride * (sh as usize).div_ceil(2) {
            return Err(EncodeError::UnsupportedPayload("nv12 buffer too small".into()));
        }
        // SAFETY: offset within bounds (checked).
        let uv = unsafe { data.as_ptr().add(y_size) };
        self.scale_planes([data.as_ptr(), uv, ptr::null(), ptr::null()], [stride as i32, stride as i32, 0, 0], dst)
    }

    /// Scale from arbitrary plane pointers (e.g. a CPU-mapped DRM frame).
    pub fn scale_planes(&self, src: [*const u8; 4], strides: [i32; 4], dst: &mut Frame) -> Result<()> {
        let (_, _, sh, ..) = self.key;
        let (dp, dl) = dst.planes_mut();
        // SAFETY: src planes cover `sh` rows (callers check), dst was allocated for the output size.
        let ret = unsafe { sys::sws_scale(self.ctx, src.as_ptr(), strides.as_ptr(), 0, sh as i32, dp.as_ptr(), dl.as_ptr()) };
        check(ret, "sws_scale")?;
        Ok(())
    }
}

impl Drop for Sws {
    fn drop(&mut self) {
        // SAFETY: we own the context.
        unsafe { sys::sws_freeContext(self.ctx) }
    }
}

// ---------------------------------------------------------------------------
// libavfilter graph (GPU scale: hwupload/hwmap + scale_cuda / scale_vaapi)
// ---------------------------------------------------------------------------

pub struct FilterGraph {
    graph: *mut sys::AVFilterGraph,
    src: *mut sys::AVFilterContext,
    sink: *mut sys::AVFilterContext,
}

// SAFETY: used from one thread at a time.
unsafe impl Send for FilterGraph {}

pub struct GraphInput<'a> {
    pub width: u32,
    pub height: u32,
    pub pix_fmt: Px,
    pub rate_hz: u32,
    /// For hw input frames (DRM PRIME): their frames context.
    pub hw_frames: Option<&'a BufRef>,
}

impl FilterGraph {
    /// `desc` is a linear filter chain, e.g. `hwupload,scale_cuda=w=640:h=360`.
    /// `device` is attached to every filter as `hw_device_ctx` (used by
    /// hwupload / hwmap to pick the encoder's device instead of creating one).
    pub fn new(desc: &str, input: GraphInput<'_>, device: Option<&BufRef>) -> Result<FilterGraph> {
        // SAFETY: follows doc/examples/filtering_video.c; all pointers are
        // checked and owned by `graph`, which is freed on every error path via Drop.
        unsafe {
            let graph = sys::avfilter_graph_alloc();
            if graph.is_null() {
                return Err(EncodeError::Ffmpeg("avfilter_graph_alloc failed".into()));
            }
            let mut g = FilterGraph { graph, src: ptr::null_mut(), sink: ptr::null_mut() };
            (*graph).nb_threads = 1;

            let args = cstr(&format!(
                "video_size={}x{}:pix_fmt={}:time_base=1/{}:pixel_aspect=1/1",
                input.width, input.height, input.pix_fmt as i32, input.rate_hz.max(1)
            ));
            let buffer = sys::avfilter_get_by_name(c"buffer".as_ptr());
            let buffersink = sys::avfilter_get_by_name(c"buffersink".as_ptr());
            check(
                sys::avfilter_graph_create_filter(&mut g.src, buffer, c"in".as_ptr(), args.as_ptr(), ptr::null_mut(), graph),
                "create buffer source",
            )?;
            if let Some(hwf) = input.hw_frames {
                let par = sys::av_buffersrc_parameters_alloc();
                if par.is_null() {
                    return Err(EncodeError::Ffmpeg("av_buffersrc_parameters_alloc failed".into()));
                }
                (*par).format = input.pix_fmt as i32;
                (*par).width = input.width as i32;
                (*par).height = input.height as i32;
                (*par).hw_frames_ctx = hwf.as_ptr(); // buffersrc takes its own ref
                let ret = sys::av_buffersrc_parameters_set(g.src, par);
                sys::av_free(par as *mut libc::c_void);
                check(ret, "av_buffersrc_parameters_set")?;
            }
            check(
                sys::avfilter_graph_create_filter(&mut g.sink, buffersink, c"out".as_ptr(), ptr::null(), ptr::null_mut(), graph),
                "create buffer sink",
            )?;

            let mut outputs = sys::avfilter_inout_alloc();
            let mut inputs = sys::avfilter_inout_alloc();
            if outputs.is_null() || inputs.is_null() {
                sys::avfilter_inout_free(&mut outputs);
                sys::avfilter_inout_free(&mut inputs);
                return Err(EncodeError::Ffmpeg("avfilter_inout_alloc failed".into()));
            }
            (*outputs).name = sys::av_strdup(c"in".as_ptr());
            (*outputs).filter_ctx = g.src;
            (*outputs).pad_idx = 0;
            (*outputs).next = ptr::null_mut();
            (*inputs).name = sys::av_strdup(c"out".as_ptr());
            (*inputs).filter_ctx = g.sink;
            (*inputs).pad_idx = 0;
            (*inputs).next = ptr::null_mut();
            let cdesc = cstr(desc);
            let ret = sys::avfilter_graph_parse_ptr(graph, cdesc.as_ptr(), &mut inputs, &mut outputs, ptr::null_mut());
            sys::avfilter_inout_free(&mut inputs);
            sys::avfilter_inout_free(&mut outputs);
            check(ret, &format!("avfilter_graph_parse_ptr({desc})"))?;

            if let Some(dev) = device {
                for i in 0..(*graph).nb_filters as usize {
                    let f = *(*graph).filters.add(i);
                    if (*f).hw_device_ctx.is_null() {
                        (*f).hw_device_ctx = dev.new_raw_ref();
                    }
                }
            }
            check(sys::avfilter_graph_config(graph, ptr::null_mut()), &format!("avfilter_graph_config({desc})"))?;
            Ok(g)
        }
    }

    /// Output pixel format, size and (for hw output) the sw format of its frames.
    pub fn output_info(&self) -> (i32, u32, u32, Option<Px>) {
        // SAFETY: configured sink.
        unsafe {
            let fmt = sys::av_buffersink_get_format(self.sink);
            let w = sys::av_buffersink_get_w(self.sink) as u32;
            let h = sys::av_buffersink_get_h(self.sink) as u32;
            let hwf = sys::av_buffersink_get_hw_frames_ctx(self.sink);
            let sw = if hwf.is_null() { None } else { Some((*((*hwf).data as *mut sys::AVHWFramesContext)).sw_format) };
            (fmt, w, h, sw)
        }
    }

    /// Push one frame and pull the (single) result.
    pub fn run(&mut self, input: &Frame) -> Result<Frame> {
        // SAFETY: valid graph; KEEP_REF leaves `input` owned by the caller.
        unsafe {
            check(
                sys::av_buffersrc_add_frame_flags(self.src, input.0, sys::AV_BUFFERSRC_FLAG_KEEP_REF as i32),
                "av_buffersrc_add_frame",
            )?;
            let out = Frame::empty()?;
            let ret = sys::av_buffersink_get_frame(self.sink, out.0);
            if ret == EAGAIN {
                return Err(EncodeError::Ffmpeg("filter graph produced no frame".into()));
            }
            check(ret, "av_buffersink_get_frame")?;
            Ok(out)
        }
    }
}

impl Drop for FilterGraph {
    fn drop(&mut self) {
        // SAFETY: frees all filters too.
        unsafe { sys::avfilter_graph_free(&mut self.graph) }
    }
}

// ---------------------------------------------------------------------------
// Encoder context
// ---------------------------------------------------------------------------

pub struct EncoderCtx {
    ctx: *mut sys::AVCodecContext,
}

// SAFETY: an AVCodecContext may be used from any single thread at a time.
unsafe impl Send for EncoderCtx {}

pub struct OpenArgs<'a> {
    pub name: &'a str,
    pub width: u32,
    pub height: u32,
    pub rate_hz: u32,
    pub gop: u32,
    pub pix_fmt: Px,
    pub sw_pix_fmt: Px,
    pub hw_frames: Option<&'a BufRef>,
    pub hw_device: Option<&'a BufRef>,
    pub opts: &'a [(String, String)],
    pub qscale: Option<i32>,
    pub unit_quant_factors: bool,
}

/// Whether FFmpeg was built with this encoder.
pub fn encoder_exists(name: &str) -> bool {
    let c = cstr(name);
    // SAFETY: lookup only.
    unsafe { !sys::avcodec_find_encoder_by_name(c.as_ptr()).is_null() }
}

/// Software pixel formats an encoder accepts (empty if unknown).
pub fn encoder_pix_fmts(name: &str) -> Vec<Px> {
    let c = cstr(name);
    let mut v = Vec::new();
    // SAFETY: pix_fmts is a NONE-terminated static array.
    unsafe {
        let codec = sys::avcodec_find_encoder_by_name(c.as_ptr());
        if codec.is_null() || (*codec).pix_fmts.is_null() {
            return v;
        }
        let mut p = (*codec).pix_fmts;
        while *p != Px::AV_PIX_FMT_NONE {
            v.push(*p);
            p = p.add(1);
        }
    }
    v
}

impl EncoderCtx {
    /// Open an encoder. Returns the context and the options it did not consume.
    pub fn open(a: &OpenArgs<'_>) -> Result<(EncoderCtx, Vec<String>)> {
        let cname = cstr(a.name);
        // SAFETY: standard avcodec_alloc_context3 / avcodec_open2 sequence;
        // ownership of the ctx is taken by `EncoderCtx` immediately so error
        // paths free it.
        unsafe {
            let codec = sys::avcodec_find_encoder_by_name(cname.as_ptr());
            if codec.is_null() {
                return Err(EncodeError::Ffmpeg("encoder not built into this FFmpeg".into()));
            }
            let ctx = sys::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(EncodeError::Ffmpeg("avcodec_alloc_context3 failed".into()));
            }
            let enc = EncoderCtx { ctx };
            let c = &mut *ctx;
            c.width = a.width as i32;
            c.height = a.height as i32;
            c.time_base = sys::AVRational { num: 1, den: a.rate_hz as i32 };
            c.framerate = sys::AVRational { num: a.rate_hz as i32, den: 1 };
            c.sample_aspect_ratio = sys::AVRational { num: 1, den: 1 };
            c.pix_fmt = a.pix_fmt;
            c.sw_pix_fmt = a.sw_pix_fmt;
            c.gop_size = a.gop as i32;
            c.max_b_frames = 0;
            c.flags |= sys::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            // All paths produce BT.601 limited-range YUV (swscale default,
            // NVENC's internal RGB->YUV, D3D11 VP / VT configured to match).
            c.color_range = sys::AVColorRange::AVCOL_RANGE_MPEG;
            c.colorspace = sys::AVColorSpace::AVCOL_SPC_SMPTE170M;
            c.color_primaries = sys::AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = sys::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            if let Some(q) = a.qscale {
                c.flags |= sys::AV_CODEC_FLAG_QSCALE as i32;
                c.global_quality = q * sys::FF_QP2LAMBDA;
            }
            if a.unit_quant_factors {
                c.i_quant_factor = 1.0;
                c.i_quant_offset = 0.0;
                c.b_quant_factor = 1.0;
                c.b_quant_offset = 0.0;
            }
            if let Some(f) = a.hw_frames {
                c.hw_frames_ctx = f.new_raw_ref();
            }
            if let Some(d) = a.hw_device {
                c.hw_device_ctx = d.new_raw_ref();
            }

            let mut dict: *mut sys::AVDictionary = ptr::null_mut();
            for (k, v) in a.opts {
                let (ck, cv) = (cstr(k), cstr(v));
                sys::av_dict_set(&mut dict, ck.as_ptr(), cv.as_ptr(), 0);
            }
            let ret = sys::avcodec_open2(ctx, codec, &mut dict);
            let mut unused = Vec::new();
            let mut e: *mut sys::AVDictionaryEntry = ptr::null_mut();
            loop {
                e = sys::av_dict_get(dict, c"".as_ptr(), e, sys::AV_DICT_IGNORE_SUFFIX as i32);
                if e.is_null() {
                    break;
                }
                unused.push(CStr::from_ptr((*e).key).to_string_lossy().into_owned());
            }
            sys::av_dict_free(&mut dict);
            check(ret, "avcodec_open2")?;
            Ok((enc, unused))
        }
    }

    pub fn extradata(&self) -> Vec<u8> {
        // SAFETY: extradata/extradata_size are consistent after open.
        unsafe {
            let c = &*self.ctx;
            if c.extradata.is_null() || c.extradata_size <= 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(c.extradata, c.extradata_size as usize).to_vec()
            }
        }
    }

    /// Send a frame (or EOF with `None`) and drain every ready packet.
    pub fn encode(&mut self, frame: Option<&Frame>, out: &mut Vec<EncodedPacket>) -> Result<()> {
        // SAFETY: valid ctx; the frame is referenced, not consumed.
        unsafe {
            let ret = sys::avcodec_send_frame(self.ctx, frame.map_or(ptr::null(), |f| f.0 as *const _));
            if !(frame.is_none() && ret == sys::AVERROR_EOF) {
                check(ret, "avcodec_send_frame")?;
            }
            let mut pkt = sys::av_packet_alloc();
            if pkt.is_null() {
                return Err(EncodeError::Ffmpeg("av_packet_alloc failed".into()));
            }
            let result = loop {
                let r = sys::avcodec_receive_packet(self.ctx, pkt);
                if r == EAGAIN || r == sys::AVERROR_EOF {
                    break Ok(());
                }
                if r < 0 {
                    break check(r, "avcodec_receive_packet").map(|_| ());
                }
                let p = &*pkt;
                let data = if p.data.is_null() || p.size <= 0 {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(p.data, p.size as usize).to_vec()
                };
                let dts = if p.dts == sys::AV_NOPTS_VALUE { p.pts } else { p.dts };
                out.push(EncodedPacket { data, pts: p.pts, dts, keyframe: p.flags & sys::AV_PKT_FLAG_KEY != 0 });
                sys::av_packet_unref(pkt);
            };
            sys::av_packet_free(&mut pkt);
            result
        }
    }
}

impl Drop for EncoderCtx {
    fn drop(&mut self) {
        // SAFETY: we own the context (also unrefs hw_frames_ctx/hw_device_ctx).
        unsafe { sys::avcodec_free_context(&mut self.ctx) }
    }
}

// ---------------------------------------------------------------------------
// Fragmented MP4 muxer
// ---------------------------------------------------------------------------

pub struct Mp4Writer {
    oc: *mut sys::AVFormatContext,
    src_tb: sys::AVRational,
    finished: bool,
}

// SAFETY: used from one thread at a time.
unsafe impl Send for Mp4Writer {}

#[derive(Debug, Clone, Copy)]
pub enum MuxCodec {
    Hevc,
    Av1,
}

impl Mp4Writer {
    pub fn create(path: &std::path::Path, codec: MuxCodec, width: u32, height: u32, rate_hz: u32, extradata: &[u8], movflags: &str) -> Result<Mp4Writer> {
        let cpath = cstr(&path.to_string_lossy());
        // SAFETY: standard muxer setup; `Mp4Writer` owns `oc` as soon as it
        // exists so every error path is cleaned up by Drop.
        unsafe {
            let mut oc: *mut sys::AVFormatContext = ptr::null_mut();
            check(
                sys::avformat_alloc_output_context2(&mut oc, ptr::null(), c"mp4".as_ptr(), cpath.as_ptr()),
                "avformat_alloc_output_context2",
            )?;
            let src_tb = sys::AVRational { num: 1, den: rate_hz as i32 };
            let mut w = Mp4Writer { oc, src_tb, finished: true };
            let st = sys::avformat_new_stream(oc, ptr::null());
            if st.is_null() {
                return Err(EncodeError::Ffmpeg("avformat_new_stream failed".into()));
            }
            let par = &mut *(*st).codecpar;
            par.codec_type = sys::AVMediaType::AVMEDIA_TYPE_VIDEO;
            let (id, tag) = match codec {
                MuxCodec::Hevc => (sys::AVCodecID::AV_CODEC_ID_HEVC, u32::from_le_bytes(*b"hvc1")),
                MuxCodec::Av1 => (sys::AVCodecID::AV_CODEC_ID_AV1, u32::from_le_bytes(*b"av01")),
            };
            par.codec_id = id;
            par.codec_tag = tag;
            par.width = width as i32;
            par.height = height as i32;
            par.format = Px::AV_PIX_FMT_YUV420P as i32;
            par.color_range = sys::AVColorRange::AVCOL_RANGE_MPEG;
            par.color_space = sys::AVColorSpace::AVCOL_SPC_SMPTE170M;
            par.color_primaries = sys::AVColorPrimaries::AVCOL_PRI_BT709;
            par.color_trc = sys::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            if !extradata.is_empty() {
                let pad = sys::AV_INPUT_BUFFER_PADDING_SIZE as usize;
                let buf = sys::av_mallocz(extradata.len() + pad) as *mut u8;
                if buf.is_null() {
                    return Err(EncodeError::Ffmpeg("av_mallocz failed".into()));
                }
                ptr::copy_nonoverlapping(extradata.as_ptr(), buf, extradata.len());
                par.extradata = buf;
                par.extradata_size = extradata.len() as i32;
            }
            (*st).time_base = src_tb;
            (*st).avg_frame_rate = sys::AVRational { num: rate_hz as i32, den: 1 };
            (*st).r_frame_rate = (*st).avg_frame_rate;
            // Push every packet through to the file (fragments are then on
            // disk as soon as the muxer emits them).
            (*oc).flush_packets = 1;

            check(sys::avio_open(&mut (*oc).pb, cpath.as_ptr(), sys::AVIO_FLAG_WRITE as i32), "avio_open")?;
            w.finished = false;
            let mut dict: *mut sys::AVDictionary = ptr::null_mut();
            let mf = cstr(movflags);
            sys::av_dict_set(&mut dict, c"movflags".as_ptr(), mf.as_ptr(), 0);
            // Timescale that is an exact multiple of the frame rate.
            let ts = cstr(&(rate_hz.max(1) * 1000).to_string());
            sys::av_dict_set(&mut dict, c"video_track_timescale".as_ptr(), ts.as_ptr(), 0);
            let ret = sys::avformat_write_header(oc, &mut dict);
            sys::av_dict_free(&mut dict);
            if ret < 0 {
                sys::avio_closep(&mut (*oc).pb);
                w.finished = true;
                check(ret, "avformat_write_header")?;
            }
            Ok(w)
        }
    }

    /// Write one packet; `pts`/`dts` in 1/rate_hz units, `duration` in frames.
    pub fn write(&mut self, data: &[u8], pts: i64, dts: i64, keyframe: bool, duration: i64) -> Result<()> {
        // SAFETY: header written; packet allocated and freed here.
        unsafe {
            let st = *(*self.oc).streams;
            let tb = (*st).time_base;
            let mut pkt = sys::av_packet_alloc();
            if pkt.is_null() {
                return Err(EncodeError::Ffmpeg("av_packet_alloc failed".into()));
            }
            let r = sys::av_new_packet(pkt, data.len() as i32);
            if r < 0 {
                sys::av_packet_free(&mut pkt);
                check(r, "av_new_packet")?;
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (*pkt).data, data.len());
            (*pkt).pts = sys::av_rescale_q(pts, self.src_tb, tb);
            (*pkt).dts = sys::av_rescale_q(dts, self.src_tb, tb);
            (*pkt).duration = sys::av_rescale_q(duration, self.src_tb, tb);
            (*pkt).stream_index = 0;
            if keyframe {
                (*pkt).flags |= sys::AV_PKT_FLAG_KEY;
            }
            let r = sys::av_write_frame(self.oc, pkt);
            sys::av_packet_free(&mut pkt);
            check(r, "av_write_frame")?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        // SAFETY: header was written and pb is open.
        unsafe {
            let r = sys::av_write_trailer(self.oc);
            let c = sys::avio_closep(&mut (*self.oc).pb);
            check(r, "av_write_trailer")?;
            check(c, "avio_close")?;
        }
        Ok(())
    }
}

impl Drop for Mp4Writer {
    fn drop(&mut self) {
        if !self.finished {
            // Best effort: flush the last fragment. Even without this the
            // file is playable up to the last complete fragment.
            let _ = self.finish();
        }
        // SAFETY: we own the context; pb already closed.
        unsafe { sys::avformat_free_context(self.oc) }
    }
}
