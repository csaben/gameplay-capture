//! Platform-independent part of the Windows Raw Input backend: turns
//! `RAWKEYBOARD` / `RAWMOUSE` fields into dataset events. Kept separate from
//! the Win32 glue so it is unit tested on every OS.
#![allow(dead_code)]

use crate::scancode;
use cap_types::{mouse_axis, mouse_button, wheel_axis, Device, EventKind, InputEvent};
use std::collections::HashSet;

// Values from WinUser.h (identical to the `windows` crate constants).
pub(crate) const RI_KEY_BREAK: u32 = 0x01;
pub(crate) const RI_KEY_E0: u32 = 0x02;
pub(crate) const RI_KEY_E1: u32 = 0x04;
pub(crate) const MOUSE_MOVE_ABSOLUTE: u16 = 0x01;
pub(crate) const MOUSE_VIRTUAL_DESKTOP: u16 = 0x02;
pub(crate) const RI_MOUSE_LEFT_BUTTON_DOWN: u32 = 0x0001;
pub(crate) const RI_MOUSE_LEFT_BUTTON_UP: u32 = 0x0002;
pub(crate) const RI_MOUSE_RIGHT_BUTTON_DOWN: u32 = 0x0004;
pub(crate) const RI_MOUSE_RIGHT_BUTTON_UP: u32 = 0x0008;
pub(crate) const RI_MOUSE_MIDDLE_BUTTON_DOWN: u32 = 0x0010;
pub(crate) const RI_MOUSE_MIDDLE_BUTTON_UP: u32 = 0x0020;
pub(crate) const RI_MOUSE_BUTTON_4_DOWN: u32 = 0x0040;
pub(crate) const RI_MOUSE_BUTTON_4_UP: u32 = 0x0080;
pub(crate) const RI_MOUSE_BUTTON_5_DOWN: u32 = 0x0100;
pub(crate) const RI_MOUSE_BUTTON_5_UP: u32 = 0x0200;
pub(crate) const RI_MOUSE_WHEEL: u32 = 0x0400;
pub(crate) const RI_MOUSE_HWHEEL: u32 = 0x0800;
const WHEEL_DELTA: f32 = 120.0;

/// Pure Raw Input translation state (unit-testable apart from the Win32 glue).
#[derive(Default)]
pub(crate) struct RawState {
    held: HashSet<u32>,
    pause_tail: bool,
    abs_prev: Option<(f64, f64)>,
}

impl RawState {
    /// Keyboard record -> event (or None for repeats / fake keys).
    pub(crate) fn keyboard(
        &mut self,
        t_ns: i64,
        make: u16,
        flags: u16,
        vkey: u16,
    ) -> Option<InputEvent> {
        let flags = flags as u32;
        let e0 = flags & RI_KEY_E0 != 0;
        let e1 = flags & RI_KEY_E1 != 0;
        let up = flags & RI_KEY_BREAK != 0;
        if vkey == 0xFF || make == 0 || make == 0xFF {
            return None; // fake shift / overrun
        }
        if self.pause_tail && !e0 && !e1 && make == 0x45 {
            // second half of E1 1D 45 (Pause)
            self.pause_tail = false;
            return None;
        }
        if e1 {
            self.pause_tail = true;
        }
        let code = scancode::from_raw_parts(make, e0, e1);
        if up {
            self.held.remove(&code);
            Some(InputEvent {
                t_ns,
                device: Device::Keyboard,
                kind: EventKind::KeyUp,
                code,
                value: 0.0,
            })
        } else if self.held.insert(code) {
            Some(InputEvent {
                t_ns,
                device: Device::Keyboard,
                kind: EventKind::KeyDown,
                code,
                value: 1.0,
            })
        } else {
            None // autorepeat
        }
    }

    /// Mouse record -> events. `screen` = (width, height) in pixels of the
    /// coordinate space for absolute devices.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn mouse(
        &mut self,
        t_ns: i64,
        us_flags: u16,
        button_flags: u16,
        button_data: u16,
        last_x: i32,
        last_y: i32,
        screen: (f64, f64),
        out: &mut Vec<InputEvent>,
    ) {
        let mv = |code, v: f32| InputEvent {
            t_ns,
            device: Device::Mouse,
            kind: EventKind::MouseMove,
            code,
            value: v,
        };
        let bf = button_flags as u32;
        if us_flags & MOUSE_MOVE_ABSOLUTE != 0 {
            // Absolute packets that only carry buttons often report 0,0.
            if !(last_x == 0 && last_y == 0 && bf != 0) {
                let x = last_x as f64 / 65535.0 * screen.0;
                let y = last_y as f64 / 65535.0 * screen.1;
                if let Some((px, py)) = self.abs_prev {
                    let (dx, dy) = ((x - px).round(), (y - py).round());
                    if dx != 0.0 {
                        out.push(mv(mouse_axis::X, dx as f32));
                    }
                    if dy != 0.0 {
                        out.push(mv(mouse_axis::Y, dy as f32));
                    }
                    // keep the sub-pixel remainder
                    self.abs_prev = Some((px + dx, py + dy));
                } else {
                    self.abs_prev = Some((x, y));
                }
            }
        } else {
            if last_x != 0 {
                out.push(mv(mouse_axis::X, last_x as f32));
            }
            if last_y != 0 {
                out.push(mv(mouse_axis::Y, last_y as f32));
            }
        }
        const BUTTONS: [(u32, u32, u32); 5] = [
            (
                RI_MOUSE_LEFT_BUTTON_DOWN,
                RI_MOUSE_LEFT_BUTTON_UP,
                mouse_button::LEFT,
            ),
            (
                RI_MOUSE_RIGHT_BUTTON_DOWN,
                RI_MOUSE_RIGHT_BUTTON_UP,
                mouse_button::RIGHT,
            ),
            (
                RI_MOUSE_MIDDLE_BUTTON_DOWN,
                RI_MOUSE_MIDDLE_BUTTON_UP,
                mouse_button::MIDDLE,
            ),
            (
                RI_MOUSE_BUTTON_4_DOWN,
                RI_MOUSE_BUTTON_4_UP,
                mouse_button::BACK,
            ),
            (
                RI_MOUSE_BUTTON_5_DOWN,
                RI_MOUSE_BUTTON_5_UP,
                mouse_button::FORWARD,
            ),
        ];
        for (down, up, code) in BUTTONS {
            let b = |v| InputEvent {
                t_ns,
                device: Device::Mouse,
                kind: EventKind::MouseButton,
                code,
                value: v,
            };
            if bf & down != 0 {
                out.push(b(1.0));
            }
            if bf & up != 0 {
                out.push(b(0.0));
            }
        }
        let detents = button_data as i16 as f32 / WHEEL_DELTA;
        if bf & RI_MOUSE_WHEEL != 0 {
            out.push(InputEvent {
                t_ns,
                device: Device::Mouse,
                kind: EventKind::Wheel,
                code: wheel_axis::VERTICAL,
                value: detents,
            });
        }
        if bf & RI_MOUSE_HWHEEL != 0 {
            out.push(InputEvent {
                t_ns,
                device: Device::Mouse,
                kind: EventKind::Wheel,
                code: wheel_axis::HORIZONTAL,
                value: detents,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scancode::key;

    #[test]
    fn keyboard_extended_repeat_and_fake_keys() {
        let mut s = RawState::default();
        let d = s.keyboard(1, 0x1E, 0, 0x41).unwrap();
        assert_eq!((d.kind, d.code), (EventKind::KeyDown, key::A));
        assert!(
            s.keyboard(2, 0x1E, 0, 0x41).is_none(),
            "autorepeat suppressed"
        );
        let u = s.keyboard(3, 0x1E, RI_KEY_BREAK as u16, 0x41).unwrap();
        assert_eq!((u.kind, u.code, u.value), (EventKind::KeyUp, key::A, 0.0));
        assert!(
            s.keyboard(4, 0x1E, 0, 0x41).is_some(),
            "pressed again after release"
        );
        let rc = s.keyboard(5, 0x1D, RI_KEY_E0 as u16, 0xA3).unwrap();
        assert_eq!(rc.code, key::RIGHT_CTRL);
        // fake shift before Print Screen (VKey 0xFF) is dropped
        assert!(s.keyboard(6, 0x2A, RI_KEY_E0 as u16, 0xFF).is_none());
        assert_eq!(
            s.keyboard(6, 0x37, RI_KEY_E0 as u16, 0x2C).unwrap().code,
            key::PRINT_SCREEN
        );
    }

    #[test]
    fn pause_sequence_collapses() {
        let mut s = RawState::default();
        let p = s.keyboard(1, 0x1D, RI_KEY_E1 as u16, 0x13).unwrap();
        assert_eq!((p.kind, p.code), (EventKind::KeyDown, key::PAUSE));
        assert!(
            s.keyboard(1, 0x45, 0, 0x13).is_none(),
            "trailing 45 of E1 1D 45 dropped"
        );
        // a real Num Lock later still works
        assert_eq!(s.keyboard(2, 0x45, 0, 0x90).unwrap().code, 0x45);
    }

    #[test]
    fn mouse_relative_buttons_wheel() {
        let mut s = RawState::default();
        let mut out = Vec::new();
        s.mouse(
            1,
            0,
            (RI_MOUSE_LEFT_BUTTON_DOWN | RI_MOUSE_WHEEL) as u16,
            (-240i16) as u16,
            5,
            -3,
            (1920.0, 1080.0),
            &mut out,
        );
        let got: Vec<_> = out.iter().map(|e| (e.kind, e.code, e.value)).collect();
        assert_eq!(
            got,
            vec![
                (EventKind::MouseMove, mouse_axis::X, 5.0),
                (EventKind::MouseMove, mouse_axis::Y, -3.0),
                (EventKind::MouseButton, mouse_button::LEFT, 1.0),
                (EventKind::Wheel, wheel_axis::VERTICAL, -2.0),
            ]
        );
        out.clear();
        s.mouse(
            2,
            0,
            (RI_MOUSE_BUTTON_4_UP | RI_MOUSE_HWHEEL) as u16,
            60,
            0,
            0,
            (1920.0, 1080.0),
            &mut out,
        );
        let got: Vec<_> = out.iter().map(|e| (e.kind, e.code, e.value)).collect();
        assert_eq!(
            got,
            vec![
                (EventKind::MouseButton, mouse_button::BACK, 0.0),
                (EventKind::Wheel, wheel_axis::HORIZONTAL, 0.5)
            ]
        );
    }

    #[test]
    fn mouse_absolute_is_differenced() {
        let mut s = RawState::default();
        let mut out = Vec::new();
        let scr = (65535.0, 65535.0); // 1 unit = 1 px for easy numbers
        s.mouse(1, MOUSE_MOVE_ABSOLUTE, 0, 0, 1000, 1000, scr, &mut out);
        assert!(out.is_empty(), "first absolute sample only sets the origin");
        s.mouse(2, MOUSE_MOVE_ABSOLUTE, 0, 0, 1010, 990, scr, &mut out);
        let got: Vec<_> = out.iter().map(|e| (e.code, e.value)).collect();
        assert_eq!(got, vec![(mouse_axis::X, 10.0), (mouse_axis::Y, -10.0)]);
        out.clear();
        // button-only absolute packet with 0,0 does not produce a huge jump
        s.mouse(
            3,
            MOUSE_MOVE_ABSOLUTE,
            RI_MOUSE_RIGHT_BUTTON_DOWN as u16,
            0,
            0,
            0,
            scr,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EventKind::MouseButton);
    }
}
