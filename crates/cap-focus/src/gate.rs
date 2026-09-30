//! Focus gating of the input stream (pure logic, no OS calls).
//!
//! [`FocusGate`] takes the `FocusRecord` stream from a [`crate::FocusTracker`]
//! and the `InputEvent` stream from `cap-input`, and returns only the events
//! that happened while the target game was focused, with three consistency
//! guarantees for the dataset:
//!
//! 1. **No stuck inputs.** When focus is lost at time `T`, every key, mouse
//!    button and gamepad button that was still held (as seen by the gate) gets
//!    a synthesised release at `T` (`key_up`, `mouse_button` 0, `button` 0),
//!    and every non-zero gamepad axis gets a synthesised `axis` 0. So a
//!    `held-key bitmap` built from the output never extends past a focus loss.
//! 2. **No orphan releases.** A release whose press was not passed through
//!    (the key went down before focus was gained, or while unfocused) is
//!    dropped, as are duplicate presses. Consequence: a key held *across* a
//!    focus gain is invisible until it is pressed again.
//! 3. **Privacy default.** Before the first focus record, nothing is focused.
//!
//! Timing: a focus change at `t` applies to events with `t_ns >= t`. Events
//! are processed in `t_ns` order; call [`FocusGate::process`] with an
//! `until_ns` (e.g. the segment end) so focus losses after the last event of a
//! batch still produce their releases in that batch. Held state carries across
//! calls, so a recorder can call it once per segment.
//!
//! [`HeldInputs`] is the underlying tracker and can be used on its own (e.g.
//! for a chat-pause hotkey: `release_all(t)` when pausing).

use cap_types::{Device, EventKind, FocusRecord, InputEvent, Nanos};
use std::collections::{BTreeMap, BTreeSet};

/// Which keys / buttons / axes are currently non-zero, as seen by the gate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeldInputs {
    keys: BTreeSet<u32>,
    mouse_buttons: BTreeSet<u32>,
    pad_buttons: BTreeMap<u32, f32>,
    pad_axes: BTreeMap<u32, f32>,
}

impl HeldInputs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update state with `ev`. Returns false if the event is inconsistent with
    /// the tracked state (duplicate press, release of something not held) and
    /// should be dropped.
    pub fn observe(&mut self, ev: &InputEvent) -> bool {
        match ev.kind {
            EventKind::KeyDown => self.keys.insert(ev.code),
            EventKind::KeyUp => self.keys.remove(&ev.code),
            EventKind::MouseButton => {
                if ev.value != 0.0 {
                    self.mouse_buttons.insert(ev.code)
                } else {
                    self.mouse_buttons.remove(&ev.code)
                }
            }
            EventKind::Button => {
                if ev.value != 0.0 {
                    self.pad_buttons.insert(ev.code, ev.value);
                    true
                } else {
                    self.pad_buttons.remove(&ev.code).is_some()
                }
            }
            EventKind::Axis => {
                if ev.value != 0.0 {
                    self.pad_axes.insert(ev.code, ev.value);
                } else {
                    self.pad_axes.remove(&ev.code);
                }
                true
            }
            EventKind::MouseMove | EventKind::Wheel => true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
            && self.mouse_buttons.is_empty()
            && self.pad_buttons.is_empty()
            && self.pad_axes.is_empty()
    }

    pub fn keys(&self) -> impl Iterator<Item = u32> + '_ {
        self.keys.iter().copied()
    }

    pub fn mouse_buttons(&self) -> impl Iterator<Item = u32> + '_ {
        self.mouse_buttons.iter().copied()
    }

    pub fn is_key_held(&self, code: u32) -> bool {
        self.keys.contains(&code)
    }

    /// Release events for everything held, all stamped `t_ns`, in a
    /// deterministic order (keys, mouse buttons, pad buttons, pad axes; each
    /// by code). Clears the state.
    pub fn release_all(&mut self, t_ns: Nanos) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for code in std::mem::take(&mut self.keys) {
            out.push(InputEvent {
                t_ns,
                device: Device::Keyboard,
                kind: EventKind::KeyUp,
                code,
                value: 0.0,
            });
        }
        for code in std::mem::take(&mut self.mouse_buttons) {
            out.push(InputEvent {
                t_ns,
                device: Device::Mouse,
                kind: EventKind::MouseButton,
                code,
                value: 0.0,
            });
        }
        for code in std::mem::take(&mut self.pad_buttons).into_keys() {
            out.push(InputEvent {
                t_ns,
                device: Device::Gamepad,
                kind: EventKind::Button,
                code,
                value: 0.0,
            });
        }
        for code in std::mem::take(&mut self.pad_axes).into_keys() {
            out.push(InputEvent {
                t_ns,
                device: Device::Gamepad,
                kind: EventKind::Axis,
                code,
                value: 0.0,
            });
        }
        out
    }
}

/// Filters input events by focus; see the module docs.
#[derive(Debug, Clone, Default)]
pub struct FocusGate {
    /// Focus state changes, sorted by time.
    transitions: Vec<(Nanos, bool)>,
    /// Transitions before this index have been applied by `process`.
    cursor: usize,
    focused: bool,
    processed_ns: Nanos,
    held: HeldInputs,
}

impl FocusGate {
    pub fn new() -> Self {
        Self {
            processed_ns: Nanos::MIN,
            ..Default::default()
        }
    }

    /// Add a focus record. Records that don't change the focused state are
    /// ignored (game_id changes while unfocused don't matter to the gate). A
    /// record older than what was already processed is applied as of the
    /// processed time (it cannot retroactively un-emit events).
    pub fn push_focus(&mut self, rec: &FocusRecord) {
        let t = rec.t_ns.max(self.processed_ns);
        let pos = self.transitions.partition_point(|&(tt, _)| tt <= t);
        let prev = pos > 0 && self.transitions[pos - 1].1;
        if prev == rec.focused {
            return; // no change at that time
        }
        // Applied transitions all have tt <= processed_ns <= t, so pos >= cursor.
        self.transitions.insert(pos, (t, rec.focused));
    }

    /// Was the target focused at `t_ns`, according to the records pushed so
    /// far? (false before the first record).
    pub fn is_focused(&self, t_ns: Nanos) -> bool {
        let pos = self.transitions.partition_point(|&(tt, _)| tt <= t_ns);
        pos > 0 && self.transitions[pos - 1].1
    }

    /// Current (processed) focus state.
    pub fn focused_now(&self) -> bool {
        self.focused
    }

    /// What the gate currently considers held.
    pub fn held(&self) -> &HeldInputs {
        &self.held
    }

    fn apply(&mut self, focused: bool, t: Nanos, out: &mut Vec<InputEvent>) {
        if self.focused && !focused {
            out.extend(self.held.release_all(t));
        }
        self.focused = focused;
    }

    fn apply_upto(&mut self, t: Nanos, out: &mut Vec<InputEvent>) {
        while self.cursor < self.transitions.len() && self.transitions[self.cursor].0 <= t {
            let (tt, f) = self.transitions[self.cursor];
            self.cursor += 1;
            self.apply(f, tt, out);
        }
    }

    /// Gate a batch of events. Events are sorted by `t_ns` (stable) and every
    /// focus transition up to `max(until_ns, last event)` is applied, emitting
    /// synthesised releases at focus-loss times. Returns events in time order.
    pub fn process(&mut self, mut events: Vec<InputEvent>, until_ns: Nanos) -> Vec<InputEvent> {
        events.sort_by_key(|e| e.t_ns);
        let mut out = Vec::with_capacity(events.len());
        for ev in events {
            self.apply_upto(ev.t_ns, &mut out);
            if self.focused && self.held.observe(&ev) {
                out.push(ev);
            }
            self.processed_ns = self.processed_ns.max(ev.t_ns);
        }
        self.apply_upto(until_ns, &mut out);
        self.processed_ns = self.processed_ns.max(until_ns);
        out
    }

    /// `process` up to the last event's time.
    pub fn filter(&mut self, events: Vec<InputEvent>) -> Vec<InputEvent> {
        let until = events
            .iter()
            .map(|e| e.t_ns)
            .max()
            .unwrap_or(self.processed_ns);
        self.process(events, until)
    }

    /// Synthesise releases for everything held at `t_ns` (e.g. chat pause,
    /// session end) without changing the focus state.
    pub fn release_all(&mut self, t_ns: Nanos) -> Vec<InputEvent> {
        self.held.release_all(t_ns)
    }

    /// Drop applied transitions (keeps the latest), bounding memory for long
    /// sessions. `is_focused` for times before the kept transition then
    /// reports that transition's predecessor state as unknown (false).
    pub fn prune(&mut self) {
        if self.cursor > 1 {
            self.transitions.drain(..self.cursor - 1);
            self.cursor = 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(t: Nanos, focused: bool) -> FocusRecord {
        FocusRecord {
            t_ns: t,
            focused,
            game_id: if focused {
                "game.exe".into()
            } else {
                "other.exe".into()
            },
        }
    }
    fn key(t: Nanos, down: bool, code: u32) -> InputEvent {
        InputEvent {
            t_ns: t,
            device: Device::Keyboard,
            kind: if down {
                EventKind::KeyDown
            } else {
                EventKind::KeyUp
            },
            code,
            value: if down { 1.0 } else { 0.0 },
        }
    }
    fn mv(t: Nanos, v: f32) -> InputEvent {
        InputEvent {
            t_ns: t,
            device: Device::Mouse,
            kind: EventKind::MouseMove,
            code: 0,
            value: v,
        }
    }
    fn btn(t: Nanos, code: u32, v: f32) -> InputEvent {
        InputEvent {
            t_ns: t,
            device: Device::Mouse,
            kind: EventKind::MouseButton,
            code,
            value: v,
        }
    }
    fn pad(t: Nanos, kind: EventKind, code: u32, v: f32) -> InputEvent {
        InputEvent {
            t_ns: t,
            device: Device::Gamepad,
            kind,
            code,
            value: v,
        }
    }
    fn summary(v: &[InputEvent]) -> Vec<(Nanos, EventKind, u32, f32)> {
        v.iter()
            .map(|e| (e.t_ns, e.kind, e.code, e.value))
            .collect()
    }

    #[test]
    fn unfocused_before_first_record() {
        let mut g = FocusGate::new();
        assert!(!g.is_focused(0));
        assert!(g.filter(vec![key(1, true, 0x11), mv(2, 3.0)]).is_empty());
    }

    #[test]
    fn is_focused_lookup() {
        let mut g = FocusGate::new();
        g.push_focus(&rec(10, true));
        g.push_focus(&rec(20, false));
        g.push_focus(&rec(20, false)); // duplicate state ignored
        g.push_focus(&rec(30, true));
        assert!(!g.is_focused(9));
        assert!(g.is_focused(10));
        assert!(g.is_focused(19));
        assert!(!g.is_focused(20));
        assert!(!g.is_focused(29));
        assert!(g.is_focused(1_000));
        assert_eq!(g.transitions.len(), 3);
    }

    #[test]
    fn focus_loss_synthesises_releases() {
        let mut g = FocusGate::new();
        g.push_focus(&rec(0, true));
        g.push_focus(&rec(100, false));
        let evs = vec![
            key(10, true, 0x11), // W down
            btn(20, 0, 1.0),     // LMB down
            key(30, true, 0x1E), // A down
            key(40, false, 0x1E),
            pad(50, EventKind::Axis, 0, 0.7),
            pad(55, EventKind::Button, 7, 0.4), // analog trigger
            mv(60, 5.0),
            mv(150, 9.0),          // unfocused: dropped
            key(160, false, 0x11), // W up while unfocused: dropped
        ];
        let out = g.process(evs, 200);
        assert_eq!(
            summary(&out),
            vec![
                (10, EventKind::KeyDown, 0x11, 1.0),
                (20, EventKind::MouseButton, 0, 1.0),
                (30, EventKind::KeyDown, 0x1E, 1.0),
                (40, EventKind::KeyUp, 0x1E, 0.0),
                (50, EventKind::Axis, 0, 0.7),
                (55, EventKind::Button, 7, 0.4),
                (60, EventKind::MouseMove, 0, 5.0),
                (100, EventKind::KeyUp, 0x11, 0.0),
                (100, EventKind::MouseButton, 0, 0.0),
                (100, EventKind::Button, 7, 0.0),
                (100, EventKind::Axis, 0, 0.0),
            ]
        );
        assert!(g.held().is_empty());
    }

    #[test]
    fn orphan_release_after_regain_is_dropped() {
        let mut g = FocusGate::new();
        g.push_focus(&rec(0, false));
        g.push_focus(&rec(50, true));
        // Space pressed while unfocused, released after focus gained.
        let out = g.filter(vec![
            key(10, true, 0x39),
            key(60, false, 0x39),
            key(70, true, 0x11),
            key(70, true, 0x11),
        ]);
        assert_eq!(summary(&out), vec![(70, EventKind::KeyDown, 0x11, 1.0)]);
        assert!(g.held().is_key_held(0x11));
    }

    #[test]
    fn transition_after_last_event_needs_until() {
        let mut g = FocusGate::new();
        g.push_focus(&rec(0, true));
        let out = g.process(vec![key(10, true, 0x11)], 50);
        assert_eq!(out.len(), 1);
        // focus lost at 80, reported late; next batch (segment) gets the release
        g.push_focus(&rec(80, false));
        let out = g.process(vec![], 100);
        assert_eq!(summary(&out), vec![(80, EventKind::KeyUp, 0x11, 0.0)]);
        // a stale focus record older than processed time is applied "now"
        g.push_focus(&rec(90, true));
        assert!(g.is_focused(100));
        assert!(!g.is_focused(95));
    }

    #[test]
    fn state_carries_across_batches_and_unsorted_input() {
        let mut g = FocusGate::new();
        g.push_focus(&rec(0, true));
        let out = g.process(vec![key(20, false, 0x11), key(10, true, 0x11)], 30);
        assert_eq!(out.len(), 2, "sorted before gating");
        let out = g.process(vec![key(40, true, 0x1E)], 60);
        assert_eq!(out.len(), 1);
        g.push_focus(&rec(70, false));
        g.push_focus(&rec(75, true));
        let out = g.process(vec![key(80, false, 0x1E)], 90);
        // released at 70 by the gate; the real release at 80 is an orphan
        assert_eq!(summary(&out), vec![(70, EventKind::KeyUp, 0x1E, 0.0)]);
        g.prune();
        assert_eq!(g.transitions.len(), 1);
        assert!(g.focused_now());
    }

    #[test]
    fn release_all_for_chat_pause() {
        let mut g = FocusGate::new();
        g.push_focus(&rec(0, true));
        g.filter(vec![key(1, true, 0x1D), key(2, true, 0x14)]);
        let rel = g.release_all(5);
        assert_eq!(
            summary(&rel),
            vec![
                (5, EventKind::KeyUp, 0x14, 0.0),
                (5, EventKind::KeyUp, 0x1D, 0.0)
            ]
        );
        assert!(g.focused_now());
    }
}
