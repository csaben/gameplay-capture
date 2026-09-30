"""Slow, obviously-correct reference for the action vectors.

A plain per-event loop, independent of the vectorised polars/numpy code in
`align.py`.  Used by the synthetic generator (to burn the expected actions
into the video) and by the tests as an oracle.
"""

from __future__ import annotations

import numpy as np

from .action_spec import ActionSpec, MOUSE_BUTTONS, PAD_AXES, PAD_BUTTONS, default_spec, key_slot


def reference_actions(events: list[tuple[int, str, str, int, float]], t: list[int] | np.ndarray,
                      offset: int, tick: int, spec: ActionSpec | None = None) -> np.ndarray:
    spec = spec or default_spec()
    n = len(t)
    ws = [int(x) + offset for x in t]
    we = ws[1:] + [ws[-1] + tick] if n else []
    out = np.zeros((n, spec.dim), dtype=np.float32)
    events = sorted(enumerate(events), key=lambda p: (p[1][0], p[0]))
    events = [e for _, e in events]
    state: dict[int, float] = {}
    j = 0
    for k in range(n):
        # everything strictly before window end updates state
        while j < len(events) and events[j][0] < we[k]:
            t_ev, dev, kind, code, val = events[j]
            j += 1
            col, v, is_sum = _col(spec, dev, kind, code, val)
            if col is None:
                continue
            if is_sum:
                if t_ev >= ws[k]:
                    out[k, col] += v
                # else: before this window. It either belonged to an earlier
                # frame's window (already consumed in order) or precedes the
                # first window (ignored for sums).
            else:
                state[col] = v
        for c, v in state.items():
            out[k, c] = v
    return out


def _col(spec: ActionSpec, dev: str, kind: str, code: int, val: float):
    if dev == "keyboard" and kind in ("key_down", "key_up"):
        s = key_slot(code)
        if s < 0:
            return None, 0, False
        return spec.slice("keys").start + s, 1.0 if kind == "key_down" else 0.0, False
    if dev == "mouse" and kind == "mouse_move" and code <= 1:
        return spec.index("mouse.dx") + code, val, True
    if dev == "mouse" and kind == "wheel" and code <= 1:
        return spec.index("wheel.v") + code, val, True
    if dev == "mouse" and kind == "mouse_button" and code < len(MOUSE_BUTTONS):
        return spec.slice("mouse_button").start + code, 1.0 if val > 0.5 else 0.0, False
    if dev == "gamepad" and kind == "axis" and code < len(PAD_AXES):
        return spec.slice("pad_axis").start + code, val, False
    if dev == "gamepad" and kind == "button" and code < len(PAD_BUTTONS):
        return spec.slice("pad_button").start + code, val, False
    return None, 0, False
