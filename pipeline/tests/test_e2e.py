"""End to end: synth -> process -> shards -> loader, plus idempotence and deletions."""

from __future__ import annotations

import json
import shutil
import tarfile
from pathlib import Path

import numpy as np
import pytest

from gamecap_pipeline.action_spec import default_spec
from gamecap_pipeline.deletions import Deletions
from gamecap_pipeline.loader import raw_dataset, shard_urls
from gamecap_pipeline.process import ProcessConfig, run
from gamecap_pipeline.reference import reference_actions
from gamecap_pipeline.segment import validate_and_load
from gamecap_pipeline.shards import rebuild_affected
from gamecap_pipeline.store import LocalStore

SPEC = default_spec()
SEC = 1_000_000_000


def all_samples(shard_dir: Path):
    urls = shard_urls(str(shard_dir))
    return list(raw_dataset(urls))


def test_summary_and_layout(processed):
    s = processed["summary"]
    assert s["found"] == 2 and s["processed"] == 2 and s["rejected"] == 0 and s["errors"] == 0
    assert s["clips"] >= 20 and s["idle_clips"] >= 4
    d = processed["shard_dir"]
    assert (d / "action_spec.json").exists()
    spec_json = json.loads((d / "action_spec.json").read_text())
    assert spec_json["dim"] == SPEC.dim and spec_json["names"] == list(SPEC.names)
    tars = sorted(d.glob("*.tar"))
    assert tars
    for t in tars:
        side = json.loads(Path(str(t) + ".sources.json").read_text())
        assert {x["segment_id"] for x in side["sources"]} <= {"sess-test/seg_000000", "sess-test/seg_000001"}
        assert side["action_spec"]["sha"] == SPEC.sha
    markers = sorted((d / "_processed").glob("*.json"))
    assert [m.name for m in markers] == ["sess-test__seg_000000.json", "sess-test__seg_000001.json"]
    assert all(json.loads(m.read_text())["status"] == "ok" for m in markers)


def test_actions_match_reference_and_script(processed, synth_root):
    samples = all_samples(processed["shard_dir"])
    assert len(samples) == processed["summary"]["clips"]
    segs = {}
    for seg_idx in (0, 1):
        seg = validate_and_load(synth_root / "sessions" / "sess-test" / f"seg_{seg_idx:06d}")
        ref = reference_actions(list(seg.inputs.iter_rows()), seg.frames["capture_ns"].to_numpy(),
                                seg.manifest["latency_offset_ns"], seg.tick_ns)
        segs[seg.id] = (seg, ref)
    kw, ke = SPEC.index("key.w"), SPEC.index("key.e")
    seen_e = seen_w = seen_mouse = seen_pad = False
    for smp in samples:
        a, meta = smp["actions"], smp["meta"]
        assert a.shape == (64, SPEC.dim) and a.dtype == np.float32
        seg, ref = segs[meta["segment_id"]]
        s = meta["start_frame"]
        np.testing.assert_allclose(a, ref[s:s + 64], atol=1e-4)
        assert meta["capture_ns"] == seg.frames["capture_ns"][s:s + 64].to_list()
        # scripted facts
        seg_ws = seg.frames["capture_ns"].to_numpy() + meta["latency_offset_ns"]
        seg_we = np.append(seg_ws[1:], seg_ws[-1] + seg.tick_ns)
        ws, we = seg_ws[s:s + 64], seg_we[s:s + 64]
        rel_end = (we - seg.manifest["t_start_ns"]) / SEC
        in_w = (rel_end > 1.0 + 1e-6) & (rel_end <= 3.0)       # W held 1.0-3.0 s
        if in_w.any():
            seen_w = True
            assert (a[in_w, kw] == 1).all()
        frames = np.arange(s, s + 64)
        if (frames == 100).any():                              # E down/up exactly on W100 / W104 starts
            seen_e = True
            held = frames[a[:, ke] == 1]
            assert held.tolist() == [100, 101, 102, 103]
        # (a repeated frame's predecessor has an empty window -> no motion there)
        in_mouse = (rel_end > 8.1) & (rel_end < 19.9) & (we - ws > 20_000_000)
        if in_mouse.any():
            seen_mouse = True
            assert (np.abs(a[in_mouse, SPEC.index("mouse.dx")]) + np.abs(a[in_mouse, SPEC.index("mouse.dy")]) > 0).all()
        if ((rel_end > 21.5) & (rel_end < 29.5)).any():
            seen_pad = True
            assert np.abs(a[:, SPEC.index("pad.axis.left_x")]).max() > 0.5
        # idle clips are those overlapping 30..45 s
        rel_cap = (np.array(meta["capture_ns"]) - seg.manifest["t_start_ns"]) / SEC
        assert meta["idle"] == bool(((rel_cap > 30.5) & (rel_cap < 44.5)).any())
        # no clip includes unfocused (48..51 s) frames
        assert not ((rel_cap > 48.0) & (rel_cap < 51.0)).any()
    assert seen_w and seen_e and seen_mouse and seen_pad


def test_second_run_is_noop(processed):
    cfg = processed["cfg"]
    s = run(ProcessConfig(**{**cfg.__dict__, "workers": 1}))
    assert s["skipped_processed"] == 2 and s["processed"] == 0 and s["shards"] == []


@pytest.mark.gpu
def test_loader_gpu_or_cpu(processed):
    torch = pytest.importorskip("torch")
    pytest.importorskip("torchcodec")
    from gamecap_pipeline.loader import iterate_clips

    dev = "cuda:0" if torch.cuda.is_available() else "cpu"
    urls = shard_urls(str(processed["shard_dir"]))
    n = 0
    for frames, actions, meta in iterate_clips(urls, decode_device=dev, num_workers=2):
        assert tuple(frames.shape) == (64, 3, 360, 640) and frames.dtype == torch.uint8
        assert frames.device.type == dev.split(":")[0]
        assert tuple(actions.shape) == (64, SPEC.dim) and actions.dtype == torch.float32
        n += 1
    assert n == processed["summary"]["clips"]


@pytest.mark.gpu
def test_loader_cpu_fallback_decodes_same_pixels(processed):
    torch = pytest.importorskip("torch")
    pytest.importorskip("torchcodec")
    from gamecap_pipeline.loader import ClipDecoder

    smp = all_samples(processed["shard_dir"])[0]
    cpu = ClipDecoder("cpu")(smp["mp4"])
    assert tuple(cpu.shape) == (64, 3, 360, 640)
    if torch.cuda.is_available():
        gpu = ClipDecoder("cuda:0")(smp["mp4"]).cpu()
        # NVDEC and libavcodec colour conversion differ slightly; content must agree
        assert (gpu.float() - cpu.float()).abs().mean() < 3.0


def test_small_shards_and_deletions(synth_root, tmp_path):
    out, work = tmp_path / "out", tmp_path / "work"
    cfg = ProcessConfig(input_url=str(synth_root / "sessions"), output_url=str(out), dataset_version="vdel",
                        workdir=work, workers=1, shard_maxsize=1_000_000)
    s = run(cfg)
    d = out / "shards" / "vdel"
    tars = sorted(d.glob("*.tar"))
    assert len(tars) >= 3, "1 MB shards should roll over"
    before = {x["key"] for x in raw_dataset(shard_urls(str(d)))}
    assert len(before) == s["clips"]

    # delete segment 0 -> rebuild
    dl = tmp_path / "deletions.txt"
    dl.write_text("# user request 123\nsess-test/seg_000000\n")
    dels = Deletions.load([str(dl)])
    res = rebuild_affected(LocalStore(out), "vdel", dels, work / "rb")
    assert res and {r["action"] for r in res} <= {"removed", "rewritten"}
    after = list(raw_dataset(shard_urls(str(d))))
    assert after and all(x["meta"]["segment_id"] == "sess-test/seg_000001" for x in after)
    assert {x["key"] for x in after} == {k for k in before if "_s000001_" in k}
    for t in sorted(d.glob("*.tar")):
        side = json.loads(Path(str(t) + ".sources.json").read_text())
        assert [x["segment_id"] for x in side["sources"]] == ["sess-test/seg_000001"]
        with tarfile.open(t) as tf:
            assert sum(m.name.endswith(".mp4") for m in tf) == side["num_samples"]
    assert json.loads((d / "_processed" / "sess-test__seg_000000.json").read_text())["status"] == "deleted"
    # idempotent
    assert rebuild_affected(LocalStore(out), "vdel", dels, work / "rb") == []

    # fresh dataset version honouring the deletion list skips the segment entirely
    s2 = run(ProcessConfig(input_url=str(synth_root / "sessions"), output_url=str(out), dataset_version="vdel2",
                           workdir=work, workers=1, deletions=[str(dl)]))
    assert s2["skipped_deleted"] == 1 and s2["processed"] == 1


def test_rejected_segment_is_marked_not_sharded(synth_root, tmp_path):
    src = tmp_path / "in" / "sess-test"
    shutil.copytree(synth_root / "sessions" / "sess-test" / "seg_000001", src / "seg_000001")
    with open(src / "seg_000001" / "video.mp4", "ab") as f:
        f.write(b"\0")
    s = run(ProcessConfig(input_url=str(tmp_path / "in"), output_url=str(tmp_path / "out"), dataset_version="vr",
                          workdir=tmp_path / "w", workers=1))
    assert s["rejected"] == 1 and s["clips"] == 0
    mk = json.loads((tmp_path / "out/shards/vr/_processed/sess-test__seg_000001.json").read_text())
    assert mk["status"] == "rejected" and any("video.mp4: size" in r for r in mk["reasons"])
    # partial folders are never picked up
    shutil.copytree(src / "seg_000001", src / "seg_000002.partial")
    s = run(ProcessConfig(input_url=str(tmp_path / "in"), output_url=str(tmp_path / "out"), dataset_version="vr",
                          workdir=tmp_path / "w", workers=1))
    assert s["found"] == 1 and s["skipped_processed"] == 1


def test_replay_renders(processed, tmp_path):
    import av
    from gamecap_pipeline.replay import replay_segment, replay_shard_sample

    tar = sorted(processed["shard_dir"].glob("*.tar"))[0]
    out = tmp_path / "r.mp4"
    res = replay_shard_sample(tar, out, index=2)
    assert res["frames"] == 64
    with av.open(str(out)) as c:
        n = sum(1 for _ in c.decode(video=0))
        assert n == 64 and c.streams.video[0].height == 360 * 2 + 170
    seg = processed["cfg"].input_url + "/sess-test/seg_000000"
    res = replay_segment(seg, tmp_path / "s.mp4", start=950, count=40)
    assert res["frames"] == 40
