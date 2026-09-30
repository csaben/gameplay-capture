"""Focus change-log filtering and idle flagging."""

from __future__ import annotations

import numpy as np
import polars as pl

from gamecap_pipeline.action_spec import default_spec
from gamecap_pipeline.align import AlignConfig, compute_actions, focus_flags, idle_flags, windows
from gamecap_pipeline.segment import validate_and_load
from gamecap_pipeline.align import align_segment

SPEC = default_spec()
SEC = 1_000_000_000


def focus_df(rows):
    return pl.DataFrame({"t_ns": [r[0] for r in rows], "focused": [r[1] for r in rows],
                         "game_id": [r[2] for r in rows]},
                        schema={"t_ns": pl.Int64, "focused": pl.Boolean, "game_id": pl.Utf8})


def ev_df(events):
    return pl.DataFrame(
        {"t_ns": [e[0] for e in events], "device": [e[1] for e in events], "kind": [e[2] for e in events],
         "code": [e[3] for e in events], "value": [e[4] for e in events]},
        schema={"t_ns": pl.Int64, "device": pl.Utf8, "kind": pl.Utf8, "code": pl.UInt32, "value": pl.Float32})


def test_focus_change_log_holds_until_next_row():
    t = np.arange(0, 1000, 100)            # frames at 0,100,...,900
    ws, we = windows(t, 0, 100)
    rows = [(0, True, "game.exe"), (250, False, ""), (430, True, "game.exe"), (800, True, "other.exe")]
    focused, games = focus_flags(focus_df(rows), t, ws, we, "manifest.exe", AlignConfig())
    # frame 2 window [200,300) contains the unfocus at 250; frames 3,4 unfocused at t;
    # frame 4 (t=400) is unfocused at t even though focus returns at 430 inside its window
    assert focused.tolist() == [True, True, False, False, False, True, True, True, True, True]
    assert games[0] == "game.exe" and games[3] == "manifest.exe" and games[8] == "other.exe"


def test_focus_before_first_row_backfill_and_strict():
    t = np.array([-50, 50, 150])
    ws, we = windows(t, 0, 100)
    rows = [(0, True, "g")]
    assert focus_flags(focus_df(rows), t, ws, we, "g", AlignConfig())[0].tolist() == [True] * 3
    strict = AlignConfig(backfill_first_focus_row=False)
    assert focus_flags(focus_df(rows), t, ws, we, "g", strict)[0].tolist() == [False, True, True]


def test_focus_empty_table_means_focused():
    t = np.array([0, 100])
    ws, we = windows(t, 0, 100)
    assert focus_flags(focus_df([]), t, ws, we, "g", AlignConfig())[0].all()


def _idle(events, t, seg_start, seg_end):
    ws, we = windows(t, 0, SEC // 20)
    acts, _ = compute_actions(ev_df(events), ws, we, SPEC)
    return idle_flags(ev_df(events), acts, ws, we, seg_start, seg_end, SPEC, AlignConfig())


def test_idle_gap_longer_than_threshold():
    t = np.arange(0, 30 * SEC, SEC // 20)  # 30 s at 20 Hz
    events = [(1 * SEC, "mouse", "mouse_move", 0, 1.0), (13 * SEC, "mouse", "mouse_move", 0, 1.0),
              (20 * SEC, "mouse", "mouse_move", 0, 1.0), (25 * SEC, "mouse", "mouse_move", 0, 1.0)]
    idle = _idle(events, t, 0, 30 * SEC)
    sec = t / SEC
    # 1 s -> 13 s is a 12 s gap: idle strictly inside it; 13->20 and 20->25 are short
    assert idle[(sec > 1.05) & (sec < 12.9)].all()
    assert not idle[(sec >= 13) & (sec < 30)].any()
    assert not idle[sec < 1.0].any()  # 0 -> 1 s gap from segment start is short


def test_held_key_is_not_idle():
    t = np.arange(0, 30 * SEC, SEC // 20)
    events = [(1 * SEC, "keyboard", "key_down", 0x11, 1.0), (14 * SEC, "keyboard", "key_up", 0x11, 0.0)]
    idle = _idle(events, t, 0, 30 * SEC)
    sec = t / SEC
    assert not idle[(sec > 1) & (sec < 13.9)].any()   # W held the whole gap
    assert idle[(sec > 14.1) & (sec < 29.9)].all()    # 14 s -> segment end (30 s) is 16 s


def test_synth_segment_focus_and_idle(seg0):
    seg = validate_and_load(seg0)
    al = align_segment(seg)
    rel = (seg.frames["capture_ns"].to_numpy() - seg.manifest["t_start_ns"]) / SEC
    # scripted: unfocused 48..51 s, idle 30..45 s (last input at 30.0, next at 45.0)
    assert not al.focused[(rel > 48.1) & (rel < 50.9)].any()
    assert al.focused[(rel > 1) & (rel < 47.8)].all()
    assert al.focused[(rel > 51.2) & (rel < 59.5)].all()
    assert al.idle[(rel > 30.2) & (rel < 44.8)].all()
    assert not al.idle[(rel > 0.5) & (rel < 29.8)].any()
    assert not al.idle[(rel > 45.1)].any()
