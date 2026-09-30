//! Shared record types: timestamps, input/frame/focus rows and the segment manifest.
//!
//! Every timestamp in the system is nanoseconds on the platform monotonic clock
//! (see `cap-clock`), so frames and inputs can be aligned without translation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Bumped whenever the on-disk segment layout or manifest fields change.
pub const SCHEMA_VERSION: u32 = 1;

/// Nanoseconds on the platform monotonic clock.
pub type Nanos = i64;

pub const NANOS_PER_SEC: Nanos = 1_000_000_000;

/// Files inside a segment folder.
pub mod files {
    pub const VIDEO: &str = "video.mp4";
    pub const FRAMES: &str = "frames.parquet";
    pub const INPUTS: &str = "inputs.parquet";
    pub const FOCUS: &str = "focus.parquet";
    /// Written last; its presence marks the segment complete.
    pub const MANIFEST: &str = "manifest.json";
    /// Data files in upload order (manifest always goes last).
    pub const DATA: [&str; 4] = [VIDEO, FRAMES, INPUTS, FOCUS];
    /// Suffix of a segment folder that is still being written.
    pub const PARTIAL_SUFFIX: &str = ".partial";
}

/// `seg_000042`
pub fn segment_dir_name(segment_idx: u32) -> String {
    format!("seg_{segment_idx:06}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Device {
    Keyboard,
    Mouse,
    Gamepad,
}

impl Device {
    pub fn as_str(self) -> &'static str {
        match self {
            Device::Keyboard => "keyboard",
            Device::Mouse => "mouse",
            Device::Gamepad => "gamepad",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    KeyDown,
    KeyUp,
    /// Raw relative motion. `code` 0 = x axis, 1 = y axis; `value` is the delta.
    MouseMove,
    /// `value` 1 = pressed, 0 = released.
    MouseButton,
    /// `code` 0 = vertical, 1 = horizontal; `value` is detents (may be fractional).
    Wheel,
    /// Gamepad axis, `value` in -1..1.
    Axis,
    /// Gamepad button, `value` 1/0 (or analog 0..1 for triggers reported as buttons).
    Button,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::KeyDown => "key_down",
            EventKind::KeyUp => "key_up",
            EventKind::MouseMove => "mouse_move",
            EventKind::MouseButton => "mouse_button",
            EventKind::Wheel => "wheel",
            EventKind::Axis => "axis",
            EventKind::Button => "button",
        }
    }
}

/// Mouse axis codes for `EventKind::MouseMove`.
pub mod mouse_axis {
    pub const X: u32 = 0;
    pub const Y: u32 = 1;
}

/// Wheel codes for `EventKind::Wheel`.
pub mod wheel_axis {
    pub const VERTICAL: u32 = 0;
    pub const HORIZONTAL: u32 = 1;
}

/// Mouse button codes for `EventKind::MouseButton` (platform-neutral).
pub mod mouse_button {
    pub const LEFT: u32 = 0;
    pub const RIGHT: u32 = 1;
    pub const MIDDLE: u32 = 2;
    pub const BACK: u32 = 3;
    pub const FORWARD: u32 = 4;
}

/// One row of `inputs.parquet`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InputEvent {
    pub t_ns: Nanos,
    pub device: Device,
    pub kind: EventKind,
    /// Scan code (keyboard), button id, or axis id.
    pub code: u32,
    /// 1/0 for buttons, delta for mouse, -1..1 for axes.
    pub value: f32,
}

/// One row of `frames.parquet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRecord {
    pub frame_idx: u32,
    /// When the fixed-rate ticker fired.
    pub tick_ns: Nanos,
    /// Capture time of the source frame that was encoded for this tick.
    pub capture_ns: Nanos,
    /// True if the window had not produced a new frame since the previous tick.
    pub repeated: bool,
}

/// One row of `focus.parquet`: emitted whenever focus or game identity changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusRecord {
    pub t_ns: Nanos,
    pub focused: bool,
    pub game_id: String,
}

/// `manifest.json`, written last in every segment folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub session_id: String,
    pub segment_idx: u32,
    pub client_version: String,
    pub os: String,
    pub gpu: String,
    pub encoder: String,
    pub encoder_params: BTreeMap<String, String>,
    pub width: u32,
    pub height: u32,
    pub rate_hz: u32,
    /// Exe name (Windows/Linux) or bundle id (macOS).
    pub game_id: String,
    pub t_start_ns: Nanos,
    pub t_end_ns: Nanos,
    pub frame_count: u32,
    pub dropped_frames: u64,
    pub latency_offset_ns: Nanos,
    /// blake3 hex digest per data file name.
    pub blake3: BTreeMap<String, String>,
    /// Byte size per data file name.
    pub sizes: BTreeMap<String, u64>,
}

/// Identity of the window being recorded.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GameIdentity {
    /// Exe name (e.g. `eldenring.exe`) or bundle id (e.g. `com.foo.game`).
    pub game_id: String,
    /// Code-signing publisher if known.
    pub publisher: Option<String>,
}

/// Recorder settings that end up (partly) in the manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureSettings {
    pub width: u32,
    pub height: u32,
    pub rate_hz: u32,
    pub segment_secs: u32,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self { width: 640, height: 360, rate_hz: 20, segment_secs: 60 }
    }
}

impl CaptureSettings {
    pub fn tick_ns(&self) -> Nanos {
        NANOS_PER_SEC / self.rate_hz as Nanos
    }
    pub fn frames_per_segment(&self) -> u32 {
        self.rate_hz * self.segment_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_names_match_spec() {
        let e = InputEvent { t_ns: 1, device: Device::Mouse, kind: EventKind::MouseMove, code: 0, value: -3.0 };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"mouse\"") && s.contains("\"mouse_move\""), "{s}");
        assert_eq!(segment_dir_name(42), "seg_000042");
        assert_eq!(CaptureSettings::default().tick_ns(), 50_000_000);
    }
}
