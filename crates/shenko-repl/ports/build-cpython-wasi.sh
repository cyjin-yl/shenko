#!/usr/bin/env bash
# Run only in the serialized development-container build phase.
set -euo pipefail
: "${CPYTHON_SOURCE:?Set CPYTHON_SOURCE to a dedicated CPython checkout}"
: "${WASI_SDK_PATH:?Set WASI_SDK_PATH to retained wasi-sdk 34}"
: "${REPL_PORT_OUTPUT:?Set REPL_PORT_OUTPUT to a new persistent output directory}"
expected=784b305df8821d57fb6fb6c52794ef67bda0a75d
actual=$(git -C "$CPYTHON_SOURCE" rev-parse HEAD)
[[ "$actual" == "$expected" ]] || { printf '%s\n' 'CPython source identity mismatch' >&2; exit 1; }
git -C "$CPYTHON_SOURCE" diff --exit-code --quiet
git -C "$CPYTHON_SOURCE" diff --cached --exit-code --quiet
[[ ! -e "$REPL_PORT_OUTPUT" ]] || { printf '%s\n' 'Output must not exist' >&2; exit 1; }
[[ -x "$WASI_SDK_PATH/bin/clang" ]]
command -v wasmtime >/dev/null
cd "$CPYTHON_SOURCE"
# Upstream CLI builds the exact matching native build-Python first, then
# wasm32-wasip1. No upstream gather/package command (both delete dist/).
python3 Platforms/WASI build --wasi-sdk "$WASI_SDK_PATH" --host-triple wasm32-wasip1 --quiet --logdir cross-build/shenko-logs
build_dir=$(python3 Platforms/WASI path wasi --host-triple wasm32-wasip1)
[[ -f "$build_dir/python.wasm" ]]
mkdir -p "$REPL_PORT_OUTPUT"
cp "$build_dir/python.wasm" "$REPL_PORT_OUTPUT/python.wasm"
# The interpreter needs its matching standard library, not just the module.
cp -a Lib "$REPL_PORT_OUTPUT/Lib"
cp -a "$build_dir" "$REPL_PORT_OUTPUT/wasi-build"
printf '%s\n' "$expected" > "$REPL_PORT_OUTPUT/source-commit"
sha256sum "$REPL_PORT_OUTPUT/python.wasm" > "$REPL_PORT_OUTPUT/python.wasm.sha256"
