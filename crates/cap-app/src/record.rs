//! `gamecap record`: target resolution, policy checks, recorder + in-process
//! upload worker, disk cap, status line, clean shutdown.

use crate::blocklist::{Blocklist, Decision};
use crate::config::{Config, Paths, State};
use crate::pause::{Hotkeys, PauseState, Reason};
use crate::upload::{fmt_stats, open_queue, runtime, token_refresher};
use crate::util::{fmt_bytes, new_session_id, stderr_is_tty, try_lock, CtrlC};
use anyhow::{bail, Context, Result};
use cap_capture::{synthetic::SyntheticSource, FrameSource, WindowTarget};
use cap_focus::{FocusTracker, WindowInfo};
use cap_recorder::testkit::{FakeEncoder, FakeSinkFactory, ScriptedFocusTracker};
use cap_recorder::{FfmpegEncoder, FfmpegSinkFactory, Recorder, RecorderConfig, RecorderError, RecorderStats, Sources};
use cap_types::GameIdentity;
use cap_upload::UploadQueue;
use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct RecordOpts {
    /// game id, window title substring or native window id.
    pub game: Option<String>,
    pub synthetic: bool,
    pub synthetic_game_id: String,
    pub synthetic_size: (u32, u32),
    pub synthetic_fps: u32,
    /// Synthetic focus: lose focus to `synthetic_foreground` for 20% of
    /// every period of this many seconds.
    pub synthetic_alt_tab_secs: Option<u64>,
    pub synthetic_foreground: String,
    /// Use the fake encoder/muxer from cap_recorder::testkit (no GPU needed).
    pub fake_encoder: bool,
    pub no_upload: bool,
    /// Stop by itself after this long.
    pub duration: Option<Duration>,
    pub drain_secs: Option<u64>,
    pub tray: bool,
    /// Controlled by a parent process (gamecap-gui): read `stop`, `stop-now`
    /// and `pause` lines from stdin (EOF = stop); never prompt on stdin.
    pub control_stdin: bool,
    /// Print machine-readable JSON events on stdout (one per line).
    pub status_json: bool,
}

/// Pick the window `query` refers to: a native id (decimal or 0x hex), an
/// exact game id (case-insensitive), or a unique title substring.
pub fn resolve_window(windows: &[WindowInfo], query: &str) -> Result<WindowInfo> {
    let q = query.trim();
    let as_id = q.strip_prefix("0x").or_else(|| q.strip_prefix("0X")).map(|h| u64::from_str_radix(h, 16).ok()).unwrap_or_else(|| q.parse().ok());
    if let Some(id) = as_id {
        if let Some(w) = windows.iter().find(|w| w.native_id == id) {
            return Ok(w.clone());
        }
    }
    let by_game: Vec<&WindowInfo> = windows.iter().filter(|w| w.identity.game_id.eq_ignore_ascii_case(q)).collect();
    if let Some(w) = by_game.first() {
        if by_game.len() > 1 {
            tracing::warn!("{} windows belong to {q}; using {:#x} {:?}", by_game.len(), w.native_id, w.title);
        }
        return Ok((*w).clone());
    }
    let ql = q.to_lowercase();
    let by_title: Vec<&WindowInfo> = windows.iter().filter(|w| w.title.to_lowercase().contains(&ql)).collect();
    match by_title.as_slice() {
        [] => bail!("no window matches {q:?} (by native id, game id or title); see `gamecap windows`"),
        [w] => Ok((*w).clone()),
        [first, rest @ ..] if rest.iter().all(|w| w.identity.game_id == first.identity.game_id) => Ok((*first).clone()),
        many => {
            let list: Vec<String> =
                many.iter().map(|w| format!("  {:#x}  {}  {:?}", w.native_id, w.identity.game_id, w.title)).collect();
            bail!("{q:?} matches several windows; be more specific (use the game id or native id):\n{}", list.join("\n"))
        }
    }
}

/// Load the local blocklist, merged with the cached server list if present.
pub fn load_blocklist(paths: &Paths) -> Result<Blocklist> {
    let mut b = Blocklist::load(&paths.blocklist)?;
    if let Ok(bytes) = std::fs::read(&paths.blocklist_cache) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            b = b.merged(Blocklist::from_server_config(&v));
        }
    }
    Ok(b)
}

/// Presigned target: refresh the cached server config (`GET /config`).
fn refresh_server_blocklist(rt: &tokio::runtime::Runtime, api_base: &str, paths: &Paths) {
    let res = rt.block_on(async {
        let api = cap_upload::api::ApiClient::new(api_base)?;
        tokio::time::timeout(Duration::from_secs(5), api.config())
            .await
            .map_err(|_| cap_upload::Error::Http("timeout".into()))?
    });
    match res {
        Ok(v) => {
            if let Err(e) = crate::config::save_json(&paths.blocklist_cache, &v, false) {
                tracing::warn!("caching server blocklist: {e:#}");
            }
        }
        Err(e) => tracing::warn!("could not fetch server config (using cached blocklist): {e}"),
    }
}

fn permission_hint(e: &RecorderError) -> &'static str {
    match e {
        RecorderError::Input(_, cap_input::InputError::PermissionDenied(_)) => {
            if cfg!(target_os = "linux") {
                "\nhint: add yourself to the `input` group (sudo usermod -aG input $USER) and log in again"
            } else if cfg!(target_os = "macos") {
                "\nhint: allow gamecap (or your terminal) under System Settings > Privacy & Security > Input Monitoring"
            } else {
                ""
            }
        }
        RecorderError::Capture(cap_capture::CaptureError::PermissionDenied) => {
            if cfg!(target_os = "macos") {
                "\nhint: allow gamecap (or your terminal) under System Settings > Privacy & Security > Screen Recording"
            } else {
                "\nhint: capture permission was denied (Wayland: accept the portal dialog)"
            }
        }
        RecorderError::Encode(cap_encode::EncodeError::NoHardwareEncoder(_)) => {
            "\nhint: no working hardware HEVC encoder; update GPU drivers, or set [encoder] force_encoder for testing"
        }
        _ => "",
    }
}

#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod portal {
    //! PipeWire source that persists the portal restore token in `state.json`.
    use super::*;
    use cap_capture::linux_pipewire::{PipeWireConfig, PipeWireSource};
    use std::path::PathBuf;
    use std::sync::Arc;

    pub struct PortalSource {
        pub inner: PipeWireSource,
        pub state_path: PathBuf,
    }

    impl PortalSource {
        pub fn new(state: &State, state_path: PathBuf) -> Self {
            let mut cfg = PipeWireConfig::default();
            cfg.portal.restore_token = state.wayland_restore_token.clone();
            Self { inner: PipeWireSource::new(cfg), state_path }
        }
    }

    impl FrameSource for PortalSource {
        fn start(&mut self, target: &WindowTarget, sink: Arc<cap_capture::LatestFrame>) -> cap_capture::Result<()> {
            let res = self.inner.start(target, sink);
            // Tokens are single-use: store the newest even if start failed later.
            let tok = self.inner.restore_token();
            if let Err(e) = crate::config::update_state(&self.state_path, |s| s.wayland_restore_token = tok) {
                tracing::warn!("saving portal restore token: {e:#}");
            }
            res
        }
        fn stop(&mut self) {
            self.inner.stop()
        }
        fn info(&self) -> Option<cap_capture::SourceInfo> {
            self.inner.info()
        }
    }
}

/// The platform frame source (Wayland: portal with the stored restore token).
pub fn platform_frame_source(state: &State, paths: &Paths) -> Result<Box<dyn FrameSource>> {
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    if cap_capture::default_backend()? == cap_capture::BackendKind::PipeWire {
        return Ok(Box::new(portal::PortalSource::new(state, paths.state.clone())));
    }
    let _ = (state, paths);
    Ok(cap_capture::default_source()?)
}

/// A running recording.
pub struct Session {
    recorder: Option<Recorder>,
    pub pause: PauseState,
    queue: Option<UploadQueue>,
    rt: Option<tokio::runtime::Runtime>,
    worker: Option<tokio::task::JoinHandle<()>>,
    sessions_root: std::path::PathBuf,
    disk_cap: u64,
    disk_used: u64,
    last_disk_check: Option<Instant>,
    last_status: Option<Instant>,
    started: Instant,
    duration: Option<Duration>,
    drain: Duration,
    pub ctrlc: CtrlC,
    tty: bool,
    status_json: bool,
    session_id: String,
    #[cfg_attr(not(all(feature = "tray", any(windows, target_os = "macos"))), allow(dead_code))]
    pub label: String,
    _locks: Vec<File>,
}

pub struct Summary {
    pub stats: RecorderStats,
    pub queue: Option<cap_upload::QueueStats>,
    pub drained: bool,
}

impl Session {
    pub fn start(cfg: &Config, paths: &Paths, opts: &RecordOpts, ctrlc: CtrlC) -> Result<Self> {
        let mut locks = Vec::new();
        let Some(l) = try_lock(&paths.record_lock)? else {
            bail!("another `gamecap record` is already running (lock {})", paths.record_lock.display());
        };
        locks.push(l);

        // Consent first: nothing is captured or uploaded without it.
        let state: State = crate::config::load_json(&paths.state)?;
        if opts.control_stdin && !crate::consent::has_consent(&state) {
            bail!("consent required: accept the terms first (`gamecap consent --accept`)");
        }
        let state = crate::consent::ensure(&paths.state, state)?;

        let rt = runtime()?;
        let presigned_api = cfg.upload.as_ref().and_then(|u| match u.target() {
            Ok(Some(crate::config::UploadTargetCfg::Presigned { api_base })) => Some(api_base),
            _ => None,
        });
        if let Some(api) = &presigned_api {
            refresh_server_blocklist(&rt, api, paths);
        }
        let blocklist = load_blocklist(paths)?;

        // Target.
        let (target, identity, focus): (WindowTarget, GameIdentity, Option<Box<dyn FocusTracker>>) = if opts.synthetic {
            let id = GameIdentity { game_id: opts.synthetic_game_id.clone(), publisher: None };
            let t = WindowTarget { native_id: 0, title: "synthetic".into(), game_id: id.game_id.clone(), pid: std::process::id() };
            (t, id, None)
        } else {
            let Some(q) = opts.game.as_deref() else { bail!("--game <game id | window title | native id> is required (or --synthetic)") };
            let tracker = cap_focus::default_tracker().context("focus tracker")?;
            let windows = tracker.list_windows().context("listing windows")?;
            let w = resolve_window(&windows, q)?;
            let t = WindowTarget { native_id: w.native_id, title: w.title.clone(), game_id: w.identity.game_id.clone(), pid: w.pid };
            (t, w.identity.clone(), Some(tracker))
        };
        if let Decision::Refused(why) = blocklist.check(&identity) {
            bail!("refusing to record: {why}");
        }

        std::fs::create_dir_all(&paths.sessions_root)
            .with_context(|| format!("creating sessions_root {}", paths.sessions_root.display()))?;
        let rep = cap_recorder::recover_partials(&paths.sessions_root);
        if !rep.finalized.is_empty() {
            eprintln!("recovered {} partial segment(s) from an earlier crash", rep.finalized.len());
        }
        for (p, why) in &rep.broken {
            tracing::warn!(path = %p.display(), "unrecoverable partial segment quarantined: {why}");
        }

        // Upload queue (in this process unless `gamecap upload` already runs one).
        let mut queue = None;
        let mut worker = None;
        if opts.no_upload {
            eprintln!("--no-upload: segments stay in {}", paths.sessions_root.display());
        } else if let Some((q, api_base)) = open_queue(cfg, paths, &rt)? {
            let added = q.scan_dir(&paths.sessions_root)?;
            if added > 0 {
                eprintln!("queued {added} segment(s) left from earlier runs");
            }
            match try_lock(&paths.upload_lock)? {
                Some(l) => {
                    locks.push(l);
                    worker = Some(rt.spawn(q.clone().run_owned()));
                    if let Some(api) = api_base {
                        rt.spawn(token_refresher(paths.clone(), api, q.clone()));
                    }
                }
                None => eprintln!("a `gamecap upload` process owns the uploader; new segments are queued for it"),
            }
            queue = Some(q);
        } else {
            eprintln!("no [upload] target configured: segments stay in {}", paths.sessions_root.display());
        }

        // Recorder.
        let settings = cfg.capture.settings();
        let mut rcfg = RecorderConfig::new(target.clone(), settings.clone(), paths.sessions_root.clone(), new_session_id());
        rcfg.encoder = cfg.encoder.encoder_config(&settings)?;
        rcfg.latency_offset_ns = cfg.latency_offset_ns();
        let pause = PauseState::new(rcfg.paused.clone());
        let hotkeys = Hotkeys {
            pause: pause.clone(),
            pause_key: cfg.hotkeys.pause_key,
            chat: cfg.chat_rule(&identity.game_id),
            blocklist: blocklist.clone(),
        };
        let sources = if opts.synthetic {
            let (w, h) = opts.synthetic_size;
            let mut focus = match opts.synthetic_alt_tab_secs {
                Some(p) if p > 0 => {
                    let p = p * 1000;
                    let script = (0..(86_400_000 / p)).flat_map(|i| [(i * p + p * 4 / 5, false), ((i + 1) * p, true)]).collect();
                    ScriptedFocusTracker::new(true, script)
                }
                _ => ScriptedFocusTracker::focused(),
            };
            focus.foreign_game_id = opts.synthetic_foreground.clone();
            Sources::new(
                Box::new(SyntheticSource::new(w, h, opts.synthetic_fps)),
                vec![Box::new(crate::synth::SyntheticInputs::new())],
                Box::new(focus),
            )
        } else {
            let inputs = cap_input::default_sources().context("input sources")?;
            Sources::new(platform_frame_source(&state, paths)?, inputs, focus.expect("tracker"))
        }
        .with_observer(hotkeys.into_observer());

        let q2 = queue.clone();
        let on_segment: cap_recorder::SegmentCallback = Box::new(move |dir| match &q2 {
            Some(q) => {
                if let Err(e) = q.enqueue(&dir) {
                    tracing::error!(path = %dir.display(), "enqueue failed: {e}");
                }
            }
            None => tracing::info!(path = %dir.display(), "segment kept locally"),
        });
        let session_id = rcfg.session_id.clone();
        let recorder = if opts.fake_encoder {
            let enc = FakeEncoder::new(settings.width, settings.height, settings.rate_hz);
            Recorder::start_with_backend(rcfg, sources, Box::new(enc), Box::new(FakeSinkFactory), on_segment)
        } else {
            let enc = FfmpegEncoder::open(&rcfg.encoder).map_err(RecorderError::from).map_err(|e| {
                let hint = permission_hint(&e);
                anyhow::anyhow!("{e}{hint}")
            })?;
            eprintln!("encoder: {}", cap_recorder::FrameEncoder::params(&enc).encoder_name);
            Recorder::start_with_backend(rcfg, sources, Box::new(enc), Box::new(FfmpegSinkFactory), on_segment)
        }
        .map_err(|e| {
            let hint = permission_hint(&e);
            anyhow::anyhow!("starting recorder: {e}{hint}")
        })?;

        let label = format!("{} ({})", identity.game_id, if opts.synthetic { "synthetic" } else { &target.title });
        eprintln!(
            "recording {label}: session {session_id}, {}x{} @ {} Hz, {} s segments -> {}",
            settings.width,
            settings.height,
            settings.rate_hz,
            settings.segment_secs,
            paths.sessions_root.join(&session_id).display()
        );
        if cfg.hotkeys.pause_key != 0 {
            eprintln!("pause key: scan code {:#04x}; Ctrl-C to stop", cfg.hotkeys.pause_key);
        }
        let drain = opts.drain_secs.or(cfg.upload.as_ref().and_then(|u| u.drain_secs)).unwrap_or(30);
        if opts.status_json {
            json_line(&serde_json::json!({
                "event": "started",
                "session": session_id,
                "game_id": identity.game_id,
                "title": target.title,
                "native_id": target.native_id,
                "width": settings.width,
                "height": settings.height,
                "rate_hz": settings.rate_hz,
                "segment_secs": settings.segment_secs,
                "dir": paths.sessions_root.join(&session_id),
            }));
        }
        if opts.control_stdin {
            spawn_stdin_control(ctrlc.clone(), pause.clone());
        }
        Ok(Self {
            recorder: Some(recorder),
            pause,
            queue,
            rt: Some(rt),
            worker,
            sessions_root: paths.sessions_root.clone(),
            disk_cap: cfg.disk_cap_bytes(),
            disk_used: 0,
            last_disk_check: None,
            last_status: None,
            started: Instant::now(),
            duration: opts.duration,
            drain: Duration::from_secs(drain),
            ctrlc,
            tty: stderr_is_tty(),
            status_json: opts.status_json,
            session_id,
            label,
            _locks: locks,
        })
    }

    pub fn stats(&self) -> RecorderStats {
        self.recorder.as_ref().map(|r| r.stats()).unwrap_or_default()
    }

    #[cfg_attr(not(all(feature = "tray", any(windows, target_os = "macos"))), allow(dead_code))]
    pub fn toggle_user_pause(&self) -> bool {
        self.pause.toggle(Reason::User)
    }

    fn check_disk(&mut self) {
        if self.last_disk_check.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
            return;
        }
        self.last_disk_check = Some(Instant::now());
        self.disk_used = cap_upload::disk_usage(&self.sessions_root).unwrap_or(self.disk_used);
        let paused = self.pause.is_set(Reason::DiskCap);
        // Hysteresis: pause at the cap, resume below 90% of it.
        if !paused && self.disk_used >= self.disk_cap {
            self.pause.set(Reason::DiskCap, true);
            let msg = format!(
                "disk cap reached ({} of {} in {}): recording PAUSED until uploads free space",
                fmt_bytes(self.disk_used),
                fmt_bytes(self.disk_cap),
                self.sessions_root.display()
            );
            self.clear_status_line();
            tracing::warn!("{msg}");
        } else if paused && self.disk_used < self.disk_cap / 10 * 9 {
            self.pause.set(Reason::DiskCap, false);
            self.clear_status_line();
            tracing::info!("disk usage {} below 90% of the cap: recording resumed", fmt_bytes(self.disk_used));
        }
    }

    /// The status line as a JSON object (`--status-json`).
    pub fn status_json(&self) -> serde_json::Value {
        let s = self.stats();
        let slots = s.frames_written + s.frames_dropped;
        let paused = self.pause.is_paused();
        let queue = self.queue.as_ref().and_then(|q| q.stats().ok()).map(|q| {
            serde_json::json!({"pending": q.pending, "uploading": q.uploading, "uploaded": q.uploaded, "verified": q.verified, "failed": q.failed})
        });
        serde_json::json!({
            "event": "status",
            "session": self.session_id,
            "elapsed_s": self.started.elapsed().as_secs_f64(),
            "state": if paused { "paused" } else if s.current_segment.is_some() { "recording" } else { "waiting" },
            "pause_reasons": if paused { self.pause.describe() } else { String::new() },
            "segment": s.current_segment,
            "frames": s.frames_written,
            "dropped": s.frames_dropped,
            "dropped_pct": if slots > 0 { s.frames_dropped as f64 * 100.0 / slots as f64 } else { 0.0 },
            "repeated_pct": s.repeated_ratio * 100.0,
            "inputs": s.input_events,
            "segments_done": s.segments_done,
            "segment_errors": s.segment_errors,
            "queue": queue,
            "disk_used": self.disk_used,
            "last_error": s.last_error,
        })
    }

    fn clear_status_line(&self) {
        if self.tty {
            eprint!("\r\x1b[2K");
        }
    }

    pub fn status_line(&self) -> String {
        let s = self.stats();
        let slots = s.frames_written + s.frames_dropped;
        let drop_pct = if slots > 0 { s.frames_dropped as f64 * 100.0 / slots as f64 } else { 0.0 };
        let state = if self.pause.is_paused() {
            format!("PAUSED ({})", self.pause.describe())
        } else {
            match s.current_segment {
                Some(i) => format!("REC seg {i}"),
                None => "REC waiting".into(),
            }
        };
        let q = match &self.queue {
            Some(q) => q.stats().map(|st| fmt_stats(&st)).unwrap_or_else(|e| e.to_string()),
            None => "off".into(),
        };
        let mut line = format!(
            "{:>6.0}s {state} | frames {} drop {:.2}% rep {:.1}% | inputs {} | segs {}{} | upload {q} | disk {}",
            self.started.elapsed().as_secs_f64(),
            s.frames_written,
            drop_pct,
            s.repeated_ratio * 100.0,
            s.input_events,
            s.segments_done,
            if s.segment_errors > 0 { format!(" ({} errors)", s.segment_errors) } else { String::new() },
            fmt_bytes(self.disk_used),
        );
        if let Some(e) = &s.last_error {
            line.push_str(&format!(" | last error: {e}"));
        }
        line
    }

    /// Call a few times per second. Returns false when recording should stop.
    pub fn tick(&mut self) -> bool {
        self.check_disk();
        if self.duration.is_some_and(|d| self.started.elapsed() >= d) {
            self.ctrlc.trigger();
        }
        let every = if self.tty || self.status_json { 1 } else { 5 };
        let due = self.last_status.is_none_or(|t| t.elapsed() >= Duration::from_secs(every));
        if due {
            self.last_status = Some(Instant::now());
            if self.status_json {
                json_line(&self.status_json());
            }
            let line = self.status_line();
            if self.tty {
                eprint!("\r\x1b[2K{line}");
            } else {
                eprintln!("{line}");
            }
            let _ = std::io::stderr().flush();
        }
        !self.ctrlc.triggered()
    }

    /// Stop recording, then let the uploader drain for up to `drain` seconds.
    pub fn finish(mut self) -> Result<Summary> {
        self.clear_status_line();
        eprintln!();
        let stats = self.recorder.take().map(|r| r.stop()).unwrap_or_default();
        let slots = stats.frames_written + stats.frames_dropped;
        eprintln!(
            "recording stopped: {} segment(s), {} frames, {} dropped ({:.3}%), {:.1}% repeated, {} inputs logged, {} gated, {} lost, tick jitter mean {:.2} ms max {:.2} ms",
            stats.segments_done,
            stats.frames_written,
            stats.frames_dropped,
            if slots > 0 { stats.frames_dropped as f64 * 100.0 / slots as f64 } else { 0.0 },
            stats.repeated_ratio * 100.0,
            stats.input_events,
            stats.input_events_gated,
            stats.input_events_lost,
            stats.jitter.mean_late_ns / 1e6,
            stats.jitter.max_late_ns as f64 / 1e6,
        );
        let mut drained = true;
        let mut qstats = None;
        if let Some(q) = &self.queue {
            if self.worker.is_some() {
                let deadline = Instant::now() + self.drain;
                let mut last = String::new();
                loop {
                    let st = q.stats()?;
                    if st.outstanding() == 0 {
                        break;
                    }
                    if Instant::now() >= deadline || self.ctrlc.count() >= 2 {
                        drained = false;
                        eprintln!("upload drain stopped with {} segment(s) outstanding; they upload next run", st.outstanding());
                        break;
                    }
                    let line = fmt_stats(&st);
                    if line != last {
                        eprintln!("draining uploads: {line}");
                        last = line;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                q.shutdown();
            }
            let st = q.stats()?;
            eprintln!("upload queue: {}", fmt_stats(&st));
            qstats = Some(st);
        }
        if let (Some(rt), Some(w)) = (self.rt.as_ref(), self.worker.take()) {
            let _ = rt.block_on(async { tokio::time::timeout(Duration::from_secs(10), w).await });
        }
        if let Some(rt) = self.rt.take() {
            rt.shutdown_timeout(Duration::from_secs(2));
        }
        Ok(Summary { stats, queue: qstats, drained })
    }
}

pub fn cmd_record(cfg: &Config, paths: &Paths, opts: RecordOpts, ctrlc: CtrlC) -> Result<()> {
    let mut session = Session::start(cfg, paths, &opts, ctrlc)?;
    if opts.tray {
        #[cfg(all(feature = "tray", any(windows, target_os = "macos")))]
        {
            return crate::tray::run(session).map(|_| ());
        }
        #[cfg(not(all(feature = "tray", any(windows, target_os = "macos"))))]
        tracing::warn!("--tray: this build has no tray support (feature `tray`, Windows/macOS only); running in the terminal");
    }
    while session.tick() {
        std::thread::sleep(Duration::from_millis(200));
    }
    let status_json = opts.status_json;
    let sum = session.finish()?;
    if status_json {
        let s = &sum.stats;
        json_line(&serde_json::json!({
            "event": "stopped",
            "segments": s.segments_done,
            "frames": s.frames_written,
            "dropped": s.frames_dropped,
            "inputs": s.input_events,
            "drained": sum.drained,
            "outstanding": sum.queue.as_ref().map(|q| q.outstanding()),
        }));
    }
    if let Some(q) = &sum.queue {
        if !sum.drained || q.failed > 0 {
            eprintln!("{} segment(s) not uploaded yet, {} failed; run `gamecap upload` or `gamecap status`", q.outstanding(), q.failed);
        }
    }
    if sum.stats.segment_errors > 0 {
        eprintln!("warning: {} segment(s) could not be finalized (left as .partial for recovery)", sum.stats.segment_errors);
    }
    Ok(())
}

pub fn json_line(v: &serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

/// `--control-stdin`: `stop` (or EOF) stops, `stop-now` also skips the upload
/// drain, `pause` toggles the user pause.
fn spawn_stdin_control(ctrlc: CtrlC, pause: PauseState) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            // .NET parents write a UTF-8 BOM before the first line.
            let cmd = line.trim().trim_start_matches('\u{feff}');
            tracing::debug!(cmd, "control");
            match cmd {
                "stop" => ctrlc.trigger(),
                "stop-now" => ctrlc.trigger_again(),
                "pause" => {
                    let now = pause.toggle(Reason::User);
                    tracing::info!("{} (control)", if now { "paused" } else { "resumed" });
                }
                "" => {}
                other => tracing::warn!("unknown control command {other:?}"),
            }
        }
        ctrlc.trigger();
    });
}

/// `gamecap windows`.
pub fn cmd_windows(json: bool) -> Result<()> {
    let tracker = cap_focus::default_tracker().context("focus tracker (Linux needs an X11/XWayland DISPLAY)")?;
    let mut ws = tracker.list_windows()?;
    ws.sort_by_key(|a| a.identity.game_id.to_lowercase());
    if json {
        let v: Vec<_> = ws
            .iter()
            .map(|w| {
                serde_json::json!({"native_id": w.native_id, "pid": w.pid, "game_id": w.identity.game_id,
                    "publisher": w.identity.publisher, "title": w.title})
            })
            .collect();
        println!("{}", serde_json::Value::Array(v));
        return Ok(());
    }
    println!("{:<14} {:>8}  {:<32} TITLE", "NATIVE_ID", "PID", "GAME_ID");
    for w in ws {
        let pubr = w.identity.publisher.as_deref().map(|p| format!(" [{p}]")).unwrap_or_default();
        println!("{:<14} {:>8}  {:<32} {}{pubr}", format!("{:#x}", w.native_id), w.pid, w.identity.game_id, w.title);
    }
    match cap_capture::default_backend() {
        Ok(b) => println!("\ncapture backend: {b:?}"),
        Err(e) => println!("\ncapture backend: unavailable ({e})"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(id: u64, game: &str, title: &str) -> WindowInfo {
        WindowInfo { native_id: id, title: title.into(), pid: id as u32, identity: GameIdentity { game_id: game.into(), publisher: None } }
    }

    #[test]
    fn window_resolution() {
        let ws = vec![
            w(0x1a, "eldenring.exe", "ELDEN RING"),
            w(0x2b, "firefox", "Elden Ring wiki - Mozilla Firefox"),
            w(0x3c, "steam", "Steam"),
            w(0x4d, "game.exe", "Launcher"),
            w(0x4e, "game.exe", "Game Main Window"),
        ];
        assert_eq!(resolve_window(&ws, "0x2b").unwrap().native_id, 0x2b);
        assert_eq!(resolve_window(&ws, "60").unwrap().native_id, 0x3c);
        assert_eq!(resolve_window(&ws, "EldenRing.exe").unwrap().native_id, 0x1a);
        assert_eq!(resolve_window(&ws, "steam").unwrap().native_id, 0x3c, "game id before title");
        assert_eq!(resolve_window(&ws, "mozilla").unwrap().native_id, 0x2b);
        let e = resolve_window(&ws, "elden").unwrap_err().to_string();
        assert!(e.contains("several windows"), "{e}");
        assert_eq!(resolve_window(&ws, "window").unwrap().native_id, 0x4e);
        assert!(resolve_window(&ws, "nothing").is_err());
    }
}
