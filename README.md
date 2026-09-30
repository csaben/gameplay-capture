# gameplay-capture

Records (frame, action) data from a game window: GPU capture → fixed-rate
sampling → hardware HEVC → 60 s self-contained segments → S3 upload (Garage on
the tailnet now, R2 later) → WebDataset shards on the 2×3090 box.
Full spec: [docs/SPEC.md](docs/SPEC.md).

| Path | What |
|---|---|
| `crates/cap-types` | Record types, manifest, segment layout |
| `crates/cap-clock` | Monotonic clock (QPC / CLOCK_MONOTONIC / mach) |
| `crates/cap-capture` | WGC, PipeWire portal, X11, ScreenCaptureKit, synthetic |
| `crates/cap-encode` | GPU scale + NVENC/AMF/QSV/VAAPI/VideoToolbox, fMP4 muxer ([README](crates/cap-encode/README.md)) |
| `crates/cap-input` | Raw Input, evdev, IOHIDManager, gilrs; one set-1 scan-code space |
| `crates/cap-focus` | Foreground tracking, game identity, focus gate |
| `crates/cap-recorder` | Ticker, bounded queues, segment writer, crash recovery |
| `crates/cap-upload` | SQLite upload queue, S3 / presigned targets |
| `crates/cap-app` | `gamecap` CLI ([README](crates/cap-app/README.md)) |
| `server/ingest-api` | Phase 2 backend: device login, presigned URLs, deletion ([README](server/ingest-api/README.md)) |
| `deploy/garage` | Phase 1 Garage on the tailnet ([README](deploy/garage/README.md)) |
| `pipeline/` | Python: validate, align, clip, shard, replay, loader ([README](pipeline/README.md), [SCHEMA](pipeline/SCHEMA.md)) |

## Build

```bash
cargo build --release -p cap-app          # gamecap binary (needs libudev-dev on Linux for gamepads)
cargo build --release -p cap-app --no-default-features --features calibrate   # without gamepads
cargo test --workspace --exclude cap-app --exclude cap-input
```

Needs FFmpeg dev libs (bundled LGPL FFmpeg 7.x with HW encoders for release
builds) and libclang. On Linux, `--features pipewire` in cap-capture needs
`libpipewire-0.3-dev libspa-0.2-dev`.

## Status

Verified on the Ubuntu 2×3090 box: synthetic source → NVENC → segments →
Garage → pipeline → shards → replay, crash recovery, disk cap, blocklist,
consent. X11 capture and focus verified under Xvfb; PipeWire SHM stream
verified against a GStreamer node.

Verified on Windows 10 (GTX 1070, FFmpeg 7.1 LGPL): WGC window capture ->
D3D11 video processor -> hevc_nvenc (zero copy), Raw Input keyboard/mouse scan
codes, gilrs gamepad, WinEvent focus gate, pause key, segments pass
`gamecap-pipeline validate`. Windows build setup: [cap-app README](crates/cap-app/README.md#windows-build).

Compile-checked only: macOS (ScreenCaptureKit, VideoToolbox, IOHIDManager),
Linux DMA-BUF / VAAPI / portal paths, calibration window, tray. M0 still needs a
2-hour session in a real game.
