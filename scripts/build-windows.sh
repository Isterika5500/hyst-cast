#!/usr/bin/env bash
# Сборка Windows-версии на Linux: mingw-w64 + GStreamer из MSYS2 (ucrt64) + упаковка в zip.
#   scripts/build-windows.sh [server.toml]   — с server.toml внутри zip (для друзей)
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=${HYST_CAST_WIN_ROOT:-$HOME/.cache/hyst-cast-win/msys2}
[ -d "$ROOT/ucrt64" ] || python3 scripts/fetch_msys2.py "$ROOT"
export PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_PATH= PKG_CONFIG_LIBDIR=$ROOT/ucrt64/lib/pkgconfig PKG_CONFIG_SYSROOT_DIR=$ROOT
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export PATH=$HOME/.cargo/bin:$PATH
cargo build --release --target x86_64-pc-windows-gnu -p hyst-cast -p hyst-cast-server
OUT=target/windows/hyst-cast
python3 scripts/bundle_windows.py target/x86_64-pc-windows-gnu/release/hyst-cast.exe "$ROOT/ucrt64" "$OUT"
cp scripts/README-windows.txt "$OUT/README.txt"
if [ -n "${1:-}" ]; then cp "$1" "$OUT/server.toml"; fi
rm -f target/windows/hyst-cast-windows.zip
(cd target/windows && python3 -c 'import shutil; shutil.make_archive("hyst-cast-windows", "zip", ".", "hyst-cast")')
ls -la target/windows/hyst-cast-windows.zip | awk '{printf "zip: %.1f МБ\n", $5/1048576}'
