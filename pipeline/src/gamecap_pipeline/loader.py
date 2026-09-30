"""Training-side loader example: WebDataset shards -> (frames uint8 [T,3,H,W], actions [T,D]).

Needs the `train` extra (torch + torchcodec):  uv sync --extra train

Design for the 2x3090 box:
  * DataLoader worker *processes* (CPU) stream tar shards from local disk or
    S3/R2 (`pipe:` URLs), split samples and parse `actions.npy` / json.  They
    never touch CUDA (CUDA in forked workers is fragile).
  * The main process decodes the compressed mp4 bytes with torchcodec on
    NVDEC (`decode_device`, default cuda:0) - HEVC decode is nearly free there
    and only compressed bytes cross PCIe.
  * The model trains on the other GPU (`train_device`, e.g. cuda:1); frames
    hop GPU->GPU.  Alternatively run DDP on both GPUs, each decoding its own
    clips on its own NVDEC.
  * If CUDA or NVDEC is unavailable, decoding falls back to torchcodec on CPU.

Example:
    from gamecap_pipeline.loader import shard_urls, iterate_clips
    urls = shard_urls("s3://gamecap/shards/v1")          # or a local directory
    for frames, actions, meta in iterate_clips(urls, decode_device="cuda:0"):
        frames = frames.to("cuda:1", non_blocking=True)  # [T,3,H,W] uint8
        ...
"""

from __future__ import annotations

import glob
import io
import json
import logging
import os
import sys

import numpy as np
import webdataset as wds

log = logging.getLogger(__name__)


def shard_urls(location: str, endpoint_url: str | None = None) -> list[str]:
    """All shard URLs under a local dir or s3:// prefix (e.g. .../shards/<dataset_version>)."""
    if location.startswith("s3://"):
        from .store import open_store

        st = open_store(location, endpoint_url)
        keys = sorted(o.key for o in st.list("") if o.key.endswith(".tar"))
        py = sys.executable
        return [f"pipe:{py} -m gamecap_pipeline.s3cat {st.uri(k)}" for k in keys]
    return sorted(glob.glob(os.path.join(location, "*.tar")))


def _to_sample(s: dict) -> dict:
    return {
        "key": s["__key__"],
        "mp4": s["mp4"],
        "actions": np.load(io.BytesIO(s["actions.npy"])),
        "meta": json.loads(s["json"]),
    }


def raw_dataset(urls: list[str], shuffle_shards: bool = False, shuffle_buffer: int = 0,
                resampled: bool = False) -> wds.WebDataset:
    """Samples with the mp4 still compressed (cheap to pass between processes)."""
    ds = wds.WebDataset(urls, shardshuffle=100 if shuffle_shards else False, resampled=resampled,
                        nodesplitter=wds.split_by_node, workersplitter=wds.split_by_worker,
                        empty_check=False)
    if shuffle_buffer:
        ds = ds.shuffle(shuffle_buffer)
    return ds.map(_to_sample)


class ClipDecoder:
    """torchcodec decode of an in-memory mp4, NVDEC with CPU fallback."""

    def __init__(self, device: str = "cuda:0"):
        import torch

        self.device = device
        if device.startswith("cuda") and not torch.cuda.is_available():
            log.warning("CUDA not available; decoding on CPU")
            self.device = "cpu"
        self.fell_back = False

    def __call__(self, mp4: bytes, num_frames: int | None = None):
        from torchcodec.decoders import VideoDecoder

        try:
            dec = VideoDecoder(mp4, device=self.device, seek_mode="exact")
        except Exception as e:  # noqa: BLE001 - NVDEC init failure, OOM, missing libs...
            if self.device == "cpu":
                raise
            log.warning("GPU decode failed (%s); falling back to CPU", e)
            self.device, self.fell_back = "cpu", True
            dec = VideoDecoder(mp4, device="cpu", seek_mode="exact")
        n = dec.metadata.num_frames if num_frames is None else num_frames
        return dec.get_frames_in_range(0, n).data  # uint8 [T,3,H,W] on self.device


def iterate_clips(urls: list[str], decode_device: str = "cuda:0", num_workers: int = 4,
                  shuffle_shards: bool = False, shuffle_buffer: int = 0,
                  multiprocessing_context: str | None = None):
    """Yield (frames uint8 [T,3,H,W] on decode_device, actions float32 [T,D] tensor, meta).

    `multiprocessing_context="forkserver"` avoids forking a multi-threaded parent
    (slower worker start-up); the default (fork on Linux) is fine in a plain
    training script because workers only parse tars and never touch CUDA.
    """
    import torch
    from torch.utils.data import DataLoader

    ds = raw_dataset(urls, shuffle_shards=shuffle_shards, shuffle_buffer=shuffle_buffer)
    dl = DataLoader(ds, batch_size=None, num_workers=num_workers,
                    persistent_workers=False, prefetch_factor=4 if num_workers else None,
                    multiprocessing_context=multiprocessing_context if num_workers else None)
    decode = ClipDecoder(decode_device)
    for s in dl:
        frames = decode(s["mp4"], num_frames=s["meta"]["num_frames"])
        actions = torch.as_tensor(s["actions"])
        if frames.shape[0] != actions.shape[0]:
            raise RuntimeError(f"{s['key']}: {frames.shape[0]} frames vs {actions.shape[0]} action rows")
        yield frames, actions, s["meta"]


def main(argv: list[str] | None = None) -> int:
    """`gamecap-pipeline load LOCATION` - smoke test / throughput check."""
    import argparse
    import time

    import torch

    ap = argparse.ArgumentParser(prog="gamecap-pipeline load")
    ap.add_argument("location", help="local shard dir or s3://bucket/shards/<version>")
    ap.add_argument("--device", default="cuda:0")
    ap.add_argument("--workers", type=int, default=2)
    ap.add_argument("--limit", type=int, default=None)
    a = ap.parse_args(argv)
    urls = shard_urls(a.location)
    print(f"{len(urls)} shards")
    t0, n, frames_total = time.time(), 0, 0
    for frames, actions, meta in iterate_clips(urls, a.device, a.workers):
        if n == 0:
            print("first:", meta["key"], tuple(frames.shape), frames.dtype, frames.device,
                  tuple(actions.shape), actions.dtype)
        n += 1
        frames_total += frames.shape[0]
        if a.limit and n >= a.limit:
            break
    if torch.cuda.is_available():
        torch.cuda.synchronize()
    dt = time.time() - t0
    print(f"{n} clips, {frames_total} frames in {dt:.2f}s ({frames_total / max(dt, 1e-9):.0f} frames/s)")
    return 0
