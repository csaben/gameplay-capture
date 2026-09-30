"""`gamecap-pipeline replay`: render a clip (or a raw segment) with its actions overlaid.

The video is scaled 2x and a panel underneath shows, per frame:
held keys, the mouse delta as an arrow (+ numbers), mouse buttons, wheel,
both gamepad sticks, triggers and pad buttons, plus idle/unfocused flags.
The panel's first line uses the same compact format as the "truth" line the
synthetic generator burns into its frames, so the two can be compared by eye.
"""

from __future__ import annotations

import io
import json
import math
import tarfile
from fractions import Fraction
from pathlib import Path

import av
import numpy as np
from PIL import Image, ImageDraw, ImageFont

from .action_spec import ActionSpec, default_spec
from .synth import truth_text

PANEL_H = 170
SCALE = 2


def _font(size):
    try:
        return ImageFont.load_default(size=size)
    except TypeError:
        return ImageFont.load_default()


def load_sample_from_tar(tar_path: str | Path, key: str | None = None, index: int = 0):
    """Return (key, mp4 bytes, actions, meta) for one sample of a local shard."""
    groups: dict[str, dict[str, bytes]] = {}
    order: list[str] = []
    with tarfile.open(tar_path) as tf:
        for m in tf:
            if not m.isfile():
                continue
            base = m.name.split("/")[-1]
            k, ext = base.split(".", 1)
            if k not in groups:
                groups[k] = {}
                order.append(k)
            groups[k][ext] = tf.extractfile(m).read()
    if key is None:
        key = order[index]
    if key not in groups:
        raise KeyError(f"{key} not in {tar_path}; keys: {order}")
    g = groups[key]
    return key, g["mp4"], np.load(io.BytesIO(g["actions.npy"])), json.loads(g["json"])


def decode_rgb(mp4: bytes | str | Path) -> list[np.ndarray]:
    src = io.BytesIO(mp4) if isinstance(mp4, (bytes, bytearray)) else str(mp4)
    with av.open(src) as c:
        return [f.to_ndarray(format="rgb24") for f in c.decode(video=0)]


def draw_panel(im: Image.Image, y0: int, row: np.ndarray, spec: ActionSpec, header: str,
               flags: list[str]) -> None:
    W = im.width
    dr = ImageDraw.Draw(im)
    f, fs = _font(22), _font(16)
    dr.rectangle([0, y0, W, y0 + PANEL_H], fill=(12, 12, 16))
    d = spec.describe_row(row)
    dr.text((10, y0 + 6), "P:" + truth_text(row)[2:], font=f, fill=(255, 210, 90))
    dr.text((10, y0 + 36), header + ("   " + " ".join(flags) if flags else ""), font=fs,
            fill=(255, 120, 120) if flags else (170, 170, 170))
    dr.text((10, y0 + 62), "keys: " + (" ".join(d["keys"]) or "-"), font=f, fill=(230, 230, 230))
    dr.text((10, y0 + 92), "mouse btn: " + (" ".join(d["mouse_buttons"]) or "-") +
            f"   wheel v{d['wheel_v']:+.1f} h{d['wheel_h']:+.1f}", font=fs, fill=(200, 200, 200))
    dr.text((10, y0 + 116), "pad btn: " + (" ".join(d["pad_buttons"]) or "-"), font=fs, fill=(200, 200, 200))

    # mouse delta arrow
    cx, cy, r = W - 420, y0 + PANEL_H // 2, 60
    dr.ellipse([cx - r, cy - r, cx + r, cy + r], outline=(90, 90, 90))
    dx, dy = d["mouse_dx"], d["mouse_dy"]
    mag = math.hypot(dx, dy)
    if mag > 0:
        s = min(r, 4 * math.sqrt(mag)) / mag  # sqrt scale so small moves stay visible
        ex, ey = cx + dx * s, cy + dy * s
        dr.line([cx, cy, ex, ey], fill=(80, 200, 255), width=4)
        ang = math.atan2(ey - cy, ex - cx)
        for da in (2.6, -2.6):
            dr.line([ex, ey, ex + 12 * math.cos(ang + da), ey + 12 * math.sin(ang + da)],
                    fill=(80, 200, 255), width=4)
    dr.text((cx - r, cy + r - 2), f"dx{dx:+.0f} dy{dy:+.0f}", font=fs, fill=(80, 200, 255))

    # gamepad sticks + triggers
    ax = d["pad_axes"]
    for i, (sx, sy, label) in enumerate([("left_x", "left_y", "L"), ("right_x", "right_y", "R")]):
        gx, gy, gr = W - 250 + i * 130, y0 + PANEL_H // 2, 50
        dr.ellipse([gx - gr, gy - gr, gx + gr, gy + gr], outline=(90, 90, 90))
        px, py = gx + ax[sx] * gr, gy + ax[sy] * gr
        dr.ellipse([px - 8, py - 8, px + 8, py + 8], fill=(120, 255, 120))
        dr.text((gx - 6, gy - gr - 20), label, font=fs, fill=(170, 170, 170))
        trig = ax["left_z" if i == 0 else "right_z"]
        dr.rectangle([gx - gr, gy + gr + 6, gx - gr + int(2 * gr * max(0.0, min(1.0, trig))), gy + gr + 14],
                     fill=(255, 150, 60))


def render(frames: list[np.ndarray], actions: np.ndarray, out_path: str | Path, rate_hz: int,
           headers: list[str] | None = None, frame_flags: list[list[str]] | None = None,
           spec: ActionSpec | None = None) -> Path:
    spec = spec or default_spec()
    if len(frames) != len(actions):
        raise ValueError(f"{len(frames)} frames but {len(actions)} action rows")
    h, w = frames[0].shape[:2]
    W, H = w * SCALE, h * SCALE + PANEL_H
    out_path = Path(out_path)
    with av.open(str(out_path), "w") as out:
        st = out.add_stream("libx264", rate=rate_hz, options={"crf": "18", "preset": "veryfast"})
        st.width, st.height, st.pix_fmt = W, H, "yuv420p"
        for i, (fr, row) in enumerate(zip(frames, actions)):
            im = Image.new("RGB", (W, H))
            im.paste(Image.fromarray(fr).resize((w * SCALE, h * SCALE), Image.NEAREST), (0, 0))
            draw_panel(im, h * SCALE, row, spec, headers[i] if headers else f"frame {i}",
                       frame_flags[i] if frame_flags else [])
            vf = av.VideoFrame.from_image(im)
            vf.pts = i
            vf.time_base = Fraction(1, rate_hz)
            for p in st.encode(vf):
                out.mux(p)
        for p in st.encode(None):
            out.mux(p)
    return out_path


def replay_shard_sample(tar_path, out_path, key: str | None = None, index: int = 0) -> dict:
    key, mp4, actions, meta = load_sample_from_tar(tar_path, key, index)
    frames = decode_rgb(mp4)
    s = meta.get("start_frame", 0)
    headers = [f"{key}  seg frame {s + i}  clip frame {i}/{len(frames)}" for i in range(len(frames))]
    flags = [["IDLE-CLIP"] if meta.get("idle") else [] for _ in frames]
    render(frames, actions, out_path, int(meta.get("rate_hz", 20)), headers, flags)
    return {"key": key, "frames": len(frames), "actions": list(actions.shape), "out": str(out_path)}


def replay_segment(seg_dir, out_path, start: int = 0, count: int | None = None) -> dict:
    """Align a raw segment folder and render (a range of) it -- no shards needed."""
    from .align import align_segment
    from .segment import validate_and_load

    seg = validate_and_load(Path(seg_dir))
    al = align_segment(seg)
    frames = decode_rgb(Path(seg_dir) / "video.mp4")
    end = len(frames) if count is None else min(len(frames), start + count)
    idx = range(start, end)
    headers = [f"{seg.id}  frame {k}" for k in idx]
    flags = [[n for n, on in (("UNFOCUSED", not al.focused[k]), ("IDLE", al.idle[k]),
                              ("GAP-AFTER", al.gap_after[k])) if on] for k in idx]
    render([frames[k] for k in idx], al.actions[start:end], out_path, int(seg.manifest["rate_hz"]),
           headers, flags)
    return {"segment": seg.id, "frames": end - start, "out": str(out_path)}
