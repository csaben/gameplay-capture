//! Wayland window capture: XDG ScreenCast portal (`ashpd`) + PipeWire stream.
//!
//! Portal half (feature `portal`): opens a ScreenCast session for a single
//! window (`SourceType::Window`, cursor hidden) with persist mode
//! `ExplicitlyRevoked`, so the compositor returns a restore token. Pass the
//! token back in `PortalConfig::restore_token` next time to skip the picker
//! dialog; read the new one with `PipeWireSource::restore_token()` after
//! `start` (tokens are single-use: always store the latest).
//!
//! On Wayland the portal picker chooses the window; `WindowTarget::native_id`
//! is ignored.
//!
//! PipeWire half (feature `pipewire`): connects to the portal's PipeWire
//! remote and negotiates raw video, preferring DMA-BUF (with the configured
//! DRM modifiers) and falling back to shared memory (MemFd/MemPtr).
//!
//! - DMA-BUF buffers are published as `FramePayload::DmaBuf` with dup'd fds.
//!   The PipeWire buffer is kept dequeued while any `Arc<CapturedFrame>` for
//!   it is alive (the latest frame in `LatestFrame` counts), so the compositor
//!   cannot render into memory the encoder may still import. At most
//!   `PipeWireConfig::max_held_buffers` are held; beyond that the oldest is
//!   returned anyway (the stream must not starve).
//! - SHM buffers are copied into `FramePayload::Cpu` and returned at once.
//! - Timestamps come from `spa_meta_header.pts` (CLOCK_MONOTONIC ns, the
//!   `cap_clock` base on Linux); if absent or implausible (> 1 s from now) we
//!   stamp `cap_clock::now_ns()` at dequeue.
//! - `SPA_META_VideoCrop` (GNOME window casts) is honoured: CPU frames are
//!   cropped; DMA-BUF frames report the crop size, and a non-zero crop origin
//!   is folded into the plane offset for LINEAR buffers (ignored, with a
//!   one-time warning, for tiled modifiers).
//!
//! The process callback runs on our own PipeWire loop thread and never blocks.

use crate::*;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use std::os::fd::OwnedFd;

// ---------------------------------------------------------------- portal ---

/// Portal options.
#[derive(Debug, Clone)]
pub struct PortalConfig {
    /// Restore token from a previous session (skips the picker dialog).
    pub restore_token: Option<String>,
    /// Ask the portal to remember the choice until explicitly revoked.
    pub persist: bool,
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self { restore_token: None, persist: true }
    }
}

/// A live portal ScreenCast session. Dropping it without `close()` leaves the
/// session to be closed when the D-Bus connection goes away.
pub struct PortalSession {
    session: Session<Screencast>,
    _proxy: Screencast,
    /// PipeWire node of the selected window.
    pub node_id: u32,
    /// Stream size reported by the portal, if any.
    pub size: Option<(i32, i32)>,
    /// New restore token (store it; the old one is now consumed).
    pub restore_token: Option<String>,
    /// PipeWire remote for this session (taken by the stream).
    pub pipewire_fd: Option<OwnedFd>,
}

impl PortalSession {
    pub fn close(self) {
        let _ = futures_executor::block_on(self.session.close());
    }
}

fn portal_err(e: ashpd::Error) -> CaptureError {
    match e {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => CaptureError::PermissionDenied,
        ashpd::Error::Portal(ashpd::PortalError::NotAllowed(_)) => CaptureError::PermissionDenied,
        other => CaptureError::Backend(format!("screencast portal: {other}")),
    }
}

/// Run the portal handshake (blocking; may show the compositor's picker).
pub fn open_portal_session(cfg: &PortalConfig) -> Result<PortalSession> {
    futures_executor::block_on(async {
        let proxy = Screencast::new()
            .await
            .map_err(|e| CaptureError::Unsupported(format!("ScreenCast portal unavailable: {e}")))?;
        let types = proxy.available_source_types().await.map_err(portal_err)?;
        if !types.contains(SourceType::Window) {
            return Err(CaptureError::Unsupported("portal cannot cast single windows".into()));
        }
        let session = proxy.create_session(Default::default()).await.map_err(portal_err)?;
        let persist = if cfg.persist { PersistMode::ExplicitlyRevoked } else { PersistMode::DoNot };
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Hidden)
                    .set_sources(Some(SourceType::Window.into()))
                    .set_multiple(false)
                    .set_persist_mode(persist)
                    .set_restore_token(cfg.restore_token.as_deref()),
            )
            .await
            .map_err(portal_err)?
            .response()
            .map_err(portal_err)?;
        let streams = proxy
            .start(&session, None, Default::default())
            .await
            .map_err(portal_err)?
            .response()
            .map_err(portal_err)?;
        let stream = streams
            .streams()
            .first()
            .ok_or_else(|| CaptureError::Backend("portal returned no streams".into()))?;
        let (node_id, size) = (stream.pipe_wire_node_id(), stream.size());
        let restore_token = streams.restore_token().map(str::to_owned);
        let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await.map_err(portal_err)?;
        Ok(PortalSession { session, _proxy: proxy, node_id, size, restore_token, pipewire_fd: Some(fd) })
    })
}

// --------------------------------------------------------------- pipewire ---

/// `DRM_FORMAT_MOD_LINEAR`.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID` (implicit modifier).
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}
pub const DRM_FORMAT_XRGB8888: u32 = fourcc(b"XR24");
pub const DRM_FORMAT_ARGB8888: u32 = fourcc(b"AR24");
pub const DRM_FORMAT_XBGR8888: u32 = fourcc(b"XB24");
pub const DRM_FORMAT_ABGR8888: u32 = fourcc(b"AB24");

#[derive(Debug, Clone)]
pub struct PipeWireConfig {
    pub portal: PortalConfig,
    /// Offer DMA-BUF at all (else SHM only).
    pub dmabuf: bool,
    /// DRM modifiers the downstream importer (VAAPI / CUDA / EGL) accepts,
    /// in preference order. Default: LINEAR, which every importer handles.
    pub dmabuf_modifiers: Vec<u64>,
    /// DMA-BUF buffers we may keep dequeued while consumers still reference them.
    pub max_held_buffers: usize,
}

impl Default for PipeWireConfig {
    fn default() -> Self {
        Self {
            portal: PortalConfig::default(),
            dmabuf: true,
            dmabuf_modifiers: vec![DRM_FORMAT_MOD_LINEAR],
            max_held_buffers: 2,
        }
    }
}

/// Wayland capture backend (portal + PipeWire).
#[cfg(feature = "pipewire")]
pub struct PipeWireSource {
    cfg: PipeWireConfig,
    portal: Option<PortalSession>,
    restore_token: Option<String>,
    thread: Option<stream::StreamThread>,
    shared: Arc<stream::Shared>,
}

#[cfg(feature = "pipewire")]
impl PipeWireSource {
    pub fn new(cfg: PipeWireConfig) -> Self {
        let restore_token = cfg.portal.restore_token.clone();
        Self { cfg, portal: None, restore_token, thread: None, shared: Arc::default() }
    }

    /// Latest restore token: the one returned by the portal on the last
    /// `start`, else the configured one. Persist it for the next run.
    pub fn restore_token(&self) -> Option<String> {
        self.restore_token.clone()
    }

    pub fn set_restore_token(&mut self, token: Option<String>) {
        self.cfg.portal.restore_token = token.clone();
        self.restore_token = token;
    }

    /// Why the stream stopped, if it did.
    pub fn last_error(&self) -> Option<String> {
        self.shared.last_error.lock().unwrap().clone()
    }

    pub fn frames_published(&self) -> u64 {
        self.shared.frames.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Capture PipeWire node `node_id` directly, bypassing the portal:
    /// `remote` is a PipeWire remote fd (e.g. from `open_portal_session`), or
    /// `None` for the user's default PipeWire daemon (only useful for nodes
    /// the session manager lets us see, e.g. tests or trusted setups).
    pub fn start_node(&mut self, remote: Option<OwnedFd>, node_id: u32, sink: Arc<LatestFrame>) -> Result<()> {
        if let Some(t) = self.thread.take() {
            t.stop();
        }
        *self.shared.info.lock().unwrap() = Some(SourceInfo { backend: "pipewire", width: 0, height: 0, hdr: false });
        *self.shared.last_error.lock().unwrap() = None;
        self.thread = Some(stream::spawn(remote, node_id, self.cfg.clone(), sink, self.shared.clone())?);
        Ok(())
    }

    /// True while the PipeWire loop thread is alive.
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.join.is_finished())
    }
}

#[cfg(feature = "pipewire")]
impl Drop for PipeWireSource {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(feature = "pipewire")]
impl FrameSource for PipeWireSource {
    fn start(&mut self, _target: &WindowTarget, sink: Arc<LatestFrame>) -> Result<()> {
        self.stop();
        let mut portal = open_portal_session(&self.cfg.portal)?;
        if let Some(t) = &portal.restore_token {
            self.restore_token = Some(t.clone());
            // Tokens are single-use; the next start must present the new one.
            self.cfg.portal.restore_token = Some(t.clone());
        }
        let fd = portal.pipewire_fd.take().expect("fresh portal session has an fd");
        let size = portal.size;
        match self.start_node(Some(fd), portal.node_id, sink) {
            Ok(()) => {
                if let (Some((w, h)), Some(i)) = (size, self.shared.info.lock().unwrap().as_mut()) {
                    if i.width == 0 {
                        (i.width, i.height) = (w.max(0) as u32, h.max(0) as u32);
                    }
                }
                self.portal = Some(portal);
                Ok(())
            }
            Err(e) => {
                portal.close();
                Err(e)
            }
        }
    }

    fn stop(&mut self) {
        if let Some(t) = self.thread.take() {
            t.stop();
        }
        if let Some(p) = self.portal.take() {
            p.close();
        }
    }

    fn info(&self) -> Option<SourceInfo> {
        self.shared.info.lock().unwrap().clone()
    }
}

#[cfg(feature = "pipewire")]
mod stream {
    use super::*;
    use pipewire as pw;
    use pw::spa;
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::{VideoFormat, VideoInfoRaw};
    use spa::param::ParamType;
    use spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
    use spa::sys as spa_sys;
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};
    use std::collections::VecDeque;
    use std::os::fd::{BorrowedFd, RawFd};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Weak;

    #[derive(Default)]
    pub(super) struct Shared {
        pub info: Mutex<Option<SourceInfo>>,
        pub last_error: Mutex<Option<String>>,
        pub frames: AtomicU64,
    }

    pub(super) struct StreamThread {
        quit: pw::channel::Sender<()>,
        pub join: std::thread::JoinHandle<()>,
    }

    impl StreamThread {
        pub fn stop(self) {
            let _ = self.quit.send(());
            let _ = self.join.join();
        }
    }

    const FORMATS: [VideoFormat; 4] = [VideoFormat::BGRx, VideoFormat::BGRA, VideoFormat::RGBx, VideoFormat::RGBA];

    fn serialize(obj: Object) -> Vec<u8> {
        spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
            .expect("pod serialisation into a Vec cannot fail")
            .0
            .into_inner()
    }

    fn id_prop(key: u32, v: u32) -> Property {
        Property::new(key, Value::Id(Id(v)))
    }

    /// EnumFormat pod; with `modifiers` it describes DMA-BUF, without it SHM.
    fn enum_format(formats: &[VideoFormat], modifiers: Option<&[u64]>) -> Vec<u8> {
        let mut props = vec![
            id_prop(FormatProperties::MediaType.as_raw(), MediaType::Video.as_raw()),
            id_prop(FormatProperties::MediaSubtype.as_raw(), MediaSubtype::Raw.as_raw()),
            Property::new(
                FormatProperties::VideoFormat.as_raw(),
                Value::Choice(ChoiceValue::Id(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: Id(formats[0].as_raw()),
                        alternatives: formats.iter().map(|f| Id(f.as_raw())).collect(),
                    },
                ))),
            ),
        ];
        if let Some(m) = modifiers {
            let m: Vec<i64> = m.iter().map(|&x| x as i64).collect();
            props.push(Property {
                key: FormatProperties::VideoModifier.as_raw(),
                flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
                value: Value::Choice(ChoiceValue::Long(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum { default: m[0], alternatives: m.clone() },
                ))),
            });
        }
        props.push(Property::new(
            FormatProperties::VideoSize.as_raw(),
            Value::Choice(ChoiceValue::Rectangle(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Rectangle { width: 1920, height: 1080 },
                    min: Rectangle { width: 1, height: 1 },
                    max: Rectangle { width: 16384, height: 16384 },
                },
            ))),
        ));
        props.push(Property::new(
            FormatProperties::VideoFramerate.as_raw(),
            Value::Choice(ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Fraction { num: 0, denom: 1 },
                    min: Fraction { num: 0, denom: 1 },
                    max: Fraction { num: 1000, denom: 1 },
                },
            ))),
        ));
        serialize(Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties: props,
        })
    }

    fn enum_formats(cfg: &PipeWireConfig) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        if cfg.dmabuf && !cfg.dmabuf_modifiers.is_empty() {
            v.push(enum_format(&FORMATS, Some(&cfg.dmabuf_modifiers)));
        }
        v.push(enum_format(&FORMATS, None));
        v
    }

    fn buffers_param(dmabuf: bool) -> Vec<u8> {
        let mask: i32 = if dmabuf {
            1 << spa_sys::SPA_DATA_DmaBuf
        } else {
            (1 << spa_sys::SPA_DATA_MemFd) | (1 << spa_sys::SPA_DATA_MemPtr)
        };
        serialize(Object {
            type_: SpaTypes::ObjectParamBuffers.as_raw(),
            id: ParamType::Buffers.as_raw(),
            properties: vec![
                Property::new(
                    spa_sys::SPA_PARAM_BUFFERS_buffers,
                    Value::Choice(ChoiceValue::Int(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Range { default: 8, min: 4, max: 16 },
                    ))),
                ),
                Property::new(
                    spa_sys::SPA_PARAM_BUFFERS_dataType,
                    Value::Choice(ChoiceValue::Int(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Flags { default: mask, flags: vec![] },
                    ))),
                ),
            ],
        })
    }

    fn meta_param(meta_type: u32, size: usize) -> Vec<u8> {
        serialize(Object {
            type_: SpaTypes::ObjectParamMeta.as_raw(),
            id: ParamType::Meta.as_raw(),
            properties: vec![
                id_prop(spa_sys::SPA_PARAM_META_type, meta_type),
                Property::new(spa_sys::SPA_PARAM_META_size, Value::Int(size as i32)),
            ],
        })
    }

    fn pods(bytes: &[Vec<u8>]) -> Vec<&Pod> {
        bytes.iter().map(|b| Pod::from_bytes(b).expect("valid serialised pod")).collect()
    }

    /// (DRM fourcc, our pixel format, has alpha) for a negotiated SPA format.
    pub(super) fn map_format(f: VideoFormat) -> Option<(u32, PixelFormat, bool)> {
        Some(match f {
            VideoFormat::BGRx => (DRM_FORMAT_XRGB8888, PixelFormat::Bgra8, false),
            VideoFormat::BGRA => (DRM_FORMAT_ARGB8888, PixelFormat::Bgra8, true),
            VideoFormat::RGBx => (DRM_FORMAT_XBGR8888, PixelFormat::Rgba8, false),
            VideoFormat::RGBA => (DRM_FORMAT_ABGR8888, PixelFormat::Rgba8, true),
            _ => return None,
        })
    }

    /// Frame timestamp: header PTS if it's plausibly CLOCK_MONOTONIC, else now.
    pub(super) fn pick_timestamp(pts: Option<i64>, now: Nanos) -> Nanos {
        match pts {
            Some(p) if p > 0 && (now - p).abs() < 1_000_000_000 => p,
            _ => now,
        }
    }

    unsafe fn find_meta(buf: *const spa_sys::spa_buffer, type_: u32, min_size: usize) -> *mut std::ffi::c_void {
        let b = &*buf;
        for i in 0..b.n_metas as usize {
            let m = &*b.metas.add(i);
            if m.type_ == type_ && m.size as usize >= min_size {
                return m.data;
            }
        }
        std::ptr::null_mut()
    }

    struct State {
        cfg: PipeWireConfig,
        sink: Arc<LatestFrame>,
        shared: Arc<Shared>,
        format: VideoInfoRaw,
        negotiated: bool,
        dmabuf: bool,
        /// DMA-BUF buffers kept dequeued while frames referencing them live.
        held: VecDeque<(*mut pw::sys::pw_buffer, Weak<CapturedFrame>)>,
        warned_crop: bool,
    }

    impl State {
        /// Return held buffers nobody references any more (and the oldest ones
        /// beyond the cap) to the stream.
        unsafe fn release_held(&mut self, stream: &pw::stream::Stream, keep: usize) {
            self.held.retain(|(b, w)| {
                if w.strong_count() == 0 {
                    stream.queue_raw_buffer(*b);
                    false
                } else {
                    true
                }
            });
            while self.held.len() > keep {
                let (b, _) = self.held.pop_front().unwrap();
                stream.queue_raw_buffer(b);
            }
        }

        /// Build a frame from a dequeued buffer. Returns (frame, keep_buffer).
        unsafe fn frame_from(&mut self, pwb: *mut pw::sys::pw_buffer) -> Option<(CapturedFrame, bool)> {
            let buf = (*pwb).buffer;
            if buf.is_null() || (*buf).n_datas == 0 || !self.negotiated {
                return None;
            }
            let (fourcc, pixfmt, has_alpha) = map_format(self.format.format())?;
            let size = self.format.size();
            let now = cap_clock::now_ns();

            let hdr = find_meta(buf, spa_sys::SPA_META_Header, std::mem::size_of::<spa_sys::spa_meta_header>())
                as *const spa_sys::spa_meta_header;
            let mut pts = None;
            if !hdr.is_null() {
                if (*hdr).flags & spa_sys::SPA_META_HEADER_FLAG_CORRUPTED != 0 {
                    return None;
                }
                pts = Some((*hdr).pts);
            }
            let capture_ns = pick_timestamp(pts, now);

            // Crop region (x, y, w, h) within the buffer.
            let (mut cx, mut cy, mut cw, mut ch) = (0u32, 0u32, size.width, size.height);
            let crop = find_meta(buf, spa_sys::SPA_META_VideoCrop, std::mem::size_of::<spa_sys::spa_meta_region>())
                as *const spa_sys::spa_meta_region;
            if !crop.is_null() {
                let r = (*crop).region;
                if r.size.width > 0 && r.size.height > 0 {
                    cx = r.position.x.max(0) as u32;
                    cy = r.position.y.max(0) as u32;
                    cw = r.size.width.min(size.width.saturating_sub(cx));
                    ch = r.size.height.min(size.height.saturating_sub(cy));
                }
            }
            if cw == 0 || ch == 0 {
                return None;
            }

            let d0 = &*(*buf).datas;
            let chunk = &*d0.chunk;
            if chunk.flags & spa_sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 {
                return None;
            }

            if d0.type_ == spa_sys::SPA_DATA_DmaBuf {
                let modifier = self.format.modifier();
                let mut planes = Vec::with_capacity((*buf).n_datas as usize);
                let mut offset_adjust = 0u32;
                if cx != 0 || cy != 0 {
                    if modifier == DRM_FORMAT_MOD_LINEAR {
                        offset_adjust = cy * chunk.stride as u32 + cx * 4;
                    } else {
                        if !self.warned_crop {
                            tracing::warn!("crop origin on a tiled DMA-BUF; delivering the uncropped buffer");
                            self.warned_crop = true;
                        }
                        (cx, cy, cw, ch) = (0, 0, size.width, size.height);
                    }
                }
                let _ = (cx, cy);
                for i in 0..(*buf).n_datas as usize {
                    let d = &*(*buf).datas.add(i);
                    let c = &*d.chunk;
                    let fd = d.fd as RawFd;
                    if fd < 0 {
                        return None;
                    }
                    let owned = BorrowedFd::borrow_raw(fd).try_clone_to_owned().ok()?;
                    let adj = if i == 0 { offset_adjust } else { 0 };
                    planes.push(DmaBufPlane { fd: owned, offset: c.offset + adj, stride: c.stride as u32 });
                }
                return Some((
                    CapturedFrame {
                        capture_ns,
                        width: cw,
                        height: ch,
                        format: pixfmt,
                        payload: FramePayload::DmaBuf { fourcc, modifier, planes },
                    },
                    true,
                ));
            }

            // SHM (MemFd / MemPtr), mapped by MAP_BUFFERS.
            if d0.data.is_null() || chunk.size == 0 {
                return None;
            }
            let base = d0.data as *const u8;
            let maxsize = d0.maxsize as usize;
            let offset = (chunk.offset as usize).min(maxsize);
            let src_stride = if chunk.stride > 0 { chunk.stride as usize } else { size.width as usize * 4 };
            let row = cw as usize * 4;
            let first = offset + cy as usize * src_stride + cx as usize * 4;
            let last_end = first + (ch as usize - 1) * src_stride + row;
            if last_end > maxsize {
                return None;
            }
            let mut out = vec![0u8; row * ch as usize];
            for y in 0..ch as usize {
                let src = std::slice::from_raw_parts(base.add(first + y * src_stride), row);
                out[y * row..(y + 1) * row].copy_from_slice(src);
            }
            if !has_alpha {
                for px in out.chunks_exact_mut(4) {
                    px[3] = 0xff;
                }
            }
            Some((
                CapturedFrame {
                    capture_ns,
                    width: cw,
                    height: ch,
                    format: pixfmt,
                    payload: FramePayload::Cpu { data: out, stride: row },
                },
                false,
            ))
        }
    }

    pub(super) fn spawn(
        fd: Option<OwnedFd>,
        node_id: u32,
        cfg: PipeWireConfig,
        sink: Arc<LatestFrame>,
        shared: Arc<Shared>,
    ) -> Result<StreamThread> {
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<pw::channel::Sender<()>>>();
        let join = std::thread::Builder::new()
            .name("cap-pipewire".into())
            .spawn(move || {
                let err_shared = shared.clone();
                if let Err(e) = run(fd, node_id, cfg, sink, shared, &init_tx) {
                    *err_shared.last_error.lock().unwrap() = Some(e.to_string());
                    let _ = init_tx.send(Err(e));
                }
            })
            .map_err(|e| CaptureError::Backend(format!("spawn pipewire thread: {e}")))?;
        match init_rx.recv() {
            Ok(Ok(quit)) => Ok(StreamThread { quit, join }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                let _ = join.join();
                Err(CaptureError::Backend("pipewire thread exited during setup".into()))
            }
        }
    }

    fn pw_err(what: &'static str) -> impl Fn(pw::Error) -> CaptureError {
        move |e| CaptureError::Backend(format!("pipewire {what}: {e}"))
    }

    fn run(
        fd: Option<OwnedFd>,
        node_id: u32,
        cfg: PipeWireConfig,
        sink: Arc<LatestFrame>,
        shared: Arc<Shared>,
        init_tx: &std::sync::mpsc::Sender<Result<pw::channel::Sender<()>>>,
    ) -> Result<()> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_err("main loop"))?;
        let context = pw::context::ContextRc::new(&mainloop, None).map_err(pw_err("context"))?;
        let core = match fd {
            Some(fd) => context.connect_fd_rc(fd, None).map_err(pw_err("connect to portal remote"))?,
            None => context.connect_rc(None).map_err(pw_err("connect to daemon"))?,
        };
        let stream = pw::stream::StreamBox::new(
            &core,
            "cap-capture",
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )
        .map_err(pw_err("stream"))?;

        let state = State {
            cfg: cfg.clone(),
            sink,
            shared: shared.clone(),
            format: VideoInfoRaw::default(),
            negotiated: false,
            dmabuf: false,
            held: VecDeque::new(),
            warned_crop: false,
        };

        let ml_weak = mainloop.downgrade();
        let _listener = stream
            .add_local_listener_with_user_data(state)
            .state_changed(move |_, st, _old, new| match new {
                pw::stream::StreamState::Error(msg) => {
                    tracing::warn!("pipewire stream error: {msg}");
                    *st.shared.last_error.lock().unwrap() = Some(msg);
                    if let Some(ml) = ml_weak.upgrade() {
                        ml.quit();
                    }
                }
                pw::stream::StreamState::Unconnected => {
                    // Window closed or cast revoked.
                    *st.shared.last_error.lock().unwrap() = Some("stream disconnected".into());
                    if let Some(ml) = ml_weak.upgrade() {
                        ml.quit();
                    }
                }
                s => tracing::debug!("pipewire stream state: {s:?}"),
            })
            .param_changed(|stream, st, id, param| {
                let Some(param) = param else { return };
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let Ok((mt, mst)) = spa::param::format_utils::parse_format(param) else { return };
                if mt != MediaType::Video || mst != MediaSubtype::Raw {
                    return;
                }
                if st.format.parse(param).is_err() {
                    return;
                }
                // Raw bits: the typed flags are gated behind newer libspa features.
                let flags = st.format.flags().bits();
                st.dmabuf = flags & spa_sys::SPA_VIDEO_FLAG_MODIFIER != 0;
                if flags & spa_sys::SPA_VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED != 0 {
                    // We offered several modifiers with DONT_FIXATE: pin the
                    // producer's pick and renegotiate (SHM stays as fallback).
                    let fixed = [st.format.modifier()];
                    let bytes = vec![enum_format(&[st.format.format()], Some(&fixed)), enum_format(&FORMATS, None)];
                    if let Err(e) = stream.update_params(&mut pods(&bytes)) {
                        tracing::warn!("modifier fixation failed: {e}");
                    }
                    return;
                }
                st.negotiated = map_format(st.format.format()).is_some();
                let size = st.format.size();
                tracing::info!(
                    "pipewire format {:?} {}x{} {} modifier {:#x}",
                    st.format.format(),
                    size.width,
                    size.height,
                    if st.dmabuf { "dmabuf" } else { "shm" },
                    st.format.modifier()
                );
                if let Some(i) = st.shared.info.lock().unwrap().as_mut() {
                    i.width = size.width;
                    i.height = size.height;
                }
                let bytes = vec![
                    buffers_param(st.dmabuf),
                    meta_param(spa_sys::SPA_META_Header, std::mem::size_of::<spa_sys::spa_meta_header>()),
                    meta_param(spa_sys::SPA_META_VideoCrop, std::mem::size_of::<spa_sys::spa_meta_region>()),
                ];
                if let Err(e) = stream.update_params(&mut pods(&bytes)) {
                    tracing::warn!("pipewire update_params: {e}");
                }
            })
            .remove_buffer(|_, st, b| {
                // Buffers are being freed (renegotiation/teardown): forget them.
                st.held.retain(|(h, _)| *h != b);
            })
            .process(|stream, st| unsafe {
                let keep = st.cfg.max_held_buffers;
                st.release_held(stream, keep);
                // Newest buffer wins; hand older ones straight back.
                let mut newest: *mut pw::sys::pw_buffer = std::ptr::null_mut();
                loop {
                    let b = stream.dequeue_raw_buffer();
                    if b.is_null() {
                        break;
                    }
                    if !newest.is_null() {
                        stream.queue_raw_buffer(newest);
                    }
                    newest = b;
                }
                if newest.is_null() {
                    return;
                }
                match st.frame_from(newest) {
                    Some((frame, hold)) => {
                        st.sink.publish(frame);
                        st.shared.frames.fetch_add(1, Ordering::Relaxed);
                        if hold {
                            // Track the Arc LatestFrame now holds; the buffer
                            // goes back once every clone of it is dropped.
                            let weak = st.sink.latest().1.as_ref().map(Arc::downgrade).unwrap_or_default();
                            st.held.push_back((newest, weak));
                            // Keep at most `keep` held, counting this one.
                            st.release_held(stream, keep.max(1));
                        } else {
                            stream.queue_raw_buffer(newest);
                        }
                    }
                    None => stream.queue_raw_buffer(newest),
                }
            })
            .register()
            .map_err(pw_err("listener"))?;

        let bytes = enum_formats(&cfg);
        stream
            .connect(
                spa::utils::Direction::Input,
                Some(node_id),
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut pods(&bytes),
            )
            .map_err(pw_err("stream connect"))?;

        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let ml = mainloop.clone();
        let _attached = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());
        let _ = init_tx.send(Ok(quit_tx));
        mainloop.run();
        // Held buffers are returned to PipeWire implicitly on disconnect.
        let _ = stream.disconnect();
        Ok(())
    }
}

#[cfg(all(test, feature = "pipewire"))]
mod tests {
    use super::stream::*;
    use super::*;
    use pipewire::spa::param::video::VideoFormat;

    #[test]
    fn fourcc_mapping() {
        assert_eq!(DRM_FORMAT_XRGB8888, 0x3432_5258);
        assert_eq!(map_format(VideoFormat::BGRx).unwrap(), (DRM_FORMAT_XRGB8888, PixelFormat::Bgra8, false));
        assert_eq!(map_format(VideoFormat::RGBA).unwrap().1, PixelFormat::Rgba8);
        assert!(map_format(VideoFormat::NV12).is_none());
    }

    #[test]
    fn timestamp_choice() {
        let now = 10_000_000_000;
        assert_eq!(pick_timestamp(Some(now - 5_000_000), now), now - 5_000_000);
        assert_eq!(pick_timestamp(Some(0), now), now);
        assert_eq!(pick_timestamp(None, now), now);
        assert_eq!(pick_timestamp(Some(now + 5_000_000_000), now), now);
    }
}
