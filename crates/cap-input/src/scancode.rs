//! One keyboard code space for the dataset: PS/2 scan code set 1.
//!
//! * Plain keys: the set-1 make code (`0x01`..`0x7F`), e.g. `A` = `0x1E`.
//! * `E0`-prefixed (extended) keys: `0xE000 | make`, e.g. Right Ctrl = `0xE01D`,
//!   Up = `0xE048`, keypad Enter = `0xE01C`, Print Screen = `0xE037`.
//! * Pause/Break (the only `E1` key): `0xE11D`.
//!
//! This is exactly what Windows Raw Input reports (`MakeCode` plus the
//! `RI_KEY_E0` / `RI_KEY_E1` flags), so Windows codes pass through unchanged.
//! Linux evdev keycodes ([`from_evdev`]) and macOS/USB HID keyboard usages
//! ([`from_hid_usage`], usage page 0x07) are translated to it.
//!
//! Keys without a set-1 equivalent are not dropped; they get a tagged code
//! outside the set-1 range so they stay distinguishable and reversible:
//! `LINUX_UNMAPPED | evdev_code` or `MAC_UNMAPPED | hid_usage`
//! (see [`evdev_to_code`] / [`hid_to_code`]).

/// Tag for evdev keycodes with no set-1 mapping: `0x2_0000 | evdev_code`.
pub const LINUX_UNMAPPED: u32 = 0x2_0000;
/// Tag for HID keyboard usages with no set-1 mapping: `0x3_0000 | usage`.
pub const MAC_UNMAPPED: u32 = 0x3_0000;

/// Prefix for `E0` extended keys.
pub const E0: u32 = 0xE000;
/// Pause/Break (sent as `E1 1D 45` by the keyboard).
pub const PAUSE: u32 = 0xE11D;

/// Handy named set-1 codes (used in tests and by callers that need e.g. a
/// chat-pause hotkey).
pub mod key {
    pub const ESC: u32 = 0x01;
    pub const W: u32 = 0x11;
    pub const A: u32 = 0x1E;
    pub const S: u32 = 0x1F;
    pub const D: u32 = 0x20;
    pub const ENTER: u32 = 0x1C;
    pub const LEFT_CTRL: u32 = 0x1D;
    pub const LEFT_SHIFT: u32 = 0x2A;
    pub const LEFT_ALT: u32 = 0x38;
    pub const SPACE: u32 = 0x39;
    pub const T: u32 = 0x14;
    pub const SLASH: u32 = 0x35;
    pub const RIGHT_CTRL: u32 = 0xE01D;
    pub const RIGHT_ALT: u32 = 0xE038;
    pub const UP: u32 = 0xE048;
    pub const DOWN: u32 = 0xE050;
    pub const LEFT: u32 = 0xE04B;
    pub const RIGHT: u32 = 0xE04D;
    pub const KP_ENTER: u32 = 0xE01C;
    pub const LEFT_META: u32 = 0xE05B;
    pub const PRINT_SCREEN: u32 = 0xE037;
    pub const PAUSE: u32 = super::PAUSE;
}

/// Combine a Windows Raw Input make code and its E0/E1 flags into a set-1 code.
pub fn from_raw_parts(make: u16, e0: bool, e1: bool) -> u32 {
    let make = (make & 0xFF) as u32;
    if e1 {
        0xE100 | make
    } else if e0 {
        E0 | make
    } else {
        make
    }
}

/// Linux evdev `KEY_*` code -> set-1 code, if the key exists on a PC keyboard.
pub fn from_evdev(code: u16) -> Option<u32> {
    // KEY_ESC (1) .. KEY_KPDOT (83) match set-1 make codes 1:1 (the Linux
    // keycode table was derived from the AT set-1 layout).
    if (1..=83).contains(&code) {
        return Some(code as u32);
    }
    let v = match code {
        86 => 0x56,                              // KEY_102ND (ISO extra key)
        87 => 0x57,                              // KEY_F11
        88 => 0x58,                              // KEY_F12
        89 => 0x73,                              // KEY_RO
        90 => 0x78,                              // KEY_KATAKANA
        91 => 0x77,                              // KEY_HIRAGANA
        92 => 0x79,                              // KEY_HENKAN
        93 => 0x70,                              // KEY_KATAKANAHIRAGANA
        94 => 0x7B,                              // KEY_MUHENKAN
        95 => 0x5C,                              // KEY_KPJPCOMMA
        96 => 0xE01C,                            // KEY_KPENTER
        97 => 0xE01D,                            // KEY_RIGHTCTRL
        98 => 0xE035,                            // KEY_KPSLASH
        99 => 0xE037,                            // KEY_SYSRQ (Print Screen)
        100 => 0xE038,                           // KEY_RIGHTALT
        102 => 0xE047,                           // KEY_HOME
        103 => 0xE048,                           // KEY_UP
        104 => 0xE049,                           // KEY_PAGEUP
        105 => 0xE04B,                           // KEY_LEFT
        106 => 0xE04D,                           // KEY_RIGHT
        107 => 0xE04F,                           // KEY_END
        108 => 0xE050,                           // KEY_DOWN
        109 => 0xE051,                           // KEY_PAGEDOWN
        110 => 0xE052,                           // KEY_INSERT
        111 => 0xE053,                           // KEY_DELETE
        113 => 0xE020,                           // KEY_MUTE
        114 => 0xE02E,                           // KEY_VOLUMEDOWN
        115 => 0xE030,                           // KEY_VOLUMEUP
        116 => 0xE05E,                           // KEY_POWER
        117 => 0x59,                             // KEY_KPEQUAL
        119 => PAUSE,                            // KEY_PAUSE
        121 => 0x7E,                             // KEY_KPCOMMA
        122 => 0x72,                             // KEY_HANGEUL
        123 => 0x71,                             // KEY_HANJA
        124 => 0x7D,                             // KEY_YEN
        125 => 0xE05B,                           // KEY_LEFTMETA
        126 => 0xE05C,                           // KEY_RIGHTMETA
        127 => 0xE05D,                           // KEY_COMPOSE (Menu / Application)
        140 => 0xE021,                           // KEY_CALC
        142 => 0xE05F,                           // KEY_SLEEP
        143 => 0xE063,                           // KEY_WAKEUP
        150 => 0xE032, // KEY_WWW (KEY_HOMEPAGE 172 is left unmapped: same set-1 code)
        155 => 0xE06C, // KEY_MAIL
        157 => 0xE06B, // KEY_COMPUTER
        158 => 0xE06A, // KEY_BACK
        159 => 0xE069, // KEY_FORWARD
        163 => 0xE019, // KEY_NEXTSONG
        164 => 0xE022, // KEY_PLAYPAUSE
        165 => 0xE010, // KEY_PREVIOUSSONG
        166 => 0xE024, // KEY_STOPCD
        183..=193 => 0x64 + (code as u32 - 183), // KEY_F13..KEY_F23 -> 0x64..0x6E
        194 => 0x76,   // KEY_F24
        _ => return None,
    };
    Some(v)
}

/// USB HID keyboard usage (page 0x07) -> set-1 code.
pub fn from_hid_usage(usage: u32) -> Option<u32> {
    // Letters a..z (0x04..0x1D) in alphabetical order.
    const LETTERS: [u32; 26] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, // a..m
        0x31, 0x18, 0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C, // n..z
    ];
    let v = match usage {
        0x04..=0x1D => LETTERS[(usage - 0x04) as usize],
        0x1E..=0x26 => 0x02 + (usage - 0x1E), // 1..9
        0x27 => 0x0B,                         // 0
        0x28 => 0x1C,                         // Enter
        0x29 => 0x01,                         // Escape
        0x2A => 0x0E,                         // Backspace
        0x2B => 0x0F,                         // Tab
        0x2C => 0x39,                         // Space
        0x2D => 0x0C,                         // - _
        0x2E => 0x0D,                         // = +
        0x2F => 0x1A,                         // [ {
        0x30 => 0x1B,                         // ] }
        0x31 => 0x2B,                         // \ |
        0x32 => 0x2B,                         // Non-US # ~ (same physical key position)
        0x33 => 0x27,                         // ; :
        0x34 => 0x28,                         // ' "
        0x35 => 0x29,                         // ` ~
        0x36 => 0x33,                         // , <
        0x37 => 0x34,                         // . >
        0x38 => 0x35,                         // / ?
        0x39 => 0x3A,                         // Caps Lock
        0x3A..=0x43 => 0x3B + (usage - 0x3A), // F1..F10
        0x44 => 0x57,                         // F11
        0x45 => 0x58,                         // F12
        0x46 => 0xE037,                       // Print Screen
        0x47 => 0x46,                         // Scroll Lock
        0x48 => PAUSE,                        // Pause
        0x49 => 0xE052,                       // Insert
        0x4A => 0xE047,                       // Home
        0x4B => 0xE049,                       // Page Up
        0x4C => 0xE053,                       // Delete
        0x4D => 0xE04F,                       // End
        0x4E => 0xE051,                       // Page Down
        0x4F => 0xE04D,                       // Right
        0x50 => 0xE04B,                       // Left
        0x51 => 0xE050,                       // Down
        0x52 => 0xE048,                       // Up
        0x53 => 0x45,                         // Num Lock
        0x54 => 0xE035,                       // KP /
        0x55 => 0x37,                         // KP *
        0x56 => 0x4A,                         // KP -
        0x57 => 0x4E,                         // KP +
        0x58 => 0xE01C,                       // KP Enter
        0x59 => 0x4F,                         // KP 1
        0x5A => 0x50,                         // KP 2
        0x5B => 0x51,                         // KP 3
        0x5C => 0x4B,                         // KP 4
        0x5D => 0x4C,                         // KP 5
        0x5E => 0x4D,                         // KP 6
        0x5F => 0x47,                         // KP 7
        0x60 => 0x48,                         // KP 8
        0x61 => 0x49,                         // KP 9
        0x62 => 0x52,                         // KP 0
        0x63 => 0x53,                         // KP .
        0x64 => 0x56,                         // Non-US \ | (ISO extra key)
        0x65 => 0xE05D,                       // Application (Menu)
        0x66 => 0xE05E,                       // Power
        0x67 => 0x59,                         // KP =
        0x68..=0x72 => 0x64 + (usage - 0x68), // F13..F23
        0x73 => 0x76,                         // F24
        0x7F => 0xE020,                       // Mute
        0x80 => 0xE030,                       // Volume Up
        0x81 => 0xE02E,                       // Volume Down
        0x85 => 0x7E,                         // KP , (Brazilian)
        0x87 => 0x73,                         // International1 (Ro)
        0x88 => 0x70,                         // International2 (Katakana/Hiragana)
        0x89 => 0x7D,                         // International3 (Yen)
        0x8A => 0x79,                         // International4 (Henkan)
        0x8B => 0x7B,                         // International5 (Muhenkan)
        0x8C => 0x5C,                         // International6 (KP JP comma)
        0x90 => 0x72,                         // LANG1 (Hangul)
        0x91 => 0x71,                         // LANG2 (Hanja)
        0xE0 => 0x1D,                         // Left Ctrl
        0xE1 => 0x2A,                         // Left Shift
        0xE2 => 0x38,                         // Left Alt
        0xE3 => 0xE05B,                       // Left GUI (Cmd / Win)
        0xE4 => 0xE01D,                       // Right Ctrl
        0xE5 => 0x36,                         // Right Shift
        0xE6 => 0xE038,                       // Right Alt
        0xE7 => 0xE05C,                       // Right GUI
        _ => return None,
    };
    Some(v)
}

/// evdev keycode -> dataset code (set-1, or tagged unmapped).
pub fn evdev_to_code(code: u16) -> u32 {
    from_evdev(code).unwrap_or(LINUX_UNMAPPED | code as u32)
}

/// HID keyboard usage -> dataset code (set-1, or tagged unmapped).
pub fn hid_to_code(usage: u32) -> u32 {
    from_hid_usage(usage).unwrap_or(MAC_UNMAPPED | (usage & 0xFFFF))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // (evdev keycode, HID usage, expected set-1) for keys present in both.
    const CROSS: &[(u16, u32, u32)] = &[
        (1, 0x29, key::ESC),
        (30, 0x04, key::A),
        (17, 0x1A, key::W),
        (31, 0x16, key::S),
        (32, 0x07, key::D),
        (20, 0x17, key::T),
        (44, 0x1D, 0x2C), // z
        (2, 0x1E, 0x02),  // 1
        (11, 0x27, 0x0B), // 0
        (28, 0x28, key::ENTER),
        (57, 0x2C, key::SPACE),
        (29, 0xE0, key::LEFT_CTRL),
        (42, 0xE1, key::LEFT_SHIFT),
        (54, 0xE5, 0x36),
        (56, 0xE2, key::LEFT_ALT),
        (97, 0xE4, key::RIGHT_CTRL),
        (100, 0xE6, key::RIGHT_ALT),
        (125, 0xE3, key::LEFT_META),
        (103, 0x52, key::UP),
        (108, 0x51, key::DOWN),
        (105, 0x50, key::LEFT),
        (106, 0x4F, key::RIGHT),
        (96, 0x58, key::KP_ENTER),
        (99, 0x46, key::PRINT_SCREEN),
        (119, 0x48, key::PAUSE),
        (59, 0x3A, 0x3B),  // F1
        (68, 0x43, 0x44),  // F10
        (87, 0x44, 0x57),  // F11
        (88, 0x45, 0x58),  // F12
        (183, 0x68, 0x64), // F13
        (194, 0x73, 0x76), // F24
        (53, 0x38, key::SLASH),
        (43, 0x31, 0x2B), // backslash
        (86, 0x64, 0x56), // ISO 102nd
        (69, 0x53, 0x45), // num lock
        (71, 0x5F, 0x47), // KP7
        (82, 0x62, 0x52), // KP0
        (83, 0x63, 0x53), // KP.
        (98, 0x54, 0xE035),
        (110, 0x49, 0xE052),
        (111, 0x4C, 0xE053),
        (102, 0x4A, 0xE047),
        (107, 0x4D, 0xE04F),
        (104, 0x4B, 0xE049),
        (109, 0x4E, 0xE051),
        (127, 0x65, 0xE05D),
        (113, 0x7F, 0xE020),
        (115, 0x80, 0xE030),
        (114, 0x81, 0xE02E),
    ];

    #[test]
    fn cross_table_agrees() {
        for &(ev, hid, want) in CROSS {
            assert_eq!(from_evdev(ev), Some(want), "evdev {ev}");
            assert_eq!(from_hid_usage(hid), Some(want), "hid {hid:#x}");
        }
    }

    #[test]
    fn evdev_mapping_is_injective() {
        let mut seen: HashMap<u32, u16> = HashMap::new();
        for c in 0..=0x2FF_u16 {
            if let Some(v) = from_evdev(c) {
                if let Some(prev) = seen.insert(v, c) {
                    panic!("evdev {prev} and {c} both map to {v:#x}");
                }
            }
        }
        assert!(seen.len() > 120, "{}", seen.len());
    }

    #[test]
    fn hid_mapping_is_injective_except_nonus_hash() {
        let mut seen: HashMap<u32, u32> = HashMap::new();
        for u in 0..=0xFF_u32 {
            if let Some(v) = from_hid_usage(u) {
                if let Some(prev) = seen.insert(v, u) {
                    // 0x31 (\|) and 0x32 (Non-US #~) are the same key position.
                    assert_eq!(
                        (prev, u),
                        (0x31, 0x32),
                        "hid {prev:#x} and {u:#x} both map to {v:#x}"
                    );
                }
            }
        }
        // Every letter/digit present.
        for u in 0x04..=0x27 {
            assert!(from_hid_usage(u).is_some());
        }
    }

    #[test]
    fn all_codes_are_valid_set1_shapes() {
        let ok = |v: u32| (1..=0x7F).contains(&v) || (0xE001..=0xE07F).contains(&v) || v == PAUSE;
        for c in 0..=0x2FF_u16 {
            if let Some(v) = from_evdev(c) {
                assert!(ok(v), "evdev {c} -> {v:#x}");
            }
        }
        for u in 0..=0xFF {
            if let Some(v) = from_hid_usage(u) {
                assert!(ok(v), "hid {u:#x} -> {v:#x}");
            }
        }
    }

    #[test]
    fn unmapped_are_tagged_not_dropped() {
        // KEY_HOMEPAGE (172), BTN range, unknown usages
        assert_eq!(evdev_to_code(172), LINUX_UNMAPPED | 172);
        assert_eq!(evdev_to_code(30), key::A);
        assert_eq!(hid_to_code(0x01), MAC_UNMAPPED | 0x01);
        assert_eq!(hid_to_code(0x04), key::A);
        const { assert!(LINUX_UNMAPPED > 0xFFFF && MAC_UNMAPPED > 0xFFFF) };
    }

    #[test]
    fn raw_parts() {
        assert_eq!(from_raw_parts(0x1E, false, false), key::A);
        assert_eq!(from_raw_parts(0x1D, true, false), key::RIGHT_CTRL);
        assert_eq!(from_raw_parts(0x1D, false, true), PAUSE);
        assert_eq!(from_raw_parts(0x48, true, false), key::UP);
    }
}
