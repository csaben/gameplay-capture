"""Alignment: window boundaries, offset handling, state vs sum features."""

from __future__ import annotations

import numpy as np
import polars as pl
import pytest

from gamecap_pipeline.action_spec import default_spec, key_slot, slot_name, PAUSE_SLOT
from gamecap_pipeline.align import assign_frames, compute_actions, windows
from gamecap_pipeline.reference import reference_actions

SPEC = default_spec()
DX = SPEC.index("mouse.dx")
DY = SPEC.index("mouse.dy")
KW = SPEC.index("key.w")


def ev_df(events):
    return pl.DataFrame(
        {"t_ns": [e[0] for e in events], "device": [e[1] for e in events], "kind": [e[2] for e in events],
         "code": [e[3] for e in events], "value": [e[4] for e in events]},
        schema={"t_ns": pl.Int64, "device": pl.Utf8, "kind": pl.Utf8, "code": pl.UInt32, "value": pl.Float32})


def run(events, t, offset, tick=100):
    ws, we = windows(np.array(t), offset, tick)
    acts, stats = compute_actions(ev_df(events), ws, we, SPEC)
    ref = reference_actions(events, t, offset, tick, SPEC)
    np.testing.assert_allclose(acts, ref, atol=1e-5)
    return acts, stats


def test_windows_last_frame_uses_tick():
    ws, we = windows(np.array([0, 100, 200]), 10, 100)
    assert ws.tolist() == [10, 110, 210]
    assert we.tolist() == [110, 210, 310]


@pytest.mark.parametrize("offset", [0, 10, -35])
def test_mouse_sums_on_boundaries(offset):
    t = [0, 100, 200, 300]
    b = [x + offset for x in t]  # window starts
    events = [
        (b[0], "mouse", "mouse_move", 0, 1.0),        # exactly at start of W0 -> frame 0
        (b[1] - 1, "mouse", "mouse_move", 0, 2.0),    # last ns of W0 -> frame 0
        (b[1], "mouse", "mouse_move", 0, 4.0),        # exactly at boundary -> frame 1
        (b[3] + 99, "mouse", "mouse_move", 1, 8.0),   # last ns of the last window -> frame 3
        (b[3] + 100, "mouse", "mouse_move", 1, 16.0),  # == end of last window -> dropped
        (b[0] - 1, "mouse", "mouse_move", 0, 32.0),   # before the first window -> dropped
    ]
    acts, stats = run(events, t, offset)
    assert acts[:, DX].tolist() == [3.0, 4.0, 0.0, 0.0]
    assert acts[:, DY].tolist() == [0.0, 0.0, 0.0, 8.0]
    assert stats["events_after_last_window"] == 1
    assert stats["events_before_first_window"] == 1


def test_offset_shifts_assignment():
    t = [0, 100, 200]
    e = [(105, "mouse", "mouse_move", 0, 1.0)]
    assert run(e, t, 0)[0][:, DX].tolist() == [0, 1, 0]
    assert run(e, t, 10)[0][:, DX].tolist() == [1, 0, 0]      # W0 = [10,110)
    assert run(e, t, -10)[0][:, DX].tolist() == [0, 1, 0]     # W1 = [90,190)
    assert run(e, t, -96)[0][:, DX].tolist() == [0, 0, 1]     # W2 = [104,204)


def test_key_held_at_window_end_boundaries():
    t = [0, 100, 200, 300]
    off = 10
    events = [
        (110, "keyboard", "key_down", 0x11, 1.0),   # exactly at W1 start
        (210, "keyboard", "key_up", 0x11, 0.0),     # exactly at W1 end (= W2 start)
    ]
    acts, _ = run(events, t, off)
    # held at end of W1 (up at 210 is not < 210), released by end of W2
    assert acts[:, KW].tolist() == [0, 1, 0, 0]


def test_tap_inside_window_is_not_held_at_end():
    acts, _ = run([(20, "keyboard", "key_down", 0x11, 1.0), (60, "keyboard", "key_up", 0x11, 0.0)],
                  [0, 100], 0)
    assert acts[:, KW].tolist() == [0, 0]


def test_state_from_before_first_window_and_autorepeat():
    t = [0, 100, 200]
    events = [(-500, "keyboard", "key_down", 0x11, 1.0)] + \
             [(x, "keyboard", "key_down", 0x11, 1.0) for x in (30, 60, 90, 120)] + \
             [(250, "keyboard", "key_up", 0x11, 0.0)]
    acts, _ = run(events, t, 0)
    assert acts[:, KW].tolist() == [1, 1, 0]


def test_repeated_frames_get_empty_window():
    t = [0, 100, 100, 200]  # frame 2 repeats frame 1's capture
    events = [(150, "mouse", "mouse_move", 0, 5.0), (100, "mouse", "mouse_move", 0, 1.0)]
    acts, _ = run(events, t, 0)
    assert acts[:, DX].tolist() == [0, 0, 6, 0]


def test_extended_keys_and_vocab():
    assert key_slot(0x1D) == 0x1D and slot_name(0x1D) == "lctrl"
    assert key_slot(0xE01D) == 0x9D and slot_name(0x9D) == "rctrl"
    assert key_slot(0xE048) == 0xC8 and slot_name(0xC8) == "up"
    assert key_slot(0xE11D) == PAUSE_SLOT == key_slot(0xE11D45)
    assert key_slot(0xE02A) == -1  # fake shift
    assert key_slot(0x1FF) == -1 and key_slot(0) == -1
    assert SPEC.dim == 292 and len(set(SPEC.names)) == SPEC.dim
    t = [0, 100]
    events = [(10, "keyboard", "key_down", 0xE01D, 1.0), (10, "keyboard", "key_down", 0x1D, 1.0),
              (10, "keyboard", "key_down", 0x1FF, 1.0)]
    acts, stats = run(events, t, 0)
    assert acts[0, SPEC.index("key.rctrl")] == 1 and acts[0, SPEC.index("key.lctrl")] == 1
    assert stats["unknown_codes"] == 1


def test_mouse_buttons_wheel_and_gamepad():
    t = [0, 100, 200, 300]
    events = [
        (10, "mouse", "mouse_button", 0, 1.0), (150, "mouse", "mouse_button", 0, 0.0),
        (20, "mouse", "wheel", 0, 1.0), (30, "mouse", "wheel", 0, 1.0), (40, "mouse", "wheel", 1, -0.5),
        (50, "gamepad", "axis", 0, 0.5), (120, "gamepad", "axis", 0, -0.25),
        (60, "gamepad", "axis", 5, 0.9),
        (210, "gamepad", "button", 0, 1.0), (220, "gamepad", "button", 9, 0.4),
        (230, "gamepad", "axis", 99, 1.0),  # unknown axis id: ignored
    ]
    acts, stats = run(events, t, 0)
    assert acts[:, SPEC.index("mouse.button.left")].tolist() == [1, 0, 0, 0]
    assert acts[:, SPEC.index("wheel.v")].tolist() == [2, 0, 0, 0]
    assert acts[:, SPEC.index("wheel.h")].tolist() == [-0.5, 0, 0, 0]
    assert acts[:, SPEC.index("pad.axis.left_x")].tolist() == [0.5, -0.25, -0.25, -0.25]
    np.testing.assert_allclose(acts[:, SPEC.index("pad.axis.right_z")], [0.9] * 4, rtol=1e-6)
    assert acts[:, SPEC.index("pad.button.south")].tolist() == [0, 0, 1, 1]
    np.testing.assert_allclose(acts[:, SPEC.index("pad.button.right_trigger2")], [0, 0, 0.4, 0.4], rtol=1e-6)
    assert stats["unknown_codes"] == 1


def test_assign_frames_side():
    ws = np.array([10, 110, 110, 210])
    # an event at a repeated start belongs to the later (non-empty) window
    assert assign_frames(np.array([5, 10, 109, 110, 300]), ws).tolist() == [-1, 0, 0, 2, 3]


@pytest.mark.parametrize("seed", range(5))
def test_random_events_match_reference(seed):
    rng = np.random.default_rng(seed)
    n = 200
    caps = np.cumsum(rng.integers(0, 90, n)) + 1_000
    offset = int(rng.integers(-60, 60))
    kinds = [("keyboard", "key_down"), ("keyboard", "key_up"), ("mouse", "mouse_move"),
             ("mouse", "mouse_button"), ("mouse", "wheel"), ("gamepad", "axis"), ("gamepad", "button")]
    events = []
    for _ in range(3000):
        dev, kind = kinds[rng.integers(len(kinds))]
        code = int(rng.choice([0x11, 0x1E, 0xE048, 0xE01D, 0x2A])) if dev == "keyboard" else int(rng.integers(0, 6))
        val = float(rng.integers(-5, 6)) if kind in ("mouse_move", "wheel") else float(rng.integers(0, 2))
        events.append((int(rng.integers(caps[0] - 200, caps[-1] + 300)), dev, kind, code, val))
    run(events, caps, offset, tick=50)
