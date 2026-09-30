//! The recorder: threads, channels, segmenting and drop accounting.
//!
//! ```text
//!  FrameSource callback ──► LatestFrame mailbox
//!                                │ (sampled)
//!  ticker thread (cap_clock, absolute deadlines) ──bounded(4), try_send──► encoder thread
//!        ▲ reads effective pause                                            │ bounded(4), blocking send
//!        │                                                                  ▼
//!  input/gate thread ◄── InputSources (bounded, try_send)             writer thread ──► finalizer thread
//!        ▲         └── gated inputs + focus rows (bounded, try_send) ──►   (muxer, journal)   (parquet, hashes,
//!  FocusTracker ─┘                                                                              manifest, rename,
//!                                                                                               on_segment_complete)
//! ```
//!
//! Timing and segment rules (all times on `cap_clock`):
//! - Ticks fire on an absolute grid `t0 + k * period` (never drifts). A tick
//!   that is late by one period or more skips the missed grid points; each
//!   missed point inside an active segment counts as a dropped frame.
//! - `FrameRecord.tick_ns` is the *scheduled* grid time (actual firing jitter is
//!   reported in `RecorderStats::jitter`).
//! - A segment starts at the first tick that has a frame (ticks before any
//!   frame ever arrived are skipped, not dropped) and spans exactly
//!   `settings.frames_per_segment()` tick slots (dropped slots included), so
//!   `t_end = t_start + slots * period` and consecutive segments are contiguous.
//! - A segment owns the inputs with `t_start <= t_ns < t_end` and the focus
//!   state at `t_start` (first row of focus.parquet) plus changes in range.
//! - Backpressure: the ticker never blocks. If the ticker→encoder queue (4) is
//!   full the newest frame is dropped and counted in that segment's
//!   `dropped_frames`. The encoder→writer queue (4) blocks the encoder instead
//!   of dropping, because dropping an already-encoded packet would corrupt the
//!   P-frame chain; a slow writer therefore surfaces as ticker-side drops.
//! - Frame indices (and muxer pts) are assigned by the encoder thread only to
//!   frames it actually encoded, so `frame_idx` is continuous from 0 in each
//!   segment and equals the frame's position in `video.mp4`. The first frame of
//!   every segment is a forced keyframe.
//! - Held state carries across segment boundaries: `inputs.parquet` of every
//!   segment after the first starts with "still held" rows at `t_start_ns`
//!   (`key_down`, `mouse_button` 1, `button`/`axis` with their last value) for
//!   everything held at the end of the previous segment ([`CarriedInputs`]).
//!   Not applied to segments rebuilt by `recover_partials` after a crash.
//! - Each tick uses the newest source frame captured at or before the tick
//!   (`capture_ns <= tick_ns`, non-decreasing).
//! - `repeated` = this frame's source frame is the same as the previous frame
//!   sent in this segment (the window did not produce a new frame).
//! - Pause (the shared `paused` flag: chat pause, disk cap, blocked game) and
//!   `stop()`: the input thread closes the gate first (synthesising releases
//!   for held keys/buttons at that instant), then publishes the *effective*
//!   pause; the ticker ends the current segment at its next tick, flushing the
//!   encoder, so the releases always fall inside the closing segment. While
//!   paused no frames are written and no inputs are logged; resuming starts a
//!   new segment (new index, keyframe, frame_idx 0).

use crate::backend::{FfmpegEncoder, FfmpegSinkFactory, FrameEncoder, SegmentSink, SinkFactory};
use crate::finalize::{finalize_segment, os_string, SegmentTables};
use crate::gate::{CarriedInputs, InputGate};
use crate::journal::{Entry, JournalWriter, JOURNAL_FILE};
use cap_capture::{CapturedFrame, FrameSource, LatestFrame, WindowTarget};
use cap_encode::{EncodedPacket, EncoderConfig, StreamParams};
use cap_focus::FocusTracker;
use cap_input::InputSource;
use cap_types::{
    files, segment_dir_name, CaptureSettings, FocusRecord, FrameRecord, InputEvent, Manifest, Nanos, SCHEMA_VERSION,
};
use crossbeam_channel::{bounded, never, select, unbounded, Receiver, Sender, TrySendError};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::*};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Capacity of the ticker→encoder and encoder→writer queues (spec: 4).
pub const QUEUE_DEPTH: usize = 4;
/// Capacity of the raw input and the gated-log queues.
const INPUT_QUEUE: usize = 65_536;
/// How long after `t_end` the writer keeps collecting a segment's inputs
/// before closing it (input backends deliver with a few ms of latency).
const INPUT_GRACE_NS: Nanos = 50_000_000;
/// Buffered inputs newer than every open segment are kept this long waiting
/// for the next segment to start.
const FUTURE_INPUT_TTL_NS: Nanos = 3_000_000_000;

#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error("encoder: {0}")]
    Encode(#[from] cap_encode::EncodeError),
    #[error("capture: {0}")]
    Capture(#[from] cap_capture::CaptureError),
    #[error("input source {0}: {1}")]
    Input(&'static str, cap_input::InputError),
    #[error("focus tracker: {0}")]
    Focus(#[from] cap_focus::FocusError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid config: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, RecorderError>;

/// Called (on the finalizer thread) with each completed segment folder.
pub type SegmentCallback = Box<dyn FnMut(PathBuf) + Send>;

#[derive(Debug, Clone)]
pub struct RecorderConfig {
    pub target: WindowTarget,
    pub settings: CaptureSettings,
    pub encoder: EncoderConfig,
    /// `sessions/`; segments go to `<sessions_root>/<session_id>/seg_NNNNNN/`.
    pub sessions_root: PathBuf,
    pub session_id: String,
    pub latency_offset_ns: Nanos,
    pub client_version: String,
    /// Shared pause flag (chat pause hotkey, disk cap, blocked game). Setting
    /// it stops input logging and frame writing and ends the current segment.
    pub paused: Arc<AtomicBool>,
    /// First segment index (use >0 to continue a session after a restart).
    pub first_segment_idx: u32,
}

impl RecorderConfig {
    pub fn new(target: WindowTarget, settings: CaptureSettings, sessions_root: PathBuf, session_id: String) -> Self {
        let encoder = EncoderConfig::new(settings.width, settings.height, settings.rate_hz);
        Self {
            target,
            settings,
            encoder,
            sessions_root,
            session_id,
            latency_offset_ns: 0,
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            paused: Arc::new(AtomicBool::new(false)),
            first_segment_idx: 0,
        }
    }
}

/// What an [`Observer`] is shown.
#[derive(Debug, Clone, Copy)]
pub enum Observed<'a> {
    /// A raw input event, *before* focus/pause gating (so hotkeys are seen even
    /// while paused). `focused` is the gate's current view of whether the
    /// target game has focus.
    Input { event: &'a InputEvent, focused: bool },
    /// A focus change from the tracker (`game_id` = foreground identity).
    Focus(&'a FocusRecord),
}

/// Optional tap on the recorder's input/focus streams, for app-level hotkeys
/// (chat pause, one-key pause) and policy (pause while a blocked game is in
/// front) without a second set of OS input hooks.
///
/// Runs inline on the input-gate thread for every event: it must be cheap and
/// must never block (setting an atomic such as `RecorderConfig::paused` is the
/// intended use). A pause requested from here takes effect for the next event.
pub type Observer = Box<dyn FnMut(Observed<'_>) + Send>;

/// The platform pieces a recorder drives.
pub struct Sources {
    pub frames: Box<dyn FrameSource>,
    pub inputs: Vec<Box<dyn InputSource>>,
    pub focus: Box<dyn FocusTracker>,
    /// See [`Observer`]. `None` for no tap.
    pub observer: Option<Observer>,
}

impl Sources {
    pub fn new(frames: Box<dyn FrameSource>, inputs: Vec<Box<dyn InputSource>>, focus: Box<dyn FocusTracker>) -> Self {
        Self { frames, inputs, focus, observer: None }
    }
    pub fn with_observer(mut self, observer: Observer) -> Self {
        self.observer = Some(observer);
        self
    }
}

/// Tick timing: lateness of each tick relative to its scheduled deadline.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct JitterStats {
    pub ticks: u64,
    pub mean_late_ns: f64,
    pub max_late_ns: i64,
    /// Ticks that fired more than 1 ms late.
    pub over_1ms: u64,
    /// Grid points skipped because a tick was a full period (or more) late.
    pub missed_ticks: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecorderStats {
    /// Frames encoded and handed to the muxer (= rows in frames.parquet).
    pub frames_written: u64,
    /// Ticks inside a segment that produced no video frame (queue full, missed
    /// tick, encode error). Sum of manifests' `dropped_frames`.
    pub frames_dropped: u64,
    pub repeated_frames: u64,
    /// repeated_frames / frames sent to the encoder.
    pub repeated_ratio: f64,
    /// Input events logged (after gating), including synthesised releases.
    pub input_events: u64,
    /// Input events discarded because the game was unfocused or recording paused
    /// (also duplicate presses / releases of keys never seen pressed).
    pub input_events_gated: u64,
    /// Input events lost to full queues or that fell outside every segment
    /// (e.g. before the first frame).
    pub input_events_lost: u64,
    pub segments_done: u64,
    pub segment_errors: u64,
    /// Segment currently being recorded by the ticker, if any.
    pub current_segment: Option<u32>,
    pub paused: bool,
    pub jitter: JitterStats,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct Counters {
    frames_sent: AtomicU64,
    frames_written: AtomicU64,
    dropped: AtomicU64,
    repeated: AtomicU64,
    inputs: AtomicU64,
    gated: AtomicU64,
    lost: AtomicU64,
    segments_done: AtomicU64,
    segment_errors: AtomicU64,
    ticks: AtomicU64,
    late_sum: AtomicI64,
    late_max: AtomicI64,
    over_1ms: AtomicU64,
    missed: AtomicU64,
}

struct Shared {
    c: Counters,
    stop: AtomicBool,
    /// Raw pause request (shared with the app).
    paused: Arc<AtomicBool>,
    /// Pause confirmed by the input gate (releases already logged).
    eff_paused: AtomicBool,
    /// Time at which the gate closed for the current pause/stop.
    gate_closed_at: AtomicI64,
    /// Current ticker segment, -1 if none.
    current: AtomicI64,
    /// The encoder has been opened for the capture's frames (see
    /// `FrameEncoder::prepare`); the ticker opens no segment before that.
    enc_ready: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl Shared {
    /// The input gate confirmed a pause/stop that began before grid time `t`
    /// (so its synthesised releases fall before a segment end at `t`).
    fn gate_closed_before(&self, t: Nanos) -> bool {
        self.eff_paused.load(SeqCst) && self.gate_closed_at.load(SeqCst) < t
    }
    fn error(&self, msg: String) {
        tracing::error!("{msg}");
        *self.last_error.lock().unwrap() = Some(msg);
    }
}

pub struct Recorder {
    shared: Arc<Shared>,
    sources: Sources,
    input_stop: Arc<AtomicBool>,
    ticker: Option<JoinHandle<()>>,
    encoder: Option<JoinHandle<()>>,
    input: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    finalizer: Option<JoinHandle<()>>,
}

// ---------------------------------------------------------------------------
// messages

enum EncMsg {
    Frame { seg: u32, t_start: Nanos, frame: Arc<CapturedFrame>, tick_ns: Nanos, repeated: bool },
    End { seg: u32, t_end: Nanos, dropped: u64, flush: bool },
}

enum WMsg {
    Start { seg: u32, t_start: Nanos, start_pts: i64, params: StreamParams },
    Frame { seg: u32, rec: FrameRecord },
    Packet(EncodedPacket),
    /// An encode error cost this segment a frame.
    Dropped { seg: u32 },
    End { seg: u32, t_end: Nanos, dropped: u64, frames: u32, flushed: bool },
}

enum LogMsg {
    Input(InputEvent),
    Focus(FocusRecord),
}

struct Finalize {
    partial: PathBuf,
    manifest: Manifest,
    tables: SegmentTables,
}

// ---------------------------------------------------------------------------

impl Recorder {
    /// Opens the hardware encoder (`cap_encode`), starts all sources and threads.
    pub fn start(cfg: RecorderConfig, sources: Sources, on_segment_complete: SegmentCallback) -> Result<Self> {
        let enc = FfmpegEncoder::open(&cfg.encoder)?;
        Self::start_with_backend(cfg, sources, Box::new(enc), Box::new(FfmpegSinkFactory), on_segment_complete)
    }

    /// Like `start`, with an explicit encoder / muxer (tests, dry runs).
    pub fn start_with_backend(
        cfg: RecorderConfig,
        mut sources: Sources,
        encoder: Box<dyn FrameEncoder>,
        sinks: Box<dyn SinkFactory>,
        on_segment_complete: SegmentCallback,
    ) -> Result<Self> {
        let s = &cfg.settings;
        if s.rate_hz == 0 || s.segment_secs == 0 {
            return Err(RecorderError::Config("rate_hz and segment_secs must be > 0".into()));
        }
        if cfg.encoder.rate_hz != s.rate_hz {
            tracing::warn!(enc = cfg.encoder.rate_hz, rec = s.rate_hz, "encoder rate differs from tick rate; using tick rate");
        }
        let session_dir = cfg.sessions_root.join(&cfg.session_id);
        std::fs::create_dir_all(&session_dir)?;

        let shared = Arc::new(Shared {
            c: Counters::default(),
            stop: AtomicBool::new(false),
            eff_paused: AtomicBool::new(cfg.paused.load(SeqCst)),
            gate_closed_at: AtomicI64::new(Nanos::MIN),
            paused: cfg.paused.clone(),
            current: AtomicI64::new(-1),
            enc_ready: AtomicBool::new(false),
            last_error: Mutex::new(None),
        });

        let template = Manifest {
            schema_version: SCHEMA_VERSION,
            session_id: cfg.session_id.clone(),
            segment_idx: 0,
            client_version: cfg.client_version.clone(),
            os: os_string(),
            gpu: cap_encode::gpu_name(),
            encoder: String::new(),
            encoder_params: Default::default(),
            width: s.width,
            height: s.height,
            rate_hz: s.rate_hz,
            game_id: cfg.target.game_id.clone(),
            t_start_ns: 0,
            t_end_ns: 0,
            frame_count: 0,
            dropped_frames: 0,
            latency_offset_ns: cfg.latency_offset_ns,
            blake3: Default::default(),
            sizes: Default::default(),
        };

        // Channels.
        let (enc_tx, enc_rx) = bounded::<EncMsg>(QUEUE_DEPTH);
        let (w_tx, w_rx) = bounded::<WMsg>(QUEUE_DEPTH);
        let (in_tx, in_rx) = bounded::<InputEvent>(INPUT_QUEUE);
        let (focus_tx, focus_rx) = unbounded::<FocusRecord>();
        let (log_tx, log_rx) = bounded::<LogMsg>(INPUT_QUEUE);
        let (fin_tx, fin_rx) = unbounded::<Finalize>();
        let latest = LatestFrame::new();
        let input_stop = Arc::new(AtomicBool::new(false));

        // Threads first (they idle until data arrives), then sources.
        let finalizer = {
            let sh = shared.clone();
            let mut cb = on_segment_complete;
            spawn("cap-finalize", move || finalizer_thread(sh, fin_rx, &mut cb))
        };
        let writer = {
            let sh = shared.clone();
            let period = s.tick_ns();
            spawn("cap-writer", move || {
                Writer::new(sh, session_dir, sinks, template, period, fin_tx).run(w_rx, log_rx)
            })
        };
        let encoder_h = {
            let sh = shared.clone();
            let latest = latest.clone();
            spawn("cap-encode", move || encoder_thread(sh, encoder, latest, enc_rx, w_tx))
        };
        let input = {
            let sh = shared.clone();
            let stop = input_stop.clone();
            let observer = sources.observer.take();
            spawn("cap-input-gate", move || input_thread(sh, stop, in_rx, focus_rx, log_tx, observer))
        };

        let mut rec = Recorder {
            shared: shared.clone(),
            sources: Sources::new(Box::new(NullSource), Vec::new(), Box::new(NullFocus)),
            input_stop,
            ticker: None,
            encoder: Some(encoder_h),
            input: Some(input),
            writer: Some(writer),
            finalizer: Some(finalizer),
        };

        // Start sources; on failure tear everything down.
        let started = (|| -> Result<()> {
            sources.focus.start(&cfg.target.game_id, focus_tx)?;
            for src in sources.inputs.iter_mut() {
                src.start(in_tx.clone()).map_err(|e| RecorderError::Input(src.name(), e))?;
            }
            sources.frames.start(&cfg.target, latest.clone())?;
            Ok(())
        })();
        drop(in_tx);
        rec.sources = sources;
        let ticker = {
            let sh = shared.clone();
            let settings = cfg.settings.clone();
            let first = cfg.first_segment_idx;
            spawn("cap-ticker", move || ticker_thread(sh, latest, enc_tx, settings, first))
        };
        rec.ticker = Some(ticker);
        if let Err(e) = started {
            rec.shutdown();
            return Err(e);
        }
        Ok(rec)
    }

    /// The shared pause flag (same `Arc` as `RecorderConfig::paused`).
    pub fn paused_flag(&self) -> Arc<AtomicBool> {
        self.shared.paused.clone()
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, SeqCst);
    }

    pub fn stats(&self) -> RecorderStats {
        let c = &self.shared.c;
        let sent = c.frames_sent.load(Relaxed);
        let rep = c.repeated.load(Relaxed);
        let ticks = c.ticks.load(Relaxed);
        let cur = self.shared.current.load(Relaxed);
        RecorderStats {
            frames_written: c.frames_written.load(Relaxed),
            frames_dropped: c.dropped.load(Relaxed),
            repeated_frames: rep,
            repeated_ratio: if sent > 0 { rep as f64 / sent as f64 } else { 0.0 },
            input_events: c.inputs.load(Relaxed),
            input_events_gated: c.gated.load(Relaxed),
            input_events_lost: c.lost.load(Relaxed),
            segments_done: c.segments_done.load(Relaxed),
            segment_errors: c.segment_errors.load(Relaxed),
            current_segment: (cur >= 0).then_some(cur as u32),
            paused: self.shared.paused.load(Relaxed),
            jitter: JitterStats {
                ticks,
                mean_late_ns: if ticks > 0 { c.late_sum.load(Relaxed) as f64 / ticks as f64 } else { 0.0 },
                max_late_ns: c.late_max.load(Relaxed),
                over_1ms: c.over_1ms.load(Relaxed),
                missed_ticks: c.missed.load(Relaxed),
            },
            last_error: self.shared.last_error.lock().unwrap().clone(),
        }
    }

    /// Ends the current segment (short final segment), flushes the encoder,
    /// finalizes everything and joins all threads. Held keys are released at
    /// the stop instant inside the final segment.
    pub fn stop(mut self) -> RecorderStats {
        self.shutdown();
        self.stats()
    }

    fn shutdown(&mut self) {
        // 1. Gate closes (releases logged) and the ticker ends the segment.
        self.shared.stop.store(true, SeqCst);
        join(self.ticker.take());
        // 2. No more frames needed.
        self.sources.frames.stop();
        // 3. Encoder drains (it exits when the ticker's sender is gone).
        join(self.encoder.take());
        // 4. Input side.
        for s in self.sources.inputs.iter_mut() {
            s.stop();
        }
        self.sources.focus.stop();
        self.input_stop.store(true, SeqCst);
        join(self.input.take());
        // 5. Writer drains inputs + packets, hands off last segments; finalizer writes them.
        join(self.writer.take());
        join(self.finalizer.take());
        self.shared.current.store(-1, SeqCst);
        let lost: u64 = self.sources.inputs.iter().map(|s| s.dropped()).sum();
        self.shared.c.lost.fetch_add(lost, Relaxed);
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.writer.is_some() {
            self.shutdown();
        }
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    std::thread::Builder::new().name(name.into()).spawn(f).expect("spawn thread")
}

fn join(h: Option<JoinHandle<()>>) {
    if let Some(h) = h {
        if h.join().is_err() {
            tracing::error!("recorder thread panicked");
        }
    }
}

struct NullSource;
impl FrameSource for NullSource {
    fn start(&mut self, _: &WindowTarget, _: Arc<LatestFrame>) -> cap_capture::Result<()> {
        Ok(())
    }
    fn stop(&mut self) {}
    fn info(&self) -> Option<cap_capture::SourceInfo> {
        None
    }
}
struct NullFocus;
impl FocusTracker for NullFocus {
    fn list_windows(&self) -> cap_focus::Result<Vec<cap_focus::WindowInfo>> {
        Ok(Vec::new())
    }
    fn start(&mut self, _: &str, _: Sender<FocusRecord>) -> cap_focus::Result<()> {
        Ok(())
    }
    fn stop(&mut self) {}
}

// ---------------------------------------------------------------------------
// ticker

struct TickSeg {
    idx: u32,
    t_start: Nanos,
    slots: u32,
    dropped: u64,
    last_sent_seq: Option<u64>,
}

struct Ticker {
    sh: Arc<Shared>,
    tx: Sender<EncMsg>,
    /// Control messages that could not be queued yet. Frames are never queued
    /// here: while it is non-empty, frames are dropped (keeps ordering).
    outbox: VecDeque<EncMsg>,
    seg: Option<TickSeg>,
    next_idx: u32,
    period: Nanos,
    slots_per_seg: u32,
    encoder_gone: bool,
    /// Last sampled source frame.
    prev: Option<(u64, Arc<CapturedFrame>)>,
}

impl Ticker {
    fn flush_outbox(&mut self) -> bool {
        while let Some(m) = self.outbox.pop_front() {
            match self.tx.try_send(m) {
                Ok(()) => {}
                Err(TrySendError::Full(m)) => {
                    self.outbox.push_front(m);
                    return false;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.encoder_gone = true;
                    self.outbox.clear();
                    return false;
                }
            }
        }
        true
    }

    fn close(&mut self, flush: bool) {
        if let Some(s) = self.seg.take() {
            let t_end = s.t_start + s.slots as Nanos * self.period;
            self.outbox.push_back(EncMsg::End { seg: s.idx, t_end, dropped: s.dropped, flush });
            self.flush_outbox();
            self.sh.current.store(-1, Relaxed);
        }
    }

    fn drop_frame(&mut self) {
        if let Some(s) = self.seg.as_mut() {
            s.dropped += 1;
            self.sh.c.dropped.fetch_add(1, Relaxed);
        }
    }

    /// A grid point that was skipped because the ticker woke up too late.
    fn missed(&mut self) {
        if self.seg.as_ref().is_some_and(|s| s.slots >= self.slots_per_seg) {
            self.close(false);
        }
        if let Some(s) = self.seg.as_mut() {
            s.slots += 1;
            self.sh.c.missed.fetch_add(1, Relaxed);
            self.drop_frame();
        }
    }

    fn tick(&mut self, t: Nanos, latest: &LatestFrame) {
        if self.sh.gate_closed_before(t) {
            self.close(true);
            return;
        }
        if self.seg.as_ref().is_some_and(|s| s.slots >= self.slots_per_seg) {
            self.close(false);
        }
        let (seq, frame) = latest.latest();
        let Some(frame) = frame else { return };
        // The tick is stamped with its scheduled grid time, but sampling happens
        // a few µs later; a frame captured in that gap is "after" the tick, so
        // use the previously sampled frame instead (keeps capture_ns <= tick_ns
        // and capture_ns monotonic).
        let (seq, frame) = match &self.prev {
            Some((pseq, pframe)) if frame.capture_ns > t && pframe.capture_ns <= t => (*pseq, pframe.clone()),
            None if frame.capture_ns > t => return, // very first frame: wait a tick
            _ => (seq, frame),
        };
        if self.seg.is_none() && !self.sh.enc_ready.load(SeqCst) {
            return; // encoder still opening (first frame); no segment yet
        }
        self.prev = Some((seq, frame.clone()));
        if self.seg.is_none() {
            self.seg = Some(TickSeg { idx: self.next_idx, t_start: t, slots: 0, dropped: 0, last_sent_seq: None });
            self.sh.current.store(self.next_idx as i64, Relaxed);
            self.next_idx += 1;
        }
        let s = self.seg.as_mut().unwrap();
        s.slots += 1;
        let repeated = s.last_sent_seq == Some(seq);
        let msg = EncMsg::Frame { seg: s.idx, t_start: s.t_start, frame, tick_ns: t, repeated };
        if !self.flush_outbox() {
            self.drop_frame();
            return;
        }
        match self.tx.try_send(msg) {
            Ok(()) => {
                let s = self.seg.as_mut().unwrap();
                s.last_sent_seq = Some(seq);
                self.sh.c.frames_sent.fetch_add(1, Relaxed);
                if repeated {
                    self.sh.c.repeated.fetch_add(1, Relaxed);
                }
            }
            Err(TrySendError::Full(_)) => self.drop_frame(),
            Err(TrySendError::Disconnected(_)) => {
                self.encoder_gone = true;
                self.drop_frame();
            }
        }
    }
}

fn ticker_thread(sh: Arc<Shared>, latest: Arc<LatestFrame>, tx: Sender<EncMsg>, settings: CaptureSettings, first: u32) {
    let period = settings.tick_ns();
    let mut t = Ticker {
        sh: sh.clone(),
        tx,
        outbox: VecDeque::new(),
        seg: None,
        next_idx: first,
        period,
        slots_per_seg: settings.frames_per_segment().max(1),
        encoder_gone: false,
        prev: None,
    };
    let t0 = cap_clock::now_ns() + period;
    let mut k: i64 = 0;
    let mut stop_seen: Option<Nanos> = None;
    loop {
        let deadline = t0 + k * period;
        cap_clock::sleep_until(deadline);
        let now = cap_clock::now_ns();
        let late = now - deadline;
        let c = &sh.c;
        c.ticks.fetch_add(1, Relaxed);
        c.late_sum.fetch_add(late, Relaxed);
        c.late_max.fetch_max(late, Relaxed);
        if late > 1_000_000 {
            c.over_1ms.fetch_add(1, Relaxed);
        }
        let skip = late / period;
        for _ in 0..skip {
            t.missed();
        }
        k += skip;
        let tick_t = t0 + k * period;
        k += 1;

        if sh.stop.load(SeqCst) {
            let since = *stop_seen.get_or_insert(now);
            // Wait (≤1 s) for the input gate to confirm the stop-pause so
            // synthesised releases land inside the final segment.
            if sh.gate_closed_before(tick_t) || t.seg.is_none() || now - since > 1_000_000_000 {
                break;
            }
        }
        t.tick(tick_t, &latest);
        if t.encoder_gone {
            sh.error("encoder thread exited; stopping ticker".into());
            break;
        }
    }
    t.close(true);
    // Shutdown only: deliver remaining control messages even if it blocks.
    while let Some(m) = t.outbox.pop_front() {
        if t.tx.send(m).is_err() {
            break;
        }
    }
    sh.current.store(-1, Relaxed);
}

// ---------------------------------------------------------------------------
// encoder

/// Open the encoder for `frame` now (errors are left for `encode` to report).
fn prepare_encoder(enc: &mut dyn FrameEncoder, frame: &CapturedFrame) {
    let t = std::time::Instant::now();
    match enc.prepare(frame) {
        Ok(()) => tracing::debug!(ms = t.elapsed().as_millis() as u64, "encoder ready"),
        Err(e) => tracing::warn!("pre-opening the encoder failed (retried on the first frame): {e}"),
    }
}

fn encoder_thread(
    sh: Arc<Shared>,
    mut enc: Box<dyn FrameEncoder>,
    latest: Arc<LatestFrame>,
    rx: Receiver<EncMsg>,
    tx: Sender<WMsg>,
) {
    // Warm up on the first captured frame before the ticker may open a segment:
    // on Windows the real encoder can only be opened on the capture's D3D11
    // device, and doing that inside the first encode stalls the queue long
    // enough to drop the segment's first frames.
    loop {
        if let (_, Some(frame)) = latest.latest() {
            prepare_encoder(enc.as_mut(), &frame);
            break;
        }
        if sh.stop.load(SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    sh.enc_ready.store(true, SeqCst);
    let mut last_frame: Option<Arc<CapturedFrame>> = None;
    let mut next_pts: i64 = 0;
    // (segment idx, frames encoded in it)
    let mut cur: Option<(u32, u32)> = None;
    let send = |m: WMsg| {
        let _ = tx.send(m); // blocking: never drop encoded packets
    };
    let send_pkts = |pkts: Vec<EncodedPacket>| {
        for p in pkts {
            let _ = tx.send(WMsg::Packet(p));
        }
    };
    for msg in rx.iter() {
        match msg {
            EncMsg::Frame { seg, t_start, frame, tick_ns, repeated } => {
                let new_seg = cur.map(|c| c.0) != Some(seg);
                if new_seg {
                    if let Some((old, frames)) = cur.take() {
                        // Shouldn't happen (End always precedes), but stay consistent.
                        send(WMsg::End { seg: old, t_end: tick_ns, dropped: 0, frames, flushed: false });
                    }
                    cur = Some((seg, 0));
                }
                let (_, frames) = cur.as_mut().unwrap();
                let res = enc.encode(&frame, next_pts, *frames == 0);
                last_frame = Some(frame.clone());
                if new_seg {
                    // Sent after the first encode so `params` describe the
                    // encoder that actually produced this segment's packets
                    // (a lazily opened encoder, e.g. on the WGC D3D11 device).
                    send(WMsg::Start { seg, t_start, start_pts: next_pts, params: enc.params() });
                }
                match res {
                    Ok(pkts) => {
                        let rec = FrameRecord { frame_idx: *frames, tick_ns, capture_ns: frame.capture_ns, repeated };
                        *frames += 1;
                        next_pts += 1;
                        send(WMsg::Frame { seg, rec });
                        send_pkts(pkts);
                    }
                    Err(e) => {
                        sh.c.dropped.fetch_add(1, Relaxed);
                        sh.error(format!("encode failed (frame dropped): {e}"));
                        send(WMsg::Dropped { seg });
                    }
                }
            }
            EncMsg::End { seg, t_end, dropped, flush } => {
                if flush {
                    match enc.flush() {
                        Ok(p) => send_pkts(p),
                        Err(e) => sh.error(format!("encoder flush failed: {e}")),
                    }
                    // Paused: re-open now rather than on resume's first frame.
                    if let (false, Some(f)) = (sh.stop.load(SeqCst), &last_frame) {
                        prepare_encoder(enc.as_mut(), f);
                    }
                }
                match cur {
                    Some((s, frames)) if s == seg => {
                        send(WMsg::End { seg, t_end, dropped, frames, flushed: flush });
                        cur = None;
                    }
                    _ => tracing::warn!(seg, dropped, "segment ended without any encoded frame; skipped"),
                }
            }
        }
    }
    match enc.flush() {
        Ok(p) => send_pkts(p),
        Err(e) => sh.error(format!("encoder flush failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// input gate

fn input_thread(
    sh: Arc<Shared>,
    stop: Arc<AtomicBool>,
    in_rx: Receiver<InputEvent>,
    focus_rx: Receiver<FocusRecord>,
    log_tx: Sender<LogMsg>,
    mut observer: Option<Observer>,
) {
    let mut gate = InputGate::new();
    let mut observe = move |o: Observed<'_>| {
        if let Some(f) = observer.as_mut() {
            f(o);
        }
    };
    let mut gate_paused = false;
    let (mut in_rx, mut focus_rx) = (in_rx, focus_rx);
    let log = |m: LogMsg| {
        if log_tx.try_send(m).is_err() {
            sh.c.lost.fetch_add(1, Relaxed);
        }
    };
    let log_events = |evs: Vec<InputEvent>| {
        for e in evs {
            sh.c.inputs.fetch_add(1, Relaxed);
            log(LogMsg::Input(e));
        }
    };
    // Apply an initial pause before anything else.
    if sh.paused.load(SeqCst) {
        gate.set_paused(true, cap_clock::now_ns());
        gate_paused = true;
        sh.gate_closed_at.store(Nanos::MIN, SeqCst);
        sh.eff_paused.store(true, SeqCst);
    }
    loop {
        let want_pause = sh.paused.load(SeqCst) || sh.stop.load(SeqCst);
        if want_pause != gate_paused {
            let t = cap_clock::now_ns();
            log_events(gate.set_paused(want_pause, t));
            gate_paused = want_pause;
            sh.gate_closed_at.store(t, SeqCst);
            sh.eff_paused.store(want_pause, SeqCst);
        }
        if stop.load(SeqCst) {
            // Drain what the (already stopped) sources delivered.
            while let Ok(r) = focus_rx.try_recv() {
                observe(Observed::Focus(&r));
                log_events(gate.set_focused(r.focused, r.t_ns));
                log(LogMsg::Focus(r));
            }
            while let Ok(e) = in_rx.try_recv() {
                observe(Observed::Input { event: &e, focused: gate.is_focused() });
                if gate.admit(&e) {
                    log_events(vec![e]);
                } else {
                    sh.c.gated.fetch_add(1, Relaxed);
                }
            }
            break;
        }
        select! {
            recv(focus_rx) -> r => match r {
                Ok(r) => {
                    observe(Observed::Focus(&r));
                    log_events(gate.set_focused(r.focused, r.t_ns));
                    log(LogMsg::Focus(r));
                }
                Err(_) => focus_rx = never(),
            },
            recv(in_rx) -> e => match e {
                Ok(e) => {
                    observe(Observed::Input { event: &e, focused: gate.is_focused() });
                    if gate.admit(&e) {
                        log_events(vec![e]);
                    } else {
                        sh.c.gated.fetch_add(1, Relaxed);
                    }
                }
                Err(_) => in_rx = never(),
            },
            default(Duration::from_millis(5)) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// writer

struct WSeg {
    idx: u32,
    partial: PathBuf,
    sink: Option<Box<dyn SegmentSink>>,
    journal: Option<JournalWriter>,
    manifest: Manifest,
    t_start: Nanos,
    t_end: Option<Nanos>,
    start_pts: i64,
    frames: Vec<FrameRecord>,
    packets: u32,
    expected_frames: Option<u32>,
    inputs: Vec<InputEvent>,
    extra_dropped: u64,
    flushed: bool,
    superseded: bool,
    failed: bool,
}

impl WSeg {
    fn contains(&self, t: Nanos) -> bool {
        t >= self.t_start && self.t_end.is_none_or(|e| t < e)
    }
    fn journal(&mut self, e: &Entry) {
        if let Some(j) = self.journal.as_mut() {
            if let Err(err) = j.append(e) {
                tracing::warn!("journal write failed: {err}");
                self.journal = None;
            }
        }
    }
    fn ready(&self, now: Nanos) -> bool {
        let Some(t_end) = self.t_end else { return false };
        let packets_done = self.flushed || self.superseded || self.expected_frames.is_some_and(|n| self.packets >= n);
        packets_done && now >= t_end + INPUT_GRACE_NS
    }
}

struct Writer {
    sh: Arc<Shared>,
    session_dir: PathBuf,
    sinks: Box<dyn SinkFactory>,
    template: Manifest,
    period: Nanos,
    fin_tx: Sender<Finalize>,
    segs: VecDeque<WSeg>,
    /// Inputs newer than every open segment, waiting for the next one to start.
    future: Vec<InputEvent>,
    /// Latest segment boundary seen (end of the newest closed segment).
    horizon: Nanos,
    focus_hist: Vec<FocusRecord>,
    /// Held keys/buttons/axes at the end of the last handed-off segment.
    carry: CarriedInputs,
}

impl Writer {
    fn new(
        sh: Arc<Shared>,
        session_dir: PathBuf,
        sinks: Box<dyn SinkFactory>,
        template: Manifest,
        period: Nanos,
        fin_tx: Sender<Finalize>,
    ) -> Self {
        Self {
            sh,
            session_dir,
            sinks,
            template,
            period,
            fin_tx,
            segs: VecDeque::new(),
            future: Vec::new(),
            horizon: Nanos::MIN,
            focus_hist: Vec::new(),
            carry: CarriedInputs::default(),
        }
    }

    fn run(mut self, w_rx: Receiver<WMsg>, log_rx: Receiver<LogMsg>) {
        let mut log_rx_sel = log_rx.clone();
        loop {
            select! {
                recv(w_rx) -> m => match m {
                    Ok(m) => self.on_msg(m),
                    Err(_) => break,
                },
                recv(log_rx_sel) -> m => match m {
                    Ok(m) => self.on_log(m),
                    Err(_) => log_rx_sel = never(),
                },
                default(Duration::from_millis(20)) => {}
            }
            self.housekeeping(false);
        }
        // Shutdown: the input thread has been joined, so this drains everything.
        while let Ok(m) = log_rx.try_recv() {
            self.on_log(m);
        }
        self.housekeeping(true);
    }

    fn seg_mut(&mut self, idx: u32) -> Option<&mut WSeg> {
        self.segs.iter_mut().find(|s| s.idx == idx)
    }

    fn focus_at(&self, t: Nanos) -> FocusRecord {
        let mut r = self
            .focus_hist
            .iter()
            .rev()
            .find(|r| r.t_ns <= t)
            .cloned()
            .unwrap_or(FocusRecord { t_ns: t, focused: false, game_id: String::new() });
        r.t_ns = t;
        r
    }

    fn on_msg(&mut self, m: WMsg) {
        match m {
            WMsg::Start { seg, t_start, start_pts, params } => self.start_segment(seg, t_start, start_pts, params),
            WMsg::Frame { seg, rec } => {
                self.sh.c.frames_written.fetch_add(1, Relaxed);
                if let Some(s) = self.seg_mut(seg) {
                    s.journal(&Entry::Frame(rec));
                    s.frames.push(rec);
                }
            }
            WMsg::Packet(p) => {
                let Some(pos) = self.segs.iter().rposition(|s| s.start_pts <= p.pts) else {
                    tracing::warn!(pts = p.pts, "packet without segment");
                    return;
                };
                for s in self.segs.iter_mut().take(pos) {
                    s.superseded = true;
                }
                let s = &mut self.segs[pos];
                s.packets += 1;
                if let Some(sink) = s.sink.as_mut() {
                    if let Err(e) = sink.write(&p) {
                        let msg = format!("segment {} mux write failed: {e}", s.idx);
                        s.failed = true;
                        s.sink = None;
                        self.sh.error(msg);
                    }
                }
            }
            WMsg::Dropped { seg } => {
                if let Some(s) = self.seg_mut(seg) {
                    s.extra_dropped += 1;
                }
            }
            WMsg::End { seg, t_end, dropped, frames, flushed } => {
                if let Some(s) = self.seg_mut(seg) {
                    s.t_end = Some(t_end);
                    s.manifest.dropped_frames = dropped + s.extra_dropped;
                    s.expected_frames = Some(frames);
                    s.flushed = flushed;
                    // Inputs routed to this still-open segment beyond t_end move on.
                    let (keep, spill): (Vec<_>, Vec<_>) = std::mem::take(&mut s.inputs).into_iter().partition(|e| e.t_ns < t_end);
                    s.inputs = keep;
                    self.future.extend(spill);
                    self.horizon = self.horizon.max(t_end);
                }
            }
        }
    }

    fn start_segment(&mut self, idx: u32, t_start: Nanos, start_pts: i64, params: StreamParams) {
        let partial = self.session_dir.join(format!("{}{}", segment_dir_name(idx), files::PARTIAL_SUFFIX));
        let mut manifest = self.template.clone();
        manifest.segment_idx = idx;
        manifest.t_start_ns = t_start;
        manifest.encoder = params.encoder_name.clone();
        manifest.encoder_params = params.params.clone();
        manifest.width = params.width;
        manifest.height = params.height;
        let mut seg = WSeg {
            idx,
            partial: partial.clone(),
            sink: None,
            journal: None,
            manifest: manifest.clone(),
            t_start,
            t_end: None,
            start_pts,
            frames: Vec::new(),
            packets: 0,
            expected_frames: None,
            inputs: Vec::new(),
            extra_dropped: 0,
            flushed: false,
            superseded: false,
            failed: false,
        };
        let res = std::fs::create_dir_all(&partial)
            .map_err(|e| e.to_string())
            .and_then(|_| self.sinks.create(&partial.join(files::VIDEO), &params, start_pts).map_err(|e| e.to_string()));
        match res {
            Ok(sink) => seg.sink = Some(sink),
            Err(e) => {
                seg.failed = true;
                self.sh.error(format!("segment {idx}: cannot create muxer: {e}"));
            }
        }
        match JournalWriter::create(&partial.join(JOURNAL_FILE)) {
            Ok(j) => seg.journal = Some(j),
            Err(e) => tracing::warn!("segment {idx}: no journal: {e}"),
        }
        seg.journal(&Entry::Header(Box::new(manifest)));
        seg.journal(&Entry::FocusInit(self.focus_at(t_start)));
        // Adopt buffered inputs.
        let mut rest = Vec::new();
        for e in std::mem::take(&mut self.future) {
            if seg.contains(e.t_ns) {
                seg.journal(&Entry::Input(e));
                seg.inputs.push(e);
            } else if e.t_ns >= t_start {
                rest.push(e);
            } else {
                self.sh.c.lost.fetch_add(1, Relaxed);
            }
        }
        self.future = rest;
        self.segs.push_back(seg);
    }

    fn on_log(&mut self, m: LogMsg) {
        match m {
            LogMsg::Input(e) => {
                if let Some(s) = self.segs.iter_mut().find(|s| s.contains(e.t_ns)) {
                    s.journal(&Entry::Input(e));
                    s.inputs.push(e);
                } else if e.t_ns >= self.horizon && self.segs.iter().all(|s| s.t_end.is_some()) {
                    self.future.push(e);
                } else {
                    self.sh.c.lost.fetch_add(1, Relaxed);
                }
            }
            LogMsg::Focus(r) => {
                let pos = self.focus_hist.partition_point(|x| x.t_ns <= r.t_ns);
                self.focus_hist.insert(pos, r.clone());
                for i in 0..self.segs.len() {
                    let s = &self.segs[i];
                    if s.contains(r.t_ns) {
                        self.segs[i].journal(&Entry::Focus(r.clone()));
                    } else if r.t_ns < s.t_start {
                        let init = self.focus_at(s.t_start);
                        self.segs[i].journal(&Entry::FocusInit(init));
                    }
                }
            }
        }
    }

    fn housekeeping(&mut self, shutdown: bool) {
        let now = cap_clock::now_ns();
        for s in self.segs.iter_mut() {
            if let Some(j) = s.journal.as_mut() {
                let _ = j.maybe_flush();
            }
        }
        // Expire buffered future inputs (e.g. while paused).
        let before = self.future.len();
        self.future.retain(|e| e.t_ns >= now - FUTURE_INPUT_TTL_NS);
        self.sh.c.lost.fetch_add((before - self.future.len()) as u64, Relaxed);

        while let Some(front) = self.segs.front() {
            if !(shutdown || front.ready(now)) {
                break;
            }
            let mut s = self.segs.pop_front().unwrap();
            let t_end = s.t_end.unwrap_or_else(|| s.frames.last().map_or(s.t_start, |f| f.tick_ns + self.period));
            self.horizon = self.horizon.max(t_end);
            self.hand_off(&mut s, t_end);
        }
        if shutdown {
            let n = self.future.len();
            self.sh.c.lost.fetch_add(n as u64, Relaxed);
            self.future.clear();
        }
    }

    fn hand_off(&mut self, s: &mut WSeg, t_end: Nanos) {
        let mut ok = !s.failed;
        if let Some(sink) = s.sink.take() {
            if let Err(e) = sink.finish() {
                ok = false;
                self.sh.error(format!("segment {}: muxer finish failed: {e}", s.idx));
            }
        }
        if let Some(mut j) = s.journal.take() {
            let _ = j.flush();
        }
        let mut inputs = std::mem::take(&mut s.inputs);
        inputs.retain(|e| e.t_ns >= s.t_start && e.t_ns < t_end);
        inputs.sort_by_key(|e| e.t_ns); // stable: keeps arrival order for equal stamps
        // Re-emit what is still held from the previous segment at t_start,
        // then carry this segment's end state to the next one.
        let carried = self.carry.still_held(s.t_start);
        for e in &inputs {
            self.carry.observe(e);
        }
        if !ok || s.frames.is_empty() {
            // Leave the .partial (with its journal) for recover_partials.
            self.sh.c.segment_errors.fetch_add(1, Relaxed);
            tracing::warn!(seg = s.idx, "segment not finalized; left as .partial");
            return;
        }
        if !carried.is_empty() {
            inputs.splice(0..0, carried);
        }
        let mut focus = vec![self.focus_at(s.t_start)];
        focus.extend(self.focus_hist.iter().filter(|r| r.t_ns >= s.t_start && r.t_ns < t_end).cloned());
        // Keep the focus history needed by later segments (state at t_end onward).
        let keep_from = self.focus_hist.partition_point(|r| r.t_ns <= t_end).saturating_sub(1);
        self.focus_hist.drain(..keep_from);

        let mut manifest = s.manifest.clone();
        manifest.t_end_ns = t_end;
        let frames = std::mem::take(&mut s.frames);
        let _ = self.fin_tx.send(Finalize {
            partial: s.partial.clone(),
            manifest,
            tables: SegmentTables { frames, inputs, focus },
        });
    }
}

fn finalizer_thread(sh: Arc<Shared>, rx: Receiver<Finalize>, cb: &mut SegmentCallback) {
    for f in rx.iter() {
        match finalize_segment(&f.partial, f.manifest, &f.tables) {
            Ok(path) => {
                sh.c.segments_done.fetch_add(1, Relaxed);
                tracing::info!(path = %path.display(), frames = f.tables.frames.len(), inputs = f.tables.inputs.len(), "segment complete");
                cb(path);
            }
            Err(e) => {
                sh.c.segment_errors.fetch_add(1, Relaxed);
                sh.error(format!("finalize {} failed: {e}", f.partial.display()));
            }
        }
    }
}
