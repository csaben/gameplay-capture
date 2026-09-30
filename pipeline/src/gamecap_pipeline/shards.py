"""Steps 7-8: WebDataset shard writing, sidecars, upload, and deletion rebuilds.

Layout under the output store:
  shards/<dataset_version>/action_spec.json
  shards/<dataset_version>/<run_id>-000000.tar
  shards/<dataset_version>/<run_id>-000000.tar.sources.json   (sidecar)
  shards/<dataset_version>/_processed/<session_id>__seg_nnnnnn.json (markers)

Sidecar: {"shard", "dataset_version", "num_samples", "size", "sources": [
            {"segment_id", "source_key", "clips", "keys"}], "action_spec": {...}}
"""

from __future__ import annotations

import io
import json
import logging
import os
import tarfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

import numpy as np
import webdataset as wds

from .action_spec import ActionSpec, default_spec
from .store import Store

log = logging.getLogger(__name__)

SIDECAR_SUFFIX = ".sources.json"


def shard_prefix(dataset_version: str) -> str:
    return f"shards/{dataset_version}"


def marker_key(dataset_version: str, segment_id: str) -> str:
    return f"{shard_prefix(dataset_version)}/_processed/{segment_id.replace('/', '__')}.json"


def npy_bytes(arr: np.ndarray) -> bytes:
    b = io.BytesIO()
    np.save(b, arr, allow_pickle=False)
    return b.getvalue()


def write_action_spec(store: Store, dataset_version: str, spec: ActionSpec) -> None:
    key = f"{shard_prefix(dataset_version)}/action_spec.json"
    obj = spec.to_json_obj() | {"sha": spec.sha}
    if store.exists(key):
        cur = json.loads(store.get_bytes(key))
        if cur.get("sha") != spec.sha:
            raise RuntimeError(f"{key} exists with a different action spec (sha {cur.get('sha')} != {spec.sha}); "
                               "use a new dataset_version")
        return
    store.put_bytes(key, json.dumps(obj, indent=1).encode())


@dataclass
class Sample:
    key: str
    mp4: bytes
    actions: np.ndarray
    meta: dict
    segment_id: str
    source_key: str


@dataclass
class ShardSink:
    """Streams samples into ~maxsize tar shards; each closed shard gets a sidecar
    and is uploaded.  Calls `on_segment_committed(segment_id)` once every shard
    holding clips of a finished segment has been uploaded."""
    store: Store
    dataset_version: str
    workdir: Path
    maxsize: int = 1_000_000_000
    maxcount: int = 100_000
    keep_local: bool = False
    spec: ActionSpec = field(default_factory=default_spec)
    # unique per run so incremental runs never overwrite each other's shards
    run_id: str = field(default_factory=lambda: time.strftime("%Y%m%dT%H%M%S") + "-" + os.urandom(3).hex())
    on_segment_committed: Callable[[str], None] | None = None

    def __post_init__(self):
        self.local_dir = Path(self.workdir) / "shards" / self.dataset_version
        self.local_dir.mkdir(parents=True, exist_ok=True)
        self._writer: wds.ShardWriter | None = None
        self._sources: dict[str, dict[str, dict]] = {}  # fname -> segment_id -> info
        self._counts: dict[str, int] = {}
        self._seg_shards: dict[str, set[str]] = {}
        self._seg_done: set[str] = set()
        self._finalized: set[str] = set()
        self._committed: set[str] = set()
        self.shards_written: list[dict] = []

    def _open(self):
        pattern = str(self.local_dir / f"{self.run_id}-%06d.tar")
        self._writer = wds.ShardWriter(pattern, maxsize=self.maxsize, maxcount=self.maxcount,
                                       post=self._post, verbose=0)

    def write(self, s: Sample) -> None:
        if self._writer is None:
            self._open()
        self._writer.write({
            "__key__": s.key,
            "mp4": s.mp4,
            "actions.npy": npy_bytes(s.actions.astype(np.float32)),
            "json": json.dumps(s.meta).encode(),
        })
        fname = self._writer.fname
        src = self._sources.setdefault(fname, {}).setdefault(
            s.segment_id, {"segment_id": s.segment_id, "source_key": s.source_key, "clips": 0, "keys": []})
        src["clips"] += 1
        src["keys"].append(s.key)
        self._counts[fname] = self._counts.get(fname, 0) + 1
        self._seg_shards.setdefault(s.segment_id, set()).add(fname)

    def segment_done(self, segment_id: str) -> None:
        self._seg_done.add(segment_id)
        self._commit_ready()

    def _commit_ready(self):
        for sid in list(self._seg_done - self._committed):
            if self._seg_shards.get(sid, set()) <= self._finalized:
                self._committed.add(sid)
                if self.on_segment_committed:
                    self.on_segment_committed(sid)

    def _post(self, fname: str) -> None:
        path = Path(fname)
        name = path.name
        key = f"{shard_prefix(self.dataset_version)}/{name}"
        sidecar = {
            "shard": name,
            "dataset_version": self.dataset_version,
            "num_samples": self._counts.get(fname, 0),
            "size": path.stat().st_size,
            "sources": sorted(self._sources.get(fname, {}).values(), key=lambda d: d["segment_id"]),
            "action_spec": self.spec.to_json_obj() | {"sha": self.spec.sha},
        }
        side_bytes = json.dumps(sidecar, indent=1).encode()
        (path.parent / (name + SIDECAR_SUFFIX)).write_bytes(side_bytes)
        # shard first, sidecar second: a sidecar implies its shard is complete
        self.store.put_file(path, key)
        self.store.put_bytes(key + SIDECAR_SUFFIX, side_bytes)
        if not self.keep_local and self.store.local_path(key) != path:
            path.unlink(missing_ok=True)
            (path.parent / (name + SIDECAR_SUFFIX)).unlink(missing_ok=True)
        self.shards_written.append({"key": key, "size": sidecar["size"], "samples": sidecar["num_samples"]})
        log.info("shard %s: %d samples, %.1f MB", key, sidecar["num_samples"], sidecar["size"] / 1e6)
        self._finalized.add(fname)
        self._commit_ready()

    def close(self) -> None:
        if self._writer is not None:
            self._writer.close()  # triggers _post for the last shard
            self._writer = None
        self._commit_ready()


# ---------------------------------------------------------------------------
# deletions
# ---------------------------------------------------------------------------

def list_sidecars(store: Store, dataset_version: str) -> list[str]:
    pre = shard_prefix(dataset_version) + "/"
    return sorted(o.key for o in store.list(pre) if o.key.endswith(".tar" + SIDECAR_SUFFIX))


def rebuild_affected(store: Store, dataset_version: str, deletions, workdir: Path,
                     dry_run: bool = False) -> list[dict]:
    """Rewrite (or remove) every shard whose sidecar lists a deleted segment.

    Filtering the existing tar is enough: the remaining samples are copied
    byte-for-byte, so no raw data is needed (it may already be gone).
    """
    actions = []
    workdir = Path(workdir)
    workdir.mkdir(parents=True, exist_ok=True)
    for sk in list_sidecars(store, dataset_version):
        side = json.loads(store.get_bytes(sk))
        hit = [s for s in side["sources"] if deletions.matches(s["segment_id"], s.get("source_key", ""))]
        if not hit:
            continue
        shard_key = sk[: -len(SIDECAR_SUFFIX)]
        drop_keys = {k for s in hit for k in s["keys"]}
        keep_sources = [s for s in side["sources"] if s not in hit]
        rec = {"shard": shard_key, "deleted_segments": [s["segment_id"] for s in hit],
               "dropped_samples": len(drop_keys),
               "remaining_samples": side["num_samples"] - len(drop_keys)}
        if dry_run:
            rec["action"] = "would-" + ("remove" if not keep_sources else "rewrite")
            actions.append(rec)
            continue
        if not keep_sources:
            store.delete(sk)  # sidecar first so a crash never leaves an unindexed shard with deleted data
            store.delete(shard_key)
            rec["action"] = "removed"
        else:
            src = workdir / Path(shard_key).name
            dst = workdir / (Path(shard_key).name + ".new")
            store.download(shard_key, src)
            n_out = 0
            with tarfile.open(src) as tin, tarfile.open(dst, "w") as tout:
                for m in tin:
                    base = m.name.split("/")[-1]
                    k = base.split(".", 1)[0]
                    if k in drop_keys:
                        continue
                    tout.addfile(m, tin.extractfile(m) if m.isfile() else None)
                    if base.endswith(".mp4"):
                        n_out += 1
            side["sources"] = keep_sources
            side["num_samples"] = n_out
            side["size"] = dst.stat().st_size
            side["rebuilt_at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
            store.delete(sk)
            store.put_file(dst, shard_key)
            store.put_bytes(sk, json.dumps(side, indent=1).encode())
            src.unlink(missing_ok=True)
            dst.unlink(missing_ok=True)
            rec["action"] = "rewritten"
        for seg in rec["deleted_segments"]:
            mk = marker_key(dataset_version, seg)
            store.put_bytes(mk, json.dumps({"segment_id": seg, "status": "deleted",
                                            "at": time.strftime("%Y-%m-%dT%H:%M:%S")}).encode())
        actions.append(rec)
    return actions
