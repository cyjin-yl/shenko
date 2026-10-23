# 41 首版规范执行合同

## 41.1 本章的地位、范围与阅读方法

本章响应 U40，定义 Shenko 首版开发者发行可以据以实现、互操作和检验的执行合同。§1–24 说明产品目标与能力，§35–38 说明首版旅程及插件职责，§39 记录来源与差异，§40 规定开发方式；本章给出记录、状态、事务、接口、错误和交错结果。遇到前文“至少包含”“原子提交或同等恢复”等概述，以本章相应具体合同为准。用户已确认的产品要求优先，不能借细化改变永续主体、自主性、role 边界或插件化。

本章中的“必须／不得”是实现不变量；“首版默认”是开发 Agent 可自行实施和按证据调整的工程选择；“后端条件”必须来自真实能力协商，不能由接口外形推定。所有新增类型名和命令签名是 Shenko 拟定的协议，不是已发布 SDK、colibri-rs 现成 API 或已实现产品。Rust、WIT、进程内调用和网络传输可采用不同编码，但必须映射同一语义，并给出协议版本及 conformance 证据。

首次实现按以下顺序读取：共用提交与记录 → 主体及 inbox → operation/取消/回执 → broker/后端 → hook/插件调用 → seat/outbox → 升级与恢复 → 故障矩阵及完整轨迹。某个插件只需实现其适用合同；“一切插件化”不是让每个插件自行解释什么叫成功、取消或恢复。

**复用与扩展边界。** 本章借鉴固定版本 [Pi Durable SPEC](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md) 的单一 mutation line、已提交可见、外部 effect 与事务分离、持久 task/cancel、ownership 和重开恢复，尤其 §4、§5.1–5.5、§6、§7、§10.1、§11。Shenko 的单线程永续主体、物理 Engine broker、可替换全部实现、A/B 自迁移、多节点 availability branch 和跨代证据接收是基于用户要求的扩展。不得把 Pi 不提供的 CRDT/offline multi-writer、非合作代码强制终止或完整进程升级归为它已经解决。来源：REF-DURABLE、REF-USER、REF-DESIGN；行为 B03/B04/B20/B24/B36–B41。

### 41.1.1 定义写完的判据

每个有状态命令必须同时给出：调用方与状态所有者、必填输入、版本/幂等/前置条件、一次原子提交包含的记录、返回的持久回执、取消和迟到结果、重开后的处理、可观察事件及至少一个失败交错。缺一项即标记该合同尚待细化，不能以“provider 自行实现”掩盖跨模块歧义。与目标环境有关的性能值需要测量；未知值不能写成保证。

首版自动验收应覆盖下面的规范行为，而不要求一次实现所有未来设备和媒体 profile。后期 WASM 移植、通话、Computer Use、REPL、存储插件继续继承这里的 intent/effect/receipt、任务、资源和版本规则；它们各自还需满足原章节的能力验收。发布清单同时列出已交付和仍待交付的 R 项，不能用首版范围删除未来正式要求。

## 41.2 默认持久存储与观察边界

存储、任务调度、broker、生命周期和恢复器都由可替换服务实现；本节没有要求将 SQLite、全部业务表或升级策略永久编入 Rust 内核。默认发行装配一组实现相同合同的插件，小核心负责启动和连接当前有效服务。

### 41.2.1 本地写入域与 SQLite profile

首版每个本地 ledger 只有一条 mutation line、一个当前 writer incarnation。数据库写锁只覆盖本地校验与提交，不覆盖模型推理、SSH、网络、构建、插件等待或用户等待。只读快照不持有此写入线。独立节点拥有独立 ledger；通过事实导入、选举和 fencing 协议汇合，不通过同时挂载同一 SQLite 文件实现跨机协作。

默认 StoreProvider 使用本地 SQLite，配置并读回确认 `journal_mode=WAL`、`synchronous=FULL`、`foreign_keys=ON`；一笔逻辑 commit 对应同一个数据库中的一笔 SQL 事务。不使用跨 ATTACH 数据库写入来冒充一个全有或全无提交。FTS、向量、缩略图、检索排序和 UI 视图是派生物；原始 entries/receipts 与对应索引工作通知同笔登记，索引可稍后重建，不能因搜索暂缺就宣称原文不存在。索引返回 `indexed_through` 与 `source_through`；追求最新事实的读取直接查原记录或等待明确水位。

打包的 SQLite 版本必须包含官方 WAL-reset 修复（官方列出 3.51.3 及以后，或相应已修复的回移版本）；build manifest 记录实际版本与 compile options。是否支持 FTS5 在启动能力探测中明确记录。WAL 不用于多个主机共享的网络文件系统。以上 SQLite 特性与限制依据 [synchronous 文档](https://www.sqlite.org/pragma.html#pragma_synchronous)、[WAL 文档](https://www.sqlite.org/wal.html)；这是 Shenko 的首版配置选择，不等于 Pi 默认配置。

每个 `CommitReceipt` 返回 `commit_ref`、`durability_profile`、`store_incarnation` 和本次写入的实体 revision。默认 profile 为 `power_loss_local`，含义是在目标 OS、VFS、设备正确兑现同步语义的条件下，确认后应经受本机进程崩溃和掉电；它不包括磁盘物理损坏、失去整机、未同步远端副本或外部系统操作。某个平台只能证实 `process_crash` 时必须返回该等级，不能沿用名称继续宣称掉电持久。Agent 可自主选择调整 profile，但调整形成版本化配置事实，不回写已发出的 receipt。

### 41.2.2 对象与元数据的提交顺序

大文件、音频和截图放 CAS，ledger 只引用已封存对象。先完成对象写入、长度和 digest 校验，再按当前 durability profile 同步文件与必要目录、获得 `sealed_object_ref`，最后允许元数据事务引用它。事务回滚可留下无引用的已封存对象，由后续 GC 回收；不得先提交一个“对象存在”的引用再异步创建内容。

一个 checkpoint 的树根、文件 manifest 和所有新对象必须满足闭包验证；未知或缺少的对象导致 `ObjectClosureIncomplete`，不得将其提升为可恢复快照。流媒体尚未完成时使用显式 open-stream 记录，已封存分块与最终对象分开；下载 URL 不是已归档附件。宿主 RW 挂载和远端机器不在 CAS 快照的隐含范围内，仍按 operation 的实际 effect boundary 处理。

GC 读取一个已提交 root snapshot，pin 活跃/候选/恢复 generation、在途 attempt、未消费回执、迁移水位和用户/主体保留的档案；删除前重新校验 root revision 或使用等价 epoch 协议。不能因“旧软件已停”就删除其 receipt decoder、未完成恢复所需闭包或新版本运行后的事实。GC 自身有任务和删除回执，空间不足不会把普通写入悄悄降成不持久。

### 41.2.3 提交失败、发布与重开

调用通过 schema、幂等和 revision 检查之前的拒绝必须证明没有持久 effect，返回 typed rejection。进入 StoreProvider.commit 后，调用者取消只停止其等待，不撤销已接纳的存储结算。已开始的 commit 必须结算；只有耐久结果明确后，才采用内存状态并发布与该 commit 对应的事件。

StoreProvider.commit 抛错、返回不确定、存储已提交但内存 adopt 失败时，当前 ledger handle 进入 `failed`，拒绝后续 mutation，终止相应 watch，通知在途 invocation 停止新决定。不得在旧内存对象上重试“同一段代码”。恢复器重开 store、读取最后真实提交，再使用原 request/operation id 查询结果；确认没有该命令的持久 admission 时可按现行条件重新准备；存在 armed attempt 时，按其外部幂等/查询合同处理，不能重新装成一条未发送命令。外部工作可能仍在运行，重开不能据此宣告它失败或释放其 Engine 占用。

状态观察遵循 `committed-only`：watch 返回不可变对象和 `commit_ref`；用户回调在写入线之外运行。首个快照含水位，后续事件按同一 ledger commit 顺序；断线重连使用该水位补读，超出保留范围返回 `ResnapshotRequired`。观察者报错或缓慢不回滚已提交事实，也不阻塞模型和任务写入。派生 UI 可以显示“生成中”，但不能把未提交 token/tool-call 当完成决定。

流式文本按有限 chunk 批量持久化后再作为可恢复内容发布，默认按 100 ms 或 4 KiB 中先满足者提交，终态强制 flush；该批量阈值是可调工程默认。未完成或被取消的 chunk 仍可作为带状态的原始轨迹保存，不成为自动重放的工具调用。只有通过完整 schema 校验并在相应事务中接纳的 action 才能执行。UI 的临时光标、传输速率等瞬时呈现不用写入每笔 ledger，也不得影响恢复。

本节对提交未知的处理参考 REF-DURABLE §10.1；持久等级参考 REF-STORAGE。特别不能把 Pi JSONL 的 `fsync:true` 误写成“每次已 ACK 尾部均保证掉电后存在”：其固定规范明确普通提交没有显式同步 main marker。Shenko 的不同 StoreProvider 必须各自通过其声称的恢复等级，不能只通过相同接口名称继承保证。


## 41.3 共用身份、类型与调用上下文

下述类型用于表达协议，不是要求以 TypeScript 编写。ID 是不复用的全局唯一标识；uint64/bigint 在 JSON 中编码为十进制字符串，不能由 JavaScript 浮点数截断。Digest 使用含算法名的形式；对象的 locator 可变化，摘要与引用的内容身份不变。record_schema_version、任务语义版本和记录 CAS 版本各自独立。

```ts
type Digest = string;
type UUID = string;
type LedgerSeq = bigint;
type CommitRef = { ledger_id: UUID; seq: LedgerSeq };
type RuntimeGeneration = string;
type ArtifactRef = { digest: Digest; locator: string };
type SchemaRef = { name: string; major: number; digest: Digest };
type VersionedRef = { artifact: ArtifactRef; symbol: string; schema: SchemaRef };
type VersionRef = VersionedRef;
type ObjectRef = {
  digest: Digest; bytes: bigint; media_type: string;
  store_namespace: string; locator: string; codec: SchemaRef;
};
type DataRef = ObjectRef;

type CallContext = {
  caller_instance_id: UUID; role_revision: string;
  task_id?: UUID; expected_task_revision?: bigint;
  invocation_id?: UUID; controller_epoch: bigint;
  software_generation: RuntimeGeneration;
  authority_refs: string[]; trace_id: UUID;
};

type CommitReceipt = {
  commit_ref: CommitRef;
  durability_profile: "process_crash" | "power_loss_local";
  store_incarnation: UUID;
  updated: Array<{ entity_id: UUID; record_revision: bigint }>;
};

type ProtocolError = {
  code: string;
  scope: "call" | "task" | "operation" | "request" | "activation" | "writer" | "seat" | "inbox";
  retry: "same_key" | "after_reconcile" | "new_revision" | "never_automatically";
  operation_id?: UUID;
  commit_ref?: CommitRef;
  details: Record<string, unknown>;
};
```

所有服务调用同时携带经 registry/IPC 验证的 CallContext；后续签名省略这一参数以便阅读，不能省略实际校验。它由有效调用者身份与当前 invocation 构造，模型参数或消息正文不能自行声明一个更高权限的上下文。EvidenceIngress 有独立的接收身份，只保存并关联事实，不接受旧 invocation 借回执发新命令。

**task_revision** 只因目标、委派、取消、角色或显式任务语义迁移而变化；**record_revision** 随每次持久状态/checkpoint 更新递增，用于 CAS。正常进度提交不自动使一个仍有效的任务决定失效。新变更按需要同时检查 expected_task_revision 和 expected_record_revision；已命中的幂等读取先返回原记录，再谈新的 mutation 前置条件。

`request_id` 唯一标识一份不可变 RequestSeal。语义 reseal 必须分配新 request_id；request_lineage_id 表示同一生成工作的演变链，request_revision 只在该 lineage 内递增，并保存 supersedes_request_id。一个 request_id 永远不能指向两个输入；队列、attempt 和结果按 request_id 关联。网络重试或正确的无损恢复可以沿用该 request_id 并增加 attempt，而非篡改 seal。

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


## 41.4 任务、效果与恢复的规范记录

`CommitRef = { ledger_id: UUID, seq: uint64 }`。`seq` 只在同一 `ledger_id` 内严格递增；允许空洞，不以墙钟或不同节点的 seq 比大小。经 JSON 传输时 uint64 使用十进制字符串。跨账本因果关系记录 `causal_refs[]`，不伪造一个全局顺序。

Task/Operation/Attempt/Receipt 等实体采用不复用的全局唯一 ID；记录有 `record_schema_version`。可变投影另有单调递增 `record_revision`，原始事件与 EvidenceReceipt 不原地覆盖。一次提交暴露完整批次，读者不能看到半个 Task 状态加半个 outbox。UI、通知与模型可读业务进展只引用已提交版本；流式输出可以小批次提交后推送。动画不属于业务事实。

`StorageProvider` 提供单提交域的原子批次、条件更新、稳定读取与重开，不把这些实现焊进 Rust 最小内核。跨 SQLite、CAS、远端平台的动作不属于一笔原子事务。

### X02. TaskRecord 必须有可以恢复的完整状态 `[P+S；DUR-P2/DUR-P4]`

下面为规范数据形状，不是要求使用 TypeScript 实现。`ObjectRef` 指带内容 digest、字节数、media/schema 类型和 store namespace 的持久对象引用；凭据只能用 secret handle。

```text
TaskRecord {
  record_schema_version: u32 = 1,
  task_id: UUID, task_revision: u64, record_revision: u64,
  agent_id: UUID, conversation_id: UUID,
  owner: { kind: conversation | task, id: UUID },
  background: bool = false,
  kind: string, definition_version: u32,
  implementation_ref: { plugin_id, artifact_digest, registry_revision },
  input_ref: ObjectRef,
  state:
    pending    { checkpoint_ref }
    | running  { checkpoint_ref, invocation_id, mode: run | abort, epoch }
    | waiting  { checkpoint_ref, on_task_ids[], join: owned_fail_fast | all_settled }
    | sleeping { checkpoint_ref, until_utc }
    | completing { outcome_ref }
    | terminal   { outcome_ref },
  abort_request?: { cancel_id, decision_ref, requested_commit, reason, scope },
  memos?: map<string, bounded JSON>,
  residual_case_ids: UUID[],
  created_by_step_id?: UUID,
  dispatch_eligibility: ready | blocked { reason_ref } | parked { reason_ref, until_utc? },
  no_progress_crashes: u32,
  first_run_at?: timestamp, terminal_at?: timestamp,
  created_commit: CommitRef, updated_commit: CommitRef
}
```

`owner`、task ID、conversation ID、输入在创建后不改；确需换输入则新建任务并关联前任。定义迁移可以转换 input/checkpoint 的编码，不能偷偷改用户任务语义。`terminal` 之后不再运行该任务代码；后续新证据更新关联 Operation 的事实视图，并创建后续任务，不重写历史 terminal outcome。

`background=true` 只允许 conversation-owned task。ordinary owned work 沿 task/conversation ownership 遍历，跳过 background task 及其整个子树；history parent 不参与取消/完成遍历。明确直接取消一个 background task 时仍取消它的 ordinary 子树。

`sleeping` 是子任务等待轮询/定时工作的能力，不把永续主体改为低频 Tick。主体拥有长期 root task；有限主体 step 不能直接拥有所有长期 child，否则按 Pi 的所有权规则每一步都会等待孩子结束。具体 SubjectStep 合同由 S0/S1 定义。

### X03. Operation 与 Attempt 分开：一次意图不等于一次网络尝试 `[S；依据 DUR-P3/DUR-P5]`

```text
OperationRecord {
  record_schema_version: u32 = 1,
  operation_id: UUID, record_revision: u64,
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

### X06. 调度器执行明确的任务转换，不凭 transcript 猜进度 `[P+S；DUR-P2/DUR-P4]`

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

### X07. T1：先提交真实的 intent 和 armed attempt，再允许外部执行 `[P+S；DUR-P3/DUR-P5]`

T1 是一个原子批次，至少包含：

1. 先按稳定调用 identity/logical key 查已提交记录。同一次 admission 的重复请求若参数匹配，只返回已存在 operation/attempt/result；不同 payload 返回冲突。已提交命中不因调用者携带的旧 revision 而被误当成新动作，也不授权再次发送。只有新 admission 或协调器已决定的新 attempt，才以预期 task_revision、record_revision、invocation_id、epoch 验证当前调用仍有效，并检查取消与现行委派。
2. 固定所有 before-hook/参数修复之后的参数、目标绑定、重试合同和 acceptance predicate。
3. 创建或核对唯一 OperationRecord，创建唯一 AttemptRecord，令 attempt=`armed`、operation=`in_flight`。
4. 保存相应 task checkpoint 与必要的 outbox/资源占用引用。

步骤 1–4 任何一项失败时整笔不成立，不发出这个 attempt。仅有 `prepared`、从未存在 armed attempt 的 operation 不能由执行器发送。准备阶段可以单独持久化 prepared operation；也可与首次 T1 合并。

`armed` 表示“这个 attempt 获准尝试效果”，不表示 bytes 已到平台。**T1 成功到收到可验证结果之间都属于可能已经发生效果的窗口**，包括崩溃实际上发生在第一字节发送前的情况。不能通过新增一个 `sent=true` 的本地标记消除这个窗口。

### X08. Effect 在事务外执行；提交幂等与业务幂等分别处理 `[P+S；DUR-P1/DUR-P3]`

运行 shell、tmux 输入、SSH、模型请求、QQ 发送、Git push、对象上传均在 Session/SQLite 写事务之外执行；不持有写锁等网络、推理、用户或 child。外部效果所需模型请求继续经过全局 broker。

T1 后只能执行已冻结的那份操作。重新展开 discovery、改参数或改目标必须成为新的明确意图，不在 replay 时偷偷再跑 before-hook 改写它。provider 返还外部 handle 时应尽早通过提交保存；handle 带 backend instance 身份，重启后验证仍有效，不能把旧 slot ID 当永久资源。

同一调用本身跨多个不可原子的外部域时，记录可区分子 Operation 及依赖。例如“改文件→git push→发 QQ”分别拥有自己的 T1/T2；三步不能共用一条“全部完成/全部回滚”。

### X09. T2：证据与完成状态不得形成撕裂 `[P+S；DUR-P1/DUR-P3]`

原始结果对象先按 Storage profile durably seal。T2 在一个批次中：新增/引用 EvidenceReceipt；更新 Attempt 和 Operation 投影；追加结果 entry；推进当前合法 task checkpoint 或 outcome；结算相应 submission；登记由结果产生的 outbox；使可重建视图得到该 commit 的通知。

若 T2 涉及任务终态，必须先满足 X06 的 ownership 收敛条件；仍有 ordinary child 时只写 completing/outcome，不提前写 terminal、最终 submission 结算或终态通知。允许的独立进展发送仍通过自身的 ReplyGrant 与 outbox。

只有确有匹配结果的 acceptance predicate 才能记录 succeeded。`transport_written`、provider accepted、远端执行完成和应用后置条件通过是不同层级。发 QQ 得到平台消息 ID，不等于用户已读；tmux 接受按键，不等于 coding agent 已接收或完成任务。

若任务已取消、epoch 已切换或 invocation 已结束，回执仍由 E1 保存；旧 callback 不能顺便完成以上任务推进。当前恢复器可读取证据，在自己有效的 T2 中处理剩余状态。终端任务和已发布事实不会因软件回退而被改写成从未执行。

### X10. 存储失败使当前 writer 失效，不能原地重试 `[P；DUR-P1/DUR-P6]`

T1、T2 或激活 CAS 的 storage settlement 一旦返回“不确定/错误”，当前 Session/Writer 封闭新写入与新 dispatch，保留已提交视图，停止以旧内存状态继续推进，按相应 scope 重开。

恢复后以相同稳定 submission/operation/attempt/activation identity 查询实际记录：存在则采用已提交结果，不存在才按重开后的条件重新提出提交。不能把 exception 当作没提交。已经进入 storage settlement 的批次，不因调用者取消等待而半途撤回；取消等待不生成 durable cancel。

存储 adapter 可以在其方法内部完成它能证明安全的 transient retry；不得把不确定提交交给上层 while-retry。新打开后如缺失已确认 CAS 对象或出现损坏，是 corruption，不按“没有这条记录”补做外部动作。

本条采用主规格规定的 SQLite durability profile，不复制 Pi 的同步默认值。Pi JSONL 即使 fsync=true，固定规范 L5399–5417 也没有在普通提交时显式 flush main marker，已 ACK 尾提交仍可能丢失，不能借其名称推导 Shenko 的断电承诺。

### X11. 恢复按记录分类；unknown 不改名为失败 `[P+S；DUR-P3/DUR-P5]`

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

### X12. 自动 replay 需同时满足持久合同和当前合同 `[P+S；DUR-P5]`

必须逐项确认：输入/目标与原意图一致；原合同与当前实现都允许重放；provider 幂等 key 的 namespace、有效期、参数约束仍成立，或条件写入的前置 revision 仍成立；最新授权与 target identity 匹配。key 过期不能通过换一个新 key 冒充原请求去重。

嵌套调用以稳定 logical key 找回原 child/operation。相同 key 不同工具名、参数或目标返回冲突；不得按照新代码调用顺序默默对应旧效果。动态分支调用推荐明确语义 key。runtime memo 的候选写入与返回赢家在一个提交内，先写者胜出；大对象放对象存储。

tmux send-keys、按回车确认、任意 shell、无端到端幂等承诺的消息发送默认不可自动 replay。主 Agent 仍可自主决定再次执行：创建新 operation，保存 decision_ref 和 `possible_duplicate_of`，显示重复效果的不确定性。这是新决定，不是恢复器偷偷重发。

### X13. Durable cancel、调用结束与平台停止分别表达 `[P+S；DUR-P4]`

取消顺序：提交 `abort_request` → 阻止新 run phase/新 T1 → signal 当前 invocation → 等待/终止其受管执行 → ordinary owned work 收敛 → 新 abort invocation 运行取消/清理策略 → 提交 outcome。

已经 `completing` 的任务保留原先决定的 outcome；后来的取消只对其仍存活的 owned work 生效，不再运行该任务的 abort handler。已经 terminal 的任务不再执行取消代码；需要补偿时创建新任务。

取消标记出现后，旧 run invocation 的 task commit 被拒绝；它已经发出的请求可能继续，回执仍进入 E1。调用者取消等待、网络断开、close、worker 本地退出，都不能代替 provider 停止证据。close 用于可恢复停机，不自动给任务写 aborted。

取消不合作的代码时只使用 runtime/进程实际支持的隔离和终止能力。不在“同进程任意 Rust callback”上承诺可靠强杀。远端进程无法证明停止时记录 stop pending/unknown 及后续 supervisor，不把主体永久卡在等待它退出的全局写锁里。

### X14. 补偿是有独立事实的新 operation `[P+S；DUR-P3/DUR-P4]`

补偿创建新 operation，`compensates=原 operation_id`，拥有独立的参数、权限、T1、attempt、T2、重试和 unknown。原效果及原回执继续存在。可以补偿部分效果，也可以失败；task outcome 应区分“取消已请求”“已停止”“已完成补偿”“补偿结果未知”。

一旦 abort handler 自身在补偿中崩溃，重开后应恢复其 checkpoint/补偿 operation，而非无条件再次撤回、删文件或退款。补偿不能绕过 child ownership 规则：要么在可恢复 abort phase 内编排，要么交给明确 supervisor 所有的持久补偿任务。取消流程不会递归取消掉唯一负责善后的任务。

缺少旧 task 定义时不运行猜测出来的补偿。Pi 的 orphaned 表示 task 代码未执行、效果可能未清理；Shenko 保留该语义，并创建带原始记录的修复/协调任务。没有代码不是成功完成。

### X15. 版本解释权与继续执行权分离 `[P+S；DUR-P4/DUR-P7]`

任务 reservation 固定 definition/version、implementation digest 与 registry snapshot；每个可交接 phase 边界再检查候选。迁移 checkpoint 与改为 running 同笔提交。缺定义、较旧实现无法读新记录或迁移失败，只阻塞相关任务；旧实例不能通过强制把版本号改小来运行。

正在执行的效果保留它调用时的 contract/decoder/reconciler。新插件接管不能用新参数 schema 重解释旧输入。持有旧 ABI 的 handler 在返回/交接前保留其运行闭包；不合作 handler 只能按真实隔离能力停止，停止后仍查外部事实。

迟到回执按原 operation、attempt、producer schema 路由到保留 reader，随后作为 E1 事实保存。producer epoch 只用于关联，不作为“丢弃事实”的理由；新控制 epoch 也不授予旧 callback 新命令权。无 reader 时保存 uninterpreted，并由主体自主安装 reader、迁移或派 child 调查。reader/ledger/验证器本身都允许升级；保留是当前恢复依赖，不是永久不可修改 Ring 0。

## 41.5 永续主体的一步与有限子任务

### 记录与生命周期

主体的长寿根记录与每次有限推理步分离。根记录不因“当前没有任务”终态化；正常可用时，每个 S1 都登记下一步，运行器立即把它交给同一 broker 调度。资源等待、适用的额度 admission pacing、故障恢复、设备挂起、人类主动暂停及有限代际交接均有明确状态。额度插件可以调节平均有限 generation 次数，但主体后继始终已登记、可被新事件重新调度；不能以没有消息为由改成低频定时思考 Tick。

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
  applied_steering_refs[],    // 独立持久且能证明本结果边界已采用的额外输入
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
| preparing → awaiting_model（S0） | 输入计划、角色工具集、persona/context revision、最终 token 预算及 provider 映射已验证 | 原子保存 InputPlan、不可变 context/wire 引用、request_id 与待入 broker 的调度意图 | 入队以 request_id 去重。S0 后的新事件不能修改原 RequestSeal；必要时取消/reseal，或使用带独立应用证据的真实 live-steering 协议 |
| awaiting_model → result_recorded | 同一 request/attempt 的完整结果已持久；可解析且结果边界已明确 | 保存原始结果、usage、结构化候选决定及来源 | 推理计算许可按 broker 的真实终止证据释放，与 S1 是否成功独立；不能为了等待写盘而让后端继续生成 |
| result_recorded → committed（S1） | 当前 epoch 有效；head 仍等于 expected_parent_commit；step_no 恰为后继；输入/工具版本与本步 RequestSeal 一致；额外 live-steering 输入有独立持久应用证据且已在本次结果边界生效；决定 schema、角色、任务与当前授权引用有效 | **一个原子批次**写决定、逐事件处置、child/memory/policy/publication 意图及稳定 ID、pending obligations、新 head 和唯一 next_step_id | 只在成功提交后向 worker/outbox 发布；发布重复时按稳定 ID 读取同一任务；下一步立即登记为 runnable 并交给同一 broker；若适用 §42 的额度策略，admission pacing 等待明确记为 quota_policy_wait，不改成无消息低频 Tick |
| preparing/awaiting_model → retry_pending | 准备失败、generation 中断或结果无法形成有效完整决定 | 记录原因、已产生证据、重试关联及仍待处理的事件；不推进 head | 不执行截断工具参数或未提交决定。确定旧请求不会继续生效且容量已对账后，再安排同一步的新 attempt |
| 任一未提交步 → retired | 代际移交、显式替代输入计划或失去控制权 | 保存 superseded/retired 原因与关联版本 | 迟到输出只进入 E1/usage；不能创建新 child、推进 head 或外发。接任者从最后 S1 接续 |



**未提交步骤替换 R0。** 当前 writer 若因新输入、版本交接或旧 attempt 无法继续而替换 head.next_step_id，必须在同一事务内：校验旧 head 的 record_revision 与旧 next_step_id；将旧步标 retired；创建同一 step_no、同一 expected_parent_commit 的新 step，记录 supersedes_step_id；把 head.next_step_id 指向该新 ID。R0 不推进 committed_step_no、不产生模型决定或 child。旧步骤的请求仍按实际停止证据记账；只有新有效步骤可提交 S1。不能单独 retire 旧步后留下指向它的 head。

S1 的比较并交换对相同 branch 的已提交 step_no 建唯一约束，对 successor_step_id 建唯一身份；不能只在内存中检查“没有另一条主循环”。单次 step 可重试多个 attempt，但最多一个决定被接受。断电后若 S1 已提交而 worker 未启动，重放调度意图；若未提交，则没有授权从该草稿产生外部动作。

S1 不能跨 LLM、SSH、网络调用或长时间等待持有 SQLite 写事务。决定、逐事件 EventDisposition、head、后继和意图 outbox 的权威同处控制账本 L；原始输入可在 I，真实任务服务/投递服务可在其他存储域，消费 L 的持久意图并按稳定 ID 去重。其他存储不能先执行草稿再等待 L 补提交；最终对外可观察的决定只有一个已提交版本。提交失败或不明时，以稳定 step_id 查明结果，不创建新 successor 试图“补一个”。

主体结果的结构化摘要可显示已提交意图、委派和依据，不要求公开内部逐 token 思维。模型内容采用 committed-only 发布：chunk 先持久化并取得稳定序号，再进入可恢复的流式发布；订阅者按序号去重/续接。未持久 token 不先显示成内容。临时 UI 仅可显示 cursor、rate 等非内容状态，不能成为动作、事件处置或完成证明。

### 连续主体与有限 child 的所有权

Pi Durable 要求普通 owned work 排空后，有限父任务才能终态化。Shenko 因此不能把所有长寿 child 挂在即将结束的 SubjectStep 下，否则每一步都会等待所有孩子。**持久委派挂在 subject_root_task_id 或相应有限委派任务下；SubjectStep 只通过 created_by_step_id 记录来源。** 这是一项 Shenko 的结构选择，不是修改 Pi 的 ownership 规则。

主体可登记“等 child D 的结果后继续目标 G”的 obligation，并在下一步继续其他思考。它不在整个主循环上执行 wait-for-all-children。有限 worker 需要等依赖时，使用持久等待记录并交还可安全释放的 invocation/执行席位；compute 只有在 never_dispatched、匹配终态或真实 gated-pause 等证明成立后才能释放。不得用 TaskWait 清除仍 armed/unknown 的实际占用。受预算限制的 ContextLease 可继续存在。

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

## 41.6 外部事件、注意力与输入放置

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
  processing_mirror?: { state: unresolved | interpreted | delegated | resolved,
                        disposition_refs[], mirrored_through: Map<ledger_id, seq> },
  record_revision
}

EventDisposition {
  disposition_id, event_id, event_revision, branch_id, step_id,
  disposition: interpreted | delegated | resolved | deferred,
  decision_ref, delegated_task_refs[], reason_ref?,
  committed_at: CommitRef
}

PresentationReceipt {
  presentation_id, event_id, event_revision,
  step_id, request_id, context_revision,
  representation: full | excerpt | aggregate_member | handle_only,
  source_span_ref?, committed_at: CommitRef
}
~~~

**权威存储位置。** 原始 InboxEvent 可以留在独立 ingress ledger I；EventDisposition、PresentationReceipt、输入撤回/选择控制记录与主体决定的权威位于控制 ledger L。S0 在 L 保存对 I 中精确 event revision 与原始对象的引用；S1 在 L 的同一事务写 EventDisposition、决定和后继。I 的 processing_mirror 仅为异步派生视图，不参与提交真相。Selection 读入 I 的候选后，必须用 L 的当前处置与撤回记录覆盖陈旧 ready 镜像；L 已提交而 I 尚未更新时崩溃，也不能重复委派该事件。对不同 availability branch 的独立处置保留 branch_id，合并时不伪造原先存在一个全局 writer。此合同不要求跨两个 SQLite 数据库原子写入。

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

## 41.7 插件操作接口与实际效果绑定

### 41.7.1 静态合同与本次实际绑定

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

### 41.7.2 调用接口与幂等身份

```ts
type AdmissionOptions = { expected_task_revision?: bigint; expected_record_revision?: bigint };
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
wait_operation(operation_id: string, after_record_revision?: bigint): Promise<OperationView>;
```

`prepare_operation()` 的唯一键是 `(task_id, logical_key)`。接纳先查询该 key：同 key、相同语义输入返回已有 operation；同 key、不同工具、合同或参数返回 `INTENT_KEY_CONFLICT`，不覆盖旧记录。只有不存在该 key 时才检查 `expected_task_revision` 等新接纳条件；后续任务提交造成旧 expected revision 过期，不阻止幂等命中取得原 receipt。revision 前置条件不属于 intent 的语义指纹。比较使用协议规定的 canonical encoding 或等价结构比较，不使用浮动格式化 JSON 文本。

`logical_key` 是父任务中稳定的语义调用位置；不是一次 provider tool-call ID，也不是时间戳。嵌套调用把子 intent 的接纳和父任务对该子 operation 的引用放在同一事务中。恢复后相同 key 重新取得同一孩子及其结果，不能再建一份相同子任务。代码升级改变一个已使用 key 的含义，必须迁移或使用新 key。

`operation_id` 在所有重试中稳定；每次允许的外部尝试使用不同 `attempt_id`。默认同一 operation 不得并存两个具有外部执行权的 attempt。上次执行状态未知时只能查询/核对，或者使用已证实仍有效的外部幂等合同重试。框架生成了 operation ID，不代表 QQ、SSH 或任意 HTTP 服务已经支持幂等。

`dispatch_operation()` 仅对从未派发的 prepared operation 原子接纳首个 attempt；已接纳的调用返回当前视图，不新增尝试。`retry_operation()` 只在合法 `reconciling → in_flight` 转换中创建后续 attempt，先按 `(operation_id, retry_key)` 查重，再检查 basis 的适用范围和当前有效性；合同声明与本次证据均不支持时返回 `RETRY_NOT_PROVEN`。已 resolved 的操作若要有意再做，创建新 operation；只有新矛盾证据通过协调事件把旧操作转回 reconciling，才重新进入该恢复路径。幂等键作用域、过期时间和对相同参数的要求属于外部协议，不能由通用 API 猜测补全。

主体决定在未知情况下有意再做一次时，创建新 intent，以 `retry_of` 与 `possible_duplicate_of` 指向原操作；记录“这是再次尝试产生效果”的决定。补偿也创建新 operation 并设置 `compensates`。二者都不修改原事实。

### 41.7.3 状态、回执与先后顺序

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

`cancel_operation()` 以 `(operation_id, cancel_key)` 幂等创建或取得 `operation.cancel` 任务及其决定，返回 `CancelReceipt`，不等待远端必然停止。该任务由持久协调 supervisor 持有；调用者取消等待不取消它。`wait_operation()` 在 record_revision 增加或已存在 resolved 状态时返回视图；取消该等待不改变操作。父任务可以在将未决 operation 和 case 职责交给持久 supervisor 后结束本轮等待，不能随父任务回收未决事实。

回执按 EvidenceIngress 的来源作用域与稳定 `source_event_key` 去重；同作用域/key、不同原文返回 `RECEIPT_KEY_CONFLICT` 并保存冲突证据，不能 last-write-wins 覆盖。只有相同内容 hash 不能证明两次外部事件是同一事件。派生的 `OperationView` 可更新，原 receipt 不可原地改写。读不懂的新 payload 保留原字节与 decoder 引用，显示 `READER_UNAVAILABLE`，不能当成功或无效果。

工具的模型文本、程序结构化输出和界面 detail 可以分别投影。`afterTool` 或摘要插件只修改投影，不修改 receipt、operation identity、实际 effect 状态或原始输出证据。任务最终结果引用 durable data，不引用将在父任务结束时自动销毁的临时文档。

来源：REF-DURABLE [§5.2 effect sandwich](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2045)、[§7.3 嵌套调用身份](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L3162)。本产品增加实际资源绑定、长期 effect receipts、跨 generation 核对；这不是任意外部效果的 exactly-once 或撤销保证。

## 41.8 Hook、上下文与请求封口

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

需要外部 I/O/LLM 的 hook 先创建可恢复准备任务，在事务与 compute permit 之外执行。可选贡献首版默认最多等待 250ms 的本地准备预算，超时登记 omitted/deferred；后台任务仍可完成并进入下一步。该预算不强制取消已发生的外部操作。必需贡献不能按 optional 跳过，必须明确等待、采用已验证的旧图/已有结果，或产生恢复事件；不得静默用空值绕过校验。

### 41.8.1 扩展点、处理器与图

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

### 41.8.2 各类输出的合成规则

| mode | 合法输出 | 合成与失败规则 |
| --- | --- | --- |
| observe | `no_change`、`defer` | 不改输入；异步观察不能阻止已提交事实发布。需要后续动作则提交独立任务 |
| transform | `no_change`、`replace`、`defer`、`reject` | 按固定顺序串行变换；每次验证 output schema 和 writable paths |
| collect | `no_change`、`contribute`、`defer`、`reject` | 按固定处理器顺序和 contribution key 合并，不能按 Promise 完成顺序拼接 |
| decide | `no_change`、`decide`、`defer`、`reject` | 第一个有效 decision 结束此扩展点；`no_change` 表示继续询问 |
| around | 本扩展点定义的结果 | `next()` 至多调用一次；重试必须成为 operation attempt，不能隐式多次 next |

同 slot/key 的冲突只有在该 slot 声明了明确 reducer 时才能合并；否则报 `CONTRIBUTION_CONFLICT`。不存在“后完成的覆盖前面的”默认行为。Transform 写入不在 `writable_paths` 中的字段报 `HOOK_WRITE_VIOLATION`。无论 failure 策略如何，非法修改都不能被采用。

`report_and_continue` 只跳过该处理器本次输出并留下诊断；它不能把一个已发出的外部效果抹掉。如果 hook 需要产生外部效果，必须调用 §41.7 operation 接口；hook 重试只重取已有 operation。纯声明“deterministic”不能免除真实外部调用的 ledger。

observe 返回 `defer` 只登记后续独立任务，不把已经完成的源单元转回等待；需要当前 request 等待其结果的处理器必须注册到允许贡献或变换的扩展点。hook 产生的嵌套模型调用进入同一 broker，默认不再次运行该 hook 自己；显式递归仍必须声明独立 dispatch identity 并通过等待环检查。

### 41.8.3 RequestSeal

```ts
type RequestSeal = {
  request_id: string;
  request_lineage_id: string;
  request_revision: number;
  supersedes_request_id?: string;
  seal_schema_version: number;
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

1. 在主循环可提交边界选择已持久事件与任务状态；提交 `InputPlan`，其中包含各 ledger 的读取水位向量、实际 selected event refs/revisions 与 causal support refs，再创建新 request_id 及同 lineage 的新 request_revision。读取到 seq 105 不代表已消费所有 seq≤105 的事件。
2. 固定本次 hook graph，运行 context/tool/provider 的语义变换与必要依赖任务。
3. 组装人格、历史、已选择事件、按需能力和瞬态提醒，执行预算与工具调用结构校验；保存各 contribution 的来源及本次生命周期。
4. 将模型将看到的精确 messages、工具声明及模型/provider 语义参数提交为 `RequestSeal`。
5. 进入推理 broker 队列，获得 permit，调用后端；实际 lane/KV affinity/尝试与资源回执另行记录。

上述次序是协议顺序；实现可并行纯计算，但不能越过依赖和提交边界。需要等待模型的 hook 发生在步骤 4 之前，且父请求没有持有推理 permit。若某处理器在已有 permit 的路径上要求模型依赖，只有尚未 armed 的 attempt 才可在同一事务中撤销派发资格、形成 `never_dispatched` 证明并释放 Allocation，再登记 `waiting_dependency`。已经 armed/active/unknown 的请求必须先取得本章认可的 QuiescenceProof 才能释放；无法取得时，把模型辅助工作延后到后继请求。不能仅改成等待状态就清除物理占用。禁止持有 writer 事务等待，也禁止等待只有“本主请求完成后”才能产生的依赖；后者返回 `DEPENDENCY_CYCLE` 或将该工作改成下一轮贡献。

Seal 后只允许改变不改变模型输入语义的传输字段，例如凭据刷新、连接信息及后端实际支持的 KV affinity；每次 transport attempt 记录其真实字段。切换模型、修改输出上限、修改 native 语义选项、工具 schema 或 prompt，均生成新 request_id 及同 lineage 的新 request_revision，并重新预算、封口和排队。调度器不能一边保留旧 prefix 身份，一边隐蔽改写其实际 token 输入。

`prefix_digest` 关联精确输入前缀及影响其解释的模型/tokenizer 身份，用于一致性和调度提示；摘要相同不等于后端真实 cache hit，KV 是否驻留或可恢复以 provider 的具体回执为准。

stream 开始后，新消息继续被持久接纳，但不能插入该请求过去的 messages。它进入下一边界，或触发有真实后端支持的取消/steering 路径。取消后的部分输出保留为事实；是否进入后续模型上下文由恢复/上下文合同决定，不能伪造完整 tool-call/result 配对。

工具调用必须匹配 `RequestSeal.offered_tools`。按 sealed contract 解释参数，绑定该版本实现或显式的兼容适配器。旧实现不可用且不存在声明过的适配器，返回 `TOOL_CONTRACT_UNAVAILABLE`；不能用当前同名工具静默替换。当前权限和资源绑定仍在派发时重新核验。

### 41.8.4 hook 恢复与热更新

一个 dispatch 固定 `graph_digest`、输入 revision 和处理器工件。模型辅助等非确定结果按 `(dispatch_id, handler_digest, input_digest)` 保存；重启重用已提交结果，不再次调用模型制造另一答案。输入已变化时创建新 dispatch，不把陈旧异步结果追加到已 sealed 请求。

热更新发布新图只影响新的 dispatch/request_id。旧图正在运行时保留其工件租约。晚到的 hook 输出若仍属于可继续的旧 dispatch，可以完成它；若该请求已取消/替代，记录结果并标记未消费，不能自动注入新请求。

REF-FABRIC 只支持类型化扩展点/phase 顺序方面的来源；上述持久 dispatch、permit 释放、seal 和恢复为 REF-DESIGN。REF-DURABLE 的 [§7.2 hooks](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2906) 与 [§7.4 prompt/tool 变更](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L3661) 是具体参考。与其当前 phase 再解析工具名的规则不同，Shenko 明确绑定 sealed contract，避免热更新重解释旧调用。

## 41.9 推理请求、计算许可与单并发

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
         unknown | quiesced | retry_pending | terminal,
  active_attempt_id?, allocation_id?, stop_intent_ref?, retry_decision_ref?,
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
| armed/active/stop_requested/unknown → terminal 或 retry_pending | 得到匹配 attempt 的终态事实，或确认 engine/物理执行实例已销毁；快速完整响应/明确拒绝允许跳过 active 的单独落账 | E1/T2 先保存 attempt 的终态、usage/partial 和 QuiescenceProof，并原子释放其 Allocation。逻辑结果已完成或明确不再继续才将 Request 标 terminal；同输入仍需合法重试时，保存 retry_decision_ref 并转 retry_pending |
| retry_pending → queued | 原 seal 不变；任务仍需要结果；上次 attempt 已确定静止；恢复/重试策略允许且任何外部计费风险已有记录 | 创建新的 attempt_id 并重新入队；不复活旧 Allocation。terminal Request 不重新打开；需改变语义时创建新的 request_id |
| active/stop_requested → quiesced | 后端提供可核验的暂停完成，并且恢复必须经 broker 重新准入 | 保存 gated_pause 证明后可释放 compute；ContextLease/状态字节独立保留。恢复先重新取得 Allocation |
| quiesced → queued | 原输入/恢复 frontier 有效；当前任务仍需要结果；恢复操作由 broker 独占控制 | 同一逻辑请求的恢复 attempt 入队；保留历史输出/usage，不能把 replay 当新输出再发布 |

Attempt 的终态和 Request 的逻辑终态分别记录。一个明确拒绝而没有执行的 attempt 可结束并释放占用，同时其 Request 等待重试；一次完整成功结果则通常使 Request 终态。未知执行不能被归为可重试的明确拒绝；发生过部分生成时，只有协议证明可续接，或策略明确记录重新计算、重复费用与原 partial 处理方式后，才创建后继 attempt。

后端内部 paused、Host parked、排队状态或没有 token 输出，**都不足以**形成 gated_pause。若 engine 会自行恢复原请求，它仍是 broker 未结束的工作；适配器必须继续计占用，或在后端整体受控的容量模型中证明不会超额。不得先释放 C 再希望旧请求恰好不恢复。

允许释放容量的证据由具体 adapter 合同定义。例如 Strata 的已绑定 slot 在收到对应 BDONE 后可按其协议认定执行槽空闲；仅写出 BSTOP 不足。通用 HTTP 连接断开则通常没有同等级证明。后端一个新连接或新的客户端 session ID 也不能证明旧 server request 消失。

本地 request terminal、compute 已停止、逻辑任务完成、远程 shell 已停止和外部动作已撤销是五个不同事实。取消 generation 不取消已发出的 SSH/GUI/文件效果；这些 operation 继续按自己的 ledger 回收回执和对账。

### 首版调度默认与公平性

首版默认使用有限 generation：默认输出预算 2048 token，按模型实际接口计入推理/可见输出及工具参数开销；模型 profile 可以采用适合该模型的有限上限。达到 length 边界但工具参数未完整时，不执行 partial，而把完整可用输出与不足情况存档，再安排后继/重建。上限是可改工程默认，不是 128K 输入窗口，也不是每步必须耗尽的目标。

普通 ready 请求按主体与 child 的有效任务队列公平轮转；在双方都持续可运行且没有更高等级事件的情形下，默认不连续服务同一侧超过两个完整 generation。child 侧再按任务 lineage、老化与依赖关系选择，递归创建新 task 不增加该父系的总公平份额。实际 token、prefill/replay 和时间成本用于更新策略，不用“每个请求次数相同”冒充耗时相同。

外部高紧急事件仍按 D3 可先服务；紧急来源不能提权。主循环每步立即登记后继，但登记不意味着永远抢在 child 前面。正常请求有限轮转是计算公平协议，不是低频 tick。即使后端完全不支持中途取消，有限请求完成后的轮转也必须使双方继续推进。

同一 child 在两次 durable checkpoint 之间被连续取消三次时，默认生成一次合并的 attention-starvation 建议，交给主体决定 DND、调整任务、改变模型或继续接受打断；不自动向人类索要批准。建议带取消成本、已完成/未完成工作和最老待办；同一停滞 case 在没有新证据/策略变化前不重复刷屏。任何固定阈值均属于可替换策略插件。

取消/断连对账使用确定性 I/O，不等主体取得被占用的唯一槽后才查询。首版控制请求超时只改变 knowledge state；可用有界指数退避和持久 next_probe_at 安排重试，但任何重试时钟都不代替主体连续思考，也不产生“超时即免费”的容量漏洞。无可查询路径时保持 unknown，并允许主体/恢复插件选择已授权的引擎重启、隔离或独立后端；选择前如实保留受影响容量。

## 41.10 KV 与原生后端能力

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

## 41.11 远端 Seat 问题、决定与答复

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

## 41.12 自主升级与跨代交接

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
  agent_id: UUID, selector_revision: u64,
  active_epoch: u64, generation: Digest,
  state_ref: ObjectRef, activation_id: UUID,
  route_authority_ref: ObjectRef
}
```

Selector 的成功条件更新是激活的线性化点；磁盘上的 executable symlink、进程 PID、Web UI 中显示的版本都不取代它。`ActivationStore` 提供满足该部署 profile 的持久 compare-and-swap。单机 SQLite 的条件更新不自动构成多节点共识。

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

## 41.13 自解释的版本锚点与读取接口

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

## 41.14 结构化错误与恢复动作

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

## 41.15 QQ 到远端开发再到自身升级的完整轨迹

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

## 41.16 规范故障与一致性验收

以下 DX、RT、P41 三组按相应合同选取；重叠用例可以共用 fixture，但保留每个语义断言的对应关系。本轮未执行这些产品测试，不能把规格审查当作恢复验收。

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

## 41.17 精确上游锚点与本产品差异

以下 `[P]` 表示直接吸收 Pi 固定版本规范；`[S]` 表示 Shenko 针对自身产品目标推导；`[U]` 表示用户已经裁决的需求。`[P]` 不是声称本次运行过 Pi 的实现或测试。

主要上游锚点：

- **DUR-P1 原子与可见性**：[spec.md L41–78](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L41)、[L1696–1731](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L1696)。
- **DUR-P2 任务与恢复**：[L1752–1812](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L1752)、[L1932–2043](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L1932)。
- **DUR-P3 Effect sandwich 与 terminal receipt**：[L2045–2098](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2045)。
- **DUR-P4 取消、版本和所有权**：[L2100–2309](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2100)、[L2311–2407](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2311)。
- **DUR-P5 工具重放**：[L3157–3214](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L3157)、[L4314–4372](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L4314)。
- **DUR-P6 存储失败**：[L5256–5316](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L5256)。
- **DUR-P7 迁移与非目标**：[L1591–1624](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L1591)、[L5580–5594](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L5580)。

Pi 并未在这些规范中提供 Shenko 的多节点选主、原始外部效果事实账本、完整 A/B 升级、跨版本迟到回执或任意第三方进程强杀。这些应标为 Shenko 扩展。尤其不能把 `ctx.effect` cleanup 或 Pi 的 abort handler 当成外部事务逆操作。