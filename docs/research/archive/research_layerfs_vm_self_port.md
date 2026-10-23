# LayerFS 原版进入 Linux VM；WASM 独立状态层与自主移植闭环

范围：产品规划与规格研究；未开发、安装、运行 LayerFS/BoxLite 或执行组合测试。本笔记依据已固定的一手版本做窄核验，替代此前“裁剪/移植 LayerFS 到 WASM”方向的建议，不重做广泛运行时选型。

## 1. 用户最新决定及对既有建议的覆盖

**已确认：LayerFS 不重写，作为现有 Linux 软件直接用于 vmm / 真实 litevm 环境。WASM 环境的状态、文件系统、包与恢复层按本产品最新设计独立实现。** 两者通过插件/provider 合同接入，不共享一个必须相同的底层实现。

因此应删除或明确标为已废弃：把 LayerFS WASM port、抽取其 content crate、替它重做 VFS 列为本项目默认交付前置条件。可以研究其分支/内容寻址设计，但不能再把移植 LayerFS 本身当成用户要求。LayerFS 的原版适配、打包、配置、外层协议和兼容测试仍属于必要接入工作。

**另一条保留的自主性要求：**遇到缺少的 WASM 工具、接口或依赖，Agent 应能自行研究、选择方案、委派实现、在本机或远端构建、验证、安装与回滚；任何部件均可通过候选版本替换。主体仍不直接操作文件或执行工具，实际操作由子 Agent/provider 完成。LayerFS 不重写是本次规划的明确选择，不把它外推为“所有第三方软件或所有内核以后永久不可改”。

## 2. 固定核验基线

| 项目 | 基线 | 本次用途 |
| --- | --- | --- |
| LayerFS 发布版 | v1.0.0，commit `2aaf2552ce21154928b9f9c83582e41a637f5910`，发布 2026-10-21 11:21:25 UTC | 核验实际 SDK、Docker/FUSE、Store 与 Disposable 合同 |
| BoxLite 主线 | commit `b59b8c1f39e45e49adede56a65749a431d1380e7`，2026-10-22 14:08:07 UTC | 判断当前完整 Linux guest 的适配基础与尚待验证的组合条件 |
| BoxLite 最新 release | v0.10.7，tag commit `e140b56b4c6195292cbcf26de76c406e71ab56f4`，2026-10-18 | 不把之后主线代码、未来 VMM 设计当成此发行包已具备 |

LayerFS v1.0.0 为 source-only，没有官方 npm package、预编译 binary 或公开 image；SDK、native addon 和 daemon 要来自同一 tag。旧 Store 会被拒绝，不带自动迁移。产品要构建并固定自己的完整部署闭包，而不是只记录一个 npm 版本号。[L1][L2]

## 3. “Linux VM 内可用”至少需要什么

### 3.1 Store 直接访问和 Workspace 执行分开

| 层/动作 | 真实要求 | 不能偷换的结论 |
| --- | --- | --- |
| SDK `fs` / `branch` / Store 查询 | Node 20+ 与匹配 native addon；Linux；已安装的 Store 路径；Store 位于本地 ext4/xfs/btrfs | 这些调用在当前进程直接开 Store，不会为每次调用启动 Sandbox；Docker 不在每次 Store fast path 内 |
| 初始化 Store | 明确 `storePath`、私有 `LAYERFS_HOME`、源目录、固定格式与策略；SDK 在当前进程创建并安装 Disposable Store | 不等于已经建立了可执行 Workspace，也不等于任意主机路径可用 |
| 标准 `workspace.mount/exec/commit/unmount` | Linux Docker Engine、可用 `/dev/fuse`、相应挂载 capability、安全配置、既有 plain local named volume、匹配 Linux daemon 和不可变 runtime image digest | 不是有 Linux 内核就全部可用；也不能只安装 Docker CLI 而无可用 Engine |
| 多个 SDK/daemon 打开同一 Store | **同一 Linux 内核、同一本地文件系统和可协调的 SQLite WAL/flock** | 不能把同一 SQLite 经 virtiofs/NFS/远端共享目录交给多个 VM 同时直开 |
| VM 状态持久化 | 保留 VM/Box 磁盘及 Store 依赖闭包，明确停止/删除策略 | guest 磁盘持久化不自动提升 LayerFS Store 的 crash/power-loss 保证 |

最重要的一手证据是 `kernel_guard.rs`：在打开 Store 前，对其父目录做 `statfs`，allowlist 只有 ext4、xfs、btrfs；非 Linux 直接 Unsupported。其注释明确所有 opener 必须共用一个 Linux kernel，原因是 WAL 与目录 flock 的协调范围。[L3]

这使“宿主磁盘是 ext4”不足以通过验收：**检查对象是打开 Store 的进程所见目录**。该目录如果在 guest 中表现为 virtiofs、overlayfs、tmpfs、NFS 或其他不在 allowlist 的类型，就不能把宿主底层文件系统拿来代替。即便手工使两个 VM 都看到相同 bytes，也不代表跨内核锁已成立。

### 3.2 标准 Docker Sandbox 的具体条件

LayerFS v1.0.0 的 Docker 请求代码使用 `/v1.54/...` 路由；部署资格需验证实际 Engine 能接受该协议，不能只写“任意 Docker 版本”。`plain_volume()` 要求既有 volume 的 `Name` 一致、`Driver=local`、`Scope=local`，且 driver options 为空。不是所有 Docker volume driver、网络卷或带 options 的 bind-volume 都被该路径接受。[L4]

创建 daemon 容器的配置包含：`/dev/fuse` 的 rwm 设备访问、`CAP_SYS_ADMIN`、`no-new-privileges`、专用 seccomp、`apparmor=unconfined`、Engine init、无自动 restart；Store volume 唯一挂入 `/layerfs-store`，控制端口绑定 Docker host 的 loopback。daemon 的挂载能力与命令的非 root 身份分离，不能简单把全部工作负载标为 privileged。[L4]

在 Linux VM 中运行 Docker containers **不等于再运行硬件 VM**，所以普通 Docker-in-guest 不应误要求 BoxLite 的 nested-virtualization RC。它实际需要的是 guest 的 namespaces、mount/FUSE、设备可见性、cgroup/存储/网络等条件。若是在 BoxLite 默认 OCI workload 里面再启动 Docker，还多一层外部 OCI 权限与设备配置；必须测试该组合，不能由“guest 是 Linux”推断完成。

## 4. BoxLite 能提供什么基础，哪些仍未证明

当前 BoxLite `USE_DISK_ROOTFS=true`，container rootfs 构建为 ext4 image，再经 QCOW2 COW block device 使用；该模式覆盖 `USE_OVERLAYFS`，并非默认总是 virtiofs + overlayfs。这提供了让 LayerFS Store 落在 guest 本地 ext4 的现实基础。[B1][B2]

但 BoxLite 默认工作负载的 capability baseline 不包含 `SYS_ADMIN`；存在可配置 capability API，不等于默认 `/dev/fuse`、FUSE kernel support、Docker daemon、其所需 cgroup/设备/mount 传播及 Docker API 全部可用。[B3] 本轮没有发现或运行“固定 BoxLite image + 原版 LayerFS v1.0.0 + Docker/FUSE + 多 Workspace”的整体验收。因此应将它写成 **待资格验证的 LayerFS VM profile**。

推荐按以下顺序兑现已经确定的产品路线：

1. 为 RuntimeProvider 定义能够承载原版 LayerFS 的 Linux profile，要求实际探测的能力清单与固定部署闭包。
2. 优先验证 BoxLite 的 local/self-hosted 节点路线；需要的 guest image、设备与配置由 worker 自动构建并测试。
3. 如果该 BoxLite 版本不能兑现某项必需条件，接入能够提供同一合同的真实 Linux VM provider，或使用已有被授予的 Linux 节点。准确报告选中的 backend，仍直接运行原版 LayerFS。

这不是重写 LayerFS 的理由，也不是把它搬回 WASM 的默认退路。BoxLite 此前确认的本地 `auto_delete=0`、detach 与正常 stop 分离、host/REST mount 能力差异继续适用。[B4]

## 5. 推荐适配拓扑

以**一个 Linux VM 为一个 LayerFS Store 的直接访问域**。在该 VM 内放置节点 adapter、Node SDK/native addon、Docker Engine、LayerFS daemon/Sandbox，以及位于 guest ext4/xfs/btrfs 的 Store named volume。它们共用同一 kernel，远端 Agent 不直接打开 SQLite。

本产品主体通过有限子 Agent 委派 `fork/read/write/mount/exec/commit/merge/publish`；子 Agent 调用节点 adapter，adapter 返回结构化回执与出处。LayerFS 原生 API 没有完整的本产品 task/agent attribution，外层 operation ledger 必须绑定 task ID、worker instance、scope、branch、workspace、node、runtime generation 与 invocation ID。[L2]

同一 VM 的多个有限 worker 可以各自占有 branch/workspace；需要跨机器执行时，由服务侧 adapter 串行/并行操作该 Store，再传输版本化成果，或者每台 VM 使用不同 Store 并做显式同步。现版并没有已经验证的“多个 Linux kernel 共享一个原版 SQLite Store”合同。

WASM 则继续使用本产品的独立状态层、包 generation、host mount 和恢复协议。默认 host-mounted Git 记忆、原始 session、身份、权限撤销、删除墓碑及副作用账本不因为启用 LayerFS 而迁入唯一可丢失 Store。需要将研究成果变为正式知识或包时，发布 worker 按现有 durable publication 合同提取、校验并登记，留下 LayerFS commit/branch 来源。

## 6. Store/Disposable 与 VM snapshot 的语义

LayerFS 有三个容易混淆的词：

- **Workspace 临时性**：未提交 overlay 在 unmount 后消失，已提交历史保留在 Store。
- **Store 跨普通调用的保留**：branch/commit/content 存在 SQLite 文件中，并不是每次命令结束清空。
- **crash/power-loss durability**：v1.0.0 明确不提供。SDK 文档为 Disposable/WAL/`synchronous=OFF`；这不是本产品可以用“Store 会保存”替代掉的区别。[L1][L2][L5]

同 tag 的 `layerfs-persistence` README 描述一个 under qualification、no release admission 的 macOS FULL/fullfsync profile。它不能拿来证明在 Linux VM 中的发布路径已获得 durable 模式；该文档与实际发布 Disposable 路线要按层区分。[L6]

把 Disposable Store 放在 QCOW2 上不会自动修复缺失的提交同步。BoxLite 的 disk snapshot 也不等于 SQLite 应用 checkpoint，更不会保存所有 live process。若本产品承诺某个研究成果或记忆保存已经 durable，必须由本产品另行完成可验证的持久发布；不能仅把 LayerFS `Committed` 原样当 `durably_committed`。

推荐两级回执：

1. `layerfs_committed`：记录该 Store 的 commit 与当前可读证据，明确上游 Disposable 级别。
2. `product_durable`：选定成果、必要历史/对象闭包、来源与恢复依赖已经按本产品持久化协议提交并确认；具体保留完整 LayerFS 历史还是选定发布成果由产品策略声明。

如需保存整个 LayerFS Store，可研究外层受控 quiesce/一致性备份与恢复验证，不改 LayerFS 源码；但不能热复制单个 `.sqlite` 文件而忽略 WAL，也不能先发 durable 回执再异步制作唯一备份。VM 快照、Store 备份、正式记忆 Git 和软件 A/B 恢复分别编号，不把一个“快照成功”覆盖四种保证。

LayerFS SDK 对 daemon/container 重启后的 Workspace 恢复与不确定操作重放明确排除；未知 create 可能保留 intent 并阻塞后续 start。产品 adapter 应检查真实 container/daemon incarnation 与已知 commit，完成有证据的 reconcile，不无条件重放 exec，也不通过删 intent 假装未知效果不存在。[L2]

## 7. LayerFS VM profile 的必要验收

| 验收 | 通过条件 |
| --- | --- |
| 部署闭包 | SDK、native addon、daemon 来自同一 LayerFS tag；Linux 架构、Node ABI、image digest、Engine API 可追溯；不混用 main 与 release |
| Store 所在 FS | 从真正 opener 进程检查 Store 目录为 ext4/xfs/btrfs；相同 Store 的 SDK/daemon 共用 kernel；不支持的 FS 明确拒绝 |
| Docker/FUSE | named plain local volume、Docker API、`/dev/fuse`、capability、seccomp/mount 条件通过；真实 mount→exec→commit→unmount 完成 |
| 多 worker 分支 | 独立 branch/workspace；共同可见正确历史；同文件冲突明确拒绝，只有显式 `.lww` 规则适用；不把 branch 误当授权隔离 |
| 大文件/依赖 | 实际二进制修改、包目录、rename-over、软/硬链接等所需语义可读回；不借 README benchmark 数字保证本产品性能 |
| 输出与开销 | 长 exec 输出采用有界 sink；记录 Node native worker、daemon、Docker、VM、Store/WAL/overlay 总成本；不将上游线程池 64 默认值照搬为轻设备预算 |
| 生命周期 | 普通 unmount 后已提交历史仍在；VM stop/restart、daemon 消失、Box 不自动删除；丢失的 Workspace 句柄不会伪装恢复成功 |
| 未知操作 | create/exec/commit 结果丢失有 invocation 与状态核对；未知外部效果不重放；需要重新委派时生成新 worker epoch |
| 持久回执 | `layerfs_committed` 与 `product_durable` 可区分；在强杀/满盘/中断及目标 durability profile 下恢复已确认成果；实际不能保证的级别如实显示 |
| 主体边界 | 主体仅委派或语义 memory 接口；所有文件/命令/分支由 worker/provider 执行；runtime 正常落盘不被误判为主体文件能力 |
| 全局资源 | LLM 调用仍经共享 broker；VM CPU/RAM/IO/worker 数另外计量；LayerFS Sandbox 席位不等于推理并发许可 |

这里的 daemon idle shutdown 可保留，它是有限执行环境的回收；**不得用它替换持续的个人主体思考 loop**。同理，原版 mount placement 的 16 Workspace/Sandbox、连接数与线程池是 adapter 要测量/处理的事实，不是本产品固定的 Agent 数或全局 KV 槽数量。[L2]

## 8. 自主 WASM 移植闭环：应成为正式产品能力

以下是建议工程合同，不宣称任何给定 Linux 软件必然能够自动移植成功。

### 8.1 状态与职责

主体收到“缺命令/缺包/缺接口/性能不足”的结构化事件，结合任务与资源偏好委派有限 porting worker。worker 能创建研究分支、查现成构建、改配方或源码、调用本地/远端 BuildProvider、运行验收、生成插件/包候选并报告。runtime/provider 保存完整 attempt ledger；主体通过语义 memory 了解已尝试方案和事实，不直接打开或改文件。

推荐状态：`observed → classified → candidate_selected → building → ABI_validated → behavior_verified → staged → activated → healthy/rolled_back/deferred`。每一步有 artifact/日志引用、状态 epoch、实际成本和失败分类；`built` 不等于 `runnable`，`runnable` 不等于用户需要的行为已完成。

### 8.2 候选路线与诚实标记

| 路线 | worker 可自主采取的工作 | 必须保留的语义 |
| --- | --- | --- |
| 现成兼容包 | 查已安装包、锁定 registry artifact、验证签名/哈希与依赖闭包、安装候选 | 相同名称不代表相同 ABI；不能把下载成功当可运行 |
| 源码交叉构建 | 获取固定来源，选择 sysroot/编译器，编写 recipe，补可移植代码，在真实目标运行验收 | 编译器在远端 Linux 运行、产物在 WASM 运行是有效路线，不要求路由器本机装 rustc/LLVM |
| WASI/WASIX/专用 host imports 适配 | 实现缺失的版本化接口或 guest shim，替换 runtime/plugin 候选，验证语义和资源作用域 | imports 与所需宿主能力可见；WASIX artifact 不能悄悄当标准 WASI；模拟完整 native 工具不能标为真实 guest 执行 |
| 工具/依赖移植 | 由有限 worker 研究 patch、替代依赖、构建/测试目标，保存失败与可恢复版本 | Node/Bun/Python 等具名目标逐一验收；兼容 JS 引擎或 transpiler 不能冒充完整 runtime |
| 远端/native 临时履约 | 已获授予的 Linux VM、SSH 节点或本机 provider 先完成当前任务，同时保留 WASM 移植研究项 | native/remote 履约可以有价值，但不能自动关闭真实 WASM 需求；执行位置明确 |

顺序由成本、任务急迫性、现成证据和能力差距决定，不必硬编码每次都从头穷举。多个候选可在独立 branch 试验，但每轮有预算、比较指标、停止/延后条件，避免递归搜索无限扩张。移植 worker 的 LLM 与所有孩子仍经唯一资源 broker；构建机 CPU/内存/磁盘也需要独立 admission。

### 8.3 ABI 与行为验收

包 manifest 至少绑定源码/patch/recipe digest、工具链/sysroot、WASM artifact digest、WASI preview 或 WASIX ABI、import/export/WIT 版本、所需 runtime features、依赖闭包、入口、mount/network/process/TTY/credential 能力及资源预算。

安装前检查结构有效性、imports 是否可提供、线程/shared memory/异常/SIMD 等所需 feature、启动与取消、目标平台；再验证实际用例。POSIX 工具特别检查路径与 symlink、文件锁、stdio/PTY、spawn/exec、信号、网络/socket、时钟/随机数等它真正依赖的部分。只编译成功或输出 `--version` 不足以证明 `ssh` 认证/文件传输、private Git credential helper、Python native package 或 Bun/Node API 已完成。

远端编译不代表远端自动获得宿主全部权限；worker 只使用已有 capability 和可更换 provider。缺的是实现时，它可自主补实现；缺的是尚未授予的外部账号/机器权限时，如实报告并按既有自主联系逻辑处理，不能通过源码修改伪造授权。不要为每个编译、测试、安装步骤另加默认人工审批。

### 8.4 发布、失败与任何部分可替换

验证后的产物进入 Nix-like 候选 generation，先准备完整依赖闭包与必要状态迁移，再在安全边界激活；健康检查失败回退到仍可启动的上一代。已记录会话、身份/撤销、删除墓碑与外部副作用不随软件回滚倒退。失败 attempt 的证据保留，临时 workspace 可按策略回收。

这条闭环覆盖包、工具 adapter、host imports、WASM runtime、调度器、验证器、kernel 与 rescue 的候选替换，**没有永久不可改的 Ring 0**。验证器与恢复器本身改动时，也由当前有效代完成交接并保留上一可启动闭包；“保留回退版本”不等于冻结源码。

主体是否继续投入、改选远端履约、暂缓或激活候选，由它结合收到的运行偏好与验证结果自主决定。工具安装与调用教学同步产出短 schema/recipe，帮助本地模型正确发现和使用新能力；运行中的权限、配额与协议合同不因这次自改而无声改变。

## 9. 对 SPEC 的具体修订建议

1. 将 LayerFS WASM port 从交付路线移除并注明已被最新决定覆盖；保留原版 Linux VM provider 和独立 WASM 状态层两条明确路径。
2. 为 LayerFS VM profile 增加同 kernel、本地 FS allowlist、Docker/FUSE/Engine API、完整版本闭包的资格合同，不把“有 Linux”当可用结论。
3. 把 LayerFS branch/workspace 绑定子 Agent 与 scope；远端机器经节点 adapter 操作，同一 Store 不跨内核直接共享。
4. 将 Disposable、LayerFS commit、产品 durable commit、VM disk snapshot 与 software rollback 分开；默认 Git/事实账本继续不受研究工作区回退影响。
5. 将自主发现、研究、编译/移植、运行时补接口、验收、安装与回退列为正式能力；明确远端构建和真实 WASM 执行可组合，替代执行不冒充移植完成。
6. 维持唯一持续主体、实际动作在 child/provider、全局 LLM 并发和任何部件可替换；新增工具不应迫使主体直接执行或恢复永久代码禁区。

## 一手来源

- **L1** [LayerFS v1.0.0 release](https://github.com/Ephemeral-AI-Lab/layerfs/releases/tag/v1.0.0)；[release API](https://api.github.com/repos/Ephemeral-AI-Lab/layerfs/releases/tags/v1.0.0)：source-only、版本匹配、Disposable、Linux FS 与旧 Store 兼容边界。
- **L2** [v1.0.0 SDK README](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/sdk/js/README.md)：L33–58 Store fast path/线程池；L60–123 环境、Sandbox/Workspace、unknown 与重启排除；Builds and compatibility 的同步/格式限制。
- **L3** [kernel_guard.rs](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/core/crates/layerfs-store-service/src/kernel_guard.rs)：同 kernel、statfs allowlist 与拒绝行为。
- **L4** [Docker container.rs](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/core/crates/layerfs-sandbox/src/backend/docker/container.rs)：L12–40 FUSE/capability/security/volume/loopback；L43–87 plain local volume 与 v1.54 路径。
- **L5** [SDK native project.rs](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/sdk/js/native/src/project.rs)：L39–50 实际 Store path/guard；L68–71 `SqlitePersistenceProfile::Disposable`；[direct.rs](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/sdk/js/native/src/direct.rs)：进程内 Store service，无 fallback。
- **L6** [persistence README](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/core/crates/layerfs-persistence/README.md)：under qualification、macOS FULL/fullfsync profile、非硬件掉电证明。
- **L7** [固定 release README](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/README.md)：Node 20、Linux/Docker/local FS、Store/Workspace/conflict 分层、benchmark 限度。
- **B1** [BoxLite init/types.rs](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/init/types.rs)：L21–32 `USE_DISK_ROOTFS=true` 覆盖 overlayfs 路线；L142–157 block image。
- **B2** [container_rootfs.rs](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/init/tasks/container_rootfs.rs)：L189–214 选择 disk rootfs；[guest block_device.rs](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/guest/src/storage/block_device.rs)：guest 中真实 block mount。
- **B3** [BoxLite capabilities.rs](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/guest/src/container/capabilities.rs)：默认 baseline、不含 SysAdmin 的测试与可配置策略；[capability API design](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/contributing/architecture/container-capabilities.md)。
- **B4** [BoxLite runtime/options.rs](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/runtime/options.rs)：stop/delete/detach；[nested virtualization RC](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/guides/nested-virtualization.md)：这是 guest 内再运行硬件 VM 的功能，不是 Docker 容器的同义词。

## 证据限度

未运行固定版本组合，未确认 BoxLite 的特定 guest image 能满足 Docker/FUSE 全部条件；未证明 LayerFS Store 经 VM 快照即具有 crash durability；未实现跨 VM 的 Store transport、备份协议或自主 WASM port。后半部分是明确的产品合同建议。既有 WASM、SSH/Git 与 REPL 可移植性研究继续适用，但不把此前未证实的 Bun/Node 完整 WASM build 或任意 POSIX 软件兼容性写成既成事实。
