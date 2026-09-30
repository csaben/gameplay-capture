//! Fakes for tests and dry runs: an encoder that emits tiny fake packets, a
//! muxer that writes them to a trivial container, a scripted input source and
//! a scripted focus tracker.

use crate::backend::{EncResult, FrameEncoder, SegmentSink, SinkFactory};
use cap_capture::CapturedFrame;
use cap_encode::{Codec, EncodedPacket, StreamParams};
use cap_focus::{FocusTracker, WindowInfo};
use cap_input::InputSource;
use cap_types::{Device, EventKind, FocusRecord, InputEvent, Nanos};
use crossbeam_channel::Sender;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Encoder that sleeps `delay` per frame and holds back `latency` packets
/// (like a real encoder's pipeline depth). Packet data = pts + capture_ns.
pub struct FakeEncoder {
    pub delay: Duration,
    pub latency: usize,
    width: u32,
    height: u32,
    rate_hz: u32,
    queue: VecDeque<EncodedPacket>,
}

impl FakeEncoder {
    pub fn new(width: u32, height: u32, rate_hz: u32) -> Self {
        Self { delay: Duration::ZERO, latency: 0, width, height, rate_hz, queue: VecDeque::new() }
    }
    pub fn with_delay(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }
    pub fn with_latency(mut self, n: usize) -> Self {
        self.latency = n;
        self
    }
}

impl FrameEncoder for FakeEncoder {
    fn params(&self) -> StreamParams {
        StreamParams {
            encoder_name: "fake".into(),
            width: self.width,
            height: self.height,
            rate_hz: self.rate_hz,
            codec: Codec::Hevc,
            extradata: Vec::new(),
            params: [("qp".to_string(), "0".to_string())].into_iter().collect(),
        }
    }
    fn encode(&mut self, frame: &CapturedFrame, pts: i64, force_keyframe: bool) -> EncResult<Vec<EncodedPacket>> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let mut data = pts.to_le_bytes().to_vec();
        data.extend_from_slice(&frame.capture_ns.to_le_bytes());
        self.queue.push_back(EncodedPacket { data, pts, dts: pts, keyframe: force_keyframe });
        let mut out = Vec::new();
        while self.queue.len() > self.latency {
            out.push(self.queue.pop_front().unwrap());
        }
        Ok(out)
    }
    fn flush(&mut self) -> EncResult<Vec<EncodedPacket>> {
        Ok(self.queue.drain(..).collect())
    }
}

const FAKE_MAGIC: &[u8; 8] = b"FAKEMP4\0";

/// Writes packets as `[pts i64][key u8][len u32][data]` after an 8-byte magic,
/// flushing every packet (so a crash leaves every written packet on disk).
#[derive(Default)]
pub struct FakeSinkFactory;

struct FakeSink {
    w: BufWriter<File>,
    offset: i64,
}

impl SinkFactory for FakeSinkFactory {
    fn create(&self, path: &Path, _params: &StreamParams, pts_offset: i64) -> EncResult<Box<dyn SegmentSink>> {
        let mut w = BufWriter::new(File::create(path)?);
        w.write_all(FAKE_MAGIC)?;
        w.flush()?;
        Ok(Box::new(FakeSink { w, offset: pts_offset }))
    }
}

impl SegmentSink for FakeSink {
    fn write(&mut self, p: &EncodedPacket) -> EncResult<()> {
        self.w.write_all(&(p.pts - self.offset).to_le_bytes())?;
        self.w.write_all(&[p.keyframe as u8])?;
        self.w.write_all(&(p.data.len() as u32).to_le_bytes())?;
        self.w.write_all(&p.data)?;
        self.w.flush()?;
        Ok(())
    }
    fn finish(mut self: Box<Self>) -> EncResult<()> {
        self.w.flush()?;
        Ok(())
    }
}

/// Parses a `FakeSinkFactory` file into `(pts, keyframe)` per packet.
pub fn read_fake_video(path: &Path) -> std::io::Result<Vec<(i64, bool)>> {
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    if buf.len() < 8 || &buf[..8] != FAKE_MAGIC {
        return Err(std::io::Error::other("not a fake video"));
    }
    let mut i = 8;
    let mut out = Vec::new();
    while i + 13 <= buf.len() {
        let pts = i64::from_le_bytes(buf[i..i + 8].try_into().unwrap());
        let key = buf[i + 8] != 0;
        let len = u32::from_le_bytes(buf[i + 9..i + 13].try_into().unwrap()) as usize;
        i += 13 + len;
        if i > buf.len() {
            break;
        }
        out.push((pts, key));
    }
    Ok(out)
}

/// One scripted input event: fires `at` after `start()`.
#[derive(Debug, Clone, Copy)]
pub struct ScriptedEvent {
    pub at: Duration,
    pub device: Device,
    pub kind: EventKind,
    pub code: u32,
    pub value: f32,
}

impl ScriptedEvent {
    pub fn new(at_ms: u64, device: Device, kind: EventKind, code: u32, value: f32) -> Self {
        Self { at: Duration::from_millis(at_ms), device, kind, code, value }
    }
}

/// Emits scripted events stamped with `cap_clock` at their fire time.
/// Optionally also emits a mouse-move every `stream_every` (load test).
pub struct ScriptedInputSource {
    pub script: Vec<ScriptedEvent>,
    pub stream_every: Option<Duration>,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Scheduled start time (cap_clock), set by `start`.
    pub started_at: Arc<std::sync::atomic::AtomicI64>,
}

impl ScriptedInputSource {
    pub fn new(script: Vec<ScriptedEvent>) -> Self {
        Self {
            script,
            stream_every: None,
            running: Arc::new(AtomicBool::new(false)),
            thread: None,
            started_at: Arc::new(Default::default()),
        }
    }
}

impl InputSource for ScriptedInputSource {
    fn start(&mut self, tx: Sender<InputEvent>) -> cap_input::Result<()> {
        let mut script = self.script.clone();
        script.sort_by_key(|e| e.at);
        let every = self.stream_every;
        let running = self.running.clone();
        running.store(true, Ordering::SeqCst);
        let t0 = cap_clock::now_ns();
        self.started_at.store(t0, Ordering::SeqCst);
        self.thread = Some(std::thread::spawn(move || {
            let mut next_stream = every.map(|d| t0 + d.as_nanos() as Nanos);
            let mut it = script.into_iter().peekable();
            while running.load(Ordering::SeqCst) {
                let next_script = it.peek().map(|e| t0 + e.at.as_nanos() as Nanos);
                let wake = match (next_script, next_stream) {
                    (Some(a), Some(b)) => a.min(b),
                    (Some(a), None) => a,
                    (None, Some(b)) => b,
                    (None, None) => cap_clock::now_ns() + 5_000_000,
                };
                cap_clock::sleep_until(wake.min(cap_clock::now_ns() + 5_000_000));
                let now = cap_clock::now_ns();
                while it.peek().is_some_and(|e| t0 + e.at.as_nanos() as Nanos <= now) {
                    let e = it.next().unwrap();
                    let _ = tx.try_send(InputEvent { t_ns: now, device: e.device, kind: e.kind, code: e.code, value: e.value });
                }
                if let (Some(n), Some(d)) = (next_stream.as_mut(), every) {
                    if *n <= now {
                        let _ = tx.try_send(InputEvent { t_ns: now, device: Device::Mouse, kind: EventKind::MouseMove, code: 0, value: 1.0 });
                        *n += d.as_nanos() as Nanos;
                    }
                }
            }
        }));
        Ok(())
    }
    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
    fn name(&self) -> &'static str {
        "scripted"
    }
}

/// Emits the initial focus state immediately, then `(at, focused)` changes.
pub struct ScriptedFocusTracker {
    pub initial: bool,
    pub script: Vec<(Duration, bool)>,
    pub foreign_game_id: String,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ScriptedFocusTracker {
    pub fn new(initial: bool, script: Vec<(u64, bool)>) -> Self {
        Self {
            initial,
            script: script.into_iter().map(|(ms, f)| (Duration::from_millis(ms), f)).collect(),
            foreign_game_id: "firefox".into(),
            running: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }
    /// Always focused.
    pub fn focused() -> Self {
        Self::new(true, Vec::new())
    }
}

impl FocusTracker for ScriptedFocusTracker {
    fn list_windows(&self) -> cap_focus::Result<Vec<WindowInfo>> {
        Ok(Vec::new())
    }
    fn start(&mut self, target_game_id: &str, tx: Sender<FocusRecord>) -> cap_focus::Result<()> {
        let game = target_game_id.to_string();
        let other = self.foreign_game_id.clone();
        let rec = move |t_ns: Nanos, focused: bool| FocusRecord {
            t_ns,
            focused,
            game_id: if focused { game.clone() } else { other.clone() },
        };
        let t0 = cap_clock::now_ns();
        let _ = tx.send(rec(t0, self.initial));
        let script = self.script.clone();
        let running = self.running.clone();
        running.store(true, Ordering::SeqCst);
        self.thread = Some(std::thread::spawn(move || {
            for (at, focused) in script {
                let when = t0 + at.as_nanos() as Nanos;
                while running.load(Ordering::SeqCst) && cap_clock::now_ns() < when {
                    cap_clock::sleep_until(when.min(cap_clock::now_ns() + 5_000_000));
                }
                if !running.load(Ordering::SeqCst) {
                    return;
                }
                let _ = tx.send(rec(cap_clock::now_ns(), focused));
            }
        }));
        Ok(())
    }
    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
