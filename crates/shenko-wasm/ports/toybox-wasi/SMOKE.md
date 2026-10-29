# Smoke checks for the toybox WASI port

Run after `build.sh` produces `out-wasi/*.wasm`. Every artifact must pass
inspection before it is installed — `shenko tool install` re-verifies all of
this (real ABI inspection, not name matching), so a failure here means the
build or port is wrong, not the installer.

## 1. Module inspection (per artifact)

Installation performs the inspection — `inspect_module` requires a real
`_start` export with signature `() -> ()`, a `memory` export, and every
import a `wasi_snapshot_preview1` function whose signature the p1 linker
actually provides. A component-model module is rejected as `WasiComponent`
before anything is written to the store.

## 2. Install + list

```sh
shenko tool install --name toybox-cat --version 0.8.15 --abi wasi_preview1 \
  --file out-wasi/cat.wasm
shenko tool list
```

The record appears with its real digest and `current: true`. Reinstalling
the same bytes under the same version is idempotent; different bytes under
the same `name@version` fail `TOOL_INSTALL_CONFLICT` — bump the version.

## 3. Execute through the real handler

```sh
printf 'hello\n' | shenko tool exec --tool toybox-cat
# → hello   (exit 0, stdout sealed + preview "hello\n")

shenko tool exec --tool toybox-grep -- -c hello /home/notes.txt
# exit 0/1 with the real count; no preopen → TOOL_*/WASI_* error, not a fake pass
```

Stdin flows through the preopened stdin file; stdout/stderr are captured to
files, sealed whole into CAS (bounded preview in the normalized receipt).
A guest that exceeds `timeout_secs` finishes as
`WasiRunOutcome::DeadlineExceeded` with exit 124 — the worker thread is
always joined before settlement, never abandoned.

## 4. Negative checks (must fail honestly)

- `shenko tool exec --tool toybox-env -- ls` → `env: cannot exec a program on WASI`.
- `shenko tool exec --tool toybox-find -- -exec` → `find -exec: no subprocess on WASI`.
- `shenko tool exec --tool toybox-date -- -s @0` → `cannot set date on WASI`.
- `shenko tool exec --tool toybox-stat -- -f` → `stat -f: no statfs on WASI`.
- Installing a renamed random `.wasm` under `toybox-cat` fails inspection
  unless it is a real `_start`+`memory`+p1-import module.

## 5. Effect-status check

`ls`/`cat`-only runs with all-`ReadOnly` mounts settle `EffectStatus::None`;
any run with a read-write mount keeps `Unknown` even on exit 0 — a guest
with write preopens may have written before exiting.
