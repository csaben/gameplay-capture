# Segment on-disk contract (Rust writer ⇄ Python pipeline)

This is what the processing pipeline expects from each segment folder, and
what `gamecap-pipeline synth` writes. `pipeline/src/gamecap_pipeline/segment.py`
holds the same schemas as code (`FRAMES_SCHEMA`, `INPUTS_SCHEMA`,
`FOCUS_SCHEMA`). `tests/test_validation.py::test_parquet_schema_is_canonical`
checks the synth writer against them. To check a real Rust-written segment, run
`gamecap-pipeline validate <segment dir>`.

```
sessions/<session_id>/seg_000042/          (phase 1, local)
raw/<user_id>/<session_id>/seg_000042/     (R2 key layout)
  video.mp4        fragmented MP4, HEVC, 640x360 @ 20 Hz
  frames.parquet
  inputs.parquet
  focus.parquet
  manifest.json    written last; its presence marks the segment complete
```

The segment id is `<session_id>/seg_<nnnnnn>`, taken from the manifest's
`session_id` and `segment_idx`. The pipeline also derives it from the last two
path components, so the folder must be named `seg_%06d` and sit directly
inside a `<session_id>` folder. Folders whose name ends in `.partial` are
ignored.

## Parquet schemas (Arrow types; every field non-nullable)

Write with the Rust `arrow` and `parquet` crates. Compression and row-group
layout are free (the synth writer uses zstd and one row group). The pipeline
selects columns by name. It casts losslessly compatible types (for example
LargeUtf8, a dictionary-encoded string or Int32) with a warning, but these
exact types are the contract:

### frames.parquet

| column       | Arrow type | Rust (`arrow`)                 | notes |
|--------------|------------|--------------------------------|-------|
| `frame_idx`  | `UInt32`   | `DataType::UInt32`             | **0..n-1, contiguous, = index of the frame in video.mp4** (decode order) |
| `tick_ns`    | `Int64`    | `DataType::Int64`              | ticker time. Strictly increasing. A dropped frame leaves a gap of whole ticks |
| `capture_ns` | `Int64`    | `DataType::Int64`              | capture time of the source frame. Non-decreasing |
| `repeated`   | `Boolean`  | `DataType::Boolean`            | true means same source frame as the previous row (capture_ns equal to previous) |

```rust
Schema::new(vec![
    Field::new("frame_idx",  DataType::UInt32,  false),
    Field::new("tick_ns",    DataType::Int64,   false),
    Field::new("capture_ns", DataType::Int64,   false),
    Field::new("repeated",   DataType::Boolean, false),
])
```

### inputs.parquet

| column   | Arrow type | notes |
|----------|------------|-------|
| `t_ns`   | `Int64`    | same monotonic clock as frames |
| `device` | `Utf8`     | `keyboard` \| `mouse` \| `gamepad` (`Device::as_str`) |
| `kind`   | `Utf8`     | `key_down` \| `key_up` \| `mouse_move` \| `mouse_button` \| `wheel` \| `axis` \| `button` (`EventKind::as_str`) |
| `code`   | `UInt32`   | see code tables below |
| `value`  | `Float32`  | 1/0 for keys and buttons, delta for mouse_move, detents for wheel, -1..1 for axes (0..1 triggers) |

```rust
Schema::new(vec![
    Field::new("t_ns",   DataType::Int64,   false),
    Field::new("device", DataType::Utf8,    false),   // plain Utf8, not LargeUtf8 / Dictionary
    Field::new("kind",   DataType::Utf8,    false),
    Field::new("code",   DataType::UInt32,  false),
    Field::new("value",  DataType::Float32, false),
])
```

Rows should be in time order. The pipeline sorts stably by `t_ns`, so rows with
the same timestamp keep their file order, and for state (keys, buttons, axes)
the last one wins.

### focus.parquet

| column    | Arrow type | notes |
|-----------|------------|-------|
| `t_ns`    | `Int64`    | |
| `focused` | `Boolean`  | |
| `game_id` | `Utf8`     | exe name or bundle id. `""` is allowed while unfocused |

```rust
Schema::new(vec![
    Field::new("t_ns",    DataType::Int64,   false),
    Field::new("focused", DataType::Boolean, false),
    Field::new("game_id", DataType::Utf8,    false),
])
```

This file is a change log: each row's state holds until the next row.
**Write a first row at segment open (at or before `t_start_ns`) with the
current state.** The pipeline extends the first row's state backwards to
cover frame 0's capture time and window, which can come slightly earlier. An
empty focus table means "focused throughout".

## Code tables

**Keyboard** (`device=keyboard`, `kind=key_down|key_up`): PS/2 scan code set 1
make codes, never translated text.
- base keys: `0x01..0x7F` (for example W = `0x11`, LShift = `0x2A`)
- extended (E0-prefixed) keys: `0xE000 | code` (for example RCtrl = `0xE01D`,
  Up = `0xE048`, RAlt = `0xE038`, LWin = `0xE05B`, keypad Enter = `0xE01C`)
- Pause (E1 1D 45): `0xE11D` (`0xE11D45` is also accepted)
- On Windows, Raw Input `MakeCode` plus the `RI_KEY_E0` / `RI_KEY_E1` flags map
  to these codes directly. On Linux, evdev `KEY_*` codes must be translated to
  set 1 (for example `KEY_RIGHTCTRL` (97) → `0xE01D`). On macOS, HID usages
  must be translated to set 1 too.
- Key auto-repeat `key_down`s are fine: state is idempotent.
- The pipeline ignores the "fake shift" codes `0xE02A` / `0xE036` and any
  other unknown code (counted as `unknown_codes`). This includes cap-input's
  tagged codes for keys with no set-1 equivalent (`0x2_0000 | evdev`,
  `0x3_0000 | hid_usage`): they stay in the raw inputs but get no action slot.

Cross-checked against the concurrent Rust code: `cap-recorder/src/tables.rs`
declares exactly the schemas above, and `cap-input/src/scancode.rs` and
`gamepad_code.rs` use the same key and gamepad code values.

**Mouse**, as in `cap-types`:
- `mouse_move`: code 0 = x, 1 = y, value = raw delta
- `wheel`: code 0 = vertical, 1 = horizontal, value = detents
- `mouse_button`: code 0 left, 1 right, 2 middle, 3 back, 4 forward; value 1/0

**Gamepad** (proposed; this is gilrs enum order, so `code = gilrs::Axis/Button as index`):
- `axis`: 0 left_x, 1 left_y, 2 left_z (LT), 3 right_x, 4 right_y, 5 right_z (RT), 6 dpad_x, 7 dpad_y
- `button`: 0 south, 1 east, 2 north, 3 west, 4 c, 5 z, 6 left_trigger (LB),
  7 left_trigger2 (LT), 8 right_trigger (RB), 9 right_trigger2 (RT), 10 select,
  11 start, 12 mode, 13 left_thumb, 14 right_thumb, 15 dpad_up, 16 dpad_down,
  17 dpad_left, 18 dpad_right
- All pads are merged into one; there is no pad-id column.

**Recommended:** at segment open, re-emit `key_down`, `mouse_button`, `button`
and `axis` events for anything currently held or deflected. Otherwise a key
held across a segment boundary looks released until its `key_up`, because each
segment is processed on its own.

## video.mp4

- HEVC (or AV1), fragmented MP4 (`frag_keyframe+empty_moov`), no B-frames,
  keyframe (sync sample) every `rate_hz` frames, and the first frame is a
  keyframe.
- Exactly one packet per `frames.parquet` row, in the same order:
  packet count == `frame_count` == number of rows. Dropped frames are **not**
  encoded (drop before the encoder; dropping encoded packets would break the
  reference chain). Clip cutting uses packet order and ignores pts values.

## manifest.json

The fields and types of `cap_types::Manifest` (serde, snake_case):
`schema_version` (u32 = 1), `session_id`, `segment_idx` (u32),
`client_version`, `os`, `gpu`, `encoder`, `encoder_params` (map str→str),
`width`, `height`, `rate_hz` (u32), `game_id`, `t_start_ns`, `t_end_ns` (i64),
`frame_count` (u32), `dropped_frames` (u64), `latency_offset_ns` (i64),
`blake3` (map file name → lowercase hex digest), `sizes` (map file name → bytes).
`blake3` and `sizes` must cover all four data files.

Validation rejects a segment when a hash or size mismatches,
`dropped_frames / frame_count > 0.5%` (configurable), the row count or video
packet count differs from `frame_count`, `frame_idx` is not 0..n-1,
`tick_ns` is not strictly increasing, or `capture_ns` decreases.

### Alignment and `latency_offset_ns` sign (open question)

The pipeline computes frame k's action window as
`[capture_ns[k] + latency_offset_ns, capture_ns[k+1] + latency_offset_ns)`,
using the offset exactly as stored. Row k of the action tensor is meant to be
"the input that leads from frame k to frame k+1". If calibration measures L,
the delay from an input to the first captured frame that shows it, the
correct stored value is **`latency_offset_ns = -L`** (negative). The Rust
calibration mode needs to agree on this.

## Shard output (for reference)

```
shards/<dataset_version>/action_spec.json
shards/<dataset_version>/<run_id>-000000.tar
shards/<dataset_version>/<run_id>-000000.tar.sources.json
shards/<dataset_version>/_processed/<session_id>__seg_nnnnnn.json
```

Each sample `<key>` (`<session>_s<seg>_f<start>`) consists of three files:
- `<key>.mp4`: 64 frames, remuxed with no re-encode
- `<key>.actions.npy`: float32 `[64, 292]`
- `<key>.json`: game_id, session and segment ids, source_key, start_frame,
  times, per-frame tick_ns, capture_ns and repeated, idle flag and count,
  reencoded, and an `action_spec` ref

The action layout is in `src/gamecap_pipeline/action_spec.py` and in
`action_spec.json`.
