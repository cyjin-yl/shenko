# WASM 家园、挂载与恢复合同

**内核定位。** 参考插件，建立在[内核合同](kernel.md)之上。家园里的执行是带回执的效果（[内核 5](kernel.md#kernel-5)），经[内核 7](kernel.md#kernel-7)登记，工件以单元的形式安装（[内核 8](kernel.md#kernel-8)）。


<a id="why-4"></a>
<!-- migrated-contract: WHY:4 -->
## 为什么坚持 WASM 家园，又接入真实操作系统

可移植、持续存在的 WASM 家园让主体拥有统一的工具与文件视图。但 WASI 不自动提供 Linux ELF、原生扩展、完整进程或任意 npm 包。兼容性必须具体到目标、ABI、依赖和执行路径。具名 WASM 目标应真实移植；宿主代理可以帮助开发，不能在最终验收中冒充。见 [§8](wasm-home.md#spec-8)、[§11](remote-seats.md#spec-11)、[§19](repl.md#spec-19)。

完整 Linux VM、BoxLite、SSH、Windows/macOS/Linux computer use 是可安装的执行环境。LayerFS 原版放在适合它的真实 Linux 环境里；WASM 状态层按自己的持久合同实现。这样保留各环境真实能力，避免为了一个抽象接口虚构不存在的共同保证。见 [§10](durable-state.md#spec-10)、[§22](nodes.md#spec-22)、[§24](desktop-runtime.md#spec-24)、[§34](desktop-runtime.md#spec-34)。

<!-- /migrated-contract: WHY:4 -->

本文对应 [SPEC](../INDEX.md) [§8](wasm-home.md#spec-8)–[§10](durable-state.md#spec-10)、[§41.3](durable-state.md#spec-41-3)、[§44](plugins.md#spec-44) 与 C05。家园是主体实际使用、可持久保存与恢复的运行环境，不是把宿主目录或宿主命令重新标成 WASM。本文规定功能与接口边界，不宣称某个候选已经通过运行验收。

## 组成与权威

- [`shenko-seed`](../../crates/shenko-seed/src/bootstrap.rs) 定义 `Bootstrap`、`HomeManifest`、`HomeState`、`HomeCheckpoint` 与首启输入；`validate_home_for_exec` 只读检查一个现存安装的家园绑定。
- [`shenko-wasm::mount`](../../crates/shenko-wasm/src/mount.rs) 定义 `MountSpec`、`MountMode` 与 `OwnedDir`。挂载声明、已经取得的目录句柄、客体可执行的操作是不同层次。
- [`shenko-wasm::cow`](../../crates/shenko-wasm/src/cow.rs) 定义 `BranchStore`、`CowMountStore`、`CommitRecord`、`CowDiff` 与删除标记。分支身份不能由一个可变的宿主路径替代。
- [`shenko-wasm::runtime`](../../crates/shenko-wasm/src/runtime.rs) 承担真实 WASM 实例、导入接口和调用生命周期。工具工件与安装单元的关系见 [WASM 工具合同](wasm-tools.md)，交互会话状态见 [REPL 合同](repl.md)。
- [`LaunchService`](../../crates/shenko-seed/src/launch.rs) 把进程启动绑定到已提交的 activation selector；选定代际、slot、home 与 memory 必须一致。完整交接合同见 [自升级](self-update.md)。

`/home` 是运行时声明的持久家园，`/memory` 是记忆边界；二者不能因重新打开数据库、改变 writer ledger 身份或复制目录而指向另一安装。安装、主体、分支、软件代际、写入者与一次调用各有自己的身份，不能互相代替。

## 挂载声明与实际能力

`MountSpec` 明确记录宿主 `source`、客体 `guest_path`、模式、持久性、版本化属性，以及 COW 的 `workspace`、`branch` 和独立写权限 `writable`。

| 模式 | 客体读取 | 客体写入 | 不得发生的替代 |
| --- | --- | --- | --- |
| `ReadOnly` | 已准入的源目录 | 不提供写能力 | 先写入再回滚，或只靠应用层约定不写 |
| `ReadWrite` | 已准入的源目录 | 直接修改声明的源目录 | 把直接修改声称为版本化分支提交 |
| `CopyOnWrite` | 只读基底与本分支覆盖层的合成视图 | 只修改覆盖层与删除标记 | 把 COW 重标为 RW，或把工作副本当作不可变基底 |

撤回 `writable` 只撤回写能力，不改变模式和可见分支。只读 COW 仍须看见该分支已有的覆盖内容，不能突然退回基底。

`/home` 必须是唯一、与所选安装绑定的 preopen。客体路径必须规范、绝对且无重复。不同挂载的内部存储身份也必须不同：仅拒绝重复 `guest_path` 不足以防止 `/a/b` 与 `/a_b` 等不同路径在一个有损编码中相撞。

内部挂载键使用 `mount-` 加规范 `guest_path` 原始 UTF-8 字节的 BLAKE3 摘要；覆盖目录与删除标记使用同一键。不折叠路径分隔符、反斜杠或下划线。旧有损键若曾被用于存储，迁移必须另有原挂载绑定，不能从一个已经相撞的目录名猜测归属。

## 目录句柄边界

挂载准入与执行使用同一份实际打开的目录能力。客体路径解析以该句柄为根；校验过的字符串不是目录能力。

解析到子目录后的最终文件名相对于该父目录句柄打开，不重新相对于挂载根打开。COW 的基底读取、复制到覆盖层和最终打开也遵守这条规则。创建目录时只先准备其父目录链，目标目录由实际创建操作产生；不能先把目标也创建出来，再误报它已存在。

必须拒绝或隔离以下情况，而不是先读取外部字节再发现错误：

- 校验与 preopen 之间，源路径或祖先被替换；
- 客体相对路径越过已准入的目录根；
- 符号链接或重命名把一次读写引向未声明的目录；
- 一个安装的 home/memory 声明别名到另一个安装的可写状态。

`canonicalize`、操作后的路径比对、文件名白名单都不能单独证明这个边界。平台没有可用的等价目录能力时，应如实报告该执行方式不可用，不退回失去约束的路径打开。

## COW 分支与提交

受管分支位于家园的 workspace store，区分 workspace、branch、mount 和 commit。有效视图是覆盖层优先的“覆盖层与基底的并集，扣除删除标记”。

- 首次修改基底文件时复制其实际内容到本分支；不修改基底。
- 删除基底项须留下本分支的持久删除事实；仅从覆盖目录移除文件不能让基底内容重新出现。
- 路径重命名、目录删除与覆盖需要保持整个有效视图的语义，而不只是更新一个路径字符串。
- `CommitRecord` 绑定有效视图及其内容身份；分支 head 是该记录的引用，不是成功日志的替代品。
- commit、diff、discard 与客体执行要遵守相同的分支所有权和生命周期。不能在仍有写入者时把一个不稳定视图宣布为已提交快照。
- 原子发布与持久化是不同义务：崩溃后必须能辨别完整提交、尚未发布的准备结果与不完整对象闭包，不能把半份清单当成可恢复提交。

不同分支、挂载、安装及软件代际不得因为共享路径编码或默认名称而共用覆盖层。COW 并不自动提供 REPL 内存、解释器词法环境或外部进程的回滚。

`diff_abs` 中，没有覆盖项且没有删除标记的基底项保持不变。覆盖文件或符号链接替换基底目录时，原目录的后代从有效视图移除；覆盖目录替换基底文件时，其新后代计为新增。二进制内容按固定大小缓冲区比较，读取失败须返回错误，不能把两次失败当成内容相等。

分支重新准入仅允许把未找到的删除标记文件解释为空集合；其他打开、读取和解析错误都拒绝准入。生命周期扫描要求基底和覆盖目录实际存在，丢失任一所需根或遇到读取错误时，commit 必须在追加历史和发布 head 前失败并保留原状态。这些局部检查不代替不可变检查点闭包、分支写入者互斥、运行中撤回和根感知 GC 的完整验收。

## 工件、调用与资源生命周期

执行依据真实模块字节、实际 imports/exports 和运行时提供的 ABI。清单声明不能使一个不支持的 import 成为已实现能力；宿主同名程序、native 子进程或远程工具不能冒充模块内执行。

每次调用绑定模块身份、ABI、参数、环境、挂载、安装及分支。内存、fuel、输出与取消的约束属于真实调用，不是只写在清单中的数字。错误、trap、deadline 和取消都必须留下可区分的结果。

调用结束的证据必须覆盖该调用实际拥有的 worker、子进程、文件描述符和写入者。仍有未确认终止的工作时，不能提前释放相应占用或宣布取消完成；共享引擎线程池也不能仅因线程名相似就归咎为某次调用泄漏。

确定性输入录制把事件顺序、请求摘要、结果字节与前序链绑定，并在字节返回 guest 前持久化。供给输入期间可以查询不可变的录制/重放模式，但不能再次取得已持有的事件顺序锁。重放只读原事件，不调用新输入供给者；stdout/stderr 的已核验写入可以生成本次调用独占的新输出工件，这不等于重放原文件系统写入或任意外部效果。

## 首启、检查点与恢复

首启使用被封口的 seed、profile、home manifest 与真实 guest 探针，绑定主体的 genesis 和初始运行代际。标记文件、目录存在或宿主可写，都不等于 guest 能在声明的 preopen 中读写。

seed 的 `copy_on_write_mounts` 能力声明对应已有的受管 COW 实现，状态为 `Available`。
旧家园若完整能力表仅保留此前的 COW `Unsupported` 声明，`prepare` 在 guest 探针与宿主回读
成功后持久更新该声明，保留安装身份、挂载、创建时间与已有文件。其他能力表差异仍须显式迁移，
拒绝时保留原 manifest 和数据；能力声明本身不代替具体分支的运行验证。

`HomeCheckpoint::FilesAndMetadata` 表示今天已经做到的文件与元数据层恢复，不承诺 WASM 线性内存、执行栈、解释器对象图或进程恢复。更高恢复等级必须由对应运行时及 [REPL 合同](repl.md) 的实际机制和证据支持。现行目标见 [51.4 可学习](cognition-units.md#spec-51-4)：这三项纳入自有版本化根；本检查点尚未保存它们，是实现缺口，不是把它们留在回滚目标之外。回滚是在链尾追加一个内容相同的新节点，日志只增不减，不把 `head` 倒回去并删掉 `commits.jsonl`。外部效果仍不撤回。`agent_id` 保持切换前的值。

恢复要保持以下区分：

1. **已有可用家园**：验证安装、挂载、检查点、工件与代际绑定，不能悄悄重建成空环境。
2. **候选损坏或闭包缺失**：保留原因与原件，依据实际存在的 LKG/factory 闭包恢复；不得把“尝试过”记为已恢复。
3. **代际切换**：只使用被选中的 slot 及其受管根；旧代际释放所有权后，新代际才能取得独占运行权。
4. **外部效果**：恢复文件或解释器状态不撤销已发送的消息、网络请求或其他外部操作。未知结果仍是需要核对的事实，不能自动重放。

没有模型或 provider 不得使已有离线恢复能力失效；恢复也不能捏造模型判断、安装成功或缺失工件。

## 错误与可观察性

`DATA_PATH_CONFLICT` 表示路径或安装绑定冲突，`WASM_PROFILE_UNAVAILABLE` 表示当前挂载/运行配置不能提供所声明的执行环境。工件身份、ABI、安装与执行错误见 [WASM 工具合同](wasm-tools.md)。错误必须保留原始原因、对应调用及可读事实，不以空输出、空目录或默认成功掩盖。

能力目录应区分声明、已安装工件、可运行 ABI、当前可达资源与实际验证记录。缺少一个具名工具、一个 import 或一种恢复能力，不允许由目录条目或源码存在来抵销。

## 验收边界

以 [验证与证据合同](verification.md) 的候选绑定为准，至少覆盖真实 guest 持久写入与重启读取、只读拒绝、COW 基底不变与分支隔离、不同挂载不别名、目录祖先交换、取消后 owned worker 退出、损坏/缺失闭包下的恢复，以及代际切换后的实际 home/memory 绑定。

单元测试可以约束局部规则；编译成功、manifest 能解析、探针模块存在、宿主目录检查或旧候选的成功记录，都不能代替这些真实场景。本文保留完整功能要求；缺失实现或证据仍是未达验收，不因文档已经登记而完成。

<a id="spec-8"></a>
<!-- migrated-contract: SPEC:8 -->
# 8 WASM 家园与工具兼容

<!-- /migrated-contract: SPEC:8 -->

<a id="spec-8-1"></a>
<!-- migrated-contract: SPEC:8.1 -->
## 8.1 Runtime 选择与兼容等级

截至研究日，候选包括 Wasmtime 50.0.0、Wasmer 7.5.0、WAMR 2.4.5。Wasmtime 已提供 aarch64-android 产物；Wasmer/WASIX 更值得验证 shell/process 工具环境；WAMR 值得评估受限设备 profile。不能把某 runtime 的最小 VM 数字当作完整 Bot 的内存占用，也不预先假设同一份包可跨不同 WASI/WASIX import 集运行。[Wasmtime release](https://github.com/bytecodealliance/wasmtime/releases/tag/v50.0.0)、[Wasmer release](https://github.com/wasmerio/wasmer/releases/tag/v7.5.0)、[WAMR](https://github.com/wasm-micro-runtime/wasm-micro-runtime)、[WASIX 功能](https://www.wasix.org/docs/explanation/features/)

BoxLite 作为完整 Linux microVM 的可选 RuntimeProvider 候选列入 [§34](desktop-runtime.md#spec-34)。它可在具备 KVM/HVF 的本机或自有节点承载 native 工具、REPL、构建与桌面；其本地/远端挂载、磁盘快照和资源要求分别协商，不替代本节 WASM 兼容目标。

建议以 RuntimeProvider 隔离引擎选择：正式工具执行 profile 必须经过组合验收再选主引擎；标准 component 插件和 POSIX userland 可有不同 adapter，但共享资源必须通过正式 VFS/服务协议，不假装不同 runtime 自动共享进程命名空间。最小硬件 profile 可裁剪功能，UI 与包解析器必须说明不兼容项。

| 等级 | 含义 | 能否算作真实 WASM 工具 |
| --- | --- | --- |
| W0 标准组件 | 使用明确版本的 WASI/WIT 导入 | 可以，按声明接口运行 |
| W1 扩展 POSIX userland | 使用固定 WASIX/兼容 sysroot、进程/网络等扩展 | 可以，需标注 runtime profile |
| W2 专用 host imports | 主要工具逻辑在 WASM，宿主提供所需系统能力 | 可以，但必须公开 import 与宿主依赖 |
| H 宿主适配 | 通过 API 调用 native SSH、Git 或其他程序 | 仅为可选适配，不称为对应 CLI 的 WASM 移植 |
| R 远端环境 | 通过 SSH/节点/容器运行不兼容的软件 | 完整能力的补足路径，不称为本地 WASM 兼容 |

<!-- /migrated-contract: SPEC:8.1 -->

<a id="spec-8-3"></a>
<!-- migrated-contract: SPEC:8.3 -->
## 8.3 执行和挂载合同

每个 worker 可拥有独立 cwd、环境变量、stdin/stdout/stderr、argv、退出状态、资源限制与取消句柄。结构化 argv 执行和显式 shell 字符串分开，不能依赖 JSON 字符串转义作为 shell escaping。长输出保留完整对象并返回有限预览及读取 URI。

支持任意宿主可访问路径映射到任意合理 guest 路径，不限制为固定 workspace。每项 mount 明确：来源路径、guest 路径、读写模式、是否持久、是否受版本管理、可见 worker 和撤销行为。路径解析抵御 symlink/rename 越界；不会绕过宿主 OS 权限。特殊文件、设备、proc/socket 等应单独声明兼容能力，不承诺 Linux 特殊文件语义全部可用。

挂载模式必须同时存在：只读；直接读写；受管理 COW。直接读写用于真实主机文件协作，COW 用于分支试验。解除挂载后的已有 handle 行为有明确规则；撤销后新操作被拒，进行中的 IO 尽力终止并记录结果。Agent 能否自主增加新 host mount 由已授能力决定，不能通过初始任意挂载支持自动获得整个主机。

<!-- /migrated-contract: SPEC:8.3 -->

<a id="spec-35-2"></a>
<!-- migrated-contract: SPEC:35.2 -->
## 35.2 开发者机器与 WASM 家园的关系

首版采用真正运行、可恢复的 WASM 家园和默认宿主挂载 Git 记忆；工作区、原始 session、任务与事实账本按 [§10](durable-state.md#spec-10) 分域。开发机已有的 Claude Code、OMP、tmux、Rust 工具链与其他编译器可以经 HostExecution／BuildProvider／AgentSeat 插件使用，也可以连接已授权 SSH 节点。无需把全部宿主 CLI 先移植到 WASM 才允许自开发。

“使用宿主能力”必须展示实际执行位置与 [§36](durable-state.md#spec-36) 恢复边界，不能把 host wrapper 标成 WASM 原生实现。真实 WASM SSH、认证 Git、包管理与 REPL 目标继续推进；是否已移植、可运行或已实测分别记录。路由器可委派其他节点构建；开发者发行不要求每台最小设备都安装 LLVM。

首个完整闭环建议在具备 tmux 与构建工具的 64 位 Linux 开发环境验证。macOS、Windows／WSL、Android 与路由器使用各自 profile，不因同一接口存在而宣称同时通过。QQ 账户登录和机器权限按实际已有配置接入；本规格不授权本轮连接、安装或修改开发者机器。

<!-- /migrated-contract: SPEC:35.2 -->
