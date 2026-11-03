# Smoke checks for the toybox WASI port

Run after `build.sh` produces `<run>/applets/<name>`. Every artifact must pass
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
  --file <run>/applets/cat
shenko tool list
```

The record appears with its real digest and `current: true`. Reinstalling
the same bytes under the same version is idempotent; different bytes under
the same `name@version` fail `TOOL_INSTALL_CONFLICT` — bump the version.

## 3. CLI execution (not a NativeTask settlement receipt)

```sh
shenko tool exec --tool toybox-cat --read-only --stdin-file <input-file>
# Prints the input bytes and returns the guest's exit status.

shenko tool exec --tool toybox-grep --read-only -- -c hello /home/notes.txt
# exit 0/1 with the real count; no preopen → TOOL_*/WASI_* error, not a fake pass
```

CLI stdin defaults to empty; `--stdin-file` supplies it. `--read-only`
actually grants only read capabilities on every admitted mount; it is not a
label inferred from the applet name.

For **native T1/E1/T2** observation, use the registered v2 definition with
`mount_profile:"read_only"`, not the CLI path:

```text
[delegate:wasi.exec:2] {"tool":"toybox-cat","mount_profile":"read_only","stdin_b64":"aGVsbG8K","timeout_secs":60}
```

The native handler freezes the actual mount closure before T1, runs those
permissions, and seals complete stdout/stderr into CAS with bounded result
previews. Observe the real operation/attempt/evidence and model-visible
task_result, plus the unchanged source/guest paths and denied guest writes.
A native guest that exceeds `timeout_secs` reports
`WasiRunOutcome::DeadlineExceeded` / `OperationOutcome::Interrupted`, not an
invented guest exit code — the worker is always joined, never abandoned.

## 4. Negative checks (must fail honestly)

- `shenko tool exec --tool toybox-env -- ls` → `env: cannot exec a program on WASI`.
- `shenko tool exec --tool toybox-find -- -exec` → `find -exec: no subprocess on WASI`.
- `shenko tool exec --tool toybox-date -- -s @0` → `cannot set date on WASI`.
- `shenko tool exec --tool toybox-stat -- -f` → `stat -f: no statfs on WASI`.
- `shenko tool exec --tool toybox-tar -- -I gzip -cf x.tgz dir` →
  `subprocess: no fork/exec on WASI` (compression helpers need a child).
- `shenko tool exec --tool toybox-tar -- --to-command` → `tar --to-command:
  no subprocess on WASI`.
- `shenko tool exec --tool toybox-cp -- -s` → `cp -s: no strip subprocess on WASI`.
- `shenko tool exec --tool toybox-chroot --` → `chroot: no privilege boundary
  on WASI`.
- Installing a renamed random `.wasm` under `toybox-cat` fails inspection
  unless it is a real `_start`+`memory`+p1-import module.

`uptime` is deliberately not built: `sysinfo()` and the utmp database do
not exist on preview1, so there is no honest uptime/load/user count. It is
omitted from `miniconfig.wasi`, not stubbed to a fake success. `date`/`sleep`
are real; `uname`/`arch` report the fixed wasm32-wasip1 ambient values.

## 5. Effect-status check

Only an actual all-ReadOnly closure **and** terminal guest observation can
settle `EffectStatus::None` and T2. Tool names never determine this profile.
Any native RW closure stays `Unknown`/`Reconciling` even on exit 0; restart
must preserve that operation without dispatching a second guest. Cancelled
outcomes stay Cancelled; a deadline is Interrupted, not proof of no effects.
