//! End-to-end recorder tests with the synthetic frame source, scripted inputs /
//! focus and the fake encoder + muxer.

use cap_capture::synthetic::SyntheticSource;
use cap_capture::WindowTarget;
use cap_recorder::testkit::*;
use cap_recorder::{finalize, journal, recover_partials, tables, Recorder, RecorderConfig, RecorderStats, Sources};
use cap_types::{files, CaptureSettings, Device, EventKind, FocusRecord, FrameRecord, InputEvent, Manifest};
use crossbeam_channel::{unbounded, Receiver};
use std::path::{Path, PathBuf};
use std::time::Duration;

const GAME: &str = "game.exe";

fn root(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("caprec-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn config(root: &Path, rate_hz: u32, segment_secs: u32) -> RecorderConfig {
    let target = WindowTarget { native_id: 0, title: "Game".into(), game_id: GAME.into(), pid: 0 };
    let settings = CaptureSettings { width: 64, height: 36, rate_hz, segment_secs };
    let mut c = RecorderConfig::new(target, settings, root.to_path_buf(), "sess1".into());
    c.latency_offset_ns = 12_345;
    c.client_version = "test".into();
    c
}

struct Run {
    rec: Recorder,
    done: Receiver<PathBuf>,
}

fn start(cfg: RecorderConfig, src_fps: u32, inputs: Vec<ScriptedEvent>, focus: ScriptedFocusTracker, enc: FakeEncoder) -> Run {
    let (tx, done) = unbounded();
    let sources = Sources {
        frames: Box::new(SyntheticSource::new(64, 36, src_fps)),
        inputs: vec![Box::new(ScriptedInputSource::new(inputs))],
        focus: Box::new(focus),
    };
    let rec = Recorder::start_with_backend(
        cfg,
        sources,
        Box::new(enc),
        Box::new(FakeSinkFactory),
        Box::new(move |p| tx.send(p).unwrap()),
    )
    .unwrap();
    Run { rec, done }
}

fn fake_enc() -> FakeEncoder {
    FakeEncoder::new(64, 36, 20)
}

struct Seg {
    dir: PathBuf,
    m: Manifest,
    frames: Vec<FrameRecord>,
    inputs: Vec<InputEvent>,
    focus: Vec<FocusRecord>,
    video: Vec<(i64, bool)>,
}

fn load(dir: &Path) -> Seg {
    let m: Manifest = serde_json::from_slice(&std::fs::read(dir.join(files::MANIFEST)).unwrap()).unwrap();
    // Hashes and sizes in the manifest match the files.
    for name in files::DATA {
        let (h, n) = finalize::hash_file(&dir.join(name)).unwrap();
        assert_eq!(m.blake3[name], h, "{name}");
        assert_eq!(m.sizes[name], n, "{name}");
    }
    assert!(!dir.join(journal::JOURNAL_FILE).exists(), "journal must be deleted");
    Seg {
        dir: dir.to_path_buf(),
        frames: tables::read_frames(&dir.join(files::FRAMES)).unwrap(),
        inputs: tables::read_inputs(&dir.join(files::INPUTS)).unwrap(),
        focus: tables::read_focus(&dir.join(files::FOCUS)).unwrap(),
        video: read_fake_video(&dir.join(files::VIDEO)).unwrap(),
        m,
    }
}

fn stop(run: Run) -> (RecorderStats, Vec<Seg>) {
    let stats = run.rec.stop();
    let paths: Vec<PathBuf> = run.done.try_iter().collect();
    let segs: Vec<Seg> = paths.iter().map(|p| load(p)).collect();
    (stats, segs)
}

/// Invariants every finished segment must satisfy.
fn check_segment(s: &Seg, period: i64) {
    let name = s.dir.file_name().unwrap().to_str().unwrap();
    assert_eq!(name, cap_types::segment_dir_name(s.m.segment_idx));
    assert_eq!(s.m.frame_count as usize, s.frames.len());
    assert_eq!(s.m.game_id, GAME);
    assert_eq!(s.m.latency_offset_ns, 12_345);
    assert_eq!(s.m.encoder, "fake");
    // frame_idx continuous from 0 and equal to the video position.
    for (i, f) in s.frames.iter().enumerate() {
        assert_eq!(f.frame_idx as usize, i);
        assert!(f.tick_ns >= s.m.t_start_ns && f.tick_ns < s.m.t_end_ns);
        assert_eq!((f.tick_ns - s.m.t_start_ns) % period, 0, "ticks on the grid");
        assert!(f.capture_ns <= f.tick_ns, "frame captured after its tick");
    }
    // The first slot may have been dropped under backpressure.
    assert!(s.frames[0].tick_ns >= s.m.t_start_ns);
    assert!(s.frames.windows(2).all(|w| w[0].capture_ns <= w[1].capture_ns), "capture_ns monotonic");
    assert_eq!(s.video.len(), s.frames.len(), "one packet per frame");
    for (i, (pts, key)) in s.video.iter().enumerate() {
        assert_eq!(*pts, i as i64, "pts restart at 0 per segment");
        assert_eq!(*key, i == 0, "keyframe forced exactly on the first frame");
    }
    // Slots = frames + dropped.
    let slots = (s.m.t_end_ns - s.m.t_start_ns) / period;
    assert_eq!(slots as u64, s.frames.len() as u64 + s.m.dropped_frames, "{name}");
    // Inputs inside [t_start, t_end), sorted.
    assert!(s.inputs.iter().all(|e| e.t_ns >= s.m.t_start_ns && e.t_ns < s.m.t_end_ns));
    assert!(s.inputs.windows(2).all(|w| w[0].t_ns <= w[1].t_ns));
    // Focus starts with the carried state at t_start.
    assert_eq!(s.focus[0].t_ns, s.m.t_start_ns);
    assert!(s.focus.iter().all(|r| r.t_ns >= s.m.t_start_ns && r.t_ns < s.m.t_end_ns));
}

/// Every key/button pressed within a segment is released within it.
fn assert_no_stuck_keys(s: &Seg) {
    let mut held = cap_recorder::HeldState::default();
    for e in &s.inputs {
        held.observe(e);
    }
    assert!(held.is_empty(), "stuck inputs in {:?}: {:?}", s.dir, held);
}

#[test]
fn segment_rollover_at_exact_tick_counts() {
    let r = root("rollover");
    let cfg = config(&r, 20, 2);
    let period = cfg.settings.tick_ns();
    // 10 fps source at 20 Hz ticks => about half the frames are repeats.
    let run = start(cfg, 10, vec![], ScriptedFocusTracker::focused(), fake_enc().with_latency(3));
    std::thread::sleep(Duration::from_millis(5300));
    let (stats, segs) = stop(run);
    eprintln!("stats: {stats:#?}");
    assert_eq!(segs.len(), 3, "2 full + 1 short");
    for (i, s) in segs.iter().enumerate() {
        assert_eq!(s.m.segment_idx, i as u32);
        check_segment(s, period);
    }
    for s in &segs[..2] {
        assert_eq!(s.m.t_end_ns - s.m.t_start_ns, 2_000_000_000);
        assert_eq!(s.frames.len() as u64 + s.m.dropped_frames, 40);
        assert!(s.m.dropped_frames <= 2, "{}", s.m.dropped_frames);
    }
    assert_eq!(segs[1].m.t_start_ns, segs[0].m.t_end_ns, "contiguous");
    assert_eq!(segs[2].m.t_start_ns, segs[1].m.t_end_ns, "contiguous");
    assert!(segs[2].frames.len() > 10 && segs[2].frames.len() < 40);
    assert_eq!(stats.segments_done, 3);
    assert_eq!(stats.frames_written, segs.iter().map(|s| s.frames.len() as u64).sum::<u64>());
    assert!(stats.repeated_ratio > 0.3 && stats.repeated_ratio < 0.7, "{}", stats.repeated_ratio);
    // Nothing left behind.
    let leftover: Vec<_> = std::fs::read_dir(r.join("sess1")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(leftover.len(), 3, "{leftover:?}");
    assert!(stats.last_error.is_none());
    std::fs::remove_dir_all(&r).unwrap();
}

#[test]
fn inputs_gated_by_focus_and_released_on_focus_loss() {
    let r = root("focus");
    let cfg = config(&r, 20, 60);
    let period = cfg.settings.tick_ns();
    let k = |ms, kind, code| ScriptedEvent::new(ms, Device::Keyboard, kind, code, if kind == EventKind::KeyDown { 1.0 } else { 0.0 });
    let script = vec![
        k(200, EventKind::KeyDown, 30),
        ScriptedEvent::new(500, Device::Mouse, EventKind::MouseMove, 0, 3.0),
        ScriptedEvent::new(600, Device::Gamepad, EventKind::Axis, 1, 0.7),
        // unfocused 1000..1500: all dropped
        k(1200, EventKind::KeyDown, 17),
        ScriptedEvent::new(1300, Device::Mouse, EventKind::MouseButton, 0, 1.0),
        k(1400, EventKind::KeyUp, 17),
        k(1700, EventKind::KeyDown, 31), // held through stop
    ];
    let focus = ScriptedFocusTracker::new(true, vec![(1000, false), (1500, true)]);
    let run = start(cfg, 30, script, focus, fake_enc());
    std::thread::sleep(Duration::from_millis(2200));
    let (stats, segs) = stop(run);
    eprintln!("stats: {stats:#?}");
    assert_eq!(segs.len(), 1);
    let s = &segs[0];
    check_segment(s, period);
    assert_no_stuck_keys(s);
    let kinds: Vec<(EventKind, u32, f32)> = s.inputs.iter().map(|e| (e.kind, e.code, e.value)).collect();
    assert_eq!(
        kinds,
        vec![
            (EventKind::KeyDown, 30, 1.0),
            (EventKind::MouseMove, 0, 3.0),
            (EventKind::Axis, 1, 0.7),
            (EventKind::KeyUp, 30, 0.0), // synthesised at focus loss
            (EventKind::Axis, 1, 0.0),   // synthesised at focus loss
            (EventKind::KeyDown, 31, 1.0),
            (EventKind::KeyUp, 31, 0.0), // synthesised at stop
        ]
    );
    // Focus rows: carried state at t_start, then the two changes.
    let f: Vec<(bool, &str)> = s.focus.iter().map(|r| (r.focused, r.game_id.as_str())).collect();
    assert_eq!(f, vec![(true, GAME), (false, "firefox"), (true, GAME)]);
    // Release happens exactly at the focus-loss time.
    assert_eq!(s.inputs[3].t_ns, s.focus[1].t_ns);
    // Nothing was logged while unfocused.
    assert!(!s.inputs.iter().any(|e| e.t_ns >= s.focus[1].t_ns && e.t_ns < s.focus[2].t_ns && e.value != 0.0));
    assert_eq!(stats.input_events_gated, 3);
    assert_eq!(stats.input_events, 7);
    std::fs::remove_dir_all(&r).unwrap();
}

#[test]
fn backpressure_drops_newest_and_never_blocks_ticker() {
    let r = root("backpressure");
    let cfg = config(&r, 20, 2);
    let period = cfg.settings.tick_ns();
    // Encoder takes 130 ms per frame vs a 50 ms tick.
    let run = start(cfg, 30, vec![], ScriptedFocusTracker::focused(), fake_enc().with_delay(Duration::from_millis(130)));
    std::thread::sleep(Duration::from_millis(4500));
    let (stats, segs) = stop(run);
    eprintln!("stats: {stats:#?}");
    assert!(segs.len() >= 2);
    for s in &segs {
        check_segment(s, period);
        eprintln!("seg {}: frames {} dropped {}", s.m.segment_idx, s.frames.len(), s.m.dropped_frames);
    }
    let full = &segs[0];
    assert_eq!(full.frames.len() as u64 + full.m.dropped_frames, 40);
    // Steady state ~ 2000/130 = 15 encoded per 2 s; the rest dropped.
    assert!(full.m.dropped_frames >= 15, "{}", full.m.dropped_frames);
    let total_dropped: u64 = segs.iter().map(|s| s.m.dropped_frames).sum();
    assert_eq!(total_dropped, stats.frames_dropped);
    // The ticker kept its schedule although the encoder was saturated.
    // (20 ms bound: the test box is shared with parallel builds.)
    assert!(stats.jitter.max_late_ns < 20_000_000, "{:?}", stats.jitter);
    assert_eq!(stats.jitter.missed_ticks, 0);
    std::fs::remove_dir_all(&r).unwrap();
}

#[test]
fn ticker_precision() {
    let r = root("jitter");
    let cfg = config(&r, 20, 60);
    let run = start(cfg, 60, vec![], ScriptedFocusTracker::focused(), fake_enc());
    std::thread::sleep(Duration::from_millis(3000));
    let (stats, segs) = stop(run);
    let j = stats.jitter;
    eprintln!(
        "JITTER 20 Hz over {} ticks: mean late {:.1} us, max late {:.1} us, >1ms: {}, missed: {}",
        j.ticks,
        j.mean_late_ns / 1e3,
        j.max_late_ns as f64 / 1e3,
        j.over_1ms,
        j.missed_ticks
    );
    assert!(j.ticks >= 55);
    assert!(j.max_late_ns < 20_000_000, "{j:?}");
    assert!(j.mean_late_ns < 500_000.0);
    assert_eq!(j.missed_ticks, 0);
    let s = &segs[0];
    // Scheduled grid: exactly 50 ms apart.
    assert!(s.frames.windows(2).all(|w| w[1].tick_ns - w[0].tick_ns == 50_000_000));
    std::fs::remove_dir_all(&r).unwrap();
}

#[test]
fn pause_closes_segment_and_stops_logging() {
    let r = root("pause");
    let cfg = config(&r, 20, 60);
    let period = cfg.settings.tick_ns();
    let paused = cfg.paused.clone();
    let script = vec![
        ScriptedEvent::new(300, Device::Keyboard, EventKind::KeyDown, 30, 1.0), // held into the pause
        ScriptedEvent::new(1000, Device::Keyboard, EventKind::KeyDown, 31, 1.0), // during pause: dropped
        ScriptedEvent::new(1600, Device::Mouse, EventKind::MouseButton, 1, 1.0), // held through stop
    ];
    let run = start(cfg, 30, script, ScriptedFocusTracker::focused(), fake_enc());
    std::thread::sleep(Duration::from_millis(800));
    paused.store(true, std::sync::atomic::Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(200));
    let mid = run.rec.stats();
    assert!(mid.paused && mid.current_segment.is_none(), "{mid:?}");
    std::thread::sleep(Duration::from_millis(300));
    run.rec.set_paused(false);
    std::thread::sleep(Duration::from_millis(700));
    let (stats, segs) = stop(run);
    eprintln!("stats: {stats:#?}");
    assert_eq!(segs.len(), 2, "pause creates a boundary");
    for s in &segs {
        check_segment(s, period);
        assert_no_stuck_keys(s);
    }
    let (a, b) = (&segs[0], &segs[1]);
    assert_eq!((a.m.segment_idx, b.m.segment_idx), (0, 1));
    assert!(b.m.t_start_ns - a.m.t_end_ns >= 400_000_000, "gap for the pause");
    // No frames during the pause.
    let span_a = (a.m.t_end_ns - a.m.t_start_ns) / 1_000_000;
    assert!((650..=900).contains(&span_a), "{span_a} ms");
    let ev = |s: &Seg| s.inputs.iter().map(|e| (e.kind, e.code, e.value)).collect::<Vec<_>>();
    assert_eq!(ev(a), vec![(EventKind::KeyDown, 30, 1.0), (EventKind::KeyUp, 30, 0.0)]);
    assert_eq!(ev(b), vec![(EventKind::MouseButton, 1, 1.0), (EventKind::MouseButton, 1, 0.0)]);
    assert_eq!(stats.input_events_gated, 1);
    std::fs::remove_dir_all(&r).unwrap();
}

/// Child half of `crash_recovery`: records until killed.
#[test]
#[ignore]
fn crash_child() {
    let Ok(dir) = std::env::var("CAPREC_CRASH_DIR") else { return };
    let cfg = config(Path::new(&dir), 20, 60);
    let mut inputs = ScriptedInputSource::new(vec![ScriptedEvent::new(100, Device::Keyboard, EventKind::KeyDown, 30, 1.0)]);
    inputs.stream_every = Some(Duration::from_millis(10));
    let sources = Sources {
        frames: Box::new(SyntheticSource::new(64, 36, 30)),
        inputs: vec![Box::new(inputs)],
        focus: Box::new(ScriptedFocusTracker::focused()),
    };
    let _rec =
        Recorder::start_with_backend(cfg, sources, Box::new(fake_enc()), Box::new(FakeSinkFactory), Box::new(|_| {})).unwrap();
    std::thread::sleep(Duration::from_secs(60));
    panic!("should have been killed");
}

#[test]
fn crash_recovery() {
    let r = root("crash");
    let exe = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new(exe)
        .args(["crash_child", "--exact", "--ignored", "--nocapture"])
        .env("CAPREC_CRASH_DIR", &r)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let partial = r.join("sess1").join("seg_000000.partial");
    let journal = partial.join(journal::JOURNAL_FILE);
    // Wait until at least ~2.5 s of frames are journaled, then SIGKILL.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let n = journal::read(&journal).map(|e| e.iter().filter(|e| matches!(e, journal::Entry::Frame(_))).count()).unwrap_or(0);
        if n >= 50 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "child never journaled");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(partial.is_dir() && !partial.join(files::MANIFEST).exists());

    let report = recover_partials(&r);
    assert!(report.broken.is_empty(), "{:?}", report.broken);
    assert_eq!(report.finalized, vec![r.join("sess1").join("seg_000000")]);
    let s = load(&report.finalized[0]);
    eprintln!(
        "recovered: {} frames, {} video packets, {} inputs, dropped {}",
        s.frames.len(),
        s.video.len(),
        s.inputs.len(),
        s.m.dropped_frames
    );
    assert_eq!(s.m.encoder_params.get("recovered").map(String::as_str), Some("true"));
    assert!(s.frames.len() >= 50);
    assert_eq!(s.m.frame_count as usize, s.frames.len());
    assert!(s.frames.iter().enumerate().all(|(i, f)| f.frame_idx as usize == i));
    // Fake muxer flushes per packet, journal flushes per second.
    assert!(s.video.len() >= s.frames.len());
    assert!(s.inputs.len() > 100, "{}", s.inputs.len());
    assert!(s.inputs.iter().all(|e| e.t_ns >= s.m.t_start_ns && e.t_ns < s.m.t_end_ns));
    assert_eq!(s.focus[0].t_ns, s.m.t_start_ns);
    assert!(s.focus[0].focused);
    // Idempotent.
    assert!(recover_partials(&r).finalized.is_empty());
    std::fs::remove_dir_all(&r).unwrap();
}

#[test]
fn recover_partials_publishes_or_quarantines() {
    let r = root("recover");
    let sess = r.join("s");
    // (a) crash after manifest write: just renamed.
    let a = sess.join("seg_000001.partial");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::write(a.join(files::MANIFEST), b"{}").unwrap();
    std::fs::write(a.join(journal::JOURNAL_FILE), b"").unwrap();
    // (b) no video: quarantined.
    let b = sess.join("seg_000002.partial");
    std::fs::create_dir_all(&b).unwrap();
    // (c) finished segment: untouched.
    std::fs::create_dir_all(sess.join("seg_000000")).unwrap();
    let rep = recover_partials(&r);
    assert_eq!(rep.finalized, vec![sess.join("seg_000001")]);
    assert!(!sess.join("seg_000001").join(journal::JOURNAL_FILE).exists());
    assert_eq!(rep.broken.len(), 1);
    assert!(sess.join("seg_000002.broken").is_dir());
    assert!(sess.join("seg_000000").is_dir());
    std::fs::remove_dir_all(&r).unwrap();
}
