"""Segment folder format: arrow schemas, manifest, loading and validation (steps 1-2).

See SCHEMA.md for the exact on-disk contract shared with the Rust writer.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path

import blake3
import numpy as np
import polars as pl
import pyarrow as pa
import pyarrow.parquet as pq

SCHEMA_VERSION = 1

VIDEO = "video.mp4"
FRAMES = "frames.parquet"
INPUTS = "inputs.parquet"
FOCUS = "focus.parquet"
MANIFEST = "manifest.json"
DATA_FILES = (VIDEO, FRAMES, INPUTS, FOCUS)

# Exact arrow schemas the Rust writer produces (non-nullable everywhere).
FRAMES_SCHEMA = pa.schema([
    pa.field("frame_idx", pa.uint32(), nullable=False),
    pa.field("tick_ns", pa.int64(), nullable=False),
    pa.field("capture_ns", pa.int64(), nullable=False),
    pa.field("repeated", pa.bool_(), nullable=False),
])
INPUTS_SCHEMA = pa.schema([
    pa.field("t_ns", pa.int64(), nullable=False),
    pa.field("device", pa.utf8(), nullable=False),
    pa.field("kind", pa.utf8(), nullable=False),
    pa.field("code", pa.uint32(), nullable=False),
    pa.field("value", pa.float32(), nullable=False),
])
FOCUS_SCHEMA = pa.schema([
    pa.field("t_ns", pa.int64(), nullable=False),
    pa.field("focused", pa.bool_(), nullable=False),
    pa.field("game_id", pa.utf8(), nullable=False),
])
SCHEMAS = {FRAMES: FRAMES_SCHEMA, INPUTS: INPUTS_SCHEMA, FOCUS: FOCUS_SCHEMA}

DEVICES = ("keyboard", "mouse", "gamepad")
KINDS = ("key_down", "key_up", "mouse_move", "mouse_button", "wheel", "axis", "button")

MANIFEST_FIELDS = (
    "schema_version", "session_id", "segment_idx", "client_version", "os", "gpu",
    "encoder", "encoder_params", "width", "height", "rate_hz", "game_id",
    "t_start_ns", "t_end_ns", "frame_count", "dropped_frames", "latency_offset_ns",
    "blake3", "sizes",
)


def segment_dir_name(idx: int) -> str:
    return f"seg_{idx:06d}"


def segment_id(manifest: dict) -> str:
    """Globally unique segment id: `<session_id>/seg_<nnnnnn>`."""
    return f"{manifest['session_id']}/{segment_dir_name(int(manifest['segment_idx']))}"


def blake3_file(path: Path) -> str:
    h = blake3.blake3()
    with open(path, "rb") as f:
        while chunk := f.read(4 << 20):
            h.update(chunk)
    return h.hexdigest()


class ValidationError(Exception):
    def __init__(self, reasons: list[str]):
        super().__init__("; ".join(reasons))
        self.reasons = reasons


@dataclass
class Segment:
    """A loaded, validated segment."""
    path: Path
    manifest: dict
    frames: pl.DataFrame
    inputs: pl.DataFrame
    focus: pl.DataFrame
    video_frame_count: int
    warnings: list[str] = field(default_factory=list)

    @property
    def id(self) -> str:
        return segment_id(self.manifest)

    @property
    def tick_ns(self) -> int:
        return 1_000_000_000 // int(self.manifest["rate_hz"])


def read_table(path: Path, name: str, warnings: list[str]) -> pl.DataFrame:
    """Read a parquet file and coerce it to the canonical schema.

    Exact type mismatches that are losslessly castable (e.g. large_utf8,
    dictionary strings, int32 codes) are accepted with a warning so we notice
    writer drift without throwing data away; missing columns are errors.
    """
    want = SCHEMAS[name]
    tbl = pq.read_table(path)
    missing = [f.name for f in want if f.name not in tbl.column_names]
    if missing:
        raise ValidationError([f"{name}: missing columns {missing}"])
    tbl = tbl.select([f.name for f in want])
    for f in want:
        got = tbl.schema.field(f.name).type
        if got != f.type:
            warnings.append(f"{name}.{f.name}: type {got} != {f.type} (cast)")
        if tbl.column(f.name).null_count:
            raise ValidationError([f"{name}.{f.name}: {tbl.column(f.name).null_count} nulls"])
    try:
        tbl = tbl.cast(pa.schema([pa.field(f.name, f.type) for f in want]))
    except (pa.ArrowInvalid, pa.ArrowNotImplementedError) as e:
        raise ValidationError([f"{name}: cannot cast to canonical schema: {e}"]) from e
    return pl.from_arrow(tbl)


def probe_video(path: Path) -> tuple[int, list[int], dict]:
    """Count video frames (packets; no B-frames so 1 packet = 1 frame) and keyframe indices."""
    import av

    keyframes: list[int] = []
    n = 0
    with av.open(str(path)) as c:
        vs = c.streams.video[0]
        info = {"codec": vs.codec_context.name, "width": vs.codec_context.width,
                "height": vs.codec_context.height}
        for pkt in c.demux(vs):
            if pkt.size == 0:
                continue
            if pkt.is_keyframe:
                keyframes.append(n)
            n += 1
    return n, keyframes, info


@dataclass
class ValidationConfig:
    max_dropped_ratio: float = 0.005
    check_hashes: bool = True
    expected_schema_version: int = SCHEMA_VERSION


def load_manifest(path: Path) -> dict:
    m = json.loads((path / MANIFEST).read_text())
    missing = [k for k in MANIFEST_FIELDS if k not in m]
    if missing:
        raise ValidationError([f"manifest missing fields {missing}"])
    return m


def validate_and_load(path: Path, cfg: ValidationConfig | None = None) -> Segment:
    """Step 2: verify hashes, sizes, drop ratio and frame counts; load tables.

    Raises ValidationError listing every problem found.
    """
    cfg = cfg or ValidationConfig()
    path = Path(path)
    m = load_manifest(path)
    reasons: list[str] = []
    warnings: list[str] = []

    if int(m["schema_version"]) != cfg.expected_schema_version:
        reasons.append(f"schema_version {m['schema_version']} != {cfg.expected_schema_version}")

    for f in DATA_FILES:
        p = path / f
        if not p.exists():
            reasons.append(f"{f}: missing")
            continue
        want_size = m["sizes"].get(f)
        size = p.stat().st_size
        if want_size is None:
            reasons.append(f"{f}: no size in manifest")
        elif int(want_size) != size:
            reasons.append(f"{f}: size {size} != manifest {want_size}")
        if cfg.check_hashes:
            want = m["blake3"].get(f)
            if want is None:
                reasons.append(f"{f}: no blake3 in manifest")
            elif (got := blake3_file(p)) != want.lower():
                reasons.append(f"{f}: blake3 {got[:16]}... != manifest {want[:16]}...")
    if reasons:
        raise ValidationError(reasons)

    frame_count = int(m["frame_count"])
    dropped = int(m["dropped_frames"])
    ratio = dropped / max(frame_count, 1)
    if ratio > cfg.max_dropped_ratio:
        reasons.append(f"dropped_frames {dropped}/{frame_count} = {ratio:.4%} > {cfg.max_dropped_ratio:.4%}")

    frames = read_table(path / FRAMES, FRAMES, warnings)
    inputs = read_table(path / INPUTS, INPUTS, warnings)
    focus = read_table(path / FOCUS, FOCUS, warnings)

    n_video, _kf, vinfo = probe_video(path / VIDEO)
    if frames.height != frame_count:
        reasons.append(f"frames.parquet rows {frames.height} != manifest.frame_count {frame_count}")
    if n_video != frame_count:
        reasons.append(f"video frames {n_video} != manifest.frame_count {frame_count}")
    if (vinfo["width"], vinfo["height"]) != (int(m["width"]), int(m["height"])):
        warnings.append(f"video {vinfo['width']}x{vinfo['height']} != manifest {m['width']}x{m['height']}")

    if frames.height:
        idx = frames["frame_idx"].to_numpy()
        if not np.array_equal(idx, np.arange(frames.height, dtype=idx.dtype)):
            reasons.append("frames.frame_idx is not 0..n-1 in order")
        cap = frames["capture_ns"].to_numpy()
        tick = frames["tick_ns"].to_numpy()
        if np.any(np.diff(tick) <= 0):
            reasons.append("frames.tick_ns not strictly increasing")
        if np.any(np.diff(cap) < 0):
            reasons.append("frames.capture_ns decreases")
        if np.any(cap > tick):
            warnings.append("some capture_ns > tick_ns (capture after its tick?)")

    bad_dev = set(inputs["device"].unique().to_list()) - set(DEVICES)
    bad_kind = set(inputs["kind"].unique().to_list()) - set(KINDS)
    if bad_dev:
        warnings.append(f"unknown devices {sorted(bad_dev)} (ignored)")
    if bad_kind:
        warnings.append(f"unknown kinds {sorted(bad_kind)} (ignored)")

    if reasons:
        raise ValidationError(reasons)

    inputs = inputs.with_row_index("_seq").sort(["t_ns", "_seq"]).drop("_seq")
    focus = focus.with_row_index("_seq").sort(["t_ns", "_seq"]).drop("_seq")
    return Segment(path=path, manifest=m, frames=frames, inputs=inputs, focus=focus,
                   video_frame_count=n_video, warnings=warnings)
