#!/usr/bin/env bash
# afterglow built and staged for release: afterglow.exe, its service in tools\, the
# SudoVDA install scripts, the licence and where the source is. The signed SudoVDA
# driver package is staged by the workflow after it.
# usage: stage.sh OUT_DIR [VERSION] [TARGET]   (TARGET: a Rust target, default the host's)
set -euo pipefail
OUT=$(realpath -m "$1")
VER=${2:-0.0.0}
TARGET=${3:-}
HERE=$(cd "$(dirname "$0")/.." && pwd)
cd "$HERE"

export AFTERGLOW_VERSION="$VER"
cargo test --locked ${TARGET:+--target "$TARGET"}
cargo build --release --locked ${TARGET:+--target "$TARGET"}
BIN=target/${TARGET:+$TARGET/}release

rm -rf "$OUT"
mkdir -p "$OUT/tools" "$OUT/drivers/sudovda"
cp "$BIN/afterglow.exe" "$OUT/"
cp "$BIN/afterglowsvc.exe" "$OUT/tools/"
cp drivers/sudovda/install.bat drivers/sudovda/uninstall.bat "$OUT/drivers/sudovda/"
cp LICENSE "$OUT/LICENSE.txt"
cp NOTICE "$OUT/NOTICE.txt"
REPO=${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-pmghartman/afterglow}
cat > "$OUT/SOURCE.txt" <<TXT
afterglow $VER: phosphorus's streaming host.
GNU General Public License version 3 (LICENSE.txt). Its complete source is host/ in
$REPO at commit ${GITHUB_SHA:-unknown} (tag v$VER), its Rust dependencies at the
versions host/Cargo.lock pins.
TXT
echo "$VER" > "$OUT/VERSION"
ls -la "$OUT" "$OUT/tools"
