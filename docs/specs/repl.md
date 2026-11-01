# Shenko REPL (SPEC §19/§19.1)

`crates/shenko-repl` implements stateful REPL sessions behind one contract,
with two **honest session models**:

| Model | Engine | State between cells | Recovery |
| --- | --- | --- | --- |
| `LiveProcess` | One long-lived owned interpreter process per session (WASI preview1 guest or native host process) | Variables, functions, closures and imported modules persist in interpreter memory while the process lives; the driver also serializes explicitly serializable bindings to the declared state file (R1) | Kill + rebuild: `restore()` terminates the owned process and replays journaled `Pure` cells into a fresh process, starting from the immutable baseline |
| `OneShot` | One independent WASI process per cell (raw kernels without a driver) | Nothing survives between cells | R0 work record only: the journal plus replay of `Pure` cells rebuilds the declared state file |

Driver kernels (`KernelDriver != None`) always run `LiveProcess`; raw kernels
run `OneShot`. The model is reported per kernel in `inspect` and never blurred.

## Cell protocol (driver kernels)

Host → driver framing on the owned process's stdin, per cell:

```
[u64be header_len][header JSON][u64be payload_len][cell source bytes]
header: {"seq", "cell_dir", "state_path", "env_dir"}
```

The driver writes `<cell_dir>/<seq>.out`, `<seq>.err`, and renames
`<seq>.report.json` into place **last**; the host treats the report's
appearance as cell completion. The report (`{ok, error, dropped, state_bytes,
state_verified}`) is what makes the R1 claim auditable: which bindings were
dropped because they are not serializable, and whether the state file
round-tripped. Interrupting a cell kills the process (epoch interrupt under
WASI, host kill natively); the partial streams and a `Cancelled` /
`DeadlineExceeded` outcome are journaled, and the session reports
recovery-required until `restore()` rebuilds it. Interrupt success is never
stop-proof: a guest blocked inside a blocking host call can outlive the
request, and effects already committed stand.

## Recovery ladder (claimed per kernel, never per wish)

- **R0 work record** — cell source, outputs, outcomes, order and environment
  metadata are durable (fsynced journal, content-addressed digests).
- **R1 object checkpoint** — driver kernels additionally serialize explicitly
  serializable objects (pickle for CPython, JSON for JS) to the declared
  state file; `dropped` names everything that did not survive.
- **R2 VM state** — not implemented by any kernel; nothing in this crate
  claims it.

Recovery never re-executes declared-effect cells: `Pure` cells replay to
rebuild state, `DeclaredEffect` cells keep their durable record as the
receipt, `Unknown` cells are reconciliation gaps.

## Dependency installation (candidate environments)

`install_dependencies` installs into a **candidate** directory under
`home/.shenko/envs/.candidate-<uuid>`, verifies it, and only then commits:
an `env` journal record binds the new `environment_generation` to the cell
sequence, the candidate is renamed to `envs/g-<generation>`, and the manifest
follows. A crash between journal and manifest is healed on reopen
(idempotent recompute from the kernel lock plus committed env records). Any
failure discards the candidate; the previous environment keeps serving.
There is no blanket refusal after cells — a verified commit is always
allowed.

- **Wheels**: self-contained ZIP reader (stored + deflate), CRC32 verified,
  per-entry 64 MiB / total 256 MiB ceilings, zip-slip rejected. WASI python
  accepts `none-any` and `wasm32-*wasi*` platform tags (rejects emscripten);
  native python accepts `none-any` and manylinux/musllinux/linux x86_64.
- **npm / bun**: the real installer binary resolved next to the bound
  interpreter runs in the candidate dir; a nonzero exit discards the candidate.
- No network client is implemented here: wheels must already exist on disk;
  npm/bun reach registries only through the host's real network authority.

## Limits (honest)

`ResourceLimits` declarations carry an `enforcement` report: wall-clock is
enforced per cell (epoch interrupt / host kill) whenever set; fuel and memory
reservation are declared but have no mechanism yet and are reported
unenforced.

## Native task surface

Two handler kinds, both following the `wasi.exec` ledger pattern
(T1 `arm_external_operation` → blocking op under `spawn_blocking` → sealed
frozen input / observation / bounded normalized result →
`record_external_evidence`; InFlight-after-recovery settles E1 unknown with
no second attempt):

- `repl.session` v1 — `list_kernels`, `create`, `inspect`, `checkpoint`,
  `restore`, `fork`, `reset`, `cancel`, `dispose`, `install_dependencies`.
- `repl.cell` v1 — `execute` (one journaled cell; `cell_kind` is the
  caller-declared replay boundary, default `unknown`).

Interpreter modules are admitted only from the canonical
`<data>/repl/kernels` tree and wheels from `<data>/repl/wheels`, both by
canonical-containment check. Resolution refusals (`REPL_SESSION_UNAVAILABLE`,
`REPL_KERNEL_UNAVAILABLE`, admission refusals) are recorded as the task's
explicit result.

## Named kernel targets

`python-wasi` and `quickjs-wasm` are Bindable WASI routes (live sessions via
the embedded drivers; QuickJS needs a qjs WASI build exposing the `std`
module). `pyodide`, `node-wasm` and `bun-wasm` are Unavailable (no verified
upstream WASM target; stays an obligation, not satisfied by the native
routes). `python-native-host`, `node-native-host`, `bun-native-host` are
additional Bindable native routes — real sessions, but they never satisfy a
WASM target.
