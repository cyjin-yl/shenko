# WASM 工具：真实工件、身份与安装合同

本文是 `crates/shenko-wasm` 工具层与 `shenko tool …` 安装面的功能合同，对应 [SPEC](../INDEX.md) [§8.2](wasm-tools.md#spec-8-2)、[§8.3](wasm-home.md#spec-8-3)、[§18.8](wasm-tools.md#spec-18-8)、[§35.8](self-update.md#spec-35-8)、[§44.2](plugins.md#spec-44-2) 与 [§44.7](plugins.md#spec-44-7)。它不替代 SPEC，也不放宽其中任何要求；文中区分“已经存在的接口”和“还没有运行证据的部分”。

## 一个权威，不另建工具登记表

已安装的工具就是 [shenko-node 的安装单元](../../crates/shenko-node/src/units.rs)：`UnitKind::Code`、一个 `unit_id`（`tool.<name>`）、一个版本号、一份被封口的工件、以及 `provides: ["tool:<name>"]`。节点已启用的 `provides` 进入节点能力公告，因此安装一个工具就是让它出现在能力目录里，不需要第二个注册表，也不存在“工具表”和“单元表”互相漂移的可能。

`crates/shenko-wasm/src/tool.rs` 的 `installed_tools` / `resolve_tool` 是对该权威的只读投影；`ToolRecord` 是投影而不是新的存储。`shenko tool list` 读的就是这个投影。

## 两种摘要：来源凭据与工件身份

| 摘要 | 含义 | 谁写 | 何时比较 |
| --- | --- | --- | --- |
| sha256 | 上游构建回执记录的**来源凭据**：收到的是哪一份上游字节 | 端口配方写 `BUILD-RECEIPT.txt`；工具集清单逐工件保存 | 安装前重算文件 sha256，与清单/回执比对；不符即 `TOOL_PROVENANCE_MISMATCH` |
| blake3 | **工件身份**：这些字节本身是什么 | 安装时由流式读取算出；清单在生成时也会钉住 | 单元的 `artifact_digest`、CAS 对象名、认知归档闭包都用它 |

两者描述同一批字节，但永远不互相替代、不互相比较。回执里的 sha256 不会被写成单元身份，单元身份也不会被拿去和回执比对。`ToolArtifactRef` 同时保存两者并分别命名：`digest`（blake3 身份）与 `sha256`（来源凭据）。

## 工件放在哪里：内联与分块两条真实路径

安装单元只能携带一份被封口的工件，且节点对内联工件有 1 MiB 上限（`shenko_node::MAX_ARTIFACT_BYTES`）。因此大工件不能进单元，也不能进任何 JSON 请求。

| 存储类 | 适用 | 单元的工件是什么 | 模块字节在哪里 |
| --- | --- | --- | --- |
| `node_inline` | 长度 ≤ 1 MiB | 模块字节本身 | 节点对象库（随单元复制） |
| `content_store` | 长度 > 1 MiB | **指针文档**（几百字节的 JSON：schema、kind、模块 blake3 身份、字节数、上游 sha256、分块清单 ObjectRef） | 安装 CAS（`<data>/cas`）中的 64 KiB 分块对象 + 一份分块清单对象 |

`ToolArtifactKind`（`node_inline` / `content_store`）写进单元的 `config.tool.artifact`，所以执行时能立刻知道去哪里取字节。

分块清单的字段是 `schema`、`artifact_digest`、`bytes`、`chunks`、`objects[]`、`sha256`。字段名**不是** `digest`：恢复归档的引用收集器（`crates/shenko-store/src/cognition.rs` 的 `collect_references`）遇到带 `digest` 键的映射会把它当作不透明别名而停止下降，清单内部的 `objects[]` 就不会被跟随，归档会声称一个自己没有的闭包。叫 `artifact_digest` 时分块列表是可达的，分块才会真的随安装一起走。

安装分块工件时，安装器另外写一条 task + 一条 operation（`kind = wasi.tool.install`，`EffectStatus::None`，不重放任何东西），其绑定对象以完整 ObjectRef 引用分块清单（`media_type = application/json`），因此认知归档的闭包会顺着清单走到每一个分块。归档导出时逐对象校验长度与摘要，缺一个就是阻塞性 `missing_object` 缺口，归档宁可拒绝声称可恢复。

模块字节在整个安装过程中只常驻内存一次：ABI 由模块的真实 import 决定，运行时编译的也是同一批字节。除此之外的每一步都是固定缓冲的流式读取（`ARTIFACT_STREAM_BUFFER` / `ARTIFACT_CHUNK_BYTES` 各 64 KiB）：算身份、算来源凭据、分块封口都是流式的。

## 工具集清单

一个“工具集”是同一个上游项目移植出来的一组工件（toybox 的 91 个 applet、一个 Git 客户端、一个 Gitea CLI）。清单是产品真正消费的东西：

- `source` 记上游项目、发行版、取得方式（`source_build:wasi-sdk-34`）与上游源码归档的 sha256；
- `license` 记 SPDX 标识与随端口分发的许可证文本文件；
- `build` 记工具链、产生回执与配方路径；
- `abi` 记工件真实的兼容类别；
- 每个 `tools[]` 条目记安装名、applet 名、文件名、真实字节数、上游 sha256（来源凭据）以及可选的 blake3 身份。

`shenko tool manifest --receipt <BUILD-RECEIPT.txt> --artifacts <dir> …` 从端口回执生成清单：回执的 `sha256  路径` 行变成逐工件的来源凭据，字节数与 blake3 身份在生成清单时从工件本身量出。`shenko tool install-set --manifest <file> --artifacts <dir>` 安装整个集合。

清单里已发布的 toybox 清单是 `crates/shenko-wasm/ports/toybox-wasi/toybox-wasi.manifest.json`：它由保留的 `build/c06-port-run-final-r1/BUILD-RECEIPT.txt` 与那 91 个真实工件生成，逐 applet 钉住上游 sha256 与真实字节数，并随端口分发上游 `LICENSE`（BSD-3-Clause）。该清单**没有**钉住 blake3 身份：本阶段不允许编译或运行任何东西，无法在不重新构建的前提下算出这 91 个工件的 blake3，而回执本身也只记录了 sha256。清单里缺身份不是占位——安装时身份由流式读取算出并被绑定，`shenko tool manifest` 重新生成的清单会带上它。

## 安装与执行合同

安装（`shenko tool install` / `shenko tool install-set`）的顺序与每一步的拒绝条件：

1. 名称通过 `validate_tool_name`；清单条目通过 `validate_manifest_name`。
2. 流式算出模块的 blake3 身份、sha256 来源凭据与字节数；若调用方给了来源凭据，重算值必须与它一致。
3. 从真实字节判定 ABI：只有全部 import 都能被本运行时的 preview1 linker 以完全相同的签名提供、且导出 `() -> ()` 的 `_start` 与 `memory` 的模块才算 `wasi_preview1` 可运行。声明的 ABI 与真实判定不一致即拒绝。
4. 存储类按长度选择：内联，或分块 + 指针文档 + 恢复闭包绑定。
5. 写入单元：`unit_id = tool.<name>`，版本即请求的版本，工件是模块字节（内联）或指针文档（分块）。
6. 同名同版本：字节完全相同则已存在（集合安装记为 `already_present`，不重复安装）；字节不同则是 `TOOL_INSTALL_CONFLICT`，绝不原地覆盖。

执行（`shenko tool exec` 与 `wasi.exec`）永远重新解析本安装的单元清单、重新校验工件身份、重新判定真实 ABI，从不信任物化出来的副本。`shenko_wasm::tool_module_with_store(&node, &record, &content_store)` 按记录里的存储类取字节：内联的从节点对象库取，分块的从 CAS 分块重组（逐块校验摘要，再校验整体身份）。`tool_module` 只处理内联工件，遇到分块工件以 `TOOL_ARTIFACT_OUT_OF_LINE` 拒绝——那是调用方用错解析器的信号，不是用户可读的拒绝理由。

## 具名目标的真实状态

[§8.2](wasm-tools.md#spec-8-2) 要求的是“真实差距”，不是把名字勾成完成。代码里的 `NAMED_TARGETS` 是这份差距表，`shenko tool list` 会同时打印每个目标的真实状态。

| 目标 | 上游 | 许可证 | 真实兼容类别 | 当前状态与理由 |
| --- | --- | --- | --- | --- |
| toybox | landley/toybox 0.8.15 | BSD-3-Clause | W0（标准组件） | 已有真实工件：91 个 WASI preview1 applet，按 applet 逐个安装；未被移植的 applet 不在其列（`uptime` 需要 `sysinfo()`/utmp，preview1 下无诚实实现，已从 `miniconfig.wasi` 移除而不是伪造） |
| gcc-wasm | wasi-sdk 34（clang/lld）与 GCC hosted/target 构建 | Apache-2.0 WITH LLVM-exception（clang）、GPL-3.0-or-later（gcc） | W0 | 待验证。保留的 wasi-sdk 是**宿主侧**工具链，它产出 wasm32-wasip1 模块；它是 [§8.2](wasm-tools.md#spec-8-2) 里“输出目标”那一侧，不是“编译器自身运行在 WASM 里”那一侧，两种含义不能互相顶替 |
| git | git/git | GPL-2.0-only | W2 | 未解决。clone/pull/push 需要 socket（preview1 没有），credential helper 链路需要进程派生；可用的私有远端 Git 需要 WASIX 运行时或专用 host import |
| gh | cli/cli | MIT | W2 | 未解决。wasi-gh 存在但需要本运行时没有的专用 HTTP/exec host import；Go 的 `GOOS=wasip1` 构建不带 socket，因此无法真正调用 API |
| tea | go-gitea/gitea tea | MIT | W2 | 未解决。同一类 socket/HTTP 缺口；不因 gh 有路线就勾选完成 |
| ssh | OpenSSH WASIX 移植 / WASM SSH 客户端 | BSD-3-Clause | W1 | 未解决。真实 WASM SSH 要求 SSH 协议与认证逻辑在模块内执行，宿主只提供 socket、熵、时钟与受限签名；仅在宿主起 native ssh 再转发 stdout 不算完成 |

每个未解决目标都保留了移植路线与独立验收条件，不写成“已支持”。执行位置也如实标注：真实 WASM、WASM + host imports、native/remote 三者不混用标签。

这五个目标仍然是**未完成的产品目标**。上表里的条目只记录当前真实状态，不是交付本身：每一个都还需要一次真实移植（配方、真实工件、独立验收），这些配方与工件目前并不存在，本切片也没有用目录条目、宿主同名命令或标签去替代它们。

### 已经查清的移植前提（来自保留的源码与工具链，不是转述）

保留的 wasi-sdk 34.0（`build/toolchains/wasi-sdk34-20261030/unpacked-09f37a47-v2/opt/wasi-sdk`，`VERSION` = 34.0，llvm 23.1.0，wasi-libc `2e6fb9d8ee0c`）的 `wasm32-wasip1` sysroot 给出的是硬事实：

- `include/wasm32-wasip1/unistd.h` 写着 `/* WASI has no fork/exec */`，`fork`/`execve`/`fexecve`/`vfork` 只在 `__wasilibc_unmodified_upstream` 分支里声明；
- `include/wasm32-wasip1/__header_sys_socket.h` 在 `__wasip1__` 分支只给出 `MSG_*`/`SHUT_*` 常量与 `wasi/api.h`，**不声明** `socket()`/`connect()`/`bind()`/`listen()`/`accept()`/`send()`/`recv()`；整个 wasip1 sysroot 里没有 `socket()` 声明；
- wasip1 sysroot 没有 `termios.h`，也没有 `netdb.h`（无解析器）。

toybox 端口自己已经在同一批前提上做过判定，`wasi-compat.h` 与 `patches/0010-lib-wasi-guards.patch` 把缺失设施逐条记为诚实的失败而不是假成功：`xnotify_*`（无 inotify/kqueue）、`xattr_*`（无 `sys/xattr.h`）、`xmmap`（无内存映射 I/O）、`xwaitpid`（无 waitpid）、`xgetpwuid`/`xgetgrgid`/`xgetpwnam`/`xgetgrnam`（无 passwd/group 库）、`xpopen*`/`xrun`/`xexec`/`xchroot`/`xsetuser`（无进程与权限边界）、`xbind`/`xconnect`/`xsendto`/`xrecvwait`（无 socket）、`dev_makedev`/`fs_type_name`（无 statfs）、`exit_signal` 里的 `sigprocmask`/`sigfillset`（无 POSIX 信号掩码）、`struct addrinfo` 只有形状没有解析器。

据此，每个未解决目标的下一步是具体的，而不是“以后再说”：

| 目标 | 下一步（尚未开始） | 完成判据 |
| --- | --- | --- |
| git | 移植到一个能提供 socket 的运行时（WASIX 或本产品的专用 host import），并把 credential helper 链路改为进程内实现 | 私库 clone/pull/commit/push、认证过期、凭据清理端到端通过 |
| gh | 同样的 socket 前提 + 一个真实 HTTP host import | 对真实 API 的读写成功，凭据不落进工件的明文 |
| tea | 同 gh | 独立于 gh 的验收，不因 gh 可用而勾选 |
| ssh | socket + 熵 + 时钟 + （需要时）受限签名；终端交互需要 termios 之外的行规程方案 | 选定运行时里真实 keygen、认证、执行与文件传输；未支持的高级功能公开列出 |
| gcc-wasm | 区分两种含义：保留的 wasi-sdk 是宿主侧工具链（产出 wasm32-wasip1 模块，即“输出目标”那一侧）；要得到“编译器自身运行在 WASM 里”需要另一个工件 | 两条含义各自注明编译器运行位置与输出目标，GCC 条目保持待验证直到有真实工件 |

## 端口配方与验收

toybox 端口在 `crates/shenko-wasm/ports/toybox-wasi/`：上游归档与 wasi-sdk-34 的固定校验值（`ACQUISITION.md`）、保留生成物的补丁与 `retain-move.c`、只包含 preview1 下真能工作的 applet 的 `miniconfig.wasi`、一次性构建脚本 `build.sh`、以及经 `shenko tool exec` 做的验收清单 `SMOKE.md`。构建脚本写出 `BUILD-RECEIPT.txt`（工具链路径、上游归档与 sha256、逐 applet 的 sha256），清单由这份回执生成。

本切片**没有**重新构建这些 applet，也没有重新下载任何东西：91 个工件是保留运行的既有产物，被复用而不是重建。验收步骤仍需在受控环境里真实执行（`SMOKE.md`），本阶段不执行。

## 错误与恢复合同

| 错误 | 含义 | 恢复 |
| --- | --- | --- |
| `TOOL_PROVENANCE_MISMATCH` | 文件的 sha256 与回执/清单记录不符 | 上游字节不是记录的那一份；换用正确工件或修正来源记录，不自动接受 |
| `TOOL_ARTIFACT_DIGEST_MISMATCH` / `TOOL_ARTIFACT_SIZE_MISMATCH` | 工件字节与记录的身份/长度不符 | 工件在传输或落盘过程中变了；重新取得字节，不修补记录 |
| `TOOL_ABI_UNSUPPORTED` | 声明的 ABI 与真实判定不符，或模块不是可运行的 preview1 命令 | 换真实符合的工件；不改判定去迁就声明 |
| `TOOL_IMPORT_MISMATCH` | 记录的 import 清单与模块真实 import 不一致 | 记录描述的不是这些字节 |
| `TOOL_INSTALL_CONFLICT` | 同名同版本已存在但字节不同 | 换版本或换工件，绝不原地覆盖 |
| （无错误）同名同版本且字节相同 | 幂等：什么都不写，安装结果里 `already_present: true` | 已就绪，无需动作 |
| `TOOL_ARTIFACT_OUT_OF_LINE` | 用内联解析器解析分块工件 | 改用 `tool_module_with_store` |
| `TOOL_BINDING_UNAVAILABLE` | 安装还没有主体绑定，无法写入恢复闭包 | 工具在本地照常可用，等主体绑定存在后再绑定；不伪造绑定 |
| `TOOL_UNSUPPORTED` | 具名目标已声明但没有已安装工件 | 走该目标的移植路线，不以宿主同名命令顶替 |
| `TOOL_UNAVAILABLE` | 名字未知或只有别的版本 | 指定已安装的名字与版本 |

## 当前状态：哪些是源码，哪些还没有运行证据

- 源码已实现并接线：清单生成与校验、内联/分块两条安装路径、指针文档与分块清单格式、恢复闭包绑定、执行期的分块解析、集合安装的幂等与冲突规则、具名目标状态表、CLI 的 `tool install` / `tool inspect` / `tool manifest` / `tool install-set` / `tool exec`。
- 已有真实工件但未在本阶段运行：91 个 toybox applet 是保留运行的产物；本阶段没有重新构建、没有下载、没有执行任何工件。它们的 ABI 判定、逐个安装与 `SMOKE.md` 验收需要在受控环境里真实跑一遍。
- 保留工件的确切清单（只读核对，未重建、未下载）：`build/c06-port-run-final-r1/applets/` 91 个 WASI preview1 模块，共 52 989 131 字节，最大 `iconv` 730 826 字节、最小 `false` 553 228 字节，因此**全部**走内联路径；`build/c06-port-run-final-r1/BUILD-RECEIPT.txt` 98 行（7 行头 + 91 行工件），头含 `wasi_sdk`、`source_archive`、`source_sha256=52452b68…`、`CPUS=1`、`built=91`、`skipped_config_only=toybox_force_nommu test_glue`、`retirement_index`；同目录另有 `probe.c`/`probe.wasm`、105 个构建日志、`retired/INDEX.tsv`（457 行）与 92 个退役目录；上游归档 `build/c06-toybox-upstream-0.8.15/toybox-0.8.15.tar.gz` 的 sha256 与回执一致（已核对）；工具链 `build/toolchains/wasi-sdk34-20261030/unpacked-09f37a47-v2/opt/wasi-sdk`。
- 本阶段明确未做：编译、lint、测试、格式化、native 与模型运行。分块路径（> 1 MiB 工件）还没有真实工件可跑，其正确性目前只由源码与契约承担。首次运行分块路径应当用**真实的大工件**，不用填充伪造：CPython 的 WASI 模块是一个合适的候选（由 REPL 切片取得真实解释器工件），或未来移植的编译器模块。以 64 KiB 分块计，一个 30 MB 模块会产生约 480 个分块对象与一份分块清单。
- 身份与来源凭据是字节的纯函数：同一批字节无论被清单、回执还是安装读到，都得到同一个 blake3 身份与同一个 sha256 来源凭据。重新生成清单只会让清单自身的字段与字节不同，不会让任何 applet 的身份“变化”。
- 依赖的外部事实：`shenko_store::ContentStore::new` 与 `Store::open` 的签名由父方统一调整（`ContentStore::new` 将改为返回 `Result`）；本切片的代码不自己构造 `ContentStore`，只接收已打开的 `Store` 的 `cas`。

<a id="spec-8-2"></a>
<!-- migrated-contract: SPEC:8.2 -->
## 8.2 工具清单与真实差距

| 工具目标 | 研究状态 | 发布验收 |
| --- | --- | --- |
| sh 与基本 userland | WASIX 生态有相关构建/移植线索 | 管线、重定向、进程退出、信号/取消、目录与文本工具组合可运行 |
| toybox | 保留用户点名目标；尚未核验满足全套需求的成品 | 指定 applets、ABI 与 smoke suite，不用有限内置命令冒充完整安装 |
| gcc-wasm / 编译工具链 | Clang/lld WASM 路线有现成线索；GCC hosted/target 两种含义需区分 | 既注明编译器自身运行在哪里，也注明输出目标；GCC 条目保持待验证 |
| git + gh WASM | 社区 wasi-gh 已存在，但依赖专用 host 模块；私有 Git credential helper 链路另有缺口 | 私库 clone/pull/commit/push、认证过期与凭据清理端到端通过 |
| Gitea CLI WASM | 本轮未核验可直接采用的 tea 成品 | 保留移植/适配路线与独立验收，不因 gh 可用就勾选完成 |
| SSH WASM | 社区 OpenSSH WASIX 移植有 ssh/keygen/scp/sftp；特定高级功能不完整 | 在选定 runtime 中真实 keygen、认证、执行和文件传输，公开未支持功能 |
| 任意 memory 系统 | 不能由“POSIX 兼容”推导自动可运行 | 发布 dependency profile；能 WASM 就本地运行，否则明确 adapter/native/remote 路线 |

已查到的 [wasi-gh](https://github.com/ai-ecoverse/homescoop/blob/main/packages/wasi-gh/package/README.md) 需要专用 HTTP/exec host 适配；[OpenSSH WASIX port](https://github.com/ai-ecoverse/homescoop/blob/main/packages/wasix-openssh/package/README.md) 不应被当成完整原生 OpenSSH 的等价物。相关 [slicc-kernel](https://github.com/ai-ecoverse/slicc-kernel) 生产路线以浏览器 worker/OPFS 为主，Node headless 入口不是已支持的生产 daemon；不能为了移植几件工具就悄悄给 Rust 产品增加必选浏览器内核。

“真实 SSH WASM”的判断是 SSH 客户端协议/认证逻辑在 WASM 模块执行，宿主提供 socket、熵、时钟、文件或受限签名能力。仅在宿主启动 native ssh，然后转发 stdout，不算该要求完成。

<!-- /migrated-contract: SPEC:8.2 -->

<a id="spec-18-8"></a>
<!-- migrated-contract: SPEC:18.8 -->
## 18.8 WASM 能力自主移植

WASM 兼容差距也是 Agent 可以处理的工作。执行 worker 可分析软件的系统调用、动态链接、原生扩展、线程/进程、网络与文件依赖，选择 WASI/WASIX target、专用 host import 或协议适配，生成构建配方并在本地或自有构建节点完成移植。是否采用替代实现由目标功能决定，结果必须注明“真实 WASM”“WASM + host imports”或“native/remote”，不能用标签掩盖执行位置。

移植前后都记录可用磁盘、临时构建峰值、下载/cache、运行内存与依赖闭包；空间不足时可以增量构建、复用缓存、清理未引用候选、使用远端构建或选择更小实现。保留当前可恢复 generation；构建 worker 不得把唯一记忆当临时缓存自行清理；原始记忆的迁移或真实删除按 [§12.4](memory.md#spec-12-4) 由主体决定并委派执行，当前恢复和未决动作所需状态不得由普通构建清理误删。适配器、工具链与策略本身均可继续演进。

安装验收以用户实际需要的功能为准：目标 runtime 的 imports/ABI、包/资源锁、私库认证或 SSH 等对应端到端流程、取消与恢复、自然任务中的选择和正确调用。验证后更新 capability registry 与 recipe，恢复被阻塞任务；失败则保留诊断并尝试其他路线。主体负责目标与委派，child 负责实际源码、构建、文件和安装。这个闭环保留 [§8](wasm-home.md#spec-8) 的真实 WASM 目标，也允许 [§34](desktop-runtime.md#spec-34) 的完整 Linux 环境承担构建和暂时执行。

<!-- /migrated-contract: SPEC:18.8 -->
