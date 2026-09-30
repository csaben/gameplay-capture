//! X11 window capture: XComposite + MIT-SHM `GetImage`, CPU BGRA frames.
//!
//! The target window is redirected off-screen with XComposite
//! (`RedirectWindow(Automatic)`, which coexists with a running compositor) and
//! its backing pixmap is read with `ShmGetImage`, so the capture keeps working
//! while the window is covered or partly off-screen. Without XComposite we fall
//! back to reading the window itself (only works while it is fully on screen
//! and unobscured); without MIT-SHM (remote X server) we fall back to plain
//! `GetImage`.
//!
//! X11 has no frame-arrived event we can rely on for arbitrary clients, so a
//! dedicated thread polls at `X11Config::poll_hz` (default 60 Hz). Every grab
//! is stamped with `cap_clock::now_ns()` (CLOCK_MONOTONIC), taken as the
//! midpoint of the request round trip. Frames are published via
//! `LatestFrame::publish` only; the thread never blocks on the consumer.
//!
//! Nothing here touches the game process: we only talk to the X server.

use crate::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::composite::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, EventMask, ImageFormat, ImageOrder, MapState, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// Configuration for [`X11Source`].
#[derive(Debug, Clone)]
pub struct X11Config {
    /// X display name (e.g. `":0"`); `None` uses `$DISPLAY`.
    pub display: Option<String>,
    /// Grab rate. The recorder samples the latest frame at its own rate, so
    /// this only needs to be >= the dataset rate; 60 Hz keeps capture latency low.
    pub poll_hz: u32,
    /// Use MIT-SHM when the server supports it (local servers).
    pub use_shm: bool,
    /// Redirect the window with XComposite so covered / off-screen parts are captured.
    pub use_composite: bool,
}

impl Default for X11Config {
    fn default() -> Self {
        Self { display: None, poll_hz: 60, use_shm: true, use_composite: true }
    }
}

/// Counters readable while capture runs.
#[derive(Debug, Default)]
pub struct X11Stats {
    /// Frames published.
    pub frames: AtomicU64,
    /// Polls that produced no frame (window unmapped, transient X errors).
    pub skipped: AtomicU64,
}

/// X11 single-window capture backend.
pub struct X11Source {
    cfg: X11Config,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    info: Arc<Mutex<Option<SourceInfo>>>,
    last_error: Arc<Mutex<Option<String>>>,
    stats: Arc<X11Stats>,
}

impl X11Source {
    pub fn new(cfg: X11Config) -> Self {
        Self {
            cfg,
            running: Arc::new(AtomicBool::new(false)),
            thread: None,
            info: Arc::new(Mutex::new(None)),
            last_error: Arc::new(Mutex::new(None)),
            stats: Arc::new(X11Stats::default()),
        }
    }

    /// True while the grab thread is alive (it exits when the window is
    /// destroyed or the X connection breaks).
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Why the grab thread stopped, if it stopped on its own.
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    pub fn stats(&self) -> &X11Stats {
        &self.stats
    }
}

impl Drop for X11Source {
    fn drop(&mut self) {
        self.stop();
    }
}

impl FrameSource for X11Source {
    fn start(&mut self, target: &WindowTarget, sink: Arc<LatestFrame>) -> Result<()> {
        self.stop();
        let window = u32::try_from(target.native_id)
            .map_err(|_| CaptureError::WindowNotFound(format!("{:#x} is not an X11 window id", target.native_id)))?;
        // Set up on the caller's thread so configuration errors are returned synchronously.
        let mut grabber = Grabber::new(&self.cfg, window)?;
        *self.info.lock().unwrap() = Some(grabber.source_info());
        *self.last_error.lock().unwrap() = None;

        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let info = self.info.clone();
        let last_error = self.last_error.clone();
        let stats = self.stats.clone();
        let period: Nanos = 1_000_000_000 / self.cfg.poll_hz.max(1) as Nanos;
        let thread = std::thread::Builder::new()
            .name("cap-x11-grab".into())
            .spawn(move || {
                let mut next = cap_clock::now_ns();
                while running.load(Ordering::SeqCst) {
                    match grabber.grab() {
                        Ok(Some(frame)) => {
                            let (w, h) = (frame.width, frame.height);
                            sink.publish(frame);
                            stats.frames.fetch_add(1, Ordering::Relaxed);
                            let mut i = info.lock().unwrap();
                            if let Some(i) = i.as_mut() {
                                i.width = w;
                                i.height = h;
                            }
                        }
                        Ok(None) => {
                            stats.skipped.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            tracing::warn!(window = grabber.window, "x11 capture stopped: {e}");
                            *last_error.lock().unwrap() = Some(e.to_string());
                            break;
                        }
                    }
                    next += period;
                    let now = cap_clock::now_ns();
                    if next < now - period {
                        // Fell behind (slow server); don't try to catch up with a burst.
                        next = now;
                    }
                    cap_clock::sleep_until(next);
                }
                running.store(false, Ordering::SeqCst);
                // `grabber` drops here: unredirects the window and frees server resources.
            })
            .map_err(|e| CaptureError::Backend(format!("spawn grab thread: {e}")))?;
        self.thread = Some(thread);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    fn info(&self) -> Option<SourceInfo> {
        self.info.lock().unwrap().clone()
    }
}

fn conn_err(e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(format!("x11: {e}"))
}

/// A SysV shared-memory segment attached to both us and the X server.
struct ShmBuf {
    seg: shm::Seg,
    addr: *mut u8,
    size: usize,
}

// SAFETY: the mapping is process-wide; only the grab thread touches it.
unsafe impl Send for ShmBuf {}

impl ShmBuf {
    fn new(conn: &RustConnection, size: usize) -> Result<Self> {
        // SAFETY: plain SysV shm syscalls; results are checked.
        unsafe {
            let id = libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600);
            if id < 0 {
                return Err(CaptureError::Backend(format!("shmget: {}", std::io::Error::last_os_error())));
            }
            let addr = libc::shmat(id, std::ptr::null(), 0);
            if addr as isize == -1 {
                let e = std::io::Error::last_os_error();
                libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
                return Err(CaptureError::Backend(format!("shmat: {e}")));
            }
            let seg = match conn.generate_id() {
                Ok(s) => s,
                Err(e) => {
                    libc::shmdt(addr);
                    libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
                    return Err(conn_err(e));
                }
            };
            let attached = conn.shm_attach(seg, id as u32, false).map_err(conn_err).and_then(|c| c.check().map_err(conn_err));
            // Once the server has attached (or failed to), mark the segment for
            // removal so it disappears when both sides detach, even on a crash.
            libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
            if let Err(e) = attached {
                libc::shmdt(addr);
                return Err(e);
            }
            Ok(Self { seg, addr: addr as *mut u8, size })
        }
    }

    fn release(&mut self, conn: &RustConnection) {
        let _ = conn.shm_detach(self.seg);
        let _ = conn.flush();
        // SAFETY: addr came from shmat and is detached exactly once.
        unsafe { libc::shmdt(self.addr as *const _) };
        self.addr = std::ptr::null_mut();
    }
}

/// Everything that lives on the grab thread.
struct Grabber {
    conn: RustConnection,
    window: Window,
    composite: bool,
    shm_supported: bool,
    /// Pixmap named from the redirected window; re-named after map/resize.
    pixmap: Option<u32>,
    shm: Option<ShmBuf>,
    width: u16,
    height: u16,
    border: u16,
    depth: u8,
    mapped: bool,
}

#[derive(Debug, thiserror::Error)]
enum GrabError {
    #[error("window was destroyed")]
    Gone,
    #[error("x11 connection: {0}")]
    Conn(#[from] ConnectionError),
}

impl Grabber {
    fn new(cfg: &X11Config, window: Window) -> Result<Self> {
        let (conn, _screen) = RustConnection::connect(cfg.display.as_deref())
            .map_err(|e| CaptureError::Unsupported(format!("cannot connect to X display: {e}")))?;

        if conn.setup().image_byte_order != ImageOrder::LSB_FIRST {
            return Err(CaptureError::Unsupported("big-endian X server image order".into()));
        }

        // Select StructureNotify first, then read the geometry, so no resize is missed.
        let not_found = |e: ReplyError| CaptureError::WindowNotFound(format!("{window:#x}: {e}"));
        conn.change_window_attributes(window, &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY))
            .map_err(conn_err)?
            .check()
            .map_err(not_found)?;
        let geom = conn.get_geometry(window).map_err(conn_err)?.reply().map_err(not_found)?;
        let attrs = conn.get_window_attributes(window).map_err(conn_err)?.reply().map_err(not_found)?;

        let bpp = conn.setup().pixmap_formats.iter().find(|f| f.depth == geom.depth).map(|f| f.bits_per_pixel);
        if bpp != Some(32) || !(geom.depth == 24 || geom.depth == 32) {
            return Err(CaptureError::Unsupported(format!(
                "window depth {} ({bpp:?} bpp); only 24/32-bit TrueColor is supported",
                geom.depth
            )));
        }

        let has_ext = |name: &'static str| conn.extension_information(name).ok().flatten().is_some();
        let mut composite = false;
        if cfg.use_composite && has_ext(composite::X11_EXTENSION_NAME) {
            // NameWindowPixmap needs Composite >= 0.2.
            let v = conn.composite_query_version(0, 4).map_err(conn_err)?.reply().map_err(conn_err)?;
            if (v.major_version, v.minor_version) >= (0, 2) {
                match conn.composite_redirect_window(window, composite::Redirect::AUTOMATIC).map_err(conn_err)?.check() {
                    Ok(()) => composite = true,
                    Err(e) => tracing::warn!("XCompositeRedirectWindow failed ({e}); reading the window directly"),
                }
            }
        } else if cfg.use_composite {
            tracing::warn!("X server lacks XComposite; covered or off-screen parts of the window will not be captured");
        }
        let shm_supported = cfg.use_shm
            && has_ext(shm::X11_EXTENSION_NAME)
            && conn.shm_query_version().ok().and_then(|c| c.reply().ok()).is_some();

        Ok(Self {
            conn,
            window,
            composite,
            shm_supported,
            pixmap: None,
            shm: None,
            width: geom.width,
            height: geom.height,
            border: geom.border_width,
            depth: geom.depth,
            mapped: attrs.map_state != MapState::UNMAPPED,
        })
    }

    fn source_info(&self) -> SourceInfo {
        SourceInfo { backend: "x11", width: self.width as u32, height: self.height as u32, hdr: false }
    }

    fn drop_pixmap(&mut self) {
        if let Some(p) = self.pixmap.take() {
            let _ = self.conn.free_pixmap(p);
        }
    }

    fn drain_events(&mut self) -> std::result::Result<(), GrabError> {
        while let Some(ev) = self.conn.poll_for_event()? {
            match ev {
                Event::ConfigureNotify(e) if e.window == self.window => {
                    if (e.width, e.height, e.border_width) != (self.width, self.height, self.border) {
                        self.width = e.width;
                        self.height = e.height;
                        self.border = e.border_width;
                        // The named pixmap keeps the old size; get a fresh one.
                        self.drop_pixmap();
                    }
                }
                Event::MapNotify(e) if e.window == self.window => {
                    self.mapped = true;
                    self.drop_pixmap();
                }
                Event::UnmapNotify(e) if e.window == self.window => {
                    self.mapped = false;
                    self.drop_pixmap();
                }
                Event::DestroyNotify(e) if e.window == self.window => return Err(GrabError::Gone),
                _ => {}
            }
        }
        Ok(())
    }

    /// One grab. `Ok(None)` means "nothing this time, try again".
    fn grab(&mut self) -> std::result::Result<Option<CapturedFrame>, GrabError> {
        self.drain_events()?;
        if !self.mapped || self.width == 0 || self.height == 0 {
            return Ok(None);
        }

        // Drawable + origin: the named pixmap includes the border.
        let (drawable, x, y) = if self.composite {
            if self.pixmap.is_none() {
                let p = self.conn.generate_id().map_err(|e| match e {
                    ReplyOrIdError::ConnectionError(c) => GrabError::Conn(c),
                    _ => GrabError::Conn(ConnectionError::UnknownError),
                })?;
                match self.conn.composite_name_window_pixmap(self.window, p)?.check() {
                    Ok(()) => self.pixmap = Some(p),
                    // BadMatch: not viewable yet (e.g. parent unmapped). Retry next tick.
                    Err(ReplyError::X11Error(_)) => return Ok(None),
                    Err(ReplyError::ConnectionError(c)) => return Err(c.into()),
                }
            }
            (self.pixmap.unwrap(), self.border as i16, self.border as i16)
        } else {
            (self.window, 0, 0)
        };

        let (w, h) = (self.width, self.height);
        let stride = w as usize * 4;
        let size = stride * h as usize;

        if self.shm_supported && self.shm.as_ref().is_none_or(|s| s.size < size) {
            if let Some(mut old) = self.shm.take() {
                old.release(&self.conn);
            }
            // Round up so small growth doesn't reallocate every time.
            let alloc = size.next_multiple_of(1 << 20);
            match ShmBuf::new(&self.conn, alloc) {
                Ok(b) => self.shm = Some(b),
                Err(e) => {
                    tracing::warn!("MIT-SHM unavailable ({e}); falling back to GetImage");
                    self.shm_supported = false;
                }
            }
        }

        let t0 = cap_clock::now_ns();
        let mut data = if let Some(buf) = self.shm.as_ref() {
            let reply = self
                .conn
                .shm_get_image(drawable, x, y, w, h, !0, ImageFormat::Z_PIXMAP.into(), buf.seg, 0)?
                .reply();
            match reply {
                Ok(r) if r.size as usize >= size => {}
                Ok(_) => return Ok(None),
                Err(ReplyError::X11Error(e)) => return self.on_image_error(e),
                Err(ReplyError::ConnectionError(c)) => return Err(c.into()),
            }
            // SAFETY: the server finished writing (reply received); buffer is >= size.
            unsafe { std::slice::from_raw_parts(buf.addr, size) }.to_vec()
        } else {
            let reply = self.conn.get_image(ImageFormat::Z_PIXMAP, drawable, x, y, w, h, !0)?.reply();
            match reply {
                Ok(r) if r.data.len() >= size => {
                    let mut d = r.data;
                    d.truncate(size);
                    d
                }
                Ok(_) => return Ok(None),
                Err(ReplyError::X11Error(e)) => return self.on_image_error(e),
                Err(ReplyError::ConnectionError(c)) => return Err(c.into()),
            }
        };
        let t1 = cap_clock::now_ns();

        if self.depth == 24 {
            // The pad byte of a 24-bit visual is undefined; make it opaque alpha.
            for px in data.chunks_exact_mut(4) {
                px[3] = 0xff;
            }
        }

        Ok(Some(CapturedFrame {
            capture_ns: t0 + (t1 - t0) / 2,
            width: w as u32,
            height: h as u32,
            format: PixelFormat::Bgra8,
            payload: FramePayload::Cpu { data, stride },
        }))
    }

    fn on_image_error(&mut self, e: x11rb::x11_utils::X11Error) -> std::result::Result<Option<CapturedFrame>, GrabError> {
        // Typically BadMatch (window resized/unmapped between the event and
        // the request, or not fully on screen without Composite) or
        // BadDrawable (pixmap freed on unmap). Re-sync and retry next tick.
        tracing::trace!("x11 GetImage error: {:?}", e.error_kind);
        self.drop_pixmap();
        if let Ok(Ok(g)) = self.conn.get_geometry(self.window).map(|c| c.reply()) {
            self.width = g.width;
            self.height = g.height;
            self.border = g.border_width;
        } else {
            return Err(GrabError::Gone);
        }
        Ok(None)
    }
}

impl Drop for Grabber {
    fn drop(&mut self) {
        self.drop_pixmap();
        if let Some(mut b) = self.shm.take() {
            b.release(&self.conn);
        }
        if self.composite {
            let _ = self.conn.composite_unredirect_window(self.window, composite::Redirect::AUTOMATIC);
        }
        let _ = self.conn.flush();
    }
}
