//! Passive input logging: `trait InputSource` plus per-platform backends.
//!
//! Backends emit every event at full rate with `cap_clock` timestamps. Focus
//! gating happens in the recorder, which drops events while the game is not
//! focused (see `cap_focus::FocusGate`). Never use global hooks
//! (`SetWindowsHookEx`, `rdev`), injection, or anything that opens the game
//! process.
//!
//! # Backends
//!
//! | Platform | Keyboard + mouse | Gamepad |
//! |----------|------------------|---------|
//! | Windows  | [`RawInputSource`]: Raw Input on a hidden message-only window, `RIDEV_INPUTSINK` | [`GamepadSource`] (gilrs) |
//! | Linux    | [`EvdevSource`]: `/dev/input/event*`, non-grabbing, `CLOCK_MONOTONIC` event timestamps | [`GamepadSource`] (gilrs) |
//! | macOS    | [`HidSource`]: IOHIDManager (Input Monitoring permission). **Untested.** | [`GamepadSource`] (gilrs) |
//!
//! # Code spaces (`InputEvent::code`)
//!
//! One code space for the whole dataset, whatever the OS:
//!
//! * Keyboard: PS/2 **scan code set 1** make codes; extended keys carry their
//!   prefix in the high byte (`0xE000 | make`, e.g. Right Ctrl = `0xE01D`, Up =
//!   `0xE048`; Pause = `0xE11D`). Windows reports these natively; Linux evdev
//!   keycodes and macOS HID usages are translated with [`scancode`]. Keys that
//!   have no set-1 equivalent are kept but tagged: `0x2_0000 | evdev_code`
//!   (Linux) or `0x3_0000 | hid_usage` (macOS). See [`scancode`].
//! * Mouse buttons: `cap_types::mouse_button`; mouse axes `cap_types::mouse_axis`
//!   (values are raw relative counts); wheel `cap_types::wheel_axis` (values in
//!   detents, 1.0 = one notch, fractional for hi-res wheels).
//! * Gamepad: [`gamepad_code`] (stable ids, independent of gilrs' enum order).
//!
//! # Timestamps
//!
//! All `t_ns` are `cap_clock` nanoseconds. Linux uses the kernel's per-event
//! timestamps (after `EVIOCSCLOCKID(CLOCK_MONOTONIC)`), macOS uses the HID value
//! timestamp (mach ticks), Windows and gamepads use `cap_clock::now_ns()` on
//! receipt (Windows `GetMessageTime` only has millisecond resolution and a
//! different base, so it is not used).

use cap_types::InputEvent;
use crossbeam_channel::{Sender, TrySendError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub mod gamepad_code;
pub mod scancode;

#[cfg(feature = "gamepad")]
mod gamepad;
#[cfg(feature = "gamepad")]
pub use gamepad::GamepadSource;
pub use gamepad_filter::{AxisFilter, AXIS_CHANGE_THRESHOLD};
mod gamepad_filter;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::EvdevSource;

mod hid_logic;
mod rawinput_logic;
#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::RawInputSource;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::HidSource;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, InputError>;

pub trait InputSource: Send {
    /// Start delivering events. Must never block on `tx`: use `try_send` and
    /// count drops.
    fn start(&mut self, tx: Sender<InputEvent>) -> Result<()>;
    fn stop(&mut self);
    fn name(&self) -> &'static str;
    /// Events dropped because the channel was full.
    fn dropped(&self) -> u64 {
        0
    }
}

/// Keyboard+mouse and gamepad sources for this OS.
///
/// Construction never touches devices; permission problems (Linux `input`
/// group, macOS Input Monitoring) surface from [`InputSource::start`] as
/// [`InputError::PermissionDenied`].
#[allow(clippy::vec_init_then_push)]
pub fn default_sources() -> Result<Vec<Box<dyn InputSource>>> {
    #[allow(unused_mut)]
    let mut v: Vec<Box<dyn InputSource>> = Vec::new();
    #[cfg(target_os = "linux")]
    v.push(Box::new(EvdevSource::new()));
    #[cfg(windows)]
    v.push(Box::new(RawInputSource::new()));
    #[cfg(target_os = "macos")]
    v.push(Box::new(HidSource::new()));
    #[cfg(feature = "gamepad")]
    v.push(Box::new(GamepadSource::new()));
    if v.is_empty() {
        return Err(InputError::Unsupported(
            "no input backend for this OS/feature set".into(),
        ));
    }
    Ok(v)
}

/// Non-blocking sender that counts events dropped on a full channel.
#[derive(Clone)]
pub(crate) struct EventSink {
    tx: Sender<InputEvent>,
    dropped: Arc<AtomicU64>,
}

#[allow(dead_code)]
impl EventSink {
    pub(crate) fn new(tx: Sender<InputEvent>, dropped: Arc<AtomicU64>) -> Self {
        Self { tx, dropped }
    }

    /// `try_send`; a full channel counts a drop. Returns false once the
    /// receiver is gone (the caller may shut down).
    pub(crate) fn send(&self, ev: InputEvent) -> bool {
        match self.tx.try_send(ev) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_types::{Device, EventKind};

    #[test]
    fn sink_counts_drops_and_never_blocks() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let sink = EventSink::new(tx, dropped.clone());
        let ev = InputEvent {
            t_ns: 1,
            device: Device::Keyboard,
            kind: EventKind::KeyDown,
            code: 0x1E,
            value: 1.0,
        };
        assert!(sink.send(ev));
        assert!(sink.send(ev));
        assert!(sink.send(ev));
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
        drop(rx);
        assert!(!sink.send(ev));
    }

    #[test]
    fn default_sources_nonempty() {
        let s = default_sources().unwrap();
        assert!(!s.is_empty());
    }
}
