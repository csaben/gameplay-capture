//! `gamecap calibrate`: measure input-to-captured-frame latency.
//!
//! A black test window turns white on every key press. The window is
//! captured with the platform `FrameSource` and key presses are read from the
//! platform `InputSource`s (the same backends `record` uses, so both
//! timestamps are on `cap_clock`). Per trial:
//!
//! `L = capture_ns(first bright frame after the key_down) - t_ns(key_down)`
//!
//! (L > 0: captured frames show an input L after it was logged.) The config
//! stores **`latency_offset_ns = -median(L)`**, the sign `pipeline/SCHEMA.md`
//! expects: the pipeline takes frame k's action window as
//! `[capture_ns[k] + latency_offset_ns, capture_ns[k+1] + latency_offset_ns)`,
//! i.e. the inputs that lead from frame k to frame k+1. The value is copied
//! into every manifest.

use cap_capture::{CapturedFrame, FramePayload, PixelFormat};
use cap_types::Nanos;

/// Frames with mean luma at or above this (0..1) count as "white".
pub const BRIGHT: f32 = 0.5;
/// Trials slower than this are treated as missed.
pub const MAX_LATENCY_NS: Nanos = 1_000_000_000;

/// Per-trial latencies from key-down times and (capture_ns, luma) samples.
///
/// A trial counts only if the last frame captured at or before the key-down
/// was dark (the previous flash had ended) and the first bright frame after
/// it arrives before the next key-down and within [`MAX_LATENCY_NS`].
pub fn measure(keydowns: &[Nanos], frames: &[(Nanos, f32)], threshold: f32) -> Vec<Nanos> {
    let mut keys = keydowns.to_vec();
    keys.sort_unstable();
    let mut fr = frames.to_vec();
    fr.sort_by_key(|f| f.0);
    let mut out = Vec::new();
    for (i, &t) in keys.iter().enumerate() {
        let next_key = keys.get(i + 1).copied().unwrap_or(Nanos::MAX);
        let split = fr.partition_point(|f| f.0 <= t);
        match split.checked_sub(1).map(|j| fr[j]) {
            Some((_, luma)) if luma < threshold => {}
            _ => continue, // no frame yet, or still white from the last trial
        }
        if let Some(&(c, _)) = fr[split..].iter().find(|f| f.1 >= threshold) {
            if c < next_key && c - t <= MAX_LATENCY_NS {
                out.push(c - t);
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub n: usize,
    pub median: Nanos,
    pub p90: Nanos,
    pub min: Nanos,
    pub max: Nanos,
}

/// Median (lower-middle for even n) and nearest-rank p90.
pub fn summarize(lat: &[Nanos]) -> Option<Summary> {
    if lat.is_empty() {
        return None;
    }
    let mut v = lat.to_vec();
    v.sort_unstable();
    let n = v.len();
    let median = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2 };
    let rank = ((0.9 * n as f64).ceil() as usize).clamp(1, n);
    Some(Summary { n, median, p90: v[rank - 1], min: v[0], max: v[n - 1] })
}

/// Stored `latency_offset_ns` for a measured input-to-frame latency `L`
/// (pipeline convention: negative when frames lag inputs).
pub fn offset_from_latency(latency_ns: Nanos) -> Nanos {
    -latency_ns
}

fn half_to_f32(h: u16) -> f32 {
    let s = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = ((h >> 10) & 0x1f) as i32;
    let m = (h & 0x3ff) as f32;
    s * match e {
        0 => m * 2f32.powi(-24),
        31 => f32::INFINITY,
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

/// Mean luma (0..1) of a CPU buffer, sampled on a 32x18 grid.
pub fn cpu_luma(data: &[u8], stride: usize, width: u32, height: u32, format: PixelFormat) -> Option<f32> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let mut sum = 0.0f32;
    let mut n = 0u32;
    for gy in 0..18 {
        let y = (gy * 2 + 1) * h / 36;
        for gx in 0..32 {
            let x = (gx * 2 + 1) * w / 64;
            let v = match format {
                PixelFormat::Bgra8 | PixelFormat::Rgba8 => {
                    let i = y * stride + x * 4;
                    let p = data.get(i..i + 3)?;
                    (p[0] as f32 + p[1] as f32 + p[2] as f32) / (3.0 * 255.0)
                }
                PixelFormat::Nv12 => (*data.get(y * stride + x)? as f32 - 16.0) / 219.0,
                PixelFormat::Rgba16f => {
                    let i = y * stride + x * 8;
                    let p = data.get(i..i + 6)?;
                    let c = |k: usize| half_to_f32(u16::from_le_bytes([p[k], p[k + 1]])).clamp(0.0, 1.0);
                    (c(0) + c(2) + c(4)) / 3.0
                }
            };
            sum += v;
            n += 1;
        }
    }
    Some(sum / n as f32)
}

/// Mean luma of any captured frame, reading GPU payloads back where supported.
pub fn frame_luma(f: &CapturedFrame) -> Option<f32> {
    match &f.payload {
        FramePayload::Cpu { data, stride } => cpu_luma(data, *stride, f.width, f.height, f.format),
        #[cfg(windows)]
        FramePayload::D3D11 { texture, device } => d3d11_luma(texture, device, f),
        #[cfg(target_os = "macos")]
        FramePayload::CvPixelBuffer(pb) => cv_luma(pb),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

#[cfg(windows)]
fn d3d11_luma(
    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    f: &CapturedFrame,
) -> Option<f32> {
    use windows::Win32::Graphics::Direct3D11::*;
    // Copy to a CPU-readable staging texture (calibration only; not a hot path).
    // SAFETY: plain D3D11 calls on live COM objects; the mapped pointer is
    // only read within the RowPitch * Height range and unmapped before return.
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        let mut staging: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging)).ok()?;
        let staging = staging?;
        let ctx = device.GetImmediateContext().ok()?;
        ctx.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
        let len = mapped.RowPitch as usize * desc.Height as usize;
        let data = std::slice::from_raw_parts(mapped.pData as *const u8, len);
        let luma = cpu_luma(data, mapped.RowPitch as usize, desc.Width, desc.Height, f.format);
        ctx.Unmap(&staging, 0);
        luma
    }
}

#[cfg(target_os = "macos")]
fn cv_luma(pb: &cap_capture::macos::RetainedPixelBuffer) -> Option<f32> {
    use std::ffi::c_void;
    #[link(name = "CoreVideo", kind = "framework")]
    extern "C" {
        fn CVPixelBufferLockBaseAddress(b: *mut c_void, flags: u64) -> i32;
        fn CVPixelBufferUnlockBaseAddress(b: *mut c_void, flags: u64) -> i32;
        fn CVPixelBufferIsPlanar(b: *mut c_void) -> u8;
        fn CVPixelBufferGetBaseAddress(b: *mut c_void) -> *mut c_void;
        fn CVPixelBufferGetBytesPerRow(b: *mut c_void) -> usize;
        fn CVPixelBufferGetBaseAddressOfPlane(b: *mut c_void, plane: usize) -> *mut c_void;
        fn CVPixelBufferGetBytesPerRowOfPlane(b: *mut c_void, plane: usize) -> usize;
    }
    const READ_ONLY: u64 = 1;
    let b = pb.as_ptr();
    let (w, h) = (pb.width() as u32, pb.height() as u32);
    // SAFETY: b is a live retained CVPixelBuffer; base addresses are valid
    // for bytes_per_row * height while locked.
    unsafe {
        if CVPixelBufferLockBaseAddress(b, READ_ONLY) != 0 {
            return None;
        }
        let res = if CVPixelBufferIsPlanar(b) != 0 {
            let base = CVPixelBufferGetBaseAddressOfPlane(b, 0) as *const u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(b, 0);
            (!base.is_null())
                .then(|| cpu_luma(std::slice::from_raw_parts(base, stride * h as usize), stride, w, h, PixelFormat::Nv12))
                .flatten()
        } else {
            let base = CVPixelBufferGetBaseAddress(b) as *const u8;
            let stride = CVPixelBufferGetBytesPerRow(b);
            (!base.is_null())
                .then(|| cpu_luma(std::slice::from_raw_parts(base, stride * h as usize), stride, w, h, PixelFormat::Bgra8))
                .flatten()
        };
        CVPixelBufferUnlockBaseAddress(b, READ_ONLY);
        res
    }
}

#[cfg(feature = "calibrate")]
pub use ui::cmd_calibrate;

#[cfg(feature = "calibrate")]
mod ui {
    use super::*;
    use crate::config::{set_config_int, Paths, State};
    use anyhow::{bail, Context, Result};
    use cap_capture::{FrameSource, LatestFrame, WindowTarget};
    use cap_types::{Device, EventKind};
    use std::num::NonZeroU32;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use winit::application::ApplicationHandler;
    use winit::event::{ElementState, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::keyboard::{KeyCode, PhysicalKey};
    use winit::window::{Window, WindowId};

    const TITLE: &str = "gamecap latency calibration";
    const FLASH: Duration = Duration::from_millis(350);
    const ESCAPE_SCAN: u32 = 0x01;

    struct App {
        trials: usize,
        window: Option<Rc<Window>>,
        surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
        white_until: Option<Instant>,
        created: Option<Instant>,
        presses: usize,
        last_press: Option<Instant>,
        capture: Option<Box<dyn FrameSource>>,
        capture_error: Option<String>,
        state: State,
        paths: Paths,
        latest: Arc<LatestFrame>,
        announced: bool,
    }

    impl App {
        fn draw(&mut self) {
            let (Some(w), Some(s)) = (self.window.as_ref(), self.surface.as_mut()) else { return };
            let size = w.inner_size();
            let (Some(wd), Some(ht)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
            if s.resize(wd, ht).is_err() {
                return;
            }
            let white = self.white_until.is_some_and(|t| Instant::now() < t);
            if let Ok(mut buf) = s.buffer_mut() {
                buf.fill(if white { 0x00FF_FFFF } else { 0 });
                let _ = buf.present();
            }
        }

        fn start_capture(&mut self) {
            let pid = std::process::id();
            let found = cap_focus::default_tracker()
                .and_then(|t| t.list_windows())
                .map_err(|e| e.to_string())
                .and_then(|ws| {
                    ws.into_iter()
                        .find(|w| w.pid == pid && w.title.contains(TITLE))
                        .or(None)
                        .ok_or_else(|| "calibration window not found in the window list".to_string())
                });
            let res = found.and_then(|w| {
                let target = WindowTarget { native_id: w.native_id, title: w.title, game_id: w.identity.game_id, pid };
                let mut src = crate::record::platform_frame_source(&self.state, &self.paths).map_err(|e| e.to_string())?;
                src.start(&target, self.latest.clone()).map_err(|e| e.to_string())?;
                Ok(src)
            });
            match res {
                Ok(src) => self.capture = Some(src),
                Err(e) => self.capture_error = Some(e),
            }
        }
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            let attrs = Window::default_attributes()
                .with_title(TITLE)
                .with_inner_size(winit::dpi::LogicalSize::new(640.0, 360.0));
            let w = match el.create_window(attrs) {
                Ok(w) => Rc::new(w),
                Err(e) => {
                    self.capture_error = Some(format!("create window: {e}"));
                    el.exit();
                    return;
                }
            };
            let surface = softbuffer::Context::new(w.clone()).and_then(|ctx| softbuffer::Surface::new(&ctx, w.clone()));
            match surface {
                Ok(s) => self.surface = Some(s),
                Err(e) => {
                    self.capture_error = Some(format!("softbuffer: {e}"));
                    el.exit();
                    return;
                }
            }
            self.window = Some(w);
            self.created = Some(Instant::now());
            self.draw();
        }

        fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::CloseRequested => el.exit(),
                WindowEvent::RedrawRequested => self.draw(),
                WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed && !event.repeat => {
                    if event.physical_key == PhysicalKey::Code(KeyCode::Escape) {
                        el.exit();
                        return;
                    }
                    // Ignore presses while still white (they cannot be measured).
                    if self.white_until.is_some_and(|t| Instant::now() < t + FLASH) {
                        return;
                    }
                    self.white_until = Some(Instant::now() + FLASH);
                    self.presses += 1;
                    self.last_press = Some(Instant::now());
                    self.draw();
                    eprint!("\rtrial {}/{}   ", self.presses, self.trials);
                }
                _ => {}
            }
        }

        fn about_to_wait(&mut self, el: &ActiveEventLoop) {
            let now = Instant::now();
            if self.capture.is_none() && self.capture_error.is_none() && self.created.is_some_and(|t| now - t > Duration::from_millis(700)) {
                self.start_capture();
                if let Some(e) = &self.capture_error {
                    eprintln!("capture failed: {e}");
                    el.exit();
                    return;
                }
                if !self.announced {
                    self.announced = true;
                    eprintln!(
                        "Focus the calibration window and press any key (not Esc) {} times, about once a second.",
                        self.trials
                    );
                }
            }
            if let Some(t) = self.white_until {
                if now >= t {
                    self.white_until = None;
                    self.draw();
                }
            }
            if self.presses >= self.trials && self.last_press.is_some_and(|t| now - t > Duration::from_millis(800)) {
                el.exit();
            }
            el.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_millis(5)));
        }
    }

    pub fn cmd_calibrate(paths: &Paths, trials: usize, save: bool) -> Result<()> {
        let state: State = crate::config::load_json(&paths.state)?;
        // Inputs: the same passive backends `record` uses.
        let (tx, rx) = crossbeam_channel::bounded(65_536);
        let mut inputs = cap_input::default_sources().context("input sources")?;
        for s in inputs.iter_mut() {
            s.start(tx.clone()).map_err(|e| anyhow::anyhow!("input source {}: {e}", s.name()))?;
        }
        drop(tx);
        let keydowns: Arc<Mutex<Vec<Nanos>>> = Arc::default();
        let kd = keydowns.clone();
        let collector = std::thread::spawn(move || {
            for e in rx.iter() {
                if e.device == Device::Keyboard && e.kind == EventKind::KeyDown && e.code != ESCAPE_SCAN {
                    kd.lock().unwrap().push(e.t_ns);
                }
            }
        });

        // Frame sampler: luma of every new captured frame.
        let latest = LatestFrame::new();
        let samples: Arc<Mutex<Vec<(Nanos, f32)>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let unreadable = Arc::new(AtomicBool::new(false));
        let sampler = {
            let (latest, samples, stop, unreadable) = (latest.clone(), samples.clone(), stop.clone(), unreadable.clone());
            std::thread::spawn(move || {
                let mut last_seq = 0;
                while !stop.load(Ordering::SeqCst) {
                    let (seq, f) = latest.latest();
                    if seq != last_seq {
                        last_seq = seq;
                        if let Some(f) = f {
                            match frame_luma(&f) {
                                Some(l) => samples.lock().unwrap().push((f.capture_ns, l)),
                                None => unreadable.store(true, Ordering::SeqCst),
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_micros(500));
                }
            })
        };

        let event_loop = EventLoop::new().context("no display for the calibration window")?;
        let mut app = App {
            trials,
            window: None,
            surface: None,
            white_until: None,
            created: None,
            presses: 0,
            last_press: None,
            capture: None,
            capture_error: None,
            state,
            paths: paths.clone(),
            latest,
            announced: false,
        };
        event_loop.run_app(&mut app)?;
        eprintln!();
        if let Some(mut c) = app.capture.take() {
            c.stop();
        }
        stop.store(true, Ordering::SeqCst);
        let _ = sampler.join();
        for s in inputs.iter_mut() {
            s.stop();
        }
        drop(inputs);
        let _ = collector.join();
        if let Some(e) = app.capture_error {
            bail!("calibration failed: {e}");
        }
        if unreadable.load(Ordering::SeqCst) {
            tracing::warn!("some frames had a payload that cannot be read back on the CPU (e.g. DMA-BUF); they were ignored");
        }
        let kd = keydowns.lock().unwrap().clone();
        let fr = samples.lock().unwrap().clone();
        let lat = measure(&kd, &fr, BRIGHT);
        eprintln!("{} key presses logged, {} frames captured, {} usable trials", kd.len(), fr.len(), lat.len());
        let Some(s) = summarize(&lat) else { bail!("no usable trials (was the window focused and captured?)") };
        println!(
            "input -> captured frame latency over {} trials: median {:.1} ms, p90 {:.1} ms (min {:.1}, max {:.1})",
            s.n,
            s.median as f64 / 1e6,
            s.p90 as f64 / 1e6,
            s.min as f64 / 1e6,
            s.max as f64 / 1e6
        );
        if s.n < 5 {
            bail!("fewer than 5 usable trials; not saving (run again)");
        }
        if save {
            let offset = offset_from_latency(s.median);
            set_config_int(&paths.config_file, "latency_offset_ns", offset)?;
            println!("saved latency_offset_ns = {offset} (= -median) to {}", paths.config_file.display());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Nanos = 1_000_000;

    /// A synthetic 60 Hz capture where the screen turns white `lag` after each
    /// key press and stays white for 300 ms.
    fn timeline(keys: &[Nanos], lags: &[Nanos]) -> Vec<(Nanos, f32)> {
        let mut frames = Vec::new();
        let period = 16_666_667;
        let mut t = 0;
        while t < 30_000 * MS {
            let white = keys.iter().zip(lags).any(|(&k, &l)| t >= k + l && t < k + l + 300 * MS);
            frames.push((t, if white { 0.97 } else { 0.02 }));
            t += period;
        }
        frames
    }

    #[test]
    fn measures_latency_per_trial() {
        let keys: Vec<Nanos> = (0..20).map(|i| 1000 * MS + i * 1000 * MS + i * 7 * MS).collect();
        let lags: Vec<Nanos> = (0..20).map(|i| 30 * MS + (i % 5) * 10 * MS).collect();
        let frames = timeline(&keys, &lags);
        let lat = measure(&keys, &frames, BRIGHT);
        assert_eq!(lat.len(), 20);
        for (l, lag) in lat.iter().zip(&lags) {
            // The first white frame is the first 60 Hz sample at or after key+lag.
            assert!(*l >= *lag && *l < *lag + 16_666_667 + 1, "{l} vs {lag}");
        }
        let s = summarize(&lat).unwrap();
        assert!(s.median >= 50 * MS && s.median < 67 * MS, "{s:?}");
        assert!(s.p90 >= 70 * MS && s.p90 < 87 * MS, "{s:?}");
        assert!(s.min >= 30 * MS && s.max < 87 * MS);
    }

    #[test]
    fn skips_ambiguous_trials() {
        let frames = timeline(&[1000 * MS, 5000 * MS], &[40 * MS, 40 * MS]);
        // Second press while still white (1100 ms), third never shows up,
        // fourth before any frame exists.
        let lat = measure(&[-5 * MS, 1000 * MS, 1100 * MS, 3000 * MS, 5000 * MS], &frames, BRIGHT);
        assert_eq!(lat.len(), 2, "{lat:?}");
        assert!(lat.iter().all(|&l| (40 * MS..57 * MS).contains(&l)));
        assert!(measure(&[], &frames, BRIGHT).is_empty());
        assert_eq!(summarize(&[]), None);
    }

    #[test]
    fn offset_sign_matches_pipeline() {
        // Frame k captured at c_k shows inputs from before c_k - L; the
        // pipeline window [c_k + off, c_k+1 + off) must start L earlier.
        let (l, c_k) = (40 * MS, 10_000 * MS);
        let off = offset_from_latency(l);
        assert_eq!(off, -40 * MS);
        assert_eq!(c_k + off, c_k - l);
    }

    #[test]
    fn summary_stats() {
        let s = summarize(&[5, 1, 4, 2, 3, 10, 9, 8, 7, 6]).unwrap();
        assert_eq!((s.n, s.median, s.p90, s.min, s.max), (10, 5, 9, 1, 10));
        let s = summarize(&[7]).unwrap();
        assert_eq!((s.median, s.p90), (7, 7));
    }

    #[test]
    fn luma_of_cpu_frames() {
        let (w, h) = (64u32, 36u32);
        let white = vec![255u8; (w * h * 4) as usize];
        let black: Vec<u8> = (0..w * h).flat_map(|_| [0, 0, 0, 255]).collect();
        assert!(cpu_luma(&white, (w * 4) as usize, w, h, PixelFormat::Bgra8).unwrap() > 0.99);
        assert!(cpu_luma(&black, (w * 4) as usize, w, h, PixelFormat::Bgra8).unwrap() < 0.01);
        assert!(cpu_luma(&white[..10], (w * 4) as usize, w, h, PixelFormat::Bgra8).is_none(), "short buffer");
        let one = 0x3C00u16.to_le_bytes(); // 1.0 in f16
        let hdr: Vec<u8> = (0..w * h).flat_map(|_| [one[0], one[1], one[0], one[1], one[0], one[1], one[0], one[1]]).collect();
        assert!(cpu_luma(&hdr, (w * 8) as usize, w, h, PixelFormat::Rgba16f).unwrap() > 0.99);
        assert_eq!(half_to_f32(0x3800), 0.5);
    }
}
