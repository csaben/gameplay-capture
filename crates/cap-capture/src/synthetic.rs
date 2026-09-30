//! Synthetic CPU frame source for tests and pipeline bring-up: draws a moving
//! gradient with a frame counter bar at a configurable rate.

use crate::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

pub struct SyntheticSource {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SyntheticSource {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self { width, height, fps, running: Arc::new(AtomicBool::new(false)), thread: None }
    }
}

pub fn draw_frame(width: u32, height: u32, n: u64) -> Vec<u8> {
    let mut data = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            data[i] = ((x as u64 + n * 4) % 256) as u8; // B
            data[i + 1] = ((y as u64 + n * 2) % 256) as u8; // G
            data[i + 2] = ((n * 8) % 256) as u8; // R
            data[i + 3] = 255;
        }
    }
    data
}

impl FrameSource for SyntheticSource {
    fn start(&mut self, _target: &WindowTarget, sink: Arc<LatestFrame>) -> Result<()> {
        let (w, h, fps) = (self.width, self.height, self.fps.max(1));
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        self.thread = Some(std::thread::spawn(move || {
            let period = 1_000_000_000 / fps as i64;
            let mut next = cap_clock::now_ns();
            let mut n = 0u64;
            while running.load(Ordering::SeqCst) {
                let data = draw_frame(w, h, n);
                sink.publish(CapturedFrame {
                    capture_ns: cap_clock::now_ns(),
                    width: w,
                    height: h,
                    format: PixelFormat::Bgra8,
                    payload: FramePayload::Cpu { data, stride: (w * 4) as usize },
                });
                n += 1;
                next += period;
                cap_clock::sleep_until(next);
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
    fn info(&self) -> Option<SourceInfo> {
        Some(SourceInfo { backend: "synthetic", width: self.width, height: self.height, hdr: false })
    }
}
