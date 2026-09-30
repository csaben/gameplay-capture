# cap-encode

GPU scale + colour convert, hardware encode, fragmented-MP4 segment muxing.
Public API: `Encoder::{open, params, encode, flush}`, `SegmentMuxer::{create, write, finish}`, `gpu_name()`
(plus the additions `Encoder::last_frame_path()`, `Encoder::open_with_d3d11_device()` on Windows,
`SegmentMuxer::frames_written()`, `MOVFLAGS`, and the `probe` module).

## FFmpeg linkage

- Built on `ffmpeg-sys-next` 7.1 (the FFI layer of `ffmpeg-next`). The crate needs hw-frame contexts,
  filter graphs with `hw_device_ctx`, DRM PRIME descriptors and D3D11VA contexts, none of which
  `ffmpeg-next`'s safe wrapper covers, so it calls the sys crate directly. All `unsafe` code sits in
  `src/ffi/` (spec risk item).
- Dev box: system FFmpeg 6.1 (libavcodec 60). ffmpeg-sys-next 7.1 detects the version and builds against it.
- **Production must ship a bundled LGPL FFmpeg 7.x build** with the hardware encoders enabled
  (nvenc/ffnvcodec, amf, qsv/libvpl, vaapi, videotoolbox, d3d11va, cuda + `scale_cuda` via
  `--enable-cuda-llvm`) and no GPL-only parts (no libx265/libx264, no `--enable-gpl`). libx265 is only
  used by tests, and only when `force_encoder = "libx265"`.
- Building needs libclang for bindgen. If clang's builtin headers are missing (`'limits.h' file not found`,
  as on the dev box, which has libclang-18 but not `libclang-common-18-dev`), either install that package or
  build with `BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/x86_64-linux-gnu/13/include"`.

## Encoder selection

Probe order (first that opens wins; software is never probed):

| Platform | HEVC | AV1 |
|---|---|---|
| Windows | hevc_nvenc, hevc_amf, hevc_qsv | av1_nvenc, av1_amf, av1_qsv |
| Linux | hevc_nvenc, hevc_vaapi | av1_nvenc, av1_vaapi |
| macOS | hevc_videotoolbox | none |

`force_encoder` makes that encoder the only candidate. That is the only way to get a software encoder.
Failure gives `EncodeError::NoHardwareEncoder("name: reason; ...")`.

Settings: constant QP (NVENC `rc=constqp qp=N`, AMF `rc=cqp qp_i/qp_p`, VAAPI `rc_mode=CQP qp`, QSV CQP via
`global_quality`), `max_b_frames=0`, `gop_size=cfg.gop`, global header (extradata for the muxer),
`forced-idr=1`, so a frame with `force_keyframe` becomes an IDR. NVENC also sets `preset=p4 tune=hq delay=0 zerolatency=1`.
Lossless: NVENC `tune=lossless` (it is lossless on the YUV 4:2:0 that NVENC converts to, not on the RGB input).
AMF, QSV, VAAPI and VT have no lossless mode, so they refuse it. VideoToolbox has no CQP, so QP is mapped to its
0-100 constant-quality scale (`100 - 2*qp`). Everything chosen is recorded in `StreamParams.params`
(`opt.*` = AVOptions actually applied).

Colour: every path produces BT.601 limited-range YUV 4:2:0 (tagged `bt470bg`/`tv`). NVENC's internal
RGB->YUV uses BT.601, so swscale, the D3D11 VP and the VT transfer are configured to match.

## Frame paths

| Payload -> encoder | Path | Status |
|---|---|---|
| CPU BGRA -> hevc_nvenc (Linux) | `hwupload` (CUDA, encoder's device) -> `scale_cuda` (bicubic) -> NVENC takes BGR0 CUDA frames and does the RGB->YUV on the GPU | **Tested** |
| CPU RGBA / NV12 / FP16 -> any hw encoder | swscale (bicubic, clamp for FP16) -> `av_hwframe_transfer_data` into encoder pool | **Tested** (NVENC) |
| CPU -> libx265 (forced) | swscale -> yuv420p | **Tested** |
| CPU -> hevc_vaapi | `hwupload` -> `scale_vaapi=format=nv12` | compile-only (no VAAPI encode on the dev box) |
| DMA-BUF -> hevc_vaapi | DRM PRIME frame -> `hwmap` (VAAPI device) -> `scale_vaapi` | compile-only, **untested** |
| DMA-BUF -> hevc_nvenc | DRM PRIME -> `av_hwframe_map` to CPU (linear modifier only) -> CPU path | compile-only, **untested**; zero-copy CUDA import is a TODO |
| D3D11 texture -> nvenc/amf/qsv (Windows) | AVD3D11VA device on the WGC device; `VideoProcessorBlt` BGRA/FP16 -> NV12 texture from the encoder's pool (BindFlags=RENDER_TARGET) | type-checked only, **untested** |
| CVPixelBuffer -> hevc_videotoolbox | `VTPixelTransferSessionTransferImage` into a VT pool buffer | type-checked only, **untested** |

`scale_cuda` in FFmpeg 6.1/7.x cannot convert RGB to YUV ("Unsupported conversion: bgr0 -> nv12"), which
is why the CUDA path hands NVENC scaled RGB. HDR (Rgba16f) currently gets a **clamp** only, no real tone-mapping:
swscale on CPU, and on Windows the VP with input colour space `RGB_FULL_G10_NONE_P709`.
If a GPU graph fails to build, the encoder logs a warning, falls back to the CPU path for that input
shape, and does not retry it.

Windows: `Encoder::open` probes on its own D3D11 device. The first D3D11 frame from a different
device (the WGC device) reopens the encoder on that device. The extradata is the same for the same settings.
Prefer `Encoder::open_with_d3d11_device(cfg, &capture_device)`.

## Muxer

Fragmented MP4, `movflags=frag_keyframe+empty_moov+default_base_moof`, `flush_packets=1`, HEVC tagged
`hvc1` (AV1 `av01`). Extradata comes from `StreamParams`. The track timescale is `1000*rate_hz`, packet duration is 1 frame,
and `pts_offset` is subtracted so each segment starts at 0. A fragment reaches disk at every keyframe, so a
crash loses at most the current GOP (1 s). Dropping a `SegmentMuxer` without `finish()` still writes the trailer.

## Verified on the dev box (Ubuntu, 2x RTX 3090, FFmpeg 6.1.1, headless)

`cargo test -p cap-encode`: 4 unit tests and 10 integration tests (`tests/e2e.rs`) pass. NVENC tests skip
if hevc_nvenc cannot open.
- 1280x720 synthetic BGRA -> 640x360 hevc_nvenc, 60 frames at 20 Hz, split into 2 segments (40 + 20).
  ffprobe reports hevc Main, `hvc1`, 640x360, yuv420p, `has_b_frames=0`, the right frame counts,
  keyframes exactly every 20 frames, no B pictures, and pts 0 plus a keyframe at the start of each segment. The path used
  was `gpu:hwupload+scale_cuda`.
- A mid-GOP forced keyframe (frame 30) gives an I/IDR frame at the start of the new segment.
- Truncated at 70%, the 40-frame segment still decodes 28 frames with ffmpeg (exit 0).
- libx265 (forced) gives the same results. AV1 on RTX 3090 returns `NoHardwareEncoder` (Ampere has no AV1 encode).
- Throughput (release, hevc_nvenc, CPU BGRA in, 640x360 out, synchronous `delay=0`):
  720p 1063 fps (0.94 ms/frame), 1080p 634 fps (1.58 ms), 1440p 400 fps (2.50 ms).
  CPU fallback (swscale + upload), 1080p RGBA: 118 fps (8.5 ms).

Cross-platform type-check: `scripts/xcheck.sh` runs `cargo check` for `x86_64-pc-windows-msvc` and
`aarch64-apple-darwin` against the Linux-generated FFmpeg bindings. Both pass with no warnings.
This checks types and API usage (windows 0.58 D3D11 calls, VT extern decls) but not struct layouts or runtime behaviour.
