#!/usr/bin/env bash
# Build toybox 0.8.15 standalone applets with WASI preview1.
# Usage: build.sh <wasi-sdk-dir> <toybox-0.8.15.tar.gz> <new-run-dir>
#
# The caller supplies a fresh directory on persistent storage. This script
# claims it with mkdir (no reuse), retains the probe, applets, receipt and
# logs, and unpacks a private vendor tree there. Upstream single.sh/make.sh
# remove their own output/generated/object files while rebuilding: all such
# paths, including mktemp scratch, are confined to this new run. Existing
# source trees, project inputs and previous runs are never build directories.
# Nothing is downloaded. Requires host cc, GNU make, bash, patch, tar and
# coreutils in addition to the selected WASI SDK.
set -euo pipefail

WASI_SDK="$(realpath -e -- "${1:?usage: build.sh <wasi-sdk-dir> <toybox-tarball> <new-run-dir>}")"
SOURCE_ARCHIVE="$(realpath -e -- "${2:?pass the pinned toybox-0.8.15.tar.gz}")"
RUN_DIR="${3:?pass a new persistent run directory whose parent exists}"
PORT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
SOURCE_SHA256=52452b68a6cf50f9a9728b0f844b03c4dd1f70463675b202e6cf0475bda3934f

actual_sha256="$(sha256sum -- "$SOURCE_ARCHIVE")"
actual_sha256="${actual_sha256%% *}"
[ "$actual_sha256" = "$SOURCE_SHA256" ] || {
  echo "source checksum mismatch: expected toybox 0.8.15 ($SOURCE_SHA256)" >&2
  exit 2
}
CC="$WASI_SDK/bin/clang"
[ -x "$CC" ] || { echo "no clang at $CC" >&2; exit 2; }
# mkdir itself is the exclusive claim; a separate exists check would race.
mkdir -- "$RUN_DIR" || {
  echo "cannot claim $RUN_DIR; choose a fresh path, never reuse a prior run" >&2
  exit 2
}
RUN_DIR="$(cd "$RUN_DIR" && pwd -P)"
mkdir -- "$RUN_DIR/applets" "$RUN_DIR/vendor" "$RUN_DIR/scratch" "$RUN_DIR/logs"
cp -- "$PORT_DIR/miniconfig.wasi" "$RUN_DIR/miniconfig.wasi"

# Override upstream output locations, not just the applet destination. In
# particular, genconfig may move/remove KCONFIG_ALLCONFIG if it aliases the
# output config; only the run-owned copy is ever offered to that script.
export CC HOSTCC="${HOSTCC:-cc}" CPUS=1 MAKEFLAGS=-j1 NOSTRIP=1
export CROSS_COMPILE= TMPDIR="$RUN_DIR/scratch"
export GENDIR="$RUN_DIR/vendor/generated"
export UNSTRIPPED="$GENDIR/unstripped"
export KCONFIG_CONFIG="$RUN_DIR/vendor/.config"
unset KCONFIG_ALLCONFIG
export CFLAGS="--target=wasm32-wasi --sysroot=$WASI_SDK/share/wasi-sysroot \
  -O2 -fno-builtin -Wall -Wno-unused-function \
  -include $PORT_DIR/wasi-compat.h"
export LDFLAGS=""

# Both source and binary probe are retained. There is no /tmp path and no
# cleanup trap that could erase a previous run or its evidence.
printf '%s\n' 'int main(void){return 0;}' > "$RUN_DIR/probe.c"
"$CC" --target=wasm32-wasi --sysroot="$WASI_SDK/share/wasi-sysroot" \
  -o "$RUN_DIR/probe.wasm" "$RUN_DIR/probe.c" \
  > "$RUN_DIR/logs/probe.log" 2>&1 || {
    echo "SDK probe failed; retained log: $RUN_DIR/logs/probe.log" >&2
    exit 2
  }

tar --extract --gzip --file "$SOURCE_ARCHIVE" --directory "$RUN_DIR/vendor" \
  --strip-components=1 --no-same-owner
cd "$RUN_DIR/vendor"
for p in "$PORT_DIR"/patches/*.patch; do
  patch --batch --fuzz=0 -p1 < "$p" \
    > "$RUN_DIR/logs/$(basename "$p").log" 2>&1 || {
      echo "patch failed: $p; see $RUN_DIR/logs/" >&2
      exit 1
    }
done

# -n is kconfig.c's allnoconfig mode (the Makefile target spelling isn't).
KCONFIG_ALLCONFIG="$RUN_DIR/miniconfig.wasi" scripts/genconfig.sh -n \
  > "$RUN_DIR/logs/genconfig.log" 2>&1

# Config-only symbols (TOYBOX_*, TEST_GLUE) have no applet and are skipped.
mapfile -t configs < <(grep -oE '^CONFIG_[A-Z0-9_]+=y' "$RUN_DIR/miniconfig.wasi" \
  | sed 's/^CONFIG_//; s/=y$//' | tr 'A-Z' 'a-z')
built=0
skipped=()
for name in "${configs[@]}"; do
  egrep -q "TOY[(]${name}[ ,]" toys/*/*.c || { skipped+=("$name"); continue; }
  printf 'Building %s\n' "$name"
  PREFIX="$RUN_DIR/applets/" scripts/single.sh "$name" \
    > "$RUN_DIR/logs/$name.log" 2>&1 || {
      echo "build failed: $name; retained log: $RUN_DIR/logs/$name.log" >&2
      exit 1
    }
  [ -f "$RUN_DIR/applets/$name" ] || {
    echo "single.sh produced no artifact for $name" >&2
    exit 1
  }
  built=$((built+1))
done

{
  printf 'wasi_sdk=%s\nsource_archive=%s\nsource_sha256=%s\nCPUS=1\nbuilt=%s\n' \
    "$WASI_SDK" "$SOURCE_ARCHIVE" "$SOURCE_SHA256" "$built"
  printf 'skipped_config_only=%s\n' "${skipped[*]:-none}"
  for f in "$RUN_DIR"/applets/*; do
    [ -f "$f" ] && sha256sum -- "$f"
  done
} > "$RUN_DIR/BUILD-RECEIPT.txt"
printf 'Built %s applets: %s/applets/\nReceipt: %s/BUILD-RECEIPT.txt\n' \
  "$built" "$RUN_DIR" "$RUN_DIR"
