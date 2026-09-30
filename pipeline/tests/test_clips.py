"""Clip planning, keyframe alignment and remux correctness."""

from __future__ import annotations

import io

import av
import numpy as np

from gamecap_pipeline.align import align_segment
from gamecap_pipeline.clips import ClipConfig, plan_clips, read_packets, reencode_clip, remux_clip
from gamecap_pipeline.segment import validate_and_load


def test_plan_clips_unit():
    n = 400
    kf = np.arange(0, n, 20)
    usable = np.ones(n, bool)
    assert plan_clips(n, usable, kf, ClipConfig()) == [0, 80, 160, 240, 320]
    usable[70] = False   # between clips 0..63 and 80..143: harmless
    usable[100] = False  # kills 80; next start after 100 on a keyframe is 120
    assert plan_clips(n, usable, kf, ClipConfig()) == [0, 120, 200, 280]
    assert plan_clips(n, np.ones(n, bool), kf, ClipConfig(stride=40)) == [0, 40, 80, 120, 160, 200, 240, 280, 320]
    assert plan_clips(50, np.ones(50, bool), kf[:3], ClipConfig()) == []
    u = np.ones(n, bool)
    u[100] = False
    assert plan_clips(n, u, kf, ClipConfig(reencode_mid_gop=True)) == [0, 101, 165, 229, 293]
    idle = np.zeros(n, bool)
    idle[90] = True
    assert plan_clips(n, np.ones(n, bool), kf, ClipConfig(drop_idle=True), idle) == [0, 100, 180, 260]


def decode(b: bytes):
    with av.open(io.BytesIO(b)) as c:
        return [(f.key_frame, f.to_ndarray(format="gray")) for f in c.decode(video=0)]


def test_segment_clips_decode_exactly_and_start_on_keyframes(seg0):
    seg = validate_and_load(seg0)
    al = align_segment(seg)
    vp = read_packets(seg0 / "video.mp4")
    assert vp.keyframes.tolist() == list(range(0, len(vp.packets), 20))
    usable = al.focused & ~al.gap_after
    starts = plan_clips(len(vp.packets), usable, vp.keyframes, ClipConfig(), al.idle)
    assert len(starts) >= 10
    with av.open(str(seg0 / "video.mp4")) as c:
        full = [f.to_ndarray(format="gray") for f in c.decode(video=0)]
    for s in starts:
        assert s % 20 == 0
        assert usable[s:s + 64].all()           # never spans unfocused / dropped frames
        frames = decode(remux_clip(vp, s, 64))
        assert len(frames) == 64
        assert frames[0][0]                      # first decoded frame is a keyframe
        # bit-exact remux: decoded pixels identical to the source segment
        assert np.array_equal(frames[0][1], full[s]) and np.array_equal(frames[-1][1], full[s + 63])


def test_reencode_mid_gop(seg0):
    vp = read_packets(seg0 / "video.mp4")
    b = reencode_clip(vp, 105, 64, 20, ClipConfig(reencode_mid_gop=True))
    frames = decode(b)
    assert len(frames) == 64 and frames[0][0]
    with av.open(str(seg0 / "video.mp4")) as c:
        src = [f.to_ndarray(format="gray") for i, f in zip(range(170), c.decode(video=0))]
    err = np.abs(frames[0][1].astype(int) - src[105].astype(int)).mean()
    other = np.abs(frames[0][1].astype(int) - src[104].astype(int)).mean()
    assert err < 2.0 and err < other  # it is frame 105, not a neighbour


def test_processed_clips_meta(processed):
    """Every clip in the shards: 64 decodable frames, keyframe start, no unfocused frames."""
    import tarfile, json
    shards = sorted(processed["shard_dir"].glob("*.tar"))
    n = 0
    for sh in shards:
        with tarfile.open(sh) as tf:
            for m in tf:
                if m.name.endswith(".json"):
                    meta = json.loads(tf.extractfile(m).read())
                    assert meta["start_frame"] % 20 == 0 and not meta["reencoded"]
                    assert len(meta["capture_ns"]) == 64
                    n += 1
                elif m.name.endswith(".mp4"):
                    fr = decode(tf.extractfile(m).read())
                    assert len(fr) == 64 and fr[0][0]
    assert n == processed["summary"]["clips"]
