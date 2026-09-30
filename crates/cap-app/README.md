# gamecap (cap-app)

`gamecap` is the command-line recorder. It captures one game window plus keyboard, mouse and gamepad
input into 1-minute segments, then uploads them to Garage (Phase 1) or through `ingest-api` (Phase 2).
A tray icon is optional (feature `tray`). The design is in [`docs/SPEC.md`](../../docs/SPEC.md).

```
gamecap windows                      # list capturable windows
gamecap record --game eldenring.exe  # record until Ctrl-C, upload in the background
gamecap status                       # queue, disk, consent, target
```

## Build

```bash
export PATH=$HOME/.cargo/bin:$PATH
cargo build --release -p cap-app                  # -> target/release/gamecap
```

| Feature | Default | What it does |
|---|---|---|
| `gamepad` | on | gilrs gamepads (cap-input `gamepad`). On Linux gilrs links **libudev**, so you need `libudev-dev` |
| `calibrate` | on | The `gamecap calibrate` window (winit + softbuffer, pure Rust; X11/Wayland libraries are loaded at runtime) |
| `tray` | off | Tray icon (tray-icon). **Windows and macOS only.** On Linux tray-icon needs GTK 3 and libappindicator dev packages, so the feature compiles to nothing there and `--tray` prints a warning |
| `pipewire` | off | Wayland capture through the XDG portal and PipeWire. Needs `libpipewire-0.3-dev`, `libspa-0.2-dev` and libclang. The portal restore token is kept in `state.json`, so the picker only shows once |

Build the Windows/macOS tray app with `cargo build --release -p cap-app --features tray`.

On a Linux box **without libudev-dev** (like the Ubuntu training box), leave out the gamepad feature:

```bash
cargo build --release -p cap-app --no-default-features --features calibrate
cargo test -p cap-app --no-default-features --features calibrate
```

FFmpeg: this build links the system FFmpeg (see `crates/cap-encode/README.md`). Release builds must
ship the bundled LGPL FFmpeg 7.x with hardware encoders. `.cargo/config.toml` sets
`BINDGEN_EXTRA_CLANG_ARGS` for the dev box.

### Windows build

Needs the VS 2022 Build Tools (MSVC), a **shared LGPL FFmpeg 7.1** dev build and **libclang <= 20**.

- FFmpeg: BtbN `ffmpeg-n7.1-*-win64-lgpl-shared-7.1.zip` (the `latest` release only has 8.x+; take
  one from a dated `autobuild-*` release). It has `include/`, `lib/*.lib` and the DLLs, with
  nvenc/amf/qsv enabled. Point `FFMPEG_DIR` at it and put its `bin/` on `PATH` to run.
- libclang: bindgen 0.70 (used by ffmpeg-sys-next 7.1) with LLVM 21+ turns `AVOption`,
  `AVFormatContext` and others into opaque 1-byte structs, and the build fails with
  `attempt to compute 1_usize - 64_usize` in the layout asserts. Use libclang 18, e.g. the
  `libclang` PyPI wheel (`libclang-18.1.1-py2.py3-none-win_amd64.whl` is a zip holding
  `clang/native/libclang.dll`). The wheel has no builtin headers, so pass any clang's
  `lib/clang/<ver>/include` through `BINDGEN_EXTRA_CLANG_ARGS` (this also overrides the Linux path
  in `.cargo/config.toml`).

```bash
export FFMPEG_DIR='D:\depsfmpeg-7.1'
export LIBCLANG_PATH='D:\deps\libclang18\libclang-18.1.1.data\platlib\clang
ative'
export BINDGEN_EXTRA_CLANG_ARGS='-ID:/deps/clang-include'   # copy of LLVM's lib/clang/22/include
export PATH="/d/deps/ffmpeg-7.1/bin:$PATH"
cargo build --release -p cap-app --features tray
cargo test --release --workspace
```

## Setup per platform

### Windows 10/11
- Capture uses Windows Graphics Capture and Raw Input (`RIDEV_INPUTSINK`). No global hooks, and it
  never opens the game process. No extra permission is needed.
- **Run games in borderless windowed mode.** Exclusive fullscreen can capture black frames.
- The yellow capture border is turned off where Windows allows it (Windows 11).
- Put `sessions_root` on a big, fast data drive, e.g. `sessions_root = "D:/gamecap/sessions"`.
  The default is `%LOCALAPPDATA%\gamecap\sessions` on C:.
- Keep `gamecap.exe` out of game directories. Release builds should be code-signed.

### macOS 12.3+
- Grant **Screen Recording** (ScreenCaptureKit) and **Input Monitoring** (IOHIDManager) under
  System Settings > Privacy & Security, to `gamecap` or to the terminal you run it from. Restart the
  terminal after granting. Without Input Monitoring, `record` fails with a hint.
- Game ids are bundle ids (`com.foo.game`). macOS capture and input paths are type-checked but
  **untested** on hardware.

### Linux
- Input comes from evdev (`/dev/input/event*`, read-only, never grabbed). Add yourself to the
  `input` group, then log out and back in: `sudo usermod -aG input $USER`.
- X11 and XWayland: capture via XComposite/XShm, focus via `_NET_ACTIVE_WINDOW`.
- Wayland: build with `--features pipewire`. The portal asks once, then the restore token skips the
  dialog. Focus tracking on Wayland works only for XWayland games (spec risk item).
- NVIDIA uses `hevc_nvenc`. AMD and Intel use `hevc_vaapi`, which is untested.

## Files

| Path | Default (Linux / Windows / macOS) |
|---|---|
| config | `~/.config/gamecap/config.toml` / `%APPDATA%\gamecap\config.toml` / `~/Library/Application Support/gamecap/config.toml`. Override with `--config` or `GAMECAP_CONFIG` |
| `blocklist.toml`, `state.json` (consent, Wayland token), `token.json` (API token, mode 0600) | next to the config |
| data dir (`data_dir`) | `~/.local/share/gamecap` / `%LOCALAPPDATA%\gamecap` / `~/Library/Application Support/gamecap` |
| sessions (`sessions_root`) | `<data_dir>/sessions/<session_id>/seg_NNNNNN/` |
| upload queue | `<data_dir>/upload-queue.sqlite3` |
| logs | `<data_dir>/logs/gamecap.YYYY-MM-DD.log` (daily, 14 kept). `RUST_LOG` sets the level for stderr and the file (default `info`) |

`gamecap paths` prints them all.

## Config reference (`config.toml`)

Every key is optional. An unknown key is an error, which catches typos.

```toml
# data_dir = "/big/disk/gamecap"         # sessions, queue DB, logs
# sessions_root = "D:/gamecap/sessions"  # default <data_dir>/sessions
disk_cap_gb = 20            # pause when local segments reach this (GiB); resume below 90%
latency_offset_ns = 0       # written by `gamecap calibrate` (see below)
# client_name = "gaming-pc" # device name for login; default hostname

[capture]
width = 640                 # even numbers
height = 360
rate_hz = 20
segment_secs = 60

[encoder]
codec = "hevc"              # or "av1" (av1_nvenc needs RTX 40+)
qp = 19                     # constant QP
lossless = false            # NVENC only
# gop = 20                  # keyframe interval in frames (default rate_hz = 1 s)
# force_encoder = "hevc_nvenc"   # "libx265" only for testing (software)

[hotkeys]                   # PS/2 scan code set 1; extended keys 0xE0xx
pause_key = 0x46            # Scroll Lock toggles pause (0 = off)
chat_pause = false          # default for all games
chat_key = 0x1C             # Enter opens chat -> inputs not logged...
chat_close_keys = [0x1C, 0x01]  # ...until Enter or Escape

[games."cs2.exe"]           # per-game overrides (game id, case-insensitive)
chat_pause = true
chat_key = 0x15             # Y

# Phase 1: paste exactly what deploy/garage/add-client.sh prints
[upload]
target = "s3"
user_id = "gaming-pc"       # raw/<user_id>/<session>/seg_n/...
endpoint = "http://100.x.y.z:3900"
region = "garage"
bucket = "gameplay"
access_key = "GK..."
secret_key = "..."
allow_http = true
# drain_secs = 30           # how long `record` keeps uploading after Ctrl-C

# Phase 2 instead:
# [upload]
# target = "presigned"
# api_base = "https://ingest.example.com"   # token from `gamecap login`
```

Without an `[upload]` section (or with `target = "none"`), segments stay on disk.

### Blocklist (`blocklist.toml`, next to the config)

```toml
default_deny = false        # true: only games with status = "allowed" record

[[game]]
id = "VALORANT-Win64-Shipping.exe"   # exe name or bundle id, case-insensitive
status = "blocked"                   # or "allowed"
publisher = "Riot Games, Inc."       # optional
reason = "kernel anti-cheat"
```

A blocked target is refused before anything is captured, and the error gives the reason. While
recording, a blocked game in the foreground **pauses** recording until it leaves the foreground.
A blocked entry with a `publisher` also matches when the publisher is unknown. An allowed entry with a
`publisher` needs a known publisher that matches. With `target = "presigned"`, the server list from
`GET /config` is also fetched, cached in `blocklist.server.json` and merged in. A block in either list
wins, and `default_deny` is OR-ed.

## Commands

| Command | |
|---|---|
| `gamecap windows` | Visible windows: native id (HWND / X11 id / CGWindowID), pid, game id, title, capture backend |
| `gamecap record --game <id\|title\|native id>` | Record. `--game` accepts a native id (decimal or `0x..`), an exact game id, or a unique title substring |
| `gamecap record --synthetic` | Full engine on a headless box: synthetic 1280x720@30 frames, endless scripted keyboard/mouse/gamepad input, scripted focus. Uses the real hardware encoder (NVENC here) unless `--fake-encoder` |
| `gamecap upload [--follow] [--timeout S]` | Uploader only: recovers partials (if no recorder is running), queues finished segments, drains |
| `gamecap status [-v]` | Target, consent, whether a recorder is running, local disk vs cap, finished/partial/broken folders, queue counts, failed rows with errors |
| `gamecap retry-failed` | `failed` rows go back to `pending` |
| `gamecap calibrate [--trials 20] [--no-save]` | Latency calibration (needs a display) |
| `gamecap login [--api-base URL]` | Device-code login against ingest-api. Saves `token.json`, which is refreshed automatically |
| `gamecap delete-my-data [--yes]` | `DELETE /me/data` after you type `delete` |
| `gamecap consent [--revoke]` | Show the terms and the accepted version, or revoke |
| `gamecap paths` | Print all resolved paths |

`record` options: `--control-stdin` and `--status-json` (used by `gamecap-gui`; protocol in [cap-gui README](../cap-gui/README.md#recorder-control-protocol-for-other-frontends)), `--no-upload`, `--duration S` (stop by itself), `--drain-secs S`, `--tray`,
`--fake-encoder`, and for synthetic runs `--synthetic-game-id` (default `synthetic.exe`; the
blocklist applies), `--synthetic-size WxH`, `--synthetic-fps`, `--synthetic-alt-tab N` (focus goes
to `--synthetic-foreground` for the last 20% of every N s).

### What `record` does

1. Takes `record.lock`, so only one recorder runs per data dir.
2. **Consent**: on first run it prints the terms and you must type `yes`. The version and time
   are stored in `state.json`. Anything else refuses to record. Bumping `CONSENT_VERSION` asks again.
3. Resolves the target window and applies the blocklist.
4. `recover_partials` finalizes segments a crash left as `.partial` (unrecoverable ones become
   `.broken`). `UploadQueue::scan_dir` then queues every finished segment on disk.
5. Probes the hardware encoder and refuses to record without one. Starts the recorder with the
   platform capture, input and focus sources. Finished segments go to `UploadQueue::enqueue`, and the
   upload worker runs on a tokio runtime in the same process (unless `--no-upload`, or a
   `gamecap upload` already holds `upload.lock`; then segments are only queued for that process).
6. Every 3 s it checks disk usage under `sessions_root`. At `disk_cap_gb` recording pauses with a
   warning, and it resumes below 90% of the cap as uploads free space.
7. Prints a status line every second on a TTY (every 5 s when stderr is not a TTY): time,
   `REC seg N` or `PAUSED (<reasons>)`, frames, dropped %, repeated %, inputs, segments, queue counts,
   local disk, last error.
8. **Ctrl-C** stops cleanly. The final segment is closed, held keys are released inside it, and the
   uploader drains for up to `drain_secs` (30 s). A second Ctrl-C skips the drain, and a third
   exits immediately. SIGTERM behaves the same way.

### Pause sources

A recording pauses if any of these holds, and the status line and tray show which:

| Reason | Trigger |
|---|---|
| user | `pause_key` (Scroll Lock) or tray Pause. Toggles whether or not the game is focused |
| chat | `chat_key` pressed while the game is focused (only if chat pause is on for that game). Ends on a `chat_close_keys` press |
| disk cap | local segments at or above `disk_cap_gb` |
| blocked game | a blocklisted game is in the foreground |

Hotkeys are read from the recorder's own passive input stream (evdev / Raw Input / IOHIDManager),
through the `cap_recorder::Sources::observer` tap. There are no global hooks. While paused, no frames are
written and no inputs are logged. The current segment ends at the pause, and a new one starts on
resume. The chat key press itself is logged; the text typed into chat is not.

### Latency calibration

`gamecap calibrate` opens a black 640x360 window that turns white for 350 ms on each key press. It
captures that window with the platform `FrameSource` and reads keys from the platform
`InputSource`s, which are the same clocks `record` uses. Per trial, `L = capture_ns(first bright
frame) - t_ns(key_down)`. After `--trials` presses it prints the median and p90, then writes
**`latency_offset_ns = -median(L)`** into the config. This is the sign `pipeline/SCHEMA.md` uses: frame k's action
window is `[capture_ns[k] + offset, capture_ns[k+1] + offset)`. CPU-readable frames are needed:
X11, Windows D3D11 (read back through a staging texture) and macOS CVPixelBuffer work. PipeWire
DMA-BUF frames are skipped.

## Tests

```bash
cargo test -p cap-app --no-default-features --features calibrate   # 22 unit tests
```

These cover the config (including the exact `add-client.sh` snippet), the blocklist and
default-deny rules, the pause and hotkey state machine, the consent prompt, window resolution,
session ids and locks, the synthetic input balance, and the calibration math (per-trial latency,
ambiguous trials, median/p90, luma of CPU frames, offset sign).

## Status

| | |
|---|---|
| Linux, synthetic source + NVENC + Garage upload + Python pipeline | **Tested end to end** on the Ubuntu box |
| Windows 10 + GTX 1070: WGC real window, D3D11 VP -> hevc_nvenc, Raw Input keyboard/mouse, gilrs, focus gate, pause key | **Tested** (short runs of Chrome/Notepad; segments pass `gamecap-pipeline validate`); workspace tests and ignored NVENC/crash-recovery tests pass |
| Windows tray, calibration window, AMF/QSV, 2-hour game session | built, not run |
| macOS (SCK, IOHID, VT, tray, calibration readback) | type-checked for `aarch64-apple-darwin`, untested |
| Linux X11 real window, evdev, calibration window | built, not run (the box has no display) |
| `pipewire` feature (portal token persistence) | not compiled here (no libpipewire-dev) |
| Phase 2 login / presigned / delete-my-data | built against the `cap_upload::api` client, not run against a live ingest-api |
