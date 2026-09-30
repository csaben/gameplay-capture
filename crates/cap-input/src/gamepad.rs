//! Gamepads on every OS via `gilrs` (XInput / Windows.Gaming.Input, evdev,
//! IOKit), polled at 250 Hz on a dedicated thread.
//!
//! Each poll drains gilrs' event queue (which updates its cached state, after
//! gilrs' default deadzone/jitter filters) and then compares every axis and
//! button against the last *logged* value with [`AxisFilter`]:
//! axes and analog triggers are logged when they move by at least
//! [`AXIS_CHANGE_THRESHOLD`](crate::AXIS_CHANGE_THRESHOLD), digital buttons on
//! every change. `t_ns` is `cap_clock::now_ns()` at the poll, so gamepad
//! timestamps have up to 4 ms of quantisation.
//!
//! Codes are [`crate::gamepad_code`]. With several pads connected, the first
//! pad (slot 0) uses the plain codes and pad slot `n > 0` uses
//! `(n << 16) | code`, so a single-pad dataset is never affected. On
//! disconnect, pressed buttons are logged as released and non-zero axes as 0.

use crate::gamepad_code::{axis as ax, button as bt};
use crate::gamepad_filter::AxisFilter;
use crate::{EventSink, InputError, InputSource, Result};
use cap_types::{Device, EventKind, InputEvent};
use crossbeam_channel::Sender;
use gilrs::{Axis, Button, GamepadId, Gilrs};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// Poll period (250 Hz).
pub const POLL_NS: i64 = 4_000_000;

const AXES: [(Axis, u32); 8] = [
    (Axis::LeftStickX, ax::LEFT_STICK_X),
    (Axis::LeftStickY, ax::LEFT_STICK_Y),
    (Axis::LeftZ, ax::LEFT_Z),
    (Axis::RightStickX, ax::RIGHT_STICK_X),
    (Axis::RightStickY, ax::RIGHT_STICK_Y),
    (Axis::RightZ, ax::RIGHT_Z),
    (Axis::DPadX, ax::DPAD_X),
    (Axis::DPadY, ax::DPAD_Y),
];

const BUTTONS: [(Button, u32); 19] = [
    (Button::South, bt::SOUTH),
    (Button::East, bt::EAST),
    (Button::North, bt::NORTH),
    (Button::West, bt::WEST),
    (Button::C, bt::C),
    (Button::Z, bt::Z),
    (Button::LeftTrigger, bt::LEFT_TRIGGER),
    (Button::LeftTrigger2, bt::LEFT_TRIGGER2),
    (Button::RightTrigger, bt::RIGHT_TRIGGER),
    (Button::RightTrigger2, bt::RIGHT_TRIGGER2),
    (Button::Select, bt::SELECT),
    (Button::Start, bt::START),
    (Button::Mode, bt::MODE),
    (Button::LeftThumb, bt::LEFT_THUMB),
    (Button::RightThumb, bt::RIGHT_THUMB),
    (Button::DPadUp, bt::DPAD_UP),
    (Button::DPadDown, bt::DPAD_DOWN),
    (Button::DPadLeft, bt::DPAD_LEFT),
    (Button::DPadRight, bt::DPAD_RIGHT),
];

/// Per-pad change filters, keyed by (kind, code).
#[derive(Default)]
struct PadState {
    slot: u32,
    axes: HashMap<u32, AxisFilter>,
    buttons: HashMap<u32, AxisFilter>,
}

pub struct GamepadSource {
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl GamepadSource {
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            thread: None,
        }
    }
}

impl Default for GamepadSource {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSource for GamepadSource {
    fn start(&mut self, tx: Sender<InputEvent>) -> Result<()> {
        if self.thread.is_some() {
            return Ok(());
        }
        self.stop.store(false, Ordering::SeqCst);
        let sink = EventSink::new(tx, self.dropped.clone());
        let stop = self.stop.clone();
        // Gilrs is !Send: build it on the polling thread and report the
        // outcome back.
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<()>>(1);
        let handle = std::thread::Builder::new()
            .name("cap-input-gamepad".into())
            .spawn(move || {
                let gilrs = match Gilrs::new() {
                    Ok(g) => g,
                    Err(gilrs::Error::NotImplemented(g)) => {
                        tracing::warn!("gilrs: gamepads not supported on this platform; gamepad logging disabled");
                        g
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(InputError::Backend(format!("gilrs init: {e}"))));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                poll_loop(gilrs, sink, stop);
            })
            .map_err(|e| InputError::Backend(format!("spawn gamepad thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.thread = Some(handle);
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err(InputError::Backend(
                    "gamepad thread exited during init".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }

    fn name(&self) -> &'static str {
        "gilrs-gamepad"
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for GamepadSource {
    fn drop(&mut self) {
        self.stop();
    }
}

fn emit(sink: &EventSink, t_ns: i64, kind: EventKind, code: u32, value: f32) -> bool {
    sink.send(InputEvent {
        t_ns,
        device: Device::Gamepad,
        kind,
        code,
        value,
    })
}

fn poll_loop(mut gilrs: Gilrs, sink: EventSink, stop: Arc<AtomicBool>) {
    let mut pads: HashMap<GamepadId, PadState> = HashMap::new();
    let mut next = cap_clock::now_ns();
    while !stop.load(Ordering::Relaxed) {
        // Drain events so gilrs' cached state is current.
        while gilrs.next_event().is_some() {}
        let t = cap_clock::now_ns();

        // Disconnects: release everything that was non-zero.
        let connected: Vec<GamepadId> = gilrs.gamepads().map(|(id, _)| id).collect();
        let gone: Vec<GamepadId> = pads
            .keys()
            .copied()
            .filter(|id| !connected.contains(id))
            .collect();
        for id in gone {
            if let Some(mut st) = pads.remove(&id) {
                let hi = st.slot << 16;
                for (code, f) in st.buttons.iter_mut() {
                    if f.reset() && !emit(&sink, t, EventKind::Button, hi | code, 0.0) {
                        return;
                    }
                }
                for (code, f) in st.axes.iter_mut() {
                    if f.reset() && !emit(&sink, t, EventKind::Axis, hi | code, 0.0) {
                        return;
                    }
                }
            }
        }

        for (id, gp) in gilrs.gamepads() {
            if !pads.contains_key(&id) {
                let used: Vec<u32> = pads.values().map(|p| p.slot).collect();
                let slot = (0..).find(|s| !used.contains(s)).unwrap_or(0);
                tracing::info!(name = gp.name(), slot, "gamepad connected");
                pads.insert(
                    id,
                    PadState {
                        slot,
                        ..Default::default()
                    },
                );
            }
            let st = pads.get_mut(&id).expect("inserted above");
            let hi = st.slot << 16;
            for (axis, code) in AXES {
                if gp.axis_data(axis).is_none() {
                    continue;
                }
                let v = gp.value(axis).clamp(-1.0, 1.0);
                if let Some(v) = st.axes.entry(code).or_default().update(v) {
                    if !emit(&sink, t, EventKind::Axis, hi | code, v) {
                        return;
                    }
                }
            }
            for (button, code) in BUTTONS {
                let Some(data) = gp.button_data(button) else {
                    continue;
                };
                let v = if bt::is_analog(code) {
                    data.value().clamp(0.0, 1.0)
                } else if data.is_pressed() {
                    1.0
                } else {
                    0.0
                };
                if let Some(v) = st.buttons.entry(code).or_default().update(v) {
                    if !emit(&sink, t, EventKind::Button, hi | code, v) {
                        return;
                    }
                }
            }
        }

        next += POLL_NS;
        let now = cap_clock::now_ns();
        if next < now {
            next = now; // fell behind (suspend etc.): don't burst
        } else {
            std::thread::sleep(std::time::Duration::from_nanos((next - now) as u64));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_tables_are_unique() {
        let mut a: Vec<u32> = AXES.iter().map(|x| x.1).collect();
        let mut b: Vec<u32> = BUTTONS.iter().map(|x| x.1).collect();
        a.sort();
        a.dedup();
        b.sort();
        b.dedup();
        assert_eq!(a, (0..8).collect::<Vec<_>>());
        assert_eq!(b, (0..19).collect::<Vec<_>>());
    }

    /// Live: initialise gilrs, poll for 300 ms, stop. No pad expected.
    #[test]
    #[ignore]
    fn gamepad_live_start_stop() {
        let mut g = GamepadSource::new();
        let (tx, rx) = crossbeam_channel::bounded(1024);
        g.start(tx).expect("gilrs init");
        std::thread::sleep(std::time::Duration::from_millis(300));
        g.stop();
        println!(
            "gamepad events: {}, dropped {}",
            rx.try_iter().count(),
            g.dropped()
        );
    }
}
