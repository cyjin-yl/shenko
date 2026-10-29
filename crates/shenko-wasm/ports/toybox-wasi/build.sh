#!/usr/bin/env bash
# Build toybox 0.8.15 standalone applets with WASI preview1.
# Usage: build.sh <wasi-sdk-dir> <toybox-0.8.15.tar.gz> <new-run-dir>
#
# The caller supplies a fresh directory on persistent storage. This script
# claims it with mkdir (no reuse), retains the probe, applets, receipt and
# logs, and unpacks a private vendor tree there. Patch 0008 replaces upstream
# cleanup calls with retain-move: descriptor-confined renameat2 archive moves
# that reject ancestor swaps, cross-device copies and receipt failures while
# always preserving the source. No rm command is masked and no existing file
# is deleted. Existing trees, project inputs and previous runs are never
# build directories.
# Nothing is downloaded. Requires host cc, GNU make, bash, patch, tar and
# coreutils in addition to the selected WASI SDK.
set -euo pipefail

# Command substitution strips every trailing newline, and the sentinel only
# protects bytes inside one — the caller's own $() would lose them again.
# canon() therefore assigns the destination variable directly (printf -v)
# from substitution output whose sentinel was stripped *inside* the same
# capture, so the caller's variable never passes through $() at all.
# Usage: canon <varname> <path>
canon() {
  local __v="$1" __raw
  __raw="$(realpath -e -- "$2" || exit 2; printf x)" || exit 2
  __raw="${__raw%$'\n'x}"
  printf -v "$__v" '%s' "$__raw"
}

canon WASI_SDK "${1:?usage: build.sh <wasi-sdk-dir> <toybox-tarball> <new-run-dir>}"
canon SOURCE_ARCHIVE "${2:?pass the pinned toybox-0.8.15.tar.gz}"
RUN_DIR="${3:?pass a new persistent run directory whose parent exists}"
# ${0%/*} splits on string length, not a subprocess: no nested substitution.
__port_arg="${0%/*}"
[ -n "$__port_arg" ] || __port_arg=/
[ "$__port_arg" = "$0" ] && __port_arg=.
canon PORT_DIR "$__port_arg"
unset __port_arg
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
canon RUN_DIR "$RUN_DIR"
mkdir -- "$RUN_DIR/applets" "$RUN_DIR/vendor" "$RUN_DIR/scratch" "$RUN_DIR/logs" "$RUN_DIR/retired"
cp -- "$PORT_DIR/miniconfig.wasi" "$RUN_DIR/miniconfig.wasi"
printf 'action\toriginal_path\tretained_path\n' > "$RUN_DIR/retired/INDEX.tsv"
export HOSTCC="${HOSTCC:-cc}"
"$HOSTCC" -O2 -Wall -o "$RUN_DIR/retain-move" "$PORT_DIR/retain-move.c" \
  > "$RUN_DIR/logs/retain-move.log" 2>&1 || {
    echo "retention helper build failed; retained log: $RUN_DIR/logs/retain-move.log" >&2
    exit 2
  }
export SHENKO_TOYBOX_RUN_DIR="$RUN_DIR"
export SHENKO_TOYBOX_RETAIN="$RUN_DIR/retain-move"

# Override upstream output locations, not just the applet destination. In
# particular, genconfig may move/remove KCONFIG_ALLCONFIG if it aliases the
# output config; only the run-owned copy is ever offered to that script.
export CC CPUS=1 MAKEFLAGS=-j1 NOSTRIP=1
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
  printf 'retirement_index=%s/retired/INDEX.tsv\n' "$RUN_DIR"
  for f in "$RUN_DIR"/applets/*; do
    [ -f "$f" ] && sha256sum -- "$f"
  done
} > "$RUN_DIR/BUILD-RECEIPT.txt"
printf 'Built %s applets: %s/applets/\nReceipt: %s/BUILD-RECEIPT.txt\n' \
  "$built" "$RUN_DIR" "$RUN_DIR"
