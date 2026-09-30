"""The per-frame action vector layout, defined once.

Every shard sample carries `<key>.actions.npy`: float32 `[T, D]`, one row per
video frame.  Row k summarises the input events in frame k's action window
`[t_k + offset, t_{k+1} + offset)` (see `align.py`).  The layout below is
written into every shard sidecar (`<shard>.sources.json`) and as the
dataset-level `action_spec.json`; each clip's json carries
`action_spec: {version, sha, dim}` so a loader can check it matches.

Groups (in column order)
------------------------
keys          256  held at window end (0/1).  Slot = scan-code vocabulary below.
mouse.dx/dy     2  sum of raw relative mouse deltas within the window (counts).
mouse.button    5  held at window end (0/1): left, right, middle, back, forward.
wheel.v/h       2  sum of wheel detents within the window (may be fractional).
pad.axis        8  last value at window end (-1..1, triggers 0..1), 0 if never seen.
pad.button     19  last value at window end (0/1, or analog 0..1), 0 if never seen.

Key vocabulary (256 slots)
--------------------------
Key codes in `inputs.parquet` are PS/2 scan-code set 1 make codes:
  * base keys:      0x01..0x7F           -> slot = code
  * extended keys:  0xE001..0xE07F       -> slot = 0x80 | (code & 0x7F)
    (the E0 prefix byte is carried in the high byte, e.g. right ctrl = 0xE01D,
    arrow up = 0xE048)
  * Pause (E1 1D 45), logged as 0xE11D or 0xE11D45 -> slot 0xC5 ("pause";
    E0 45 is otherwise unused)
  * the E0 2A / E0 36 "fake shift" codes some keyboards emit around extended
    keys are ignored, as is anything else outside the ranges above
    (counted in the per-segment stats as `unknown_codes`).

Gamepad codes (gilrs enum order, see SCHEMA.md)
-----------------------------------------------
axis:   0 left_x, 1 left_y, 2 left_z (LT), 3 right_x, 4 right_y, 5 right_z (RT),
        6 dpad_x, 7 dpad_y
button: 0 south, 1 east, 2 north, 3 west, 4 c, 5 z, 6 left_trigger (LB),
        7 left_trigger2 (LT), 8 right_trigger (RB), 9 right_trigger2 (RT),
        10 select, 11 start, 12 mode, 13 left_thumb, 14 right_thumb,
        15 dpad_up, 16 dpad_down, 17 dpad_left, 18 dpad_right
"""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass
from functools import lru_cache

import numpy as np

ACTION_SPEC_VERSION = 1

N_KEY_SLOTS = 256
EXTENDED_BASE = 0x80
PAUSE_SLOT = 0xC5
FAKE_SHIFT_CODES = frozenset({0xE02A, 0xE036, 0xE0AA, 0xE0B6})

# --- scan code set 1 names -------------------------------------------------
_BASE_NAMES: dict[int, str] = {
    0x01: "esc", 0x02: "1", 0x03: "2", 0x04: "3", 0x05: "4", 0x06: "5",
    0x07: "6", 0x08: "7", 0x09: "8", 0x0A: "9", 0x0B: "0", 0x0C: "minus",
    0x0D: "equal", 0x0E: "backspace", 0x0F: "tab", 0x10: "q", 0x11: "w",
    0x12: "e", 0x13: "r", 0x14: "t", 0x15: "y", 0x16: "u", 0x17: "i",
    0x18: "o", 0x19: "p", 0x1A: "lbracket", 0x1B: "rbracket", 0x1C: "enter",
    0x1D: "lctrl", 0x1E: "a", 0x1F: "s", 0x20: "d", 0x21: "f", 0x22: "g",
    0x23: "h", 0x24: "j", 0x25: "k", 0x26: "l", 0x27: "semicolon",
    0x28: "apostrophe", 0x29: "grave", 0x2A: "lshift", 0x2B: "backslash",
    0x2C: "z", 0x2D: "x", 0x2E: "c", 0x2F: "v", 0x30: "b", 0x31: "n",
    0x32: "m", 0x33: "comma", 0x34: "period", 0x35: "slash", 0x36: "rshift",
    0x37: "kp_multiply", 0x38: "lalt", 0x39: "space", 0x3A: "capslock",
    0x3B: "f1", 0x3C: "f2", 0x3D: "f3", 0x3E: "f4", 0x3F: "f5", 0x40: "f6",
    0x41: "f7", 0x42: "f8", 0x43: "f9", 0x44: "f10", 0x45: "numlock",
    0x46: "scrolllock", 0x47: "kp7", 0x48: "kp8", 0x49: "kp9",
    0x4A: "kp_minus", 0x4B: "kp4", 0x4C: "kp5", 0x4D: "kp6", 0x4E: "kp_plus",
    0x4F: "kp1", 0x50: "kp2", 0x51: "kp3", 0x52: "kp0", 0x53: "kp_period",
    0x56: "intl_backslash", 0x57: "f11", 0x58: "f12", 0x59: "kp_equal",
    0x64: "f13", 0x65: "f14", 0x66: "f15", 0x67: "f16", 0x68: "f17",
    0x69: "f18", 0x6A: "f19", 0x6B: "f20", 0x6C: "f21", 0x6D: "f22",
    0x6E: "f23", 0x70: "kana", 0x73: "ro", 0x76: "f24", 0x79: "henkan",
    0x7B: "muhenkan", 0x7D: "yen",
}
_EXT_NAMES: dict[int, str] = {
    0x10: "media_prev", 0x19: "media_next", 0x1C: "kp_enter", 0x1D: "rctrl",
    0x20: "mute", 0x21: "calculator", 0x22: "media_play_pause",
    0x24: "media_stop", 0x2E: "volume_down", 0x30: "volume_up",
    0x32: "browser_home", 0x35: "kp_divide", 0x37: "printscreen",
    0x38: "ralt", 0x46: "ctrl_break", 0x47: "home", 0x48: "up",
    0x49: "pageup", 0x4B: "left", 0x4D: "right", 0x4F: "end", 0x50: "down",
    0x51: "pagedown", 0x52: "insert", 0x53: "delete", 0x5B: "lmeta",
    0x5C: "rmeta", 0x5D: "menu", 0x5E: "power", 0x5F: "sleep", 0x63: "wake",
    0x65: "browser_search", 0x66: "browser_favorites", 0x67: "browser_refresh",
    0x68: "browser_stop", 0x69: "browser_forward", 0x6A: "browser_back",
    0x6B: "my_computer", 0x6C: "mail", 0x6D: "media_select",
}

MOUSE_BUTTONS = ["left", "right", "middle", "back", "forward"]
PAD_AXES = ["left_x", "left_y", "left_z", "right_x", "right_y", "right_z", "dpad_x", "dpad_y"]
PAD_BUTTONS = [
    "south", "east", "north", "west", "c", "z", "left_trigger", "left_trigger2",
    "right_trigger", "right_trigger2", "select", "start", "mode", "left_thumb",
    "right_thumb", "dpad_up", "dpad_down", "dpad_left", "dpad_right",
]


def key_slot(code: int) -> int:
    """Map a logged scan code to its vocabulary slot, or -1 if unknown/ignored."""
    code = int(code)
    if code in FAKE_SHIFT_CODES:
        return -1
    if 0x01 <= code <= 0x7F:
        return code
    if 0xE001 <= code <= 0xE07F:
        return EXTENDED_BASE | (code & 0x7F)
    if code in (0xE11D, 0xE11D45):
        return PAUSE_SLOT
    return -1


def slot_scan_code(slot: int) -> int:
    """Canonical scan code for a slot (inverse of `key_slot`)."""
    if slot == PAUSE_SLOT:
        return 0xE11D
    if slot >= EXTENDED_BASE:
        return 0xE000 | (slot & 0x7F)
    return slot


def slot_name(slot: int) -> str:
    if slot == PAUSE_SLOT:
        return "pause"
    if slot >= EXTENDED_BASE:
        low = slot & 0x7F
        return _EXT_NAMES.get(low, f"sc_e0{low:02x}")
    if slot == 0:
        return "sc_00"
    return _BASE_NAMES.get(slot, f"sc_{slot:02x}")


KEY_NAME_TO_CODE: dict[str, int] = {
    **{name: code for code, name in _BASE_NAMES.items()},
    **{name: 0xE000 | low for low, name in _EXT_NAMES.items()},
    "pause": 0xE11D,
}


@dataclass(frozen=True)
class ActionSpec:
    names: tuple[str, ...]
    groups: dict  # group name -> {"start", "stop", "semantics"}

    @property
    def dim(self) -> int:
        return len(self.names)

    def slice(self, group: str) -> slice:
        g = self.groups[group]
        return slice(g["start"], g["stop"])

    def index(self, name: str) -> int:
        return self.names.index(name)

    def to_json_obj(self) -> dict:
        return {
            "version": ACTION_SPEC_VERSION,
            "dim": self.dim,
            "dtype": "float32",
            "groups": self.groups,
            "names": list(self.names),
            "key_vocabulary": {
                "space": "PS/2 scan code set 1",
                "rule": "0x01..0x7F -> slot=code; 0xE001..0xE07F -> slot=0x80|(code&0x7F); "
                        "0xE11D/0xE11D45 (Pause) -> slot 0xC5; others ignored",
                "slot_scan_codes": [slot_scan_code(s) for s in range(N_KEY_SLOTS)],
            },
            "window": "[capture_ns[k] + latency_offset_ns, capture_ns[k+1] + latency_offset_ns); "
                      "last frame: [t_k + offset, t_k + tick + offset)",
        }

    @property
    def sha(self) -> str:
        blob = json.dumps(self.to_json_obj(), sort_keys=True).encode()
        return hashlib.sha256(blob).hexdigest()[:16]

    def ref(self) -> dict:
        return {"version": ACTION_SPEC_VERSION, "sha": self.sha, "dim": self.dim}

    # --- helpers for humans (replay overlay, debugging) ---
    def describe_row(self, row: np.ndarray) -> dict:
        keys = [self.names[i][4:] for i in range(*self.slice("keys").indices(self.dim)) if row[i] > 0.5]
        mb = [MOUSE_BUTTONS[i] for i, v in enumerate(row[self.slice("mouse_button")]) if v > 0.5]
        pb = [PAD_BUTTONS[i] for i, v in enumerate(row[self.slice("pad_button")]) if v > 0.5]
        return {
            "keys": keys,
            "mouse_dx": float(row[self.index("mouse.dx")]),
            "mouse_dy": float(row[self.index("mouse.dy")]),
            "mouse_buttons": mb,
            "wheel_v": float(row[self.index("wheel.v")]),
            "wheel_h": float(row[self.index("wheel.h")]),
            "pad_axes": {n: float(v) for n, v in zip(PAD_AXES, row[self.slice("pad_axis")])},
            "pad_buttons": pb,
        }


@lru_cache(maxsize=1)
def default_spec() -> ActionSpec:
    names: list[str] = []
    groups: dict = {}

    def add(group: str, cols: list[str], semantics: str) -> None:
        start = len(names)
        names.extend(cols)
        groups[group] = {"start": start, "stop": len(names), "semantics": semantics}

    add("keys", [f"key.{slot_name(s)}" for s in range(N_KEY_SLOTS)], "held_at_window_end")
    add("mouse_delta", ["mouse.dx", "mouse.dy"], "sum_over_window")
    add("mouse_button", [f"mouse.button.{b}" for b in MOUSE_BUTTONS], "held_at_window_end")
    add("wheel", ["wheel.v", "wheel.h"], "sum_over_window")
    add("pad_axis", [f"pad.axis.{a}" for a in PAD_AXES], "last_value_at_window_end")
    add("pad_button", [f"pad.button.{b}" for b in PAD_BUTTONS], "last_value_at_window_end")
    assert len(set(names)) == len(names), "duplicate action column names"
    return ActionSpec(names=tuple(names), groups=groups)


ACTION_DIM = default_spec().dim  # 292
