"""Per-frame input state for the viewer.

Frame k owns the input events with ``tick_ns[k] + off <= t < tick_ns[k+1] + off``
(``off`` = manifest ``latency_offset_ns``; the last frame's window is one tick
long). Events before frame 0's window are folded into frame 0. State (held keys,
mouse buttons, pad buttons, axes) is the state at the end of the window; mouse
motion and wheel are summed over the window.
"""

from __future__ import annotations

import io

import pyarrow.parquet as pq

# PS/2 scan code set 1 make codes (extended keys = 0xE000 | code); see SCHEMA.md.
KEY_NAMES: dict[int, str] = {
    0x01: "Esc", 0x0C: "-", 0x0D: "=", 0x0E: "Bksp", 0x0F: "Tab", 0x1A: "[", 0x1B: "]", 0x1C: "Enter",
    0x1D: "LCtrl", 0x27: ";", 0x28: "'", 0x29: "`", 0x2A: "LShift", 0x2B: "\\", 0x33: ",", 0x34: ".",
    0x35: "/", 0x36: "RShift", 0x37: "KP*", 0x38: "LAlt", 0x39: "Space", 0x3A: "Caps", 0x45: "NumLk",
    0x46: "ScrLk", 0x47: "KP7", 0x48: "KP8", 0x49: "KP9", 0x4A: "KP-", 0x4B: "KP4", 0x4C: "KP5",
    0x4D: "KP6", 0x4E: "KP+", 0x4F: "KP1", 0x50: "KP2", 0x51: "KP3", 0x52: "KP0", 0x53: "KP.",
    0x56: "<>", 0x57: "F11", 0x58: "F12",
    0xE01C: "KPEnter", 0xE01D: "RCtrl", 0xE035: "KP/", 0xE037: "PrtSc", 0xE038: "RAlt",
    0xE047: "Home", 0xE048: "Up", 0xE049: "PgUp", 0xE04B: "Left", 0xE04D: "Right", 0xE04F: "End",
    0xE050: "Down", 0xE051: "PgDn", 0xE052: "Ins", 0xE053: "Del", 0xE05B: "LWin", 0xE05C: "RWin",
    0xE05D: "Menu", 0xE11D: "Pause", 0xE11D45: "Pause",
}
for _i, _ch in enumerate("1234567890"):
    KEY_NAMES[0x02 + _i] = _ch
for _row, _start in (("QWERTYUIOP", 0x10), ("ASDFGHJKL", 0x1E), ("ZXCVBNM", 0x2C)):
    for _i, _ch in enumerate(_row):
        KEY_NAMES[_start + _i] = _ch
for _i in range(10):
    KEY_NAMES[0x3B + _i] = f"F{_i + 1}"

MOUSE_BUTTONS = ["LMB", "RMB", "MMB", "Mouse4", "Mouse5"]
# gilrs order (SCHEMA.md "Gamepad")
PAD_BUTTONS = ["A", "B", "Y", "X", "C", "Z", "LB", "LT", "RB", "RT", "Select", "Start", "Mode",
               "LS", "RS", "DUp", "DDown", "DLeft", "DRight"]
PAD_AXES = ["LX", "LY", "LZ", "RX", "RY", "RZ", "DPadX", "DPadY"]
# Fake shifts from E0-prefixed navigation keys; ignored like the pipeline does.
IGNORED_KEYS = {0xE02A, 0xE036}


def key_name(code: int) -> str:
    return KEY_NAMES.get(code, f"0x{code:X}")


def _label(device: str, kind: str, code: int) -> str:
    if device == "mouse":
        return MOUSE_BUTTONS[code] if code < len(MOUSE_BUTTONS) else f"Mouse{code + 1}"
    if device == "gamepad":
        return "Pad " + (PAD_BUTTONS[code] if code < len(PAD_BUTTONS) else str(code))
    return key_name(code)


def read_parquet_columns(data: bytes) -> dict[str, list]:
    return pq.read_table(io.BytesIO(data)).to_pydict()


def compute_timeline(frames: dict[str, list], inputs: dict[str, list], focus: dict[str, list],
                     manifest: dict) -> dict:
    """Column dicts (as from ``read_parquet_columns``) -> JSON-ready per-frame arrays."""
    ticks = frames["tick_ns"]
    n = len(ticks)
    rate = int(manifest.get("rate_hz") or 20)
    period = (ticks[1] - ticks[0]) if n > 1 else int(1e9 // rate)
    off = int(manifest.get("latency_offset_ns") or 0)

    order = sorted(range(len(inputs.get("t_ns", []))), key=lambda i: inputs["t_ns"][i])  # stable
    ev_t = [inputs["t_ns"][i] for i in order]
    ev_dev = [inputs["device"][i] for i in order]
    ev_kind = [inputs["kind"][i] for i in order]
    ev_code = [int(inputs["code"][i]) for i in order]
    ev_val = [float(inputs["value"][i]) for i in order]

    f_order = sorted(range(len(focus.get("t_ns", []))), key=lambda i: focus["t_ns"][i])
    f_t = [focus["t_ns"][i] for i in f_order]
    f_val = [bool(focus["focused"][i]) for i in f_order]

    keys: set[int] = set()
    mb: set[int] = set()
    pad: set[int] = set()
    axes = [0.0] * len(PAD_AXES)
    focused = True  # empty focus table = focused throughout
    has_pad = any(d == "gamepad" for d in ev_dev)

    out = {k: [] for k in ("keys", "mb", "pad", "dx", "dy", "wheel", "pressed", "ev", "focused", "repeated")}
    if has_pad:
        out["axes"] = []
    seen_keys: set[int] = set()
    i = fi = 0
    for k in range(n):
        end = (ticks[k + 1] if k + 1 < n else ticks[k] + period) + off
        dx = dy = wheel = 0.0
        pressed: list[str] = []
        cnt = 0
        while i < len(ev_t) and ev_t[i] < end:
            dev, kind, code, v = ev_dev[i], ev_kind[i], ev_code[i], ev_val[i]
            i += 1
            cnt += 1
            if kind == "key_down":
                if code in IGNORED_KEYS:
                    continue
                if code not in keys:
                    pressed.append(_label(dev, kind, code))
                keys.add(code)
                seen_keys.add(code)
            elif kind == "key_up":
                keys.discard(code)
            elif kind == "mouse_button":
                if v > 0.5:
                    if code not in mb:
                        pressed.append(_label(dev, kind, code))
                    mb.add(code)
                else:
                    mb.discard(code)
            elif kind == "mouse_move":
                if code == 0:
                    dx += v
                else:
                    dy += v
            elif kind == "wheel":
                if code == 0:
                    wheel += v
            elif kind == "button":
                if v > 0.5:
                    if code not in pad:
                        pressed.append(_label(dev, kind, code))
                    pad.add(code)
                else:
                    pad.discard(code)
            elif kind == "axis" and code < len(axes):
                axes[code] = v
        while fi < len(f_t) and f_t[fi] < end:
            focused = f_val[fi]
            fi += 1
        out["keys"].append(sorted(keys))
        out["mb"].append(sorted(mb))
        out["pad"].append(sorted(pad))
        out["dx"].append(round(dx, 2))
        out["dy"].append(round(dy, 2))
        out["wheel"].append(round(wheel, 2))
        out["pressed"].append(pressed)
        out["ev"].append(cnt)
        out["focused"].append(1 if focused else 0)
        out["repeated"].append(1 if frames["repeated"][k] else 0)
        if has_pad:
            out["axes"].append([round(a, 3) for a in axes])

    return {
        "n": n,
        "rate_hz": rate,
        "offset_ns": off,
        "has_gamepad": has_pad,
        "events": len(ev_t),
        "key_names": {str(c): key_name(c) for c in sorted(seen_keys)},
        **out,
    }


def timeline_from_bytes(frames: bytes, inputs: bytes, focus: bytes | None, manifest: dict) -> dict:
    fc = read_parquet_columns(focus) if focus else {"t_ns": [], "focused": [], "game_id": []}
    return compute_timeline(read_parquet_columns(frames), read_parquet_columns(inputs), fc, manifest)
