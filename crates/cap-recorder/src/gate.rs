//! Streaming focus / pause gate for the input stream, with held-state tracking
//! (`cap_focus::HeldInputs`) so a gate closing never leaves a key, button or
//! stick "stuck" in the logged data. (`cap_focus::FocusGate` is the batch,
//! time-ordered equivalent; the recorder needs a streaming gate that also
//! handles pause.)

use cap_types::{InputEvent, Nanos};

/// Held-key tracker shared with `cap-focus` (drops duplicate presses and
/// releases of things never seen pressed; `release_all` synthesises releases).
pub use cap_focus::HeldInputs as HeldState;

/// Decides which input events are logged: only while the target game is focused
/// and recording is not paused. When the gate closes (focus lost or pause
/// begins) it emits release events for everything still held.
#[derive(Debug, Clone)]
pub struct InputGate {
    focused: bool,
    paused: bool,
    /// Events stamped before this time are dropped (gate opened at this time).
    open_since: Nanos,
    held: HeldState,
    pub dropped_gated: u64,
}

impl Default for InputGate {
    fn default() -> Self {
        Self::new()
    }
}

impl InputGate {
    /// Starts closed (unfocused, not paused) until the focus tracker reports.
    pub fn new() -> Self {
        Self { focused: false, paused: false, open_since: Nanos::MAX, held: HeldState::default(), dropped_gated: 0 }
    }

    pub fn is_open(&self) -> bool {
        self.focused && !self.paused
    }

    fn transition(&mut self, was_open: bool, t_ns: Nanos) -> Vec<InputEvent> {
        let open = self.is_open();
        if was_open && !open {
            self.open_since = Nanos::MAX;
            return self.held.release_all(t_ns);
        }
        if !was_open && open {
            self.open_since = t_ns;
        }
        Vec::new()
    }

    /// Focus changed at `t_ns`. Returns synthesised releases to log (if the gate closed).
    pub fn set_focused(&mut self, focused: bool, t_ns: Nanos) -> Vec<InputEvent> {
        let was = self.is_open();
        self.focused = focused;
        self.transition(was, t_ns)
    }

    /// Pause changed at `t_ns`. Returns synthesised releases to log (if the gate closed).
    pub fn set_paused(&mut self, paused: bool, t_ns: Nanos) -> Vec<InputEvent> {
        let was = self.is_open();
        self.paused = paused;
        self.transition(was, t_ns)
    }

    /// True if `ev` should be logged. Updates held state for logged events.
    pub fn admit(&mut self, ev: &InputEvent) -> bool {
        // Releases of things pressed before the gate opened and duplicate
        // presses (key auto-repeat) are dropped by `HeldInputs::observe`.
        if self.is_open() && ev.t_ns >= self.open_since && self.held.observe(ev) {
            true
        } else {
            self.dropped_gated += 1;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_types::{Device, EventKind};

    fn ev(t: Nanos, device: Device, kind: EventKind, code: u32, value: f32) -> InputEvent {
        InputEvent { t_ns: t, device, kind, code, value }
    }

    #[test]
    fn releases_on_focus_loss_and_pause() {
        let mut g = InputGate::new();
        assert!(!g.admit(&ev(1, Device::Keyboard, EventKind::KeyDown, 30, 1.0)));
        assert!(g.set_focused(true, 10).is_empty());
        assert!(!g.admit(&ev(9, Device::Keyboard, EventKind::KeyDown, 30, 1.0)), "stamped before focus");
        assert!(g.admit(&ev(11, Device::Keyboard, EventKind::KeyDown, 30, 1.0)));
        assert!(g.admit(&ev(12, Device::Mouse, EventKind::MouseButton, 0, 1.0)));
        assert!(g.admit(&ev(13, Device::Gamepad, EventKind::Axis, 1, 0.5)));
        assert!(g.admit(&ev(14, Device::Gamepad, EventKind::Button, 3, 1.0)));
        assert!(g.admit(&ev(15, Device::Keyboard, EventKind::KeyDown, 31, 1.0)));
        assert!(g.admit(&ev(16, Device::Keyboard, EventKind::KeyUp, 31, 0.0)));
        assert!(!g.admit(&ev(17, Device::Keyboard, EventKind::KeyUp, 99, 0.0)), "orphan release");
        assert!(!g.admit(&ev(18, Device::Keyboard, EventKind::KeyDown, 30, 1.0)), "auto-repeat");
        let rel = g.set_focused(false, 20);
        assert_eq!(rel.len(), 4);
        assert!(rel.iter().all(|e| e.t_ns == 20 && e.value == 0.0));
        assert!(rel.iter().any(|e| e.kind == EventKind::KeyUp && e.code == 30));
        assert!(rel.iter().any(|e| e.kind == EventKind::MouseButton && e.code == 0));
        assert!(rel.iter().any(|e| e.kind == EventKind::Axis && e.code == 1));
        assert!(rel.iter().any(|e| e.kind == EventKind::Button && e.code == 3));
        assert!(!g.admit(&ev(21, Device::Keyboard, EventKind::KeyDown, 30, 1.0)));

        g.set_focused(true, 30);
        assert!(g.admit(&ev(31, Device::Keyboard, EventKind::KeyDown, 30, 1.0)));
        let rel = g.set_paused(true, 40);
        assert_eq!(rel.len(), 1);
        assert!(g.set_focused(false, 45).is_empty(), "already closed");
        assert!(!g.admit(&ev(46, Device::Keyboard, EventKind::KeyDown, 30, 1.0)));
    }
}
