//! Synthetic input source for `record --synthetic`: an endless, deterministic
//! mix of keyboard, mouse and gamepad events stamped on `cap_clock`, so the
//! whole engine (gating, segmenting, parquet, upload) can run on a headless box.

use cap_input::InputSource;
use cap_types::{mouse_axis, mouse_button, wheel_axis, Device, EventKind, InputEvent, Nanos};
use crossbeam_channel::{Sender, TrySendError};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// W, A, S, D, Space, Left Shift (scan code set 1).
const KEYS: [u32; 6] = [0x11, 0x1E, 0x1F, 0x20, 0x39, 0x2A];

pub struct SyntheticInputs {
    running: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl SyntheticInputs {
    pub fn new() -> Self {
        Self { running: Arc::new(AtomicBool::new(false)), dropped: Arc::default(), thread: None }
    }
}

impl Default for SyntheticInputs {
    fn default() -> Self {
        Self::new()
    }
}

/// Events due in the 10 ms step `step` (pure, for tests).
pub fn events_for_step(step: u64, t_ns: Nanos) -> Vec<InputEvent> {
    let ev = |device, kind, code, value| InputEvent { t_ns, device, kind, code, value };
    let mut out = Vec::new();
    // Mouse: 100 Hz relative motion on a slow circle.
    let a = step as f32 * 0.02;
    out.push(ev(Device::Mouse, EventKind::MouseMove, mouse_axis::X, (a.cos() * 6.0).round()));
    out.push(ev(Device::Mouse, EventKind::MouseMove, mouse_axis::Y, (a.sin() * 4.0).round()));
    // Keys: every 400 ms press the next key, release it 250 ms later.
    let k = KEYS[((step / 40) % KEYS.len() as u64) as usize];
    match step % 40 {
        0 => out.push(ev(Device::Keyboard, EventKind::KeyDown, k, 1.0)),
        25 => out.push(ev(Device::Keyboard, EventKind::KeyUp, k, 0.0)),
        _ => {}
    }
    // Left click every 2 s (held 100 ms); wheel notch every 3 s.
    match step % 200 {
        50 => out.push(ev(Device::Mouse, EventKind::MouseButton, mouse_button::LEFT, 1.0)),
        60 => out.push(ev(Device::Mouse, EventKind::MouseButton, mouse_button::LEFT, 0.0)),
        _ => {}
    }
    if step % 300 == 150 {
        out.push(ev(Device::Mouse, EventKind::Wheel, wheel_axis::VERTICAL, -1.0));
    }
    // Gamepad: left stick X sweeps at 20 Hz; button A tap every 5 s.
    if step.is_multiple_of(5) {
        out.push(ev(Device::Gamepad, EventKind::Axis, cap_input::gamepad_code::axis::LEFT_STICK_X, (step as f32 * 0.01).sin()));
    }
    match step % 500 {
        100 => out.push(ev(Device::Gamepad, EventKind::Button, cap_input::gamepad_code::button::SOUTH, 1.0)),
        120 => out.push(ev(Device::Gamepad, EventKind::Button, cap_input::gamepad_code::button::SOUTH, 0.0)),
        _ => {}
    }
    out
}

impl InputSource for SyntheticInputs {
    fn start(&mut self, tx: Sender<InputEvent>) -> cap_input::Result<()> {
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let dropped = self.dropped.clone();
        self.thread = Some(
            std::thread::Builder::new()
                .name("gamecap-synth-input".into())
                .spawn(move || {
                    let t0 = cap_clock::now_ns();
                    let mut step = 0u64;
                    while running.load(Ordering::SeqCst) {
                        let due = t0 + step as Nanos * 10_000_000;
                        cap_clock::sleep_until(due);
                        for e in events_for_step(step, cap_clock::now_ns()) {
                            match tx.try_send(e) {
                                Ok(()) => {}
                                Err(TrySendError::Full(_)) => {
                                    dropped.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(TrySendError::Disconnected(_)) => return,
                            }
                        }
                        step += 1;
                    }
                })
                .map_err(|e| cap_input::InputError::Backend(e.to_string()))?,
        );
        Ok(())
    }
    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
    fn name(&self) -> &'static str {
        "synthetic"
    }
    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_presses() {
        let mut held = cap_focus::HeldInputs::new();
        let mut n = 0;
        for step in 0..4000 {
            for e in events_for_step(step, step as i64) {
                held.observe(&e);
                n += 1;
            }
        }
        assert!(n > 8000);
        // At a step boundary where nothing should be held (after key release,
        // click release and button release), only the stick axis may be non-zero.
        assert!(held.keys().count() == 0 && held.mouse_buttons().count() == 0, "{held:?}");
    }
}
