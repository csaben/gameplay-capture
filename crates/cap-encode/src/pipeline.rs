//! Encoder probing and the per-frame scale/convert pipeline (safe code; all
//! FFI is in `ffi`).
//!
//! Frame paths (see `FramePath`):
//! - `CudaGraph`:  CPU BGRA/RGBA -> hwupload (CUDA) -> scale_cuda -> NVENC.
//!   scale_cuda in FFmpeg 6.x/7.x cannot convert RGB->YUV, so NVENC receives
//!   scaled BGR0 CUDA frames and performs the RGB->YUV 4:2:0 conversion on the
//!   GPU itself (BT.601 limited range).
//! - `VaapiGraph`: CPU -> hwupload (VAAPI) -> scale_vaapi(format=nv12) -> hevc_vaapi.
//! - `DrmVaapiGraph`: DMA-BUF -> DRM PRIME -> hwmap (VAAPI) -> scale_vaapi -> hevc_vaapi.
//! - `CpuUpload`:  swscale on the CPU, then av_hwframe_transfer_data into the
//!   encoder's hw frames pool (fallback when a GPU graph is unavailable, and
//!   for HDR half-float CPU frames).
//! - `CpuSoftware`: swscale straight into a software encoder (forced only).
//! - `D3d11VideoProcessor` (Windows) and `VtTransfer` (macOS): see `ffi`.

use crate::ffi::{self, BufRef, DeviceType, EncoderCtx, FilterGraph, Frame, GraphInput, OpenArgs, Px, Sws};
use crate::probe::{self, Candidate, Family, HwKind};
use crate::{EncodeError, EncodedPacket, EncoderConfig, Result, StreamParams};
use cap_capture::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::{BTreeMap, HashSet};

/// Variants are platform specific, so some are unused on any given target.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FramePath {
    CudaGraph,
    VaapiGraph,
    DrmVaapiGraph,
    CpuUpload,
    CpuSoftware,
    D3d11VideoProcessor,
    VtTransfer,
}

impl FramePath {
    pub fn as_str(self) -> &'static str {
        match self {
            FramePath::CudaGraph => "gpu:hwupload+scale_cuda",
            FramePath::VaapiGraph => "gpu:hwupload+scale_vaapi",
            FramePath::DrmVaapiGraph => "gpu:dmabuf+hwmap+scale_vaapi",
            FramePath::CpuUpload => "cpu:swscale+hwupload",
            FramePath::CpuSoftware => "cpu:swscale",
            FramePath::D3d11VideoProcessor => "gpu:d3d11_video_processor",
            FramePath::VtTransfer => "gpu:vt_pixel_transfer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GraphKey {
    path: FramePath,
    w: u32,
    h: u32,
    fmt: Px,
}

pub struct Inner {
    cfg: EncoderConfig,
    cand: Candidate,
    enc: EncoderCtx,
    /// Encoder input: hw pix fmt (or the sw fmt for software encoders).
    pix_fmt: Px,
    /// Software layout of the frames the encoder consumes.
    sw_fmt: Px,
    device: Option<BufRef>,
    enc_frames: Option<BufRef>,
    graph: Option<(GraphKey, FilterGraph)>,
    failed_graphs: HashSet<GraphKey>,
    sws: Option<Sws>,
    needs_reopen: bool,
    pub last_path: Option<FramePath>,
    pub params: StreamParams,
    #[cfg(target_os = "linux")]
    drm: Option<ffi::drm::DrmImporter>,
    #[cfg(windows)]
    d3d: Option<ffi::d3d11::D3d11State>,
    #[cfg(target_os = "macos")]
    vt: Option<ffi::vt::VtTransfer>,
}

/// Probe candidates in order and open the first that works.
pub fn open(cfg: &EncoderConfig, #[allow(unused)] platform_device: Option<PlatformDevice>) -> Result<Inner> {
    ffi::init();
    validate(cfg)?;
    let mut tried = Vec::new();
    for cand in probe::candidates(cfg) {
        let forced = cfg.force_encoder.as_deref() == Some(cand.name.as_str());
        if !probe::is_hardware_name(&cand.name) && !forced {
            // Never fall back to software unless it was named explicitly.
            tried.push(format!("{}: software encoder not allowed", cand.name));
            continue;
        }
        match try_open(cfg, &cand, platform_device.clone()) {
            Ok(inner) => {
                tracing::info!(encoder = %cand.name, "opened encoder");
                return Ok(inner);
            }
            Err(e) => {
                tracing::debug!(encoder = %cand.name, error = %e, "encoder probe failed");
                tried.push(format!("{}: {e}", cand.name));
            }
        }
    }
    if tried.is_empty() {
        tried.push(format!("no {:?} hardware encoder for this platform", cfg.codec));
    }
    Err(EncodeError::NoHardwareEncoder(tried.join("; ")))
}

fn validate(cfg: &EncoderConfig) -> Result<()> {
    if cfg.width == 0 || cfg.height == 0 || !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) {
        return Err(EncodeError::Ffmpeg(format!("output size {}x{} must be non-zero and even", cfg.width, cfg.height)));
    }
    if cfg.rate_hz == 0 {
        return Err(EncodeError::Ffmpeg("rate_hz must be > 0".into()));
    }
    Ok(())
}

/// Optional platform device the encoder should share with capture.
#[derive(Clone)]
pub enum PlatformDevice {
    #[cfg(windows)]
    D3D11(windows::Win32::Graphics::Direct3D11::ID3D11Device),
    #[allow(dead_code)]
    None,
}

fn try_open(cfg: &EncoderConfig, cand: &Candidate, #[allow(unused)] pd: Option<PlatformDevice>) -> Result<Inner> {
    if !ffi::encoder_exists(&cand.name) {
        return Err(EncodeError::Ffmpeg("not built into this FFmpeg".into()));
    }
    let set = probe::options_for(cand, cfg);
    if let Some(why) = set.unsupported {
        return Err(EncodeError::Ffmpeg(why));
    }
    let (w, h) = (cfg.width, cfg.height);
    let mut device: Option<BufRef> = None;
    let mut enc_frames: Option<BufRef> = None;
    #[cfg(windows)]
    let mut d3d: Option<ffi::d3d11::D3d11State> = None;
    let (pix_fmt, sw_fmt) = match cand.hw {
        HwKind::Cuda => {
            let dev = ffi::hw_device(DeviceType::AV_HWDEVICE_TYPE_CUDA, None)?;
            // BGR0 == BGRA memory layout with alpha ignored; NVENC converts to YUV.
            enc_frames = Some(ffi::hw_frames(&dev, Px::AV_PIX_FMT_CUDA, Px::AV_PIX_FMT_BGR0, w, h, 0, |_| {})?);
            device = Some(dev);
            (Px::AV_PIX_FMT_CUDA, Px::AV_PIX_FMT_BGR0)
        }
        HwKind::Vaapi => {
            let node = std::env::var("CAP_VAAPI_DEVICE").ok();
            let dev = ffi::hw_device(DeviceType::AV_HWDEVICE_TYPE_VAAPI, node.as_deref())?;
            enc_frames = Some(ffi::hw_frames(&dev, Px::AV_PIX_FMT_VAAPI, Px::AV_PIX_FMT_NV12, w, h, 0, |_| {})?);
            device = Some(dev);
            (Px::AV_PIX_FMT_VAAPI, Px::AV_PIX_FMT_NV12)
        }
        HwKind::VideoToolbox => {
            let dev = ffi::hw_device(DeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX, None)?;
            enc_frames = Some(ffi::hw_frames(&dev, Px::AV_PIX_FMT_VIDEOTOOLBOX, Px::AV_PIX_FMT_NV12, w, h, 0, |_| {})?);
            device = Some(dev);
            (Px::AV_PIX_FMT_VIDEOTOOLBOX, Px::AV_PIX_FMT_NV12)
        }
        HwKind::D3d11 => {
            #[cfg(windows)]
            {
                let d3d_dev = match pd {
                    Some(PlatformDevice::D3D11(d)) => d,
                    _ => ffi::d3d11::create_default_device()?,
                };
                let st = ffi::d3d11::D3d11State::new(&d3d_dev, w, h, cfg.rate_hz)?;
                device = Some(st.device_ref.try_clone()?);
                enc_frames = Some(st.frames_ref.try_clone()?);
                d3d = Some(st);
                (Px::AV_PIX_FMT_D3D11, Px::AV_PIX_FMT_NV12)
            }
            #[cfg(not(windows))]
            {
                return Err(EncodeError::Ffmpeg("D3D11 transport only exists on Windows".into()));
            }
        }
        HwKind::Software => {
            let fmts = ffi::encoder_pix_fmts(&cand.name);
            let pick = [Px::AV_PIX_FMT_YUV420P, Px::AV_PIX_FMT_NV12]
                .into_iter()
                .find(|p| fmts.is_empty() || fmts.contains(p))
                .ok_or_else(|| EncodeError::Ffmpeg("encoder accepts neither yuv420p nor nv12".into()))?;
            (pick, pick)
        }
    };

    let args = OpenArgs {
        name: &cand.name,
        width: w,
        height: h,
        rate_hz: cfg.rate_hz,
        gop: cfg.gop.max(1),
        pix_fmt,
        sw_pix_fmt: sw_fmt,
        hw_frames: enc_frames.as_ref(),
        hw_device: None,
        opts: &set.opts,
        qscale: set.ctx.qscale,
        unit_quant_factors: set.ctx.unit_quant_factors,
    };
    let (enc, unused) = EncoderCtx::open(&args)?;
    if !unused.is_empty() {
        tracing::warn!(encoder = %cand.name, ?unused, "encoder ignored options");
    }
    let extradata = enc.extradata();
    if extradata.is_empty() {
        return Err(EncodeError::Ffmpeg("encoder produced no extradata (global header)".into()));
    }

    let mut params = BTreeMap::new();
    params.insert("encoder".into(), cand.name.clone());
    params.insert("family".into(), format!("{:?}", cand.family).to_lowercase());
    params.insert("gop".into(), cfg.gop.max(1).to_string());
    params.insert("bframes".into(), "0".into());
    params.insert("lossless".into(), cfg.lossless.to_string());
    params.insert("qp".into(), if cfg.lossless { "lossless".into() } else { cfg.qp.to_string() });
    params.insert(
        "rate_control".into(),
        match (cand.family, cfg.lossless) {
            (_, true) => "lossless",
            (Family::VideoToolbox, _) => "constant_quality",
            _ => "constqp",
        }
        .into(),
    );
    if let Some(q) = set.ctx.qscale {
        params.insert("global_quality".into(), q.to_string());
    }
    params.insert("input_pix_fmt".into(), format!("{}/{}", ffi::pix_fmt_name(pix_fmt), ffi::pix_fmt_name(sw_fmt)));
    params.insert("color".into(), "bt601-limited(smpte170m matrix, bt709 primaries/trc)".into());
    params.insert("scale_algo".into(), "bicubic".into());
    params.insert("hdr_tonemap".into(), "clamp".into());
    params.insert(
        "scale_path".into(),
        match cand.hw {
            HwKind::Cuda => "gpu:hwupload+scale_cuda (cpu fallback: swscale+hwupload)",
            HwKind::Vaapi => "gpu:hwupload|hwmap+scale_vaapi (cpu fallback: swscale+hwupload)",
            HwKind::D3d11 => "gpu:d3d11_video_processor",
            HwKind::VideoToolbox => "gpu:vt_pixel_transfer (cpu: swscale+upload)",
            HwKind::Software => "cpu:swscale",
        }
        .into(),
    );
    params.insert("ffmpeg_version".into(), ffi::version_info());
    for (k, v) in &set.opts {
        if !unused.contains(k) {
            params.insert(format!("opt.{k}"), v.clone());
        }
    }

    Ok(Inner {
        cfg: cfg.clone(),
        cand: cand.clone(),
        enc,
        pix_fmt,
        sw_fmt,
        device,
        enc_frames,
        graph: None,
        failed_graphs: HashSet::new(),
        sws: None,
        needs_reopen: false,
        last_path: None,
        params: StreamParams {
            encoder_name: cand.name.clone(),
            width: w,
            height: h,
            rate_hz: cfg.rate_hz,
            codec: cfg.codec,
            extradata,
            params,
        },
        #[cfg(target_os = "linux")]
        drm: None,
        #[cfg(windows)]
        d3d,
        #[cfg(target_os = "macos")]
        vt: None,
    })
}

/// Software layout + bytes/pixel of a packed CPU capture format.
fn cpu_src_format(f: PixelFormat) -> (Px, usize) {
    match f {
        PixelFormat::Bgra8 => (Px::AV_PIX_FMT_BGRA, 4),
        PixelFormat::Rgba8 => (Px::AV_PIX_FMT_RGBA, 4),
        PixelFormat::Rgba16f => (Px::AV_PIX_FMT_RGBAF16LE, 8),
        PixelFormat::Nv12 => (Px::AV_PIX_FMT_NV12, 1),
    }
}

/// Alpha-ignoring equivalent accepted by hwupload (CUDA/VAAPI).
fn upload_format(f: PixelFormat) -> Option<Px> {
    match f {
        PixelFormat::Bgra8 => Some(Px::AV_PIX_FMT_BGR0),
        PixelFormat::Rgba8 => Some(Px::AV_PIX_FMT_RGB0),
        PixelFormat::Nv12 => Some(Px::AV_PIX_FMT_NV12),
        // Half float cannot be uploaded to CUDA/VAAPI surfaces; CPU tone-map (clamp).
        PixelFormat::Rgba16f => None,
    }
}

impl Inner {
    pub fn encode(&mut self, frame: &CapturedFrame, pts: i64, force_keyframe: bool) -> Result<Vec<EncodedPacket>> {
        if self.needs_reopen {
            self.reopen()?;
        }
        let mut f = self.prepare(frame, pts)?;
        f.set_pts(pts);
        f.set_force_keyframe(force_keyframe);
        let mut out = Vec::with_capacity(1);
        self.enc.encode(Some(&f), &mut out)?;
        Ok(out)
    }

    pub fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        if self.needs_reopen {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        self.enc.encode(None, &mut out)?;
        // The codec context is at EOF; reopen lazily if encoding continues.
        self.needs_reopen = true;
        Ok(out)
    }

    fn reopen(&mut self) -> Result<()> {
        #[cfg(windows)]
        let pd = self.d3d.as_ref().map(|d| PlatformDevice::D3D11(d.device.clone()));
        #[cfg(not(windows))]
        let pd = None;
        let fresh = try_open(&self.cfg, &self.cand, pd)?;
        if fresh.params.extradata != self.params.extradata {
            tracing::warn!("encoder extradata changed on reopen; new segments use the new parameters");
        }
        *self = fresh;
        Ok(())
    }

    fn prepare(&mut self, frame: &CapturedFrame, pts: i64) -> Result<Frame> {
        match &frame.payload {
            FramePayload::Cpu { data, stride } => self.prepare_cpu(frame, data, *stride, pts),
            #[cfg(target_os = "linux")]
            FramePayload::DmaBuf { fourcc, modifier, planes } => self.prepare_dmabuf(frame, *fourcc, *modifier, planes, pts),
            #[cfg(windows)]
            FramePayload::D3D11 { texture, device } => self.prepare_d3d11(frame, texture, device),
            #[cfg(target_os = "macos")]
            FramePayload::CvPixelBuffer(pb) => self.prepare_cvpixelbuffer(frame, pb.0),
        }
    }

    fn prepare_cpu(&mut self, frame: &CapturedFrame, data: &[u8], stride: usize, pts: i64) -> Result<Frame> {
        let (w, h) = (frame.width, frame.height);
        let graph_path = match self.cand.hw {
            HwKind::Cuda => Some(FramePath::CudaGraph),
            HwKind::Vaapi => Some(FramePath::VaapiGraph),
            _ => None,
        };
        if let (Some(path), Some(up)) = (graph_path, upload_format(frame.format)) {
            // CUDA frames must match the encoder's BGR0 layout (see module docs).
            let usable = !(path == FramePath::CudaGraph && up != Px::AV_PIX_FMT_BGR0);
            let key = GraphKey { path, w, h, fmt: up };
            if usable && !self.failed_graphs.contains(&key) {
                match self.run_graph(key, frame, data, stride, pts) {
                    Ok(f) => {
                        self.last_path = Some(path);
                        return Ok(f);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "GPU scale graph unavailable; using CPU swscale fallback");
                        self.failed_graphs.insert(key);
                        self.graph = None;
                    }
                }
            }
        }
        // CPU scale/convert.
        let (src_fmt, bpp) = cpu_src_format(frame.format);
        let mut sw = self.cpu_scale(src_fmt, w, h, |sws, dst| {
            if src_fmt == Px::AV_PIX_FMT_NV12 {
                sws.scale_nv12(data, stride, dst)
            } else {
                if stride < w as usize * bpp {
                    return Err(EncodeError::UnsupportedPayload(format!("stride {stride} < row {}", w as usize * bpp)));
                }
                sws.scale_packed(data, stride, dst)
            }
        })?;
        self.finish_cpu(&mut sw)
    }

    /// swscale `src` into a new frame of the encoder's software layout.
    fn cpu_scale(&mut self, src_fmt: Px, w: u32, h: u32, run: impl FnOnce(&Sws, &mut Frame) -> Result<()>) -> Result<Frame> {
        if !ffi::sws_supports_input(src_fmt) {
            return Err(EncodeError::UnsupportedPayload(format!("swscale cannot read {}", ffi::pix_fmt_name(src_fmt))));
        }
        let key = (src_fmt, w, h, self.sw_fmt, self.cfg.width, self.cfg.height);
        if self.sws.as_ref().map(|s| s.key) != Some(key) {
            self.sws = Some(Sws::new(src_fmt, w, h, self.sw_fmt, self.cfg.width, self.cfg.height)?);
        }
        let mut dst = Frame::sw(self.sw_fmt, self.cfg.width, self.cfg.height)?;
        run(self.sws.as_ref().unwrap(), &mut dst)?;
        Ok(dst)
    }

    /// Hand a CPU-scaled frame to the encoder (uploading into its hw pool).
    fn finish_cpu(&mut self, sw: &mut Frame) -> Result<Frame> {
        match &self.enc_frames {
            None => {
                self.last_path = Some(FramePath::CpuSoftware);
                Ok(std::mem::replace(sw, Frame::empty()?))
            }
            Some(pool) => {
                let mut hw = Frame::from_hw_pool(pool)?;
                hw.transfer_from(sw)?;
                self.last_path = Some(FramePath::CpuUpload);
                Ok(hw)
            }
        }
    }

    fn graph_desc(&self, path: FramePath) -> String {
        let (w, h) = (self.cfg.width, self.cfg.height);
        match path {
            // interp_algo=bicubic matches swscale's SWS_BICUBIC fallback.
            FramePath::CudaGraph => format!("hwupload,scale_cuda=w={w}:h={h}:interp_algo=bicubic"),
            FramePath::VaapiGraph => format!("hwupload,scale_vaapi=w={w}:h={h}:format=nv12"),
            FramePath::DrmVaapiGraph => format!("hwmap,scale_vaapi=w={w}:h={h}:format=nv12"),
            _ => unreachable!("not a graph path"),
        }
    }

    fn ensure_graph(&mut self, key: GraphKey, hw_in: Option<&BufRef>) -> Result<()> {
        if self.graph.as_ref().map(|g| g.0) == Some(key) {
            return Ok(());
        }
        self.graph = None;
        let desc = self.graph_desc(key.path);
        let fmt = if hw_in.is_some() { Px::AV_PIX_FMT_DRM_PRIME } else { key.fmt };
        let g = FilterGraph::new(
            &desc,
            GraphInput { width: key.w, height: key.h, pix_fmt: fmt, rate_hz: self.cfg.rate_hz, hw_frames: hw_in },
            self.device.as_ref(),
        )?;
        let (ofmt, ow, oh, osw) = g.output_info();
        let want_sw = self.sw_fmt;
        if ofmt != self.pix_fmt as i32 || ow != self.cfg.width || oh != self.cfg.height || osw != Some(want_sw) {
            return Err(EncodeError::Ffmpeg(format!(
                "graph '{desc}' outputs fmt {ofmt} {ow}x{oh} sw {osw:?}; encoder wants {:?}/{:?} {}x{}",
                self.pix_fmt, want_sw, self.cfg.width, self.cfg.height
            )));
        }
        self.graph = Some((key, g));
        Ok(())
    }

    fn run_graph(&mut self, key: GraphKey, frame: &CapturedFrame, data: &[u8], stride: usize, pts: i64) -> Result<Frame> {
        self.ensure_graph(key, None)?;
        let mut input = if key.fmt == Px::AV_PIX_FMT_NV12 {
            Frame::sw_nv12_from(frame.width, frame.height, data, stride)?
        } else {
            Frame::sw_packed_from(key.fmt, frame.width, frame.height, data, stride, 4)?
        };
        input.set_pts(pts);
        self.graph.as_mut().unwrap().1.run(&input)
    }

    #[cfg(target_os = "linux")]
    fn prepare_dmabuf(
        &mut self,
        frame: &CapturedFrame,
        fourcc: u32,
        modifier: u64,
        planes: &[cap_capture::DmaBufPlane],
        pts: i64,
    ) -> Result<Frame> {
        // UNTESTED (no DMA-BUF producer on the dev box). See ffi::drm.
        use std::os::fd::AsRawFd;
        if self.drm.is_none() {
            self.drm = Some(ffi::drm::DrmImporter::new()?);
        }
        let pl: Vec<_> =
            planes.iter().map(|p| ffi::drm::DrmPlaneIn { fd: p.fd.as_raw_fd(), offset: p.offset, stride: p.stride }).collect();
        let mut drm_frame = self.drm.as_mut().unwrap().wrap(frame.width, frame.height, fourcc, modifier, &pl)?;
        drm_frame.set_pts(pts);
        let sw = ffi::drm::sw_format_for_fourcc(fourcc).unwrap();

        if self.cand.hw == HwKind::Vaapi {
            let key = GraphKey { path: FramePath::DrmVaapiGraph, w: frame.width, h: frame.height, fmt: sw };
            if !self.failed_graphs.contains(&key) {
                let hw_in = self.drm.as_mut().unwrap().frames_ctx(frame.width, frame.height, sw)?.try_clone()?;
                let r = self.ensure_graph(key, Some(&hw_in)).and_then(|_| self.graph.as_mut().unwrap().1.run(&drm_frame));
                match r {
                    Ok(f) => {
                        self.last_path = Some(FramePath::DrmVaapiGraph);
                        return Ok(f);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "DMA-BUF VAAPI import failed; falling back to CPU map");
                        self.failed_graphs.insert(key);
                        self.graph = None;
                    }
                }
            }
        }
        // CPU map (linear only) -> the regular CPU path.
        let mapped = ffi::drm::map_to_memory(&drm_frame, modifier)?;
        let (mp, ml) = mapped.planes();
        let (w, h) = (frame.width, frame.height);
        let mut out = self.cpu_scale(sw, w, h, |sws, dst| sws.scale_planes(mp, ml, dst))?;
        drop(mapped);
        self.finish_cpu(&mut out)
    }

    #[cfg(windows)]
    fn prepare_d3d11(
        &mut self,
        frame: &CapturedFrame,
        texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    ) -> Result<Frame> {
        // UNTESTED on real hardware (written on Linux; see ffi::d3d11).
        if self.cand.hw != HwKind::D3d11 {
            return Err(EncodeError::UnsupportedPayload(format!("D3D11 texture for {}", self.cand.name)));
        }
        let same = self.d3d.as_ref().map(|d| d.same_device(device)).unwrap_or(false);
        if !same {
            // The encoder must live on the capture device; reopen on it.
            tracing::info!("reopening encoder on the capture D3D11 device");
            let fresh = try_open(&self.cfg, &self.cand, Some(PlatformDevice::D3D11(device.clone())))?;
            if fresh.params.extradata != self.params.extradata {
                tracing::warn!("encoder extradata changed after device switch");
            }
            *self = fresh;
        }
        let hdr = frame.format == PixelFormat::Rgba16f;
        let st = self.d3d.as_mut().unwrap();
        let f = st.process(texture, frame.width, frame.height, hdr)?;
        self.last_path = Some(FramePath::D3d11VideoProcessor);
        Ok(f)
    }

    #[cfg(target_os = "macos")]
    fn prepare_cvpixelbuffer(&mut self, _frame: &CapturedFrame, pb: *mut std::ffi::c_void) -> Result<Frame> {
        // UNTESTED (no macOS machine available). See ffi::vt.
        if self.cand.hw != HwKind::VideoToolbox {
            return Err(EncodeError::UnsupportedPayload(format!("CVPixelBuffer for {}", self.cand.name)));
        }
        if self.vt.is_none() {
            self.vt = Some(ffi::vt::VtTransfer::new()?);
        }
        let pool = self.enc_frames.as_ref().unwrap();
        let f = self.vt.as_mut().unwrap().transfer(pb, pool)?;
        self.last_path = Some(FramePath::VtTransfer);
        Ok(f)
    }
}
