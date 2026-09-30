"""Orchestration of the 8 pipeline steps for a batch of segments."""

from __future__ import annotations

import json
import logging
import multiprocessing
import shutil
import tempfile
import time
from collections import deque
from concurrent.futures import ProcessPoolExecutor
from dataclasses import asdict, dataclass, field
from pathlib import Path

from .action_spec import default_spec
from .align import AlignConfig, align_segment
from .clips import ClipConfig, plan_clips, read_packets, reencode_clip, remux_clip
from .deletions import Deletions
from .segment import DATA_FILES, MANIFEST, ValidationConfig, ValidationError, segment_id, validate_and_load
from .shards import Sample, ShardSink, marker_key, shard_prefix, write_action_spec
from .store import Store, open_store

log = logging.getLogger(__name__)


@dataclass
class ProcessConfig:
    input_url: str
    output_url: str
    dataset_version: str = "v0"
    workdir: Path = Path(".gamecap-work")
    endpoint_url: str | None = None
    workers: int = 4
    limit: int | None = None
    shard_maxsize: int = 1_000_000_000
    keep_local: bool = False
    retry_rejected: bool = False
    deletions: list[str] = field(default_factory=list)
    validation: ValidationConfig = field(default_factory=ValidationConfig)
    align: AlignConfig = field(default_factory=AlignConfig)
    clips: ClipConfig = field(default_factory=ClipConfig)


@dataclass
class SegmentRef:
    key: str          # segment folder key relative to the input store root
    path_id: str      # "<session_id>/seg_nnnnnn" derived from the path


def list_ready_segments(store: Store) -> list[SegmentRef]:
    """Step 1: a segment is ready once its manifest.json exists."""
    refs = []
    for o in store.list(""):
        if not o.key.endswith("/" + MANIFEST) and o.key != MANIFEST:
            continue
        seg_key = o.key[: -len(MANIFEST)].rstrip("/")
        parts = seg_key.split("/")
        if any(p.endswith(".partial") for p in parts):
            continue
        path_id = "/".join(parts[-2:]) if len(parts) >= 2 else seg_key
        refs.append(SegmentRef(seg_key, path_id))
    refs.sort(key=lambda r: r.key)
    return refs


def processed_markers(store: Store, dataset_version: str) -> dict[str, dict]:
    pre = f"{shard_prefix(dataset_version)}/_processed/"
    out = {}
    for o in store.list(pre):
        if o.key.endswith(".json"):
            sid = o.key[len(pre):-5].replace("__", "/")
            out[sid] = {"key": o.key}
    return out


def clip_key(manifest: dict, start: int) -> str:
    sess = "".join(c if c.isalnum() or c in "-_" else "-" for c in str(manifest["session_id"]))
    return f"{sess}_s{int(manifest['segment_idx']):06d}_f{start:05d}"


def _materialize(store: Store, seg_key: str, tmp: Path) -> Path:
    lp = store.local_path(seg_key)
    if lp is not None:
        return lp
    d = tmp / "seg"
    for f in (*DATA_FILES, MANIFEST):
        store.download(f"{seg_key}/{f}", d / f)
    return d


def process_one(cfg: ProcessConfig, ref: SegmentRef) -> dict:
    """Steps 2-6 for one segment.  Runs in a worker process."""
    t0 = time.time()
    store = open_store(cfg.input_url, cfg.endpoint_url)
    spec = default_spec()
    tmp = Path(tempfile.mkdtemp(prefix="gamecap-seg-", dir=cfg.workdir))
    try:
        path = _materialize(store, ref.key, tmp)
        try:
            seg = validate_and_load(path, cfg.validation)
        except ValidationError as e:
            return {"ref": asdict(ref), "status": "rejected", "reasons": e.reasons, "segment_id": ref.path_id}
        m = seg.manifest
        sid = segment_id(m)
        al = align_segment(seg, cfg.align, spec)
        vp = read_packets(path / "video.mp4")
        usable = al.focused & ~al.gap_after
        # the very last frame's "gap" is the segment end, which is fine
        starts = plan_clips(len(vp.packets), usable, vp.keyframes, cfg.clips, al.idle)
        L = cfg.clips.clip_len
        tick_ns = seg.frames["tick_ns"].to_numpy()
        cap_ns = seg.frames["capture_ns"].to_numpy()
        rep = seg.frames["repeated"].to_numpy()
        kfs = set(int(k) for k in vp.keyframes)
        samples = []
        for s in starts:
            on_kf = s in kfs
            mp4 = remux_clip(vp, s, L) if on_kf else reencode_clip(vp, s, L, int(m["rate_hz"]), cfg.clips)
            sl = slice(s, s + L)
            idle_n = int(al.idle[sl].sum())
            key = clip_key(m, s)
            meta = {
                "key": key,
                "dataset_version": cfg.dataset_version,
                "game_id": al.game_id[s],
                "session_id": m["session_id"],
                "segment_idx": int(m["segment_idx"]),
                "segment_id": sid,
                "source_key": ref.key,
                "start_frame": s,
                "num_frames": L,
                "rate_hz": int(m["rate_hz"]),
                "width": int(m["width"]),
                "height": int(m["height"]),
                "t_start_ns": int(cap_ns[s]),
                "t_end_ns": int(al.win_end[s + L - 1]),
                "latency_offset_ns": int(m["latency_offset_ns"]),
                "time_column": cfg.align.time_column,
                "tick_ns": tick_ns[sl].tolist(),
                "capture_ns": cap_ns[sl].tolist(),
                "repeated": rep[sl].tolist(),
                "idle": idle_n > 0,
                "idle_frames": idle_n,
                "reencoded": not on_kf,
                "encoder": m["encoder"],
                "client_version": m["client_version"],
                "os": m["os"],
                "action_spec": spec.ref(),
            }
            samples.append(Sample(key=key, mp4=mp4, actions=al.actions[sl].copy(), meta=meta,
                                  segment_id=sid, source_key=ref.key))
        return {"ref": asdict(ref), "status": "ok", "segment_id": sid, "samples": samples,
                "stats": al.stats | {"clips": len(samples), "seconds": time.time() - t0,
                                     "video_seconds": seg.frames.height / int(m["rate_hz"])},
                "warnings": seg.warnings}
    except Exception as e:  # noqa: BLE001 - one bad segment must not kill the batch
        log.exception("segment %s failed", ref.key)
        return {"ref": asdict(ref), "status": "error", "segment_id": ref.path_id, "reasons": [repr(e)]}
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def _put_marker(store: Store, cfg: ProcessConfig, sid: str, obj: dict) -> None:
    obj = {"segment_id": sid, "dataset_version": cfg.dataset_version,
           "at": time.strftime("%Y-%m-%dT%H:%M:%S")} | obj
    store.put_bytes(marker_key(cfg.dataset_version, sid), json.dumps(obj, indent=1).encode())


def run(cfg: ProcessConfig) -> dict:
    t_start = time.time()
    cfg.workdir = Path(cfg.workdir).resolve()
    cfg.workdir.mkdir(parents=True, exist_ok=True)
    src = open_store(cfg.input_url, cfg.endpoint_url)
    dst = open_store(cfg.output_url, cfg.endpoint_url)
    spec = default_spec()
    write_action_spec(dst, cfg.dataset_version, spec)
    deletions = Deletions.load(cfg.deletions, cfg.endpoint_url)

    refs = list_ready_segments(src)
    markers = processed_markers(dst, cfg.dataset_version)
    summary = {"found": len(refs), "skipped_processed": 0, "skipped_deleted": 0, "rejected": 0,
               "errors": 0, "processed": 0, "clips": 0, "idle_clips": 0, "reencoded_clips": 0,
               "video_seconds": 0.0, "clip_bytes": 0, "shards": [], "rejections": []}
    todo = []
    for r in refs:
        if deletions.matches(r.path_id, r.key):
            summary["skipped_deleted"] += 1
            continue
        mk = markers.get(r.path_id)
        if mk is not None:
            if cfg.retry_rejected:
                st = json.loads(dst.get_bytes(mk["key"])).get("status")
                if st in ("rejected", "error"):
                    todo.append(r)
                    continue
            summary["skipped_processed"] += 1
            continue
        todo.append(r)
    if cfg.limit is not None:
        todo = todo[: cfg.limit]
    log.info("%d ready segments, %d to process", len(refs), len(todo))

    pending_ok: dict[str, dict] = {}

    def committed(sid: str) -> None:
        info = pending_ok.pop(sid, None)
        if info is not None:
            _put_marker(dst, cfg, sid, info)

    sink = ShardSink(store=dst, dataset_version=cfg.dataset_version, workdir=cfg.workdir,
                     maxsize=cfg.shard_maxsize, keep_local=cfg.keep_local, spec=spec,
                     on_segment_committed=committed)

    def handle(res: dict) -> None:
        sid = res["segment_id"]
        if res["status"] != "ok":
            summary["rejected" if res["status"] == "rejected" else "errors"] += 1
            summary["rejections"].append({"segment": res["ref"]["key"], "reasons": res["reasons"]})
            _put_marker(dst, cfg, sid, {"status": res["status"], "reasons": res["reasons"],
                                        "source_key": res["ref"]["key"]})
            log.warning("segment %s %s: %s", res["ref"]["key"], res["status"], "; ".join(res["reasons"]))
            return
        if sid != res["ref"]["path_id"]:
            log.warning("segment path %s does not match manifest id %s", res["ref"]["key"], sid)
        samples: list[Sample] = res["samples"]
        pending_ok[sid] = {"status": "ok", "source_key": res["ref"]["key"], "clips": len(samples),
                           "stats": res["stats"], "warnings": res["warnings"],
                           "keys": [s.key for s in samples]}
        for s in samples:
            sink.write(s)
            summary["clip_bytes"] += len(s.mp4)
            summary["idle_clips"] += int(s.meta["idle"])
            summary["reencoded_clips"] += int(s.meta["reencoded"])
        summary["processed"] += 1
        summary["clips"] += len(samples)
        summary["video_seconds"] += res["stats"]["video_seconds"]
        sink.segment_done(sid)

    try:
        if cfg.workers <= 1:
            for r in todo:
                handle(process_one(cfg, r))
        else:
            # bounded in-flight window keeps memory flat for large backlogs
            # spawn, not fork: polars/arrow thread pools deadlock in forked children
            ctx = multiprocessing.get_context("spawn")
            with ProcessPoolExecutor(max_workers=cfg.workers, mp_context=ctx) as ex:
                inflight: deque = deque()
                for r in todo:
                    inflight.append(ex.submit(process_one, cfg, r))
                    if len(inflight) >= 2 * cfg.workers:
                        handle(inflight.popleft().result())
                while inflight:
                    handle(inflight.popleft().result())
    finally:
        sink.close()
    summary["shards"] = sink.shards_written
    summary["wall_seconds"] = round(time.time() - t_start, 3)
    if summary["wall_seconds"] > 0:
        summary["realtime_factor"] = round(summary["video_seconds"] / summary["wall_seconds"], 1)
    return summary
