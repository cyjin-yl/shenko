# Shenko 运行合同增量：主体、事件、推理、Seat 与升级交接

适用：当前 v0.3.0 的 §5、§13、§15、§17、§35、§37；供根稿重排并入 §41。状态：**首版待实现的规范文本，不是完成实现或实机验证声明。** 本文的字段、状态机和工程默认属于 Shenko 的产品设计；引用上游仅证明明确标注的来源行为。框架、主体人格、恢复器、调度策略与本合同实现均可自主升级；当前运行的一代必须兑现当前合同。这里没有永久不可修改组件，也没有新增逐次人类审批。

## 共用身份与提交记号

复用持久执行章节的 CommitRef、TaskRecord、OperationRecord、AttemptRecord、EvidenceReceipt 和 ActivationRecord，不另建互不关联的任务或副作用日志。

- CommitRef = { ledger_id, seq }。seq 只在一个 ledger 内严格递增，允许空洞；不同 ledger 的 seq、墙钟时间不能相互比较来推断全局先后。
- T1：在持久事务内冻结操作参数与合同、资源绑定、operation_id，并把一次 attempt 登记为 armed；事务外才可能产生外部效果。
- E1：接收者追加 EvidenceReceipt。旧 epoch 的回执仍可作为真实证据落账；落账不授予旧执行者推进当前任务的权力。
- T2：在当前任务版本与 invocation 有效时，原子提交 receipt 或对既有 E1 的引用、operation 归纳状态、checkpoint/result 和相应 outbox。终态任务不因迟到证据重新变成 running；后续核验属于独立 reconciliation task。
- S0：固定一次主体步的输入计划与上下文版本；S1：提交该步决定、事件处置、委派意图和唯一后继。S0/S1 不代替外部操作的 T1/T2。
- storage 返回“提交是否成功不明”时，关闭该写入 session，reopen 后按稳定 ID 查询；不得在原 session 中猜测失败并重复执行事务外操作。持久 ACK 的断电保证由选定 StorageProfile 决定。

所有可变记录带 record_revision；更新使用 expected_revision 比较并交换。所有状态变更记录 causation_ref、producer、schema_version。身份不可复用；软件 generation、控制者 epoch、任务 revision、外部进程 incarnation 和后端 engine incarnation 是不同维度。

| 标识 | 变化原因 | 不能据此推断 |
| --- | --- | --- |
| agent_id | 实例创建 | 改名、换人格、升级软件不创建新实例 |
| branch_id | 明确建立可用性分支或历史分叉 | 改 branch_id 不释放同一 GPU 的占用 |
| controller_epoch | 同一分支的有效控制者交接 | epoch 增加不证明旧进程已停止外部操作 |
| software_generation | 激活另一软件闭包 | 升级不能自动丢弃任务、事件或外部事实 |
| task_revision | 改目标、取消、改委派或显式迁移任务 | 软件升级不必无条件增加所有任务 revision |
| seat_incarnation | 远端程序/会话或 seat 身份发生不可连续变化 | 同一 pane 字符串不能证明仍是同一程序 |
| engine_incarnation | 实际后端引擎重建 | 仅本地连接重建不能伪造一个“旧请求已消失”的新 engine |

## 主体一步的持久合同

### 记录与生命周期

主体的长寿根记录与每次有限推理步分离。根记录不因“当前没有任务”终态化；正常可用时，每个 S1 都登记下一步，运行器立即把它交给同一 broker 调度。资源等待、故障恢复、设备挂起、人类主动暂停及有限代际交接均有明确状态，不能改成按低频定时器唤醒。

~~~text
SubjectBranchHead {
  agent_id, branch_id,
  subject_root_task_id,
  controller_epoch, software_generation,
  committed_step_no, committed_step_id, committed_at: CommitRef,
  next_step_id,
  mode: active | quiescing | handoff | paused | recovery_blocked,
  mode_reason_ref?, record_revision
}

SubjectStep {
  step_id, branch_id, step_no, expected_parent_commit: CommitRef,
  controller_epoch, software_generation,
  state: planned | preparing | awaiting_model | result_recorded |
         committed | retry_pending | retired,
  input_plan_ref?, context_revision?, hook_graph_ref?,
  active_request_id?, active_attempt_id?, model_result_ref?,
  decision_ref?, commit_ref?, retry_reason_ref?, record_revision
}

StepDecision {
  step_id, input_plan_ref, model_result_ref,
  event_dispositions[],       // 明确逐项处置或明确的有界集合引用
  delegated_task_intents[],   // 含稳定 task_id、task_revision、完整 child prompt/model
  child_dialogue_intents[], memory_intents[],
  policy_intents[], publication_intents[],
  pending_obligations[], evidence_refs[],
  successor_step_id
}
~~~

delegated_task_intents 可包含完整 system prompt、所选模型、任务目标、成功/结束条件、工具 scope、已有授权引用、预算和 ReplyGrant；字段通过 capability discovery 提供给主体。实际创建 worker、写文件、SSH、安装、版本提交及发消息仍由执行服务/child 完成。主体 schema 不得包含任意文件/网络执行通道。

“主体可读写语义记忆”表现为 memory_intents 与 memory worker 结果，不把 SQL、路径读取或 shell 隐藏在通用插件调用中。运行器保存 SubjectStep 属于基础设施持久化，不是主体的文件工具。

### 转移、前置条件与提交点

| 转移 | 前置条件 | 本地持久提交 | 事务外动作与恢复 |
| --- | --- | --- | --- |
| planned → preparing | 分支 active；step_id 等于 head.next_step_id；epoch 有效；不存在另一个可提交的同一步 attempt | 记录 step 对父 commit、epoch、hook graph 的绑定 | 准备所需已提交资料。需要额外推理的贡献作为独立任务；不得占用主体 compute permit 等它 |
| preparing → awaiting_model（S0） | 输入计划、角色工具集、persona/context revision、最终 token 预算及 provider 映射已验证 | 原子保存 InputPlan、不可变 context/wire 引用、request_id 与待入 broker 的调度意图 | 入队以 request_id 去重。S0 后的新事件不能修改已 seal 的请求；必要时取消/替换 attempt |
| awaiting_model → result_recorded | 同一 request/attempt 的完整结果已持久；可解析且结果边界已明确 | 保存原始结果、usage、结构化候选决定及来源 | 推理计算许可按 broker 的真实终止证据释放，与 S1 是否成功独立；不能为了等待写盘而让后端继续生成 |
| result_recorded → committed（S1） | 当前 epoch 有效；head 仍等于 expected_parent_commit；step_no 恰为后继；输入/工具版本与本步 RequestSeal 一致；决定 schema、角色、任务与当前授权引用有效 | **一个原子批次**写决定、逐事件处置、child/memory/policy/publication 意图及稳定 ID、pending obligations、新 head 和唯一 next_step_id | 只在成功提交后向 worker/outbox 发布；发布重复时按稳定 ID 读取同一任务；下一步立即进入 ready 调度 |
| preparing/awaiting_model → retry_pending | 准备失败、generation 中断或结果无法形成有效完整决定 | 记录原因、已产生证据、重试关联及仍待处理的事件；不推进 head | 不执行截断工具参数或未提交决定。确定旧请求不会继续生效且容量已对账后，再安排同一步的新 attempt |
| 任一未提交步 → retired | 代际移交、显式替代输入计划或失去控制权 | 保存 superseded/retired 原因与关联版本 | 迟到输出只进入 E1/usage；不能创建新 child、推进 head 或外发。接任者从最后 S1 接续 |

S1 的比较并交换对相同 branch 的已提交 step_no 建唯一约束，对 successor_step_id 建唯一身份；不能只在内存中检查“没有另一条主循环”。单次 step 可重试多个 attempt，但最多一个决定被接受。断电后若 S1 已提交而 worker 未启动，重放调度意图；若未提交，则没有授权从该草稿产生外部动作。

S1 不能跨 LLM、SSH、网络调用或长时间等待持有 SQLite 写事务。需要跨存储时，必须提供等效的可恢复发布协议；最终对外可观察的决定只有一个已提交版本。提交失败或不明时，以稳定 step_id 查明结果，不创建新 successor 试图“补一个”。

主体结果的结构化摘要可显示已提交意图、委派和依据，不要求公开内部逐 token 思维。模型内容采用 committed-only 发布：chunk 先持久化并取得稳定序号，再进入可恢复的流式发布；订阅者按序号去重/续接。未持久 token 不先显示成内容。临时 UI 仅可显示 cursor、rate 等非内容状态，不能成为动作、事件处置或完成证明。

### 连续主体与有限 child 的所有权

Pi Durable 要求普通 owned work 排空后，有限父任务才能终态化。Shenko 因此不能把所有长寿 child 挂在即将结束的 SubjectStep 下，否则每一步都会等待所有孩子。**持久委派挂在 subject_root_task_id 或相应有限委派任务下；SubjectStep 只通过 created_by_step_id 记录来源。** 这是一项 Shenko 的结构选择，不是修改 Pi 的 ownership 规则。

主体可登记“等 child D 的结果后继续目标 G”的 obligation，并在下一步继续其他思考。它不在整个主循环上执行 wait-for-all-children。有限 worker 需要等依赖时，则使用持久等待记录并释放其 invocation、compute permit 及依赖启动必需的执行席位；受预算限制的 ContextLease 可继续存在。

~~~text
TaskWait {
  waiting_task_id, expected_task_revision,
  on_task_ids[], policy: all_settled | owned_fail_fast,
  resume_checkpoint_ref, created_commit: CommitRef
}
~~~

登记 TaskWait 时校验目标存在且 wait-for 图无环；图同时包含显式等待边与有限父任务完成时必须等待的 owned-work 边。拒绝等待自己、等待仍需自己结束的父任务等环，并生成可诊断结果。非 owned 任务默认 all_settled；不能因为观察别人失败而自动取消别人的工作。

依赖终态与 resume-ready 的形成必须可从已提交任务记录推导；重启后重做 reconcile 即可，不依赖一次性内存 callback。child 若需要主体作选择，先提交问题再 parked；主体将答案作为后续 step 的持久 dialogue 意图发送。任何 hook/child 不得等待“当前主体步未来才会提交的答案”而同时阻塞该步。

child 进度、结果产物、任务终态、对话投递、渠道发送分别建记录。进度消息不能消费最终结果槽；任务完成不能推断 QQ 已发送；外部执行 unknown 不能仅因 child 退出而当作消失。有限任务结束后仍需对账的 operation，挂到明确的 reconciliation task，并保留恢复闭包。

## 外部事件、注意力与输入放置

### 事件身份与三个独立进度

~~~text
InboxEvent {
  event_id, event_revision, event_kind,
  source: { provider_id, account_id, transport_identity_domain,
            conversation_id?, thread_id?, external_event_id?,
            source_event_revision?, source_cursor? },
  actor: { authenticated_platform_id?, person_link_ref?, role_grant_ref? },
  occurred_at?, received_at, admitted_at: CommitRef,
  causal_parents[], supersedes_ref?, raw_ref, normalized_ref?, attachment_refs[],
  normalization_state: ready | pending | failed,
  attention: { disposition: ready | deferred | quarantined,
               policy_revision, reason_ref?, review_condition? },
  processing_state: unresolved | interpreted | delegated | resolved,
  processing_refs[], record_revision
}

PresentationReceipt {
  presentation_id, event_id, event_revision,
  step_id, request_id, context_revision,
  representation: full | excerpt | aggregate_member | handle_only,
  source_span_ref?, committed_at: CommitRef
}
~~~

入站至少区分：①本地可靠接收；②被放进某次模型输入；③主体已提交何种处置。另有下游任务完成和人类已读状态，不能混在第三项里。模型 request 被取消后，其 PresentationReceipt 仍可用于诊断，但消息仍 unresolved，直到 S1 明确解释、委派、解决或决定无需回复。

默认强去重键为 provider_id、account_id、transport_identity_domain、conversation identity、事件种类、external_event_id 和平台修订标识。platform message ID 相同但内容发生真实编辑时保存新 revision；来源声称同一 revision 却给出不同内容时保存冲突证据，不覆盖原文。无法取得稳定平台身份时，标记 weak_dedup，并使用接收 spool 的稳定记录 ID；不能宣称已经实现跨重连强去重。

接收事务先保存原始对象及可用的归一化记录，再确认本地 accepted；支持 ACK/可重放游标的 provider 仅在提交成功后推进其 ACK/cursor。NapCat WS 没有因此自动获得上游持久 replay：本地 spool 已接收的事件必须续接，连接缺口须独立登记 gap。若归一化需要异步工作，先保留 raw_ref 和 pending 状态，再由 worker 完成，不能因插件解析错误把消息丢在内存里。

### 优先级空洞不能用一个消费游标覆盖

~~~text
InputPlan {
  input_plan_id, step_id, plan_revision,
  snapshot_watermarks: Map<ledger_id, seq>,
  selected_event_refs[], deferred_event_refs[],
  causal_support_refs[], ordering_policy_ref,
  required_context_refs[], optional_contribution_refs[],
  budget: { max_events, max_input_tokens, actual_tokens?, count_method },
  projection_digest, created_commit: CommitRef
}
~~~

snapshot_watermarks 只是本次读视图的边界，不代表之前所有事件均已处理。主体先处理序号 105 的来电、尚未处理序号 101 的研究结果时，必须保留 101 的状态；不能把 last_consumed_seq 更新成 105 而丢掉它。实现可用事件状态索引和显式集合，不能仅用一条单调消费游标表达注意力进度。

首版工程默认每步最多直接选取 64 条事件、为事件投影预留 8192 个实际输入 token，取先达到的限额；这些是可改默认，不覆盖总上下文窗口校验。超过额度时先使用确定性的计数/来源分组及可分页原文引用，已有合格摘要也可复用；需要语义摘要时，创建通过同一 broker 的有限 summary child，不隐式增加模型并发。摘要中涉及 100 条消息不等于这 100 条均 resolved；S1 必须记录它实际处理的集合和所采取的处置，未处理记录仍可继续选取。若事件 token 数暂不可可靠计算，采用保守字节/文本预算并在最终 tokenizer 阶段重新校验。

选择顺序默认先满足因果约束，再按有效注意力等级、等待老化与稳定 tie-break 排列；相同来源的修订、撤销、问题关闭不能被孤立解释。跨来源没有已知因果时，用 admitted CommitRef 的 ledger_id/seq 与 event_id 作确定性 tie-break，只保证可复现，不伪称真实发生顺序。平台时间只作信息，不能倒置已知因果。

外部消息依 D3 通常最高紧急，但正文不能自行把 trusted priority、管理员身份或工具权限改高。DND/过滤仅改变可追溯的 attention disposition；主体仍可回看原始事件。归一化的权限撤销、seat 已被接管、问题关闭及任务取消等控制事实，在实际 effect arm 前必须核对最新 revision，即使模型当前仍使用较早的输入快照。

### generation 期间的注入

普通路径为：事件持久接收 → 发出合并的 attention notification → 下一 S0 选取。已发送请求的 context_revision 和 wire_digest 不可变。runtime 不得直接追加字符串到正在 decode 的历史对象。

若 adapter 有已验证的 live steering：保存 steering operation、适用 request/engine incarnation 及后端确认的生效边界；收到后端接受不等于该事件已被主体处理。若只有 cancel：先持久 stop intent，等待 broker 确认旧请求停止，再为同一步构建新的 InputPlan/request revision；原模型 partial 保留但不执行不完整工具调用。若没有请求内停止能力：等有限 generation 结束，在下一边界注入。

一个当前请求只保留一个合并的 stop/escalation 意图；同一轮连续来十条消息不发送十次无意义 cancel。必要事件始终入 inbox，通知丢失可由 ready 索引恢复，不能依赖“收到通知”才知道有消息。

## Hook、上下文与请求封口

每个主体/child request 固定一份 hook graph、插件实现闭包和 schema 协商结果。模型输入的内容顺序、hook 执行顺序、事件注意力顺序、插件激活顺序分别有版本，不能共用一个 priority 字段。

| 阶段 | 输出 | 允许的工作 | 硬边界 |
| --- | --- | --- | --- |
| ingress normalize/persist | InboxEvent/raw refs | 纯归一化；异步补充转 worker | durable accepted 前不能推进可重放 ACK；正文不升级成 system |
| inputs select | InputPlan | 选事件、因果支撑、既有 child 状态 | 固定集合与读水位；后到事件另排 |
| context collect | Contribution refs | 读取已提交结果、发起有界准备任务 | 不占唯一模型 permit 等另一次模型调用 |
| history finalize/compact | Window revision | 选一个明确 compact plan；保存寻址 | 不让多个 compact 插件反复覆盖彼此 |
| context assemble/validate | ContextRevision | 固定角色工具集、system、后置人格、历史、事件、瞬态贡献 | 完整 token 校验；不可省略工具参数开销 |
| model prepare/seal | RequestSeal | 映射 native 参数、cache hint 与可解释降级，持久化工具合同及 implementation digest | 改模型内容、模板、工具或预算必须重新 seal |
| broker admission | Allocation/ContextBinding | 取得已声明资源、添加真实 slot/lane 等运输绑定 | 运输绑定若改变语义或预算，退回重新 seal |
| dispatch/stream/result | Attempt/EvidenceReceipt | T1 → 外部请求 → E1/T2 | observer 不能改已发请求，stream 不直接执行未完成调用 |
| decision commit | S1/child intents/successor | 校验当前 epoch/revision 并提交 | 成功提交前不外发、不启动草稿委派 |

所有 phase handler 返回 typed patch、contribution、decision 或 observation；声明读写区、合并器、必需/可选依赖、取消/超时语义。没有约定的冲突拒绝候选图，旧健康图继续。异步完成顺序不能决定最后谁覆盖字段；无依赖项以稳定 handler ID 排序。

需要外部 I/O/LLM 的 hook 先创建可恢复准备任务，在事务与 compute permit 之外执行。可选贡献首版默认最多等待 250ms 的本地准备预算，超时登记 omitted/deferred；后台任务仍可完成并进入下一步。该预算不强制取消已发生的外部操作。必需贡献不能按 optional 跳过，必须明确等待、采用已验证的旧图/已有结果，或产生恢复事件；不得静默用空值绕过校验。

非确定性贡献保存实际结果与来源，回放复用相同 contribution_ref；重算形成新 revision。effectful hook 使用稳定 logical_key/operation_id；兼容旧新 hook payload 只转换一次调用，不各自执行外部动作。around/next 默认至多一次；重试由 OperationLedger 负责。

Graph 热更新只影响后续 dispatch。旧模型按旧工具 schema 产生的调用继续绑定旧合同；旧实现缺失时返回 contract_unavailable 并重规划，不交给同名新函数猜参数。旧 epoch 回执可走 E1/decoder bridge 保存，不能被旧 handler 当作推进当前主体的入口。

S1 校验针对该步已 seal 的版本，不要求它等于刚热装的全局最新 registry；当前授权、任务和资源绑定仍需在实际派发时重新核验。

## 推理请求状态机与 C=1 资源归还

### 记录与容量不变量

~~~text
InferenceRequest {
  request_id, request_lineage_id, request_revision, supersedes_request_id?,
  owner: { task_id, task_revision, branch_id?, step_id?, purpose },
  resource_group_id, engine_incarnation, capability_snapshot_ref,
  context_revision, context_binding_ref?, hook_graph_ref, request_seal_ref,
  wire_ref, wire_digest,
  model_profile_ref, input_tokens, output_reservation, state_cost_vector,
  priority: { class, inherited_from?, policy_revision, trusted_origin },
  state: sealed | queued | reserved | armed | active | stop_requested |
         unknown | quiesced | terminal,
  active_attempt_id?, allocation_id?, stop_intent_ref?,
  result_ref?, terminal_reason?, record_revision
}

ComputeAllocation {
  allocation_id, resource_group_id, engine_incarnation,
  request_id, attempt_id, units, owner_broker_epoch,
  state: held | released,
  acquired_commit, release_proof_ref?, released_commit?, record_revision
}

QuiescenceProof {
  proof_id, request_id, attempt_id, engine_incarnation,
  provider_request_handle?, native_slot_generation?,
  scope: request | engine | physical_resource,
  kind: never_dispatched | request_terminal | gated_pause | engine_destroyed,
  native_evidence_ref, observation_commit,
  auto_resume_possible: false,
  capability_contract_ref
}
~~~

request_id 唯一标识一份不可变 RequestSeal 对应的逻辑生成请求；网络 attempt、后端 handle 与实际 slot 不混作该 ID。**任何改变模型输入、工具/模型语义参数或预算的 reseal，都同时分配新 request_id、在同一 request_lineage_id 内递增 request_revision，并填写 supersedes_request_id。** 同一个 request_id 不得指向两个语义版本。因此入队、恢复和结果关联可按 request_id 去重，不会把新的输入误认成旧 generation。对同一上游调用的状态查询不生成第二次 generation；原输入未变且符合恢复合同的重试可新增 attempt。

同一 resource_group 的所有 held ComputeAllocation，加已核验且可强制的外部预留，不能超过该组有效 C。reserved、armed、active、stop_requested 和 unknown 都可能占容量；UI 状态名称不决定是否计数，Allocation 记录才决定。resource_group 绑定实际共享 engine/device，而非 URL、模型别名或 API 协议。递归 child、memory、judge、媒体与远端 coding agent 使用该物理域时走同一账本。

多 broker 接管需要共同的持久 authority/epoch 与受管 dispatch 通路。若旧 broker 仍可绕过通路直接请求同一后端，则仅增加本地 epoch 不能兑现硬 C；该部署必须标为未完全受管，完成入口接管或取得可验证的隔离后才能宣称硬上限。软件 A 与候选 B 不能各自初始化一份满额许可。

### 转移表

| 转移 | 前置条件/提交点 | 容量处理与恢复 |
| --- | --- | --- |
| sealed → queued | wire/context/token/native 能力校验通过；request_id 幂等入队 | 不持有 compute；缓存准备另有自己的预算 |
| queued → reserved | broker 当前 epoch 有效；C 与实际资源向量可行；必要 ContextBinding 已可用或可回滚预留成功 | 原子取得 held Allocation；不能先占许可无限等另一资源 |
| reserved → armed（T1） | 最终 context/slot 绑定与 request revision 相符；请求未撤销；dispatch owner 有效 | 持久记录 attempt 与全部外发参数，然后才允许写网络/SDK。armed 后崩溃视为可能已发送 |
| armed → active | 观察到匹配后端 handle 的接受/流事件 | 保存 E1、原始 handle/engine/slot generation；不把 HTTP 200 当完成 |
| queued/reserved → terminal(cancelled_before_dispatch) | 取消事务先赢得未 armed 的状态，dispatcher 再也不能发送该 attempt | 若有 Allocation，以 never_dispatched 证明原子释放；不得仅靠调用者说“我大概没发” |
| armed/active → stop_requested | 持久 stop intent 成功；记录 cancel/yield 的真实作用域 | 事务外发控制操作；仍 held。accepted 是收到控制请求，不是终止证明 |
| armed/active/stop_requested → unknown | 连接断开、超时、controller 崩溃或无法判定请求是否仍运行 | 仍 held；创建无须 LLM 的状态对账工作。deadline、心跳或租约过期均不释放 C |
| armed/active/stop_requested/unknown → terminal | 得到匹配 attempt 的终态事实，或确认 engine/物理执行实例已销毁；快速完整响应/明确拒绝允许跳过 active 的单独落账 | E1/T2 保存结果/usage/partial 与 QuiescenceProof；同一批次释放 Allocation，随后才调度下一请求 |
| active/stop_requested → quiesced | 后端提供可核验的暂停完成，并且恢复必须经 broker 重新准入 | 保存 gated_pause 证明后可释放 compute；ContextLease/状态字节独立保留。恢复先重新取得 Allocation |
| quiesced → queued | 原输入/恢复 frontier 有效；当前任务仍需要结果；恢复操作由 broker 独占控制 | 同一逻辑请求的恢复 attempt 入队；保留历史输出/usage，不能把 replay 当新输出再发布 |

后端内部 paused、Host parked、排队状态或没有 token 输出，**都不足以**形成 gated_pause。若 engine 会自行恢复原请求，它仍是 broker 未结束的工作；适配器必须继续计占用，或在后端整体受控的容量模型中证明不会超额。不得先释放 C 再希望旧请求恰好不恢复。

允许释放容量的证据由具体 adapter 合同定义。例如 Strata 的已绑定 slot 在收到对应 BDONE 后可按其协议认定执行槽空闲；仅写出 BSTOP 不足。通用 HTTP 连接断开则通常没有同等级证明。后端一个新连接或新的客户端 session ID 也不能证明旧 server request 消失。

本地 request terminal、compute 已停止、逻辑任务完成、远程 shell 已停止和外部动作已撤销是五个不同事实。取消 generation 不取消已发出的 SSH/GUI/文件效果；这些 operation 继续按自己的 ledger 回收回执和对账。

### 首版调度默认与公平性

首版默认使用有限 generation：默认输出预算 2048 token，按模型实际接口计入推理/可见输出及工具参数开销；模型 profile 可以采用适合该模型的有限上限。达到 length 边界但工具参数未完整时，不执行 partial，而把完整可用输出与不足情况存档，再安排后继/重建。上限是可改工程默认，不是 128K 输入窗口，也不是每步必须耗尽的目标。

普通 ready 请求按主体与 child 的有效任务队列公平轮转；在双方都持续可运行且没有更高等级事件的情形下，默认不连续服务同一侧超过两个完整 generation。child 侧再按任务 lineage、老化与依赖关系选择，递归创建新 task 不增加该父系的总公平份额。实际 token、prefill/replay 和时间成本用于更新策略，不用“每个请求次数相同”冒充耗时相同。

外部高紧急事件仍按 D3 可先服务；紧急来源不能提权。主循环每步立即登记后继，但登记不意味着永远抢在 child 前面。正常请求有限轮转是计算公平协议，不是低频 tick。即使后端完全不支持中途取消，有限请求完成后的轮转也必须使双方继续推进。

同一 child 在两次 durable checkpoint 之间被连续取消三次时，默认生成一次合并的 attention-starvation 建议，交给主体决定 DND、调整任务、改变模型或继续接受打断；不自动向人类索要批准。建议带取消成本、已完成/未完成工作和最老待办；同一停滞 case 在没有新证据/策略变化前不重复刷屏。任何固定阈值均属于可替换策略插件。

取消/断连对账使用确定性 I/O，不等主体取得被占用的唯一槽后才查询。首版控制请求超时只改变 knowledge state；可用有界指数退避和持久 next_probe_at 安排重试，但任何重试时钟都不代替主体连续思考，也不产生“超时即免费”的容量漏洞。无可查询路径时保持 unknown，并允许主体/恢复插件选择已授权的引擎重启、隔离或独立后端；选择前如实保留受影响容量。

## KV/原生后端的句柄与恢复合同

### 能力声明和句柄

~~~text
BackendCapabilitySnapshot {
  capability_id, provider_implementation_digest, backend_family, backend_build,
  transport, configuration_digest, engine_incarnation,
  model_fingerprint, tokenizer_fingerprint, template_fingerprint,
  operations: Map<name, {
    availability: available | requires_configuration | unsupported | unknown,
    scope, guarantee: advisory | best_effort | acknowledged | reserved,
    input_schema_ref, output_schema_ref, native_field_mapping_ref,
    cancellation_semantics_ref, evidence_ref
  }>,
  capacities: [{ pool, unit, value?, provenance: measured | configured | estimated | unknown }]
}

ContextLease {
  context_id, context_revision, owner_task_id, owner_scope_ref,
  provider_id, engine_incarnation, capability_snapshot_ref,
  native_handle_ref, native_handle_generation?,
  model_fingerprint, tokenizer_fingerprint, template_fingerprint, toolset_digest,
  window_id, prompt_token_digest, prefix_length, frontier_ref?,
  state: requested | reserving | resident | parked | transferring |
         reclaim_pending | invalid | released,
  copies: [{ tier, pool, units, identity_ref, readiness, sharing_ref? }],
  retention: { guarantee, entitled, used, borrowed, reclaim_pending, expires_at? },
  supported_operations[], lease_revision
}
~~~

scope 指真实数据/缓存可复用域，不要求为每个群创建隔绝的主体知识。原生 handle 可以是不透明引用；不能把它当模型可自行猜写的 slot 整数。续接必须同时核对 engine incarnation、模型/模板/工具指纹、实际 token 前缀及窗口/恢复 frontier；任何不符都按原始事实重建或使用后端明确提供的转换。

cache affinity、prefix hint、可恢复 Host 状态和 GPU 硬驻留分别标记。不能仅凭请求带 session_id/prompt_cache_key、HTTP 200 或 cache hit 指标就把 retention.guarantee 标成 reserved。capacity 不可知时保留 null/unknown，token 计数不能直接冒充物理字节。

主体的多个 context 合计使用算法给主体分配的份额；children 合计使用另一侧实际可行的份额。理想对称场景下算法自动得到约 50/50 是验收场景；本合同不新增手动 half-pool 开关，也不把比例写进 kernel。共享 prefix 页只按实际物理占用计一次，公平归账另记。

### 迁移与恢复的实际提交点

一次 cache move/spill/restore 采用独立 operation：先预留目的空间与 source reader lease，记录迁移意图，再执行拷贝；只有原生完成证据到达，才发布 ready 副本并释放确实不再有 reader/引用的源。DMA/传输提交后取消，要等真实 reader 结束，不能因本地 future 被丢弃而重用源内存。目的未 ready 不得成为下一次 restore 来源。

恢复计划必须计算源仍存在时的目的/临时峰值；“最终能装下”不足以通过 admission。等待必要 child 前，必须为至少一个依赖的实际执行保留可行路径；主体若硬 pin 全部状态池而 child 无法启动，需迁移/回收/改变计划或报告 infeasible，不能持有父状态无限等待。

Snapshot 和 Replay 保留不同的内容与成本。若 adapter 只能根据持久 token/历史重算，报告 replay/prefill；只有真实执行状态和必要 KV/StateImage 可恢复时才报告 snapshot。Replay 不能重复发布历史输出、再次执行已完成工具或再扣同一次逻辑输出预算；真实新增计算成本仍应记录。

软件更新若 engine 没有重启，句柄仍需由新 adapter 按相同 capability/engine identity 重新验证后接管；不能把进程内指针序列化当可恢复 handle。engine 确认重建后，旧 ContextLease 全部 invalid；原始会话、任务与身份不受缓存丢失影响。

### 本轮上游事实与首版适配边界

| 上游一手证据 | 确认的事实 | Shenko 必须保留的边界 |
| --- | --- | --- |
| [NInfer 固定调度说明 §§3.2、8.3、9](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/maintainer/resource-scheduling-and-context-cache.md) | typed pool 的联合预留；Snapshot/Replay；reader lease、目的预留、真实传输完成后交接 | 文档内的 engine 暂停/恢复不能自动当 HTTP 客户端已取得任意 pause/pin API；必须绑定 transport/build/config 及真实控制能力 |
| [Strata 固定 batching/engine 协议](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md#L547) | BGEN/BADM/BT/BDONE、BSTOP、BYIELD/YIELDED；BDONE 后执行槽空闲且 conversation 可保留 | 原生 stdio 由唯一 companion/服务所有者操作；不能伪造现成 HTTP 字段。BSTOP 与 prompt-read BYIELD 不是同一种停止语义 |
| [Strata conversation parking](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md#L507) | 有可配置的 conversation parking 与恢复路径 | Host parking 不宣称是 GPU hard residency；是否命中和恢复成本以该部署实测为准 |

vLLM 与 1Cat 的具体字段继续采用 §13.7 已固定的能力矩阵；本次不扩称新的可用 API。共同的 native.* 命名空间保留原始值、版本和返回，不经 OpenAI-compatible 最小公约数丢掉能力。native 参数在 seal 前校验；后端接受后忽略字段属于未生效，必须可诊断，不能被标为成功透传。

## Seat 问题、决定与答复效果

### Seat 与问题记录

~~~text
TaskSeatBinding {
  seat_id, seat_revision, seat_incarnation,
  adapter_id, adapter_version, host_identity_ref,
  remote_agent_session_id?, tmux_server_id?, session_id?, window_id?, pane_id?,
  process_identity: { pid?, process_start_token?, executable_digest? },
  cwd_identity_ref?, bound_task_id, bound_task_revision,
  writer: { lease_id, lease_epoch, holder, fence_capability, expires_at? },
  observation_ref, observation_revision, event_cursor_ref?,
  supersedes_binding_ref?, record_revision
}

SeatQuestion {
  question_id, question_revision,
  seat_id, seat_incarnation, task_id, task_revision,
  source_kind: native_rpc | sdk_callback | terminal_inference,
  native_request_id?, native_tool_use_id?, origin_event_ref,
  question_kind: planning | information | tool_permission | new_authority,
  text_ref, options: [{ option_id, native_value, label, meaning_ref? }],
  requested_action_ref?, exact_input_digest?, answer_schema_ref,
  observed_screen_revision?, raw_evidence_refs[],
  expires_at?, external_default_ref?,
  state: open | decision_committed | responding | answer_unknown |
         answered | expired | withdrawn | superseded,
  effective_answer_ref?, closure_evidence_ref?, record_revision
}

AnswerDecision {
  decision_id, decision_revision,
  question_id, expected_question_revision,
  expected_seat_incarnation, expected_task_revision,
  deciding_actor, delegation_grant_ref, authority_revision,
  selected_answer, exact_effect_input_digest,
  reason_summary, evidence_refs[],
  operation_id, valid_until?, committed_at: CommitRef
}
~~~

question_revision 只在问题语义、选项、实际 action/参数、期限或相关绑定变化时增加；spinner、无关日志、光标闪烁等变化增加 observation_revision，不无意义地产生新语义问题。一个问题关闭后又出现，即使文本完全相同，仍有新的 question_id 或原生请求身份。不能用整屏文字 hash 代替问题身份。

terminal_inference 必须保留置信与未知信息；缺少高亮时不得根据选项顺序猜当前选择。tmux capture-pane -e、screen buffer、ANSI/宽字符等恢复由观察插件负责。独立 wrapper 能力提升经过安装/验证后，新的观察合同和版本写进后续 Question，不能改写旧证据。

### respond 合同

respond 接受 question_id、expected_question_revision、expected_seat_incarnation、expected_task_revision、decision_id、expected_writer_lease_epoch 和稳定 operation_id。模型只决定语义答案；确定性 adapter 把它映射为准确的 native response 或有限终端输入，保存最终 wire/input digest 后才 arm。

| 步骤 | 必须验证/提交 | 失败或崩溃后的处理 |
| --- | --- | --- |
| 选择答案 | 主体或有明确 decision scope 的有限 child；既有委托覆盖实际问题/action | 常规实现选择、已授权限内许可可自主决定；不额外固定问人。确需更多信息/新授权时形成后续调查或联系计划 |
| 提交 AnswerDecision | 问题仍 open；语义 revision、seat/task/authority 版本匹配；答案符合 schema | CAS 失败返回 stale_question/stale_task/stale_authority；保留候选但不输入 |
| preflight | 通过当前受管 writer 获取最新现场/原生请求存活状态，确认仍是原程序和问题 | 人类已回答、进程退回 shell、pane 复用、任务变更即使旧屏幕仍像问题也不继续输入 |
| T1 arm | 再核对当前 Decision/lease/revision；登记 operation 与准确 effect input | armed 后即存在“可能已送出”窗口；不可用临时进程内 sent 标志作唯一依据 |
| 实际输入 | 在事务外使用原生响应接口，或受管终端写入通道 | 网络/SSH 写完成最多证明 transport_written，不证明远端接受答案或其工具已经成功 |
| E1/T2 回执 | 归档原生答复确认、问题结果、工具/任务后继或其他明确证据；记录真正生效的答案来源 | 无确认则 answer_unknown；问题从屏幕消失本身不能证明是我们的答案生效 |
| 结束/恢复 | answered/expired/withdrawn/superseded 均保留 closure evidence；恢复先 inspect 原 request/seat | 不重发未知的 Enter 或 UI response。安全重试只在 adapter 明确提供幂等/去重合同或确认未应用时成立 |

“已作决定”和“答案已生效”分开。候选答案可在发送前被较新事实取代；T1 后禁止原地改同一 operation 的 input，改变主意产生新的决定/operation，并先确定旧效果。人类与 Agent 的竞态以目标端真实处理证据或受管通道的原子仲裁确定；不能凭两边本地时钟先后宣布获胜。

原生请求 ID 和有效 callback 可提供比画面更强的关联，但不自动具有跨进程 durable 去重。原始 tmux 没有原子“比较屏幕/问题 revision 并输入”的操作：受管 writer 可串行化本产品的输入，无法防止旁路人类输入。该路径明确标 check_and_input=best_effort；每次输入前现场复核并监听接管，不能把本地 lease 宣传成目标端强围栏，也不因此给所有操作新增人类批准步骤。

AnswerDecision 允许 allow、deny、选项、文本、修改后的参数等适合该原生请求的结果；扩大成永久权限规则是独立语义，不能因为一次 allow 顺便提升长期授权。QQ 发信人的平台身份、联系人推断、紧急级别与管理授权分别验证；开放聊天不把群员或群主提升为管理员。

### 真实上游语义，不将展示或默认选择当成 Agent 决定

- [OMP 固定 RPC UI 协议](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/docs/rpc.md#L1202) 提供带 id 的 UI request/response，并区分 tool UI 与 extension approval。超时/abort 可产生 cancel/targetId，迟到答案会被忽略；ask 的超时默认可能选 recommended 或第一项。Shenko 应记录 external_default，并继续核验后果，不能记成“主体自主批准”。
- 同一 OMP 文档的 [yield/settled 说明](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/docs/rpc.md#L345) 区分 prompt_result 与 session_settled。前者不证明后台工作全部结束；也不证明某次 UI response 已取得外部 exactly-once 保证。
- [Claude 官方 user-input](https://code.claude.com/docs/en/agent-sdk/user-input) 将工具许可与 AskUserQuestion 交给 canUseTool；callback 的取消信号与实际请求一起绑定。该接口可以由 Shenko 的受委派决定者回答，不要求产品再固定插一轮人类审批。callback 的内存 Promise 不能替代跨进程问题记录；defer/resume 要使用实际版本支持的条件。
- [Claude 官方 permissions](https://code.claude.com/docs/en/agent-sdk/permissions) 的 canUseTool 不是所有工具的统一审计入口；自动允许的调用可能先被解决。因此任务 scope、效果记录与全局推理约束不能只放在一个可能不被调用的许可 callback 中。

### 动态联系与旧答复

确需联系人的问题登记 ContactPlan：decision_id、question/ref 与 revision、目标 person/已验证账号关系、候选已授权渠道、内容可见范围、已发 operation、答复关联、复核条件。主体按人格、记忆、可达性与急迫程度选择行动；不用固定三分钟/固定电话作为内核规则。

跨渠道答复先作为普通持久外部事件进入同一主体，再绑定其引用的 decision/question revision。一个有效答复使其他未发送或仍待答副本收敛；旧 question 已关闭时，答复保留为后续上下文，不自动落到新问题或 shell。没有人答复不是新授权；主体可继续其他工作、自行补查或采用已经获授的替代方案。

## QQ → 既有 tmux → 自开发 → 升级交接的完整时序

以下是规范示例，所有 ID/序号为说明用值，**不是已运行结果**。场景已有开发者手动设置的账号身份和持续自开发委托；QQ 所有普通用户仍可聊天。开发机有既有 tmux 中的 OMP；实际操作由 child 执行。为把 C=1 的归还点写清，示例采用已验证 Strata 原生控制适配器；其他 backend 必须提供其自己的等效证据，不能照抄 BDONE 名称。

初态：A 为软件 generation 7、controller epoch 18；主体最后已提交 step 100，唯一后继为 step 101；engine E9 的 C=1。一个有限研究 child 的请求 R0 正在执行并持有 Allocation A0；主体 context K-main 与研究 context K-research 的实际驻留按该后端能力记录。所有记录分别位于控制账本 L、渠道 spool I 和 seat/回执 spool R；三个账本没有共同 seq。

| 序号 | 发生的事件/动作 | 持久提交与执行条件 | 下一步与不可混淆的状态 |
| --- | --- | --- | --- |
| 1 | 开发者 QQ 消息 m900：“补好终端高亮观察，继续开发并升级 Shenko”，附截图 | I:501 原子保存原文、平台账号/会话/消息身份及 attachment handle；重复 m900 返回同一 event | 只宣告本地可靠接收，尚未宣告主体处理；不会从 QQ 文本产生管理员角色 |
| 2 | attention policy 请求让出 R0 | 保存 stop intent，并向当前 E9/R0 绑定的 slot 发 BSTOP | A0 仍 held，C 仍为 1；QQ 高优先级不让主体偷偷开第二请求 |
| 3 | adapter 收到匹配 R0/slot generation 的 BDONE(cancel) | E1 保存原始终态；T2 记录 partial/usage、QuiescenceProof 并释放 A0 | C 变 0 后才准入主体；conversation 是否仍有可用缓存按 ContextLease 核实 |
| 4 | step 101 读取 m900，决定先回忆开发现场 | S0 固定 I:501 和原始 handle；RequestSeal 后入 broker；S1 保存 memory intent M1、pending obligation 和 step 102 | 主体没有打开文件或 SSH；Memory worker 查询已有语义记录。无关后续事件不被 I:501 水位覆盖 |
| 5 | M1 返回源码/运行版本与既有 seat 的线索 | memory 结果按 task/revision 进入 E1/任务结果，再进入主体 inbox | 记忆中的 pane/cwd 只是候选位置，不自动证明远端仍是原任务 |
| 6 | 后续主体步形成 DevTask D1 | S1 原子创建 D1、完整 child prompt/model、已有自开发授权引用、有限 DecisionGrant 和目标 QQ 的 ReplyGrant；登记下一步 | D1 由 subject root 拥有；当前 SubjectStep 可以提交结束，不等待 D1 完成 |
| 7 | D1 核验工作树、seat 和观察能力 | 子 worker 读取当前 source/artifact，检查未提交修改；创建独立候选工作区；确认 tmux server/pane、进程启动身份、OMP session、cwd 与 writer lease | 若旧 pane 已换程序，修正 binding 后再继续；不把 worktree 当权限沙箱 |
| 8 | D1 向现有 TUI 提交具体开发任务 | T1 保存 submit operation S-submit 与准确输入；worker 在事务外输入；transport_written、后续实际 agent 接收/任务证据分别记 E1 | 此时控制器崩溃，恢复查询同一 seat/session 和 S-submit；没有可验证未应用证明就不重复输入任务 |
| 9 | OMP 在开发中发起模型/子任务 | 它的已配置模型入口经过同一 broker；其递归调用关联 D1 lineage | 主体继续有限串行步骤；等待终端/编译的 D1 释放自己的 compute/执行席位。未纳管 CLI 不能通过本项硬上限验收 |
| 10 | TUI 出现需选择观察实现方式的问题 | Seat adapter 保存原始带属性屏幕、process/session 身份、Question Q7 revision 1；问题归一化进入 inbox/已授权 child 的任务流 | spinner 改变只更新 observation revision；同名选项不靠文本顺序猜高亮 |
| 11 | D1 在既有 DecisionGrant 内自主选择实现方案 | 提交 AnswerDecision；执行前刷新问题，核对 Q7/task/lease/授权 revision；T1 arm 后答复 | 不要求开发者逐次批准。原生日志/工具结果或适配器明确证据确认生效后才 answered；仅画面消失则仍 unknown，继续核验 |
| 12 | coding seat 产出候选与测试材料 | D1 保存 diff、source revision、构建 recipe、artifact digest 和原始结果；独立验证 child 对照当前行为合同执行相关检查 | 口头“完成”不等于验证通过；高亮样例包含中文宽字符、反显、同名选项、resize 与退回 shell |
| 13 | 主体决定激活 B | S1 保存 activation intent；Update child 准备 generation 8 副本、迁移数据、追赶与 smoke | B 的验证推理仍计入同一 broker；B 不能同时启动第二条永久主体，也不能把回放发送到真实 QQ/seat |
| 14 | A 在等待验证期间发送一次准确进展，发送响应丢失 | outbox 的 Q-progress 已有 T1，网络结果 unknown；外部事实留在 OperationLedger | 升级不得把 unknown 清回 prepared；B 接管后不能因看不到 message_id 就再次发送同一逻辑进展 |
| 15 | 最终交接开始；同时又有 QQ 消息 m901 到达 | A 在 S1 后的安全边界进入 quiescing，封住新的主体决定；控制账本最终 w1 固定。独立 ingress 继续写 I:502；seat spool R 继续收事实 | w1 只约束 A 的控制决定，不能声称 I/R 都已停在同一个全球水位；保留 imported_through 向量和未接纳范围 |
| 16 | 移交任务、seat 与 broker 状态 | ActivationRecord 带现存 task revisions、Q7 状态、outbox、未知操作、所有 held allocations、ContextLease、hook/decoder/recovery 闭包；接任者承认这些占用 | 任何仍 unknown 的请求继续占 C。旧远端 agent 可继续工作，新的控制者不能重建空容量账本 |
| 17 | 提交有效交接并切路由 | 激活事务比较 expected active generation/epoch，发布 B、epoch 19、routes revision 与明确的 subject head；迁移/接管唯一 next_step_id | 旧 A 不再提交决定或发新指令；受管 companion 拒旧 fence。不能围栏的旁路仍按真实保障级别记录 |
| 18 | B 接续原主体 | B 先核对控制 head、spool 向量、inflight/unknown 与 seat live identity，读 I:502 等新事件；对账插件无需 LLM 即可运行 | 没有资源就显示等待真实请求释放；可执行时继续 step 101 之后的当前后继，而不是初始化新人格/新聊天主体 |
| 19 | Q-progress 的迟到成功回执来自旧 A | E1 接纳旧 attempt 的真实 receipt；当前 reconciliation T2 把原 operation 对账为已发，补平台 message_id | 旧 epoch 回执不会使 A 复活，也不会生成第二条进展；人类已读仍未知 |
| 20 | 新观察插件与 B 实际接手得到验证 | 验证运行 artifact digest、当前 plugin/tool index、真实高亮读取与未完 D1/消息续接；保存 activation observed 证据 | 只有这时才将升级目标记为完成；候选 build 成功与运行激活是不同状态 |
| 21 | 有效 ReplyGrant 的单一发布责任人回 QQ | outbox 按 task/logical_reply_id/result_revision 生成一次最终结果，T1 → send → E1/T2 | 平台接受与任务完成分别保存。主体消费交付摘要后不再重复复述相同最终答案 |

若在步骤 17 后 B 健康失败：恢复插件可以自主选择旧可启动闭包作为 generation 9/新的 controller epoch，导入/解释 B 已产生的事实，再继续同一 subject head。不得把控制账本/渠道记录回到旧 snapshot 后将 m901、Q-progress 或远端实际修改抹掉。无法解码的 receipt 保留原字节与 reader 引用，只阻塞相关操作，允许自主恢复所需适配器；不是要求把恢复器永远冻结。

ReplyGrant 绑定 task generation/revision、受众和逻辑回复责任，不等于绑定某个永远不变的软件进程。升级可显式移交仍有效的 grant；已失去控制权的旧 executor 不可继续派发，新 executor 仍需核对当前任务、授权与 outbox。取消或改目标时，迟到的已完成研究结果可留作事实，但不抢发已失效的回复。

## 运行时错误与可恢复动作

错误码为产品工程合同，可与插件章节的同义错误统一；不以字符串模糊匹配上游错误。下表“停止”只作用于受影响 request/operation/写入 session，不一概停止整个主体或要求人类介入。

| code | 当前调用的确定行为 | 默认后续动作 |
| --- | --- | --- |
| STALE_SUBJECT_EPOCH | 不接受 S1，不发布草稿委派；保存迟到事实 | 接任者从最后已提交 head 继续 |
| INPUT_IDENTITY_CONFLICT | 保留两个冲突原文及来源，不覆盖 | 归一化/核验 worker 调查，形成新 revision |
| DEPENDENCY_CYCLE | 不登记会死锁的 TaskWait | 自主改成后续事件、拆依赖或改变所有权 |
| REQUEST_SEAL_MISMATCH | 不发送和当前 seal 不符的内容 | 新 request revision，重新校验/排队 |
| COMPUTE_RELEASE_UNPROVEN | 保持 Allocation held | 无模型对账；原生停止/查询或已授权恢复计划 |
| NATIVE_CAPABILITY_UNAVAILABLE | 不假装 native 字段已生效 | 明确降级或委派补 adapter/companion；硬保证验收仍不通过 |
| CONTEXT_BINDING_INVALID | 旧缓存不用于续接 | 从持久历史重建/换窗，成本如实记账 |
| STALE_QUESTION / SEAT_IDENTITY_CHANGED | 不输入旧答案；保留候选决定 | inspect 当前现场、重定位或重新决策 |
| EFFECT_UNKNOWN | 不推定失败或自动重放 | 按原 operation 查询/等待/核验；必要时另建明确补偿/替代操作 |
| COMMIT_UNCERTAIN | 当前 writer 停新派发并 reopen | 以原 step/request/operation ID 查询实际提交结果；不能换 ID 盲重试 |

## 首版运行合同验收

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

## 来源与采用范围

1. **Pi Durable，固定 c60de42e98428dc1ef7440d78c23f5915966cdb3，package 1.4.0。** 本轮直接读取 [规范 §§1–2](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L41)、[任务调度与恢复](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L1993)、[effect sandwich](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2045) 及 [structured concurrency/waits](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2312)。采用原子提交后发布、意图/效果/结果、持久等待与所有权思想；Shenko 的唯一连续主体、跨节点/软件代际 authority、broker、渠道与 seat 效果协议为本产品增加的合同。Pi 文档本身不能证明这些 Rust 实现已存在。
2. **OMP，固定 0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0。** 本轮直接读取 [docs/rpc.md](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/docs/rpc.md) 的 command correlation、yield/settled 和 UI 子协议。上游 id 关联与进程内结果语义不扩大成网络重试的 durable exactly-once。
3. **NInfer，固定 64d10fbf9f8e6bfc79ab0570b94674c5b9997929；Strata，固定 011c9e600a93bbc9fd451c563501b5831c32bbca。** 上文给出本轮直接读取的资源/缓存、parking 与 engine 协议路径；实际发行构建、配置与传输仍需 adapter 契约测试。
4. **Claude 官方 Agent SDK 文档，读取日期 2026-10-22。** [User input](https://code.claude.com/docs/en/agent-sdk/user-input)、[Permissions](https://code.claude.com/docs/en/agent-sdk/permissions) 为动态官方页面；本轮网页引用 turn213view0 / turn213view1，根稿引用前自行打开。本文不凭 SDK 回调推断可接管任意现有 tmux TUI。
5. **NapCat/CLI 的已核验固定来源。** [NapCat WS 实现](https://github.com/NapNeko/NapCatQQ/blob/3420f0b76d7dc0ac92cde1663f21ea27f61e13bb/packages/napcat-onebot/network/websocket-server.ts)、[napcat-cli 事件存储](https://github.com/cyjin-yl/napcat-cli/blob/3da08c03ff491cb8b86896102b0eab06c9b0c145/napcat_cli/lib/events_sqlite.py)。本轮复用此前核验：断线事件不能凭重连自动补投，外部发送 echo 不提供端到端幂等；不再把它们当新的全领域研究。

上述上游行为只提供具体设计依据；本文的工程默认、字段名、跨模块原子批次、错误语义与 RT 用例均为 Shenko 的规范要求。它们允许以后按版本演进，不因此构成永久冻结的内核接口。

