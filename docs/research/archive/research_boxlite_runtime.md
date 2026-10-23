# BoxLite：运行时、持久化与接入边界核验

研究范围：只读取上游文档、源码、GitHub release 元数据；未安装、启动、执行 benchmark 或连接用户机器。供父 Agent 合入 SPEC，不修改主稿。结论截至本次读取到的 2026-10-22 主线。

## 1. 结论

**BoxLite 适合作为可选的完整 Linux RuntimeProvider 候选，部署在具备硬件虚拟化能力的本机或自有远端节点；它不是 WASM runtime，不能用来宣称已完成本项目的 WASM/POSIX 沙箱、WASM SSH 或 Node/Bun WASM 目标。** 它把 OCI 工作负载放在有独立 Linux 内核的 microVM 内，当前生产后端为 libkrun。其磁盘 COW、clone、snapshot、export/import 值得接入，但当前实现不等于进程/RAM/网络状态的完整快照，更不等于跨宿主挂载、SQLite 控制账本、远端副作用的整体事务回滚。

固定研究基线：

- 主线 commit：`b59b8c1f39e45e49adede56a65749a431d1380e7`，提交时间 `2026-10-22T14:08:07Z`。
- 最新正式 release：`v0.10.7`，发布 `2026-10-18T00:37:38Z`；tag 指向 commit `e140b56b4c6195292cbcf26de76c406e71ab56f4`。
- 下文源码结论均来自上述固定主线，release 包体数字来自 v0.10.7。**主线新增能力不能直接写成 v0.10.7 发行包已具备**；真正接入时应选定一个发布闭包并重新核验。
- 一手入口：[固定主线](https://github.com/boxlite-ai/boxlite/tree/b59b8c1f39e45e49adede56a65749a431d1380e7)、[release](https://github.com/boxlite-ai/boxlite/releases/tag/v0.10.7)、[release API](https://api.github.com/repos/boxlite-ai/boxlite/releases/tags/v0.10.7)、[tag ref API](https://api.github.com/repos/boxlite-ai/boxlite/git/ref/tags/v0.10.7)。

## 2. 它是哪类隔离环境

执行结构是宿主 Rust runtime → 独立 `boxlite-shim` 进程 → libkrun microVM → Linux guest agent → libcontainer OCI 工作负载。硬件隔离依赖 Linux KVM 或 macOS Hypervisor.framework，宿主再用 jailer/seccomp 等机制限制 shim。它接受 OCI image 是用户接口和 rootfs 分发方式，不代表它只是共享宿主内核的普通容器；也不代表运行的是 WASI 模块。[S1][S2]

源码 `VmmKind` 默认 `Libkrun`，`krun/factory.rs` 注册该实现。新 `src/vmm`、`src/hypervisor` 正在开发；设计文档明确 **libkrun 在 M5 切换前继续原样提供服务**。内存快照、动态资源、热插拔 mount、GPU 位于 M6–M9 的后续空间；Windows WHP 为 M10。不能看到设计文档中有相应表格，就把未来能力写成当前产品功能。[S2][S3]

OCI “any image” 也应限定：`images/store.rs::detect_platform()` 使用 `os="linux"` 与宿主架构，`aarch64→arm64`、`x86_64→amd64`，选择匹配的 manifest。Apple Silicon 上仍是 Linux/arm64 工作负载；未找到现有路径提供跨 ISA 自动仿真或 Windows/macOS guest 的证据。[S4]

## 3. 宿主平台与硬件依赖

| 环境 | 已核验上游状态 | 对本项目的解释 |
| --- | --- | --- |
| Linux x86_64 / ARM64 | README 支持；必须可访问并实际使用 `/dev/kvm` | 可以是可选本地或自有节点 runtime；不是“只要 POSIX 就能运行” |
| macOS Apple Silicon | README 当前称 macOS 12+，使用 HVF | 可作为 Linux 工作环境，不代表操作 macOS 原生桌面；某些发行包平台 tag 更高，具体包要另核验 |
| Windows | README 只列 WSL2 x86_64 且需 KVM | 不是当前原生 Windows WHP 后端；Windows 桌面 Computer Use 仍是独立插件 |
| Intel Mac | README coming soon；新 VMM 设计排除 Intel Mac | 不列入已支持 |
| 路由器 | 未见专门 OpenWrt/musl/MIPS/32 位发行或验收；GNU Linux x64/ARM64 release 不能等同路由器普适支持 | 高配 x86/ARM64、KVM 可用的设备可能适配，须验证；通常由轻量主体访问远端 BoxLite 更吻合需求 |
| Android | 当前 `system_check.rs` 只接受 `target_os=linux/macos`，Android target 落入 Unsupported；未见正式 Android/Bionic 发行 | Android 使用 Linux 内核不足以推导可运行；OS gate、KVM 暴露、权限、ABI、应用生命周期需要独立移植验证；可先作为访问自有节点的客户端 |

上述 router/Android 结论是基于支持矩阵和依赖的**适用性判断**，不是证明所有特殊设备绝不可能运行。不能将 BoxLite 变成必须常驻的基础依赖，破坏本项目原有 WASM 基线与轻设备目标。[S1][S5]

嵌套虚拟化另有 RC：仅 local runtime；Linux x86_64 需 nested KVM enabled；macOS 需 M3+、macOS 15+；Linux ARM64 和 Intel Mac 不支持。若 BoxLite 自己已运行在外层 VM 中，还需要外层 hypervisor 暴露所需能力。它不采用无硬件条件时自动退回软件仿真的策略。这个 RC 不是运行普通 Box 的普遍要求；它用于 Box 内再启动 VM。[S6]

## 4. 资源与体积：哪些数字真的可证实

### 4.1 当前生产代码默认是 1 vCPU / 1024 MiB guest memory

`runtime/constants.rs` 定义 `DEFAULT_CPUS=1`、`DEFAULT_MEMORY_MIB=1024`；`vmm/krun/engine.rs` 在未指定配置时确实使用这两个值。配置文档仍写 512 MiB，所以不能把该文档数字当当前实现事实。**1024 MiB 是配置的 guest 内存额度，不是测得的常驻 RSS；也不是已证实的最小可运行内存。** [S7]

存在 `DEFAULT_DISK_SIZE_GB=10` 常量，不足以单独证明所有未指定磁盘的创建路径都按 10 GiB 分配；rootfs 文档明确磁盘不小于 image、设置 `disk_size_gb` 用于扩大。不得把稀疏盘的逻辑容量当实际占用，也不从常量直接推导产品磁盘下限。

### 4.2 release asset 的真实压缩包尺寸

| v0.10.7 发布物 | GitHub API bytes | MiB（bytes / 2²⁰） |
| --- | ---: | ---: |
| runtime Linux x64 GNU | 28,138,146 | 26.835 |
| runtime Linux ARM64 GNU | 31,770,602 | 30.299 |
| runtime macOS ARM64 | 29,637,427 | 28.264 |
| CLI Linux x64 GNU | 40,679,722 | 38.795 |
| CLI Linux ARM64 GNU | 44,324,017 | 42.271 |
| CLI macOS ARM64 | 41,101,123 | 39.197 |

这些是已发布 `.tar.gz` artifact 大小，不是单个 Rust kernel 的体积、解压大小、RSS、guest image 总大小或全部安装占用。不能把 SDK 包装层的几十 KB npm 包当完整 runtime。[S8]

找到 `sdks/node/README.md` 中约 50–100 MB/Box 的性能列表，但没有在该条目旁提供硬件、image、负载、采样方式与原始结果；发现的 benchmark 主要是启动与 import/export。故本轮**没有独立、可复现证据证明最小 RSS 或本地产品的低资源保证**。上游发布物和源码默认是事实；启动时间、idle RSS、fork 峰值、export 磁盘放大与并发可用数仍需后续在目标设备上测。[S9]

## 5. 持久化、停止、脱离宿主进程是不同合同

Box 的 rootfs 通过 per-box QCOW2 overlay 持久化，metadata 在 `~/.boxlite/db/boxlite.db`；缓存的 OCI layers、ext4 bases、clone bases 与 per-box 磁盘构成依赖闭包。直接删除基础盘可能破坏仍依赖它的 Box。export 能 flatten container backing chain，不能简单搬一个顶层 QCOW2 就当迁移完整。[S10][S11]

**关键默认陷阱：本地主线 `BoxOptions::default()` 的 `auto_delete=None` 会回退到旧 `auto_remove=true`，因此本地默认 stop 会删除 Box。** 持久家园必须显式 `auto_delete=Some(0)`。REST 的缺省行为由服务器控制；`Some(n>0)` 在 REST 是停止后 n 秒删除，本地没有 sweeper，反而立即 stop 删除。不能把同一字段在本地/远端解释成完全相同计时语义。[S12]

`detach=false` 默认创建者 runtime 退出后 Box 停止；`detach=true` 支持宿主应用退出后独立运行并由新 runtime reattach。它不等于机器断电后保留进程内存。持久磁盘与 `detach` 应分别配置，不能用 detach 替代恢复系统。`detach=true` 与有效 stop-delete 策略会被拒绝。[S12]

本项目主循环不应被装进会因上游 idle policy 自动停掉/删除的唯一 Box；若采用远端工作环境，主循环仍按当前唯一主体与持续推理合同运行，worker 的 Box 可自行停止或保留。是否脱离父进程由产品生命周期决定，不需要额外引入人工审批。

## 6. snapshot / clone / rollback 实际保存什么

| 操作 | 源码实际行为 | 不能由它推导的能力 |
| --- | --- | --- |
| snapshot create | 暂时 quiesce；把 container `disk.qcow2` 移为 snapshot，再创建原路径 COW child；SQLite 登记 snapshot | 不是 RAM、CPU、线程、PTY、socket、REPL 全对象快照 |
| snapshot restore | 拒绝 active/running Box；用 snapshot 创建新的 container COW child；删除 guest-rootfs overlay，下次从 cache 重建 | 不恢复当时进程，不回滚宿主 bind mount、消息、远端机器或已完成网络副作用 |
| clone / batch clone | 创建共享 container base，再为每个新 Box 创建薄 overlay；克隆 Box 初始 `Stopped`；guest rootfs 首次启动新建 | 不复制正在运行的 execution，也不提供分支合并或 Git 式语义冲突解决 |
| export/import | flatten container disk；SHA-256 校验；manifest + disk 的 zstd tar `.boxlite` | 不是完整运行中 VM 迁移，也不是宿主挂载内容的备份；需匹配运行平台/能力与可用 guest assets |

以上来自 `local_snapshot.rs`、`snapshot_mgr.rs`、`clone_export.rs` 和 `archive.rs`，不是只采用 README 术语。[S11][S13][S14]

### 6.1 quiesce 质量不能写成无条件应用一致性

`BoxImpl::with_quiesce_async` 先进行 guest filesystem freeze RPC，再 `SIGSTOP` shim，做磁盘操作后 `SIGCONT` + thaw。源码明确 guest RPC 是 5 秒 timeout 的 best effort：失败仍继续，以其代码所称的 crash-consistent 模式执行。guest freeze 本身会跳过不支持 freeze 的 FS，对部分失败记录日志并返回成功冻结的列表。snapshot API 的成功不自动证明数据库应用已完成 checkpoint，也不能当全挂载一致性回执。[S15]

代码有 `.snapshot_pending` 恢复标记、已有 backing-chain 检查及恢复路径；这是实际恢复机制。与此同时，在已读到的 snapshot/rename/manifest 路径中，没有足够证据证明跨 SQLite、目录项与对象文件的严格断电提交合同：标记 `std::fs::write`、rename、COW 创建、DB 登记是分步发生，不能仅凭“atomic fork”的注释就推出电源故障原子性。本轮不把源码审读升级为已证实缺陷，也不把它升级为已证明的断电保证；需要故障注入与实际存储 profile 验收。[S13][S15][S16]

### 6.2 GC 可参考，但不替代本项目跨域 roots

`base_disk.rs` 追踪 box overlays 与 bases 完整 backing ancestry；孤儿回收用 DB row 与磁盘引用两个守卫、等待 grace period，数据库查询失败保留对象。snapshot 删除会拒绝仍被 live disk、其他 snapshot 或 clone base 引用的快照。这可作为实现参考，但本项目仍需额外保护消息附件、Git 历史、在途发布、子 Agent workspace、候选/上一代救援闭包等根，不能把上游 GC 当整个 Agent 的 GC。[S14][S17]

## 7. 挂载：本地与 REST 的能力不同

现有 `VolumeSpec.volumes` 可在本地把宿主目录或文件以 RO/RW 通过 virtiofs 分享给 guest，再绑定进 OCI workload。单文件 mount 有明确 subpath 处理；它不是只支持固定 `/workspace`。路径仍受宿主 OS 权限、jailer 和实际文件类型支持约束。[S18]

但新 `BoxOptions.mounts: Vec<MountSpec>` 目前只是类型与校验先行：本地 `reject_local_unsupported_options` 明确返回 `typed mounts are not supported ... yet`；REST 也拒绝。不能只读到结构体就宣布新 typed mount API 已实现。[S19]

REST `sanitize_remote` 还有两条直接影响规格的限制：

- 拒绝 `VolumeSpec` 的 host bind path，要求 remote managed volume。路径来自谁的机器不是可忽略细节。
- 当前拒绝 `read_only=true` 的 managed volume，而不是静默降级为 RW。

因此，想在自有远端 BoxLite 节点挂“该节点任意已授予宿主路径”，需要节点侧可信本地 adapter 创建/管理映射，或另行扩展受控远端协议；不能声称现有公共 REST 已完成。用户原本的任意挂载目标必须保留，只在 provider capability 中准确标明兑现路径。[S19]

**clone 不是宿主卷隔离。** `clone_export.rs` 复制原 `BoxOptions`，因此克隆仍可引用同一 host path；磁盘 COW 不会自动让宿主共享目录分叉。用于多方案研究时，worker adapter 必须为可写宿主目录单独准备 worktree/COW workspace/独立目录或明确共享语义，避免孩子互相覆盖。默认 host-mounted Git 也应沿用现有 worktree/串行发布合同。[S11][S18]

## 8. 凭据与归档边界

BoxLite “真实 secret 不进入 VM”的网络注入机制，不等于真实 secret 不落宿主磁盘。`Secret.value` 的源码注释明确该值参与序列化，用于 DB 与 shim 配置管道，只对 Debug/Display 隐藏。[S12]

更进一步，`do_export_finalize` 把 `Some(config_options.clone())` 放入 archive manifest，`ArchiveManifest.box_options` 保存完整配置，而 archive builder 打包 manifest。因此在包含 secrets 的配置下，不能默认将 `.boxlite` export 视作无秘密、可公开或可写入普通记忆 Git 的产物。**这是序列化调用链的推论**；本轮未生成真实归档验证。应在 adapter 设计中要求 export 检查、secret 引用重绑定/脱敏或受控加密归档，并重新验证 mount、权限与接入凭据。[S11][S12][S20]

`.boxlite` archive 的 checksum 证明读到的内容匹配归档内的声明，不证明发布者身份、授权范围或可信软件来源。源码对 REST 上传的 archive 拒绝 host rootfs 路径、卷、自定义内核/嵌套等高权限配置，也说明跨信任域 import 并不是无条件保留所有 local options。[S21]

## 9. 远端完整 Linux runtime 的实际价值

BoxLite 提供库嵌入、本地 CLI、REST 服务及 host/guest 文件传输；REST 的 snapshot/clone/export 操作会先检查服务器是否开启对应能力。这让“轻设备主体 + 自有重型 Linux 节点”的插件路线有现成候选。它可容纳当前难以原生移植到 WASM 的工具链、Linux memory 系统或全功能 Python/Node/Bun REPL，而这些仍必须明确标记为 Linux runtime 的能力，不作为对应 WASM 条目的验收替身。[S1][S22]

本项目适合包一层 `BoxLiteRuntimeProvider`：提供可检验的 engine/版本、目标架构、exec/PTY/copy/mount 能力、磁盘快照等级、持久化策略、资源用量和 node identity。主体只委派，有限 worker 获得操作该 provider 的 capability。全局模型 broker 与单主体 epoch、原始会话、身份和副作用 ledger 继续由现行框架合同约束；Box 的恢复不能复活旧 worker 或倒退授权状态。

## 10. 对现有 SPEC 的增改建议（8 条）

1. **§7–8 / §24：加入可选 `BoxLiteRuntimeProvider`。** 定义为本地/自有远端 Linux microVM provider，WASM 仍为基础目标；可供 native Linux 工具、REPL、构建与 Linux GUI 工作负载使用，不替代真实 WASM 验收，也不等同 Windows/macOS 桌面后端。
2. **§8 / §22：增加版本化能力矩阵。** 记录当前 libkrun、受支持架构、KVM/HVF、local/REST 差别；新 VMM、RAM snapshot、WHP、hot-plug 和 nested RC 均按真实阶段标注。router/Android 保持条件式可行性，优先可访问远端节点。
3. **§10 / §23：显式配置生命周期并验收。** 持久 home 设置 `auto_delete=0`；将 detach、stop、host exit、host reboot 分开；读取服务器实际 idle/delete policy；不把上游缺省创建方式直接用作永续个人家园。
4. **§10 / §19：给 snapshot 声明 `disk-only` 等级。** 恢复需 stop，新进程从磁盘重建；单独记录 quiesce 是否降级；REPL 依靠 journal/对象恢复时不能报 full VM restore。消息、身份、撤销、删除墓碑、外部副作用 ledger 与默认 host Git 继续放在不能被工作区回滚的持久域。
5. **§8 / §10：为本地、REST 和自有节点挂载写清路径。** 已实现的 VolumeSpec 与未实现的 typed mounts 区分；REST host bind/managed RO 限制明示。通过已授予节点 adapter 兑现任意可访问路径；研究 clone 必须重绑定可写 workspaces，不能继承同一 live host Git 工作树。
6. **§9–11：定义可迁移闭包及凭据过滤。** flatten/export 不自动备份 host mounts；迁移带 image/engine/guest assets/architecture 与 checksum。归档清理或重绑定 raw secret、host path 和 capability，恢复时重新取得有效 lease；不用普通记忆 Git 直接承载含秘密的 BoxOptions 归档。
7. **§10.5 / §23：复用磁盘机制，保留独立 durability/恢复合同。** snapshot 返回与框架 `committed/local-durable/replicated` 的语义必须对齐；补跨 disk/DB/marker、满盘、强杀、断电、GC 与恢复未知副作用的验收。BoxLite 自带 snapshot 不能替代 kernel/rescue A/B 交接，也不能回退已经发生的事实。
8. **§22：完整核算资源，不把项目名中的 Lite 当指标。** 参考代码默认 1 vCPU/1024 MiB 与 release runtime 约 27–30 MiB 压缩包，但分别报告 runtime/shim、guest、image cache、工作集、clone/export 峰值和默认常驻插件总成本。只有测得目标设备数据后才承诺低端本地部署；插件未启用时不强制加载整套 VM 栈。

## 一手来源索引

所有 `blob/` 下链接固定为主线 SHA `b59b8c1f39e45e49adede56a65749a431d1380e7`。

- **S1** [README：架构、接口、平台和系统要求](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/README.md)。
- **S2** [Architecture：生产 libkrun、shim、guest/container 分层](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/contributing/architecture/README.md)；[KrunFactory](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/vmm/krun/factory.rs)。
- **S3** [新 VMM 设计：L3–14、L28–50 的当前/未来边界](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/contributing/architecture/vmm/README.md)。
- **S4** [image 平台选择：detect_platform / select_manifest_for_platform](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/images/store.rs)。
- **S5** [README 支持矩阵](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/README.md)；[system_check.rs：OS gate 与 KVM smoke test](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/system_check.rs)；[v0.10.7 实际 artifact 平台](https://api.github.com/repos/boxlite-ai/boxlite/releases/tags/v0.10.7)。
- **S6** [Nested virtualization RC](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/guides/nested-virtualization.md)；[Custom kernel RC](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/guides/custom-kernel.md)。
- **S7** [runtime/constants.rs：L60–69](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/runtime/constants.rs)；[krun/engine.rs：L298–306 的实际缺省取值](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/vmm/krun/engine.rs)；[存在出入的配置文档](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/reference/configuration.md)。
- **S8** [release API 的 assets[].size](https://api.github.com/repos/boxlite-ai/boxlite/releases/tags/v0.10.7)。
- **S9** [Node SDK：Performance 列表](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/node/README.md)；[import/export benchmark 源码](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/tests/import_export_benchmark.rs)；[REST startup benchmark](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/apps/e2e/bench_rest_startup.py)。
- **S10** [File formats：磁盘、cache、SQLite 的实际布局](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/reference/file-formats.md)。
- **S11** [clone_export.rs：clone batch、L149 Stopped、flatten、L322 完整 BoxOptions 进入 manifest](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/clone_export.rs)。
- **S12** [runtime/options.rs：auto_delete L413–454、Secret L521–536、default L585–634](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/runtime/options.rs)。
- **S13** [local_snapshot.rs：L56 写 pending marker；L120–143 restore 的 stop gate；L177 起 recovery](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/local_snapshot.rs)。
- **S14** [snapshot_mgr.rs：L139 起 container disk 保存；L285 起 restore；L322 guest-rootfs 重建；remove 的依赖检查](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/snapshot_mgr.rs)。
- **S15** [box_impl.rs：with_quiesce_async L1385–1545](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/box_impl.rs)；[guest fsfreeze 的实际跳过/错误路径](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/guest/src/storage/fsfreeze.rs)。
- **S16** [disk/mod.rs：fork_qcow2 的 rename/COW 与 EXDEV copy+remove 路径](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/disk/mod.rs)。
- **S17** [base_disk.rs：backing roots、grace period、DB error fail-closed](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/disk/base_disk.rs)。
- **S18** [volumes 使用文档](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/guides/volumes.md)；[container_volume.rs：文件 subpath、RO、virtiofs 到容器的映射](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/volumes/container_volume.rs)；[guest_volume.rs](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/volumes/guest_volume.rs)。
- **S19** [rt_impl.rs：L1758–1787 local typed mounts 拒绝](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/runtime/rt_impl.rs)；[rest/runtime.rs：L262–308 host bind、managed RO、typed mounts 限制](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/rest/runtime.rs)。
- **S20** [archive.rs：L57–85 manifest full BoxOptions；L90–136 tar 包内容](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/archive.rs)。
- **S21** [runtime/import.rs：trusted/untrusted import 与 options/checksum 校验](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/runtime/import.rs)。
- **S22** [rest/litebox.rs：clone/export 与 SnapshotBackend require_*_enabled](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/rest/litebox.rs)。

## 证据限度

没有本地 build/运行、RSS/冷启动/压力测量、Android/OpenWrt 移植验证、full VM memory snapshot 实验、power-cut durability qualification 或带真实凭据的 export 测试。无法从源码审读证明任意 OCI image、任意设备、任意数据库事务与任意崩溃状态都受支持。文档默认值与实现已出现差异；接入验收应以选定发行闭包和实际能力探测为准。父 Agent 若使用 web citation 需要自行打开相应来源；本笔记的原始依据由 GitHub connector 读取固定文件，精确 URL 已列出。
