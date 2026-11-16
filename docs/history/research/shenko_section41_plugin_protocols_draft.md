# §41 规范接口补充：插件操作、注入、自解释与版本交接

状态：**拟定的规范接口，尚未实现或验证**。本稿供 §41 合并；P1–P8 是临时分节号。类型描述持久语义和调用合同，不规定最终 Rust ABI、WIT 或 IPC 编码。本稿中的服务均通过既定 colibri-rs 插件主干提供，不新建第二套插件框架，不将任何服务、解释器或救援器规定为永久不可替换。

本稿来源：REF-USER、REF-DESIGN，以及 REF-DURABLE、REF-FABRIC、REF-COLIBRI、REF-OMP。下文新增类型、状态、默认处理和验收是 **DERIVED**；引用上游表示具体借鉴范围，不表示本产品已经具备该实现。

## P1 共用身份、持久化与错误语义

### P1.1 共用类型

```ts
type Digest = string;
type LedgerSeq = bigint;             // 仅在对应 ledger_id 内有序
type RuntimeGeneration = string;     // 一次激活的软件闭包，不是模型 generation
type ArtifactRef = { digest: Digest; locator: string };
type SchemaRef = { name: string; major: number; digest: Digest };
type VersionedRef = { artifact: ArtifactRef; symbol: string; schema: SchemaRef };
type DataRef = ObjectRef;             // 复用共用 ObjectRef；codec/schema 写入其类型元数据

type ProtocolError = {
  code: string;
  scope: "call" | "operation" | "request" | "activation" | "writer";
  retry: "same_key" | "after_reconcile" | "new_revision" | "never_automatically";
  operation_id?: string;
  commit_ref?: CommitRef;
  details: Record<string, unknown>;
};
```

`locator` 是可更新的寻址信息；`digest` 才是工件或数据身份。相同 URL 指向了新内容，不视为同一工件。`SchemaRef.major` 表示协议解释规则；摘要标识精确 schema，不能仅凭插件包的 semver 判断兼容。

凡 API 返回持久成功，均已完成该存储 profile 承诺的提交等级。界面、主循环、hook observer 和调用者只接收已提交状态；缓存、内存变量或流式 token 不能构成持久成功回执。提交等级通过存储插件报告，不能把 Disposable 或未同步写入标成断电可恢复。

一次本地事务可以同时提交操作记录、任务检查点、工具结果引用和待投递事件。网络、模型、进程、SSH、tmux 和其他外部操作在事务之外执行。事务内禁止等待模型或子任务，禁止递归开启同一 writer 的事务。

调用者取消等待，仅结束该次等待。它不撤销已接纳的 intent，不等同 `cancel_operation()`。进程关闭也不自动成为业务取消；恢复时由持久状态确定继续、查询、取消或待处理。

### P1.2 不确定提交

提交返回 `COMMIT_UNCERTAIN` 时，该 writer 停止接纳新的变更和新的外部派发，进入恢复。恢复重新打开存储并查询原来的 admission key；不得用一个新 key 猜测性重做。调度器撤回故障 invocation 的继续派发权；仍可能执行的后端请求保留为不确定占用，经后端完成、停止或核对协议结算。writer 故障本身不能成为释放实际 Engine 占用、放行超额请求的证据。

单个插件的参数错误、迁移失败或 hook 失败不自动变成整个 writer 故障。错误作用域固定见 P7；实现不能把所有异常统一转为“重试一下”。

来源：REF-DURABLE [§4 的事务/外部效果边界](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L1676)、[§10.1 的错误分类](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L5256)；本产品采用上述行为原则，不要求逐字移植其 Storage API。

## P2 Operation / effect 规范接口

### P2.1 静态合同与本次实际绑定

```ts
type OperationContract = {
  capability_id: string;
  operation_name: string;
  input: SchemaRef;
  output: SchemaRef;
  effect_contract: SchemaRef;
  implementation: VersionedRef;
  resource_resolver: VersionedRef;
  receipt_reader: VersionedRef;
  reconcile?: VersionedRef;
  compensate?: VersionedRef;
  retry: "no_automatic_retry" | "external_idempotency" | "proved_repeatable";
  cancel: "before_dispatch_only" | "cooperative" | "external_confirmed_stop";
};

type ResourceBinding = {
  resource_id: string;
  kind: "managed_snapshot" | "host_bind" | "remote" | "external_service";
  target_identity: Record<string, unknown>;
  coverage: "enumerated" | "bounded_scope" | "opaque_scope";
  scope: Record<string, unknown>;
  observed_revision?: string;
  precondition?: DataRef;
  on_restore: "restore" | "retain" | "reconcile" | "unsupported";
  snapshot_domain?: string;
};

type OperationIntent = {
  logical_key: string;
  task_id: string;
  requested_capability: string;
  offered_contract_digest: Digest;
  final_arguments: DataRef;
  parent_operation_id?: string;
  retry_of?: string;                 // 新 operation 与旧意图的关联，不是 attempt retry
  possible_duplicate_of: string[];
  compensates?: string;
};

type PreparedOperation = {
  operation: OperationRecord;
  contract: OperationContract;
  bindings: ResourceBinding[];
  recovery_closure: VersionedRef[];
};
```

`OperationRecord`、`AttemptRecord`、`EvidenceReceipt`、`ReconciliationCase` 是持久执行合同的唯一规范记录。`PreparedOperation` 仅是解析 `OperationRecord` 引用后得到的只读视图，不创建另一份 operation 数据库记录。`OperationIntent.logical_key` 直接映射 `OperationRecord.logical_key`，`final_arguments` 映射 `final_input_ref`；绑定、合同与 recovery closure 使用该记录的引用闭包。

以下规则构成静态合同与实际边界的分工：

1. `OperationContract` 描述该版本能提供的保证；`ResourceBinding` 记录本次调用实际作用的位置。插件宣称支持快照，不使一个 host bind mount 自动进入快照。
2. 首次接纳先完成参数修复、参数 hook、schema 校验，再解析目标并提交规范 OperationRecord。`offered_contract_digest` 解析为 sealed request 所绑定的合同；参数解释与 `resource_resolver` 使用该版本工件或显式兼容适配器，不查询当前同名定义代替。旧 resolver 不可用则返回合同不可用。重放不重新运行这些变换，不重新解释旧参数，不按当前挂载表悄悄选择另一目录。
3. `coverage=opaque_scope` 是合法的诚实合同，例如任意 shell。它列明可触及的宿主/网络范围和已知恢复边界，不伪造完整文件清单。已观察到的新效果通过 receipt 增补事实，不能事后声称未声明的外部效果已被快照覆盖。
4. 规范记录的 `authority_ref` 引用实际执行权限或凭据句柄，不能保存可泄露的私钥、token。每次真正派发重新核验当前权限与目标身份；旧 intent 不冻结一份永不撤销的权限。
5. 资源身份或要求的 revision 已改变时，返回 `BINDING_STALE`。调用者可重新观察并以新 intent 明确接纳新目标；恢复器不得把旧操作无声改绑。

### P2.2 调用接口与幂等身份

```ts
type AdmissionOptions = { expected_task_rev?: bigint };
type CancelReceipt = {
  operation_id: string;
  cancel_key: string;
  cancel_task_id: string;
  commit_ref: CommitRef;
  already_requested: boolean;
  view: OperationView;
};
type RetryBasis =
  | { kind: "confirmed_not_executed"; evidence: DataRef }
  | { kind: "external_idempotency"; key: string; evidence: DataRef; valid_until: string }
  | { kind: "proved_repeatable"; contract_digest: Digest; evidence: DataRef };

prepare_operation(intent: OperationIntent, options?: AdmissionOptions): Promise<PreparedOperation>;
dispatch_operation(operation_id: string): Promise<OperationView>;
retry_operation(operation_id: string, retry_key: string, basis: RetryBasis): Promise<OperationView>;
get_operation(operation_id: string): Promise<OperationView>;
cancel_operation(operation_id: string, cancel_key: string): Promise<CancelReceipt>;
reconcile_operation(operation_id: string, reconcile_key: string): Promise<OperationView>;
append_receipt(receipt: EvidenceReceiptDraft): Promise<{
  receipt_id: string; commit_ref: CommitRef; duplicate: boolean
}>;
wait_operation(operation_id: string, after_operation_rev?: bigint): Promise<OperationView>;
```

`prepare_operation()` 的唯一键是 `(task_id, logical_key)`。接纳先查询该 key：同 key、相同语义输入返回已有 operation；同 key、不同工具、合同或参数返回 `INTENT_KEY_CONFLICT`，不覆盖旧记录。只有不存在该 key 时才检查 `expected_task_rev` 等新接纳条件；后续任务提交造成旧 expected revision 过期，不阻止幂等命中取得原 receipt。revision 前置条件不属于 intent 的语义指纹。比较使用协议规定的 canonical encoding 或等价结构比较，不使用浮动格式化 JSON 文本。

`logical_key` 是父任务中稳定的语义调用位置；不是一次 provider tool-call ID，也不是时间戳。嵌套调用把子 intent 的接纳和父任务对该子 operation 的引用放在同一事务中。恢复后相同 key 重新取得同一孩子及其结果，不能再建一份相同子任务。代码升级改变一个已使用 key 的含义，必须迁移或使用新 key。

`operation_id` 在所有重试中稳定；每次允许的外部尝试使用不同 `attempt_id`。默认同一 operation 不得并存两个具有外部执行权的 attempt。上次执行状态未知时只能查询/核对，或者使用已证实仍有效的外部幂等合同重试。框架生成了 operation ID，不代表 QQ、SSH 或任意 HTTP 服务已经支持幂等。

`dispatch_operation()` 仅对从未派发的 prepared operation 原子接纳首个 attempt；已接纳的调用返回当前视图，不新增尝试。`retry_operation()` 只在合法 `reconciling → in_flight` 转换中创建后续 attempt，先按 `(operation_id, retry_key)` 查重，再检查 basis 的适用范围和当前有效性；合同声明与本次证据均不支持时返回 `RETRY_NOT_PROVEN`。已 resolved 的操作若要有意再做，创建新 operation；只有新矛盾证据通过协调事件把旧操作转回 reconciling，才重新进入该恢复路径。幂等键作用域、过期时间和对相同参数的要求属于外部协议，不能由通用 API 猜测补全。

主体决定在未知情况下有意再做一次时，创建新 intent，以 `retry_of` 与 `possible_duplicate_of` 指向原操作；记录“这是再次尝试产生效果”的决定。补偿也创建新 operation 并设置 `compensates`。二者都不修改原事实。

### P2.3 状态、回执与先后顺序

```ts
type OperationView = {
  operation: OperationRecord;
  attempts: AttemptRecord[];
  receipts: EvidenceReceipt[];
  reconciliation_cases: ReconciliationCase[];
  cancellation_tasks: TaskRecord[];
};

type EvidenceReceiptDraft = Omit<EvidenceReceipt, "receipt_id" | "received_commit">;
```

`OperationView` 只读上述规范记录，不另存 execution/effect/outcome 的第二套状态机。Operation 的 `state` 固定为 `prepared | in_flight | reconciling | resolved`，效果使用 `effect_status = none | partial | complete | unknown`；Attempt 的执行过程使用其 `dispatch_stage`。入站证据中关于合同摘要、实际绑定摘要、工件 generation 和结果的声明，存入 `raw_ref/normalized_ref` 引用的版本化内容，不能替代实际验证。

状态转换按以下顺序执行：

| 当前状态与事件 | 必须提交的规范结果 | 之后允许的动作 |
| --- | --- | --- |
| 首次接纳 | Operation `prepared / effect_status=none` | 尚未执行任何外部调用；可以与首个 T1 合并 |
| 派发准备完成 | T1 创建 armed Attempt，Operation `in_flight / unknown` | T1 成功后才调用外部执行器；armed 不是 provider 已接收的证明 |
| 外部确认仍运行 | Attempt `running`，保存 handle；Operation 仍 in_flight | 查询、等待、请求取消；effect_status 由证据确定 |
| 确定结果及实际效果 | T2 追加/引用证据，Attempt ended，Operation resolved 与真实 outcome/effect_status | 当前合法 invocation 同事务推进任务检查点和结果/outbox |
| 传输失联、崩溃、取消没有确认 | Operation reconciling；Attempt unknown 或保留已知 running | 对账、等待回执或按已证实幂等合同重试；不推断 none |
| prepared 且从未 armed 时取消 | Operation `resolved / none / cancelled` | 不派发 |
| 派发后请求取消 | 先持久提交取消任务/决定 | 发出取消；确认结果另由 EvidenceIngress 接纳 |
| 新旧 generation 的迟到回执 | 追加原 operation 的事实，重算视图 | 不重新授予旧 generation 控制权，不重新发送原操作 |

Attempt 的 `ended` 不自动表示没有副作用；命令退出非零仍可写入一半文件。持久取消请求表示取消意图，不表示远端已停止。无法消除的不确定性保留在 ReconciliationCase；主体/获委派 child 可把 case 置为 `acknowledged_unknown`，不新增 abandoned operation/task outcome，也不把 Operation 伪标 resolved。

`cancel_operation()` 以 `(operation_id, cancel_key)` 幂等创建或取得 `operation.cancel` 任务及其决定，返回 `CancelReceipt`，不等待远端必然停止。该任务由持久协调 supervisor 持有；调用者取消等待不取消它。`wait_operation()` 在 operation_rev 增加或已存在 resolved 状态时返回视图；取消该等待不改变操作。父任务可以在将未决 operation 和 case 职责交给持久 supervisor 后结束本轮等待，不能随父任务回收未决事实。

回执按 EvidenceIngress 的来源作用域与稳定 `source_event_key` 去重；同作用域/key、不同原文返回 `RECEIPT_KEY_CONFLICT` 并保存冲突证据，不能 last-write-wins 覆盖。只有相同内容 hash 不能证明两次外部事件是同一事件。派生的 `OperationView` 可更新，原 receipt 不可原地改写。读不懂的新 payload 保留原字节与 decoder 引用，显示 `READER_UNAVAILABLE`，不能当成功或无效果。

工具的模型文本、程序结构化输出和界面 detail 可以分别投影。`afterTool` 或摘要插件只修改投影，不修改 receipt、operation identity、实际 effect 状态或原始输出证据。任务最终结果引用 durable data，不引用将在父任务结束时自动销毁的临时文档。

来源：REF-DURABLE [§5.2 effect sandwich](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2045)、[§7.3 嵌套调用身份](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L3162)。本产品增加实际资源绑定、长期 effect receipts、跨 generation 核对；这不是任意外部效果的 exactly-once 或撤销保证。

## P3 Hook 与 context seal 规范接口

### P3.1 扩展点、处理器与图

```ts
type HookPoint = {
  id: string;
  major: number;
  input: SchemaRef;
  output: SchemaRef;
  mode: "observe" | "transform" | "collect" | "decide" | "around";
  writable_paths: string[];
  timeout_ms: number;
  failure: "stop_unit" | "report_and_continue";
};

type HookHandler = {
  id: string;
  implementation: VersionedRef;
  point_id: string;
  point_major: number;
  before: string[];
  after: string[];
  requires: string[];
  possible_model_call: boolean;
  deterministic: boolean;
};

type HookDispatch = {
  dispatch_id: string;
  graph_digest: Digest;
  handler_digests: Digest[];
  input_revision: string;
  input: DataRef;
  request_id?: string;
  request_revision?: number;
};

type HookResult =
  | { kind: "no_change" }
  | { kind: "replace"; value: DataRef }
  | { kind: "contribute"; items: ContextContribution[] }
  | { kind: "decide"; value: DataRef }
  | { kind: "defer"; dependency_task_id: string }
  | { kind: "reject"; code: string; details: DataRef };

type ContextContribution = {
  key: string;
  slot: string;
  source: VersionedRef;
  data: DataRef;
  input_revision: string;
  lifetime: "this_request" | "until_event_revision";
  required: boolean;
  budget_tokens: number;
};
```

图在激活前完成 schema 兼容检查、硬依赖检查和拓扑排序。有环或缺少硬依赖时拒绝候选图，保留当前图。合法拓扑顺序相同时，按稳定 handler ID 决定顺序；插件安装时刻、异步完成时刻和任务紧急优先级均不成为隐式排序键。

`before/after` 只约束同一 hook point 中存在的处理器；`requires` 明确声明必须存在的依赖。指向不存在的可选处理器不阻止激活。Phase 是 API 先后关系，消息 priority 是注意力/资源调度，两者不复用同一字段。

每个 `HookPoint` 必须给出有限的 `timeout_ms` 与失败策略；标准安装 profile 提供数值，插件缺省不等于无限等待。声明决定该扩展点的合同，单个处理器不能把本来 required 的 schema 校验改成可忽略。所有处理器遵循同一资源核算；`possible_model_call` 不是不受全局并发限制的许可。

### P3.2 各类输出的合成规则

| mode | 合法输出 | 合成与失败规则 |
| --- | --- | --- |
| observe | `no_change`、`defer` | 不改输入；异步观察不能阻止已提交事实发布。需要后续动作则提交独立任务 |
| transform | `no_change`、`replace`、`defer`、`reject` | 按固定顺序串行变换；每次验证 output schema 和 writable paths |
| collect | `no_change`、`contribute`、`defer`、`reject` | 按固定处理器顺序和 contribution key 合并，不能按 Promise 完成顺序拼接 |
| decide | `no_change`、`decide`、`defer`、`reject` | 第一个有效 decision 结束此扩展点；`no_change` 表示继续询问 |
| around | 本扩展点定义的结果 | `next()` 至多调用一次；重试必须成为 operation attempt，不能隐式多次 next |

同 slot/key 的冲突只有在该 slot 声明了明确 reducer 时才能合并；否则报 `CONTRIBUTION_CONFLICT`。不存在“后完成的覆盖前面的”默认行为。Transform 写入不在 `writable_paths` 中的字段报 `HOOK_WRITE_VIOLATION`。无论 failure 策略如何，非法修改都不能被采用。

`report_and_continue` 只跳过该处理器本次输出并留下诊断；它不能把一个已发出的外部效果抹掉。如果 hook 需要产生外部效果，必须调用 P2 operation 接口；hook 重试只重取已有 operation。纯声明“deterministic”不能免除真实外部调用的 ledger。

observe 返回 `defer` 只登记后续独立任务，不把已经完成的源单元转回等待；需要当前 request 等待其结果的处理器必须注册到允许贡献或变换的扩展点。hook 产生的嵌套模型调用进入同一 broker，默认不再次运行该 hook 自己；显式递归仍必须声明独立 dispatch identity 并通过等待环检查。

### P3.3 RequestSeal

```ts
type RequestSeal = {
  request_id: string;
  request_lineage_id: string;
  request_revision: number;
  supersedes_request_id?: string;
  runtime_generation: RuntimeGeneration;
  graph_digest: Digest;
  context_revision: string;
  input_plan_ref: DataRef;           // 引用 InputPlan；不是已消费的消息游标
  rendered_messages: DataRef;
  offered_tools: Array<{
    tool_name: string;
    contract_digest: Digest;
    implementation: VersionedRef;
    declaration: DataRef;
  }>;
  contributions: DataRef[];
  provider: VersionedRef;
  model_config: DataRef;
  semantic_native_options: DataRef;
  token_budget: { input_limit: number; output_limit: number; accounting: string };
  prefix_digest: Digest;
};
```

首版请求顺序固定为：

1. 在主循环可提交边界选择已持久事件与任务状态；提交 `InputPlan`，其中包含各 ledger 的读取水位向量、实际 selected event refs/revisions 与 causal support refs，再创建新的 request revision。读取到 seq 105 不代表已消费所有 seq≤105 的事件。
2. 固定本次 hook graph，运行 context/tool/provider 的语义变换与必要依赖任务。
3. 组装人格、历史、已选择事件、按需能力和瞬态提醒，执行预算与工具调用结构校验；保存各 contribution 的来源及本次生命周期。
4. 将模型将看到的精确 messages、工具声明及模型/provider 语义参数提交为 `RequestSeal`。
5. 进入推理 broker 队列，获得 permit，调用后端；实际 lane/KV affinity/尝试与资源回执另行记录。

上述次序是协议顺序；实现可并行纯计算，但不能越过依赖和提交边界。需要等待模型的 hook 发生在步骤 4 之前，且父请求没有持有推理 permit。若某处理器在已有 permit 的路径上要求模型依赖，父请求先释放 permit、持久化 `waiting_dependency`，再提交子请求。禁止持有 writer 事务等待，也禁止等待只有“本主请求完成后”才能产生的依赖；后者返回 `DEPENDENCY_CYCLE` 或将该工作改成下一轮贡献。

Seal 后只允许改变不改变模型输入语义的传输字段，例如凭据刷新、连接信息及后端实际支持的 KV affinity；每次 transport attempt 记录其真实字段。切换模型、修改输出上限、修改 native 语义选项、工具 schema 或 prompt，均生成新 request revision，并重新预算、封口和排队。调度器不能一边保留旧 prefix 身份，一边隐蔽改写其实际 token 输入。

每次语义 reseal 同时分配新 `request_id`，在同一 `request_lineage_id` 中递增 `request_revision`，以 `supersedes_request_id` 关联被替代请求；同一 request_id 始终只代表一个不可变 seal。语义未变且满足恢复合同的 transport retry 可以新增 attempt，不改成第二份逻辑请求。

`prefix_digest` 关联精确输入前缀及影响其解释的模型/tokenizer 身份，用于一致性和调度提示；摘要相同不等于后端真实 cache hit，KV 是否驻留或可恢复以 provider 的具体回执为准。

stream 开始后，新消息继续被持久接纳，但不能插入该请求过去的 messages。它进入下一边界，或触发有真实后端支持的取消/steering 路径。取消后的部分输出保留为事实；是否进入后续模型上下文由恢复/上下文合同决定，不能伪造完整 tool-call/result 配对。

工具调用必须匹配 `RequestSeal.offered_tools`。按 sealed contract 解释参数，绑定该版本实现或显式的兼容适配器。旧实现不可用且不存在声明过的适配器，返回 `TOOL_CONTRACT_UNAVAILABLE`；不能用当前同名工具静默替换。当前权限和资源绑定仍在派发时重新核验。

### P3.4 hook 恢复与热更新

一个 dispatch 固定 `graph_digest`、输入 revision 和处理器工件。模型辅助等非确定结果按 `(dispatch_id, handler_digest, input_digest)` 保存；重启重用已提交结果，不再次调用模型制造另一答案。输入已变化时创建新 dispatch，不把陈旧异步结果追加到已 sealed 请求。

热更新发布新图只影响新的 dispatch/request revision。旧图正在运行时保留其工件租约。晚到的 hook 输出若仍属于可继续的旧 dispatch，可以完成它；若该请求已取消/替代，记录结果并标记未消费，不能自动注入新请求。

REF-FABRIC 只支持类型化扩展点/phase 顺序方面的来源；上述持久 dispatch、permit 释放、seal 和恢复为 REF-DESIGN。REF-DURABLE 的 [§7.2 hooks](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2906) 与 [§7.4 prompt/tool 变更](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L3661) 是具体参考。与其当前 phase 再解析工具名的规则不同，Shenko 明确绑定 sealed contract，避免热更新重解释旧调用。

## P4 自解释规范接口

```ts
type SelfTarget =
  | { kind: "active" }
  | { kind: "generation"; id: RuntimeGeneration }
  | { kind: "candidate"; id: string }
  | { kind: "source"; revision: string };

type SelfQuery = {
  query: string;
  target?: SelfTarget;                 // 缺省 active
  kinds?: Array<"design" | "implementation" | "manual" | "runtime_fact">;
  capability_id?: string;
  max_tokens: number;
  cursor?: string;
};

type SelfAnswer = {
  anchor: {
    generation: RuntimeGeneration;
    artifact_digest: Digest;
    source_digest?: Digest;
    plugin_lock_digest: Digest;
    docs_digest?: Digest;
    state_revision: string;
  };
  items: Array<{
    kind: "design" | "implementation" | "manual" | "runtime_fact";
    status: "planned" | "candidate" | "installed" | "enabled" | "observed" | "unknown";
    title: string;
    ref: DataRef;
    excerpt: string;
    source_refs: string[];
    observed_at?: string;
    valid_until?: string;
    implementation?: VersionedRef;
  }>;
  completeness: "complete_for_query" | "partial";
  missing: Array<{ kind: string; reason: string }>;
  active_changed_to?: RuntimeGeneration;
  cursor?: string;
};

type SelfRange =
  | { kind: "lines"; start: number; end?: number }
  | { kind: "section"; id: string };
type SelfReadResult = {
  ref: DataRef;
  applied_range: SelfRange;
  content: string;
  truncated: boolean;
  next_range?: SelfRange;
};
type SelfContractResult = {
  anchor: SelfAnswer["anchor"];
  capability_id: string;
  contract_digest: Digest;
  contract: DataRef;
  implementation: VersionedRef;
  documentation: DataRef[];
  recovery: DataRef[];
  conformance_cases: string[];
};

self_query(query: SelfQuery): Promise<SelfAnswer>;
self_read(ref: DataRef, range: SelfRange, max_tokens: number): Promise<SelfReadResult>;
self_contract(capability_id: string, target?: SelfTarget): Promise<SelfContractResult>;
```

1. `active` 在调用开始时解析成具体 generation anchor；同一回答及分页游标固定此 anchor。查询期间发生更新，可返回旧 anchor 的一致回答并填入 `active_changed_to`，不得把旧二进制、新 main 分支文档与另一插件配置拼成“当前自身”。
2. `implementation` 由构建 provenance 和工件摘要关联；Git commit 不覆盖未提交 patch、构建 flag 或不同插件闭包。`runtime_fact` 带实际观察时间和来源；外部系统的不同观察不能仅靠一个本地 state revision 冒称原子快照。
3. `installed`、`enabled`、`observed` 分别表示安装记录、当前选择和运行证据，不相互推导；`planned` 内容不作为已实现能力返回。用户问“当前能做什么”，缺省只据 active 事实回答，计划项显式标识。
4. 文档缺失、源码不可用、旧 reader 缺失和索引未建成分别返回 `missing`，不编造正文，也不要求暂停主体。离线最小索引能返回当前工件/插件/合同身份；完整源码可由 child 经授权来源获取。
5. `self_query/read/contract` 是语义只读接口，不给主体开放任意路径读写、shell 或 SSH。服务实现可读取其资料；需要探测或修复时由 child 调用有 operation 合同的工具。共享能力目录不改变角色执行边界。
6. 自解释索引或检索若调用 embedding/LLM，其成本与推理申请仍经过同一 broker。普通自身查询不强制先跑一次额外 LLM 分类器。
7. 文档缓存按内容摘要命中；运行事实按状态 revision/有效期命中。升级通知只使 active 别名指向新 anchor，不能修改旧引用所代表的内容。
8. child 开始改写自身时使用 `self_contract()` 取得确切的目标实现、输入/输出合同、恢复边界、关联源码/文档/验收；提交候选前检查实际基线仍匹配。基线变化时 rebase 或重新制定候选，不靠过期文档盲改。

来源：REF-OMP 的 §38 已核验 `omp://` 文档发现路径；上述 runtime/source/doc provenance、active anchor、版本查询和恢复链为 REF-DESIGN，不归为 OMP 已有完整保证。

## P5 版本兼容、激活与回退规范接口

```ts
type ActivationPlan = {
  activation: ActivationRecord;
  compatibility: Array<{
    stored_schema: SchemaRef;
    action: "read_native" | "migrate_copy" | "retain_reader" | "block_affected_unit";
    adapter?: VersionedRef;
  }>;
};

prepare_activation(plan: ActivationPlan): Promise<ActivationRecord>;
get_activation(activation_id: string): Promise<ActivationRecord>;
activate(activation_id: string, expected_selector_revision: bigint): Promise<ActivationSelector>;
prepare_recovery(from_generation: RuntimeGeneration, target_generation: RuntimeGeneration): Promise<ActivationRecord>;
```

本节复用持久执行合同的 `ActivationRecord`、`ActivationSelector` 和 phase 状态机。`ActivationPlan` 是该记录及其 recovery closure 中 compatibility 映射的只读展开，不形成另一份激活状态。迁移、目标副本、未决 operation、烟测与回退工件分别使用规范 ActivationRecord 已有引用。

1. 软件包版本、hook point major、operation 合同、receipt codec、任务检查点 schema 和存储 schema 分别版本化。改变执行时点、重放意义或字段解释属于语义变化，即使 JSON 字段名字没变也不能算无变化。
2. active 版本继续服务时，候选使用独立数据副本完成迁移与烟测。候选没有生产目标的执行权；测试使用隔离目标或明确的测试 operation。未提交 handoff 之前，候选的假设不能成为正式已执行事实。
3. 通过 smoke 进入 handoff 的必要条件是候选闭包可加载、目标状态可读、必要检查通过、未决 operation 有可用的 reader/reconciler 或明确保留的执行器。`block_affected_unit` 必须列明受影响单元及自主修复路径，不能用它掩盖“启动即失去全部消息或恢复能力”。
4. `activate()` 通过 `expected_selector_revision` 比较和 Selector 持久 CAS 切换当前写者。新旧版本可同时运行独立副本；同一生产控制域在交接点只有一个当前控制权。不中断可存活的外部 seat，只转交 seat identity、observer cursor、操作记录和管理权。
5. 旧 generation 不再被允许新增生产命令，但其迟到回执仍由当前 receipt ingress 接纳。回执的 producer generation 是事实来源，不是新的控制 lease。旧进程无法直接写新 store 时，通过保留的转送/暂存通道送达；通道也属于 recovery bundle。
6. 回退不是把一切指针指向旧 A。A 必须能读取或接收转换后的 B 期间新事实，或继续使用保留的 B reader/reconciler 服务。无法解释的新 receipt 保留原字节和读取工件，并阻止相关操作的猜测性重试；主体可继续诊断、修复与处理无关任务。
7. 活跃调用、未决 effects、保留快照和恢复计划所引用的工件构成 GC roots。卸载插件先从新请求发现中移除；正在执行的闭包保留至完成、显式迁移或终止核对结束。强制删除造成的缺口报告为 `CONTRACT_UNAVAILABLE`，不能伪装正常取消。
8. `ActivationRecord.recovery_closure_ref` 包含可解释当前 handoff、状态副本和未决 effects 的可运行闭包。bootstrap/core/救援器可以替换；升级它们时同样先保留可启动旧副本、验证新副本并交接。不要求永久固定某种文件格式或永久保留不可改 Ring 0。
9. 迁移是自主执行能力，不自带人类审批步骤。主体决定升级即可进入上述状态机；失败返回可行动的诊断与旧版本继续状态，而不是强制请求用户按确认键。

REF-DURABLE 的 [§3.6 数据版本](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L1591) 和 [§7.5 reload 生命周期](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L3791) 支持版本/阶段边界的比较。A/B 副本、全 core 可替换、跨代 effect receipt 和自主 handoff 是 REF-USER/REF-DESIGN；不能把上游不可替换 built-in task 的限制引入 Shenko。

## P6 首版一致性验收：可执行反例

下表收紧现有 E/H/K 用例的可判定结果，不取代它们，也不要求每次文案编辑重跑整套测试。实现触及相关合同才运行对应故障用例。

| Case | 注入的情况 | 通过条件 |
| --- | --- | --- |
| P41-01 | 相同 intent key 重试 3 次；第 3 次参数不同 | 前两次相同 operation；第 3 次 `INTENT_KEY_CONFLICT`；没有新增外部派发 |
| P41-02 | QQ 已收到消息，成功回执提交前杀进程 | 原 operation 为 unknown；重开不自动再发；查询/迟到证据能完成原记录 |
| P41-03 | SSH 命令写入 host bind 文件后取消，并 restore WASM | host 文件真实改变仍在；operation 标清效果；恢复页面不宣称该文件已还原 |
| P41-04 | 旧工具参数为字节，新同名工具改为 KiB；请求在更新前已 seal | 按旧 contract 执行或明确 unavailable，不能用新单位执行 |
| P41-05 | C=1，主请求 preflight hook 委派模型检索 | 父请求不占 permit；检索完成后父请求继续；子任务与主体均可推进 |
| P41-06 | stream 到一半到达 50 条外部消息，异步 PMA 同时完成 | 消息全数持久化；本 stream seal 不变；后续按水位消费，旧贡献不被重复插入 |
| P41-07 | 两个 collector 以不同完成顺序返回相同 slot/key | 每次都得到相同冲突或声明过的 reducer 结果；完成时序不影响模型输入 |
| P41-08 | 新 hook 图 A→B→A；或 around 调用 next 两次 | 有环图不激活；第二次 next 不产生第二外部效果；已发生第一效果仍有 receipt |
| P41-09 | 查询 active 自身文档时切换版本，并继续下一页 | 每页同一 anchor，或明确返回旧 anchor 与新 active；不混合版本 |
| P41-10 | B 生成 A 读不懂的 receipt 后回退 A | 原 receipt、codec 和事实完整；保留 reader/转换器后能解释；否则相关操作停留待恢复，不盲重放 |
| P41-11 | 旧 worker 在 handoff 后才返回成功 | 新 writer 接纳原操作回执；旧 worker 不获得新命令权；不重复完成同一任务/outbox |
| P41-12 | outcome 提交发生不确定存储错误 | writer 停止新派发；重开查原 admission/operation key；既有外部事实不因新 retry key 被重复执行 |

## P7 统一错误处理表

| code | scope / retry | 固定行为 |
| --- | --- | --- |
| `INTENT_KEY_CONFLICT` / `RECEIPT_KEY_CONFLICT` | call / never_automatically | 原记录保持；返回冲突字段摘要；不覆盖、不重做 |
| `BINDING_STALE` / `AUTHORITY_CHANGED` | operation / new_revision | 不派发本次尝试；重新观察/授权解析后由新 intent 表达新决定 |
| `EFFECT_UNKNOWN` | operation / after_reconcile | 保存原 intent、attempt、证据；查询、等待或显式处理 |
| `RETRY_NOT_PROVEN` | operation / after_reconcile | 不新建 attempt；补充真实幂等/未执行/可重复证据，或由新 intent 表达有意再做一次 |
| `TOOL_CONTRACT_UNAVAILABLE` / `READER_UNAVAILABLE` | operation / after_reconcile | 暂停受影响调用；保留原数据；允许自主安装/修复所需适配器 |
| `HOOK_WRITE_VIOLATION` / `CONTRIBUTION_CONFLICT` | request / new_revision | 不采用非法输出；按 point failure 策略结束当前 request 或跳过该处理器 |
| `DEPENDENCY_CYCLE` | request / new_revision | 不入不可完成的等待；改成后续轮贡献或重组依赖 |
| `STALE_HOOK_RESULT` | call / new_revision | 保存未消费结果；不修改 sealed request |
| `ACTIVATION_CONFLICT` | activation / new_revision | expected_selector_revision 不匹配；候选不接管；重新评估当前 Selector |
| `MIGRATION_FAILED` | activation / never_automatically | 旧版本继续；保留候选/诊断；自主修复后创建新迁移尝试 |
| `COMMIT_UNCERTAIN` | writer / after_reconcile | 停新派发，恢复 writer，以原 key 查询是否已提交 |

`never_automatically` 禁止相同坏输入的机械重试，不禁止主体/child 分析原因、修改候选或决定新的动作。`new_revision` 的修复默认自主执行，不是必须找人审批。

## P8 产品正文与工程合同的分工

产品 SPEC 保留：单串行主体、全插件、主体执行角色、消息连续性、自主更新、隐私与真相源、设备 profile、首版用户流程、恢复语义、跨端 UI 行为以及各用例的用户可观察结果。§36–38 保留这些边界及本节索引，不再复制同一类型的第二份定义。

工程协议保留：上面的规范类型、状态转换、线性化/封口点、兼容规则、错误表与故障验收。后续可以拆为 operations、hooks-context、self-knowledge、activation 四份协议文件；每个概念只有一处规范定义，产品正文链接版本，不在多个章节维护略有不同的字段表。

Rust trait、WIT 或序列化 schema、数据库迁移、时限 profile、数据索引和测试 fixture 由开发任务依据这些合同选择；它们是实现选择，不要求用户逐个批准。接口演进通过版本化和迁移，不能把本稿字段永久锁死。

本轮四个范围内没有新增必须由用户裁决的产品矛盾。准确的 **colibri-rs 仓库/版本身份**仍是既有信息缺口：它决定具体 lifecycle/service 绑定，不能用同名项目猜测补齐；在确认前可继续完善上述与框架无关的逻辑合同。是否允许自改 core、是否主动升级、主循环是否持续、是否总要人工批准均已有裁决，不再重问。
