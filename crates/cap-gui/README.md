# gamecap-gui (cap-gui)

Desktop app for Windows: pick a window, start/stop recording, global keyboard / mouse / gamepad
hotkeys, and a list of recordings that links into the web viewer
(`gamecap-pipeline viewer`, see [pipeline/README.md](../../pipeline/README.md#viewer)).

Recording runs in a child process, `gamecap record --game <hwnd> --control-stdin --status-json`.
If that process crashes, the UI stays up. The GUI's own Raw Input registration (for hotkeys) also
stays separate from the recorder's.

## Build and package

```bash
source /d/deps/env.sh                          # FFMPEG_DIR, libclang, ... (see cap-app README)
scripts/package-windows.sh D:/gamecap/app      # gamecap.exe + gamecap-gui.exe + FFmpeg DLLs
D:/gamecap/app/gamecap-gui.exe
```

The GUI looks for `gamecap.exe` next to itself. If the FFmpeg DLLs aren't next to `gamecap.exe`, it
adds `FFMPEG_DIR\bin` (or the folder set under Hotkeys > Advanced) to the child's PATH. The GUI
itself doesn't link FFmpeg.

## Tabs

- **Record**
  - The list of capturable windows has a filter; double-click a window to start.
  - The selected window gets a live preview, composited by DWM with `DwmRegisterThumbnail`. Nothing
    touches the game process.
  - Start, Stop and Pause buttons, plus status cards: segment, frames, dropped %, repeated %,
    inputs, upload queue, disk.
  - The recorder log is collapsible.
  - Stopping waits for uploads to drain, up to `drain_secs`. Clicking Stop again skips the wait.
- **Hotkeys**
  - Two actions: *Start/stop* (default Ctrl+Shift+F9) and *Pause/resume* (unset by default;
    Scroll Lock still pauses via the recorder).
  - **Record binding** listens for a combo. Press any mix of keys, mouse middle/back/forward and
    gamepad buttons (triggers count as pressed past 50%), then release everything. Save,
    Try again or Cancel.
  - It warns about:
    - duplicates (saving moves the binding);
    - single keys that would fire during play;
    - shortcuts another app has registered with `RegisterHotKey`. Windows sends those to that app
      and never to Raw Input, so only the modifiers arrive.
  - Options: the start hotkey records the **foreground window** (default: press it in-game) or the
    window selected in Record. Beep on start/stop. Upload on/off (`--no-upload`).
- **Recordings**
  - Local sessions under `sessions_root`, with a *View* link into the viewer.
  - Verified uploads are deleted locally, so those sessions show as *uploaded* and link to the
    `garage` source (`#s=garage&id=<user>/<session>`).
  - The `gamecap status` output is also shown here.

Settings are stored in `gui.json` next to gamecap's `config.toml` (`%APPDATA%\gamecap\`). That
config file rejects unknown keys, so the GUI keeps its own file.

## Hotkeys: how they're read

This uses the same passive sources as the recorder: `cap_input::default_sources()`, which gives
Raw Input with `RIDEV_INPUTSINK` plus gilrs. It doesn't use `SetWindowsHookEx`, and it doesn't use
`RegisterHotKey`, which would also swallow the combo from the game.

A binding fires once, on the press that completes it while its other elements are held. Other held
keys don't block it, so Ctrl+Shift+F9 still works while you hold W. Key auto-repeat doesn't fire it
again. The combo keystrokes are also logged as ordinary inputs in the recording, like everything
else pressed while the game is focused.

## Recorder control protocol (for other frontends)

`gamecap record --control-stdin --status-json`:

- **stdin:** `stop` stops and drains uploads. `stop-now` also skips the drain. `pause` toggles the
  user pause. EOF means `stop`, so the recorder stops cleanly if the parent dies.
- **stdout:** JSON lines:
  - `{"event":"started",...}` (session, game_id, title, size, rate, dir);
  - `{"event":"status",...}` every second (state recording/waiting/paused, segment, frames,
    dropped_pct, inputs, queue{pending,uploading,verified,failed}, disk_used, last_error);
  - `{"event":"stopped",...}` at the end (segments, frames, dropped, inputs, drained, outstanding).
- **Other commands:** `gamecap windows --json` lists windows. `gamecap consent --json` returns the
  terms and whether they were accepted. `gamecap consent --accept` records acceptance; the GUI shows
  the terms first.

## Status

Tested on inari (Windows 10, GTX 1070) with Notepad and Chrome:

- window list, preview, and start/stop from the button and from the hotkey (foreground target);
- pause/resume with a custom binding (Ctrl+Shift+K): the segment splits at the pause and nothing is
  dropped after the fix below;
- binding capture, including the "another app owns this shortcut" case (Ctrl+Alt+P was taken);
- the consent flow, the Recordings tab, and upload to Garage.

Gamepad bindings go through the same code path as keys and are unit-tested, but they haven't been
pressed on a real pad yet. The app has no tray icon; keep the window open or minimized.
