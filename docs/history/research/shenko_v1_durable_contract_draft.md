# Shenko V1：持久执行与自升级合同草案

> 供主 SPEC §41 合并的只读研究产物；不是已经实现或测试通过的代码。研究基线：现有 `personal_agent_spec_v0_3_0.md` §9、§10.6、§35、§36；上游固定为 `earendil-works/pi@c60de42e98428dc1ef7440d78c23f5915966cdb3`。本稿不改变“当前阶段只研究和写规格”的授权范围。

## A. 根本缺口与来源边界

现有要求方向正确，但尚不足以让两个实现者写出相同的恢复器：没有精确规定每笔事务包含什么、哪类调用能修改哪一条记录、storage 抛错后能否继续、旧 invocation 的回执如何进入新版本，以及 A/B 激活究竟以哪条记录为准。

| 现有位置 | 已有内容 | 本稿补齐 |
| --- | --- | --- |
| §9.5，L384–398 | prepare/copy、catch_up、smoke、handoff、retire | ActivationRecord、独立 spool 水位、可比较交换的唯一选择记录、切换失败后的唯一恢复入口 |
| §10.6，L460–480 | 原子提交、持久任务树、intent/effect/receipt、取消 | TaskRecord 联合状态、T1/T2 具体批次、commit outcome 不明时失效并重开 |
| §35.7，L2203–2211 | A 停决定、追平、B 接管 | 哪些 writer 被封口，什么仍可接收事实，何时 B 才取得执行权 |
| §36.5–36.6，L2303–2323 | operation/attempt、unknown、补偿、旧版本回执 | 精确字段、Attempt 的 armed 窗口、EvidenceIngress、版本 reader 与调用权限分离 |

以下 `[P]` 表示直接吸收 Pi 固定版本规范；`[S]` 表示 Shenko 针对自身产品目标推导；`[U]` 表示用户已经裁决的需求。`[P]` 不是声称本次运行过 Pi 的实现或测试。

主要上游锚点：

- **P1 原子与可见性**：[spec.md L41–78](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L41)、[L1696–1731](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L1696)。
- **P2 任务与恢复**：[L1752–1812](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L1752)、[L1932–2043](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L1932)。
- **P3 Effect sandwich 与 terminal receipt**：[L2045–2098](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2045)。
- **P4 取消、版本和所有权**：[L2100–2309](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2100)、[L2311–2407](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2311)。
- **P5 工具重放**：[L3157–3214](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L3157)、[L4314–4372](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L4314)。
- **P6 存储失败**：[L5256–5316](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L5256)。
- **P7 迁移与非目标**：[L1591–1624](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L1591)、[L5580–5594](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L5580)。

Pi 并未在这些规范中提供 Shenko 的多节点选主、原始外部效果事实账本、完整 A/B 升级、跨版本迟到回执或任意第三方进程强杀。这些应标为 Shenko 扩展。尤其不能把 `ctx.effect` cleanup 或 Pi 的 abort handler 当成外部事务逆操作。

## B. 18 条可执行规范

### X01. 同一提交域内只有一条写入序列，事实只在提交后发布 `[P+S；P1]`

`CommitRef = { ledger_id: UUID, seq: uint64 }`。`seq` 只在同一 `ledger_id` 内严格递增；允许空洞，不以墙钟或不同节点的 seq 比大小。经 JSON 传输时 uint64 使用十进制字符串。跨账本因果关系记录 `causal_refs[]`，不伪造一个全局顺序。

Task/Operation/Attempt/Receipt 等实体采用不复用的全局唯一 ID；记录有 `record_schema_version`。可变投影另有单调递增 `record_rev`，原始事件与 EvidenceReceipt 不原地覆盖。一次提交暴露完整批次，读者不能看到半个 Task 状态加半个 outbox。UI、通知与模型可读业务进展只引用已提交版本；流式输出可以小批次提交后推送。动画不属于业务事实。

`StorageProvider` 提供单提交域的原子批次、条件更新、稳定读取与重开，不把这些实现焊进 Rust 最小内核。跨 SQLite、CAS、远端平台的动作不属于一笔原子事务。

### X02. TaskRecord 必须有可以恢复的完整状态 `[P+S；P2/P4]`

下面为规范数据形状，不是要求使用 TypeScript 实现。`ObjectRef` 指带内容 digest、字节数、media/schema 类型和 store namespace 的持久对象引用；凭据只能用 secret handle。

```text
TaskRecord {
  record_schema_version: u32 = 1,
  task_id: UUID, task_rev: u64,
  subject_id: UUID, conversation_id: UUID,
  owner: { kind: conversation | task, id: UUID },
  background: bool = false,
  kind: string, definition_version: u32,
  implementation_ref: { plugin_id, artifact_digest, registry_revision },
  input_ref: ObjectRef,
  state:
    pending    { checkpoint_ref }
    | running  { checkpoint_ref, invocation_id, mode: run | abort, epoch }
    | waiting  { checkpoint_ref, on_task_ids[], join: fail_fast | all_settled }
    | sleeping { checkpoint_ref, until_utc }
    | completing { outcome_ref }
    | terminal   { outcome_ref },
  abort_request?: { cancel_id, decision_ref, requested_commit, reason, scope },
  memos?: map<string, bounded JSON>,
  residual_case_ids: UUID[],
  no_progress_crashes: u32,
  first_run_at?: timestamp, terminal_at?: timestamp,
  created_commit: CommitRef, updated_commit: CommitRef
}
```

`owner`、task ID、conversation ID、输入在创建后不改；确需换输入则新建任务并关联前任。定义迁移可以转换 input/checkpoint 的编码，不能偷偷改用户任务语义。`terminal` 之后不再运行该任务代码；后续新证据更新关联 Operation 的事实视图，并创建后续任务，不重写历史 terminal outcome。

`background=true` 只允许 conversation-owned task。ordinary owned work 沿 task/conversation ownership 遍历，跳过 background task 及其整个子树；history parent 不参与取消/完成遍历。明确直接取消一个 background task 时仍取消它的 ordinary 子树。

`sleeping` 是子任务等待轮询/定时工作的能力，不把永续主体改为低频 Tick。主体拥有长期 root task；有限主体 step 不能直接拥有所有长期 child，否则按 Pi 的所有权规则每一步都会等待孩子结束。具体 SubjectStep 合同由 S0/S1 定义。

### X03. Operation 与 Attempt 分开：一次意图不等于一次网络尝试 `[S；依据 P3/P5]`

```text
OperationRecord {
  record_schema_version: u32 = 1,
  operation_id: UUID, operation_rev: u64,
  task_id: UUID, logical_key: string,
  operation_name: string,
  input_schema_version: u32, output_schema_version: u32,
  final_input_ref: ObjectRef, input_digest: Digest,
  contract_ref: { plugin_id, artifact_digest, contract_digest, receipt_reader_ref },
  resource_bindings_ref: ObjectRef,
  authority_ref: { delegation_id, delegation_revision, decision_ref },
  acceptance_predicate_ref: ObjectRef,
  retry_contract_ref: ObjectRef,
  checkpoint_scope_ref?: ObjectRef,
  state: prepared | in_flight | reconciling | resolved,
  outcome?: succeeded | failed | cancelled | interrupted,
  effect_status: none | partial | complete | unknown,
  retry_of?: UUID, compensates?: UUID,
  possible_duplicate_of: UUID[],
  prepared_commit: CommitRef,
  evidence_through?: CommitRef
}

AttemptRecord {
  record_schema_version: u32 = 1,
  attempt_id: UUID, operation_id: UUID, attempt_no: u32,
  invocation_id: UUID, worker_id: UUID, node_id: UUID,
  epoch: u64, implementation_ref: VersionRef,
  dispatch_stage: armed | accepted | running | ended | unknown,
  armed_commit: CommitRef,
  provider_handle?: { provider_id, backend_instance_id, handle_type, value, version },
  provider_idempotency?: { key, namespace, expires_at, argument_digest },
  transport_ref?: ObjectRef,
  started_at?: timestamp, last_observed_at?: timestamp,
  final_receipt_id?: UUID
}
```

`(task_id, logical_key)` 唯一。重新访问相同 key 时，operation name、最终参数、实际目标和合同必须匹配；不匹配返回显式冲突，不偷偷创建第二次操作。`attempt_id` 每次真实执行尝试不同；provider 幂等 key 在同一幂等语义下重试时保持不变。

Bindings 至少记录实际 provider、账号/资源 identity、runtime/node、挂载 generation、相关 observation/revision 和能力等级；不能只记录 `/work` 或一个可重绑 URL。输入、bindings、acceptance 和重试合同在 T1 后冻结。`effect_status` 只表示声明域内已查明的效果，不等于任务成败。`failed` 可以伴随 `partial` 或 `unknown`，`cancelled` 不推导 `none`。

### X04. EvidenceIngress 接收事实，不承接旧调用的控制权 `[S]`

```text
EvidenceReceipt {
  record_schema_version: u32 = 1,
  receipt_id: UUID,
  operation_id?: UUID, attempt_id?: UUID,
  source_id: string, source_instance_id: string,
  source_event_key?: string,
  producer_epoch?: u64,
  producer_schema_version?: string,
  raw_ref: ObjectRef,
  decoder_ref?: VersionRef,
  decode_status: decoded | uninterpreted | invalid,
  observation_level?: transport_written | provider_accepted
                      | remote_completed | postcondition_verified,
  normalized_ref?: ObjectRef,
  observed_at?: timestamp,
  received_commit: CommitRef
}

ReconciliationCase {
  case_id: UUID, operation_id: UUID,
  supervisor_task_id: UUID,
  reason: string,
  state: open | working | waiting | resolved | acknowledged_unknown,
  evidence_refs: UUID[],
  next_action_ref?: ObjectRef,
  decision_ref?: ObjectRef,
  created_commit: CommitRef, updated_commit: CommitRef
}
```

EvidenceIngress 的 E1 提交保存原始 bytes 的持久引用、来源、对应关系和处理状态。它可吸收旧 epoch、已取消 task、旧插件或未知 schema 的回执；这些事实不会使旧 invocation 重新获得 `commit`、发信、输入终端或创建任务的权力。无法识别 operation 的原始回执进入待关联队列，不丢弃。

来源有稳定 event key 时以其作用域去重；同 key 不同原文是冲突证据，不能覆盖前一份。只有相同内容 hash 不能证明两次外部事件是同一事件。E1 可由合法的 T2 同笔完成；已经 E1 落账的证据可被 T2 引用，不必再复制。

### X05. 激活选择必须成为可恢复的数据 `[S]`

```text
ActivationRecord {
  activation_id: UUID,
  from_generation: Digest, to_generation: Digest,
  from_state_ref: ObjectRef, to_state_ref: ObjectRef,
  migrator_ref: VersionRef, recovery_closure_ref: ObjectRef,
  contract_revision: string,
  phase: prepared | copying | catch_up | smoke | handoff
         | activated | observing | retired | abandoned,
  w0?: CommitRef, w1?: CommitRef, imported_through?: CommitRef,
  ingress_cursors_ref?: ObjectRef, evidence_cursors_ref?: ObjectRef,
  pending_operations_ref?: ObjectRef, inference_allocations_ref?: ObjectRef,
  expected_selector_revision: u64, target_epoch: u64,
  smoke_evidence_ref?: ObjectRef, rollback_plan_ref: ObjectRef,
  last_error_ref?: ObjectRef
}

ActivationSelector {
  subject_id: UUID, selector_revision: u64,
  active_epoch: u64, generation: Digest,
  state_ref: ObjectRef, activation_id: UUID,
  route_authority_ref: ObjectRef
}
```

Selector 的成功条件更新是激活的线性化点；磁盘上的 executable symlink、进程 PID、Web UI 中显示的版本都不取代它。`ActivationStore` 提供满足该部署 profile 的持久 compare-and-swap。单机 SQLite 的条件更新不自动构成多节点共识。

### X06. 调度器执行明确的任务转换，不凭 transcript 猜进度 `[P+S；P2/P4]`

| 转换 | 必须满足的条件/同笔动作 |
| --- | --- |
| pending/waiting/sleeping → running | 条件已满足；定义可读；迁移成功；无有效取消祖先；提交新 invocation_id 与 epoch 后才运行 handler |
| running → running | handler 提交不同的完整 checkpoint；引用的数据已经持久 |
| running → waiting/sleeping | checkpoint 与等待条件一同提交；结束 invocation；不占模型执行许可 |
| running → completing | 已决定 outcome，但仍有 ordinary owned work；本任务不再运行新 phase |
| running/completing → terminal | ordinary owned work 已结束；任务结果、结果 entry、submission 结算与 task-scoped 状态退休在规定批次完成 |
| crash 后 running → pending | 保留 checkpoint、abort mark、Operation/Attempt；增加无进展崩溃计数，随后按恢复规则运行 |
| 缺定义/版本不兼容/迁移失败 | 保留可恢复状态，显示 blocked reason；安装可用实现后再评估，不因缺代码直接标成功或清空任务 |

显式 durable wait 必须检查等待图（包括 owned-work finishing 边）中的环；发现环拒绝该等待提交并返回路径。调用结束后，旧 invocation 的新 checkpoint/child/outbox 写入一律拒绝。

普通 child 不应活过 owner 的 terminal。若任务要结束而仍有不确定外部效果，须先把这些 Operation 与后续查询职责交给持久 supervisor/reconciliation case；这不是宣称外部效果已停止。主体 root 和明确 detached 的持续服务拥有自己的生命周期。

### X07. T1：先提交真实的 intent 和 armed attempt，再允许外部执行 `[P+S；P3/P5]`

T1 是一个原子批次，至少包含：

1. 先按稳定调用 identity/logical key 查已提交记录。同一次 admission 的重复请求若参数匹配，只返回已存在 operation/attempt/result；不同 payload 返回冲突。已提交命中不因调用者携带的旧 revision 而被误当成新动作，也不授权再次发送。只有新 admission 或协调器已决定的新 attempt，才以预期 task_rev、invocation_id、epoch 验证当前调用仍有效，并检查取消与现行委派。
2. 固定所有 before-hook/参数修复之后的参数、目标绑定、重试合同和 acceptance predicate。
3. 创建或核对唯一 OperationRecord，创建唯一 AttemptRecord，令 attempt=`armed`、operation=`in_flight`。
4. 保存相应 task checkpoint 与必要的 outbox/资源占用引用。

步骤 1–4 任何一项失败时整笔不成立，不发出这个 attempt。仅有 `prepared`、从未存在 armed attempt 的 operation 不能由执行器发送。准备阶段可以单独持久化 prepared operation；也可与首次 T1 合并。

`armed` 表示“这个 attempt 获准尝试效果”，不表示 bytes 已到平台。**T1 成功到收到可验证结果之间都属于可能已经发生效果的窗口**，包括崩溃实际上发生在第一字节发送前的情况。不能通过新增一个 `sent=true` 的本地标记消除这个窗口。

### X08. Effect 在事务外执行；提交幂等与业务幂等分别处理 `[P+S；P1/P3]`

运行 shell、tmux 输入、SSH、模型请求、QQ 发送、Git push、对象上传均在 Session/SQLite 写事务之外执行；不持有写锁等网络、推理、用户或 child。外部效果所需模型请求继续经过全局 broker。

T1 后只能执行已冻结的那份操作。重新展开 discovery、改参数或改目标必须成为新的明确意图，不在 replay 时偷偷再跑 before-hook 改写它。provider 返还外部 handle 时应尽早通过提交保存；handle 带 backend instance 身份，重启后验证仍有效，不能把旧 slot ID 当永久资源。

同一调用本身跨多个不可原子的外部域时，记录可区分子 Operation 及依赖。例如“改文件→git push→发 QQ”分别拥有自己的 T1/T2；三步不能共用一条“全部完成/全部回滚”。

### X09. T2：证据与完成状态不得形成撕裂 `[P+S；P1/P3]`

原始结果对象先按 Storage profile durably seal。T2 在一个批次中：新增/引用 EvidenceReceipt；更新 Attempt 和 Operation 投影；追加结果 entry；推进当前合法 task checkpoint 或 outcome；结算相应 submission；登记由结果产生的 outbox；使可重建视图得到该 commit 的通知。

只有确有匹配结果的 acceptance predicate 才能记录 succeeded。`transport_written`、provider accepted、远端执行完成和应用后置条件通过是不同层级。发 QQ 得到平台消息 ID，不等于用户已读；tmux 接受按键，不等于 coding agent 已接收或完成任务。

若任务已取消、epoch 已切换或 invocation 已结束，回执仍由 E1 保存；旧 callback 不能顺便完成以上任务推进。当前恢复器可读取证据，在自己有效的 T2 中处理剩余状态。终端任务和已发布事实不会因软件回退而被改写成从未执行。

### X10. 存储失败使当前 writer 失效，不能原地重试 `[P；P1/P6]`

T1、T2 或激活 CAS 的 storage settlement 一旦返回“不确定/错误”，当前 Session/Writer 封闭新写入与新 dispatch，保留已提交视图，停止以旧内存状态继续推进，按相应 scope 重开。

恢复后以相同稳定 submission/operation/attempt/activation identity 查询实际记录：存在则采用已提交结果，不存在才按重开后的条件重新提出提交。不能把 exception 当作没提交。已经进入 storage settlement 的批次，不因调用者取消等待而半途撤回；取消等待不生成 durable cancel。

存储 adapter 可以在其方法内部完成它能证明安全的 transient retry；不得把不确定提交交给上层 while-retry。新打开后如缺失已确认 CAS 对象或出现损坏，是 corruption，不按“没有这条记录”补做外部动作。

本条采用主规格规定的 SQLite durability profile，不复制 Pi 的同步默认值。Pi JSONL 即使 fsync=true，固定规范 L5399–5417 也没有在普通提交时显式 flush main marker，已 ACK 尾提交仍可能丢失，不能借其名称推导 Shenko 的断电承诺。

### X11. 恢复按记录分类；unknown 不改名为失败 `[P+S；P3/P5]`

| 可恢复事实 | 允许的动作 |
| --- | --- |
| 没有已提交 T1 | 不存在已获准的此 attempt；重新准备或取消 |
| prepared 且从未 armed | 可按当前有效授权执行 T1；不能已向外发出 |
| armed/accepted/running，无 conclusive outcome | operation → reconciling；先走对应合同的查询、有效幂等重试或保持 unknown |
| 已持久 provider handle，远端还活着 | 重新绑定并追踪原 handle；不新开一份同任务 |
| T2 已完成 | 读取原结果继续，不再次执行效果 |
| task 已 terminal，但外部后来补证 | E1 追加事实，更新 Operation 视图并唤起后续协调；不复活旧 task |
| 格式暂时不可解码 | 保留原始 bytes、版本和引用闭包；相关 operation 保持待核验；不影响无关联的已确定工作 |

`resolved` 只能在结果和实际效果按合同已有明确证据时成立。无法消除的不确定性可由主体/获委派 child 选择 `acknowledged_unknown`：记录决定和剩余影响，允许其他工作继续；以后收到新证据仍重新评估。不能伪造“零副作用”。

Operation 的合法转换为 `prepared → in_flight`（T1）、`in_flight → resolved`（有充分证据的 T2）、`in_flight → reconciling`（结果不明）、`reconciling → in_flight`（已决定且满足合同的新 attempt）、`reconciling → resolved`（查证完成）；`prepared → resolved` 且 `outcome=cancelled` 仅适用于不存在任何 armed attempt 的撤销。已 resolved 后出现矛盾证据，可以通过新协调事件转回 reconciling；原结果和其证据不删除。

Attempt 的通常转换为 `armed → accepted → running → ended`，允许以证据跳过中间状态。任一未结束阶段失去可确认执行状态可记 `unknown`；后来证据允许 `unknown → accepted/running/ended`。早先已知的 accepted 或 running 事实始终留在证据链中，不因当前是否完成未知而被撤销。`ended` 只说明该 attempt 不再执行，成功及效果完整性另由 outcome/effect_status 表达；同一 attempt 永不重新 armed，再次执行必须创建新 attempt。

### X12. 自动 replay 需同时满足持久合同和当前合同 `[P+S；P5]`

必须逐项确认：输入/目标与原意图一致；原合同与当前实现都允许重放；provider 幂等 key 的 namespace、有效期、参数约束仍成立，或条件写入的前置 revision 仍成立；最新授权与 target identity 匹配。key 过期不能通过换一个新 key 冒充原请求去重。

嵌套调用以稳定 logical key 找回原 child/operation。相同 key 不同工具名、参数或目标返回冲突；不得按照新代码调用顺序默默对应旧效果。动态分支调用推荐明确语义 key。runtime memo 的候选写入与返回赢家在一个提交内，先写者胜出；大对象放对象存储。

tmux send-keys、按回车确认、任意 shell、无端到端幂等承诺的消息发送默认不可自动 replay。主 Agent 仍可自主决定再次执行：创建新 operation，保存 decision_ref 和 `possible_duplicate_of`，显示重复效果的不确定性。这是新决定，不是恢复器偷偷重发。

### X13. Durable cancel、调用结束与平台停止分别表达 `[P+S；P4]`

取消顺序：提交 `abort_request` → 阻止新 run phase/新 T1 → signal 当前 invocation → 等待/终止其受管执行 → ordinary owned work 收敛 → 新 abort invocation 运行取消/清理策略 → 提交 outcome。

已经 `completing` 的任务保留原先决定的 outcome；后来的取消只对其仍存活的 owned work 生效，不再运行该任务的 abort handler。已经 terminal 的任务不再执行取消代码；需要补偿时创建新任务。

取消标记出现后，旧 run invocation 的 task commit 被拒绝；它已经发出的请求可能继续，回执仍进入 E1。调用者取消等待、网络断开、close、worker 本地退出，都不能代替 provider 停止证据。close 用于可恢复停机，不自动给任务写 aborted。

取消不合作的代码时只使用 runtime/进程实际支持的隔离和终止能力。不在“同进程任意 Rust callback”上承诺可靠强杀。远端进程无法证明停止时记录 stop pending/unknown 及后续 supervisor，不把主体永久卡在等待它退出的全局写锁里。

### X14. 补偿是有独立事实的新 operation `[P+S；P3/P4]`

补偿创建新 operation，`compensates=原 operation_id`，拥有独立的参数、权限、T1、attempt、T2、重试和 unknown。原效果及原回执继续存在。可以补偿部分效果，也可以失败；task outcome 应区分“取消已请求”“已停止”“已完成补偿”“补偿结果未知”。

一旦 abort handler 自身在补偿中崩溃，重开后应恢复其 checkpoint/补偿 operation，而非无条件再次撤回、删文件或退款。补偿不能绕过 child ownership 规则：要么在可恢复 abort phase 内编排，要么交给明确 supervisor 所有的持久补偿任务。取消流程不会递归取消掉唯一负责善后的任务。

缺少旧 task 定义时不运行猜测出来的补偿。Pi 的 orphaned 表示 task 代码未执行、效果可能未清理；Shenko 保留该语义，并创建带原始记录的修复/协调任务。没有代码不是成功完成。

### X15. 版本解释权与继续执行权分离 `[P+S；P4/P7]`

任务 reservation 固定 definition/version、implementation digest 与 registry snapshot；每个可交接 phase 边界再检查候选。迁移 checkpoint 与改为 running 同笔提交。缺定义、较旧实现无法读新记录或迁移失败，只阻塞相关任务；旧实例不能通过强制把版本号改小来运行。

正在执行的效果保留它调用时的 contract/decoder/reconciler。新插件接管不能用新参数 schema 重解释旧输入。持有旧 ABI 的 handler 在返回/交接前保留其运行闭包；不合作 handler 只能按真实隔离能力停止，停止后仍查外部事实。

迟到回执按原 operation、attempt、producer schema 路由到保留 reader，随后作为 E1 事实保存。producer epoch 只用于关联，不作为“丢弃事实”的理由；新控制 epoch 也不授予旧 callback 新命令权。无 reader 时保存 uninterpreted，并由主体自主安装 reader、迁移或派 child 调查。reader/ledger/验证器本身都允许升级；保留是当前恢复依赖，不是永久不可修改 Ring 0。

### X16. A/B 准备阶段复制可运行状态，B 尚无真实执行权 `[S+U]`

ActivationRecord 依次经过 prepared → copying → catch_up → smoke → handoff。A 正常运行期间，自一致 w0 复制到 B 副本；migrator 消费后续确定提交，记录 imported_through，保证重跑同一增量不重复创建事实。对象闭包和所需解码器一并可取。

B 的 smoke 用已记录结果、mock transport、明确隔离测试账号或显式独立探测 operation；没有旧 task 的真实 writer lease，不回放 QQ 发送/按键，也不启动第二条无限对外主体。通过标准绑定 contract revision 与实际 artifact digest，不能把“进程启动了”视为消息、账本、迁移及资源调度全通过。

恢复闭包包括 A 程序与数据、B 新事实的 reader/importer 或可继续承接这些事实的恢复运行时、对象/secret handles、rollback plan 与相应验证证据。B 不要求与 A 保持同 schema；需要的是明确可运行的恢复路径。

### X17. A/B 切换只认一次选择提交；回退也是新激活 `[S+U]`

handoff 执行顺序：

1. 在主体明确的有限 step/轮次边界停 A 的新决定，封闭旧数据域的新任务变更与 dispatch admission；已获准的在途效果继续被跟踪。
2. 形成最终控制账本水位 w1，B 导入至 w1；核验 task/operation/outbox、原始记忆、人格/灵魂、删除与撤销、未决推理占用的闭包。
3. 独立 ingress、Seat companion、EvidenceIngress 可以继续接收，但其进度用各自 cursor 向量记录；不能假定 w1 同时冻结外部世界。交接明确哪个消费者从哪个 cursor 继续，不跳过切换期间的新消息。
4. B 的候选状态与恢复闭包满足持久条件后，在 ActivationStore 同笔提交 selector 的预期 revision CAS、active_epoch 单调递增和 ActivationRecord=activated。此提交之前 B 无真实执行权；成功之后 A 无新决定/新 dispatch 权。
5. 路由和各受管 writer 采用新 selector revision/epoch；B 从持久状态开始一条主体序列。旧在途请求占用不会因切 epoch 被释放，旧回执由 E1 接续。

若激活提交结果不确定，先重开读 selector，不按“最后一次调用报错”决定启动谁。切换前失败可放弃 B 并以新的有效 invocation 恢复 A；切换后失败创建新的 ActivationRecord，epoch 不倒退、不复用旧 writer token。

B 已产生的新事实必须通过既定恢复路径进入将接管的状态或兼容恢复服务。不能把 A 的 w0 数据直接当“现在”启动后重发旧任务。若某条记录暂不可读，保留并停该项的自动执行，其余已确定工作仍可继续。工作区回退不还原权限撤销，不消灭已发 QQ/远端命令。

本地 fencing 只能约束受管 writer；目标端真正支持 fencing 时才承诺拒绝旧 token。原生人类输入、绕过 companion 的程序和目标不支持的外部动作不能被描述为已完全围栏。

### X18. 这些是可替换插件的行为合同，不是人类审批点 `[S+U]`

DurableExecution、Storage、EvidenceIngress、Reconciler、Update/Migration、验证器与恢复器均可替换，按当前有效版本完成交接。正常修复、安装 reader、迁移、重试策略选择、构建、烟测、激活与回退由主体/获委派 child 在现有授权内自主完成；不添加逐 PR、逐更新或 unknown 一律问人的固定门槛。

V1 默认在同一 checkpoint 连续 5 次非正常重启且无持久 phase 进展后 park 该任务；正常 close 不计数，变更 checkpoint/进入 durable wait/完成会清零，重复写同 checkpoint、memo 或普通日志不算进展。该值和策略可由主体自主调整。达到阈值时仅 park 相应任务并委派诊断；主体可换实现、重试或停止它。不会让一个坏插件不断击穿整个主体，也不把所有主体推理改成定时唤醒。

缺少当前任务所需的账号、机器权限或不可推断的用户意图时，Agent 自主选择是否以及通过何种可达渠道联系用户。源事实、插件实现、恢复能力与权限来源各自记录；自主性不通过伪造成功、清空失败历史或删除未解释回执实现。

## C. 必须可以故障注入验证的矩阵

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

## D. 合并时应保持的接口名称

- `CommitRef`、`TaskRecord`、`OperationRecord`、`AttemptRecord`、`EvidenceReceipt`、`ReconciliationCase`、`ActivationRecord`、`ActivationSelector`。
- `T1` = intent + attempt armed；`T2` = receipt/outcome/checkpoint 原子落账；`E1` = 独立事实接收。主体输入/决定使用运行合同的 `S0`/`S1`，避免同名。
- Storage 默认 PRAGMA 和持久等级由 root 在 §41 统一；本稿不另外指定冲突的 profile。
- 所有额外模型调用，包括协调、迁移诊断和 smoke，继续遵守实际 InferenceBroker；waiting、远端长任务与 durable polling 不持续持有推理执行许可。
