# Python / Bun / Node：WASM 内可恢复 REPL 规格研究

状态：仅研究和规格建议，未安装、编译或运行候选。核验截至 **2026-10-22 UTC**。用户新增三项命名需求分别保留：Python REPL、Bun REPL、Node REPL；均希望运行在 WASM、可恢复、动态安装包，并采用插件方式。不能以 QuickJS 或“支持 JS”声称已经完成 Bun/Node。

## 1. 可行性结论

| 命名需求/候选 | 已验证事实 | 未被证明的部分 | SPEC 应如何标记 |
|---|---|---|---|
| Python / CPython WASI | 上游正式 WASI 构建，tier 2；可在 Wasmtime 等运行；当前配置目标 wasm32-wasip1 | 任意 PyPI 包、所有 POSIX 模块、完整进程恢复、动态 native extension 生态 | WASM Python 的首要候选；逐包/接口验收，不能称任意 Linux Python 环境 |
| Python / Pyodide | CPython→Emscripten/WASM；micropip 动态装纯 Python 与匹配的 PyEmscripten binary wheels；科学计算生态较丰富 | 它不是可直接丢进纯 WASI runner 的无 JS-host binary；本产品 Rust host 中的适配开销与权限尚未验证 | 单列 Python backend/能力 profile；不能与 CPython WASI ABI 混写 |
| Node upstream | Node 能作为 WASM host；v26.12.1 支持平台矩阵未列 wasm；官方 WASM 页面讨论在 Node 中运行模块 | Node 整个 runtime 的上游 WASI/WASM release 未核验到 | 仍是独立硬需求和移植/后端待决项；不可仅把 Node 原生 sidecar 标成 Node-in-WASM |
| Bun upstream | v1.4.4 官方 release 为各 OS/CPU native assets；存在 WASM transpiler 记录 | 完整 Bun runtime 的 upstream WASI/WASM build 未核验到 | 保留 Bun-in-WASM 研究/验收项；bun-wasm transpiler 不算 Bun runtime |
| Javy | Bytecode Alliance JS→WASM 工具链，脚本运行于嵌入式 JS 引擎 | Node/Bun API、任意 npm、通用常驻 REPL 并非等同现成提供 | 可以追加“轻量 JS WASM backend”，不能替换用户三项命名要求 |
| ComponentizeJS / StarlingMonkey | SpiderMonkey embedding 将 JS/WIT world 制成 Wasm Component；有 fetch/streams 等 API | 不是完整 Node 或 Bun；项目仍标 experimental；通用动态 REPL 需设计 | 可供插件 JS 实现/额外 REPL backend 评估 |
| WebContainers | 厂商公开说明浏览器中执行 Node 应用与 OS commands | 不是已证实可嵌入 Rust/Wasmtime 的上游 node.wasm；浏览器依赖、授权与 API 需单列 | 证明第三方浏览器 Node 路线存在，不证明本项目目标已解决 |

“未找到 upstream build”是本次核验范围的结论，不是数学上不可能移植，也不是所有社区项目都不存在。搜索出现零散 bun-wasm/fork 项目，未取得可验证的完整 Bun runtime 构建、ABI 和兼容性证据，不能把名称命中当完成。

## 2. 一手来源与版本

| ID | 来源 | 核验信息 |
|---|---|---|
| RP1 | [CPython WASI README](https://github.com/python/cpython/blob/784b305df8821d57fb6fb6c52794ef67bda0a75d/Platforms/WASI/README.md) | main 固定到 `784b305df8821d57fb6fb6c52794ef67bda0a75d`，2026-10-22 18:24:53 UTC；README 为 WASI tier 2；GitHub 插件读取 |
| RP2 | [CPython WASI config](https://github.com/python/cpython/blob/784b305df8821d57fb6fb6c52794ef67bda0a75d/Platforms/WASI/config.toml) | WASI SDK 34，host-triple wasm32-wasip1 |
| RP3 | [Python developer build guide](https://devguide.python.org/getting-started/setup-building/index.html#wasi) | 文档 2026-10-01 更新；3.15+ 用 Platforms/WASI；产出 wasm32-wasip1/python.wasm；WASI 目录按 guest path；ref `turn80view2` |
| RP4 | [Python WebAssembly platforms](https://docs.python.org/3/library/intro.html#webassembly-platforms) | POSIX 子集，process/thread/network/signal/IPC 存在可用性差异；ref `turn75search4` |
| RP5 | [Pyodide changelog](https://pyodide.org/en/stable/project/changelog.html) | stable 314.0.7，2026-09-14；314.0.0 为 2026-06-09，CPython 3.14.2 / Emscripten 5.0.3；refs `turn77view0`, `turn78view3`, `turn79view0` |
| RP6 | [Pyodide package loading](https://pyodide.org/en/stable/usage/loading-packages.html) | micropip 支持 pure Python 与对应 binary wheels；implicit imports 不等于任意 PyPI install；ref `turn77view1` |
| RP7 | [micropip API](https://micropip.pyodide.org/en/stable/project/api.html) | 文档版本 0.11.1；install/index/constraints 等；ref `turn78view1` |
| RP8 | [Pyodide filesystem](https://pyodide.org/en/stable/usage/file-system.html) | MEMFS 默认不持久；可挂 IDBFS/NODEFS 等；nativefs 需 sync；ref `turn77view2` |
| RP9 | [Pyodide sockets](https://pyodide.org/en/stable/usage/socket.html) | Node host 中 experimental useNodeSockFS；browser 不支持通用 raw sockets；需 JSPI；ref `turn80view1` |
| RP10 | [PyEmscripten future ABI](https://pyodide.org/en/stable/development/abi/315.html) | pyemscripten_2026_5 页面明确 planned，Python 3.15/Emscripten 6.0.5；不能因出现在 stable 文档导航就说 Pyodide 3.15 已发布；ref `turn80view0` |
| RN1 | [Node v26.12.1 BUILDING](https://github.com/nodejs/node/blob/v26.12.1/BUILDING.md) | GitHub release 2026-10-21；官方支持平台列表无 WASM；已读取固定 tag，不能拿 main 的更高系统版本要求代替当前 stable |
| RN2 | [Node with WebAssembly](https://nodejs.org/learn/getting-started/nodejs-with-webassembly) | Node 运行 WASM 模块，方向为 host→guest；ref `turn75search2` |
| RN3 | [Node V8 APIs](https://nodejs.org/api/v8.html) | 当前 docs v26.12.1；value serialization、heap diagnostics、startup snapshots 分别是不同功能；refs `turn77view3`, `turn78view5`, `turn78view6` |
| RB1 | [Bun v1.4.4 release](https://github.com/oven-sh/bun/releases/tag/bun-v1.4.4) | 2026-10-22 10:11:32 UTC，target `38afee6c84fcf48c0edb2466aa4b27d287a59c30`；官方 asset 列表无 wasm，已有 Android native assets；GitHub 插件 API 读取 |
| RB2 | [Bun installation](https://bun.com/docs/installation) | 安装平台文档；ref `turn76search1` |
| RB3 | [Bun v0.8.0 official changelog](https://bun.com/blog/bun-v0.8.0) | 2023-08-24，PR #4043 是 transpiler→WASM；不能推断完整 runtime；refs `turn82view4`, `turn83view2` |
| RJ1 | [Javy](https://github.com/bytecodealliance/javy) | release v9.1.0，2026-07-30；README/官方代码读取；refs `turn76search3`, `turn76search31` |
| RJ2 | [ComponentizeJS](https://github.com/bytecodealliance/ComponentizeJS) | release 0.23.0，2026-09-21；README 明示 SpiderMonkey embedding 与 experimental；ref `turn76search6`；Wizer pre-init 原理可核验 |
| RW1 | [WebContainers introduction](https://webcontainers.io/guides/introduction) | 浏览器 Node runtime；ref `turn82view0` |
| RW2 | [WebContainers native add-on limit](https://webcontainers.io/guides/troubleshooting#cannot-load-native-addon-error) | 普通 native add-on 不能直接运行；需 JS/WASM 替代/移植；ref `turn82view1` |
| RS1 | [Python pickle](https://docs.python.org/3/library/pickle.html) | 对象结构序列化与持久化区别；类定义/模块依赖、格式与信任边界；ref `turn79view1` |
| RS2 | [Wasmtime module serialization](https://docs.wasmtime.dev/examples-serialize.html) | 保存编译后的 Module，省编译成本，不是保存所有 live Store/OS 资源；ref `turn83view1` |
| RS3 | [Wizer](https://github.com/bytecodealliance/wizer) | 初始化后状态制成新模块；旧项目约束需对所用版本核验；ref `turn78view2` |
| RS4 | [wasmtime_wizer API](https://docs.wasmtime.dev/api/wasmtime_wizer/index.html) | 文档标 **51.0.0-dev**，包括 module/component instance state；不能当 Wasmtime 50 稳定功能承诺；ref `turn83view0` |

## 3. Python 的两条路线必须分开

### CPython WASI

这是 Rust+Wasmtime 类宿主内的自然候选：真正 CPython guest，宿主提供 preopen paths、stdio、时钟/随机数和选定接口。其 `os.name == posix` 并不意味着 Linux 模块生态自动可用。[RP1–RP4]

动态安装纯 Python wheel 可以设计为 package worker 将经过依赖解析的包树加入 generation/site-packages；但包含 ctypes/FFI、ELF `.so`、native subprocess、系统服务或特定 sockets/thread 功能的包，需要实际 ABI 和能力验收。不能把文件下载成功当 import 成功，也不能把 import 成功当所需功能全部成功。

本次未证明 vanilla CPython WASI 与 Pyodide 的 binary wheels 能互换。WASI p1、Emscripten/PyEmscripten、CPython ABI/版本、异常处理/线程配置、依赖 C/Rust 库都属于包兼容键。

### Pyodide

适合交互数据工作和已有 scientific wheels；2026 已不能只用“micropip 只支持纯 Python”概括。官方 314.0.2 更新已列因 PyPI 上有 PyEmscripten wheels 而从内置 lockfile 移出的包。[RP5–RP7]

但 Pyodide 依赖 Emscripten 与 JavaScript glue。若本产品选 Rust 核心且只嵌 Wasmtime，不应声称额外装一个 Pyodide wasm 就完成：需要可用的 JS host/glue、filesystem 和 async/JSPI 配套，可能改变体积与部署面。它可以是可选 backend，默认选择尚未确认。

网络和持久化要按具体 host 说：Pyodide Node 模式的实验 sockets 依赖 Node/JSPI；不能据此承诺 browser 或纯 WASI 也拥有同样网络能力。MEMFS 数据刷新即丢；挂载/显式 flush 的持久化协议必须接到同一 snapshot generation。[RP8/RP9]

## 4. Bun/Node 与 WASM 的方向性辨识

至少分四种不同事实：

1. **Node/Bun 运行 WASM**：JS runtime 是宿主，符合官方主流文档，但不满足“runtime 本身在 WASM 沙箱内”。
2. **Bun transpiler 被编译为 WASM**：只实现转换/分析前端，不等于 Bun runtime、package manager、REPL、native APIs 都在 WASM。
3. **JS engine 编译成 WASM**：Javy/SpiderMonkey/QuickJS 类，可提供额外 JS backend；Node/Bun 的模块、系统 API 和事件循环兼容仍需专门实现。
4. **第三方 Node-like/浏览器 Node 环境**：WebContainers 有实用路线，但浏览器模型、内核/许可/扩展机制与 Rust headless host 需求不同。

因此推荐 SPEC 中以 `engine_identity`、`implementation`、`abi_target`、`host_requirements`、`api_compatibility_profile` 分别表达。不能让插件把自己随意命名为 node 或 bun，便获得“完整 Node/Bun 支持”的验收结果。版本字符串也不够，需构建来源、artifact digest、兼容测试结果。

如果用户接受 native/SSH/后续 container 的 Node/Bun backend，能先覆盖语言工作流与包生态，但这仍是**待确认的替代执行位置**。这两个 WASM 条目继续列为未完成/待验证，直到真实 runtime port 与用例通过。

## 5. 可恢复 REPL 的三层定义

会话常驻时，连续 cells 共用变量并不难；跨崩溃、更新、节点迁移恢复是另外的承诺。建议明确分级，不能用保存代码历史冒充恢复所有运行中对象。

| 层级 | 保存/恢复什么 | 可承诺的语义 | 必须显式暴露的限制 |
|---|---|---|---|
| L1：环境和 cell journal | cells 顺序/源码/参数、结果引用、输入数据、fs snapshot、runtime artifact、pkg lock、mount manifest | 重建同一环境；从已知安全边界恢复并可选择重放 | 外部世界可能变化；非幂等 cell 不自动重跑；不等于保留全部变量 |
| L2：可序列化对象 | 明确导出的数据对象/数组/表/结构、codec schema、依赖版本 | 恢复声明可保存的工作数据并继续交互 | closure/生成器/活动 coroutine/host proxy/文件句柄/线程状态可能不在范围；不允许默默丢弃 |
| L3：完整执行状态/VM snapshot | 可支持时保存 guest memory/globals/tables/控制栈及外部资源映射 | 仅在已验证 runtime/ABI/安全点/资源集合内声明 checkpoint/restart | host handles、socket 对端、外部事务、timers、threads、JS host objects 不因复制线性内存自动恢复；升级兼容不能想当然 |

本层级是建议定义。用户的“可恢复”究竟要求 L1+L2，还是把 L3 也作为首版硬验收，仍需规格问答，不能团队自行降为 L1。

证据边界：Python pickle 序列化对象，不负责整体持久化/并发；Node V8 value serialization、diagnostic heap snapshot、startup snapshot 不是同一功能；Wasmtime Module.serialize 保存编译 artifact；Wizer 的预初始化 snapshot 可加快启动，但不是所有时刻/所有外部资源的透明进程迁移。[RS1/RS2/RS3/RN3] 新 wasmtime_wizer dev API 的存在值得跟踪，不应反过来说 WASM 永远不能做快照，也不能由此声称已具备无限制 L3。[RS4]

推荐 checkpoint 选择 cell 结束且已完成资源交接的 quiescent boundary；不支持的对象返回精确清单，例如某 socket 需要重新连接、某 generator 需要重跑到安全点。可借助 plugin-specific serializer 保存部分高价值对象，仍不伪装为 full VM image。

## 6. Journal、外部副作用与快照一致性

这是本产品设计建议：REPL 也只能由子 Agent 持有、执行，主体通过 handoff 提交语义工作，不暴露万能 `repl.eval` 工具给主 loop。

每个 REPL session 绑定 child/task/workspace/branch/namespace/capability generation。Runtime 负责持久 journal；记录 accepted cell、执行开始、外部操作意图/receipt、stdout/stderr、artifact refs、结果/异常、checkpoint ID 与 package generation。原始 journal 属于不可删历史，而输出缓存可以按明确规则重建。

恢复状态不能只有 success/failed：在外部操作已经发生、receipt 尚未落盘的断点，必须可表达 `outcome_unknown`。文件快照不能撤销已经发送的消息、远端 SSH 命令或外部 API mutation；自动 replay 需要幂等键、结果查询或明确的恢复策略。这个问题与使用 WASM 还是 native 无关。

重放策略建议分纯计算、只读 I/O、可幂等写、不可幂等/未知副作用。记录时间/随机数/输入引用可增加重现能力，但不能承诺仅凭源码自动得到相同结果。发生 socket 断线应重连/核对协议状态，不能把旧数字 fd 当有效新连接。

文件系统 snapshot、变量 checkpoint、包 generation、cell 序号应通过同一 checkpoint manifest 绑定；恢复时校验 digest 和 ABI，不允许把新 package tree 与旧解释器对象盲目拼接。跨设备若只能恢复 L1/L2，要准确报告恢复层级。

## 7. 动态装包、Nix-like generation 与插件契约

动态安装是用户能力要求，不能因为可重复性而禁止。建议把 install 变成生成新的 immutable package closure 与 lock，验证后给该 REPL session 激活；旧 generation 继续可回滚。首次安装与升级已经载入的模块是两种操作：后者可能需要新 session/checkpoint+restart，不能承诺所有语言都可安全热替换已有对象。

package compatibility 至少包括：

- 包来源/版本/内容 digest、Python/JS runtime 版本和 wasm ABI；依赖完整闭包。
- pure code、WASM binary wheel/module、native `.node/.so/.dll`、FFI、需 spawn 的工具链、postinstall scripts 分别识别。
- npm 的 ESM/CJS、Node builtins、Bun-specific API、native addon 与 transitive dependencies；下载 npm 包不等于 Node API 已存在。
- Python 的 wheel tags、CPython ABI、PyEmscripten 平台、依赖动态库/异常/线程选项；ELF wheel 不能在 WASM 直接 import。
- 安装脚本执行所需能力、网络/磁盘/时间资源预算、取消/rollback 与离线 cache。

`ReplProvider` 建议公开 create/attach/eval/interrupt/inspect/export/import/checkpoint/restore/close/install/describe-capabilities；每项都有真实支持状态。L1/L2/L3、top-level await、stdio streaming、binary outputs、rich display、包管理器、网络/文件接口均在 capability manifest 中声明。模型可以查询这些能力降低试错，不靠猜测 shell 参数。

“全插件”不意味着把持久化/授权信任根也交给任意插件：Rust 核心保留 session 身份、能力、journal、budget、恢复协议；每个语言 runtime、包解析/构建适配、serializer 是可替换插件。语言插件崩溃不应使 rescue、主 loop 事件 ledger 或默认 Git memory 一起丢失。

REPL 如需调用本地 LLM，使用 broker-mediated API handle。若允许任意 guest HTTP 直连同一模型 endpoint，而凭据/网络路径也不受控，就不能声称“所有模型请求均受全局并发锁”。这需要在 endpoint egress/credential delegation 层实际 enforce，或准确限定全局保证只涵盖框架受管请求。包下载、执行 CPU 与模型请求采用分别的资源预算，不应占住 LLM permit 等下载。

## 8. 首版验收与规格追问

建议验收：

1. Python/Bun/Node 三个命名条目有独立实现/待验证状态与 artifact 证据；额外 JS engine 不会自动通过另外两项。
2. 可在同一 REPL 连续执行 cells、保留变量、取消长运行；输出有截断/分页/附件语义。
3. 自主动态安装一个纯代码包及一个实际匹配 ABI 的 binary 包；不匹配 native 包报具体原因及可选移植路径。
4. session crash、host restart、package upgrade 后按承诺层级恢复；不支持对象可见，不能静默丢状态后报告全部恢复。
5. checkpoint 与 package/fs/journal 序号一致；恢复旧环境时原始 history 仍在，fork 不污染 sibling。
6. 在外部写完成但 receipt 未落盘的故障点，避免盲目重复操作；无法确认时报告 unknown 并执行恢复策略。
7. 主体无法直接 eval；执行子 Agent 的 REPL 访问权限不超过其 task capability；模型 API 不绕 broker。
8. Android/router 对每个语言 backend 分开测常驻/冷启动/执行内存与磁盘；不能拿 Rust 核心小证明 Pyodide/Node/Bun 都便宜。

需要用户裁决但不应阻止写完待决 SPEC：

- “Python/Bun/Node in WASM”要求三个完整 upstream runtime，还是允许清楚标识的兼容 runtime？若不接受替代，Bun/Node 必须保留专门移植研究 gate。
- 是否允许以 native/SSH/container backend 先覆盖 Node/Bun 工作流？未回答不能视作已同意。
- 可恢复首版最低承诺是 L1、L1+L2，还是包含 L3？最大 checkpoint/包缓存/REPL 数量要按哪类设备约束？
- 动态安装已载入包的新版本时，是否接受创建新 generation 并恢复到新 REPL？哪些场景要求保留活跃对象？

以上均为规格材料，不是开发进度或已通过的实现测试。
