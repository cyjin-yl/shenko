# WASM、持久化与可恢复 Agent：研究记录

研究时间：2026-10-23（Asia/Shanghai）。仅研究与规格建议，未开发、未修改上游。

## 核心发现

1. **WASM 可移植性、POSIX 源码兼容、Linux 二进制兼容是三个不同承诺。** WASI/WASIX 可以运行针对其 ABI 构建的软件；现有 Linux ELF、任意 Python native wheels、CUDA/torch、完整 Node/npm native addons 并不会因 POSIX API 相近而自动运行。以任意现有 Linux 软件为硬门槛，就必须接受 Linux/CPU 仿真或 native/remote 容器后端，不能同时宣称极小 runtime 与无条件兼容。
2. **LayerFS 很值得借鉴，但当前不能直接承诺为不可重建记忆提供断电持久性。** root README 的 1.0.0 仍声明 Disposable profile、无 crash/power-loss durability；main 新增的 WAL/FULL 持久化层明确 under qualification、no release admission，只有 macOS fullfsync profile，其他平台拒绝。应分开评估 canonical object/branch 设计与持久化合格程度。
3. **gh.wasm 和 ssh.wasm 不是空想：已有真实、可阅读的 2026 社区实现。** ai-ecoverse/homescoop 具有 wasi-gh 和 wasix-openssh。但其专用 host imports、sysroot 补丁、SLICC kernel 依赖必须保留在兼容性说明中，不能当作普通 Wasmtime/WAMR 能直接运行的通用模块。
4. **Nix-like 是合理的第一版产品承诺。** 固定 derivation、不可变包存储、依赖闭包、profile generation、原子切换、回滚，可由 Rust 宿主实现小型部署协议；Nix/wasinix 可以放到构建端。真正把完整 Nix 包管理器和 nixpkgs 全部移植进 WASM 不是同一个工作量。
5. **Android runtime 可行，但“普通 Android 永远在线”不可由 WASM 保证。** Wasmtime 50 已有 aarch64-android 官方发行资产；后台运行仍受 Doze、进程清理和前台服务类型时限约束。常驻服务可优先 Linux，Android 给出独立 native wrapper/foreground lifecycle 规格及恢复语义。

## 已核验的版本与文件

| 组件 | 核验结果 | 来源 |
|---|---|---|
| LayerFS main | `b5814bf3a8d886ef5e32298118dd1661c352bd2a`，提交 2026-10-22T15:28:30Z | GitHub API commits/main |
| LayerFS 发行说明 | root 标注 1.0.0 source release；Disposable；benchmark 基于 `e5c7aaae05` | README.md |
| Wasmer | v7.5.0，2026-10-01T11:32:50Z | releases/latest |
| Wasmtime | v50.0.0，2026-10-20T16:22:46Z；包含 aarch64-android CLI/C API assets | releases/latest |
| WAMR | WAMR-2.4.5，2026-06-29T03:11:43Z | releases/latest |
| SLICC gh | GitHub CLI 2.102.0，上游 commit `fc4b137`，Go 1.27.1 `GOOS=wasip1`，社区补丁 | homescoop/packages/wasi-gh/package/README.md |
| SLICC OpenSSH | OpenSSH portable 10.6p1 client；包 10.6.0-13；wasix-sysroot 2025.9.30-40 | homescoop/packages/wasix-openssh/package/README.md |

版本只是本轮检查时的事实，不应硬编码为未来开发的“最新”。

## 运行时路径比较

| 路线 | 契合点 | 真实边界 | 建议 |
|---|---|---|---|
| Rust + Wasmer/WASIX | C/Rust toolchain、fork/exec/wait、socket、TTY、pthreads；最贴近用户要求的 shell userland | 非标准 WASI 扩展；端口和编译链有维护成本；需要每个 package 验证，不能随意替换 Wasmtime；精简/Android 预算未证明 | 若 POSIX shell 和现成程序优先，作为第一候选做规格可行性 spike |
| Rust + Wasmtime/WASI + typed host capabilities | Rust 嵌入、Component/WIT、明确 preopens、AOT、Pulley、多平台；Android release 已存在 | WASI 不是 Linux，POSIX shell/process/SSH/gh 需要额外 host ABI 或移植；不能直接承诺兼容 WASIX binaries | 若小内核与稳定接口优先，作为核心方向，rich userland 经 profile/backend 分层 |
| Rust + WAMR | 官方支持 ARM、MIPS、RISC-V、Android；小 VM core；AOT/interpreter | C runtime（Rust host 可 FFI），ABI 和 Wasm features 需按 profile；小 core 的 KB 数值不含服务、数据库、TLS、工具或 agent | router profile 候选；不以“同一软件包无条件通用”承诺 |
| container2wasm | 能把现有 Linux container 转成 WASM，映射 host dir，Bochs/TinyEMU/QEMU 实现 | 真 CPU/OS emulation，项目自称 experimental；不符合极低开销的默认路线 | 可选 compatibility backend；必须显示是仿真模式 |
| SLICC kernel/homescoop | 真实 gh、ssh、git、bash、Clang、Python ecosystem port 与测试 | 主部署依赖 browser Dedicated Worker + OPFS；Node entry 明确 testing-only、不作为 supported runtime；不能直接成为纯 Rust 常驻内核 | 深度参考 syscall/工具移植和测试；不要整个打包作默认运行时 |
| Strands Shell | Rust 实现、in-process Bourne-like shell、VFS/credential binding、低启动成本 | 内置命令模拟，非任意 POSIX binary execution；不是完整 gcc/ssh 环境 | 适合低成本基础文件工具层，对用户完整环境不能单独交差 |

无需 v1 同时维护所有 runtime。接口应从执行能力（exec/read/write/stat/stream/kill/snapshot/restore、网络和凭据绑定）设计，但同一个 ABI/profile 的包才可相互替换。runtime 可替换 ≠ binary 可替换。

## 硬需求可行性矩阵

| 用户要求 | 判定 | 应写入 SPEC 的精确定义/验收 |
|---|---|---|
| 持久运行环境 | 可行 | 持久身份、文件、工具配置、任务状态；进程可重建。默认不承诺任意进程内存/FD/TCP 会话热恢复 |
| Linux/POSIX 兼容 WASM | 有条件 | 指定 ABI 与测试集；说明哪部分 shell/process/socket/tty/mmap/locks/symlinks 行为支持 |
| 挂载任意路径 | 可行但受宿主权限 | 管理员可将宿主可访问路径 bind；路径级 RO/RW/COW，不绕过 Android/主机权限；agent 只能使用其被授予的挂载范围 |
| toybox 等基础工具 | 有真实移植探索 | 2026-04 上游 mailing list 有 Emscripten 方案；实际可用 applet 白名单与 conformance 集合必须验收；Coreutils ports 可作为替代候选但不能私自改用户的必选项 |
| gcc-wasm 等重工具 | 部分可行、名称待明确 | 区分“编译器生成 WASM”与“编译器本身运行在 WASM”；2026 GCC 官方补丁是新 target backend。WASM-hosted Clang/LLD 有更直接的现成参考；完整 GCC 需用户指定目标/验证 |
| 任意工具自主安装 | 有条件 | 支持兼容 ABI 的任意用户包 + build recipe，不保证任何 Linux package 直接工作；包管理 transaction、依赖 closure 和 activation rollback |
| 任意本地记忆系统 | 不可直接无条件保证 | 文件协议/stdio/HTTP/MCP adapter；纯脚本及可移植依赖尝试 WASM，不兼容 native 系统经 SSH/container；原始 session truth 独立保存 |
| gh / Gitea CLI wasm 认证 | gh 有社区先例；tea 未找到已核验发行 | gh port 有专用 HTTP/exec imports、没有 OS keyring；必须独立验收 private clone/pull/push、credential-helper 循环。tea 要单列移植验证，不能编造通用支持 |
| ssh wasm/keygen/pubkey UI | 有先例且主流程可行 | ssh-keygen Ed25519、远程 exec、SFTP/SCP、known_hosts；展示公钥不展示私钥。ProxyJump/ControlMaster 等不能默认承诺 |
| Nix/Nix-like | Nix-like 可行 | immutable store + signed lock graph + generation + atomic activation，构建端可使用 Nix；不等于完整 Nix runtime 已在 WASM 内支持 |
| 二进制友好 snapshot/fork/merge | 可行 | content-addressed chunks + Merkle roots + overlays + CAS branch heads；二进制冲突不能“语义合并”；需 GC、pins、transaction、备份、损坏恢复 |
| sandbox down 自动恢复 | 可行 | 捕获 trap/OOM/health failure，先 last-known-good generation，失败再 factory image；隔离坏 generation；恢复时不倒退 memory/event log/credential revocation |
| 任意挂载也可整体回滚 | 不可无条件保证 | Managed COW root 可回滚；direct host RW、SSH remote mutation、已发送群消息、外部 API 副作用不可由 sandbox snapshot 撤销 |
| 路由器低开销 | 可行性需目标参数 | 必须定 CPU arch、RAM、storage、OpenWrt/musl；小内核目标和 workload 峰值分开。不得把 WAMR 的 ~30KB VM core 当整套 Agent 内存 |
| Android | 可移植 runtime 已证实 | native wrapper 受宿主 permissions/lifecycle；对休眠断网/进程被杀的恢复进行验收；“永久在线”需要专用/常供电部署条件 |

## LayerFS 深度解读

其 architecture 有可拆分价值：一个 SQLite Store 放 branch/commit/objects；canonical object bytes 以 ObjectId 验证；GEAR content-defined chunking、rope extents、tree objects 让小修改复用大文件既有内容；branch fork 复用 root；提交通过 branch-head compare-and-swap 处理并发；workspace 可 Materialize 或 FUSE projection。因此不应把“裁剪 LayerFS”解释成“必须在 wasm 内实现 FUSE”。对 WASM 的 VFS 适配可直接暴露 Store-backed filesystem hostcalls。

`layerfs-content` 依赖小（blake3 + telemetry），`layerfs-storage` 引入 zstd-sys，`layerfs-persistence` 为 rusqlite/平台同步 profile；FUSE、daemon、sandbox 是其他 crates。这提示先验证 content/history/store abstractions 的可移植性，再写宿主 VFS adapter。尚无已核验上游 WASM port，不能在规格写成可直接依赖的事实。

关键区别：当前 root/release 的 Disposable 模式允许 transient workspace history 但不等于 crash-durable。新 `layerfs-persistence/README.md` 写明要求 WAL/FULL、macOS fullfsync 与 checkpoint_fullfsync、配置 readback、未获 release admission；这代表有改善工作，但尚不能将此当 Linux/Android/router 的合格默认存储。

在产品内最好分为三个独立根：

- **runtime/package roots**：系统 image、工具包、agent 代码，可 atomically activate/rollback。
- **managed workspace roots**：试验、subagent 分支、COW 文档，可 snapshot/merge；用户明确指定持久 memory git mount 默认不被 factory reset 清掉。
- **control/history/vault roots**：任务 ledger、session 原文、发送 outbox、lease epochs、管理员策略、密钥和撤销状态，guest 不可写关键控制元数据，不随 runtime rollback 倒退。

control store 与 workspace commit 需定义跨存储 crash recovery 协议：intent → durable objects → committed root → receipt/visibility，恢复时根据阶段重试或标 unknown；不能把两个 SQLite database 的独立提交误称为一个 atomic transaction。SQLite 活跃 WAL 不能通过复制主 .db 文件完成安全备份。

## SSH / Git 的产品边界

SLICC WASIX OpenSSH README 真正支持 ssh、ssh-keygen、scp、sftp、ssh-add（不带 ssh-agent），Ed25519/ECDSA/RSA、公钥/密码/keyboard-interactive、known_hosts、PTY 与 resize。它的 `scp`/`sftp` 有 posix_spawnp 补丁，运行需要 sock_pair；ProxyJump、ProxyCommand、SSH_ASKPASS、ControlMaster、-f/daemonize、sshd 明确不支持。这些应成为我们 port 的能力表和测试用例。

gh.wasm 使用上游 CLI 的社区补丁，HTTP 和 internal/run 透传专用 host module，不能直接拿到普通 WASI runtime 跑联网命令；交互 TUI、pager、OS keyring、SSH/codespaces/native extensions 不支持。其 git credential helper 从 git 再调用 gh 的路径明确不工作，故“有 gh api”不足以满足“私有记忆 repo pull/push”。必须验证 Git transport 与 credential-helper 组合。

建议由 agent 请求生成独立 identity key，使用宿主安全 RNG，初始公钥和 fingerprint 在 UI 展示；私钥默认隔离保存，不进入 git、snapshot、日志、FTS。若用户坚持 guest 生成，则使用一次性/密钥文件专用挂载及后续 import；要求复原 runtime 不换 identity、不复活被撤销 key。每个 target 绑定 known_hosts identity 与 credential scope；这也是连接正确人类机器所需的确定性，而非对正常自治反复审批。

默认初始化：建立宿主 memory 路径，作为 guest 的 `/memory` 持久挂载；初始化本地 Git 仓库；无 remote 时只在本地记录，配置认证 remote 后才 push。明确本地 Git 与 encrypted backup 是两个功能；Git 私有仓库不会自动给文件内容端到端加密。

## Nix-like 精确定义

借用 Nix 的 immutable package paths、dependency closures、generation profiles、atomic symlink/root pointer switch；包格式需包含 ABI、runtime feature requirements、source commit/hash、build recipe hash、artifact hash、dependency hashes、license、capabilities、entrypoints、health checks。Agent 能提出和执行安装、移除、构建、切换 generation；安装操作失败只丢候选 generation，不覆盖当前已运行工具。

Wasix 官方的 builds 页面现列 Nix-based wasinix，示例在 x86_64 Linux 上 `nix build` Git WASIX 包，Python registry 标 2026-09-12 434 包。但本次 github.com/wasix-org/wasinix 在 web 和 GitHub API 都 404，不能把其配方现状/可靠性假装已审阅。应允许之后查证并接入其 registry，不把项目锁定在一个当前无法读到的仓库。

AOT 缓存必须按 runtime version、target ISA、feature flags、ABI 和源 wasm digest 匹配；Wasmtime 官方说明 native precompiled blobs 不能按普通 untrusted wasm 验证安全性，必须只加载可信构建器产出的 artifacts。对路由器用 trusted builder/offline cache 很合适，但这不是“任意下载 .cwasm 即可安全运行”。

## 推荐需问用户的真正取舍

1. “POSIX 兼容”究竟要直接运行现有 Linux 软件，还是接受兼容包重编译，并让不兼容程序去 SSH/native 容器？这是最大架构分叉。
2. 首个验收设备是什么，最低内存/架构多少？256MiB OpenWrt MIPS 与 1GiB ARM64 手机不是同一个 runtime profile。
3. 安装 gcc 是需要 GCC 本体在 WASM 自举、能编译 C/C++ 即可，还是需要构建 native Linux 程序？编译器 host/target 必须写清。
4. gh/tea/ssh 必须以真实移植 CLI 在 WASM 中运行，还是允许同样命令界面调用 Rust host transport？不能默默用 broker 冒充真实 port。
5. 自动恢复保留哪些记忆？建议只回滚软件/managed workspace；历史原文、identity、权限撤销、已执行外部副作用记录绝不倒退。
6. 任意挂载需要 live direct RW 还是默认 COW？前者有即时副作用，后者才能真正 branch/rollback；可以同时支持但必须 UI 清楚。
7. Android 是常供电专用节点、Termux 环境，还是日常手机普通 APK？这决定“永远运行”的可实现承诺。
8. 默认完整自治是否包含升级自身核心/权限配置？建议核心自恢复/自动签名更新与 agent 自改插件分开，避免 self-repair 摧毁恢复引导程序。

## 关键验收场景（规格，未执行）

- 实际运行 `sh` 管线、文件 rename/symlink、取消超时、并发 read/write、shell spawn exit/signal；公开所选 POSIX test subset 和 unsupported 分类。
- 在 WASM 内读写 `/memory`，进程被杀重启后存在；升级坏包可回退；恢复 factory image 不丢最近收到的用户消息。
- 私有 GitHub/Gitea repo clone/pull/push、token 到期、SSH known_hosts mismatch、passphrase key import、网络断开后 resumable file transfer。
- 100 个 subagent workspace 共用大文件，测 fork 时间、peak RSS、物理存储增量和冲突，而非只报瞬间 clone 数字。
- 故障注入覆盖对象写入、fsync、root publication、event receipt 任意阶段；测 RPO/RTO，明确 process-kill 与 power-cut 不同测试。
- 直接挂载写操作在 UI 标为外部实时状态，snapshot restore 不声称能恢复该挂载；remote SSH 与聊天 outbox 仍保持幂等标识。
- Android kill/doze/断网/重启、路由器慢闪存/磁盘满/低内存被测；资源目标必须把宿主、sandbox worker、工具、索引、LLM backend 分开。

## 直接来源与可供父 Agent 重新 open 的 refs

- LayerFS repo/README: https://github.com/Ephemeral-AI-Lab/layerfs （web `turn5view2`）
- pinned commit: https://github.com/Ephemeral-AI-Lab/layerfs/commit/b5814bf3a8d886ef5e32298118dd1661c352bd2a
- concepts: https://github.com/Ephemeral-AI-Lab/layerfs/blob/main/docs/general/concepts.md （GitHub plugin 已读）
- persistence: https://github.com/Ephemeral-AI-Lab/layerfs/blob/main/core/crates/layerfs-persistence/README.md （GitHub plugin 已读）
- content: https://github.com/Ephemeral-AI-Lab/layerfs/blob/main/core/crates/layerfs-content/README.md （GitHub plugin 已读）
- WASIX features: https://www.wasix.org/docs/explanation/features/ （`turn7view2`）
- WASIX build collection: https://www.wasix.org/builds/ （`turn7view1`）
- Wasmer runtime: https://docs.wasmer.io/runtime/ （`turn28view1`）
- Wasmer latest release: https://github.com/wasmerio/wasmer/releases/tag/v7.5.0 （GitHub plugin 已读）
- Wasmtime latest release: https://github.com/bytecodealliance/wasmtime/releases/tag/v50.0.0 （GitHub plugin 已读）
- Wasmtime platforms: https://docs.wasmtime.dev/stability-platform-support.html （`turn3view1`）
- minimal embedding: https://docs.wasmtime.dev/examples-minimal.html （`turn28view2`）
- AOT guidance/trust: https://docs.wasmtime.dev/examples-pre-compiling-wasm.html （`turn28view3`）
- preopens: https://docs.wasmtime.dev/api/wasmtime_wasi/struct.WasiCtxBuilder.html （`turn17search21`）
- WAMR: https://github.com/wasm-micro-runtime/wasm-micro-runtime （`turn1view2`）
- container2wasm: https://github.com/container2wasm/container2wasm （`turn13view0`）
- SLICC kernel: https://github.com/ai-ecoverse/slicc-kernel （GitHub plugin 已读完整 README；web open 有 DisabledError）
- gh wasm: https://github.com/ai-ecoverse/homescoop/blob/main/packages/wasi-gh/package/README.md （GitHub plugin 已读）
- ssh wasm: https://github.com/ai-ecoverse/homescoop/blob/main/packages/wasix-openssh/package/README.md （GitHub plugin 已读）
- homescoop overview: https://github.com/ai-ecoverse/homescoop （`turn10search1`）
- Strands Shell: https://github.com/strands-agents/shell （`turn13view1`）
- Nix profiles: https://nix.dev/manual/nix/2.35/package-management/profiles （`turn31view1`）
- GCC wasm target proposal 2026-05: https://gcc.gnu.org/pipermail/gcc-patches/2026-May/717771.html （`turn35view2`）
- Toybox wasm upstream discussion 2026-04: https://lists.landley.net/pipermail/toybox/2026-April/009924.html 及 009925.html （`turn34search3`、`turn34search0`；open 失败，只有搜索摘录，勿过推断）
- Android Doze: https://developer.android.com/training/monitoring-device-state/doze-standby （`turn31view2`）
- Android service timeouts: https://developer.android.com/develop/background-work/services/fgs/timeout （`turn31view3`，页面更新 2026-10-16）

引用限制提醒：每来源默认归纳限 200 words，verbatim <=25 words；此记录主要为原创设计建议，不能把所有细节都当单个 README 的可无界转述。父 Agent 引用 web refs 前需要自行 open/search 核验；GitHub 插件直接 URL 可用于技术证据链接。
