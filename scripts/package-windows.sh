#!/usr/bin/env bash
# Assemble a standalone Windows folder: gamecap.exe, gamecap-gui.exe and the
# FFmpeg DLLs side by side (the GUI finds gamecap.exe next to itself, and
# gamecap.exe finds the DLLs next to itself).
#   FFMPEG_DIR=D:/deps/ffmpeg-7.1 scripts/package-windows.sh D:/gamecap/app
set -euo pipefail
out=${1:?usage: package-windows.sh <out dir>}
: "${FFMPEG_DIR:?set FFMPEG_DIR to the shared FFmpeg 7.1 build}"
cd "$(dirname "$0")/.."
cargo build --release -p cap-app --features tray
cargo build --release -p cap-gui
mkdir -p "$out"
cp target/release/gamecap.exe target/release/gamecap-gui.exe "$out"/
for d in avcodec avformat avfilter avutil swscale swresample avdevice; do
  cp "$FFMPEG_DIR"/bin/$d-*.dll "$out"/
done
echo "packaged into $out"
ls -la "$out"
