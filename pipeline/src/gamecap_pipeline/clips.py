"""Step 6: plan clips on keyframes and cut them out of the segment video.

Clips are remuxed (packets copied, no re-encode) into a plain MP4.  A clip
never spans an unfocused frame or a dropped-frame gap.  With
`reencode_mid_gop=True` clips may also start mid-GOP; those (only those) are
decoded from the preceding keyframe and re-encoded.
"""

from __future__ import annotations

import io
from dataclasses import dataclass
from fractions import Fraction
from pathlib import Path

import av
import numpy as np


@dataclass
class ClipConfig:
    clip_len: int = 64
    # Minimum distance between consecutive clip starts (default = clip_len, no overlap).
    stride: int | None = None
    reencode_mid_gop: bool = False
    reencode_codec: str = "libx265"  # or hevc_nvenc
    reencode_qp: int = 19
    drop_idle: bool = False


@dataclass
class RawPacket:
    data: bytes
    pts: int | None
    dts: int | None
    duration: int | None
    keyframe: bool


@dataclass
class VideoPackets:
    packets: list[RawPacket]
    time_base: Fraction
    keyframes: np.ndarray  # frame indices
    path: Path


def read_packets(path: Path) -> VideoPackets:
    pkts: list[RawPacket] = []
    with av.open(str(path)) as c:
        vs = c.streams.video[0]
        tb = vs.time_base
        for p in c.demux(vs):
            if p.size == 0:
                continue
            pkts.append(RawPacket(bytes(p), p.pts, p.dts, p.duration, bool(p.is_keyframe)))
    kf = np.array([i for i, p in enumerate(pkts) if p.keyframe], dtype=np.int64)
    return VideoPackets(pkts, tb, kf, Path(path))


def plan_clips(n: int, usable: np.ndarray, keyframes: np.ndarray, cfg: ClipConfig,
               idle: np.ndarray | None = None) -> list[int]:
    """Return clip start frames.  A start must be a keyframe (unless re-encoding
    is enabled) and frames [s, s+L) must all be usable."""
    L = cfg.clip_len
    stride = cfg.stride or L
    if n < L:
        return []
    # bad prefix sums for O(1) range checks
    bad = np.concatenate([[0], np.cumsum(~usable.astype(bool))])
    kfset = set(int(k) for k in keyframes)
    starts: list[int] = []
    next_allowed = 0
    s = 0
    while s + L <= n:
        if s < next_allowed:
            s = next_allowed
            continue
        if not cfg.reencode_mid_gop and s not in kfset:
            later = keyframes[keyframes > s]
            if not len(later):
                break
            s = int(later[0])
            continue
        if bad[s + L] - bad[s] == 0 and not (cfg.drop_idle and idle is not None and idle[s:s + L].any()):
            starts.append(s)
            next_allowed = s + stride
            s = next_allowed
        else:
            # jump past the last bad frame in the range
            rng = ~usable[s:s + L].astype(bool)
            if rng.any():
                s = s + int(np.nonzero(rng)[0][-1]) + 1
            else:  # idle-rejected
                s += 1
    return starts


def remux_clip(vp: VideoPackets, start: int, length: int) -> bytes:
    """Copy packets [start, start+length) into a fresh MP4 (start must be a keyframe)."""
    if not vp.packets[start].keyframe:
        raise ValueError(f"clip start {start} is not a keyframe")
    buf = io.BytesIO()
    with av.open(str(vp.path)) as src:
        in_stream = src.streams.video[0]
        with av.open(buf, "w", format="mp4") as out:
            os_ = out.add_stream_from_template(in_stream)
            first = vp.packets[start]
            base_pts = first.pts if first.pts is not None else 0
            base_dts = first.dts if first.dts is not None else base_pts
            for rp in vp.packets[start:start + length]:
                p = av.Packet(rp.data)
                p.pts = None if rp.pts is None else rp.pts - base_pts
                p.dts = None if rp.dts is None else rp.dts - base_dts
                if rp.duration:
                    p.duration = rp.duration
                p.is_keyframe = rp.keyframe
                p.time_base = vp.time_base
                p.stream = os_
                out.mux(p)
    return buf.getvalue()


def reencode_clip(vp: VideoPackets, start: int, length: int, rate_hz: int, cfg: ClipConfig) -> bytes:
    """Decode from the keyframe at/before `start`, re-encode frames [start, start+length)."""
    kf = vp.keyframes[vp.keyframes <= start]
    k0 = int(kf[-1]) if len(kf) else 0
    buf = io.BytesIO()
    with av.open(str(vp.path)) as src:
        in_stream = src.streams.video[0]
        dec = av.CodecContext.create(in_stream.codec_context.name, "r")
        dec.extradata = in_stream.codec_context.extradata
        with av.open(buf, "w", format="mp4") as out:
            opts = ({"x265-params": f"keyint={rate_hz}:min-keyint={rate_hz}:bframes=0:scenecut=0:qp={cfg.reencode_qp}:log-level=error"}
                    if cfg.reencode_codec == "libx265" else
                    {"rc": "constqp", "qp": str(cfg.reencode_qp), "bf": "0", "g": str(rate_hz)})
            ost = out.add_stream(cfg.reencode_codec, rate=rate_hz, options=opts)
            ost.width = in_stream.codec_context.width
            ost.height = in_stream.codec_context.height
            ost.pix_fmt = "yuv420p"
            ost.codec_context.gop_size = rate_hz
            ost.codec_context.max_b_frames = 0
            idx = k0
            written = 0
            for rp in vp.packets[k0:start + length]:
                pkt = av.Packet(rp.data)
                pkt.pts, pkt.dts = rp.pts, rp.dts
                for fr in dec.decode(pkt):
                    if idx >= start and written < length:
                        fr.pts = written
                        fr.time_base = Fraction(1, rate_hz)
                        fr.pict_type = av.video.frame.PictureType.I if written == 0 else av.video.frame.PictureType.NONE
                        out.mux(ost.encode(fr))
                        written += 1
                    idx += 1
            for fr in dec.decode(None):
                if idx >= start and written < length:
                    fr.pts = written
                    fr.time_base = Fraction(1, rate_hz)
                    out.mux(ost.encode(fr))
                    written += 1
                idx += 1
            out.mux(ost.encode(None))
    if written != length:
        raise RuntimeError(f"re-encode produced {written} frames, wanted {length}")
    return buf.getvalue()
