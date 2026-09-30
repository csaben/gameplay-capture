//! Platform-independent part of the macOS IOHIDManager backend: HID
//! (usage page, usage, value) -> dataset events. Unit tested on every OS.
#![allow(dead_code)]

use crate::scancode;
use cap_types::{mouse_axis, mouse_button, wheel_axis, Device, EventKind, InputEvent};
use std::collections::HashSet;

const PAGE_GENERIC_DESKTOP: u32 = 0x01;
const PAGE_KEYBOARD: u32 = 0x07;
const PAGE_BUTTON: u32 = 0x09;
const PAGE_CONSUMER: u32 = 0x0C;
const USAGE_X: u32 = 0x30;
const USAGE_Y: u32 = 0x31;
const USAGE_WHEEL: u32 = 0x38;
const USAGE_AC_PAN: u32 = 0x0238;

/// Pure translation of one HID value (unit-tested logic shared with the callback).
#[derive(Default)]
pub(crate) struct HidTranslator {
    held: HashSet<u32>,
}

impl HidTranslator {
    pub(crate) fn translate(
        &mut self,
        t_ns: i64,
        page: u32,
        usage: u32,
        value: i64,
    ) -> Option<InputEvent> {
        let ev = |device, kind, code, value| {
            Some(InputEvent {
                t_ns,
                device,
                kind,
                code,
                value,
            })
        };
        match page {
            PAGE_KEYBOARD if (0x04..=0xE7).contains(&usage) => {
                let code = scancode::hid_to_code(usage);
                if value != 0 {
                    if self.held.insert(code) {
                        ev(Device::Keyboard, EventKind::KeyDown, code, 1.0)
                    } else {
                        None
                    }
                } else if self.held.remove(&code) {
                    ev(Device::Keyboard, EventKind::KeyUp, code, 0.0)
                } else {
                    None
                }
            }
            PAGE_GENERIC_DESKTOP => match usage {
                USAGE_X if value != 0 => ev(
                    Device::Mouse,
                    EventKind::MouseMove,
                    mouse_axis::X,
                    value as f32,
                ),
                USAGE_Y if value != 0 => ev(
                    Device::Mouse,
                    EventKind::MouseMove,
                    mouse_axis::Y,
                    value as f32,
                ),
                USAGE_WHEEL if value != 0 => ev(
                    Device::Mouse,
                    EventKind::Wheel,
                    wheel_axis::VERTICAL,
                    value as f32,
                ),
                _ => None,
            },
            PAGE_CONSUMER if usage == USAGE_AC_PAN && value != 0 => ev(
                Device::Mouse,
                EventKind::Wheel,
                wheel_axis::HORIZONTAL,
                value as f32,
            ),
            PAGE_BUTTON => {
                let b = match usage {
                    1 => mouse_button::LEFT,
                    2 => mouse_button::RIGHT,
                    3 => mouse_button::MIDDLE,
                    4 => mouse_button::BACK,
                    5 => mouse_button::FORWARD,
                    _ => return None,
                };
                ev(
                    Device::Mouse,
                    EventKind::MouseButton,
                    b,
                    if value != 0 { 1.0 } else { 0.0 },
                )
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scancode::key;

    #[test]
    fn keyboard_and_mouse() {
        let mut t = HidTranslator::default();
        let d = t.translate(1, PAGE_KEYBOARD, 0x1A, 1).unwrap(); // W
        assert_eq!((d.kind, d.code), (EventKind::KeyDown, key::W));
        assert!(
            t.translate(2, PAGE_KEYBOARD, 0x1A, 1).is_none(),
            "duplicate down"
        );
        assert_eq!(
            t.translate(3, PAGE_KEYBOARD, 0x1A, 0).unwrap().kind,
            EventKind::KeyUp
        );
        assert!(
            t.translate(3, PAGE_KEYBOARD, 0x1A, 0).is_none(),
            "orphan up"
        );
        assert!(
            t.translate(3, PAGE_KEYBOARD, 0x01, 1).is_none(),
            "ErrorRollOver ignored"
        );
        assert_eq!(
            t.translate(4, PAGE_KEYBOARD, 0xE4, 1).unwrap().code,
            key::RIGHT_CTRL
        );
        let m = t.translate(5, PAGE_GENERIC_DESKTOP, USAGE_Y, -4).unwrap();
        assert_eq!(
            (m.kind, m.code, m.value),
            (EventKind::MouseMove, mouse_axis::Y, -4.0)
        );
        assert!(t.translate(5, PAGE_GENERIC_DESKTOP, USAGE_X, 0).is_none());
        let w = t.translate(6, PAGE_CONSUMER, USAGE_AC_PAN, 1).unwrap();
        assert_eq!((w.kind, w.code), (EventKind::Wheel, wheel_axis::HORIZONTAL));
        let b = t.translate(7, PAGE_BUTTON, 2, 1).unwrap();
        assert_eq!(
            (b.kind, b.code, b.value),
            (EventKind::MouseButton, mouse_button::RIGHT, 1.0)
        );
        assert!(t.translate(7, PAGE_BUTTON, 9, 1).is_none());
    }
}
