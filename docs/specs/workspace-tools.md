# 工作区工具：可配置插件与默认组合

**内核定位。** 文件读取、搜索、创建及编辑属于可替换执行插件，依赖[内核 5](kernel.md#kernel-5)、[内核 7](kernel.md#kernel-7)与[内核 8](kernel.md#kernel-8)的效果、生命周期和安装合同。编辑方言及默认选择不进入内核的语义判断；执行仍由有限 child 承担，见[模型 child](model-children.md#4-工具调用协议)。

## 来源与 Why

用户在本对话 2026-11-17（Asia/Shanghai）的[明确决定](../requirements/user-sources.md#workspace-tools-20261117)是提供多种可配置工具插件，并由实验或推理选择一组默认组合。此前关于 code mode、盲改和修复成本的长文是用户提供的观察材料；其日志、统计方法和比例没有在 Shenko 中复现，不作为本产品的实测结论。

工具设计要同时降低修改的表达成本与错误发现成本。批量读取可以节省调用轮次，但执行器在一个脚本里读取后立即修改，并不证明模型在写下修改前观察过源码。精确匹配或行锚能拒绝部分错位修改，也不能证明模型理解了调用关系。因此保留不同编辑方言、明确返回修改结果，并以完整任务成效决定是否更换默认值，不把“调用更少”当作独立胜利条件。

这是参考插件的工程默认，可以随实际模型、任务和证据修订。没有“任何模型必须使用某种编辑器”或“每次必须读完整文件”的内核规则，也不增加人工审批。

## 已实现范围与配置边界

本轮提供 `workspace.read`、`workspace.search`、`workspace.create`，以及 **`workspace.edit.replace` / `workspace.edit.hashline` 两个互斥的编辑插件**。它们进入已有 native handler、能力目录与 Node 安装单元路径，不另设工具注册表或安装数据库。同一 profile 只选择其中一个编辑器，避免给 child 同时披露两套互相竞争的语法。

默认组合为读取、搜索、创建和 **replace**。配置入口：

```sh
shenko tool profile inspect
shenko tool profile configure --editor replace
shenko tool profile configure --editor hashline --root /path/to/workspace
shenko tool profile disable
shenko tool profile enable
```

首次创建 profile 且未提供 root 时，从已配置 WASM `/home` 的可写 `ReadWrite` host mount 取得真实宿主目录，不把虚拟 `/home` 当作宿主路径，也不猜测当前 shell 目录。COW mount 的 source 只是 base，不能据此推定写入目录；没有合适挂载时明确报告工具包 unavailable，主体仍可启动，使用该包前需配置实际 root。所选编辑器与 root 由本节点的 `tools.workspace` Code unit 持久保存；之后 `configure` 省略 `--root` 会保留旧值。旧／新 Node 缺少该单元时才尝试补默认 replace，已有单元的 root、editor 和 enabled 状态保留，disabled 不被默认装配重新打开。未配置的包也可以关闭，disabled 且未配置与已有有效配置分别可见。Node 自身保留的 `cognition` 观察元数据独立于严格校验的工具配置，不因登记观察而使工具配置失效。

配置及启停改变下次 runtime start 的装配选择，不能把写入配置报告为已热切换当前运行器。这是本轮 CLI 的实现边界，[共享能力与运行时替换](model-children.md#spec-5-5)的既有目标不因此缩减。

目录已注册与某个 child 已获工具是不同事实。作者仍在 `ModelChildSpec.tools` 显式选择真实 `(kind, definition_version)`；已有 `tools: []` 的纯文本 child 保持原义，不能因为 profile 默认安装工具就自动获得它们。主体本身的角色边界保持不变。

`wasi.exec`、REPL 和 SSH 继续复用各自已经存在的执行路径。这里没有新增 code mode 的 tools callback 桥接，也没有实现 patch 方言；能运行代码不能被记成代码可以回调任意工具。工作区编辑插件也不是对全部 shell／远端写入的全局拦截器。

## 编辑方言与结果

`workspace.read` 接收 `path` 及可选 `start_line` / `max_lines`，返回范围和完整文件的 BLAKE3 `digest`；replace 模式提供 `text`，hashline 模式提供含文本及行结束信息的 `anchors`，不重复输出同一段源码。`workspace.search` 默认按字面 query 搜索内容，提供实际命中的路径、行号、上下文和摘要；`mode: "path"` 搜索真实文件路径，空 query 可以列路径。结果明确标记边界及截断，跳过遍历中的 symlink 与 `.git`。搜索摘要不代表未显示的内容已经进入模型上下文。`workspace.create` 接收 `path` / `text`，只创建不存在的文件，不覆盖已有文件，父目录须已存在。

本参考实现处理最多 1 MiB 的普通 UTF-8 文件；路径相对于显式配置的 root，不接受 `..` 或 root 外的解析结果。两个编辑器均不能给已存在的空文件插入首段文字，空文件、二进制、大文件和超长单行可使用既有执行能力处理。读写采用同步文件 IO；字节上限不承诺慢磁盘或网络文件系统的墙钟期限，执行中的操作也不承诺可被中断。没有用未追踪的后台写入伪造取消完成。

模型 child 的工具回包上限为 16 KiB，本插件把 normalized 投影控制在约 12 KiB 内，并明确标记截断；完整差异保留在原始 receipt／CAS。超长单行明确报错，不依赖外层截断 JSON 或锚来勉强返回。输出范围与实际披露内容分别可见，不能把完整文件摘要误当完整文件展示。

两个编辑入口均接收 `{path, expected_digest, edits}`，要求完整文件摘要与当前内容一致，并在提交前重查；不是只凭模型给出的路径就覆盖。这个检查没有追踪 read exposure，也不能证明模型已读。宿主使用 runtime 内共享 mutex、同目录临时文件及 rename；它不是跨进程锁，任意 shell 或外部程序不参与该锁，因此不能把它写成覆盖全部写入者的无竞态 CAS。

两种编辑器都以明确目标和实际源码为输入，不作模糊修补。一次请求内的编辑针对同一原文定位，先完成整批范围与匹配检查，再交给宿主写入；重叠修改、无变化和定位失败均明确返回，不能把没有改动伪装成成功。

- **replace** 使用 `old_text` / `new_text`。`old_text` 必须非空并在原文中精确出现一次；零匹配、多匹配及重叠出现都拒绝，不自动裁剪空白或猜测近似文本。
- **hashline** 使用 `start` / `end` 行锚和 `new_text`。`N:HASH` 中的 `N` 为从 1 开始的物理行号，`HASH` 为该行含换行字节的 BLAKE3 前 16 个十六进制字符。范围包含末行及其换行，替换文本按字面写入。读取结果同时提供 `text` 与 `ending`，避免把换行存在与否藏在显示格式里。这是 Shenko 的 JSON 行锚协议，不是 OMP 的 `PUT` / `CUT` 寄存器语言。

成功结果返回实际 `changes`，其中 `start_byte` / `end_byte` 是原文中的半开字节范围，`old_text` / `new_text` 记录实际差异。整批检查成功不等于外部效果已经完成；输入在 T1 前冻结，实际效果后记录回执，未决修改先协调而不自动重放。

模型看到的说明必须与当前安装方言一致。当前选择由 profile 显式配置，不根据模型名静默换格式，也不声称已经做过自动方言适配。后续比较可以改变默认或增加适配，但不能靠关闭真实错误报告来制造兼容。

执行器的匹配结果只证明其检查过的结构条件。保存过源码摘要、读过文件或匹配了原文，均不能单独证明相关内容仍在模型当前上下文，更不等于任务修复正确。实际修改、失败和未决效果沿用同一 native task / operation / evidence 路径；恢复时按原调用协调，不能因换 profile 自动重放旧修改。

## 默认选择与证据状态

**replace 是基于推理的暂定默认（reasoned provisional），不是实测胜者。** 它让模型用局部原文及替换结果表达修改，协议较小；精确唯一匹配使零匹配和歧义显式失败。hashline 可减少对长原文的重复输出，并直接标识行范围，但增加了锚和物理换行的表达要求。这些是选择的理由及待比较的代价，不是跨模型的性能排名。

同行的一手资料支持保留不同方言，而不是直接证明某种方言对 Shenko 最优：

- [OMP edit 文档](https://github.com/can1357/oh-my-pi/blob/main/docs/tools/edit.md)（核对日期 2026-11-17）提供多种模式及模型相关的 replace fallback；`enforceSeenLines` 默认开启，但没有保留读取快照时有明确退化规则。不能把它概括成所有情形下绝对强制先读，也不能把其 fallback 或寄存器语法算作本产品已实现。
- [Aider edit formats](https://aider.chat/docs/more/edit-formats.html)（核对日期 2026-11-17）说明不同模型适合不同格式，以及整文件输出与局部编辑的成本区别。这支持将语法保持为可配置选择，不提供本产品的胜率数据。

当前尚未获得本轮工作区工具的真实模型行为比较；安装、编译或确定性检查通过也不改变这一状态。有限的比较设计见[默认组合的证据设计](../development/workspace-tool-defaults.md)。
