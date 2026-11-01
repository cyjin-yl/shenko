#!/usr/bin/env bash
# Build the real QuickJS-NG command target, NOT its no-_start reactor.
set -euo pipefail
: "${QUICKJS_SOURCE:?Set QUICKJS_SOURCE to a dedicated quickjs-ng/quickjs checkout}"
: "${WASI_SDK_PATH:?Set WASI_SDK_PATH to retained wasi-sdk 34}"
: "${REPL_PORT_OUTPUT:?Set REPL_PORT_OUTPUT to a new persistent output directory}"
expected=d7215b0ad3fc2c562e8317fd9bca058fd4863804
actual=$(git -C "$QUICKJS_SOURCE" rev-parse HEAD)
[[ "$actual" == "$expected" ]] || { printf '%s\n' 'QuickJS source identity mismatch' >&2; exit 1; }
git -C "$QUICKJS_SOURCE" diff --exit-code --quiet
git -C "$QUICKJS_SOURCE" diff --cached --exit-code --quiet
[[ ! -e "$REPL_PORT_OUTPUT" ]] || { printf '%s\n' 'Output must not exist' >&2; exit 1; }
mkdir -p "$REPL_PORT_OUTPUT"
cmake -S "$QUICKJS_SOURCE" -B "$REPL_PORT_OUTPUT/build" \
  -DCMAKE_TOOLCHAIN_FILE="$WASI_SDK_PATH/share/cmake/wasi-sdk-p1.cmake" \
  -DCMAKE_BUILD_TYPE=Release -DQJS_WASI_REACTOR=OFF -DQJS_BUILD_EXAMPLES=OFF
cmake --build "$REPL_PORT_OUTPUT/build" --target qjs_exe
cp "$REPL_PORT_OUTPUT/build/qjs.wasm" "$REPL_PORT_OUTPUT/qjs.wasm"
printf '%s\n' "$expected" > "$REPL_PORT_OUTPUT/source-commit"
sha256sum "$REPL_PORT_OUTPUT/qjs.wasm" > "$REPL_PORT_OUTPUT/qjs.wasm.sha256"
