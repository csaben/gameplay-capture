"""Synthetic, spec-exact segment generator (`gamecap-pipeline synth`).

Produces segment folders exactly as the Rust recorder will: real HEVC
fragmented MP4 (hevc_nvenc if it works, else libx265; GOP 20, no B-frames,
frag_keyframe+empty_moov), frames/inputs/focus parquet with the canonical
arrow schemas, and manifest.json (written last) with blake3 hashes and sizes.

Each video frame shows its tick number, a square that moves with the scripted
mouse, and a "truth" line: the expected action row for that frame computed by
the slow reference implementation.  `gamecap-pipeline replay` draws the
pipeline's aligned actions below it, so a replay can be checked by eye.

Scripted timeline (seconds from segment start; see `script_events`):
  1.0-3.0 W held (with 33 ms auto-repeat), 2.0-2.5 LShift, 3.5-5.0 A taps,
  E down/up exactly on window boundaries of frames 100 and 104,
  6.0-7.0 Up arrow (E0 48), 6.2-6.4 RCtrl (E0 1D), 6.5 Pause tap,
  8-20 mouse circles at 125 Hz, 12-13 LMB, 14.0-14.2 RMB, wheel at 15-16.5,
  21-30 gamepad left stick sine + RT ramp + South/Start taps,
  30-45 nothing (idle > 10 s), 45-47 D + mouse, 48-51 UNFOCUSED (no events),
  52-58 W + mouse jitter, 59.0 S down (released in the next segment).
Dropped ticks default to 1100 and 1101 (2 frames, 0.17%).
"""

from __future__ import annotations

import json
import math
import os
import shutil
import subprocess
import uuid
from dataclasses import dataclass, field
from functools import lru_cache
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq
from PIL import Image, ImageDraw, ImageFont

from .action_spec import default_spec, KEY_NAME_TO_CODE
from .reference import reference_actions
from .segment import (FOCUS, FOCUS_SCHEMA, FRAMES, FRAMES_SCHEMA, INPUTS, INPUTS_SCHEMA,
                      MANIFEST, SCHEMA_VERSION, VIDEO, blake3_file, segment_dir_name)

SEC = 1_000_000_000
Event = tuple[int, str, str, int, float]


@dataclass
class SynthConfig:
    out: Path
    session_id: str = field(default_factory=lambda: str(uuid.uuid4()))
    segments: int = 1
    first_segment_idx: int = 0
    seconds: int = 60
    rate_hz: int = 20
    width: int = 640
    height: int = 360
    gop: int = 20
    qp: int = 19
    latency_offset_ns: int = 0
    game_id: str = "synthgame.exe"
    t0_ns: int = 5_000 * SEC
    drop_ticks: tuple[int, ...] = (1100, 1101)
    repeat_prob: float = 0.08
    seed: int = 0
    layout: str = "sessions"  # "sessions" -> sessions/<sid>/seg_n ; "raw" -> raw/<user>/<sid>/seg_n
    user_id: str = "synth-user"
    encoder: str | None = None  # force hevc_nvenc / libx265
    unfocused: tuple[float, float] | None = (48.0, 51.0)


@lru_cache(maxsize=1)
def pick_encoder() -> str:
    forced = os.environ.get("GAMECAP_SYNTH_ENCODER")
    if forced:
        return forced
    try:
        r = subprocess.run(
            ["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
             "color=s=640x360:r=20:d=0.2", "-c:v", "hevc_nvenc", "-f", "null", "-"],
            capture_output=True, timeout=30)
        if r.returncode == 0:
            return "hevc_nvenc"
    except (OSError, subprocess.TimeoutExpired):
        pass
    return "libx265"


def _encoder_args(enc: str, gop: int, qp: int) -> tuple[list[str], dict[str, str]]:
    if enc == "hevc_nvenc":
        args = ["-c:v", "hevc_nvenc", "-preset", "p4", "-rc", "constqp", "-qp", str(qp),
                "-g", str(gop), "-bf", "0", "-strict_gop", "1"]
    else:
        args = ["-c:v", "libx265", "-preset", "fast",
                "-x265-params", f"keyint={gop}:min-keyint={gop}:bframes=0:scenecut=0:qp={qp}:log-level=error"]
    params = {"rc": "constqp", "qp": str(qp), "gop": str(gop), "bf": "0",
              "movflags": "frag_keyframe+empty_moov"}
    return args, params


# ---------------------------------------------------------------------------
# timeline + scripted input
# ---------------------------------------------------------------------------

def make_timeline(cfg: SynthConfig, t_start: int, rng: np.random.Generator):
    tick = SEC // cfg.rate_hz
    n_ticks = cfg.seconds * cfg.rate_hz
    tick_idx = np.array([k for k in range(n_ticks) if k not in set(cfg.drop_ticks)], dtype=np.int64)
    tick_ns = t_start + tick_idx * tick
    capture = np.empty_like(tick_ns)
    repeated = np.zeros(len(tick_ns), dtype=bool)
    for i, tn in enumerate(tick_ns):
        if i > 0 and rng.random() < cfg.repeat_prob:
            capture[i] = capture[i - 1]
            repeated[i] = True
        else:
            c = tn - int(rng.integers(3_000_000, 25_000_000))
            capture[i] = max(c, capture[i - 1] if i else c)
    return tick_idx, tick_ns, capture, repeated


def script_events(t_start: int, ws: np.ndarray, cfg: SynthConfig, seg_idx: int) -> list[Event]:
    ev: list[Event] = []
    S = lambda s: t_start + int(round(s * SEC))
    K = KEY_NAME_TO_CODE

    def hold(key: str, a: float, b: float, repeat_ms: int | None = None):
        code = K[key]
        ev.append((S(a), "keyboard", "key_down", code, 1.0))
        if repeat_ms:
            t = a + 0.5
            while t < b:
                ev.append((S(t), "keyboard", "key_down", code, 1.0))
                t += repeat_ms / 1000
        ev.append((S(b), "keyboard", "key_up", code, 0.0))

    if seg_idx > cfg.first_segment_idx:
        ev.append((S(0.5), "keyboard", "key_up", K["s"], 0.0))  # S held over the boundary
    hold("w", 1.0, 3.0, repeat_ms=33)
    hold("lshift", 2.0, 2.5)
    t = 3.5
    while t < 5.0:
        hold("a", t, t + 0.12)
        t += 0.3
    # exactly on window boundaries
    if len(ws) > 300:
        ev.append((int(ws[100]), "keyboard", "key_down", K["e"], 1.0))
        ev.append((int(ws[104]), "keyboard", "key_up", K["e"], 0.0))
        ev.append((int(ws[300]), "mouse", "mouse_move", 0, 7.0))
    hold("up", 6.0, 7.0)
    hold("rctrl", 6.2, 6.4)
    hold("pause", 6.5, 6.6)
    ev.append((S(6.7), "keyboard", "key_down", 0x1FF, 1.0))  # unknown code: ignored
    ev.append((S(6.75), "keyboard", "key_up", 0x1FF, 0.0))
    # mouse circles at 125 Hz
    t = 8.0
    while t < 20.0:
        dx = round(6 * math.cos(2 * math.pi * t / 4))
        dy = round(4 * math.sin(2 * math.pi * t / 4))
        if dx:
            ev.append((S(t), "mouse", "mouse_move", 0, float(dx)))
        if dy:
            ev.append((S(t), "mouse", "mouse_move", 1, float(dy)))
        t += 0.008
    ev += [(S(12.0), "mouse", "mouse_button", 0, 1.0), (S(13.0), "mouse", "mouse_button", 0, 0.0),
           (S(14.0), "mouse", "mouse_button", 1, 1.0), (S(14.2), "mouse", "mouse_button", 1, 0.0)]
    ev += [(S(x), "mouse", "wheel", 0, 1.0) for x in (15.0, 15.1, 15.2)]
    ev += [(S(16.0), "mouse", "wheel", 0, -1.0), (S(16.5), "mouse", "wheel", 1, 0.5)]
    # gamepad: polled at 250 Hz, logged on change > 0.02
    last = {0: 0.0, 5: 0.0}
    t = 21.0
    while t < 30.0:
        vals = {0: round(math.sin(2 * math.pi * (t - 21.0) / 3.0), 3),
                5: round(min(max((t - 24.0) / 2.0, 0.0), 1.0), 3) if t < 26.5 else 0.0}
        for code, v in vals.items():
            if abs(v - last[code]) > 0.02:
                ev.append((S(t), "gamepad", "axis", code, float(v)))
                last[code] = v
        t += 0.004
    for code in (0, 5):
        if last[code] != 0.0:
            ev.append((S(30.0), "gamepad", "axis", code, 0.0))
    ev += [(S(22.0), "gamepad", "button", 0, 1.0), (S(22.2), "gamepad", "button", 0, 0.0),
           (S(23.0), "gamepad", "button", 0, 1.0), (S(23.3), "gamepad", "button", 0, 0.0),
           (S(28.0), "gamepad", "button", 11, 1.0), (S(28.4), "gamepad", "button", 11, 0.0)]
    # 30-45 idle
    hold("d", 45.0, 47.0)
    t = 45.0
    while t < 47.0:
        ev.append((S(t), "mouse", "mouse_move", 0, 2.0))
        t += 0.01
    # 48-51 unfocused -> no events at all (the recorder gates them)
    hold("w", 52.0, 58.0, repeat_ms=33)
    t = 52.0
    rng = np.random.default_rng(cfg.seed + seg_idx)
    while t < 58.0:
        ev.append((S(t), "mouse", "mouse_move", int(rng.integers(0, 2)), float(rng.integers(-3, 4) or 1)))
        t += 0.016
    ev.append((S(59.0), "keyboard", "key_down", K["s"], 1.0))
    if cfg.unfocused:
        a, b = S(cfg.unfocused[0]), S(cfg.unfocused[1])
        ev = [e for e in ev if not (a <= e[0] < b)]
    end = t_start + cfg.seconds * SEC
    ev = [e for e in ev if t_start <= e[0] < end]
    ev.sort(key=lambda e: e[0])
    return ev


def focus_rows(t_start: int, cfg: SynthConfig) -> list[tuple[int, bool, str]]:
    rows = [(t_start, True, cfg.game_id)]
    if cfg.unfocused:
        rows.append((t_start + int(cfg.unfocused[0] * SEC), False, ""))
        rows.append((t_start + int(cfg.unfocused[1] * SEC), True, cfg.game_id))
    return rows


# ---------------------------------------------------------------------------
# rendering
# ---------------------------------------------------------------------------

def _font(size: int):
    try:
        return ImageFont.load_default(size=size)
    except TypeError:  # very old Pillow
        return ImageFont.load_default()


def truth_text(row: np.ndarray) -> str:
    d = default_spec().describe_row(row)
    parts = ["T:" + ("+".join(d["keys"]) or "-")]
    parts.append(f"m{d['mouse_dx']:+.0f},{d['mouse_dy']:+.0f}")
    if d["mouse_buttons"]:
        parts.append("mb:" + "+".join(d["mouse_buttons"]))
    if d["wheel_v"] or d["wheel_h"]:
        parts.append(f"wh{d['wheel_v']:+.1f},{d['wheel_h']:+.1f}")
    ax = d["pad_axes"]
    if abs(ax["left_x"]) > 0.01 or abs(ax["right_z"]) > 0.01:
        parts.append(f"lx{ax['left_x']:+.2f} rt{ax['right_z']:.2f}")
    if d["pad_buttons"]:
        parts.append("pb:" + "+".join(d["pad_buttons"]))
    return " ".join(parts)


def render_frames(cfg: SynthConfig, tick_idx: np.ndarray, truth: np.ndarray, unfocused: np.ndarray):
    spec = default_spec()
    big, small = _font(64), _font(18)
    dx = truth[:, spec.index("mouse.dx")]
    dy = truth[:, spec.index("mouse.dy")]
    # square position after the previous frame's window
    px = np.concatenate([[0.0], np.cumsum(dx)[:-1]])
    py = np.concatenate([[0.0], np.cumsum(dy)[:-1]])
    for i, k in enumerate(tick_idx):
        bg = (80, 30, 30) if unfocused[i] else (32, 48, 80)
        im = Image.new("RGB", (cfg.width, cfg.height), bg)
        dr = ImageDraw.Draw(im)
        x = int(cfg.width / 2 + px[i] * 0.5) % (cfg.width - 40)
        y = int(cfg.height / 2 + py[i] * 0.5) % (cfg.height - 80)
        dr.rectangle([x, y, x + 40, y + 40], fill=(250, 220, 40))
        dr.text((16, 8), f"{int(k)}", font=big, fill=(255, 255, 255))
        dr.text((cfg.width - 150, 16), f"frame {i}", font=small, fill=(200, 200, 200))
        dr.text((8, cfg.height - 28), truth_text(truth[i])[:70], font=small, fill=(120, 255, 120))
        yield im.tobytes()


def encode_video(path: Path, frames_iter, cfg: SynthConfig) -> tuple[str, dict[str, str]]:
    enc = cfg.encoder or pick_encoder()
    args, params = _encoder_args(enc, cfg.gop, cfg.qp)
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
           "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", f"{cfg.width}x{cfg.height}",
           "-r", str(cfg.rate_hz), "-i", "-", *args, "-pix_fmt", "yuv420p",
           "-movflags", "frag_keyframe+empty_moov", "-f", "mp4", str(path)]
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        for fb in frames_iter:
            p.stdin.write(fb)
        p.stdin.close()
        err = p.stderr.read().decode(errors="replace")
        if p.wait() != 0:
            raise RuntimeError(f"ffmpeg failed ({enc}): {err}")
    finally:
        if p.poll() is None:
            p.kill()
    return enc, params


# ---------------------------------------------------------------------------
# parquet + manifest
# ---------------------------------------------------------------------------

def write_parquet(path: Path, schema: pa.Schema, cols: dict) -> None:
    tbl = pa.Table.from_pydict(cols, schema=schema)
    pq.write_table(tbl, path, compression="zstd")


def write_segment(cfg: SynthConfig, seg_idx: int) -> Path:
    rng = np.random.default_rng(cfg.seed * 1000 + seg_idx)
    tick = SEC // cfg.rate_hz
    t_start = cfg.t0_ns + (seg_idx - cfg.first_segment_idx) * cfg.seconds * SEC
    t_end = t_start + cfg.seconds * SEC
    tick_idx, tick_ns, capture, repeated = make_timeline(cfg, t_start, rng)
    ws = capture + cfg.latency_offset_ns
    events = script_events(t_start, ws, cfg, seg_idx)
    truth = reference_actions(events, capture, cfg.latency_offset_ns, tick)
    fr = focus_rows(t_start, cfg)
    ft = np.array([r[0] for r in fr])
    fv = np.array([r[1] for r in fr])
    unfocused = ~fv[np.searchsorted(ft, capture, side="right") - 1]

    if cfg.layout == "raw":
        parent = cfg.out / "raw" / cfg.user_id / cfg.session_id
    else:
        parent = cfg.out / "sessions" / cfg.session_id
    final = parent / segment_dir_name(seg_idx)
    part = parent / (segment_dir_name(seg_idx) + ".partial")
    if part.exists():
        shutil.rmtree(part)
    if final.exists():
        shutil.rmtree(final)
    part.mkdir(parents=True)

    enc, params = encode_video(part / VIDEO, render_frames(cfg, tick_idx, truth, unfocused), cfg)
    n = len(tick_ns)
    write_parquet(part / FRAMES, FRAMES_SCHEMA, {
        "frame_idx": np.arange(n, dtype=np.uint32), "tick_ns": tick_ns, "capture_ns": capture,
        "repeated": repeated})
    write_parquet(part / INPUTS, INPUTS_SCHEMA, {
        "t_ns": [e[0] for e in events], "device": [e[1] for e in events],
        "kind": [e[2] for e in events], "code": [e[3] for e in events],
        "value": [e[4] for e in events]})
    write_parquet(part / FOCUS, FOCUS_SCHEMA, {
        "t_ns": [r[0] for r in fr], "focused": [r[1] for r in fr], "game_id": [r[2] for r in fr]})

    files = (VIDEO, FRAMES, INPUTS, FOCUS)
    manifest = {
        "schema_version": SCHEMA_VERSION,
        "session_id": cfg.session_id,
        "segment_idx": seg_idx,
        "client_version": "gamecap-synth/0.1.0",
        "os": "linux-synthetic",
        "gpu": "synthetic",
        "encoder": enc,
        "encoder_params": dict(sorted(params.items())),
        "width": cfg.width,
        "height": cfg.height,
        "rate_hz": cfg.rate_hz,
        "game_id": cfg.game_id,
        "t_start_ns": t_start,
        "t_end_ns": t_end,
        "frame_count": n,
        "dropped_frames": len(cfg.drop_ticks),
        "latency_offset_ns": cfg.latency_offset_ns,
        "blake3": {f: blake3_file(part / f) for f in sorted(files)},
        "sizes": {f: (part / f).stat().st_size for f in sorted(files)},
    }
    (part / MANIFEST).write_text(json.dumps(manifest, indent=2))
    part.rename(final)
    return final


def synth(cfg: SynthConfig) -> list[Path]:
    cfg.out = Path(cfg.out)
    return [write_segment(cfg, cfg.first_segment_idx + i) for i in range(cfg.segments)]
