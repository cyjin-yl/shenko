#!/usr/bin/env bash
# Build the guest side of the Shenko WASI network transport
# (`cap.network.tcp/1`): a static archive of the descriptor shim plus the real
# smoke command that exercises it.
#
# Usage: build.sh <wasi-sdk-dir> <new-run-dir>
#
# The caller supplies a fresh directory on persistent storage; this script
# claims it with mkdir (no reuse) and never builds in an existing tree or in
# this port directory. Nothing is downloaded: pass a wasi-sdk that is already
# on disk. The resulting archive is an input to the git/gh/tea/openssh ports;
# no tool artifact is produced or claimed here.
set -euo pipefail

WASI_SDK="${1:?usage: build.sh <wasi-sdk-dir> <new-run-dir>}"
RUN_DIR="${2:?pass a new persistent run directory whose parent exists}"

PORT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
CC="$WASI_SDK/bin/clang"
AR="$WASI_SDK/bin/llvm-ar"
[ -x "$CC" ] || { echo "no clang at $CC" >&2; exit 2; }
[ -x "$AR" ] || { echo "no llvm-ar at $AR" >&2; exit 2; }

# mkdir itself is the exclusive claim; a separate exists check would race.
mkdir -- "$RUN_DIR" || {
  echo "cannot claim $RUN_DIR; choose a fresh path, never reuse a prior run" >&2
  exit 2
}
mkdir -- "$RUN_DIR/obj" "$RUN_DIR/logs"

CFLAGS=(--target=wasm32-wasip1 -O2 -std=c11 -Wall -Wextra -Werror
  -I "$PORT_DIR/include")

"$CC" "${CFLAGS[@]}" -c "$PORT_DIR/src/shenko_net.c" \
  -o "$RUN_DIR/obj/shenko_net.o" > "$RUN_DIR/logs/shenko_net.log" 2>&1 || {
    echo "shim compile failed; retained log: $RUN_DIR/logs/shenko_net.log" >&2
    exit 2
  }
"$AR" rcs "$RUN_DIR/libshenko_net.a" "$RUN_DIR/obj/shenko_net.o"

"$CC" "${CFLAGS[@]}" "$PORT_DIR/smoke/transport-smoke.c" \
  "$RUN_DIR/libshenko_net.a" -o "$RUN_DIR/transport-smoke.wasm" \
  > "$RUN_DIR/logs/transport-smoke.log" 2>&1 || {
    echo "smoke command build failed; retained log: $RUN_DIR/logs/transport-smoke.log" >&2
    exit 2
  }

# The archive leaves no unresolved import other than the preview1 calls the
# shim deliberately uses; record what it really linked instead of asserting it.
"$WASI_SDK/bin/llvm-nm" -u "$RUN_DIR/libshenko_net.a" \
  > "$RUN_DIR/imports.txt" 2>/dev/null || true
sha256sum -- "$RUN_DIR/libshenko_net.a" "$RUN_DIR/transport-smoke.wasm" \
  > "$RUN_DIR/SHA256SUMS"

printf 'run_dir\t%s\nshim\t%s\nsmoke\t%s\n' \
  "$RUN_DIR" "$RUN_DIR/libshenko_net.a" "$RUN_DIR/transport-smoke.wasm" \
  > "$RUN_DIR/BUILD-RECEIPT.txt"
cat "$RUN_DIR/BUILD-RECEIPT.txt"
