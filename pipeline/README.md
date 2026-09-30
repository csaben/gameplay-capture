# gamecap-pipeline

This package runs on the Ubuntu 2×3090 box. It turns recorded segments
(`video.mp4` with frames, inputs and focus parquet, plus `manifest.json`) into
training-ready WebDataset shards of short clips with aligned per-frame action
tensors. The spec is in `docs/SPEC.md` under "Processing pipeline". The
on-disk contract with the Rust recorder is in [SCHEMA.md](SCHEMA.md).

```
list ready segments ─► validate ─► align (polars) ─► action vectors ─► focus / idle filter
     (manifest.json)    blake3,      [t_k+off,          292-d float32     unfocused: dropped
                        sizes,        t_{k+1}+off)                        idle > 10 s: flagged
                        counts
  ─► cut 64-frame clips on keyframes (PyAV remux, no re-encode) ─► WebDataset shards (~1 GB)
  ─► shards/<dataset_version>/ on Garage/R2 (or a local dir) + sidecar + processed markers
```

## Install

```bash
cd pipeline
uv sync                  # pipeline only
uv sync --extra train    # + torch 2.14 / torchcodec 0.16 (CUDA 13 wheels, several GB) for loader.py
```

This needs the `ffmpeg` CLI, but only for `synth`. On this box it has
`hevc_nvenc`; otherwise `libx265` is used. PyAV brings its own libav, which is
used for remuxing, re-encoding and replay. torchcodec uses the system FFmpeg
libraries (4–7; Ubuntu 24.04 ships 6.1).

## Commands

```bash
# synthetic, spec-exact segments (real HEVC fMP4 via ffmpeg, scripted inputs)
uv run gamecap-pipeline synth --out /data/synth --session-id s1 --segments 3 --latency-offset-ms -35
#   --layout raw --user-id u1   -> raw/<user>/<session>/seg_n (R2 layout)

# validate folders and print alignment stats
uv run gamecap-pipeline validate /data/synth/sessions/s1/seg_000000

# steps 1-8
uv run gamecap-pipeline process --input /data/synth/sessions --output /data/out --dataset-version v1
uv run gamecap-pipeline process --input s3://gamecap/raw --output s3://gamecap --dataset-version v1 \
    --deletions s3://gamecap/deletions --workers 8

# deletion requests: rewrite or remove every shard whose sidecar lists a deleted segment
uv run gamecap-pipeline rebuild-affected --output s3://gamecap --dataset-version v1 \
    --deletions deletions.txt [--dry-run]

# M1 check: a clip replayed with its actions overlaid
uv run gamecap-pipeline replay --shard /data/out/shards/v1/<run>-000000.tar --index 3 -o clip.mp4
uv run gamecap-pipeline replay --segment /data/synth/sessions/s1/seg_000000 --start 150 --count 200 -o seg.mp4

# loader smoke test and throughput (needs the train extra)
CUDA_VISIBLE_DEVICES=1 uv run gamecap-pipeline load /data/out/shards/v1 --device cuda:0

uv run gamecap-pipeline action-spec     # the 292-column layout as json
```

### `process` options

| option | default | |
|---|---|---|
| `--input` / `--output` | required | local dir or `s3://bucket[/prefix]`. Segments are found by any `*/manifest.json` under the input |
| `--dataset-version` | `v0` | output goes to `shards/<version>/` |
| `--workers` | 4 | process pool (spawn) for validate, align and cut. Shard writing stays in the main process |
| `--shard-size-mb` | 1000 | WebDataset `ShardWriter` maxsize |
| `--clip-len` / `--stride` | 64 / clip-len | clip starts are keyframes at least `stride` frames apart |
| `--reencode-mid-gop` | off | also allow clips starting mid-GOP; only those are re-encoded (`--reencode-codec libx265\|hevc_nvenc`) |
| `--drop-idle` | off | drop clips that contain idle frames. Without it they are kept and flagged `idle` |
| `--idle-seconds` | 10 | input-free gap length that counts as idle |
| `--max-dropped-ratio` | 0.005 | reject segments with `dropped_frames / frame_count` above this |
| `--time-column` | `capture_ns` | or `tick_ns` |
| `--deletions` | – | text file, local dir or `s3://` prefix of text files (repeatable) |
| `--retry-rejected` | off | re-process segments whose marker says `rejected` or `error` |
| `--keep-local` | off | keep local shard copies after upload |
| `--config` | – | TOML file with the same keys (`input`, `output`, `dataset_version`, `workers`, `shard_size_mb`, `clip_len`, `deletions = [...]`, …) |

S3 settings come from the environment: `GAMECAP_S3_ENDPOINT` (or
`AWS_ENDPOINT_URL`), `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and
`AWS_REGION` (default `garage`; use `auto` for R2). Path-style addressing is
used by default, which Garage requires (`GAMECAP_S3_ADDRESSING=virtual` to
change it).

## What each step does

1. **List**: every folder with a `manifest.json` (and not `*.partial`).
   Segments already processed are skipped using marker objects in
   `shards/<v>/_processed/<session>__seg_n.json` on the output store, so a
   second machine sees them too. A segment is marked `ok` only after every
   shard containing its clips has been uploaded. Rejected segments get a
   `rejected` marker with the reasons. Segments on the deletion list are
   skipped.
2. **Validate** (`segment.py`): blake3 and size of all four files against the
   manifest, dropped ratio, and `frames.parquet` rows == video packets (PyAV
   demux) == `manifest.frame_count`. Also checks that `frame_idx` is 0..n-1,
   `tick_ns` strictly increases, `capture_ns` never decreases, and the
   parquet schemas match.
3. **Align** (`align.py`, polars and numpy): frame k's window is
   `[t_k + offset, t_{k+1} + offset)`, and for the last frame
   `[t_k + offset, t_k + tick + offset)`. `t = capture_ns` and
   `offset = latency_offset_ns`. Windows tile time, so an event exactly on a
   boundary belongs to the later frame. A repeated frame shares its capture
   time with the previous frame, which gives that previous frame an empty
   window.
4. **Action vector** (`action_spec.py`, D = 292):
   - 256 key slots, held at window end. The slots are the PS/2 set-1
     vocabulary: `0x01..0x7F` go to the same slot, `0xE0xx` go to
     `0x80 | xx`, and Pause goes to `0xC5`.
   - mouse dx/dy, summed over the window
   - 5 mouse buttons, held at window end
   - wheel v/h, summed
   - 8 pad axes, last value at window end
   - 19 pad buttons, last value at window end

   The layout is written to `shards/<v>/action_spec.json` and to every sidecar.
   Each clip json carries `{version, sha, dim}`. `reference.py` is a slow
   per-event implementation that the tests use as an oracle.
5. **Filter**: `focus.parquet` is a change log. A frame is usable only if focus
   holds at its capture time and through its whole action window. Idle means
   the frame's window sits in a gap of more than 10 s with no input events and
   nothing is held (keys, buttons, stick outside a 0.2 deadzone).
6. **Clips** (`clips.py`): 64-frame clips start on keyframes (GOP 20) and never
   span an unfocused frame or a dropped-frame gap (a tick gap of more than
   1.5 ticks). They are remuxed packet for packet into a normal MP4, so the
   pixels are bit-identical to the source. With `--reencode-mid-gop`, clips
   are decoded from the previous keyframe and re-encoded.
7. **Shards** (`shards.py`): `webdataset.ShardWriter`. Each sample is
   `<key>.mp4`, `<key>.actions.npy` (float32 `[T, D]`) and `<key>.json`
   (game, source segment and key, start frame, times, per-frame tick and
   capture times, idle flag and count). Every closed shard gets a
   `<shard>.sources.json` sidecar listing its source segment ids, source keys
   and sample keys.
8. **Upload**: each shard is uploaded as soon as it closes, shard first and
   then its sidecar. With the default settings the local copy is removed.

**Deletions**: an entry matches a segment id (`<session>/seg_000042`), a
session id, or a `/`-bounded prefix of the source key, for example `<user_id>`
in the R2 layout, which covers `DELETE /me/data`. `rebuild-affected` filters
the existing tars byte for byte, so the raw data isn't needed. It rewrites the
shard and sidecar, or removes both if nothing is left, and marks the segment
`deleted`.

## Training: `loader.py` and the 2×3090 split

```python
from gamecap_pipeline.loader import shard_urls, iterate_clips
urls = shard_urls("s3://gamecap/shards/v1")     # pipe:python -m gamecap_pipeline.s3cat s3://...
for frames, actions, meta in iterate_clips(urls, decode_device="cuda:0", num_workers=4):
    frames = frames.to("cuda:1", non_blocking=True)   # uint8 [T,3,H,W]; actions float32 [T,292]
```

- DataLoader worker processes run on the CPU. They stream tars from disk or
  S3 through `pipe:` URLs and parse the npy and json files. They never touch
  CUDA.
- The main process decodes the mp4 bytes with torchcodec on NVDEC
  (`device="cuda:0"`). Only compressed bytes cross PCIe. If CUDA or NVDEC
  isn't available, it falls back to CPU decoding.
- Split: **GPU 0 decodes and preprocesses, GPU 1 trains.** Alternatively,
  run DDP on both GPUs, each decoding its own clips on its own NVDEC.
  `wds.split_by_node` is already in the pipeline for that. Other services
  share these GPUs, so pin with `CUDA_VISIBLE_DEVICES`.

## Tests

```bash
uv run pytest -q          # 46 tests, ~15 s. The gpu-marked loader tests need the train extra
CUDA_VISIBLE_DEVICES=1 uv run pytest -q -m gpu
```

- alignment boundaries, offsets, repeated frames, the key vocabulary, and random events checked against the reference
- focus change log, focus backfill, and idle detection
- validation failures: hash, size, row count, frame count, dropped ratio, missing file or field, schema drift, decreasing capture_ns
- clip planning and cutting: every clip decodes to exactly 64 frames, starts on a keyframe and is pixel-identical to the source; mid-GOP re-encode
- end to end: synth → process → shards → loader. Checks the actions against the reference and the scripted facts (for example E held on exactly frames 100–103), idempotent re-runs, shard rollover, deletions and rebuilds, rejected segments, and replay
- S3 via an in-process moto server: raw layout, `pipe:` loader URLs, whole-user deletion
