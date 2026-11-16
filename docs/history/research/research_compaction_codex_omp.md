# Codex budget compact 与 OMP snapcompact：源码核验增量

研究日期：2026-10-22。仅用于完善产品SPEC §12.3；未修改主稿、未实现或运行性能测试。

**结论：两种机制都值得保留，但职责不同。** Codex budget compact是无摘要的新上下文窗口切换；OMP snapcompact把历史文字渲染为模型能读的PNG帧。后者不是进程、文件系统或KV快照，而且其档案经过截断/规范化，不能替代原始历史。两者都不要求把持续主意识改成低频Tick。

## 1. 固定版本证据

| 项目 | 本次读取版本 | 已核验位置 |
|---|---|---|
| OpenAI Codex | `a3d93e1e692f1144cf8685175a5d1f6bf7294f57` | `compact_token_budget.rs`、`session/mod.rs`、`context/token_budget_context.rs`、`ext/history-notes` |
| can1357/oh-my-pi | `0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0` | `packages/snapcompact`、`session-maintenance.ts`、历史协议、原生renderer、相关测试 |
| snapcompact包版本 | 上述提交的 `package.json` 为 **18.12.6** | 表示源码包版本，不声称已经核验npm发布状态 |

指定提交已包含所需机制，因此未用后续主线覆盖用户固定版本。[C1][O1][O2]

## 2. Codex预算切窗：明确跳过摘要

`compact_token_budget.rs` 的manual和auto入口都运行正常pre/post compaction hooks，并发出ContextCompaction开始/完成事件；中间调用 `start_new_context_window`，**跳过model/server summarization**。[C1]

新窗口不是没有任何状态的裸空prompt：`session/mod.rs`重建initial context与world state；在相应feature开启时保留有预算上限的client tool outputs/developer messages；更新window number及first/previous/current window IDs，并重新计算token usage。替换模型可见历史后，向rollout追加Compacted、WorldState、TurnContext等记录。这说明切换上下文表示与删除原事件是两件事；不能据此宣称所有任意未完成工具都已被自动恢复。[C2][C3]

`history-notes`公开源码提供按agent/window/item列举、读取、搜索历史，以及持久notes读写；read_item支持有界字符区间，search_contents是大小写敏感的literal substring。接口说明历史为eventually consistent，不能假定刚生成的条目立刻可检索。该extension受token-budget配置、OpenAI provider及Codex backend认证条件控制，不能复制几个工具名就当成本地离线实现。[C4][C5]

**可借鉴**：稳定窗口身份、来源item IDs、按需有界恢复、独立笔记与正常生命周期。**需要本产品实现**：SQLite/FTS5等本地真相源、可靠提交与读取一致性、未完成工作ledger、适合27B模型的显式恢复提示及验证。

## 3. OMP snapcompact：本地文字转图像的有界上下文表示

### 3.1 实际流程与损失边界

指定版本的README和实现一致：先序列化待移出的对话，再本地确定性渲染为密集文字PNG，下一次由具视觉能力的模型直接读取。生成档案本身不调用LLM；字体栅格化和PNG编码由Rust原生代码执行。[O1][O3][O4]

具体实现并非无损存档：

- 工具输出默认head/tail截断至2,000字符；单参数值500字符、单tool call 2,000字符等预算可调。
- 归一化会处理ANSI、空白、换行、兼容字符与emoji；CJK有嵌入字体fallback与自动形状选择，但仍需检查可渲染字符。
- `Archive.text`是**有界、已归一化、保留下来的source**；并非原始完整对话。记录 `truncatedChars`。文字头尾环绕图像中段；档案较大时中段按HQ/LQ/HQ布局。
- 再次compact时把旧档案的有界source与新历史合并重渲染；不是无限叠加旧PNG，也不能从旧PNG重新找回已丢弃原文。
- `summary`是短恢复引导与文件列表，不是模型生成的语义摘要。[O3]

其 `CompactionEntry.preserveData.snapcompact`保存档案数据；上下文重建时再挂载。源码与测试支持frame blob引用及惰性读取，缺失blob不能直接当有效图像发给模型。这是可参考的CAS派生资源设计，不是WASM linear-memory或GPU KV snapshot。[O3][O5]

### 3.2 它实际处理多种预算，不只是字符压缩率

维护层同时考虑system/tools、近期保留内容、pending prompt、图像token、frame数量、图像payload字节与真实context window；视觉不可用时自动跳过该方法，手动请求则报告不可用。渲染后仍会检查实际减小量与窗口适配，不满足就尝试后续方法或报出无法缩小。[O6]

该版本有“档案占剩余trigger room的一半”、压至threshold约60%的规划值及80%恢复区间等实现启发式。**这里的0.5是单次上下文图像档案预算，完全不是用户要求的主/子Agent KV 50/50调度算法。** 不应把这些常量照搬为本产品的固定策略。[O6]

还需按模型及实际host选择frame形状、图像数量/字节预算和缩放路径；OpenAI-compatible wire类型不证明后面的本地模型有同样视觉token规则。PNG生成不用推理槽，但下次读取图像仍有vision encode、expanded tokens、prefill、传输及模型阅读错误的成本。源码包采用Bun/TypeScript与N-API Rust renderer，不能直接声称已有WASI版。[O2][O3][O4]

### 3.3 同版本还有更贴近budget切窗的独立模式

OMP另有默认关闭的实验性 **notes-backed context windows**：

- `context_notes`写分支journal里的笔记revision，最新可见revision注入当前context；每次写入有16,384 UTF-8 bytes限制。
- `new_context`在安全tool-loop边界切窗，无需摘要；保留完整近期tool-call/result单元和笔记。
- `read/grep`通过 `history://current/full`恢复当前分支的原始journal条目，携带稳定entry IDs、parent IDs与窗口边界。
- 只有有效工具面同时有context_notes/new_context/read/grep才启用；否则维持旧维护路径。
- 旧的显式破坏性操作无法被这个模式复原，文本history中的原图仍需附件真相源。[O7][O8][O9]

这与snapcompact是不同ContextPolicy。该实验模式的目的和本产品的原文回忆更接近，可直接研究其“切窗前提醒笔记、恢复工具可用性检查、分支绑定”设计。

## 4. 未完成工具、持续思考与KV代价

| 维度 | budget-window | summary + recent | snapcompact |
|---|---|---|---|
| 切换工作 | 新窗口与恢复引用，无必需摘要调用 | 生成/更新语义摘要 | 本地序列化、布局、PNG生成 |
| 历史进入新context方式 | 笔记/必要状态 + 按需读原文 | 摘要 + 近期原事件 | 引导 + 文本边缘 + 视觉帧 + 近期原事件 |
| 主要损失风险 | 模型忘记主动回忆、笔记不足 | 摘要遗漏/歪曲、反复摘要失真 | 截断/规范化、图像缩放、视觉误读 |
| 真相源要求 | 独立持久事件/附件 | 同左 | 同左；Archive.text也不能替代 |
| KV影响 | 新投影只复用实际相同的前缀 | 摘要变化处以后通常需重建 | 帧或排列变化可改变媒体身份及其后缀；不能按旧文字KV继续 |
| 不具备的保证 | 任意外部任务自动恢复 | 撤销已发生动作 | GPU/进程快照、无损原文、零prefill |

表中KV影响是根据实际输入身份与前缀缓存语义的设计推论；不是在目标GPU上的实测结果。保留共同稳定prefix可以帮助缓存，但保留旧句柄本身不证明新表示还能命中。

OMP对运行中手动compact先abort，持久保存实际得到的工具结果，再在符合条件时续接被打断turn；源码测试明确覆盖“中断工具结果仍在旧session”以及“不伪造未发生的rewind”。这能参考其事件顺序，**不能推导出任意SSH命令/外部操作被回滚或恢复到原CPU指令**。[O6][O10]

本产品应单独持久保存call ID、输入引用、执行/取消/结果未知状态、远端operation ID及已确认结果。切窗保留完整调用单元或其可恢复引用；不伪造成功结果，不把未完成JSON参数当可执行工具调用，恢复时不盲目重复副作用。

上下文切换应在安全边界执行：准备新投影→核对源window/branch leaf/model generation→提交切换事件与恢复包→续接原逻辑序列。期间新消息保留于事件队列，不能被旧快照覆盖。OMP的源session、branch leaf、model复核值得借鉴。[O6]

主意识仍为单序列、持续调度后继。上下文政策是可替换插件，维护写入与档案加工由插件/child完成；主loop不因此获得直接文件写权限。摘要类LLM工作与所有children共用broker，C=1时不得持主推理许可等待摘要child。确定性渲染不占LLM许可，但CPU/RAM/IO也需纳入设备负担。

## 5. 四流128K基线与可采用方案

用户已确认基线为**四流、每流标称128K**。133属于配置冗余，不把133120改写成标称上下文。具体token单位与计数由部署manifest统一；每次请求仍需把system、tools、有效输入、媒体展开及预留输出合并验算。并发数、逻辑窗口和物理KV池仍分别记录。

尤其需要核实snapcompact的视觉路径：用户给定的NInfer固定提交 `docs/serving.md`写明Vision默认关闭；启用后当前Vision runtime有32,768 merged-token envelope，有效Vision请求上限为 `min(--max-context,32768)`。因此同一部署的文本128K能力不能自动沿用到带snapcompact图像的请求。后续版本若改善，应按实际build重新协商，而非把当前限制永远写死。[N1]

建议SPEC保留三种可选择策略：

1. **原文可寻址的budget-window**：独立原文journal + 有预算恢复包 + 近期完整调用单元；不依赖Codex云history服务。
2. **摘要与近期历史**：作为语义压缩选项；模型选择、预算和fallback通过插件调度。
3. **snapcompact视觉档案**：可选实验插件，仅在实际模型/后端视觉能力、中文阅读质量、窗口与成本通过测量后使用。字体/帧形状及成本模型可更新，不固定成云模型的参数。

没有足够证据选择snapcompact为四流27B部署的默认。应通过目标模型回归决定默认；这是能力与效果核验，不需要重新向用户确认功能是否应该纳入。

## 6. 最小验收

- **回忆完整性**：切窗多次后，按稳定ID找回原始承诺、工具失败、来源与附件；所有派生摘要/帧都能定位原记录，删改/撤回状态一致。
- **未完成操作**：在工具执行/取消中切窗并重启；实际结果不丢失，不制造成功，不重复已提交外部动作。
- **27B中英文阅读**：比较三策略的任务完成、原文定位、数字/路径/代码精确性、长尾知识回忆；snapcompact额外测CJK、dense text、缩放后OCR、长输出中段被截断。
- **预算与无收益回退**：计入pending prompt、vision tokens、输出reserve及HTTP payload；无缩减或超限时不无限compact；选择下一可用策略，仍保持原逻辑任务可恢复。
- **KV真实成本**：记录render耗时、网络字节、vision encode、cached/computed prefill、Replay及后续完成时间；不把“零摘要LLM调用”写成“零算力”或“零prefill”。
- **四流128K与C=1退化**：四流资源验收按真实backend进行；另测C=1摘要child无死锁。带图请求使用其真实较低上限，不能借133冗余掩盖超限。
- **模型/传输切换**：视觉能力、frame预算或字体发生变化时重新验证；文本模型不能收不可读的帧，missing blobs不能静默当完整历史。

## 固定源码链接

[C1]: https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/core/src/compact_token_budget.rs
[C2]: https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/core/src/session/mod.rs
[C3]: https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/core/src/context/token_budget_context.rs
[C4]: https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/ext/history-notes/src/tools.rs
[C5]: https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/ext/history-notes/src/extension.rs
[O1]: https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/snapcompact/README.md
[O2]: https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/snapcompact/package.json
[O3]: https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/snapcompact/src/snapcompact.ts
[O4]: https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/crates/pi-natives/src/snapcompact.rs
[O5]: https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/test/session-manager/snapcompact-frame-lazy-resolution.test.ts
[O6]: https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/session/session-maintenance.ts
[O7]: https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/docs/compaction.md
[O8]: https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/internal-urls/history-protocol.ts
[O9]: https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/session/context-notes.ts
[O10]: https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/test/agent-session-compact-running-tool.test.ts
[N1]: https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/serving.md

文中源码测试仅作为维护者声明的边界证据，本次未自行运行；不将测试标题等同于本产品已经实现。

