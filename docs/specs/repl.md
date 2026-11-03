# Shenko REPL (SPEC §19/§19.1)

`crates/shenko-repl` implements stateful REPL sessions behind one contract,
with two **honest session models**:

| Model | Engine | State between cells | Recovery |
| --- | --- | --- | --- |
| `LiveProcess` | One long-lived owned interpreter process per session (WASI preview1 guest or native host process) | Variables, functions, closures and imported modules persist in interpreter memory while it lives; driver-supported objects also have explicit R1 checkpoints | Stop/join, verify and restore immutable per-cell object checkpoints, then start a fresh interpreter; authored source and effects are not replayed |
| `OneShot` | One independent WASI process per cell (raw kernels without a driver) | No interpreter-memory guarantee | Restore exact declared-state byte checkpoints for Pure cells when available; otherwise replay only successfully completed Pure cells with sealed host-input receipts |

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
round-tripped. The host reads that report once and retains the same value
beside the recorded outcome; a later reread cannot silently erase it. A
Python cell exception is an unsuccessful cell with its traceback in the
cell's stderr; the report may refer to that file rather than copy its body.
Interrupting a cell kills the process (epoch interrupt under
WASI, host kill natively); the partial streams and a `Cancelled` /
`DeadlineExceeded` outcome are journaled, and the session reports
recovery-required until `restore()` restores its explicit checkpoint. Interrupt success is never
stop-proof: a guest blocked inside a blocking host call can outlive the
request, and effects already committed stand.

## Recovery ladder (claimed per kernel, never per wish)

- **R0 work record** — cell source, outputs, outcomes, order and environment
  metadata are durable (fsynced journal, content-addressed digests).
- **R1 object checkpoint** — driver kernels serialize supported objects to
  the declared state file. Each completed checkpoint is copied into an
  immutable cell-owned file and its size/digest enters the cell journal.
  CPython restores only restricted data pickles, never arbitrary encoded
  globals/callables. Native JavaScript uses the real REPL evaluator and its
  completion/scope view; QuickJS's JSON checkpoint covers enumerable global
  properties, not inaccessible lexical bindings. Non-serializable objects
  are reported in `dropped`; this is not R2 lexical/closure restoration.
- **R2 VM state** — not implemented by any kernel; nothing in this crate
  claims it.

Recovery never re-executes declared-effect cells. Live backends restore
explicit R1 objects, including checkpoints taken after effect cells; effects
themselves remain historical receipts. Missing final checkpoints keep the
durable restore fence rather than silently starting with empty variables.
Native/SSH and unrecorded live source replay are unsupported. Raw WASI
`Pure` remains a caller declaration, not proof of deterministic inputs.

One-shot WASI records ordered host-input receipts through the owned P1Host:
random bytes, clock values, successful read-only opens and file/stdin reads.
Replay opens virtual receipt descriptors and never consults fresh file
contents. The binding includes actual module, host-ABI source identity,
argv/environment/mount closure, environment generation, source, scope and
cell. Each request and returned value participates in a sequence/hash chain;
the record is synced before the guest observes input. The final receipt is
part of `CellRecord`, not merely a nearby mutable file. Replay requires the
exact receipt and complete transcript consumption. Historical records with
no receipt gain no replay authority.

New journal writers emit schema 4. Historical schema 3 remains raw evidence
with its original version, but cannot carry newly invented input/checkpoint
authority. No historical bytes are rewritten during admission. If a live
restore lacks its final verified checkpoint, `restore` reports
`recovery_required` rather than success. An explicit `reset` may abandon that
attempt through a distinct `restore_abandoned` boundary and reset declared
state while retaining all work/effect history; it never fabricates a
successful restore receipt.

Recorded stdout/stderr writes are matched before materializing the new
invocation's owned replay output artifacts. Unsupported observations
(including fresh file metadata and poll results), writable opens and
filesystem effects fail closed during replay; no unobserved value is
silently fetched and no original external effect is repeated. This is a
bounded supported replay surface, not universal WASI determinism. Native
code can escape a process group by daemonizing: group absence is not proof
that such descendants stopped.

## Dependency installation (candidate environments)

`install_dependencies` installs into a **candidate** directory under
`home/.shenko/envs/.candidate-<uuid>`, verifies it, and only then commits:
the candidate is renamed to `envs/g-<generation>` and the directory is
synced before an `env` journal record binds the new `environment_generation`
to the cell sequence; the manifest follows. A crash before the journal
leaves an unreferenced directory, not a journal pointing to absent packages.
A crash between journal and manifest is healed on reopen
(idempotent recompute from the kernel lock plus committed env records). Any
failure discards the candidate; the previous environment keeps serving.
There is no blanket refusal after cells — a verified commit is always
allowed.

- **Wheels**: self-contained ZIP reader (stored + deflate), CRC32 verified,
  per-entry 64 MiB / total 256 MiB ceilings, zip-slip rejected. Admission
  expands compressed filename tags and requires an exact compatible tag
  reported by the executing Python driver. At startup the driver captures
  `packaging.tags.sys_tags()` when available; otherwise it reports only
  version-correct pure-Python tags. No host Linux assumption or arbitrary
  `wasi` substring admits a binary wheel. Execute a Python cell first to
  retain the actual interpreter compatibility observation in its journal.
- **npm / bun**: the real installer binary resolved next to the bound
  interpreter runs in the candidate dir under the native address-space cap,
  wall-clock cancellation and independent cancellation token. Its owned
  process group must disappear before capacity is released. Successful
  locks bind a digest of the actual installed tree (including transitive
  dependencies and literal symlinks); wheel locks bind the archive bytes.
  The previous environment is copied into the candidate so incremental
  installation does not silently remove earlier dependencies.
- No network client is implemented here: wheels must already exist on disk;
  npm/bun reach registries only through the host's real network authority.

## Limits (honest)

`ResourceLimits` carries an individual `enforcement` report. Wall-clock
requests cancellation per cell; a blocking host call may delay observed
termination. WASI stores enable Wasmtime instruction fuel and a linear-memory
limiter, including initial allocation and failing growth. Fuel belongs to
the actual executing store: one-shot fuel is per invocation, live-process
fuel is cumulative for that interpreter lifetime, never reset by another
thread at a purported cell boundary. Native children inherit an `RLIMIT_AS`
hard address-space ceiling; this is per process, not aggregate RSS across
descendants. Native instruction fuel has no corresponding mechanism and is
reported unenforced. Cancellation kills the owned native process group and
retains worker ownership until the interpreter is reaped. None of these
source mechanisms is a retained native execution receipt.

Upgrade participation uses `SessionStore::quiesce_for_upgrade()` and
`resume_after_upgrade_abort()`. Quiescence fences admission and existing
session execution, stops and joins owned interpreters, and returns actual
stopped/idle identities. Partial failure retains the fence and recovery
ownership; abort resumption propagates restoration failures rather than
claiming that a stopped interpreter is still live.

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

### Source-bound build routes

`crates/shenko-repl/ports/build-cpython-wasi.sh` accepts a dedicated checkout
at CPython commit `784b305df8821d57fb6fb6c52794ef67bda0a75d`, the retained
WASI SDK 34, and a new persistent output path. It invokes the upstream
`Platforms/WASI build` CLI for `wasm32-wasip1` and retains the module,
matching standard library and complete cross-build metadata. Source:
[CPython WASI instructions](https://github.com/python/cpython/blob/784b305df8821d57fb6fb6c52794ef67bda0a75d/Platforms/WASI/README.md).
Prerequisites are the exact source checkout, native compiler/make, a supported
host Python and Wasmtime on PATH; the SDK alone is not an interpreter.

`ports/build-quickjs-wasi.sh` uses QuickJS-NG commit
`d7215b0ad3fc2c562e8317fd9bca058fd4863804` (0.18.0), CMake and SDK 34's
`wasi-sdk-p1.cmake` to build upstream `qjs_exe`, including its `std` module.
It explicitly disables the reactor target because a reactor without `_start`
cannot satisfy this command adapter. Source:
[QuickJS-NG build definitions](https://github.com/quickjs-ng/quickjs/blob/d7215b0ad3fc2c562e8317fd9bca058fd4863804/CMakeLists.txt).
Both scripts refuse a different source commit or existing output directory;
neither has been executed in this source-only phase. Obtain the pinned
upstream checkout before the serialized build; no interpreter artifact or
successful state/package/cancellation proof is claimed by these recipes.
