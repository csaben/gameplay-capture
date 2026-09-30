"""Steps 3-5: align input events to frames, build action vectors, focus + idle flags.

Frame k (capture time t_k) owns the action window
    W_k = [t_k + offset, t_{k+1} + offset)          (k < n-1)
    W_k = [t_k + offset, t_k + tick + offset)       (last frame)
with offset = manifest.latency_offset_ns.  An event at time t belongs to the
unique frame whose window contains t; windows tile time, so an event exactly
on a boundary t_{k+1} + offset belongs to frame k+1.  Repeated frames share a
capture time with their predecessor, which gives the predecessor an empty
window (its "sum" features are 0 and its "held" features equal the state at
that instant).

Sum features (mouse delta, wheel) add up events inside W_k.  State features
(keys, mouse buttons, pad buttons, pad axes) are the state after every event
with t < end(W_k), i.e. "held at window end"; events before the first window
still initialise state.
"""

from __future__ import annotations

from dataclasses import dataclass, field

import numpy as np
import polars as pl

from .action_spec import ActionSpec, default_spec, key_slot, MOUSE_BUTTONS, PAD_AXES, PAD_BUTTONS

@dataclass
class AlignConfig:
    time_column: str = "capture_ns"  # or "tick_ns"
    idle_threshold_ns: int = 10_000_000_000
    idle_axis_deadzone: float = 0.2
    gap_factor: float = 1.5  # tick gap > gap_factor * tick => dropped frame(s) after k
    # Times before the first focus row take the first row's state (the recorder
    # writes the state at segment open; frame 0's capture time / window may
    # precede it slightly).  Set False to treat that time as unfocused.
    backfill_first_focus_row: bool = True


@dataclass
class Alignment:
    actions: np.ndarray        # float32 [n, D]
    win_start: np.ndarray      # int64 [n]
    win_end: np.ndarray        # int64 [n]
    focused: np.ndarray        # bool [n]  focused over the frame and its whole window
    idle: np.ndarray           # bool [n]
    gap_after: np.ndarray      # bool [n]  dropped frames between k and k+1
    game_id: list[str]         # per frame, from focus rows (fallback manifest)
    stats: dict = field(default_factory=dict)


def windows(t: np.ndarray, offset: int, tick: int) -> tuple[np.ndarray, np.ndarray]:
    t = t.astype(np.int64)
    ws = t + offset
    we = np.empty_like(ws)
    we[:-1] = ws[1:]
    if len(t):
        we[-1] = ws[-1] + tick
    return ws, we


def assign_frames(t_ev: np.ndarray, ws: np.ndarray) -> np.ndarray:
    """Index of the window containing each event (-1 before the first window).

    Callers must still drop events with t >= win_end[-1].
    """
    return np.searchsorted(ws, t_ev, side="right").astype(np.int64) - 1


def _slots(inputs: pl.DataFrame, spec: ActionSpec) -> pl.DataFrame:
    """Annotate events with (column index, value, mode) where mode is 'sum' or 'state'."""
    ks = spec.slice("keys").start
    mb0 = spec.slice("mouse_button").start
    pa0 = spec.slice("pad_axis").start
    pb0 = spec.slice("pad_button").start
    dx = spec.index("mouse.dx")
    wv = spec.index("wheel.v")

    codes = inputs["code"].to_numpy().astype(np.int64)
    kind = inputs["kind"].to_numpy()
    device = inputs["device"].to_numpy()
    val = inputs["value"].to_numpy().astype(np.float64)
    n = len(codes)
    col = np.full(n, -1, dtype=np.int64)
    out_val = val.copy()
    is_sum = np.zeros(n, dtype=bool)

    is_key = (device == "keyboard") & ((kind == "key_down") | (kind == "key_up"))
    if is_key.any():
        uniq, inv = np.unique(codes[is_key], return_inverse=True)
        lut = np.array([key_slot(c) for c in uniq], dtype=np.int64)
        s = lut[inv]
        col[is_key] = np.where(s >= 0, ks + s, -1)
        out_val[is_key] = (kind[is_key] == "key_down").astype(np.float64)

    m = (device == "mouse") & (kind == "mouse_move") & (codes <= 1)
    col[m] = dx + codes[m]
    is_sum |= m
    m = (device == "mouse") & (kind == "wheel") & (codes <= 1)
    col[m] = wv + codes[m]
    is_sum |= m
    m = (device == "mouse") & (kind == "mouse_button") & (codes < len(MOUSE_BUTTONS))
    col[m] = mb0 + codes[m]
    out_val[m] = (val[m] > 0.5).astype(np.float64)
    m = (device == "gamepad") & (kind == "axis") & (codes < len(PAD_AXES))
    col[m] = pa0 + codes[m]
    m = (device == "gamepad") & (kind == "button") & (codes < len(PAD_BUTTONS))
    col[m] = pb0 + codes[m]

    return pl.DataFrame({
        "t_ns": inputs["t_ns"].to_numpy(),
        "col": col,
        "val": out_val,
        "is_sum": is_sum,
    })


def compute_actions(inputs: pl.DataFrame, ws: np.ndarray, we: np.ndarray,
                    spec: ActionSpec | None = None) -> tuple[np.ndarray, dict]:
    spec = spec or default_spec()
    n, d = len(ws), spec.dim
    out = np.zeros((n, d), dtype=np.float32)
    stats = {"events": inputs.height, "unknown_codes": 0, "events_after_last_window": 0,
             "events_before_first_window": 0}
    if n == 0 or inputs.height == 0:
        return out, stats

    # stable time order: ties keep file order (last write wins for state)
    inputs = inputs.with_row_index("_o").sort(["t_ns", "_o"]).drop("_o")
    ev = _slots(inputs, spec)
    stats["unknown_codes"] = int((ev["col"] < 0).sum())
    ev = ev.filter(pl.col("col") >= 0)
    t = ev["t_ns"].to_numpy()
    frame = assign_frames(t, ws)
    after = t >= we[-1]
    stats["events_after_last_window"] = int(after.sum())
    stats["events_before_first_window"] = int((frame < 0).sum())
    ev = ev.with_columns(frame=pl.Series(frame), after=pl.Series(after)).with_row_index("seq")

    # Sums: only events inside some window.
    sums = (ev.filter(pl.col("is_sum") & (pl.col("frame") >= 0) & ~pl.col("after"))
              .group_by(["frame", "col"]).agg(pl.col("val").sum()))
    if sums.height:
        np.add.at(out, (sums["frame"].to_numpy(), sums["col"].to_numpy()),
                  sums["val"].to_numpy().astype(np.float32))

    # State: last event per (frame, col); pre-window events land in frame 0.
    st = (ev.filter(~pl.col("is_sum") & ~pl.col("after"))
            .with_columns(pl.col("frame").clip(lower_bound=0))
            .sort("seq")
            .group_by(["frame", "col"]).agg(pl.col("val").last()))
    if st.height:
        cols = np.unique(st["col"].to_numpy())
        cidx = {c: i for i, c in enumerate(cols)}
        mat = np.full((n, len(cols)), np.nan)
        mat[st["frame"].to_numpy(), [cidx[c] for c in st["col"].to_numpy()]] = st["val"].to_numpy()
        # forward fill along frames
        has = ~np.isnan(mat)
        src = np.where(has, np.arange(n)[:, None], 0)
        np.maximum.accumulate(src, axis=0, out=src)
        filled = mat[src, np.arange(len(cols))[None, :]]
        out[:, cols] = np.nan_to_num(filled, nan=0.0).astype(np.float32)
    return out, stats


def focus_flags(focus: pl.DataFrame, t: np.ndarray, ws: np.ndarray, we: np.ndarray,
                default_game: str, cfg: AlignConfig) -> tuple[np.ndarray, list[str]]:
    """Frame k is focused iff focus holds at t_k and throughout W_k.

    focus.parquet is a change log; each row's state holds until the next row.
    An empty table means "focused for the whole segment" (nothing to gate).
    """
    n = len(t)
    if focus.height == 0:
        return np.ones(n, dtype=bool), [default_game] * n
    ft = focus["t_ns"].to_numpy()
    fv = focus["focused"].to_numpy().astype(bool)
    fg = focus["game_id"].to_list()

    def state_at(x: np.ndarray) -> np.ndarray:
        i = np.searchsorted(ft, x, side="right") - 1
        before = fv[0] if cfg.backfill_first_focus_row else False
        return np.where(i >= 0, fv[np.clip(i, 0, None)], before)

    unf_t = ft[~fv]
    # number of focused=false rows with t_ns < x
    unf_in_window = (np.searchsorted(unf_t, we, side="left") - np.searchsorted(unf_t, ws, side="left")) > 0
    focused = state_at(t) & state_at(ws) & ~unf_in_window
    gi = np.clip(np.searchsorted(ft, t, side="right") - 1, 0, None)
    games = [fg[i] or default_game for i in gi]
    return focused, games


def idle_flags(inputs: pl.DataFrame, actions: np.ndarray, ws: np.ndarray, we: np.ndarray,
               seg_start: int, seg_end: int, spec: ActionSpec, cfg: AlignConfig) -> np.ndarray:
    """Frame k is idle iff its window lies in an input-free gap longer than the
    threshold and nothing is held at window end (a held key / stick counts as input).
    Segment start/end bound the gaps (we cannot see the neighbouring segments here).
    """
    n = len(ws)
    if n == 0:
        return np.zeros(0, dtype=bool)
    et = np.sort(inputs["t_ns"].to_numpy()) if inputs.height else np.zeros(0, dtype=np.int64)
    lo = min(seg_start, int(ws[0]))
    hi = max(seg_end, int(we[-1]))
    i_prev = np.searchsorted(et, ws, side="right") - 1          # last event <= ws
    prev = np.where(i_prev >= 0, et[np.clip(i_prev, 0, None)] if len(et) else lo, lo)
    i_next = np.searchsorted(et, ws, side="right")               # first event > ws
    nxt = np.where(i_next < len(et), et[np.clip(i_next, 0, len(et) - 1)] if len(et) else hi, hi)
    quiet = (nxt >= we) & ((nxt - prev) > cfg.idle_threshold_ns)
    held = (actions[:, spec.slice("keys")].max(axis=1) > 0.5)
    held |= actions[:, spec.slice("mouse_button")].max(axis=1) > 0.5
    held |= actions[:, spec.slice("pad_button")].max(axis=1) > 0.5
    held |= np.abs(actions[:, spec.slice("pad_axis")]).max(axis=1) > cfg.idle_axis_deadzone
    return quiet & ~held


def align_segment(seg, cfg: AlignConfig | None = None, spec: ActionSpec | None = None) -> Alignment:
    cfg = cfg or AlignConfig()
    spec = spec or default_spec()
    m = seg.manifest
    tick = seg.tick_ns
    offset = int(m["latency_offset_ns"])
    t = seg.frames[cfg.time_column].to_numpy().astype(np.int64)
    ticks = seg.frames["tick_ns"].to_numpy().astype(np.int64)
    ws, we = windows(t, offset, tick)
    actions, stats = compute_actions(seg.inputs, ws, we, spec)
    focused, games = focus_flags(seg.focus, t, ws, we, m["game_id"], cfg)
    idle = idle_flags(seg.inputs, actions, ws, we, int(m["t_start_ns"]), int(m["t_end_ns"]), spec, cfg)
    gap_after = np.zeros(len(t), dtype=bool)
    if len(t) > 1:
        gap_after[:-1] = np.diff(ticks) > cfg.gap_factor * tick
    stats.update({
        "frames": int(len(t)),
        "unfocused_frames": int((~focused).sum()),
        "idle_frames": int(idle.sum()),
        "tick_gaps": int(gap_after.sum()),
        "ticks_missing": int(np.maximum(np.round(np.diff(ticks) / tick) - 1, 0).sum()) if len(t) > 1 else 0,
    })
    return Alignment(actions=actions, win_start=ws, win_end=we, focused=focused, idle=idle,
                     gap_after=gap_after, game_id=games, stats=stats)
