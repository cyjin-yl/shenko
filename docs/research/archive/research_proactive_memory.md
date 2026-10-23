# Proactive Memory Agent：论文、代码核验与产品规格建议

状态：内部研究笔记，未进入开发。核验时间为 **2026-10-22 UTC**（clock 返回 18:21:13）。本文把论文事实、代码事实、我们的设计建议分别标明。用户未回答的架构选择继续属于未确认假设；引用这篇论文不代表用户同意复制其默认模型、触发频率或权限边界。

## 1. 给完整 SPEC 的结论

建议吸收“保存执行状态，并在相关时刻选择性提醒”的机制，作为**有界 memory worker 子任务**，服务于唯一主体 loop 及执行子 Agent。它不应获得第二个持续人格或独立长期目标，也不应借 memory injection 修改核心协议、绕过全局模型 broker 或让主体获得文件能力。

不能把本研究包装成现成的廉价本地记忆方案：论文的主要增益来自 Opus 4.6 memory 模型；公开代码每次 memory 触发实际进行两次串行模型调用，silent 也已经支付这些调用。作者公开代码还存在注释、默认值与实际路径不一致，必须按运行路径解释。

主表与消融表有真实差异，PDF 和 HTML 都如此。不能笼统写“selective 在所有指标胜过 always-on”。我们的 event-driven 触发、worker 权限、不可删除历史、Git/SQLite 双层持久化、稳定 system 前缀、统一 broker 与 KV 管理都是产品设计建议，尚未由该论文验证。

## 2. 一手来源与固定版本

| ID | 来源 | 固定标识与核验范围 |
|---|---|---|
| PM-P1 | [arXiv metadata](https://arxiv.org/abs/2607.08716v1) | v1，提交 2026-07-09 17:26:28 UTC；题名与八名作者可核验；web ref `turn63view0` |
| PM-P2 | [arXiv PDF](https://arxiv.org/pdf/2607.08716v1) | 12 页，Meta AI 署名；首页 Date 为 July 10, 2026；方法 §3、实验 §4、表 1/2/4；refs `turn61view2`, `turn62view0`, `turn63view1` |
| PM-P3 | [arXiv HTML](https://arxiv.org/html/2607.08716v1) | 页首 v1 09 Jul 2026，但 date 脚注 August 24, 2026；refs `turn61view0`, `turn61view1`, `turn64view0` |
| PM-C0 | [作者仓库固定 commit](https://github.com/yifannnwu/proactive-memory-agent/tree/89e5c0d6aadfe531a1aee42fd290d48be89973dd) | main 核验为 `89e5c0d6aadfe531a1aee42fd290d48be89973dd`，2026-07-14 04:48:04 UTC，initial commit；GitHub 插件 API 取得，tree 非截断 |
| PM-C1 | [README](https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/README.md) | Python 3.12+、LiteLLM、Harbor/Enroot 的 Terminal-Bench 路径；README 标示 Apache-2.0；存在五组演示轨迹 |
| PM-C2 | [memory_agent.py](https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/src/memory_agent/memory/memory_agent.py) | 两个 phase 的 prompt、模型调用与工具执行；本地读取定位 137–208、246–258、269–330、353–400 |
| PM-C3 | [memory_enabled_agent.py](https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/src/memory_agent/memory_enabled_agent.py) | 最近 step window、触发、one-shot pending、system/user 注入分支、episode reset、记录；定位 145–190、198–274、294–325 |
| PM-C4 | [memory_terminalbench.yaml](https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/configs/memory_terminalbench.yaml) | Sonnet 4.5 action / Opus 4.6 memory；interval 1，window 8，user_turn 注入；批量 n_parallel 12 |
| PM-C5 | [config.py](https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/src/memory_agent/config.py) | MemoryConfig 默认 system_prompt；status 每 episode reset，knowledge/procedural 默认不 reset |
| PM-C6 | [trigger.py](https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/src/memory_agent/memory/trigger.py) | first step + 固定 interval，默认 1；无已发布的学习式 event trigger |
| PM-C7 | [universal_memory.py](https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/src/memory_agent/memory/universal_memory.py) | bank 为内存对象，JSON save/load；delete 是条目移除；BM25；status 不在默认 action 渲染中；不具备本产品要求的 immutable history 语义 |
| PM-C8 | [vendored Chat](https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/external/harbor/src/harbor/llms/chat.py) | Chat.chat 将收到的完整 prompt 追加到内部 user history；解释 user_turn 提醒的后续存留 |
| PM-C9 | [vendored LiteLLM](https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/external/harbor/src/harbor/llms/lite_llm.py) | call 默认空 message_history；将 system + history + user prompt 组成请求；定位 274–323；未看到 memory phase 共用 conversation/slot 的传参 |
| PM-C10 | [runner.py](https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/src/memory_agent/runner.py) | 批量任务 semaphore 是 task-level；不等于本产品跨角色/跨 endpoint 共用设备的全局 inference broker |

父 Agent 引用网页 ref 前应自己 reopen；GitHub 正文通过插件读取，网页 open 此次返回 DisabledError，因此不要拿失败页面当代码证据。可用上表固定 URL，或父 Agent 自己通过 GitHub 插件重新读取。

### 版本差异处理

日期不一致应保留而非推测原因：arXiv submission 为 Jul 9，PDF Date 为 Jul 10，HTML date 为 Aug 24。用 `arXiv:2607.08716v1` 固定学术版本，不擅自解释成另一个已发布修订版。代码 commit 晚于初次提交，不能自动宣称它逐字复现论文所有实验。

## 3. 方法核验：能从论文/公开实现确认什么

论文研究的 behavioral state decay，是已出现的事实/要求/失败诊断不再影响下一决策。它不等同于“历史文件消失”，因此仅增加存储或扩大上下文不构成完整解决方案。[PM-P2/P3]

| 方面 | 已核验设计 | 对本产品的意义 |
|---|---|---|
| 观察 | 任务、近期轨迹、当前 bank | Memory worker 应接收有 scope 的证据窗口；不自动获得整个机器/全部人的历史 |
| Bank | private status、knowledge、procedural | 区分工作进度、事实、尝试及结果；事实需有来源/置信，不把推测当已验证状态 |
| Phase 1 | 四类结构化 bank 操作，由系统执行 | 适合 worker-only writer；主体的 semantic save 与持久化提交分离 |
| Phase 2 | reminder 或 silent，不能改 bank | 适合独立生成 typed MemoryHint；无需给提醒 worker 任意外部执行能力 |
| 注入 | 论文说下一 action 调用的 transient context，基础指令/工具不变 | 这是一项语义承诺；具体 role、history retention、cache 需要我们定义 |
| 触发 | 主实验 first step 后每 step；k=8 | “选择性提醒”不等于“选择性调用 memory 模型”；节流需另行设计和评估 |

公开代码另有两点有用细节：bank 条目超过默认阈值 50 后用 BM25 对 knowledge/procedural 各选 top 20 给 memory 模型；private status 被排除在普通 action bank 渲染外，注释指出直接暴露“完成”判断可能导致提前结束。[PM-C2/C7] 本产品可以继承分层状态思路，但不能由 memory worker 自行宣告全局任务完成，完成状态仍由任务契约、证据与控制器决定。

### 代码与文字描述的关键差异

1. **“一个调用/共享 conversation”注释不等于运行事实。** memory_agent.process 顺序 await Phase 1 与 Phase 2；二者使用不同 system prompt，直接调用同一 LiteLLM 实例，但未传 message_history。该 LiteLLM.call 默认空 history。可确认每次成功触发路径有两次串行请求；不能确认作者注释承诺的共享 KV 或同一多轮 conversation。[PM-C2/C9]
2. **注入默认值并不统一。** 类/MemoryConfig 默认 system_prompt；发布的 TB YAML 选择 user_turn。system 分支把 memory context 作为本次 system 参数；user 分支把它拼到 prompt 末尾。[PM-C3/C4/C5]
3. **pending 清空不等于历史里消失。** user_turn 分支的修改后 prompt 被 Chat.chat 追加为 user 历史，所以未来调用仍可能看到该提醒。论文的 transient 术语与此路径不完全相符；SPEC 应定义请求 overlay 与 durable audit 的分别，不复制这个含混语义。[PM-C3/C8]
4. **论文 k=8 messages；代码 window 是最近 8 个 step records。** 每个 record 可含 analysis、plan、commands、terminal output；不能把 window=8 解读为严格小 token budget。[PM-C3]
5. **语义“私有 status”没有强隔离保证。** Phase 2 prompt 仍能看 bank status；普通 bank renderer 排除 status 并不证明模型不会在自由生成 reminder 中转述它。产品若要禁止某字段注入，应做数据隔离和输出约束，不能只依赖不展示某个 XML section。[PM-C2/C7]
6. **发表实现不是 production persistence。** bank JSON save/load、可变列表删除、episode reset 可以服务研究；它们没有声明事件不可变、并发 merge、crash 原子性、Git publish 与 source ledger 的一致性。[PM-C7]

以上都是静态核验，未运行、复现或基准测试作者代码。

## 4. 结果、消融、训练与证据限度

以下仅摘录决定产品判断的数值；不以表格中的实验成绩作为本产品承诺。[PM-P2 表 1、2、4]

| 结果 | 基线 | Memory | 条件/限定 |
|---|---:|---:|---|
| TB2，Sonnet 4.5 | 37.6% | 45.9% | Opus 4.6 memory；85 paired tasks；89 中排除 4 个环境失败 |
| TB2，Opus 4.6 | 43.5% | 45.9% | 同一 memory 模型；+2.4pp |
| tau2，Sonnet weighted | 55.0% | 61.8% | 表 1；278 tasks，50/114/114；+6.8pp |
| tau2，Opus weighted | 66.2% | 68.7% | +2.5pp；并非每个 domain 都提升 |
| tau2 selective，macro/micro | — | 64.3 / 61.2 | 表 2；retail 57.0，与表 1 的 58.8 不一致 |
| tau2 always-inject，macro/micro | — | 63.5 / 61.5 | micro 高于 selective 0.3pp；macro 低 0.8pp |
| SETA reward，27B base/SFT/GRPO | 0.709 | 0.693 / 0.720 / 0.734 | action 固定 Qwen3.5-122B-A10B；未经训练的 memory 反而降低 reward |
| TB2 trained 27B memory | 37.6% | 41.1% | Qwen action，85 tasks，+3.5pp，论文称早期 partial transfer |

主表与消融表差异同时出现在 PDF 与 HTML，不能归咎于 HTML 排版。作者称 0.3pp micro 差在预期 run variance 内，但所读全文未给出支撑该判断的多 seed 区间或显著性检验。最稳妥表述是“维护 bank 加选择性提醒在该消融的跨 domain macro 上最佳；其 micro 并非最高”。

训练披露：SETA 进行 SFT 与 GRPO；SFT 学习两个 phase；GRPO 针对离线 rollout 标注的 pivot turns，action 模型冻结，TB2 held-out。[PM-P2 §4.5] 固定公开 repo 的完整 tree 未发现 SFT/GRPO 训练入口、训练超参数、模型权重、pivot labeling 实现、tau2 runner 或全套聚合原始结果。不能说训练模型已经可直接下载部署；也不能说仓库不存在别处未链接的材料。

论文未提供足够信息证明：跨月个人记忆可靠性、中文/group identity 准确性、错误记忆污染控制、低端设备开销、统一 broker 下 latency、KV cache 复用率、每任务总 token/cost、长期存储增长、对机器人恶意消息的边界保持、严格等推理预算下优于更强 action baseline。没有这些结果不等于方案无效，只意味着需要产品评估。

## 5. 成本与调度：不能误读 silent

从公开调用路径推导：若执行了 T 次 memory 周期，至少计划 2T 次 memory forward requests（成功路径；不计 retry），另加 action 请求。silent 节省的是注入内容及潜在干扰，不会退回已执行 Phase 1/2 的计算成本。其两个 phase 在代码里串行，默认每 step 路径会增加关键路径等待。[PM-C2/C3]

因此建议定义两种配置，并明确它们不是论文证明等价的实现：

- **研究对照配置**：first step + interval=1、原两 phase，用于复现或等条件比较。
- **产品候选配置（未确认）**：runtime 记录全部轨迹增量；只有合并后的 save/read 意图、上下文整理、handoff、持续重复失败、事实冲突等可配置信号触发有界 memory job；适当周期兜底。触发规则只是计算调度，不承担主体人格或自行决定人生目标。

Memory 推理、embedding、rerank、compaction、身份相关模型、子 Agent 和主 loop 均进入同一 broker。budget 应按物理 endpoint/共享硬件资源组计，而不只按角色。请求使用 `request_id / task_id / role / provider_resource_group / priority / deadline / max_input_tokens / max_output_tokens / cache_affinity`；这些字段为建议协议，未确认具体命名。

不要让主体持有唯一 model permit 等 memory 子 Agent；完成一次模型响应即释放 permit，后续 memory 与主体按优先级/公平性排队。cap=1 下必须仍能完成 save→commit→receipt。连续主体 loop 不能永远抢占最高 priority 饿死维护 worker；可以保留紧急抢占机会、aging 或配额，但算法与参数需要实测。

至少观察：memory 请求数和 silent 比率、phase token、prefill token、cache hit/miss、排队与端到端延迟、提醒 token、后续纠错/无效提醒比例、任务成功率、重复失败减少量。`silent_rate` 单独不是效率或质量成功指标。

## 6. 接入唯一主 loop：worker-only 的能力边界

以下为推荐，遵守已确认 R046–R054，不把 runtime 落盘误写为主体文件权限。

| 角色 | 输入/职责 | 工具边界 | 生命周期 |
|---|---|---|---|
| 主体 loop | 接收外部事件、语义 memory、子任务回执；持续选择下一步 | semantic read/save、handoff；无任意文件/exec/SSH/SQL | 唯一顺序决策流 |
| Memory maintenance worker | 处理 save 意图、增量观察、矛盾/失效条目；维护默认 host-mounted Git memory | 限定 scope 的证据读取、bank mutation 与其 workspace 写权限 | 每个 job 有输入游标、预算与完成条件 |
| Memory reminder worker | 读授权 bank 快照与近期轨迹，输出 hint 或 silent | 只读 memory 与 typed output；无需外部操作/发送消息/改协议 | 一次或有限两 phase，结束即归还资源 |
| 执行子 Agent | 接收任务、限定 memory hint，操作自己的 workspace/remote target | 按角色授予 SSH/file/package 等真实执行工具 | 有界任务，可 handoff 子任务 |
| Runtime/controller | 稳定收件箱、ledger、调度、能力校验、提交回执、请求 context assembly | 确定性持久化/索引/授权原语 | 不充当第二个自由思考人格 |

两个 memory 角色可以是一个 worker 的两个 phase，无需两个常驻服务。它们的 private status 是 job/namespace 内的进度，不是一套可脱离主体自主延伸的身份/愿望/人格。允许跨 job 持久 checkpoint，不允许借此启动无限自我循环。

自主 memory 维护可以来自主体发出的长期 delegated service manifest：授权 scope、事件订阅、触发条件、预算、期限/取消条件。Runtime 按 manifest 派发有限任务，memory worker 不能自授无限权限或绕 broker 开自己的后台 LLM 流。

## 7. 记忆提示的注入、权限与 KV 约束

建议 `MemoryHint` 至少包含 target controller/child ID、task/episode epoch、memory version、依据的 source event IDs、事实/推测分类、适用条件、有效期、幂等键、建议短文。Runtime 校验 scope/epoch/版本/时效后，在**下一次未启动的目标模型调用**附上；过期结果不追写已经运行的模型上下文。

提示为证据与观察，不是 system/developer 权限升级。memory 中的引用或工具输出也不成为新权限；身份/管理员/网络访问能力只来自控制面。对不支持独立 memory role 的 provider，通过合法消息角色和清晰来源编码适配，不能伪造不存在的 tool_call 或把外部内容变成核心 system。

Prompt 稳定区包含 core protocol、版本固定的人格（按用户要求放 core 后）及稳定 tool schema。动态状态、消息与 memory hint 在其后。不要每步重写 core/persona 以放入新记忆；人格更新本身是版本化事件而非任意 worker 的操作。

必须明确区分两种语义：

1. **瞬时 overlay**：某次 call manifest 引用 hint，raw ledger 永久记录“当时注入了什么”，后续默认 prompt 不自动包含；需要时再检索。删除本次尾部 overlay 会影响该尾部的 KV 连续性，不能声称全量 cache 始终命中。
2. **持久子任务回执**：作为正常历史 event 追加，可以被后续 context 看见；这有利于前缀复用，但它不是论文意义的“只下一步 transient”。

两个语义都可以支持，但必须在配置、测试与指标中可辨认。不能因为 pending 字段已清空就宣称真正 transient。

主、执行子 Agent 与 memory worker 使用不同会话/slot lease；可共用适用的不可变前缀缓存，但不能混用 private scope。provider 明示支持 slot/sequence affinity 时透传 broker 授予的 lease；否则降级为普通请求并准确报告不能保证 slot 复用。两 phase 若切换 system 或 tools，则必须实测缓存，不根据同一模型对象推断 KV 连续。

## 8. 历史不可删、语义 forget/reset 与默认 Git memory

这些是本产品约束，不能直接采用研究实现的 `memory_delete` 列表删除或 JSON 覆盖来满足。

| 数据层 | 推荐真相/存储角色 | 允许的变动 | 不允许借机发生的变动 |
|---|---|---|---|
| 原始 history/event ledger | SQLite append-only 事件；大附件可内容寻址；永久 source IDs | 补充事件、校正说明、索引重建；runtime 落盘 | compaction、forget、episode reset、sandbox rollback 不得删除已记录历史 |
| FTS5/向量/实体索引 | 可重建派生结构，索引原始内容或显式投影 | 重建、升级、scope 过滤、tombstone 投影 | 将索引命中或摘要当唯一历史真相 |
| 默认文件化 memory | 用户指定的 host-mounted Git 仓库；memory 子 Agent 发布 | 有来源的文档更新、纠正、归档、语义排除 | 偷换成 SQLite-only；回滚沙箱时删除宿主 Git memory |
| Active memory bank | 可变的事实/经验/状态投影 | supersede、invalidate、tombstone、forget 标记 | 修改原始消息冒充“当时没有发生” |
| Context/checkpoint | 为当前模型调用构造的预算化视图 | compact、reset、新 epoch、选择性 recall | 把清空上下文当清空历史 |

**语义 forget**：排除条目参与默认 recall/主动提醒，可保留原因、范围、被替代版本与 source。避免 rebuild 时从旧历史自动“复活”该记忆：forget/tombstone 本身也是高优先级投影事件，并在重建、导入与跨 namespace merge 时持续生效。需要明确指令才重新激活；“可查到旧原文”与“可主动用于当前行为”是两种权限/语义。

**Reset session/episode**：开启新 context epoch，清理临时 status/slot/hints，保留原始历史、身份、授权与持久记忆。Factory sandbox rollback 恢复可执行 generation，不应回滚已接收事件、凭据撤销或已提交 memory；具体回滚包络仍需产品裁决。

**永久删除**：不应成为主体或 worker 的普通工具，因为用户要求历史不可删。若日后人类要求设备退役或抹除数据，那是另一个显式管理流程；本 SPEC 不应偷偷引入 auto-retention 或让 forget 承担物理销毁语义。

并发写入推荐：每个 memory worker 在独立 branch/workspace 工作，基于 bank revision 提交；发布序列化或 CAS 校验，冲突作为事件交给 worker 解决。Runtime journal 记录 intent 与 commit receipt，Git 内文件变化后才能发 `committed`。SQLite ledger 与 Git 不天然是同一个事务，需可重放的 outbox/manifest 协议；崩溃在中间可定位 pending/reconcile 状态。此协议是设计建议，不是论文现成功能。

## 9. Message provider、API provider 与身份插件补充

用户新增“消息 provider / API provider 插件、外部消息注入主体、别名/ID/认识人编号/身份插件、可选声纹”必须独立进入 SPEC，不能被泛称插件系统吞没。

### Message provider

建议统一 envelope：provider 类型与实例 ID、平台/tenant/account、conversation/thread ID、稳定 message ID、sender platform ID、显示名/别名快照、时间戳、reply/mention、附件引用、edit/delete/update 类型、传输认证结果。Display name 不充当稳定主键。

所有外部消息先由 runtime 去重并落 durable inbox，再按会话/到达序号变为主体可读 external/user 事件。它们不会直接成为 system prompt，也不会被平台显示名/群头衔赋予管理权限。主体仍只有一条顺序思考流；多平台并发入站表现为有序 inbox，而非为每条消息复制另一个主 loop。如何控制多群关注优先级是主体策略/配置，未确认默认。

provider 支持发送/回复/附件/编辑等能力声明；外发 side effect 由相应执行子任务或专用受限发送路径完成并记 receipt，不能为了聊天让主 loop 获得万能文件或网络工具。若“主体可直接输出一句话，由 runtime 作为消息发出”被采用，需明定这是模型响应通道而非任意 API 执行能力。

### API/model provider

只负责端点协议、模型能力、token/context 限制、streaming/tool schema、取消、usage、cache/slot extension 等适配。API key 使用 credential handle；provider 注册不等于获得新的并发配额。全部调用先取得 broker admission，重试也重新记账，不能插件内部偷偷新增无限并行。

消息插件与模型 API 插件须为不同接口；前者标识“谁从哪里说了什么”，后者标识“在哪种资源预算下向哪个模型推理”。身份与 memory 插件也不能因为都叫 provider 而共享无边界上下文。

### 身份与熟人编号

建议稳定不复用的 `PersonId`/熟人编号，外加带 scope 的 platform identity links、alias history 与 evidence。UI 可以展示“编号 0042 + 当前别名”，但编号不是数据库自增值直接公开全部内部信息的必要要求。

同名、相似头像、模型记忆说“这是张三”不构成跨平台账户合并证据。允许显式验证链接、人工修正、split/merge 可审计事件；每个别名都记录平台/会话/时间范围。已删除/失效链接不应因旧 memory 被自动恢复。

认识某人、能与其对话、允许使用某个私密 memory scope、拥有管理员权限，是分别建模的关系。默认允许对话仍成立；admin 仍手动设置。群消息进入主体不意味着本群可读其他私聊或机器凭据，私密 memory 需要按来源与目标 scope 过滤；事实检索与授权不是同一操作。

### 可选声纹

声纹插件建议返回候选 `PersonId`、score/校准信息、音频时间段、质量/重叠语音指标及 `unknown/uncertain`；speaker diarization 的“说话人 A/B”与已验证身份不是一回事。不要把未经校准的相似分数标成身份正确概率。

可以使用本地、由人类显式登记的参考样本形成候选匹配；声音身份不自动授予 admin、外部 SSH 或私密数据访问。低质量、多说话人、缺少参考样本时保留未知，不强迫把每段音频归为某个熟人。识别结果作为带来源与置信的事件，用户纠正也成为事件；不静默改写过去谁说了什么。

声纹不是必须开启的核心依赖，模型/模板格式/硬件预算/保存音频与模板期限都应可配置。若用外部识别 API，仍经过显式 provider 配置与同一 privacy/broker 资源契约；极致隐私默认不能因为一个 identity 插件而隐式上传音频。

## 10. 可直接转入 SPEC 的验收门槛

以下为建议验收，具体量化阈值尚未确认；以实质风险为目标，不是要求当前阶段写测试。

1. 主体 loop 无 file/exec/SSH/SQL 能力，仍能 semantic save→worker 写入→versioned receipt→read；runtime crash 恢复 pending 保存意图。
2. 一次 memory job 有明确轨迹游标、scope、budget、终点；没有衍生无限主动人格。唯一主体同时最多一个决策 turn。
3. cap=1 时主体/worker/子 Agent 可推进；所有 phase、重试、embedding 和 compaction 可追踪到 broker 记录，parent wait 不占 permit。
4. Memory silent 不自动修改下次 action 上下文；错误/超时按可配置失败策略报告，不伪造成功的 bank 更新。
5. 一个 hint 在 epoch 改变、task 结束、scope 不匹配或过期后不能注入；来源可点开原始 event，推测标记不会在 merge 后变成 verified fact。
6. 选择 transient 模式时，下一次 call manifest 默认不重复携带旧 hint；durable audit 仍存在；选择 persistent receipt 模式时明确显示其不同语义。
7. 动态 hint 与普通外部消息不能改变 core protocol/tool schema/persona hash；传输 adapter 不能伪造管理员身份。
8. 带恶意命令的记忆文本只能作为数据/引文，不改变目标 Agent capability。worker 不能用 memory.save 的 path/script 字段绕过主体禁令。
9. compact、reset、forget、factory sandbox rollback 之后，原始 history 数量/IDs/来源引用仍完整；forgotten 投影不会被索引重建自动激活。
10. 两个 worker 同时修改同一事实，出现明确 conflict/serialized commit，而不是覆盖对方；崩溃在 Git commit 与 ledger receipt 之间可 reconciliation。
11. 相同显示名的两个平台用户不会自动合并；手动绑定/拆分保留历史；熟人身份不能授予 admin。
12. 可选声纹对未知/模糊样本能返回 uncertain，并且不会仅凭匹配分数授予机器操作权限。
13. 性能报告并列无 memory、显式 retrieve、论文 every-step two-phase、产品 selective-trigger；按相同 action 模型与总推理预算比较，同时报告质量、token、queue/latency、cache/prefill、重复失败、误提醒。
14. 中文/群聊/跨会话测试单列；不能拿英文 terminal benchmark 成绩替代产品的个人长期记忆验收。

## 11. 仍需用户裁决或实测的点

- 是否允许 event-driven/coalesced memory job 为默认，而把每步两 phase 保留为可选配置？这不改变唯一主体持续思考的要求，只改变辅助 memory 的调度。
- 主体持续 thought loop 在 broker 排队/模型故障/断电时如何呈现“持续意图但本次无推理”的状态；不能许诺物理不可能的零间断推理。
- 对必需事实冲突，是否等待 memory 更新，还是主体继续带 stale 标记运行？不同 priority/任务可不同；推荐不一刀切阻塞全部主 loop。
- 默认 memory hint 用 transient overlay，还是允许语义上持久的回执事件？两者 cache/审计/行为效果不同。
- 私有 persona/熟人/跨群 memory 的共享范围、账户绑定责任人、声纹是否启用，均未由第一轮问答确认。
- 原始历史“不可删”的管理例外、存储耗尽与设备退役语义须显式裁决；不能靠自动 GC 假定用户同意删历史。

这些追问不阻止完成带待决标记的完整 SPEC，也不构成现在开始开发的授权。
