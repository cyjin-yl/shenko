# 外部建议审查：WASM、文件系统、恢复与核心自改

状态：2026-10-23（UTC+8）研究/规格审查，未开发、未修改主稿。已读附件 `upload/粘贴的文本 (1)(8).txt` 全文及父 Agent 转交的 inline 建议。附件中的工作名、版本、性能数字、分期与审批默认值均不是用户新裁决；用户已说明外部模型可能幻觉。本文件只审查本分工相关部分。

**本轮最新已确认边界：**用户再次明确拒绝永久不可改 Ring0，也拒绝用低频 tick/被动 observe 替换永续思考 loop。KV 调度由可替换算法/插件自动完成；50/50 是在驻留需求与切换成本适合均分时，自动算法应能自行形成的理想分配情形。仅让用户手动配置 50/50 并跑通不算满足；需求变化时应自适应，不写死内核、不要求始终各半，也不推断用户已接受其他固定比例。

## 1. 结论与采用判断

**值得吸收的是分层、候选环境、内容寻址和插件清理思路；不能照收永久不可改 Ring0、把 cleanup 当持久事务、把 CAS 当天然合并/掉电保证、或以设备不能本地编译为由取消任何部分自改。**目前 SPEC 已有的大部分边界不需推倒重写，最值得补强的是构建委派、对象发布顺序和 GC 协议。

| 外部建议 | 审查判断 | 对产品规格的处理 |
|---|---|---|
| immutable Ring0 小于 10 MB，固定放 Cordis、WASM engine、VFS、SQLite、锁、KV 与硬件 watchdog | **不直接采纳。**永久不可改与 R088 冲突；把这些实现全部固化进核心与 R087 冲突。10 MB 没有目标架构、构建 feature 或测量证据；binary size 与 RSS 不能混用 | 保持极小启动/恢复/lifecycle/capability/hard-quota 原语；业务 provider 可换。验收 kernel 与默认插件总成本；硬件 watchdog 仅在设备实际支持的 profile 选用 |
| 路由器放不下原生 rustc，因此核心必须禁止 Agent 修改 | **错误推论。**部署设备不需要同时充当构建设备；Cargo 支持目标构建，Nix 有 SSH remote build [S8–S9] | 子 Agent 可以在可达构建节点生成目标产物，再将经过验证的候选交给设备激活；本地缺工具链是构建位置/可用性限制，不是永久源码禁令 |
| self-rewrite 只限 Ring1；kernel/调度器改动必须再向人类审批 | **与已确认需求冲突。**用户已明确允许改任何部分 | 保留候选、验证、代际交接；不重新引入按组件永久禁改或核心改动必问。行为合同持续成立，实施这些合同的代码同样可换 |
| `observe` 长时间不调用主体模型，只跑低频 tick，也宣称满足永续思考 | **已被用户明确拒绝。**不同推理深度可以研究，但不能偷换掉连续主体模型决策 | 保持唯一连续主 loop；资源排队、模型离线与恢复如实显示；借公平调度使主/子轮转，不用被动观察器替代主体 |
| 固定半池不是最佳算法，所以删除 50/50 场景、改永久 pinned 主份额 | **不接受这种替换。**改进调度和插件化已获支持；用户要求的是自动算法在适合均分时自行达到约各半 | KVPolicy 可换成 auto、主体偏好或其他策略；验收无手动比例的理想工作负载和需求变化后的自适应，不能以静态 50/50 配置交差，也不擅自改成 15%–20% 等固定比例 |
| `ctx.effect` 注册逆操作，所以任意副作用天然可撤销 | **事实不成立。**源码是 generation-owned cleanup；并非磁盘 WAL、外部事务或通用逆操作生成器 [S6–S7] | 用于插件监听器、任务、句柄等生命周期清理。跨进程恢复和外部操作另有持久 intent、receipt、幂等与 uncertain 状态 |
| LayerFS 当前是 0.1.6/0.1.7，硬依赖 Linux+Docker+ext4/xfs/btrfs | **版本混用，平台限制部分有真实依据。**1.0.0 已于 2026-10-21 发布源码；v1 release 的进程内 Store 调用确要求 Linux 与指定 FS。0.1.7 标准 Sandbox 路径确用 Docker/Linux daemon [S1–S4] | 逐版本、逐层记录限制。不得把真实接入限制说成幻觉，也不得将其推成 CAS/content 逻辑天然只能 Linux；抽取后端需重新验证，不能宣称已经获得 WASM port |
| 使用宿主 `wasi-vfs` 就得到可写、持久、跨运行时 CAS VFS | **不成立。**该具体项目只列只读嵌入 FS，依赖 wasi-libc 的导入链接方式，并非通用宿主可写存储引擎 [S5] | 自定义版本化 filesystem/provider 合同；WASI P1/P2、WASIX、宿主挂载分别适配。可借鉴或另评 WASI-Virt，但不把该名称当完成方案 |
| BLAKE3 + SQLite + Merkle 天然零拷贝 fork、天然 LWW | **只接受结构共享的设计启发。**hash/树不定义冲突合并策略。LayerFS 默认拒绝同文件冲突，`.lww` 才是显式例外 [S2] | fork 重用根与对象不等于无元数据写入、无读放大、无 I/O、无 RAM 成本；默认冲突须显式，LWW 按 namespace/path 单独选择 |
| LayerFS 外面再放 Git remote 或宿主快照即可保证 durable | **不足。**额外副本只有在时序、闭包、确认点和恢复协议成立时才提供明确 RPO [S1、S3、S10] | 先持久化完整对象闭包，再发布可见引用/完成回执；异步备份声明滞后；Git 不保存的对象和密钥引用必须一同可恢复 |
| Git 管文本，CAS 指针管二进制，可用 WebDAV/S3/LFS 并带 keyref | **可选插件启发。**不能把自定义 BLAKE3/cas-ptr 称为标准 Git LFS；标准 LFS pointer 规定 sha256 oid/size [S11] | 保留默认 host-mounted Git memory；自定义 pointer 单独 version，明确 payload/backend/加密/引用生命周期；WebDAV/S3 不成为启动或私有记忆必需条件 |
| EphemeralRamFs 为默认首启，GitRemote 才是持久 memory | **不采纳为产品默认。**违背默认 host-mounted 本地 Git memory 和无 remote 可工作 | RAM FS 可用于一次性实验/救援 staging；首启正式 memory 路径仍是持久宿主挂载 Git，SQLite/session truth 独立落盘 |
| 纯 Rust SSH 库编译一下即可成为 `wasm-ssh` | **缺实现证据。**纯 Rust 不自动消除 sockets、poll、线程、DNS、crypto RNG 的目标依赖 | 保留实际 SSH WASM 目标和已查 OpenSSH WASIX port 证据；`wasm-ssh` 若只是插件名，应明确尚非验证过的 runtime [S12] |

以上判断不采用附件新名字 `Homun`，不冻结附件的 v1/v2 阶段，也不把其默认审批表并入用户的既有自主授权。

## 2. 已核验事实的精确边界

### 2.1 LayerFS：发布版、当前主线与可抽取逻辑分开

- **v1.0.0 release**：GitHub release 时间 `2026-10-21T11:21:25Z`；tag ref 指向 commit **`2aaf2552ce21154928b9f9c83582e41a637f5910`**。这是 source-only release，没有 npm package、预编译 binary 或公开 image。release 明确进程内 Store 调用在 Linux 的 ext4/xfs/btrfs 上运行，其他环境拒绝；其 Store 仍为 Disposable，无 crash/power-loss durability。[S1]
- **此前已固定研究主线**：commit **`b5814bf3a8d886ef5e32298118dd1661c352bd2a`**。root README 与 release 的 durability 警告一致；并明确默认冲突拒绝、显式 `.lww` 例外。不能将“最新编辑获胜”当 Merkle/CAS 的天然或全局规则。[S2]
- **新 persistence crate**：同一固定主线中仍标 under qualification/no release admission；目前要求 macOS fullfsync profile，其他平台拒绝。源码里存在 WAL/FULL 不代表 Linux/Android/router 的生产掉电资格已完成。它还明确 sandbox host transport 是未解决的应用适配问题，Rust handle 不会自动跨进程。[S3]
- **抽取候选**：`layerfs-content` 的直接依赖仅见 BLAKE3 与自身 telemetry；这支持“分开考察纯内容层”的工程判断，**不证明**完整 crate 已通过 wasm32 编译或后端具备可用的持久适配。[S4]

Linux/Docker/文件系统限制应成为 LayerFsHost provider 的 capability 声明。我们可以选择另写 VFS adapter 或抽取逻辑，但它是需要验证的移植工作，不能把现成发行的名字当作该工作已完成。

### 2.2 `wasi-vfs` 与真正宿主 VFS 的差距

本轮核验 `kateinoigakukun/wasi-vfs` main 为 **`0f4db4b64882db50ef8c04018e7e98bf59a63f92`**，commit 时间 `2026-04-12T20:37:34Z`，版本 0.6.3。README 描述将目录打包进 WASM 的只读嵌入文件系统；通过 wasi-libc 的导入符号完成拦截。README 还特别指出直接调用 WASI 的 Rust 程序不在其当前支持范围，并建议 Preview2/Component 用户另看 WASI-Virt。[S5]

因此，产品所需的写入、rename、共享目录、事务、COW branch、持久同步、host path mounts 和可取消 I/O 仍需独立 filesystem provider。`wasi-vfs` 可以是某些基础资源打包路线，不是上述完整 VFS 的可直接替换品。

### 2.3 Cordis effect 是生命周期资源清理

本轮搜索固定 Cordis source 为 **`1754062cef67cfc119fc7a2084c0321a231b9092`**。`effect.rs` 的 cleanup journal 是进程内 `BTreeMap`，登记 `FnOnce` cleanup；generation drain 顺序调用，失败或 panic 也会消耗该 occurrence。其 completion runtime 可以独立于调用者 future 推进，**不是**独立于进程终止、掉电或机器丢失继续执行。[S6]

README 的版本判断基本正确：应用 facade `cordis-rs 0.11`、semantic `cordis-core 0.6`、v3、Rust 1.88/Edition 2024；“0.6”同时也可能指旧 facade 维护线，不能省略包名混为同一个版本。[S7]

可以用 effect 注册“停止任务、关闭句柄、释放 lease”的清理；不能自动撤回已发群消息、恢复另一个客户端改过的 SSH 文件、或在硬断电后运行内存里的 closure。更不能因为一条 compensation 返回成功就把原始外部操作历史删除。

## 3. 建议正文增补：远端构建与任何部分自改

建议放入 **§9.1–9.2、§7、§22**，以下为产品合同建议，不是上游现成能力宣称：

1. **构建和运行位置独立。**BuildProvider 接收源树 digest、依赖锁、target triple、libc/OS/CPU features、工具链 revision、功能 profile 和输出清单。子 Agent 可选本机、SSH 构建节点或既有 CI/构建服务；低资源控制节点不要求安装 rustc、LLVM、完整 Nix 或 linker。Nix remote build 是可复用实现之一，不是每台路由器必须运行完整 Nix 的理由。[S8–S9]
2. **交叉编译不是跨平台运行保证。**目标的 sysroot、native 依赖、链接器、最低 OS/ISA、签名要求均须匹配；构建节点上的测试不能替代目标设备 smoke/恢复验证。无法构建时报告 blocked/queued，不悄悄降级成“核心永久不可改”。
3. **候选覆盖所有部分。**kernel、rescue、WASM engine、SQLite/provider、broker、persona/harness 和插件都能生成候选。Agent 的实际代码修改仍由子 Agent 执行；不赋给主体文件/exec 工具。
4. **保留完整上一代闭包。**不仅保留旧 binary，还保留可启动 manifest、必需插件、恢复入口和兼容持久状态。schema migration 需支持恢复路径；新 schema 已不可逆覆盖唯一数据时，旧 binary 不构成有效 rollback。
5. **激活与健康确认分开。**候选产物校验完成后进入目标设备的待激活代；轮次/epoch 边界交接唯一主体，启动测试通过后标 last-known-good。失败由上一代独立恢复；当前候选不得依赖自己修复自己的唯一救援入口。
6. **一代只读，不是永远不可改。**某份产物可作为 hash-addressed immutable generation；后继 generation 可以替换其全部实现。保留上一代的恢复条件不构成永久 Ring0 源码禁区。

这一路线在工程上与轻量部署兼容；尚未证明所有目标设备均能承受候选驻留、下载空间、迁移峰值和启动验证，验收须计入这些峰值。所有构建若不可达，Agent 保持运行或排队等待，这是现实可用性边界，不改变自主改写的授权范围。

## 4. 建议正文增补：durability、发布引用与 GC

建议放入 **§10.2–10.4、§12.1、§23**。以下是我们的提交协议建议，不能反向宣传为 LayerFS 现有保证。

### 4.1 CAS 与本地提交

- 本节的内容寻址存储（content-addressed storage）与 head 比较交换（compare-and-swap）是两个不同概念，虽然英文都常缩写 CAS；前者本身不提供后者的并发发布保证。
- 对象 ID 使用带格式版本、类型与编码的明确 hash domain，规定 canonical serialization；BLAKE3 可作为候选，但不将 hash 名称当成持久性或身份认证。
- 工作区 commit 必须具备 durable intent、完整对象闭包与可恢复阶段。若对象和 head 同在一个 SQLite DB，尽量在有界事务内完成所需原子发布；若对象在外部文件/对象服务，则先完成 payload 持久化，再在 metadata store 发布引用，并处理两存储间的 crash gap。
- 更新 branch head 使用预期旧 revision 的 compare-and-swap 或等价冲突协议；失败保留候选/冲突，不以 LWW 静默覆盖他人工作。若允许某些路径 LWW，明确规则、scope 与原版本可追溯性。
- `accepted`、`committed-local`、`replicated` 是不同状态。调用返回 durable success 前，必须达到该部署 profile 声明的同步确认点。WAL 模式本身不等于每次提交抗掉电：SQLite 官方明确 FULL 与 NORMAL 的同步差别；设备诚实执行 flush/屏障仍是前提。[S10]
- fault injection 至少覆盖：对象写到一半、对象已落盘/head 未提交、head 提交/receipt 未发送、磁盘满、checkpoint 中断。process kill 与真实掉电不能混称同一个验收。

### 4.2 Git 指针、远端同步与发布顺序

- 默认本地 host-mounted Git memory 不改变。大二进制可选 CAS/标准 LFS provider，不能让 GitRemote 或云 bucket 变成唯一持久默认。
- 自定义 `cas-ptr` 明确 version、object/manifest ID、size、backend namespace、可选 encryption scheme 与不含秘密的 opaque keyref；standard Git LFS 使用它自己的格式/算法规则。自定义 BLAKE3 pointer 不自动与 Git LFS 客户端互通。[S11]
- **先上传并确认对象闭包，再发布 Git ref/共享 root。**同步状态只有在目标能按其承诺读取完整闭包时才为 replicated。上传成功而 ref publish 失败可留下待回收对象；不得把 Git push 成功但二进制尚不可取误报成完整同步。
- 网络断开、远端 token 过期、对象缺失、keyref 无法解析要有独立错误与恢复状态。S3/WebDAV 的读写、条件更新、列举一致性、multipart cleanup 与认证能力由各 provider 声明，不能用统一名字推断相同语义。
- 内容 hash 提供一致性核验，不自动提供私密性或来源认证；私有 Git 与 keyref 也不等于端到端加密。加密/去重与可见 metadata 的取舍明确列入存储策略。

### 4.3 GC 是独立生命周期协议

- GC root 至少包括：活跃 branch/commit、受保留 Git 历史指针、进行中的读者/transfer lease、候选及上一代 recovery 闭包、pending save/提交/上传、owner pin 和保留策略覆盖的原始会话附件。
- 分离“可删除失败实验的可见分支”和“删除原始 session 证据”。research reset 不删除原始历史；owner 明确 erase 使用独立协议。
- GC 在一致 roots snapshot/epoch 下标记；删除前处理并发新根与 lease，保护尚未发布但仍在提交窗口的对象。不能只扫描当前 main root 就删其余对象，也不能只看本地 branch 删除远端仍被 Git 历史引用的 payload。
- 采用有界批次、grace window/隔离区、容量告警和 GC receipt；grace period 是补充，不能代替可达性及并发协议。备份恢复必须能找到对象与必要解密资料，备份保留策略和真实 erase 冲突需明确处理。
- 数据全在 SQLite 的场景也不能假设物理空间即时归还；checkpoint/压缩/回收的耗时、写放大和崩溃恢复要计入设备预算。主循环不能持有写事务等模型结果。[S10]

## 5. 最小增补验收

| 验收 | 必须证明的结果 |
|---|---|
| 低资源设备不安装工具链，远端为它构建新 kernel/rescue | 设备可自主获取、验证并切换候选，功能不要求本机 rustc；构建节点失联只影响更新可用性 |
| 新 kernel、rescue 或必需存储插件故障 | 上一代能独立启动，原消息/身份/撤销状态不倒退，唯一主 loop 不分裂 |
| `ctx.effect` 正常卸载、panic、进程被杀分别注入 | lifecycle cleanup 与持久恢复结果分别报告，不对 kill/power loss 声称内存 callback 已执行 |
| CAS payload、metadata、receipt 三处交错失败 | 不出现 visible committed root 引用缺失对象；或明确 quarantined/unknown，能够根据 intent 恢复 |
| 二进制对象上传中断后尝试发布 Git ref | 未完整可取的闭包不被宣布 replicated；重试不误删共享对象 |
| 新 fork/读 lease/上传与 GC 并发 | GC 不删除活跃、待提交或上一代恢复需要的对象；实验清理不抹掉原始 session 来源 |
| 双 worker 修改同一文件 | 默认明确冲突；仅配置了 LWW 的路径采用规定的策略；旧证据仍可查 |

## 6. 精确一手来源，供 root 重新打开

正文中的来源代号是本报告自己的索引；GitHub 文件通过插件读取。外部引用前可直接打开下列 URL。

| 来源 | 固定版本/关键证据 | 直接 URL |
|---|---|---|
| S1 LayerFS 1.0.0 release 与 tag | 发布 2026-10-21T11:21:25Z；source-only；tag commit `2aaf2552ce21154928b9f9c83582e41a637f5910`；Linux/ext4/xfs/btrfs 与 Disposable 限制确实存在 | https://github.com/Ephemeral-AI-Lab/layerfs/releases/tag/v1.0.0 ; https://api.github.com/repos/Ephemeral-AI-Lab/layerfs/releases/tags/v1.0.0 ; https://api.github.com/repos/Ephemeral-AI-Lab/layerfs/git/ref/tags/v1.0.0 |
| S2 LayerFS 固定主线 README | `b5814bf3a8d886ef5e32298118dd1661c352bd2a`；默认同文件 conflict，`.lww` 才例外；Store/Workspace 分层 | https://github.com/Ephemeral-AI-Lab/layerfs/blob/b5814bf3a8d886ef5e32298118dd1661c352bd2a/README.md |
| S3 LayerFS persistence | 同上主线；under qualification/no release admission；macOS fullfsync profile；不宣称硬件掉电保证 | https://github.com/Ephemeral-AI-Lab/layerfs/blob/b5814bf3a8d886ef5e32298118dd1661c352bd2a/core/crates/layerfs-persistence/README.md |
| S4 LayerFS 0.1.7 与 content 依赖 | 0.1.7 guide 明列 Docker、Linux daemon 与 host toolchain；content crate 可分开研究 | https://github.com/Ephemeral-AI-Lab/layerfs/blob/b5814bf3a8d886ef5e32298118dd1661c352bd2a/docs/versioned/0.1.7/sdk-and-ui.md ; https://github.com/Ephemeral-AI-Lab/layerfs/blob/b5814bf3a8d886ef5e32298118dd1661c352bd2a/core/crates/layerfs-content/Cargo.toml |
| S5 wasi-vfs | v0.6.3；`0f4db4b64882db50ef8c04018e7e98bf59a63f92`；只读嵌入 FS/wasi-libc hooks | https://github.com/kateinoigakukun/wasi-vfs/blob/0f4db4b64882db50ef8c04018e7e98bf59a63f92/README.md |
| S6 Cordis effect 源码 | `1754062cef67cfc119fc7a2084c0321a231b9092`；进程内 journal、FnOnce cleanup、at-most-once claim、失败消耗；非持久事务 | https://github.com/dshbox/cordis-rs/blob/1754062cef67cfc119fc7a2084c0321a231b9092/crates/cordis-core/src/effect.rs |
| S7 Cordis README | facade/core 版本分开；generation 资源清理；drop handle 不自动 dispose | https://github.com/dshbox/cordis-rs/blob/1754062cef67cfc119fc7a2084c0321a231b9092/README.md |
| S8 Nix remote builds | 2.35.2 手册；可通过 SSH 委派构建节点 | https://nix.dev/manual/nix/2.35/advanced-topics/distributed-builds |
| S9 Cargo target 构建 | `cargo build --target` 的目标语义，非所有依赖自动可移植保证 | https://doc.rust-lang.org/cargo/commands/cargo-build.html |
| S10 SQLite commit/WAL | 原子提交的硬件假设；WAL FULL/NORMAL、checkpoint、跨多个 DB 不自动全局原子 | https://www.sqlite.org/atomiccommit.html ; https://www.sqlite.org/wal.html |
| S11 Git LFS pointer | 标准 pointer 的 version、oid、size；当前 oid 算法 sha256 | https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md |
| S12 实际 OpenSSH WASIX port | 先前已审阅的 community port；专用 imports/能力限制不可省略 | https://github.com/ai-ecoverse/homescoop/blob/main/packages/wasix-openssh/package/README.md |

网页检索参考标记：Nix remote builds、Cargo、SQLite atomic commit、Git LFS 分别由本轮 web open 返回；SQLite WAL 也已直接读取。root 若需在最终回答引用，直接重开以上 URL 即可。没有将本报告的设计推论冒充任何上游项目已经实现的功能。
