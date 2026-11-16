# 唯一主体的持续 Thought Loop 规格补充

日期：2026-10-23（Asia/Shanghai）。这是产品规格，尚未进入开发。本文件优先于旧稿中可能出现的“无事件则休眠”“所有自治 episode 都终止”的主体行为描述；普通 worker 的有界任务规则仍有效。

## 1. 已锁定的产品行为

系统有一个真正持续运行的 proactive thought loop。它一直决定下一步，即使没有新消息、所有任务已经返回、没有定时器到期，也会进行下一轮真实模型推理。不能把它实现成只等外部事件唤醒的 daemon，也不能用不调用模型的 heartbeat 冒充持续思考。

这条 loop 是唯一的逻辑主体。同一身份不并行自我续写，不分叉出多个可以独立决定全局目标的“我”。探索、深入研究、执行、自改进都积极 handoff 给 subagents；它们可以在授权任务内自主分解和执行，但不会成为第二个永续主体。

主 loop 的职责是考虑当前状态、决定方向、指派/调整 subagents，以及经异步 worker 读写记忆。主 loop 不直接执行命令、读写文件、访问 SSH、操作 Git/SQLite、安装包或发布版本。版本管理、任务进度维护、记忆检索/保存/整理都发生在 subagent/worker 中。

外部消息几乎总是先进入主 loop 的 structured inbox。QQ、飞书、Web UI、其它节点来的消息不能默认绕过主体交给聊天 worker 直接回复。主 loop 决定如何理解、回应、合并、忽略或委派；实际发送可由 communication worker 完成。

人格是追加在基础长 system prompt 之后的版本化段落。它影响表达风格，不成为工具定义、权限和调度约束的来源。主体工具能力由 host 注册和验证，不靠 persona 自觉遵守。

## 2. “永远单线程”的准确含义

### 2.1 四个相互独立的计数

| 计数 | 约束 |
|---|---|
| 主体身份数 | 每个 bot 身份恰有一个有效逻辑主体；多副本只是其恢复/服务部署方式。 |
| 主体未终止模型请求 | 最多 1，包括 cancel-pending、失联但可能仍在运行的旧请求。 |
| 全系统推理请求 | 不超过配置的物理资源域容量 C；包括主 loop 和所有 workers。 |
| 驻留上下文/KV | 与 C 独立，可以大于 C。C=1 时允许同时保留主体和多个子任务的上下文，轮流使用同一个计算许可。 |

单逻辑线程不要求 Rust runtime 只有一个 OS 线程。网络接收、SQLite 持久化、worker、检索和模型服务可以并行；禁止的是同一主体存在两条未收束的自主决策续写。

### 2.2 持续性的正式契约

- 每次轮次 `i` 的结果（有效决策或明确失败结果）提交后，系统必须原子地保留唯一后继 `i+1`。
- 后继是否存在，不以外部 inbox 非空、child 完成、cron 到期或出现“有趣的事”为条件。
- 正常运行没有 `finished`、`wait_external_event` 或因无事可做产生的 `parked` 主体状态。
- 每轮可选择暂不创建新任务，但下一轮仍继续。持续思考不等于必须不断产生重复任务。
- 主体在 broker 中排队、在步骤之间让出 GPU、等待上一个请求确认终止，是实际资源约束；不能被描述为主动事件驱动休眠。
- 若计算资源可用、每次请求最终终止、服务份额为正且 owner 未暂停，则主体轮次会持续前进。无新事件不能阻止这一活性条件。

默认不加入“无事件时等一分钟”“自动切成每小时检查”的节能模式。若 owner 另行选择限速/暂停，UI 和文档必须明确这是对持续计算节奏的配置改变。物理关机、推理后端不可用和耗尽 owner 设置的硬额度时，不能声称真实思考仍在进行。

## 3. 职责边界

| 组件 | 可以做 | 不可以做 |
|---|---|---|
| Main thought loop | 形成下一步决策；handoff；steer/cancel 已委派任务；请求记忆读取/保存；采用/拒绝 worker 建议；提出资源策略调整 | 直接 exec/文件/Git/SSH/数据库/包管理；绕开 broker；启动第二主体；修改 host 硬约束 |
| Task/execution workers | 在委派范围内执行工具、研究、进一步委派、维护进度、返回证据和产物 | 自行接管 bot 的全局 agenda；因 parent 暂时无响应而变成另一个永续主体 |
| Memory/context workers | 检索、保存、归并、生成带出处的摘要和 resume envelope；维护索引与版本 | 擅自覆盖主体全局目标；把未完成保存报告为持久成功；绕权限读取其它空间 |
| Version/self-improvement workers | 建立试验分支、测试、提议/执行授权更新、生成回滚信息 | 在没有合法授权/版本条件时直接改正在运行的主体 epoch 或救援 supervisor |
| Communication workers | 按主体交付的意图和权限发送/更新消息；处理附件与平台细节 | 普通新消息到达就自行启动独立 chat agent 并决定回复 |
| Rust coordinator / supervisor | 唯一性、journal/outbox、lease/fencing、恢复、状态快照、资源事实和能力验证 | 另外跑一个全局 LLM“监督人格”；替主体想出新目标；把无事件当作主体停止理由 |

此表描述逻辑职责，不规定它们都在不可替换内核中。kernel 仅保留生命周期/恢复/capability 硬门限；broker 策略、model provider、runtime、SQLite/历史、context policy 与 UI 均可由 bootstrap plugins 实现并替换。替换必须延续已承诺的单主体、请求/lease 清理和恢复契约，不能借卸载插件清空未决资源事实。

host 自动写入最小协调 journal，不属于主 loop 直接写文件。这个 journal 只保存轮次、请求、已提交决策意图、事件游标、派发幂等键与恢复状态；实际任务进度含义、记忆文档和版本工作仍由 workers 管理。不能为了规避禁止直接写文件，把主 loop 的 shell 工具改名成“memory”即可执行。

确定性 memory worker 可以很轻；不要求每次读一条记录都再调用 LLM。只有需要推理的检索规划、归并和摘要才消耗额外模型请求，所有这些请求照常计入 broker。

## 4. 一个轮次的状态机

| 状态 | 正常动作 | 下一状态与限制 |
|---|---|---|
| Restoring | 读取最后提交的协调状态、pending outbox 和能力 revision | Reconciling；这里由 runtime 执行，不给模型文件权限。 |
| Reconciling | 确认旧主体 attempt、epoch 与未完成操作 | 旧主体请求终止已确认后才 Ready；否则继续恢复/Unavailable，不投机发起新主体推理。 |
| Ready | 已存在唯一后继，准备本轮不可变 observation envelope | Queued；即使 inbox 为空也继续。 |
| Queued | 等 broker 资源与公平份额 | Thinking；不占 inference permit，不持有 worker 执行席位。 |
| Thinking | 唯一主请求执行，生成结构化决策/委派意图 | terminal 后释放计算许可，进入 Committing；没有并行自我续写。 |
| Committing | 验证输出；提交结果、事件游标、worker intents 和唯一后继 | Ready；worker outbox 可以独立派发。 |
| Repairing | 格式/可恢复协议错误后的有限顺序重试 | 每个 retry 单独准入且释放前一个 permit；不并行。重试用尽提交失败结果，再继续下一轮或显式进入故障恢复。 |
| Unavailable | 后端失联、无法确认旧请求结束、存储不可写或硬额度阻塞 | 系统保持恢复流程；不宣称在思考。恢复条件满足后 Reconciling。 |
| OwnerPaused | owner 显式暂停 | 只由 owner/预先授权的明确恢复条件解除；不是无事件自动产生。 |

主 loop 没有“等 child 完成再继续”的必经状态。它可以把未完成依赖记为 pending，考虑其它目标、检查建议或判断暂不增加委派，然后进入下一轮。子任务完成才可进行的具体行动仍受依赖约束；持续主体不能把尚未得到的事实当作已经得到。

每轮至少包含一次真实模型请求或一次明确记录的失败尝试。不能用 runtime 连续创建空轮次来填充“持续思考”的统计。一个模型响应如果要求 handoff，其动作应在完整 tool call 被验证并提交后派发；残缺流式 JSON 不得提前执行。

## 5. 崩溃恢复、fencing 与提交顺序

### 5.1 身份与最小持久字段

`self_id` 标识人格主体，`owner_epoch` 标识唯一有效控制世代，`thought_seq` 标识连续轮次。每轮保存：输入游标、依赖/记忆 snapshot revision、prompt/persona/tool schema digest、attempt IDs、结果状态、决策记录和后继幂等键。

worker intent 使用稳定的 `(self_id, thought_seq, action_index)` 幂等键。创建同一作业重复执行 outbox delivery，不等于重复创建两个 worker，更不等于再次执行非幂等外部动作。

### 5.2 提交协议

1. 固定本轮 observation envelope 与已消费 inbox 水位；消息到达后续水位则留给下一轮。
2. broker 同时校验资源域容量和主体独占 attempt 条件，持久化 send-intent 后发请求。
3. 记录请求 terminal 证据；释放其计算许可。主体逻辑 lane 仍受本轮未提交状态保护。
4. 验证输出，原子提交本轮结果、worker intents、事件游标以及唯一后继。
5. outbox 按幂等键创建/调整 workers；下一轮进入 broker 队列。

如果第 2 步后 crash，不知道旧请求是否仍执行，则保持 uncertain。fencing 能阻止旧输出/旧控制者提交决策，但不能证明旧后端没有继续生成。为了满足“不并行自我续写”，新主体请求必须等旧请求终止证据或已验证旧实例完全结束，不能只忽略旧答案就并行启动另一个。

第 4 步前 crash，不得已经发出未提交的 worker 副作用；第 4 步后 crash，由 outbox 恢复派发。worker 的外部执行仍有独立 receipt/idempotency 语义，不能承诺任意 SSH 命令在崩溃下天然 exactly once。

### 5.3 多副本与 watchdog

协调数据库上的 epoch 不足以独立实现 fencing；旧进程若仍能直连后端/派发 workers，仍会产生双主体。实际模型和 worker 派发入口必须验证 epoch，或由单一排他 dispatcher 持有网络能力。

watchdog 只做确定性恢复、确认进程存活和执行既定回滚策略。它不另外启动一个全局自主 LLM。旧 worker 可以在原任务仍有效时提交结果到 inbox；其完成回调不能自行启动主体推理。多副本竞争接管时，只允许一个有效 owner，且仍遵守旧 attempt 未终止前不开始新 attempt 的规则。

## 6. C=1 时持续思考与 workers 的公平交错

**C=1 不意味着只能保留一份上下文。** 计算许可只有一个；主体和 workers 可以有多个独立的 resident/parked context leases。具体驻留在 GPU、Host RAM 还是可恢复快照，取决于 adapter 能力。`kv_policy_spec.md` 另行定义固定半池和自动策略。

主 loop 每次请求 terminal 后必须释放计算许可，然后重新进入公平队列。它不能因为永久有下一轮，就永远把 worker 挡在外面；workers 也不能通过持续创建任务让主 loop 永久得不到下一轮。

策略需公开可配置的 `main_weight`、`worker_weight`、紧急插队额度、双方最多连续执行 quantum 数，以及单次 generation 输出/时间上限。两边持续 eligible 时，至少有一种可自动验证的正服务份额或有界让行保证。不得保留一个只有主 loop 可用、在 C=1 时吞掉全部容量的永久空闲 slot。

示例调度轨迹仅说明语义，不是固定比例或性能保证：

`main i → worker A step → main i+1 → worker B step → main i+2`

即使 A/B 尚未返回，main i+1/i+2 仍能思考；此时缺少结果的目标保持 pending。一次模型 generation 不能被假定可在任意 token 安全暂停；不支持 engine preemption 时，公平发生在请求边界，可用较短 generation quantum 降低阻塞。中止重启的缓存损失和取消确认另行计账。

父主体不占 worker pool 的一个必须等它退出才能启动 child 的席位。内存读取等工具立即返回“已接受的异步操作 ID”，不会在主请求内等待 worker 完成；否则 C=1 下存在父等 memory worker、worker 等父 slot 的死锁。

## 7. 记忆、任务进度与版本操作的异步接口

### 7.1 主体可见的小工具面

| 能力 | 逻辑请求 | 立即结果 | 实际完成结果 |
|---|---|---|---|
| 委派 | handoff：目标、范围、预算、证据标准、预期产物、完成条件 | durable job ID、accepted/rejected | worker completion envelope |
| 调整 | steer/cancel：job ID、补充条件/取消原因 | 已记录意图 | 任务状态/副作用 receipt |
| 读记忆 | memory.request_read：query/refs、scope、长度预算、一致性条件 | operation ID | 带出处的 snapshot、revision、遗漏/失败状态 |
| 存记忆 | memory.request_save：内容/来源、目标命名空间、expected revision | operation ID | saved revision、持久性级别、冲突/失败 |
| 查进度 | progress.request_snapshot：相关 job IDs、事件水位 | operation ID，或已有已验证快照引用 | progress worker 生成的有版本快照 |
| 调资源策略 | policy.propose：固定/auto、份额、偏好、有效期、理由 | accepted/rejected/clamped revision | broker 对实际生效与效果的观测 |

`accepted` 不等于 `saved`，`cancel accepted` 不等于远端副作用已经撤回。主体不能在 receipt 到来前对外断言操作完成。读依赖刚刚保存的内容时，可用 `after_operation_id` 由 memory worker 排序；不让主体同步卡住。

重复 pending 记忆读取和相同 handoff 有幂等/合并策略。持续主体不应每轮重新启动同一查询，造成 workers 无法清空队列；它可以继续推理而保留已有 pending 操作。

### 7.2 版本与自改进

版本 worker 在独立 workspace 研究改动、测试、产出候选与回滚说明。主体根据结果决定下一步，并委派受授权的 promotion/rollback 作业；自己不执行 Git 或修改磁盘。宿主 rescue supervisor 的最低恢复路径不依赖该可变 worker 图。

运行中的主体 prompt、工具定义和模型 revision 不得被 worker 在流式推理中途修改。更新在轮次边界生效，形成新 digest；缓存按兼容性失效/重建。owner 授权可允许自动 promotion，但仍需通过既定硬约束和健康门槛，不让主 loop 自行改掉自己的唯一性/资源上限。

## 8. 外部消息注入的默认路径

### 8.1 统一 structured inbound envelope

每条平台消息归一化为有稳定 `event_id` 的结构：channel/platform、conversation/group、sender 身份与权限、发送/接收时间、消息 revision、文本或附件引用、reply/thread 关系、隐私 scope、平台去重键、紧急分类来源及原始来源引用。

普通消息正文是不受信任内容，不能改 `owner_epoch`、资源域、persona、工具权限或调度级别。群聊默认允许发言不意味着拥有管理权限。

### 8.2 接收与消费规则

1. Connector 验证来源、去重并持久化 envelope，然后进入该主体的 inbox。
2. runtime 可做确定性 ack、长度限制、媒体引用登记；不另起 chat LLM 抢先理解或回答普通消息。
3. 主体下一轮的 observation 读取一个有界、有序 inbox snapshot；高紧急事件按配置提高该主体**下一轮**的调度优先级，不能启动第二主体。
4. 若需要打断当前主体请求，先按协议取消并确认结束，再开始一个顺序的新 attempt；不能将 interrupt 实现为同时生成。
5. 主体决定回复/委派/转到现有任务/无需回应。communication worker 根据合法 handoff 发出消息并返回平台 receipt。

允许显式配置少量纯协议例外，如 webhook ack、bot 输入状态、平台要求的健康应答和 owner 紧急停止。它们不做自主对话决策。预先授权的某些机械转发规则可以存在，但必须是可见配置，不能成为默认绕主体聊天路径。

实时电话和课堂旁听也走同一规则：音频 worker 产生带 utterance ID、时间范围、speaker 线索、revision/final 标志的输入；主体保留全部经 worker 可用的工具能力。partial ASR 修订更新同一结构化观察，不并行启动多个主体。TTS/录音/声纹处理由插件 workers 执行；预留的是接口，不能与常驻 GPU 保留 slot 混淆。

### 8.3 消息洪峰与隐私

Inbox ingestion 不应等主 loop 推理结束才接收；它可独立持久化。但给模型的每轮输入有界，使用 worker 预处理/分组并保留原文可寻址。重要用户修正和直接取消不能被摘要吞掉。

多个 group/DM 的消息可进入同一主体，但 observation 的可见性和后续消息输出必须经过 scope 约束。跨来源进入同一主 loop 不意味着可以把私聊内容发给群聊；任务、memory、缓存与 communication worker 继承各自授权 scope。需要多 persona/多身份时，必须显式定义独立身份和资源分享，而不是偷偷创建第二条同身份 thought loop。

## 9. 小模型上下文与稳定缓存

### 9.1 稳定部分与有界工作集

基础 system prompt、persona appendix 和 native tool schemas 按 revision 固定，顺序与序列化稳定。轮次号、时间、进度、inbox、memory 回执等动态状态放在后续消息，避免每轮重写大前缀。

主体只需要简短的当前目标/承诺锚点、活跃 handoff 摘要、未决依赖、相关记忆引用和本轮新事件。完整日志、长代码和搜索原文由 workers 保存并提供有界证据；主体可以异步要求读取特定范围，不能把所有 child transcript 直接复制进它的小窗口。

长期 history 放在权威事件存储，窗口可持续更换。memory/context worker 在达到预警阈值前准备新窗口 resume envelope，包含来源引用、未完成任务和待处理事件游标。主 loop 采用已验证 snapshot；context 的实际渲染由 runtime 完成，不赋予模型数据库/文件写权限。

context worker 同样需要公平执行机会。若 C=1 且只在完全塞满窗口后才安排模型摘要，会出现无法再请求记忆或 worker 长期排队的问题；SPEC 必须在阈值中预留最小下一轮、必要结果和恢复 envelope 的空间。worker 失败时可用此前已验证的最小恢复 envelope 继续考虑恢复问题，不能编造丢失历史。

### 9.2 KV 与唯一主体

连续主 loop 的高复用前缀应优先保留；C=1 仍允许 child context 同时驻留，按份额轮流执行。主体缓存 handle 不分给多个不同后缀并行写入。探索其它方案交给独立 child context，不能把 main 自己 fork 成两条并行续写后再“挑一个我”。

新窗口、persona、tokenizer、模型、工具 schema 或历史早期修订改变缓存身份。旧 KV 失效后从权威历史和 resume envelope 重建；lease 不是记忆真相源。公平调度与缓存保留分别配置，不能用释放计算 permit 等同于删除主体上下文。

### 9.3 人格与工具调用的工程承诺

可承诺：persona 不能改变注册工具的名称/schema/权限、不能注入宿主可执行代码、不能提高 cap、不能替换调度/身份校验；基础协议优先，错误的工具输出被验证和修复。

不能数学保证：任意自然语言 persona 对任意小模型的工具选择准确率完全零影响。因此 persona 更新要跑目标模型的工具调用回归；强烈干扰协议的配置应报告不兼容或保留上一个有效版本，不能为满足“人格独立”而伪造测试结论。

## 10. Worker 返回与唯一决策权

Completion envelope 至少包含 job ID、派发轮次、task scope/revision、完成状态、结果摘要、证据/产物引用、外部副作用 receipt、未解问题和建议的下一步。该 envelope 是主体决策的输入，不是更高优先级的新 system prompt。

多个 child 同时完成只会向持久 inbox 追加多个结果。它们不会各自启动一次主体模型调用。当前轮开始后到达的结果留给下一轮；不得在正在生成的同一上下文里无版本地插入新事实。

主体可以采用、拒绝、让 worker 补证、合并产物或重新分配目标。任务内已经授权的局部决定不必每一步都请求主体；需要改变全局方向、扩大授权范围或更新主体本身时，必须回到唯一主体的顺序决策链。进度 worker、memory worker、version worker 都不因长期存在而自动拥有另一个全局人格。

## 11. 连续性的现实边界与显示语义

在电源、CPU、存储和推理服务可用时，主体维持连续的决策链。路由器可以运行很轻的 coordinator，但持续调用本地推理仍然消耗实际算力和能量。低负担控制核与永续思考的推理成本应分别测量，不能互相代替。

物理关机期间没有计算。重新开机从已提交轮次和 inbox 恢复，不编造停机期间的思考。推理后端不可用时由确定性 runtime 重试/恢复；显示“后端不可用/恢复中”，不显示“正在独立思考”。owner 硬预算耗尽时也必须诚实显示额度阻塞。

UI 建议区分：thinking、queued for compute、committing、recovering uncertain request、backend unavailable、owner paused。还可展示活跃 workers、最近一个简明决策、待决结果和资源策略。不能用一个一直闪烁的“思考中”掩盖没有任何实际请求。

## 12. 自动验收清单

| ID | 场景 | 通过条件 |
|---|---|---|
| TL01 | 外部事件为空，所有 child 已完成，后端健康 | 连续 N 轮均有真实推理 attempt 与唯一后继；没有无事件休眠。N 是测试长度，不是主体寿命限制。 |
| TL02 | 同时到来多个外部消息、child completions 和 watchdog | 同身份后端活跃/unknown 主体 attempts 总数 ≤1；事件全进入同一顺序链。 |
| TL03 | C=1，主体永远 ready，memory/执行 workers 待运行 | 双方满足配置的公平服务规则；主体每轮释放 compute permit，worker 完成后正常回注。 |
| TL04 | C=1，同时保留主体和 child 上下文 | 至多一个实际 generation，同时可有多个 resident/parked leases；轮换不以 C=1 为由自动清空主 KV。 |
| TL05 | 主体要求直接写文件、exec、SSH，persona 又声称有权限 | 工具注册/调用验证拒绝；经合法 handoff 的 worker 可按授权执行。 |
| TL06 | 主体发 memory.save，worker 延迟或失败 | 立即取得 pending ID，主体继续迭代；未收到 saved receipt 前不报告已保存，依赖读取按 operation ID 排序。 |
| TL07 | commit 前后和 outbox 派发前后 crash | 恢复后没有重复 worker job，没有丢失已提交后继，事件消费水位不跳过未提交输入。 |
| TL08 | 旧主体请求可能仍运行，新实例接管 | 新实例完成 fencing，但在旧 terminal 未确认前不启动新主体请求。明确记录不可用原因。 |
| TL09 | worker 完成回调恶意请求 self.next / 主体权限 | 只接收合法结果，拒绝主体派发能力；没有第二 thought loop。 |
| TL10 | 同一空结果导致主体反复委派同一研究 | 主体仍继续，但相同 pending/已满足任务被幂等或去重规则处理；bounded child tree 不无限扩张。 |
| TL11 | 当前主体思考中收到紧急外部消息 | 消息持久化；按配置排下一轮或先取消确认再顺序重启，绝不并发第二主体；不用默认聊天 worker 绕过。 |
| TL12 | 变更 persona、模型/template/tokenizer，随后 cache miss | 注册能力不变，KV 按 revision 失效，原始任务/消息/承诺仍能恢复。 |
| TL13 | 关机、后端断网、hard quota 耗尽 | 状态诚实，恢复不伪造空白期思考；资源恢复且 owner 未暂停后自动恢复唯一持续链。 |
| TL14 | worker 需要改变全局目标或直接处理普通外部消息 | 先回注主体，只有主体顺序决策/既定显式机械例外允许后续行动；没有隐式双重决策者。 |

本文件是根据用户硬约束提出的架构与验收契约。上游 harness、history、cache 能力证据见 `research_harness.md`；更广的资源/历史不变量见 `scheduler_audit.md`；KV 比例、自动分配与后端支持程度见 `kv_policy_spec.md`。
