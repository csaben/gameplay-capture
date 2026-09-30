#!/usr/bin/env bash
# Type-check cap-encode for Windows and macOS from Linux.
#
# ffmpeg-sys-next cannot be cross-built (it needs target FFmpeg headers and a
# target C compiler), so this builds a throwaway workspace in which
# ffmpeg-sys-next is replaced by the *Linux-generated* bindings (layout
# assertions stripped). That is enough for `cargo check` to type-check all
# cfg(windows) / cfg(target_os = "macos") code in this crate. It proves
# nothing about ABI/layout or runtime behaviour.
#
# Needs: a prior Linux build of cap-encode (for bindings.rs), rustup targets
# x86_64-pc-windows-msvc and aarch64-apple-darwin.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
WORK="${1:-$(mktemp -d)}"
BINDINGS="$(ls -t "$REPO"/target/*/build/ffmpeg-sys-next-*/out/bindings.rs | head -1)"
SYS_SRC="$(dirname "$(ls -d ~/.cargo/registry/src/*/ffmpeg-sys-next-7.1.*/src/lib.rs | tail -1)")"

rm -rf "$WORK/fake-sys" "$WORK/enc" "$WORK/snap"
mkdir -p "$WORK/fake-sys/src" "$WORK/enc"
cp -r "$SYS_SRC/avutil" "$WORK/fake-sys/src/"
python3 - "$BINDINGS" "$WORK/fake-sys/src/bindings.rs" <<'EOF'
import re, sys
s = open(sys.argv[1]).read()
s = re.sub(r'#\[allow\(clippy::unnecessary_operation, clippy::identity_op\)\]\nconst _: \(\) = \{.*?\n\};\n', '', s, flags=re.S)
open(sys.argv[2], 'w').write(s)
EOF
{ echo '#![allow(warnings)]'; sed 's#include!(concat!(env!("OUT_DIR"), "/bindings.rs"));#include!("bindings.rs");#' "$SYS_SRC/lib.rs"; } > "$WORK/fake-sys/src/lib.rs"
cat > "$WORK/fake-sys/Cargo.toml" <<'EOF'
[package]
name = "ffmpeg-sys-next"
version = "7.1.3"
edition = "2015"
[dependencies]
libc = "0.2"
EOF

# Snapshot the sibling crates from git HEAD so concurrent edits don't interfere.
( cd "$REPO" && for c in cap-capture cap-clock cap-types; do
    for f in $(git ls-tree -r --name-only HEAD "crates/$c"); do
      mkdir -p "$WORK/snap/$(dirname "${f#crates/}")"
      git show "HEAD:$f" > "$WORK/snap/${f#crates/}"
    done
  done )

cat > "$WORK/Cargo.toml" <<'EOF'
[workspace]
resolver = "2"
members = ["fake-sys", "enc", "snap/cap-capture", "snap/cap-clock", "snap/cap-types"]
[workspace.package]
version = "0.1.0"
edition = "2021"
license = "UNLICENSED"
publish = false
[workspace.dependencies]
cap-types = { path = "snap/cap-types" }
cap-clock = { path = "snap/cap-clock" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tracing = "0.1"
EOF
# Same manifest as the real crate, with ffmpeg-sys-next swapped for fake-sys.
sed -e "s#^\[package\]#[lib]\npath = \"$REPO/crates/cap-encode/src/lib.rs\"\n\n[package]#" \
    -e 's#^cap-types.workspace = true#cap-types = { path = "../snap/cap-types" }#' \
    -e 's#^cap-capture.workspace = true#cap-capture = { path = "../snap/cap-capture" }#' \
    -e 's#^ffmpeg-sys-next = .*#ffmpeg-sys-next = { path = "../fake-sys" }#' \
    -e 's#^\(version\|edition\|license\|publish\).workspace = true#\1.workspace = true#' \
    -e 's#^thiserror.workspace = true#thiserror = "2"#' \
    -e 's#^tracing.workspace = true#tracing = "0.1"#' \
    "$REPO/crates/cap-encode/Cargo.toml" > "$WORK/enc/Cargo.toml"

cd "$WORK"
for t in x86_64-pc-windows-msvc aarch64-apple-darwin; do
  echo "== cargo check --target $t"
  cargo check -p cap-encode --target "$t"
done
