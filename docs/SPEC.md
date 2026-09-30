# Gameplay Frame–Action Capture: Technical Spec
2026-09-29 · @Clark Saben
## Goals and non-goals
A Rust engine records (frame, action) data from a game window at a fixed rate, with no effect on the live stream and no anti-cheat risk. It runs standalone on Windows, Linux and macOS, with an optional OBS plugin frontend later.
Goals
- Capture one game window on the GPU, downscale, and hardware-encode at a fixed rate (default 640×360 at 20 Hz).
- Log keyboard, mouse and gamepad events passively, with timestamps on the same monotonic clock as frames.
- Write self-contained 1-minute segments locally, then upload them to S3-compatible storage (tailnet box now, R2 later).
- Never block the game or OBS: bounded queues, drop and count frames instead of stalling.
- Build training-ready WebDataset shards on the Ubuntu 2×3090 box.
Non-goals (for now)
- Live streaming of the dataset feed. Segments sync after the fact.
- Reading game memory or game state, or injecting anything into the game process.
- Capturing the facecam or the composited OBS output.
## Architecture
(Architecture diagram: game window → capture (WGC · PipeWire · SCK) → fixed-rate sampler (20 Hz, latest frame) → scale + HW encode (NVENC · AMF · VT) → segment writer (60 s, manifest last) ← event logger (t_ns on same clock) ← focus gate ← input devices; segment writer → upload queue (SQLite · object_store) → Garage storage (S3) → validate + align (polars · PyAV) → WebDataset shards → training (NVDEC via torchcodec).)
Highlighted boxes keep frames in GPU memory; only compressed video and small event tables reach the CPU, disk and network. Video and inputs meet in the segment writer, stamped on one clock.
## Rust workspace layout
One Cargo workspace. Platform code sits behind traits so the recorder never sees OS types.
Crate | Responsibility | Key dependencies
cap-types | Timestamps, segment manifest, event and frame record types | serde, serde_json
cap-clock | One monotonic clock per platform (QPC, CLOCK_MONOTONIC, mach_absolute_time) | windows, libc, mach2
cap-capture | trait FrameSource + backends: win_wgc, linux_pipewire, linux_x11, mac_sck | see Capture backends
cap-encode | GPU scale + colour convert, hardware encode, segment muxing | ffmpeg-next (+ ffmpeg-sys-next)
cap-input | trait InputSource + backends: Raw Input, evdev, IOHIDManager, gamepads | windows, evdev, gilrs
cap-focus | Is the target game window focused? Game identity (exe, bundle id) | windows, x11rb, objc2-app-kit, sysinfo
cap-recorder | Orchestration: fixed-rate ticker, bounded channels, segmenter, drop accounting | crossbeam-channel, tracing
cap-upload | Durable upload queue, retries, integrity hashes | object_store, tokio, rusqlite, blake3
cap-app | CLI first, then tray app; config, blocklist fetch, consent screen | clap, tray-icon, toml, tracing-subscriber
ingest-api (server) | Presigned upload URLs, auth, session metadata | axum, sqlx (Postgres), object_store
Threads, not async, on the hot path: capture callback → scale/encode thread → mux/writer thread. `tokio` is only used in `cap-upload` and the server.
## Capture backends
Every backend captures a single window, returns a GPU texture, and stamps it with the platform monotonic clock. None injects into the game process.
Platform | API | Frame type | Timestamp | Rust starting point
Windows 10/11 | Windows Graphics Capture (WGC) | ID3D11Texture2D (BGRA8, or FP16 if HDR) | SystemRelativeTime (QPC based) | windows-capture crate, or windows crate directly
Linux, Wayland | XDG ScreenCast portal + PipeWire | DMA-BUF (fallback: shared memory) | PipeWire buffer PTS, CLOCK_MONOTONIC | ashpd (portal) + pipewire (pipewire-rs)
Linux, X11 | XComposite + XShm | CPU buffer | CLOCK_MONOTONIC at grab | x11rb
macOS 12.3+ | ScreenCaptureKit | CVPixelBuffer (IOSurface backed) | Host-time CMTime | screencapturekit crate, or objc2 bindings
Notes:
- WGC is event driven: it delivers a frame only when the window changes. The recorder samples the latest frame on its own clock (next section).
- On Windows 11, disable the yellow capture border with `IsBorderRequired = false` (requires the capture access permission).
- Wayland shows a one-time consent dialog. Store the portal restore token so later sessions skip it.
- Exclusive-fullscreen games can capture black. Recommend borderless windowed in onboarding.
- The `scap` crate (cross-platform) is a useful reference for all three platforms, even if you don't depend on it.
## Downscale, fixed-rate sampling and encoding
Frames stay on the GPU from capture to encoder; only compressed bitstream reaches CPU memory.
Fixed-rate sampling. A ticker on the monotonic clock fires at the target rate (default 20 Hz). Each tick takes the most recent captured frame, or repeats the previous one if the window hasn't changed. Every output frame records both its tick time and the capture time of the source frame it used, so alignment can use real capture times.
GPU scale and colour convert (RGB → NV12) per platform
Platform | Scale / convert | Hands off to
Windows | D3D11 Video Processor (ID3D11VideoContext::VideoProcessorBlt) | FFmpeg D3D11 hardware frames → hevc_nvenc, hevc_amf, hevc_qsv
Linux, NVIDIA | Import DMA-BUF into CUDA, FFmpeg scale_cuda | hevc_nvenc
Linux, AMD/Intel | FFmpeg scale_vaapi on imported DMA-BUF | hevc_vaapi
macOS | VTPixelTransferSession | hevc_videotoolbox
Encoder settings (dataset, not stream)
- HEVC Main, constant QP (start around QP 18–20 for NVENC), no B-frames, keyframe every 1 s so segments and random access are clean.
- AV1 (`av1_nvenc` on RTX 40-series and newer) as an option where available.
- Lossless mode behind a flag for experiments; expect several times the storage.
- Container: fragmented MP4 (`movflags=frag_keyframe+empty_moov`), so a crash leaves a playable partial file.
Backpressure. Capture → encoder and encoder → writer channels hold at most 4 items. When full, drop the newest frame, increment `dropped_frames` in the manifest, and never block the capture callback.
FFmpeg linkage. Use `ffmpeg-next` against FFmpeg 7.x or newer. Ship a bundled LGPL build with hardware encoders enabled (no GPL-only components) to keep commercial licensing simple. Encoder probe at startup picks the first working hardware encoder, and refuses to record (rather than fall back to CPU) if none works.
## Input logging, focus gating and clocks
Inputs are logged as a full-rate event stream, only while the game window has focus, stamped on the same clock as frames. Resampling to the dataset rate happens offline.
Source | Windows | Linux | macOS
Keyboard + mouse | Raw Input on a hidden message-only window, RIDEV_INPUTSINK (windows crate) | evdev crate on /dev/input/event* (user in input group) | IOHIDManager (Input Monitoring permission)
Gamepad | gilrs (XInput / Windows.Gaming.Input) | gilrs (evdev) | gilrs (IOKit)
Focus + game identity | SetWinEventHook(EVENT_SYSTEM_FOREGROUND) out-of-context, GetWindowThreadProcessId | X11: _NET_ACTIVE_WINDOW via x11rb. Wayland: no general API (see Risks) | NSWorkspace.frontmostApplication
Clock | QueryPerformanceCounter | CLOCK_MONOTONIC; set evdev to it with EVIOCSCLOCKID | mach_absolute_time
Do not use global hooks (`SetWindowsHookEx`, and crates built on them such as `rdev` on Windows), input injection, or anything that opens the game process.
Event record (one row per event): `t_ns` (i64, monotonic), `device` (keyboard | mouse | gamepad), `kind` (key_down, key_up, mouse_move, mouse_button, wheel, axis, button), `code` (u32: scan code, button id or axis id), `value` (f32: 1/0 for buttons, delta for mouse, −1..1 for axes).
- Keys are logged as scan codes, never translated text.
- Mouse movement uses raw relative deltas, which is what games consume.
- Gamepad axes are polled at 250 Hz and logged only on change beyond a small deadzone.
Latency calibration. A calibration mode shows a test window that flashes on keypress and measures input-to-captured-frame delay. Store the measured offset in each manifest.
## Segment format
Each 60-second segment is a self-contained folder that can be uploaded, verified and processed on its own.
```
sessions/<session_id>/seg_000042/
  video.mp4        fragmented MP4, HEVC, 640x360 @ 20 Hz
  frames.parquet   frame_idx, tick_ns, capture_ns, repeated (bool)
  inputs.parquet   t_ns, device, kind, code, value
  focus.parquet    t_ns, focused (bool), game_id
  manifest.json    written last; its presence marks the segment complete
```
`manifest.json` fields: `schema_version`, `session_id`, `segment_idx`, `client_version`, `os`, `gpu`, `encoder`, `encoder_params`, `width`, `height`, `rate_hz`, `game_id` (exe name or bundle id), `t_start_ns`, `t_end_ns`, `dropped_frames`, `latency_offset_ns`, `blake3` per file.
Parquet is written with the `arrow` and `parquet` crates at segment close; events for one minute fit easily in memory. The writer creates the folder under a `.partial` name and renames it once `manifest.json` is flushed, so the uploader never sees a half-written segment.
## Upload and storage
Clients upload finished segments over the S3 API. Only the endpoint and credentials change between the tailnet phase and R2.
Client upload queue (cap-upload)
1. The recorder hands over a completed segment folder.
2. A SQLite table (`rusqlite`) records its state: `pending`, `uploading`, `uploaded`, `verified`.
3. Files upload via `object_store` (multipart above 64 MB), manifest last.
4. After a `HEAD` confirms sizes, the local folder is deleted. Failures retry with exponential backoff; the queue survives restarts.
5. A disk cap (default 20 GB) pauses recording with a notice rather than filling the drive.
Phase 1: tailnet box
- Run Garage on the Ubuntu machine: a lightweight, self-hosted S3-compatible server written in Rust.
- Bind it to the Tailscale interface only. One access key per client machine.
- Confirm with `tailscale ping` that clients connect directly rather than through a relay.
- Uploads are HTTPS/TCP, so the tailnet's lower MTU is not a concern.
Phase 2: Cloudflare R2
- Same `object_store` code, different endpoint. Clients no longer hold keys; they request presigned PUT URLs from `ingest-api`.
- Object key layout: `raw/<user_id>/<session_id>/seg_<n>/<file>`. Processed shards go under `shards/<dataset_version>/`.
- Storage costs $0.015 per GB-month with no egress fees, so the training box can re-read data freely (pricing).
## Backend for multiple users
A small `axum` service issues presigned URLs, records metadata, and serves the game blocklist. It is not needed in Phase 1.
Endpoint | Purpose
POST /auth/device | Device login (OAuth device-code flow), returns a short-lived token
GET /config | Blocklist, allowed encoders, rate and resolution, minimum client version
POST /sessions | Start a session: game id, client version, consent version accepted
POST /sessions/{id}/segments/{n}/upload | Returns presigned PUT URLs for each file of the segment
POST /sessions/{id}/segments/{n}/complete | Client reports hashes; server checks objects exist and marks it ready
DELETE /me/data | Queues deletion of all of a user's raw data and removes them from future shards
Postgres tables (sqlx): `users`, `devices`, `consents` (version, accepted_at), `sessions`, `segments` (state, sizes, hashes, dropped_frames), `games` (id, status: allowed or blocked, notes).
Auth can start with a hosted provider rather than custom accounts. Deletion must reach raw objects and any shard built from them, which is why shards carry a list of their source segments.
## Processing pipeline (Ubuntu 2×3090)
A Python batch job turns verified segments into WebDataset shards of short clips with aligned action tensors. It runs on the tailnet box in both phases, reading from Garage or R2.
1. List ready segments from Garage/R2 (and later from Postgres), skipping ones already processed.
2. Validate: hashes match, `dropped_frames` under a threshold, frame count matches `frames.parquet`.
3. Align with `polars`: for frame k with capture time t_k, the action window is events in [t_k + offset, t_{k+1} + offset), with offset = `latency_offset_ns`.
4. Build per-frame action vectors: held-key bitmap at window end, summed mouse deltas, mouse buttons, wheel, last gamepad axis values, gamepad buttons.
5. Filter: drop spans where `focused` is false; flag idle spans with no input for more than 10 s.
6. Cut clips (e.g. 64 frames at 20 Hz = 3.2 s) on keyframes and remux without re-encoding (PyAV). Re-encode only if clips must start mid-GOP.
7. Write shards with the `webdataset` library, about 1 GB each: `<key>.mp4`, `<key>.actions.npy`, `<key>.json` (game, source segment, times). Each shard gets a sidecar listing its source segments for deletion requests.
8. Upload shards to `shards/<dataset_version>/`.
For training, decode on the GPU with `torchcodec` (NVDEC) and read shards straight from S3/R2 through `webdataset` pipes. With 2×3090, one GPU can decode and preprocess while the other trains, or both train with DDP.
## Anti-cheat, privacy and legal
The engine only observes: OS capture APIs, passive input reads, and no contact with the game process. That minimises risk but cannot guarantee any given anti-cheat won't flag it.
Anti-cheat
- Allowed APIs only: WGC / PipeWire / ScreenCaptureKit, Raw Input / evdev / IOHIDManager, `gilrs`, out-of-context WinEvent hooks.
- Never: DLL injection, `SetWindowsHookEx` keyboard or mouse hooks, `OpenProcess` on the game, memory reads, input injection.
- Code-sign the Windows binary and keep it out of game directories.
- Test each supported game on an alt account before allowlisting it.
Game blocklist
- Served from `GET /config`, cached locally; in Phase 1 a local `blocklist.toml`.
- Matched on exe name or bundle id plus optional publisher. A blocked game stops recording and shows why.
- Default-deny is safer for the product: record only allowlisted games once there are outside users.
Privacy
- Inputs are recorded only while the target game is focused; alt-tab stops logging immediately.
- Keys stored as scan codes. A "chat pause" hotkey, and optional per-game rules to drop spans where a chat key opened a text box.
- Only the game window is captured, never the desktop, facecam or microphone.
- Visible recording indicator in the tray, and a one-key pause.
Legal (for review by counsel)
- User ToS and consent covering recording, commercial use of gameplay and inputs for model training, retention and deletion.
- Per-game review of EULAs on recording and commercial use.
- Data-protection obligations (e.g. GDPR/CCPA deletion and access requests) once users are outside your own machines.
## Milestones
Windows + NVIDIA first, because that is the most common gaming setup; each milestone ends with a concrete check.
1. M0 · Local recorder (Windows, NVENC). WGC capture, D3D11 scale, `hevc_nvenc`, Raw Input + `gilrs`, 1-minute segments on disk.
  - Done when: a 2-hour session records with zero stalls, dropped frames under 0.1%, and no measurable FPS loss in the game.
2. M1 · Tailnet pipeline. Garage on the Ubuntu box, `cap-upload` queue, alignment script, first WebDataset shards.
  - Done when: a recorded session appears as shards and a sample clip replays with its actions overlaid correctly.
3. M2 · Safety and privacy. Focus gating, blocklist, chat pause, tray indicator, latency calibration.
  - Done when: alt-tabbing into a browser logs nothing, and a blocked game refuses to record.
4. M3 · Other platforms and GPUs. AMF and QSV on Windows; Linux (PipeWire, VAAPI/NVENC); macOS (ScreenCaptureKit, VideoToolbox).
  - Done when: the same test session produces valid segments on each platform.
5. M4 · Multi-user backend on R2. `ingest-api`, device login, presigned uploads, consent, deletion.
  - Done when: an outside tester can install, consent, record and later delete their data end to end.
6. M5 · OBS plugin frontend (optional). Feeds OBS's game-source texture into the same core for streamers.
## Risks and open questions
Risk | Impact | Mitigation
Anti-cheat flags the recorder in some game | Account bans for users | Observe-only APIs, per-game alt-account testing, default-deny allowlist
Wayland has no general focus API | Can't gate inputs reliably on Linux Wayland | Per-compositor support (KWin, GNOME extensions), or X11/XWayland games only at first
Exclusive fullscreen or HDR games | Black or washed-out frames | Recommend borderless; tone-map FP16 to SDR in the scale step
FFmpeg hardware-frame interop in Rust is unsafe-heavy | Slow M0, crash bugs | Keep interop in one small module; fall back to vendor SDK bindings if it stalls
Encoder session limits on older drivers | Recording fails while streaming | Probe at startup and show a clear error
Game EULAs forbid commercial use | Legal exposure | Counsel review per game before allowlisting
Open questions
- Final dataset resolution and rate: 640×360 at 20 Hz, or higher for some games?
- Clip length for training samples (64 frames assumed).
- Does the action vector need absolute cursor position for menu-heavy games, or only raw deltas?
- Which games are in the first allowlist?