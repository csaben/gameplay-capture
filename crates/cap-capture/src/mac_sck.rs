//! macOS ScreenCaptureKit single-window capture (macOS 12.3+).
//!
//! - The window is looked up by CGWindowID in `SCShareableContent` and
//!   captured with `SCContentFilter(desktopIndependentWindow:)`, so only that
//!   window's pixels are delivered (not what covers it). Cursor hidden, no audio.
//! - Frames arrive as IOSurface-backed `CVPixelBuffer`s on our own serial
//!   dispatch queue; the handler retains the buffer into
//!   `FramePayload::CvPixelBuffer`, publishes it with `LatestFrame::publish`
//!   and returns. It never blocks.
//! - Timestamps: the sample buffer's presentation time is a host-time
//!   `CMTime`. `CMClockConvertHostTimeToSystemUnits` turns it back into
//!   `mach_absolute_time` ticks, and `cap_clock::ticks_to_ns` puts it on the
//!   same base as `cap_clock::now_ns()`.
//! - Only frames with `SCFrameStatus::Complete` are published (idle/blank
//!   frames carry no new pixels).
//! - Resize: the output size follows the window. Each complete frame's
//!   `ContentRect x ScaleFactor` is compared with the configured size and the
//!   stream is reconfigured (asynchronously) when it changes.
//! - Needs the Screen Recording permission (TCC). Without it `start` asks for
//!   it once (system prompt) and returns `PermissionDenied`.

use crate::macos::RetainedPixelBuffer;
use crate::*;
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass};
use objc2_core_foundation::{CFDictionary, CFRetained, CGRect};
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRectMakeWithDictionaryRepresentation, CGRequestScreenCaptureAccess};
use objc2_core_media::{CMClock, CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamFrameInfoContentRect, SCStreamFrameInfoScaleFactor, SCStreamFrameInfoStatus, SCStreamOutput,
    SCStreamOutputType, SCWindow,
};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// CoreVideo pixel formats we ask ScreenCaptureKit for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SckPixelFormat {
    /// `kCVPixelFormatType_32BGRA`.
    #[default]
    Bgra,
    /// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` ('420v'); lets
    /// VideoToolbox skip a colour conversion.
    Nv12,
}

impl SckPixelFormat {
    fn ostype(self) -> u32 {
        match self {
            SckPixelFormat::Bgra => u32::from_be_bytes(*b"BGRA"),
            SckPixelFormat::Nv12 => u32::from_be_bytes(*b"420v"),
        }
    }
    fn pixel_format(self) -> PixelFormat {
        match self {
            SckPixelFormat::Bgra => PixelFormat::Bgra8,
            SckPixelFormat::Nv12 => PixelFormat::Nv12,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SckConfig {
    pub pixel_format: SckPixelFormat,
    /// Upper bound on delivered frame rate (ScreenCaptureKit `minimumFrameInterval`).
    pub max_fps: u32,
    /// ScreenCaptureKit surface queue depth (3..=8).
    pub queue_depth: isize,
    /// Timeout for ScreenCaptureKit completion handlers during `start`/`stop`.
    pub timeout: Duration,
}

impl Default for SckConfig {
    fn default() -> Self {
        Self { pixel_format: SckPixelFormat::Bgra, max_fps: 60, queue_depth: 5, timeout: Duration::from_secs(10) }
    }
}

/// Counters readable while capture runs.
#[derive(Debug, Default)]
pub struct SckStats {
    pub frames: AtomicU64,
    /// Sample buffers without new pixels (idle / blank / suspended).
    pub skipped: AtomicU64,
    pub reconfigures: AtomicU64,
}

/// Host-time CMTime -> `cap_clock` ns (mach_absolute_time base).
pub fn host_time_to_ns(t: CMTime) -> Option<Nanos> {
    if !t.flags.contains(CMTimeFlags::Valid) || t.timescale <= 0 {
        return None;
    }
    // SAFETY: pure conversion function.
    let ticks = unsafe { CMClock::convert_host_time_to_system_units(t) };
    Some(cap_clock::ticks_to_ns(ticks as i64))
}

/// Shared between the output object (dispatch queue) and `SckSource`.
struct Shared {
    sink: Arc<LatestFrame>,
    stats: Arc<SckStats>,
    info: Arc<Mutex<Option<SourceInfo>>>,
    last_error: Arc<Mutex<Option<String>>>,
    cfg: SckConfig,
    /// Currently configured output size (pixels), packed w << 16 | h is not
    /// enough for 8K+ so keep two atomics.
    cfg_w: AtomicU32,
    cfg_h: AtomicU32,
}

fn make_config(cfg: &SckConfig, w: u32, h: u32) -> Retained<SCStreamConfiguration> {
    // SAFETY: plain property setters on a fresh configuration object.
    unsafe {
        let c = SCStreamConfiguration::new();
        c.setWidth(w.max(2) as usize);
        c.setHeight(h.max(2) as usize);
        c.setPixelFormat(cfg.pixel_format.ostype());
        c.setShowsCursor(false);
        c.setCapturesAudio(false);
        c.setQueueDepth(cfg.queue_depth.clamp(3, 8));
        c.setMinimumFrameInterval(CMTime {
            value: 1,
            timescale: cfg.max_fps.max(1) as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        });
        c
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; we don't implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "CapCaptureSckOutput"]
    #[ivars = Shared]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(&self, stream: &SCStream, sample_buffer: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Screen {
                self.on_sample(stream, sample_buffer);
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            let msg = error.localizedDescription().to_string();
            tracing::warn!("ScreenCaptureKit stream stopped: {msg}");
            *self.ivars().last_error.lock().unwrap() = Some(msg);
        }
    }
);

/// Frame attachment dictionary values we care about.
struct FrameInfo {
    status: Option<isize>,
    /// Content rect in points and the points->pixels scale.
    content: Option<(CGRect, f64)>,
}

unsafe fn frame_info(sample: &CMSampleBuffer) -> FrameInfo {
    let mut out = FrameInfo { status: None, content: None };
    let Some(arr) = sample.sample_attachments_array(false) else { return out };
    // SAFETY: CFArray / CFDictionary are toll-free bridged with NSArray / NSDictionary.
    let arr: &NSArray<NSDictionary<NSString, AnyObject>> = &*(CFRetained::as_ptr(&arr).as_ptr() as *const _);
    let Some(dict) = arr.firstObject() else { return out };
    let num = |key: &NSString| dict.objectForKey(key).and_then(|o| o.downcast::<NSNumber>().ok());
    out.status = num(SCStreamFrameInfoStatus).map(|n| n.integerValue());
    let scale = num(SCStreamFrameInfoScaleFactor).map(|n| n.doubleValue());
    if let (Some(rect_obj), Some(scale)) = (dict.objectForKey(SCStreamFrameInfoContentRect), scale) {
        let mut rect = CGRect::default();
        let cf: &CFDictionary = &*(Retained::as_ptr(&rect_obj) as *const CFDictionary);
        if CGRectMakeWithDictionaryRepresentation(Some(cf), &mut rect) {
            out.content = Some((rect, scale));
        }
    }
    out
}

impl StreamOutput {
    fn new(shared: Shared) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }

    unsafe fn on_sample(&self, stream: &SCStream, sample: &CMSampleBuffer) {
        let sh = self.ivars();
        if !sample.is_valid() {
            return;
        }
        let info = frame_info(sample);
        if info.status != Some(SCFrameStatus::Complete.0) {
            sh.stats.skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(image) = sample.image_buffer() else {
            sh.stats.skipped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let capture_ns = host_time_to_ns(sample.presentation_time_stamp()).unwrap_or_else(cap_clock::now_ns);
        // Hand the +1 reference from CFRetained to RetainedPixelBuffer.
        let pb = RetainedPixelBuffer(CFRetained::into_raw(image).as_ptr() as *mut c_void);
        let (w, h) = (pb.width() as u32, pb.height() as u32);

        sh.sink.publish(CapturedFrame {
            capture_ns,
            width: w,
            height: h,
            format: sh.cfg.pixel_format.pixel_format(),
            payload: FramePayload::CvPixelBuffer(pb),
        });
        sh.stats.frames.fetch_add(1, Ordering::Relaxed);
        if let Some(i) = sh.info.lock().unwrap().as_mut() {
            (i.width, i.height) = (w, h);
        }

        // Follow window resizes (asynchronous; no waiting here).
        if let Some((rect, scale)) = info.content {
            let want_w = (rect.size.width * scale).round() as u32;
            let want_h = (rect.size.height * scale).round() as u32;
            let (cw, ch) = (sh.cfg_w.load(Ordering::Relaxed), sh.cfg_h.load(Ordering::Relaxed));
            if want_w >= 2 && want_h >= 2 && (want_w.abs_diff(cw) > 1 || want_h.abs_diff(ch) > 1) {
                sh.cfg_w.store(want_w, Ordering::Relaxed);
                sh.cfg_h.store(want_h, Ordering::Relaxed);
                sh.stats.reconfigures.fetch_add(1, Ordering::Relaxed);
                stream.updateConfiguration_completionHandler(&make_config(&sh.cfg, want_w, want_h), None);
            }
        }
    }
}

/// Wrapper to move Objective-C objects through a channel from a completion
/// handler (ScreenCaptureKit objects are safe to use from any thread).
struct SendBox<T>(T);
// SAFETY: see above.
unsafe impl<T> Send for SendBox<T> {}

fn ns_err(e: *mut NSError) -> Option<String> {
    // SAFETY: e is either null or a valid NSError for the duration of the callback.
    unsafe { e.as_ref() }.map(|e| e.localizedDescription().to_string())
}

fn shareable_content(timeout: Duration) -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel::<SendBox<std::result::Result<Retained<SCShareableContent>, String>>>();
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        // SAFETY: content is null or a valid object we retain before returning.
        let r = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(c),
            None => Err(ns_err(err).unwrap_or_else(|| "no shareable content".into())),
        };
        let _ = tx.send(SendBox(r));
    });
    // SAFETY: the block lives until the call returns; SCK copies it.
    unsafe { SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(true, false, &block) };
    match rx.recv_timeout(timeout) {
        Ok(SendBox(Ok(c))) => Ok(c),
        // A denied TCC permission surfaces here as an error.
        Ok(SendBox(Err(e))) => Err(CaptureError::Backend(format!("SCShareableContent: {e}"))),
        Err(_) => Err(CaptureError::Backend("SCShareableContent timed out".into())),
    }
}

fn wait_completion(timeout: Duration, f: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let block = RcBlock::new(move |err: *mut NSError| {
        let _ = tx.send(ns_err(err));
    });
    f(&block);
    match rx.recv_timeout(timeout) {
        Ok(None) => Ok(()),
        Ok(Some(e)) => Err(CaptureError::Backend(e)),
        Err(_) => Err(CaptureError::Backend("ScreenCaptureKit completion timed out".into())),
    }
}

struct Running {
    stream: Retained<SCStream>,
    _output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

/// ScreenCaptureKit single-window capture backend.
pub struct SckSource {
    cfg: SckConfig,
    running: Option<Running>,
    info: Arc<Mutex<Option<SourceInfo>>>,
    last_error: Arc<Mutex<Option<String>>>,
    stats: Arc<SckStats>,
}

// SAFETY: SCStream and our output object may be used from any thread; the
// dispatch queue handle is thread-safe.
unsafe impl Send for SckSource {}

impl SckSource {
    pub fn new(cfg: SckConfig) -> Self {
        Self {
            cfg,
            running: None,
            info: Arc::new(Mutex::new(None)),
            last_error: Arc::new(Mutex::new(None)),
            stats: Arc::new(SckStats::default()),
        }
    }

    pub fn stats(&self) -> &SckStats {
        &self.stats
    }

    /// Set when ScreenCaptureKit stopped the stream (window closed, permission revoked...).
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    /// Does this process currently hold the Screen Recording permission?
    pub fn has_permission() -> bool {
        CGPreflightScreenCaptureAccess()
    }
}

impl Drop for SckSource {
    fn drop(&mut self) {
        self.stop();
    }
}

impl FrameSource for SckSource {
    fn start(&mut self, target: &WindowTarget, sink: Arc<LatestFrame>) -> Result<()> {
        self.stop();
        if !CGPreflightScreenCaptureAccess() {
            // Shows the system prompt once; the user must grant it in System
            // Settings > Privacy & Security > Screen Recording and restart us.
            let _ = CGRequestScreenCaptureAccess();
            return Err(CaptureError::PermissionDenied);
        }
        let window_id = u32::try_from(target.native_id)
            .map_err(|_| CaptureError::WindowNotFound(format!("{:#x} is not a CGWindowID", target.native_id)))?;

        let content = shareable_content(self.cfg.timeout)?;
        // SAFETY: plain getters.
        let window: Retained<SCWindow> = unsafe { content.windows() }
            .iter()
            .find(|w| unsafe { w.windowID() } == window_id)
            .ok_or_else(|| CaptureError::WindowNotFound(format!("CGWindowID {window_id}")))?;

        // SAFETY: standard initialisers / getters.
        let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window) };
        let frame = unsafe { window.frame() };
        // pointPixelScale exists on macOS 14+; before that start at 1x and let
        // the ScaleFactor attachment of the first frame correct the size.
        let scale = if filter.respondsToSelector(sel!(pointPixelScale)) {
            unsafe { filter.pointPixelScale() as f64 }
        } else {
            1.0
        };
        let (w, h) = ((frame.size.width * scale).round() as u32, (frame.size.height * scale).round() as u32);

        *self.info.lock().unwrap() = Some(SourceInfo { backend: "screencapturekit", width: w, height: h, hdr: false });
        *self.last_error.lock().unwrap() = None;

        let output = StreamOutput::new(Shared {
            sink,
            stats: self.stats.clone(),
            info: self.info.clone(),
            last_error: self.last_error.clone(),
            cfg: self.cfg.clone(),
            cfg_w: AtomicU32::new(w),
            cfg_h: AtomicU32::new(h),
        });
        let config = make_config(&self.cfg, w, h);
        let delegate = ProtocolObject::<dyn SCStreamDelegate>::from_ref(&*output);
        // SAFETY: filter/config are valid; the delegate is kept alive in `Running`.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, Some(delegate))
        };
        let queue = DispatchQueue::new("cap-capture.sck", None);
        let out_proto = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*output);
        // SAFETY: serial queue owned by us; output kept alive in `Running`.
        unsafe { stream.addStreamOutput_type_sampleHandlerQueue_error(out_proto, SCStreamOutputType::Screen, Some(&queue)) }
            .map_err(|e| CaptureError::Backend(format!("addStreamOutput: {}", e.localizedDescription())))?;
        // SAFETY: the completion block outlives the call (SCK copies it).
        wait_completion(self.cfg.timeout, |b| unsafe { stream.startCaptureWithCompletionHandler(Some(b)) })
            .map_err(|e| match e {
                CaptureError::Backend(m) if m.contains("declined") || m.contains("TCC") => CaptureError::PermissionDenied,
                other => other,
            })?;
        self.running = Some(Running { stream, _output: output, _queue: queue });
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            // SAFETY: as in start.
            let _ = wait_completion(self.cfg.timeout, |b| unsafe { r.stream.stopCaptureWithCompletionHandler(Some(b)) });
        }
    }

    fn info(&self) -> Option<SourceInfo> {
        self.info.lock().unwrap().clone()
    }
}
