# 具名 WASM 工具：安装与执行（C06）

本文记录 named WASM tool 的真实状态、接口和获取配方。SPEC §8.2/§8.3/§9/§18.8；
需求 R008/R014/R019/R101；验收 case A09（仅部分推进，未完成 A09 要求的
Git/SSH 端到端认证）。

## 架构（authority = installation-unit inventory）

工具的注册权威是节点既有的安装单元库存（`unit_versions`/`unit_current`
+ 内联对象存储），**不是** `<data>/tools` 下任何文件：

- **`shenko tool install`** 把工件字节构成 schema-3 `UnitExport`
  （`unit_id = "tool.<name>"`、`revision = <version>`、`artifact_digest` =
  BLAKE3、`config.tool` = 名称/ABI/入口/来源/imports 声明），经
  `Node::handle_unit_request(UnitRequest::Import)` 入库并把字节物化到
  `package_root = <data>/tools` 下的
  `units/<unit_id>/<revision>/<digest>/package.bin`。`tools/` 目录因此只是
  **物化**，不是第二个注册表——node_sync 的 `export_installation_units`
  复制路径天然覆盖工具单元。
- **验收读取**：`Node::tool_unit_facts()`（只读投影）+
  `Node::unit_object_bytes()`（密封对象字节，读取时重验 digest）。
  `shenko-wasm::resolve_tool(facts, spec)` 做 `name[@version]` 解析：
  `TOOL_UNAVAILABLE`/`TOOL_UNSUPPORTED`/`TOOL_AMBIGUOUS`/
  `TOOL_ABI_UNSUPPORTED` 四个显式错误。
- **`Node::open_inspect`** 提供不建库、不迁移的只读打开；`tool list`
  只读、不会初始化或改写任何存储。
- **安装身份**：CLI 执行先通过 `Store::inspect_installation_binding`：将
  主库及其 -journal/-wal 侧车复制进临时快照（原始文件元组零写入——只读
  打开会在 -shm 缺失时写原路径，immutable=1 则会静默漏掉未 checkpoint
  的 WAL，两者都不可用），quick_check 后读绑定，并要求复制窗口内原库
  签名不变；持续变化的库如实报不稳定而非承认撕裂快照。比较 home
  manifest 的 installation_id 与真实 ledger_id；缺库、旧库或异库不会在
  拒绝前被创建/迁移。CLI、native handler 与 `node_sync::open_node` 共用
  subject/genesis/branch 的身份投影：subject_genesis 是可复制的历史，多
  genesis 安装只绑定本安装真正拥有的主体（genesis 或 head 在本 ledger
  提交者），外来 genesis 保留为 provenance 而非身份候选，也绝不删除；
  多个本地候选是完整性错误。node 不匹配则拒绝，native 也检查路径上
  数据库仍匹配活跃 Store。
- **执行重验**：`wasi.exec` 与 `tool exec` 每次执行前用 unit_object_bytes
  重新读封 bytes 并对 recorded digest——被替换的物化文件没有权威，
  外来 `tools/` 目录永远顶替不了已装记录（没有独立 store 可被换绑）。
- **`inspect_module`（`crates/shenko-wasm/src/tool.rs`）**：ABI 按真实
  字节判定——所有 import 必须是 preview1 linker 实际能解析且函数签名
  逐一匹配的 `wasi_snapshot_preview1` 函数；`_start` 必须是 `() -> ()`
  函数、`memory` 必须导出。只查名字或只查导出存在都不够。
- **`Bootstrap::validate_target`**：root 存在非空 `tools/` 时要求其是本
  安装 node 库存的物化根（`Node::materialization_roots` 命中），否则在
  写入任何安装状态前就 `DATA_PATH_CONFLICT`。
- **`wasi.exec` native definition**（`crates/shenko-exec-host/src/wasi_task.rs`）：
  当前为 **v2**：`[delegate:wasi.exec:2]`
  `{tool, args, env, stdin_b64, timeout_secs, mount_profile}`；T1 arm 后 guest 在真实
  WasmRuntime 跑工件本体；`timeout_secs` 是真实 C05 guest 控制
  （`WasiRunControl::deadline`，epoch checkpoint 处中断——CPU 忙 guest
  会终止；阻塞在 host call 里的 guest 如实不受此保证），worker 总是被
  join（无 detached guest）。stdout/stderr 完整字节经 `cas.seal_reader`
  流式落入 CAS（SealedObjectRef 可整流回放），normalized result 只放
  ≤16KiB utf8 preview + `ObjectRef` 身份，序列化远低于 512KiB settlement
  上限。
  durable `abort_request` 经每调用独立 cancellation token 到 guest；取消
  如实记为 `OperationOutcome::Cancelled`，deadline 记为 `Interrupted`，均不
  推导效果。取消控制读取失败会请求停止并 join，然后传播原错误，不忽略失败。
- **显式只读 profile**：`mount_profile:"read_only"`（CLI 为 `--read-only`）
  从已准入挂载保持 source/guest path/持久标志，只将实际 guest 权限降为
  ReadOnly；这份实际闭包完整冻结进 final_input 和 T1 bindings，并由同一
  FrozenInput 移交 worker，不改 home manifest，也不按 `cat`/`ls` 名称猜测。
  默认 `managed_home` 保留原声明权限。
  `EffectStatus` 仅当**实际**闭包全只读且获得真实 terminal outcome 时为
  `None`，因此只读调用可复用现有 T2/task_result 路径。任何 RW 闭包仍是
  `Unknown`/`Reconciling`，没有假 `Partial`/`Complete`，没有自动重放；普通
  RW 结果尚不能完成 T2，这是仍 open 的效果合同缺口，不是本次消除。
  v2 的 definition/implementation/codec/operation/receipt metadata 一起迁移；
  v1 旧任务不能借新 authority 重跑。
- **opaque CAS**：发送端和接收端只展开声明为 JSON 的对象；guest 流
  即使输出 ObjectRef 形状 JSON 也不产生引用权威。同 digest 先 opaque 后
  typed-JSON 的引用仍会补展开真实 child，内容收录与 JSON 展开分别去重。

## 具名目标真实状态（`NAMED_TARGETS`）

| 目标 | 状态 | 真实原因 |
| --- | --- | --- |
| toybox | **unresolved**（移植配方已就绪） | 上游无已发布 WASI 工件；landley/toybox 0.8.15 源码移植配方在 `crates/shenko-wasm/ports/toybox-wasi/`（miniconfig + build.sh + patches + ACQUISITION），需受管构建者以 wasi-sdk-34 构建后逐 applet 安装 |
| gcc-wasm | unresolved | 未核验 WASM 编译器成品；hosted-vs-target 含义未定（§8.2） |
| git | unresolved | 无已核验 wasm32-wasip1 Git client；WASIX ports 需要 WASIX runtime |
| gh | unresolved | wasi-gh 存在但需专用 HTTP/exec host imports（§8.2 W2），本 runtime 不提供 |
| tea | unresolved | 未发现可核验的 Gitea CLI WASM 成品 |
| ssh | unresolved | OpenSSH WASIX port 需要 WASIX runtime + socket imports；wasmtime-45 preview1 均无；宿主 ssh 不算 WASM SSH（R019） |

## 给受管构建者的获取配方（parent action）

所有产物路径只用 owner-persistent 存储：`/tank/shared/projects/shenko/build/…`。

### 1. toybox 真实源码移植（具名目标的主路径）

见 `crates/shenko-wasm/ports/toybox-wasi/ACQUISITION.md`（精确资产清单 +
校验方法）与 `build.sh`。要点：

- 源：`landley/toybox` 0.8.15 tarball（commit a57b091d05ec366dcd4dd737784c03761cb6a4b1，
  sha256 `52452b68a6cf50f9a9728b0f844b03c4dd1f70463675b202e6cf0475bda3934f`）。
- 工具链：wasi-sdk-34.0（`wasm32-wasip1`）；构建脚本还需宿主 C 编译器
  （kconfig/mkflags/instlist 在宿主上运行）。
- 产物：每个 applet 一个独立 `<applet>.wasm`（WASI 无进程，不用
  multiplexer）；然后逐个 `tool install --name <applet>`。
- 能编出的只是 WASI-compatible applet 子集（无 fork/exec/sockets/uid/
  signals/termios）——这是真实移植结果，不得用宿主 toybox 或 WAT 仿制品
  冒充；依赖缺口的操作在运行时明确报错（ENOSYS），不是静默降级。

```sh
# 例（构建完成后）：
./shenko --data-dir <data> tool install \
  --file /tank/shared/projects/shenko/build/c06-toybox-wasi-<date>/cat \
  --name cat --version 0.8.15-wasi --abi wasi_preview1 \
  --expected-digest blake3:<b3sum-of-file> \
  --source-uri https://github.com/landley/toybox/releases/tag/0.8.15 \
  --source-release 0.8.15 --provenance source_build:wasi-sdk-34.0
./shenko --data-dir <data> tool exec --tool cat -- /home/a.txt
```

### 2. 已发布 WASI 工件（证明端到端管线；非具名目标）

- `yamt/toywasm` v76.0.0：`toywasm-v76.0.0-wasm32-wasi.tgz`（GitHub release，
  ~600KB preview1 command 模块；上游无校验和，安装以实际 BLAKE3 落账）。
- `vmware-labs/webassembly-language-runtimes` `python/3.12.0+20231211-040d5a6`：
  `python-3.12.0.wasm`（26MB）超过当前 canonical unit 的 1MiB 工件上限，
  **不能由本切片安装**，不能拿它宣称当前端到端管线已覆盖大型工具。

```sh
WORK=/tank/shared/projects/shenko/build/c06-tool-artifacts
mkdir -p "$WORK" && cd "$WORK"
curl -fsSL -o toywasm.tgz \
  https://github.com/yamt/toywasm/releases/download/v76.0.0/toywasm-v76.0.0-wasm32-wasi.tgz
tar -xzf toywasm.tgz -C toywasm-v76
b3sum toywasm-v76/**/toywasm.wasm   # 得 <hex>
./shenko --data-dir <data> tool install \
  --file "$WORK/toywasm-v76/<path>/toywasm.wasm" \
  --name toywasm --version v76.0.0 --abi wasi_preview1 \
  --expected-digest blake3:<hex> \
  --source-uri https://github.com/yamt/toywasm/releases/tag/v76.0.0 \
  --source-release v76.0.0 --provenance upstream_release
./shenko --data-dir <data> tool exec --tool toywasm -- --version
# 具名目标拒绝：./shenko --data-dir <data> tool exec --tool toybox → TOOL_UNSUPPORTED
```

## 未验证边界（如实）

- 本切片未跑 build/test/native 验证：共 9 个新增独立永久场景
  （inspect_module 真实 ABI：runnable/错误 _start 签名/外部 import；
  unit-import→resolve→执行→重开 roundtrip；prepare 分类：named-target 拒绝、
  argv 超界、坏 base64）；另 2 个真实 CAS/archive 场景分别覆盖 opaque
  guest JSON 不扩图、先 opaque 后 typed alias 的 child 闭包。由 parent
  在受管构建者上验证；显式 RO 的权限拒写、T1 闭包、T2、重启复用及
  RW 不结清/不重跑需要独立 native consumer 观察，未在本 worker 运行。
- artifact 每次执行经 unit_object_bytes 全量读封 + 每次执行重新编译模块
  （无 module 缓存）；沿用 Node 1MiB 工件上限，大型工具安装仍有明确缺口。
- guest 流的 io 目录文件在 join 后删除，但完整字节已先 seal 进 CAS；
  retained 证据是 `ObjectRef`（digest+bytes）+ 有界 head preview。
- deadline 中断阻塞在 host call（如 fd_read 空流）内的 guest 不受保证
  （wasmtime 能力边界，如实标注）；此时 worker 仍会被 join 等待。
- `tool install` 的同名@同版本检查依赖单写入者假设（与 home manifest 同级
  强度）；并发 install 竞态不在防御范围。
- A09 要求（私有 Git/GitHub/Gitea 端到端认证、真实 WASM SSH keygen/exec/
  transfer）**未达成**；该 case 保持 open。
