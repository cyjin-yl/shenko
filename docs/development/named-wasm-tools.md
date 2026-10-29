# 具名 WASM 工具：安装与执行（C06）

本文记录 named WASM tool 的真实状态、接口和获取配方。SPEC §8.2/§8.3/§9/§18.8；
需求 R008/R014/R019/R101；验收 case A09（仅部分推进，未完成 A09 要求的
Git/SSH 端到端认证）。

## 已实现（本切片，源码层）

- **`ToolStore`（`crates/shenko-wasm/src/tool.rs`）**：`<data>/tools/` 下的
  受管工具库。工件按 BLAKE3 摘要存为 `artifacts/<digest>.wasm`；
  `tools.json` 注册表经 tmp+fsync+rename+目录 fsync 原子发表。
  - `ToolStore::open` 空目录初始化；有 registry 则逐条**重新校验工件字节 ==
    记录 digest**（稳定 reopen）；有内容无 registry → `DATA_PATH_CONFLICT`，
    绝不收养未知数据。`bind_installation` 把 registry 绑定到当前
    installation（ledger id），外来安装的 store 被拒。
  - `install`：只收二进制模块（`\0asm`）；经 wasmtime `Module::new` 读真实
    imports/exports 判定 `preview1-runnable`（imports ⊆
    `wasi_snapshot_preview1` 且导出 `_start`+`memory`）。声明 ABI 与实际
    必须一致；其他 ABI（wasix/component/host_imports/other）可入库编目但
    **不可执行**。同名@同版本不同内容 → `TOOL_INSTALL_CONFLICT`；多版本共存，
    `name@version` 消歧。
  - `resolve`：未知名 → `TOOL_UNAVAILABLE`；已声明具名目标未装工件 →
    `TOOL_UNSUPPORTED`（含真实原因）；工件被改 → `TOOL_ARTIFACT_MISMATCH`，
    拒绝执行也不自愈。
- **`wasi.exec` native definition（`crates/shenko-exec-host/src/wasi_task.rs`）**：
  kind `wasi.exec` v1。`{tool:"name[@version]", args, env, stdin_b64,
  timeout_secs}` → 解析记录、arm T1、在真实 WasmRuntime（宿主挂载闭包 +
  argv/env/stdin/stdout/stderr 文件流）里跑**工件本体**，回执带真实退出码、
  流字节数、截断标志；resolution 拒绝作为明确 TOOL_* 结果返回给模型；
  deadline 只是宿主观察超时（不证明 guest 停止）；reconcile 不重放。
  宿主二进制永远不会顶替具名目标。
- **`shenko tool` CLI（`bin/shenko`）**：`list`（记录 + 具名目标真实状态）、
  `install --file --name --version --abi [--expected-digest ...]`、
  `exec --tool name [-- args] [--env K=V] [--stdin-file/--stdout-file/--stderr-file]`。
  退出码原样透传；传输错误 exit 2。
- **`Bootstrap::prepare`**：打开并绑定 ToolStore；`validate_target` 允许
  `tools/`。home-manifest 的 static capabilities/checkpoint 声明**未变**——
  否则既有 data dir 的重开会因声明漂移被拒（SEED_CLOSURE_INCOMPLETE）；
  目标级真相看 `tools.json` 与 `shenko tool list`。

## 具名目标真实状态（`NAMED_TARGETS`）

| 目标 | 状态 | 真实原因 |
| --- | --- | --- |
| toybox | **unresolved** | landley/toybox 0.8.x 无已发布 WASI 工件；需用 wasi-sdk 从源码做 wasm32-wasip1 移植（依赖 fork/exec/sockets 的 applets 在 preview1 下不适用） |
| gcc-wasm | unresolved | 未核验 WASM 编译器成品；hosted-vs-target 含义未定（§8.2） |
| git | unresolved | 无已核验 wasm32-wasip1 Git client；WASIX ports 需要 WASIX runtime |
| gh | unresolved | wasi-gh 存在但需专用 HTTP/exec host imports（§8.2 W2），本 runtime 不提供 |
| tea | unresolved | 未发现可核验的 Gitea CLI WASM 成品 |
| ssh | unresolved | OpenSSH WASIX port 需要 WASIX runtime + socket imports；wasmtime-45 preview1 均无；宿主 ssh 不算 WASM SSH（R019） |

可直接安装的已发布 WASI preview1 工件（非具名目标，但证明端到端管线）：

- `yamt/toywasm` v76.0.0：`toywasm-v76.0.0-wasm32-wasi.tgz`（GitHub release
  资产，wasi-sdk 构建的 preview1 command 模块，~600KB；上游未发布校验和，
  安装时以实际 BLAKE3 落账）。
- `vmware-labs/webassembly-language-runtimes` `python/3.12.0+20231211-040d5a6`：
  `python-3.12.0.wasm`（26MB）带官方 `.sha256sum` —— 最可验证的已发布工件。

## 给受管构建者的获取配方（parent action）

优先路径（最小下载、最快通过验收）：**toywasm 工件安装 + 执行**。

```sh
# 1) 下载（~614KB）：
curl -fsSL -o /tmp/toywasm.tgz \
  https://github.com/yamt/toywasm/releases/download/v76.0.0/toywasm-v76.0.0-wasm32-wasi.tgz
tar -xzf /tmp/toywasm.tgz -C /tmp/toywasm-v76
# 取其中的 toywasm.wasm（真正的 wasm32-wasi command 模块），记录 b3sum。

# 2) 安装到受管 store（在已有 installation 的 data_dir 上）：
b3sum /tmp/toywasm-v76/**/toywasm.wasm   # 得 <hex>
./shenko --data-dir <data> tool install \
  --file /tmp/toywasm-v76/**/toywasm.wasm \
  --name toywasm --version v76.0.0 --abi wasi_preview1 \
  --expected-digest blake3:<hex> \
  --source-uri https://github.com/yamt/toywasm/releases/tag/v76.0.0 \
  --source-release v76.0.0 --provenance upstream_release

# 3) 执行（真实 argv/流/退出码）：
./shenko --data-dir <data> tool exec --tool toywasm -- --version
# 具名目标拒绝：./shenko --data-dir <data> tool exec --tool toybox → TOOL_UNSUPPORTED

# 4) 模型路径（可选，需 start）：[delegate:wasi.exec:1] {"tool":"toywasm","args":["--version"]}
```

可验证性更强的备选（发布方带 sha256sum；文件 26MB，首次编译有可见延迟）：

```sh
REL=python/3.12.0+20231211-040d5a6
BASE=https://github.com/vmware-labs/webassembly-language-runtimes/releases/download/$REL
curl -fsSL -o /tmp/python.wasm "$BASE/python-3.12.0.wasm"
curl -fsSL "$BASE/python-3.12.0.wasm.sha256sum"   # 核对 sha256
./shenko --data-dir <data> tool install --file /tmp/python.wasm \
  --name python --version 3.12.0 --abi wasi_preview1 \
  --expected-digest blake3:<b3sum-of-file> \
  --source-uri "$BASE" --source-release "$REL" --provenance upstream_release
./shenko --data-dir <data> tool exec --tool python -- -c 'print(40+2)'
```

具名目标路径（**toybox，需要真实移植工作，不能跳过**）：

```sh
# 源 + 工具链（均带已发布 sha256sum 资产）：
# - landley/toybox 0.8.15, commit a57b091d05ec366dcd4dd737784c03761cb6a4b1
# - WebAssembly/wasi-sdk wasi-sdk-34.0-x86_64-linux.tar.gz (+ libclang_rt-34.0)
# 构建：CC=wasi-sdk clang --target=wasm32-wasip1，POSIX 缺口逐 applet 处理；
# 只能编出 WASI-compatible applet 子集是真实移植结果的一部分，
# 不得以宿主 toybox 或 WAT 仿制品冒充。完成后按上方 tool install 安装，
# provenance "source_build:wasi-sdk-34.0"。
```

## 未验证边界（如实）

- 本切片未跑 build/test/native 验证：ToolStore 6 个新单测场景与 wasi.exec
  handler 均由 parent 在受管构建者上验证（仓库 shenko-dev-verify 约束）。
- C05 同栈把 `run_wasi_module` 改为 `Result<WasiRunOutcome>`（Exited /
  DeadlineExceeded / Cancelled）并给 `WasiCommand` 加 `control` 字段；本切片
  按当前 `Result<i32>` API 写，合并后以 `WasiRunOutcome` 映射 exit/cancel
  并可把 timeout 换成真实 `WasiRunControl`。
- artifact 每次 resolve 全量读文件 + 每次执行重新编译模块（无 module 缓存）；
  toywasm 600KB 级可接受，26MB python 级首次编译有可见延迟。
- `ToolStore::open` 的 foreign-content 检查在 registry 缺失时拒绝一切文件；
  崩溃可能留下 `.tools.*.tmp`/`artifacts/*.wasm.tmp` 临时文件——下次 open 会因
  foreign-content 拒绝收养，需要 parent 观察是否可接受（或后续显式 GC）。
- ToolStore 假设单写入者（与 home manifest/persist_manifest 同级强度）：两个
  并发 `shenko tool install` 进程的竞争不在防御范围；registry 最终一致性依赖
  原子 rename，但并发冲突检测可能漏判同名@同版本。
- A09 要求（私有 Git/GitHub/Gitea 端到端认证、真实 WASM SSH keygen/exec/
  transfer）**未达成**；该 case 保持 open。
