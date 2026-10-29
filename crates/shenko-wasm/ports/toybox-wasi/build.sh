#!/usr/bin/env bash
# Build WASI preview1 toybox applets from upstream toybox 0.8.15.
#
# Produces one standalone .wasm per applet — NO multiplexer (toybox's
# self-exec machinery cannot run on WASI preview1). Uses scripts/single.sh
# per applet, which builds allnoconfig + just that command, so cross-applet
# blockers never leak in. See ACQUISITION.md for inputs, SMOKE.md for checks.
#
# Usage:
#   ./build.sh /path/to/wasi-sdk-34.0-x86_64-linux [src-dir] [out-dir]
#
# Requires: wasi-sdk toolchain (bin/clang targeting wasm32-wasi + wasi-libc
#           sysroot), host cc (for toybox's kconfig/mkflags helpers), GNU make,
#           bash, sed. Downloads NOTHING; all inputs are local paths.

set -euo pipefail

WASI_SDK="${1:?usage: build.sh <wasi-sdk-dir> [src-dir] [out-dir]}"
PORT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC_DIR="${2:-$PORT_DIR/toybox-0.8.15}"
OUT_DIR="${3:-$(pwd)/out-wasi}"

[ -d "$SRC_DIR/scripts" ] || {
  echo "toybox source not found at $SRC_DIR — unpack toybox-0.8.15.tar.gz"
  echo "and apply patches/ first (see ACQUISITION.md)." >&2
  exit 2
}
CC="$WASI_SDK/bin/clang"
[ -x "$CC" ] || { echo "no clang at $CC" >&2; exit 2; }
# A sysroot probe: fail early with a clear message if the SDK is wrong.
echo 'int main(void){return 0;}' > /tmp/.toybox-wasi-probe.c
"$CC" --target=wasm32-wasi --sysroot="$WASI_SDK/share/wasi-sysroot" \
  -o /tmp/.toybox-wasi-probe.wasm /tmp/.toybox-wasi-probe.c \
  || { echo "clang cannot produce a wasm32-wasi binary with this SDK" >&2; exit 2; }
rm -f /tmp/.toybox-wasi-probe.*

export HOSTCC="${HOSTCC:-cc}"
export NOSTRIP=1
export CFLAGS="--target=wasm32-wasi --sysroot=$WASI_SDK/share/wasi-sysroot \
  -O2 -fno-builtin -Wall -Wno-unused-function \
  -include $PORT_DIR/wasi-compat.h"
export LDFLAGS=""

# Patches must be applied to $SRC_DIR beforehand (see ACQUISITION.md).
grep -q '__wasi__' "$SRC_DIR/lib/xwrap.c" \
  || { echo "patches not applied to $SRC_DIR" >&2; exit 2; }

cd "$SRC_DIR"
mkdir -p "$OUT_DIR"

# Generate headers once (kconfig needs the host compiler for its tools).
KCONFIG_ALLCONFIG="$PORT_DIR/miniconfig.wasi" \
  scripts/genconfig.sh allnoconfig

# Each CONFIG_FOO=y in miniconfig maps to a NEWTOY(FOO) somewhere under toys/.
# Derive the applet list from the same file so miniconfig stays authoritative;
# symbols without a NEWTOY entry (TEST_GLUE, TOYBOX_*) are skipped by egrep.
mapfile -t configs < <(grep -oE '^CONFIG_[A-Z0-9_]+=y' "$PORT_DIR/miniconfig.wasi" \
                        | sed 's/^CONFIG_//; s/=y$//' | tr 'A-Z' 'a-z')
built=0
skipped=()
for name in "${configs[@]}"; do
  upper="$(echo "$name" | tr 'a-z' 'A-Z')"
  egrep -q "TOY[(]${name}[ ,]" toys/*/*.c || { skipped+=("$name"); continue; }
  echo "== $name"
  PREFIX="$OUT_DIR/" scripts/single.sh "$name" || {
    echo "FAILED: $name" >&2; exit 1;
  }
  mv "$OUT_DIR/$name" "$OUT_DIR/$name.wasm" 2>/dev/null || true
  built=$((built+1))
done

echo
echo "built $built standalone WASI applets -> $OUT_DIR"
[ ${#skipped[@]} -eq 0 ] || echo "skipped (no NEWTOY, config-only): ${skipped[*]}"
