# Shenko REPL ([§19](repl.md#spec-19)、[§19.1](repl.md#spec-19-1))

**内核定位。** 参考插件，建立在[内核合同](kernel.md)之上。每次执行是带回执的效果（[内核 5](kernel.md#kernel-5)），处理器经[内核 7](kernel.md#kernel-7)登记。

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

<a id="spec-19"></a>
<!-- migrated-contract: SPEC:19 -->
# 19 有状态 REPL 插件

REPLProvider 提供 create/execute/inspect/checkpoint/restore/fork/install_dependencies/cancel/dispose。每个 REPL 绑定 task、worker、scope、runtime/ABI、环境 generation、依赖锁、工作目录和资源限制。主体只委派计算任务，不能以 REPL 绕过禁止直接执行代码/写文件的约束。

| 目标 | 当前可用研究路线 | 兼容表述 |
| --- | --- | --- |
| Python in WASM | CPython WASI、Pyodide 各自有实际实现 | 前者面向 WASI；后者依赖 Emscripten/JS host，需独立 profile |
| JS in WASM | 可评估 Javy/ComponentizeJS 等解释器路线 | 这是 JS backend，不等于完整 Node/Bun |
| Node in WASM | 本轮未核验可直接使用的上游完整 WASM target | 保留移植/兼容目标；native/SSH Node 插件另列 |
| Bun in WASM | 本轮未核验完整 Bun WASM target | Bun 能执行 WebAssembly 不等于 Bun 本体运行于 WASM |

完整 Linux REPL 可使用 [§34](desktop-runtime.md#spec-34) 的 BoxLite adapter 作为候选环境，支持的原生依赖按实际 image/架构验证。这是独立 native runtime 路线；BoxLite 当前磁盘快照不等于活 REPL 的内存对象或外部句柄恢复。

依据：[CPython WASI 平台](https://github.com/python/cpython/tree/main/Platforms/WASI)、[Pyodide 包加载](https://pyodide.org/en/stable/usage/loading-packages.html)、[Javy](https://github.com/bytecodealliance/javy)、[ComponentizeJS](https://github.com/bytecodealliance/ComponentizeJS)、[Node 构建目标](https://github.com/nodejs/node/blob/main/BUILDING.md)、[Bun](https://github.com/oven-sh/bun)。不能用 QuickJS 兼容接口或宿主 Node 悄悄勾选“Node/Bun WASM 完成”。

动态装包通过 PackageProvider 接入锁定依赖与版本 generation。Python 可装兼容纯 Python wheel 与对应 WASM ABI 的二进制 wheel；Pyodide 不是只能装纯 Python，但普通 Linux wheel 仍不能直接用。npm 包要区分纯 JS、Node/Bun 内建 API、native addon、FFI、postinstall 和平台二进制。未知包可以在试验分支验证，失败不会污染当前可用 REPL。

恢复能力分级并对每个 backend 明示：

| 级别 | 保证内容 | 限制 |
| --- | --- | --- |
| R0 工作记录 | cell 源码、输出、异常、文件、依赖锁与执行顺序持久 | 不声称变量还在内存里 |
| R1 对象恢复 | 支持显式可序列化对象/数据集/数组的 checkpoint | 函数闭包、打开文件、socket、线程与 native handle 未必可恢复 |
| R2 VM 状态 | 特定引擎/模块/ABI 下的完整可恢复执行状态 | 必须实现对应外部资源重绑定与版本校验，不能由复制 linear memory 自动获得 |

R0 是所有 backend 的最低保证；有状态会话在进程存活时保留变量，重启后按 R1/R2 能力恢复。需要重放 cell 才恢复时，区分纯计算与有副作用代码。已发送消息、远端写入、付款/提交等不能因 notebook replay 重复执行；保存 operation receipt，重放使用结果替身或重新核验。随机数、时间、网络响应和输入文件版本纳入可复现记录。

REPL 僵死可单独终止并恢复，不拖垮主体。输出包含结构化错误、stdout/stderr、表格/图像/文件引用与截断标记；大对象按需读取，防止塞满模型 context。插件关闭时不占常驻解释器内存，热实例数量受设备 profile 控制。

<!-- /migrated-contract: SPEC:19 -->

<a id="spec-19-1"></a>
<!-- migrated-contract: SPEC:19.1 -->
## 19.1 多 kernel 与统一会话生命周期

2026-10-26 用户补充：需要 Python、Node、Bun、QuickJS 等各种 REPL kernel，并明确要求提交设计。它们是独立可替换的 kernel 插件，共用能力发现、任务绑定和会话生命周期，继续沿用本章 REPLProvider 与 [§41](architecture.md#spec-41) 的执行/恢复合同；不新增一套主体，也不把解释器嵌成不可替换的核心。

统一的是语义接口，不是运行时兼容性。Python、Node、Bun、QuickJS 分别报告实际语言/runtime/ABI、模块与包能力、原生扩展、异步机制、网络和文件能力。JavaScript 不等于 Node API，QuickJS 不冒充 Node/Bun；native、SSH 与 WASM 后端分别标识，不能默认全部 kernel 在 WASM 内或拥有同一种沙箱。包安装与环境变更仍复用既有 PackageProvider/generation，不扩大底座或修改现有安全模型。

除一次性执行外，可选择保留变量和对象的长寿 session；允许多个相互隔离的 session。会话、cell/执行与异步输出分别有稳定标识和归属，统一提供执行、检查、中断、重置、关闭语义：沿用 create/execute/inspect/cancel/dispose，interrupt/reset/close 等前端名称由 adapter 映射，不声称当前已有新的可调用 API。中断请求、任务取消、执行已停止、外部效果已确认与结果未知分别报告；不能用 interrupt 的成功返回证明所有工作都已停止。并发 cell、后台任务和迟到输出按 session/执行身份归属，不串入另一会话；adapter 明示其串行/并行能力。

重置或关闭会话时说明哪些变量、对象和后台任务会失去或仍未决，保留既有工作记录和效果责任。checkpoint/restore/fork 按本章 R0/R1/R2 的实际能力报告；活内存对象不会因统一接口自动跨 runtime 或节点迁移。跨节点接续须显式绑定目标环境、可用 checkpoint、依赖及外部资源恢复条件；仅重放代码不冒称恢复了原内存状态。

主体与 worker 的工具职责不变：主体发现能力、理解结果并委派，worker 通过相应 kernel 执行。浏览器可在合适的 Eval/REPL facade 中自然调用（[§24.6](browser.md#spec-24-6)），但 facade 不给主体新增绕开委派直接执行代码的路径，也不要求每个 kernel 支持相同宿主 API。

<!-- /migrated-contract: SPEC:19.1 -->
