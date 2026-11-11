# 验证、证据与完整验收合同

本文对应 [SPEC](../INDEX.md) [§23](verification.md#spec-23)、[§27](verification.md#spec-27)–[§29](verification.md#spec-29)、[§40](../governance/development-contract.md#spec-40)、[§46](../governance/COMPLETION.md#spec-46) 及全部验收表，约束证据怎样回答“哪个候选，在什么条件下，实际发生了什么”。它不替代功能实现，也不以工作流、审批或某个模型名称限制 Shenko 的自主能力。

## 验收分母来自规格

当前分域规格包含 165 项需求、63 组行为、59 条来源和 236 个具名验收场景。需求、行为、场景和来源之间的关系须从实际规格保存；[机器可读需求目录](../requirements/README.md) 与 coverage 记录是派生视图，不是更小分母的授权。

- 不能只统计已有测试、当前能运行的模块或某个切片引用的场景。
- 范围引用中的所有成员都必须保留。例如 `E01–E10` 不是只引用首尾两项，`V1-01` 与 `P41-01` 的数字也不能误作场景族后缀。
- 没有对应行为、缺少场景映射、未知编号与无法分类的义务必须显式保留，不能通过过滤把它们消失掉。
- 追加规格可以增加义务；不修改原始要求来迁就实现或当前证据。

历史原件保全、当前合同修订与导航规则见 [文档体系合同](documentation.md)。文档校验通过只证明它所检查的完整性与引用关系，不证明产品通过验收。

## 证据所需的绑定

一次结果必须能够回到实际执行对象，而不只回到一个 PR 标题或文件名。

| 绑定 | 必须能够辨别的事实 |
| --- | --- |
| 源码 | 仓库、准确提交；若有未提交变更，还须绑定实际输入差异和内容身份 |
| 产物 | 实际执行的二进制或 WASM 工件、内容摘要、构建配置与依赖；不是同名的旧产物 |
| 运行配置 | 实际 profile、provider 协议、模型/检查点、量化、资源与运行时选项 |
| 环境 | 平台、目标 ABI、设备、服务、安装、slot、home 和相关持久状态 |
| 输入与场景 | 原始输入、对应需求/场景、故障注入点、对照条件和可观察的通过条件 |
| 过程与结果 | 开始、结束、退出状态、原生输出、状态变化及失败/恢复证据 |
| 边界 | 本次没有运行、没有观察到或不能由该结果推出的事项 |

路径存在、哈希格式合法、JSON 能解析和退出码为零，分别只是局部事实。不能把它们扩张成整个行为成功。带失败标记、明确拒绝或未满足条件的结果，即使外层命令退出零，也仍然是失败或未达验收。

凭据不属于可公开的证据正文。保留必要身份与来源摘要，不把 token、密码、会话能力或签名下载地址复制进日志、pane、文档或共享工件。

## 四类证明范围

一个场景可以同时需要多类证明，不能互相抵销。

| 范围 | 所需证据 | 不能替代它的材料 |
| --- | --- | --- |
| `native` | 实际产品、真实运行时、协议、文件系统、浏览器、设备或宿主界面上的行为 | 类型、源码、mock 回声、静态截图资源或一个同名宿主程序 |
| `model` | 真实服务模型通过真实 Store、subject step、provider wire 与 parser 作出的判断和行动 | 固定答案、关键词判分器、直接调用决策函数或脚本代替主体 |
| `failure` | 对指定故障的真实观察，以及故障后应保留的事实、占用与错误 | 只构造一个错误枚举、延长 deadline 后重试、把未知结果改成未发生 |
| `recovery` | 故障后的新进程或新代际真正取得状态与控制并继续工作 | 同一进程内重置对象、仅能读出归档、重新执行原始副作用来伪装恢复 |

范围来自可审阅的规格声明，不由运行时关键词扫描决定。一个场景的完整通过记录必须覆盖它实际要求的全部范围；一个原生成功用例不会自动补齐模型、故障或恢复证据。

## 当前候选、历史记录与未绑定记录

历史记录保留它原本证明的事实，不因新提交出现而删除，也不自动变成新候选的证据。

- **当前候选**：证据明确绑定正在评估的源码与实际产物；需要时还要核对 profile、数据格式和环境。
- **历史候选**：绑定其他源码或产物。可以作为背景、回归依据或重验输入，不能直接宣布当前候选通过。
- **未绑定**：缺少足以确认候选的身份。不能因为没有发现不相等就当作相等。

重新绑定必须有实际、可检查的等价性依据，保留原记录及新绑定的理由和范围。只更新 `candidate_commit`、文件名、日期或 status，不会产生新的运行证据。

一个需求有多个场景时，每个应满足的场景都需要相应证明。命名了需求但没有证明其场景、只列 `cases_reference_only`、缺少必要范围或无法访问原始 receipt，都不能把该需求变成已满足。

## 报告接口与信任边界

[`shenko-evidence`](../../crates/shenko-evidence/src/lib.rs) 提供以下报告面：

- `SpecIndex`：规格中的需求、行为和验收场景，以及其来源位置；
- `EvidenceRecord`：记录声明的候选、需求、完整通过场景、仅引用场景、status 与 native receipt 路径；
- `EvidenceInventory` / `Binding`：已读到的记录及其候选绑定分类；
- `ScopeDeclaration`：场景所需范围与分类理由；
- `CoverageReport` / `RequirementObligation`：需求、适用场景、证据引用与未满足原因；
- `ReportInputs`、`analyze`、`Analysis::to_json`：把规格、证据目录和候选输入组成机器可读报告。

这些数据结构不是一个可以凭声明判断真假的验证者。尤其是 `full_cases_passed`、`status` 和 `native_receipt` 路径，只是输入记录的声明，不能单独证明真实通过。报告若未核对对应原件及绑定，不得将声明包装成已验证的产品状态。

库存读取必须区分空目录、目录不可读、记录损坏、非证据 JSON 与有效记录；不得悄悄跳过错误后把变小的库存当成完整。报告必须保留未绑定、未知、缺失和未达验收的义务。现有接口的存在本身不构成这一完整合同已实现或已验收的证明。

### 机器准入格式与边界

`shenko-evidence` 的报告 JSON 使用 schema 3，库存 JSON 使用 schema 2。`unresolved_references` 保留未定义行为、场景和不完整范围；`B15.Q1` 这类子编号不是 `B15` 定义，不能覆盖父项。定义表按 `Case` 列或明确的通过条件列识别，其他参考表中的数字标签不是场景。范围按实际场景集合解析，不按外部给出的巨大数字上界分配成员。定义表中无法分类的场景保持可见、未达验收。

报告的 `acceptance_case_matrix` 与 `case_status_counts` 以场景为单位给出 `proven`、`partial`、`unproven`、`unclassified`，`withdrawals` / `dangling_withdrawals` 保留撤回声明及其未匹配引用；场景级状态不因某条需求的其他场景通过而改变。

库存中的 `issues` 保留目录、条目、JSON 与字段读取失败，`non_evidence_files` 单列非证据 JSON。存在读取失败时，`analyze` 把库存标为不完整，并为需求保留 `inventory_incomplete`；不把仍能读取的子集当作完整库存。候选字段别名若互相矛盾，记录也是无效输入。

完整通过声明需要原始记录中的 `native_receipt` 与 `native_receipt_sha256`。准入重新读取该记录并核对已解析字段，读取原始回执、校验其 SHA-256，再检查以下结构：

| 原始回执字段 | 机器核对的条件 |
| --- | --- |
| `kind`、`schema_version` | `shenko_native_case_receipt`、`1`；其他历史格式保持可见，但不会自动转换为完整通过 |
| `evidence_id`、`candidate_commit`、`status` | 与记录和当前候选一致，状态为 `passed`；未绑定、历史候选和失败声明不计为当前证明 |
| `source`、`artifact` | 各含 `path`、`sha256`，对应输入材料和实际工件的原件可读且摘要一致 |
| `build`、`configuration`、`environment`、`boundaries` | 保留非空构建、配置、环境对象以及明确的边界数组，供原生审阅；不能由对象存在推断内容真实或完整 |
| `run` | `started_unix_ms`、`finished_unix_ms` 有序，`exit_code` 为零且没有相矛盾的失败标记；这些时间字段不是 7/30 天经过时间的独立证明 |
| `cases` | 不重复的场景编号与成功状态；声明的完整通过场景必须实际出现在原始回执中 |
| 每个场景的 `observations` | 按 `native`、`model`、`failure`、`recovery` 分开记录；每项有成功状态、非空且全为真的布尔 `checks`、经摘要核对的原始 `output` 文件 |

相对 `native_receipt` 从记录所在工作树根解析；独立库存没有 Git 工作树时，从记录目录解析，不搜索进程当前目录。回执中的绑定文件从回执目录解析，也可以使用明确的绝对路径。JSON 输入上限为 8 MiB，超限是可见错误；工件摘要使用流式读取。

每个场景都必须由同一份当前回执覆盖它所需的全部范围，不能借用另一场景或历史候选的范围。无关的旧记录继续保留，但不会否定另一份已经满足该场景合同的当前回执。报告用 `admitted_receipts` 固定本次读取的回执摘要及来源绑定，用 `evidence_rejections` 保留未准入原因；`receipt_present` 仅表示文件当前可访问，不表示准入成功。

`is_met` / `met` 表示上述机械覆盖条件成立，**不是独立的产品验收判断**。这个读取器不会证明记录者诚实、构建确实由声明源码产生、模型判断正确、故障真实发生或硬件行为满足规格；这些仍须由实际运行、原件审阅和本合同的对应验证完成。不能为历史结果补写一个 `passed` 回执来替代缺失的执行，也不能把格式验证样例写入正式完成记录。

## 原生与故障场景

运行的是被绑定的候选，使用隔离且真实的产品状态。对持久化、并发、生命周期、进程边界、升级和恢复，必须观察对应边界两侧，而不只观察调用返回。

- CAS、事务、outbox 或发送事实需要证明崩溃窗口中的持久结果与恢复后状态；一次普通成功写入不是断电证明。
- 取消、timeout 和断链需要保留未知结果及真实占用，直到有足以解除它们的证据。
- 升级需要实际旧进程退出、新进程独占所有权、选定代际与持久根一致；一次复制或 selector 写入不是完整交接。
- UI 需要实际界面上的渲染与交互。asset build、服务端返回 HTML 或一张与候选未绑定的图片不能替代它。
- WASM 需要真实模块、真实 imports/exports 和执行结果。目录条目、配方文件、native/remote 替代执行不能填满具名 WASM 工具的验收项。
- 音频、声纹、ASR/TTS、电话和设备行为需要各自真实输入与实际 provider/硬件通路，不用伪造媒体或模拟硬件状态作为完成证据。

已观察的失败是结果，不通过盲目重跑、改通过条件或扩大观测窗口把它抹掉。修复后重新运行时，保留修复前输入、候选、输出与新结果之间的对应关系。

## 模型行为与自主能力

模型行为验收使用真实服务端点与真实 Shenko 执行链，记录实际 checkpoint、架构、量化、协议、配置及原始输出。模型别名不是底层模型身份，也不是能力名单。

对 awareness、perception、Skill、上下文策略或其他影响行为的机制，应按相应规格采用多个场景、重复试次及启用/禁用的匹配对照，保留全部失败和拒绝。提示单元应表达原则与观察，不把期望步骤或答案写成让模型照抄的脚本。关键词过滤、测试名称匹配、人工指定正确工具或直接代行操作，不能证明自主发现与选择。

一个单元写入成功不等于有效；自写单元的采用需要真实沙箱自测与相应证据。某模型上的结果也不能被扩张为所有模型的保证。模型选择与产品能力发现保持可扩展，不把验收基线变成运行时白名单或额外人工审批门禁。

## 七天与三十天观察

规格要求的真实 7 天、30 天观察必须有真实经过的时间、服务生命周期及中断记录。

- 保存实际起止时间、单调时间信息、进程/主机重启、采样间隙、停机与恢复事实。
- 累计进程运行时间、墙钟区间、有效观测时长与服务可用时长分别说明，不混成一个数字。
- 回调计数、压缩循环、人工修改时间戳、历史片段拼接或“计划运行到某日”不能替代真实时长。
- 中途换候选、产物、profile 或观测环境时，记录边界，不能把不等价的区间当作同一候选的连续证明。
- 观察中断或 collector 失败必须可见；没有数据的区间不是成功区间。

`shenko-evidence --long-run PATH=SHA256` 只准入 `scripts/observe_longrun.py` 生成的 schema 5 报告：调用方给出的摘要必须与保留字节一致，报告必须绑定同一候选提交、所测二进制摘要与 profile。每个存活样本必须从打开的 `/proc/<pid>/exe` 文件描述符读取实际字节并核验 SHA-256；相同文件长度或修改时间不能代替摘要，无法读取的身份保持未知。`candidate_binary_binding` 为 `proc_exe_sha256`，验证样本列表必须无重复并覆盖全部存活样本，未知或不匹配样本不得成为时长证据。旧 schema 4 报告没有这项必需绑定，保留原字节并明确拒绝，不推断升级。其余条件仍须成立：`integrity.intact` 为真，`lifecycle.sealed` 且 `stop_reason` 为 `completed`，停止样本不晚于已记录样本，且至少一个声明窗口的合格秒数达到其声明时长；`duration_status` 只有在名为 `7d` 与 `30d` 的窗口都按 SPEC 时长合格时才为 `observed`，以这两个名字声明更短时长会被拒绝。被拒绝的报告不消失：其声明窗口仍留在分母并保持 `unobserved`，拒绝原因写入 `rejections`。多个已准入报告只有在二进制与 profile 完全一致时才能合并窗口；不一致时 `identity_conflicts` 显式列出，`duration_status` 保持 `unobserved`。没有绑定报告时该字段就是 `unobserved`。

达到时间长度也不是全产品通过：期间必须有规格要求的实际场景、正确性、资源和恢复观测。尚未经过所需时间的义务保持未达验收，不使用伪造完成日期。

## 性能结果与交付

性能数据必须绑定工作负载、并发、输入/输出规模、实际完成量与统计口径。prefill、完整流式 decode、排队时间和端到端耗时分开；真实模型输出不足、失败请求或中断不能从分母中静默移除。

交付记录分别说明源码、构建、测试、原生场景、模型行为、硬件/媒体/UI、故障恢复与长期观察的结果。通过一个层次不会自动通过另一个层次。只有全部适用需求与场景都有符合本合同的证据，才可宣布完整验收；否则保留完整分母和每个尚缺的实现或证明。

<a id="spec-23"></a>
<!-- migrated-contract: SPEC:23 -->
# 23 验收与发布阻断项

所有数值指标应记录设备、模型/量化、tokenizer/template、runtime、plugin lock、数据集与运行次数。以下是未来开发验收，不是已经通过的测试。

| 编号 | 组合场景 | 通过条件 |
| --- | --- | --- |
| A01 | 无来信持续运行 | 正常运行主体持续产生下一轮；每分支一个串行决策链，任务空也不自动退出，故障分叉按 A42 收敛 |
| A02 | 主体要求写文件/调用 SSH/REPL | 无对应工具或能力；只能委派；人格与插件不能突破 |
| A03 | C=1 且递归三级与 memory save | 全部可推进；父任务不占孩子必须的许可/执行席位 |
| A04 | 自动分配产生半池结果 | 无手动比例，在适合均分的 C=1 工作负载上算法自动形成约 50/50 驻留；改变需求后能自适应；报告 page 误差、命中、spill、rebuild 与真实保证等级 |
| A05 | K 未配置、共享 alias 与取消失联 | 自动能力发现/保守降级；不重复放行；未确认终止请求仍计占用 |
| A06 | 连续来信与群爆发 | durable event 可追溯；在主体安全边界处理；DND/过滤状态明确，消息不静默丢失或伪记已处理 |
| A07 | 沙箱与坏包强杀 | 自动 LKG/factory；最近已确认消息、身份、撤销与副作用 ledger 不回退 |
| A08 | arbitrary mounts 与分支 | RO/direct RW/COW 行为真实；不把 direct/SSH 状态宣传为可快照回滚 |
| A09 | Git 与 SSH WASM | 私有 GitHub/Gitea 端到端认证；真实 WASM SSH keygen/exec/transfer；限制明示 |
| A10 | 中文历史检索和统一知识 | 单/双/三字、中英混合及原始失败可查；主体可跨私聊/群聊回忆，保密选择由人格/灵魂判断，非硬编码受众屏蔽 |
| A11 | memory commit 崩溃与冲突 | accepted/committed 分明；跨 Git/DB 修复；两个 worker 不覆盖同一 live worktree |
| A12 | reset 与提醒 | 新研究 worker 不自动看到旧失败叙事；真实档案仍在；约束/预算不被 reset 洗掉 |
| A13 | 插件认知 | 不点名插件的真实任务能发现并用对；反例不滥用；旧版本/缺权限可恢复 |
| A14 | 远端 Agent 选择题 | 正确 seat/epoch/question；自主选择已授权事项；过期/超时不冒充授权；跨渠道询问收敛 |
| A15 | REPL 崩溃 | 显示实际恢复等级；代码/依赖/对象一致；不重放已完成外部副作用 |
| A16 | 通话与旁听 | 同一主体接受音频事件；录音状态真实；仍能调用完整委派链；barge-in 与转写修订不重复动作 |
| A17 | Android/慢闪存/满盘 | 恢复真实；未知消息缺口可见；内核与基础插件可继续诊断或正确阻断新写入 |
| A18 | Plugin generation 升级/卸载 | 无孤儿 timer/process/request/KV/SSH；core rescue 不随业务插件损坏 |
| A19 | UI brief 与 Proto 主线 | Claude Opus 设计实现符合功能/状态合同和生命感要求；诊断可展开，不强制监控首页；最新主线、compiler seam、键盘/小屏/减少动画通过 |
| A20 | 研究/记忆优化评估 | 等总预算，计入辅助模型与 prefill；多次运行；不以论文最大收益作产品承诺 |
| A21 | kernel/rescue 自主替换失败 | 候选失败有可启动旧闭包及可读旧数据；切换后的新事实有导入/修复路径，同分支不重复提交；旧权限/lease 不复活 |
| A22 | Computer Use 并行观察/竞争输入 | 一个共享桌面仅一个有效写入 lease；不同隔离桌面可分别操作；人工接管使旧输入失效 |
| A23 | 过期屏幕与未知 GUI 效果 | DPI/窗口/页面变化后旧坐标失效；发送后断线先核验，不盲重放；结果与传输 ACK 分开 |
| A24 | 个人 Agent 连续性 | 卸载编码插件后仍可认识人、履行生活承诺、收消息、主动维护；跨平台/课堂/远程任务共同经历可溯源；正常统一主体，故障分叉按协议汇合 |
| A25 | 动态 UI 跨渠道与流式中断 | 同意图可投影 Web/卡片/文字/语音；弱渠道能完成选择/输入；半成品动作不启用；最后有效视图、草稿和权限保留 |
| A26 | 动态动作版本与重复提交 | 外观改动保留有效 action；实质目标变化使旧 action 失效；跨渠道回答去重；交互进入同一主体，renderer 升级不改变业务事实 |
| A27 | 自主更新策略与版本选择 | 无用户在线确认时，主体可决定启停自动更新、更新/延后/跳过并委派执行；失败自动恢复；决定和效果可追溯，不把发现新版本等同强制更新 |
| A28 | 代理选择与配置恢复 | 配置问题进入同一主体，Agent 自主选择路由并由子 Agent 验证和生效；断链按计划恢复；待发事件保留，旧模型请求占用不因换代理清零 |
| A29 | 可选 BoxLite adapter（采用后验收） | 显式保留与恢复等级真实；clone 不混写宿主卷；本地/远端 GUI 与网络路径分别通过；终止、模型 broker、凭据归档及版本能力满足 [§34.7](desktop-runtime.md#spec-34-7) |
| A30 | 自主发现 tmux 能力缺口 | 文本相同但高亮改变时承认未知；检查 -e/属性传输后自主修复或开发插件，验证原任务、宽字符、resize/重连；自然任务会正确使用 |
| A31 | 共享能力与主子双向讨论 | 同一 registry 能力可由主子发现；有限对话可恢复，不复制全部历史；主体仍不直接文件/exec，等待不占孩子所需槽 |
| A32 | 四后端原生 API 透传 | 版本/传输/配置与保证明确；被忽略字段、HTTP 假成功、Snapshot/Replay、Host/GPU/decode、同物理别名分别处理；无虚构 slot |
| A33 | Pi Durable 式任务恢复 | intent/外部效果/receipt 各崩溃点可对账，已完成调用不盲重放；补偿本身可恢复；断电保证与实际 storage profile 一致 |
| A34 | LayerFS 原版 Linux VM | 实际 Store 满足同 kernel/本地 FS；workspace Docker/FUSE 组合通过后才标可用；layerfs_committed 与 product_durable 不混用 |
| A35 | Life 身份复用 | 稳定 person ID 与 recording-local rank 分离，模型/数据 revision 可追溯；候选与确认不混淆，识别结果不授予 admin |
| A36 | 灵魂与 child prompt/model 自主性 | 主体可修改处事哲学和保密判断，无固定亲密度；可逐次指定 child 完整 system/model；角色、schema、模型路由及资源域仍准确 |
| A37 | 有限 child 直接发布 | 无逐条主体审稿即可发送有效委派结果；进展不吞最终/失败；旧 generation 晚结果失效，主体不重复外发，unknown 可恢复 |
| A38 | 打断、DND 与误屏蔽 | 高频消息停滞建议能到主体；主体自主接受/拒绝/改 DND，结束处理原队列；通配符误屏蔽仍有独立健康证据并可自主修复 |
| A39 | breaking-change 自迁移 | 旧 A 服务时完成 B 复制/追赶/烟测；各阶段崩溃可恢复；B 产生新事实后故障不丢新历史；无固定旧 schema 兼容要求 |
| A40 | 永久默认与空间处置 | 文字/录音/帧无隐含 TTL；主体可自主归档或 erase；半上传不能删除唯一副本；墓碑跨回滚/分支同步生效 |
| A41 | 长期自主兴趣 | 无用户相关性仍可持续续开；升级、切窗和接管保留目标证据；阶段预算可自主续配，真实 C/设备约束不被绕过 |
| A42 | 多节点在线、异构模型与递归接管 | 每个连通域一个主节点；主机断线后依赖其模型的路径不可用，按真实可达能力与隐私策略选择备用，可继续递归；完全隔离的可 access LLM 节点各自演进。重连停止落选主体、fence 旧主并汇合，保留重复/unknown，不伪造全局 quorum，同 Engine 不超 C |
| A43 | 用户 Tailnet 基线 | 按四流标称 128K 验收，133120 仅冗余；记录实际模型/模板/1Cat patch 与硬件；生成/取消/回传断链不视为已释放请求 |
| A44 | budget / notes / summary / snapcompact | 多次切窗可按 ID 读原文并续接未完成工具；PNG 不冒充无损档案；实测中英文数字/路径/代码、图像预算、KV/总成本；C=1 摘要无死锁 |
| A45 | 两种 compact 独立插件交付 | budget compact 与 snapcompact 均存在可安装实现，各自独立安装/更新/卸载/替换；策略不进入核心；视觉缺失报告真实并可转其他策略；切换时原文、已提交窗口和未完成动作可续接，旧插件晚结果失效 |
| A46 | 框架品牌与实例命名 | 初始化支持用户取名或 Agent 自主取名；日常显示与称呼采用实例姓名，不强制框架品牌；改名、升级与节点接管保持同一 identity、历史和任务 |
| A47 | 新设备身份加入与跨设备操作 | 较晚新节点确认可信较早身份后撤销自己的独立身份并同步，已有经历/效果保留；最早身份不强制永远当主。已授权 SSH Key、内网/Tailnet/ZeroTier 路径与真实 root Android/Linux/macOS/Windows 能力可发现并委派；provider/凭据不被假称已同步 |
| A48 | 环境漏洞/故障察觉与自主修复 | 已授权节点的实际版本或故障证据进入主体，有限 child 自主检查、修复或更新；结果和恢复覆盖真实，无新增人类 gate；未检查不报健康，失去模型后的已有恢复继续按实际合同执行 |
| A49 | 所有 LLM 失效、外部导出与离线镜像恢复 | 可工作控制/OS 继续日志；持正确 credentials 的外部工具无需模型提取原始记忆、经历和效果。无法启动时从镜像在其他节点读取/复活/合并；缺密钥/损坏/未复制事实报告缺口，unknown 不盲重放，不以新摘要替代原文 |

建议可靠性目标：已确认接收的本地事件在进程崩溃测试中 RPO=0；可用本地存储与健康闭包条件下恢复控制服务目标 30 秒内，不含模型重新装载/不可达平台。真实断电 RPO、音频端到端时延、工具/FTS p95、持续 7/30 天 RSS 漂移需参考设备测量后定值。

不满足即不得宣称完整目标完成：真实 SSH WASM；默认 host-mounted Git memory；C=1 多驻留能力；主体无文件/exec 权限；原始历史可恢复；默认开放聊天；无人持续思考；所有已声明功能的真实插件边界。尚未支持的工具/平台可在阶段版本注明，但不能从最终 SPEC 删除。

首版开发者发行适用 [§35](self-update.md#spec-35) 的 V1-01–V1-15、[§36](durable-state.md#spec-36) 的 E01–E10、[§37](context-policies.md#spec-37) 的 H01–H10、[§38](cognition-units.md#spec-38) 的 K01–K09，以及 [§41](architecture.md#spec-41) 的规范合同与相应故障矩阵。它们与该 profile 实际涉及的 A 项组合，不能把未来所有媒体、设备和平台用例都强行设为首版前置。[§40](../governance/development-contract.md#spec-40) N 项按变更影响选择。技术验收自动执行、修复和推进，不默认等人签字；当前尚未进行产品实现或实机验收。

<!-- /migrated-contract: SPEC:23 -->

<a id="spec-27"></a>
<!-- migrated-contract: SPEC:27 -->
# 27 共同排障与接入验收 Common troubleshooting

| 症状 / Symptom | 中文检查 | English check |
| --- | --- | --- |
| C=1 但显卡仍有多请求 | 检查未受管直连、相同 engine 的不同 alias、取消未确认和旧 broker epoch | Look for unmanaged clients, aliases assigned to separate groups, unconfirmed cancellation and requests from an older broker epoch. |
| 主/子切换总是长 prefill | 比较真实 token prefix、工具顺序、人格/模板 revision、scope、backend epoch；确认完整 state 可恢复 | Compare actual prefixes, tool ordering, persona/template revisions, scopes and backend epochs; verify complete state restoration. |
| cache hit 很高但响应慢 | 区分排队、Host→GPU 恢复、decode、工具和网络；命中不等于零搬运 | Separate queueing, restore transfer, decode, tools and network latency; a hit can still require expensive transfers. |
| GPU 或 RAM 不足 | 分别观察 weights/workspace、KV、StateImage、Host backing 和迁移中目标；先减工作集 | Inspect weights/workspace, KV, recurrent state, Host backing and in-flight destinations separately; reduce the working set first. |
| 插件内部偷偷调用模型 | provider 声明其模型入口，接 broker/proxy；预留须可强制且从总额扣除，否则标明 unmanaged 和软保证 | Route internal inference through the broker/proxy. Reservations must be enforceable and deducted from the shared total; otherwise label the consumer unmanaged and the guarantee soft. |
| Chat 能用，工具不能用 | 核验模型 template、tool schema、stop/stream 解析和 provider 真实能力 | Verify the model template, tool schema, streaming/stop parsing and actual provider support. |
| 恢复出现旧群消息/旧权限 | scope/epoch/revocation 不跟沙箱回滚，旧 context 失效；重新从原始事实投影 | Keep scope, epoch and revocations outside sandbox rollback; invalidate stale contexts and rebuild from source facts. |
| 切窗或 reset 后又想起所有失败 | 检查自动 memory 注入、共享工作树、FTS/Git 可见范围；新研究 epoch 隔离旧叙事 | Check automatic memory injection and visible workspace/history; isolate old narratives from the fresh research epoch. |

两后端共同验收：单个短请求、工具调用闭环、主子交替、递归依赖、已取消但未停止请求、backend 重启、context 溢出、权限隔离、真实 cache miss 与 UI 解释。测试数据不含真实私密记录；实测结果须带完整环境 manifest。尚未有 adapter 的条目标为 planned，不能以教程可读代替接入完成。

For both adapters, validate a short request, a complete tool round trip, alternating controller/worker contexts, recursive dependencies, cancellation uncertainty, backend restart, context overflow, scope isolation and truthful cache diagnostics. Record the full environment manifest. Documentation readiness is not adapter readiness.

<!-- /migrated-contract: SPEC:27 -->

<a id="spec-29"></a>
<!-- migrated-contract: SPEC:29 -->
# 29 交付路线与阶段验收

这是一条完整开发的工作顺序；阶段编号不等于工期。本次仅准备交接仓库，接收 GOAL.md 后按 U50 连续实现全部 S1–S5，不以首版结束。最新裁决把 NapCat、已有 tmux Agent seat、能力自主补全、候选验证、自迁移和运行接续一并前移到 S2 首版开发者发行；插件恢复合同、API 注入兼容与内置自解释同时交付。后期能力仍在最终范围，每阶段提供可评审证据和未满足需求清单。

| 阶段 | 交付与要证明的问题 | 结束条件 |
| --- | --- | --- |
| S0 规格收敛 | 把已确认目标写成具体合同，记录可自主执行的默认和少量真实未决；锁定研究基线、来源/行为对照与故障验收；采用已确认 Cordis 主干，细化 bootstrap 与开放来源/可原子化的 awareness | v0.6.2 文档交接后，接收 GOAL.md 即按 [§40](../governance/development-contract.md#spec-40)、[§46](../governance/COMPLETION.md#spec-46) 开始完整开发，不逐阶段再设人工 gate |
| S1 首版关键可行性 | 真正 WASM 家园与恢复、默认宿主挂载 Git；NapCat 收发、现有 tmux 属性观察、Claude/OMP 控制；构建身份、effect/receipt、hook 兼容、C=1 与四流；核心/恢复器迁移证明 | 关键路径及失败证据可复现；确认首版使用宿主哪些能力，不能用 host wrapper 冒充 WASM；不要求先移植全部工具或完成全部设备 profile |
| S2 首版开发者发行 | 小核心与持续个人主体，durable 委派/记忆/消息，全局 broker；默认 NapCat、HostExecution/SSH、真实 Agent seat、自解释、注入兼容、自开发/验证/更新恢复插件；两个独立 compact 插件；Proto-UI/shadcn 单主体时间线、对话配置与查看面板、基础持久全栈 Playground、首启 seed 与可分别安装的轻量 awareness | 完成 [§35](self-update.md#spec-35) 的 QQ→现有 coding seat→自身候选→验证→迁移激活→继续工作，及 [§36](durable-state.md#spec-36)–[§41](architecture.md#spec-41)、[§43](configuration-playground.md#spec-43) 的适用合同；可自主升级核心，无固定逐次审批；不是仅产出补丁 |
| S3 更多平台与设备 | 更完整个人/群聊发行、iLinkBot/官方机器人/飞书等、联系人、native 后端与双语教程、WASM SSH/Git/包/更多工具移植、Android/路由器 profile、UI 深化 | 逐项标明 planned/ported/verified；保留默认开放聊天与真实原生透传，未支持功能不伪报 |
| S4 能力与环境扩展 | 长期兴趣/保留插件、研究 reset/主动提醒完整效果验证、Python/Node/Bun 目标逐一推进、Linux/Windows/macOS Computer Use、实时通话/课堂、动态生成 UI/Playground 深化，Provider 周期额度观测与模型/频率自适应插件；自主改进持续扩展 | 各能力独立可安装且组合不突破主循环/资源/来源/副作用合同；首版已有的自开发、hook 与自迁移继续完善，不延期重做 |
| S5 长期稳定与生态 | 7/30 天无人运行、断电/满盘/撤销/网络分区、多节点选主与合并、市场/兼容、模型与 UI compiler 迁移、性能调优 | 报告资源/正确性/失败分布，回归覆盖需求矩阵，不以一次 demo 当长期稳定 |

动态 UI、媒体、Computer Use 的能力与事件接口从 S2 保留，完整功能分阶段推进。cordis-rs 主干上的 hook 合同、自解释、外部效果记账与自开发闭环不能等到 S4 才补；它们本身仍为可替换模块。真实 Node/Bun WASM、任意 native memory 兼容、各设备持久性等难项仍需逐项验证，阶段表不构成兼容性证明。

<!-- /migrated-contract: SPEC:29 -->

<a id="spec-29-1"></a>
<!-- migrated-contract: SPEC:29.1 -->
## 29.1 文档与工程维护规则

每个开发任务开始：核对本文现行用户合同、Proto-UI 最新主线及相关 upstream；记录兼容矩阵、pin SHA 与变更范围。模型/provider/schema/template 变化都进入环境 manifest。每项 feature 附需求 ID、完整用户旅程、失败/恢复语义、资源归属与真实验收结果；没有实现的字段明确标拟议接口。

插件文档包括中文与英文最小路径、短 capability card、完整参考、可运行 conformance、当前设备/模型覆盖和已知差距。自改任务使用相同要求。后续替换 Proto-UI compiler、memory backend、model provider 或 harness，不应需要重写所有业务插件；若接口变更则提供可验证迁移，而不是只更新 README。

<!-- /migrated-contract: SPEC:29.1 -->

<a id="spec-29-2"></a>
<!-- migrated-contract: SPEC:29.2 -->
## 29.2 本轮研究组织与完成状态

| 子 Agent 分工 | 研究范围 | 对主稿的审查贡献 |
| --- | --- | --- |
| WASM 与持久化 | Runtime/ABI、工具移植、任意挂载、Nix-like、LayerFS、Git/SSH、REPL、移动端、PMA、逐项需求审查 | 区分真实移植与 host wrapper；确认恢复边界与全部需求可追踪 |
| Harness 与推理 | pi/omp/DSH/cordis/Codex、session truth、单主体、全局限额、KV 算法、NInfer/Strata、插件认知与 27B 论文 | 审查 C=1 多驻留、父子死锁、旧请求交接、半池与工具使用验收 |
| 竞品与渠道 | dots/Duet/Muse/AstrBot/OpenClaw、国内接入、Proto/shadcn、遗忘论文、remote seats、Computer Use、个人关系、Dynamic Response/Intelligent UI | 审查开放对话、动态联系、OS 边界、个人框架定位与动态 UI 能力降级 |
| 本轮独立增量 | Pi Durable/OMP 结果投递；NInfer/Strata/vLLM/1Cat 原生能力；LayerFS Linux VM；迁移/保留/选举；DND；budget/snapcompact | 分别由子 Agent 调研，再由主体核对一手源码与合入；未把资料审计当部署或性能验证 |
| 首版开发者闭环增量 | NapCat/OneBot/napcat-cli、Claude/OMP/tmux、源码到运行身份、A/B 升级与效果边界 | 三路子 Agent 研究并交叉审查，前移路线图、区分真实协议和 TUI、补首版组合与故障验收 |
| 插件时序与自解释增量 | Fabric API/Mixin 顺序参考、C=1 hook 死锁、Cordis 指代纠正与真实生命周期、OMP 内部文档与哲学 | 沿已确认 Cordis 主干定义 API 兼容；自解释绑定运行/源码/手册/设计版本 |

三条研究线都读取了一手资料，并对最新追加要求补做专项和覆盖审查；主稿由主 Agent 统一解决冲突、复核关键来源和维护一致性。没有为了实现本轮研究而安装本产品、运行上游 benchmark、连接用户机器或对外发布应用。

<!-- /migrated-contract: SPEC:29.2 -->

<a id="spec-41-1"></a>
<!-- migrated-contract: SPEC:41.1 -->
## 41.1 本章的地位、范围与阅读方法

本章响应 U40，定义 Shenko 首版开发者发行可以据以实现、互操作和检验的执行合同。[§1](architecture.md#spec-1)–[§24](desktop-runtime.md#spec-24) 说明产品目标与能力，[§35](self-update.md#spec-35)–[§38](cognition-units.md#spec-38) 说明首版旅程及插件职责，[§39](../requirements/design-provenance.md#spec-39) 记录来源与差异，[§40](../governance/development-contract.md#spec-40) 规定开发方式；本章给出记录、状态、事务、接口、错误和交错结果。遇到前文“至少包含”“原子提交或同等恢复”等概述，以本章相应具体合同为准。用户已确认的产品要求优先，不能借细化改变永续主体、自主性、role 边界或插件化。

本章中的“必须／不得”是实现不变量；“首版默认”是开发 Agent 可自行实施和按证据调整的工程选择；“后端条件”必须来自真实能力协商，不能由接口外形推定。所有新增类型名和命令签名是 Shenko 拟定的协议，不是已发布 SDK、cordis-rs 现成 API 或已实现产品。Rust、WIT、进程内调用和网络传输可采用不同编码，但必须映射同一语义，并给出协议版本及 conformance 证据。

首次实现按以下顺序读取：共用提交与记录 → 主体及 inbox → operation/取消/回执 → broker/后端 → hook/插件调用 → seat/outbox → 升级与恢复 → 故障矩阵及完整轨迹。某个插件只需实现其适用合同；“一切插件化”不是让每个插件自行解释什么叫成功、取消或恢复。

**复用与扩展边界。** 本章借鉴固定版本 [Pi Durable SPEC](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md) 的单一 mutation line、已提交可见、外部 effect 与事务分离、持久 task/cancel、ownership 和重开恢复，尤其 [§4](subject-runtime.md#spec-4)、[§5.1](subject-runtime.md#spec-5-1)–[§5.5](model-children.md#spec-5-5)、[§6](subject-runtime.md#spec-6)、[§7](plugins.md#spec-7)、[§10.1](durable-state.md#spec-10-1)、[§11](remote-seats.md#spec-11)。Shenko 的单线程永续主体、物理 Engine broker、可替换全部实现、A/B 自迁移、多节点 availability branch 和跨代证据接收是基于用户要求的扩展。不得把 Pi 不提供的 CRDT/offline multi-writer、非合作代码强制终止或完整进程升级归为它已经解决。来源：REF-DURABLE、REF-USER、REF-DESIGN；行为 B03/B04/B20/B24/B36–B41。

<!-- /migrated-contract: SPEC:41.1 -->

<a id="spec-41-1-1"></a>
<!-- migrated-contract: SPEC:41.1.1 -->
### 41.1.1 定义写完的判据

每个有状态命令必须同时给出：调用方与状态所有者、必填输入、版本/幂等/前置条件、一次原子提交包含的记录、返回的持久回执、取消和迟到结果、重开后的处理、可观察事件及至少一个失败交错。缺一项即标记该合同尚待细化，不能以“provider 自行实现”掩盖跨模块歧义。与目标环境有关的性能值需要测量；未知值不能写成保证。

首版自动验收应覆盖下面的规范行为，而不要求一次实现所有未来设备和媒体 profile。后期 WASM 移植、通话、Computer Use、REPL、存储插件继续继承这里的 intent/effect/receipt、任务、资源和版本规则；它们各自还需满足原章节的能力验收。发布清单同时列出已交付和仍待交付的 R 项，不能用首版范围删除未来正式要求。

<!-- /migrated-contract: SPEC:41.1.1 -->

<a id="spec-41-16"></a>
<!-- migrated-contract: SPEC:41.16 -->
## 41.16 规范故障与一致性验收

以下 DX、RT、P41 三组按相应合同选取；重叠用例可以共用 fixture，但保留每个语义断言的对应关系。本轮未执行这些产品测试，不能把规格审查当作恢复验收。

<!-- /migrated-contract: SPEC:41.16 -->

<a id="spec-41-16-1"></a>
<!-- migrated-contract: SPEC:41.16.1 -->
### 41.16.1 提交与代际故障

测试必须在确定的断点主动终止/抛错，再从磁盘重开验证；仅在代码里 mock 一个 success boolean 不算恢复测试。模拟进程崩溃与 OS/供电故障分组报告，不能用前者替代后者。

| ID | 注入位置/竞态 | 预期持久状态 | 必须观察到的行为 |
| --- | --- | --- | --- |
| DX01 | T1 admission 前杀 worker | 无 armed attempt | provider 外部计数为 0；原任务可重新准备 |
| DX02 | 对象 durably seal 后、DB 引用前终止 | 允许孤立对象，不允许已确认悬空引用 | 恢复不把孤立对象当已提交结果；GC 不误删 pending root |
| DX03 | T1 实际提交后让 storage 向 caller 抛错 | T1 可能已存在；当前 writer 失效 | 重开按同 operation/attempt identity 查找，不重复创建 attempt，不继续旧 callback |
| DX04 | armed 落盘后、socket/send-keys 前杀死 | armed，无 outcome | 进入 reconciling；不得因为测试知道尚未发出就让通用恢复器假定无效果 |
| DX05 | QQ 已发/远端命令已启动，但响应丢失 | 无 conclusive outcome | 有真实查询/幂等能力则利用；否则保持 unknown，不重复按回车/发送 |
| DX06 | 收到结果后、T2 提交前杀死 | 可能只有 E1 或只有外部事实 | 依据 E1/查询协调；不会单凭 transcript 空缺重做效果 |
| DX07 | T2 提交成功、UI/调用者 ACK 前杀死 | 结果与 task/submission 同步落账 | 重开只重投已提交视图/通知事件，外部效果计数不增加 |
| DX08 | task abort mark 与旧 run T2 竞争 | 二者按同一写入序列排序 | mark 后旧 run 不推进 task；后来真实成功回执仍 E1 入账，不能被改成“未发生” |
| DX09 | 同 logical key 在新版本换了参数/target | 已存在原 Operation | 明确 key conflict；不执行新参数，也不返回不相关旧结果 |
| DX10 | 补偿外部成功后、补偿 T2 前崩溃 | 原效果仍在，补偿 attempt 未决 | 恢复补偿自身的合同；不盲目第二次撤回/删除/退款 |
| DX11 | 当前版本不能解码旧 receipt | 原始 bytes 与版本保留 | uninterpreted case；安装 reader 后可解码；其余无关任务继续 |
| DX12 | B smoke 重放含 QQ/tmux 的历史 | B 未激活、无真实 writer | 真实账号消息数和真实 pane 输入数均不增加 |
| DX13 | A 达到 w1，selector CAS 前崩溃 | selector 仍 A；B 仅候选 | 恢复器读选择记录，恢复 A 或完成既定切换；不同时启用两条对外主体 |
| DX14 | selector CAS 成功但 ACK 丢失 | selector B，新 epoch | 重开启动 B；A 不能因超时重新取得旧写入权 |
| DX15 | B 已产生新格式事实后回退 A | B 事实及 reader/recovery closure 保留 | A/恢复服务能接续或隔离未解释项；没有按 w0 重发旧外部操作 |
| DX16 | A 的请求在 B 激活后才返回 | producer_epoch 旧，接收 epoch 新 | E1 保留原 operation 的事实；旧 callback 无新决定权；实际占用按真状态释放 |
| DX17 | 人类绕过 companion 同时操作 tmux | 观察可能过期，外部 writer 不受 lease 约束 | 不宣称原子 verify-and-input；重新观察/协调，不盲重放选项输入 |
| DX18 | 删除 task 定义或重复崩溃直到阈值 | checkpoint 与原 Operation 保留 | blocked/parked 可发现；主体能委派修复；不把任务静默标完成或把全系统持续重置 |

<!-- /migrated-contract: SPEC:41.16.1 -->

<a id="spec-41-16-2"></a>
<!-- migrated-contract: SPEC:41.16.2 -->
### 41.16.2 主体、队列、后端与 Seat

RT 编号独立于已有 A/E/H/K/P 编号。均为实现后的可执行用例；本轮尚未执行。每项保留 fixture、故障点、提交日志、外部可见行为及资源曲线作为证据。

| ID | 给定状态与故障注入 | 必须观察到的结果 |
| --- | --- | --- |
| RT01 | S1 存储提交前、提交已落盘但返回前、提交后派 child 前分别终止控制器 | 每步至多一个已提交决定和一个 successor；已提交 child 意图能恢复；未提交草稿不执行；提交结果不明不重复派发 |
| RT02 | 事件 101 为普通结果，105 为高紧急来信；先选 105，之后重启 | 101 仍可查询并被处理；105 的 presentation/handling 清楚；没有以 consumed=105 跳过 101 |
| RT03 | 主体创建一个长时间运行的 child 后，连续没有外部消息 | SubjectStep 继续提交后继；child 不把每步卡在 completing；主体正常连续推理，child 也取得有限轮转机会 |
| RT04 | 有限父 child 等子 child；同时制造 wait-on-self、wait-on-finishing-owner 两个反例 | 合法等待释放 compute 和必要 worker 席位；反例返回明确 cycle，不进入无限等候 |
| RT05 | C=1，R0 正运行，QQ 连发 20 条消息；取消 API 返回 accepted 后断线 | 只形成合并的停止意图；R0 allocation 仍 held；主请求不越限开始；终止证据到达后才推进；20 个原始事件均存在 |
| RT06 | adapter 报内部 paused，但后端可自行恢复；另一个 adapter 提供真正 gated pause | 前者不能提前释放 C；后者凭有效 proof 释放并在恢复前重新准入；占用曲线从不超过实际声明的硬上限 |
| RT07 | 主 context 占满状态池，必要 child 需要 state；或一次 swap 最终装得下但中间峰值装不下 | planner 识别真实不可行性，迁移/回收/改变计划；不持有父硬 pin 无限等 child，也不提前释放正在传输的源 |
| RT08 | 同 engine 的两个 URL、Chat 与 native 接口、远端 OMP 递归任务、候选 B smoke 同时请求 | 共享一个资源域/许可账本；所有受管请求合计满足 C；未纳管者明确显示且不冒称硬保证完整 |
| RT09 | Q7 答案决定提交后，人类先回答、问题改 revision、pane 退出到 shell、SSH 在输入后断线 | 前三种在可观察到变化时拒绝旧输入；断线后的效果保留 unknown 不盲按 Enter；tmux 剩余竞态明示，不以测试通过宣称原子协议 |
| RT10 | OMP UI ask 超时选择默认；随后本产品的答案才到 | 记录 external_default/late ignored；不记成主体主动选择，不把问题关闭当成我们答复生效 |
| RT11 | 原生 slot 复用、engine 重建或模板/tool schema 改变后，旧 KV/旧完成事件迟到 | 按 engine/slot generation/内容指纹拒绝错误续接；旧事实可保存；不把旧终态证明用于释放另一个请求 |
| RT12 | A/B handoff 时 I/R spool 继续接收；A 留有 unknown generation 和发送 unknown | B 接收真实向量与全部占用，不重置 C，不丢事件，不重发未知 QQ；迟到回执可入账但旧 A 不能再决定 |
| RT13 | B 已发消息并写新格式 receipt 后回退旧闭包 | 新事实保留且能由保留 reader/迁移协议核验；不可解码局部保留待恢复；旧 snapshot 不复活已执行 operation |
| RT14 | 65+ 条事件触发摘要需求；summary hook 需要共享后端推理；C=1 | 先有有界计数/handles；摘要作为可追踪 child 通过 broker；父无 permit 等子，摘要不伪标所有事件 handled |
| RT15 | stream chunk 持久前崩溃、持久后发布前崩溃、发布后连接重连 | 未提交内容没有被当正式流内容展示；已提交 chunk 可按稳定序号续传，重复投递去重；partial tool 参数不执行 |
| RT16 | 主体自主语义等待、条件变化、600 秒到期与重启接管 | 同一主模型沿同一串行后继持续轮询前文和新增上下文；无新事件也接续，无时间 sleep/第二 waiter。条件成立或改变判断可提前退出，到期恢复正常思考，期限不因轮询或恢复续算；容量/OS 实际阻断另记，child sleep 仍可用 |

<!-- /migrated-contract: SPEC:41.16.2 -->

<a id="spec-41-16-3"></a>
<!-- migrated-contract: SPEC:41.16.3 -->
### 41.16.3 插件与版本交错

下表收紧现有 E/H/K 用例的可判定结果，不取代它们，也不要求每次文案编辑重跑整套测试。实现触及相关合同才运行对应故障用例。

| Case | 注入的情况 | 通过条件 |
| --- | --- | --- |
| P41-01 | 相同 intent key 重试 3 次；第 3 次参数不同 | 前两次相同 operation；第 3 次 `INTENT_KEY_CONFLICT`；没有新增外部派发 |
| P41-02 | QQ 已收到消息，成功回执提交前杀进程 | 原 operation 为 unknown；重开不自动再发；查询/迟到证据能完成原记录 |
| P41-03 | SSH 命令写入 host bind 文件后取消，并 restore WASM | host 文件真实改变仍在；operation 标清效果；恢复页面不宣称该文件已还原 |
| P41-04 | 旧工具参数为字节，新同名工具改为 KiB；请求在更新前已 seal | 按旧 contract 执行或明确 unavailable，不能用新单位执行 |
| P41-05 | C=1，主请求 preflight hook 委派模型检索 | 父请求不占 permit；检索完成后父请求继续；子任务与主体均可推进 |
| P41-06 | stream 到一半到达 50 条外部消息，异步 PMA 同时完成 | 消息全数持久化；本 stream seal 不变；后续按明确 event/revision 处置，水位不代替逐事件消费，旧贡献不被重复插入 |
| P41-07 | 两个 collector 以不同完成顺序返回相同 slot/key | 每次都得到相同冲突或声明过的 reducer 结果；完成时序不影响模型输入 |
| P41-08 | 新 hook 图 A→B→A；或 around 调用 next 两次 | 有环图不激活；第二次 next 不产生第二外部效果；已发生第一效果仍有 receipt |
| P41-09 | 查询 active 自身文档时切换版本，并继续下一页 | 每页同一 anchor，或明确返回旧 anchor 与新 active；不混合版本 |
| P41-10 | B 生成 A 读不懂的 receipt 后回退 A | 原 receipt、codec 和事实完整；保留 reader/转换器后能解释；否则相关操作停留待恢复，不盲重放 |
| P41-11 | 旧 worker 在 handoff 后才返回成功 | 新 writer 接纳原操作回执；旧 worker 不获得新命令权；不重复完成同一任务/outbox |
| P41-12 | outcome 提交发生不确定存储错误 | writer 停止新派发；重开查原 admission/operation key；既有外部事实不因新 retry key 被重复执行 |

<!-- /migrated-contract: SPEC:41.16.3 -->

<a id="spec-55"></a>
<!-- migrated-contract: SPEC:55 -->
# 55 自主能力验收合同 / Autonomous Capability Acceptance

来源：用户指示 2026-11-07，对 [§54](architecture.md#spec-54) 原则的落地要求。本节把「更自主、不受模型限制、可扩展插件化、更好的 awareness 与 perception」从方向改写成可判定的验收合同。不修改 [§1](architecture.md#spec-1)–[§54](architecture.md#spec-54) 原文；与旧读法冲突时以本节和 [§51.10](cognition-units.md#spec-51-10) 治理为准。

<!-- /migrated-contract: SPEC:55 -->

<a id="spec-55-1"></a>
<!-- migrated-contract: SPEC:55.1 -->
## 55.1 自主性的可判定化

「更自主」验收为以下可观察行为，不是主观评价：

1. **无提示继续**：主体在没有新的用户消息时，按 [§51.3](cognition-units.md#spec-51-3) 的持续思考执行下一步——自检、反思、维护、兴趣探索——且该步的输入与产出写入账本，可回放判定其并非空转。
2. **能力缺口自补**：当主体遇到当前能力集未覆盖的需求，它走 [§18.7](cognition-units.md#spec-18-7) 的缺口发现路径，产出一条新的可安装单元或一条带理由的「不可行」事实，而不是把缺口吞掉或反复报错同一行。
3. **自主修复边界**：主体对自身可逆状态的回滚、对插件的重装、对失败的 task 的重排，不经人工逐步批准；每一次自主动作都留下带理由的态度记录（[§51.1](cognition-units.md#spec-51-1)），事后可被审计或撤销。
4. **拒绝代替提问**：主体对真实外部约束（账户、设备、OS、上游服务）如实记账为外部事实，不把「做不到」改写成「问过人类」或「已尝试」。

<!-- /migrated-contract: SPEC:55.1 -->

<a id="spec-55-2"></a>
<!-- migrated-contract: SPEC:55.2 -->
## 55.2 模型限制不作为设计边界

[§54.2](architecture.md#spec-54-2) 落地为：任何把「当前模型做不到」当作永久边界写进架构判断的代码或文档，都视为缺陷。验收时检查：

- 凡是出现模型特有 token 预算、并发上限、工具调用格式假设的位置，必须能指出它位于可替换的投影/适配层（taste、channel adapter、provider binding），而不是内核契约。
- 内核与账本中不得出现针对特定模型的硬编码分支；适配归适配层，且每处适配都可随模型升级独立替换或删除。
- 当模型能力扩展（更大上下文、更强推理、多模态），主体应能仅通过更新 taste 投影与 provider binding 利用新能力，不需要重构内核。

<!-- /migrated-contract: SPEC:55.2 -->

<a id="spec-55-3"></a>
<!-- migrated-contract: SPEC:55.3 -->
## 55.3 插件化验收

[§54.3](architecture.md#spec-54-3) 落地为：插件化是产品级承诺，以下每条都可实测：

- **能力可安装**：新增一个 awareness/perception/Skill 单元不需要改动 `shenko-subject` 或 `shenko-store` 的内核代码，只需要按能力契约注册。
- **能力可替换**：同一能力契约 `cap.X` 可由两个不同单元提供，主体可在运行时切换提供者而不重启；依赖该能力的使用方不感知替换。
- **能力可学习**：awareness、fact、conclusion、attitude 的修订走 [§51.1](cognition-units.md#spec-51-1) 的带理由新内容 + 旧版本保留路径，可在账本中追到每一次学习的来源与理由终点。
- **协议可插拔**：新增一条通道（新 `ChannelPlugin`）不改动已有通道的代码路径；通道的注册、发现、故障隔离彼此独立。
- **插件可回滚**：对有实现的插件，其版本切换走 [§51.4](cognition-units.md#spec-51-4) 的版本化根追加节点，旧版本字节不被改写。

<!-- /migrated-contract: SPEC:55.3 -->

<a id="spec-55-4"></a>
<!-- migrated-contract: SPEC:55.4 -->
## 55.4 Awareness 与 Perception 的持续演进

[§54.4](architecture.md#spec-54-4) 落地为可度量的目标，不是愿景：

- **Awareness 原子化**：每则 awareness 只承担一个可单独安装/替换/学习的注意；验收时能把任意一则 awareness 单独停用而不破坏其他 awareness 或 perception。
- **Perception 多源融合**：主体在时间、环境、社交、通道状态、资源压力等多源感知输入下的判断，能被测量为对单源输入更优或至少不更差；测量走真实的留出场景（held-out），不靠同一 fixture 自证。
- **态度驱动行为**：主体的态度记录能可观察地影响其注意力分配、任务排序、通道选择或遗忘决策；验收时改变一条态度，能在后续主体的决策记录中追溯到该变化的理由链。
- **效能可测**：awareness/perception 的效能测量使用真实模型 + 真实 Store + 留出场景，判定阈值在测量前写死，不把「模型没按格式回答」计为「awareness 无效」。

<!-- /migrated-contract: SPEC:55.4 -->

<a id="spec-55-5"></a>
<!-- migrated-contract: SPEC:55.5 -->
## 55.5 同行参考的落地边界

[§54.5](architecture.md#spec-54-5) 的四个参考方向，各自对应一条可验收的落地边界：

- 自修改/可逆（DeepSeek SuperAgent）：版本化根追加节点 + COW 工作区 = 结构可逆；验收为「切换后追加一个与某历史节点内容相同的新根节点，日志不截断」。
- 持续个性/情感（Muse）：持续身份 + 态度一等记录 + personality awareness；验收为「跨重启主体身份与态度连续，不重置为出厂态」。
- 协作/多代理（Claude Duet）：主体-子代理 + ChildIdentity；验收为「单一主体，委派可命名、可接续、可双向」。
- 个人助手/记忆（OpenAI Dot）：Git 记忆 + 原始会话证据 + 跨设备同步；验收为「经历字节不覆盖，按版本可读，跨节点可同步声明」。

<!-- /migrated-contract: SPEC:55.5 -->

<a id="spec-57-13"></a>
<!-- migrated-contract: SPEC:57.13 -->
## 57.13 验收

| AW10 | 安装中恰好一个启用单元提供 `cap.subject-frame` | 该单元当前正文成为主体角色前缀；无提供者时使用编译回退文本；停用单元不参与解析 |
| AW11 | 安装中恰好一个启用单元提供 `cap.idle-perception` | 空闲时主体收到的观察取自该单元正文；无提供者时使用编译回退句 |
| AW12 | 两个不同单元同时提供同一能力契约 | 报告歧义，harness 不选择其一，编译回退继续生效；歧义对主体可见 |
| AW13 | 主体未处置某条输入 | 该输入被记为『未处置』（ignored + 理由），不被 harness 记成已处理；主体显式 deferred 的输入继续留在队列里 |
| AW14 | 主体对某来源声明注意力暂停并给出 review_after | 该来源不进入当前窗口；事实仍在账本；到期复审结果交回主体 |
| AW15 | 安装中恰好一个启用单元提供 `cap.child-role-frame` | 该单元当前正文成为 child 角色前缀；无提供者时使用编译回退合同；停用单元不参与解析 |
| RT17 | 出站正文含 `@<id>`、`[quote:<id>]` 与 Markdown 标记 | 插件把它们翻译成平台段；正文不再把 `**`、标题符与代码围栏原样发给用户；不翻译时退回单段纯文本 |
| RT18 | 平台收到一条撤回或成员变动事件 | 事件作为观察进入主体输入序列，带来源与被撤回消息标识；主体不再引用已不存在的消息 |
| RT19 | 某通道声明 edit/retract/typing 能力 | 能力表只列 describe 实际能产出的动作；不可达的动作不计入能力 |
| RT20 | 真实流式调用 | chunk 先落盘再广播，控制行不外泄，最终正文与已提交内容一致；未接通生产者时界面诚实报告流式不可用 |
| PG06 | 主体经原生任务创建并迭代一件作品 | 作品版本、状态与事件按既有闭包入账，事件进入同一 inbox，回滚保留较新状态 |
| PG07 | 主体或用户在时间线内操作一件作品 | 作品可打开并操作，动作绑定 revision 与请求标识，重复提交不产生第二次效果 |
| UI-S11 | 主体提交一段 `shenko-ui` 动态文档 | 界面按能力渲染真实组件；降级路径保持同一语义与动作身份，不出现 JSON 调试视图 |
| UI-S12 | 系统偏好浅色 | 首屏不出现暗色闪烁；样式来自单一来源，构建产物与源无偏差 |
| UI-S13 | 时间线出现新的已提交内容 | 屏幕阅读器得到一次有界宣布，不逐字重读整段回答 |
| UI-S14 | 时间线同时出现正文、作品与其动作 | 三者保持稳定身份与阅读锚点，展开与更新不互相打断 |
| G57 | 主体需要对话、资源政策或运行政策能力 | 接口不再因缺派发器而静默 blocked；缺口以可安装能力的诚实记录出现并可被主体看见 |
| G58 | 主体修订自己的心智框架或回滚 | 修订留下带理由的态度记录；回滚按只增版本链执行，旧字节不覆盖，外部副作用不撤回 |
| G59 | 同一能力契约由两个单元提供 | 两者可共存；主体可在运行时切换提供者而不重启，使用方不感知切换 |
| G60 | 复核本轮全部新增机制 | 没有新增人工审批、产品 allowlist 或静态 hard deny；真实约束按事实记录 |

<!-- /migrated-contract: SPEC:57.13 -->

<a id="spec-57-14"></a>
<!-- migrated-contract: SPEC:57.14 -->
## 57.14 实现状态与证据边界

本章的合同按"已实现 / 已打样 / 待做"分开记账，不把方向写成成就：

- **已打样（本轮亲手完成并验证）**
  - **A 心智框架**：`cap.subject-frame` 与 `cap.idle-perception` 的能力解析（`shenko-awareness-core/src/cognition.rs` 的 `capability_unit`、`CapabilityUnit`）、主体对 frame 与空闲观察的解析（`shenko-subject/src/loop_runner.rs` 的 `subject_frame`/`idle_perception`、`shenko-subject/src/lib.rs` 的 `subject_prompt_with_frame`）、两个种子单元 `subject.frame` 与 `subject.idle` 随种子安装（`shenko-awareness-core/seed-awareness/`）、四条判定用例（`shenko-subject/src/frame_tests.rs`）。种子正文逐字等于今天的编译文本，因此行为不变；变的是"这些字归谁"。
  - **B QQ 表面语法与平台事件**（`crates/shenko-channel-napcat/src/outbound.rs`、`channel.rs` 的 `classify_notice`）：行首 `@<qq>` 与 `[quote:<message_id>]` 翻译为真正的 OneBot 段，Markdown 降级为 QQ 可读文本；没有标记的正文仍是一个纯文本段，与改动前逐字节相同。撤回/入群/离开/戳一戳被分类为观察并进入主体输入序列，重投仍是同一条，未知 notice 仍不分类。十一条新判定用例通过。
  - **C 流式接线**：`shenko-subject/src/stream_journal.rs`（store 支撑的 [相关领域合同](durable-state.md#content-stream-journal) 日志）、`shenko-ui/src/stream.rs`（读者表面）、`shenko-broker` 的 `submit_stream_recorded` 与带 stream/attempt 身份的 chunk、`bin/shenko` 的接线。chunk 先落盘再广播，控制行不外泄，committed-only 发布不变；没有接线时仍诚实报告流式不可用。broker 层判定用例证明读者表面收到的是公开正文、不是控制行，且流式调用仍然是**被记录**的调用。
  - **界面样式同步**：`crates/shenko-ui/src/static/app.css` 与 `web/src/app.css` 逐字一致（构建脚本的 copy 步骤）。此前发布版仍在用旧的类选择器，而已发布的 `app.js` 输出的是 `data-pui-style~="shenko-status-*"` 属性，传输状态芯片在线上没有样式，动态文档样式整段缺失。
  - **界面的一次真实构建**（[§57.7](web-interface.md#spec-57-7)）：此前无法构建，是因为构建需要钉住的 Proto-UI 上游源码与它的依赖。现在上游按 `upstream-pin.json` 的钉住提交（971cd35a，lock 文件 sha256 与钉住一致）检出并构建，`pnpm install` 走镜像，真实构建产出 9 个哈希资源；两次构建的产物逐字节相同（只有 manifest 的 `built_at` 不同）。**rich 文档不再渲染成 JSON 调试视图**：文档节点树按其种类渲染成真实组件（section/text/markdown/code/table/compare/select/form/chart/media/actions），未知的种类被点名并保留自己的文字，而不是把 JSON 倒给读者；动作仍走文档自己的 action identity。**浅色不再闪暗**：主题在第一帧之前决定（`<head>` 里的内联脚本按 查询参数 → 存储的选择 → 系统偏好 的顺序定下 `data-theme`），没有选择过时跟随系统的 `prefers-color-scheme`。**新提交的内容只被宣布一次**：时间线出现新的已提交内容时，屏幕阅读器得到一次有界的宣布（"第 N 轮已提交内容"），不逐字重读整段回答。判定：web `tsc --noEmit` 通过、21 条 web 用例通过（新增 5 条 rich 渲染用例）、真实构建通过；构建产物与源不再有偏差。
  - **D 主体不再被 harness 代为处置**（[§57.3](subject-runtime.md#spec-57-3)）：主体没处置的输入记为『未处置』（`Ignored` + 理由），不再被记成"已处理"；主体显式 `deferred` 的输入继续留在队列里；注意力暂停真正生效——被暂停的来源不进入当前窗口，事实保留，`review_after` 到期自动复审并把事实放回窗口。
  - **H 主体有自己的手**（[§57.6](configuration-playground.md#spec-57-6)）：新增原生子任务 `playground.manage:1`——主体可以创建作品、写入新内容、把当前版本作为候选、采纳已暂存的候选、回退到某个旧修订、列出自己的作品。每一个动作都走 Web UI 用的同一套耐久生命周期（版本内容寻址、暂存是候选、采纳是显式姿态、回退是**追加**新版本而不是抹掉旧的）；主体与 Web UI 共用一个 Playground 实例，所以主体写的版本就是人看到的版本。契约不许的请求在动手前就被拒；没有可采纳的候选时如实说没有。
  - **child 角色框归主体**（[§57.10](self-update.md#spec-57-10)）：孩子原本只有一条尾巴可写——角色合同 `model_child.rs` 的 `ROLE_CONTRACT` 是编译期常量，主体说"派一个不同的人"却改不了它。现在提供 `cap.child-role-frame` 的单元正文就是孩子的角色前缀；种子单元 `subject.child-frame` 的正文逐字等于今天的编译合同，因此行为零漂移，变的是"这些字归谁"。没有提供者时用编译回退；两个提供者时报告歧义而不替主体选择。协议边界（工具目录、回复授权、预算）仍在 harness 手里。
  - **G 注意力预算归主体**（[§57.3](subject-runtime.md#spec-57-3)）：每步读多少输入、空等多久，原先是 harness 的常量（64 事件 / 80 ms）；现在由提供 `cap.attention-policy` 的单元决定，harness 只留三条界（批次上限、等待下限、等待上限）。种子单元 `subject.attention` 的取值与今天的常量相同，因此行为零漂移；主体修订它即修订自己的注意力形状。
  - **E 政策请求可见**（[§57.8](subject-runtime.md#spec-57-8)）：新增 `[policy]` 控制行与 `unbound_subject_intents` 读取器；主体对自身运行政策（算力份额／自更新／联网）的请求成为耐久事实，理由说清"能力未安装"，并在主体下一步的材料里可见。
  - **F 通道能力可达**（[§57.4](channels.md#spec-57-4)）：官方 QQ 与飞书适配器此前声明 edit/retract/typing 却只有 `describe` 产得出 Send。现在真正可达的动作是撤回——主体指名**会话**，插件指向本账号在该会话里最后一条自己发出的消息（平台按消息 id 撤回，而 bot 只能动自己的）；发送成功后记录消息 id 与受众，缓存以平台自己的窗口为界，窗口外不再承诺可撤回；没有可撤回对象时诚实拒绝；超过平台自己的撤回时限（官方 QQ 为 2 分钟）时带着平台的规则拒绝，而不是发一个注定失败的请求。**编辑**：官方 QQ 的接口索引里没有任何消息编辑端点，因此这个动作此前是"代码有、文档没有"的假能力，按 RT19 删除（动作、合同摘要、API 方法一并删除，收到编辑派发时按平台事实拒绝）；飞书的编辑端点真实存在，保留。`typing` 按可达事实从能力表里去掉：一次回复就是一个效果，打字要么取代答案、要么在答案之后到达（那时它什么也没说），理由写进能力表说明。iLink 的能力表本来就诚实，未改。
  - **官方 QQ 自己的表面语法**（[§57.4](channels.md#spec-57-4)）：插件拥有本平台的行内语法——`@<member_openid>` 变成平台自己的 @ 标记（群场景 `<@openid>`、频道场景 `<@!id>`，两个平台写法不同），`[quote:<id>]` 变成平台的 `message_reference`，正文带 Markdown 时按平台的消息类型走（单聊与群聊已开放，频道仍需内邀 → 降级为纯文本；代码围栏平台不支持 → 降级为纯文本行）。没有标记的正文仍是一条普通文本消息，与改动前逐字节相同。逐项对照见 `docs/evidence/qq-official-capability-audit-20261110.md`。
  - **读者也能打开并操作作品**（[§57.6](configuration-playground.md#spec-57-6) / PG07）：作品此前只有主体这一条路，人看到的只是一份只读清单。现在**动作绑定 revision 与请求标识**：暂存与采纳都带着请求方看到的那个 revision（作品往前走时那次动作被拒绝，而不是作用在"现在 whatever"上），请求标识随耐久事实走（状态变更事件的 payload），重复提交是同一次请求而不是第二次动作（作品自己的重放语义保证不写出第二次转换），回滚绑定源修订且只**追加**新版本。界面上：时间线带上主体的作品（只带最近几件 —— 时间线是主体的故事，不是作品管理大屏），点"打开作品"在详情面板里看到内容、修订与候选，面板的三个动作都带着面板当时看到的那个 revision。
  - **能力契约优先于包名**（[§57.9](cognition-units.md#spec-57-9) / G59）：同一能力契约由两个单元提供时，两者可共存（歧义对主体可见，harness 不替主体挑）；主体可在运行时切换提供者而不重启 —— 停用一个单元是一条耐久事实，同一个消费方（主体的角色框架）下一次解析就拿到另一个提供者的正文，重开安装后也一样；使用方不感知切换（同一个调用、同一个解析口）。判定：`the_subject_switches_between_two_providers_without_a_restart`（并存 → 主体停用一个 → 另一个提供者的正文生效 → 重开后仍然如此）。
- **能力表是插件合同的一部分**（[§57.4](channels.md#spec-57-4) / RT19）：`ChannelCapabilities` 从"某个 adapter 家族自己的类型"搬进插件合同（`shenko-types::capabilities`），`ChannelPlugin` 新增 `capabilities()` 与 `account_scope()`。每个已安装通道只声明一次自己的真实能力，运行时按注册表统一记录，不再由宿主为某几个平台各写一份。**NapCat 现在也有能力表**：它只调用 `send_group_msg` 与 `send_private_msg`，所以既不承诺编辑也不承诺撤回；QQ 不渲染富文本（正文是纯文本，Markdown 标记在发送前降级），因此 `rich_text` 为 false 而不是 true——平台能做与插件能产出是两件事。声明同时进入主体的自省面（`self://runtime/capabilities` 的"已安装通道的真实能力"），主体读的是插件自己的声明，而不是产品功能清单。
- **待做**：这些按 [§57.13](verification.md#spec-57-13) 的编号逐条推进，每条以实现证据判定，不以"代码存在"判定。
- **证据边界**：本章的验收项是确定性记账与真实消费者行为（真实 Store、真实安装权威、真实平台段格式、真实浏览器），不是模型效能。模型效能按 [§55.4](verification.md#spec-55-4) 用真实模型 + 留出场景另测；"框架不再写死主体的话"不等于"主体因此更聪明"，后者需要独立测量。

<!-- /migrated-contract: SPEC:57.14 -->
