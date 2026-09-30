"""Validation failure cases (and the happy path)."""

from __future__ import annotations

import json

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from gamecap_pipeline.segment import (FRAMES_SCHEMA, ValidationConfig, ValidationError, blake3_file,
                                      validate_and_load)


def rewrite_manifest(seg, **fields):
    m = json.loads((seg / "manifest.json").read_text())
    m.update(fields)
    (seg / "manifest.json").write_text(json.dumps(m))
    return m


def refresh_hash(seg, name):
    m = json.loads((seg / "manifest.json").read_text())
    m["blake3"][name] = blake3_file(seg / name)
    m["sizes"][name] = (seg / name).stat().st_size
    (seg / "manifest.json").write_text(json.dumps(m))


def test_valid_segment(seg0):
    seg = validate_and_load(seg0)
    assert seg.frames.height == seg.manifest["frame_count"] == seg.video_frame_count
    assert seg.warnings == []
    assert seg.id == "sess-test/seg_000000"


def test_bad_hash(seg_copy):
    p = seg_copy / "video.mp4"
    b = bytearray(p.read_bytes())
    b[len(b) // 2] ^= 0xFF  # same size, different content
    p.write_bytes(bytes(b))
    with pytest.raises(ValidationError) as ei:
        validate_and_load(seg_copy)
    assert any("video.mp4: blake3" in r for r in ei.value.reasons)


def test_bad_size(seg_copy):
    with open(seg_copy / "inputs.parquet", "ab") as f:
        f.write(b"x")
    with pytest.raises(ValidationError) as ei:
        validate_and_load(seg_copy)
    assert any("inputs.parquet: size" in r for r in ei.value.reasons)


def test_frames_parquet_row_count_mismatch(seg_copy):
    tbl = pq.read_table(seg_copy / "frames.parquet")
    pq.write_table(tbl.slice(0, tbl.num_rows - 1), seg_copy / "frames.parquet")
    refresh_hash(seg_copy, "frames.parquet")
    with pytest.raises(ValidationError) as ei:
        validate_and_load(seg_copy)
    assert any("frames.parquet rows" in r for r in ei.value.reasons)


def test_manifest_frame_count_mismatch(seg_copy):
    m = json.loads((seg_copy / "manifest.json").read_text())
    rewrite_manifest(seg_copy, frame_count=m["frame_count"] + 5)
    with pytest.raises(ValidationError) as ei:
        validate_and_load(seg_copy)
    r = " ".join(ei.value.reasons)
    assert "video frames" in r and "frames.parquet rows" in r


def test_too_many_dropped_frames(seg_copy):
    rewrite_manifest(seg_copy, dropped_frames=50)  # 50/1198 = 4%
    with pytest.raises(ValidationError) as ei:
        validate_and_load(seg_copy)
    assert any("dropped_frames" in r for r in ei.value.reasons)
    validate_and_load(seg_copy, ValidationConfig(max_dropped_ratio=0.05))  # threshold is configurable


def test_missing_file_and_field(seg_copy):
    (seg_copy / "focus.parquet").unlink()
    with pytest.raises(ValidationError, match="focus.parquet: missing"):
        validate_and_load(seg_copy)
    m = json.loads((seg_copy / "manifest.json").read_text())
    del m["latency_offset_ns"]
    (seg_copy / "manifest.json").write_text(json.dumps(m))
    with pytest.raises(ValidationError, match="manifest missing fields"):
        validate_and_load(seg_copy)


def test_type_drift_is_cast_with_warning(seg_copy):
    tbl = pq.read_table(seg_copy / "inputs.parquet")
    tbl = tbl.set_column(1, "device", tbl.column("device").cast(pa.large_utf8()))
    pq.write_table(tbl, seg_copy / "inputs.parquet")
    refresh_hash(seg_copy, "inputs.parquet")
    seg = validate_and_load(seg_copy)
    assert any("inputs.parquet.device" in w for w in seg.warnings)


def test_decreasing_capture_ns(seg_copy):
    tbl = pq.read_table(seg_copy / "frames.parquet")
    cap = tbl.column("capture_ns").to_pylist()
    cap[10], cap[11] = cap[11] + 1, cap[10]
    tbl = tbl.set_column(2, FRAMES_SCHEMA.field("capture_ns"), pa.array(cap, pa.int64()))
    pq.write_table(tbl, seg_copy / "frames.parquet")
    refresh_hash(seg_copy, "frames.parquet")
    with pytest.raises(ValidationError, match="capture_ns decreases"):
        validate_and_load(seg_copy)


def test_parquet_schema_is_canonical(seg0):
    """The synth writer must produce exactly the schema documented in SCHEMA.md."""
    from gamecap_pipeline.segment import FOCUS_SCHEMA, INPUTS_SCHEMA
    for name, want in [("frames.parquet", FRAMES_SCHEMA), ("inputs.parquet", INPUTS_SCHEMA),
                       ("focus.parquet", FOCUS_SCHEMA)]:
        got = pq.read_schema(seg0 / name)
        assert got.remove_metadata().equals(want, check_metadata=False), (name, got, want)
