# Durable State — Ledger, Archive and Stream Journal

**内核定位。** 本文承载[内核合同](kernel.md)中[内核 1](kernel.md#kernel-1)、[内核 3](kernel.md#kernel-3)、[内核 4](kernel.md#kernel-4)与[内核 5](kernel.md#kernel-5)的账本形状。文中的认知档案等领域表是参考插件的状态；收敛方向是让它们随各自的插件迁出账本 crate，见[内核收敛计划](../development/kernel-convergence.md)。该步骤落地之前，本文描述的归属仍是实际情况。

Scope: the `shenko-store` ledger/commit surface, the `shenko-types` store and
commit types, the store migrations, the cognition archive (schema, fact
classes, conflict units) and the durable content stream journal.

CAS publication internals and the raw-session redactor are owned elsewhere;
this document states only the contracts this surface relies on. Everything
described here is source in this worktree. The build, the native proof and
the cross-store recovery run are the integrator's work — nothing here is
claimed as verified.

## Generic write path ([§41.2](durable-state.md#spec-41-2))

`Store::commit(WriteBatch)` is the generic admission line. Its guarantees:

- **Objects before facts.** Every `batch.raw_objects` entry is sealed into
  CAS before the ledger transaction opens; `CommitReceipt.sealed_objects`
  reports the object refs admitted. An object left behind by a failed commit
  is a content-addressed orphan, never a dangling reference.
- **E1 closure.** Every `evidence_upserts` receipt's `raw_ref` (and
  `normalized_ref` when present) must already resolve in CAS — sealed by this
  batch or by an earlier commit — or the whole batch is refused with
  `ShenkoError::ObjectClosureIncomplete`. When a new receipt names an
  existing `operation_id`, that operation's `evidence_through` cursor
  advances to this commit in the same transaction. A resolved operation
  already holds its final fact, so its cursor is not rewritten; the receipt
  itself stays durable.
- **Revision CAS.** `task_upserts` and `operation_upserts` admit only the
  successor of the stored `record_revision`. An identical replay is a no-op;
  anything else is a typed `ShenkoError::Conflict`, never `INSERT OR
  REPLACE`. A task or operation that already holds a terminal fact (a
  terminal task, an operation with an outcome in state `Resolved`) is never
  rewritten by any writer, successor revision or not.
- **Append-only facts.** `attempt_upserts` and `evidence_upserts` are
  immutable: same identity with different content is a conflict.
- **X05 source-key dedup.** A receipt whose `source_event_key` already bound
  an earlier durable receipt binds to the first fact and inserts nothing.
- **Inbox events.** `inbox_event_upserts` no longer uses `INSERT OR IGNORE`:
  identical replays are no-ops, a different fact under an existing event id
  is a typed conflict — the raw-session semantics.

## Migrations

`run_migrations` stays additive-only. It applies every domain's schema
unconditionally through `AGGREGATE_DOMAIN_SCHEMA`, so the replicated fact
classes exist on every store regardless of which module first asked for
them. Domain modules keep their own `CREATE IF NOT EXISTS` constants;
double application is idempotent.

The aggregate mirrors, column for column, the owning domain's DDL:

| Fact class | Owner |
| --- | --- |
| research epochs / candidates / intervention bank / reminders / retention tombstones / archive moves | `store::research` |
| seat records / leases / questions / answer decisions | `store::seat` |
| person claims / identity events / admin facts / life voiceprint ranks | `store::contacts` |
| owner grants, config entries / changes / adoptions | `store::config` |
| artifact records / manifests / runtime instances | `store::artifacts` |
| channel runtime facts / record migrations | `store::channel` |
| activation records / selectors | `store::activation` |
| subject output parts | `store::output_parts` |
| media recordings / recording controls / priority inputs | `store::media` |
| content streams / content stream units | `store::content_stream` |
| subject request attempts | `store::subject_request` |
| subject obligations / subject attention policies | `store::subject_state` |

The credential digest table `owner_admin_tokens` exists locally and is never
replicated. Process-local bookkeeping (`cognition_*`, `commit_log`,
`subject_dispatch_owner`, `subject_quiesce_requests`) is never replicated
either. Memory-v3 columns (entry confidence / supersession / Git refs; intent
knowledge kind / applicability / base revision / idempotency / sources /
supersedes) run after `intents::SCHEMA`, and the subject-owned memory intent
keeps its dedicated representation: the operation is `save`, the knowledge
kind is `note`, and a missing binding sidecar is that representation's
shape, not corruption.

## Cognition archive ([§5.4](nodes.md#spec-5-4)、[§36](durable-state.md#spec-36))

`ARCHIVE_SCHEMA` is 5. Readers admit schemas 2–5; an archive naming a fact
class this reader does not know is refused whole at manifest validation.

**Membership is declared, not assumed.** Every archive carries the list of
fact classes it was taken from (`CognitionManifest.fact_classes`). The
reader checks it: an archive naming an unknown class is refused, and an
archive carrying a row outside its own declared membership is refused. A
class is a durable fact family, not a table with rows — an export of a store
with no research epoch still declares the research class.

**Version and membership migration.** An archive of an older schema is
migrated to the current column layout before it is validated: the layout
only ever grew, so the migration is by column NAME and every fact keeps its
identity. A legacy column this schema no longer knows is reported as a
non-blocking `migrated_legacy_column` gap instead of disappearing silently.
The current schema is checked strictly: a row whose column layout does not
match exactly is refused.

**REQUIRED facts.** Some facts cannot exist without their anchor: a stream
unit without its stream, a seat lease without its seat, an answer decision
without its question, a research candidate without its epoch, a memory
binding without its intent, a quota send without its attempt, a media
recording or priority input without its stream, an artifact manifest or
runtime instance without its artifact. A peer that ships a member without
its anchor has not shipped a complete fact: the export reports the gap and
the import refuses the archive instead of admitting an orphan.

A quota event names the fact it describes by id in its payload, and it is
written in the same transaction as that fact. The archive therefore also
refuses an event whose `snapshot_id` / `forecast_id` / `decision_id` /
`policy_id` is not carried by the same archive (`missing_cause`): an event
never travels without its cause.

The subject's own facts carry the same rule: an obligation and an attention
policy name the sealed decision of the SAME step that authored them, and an
obligation's origin event must be an event the archive knows. A row whose
step or event is missing (`missing_step`, `missing_origin_event`) or whose
decision is not its step's decision (`decision_mismatch`) is an incomplete
fact: the archive is refused rather than interpreted loosely. Whether the
origin event was actually SELECTED into that step's sealed S0 is checked
where the decision is admitted (the subject's own decision gate), not
re-derived from archive rows.

**Conflict units, not LWW.** Rows of a semantic unit apply inside one
savepoint; a member that diverges from a local fact rejects the whole unit
and every member is reported:

- `epoch/{epoch_id}` — research epoch, candidates, bank entries, reminders
- `seat/{seat_id}` — seat record, leases, questions, decisions (a decision
  binds through its question's seat)
- `quota_request/{request_id}` — quota attempts and sends
- `quota_event/{event_id}` — one quota event (its own unit)
- `tombstone/{tombstone_id}` — one retention tombstone
- `channel_fact/{fact_id}` — one channel runtime fact
- `channel_migration/{record_kind}/{record_id}` — one channel record migration
- `person/{agent}/{person_id}` — claims, identity events, person admin facts
- `activation/{agent_id}` — activation records and the live selector
- `media/{stream_key}` — recordings, recording controls, priority inputs
- `artifact/{artifact_id}` — artifact record, manifests, runtime instances
- `config/{namespace}/{logical_key}` — config entry, changes, adoptions
- `grant/{installation_id}` — owner grants
- `request/{request_id}` — the subject's durable request attempt
- `obligation/{obligation_id}` — the subject's durable obligation
- `attention/{intent_id}` — the subject's own attention decision
- `output/{agent}/{stream}/{message}` — output parts of one message
- `stream/{stream_id}` — the journal stream and all of its units

**Merge rules per class, not a blind verbatim copy.** Two classes have their
own rule:

- *Monotone* facts (`intervention_bank_entries`, `retention_tombstones`) —
  a peer may retire a live local row; a peer may never revive a retired
  one, and the first durable retirement of a fact is the one that stays.
- *Revisioned* facts (config entries / changes / adoptions, owner grants,
  artifact records / manifests / runtime instances, activation records and
  selectors, channel runtime facts, the subject's attention decisions) —
  the peer advances the record only with a strictly greater revision of the
  record itself. Which peer answered first never decides.
- *Laddered* facts (the subject's request attempts and obligations) — the
  peer may move the row FORWARD along its state ladder and never backward:
  `armed -> dispatched -> terminal` for a request attempt, `open ->
  completed | cancelled` for an obligation. A rewind is a conflict, and a
  terminal state is never rewritten. The stored state text is read in both
  the bare and the serialized form, so the writer's encoding does not
  change the rule.

Every other class keeps the delivered-baseline rule: the peer may update a
record only while this store still holds the value the peer last delivered,
and any other divergence is a reported conflict, not an overwrite.

Legacy archives may migrate columns by name, but migration is not permission
to accept contradictory subject-head facts. Before publishing rows or files,
the importer compares the head's identity, branch, step, revision and mode
index columns with the typed serialized head. A mismatch rejects the import
without advancing the receiving ledger or publishing a subject. Current
schemas additionally retain the canonical column-layout check.

**The closure collector.** A JSON map that names a digest is still a
container: the collector records the digest as an alias AND descends into
the map's other values, so a manifest that names its artifact inline does
not hide the objects it points at.

<a id="content-stream-journal"></a>
## Content stream journal

`content_streams` / `content_stream_units` record every unit a transport
delivered, bound to the durable work identity, so a UI reconnect or a
recovering consumer re-reads what it missed instead of trusting a callback
that died with the process.

Contract on `Store`:

- `open_content_stream(stream_id, agent_id, kind, binding)` — registers a
  stream against the durable identity of the work that produces it. The
  same stream id with a different binding is a conflict.
- `journal_content_unit(stream_id, seq, shape, content_kind, data, terminal)`
  — one unit. `shape` is `whole` (a complete request/result/effect unit) or
  `chunk` (one bounded piece of a stream); the two shapes are never merged.
- `journal_content_batch(stream_id, seq, content_kind, data)` — one bounded
  chunk unit: the producer's flush boundary IS the durable unit boundary.
- `journal_content_terminal(stream_id, seq, content_kind, data)` — the whole
  unit that ends the stream. The terminal row exists even when the stream
  published no chunk at all.
- `close_content_stream(stream_id)` — closes a stream; idempotent.
- `content_stream_units(stream_id, since_seq)` — the reconnect read.
- `content_streams(agent_id)` — every stream of one agent.
- `content_stream::provider_stream_id(request_id)` — the one canonical
  stream name (`provider/{request_id}`) for provider, subject and UI.
- `ContentStreamBatcher::new(store, stream_id, content_kind)` — the
  producer-side bound: a batch is journaled at 4 KiB or 100 ms, whichever
  comes first, and the last partial batch is a real unit.

Bounds are durable, not advisory: a chunk unit above 4 KiB and a whole unit
above 1 MiB are refused as typed conflicts rather than silently split. A
unit after the terminal unit or after an explicit close is refused. An
identical replay of `(stream_id, seq)` is a no-op that answers with the
original admission commit; a different unit at the same position is a
conflict.

`commit_step_and_advance_head` journals each committed step's model result
as a `whole` `model_result` unit of the `model/{agent}/{branch}` stream in
the same S1 transaction, bound to the request / attempt / step identity.

Journal rows replicate under the `stream/{stream_id}` conflict unit, so a
recovered node replays the same units instead of re-streaming.

## Error and recovery notes

- Staged installation inspection is Linux-only: its confined snapshot path
  uses Linux `pipe2`, errno and fork behavior. Other targets return an
  unsupported error rather than substituting an unconfined copy.
- Directory publication uses the shared `sync_directory` boundary for both
  CAS objects and restored recovery databases. Unix retains directory
  `fsync`; non-Unix returns `io::ErrorKind::Unsupported`. Journaling is not
  a replacement for this requested durability barrier. A failure must not
  be reported as durable success and does not promise rollback of files
  already created. This change does not make Windows Store usable or prove
  crash/power-loss recovery on any platform.
- `ObjectClosureIncomplete` and `Conflict` on the generic path roll the
  whole batch back; nothing partial commits.
- A conflicting unit keeps the complete local unit and reports every
  member; the import acknowledgement stays honest about what was refused.
- An archive with a missing anchor, an unknown fact class, a row outside
  its declared membership or an unverifiable payload publishes nothing.
- A journal unit is never rewritten: a corrected tail is a new sequence
  position, and the reader re-reads by position.

The peer's "verified recoverable" claim is derived, never asserted: a
receipt is verified only when the imported snapshot has no blocking gap and
no conflict. Node unit storage is not a replicated fact class here — it
travels as the bytes of the node's own database under a memory or workspace
root, and this reader never interprets its sections. A reader that must
admit a unit schema it does not know has to refuse that unit, not skip the
section and keep claiming a verified recovery.

<a id="spec-10"></a>
<!-- migrated-contract: SPEC:10 -->
# 10 文件版本与恢复

首版 StoreProvider、TaskRecord、OperationRecord、AttemptRecord、T1/E1/T2 与失败重开的规范合同见 [§41](architecture.md#spec-41)。文件系统快照、任务状态与已经发生的外部事实各自有记录，恢复不能将它们混为一次 rollback。

<!-- /migrated-contract: SPEC:10 -->

<a id="spec-10-1"></a>
<!-- migrated-contract: SPEC:10.1 -->
## 10.1 三个状态域

| 状态域 | 示例 | 回滚规则 |
| --- | --- | --- |
| 可替换软件 | rootfs、包闭包、harness/plugin generation | 可退到健康版本，最终退 factory |
| 受管理工作区 | 子任务文件、试验产物、受管记忆分支 | 以指定 snapshot 分叉/比较/恢复/提升 |
| 不随沙箱倒退的持久事实 | 身份、公钥绑定、权限撤销、原始会话、任务/副作用回执、删除墓碑 | 沙箱恢复不可回滚；专用备份/迁移/删除协议 |

快照是文件状态，不是 CPU/进程内存、活跃 SSH 连接、模型 KV 或已经发出的消息。直接 RW host mount、远端 SSH 写入与对外推送均是外部状态；恢复文件快照不能宣称撤销了这些行为。

<!-- /migrated-contract: SPEC:10.1 -->

<a id="spec-10-2"></a>
<!-- migrated-contract: SPEC:10.2 -->
## 10.2 二进制友好的版本层

本产品的 WASM 状态层采用独立实现：不可变内容对象、文件树与原子 branch head，研究内容定义分块/rope 和 copy-on-write；二进制共享已有对象，文本支持人类可读 diff。二进制冲突保留候选，不假合并；已压缩/加密文件是否适合细分块需基准验证。它不是重新实现完整 LayerFS；完整 Linux 分支环境按下一节直接使用原版。

每个 workspace 绑定 task、worker instance、scope、capability、base generation 和 parent branch。fork 不复制凭据或控制 ledger。多个 memory worker 用不同 worktree/COW branch，再串行发布，不能同时 checkout 同一个 live Git 工作树。版本服务向子 Agent 提供 fork/checkpoint/diff/merge/promote/discard，主体只能委派这些动作并接收回执。

<!-- /migrated-contract: SPEC:10.2 -->

<a id="spec-10-2-1"></a>
<!-- migrated-contract: SPEC:10.2.1 -->
### 10.2.1 LayerFS 原版运行于真实 Linux VM

按用户最新决定，完整 Linux 分支工作区直接接入 LayerFS 原版；不把重新实现 LayerFS、裁剪其 Store 或移植其 Docker/FUSE 路径作为默认工程。本产品的 WASM 文件状态则独立设计，复用已验证的内容寻址、不可变对象、原子根引用和持久提交原则，两者通过版本引用与显式同步协作。自主移植能力仍然存在，但不再把 LayerFS WASM port 作为实现个人 Agent 的前置条件。

固定 v1.0.0 source release `2aaf2552ce21154928b9f9c83582e41a637f5910` 的 Store 有明确平台合同：所有直接 opener 必须共享同一个 Linux kernel 和本地文件系统；打开前的 guard 检查 Store 父目录的 ext4/xfs/btrfs 类型。跨 VM 的 virtiofs/NFS 共享目录不能据宿主底层是 ext4 就当作满足条件。SDK 的 fs/branch fast path 与完整 workspace 也不同：前者直接打开 Store，后者的标准 mount/exec/commit 路径还需要 Docker Engine、FUSE、设备和挂载权限、匹配 runtime image 及 plain local named volume。[Store guard](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/core/crates/layerfs-store-service/src/kernel_guard.rs)、[SDK 合同](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/sdk/js/README.md)、[Docker backend](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/core/crates/layerfs-sandbox/src/backend/docker/container.rs)

推荐拓扑是一个合格 Linux VM 内共同运行 node adapter、同版 SDK/native addon、Docker Engine、daemon/Sandboxes 与本地 Store；有限 child 经 adapter 操作 branch/workspace。其他节点调用该 adapter 或交换版本化产物，不跨 kernel 直接打开同一个 SQLite Store。Adapter 是本产品计划实现的插件接口，不宣称上游已有完整远程 Store API。操作账本绑定 task、worker、node、runtime generation、scope、branch/workspace 与 invocation ID；镜像升级和 Store schema 升级分开验证。

BoxLite 作为 [§34](desktop-runtime.md#spec-34) 的优先候选进入组合验收；其实际 ext4 block/QCOW2 rootfs 路径提供基础，但仍须验证 guest 内 Docker、FUSE、设备、cgroup、mount 和权限配置。普通 Docker-in-guest 不等于嵌套硬件虚拟化。若某 BoxLite 版本/profile 不满足，就选择其他合格的 Linux VM provider 运行原版 LayerFS。[BoxLite rootfs](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/init/tasks/container_rootfs.rs)、[guest capabilities](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/guest/src/container/capabilities.rs)

发布版 Disposable/WAL/`synchronous=OFF` 不提供崩溃/断电耐久保证，不能因放入 VM 或使用 QCOW2 自动升级。产品区分 `layerfs_committed` 与 `product_durable`：前者是上游当前 profile 的提交回执，后者只有在选定成果、所需对象闭包、来源及 durable manifest 按本产品持久协议发表后才成立。唯一原始 session、身份/撤销、外部效果账本和已确认记忆不只放在 Disposable Store。备份整个 Store 时必须满足一致的 DB/WAL/对象条件，不能只复制活跃的 .sqlite 文件。[发布版持久状态](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/sdk/js/README.md#L374)、[持久性 qualification](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/core/crates/layerfs-persistence/README.md)

节点/VM 重启后先对账实际 workspace incarnation、操作意图和外部结果，不假设上游已替产品提供未知操作的自动重放。此集成保留 LayerFS 的原有分支与内容能力；WASM 状态、durable task journal、Git 和 VM 磁盘各自的恢复保证继续独立表达。

<!-- /migrated-contract: SPEC:10.2.1 -->

<a id="spec-10-4"></a>
<!-- migrated-contract: SPEC:10.4 -->
## 10.4 恢复流程

健康检查区分核心失活、单 worker 崩溃、坏包、只读文件系统和外部服务不可达。单个 SSH target 失败不触发全环境重置。故障 generation 停止新委派，已发副作用保留 ledger；尝试重启，超出已配置失败阈值退到 last-known-good，再失败退 factory，保留诊断材料和原始持久事实。

恢复后的主体从最后提交轮次与未完成委派恢复；worker 根据 receipt/当前外部状态继续或报告不确定，不盲目重放。被撤销密钥和被删除记忆不能因为旧 snapshot 回来。磁盘满时阻断新增非必要业务/媒体写入；在预留控制空间内继续记录迁移、删除、回收及恢复所需状态，并按 [§12.4](memory.md#spec-12-4) 执行主体的压力处置策略。连控制空间也不可用时明确标记无法提交与实际缺口，不伪造持久成功，也不不断回滚加剧占用。

持久性门槛：对象落盘后才能发布引用它的 manifest；SQLite 事件与需要确认的事务使用明确同步策略；恢复测试区分进程强杀、OS 崩溃与真实断电。没有电源故障试验前，不宣称已证实断电零丢失。

<!-- /migrated-contract: SPEC:10.4 -->

<a id="spec-10-6"></a>
<!-- migrated-contract: SPEC:10.6 -->
## 10.6 参考 Pi Durable 的持久执行协议

正式吸收 Pi `packages/durable` 的架构：保存工作本身、任务阶段、父子所有权、checkpoint、取消意图、调用日志和结果，重启后由可恢复的状态机继续。它补足进程内生命周期清理，不能只靠重新打开 transcript 猜测进度。本轮固定 `c60de42e98428dc1ef7440d78c23f5915966cdb3`，包版本 1.4.0，README 仍标 Experimental；参考合同不意味着将 TypeScript 及其全部依赖塞进 Rust 核心。[README](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/README.md)、[规范](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md)

DurableExecutionProvider 与 StorageProvider 保持可替换，主体只提出决定/委派。关键合同如下：

| 机制 | 本产品要求 |
| --- | --- |
| 原子提交后发布 | 主体决定、入站游标、任务/checkpoint、委派 outbox 与必要文档/对象引用以一致批次提交；成功落入声明的持久等级后才确认或发布 |
| 持久任务树 | 记录 owner、task kind/version、code generation、checkpoint、child handles、scope、runtime/seat、epoch；重启对账并恢复，父任务不丢已创建孩子 |
| 副作用 intent/effect/receipt | 先保存稳定 operation/idempotency key 与意图，再执行，最后保存真实结果；恢复到 intent 表示效果可能已发生，需查询、幂等重试或保留 unknown |
| 持久取消和补偿 | 先提交取消意图，再通知执行；按所有权处理孩子和可用补偿。补偿本身也是持久任务，可在再次崩溃后继续；服务真实支持何种撤销就记录何种结果 |
| 工具与嵌套调用重放 | 已完成调用按稳定 key 读取结果；只有持久记录和当前实现均满足 replay 合同才自动重跑未完成调用；不以“重启”删除原有费用或副作用事实 |
| 代码/插件演进 | 当前执行绑定实现版本和环境 fingerprint；缺定义、迁移不兼容或路径分叉进入可诊断状态，由主体委派修复；不让旧 journal 静默套用到不匹配的新代码 |
| 唤醒与单主体 | supervisor/boot recovery/Android 生命周期和多节点 fencing 分别负责恢复；持久 DB 不等于活进程，正常可运行时继续永续主体 |

Pi Durable 的规范明确使用 intent → effect → outcome，并要求 intent 阶段恢复时采用安全重试、查询外部 handle 或记录中断。它的 abort handler 可承载真实业务补偿；能补偿的工作应尽量设计为可恢复补偿，不再仅留下“无法撤销”的空泛说明。对于没有真实逆操作的效果，保留已发生事实，不能把 task 取消当成外部效果已消失。[effect 协议](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2045)、[任务与 abort](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2137)、[工具重放实现](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/src/harness/tool.ts)

断电承诺由实际 storage profile 决定。Pi 的 Node SQLite 默认 WAL + `synchronous=NORMAL`，README 明示最新 commit 可能因断电/宿主故障丢失；JSONL 的 `fsync:true` 是显式选项且完整历史会占用内存。本产品需选择满足 [§10.4](durable-state.md#spec-10-4) ACK 语义的 SQLite/对象持久策略，并在目标设备验证；不能仅借用 durable 名称就宣称断电通过。Pi 的单进程 storage ownership 也不替代本产品跨节点 epoch/fencing。[storage 说明](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/README.md#L752)、[Node SQLite adapter](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/chord/src/sqlite/node.ts)

原始 session、语义记忆、WASM 文件状态、LayerFS 分支、BoxLite 磁盘和 durable task journal 是可关联而不同的状态。Checkpoint manifest 保存它们的稳定引用和保证等级；恢复按有效任务与外部事实对账，不能用一次 VM/FS 回滚覆盖全部状态。所有需要模型的恢复、诊断和协作照常经过 [§13](inference.md#spec-13) broker；等待与持久休眠的子任务不占推理许可，主体持续思考语义保持不变。

插件逐操作的静态声明、动态资源绑定、checkpoint manifest、外部效果回执和跨版本恢复合同详见 [§36](durable-state.md#spec-36)；插件/hook 的生命周期清理不能替代这份持久合同。

<!-- /migrated-contract: SPEC:10.6 -->

<a id="spec-36"></a>
<!-- migrated-contract: SPEC:36 -->
# 36 插件的副作用、重置与恢复边界

本章定义插件效果范围；[§41](architecture.md#spec-41) 将它落实为具体 Operation/Attempt/Evidence、版本绑定、幂等、取消和补偿合同。边界声明须在实际调用时解析，不能仅存在于一段说明文字。

<!-- /migrated-contract: SPEC:36 -->

<a id="spec-36-1"></a>
<!-- migrated-contract: SPEC:36.1 -->
## 36.1 正式合同与适用范围

插件应以机器可读、模型可理解的合同说明：操作会接触什么状态，哪些能由 checkpoint 恢复，哪些是外部效果，如何取消、核验、重试或补偿。该合同适用于 WASM 插件、宿主 provider、SSH/Seat、渠道、REPL、包管理、模型服务、更新与存储插件，以及 [§37](context-policies.md#spec-37) 中可能产生效果的 hook。

采用“版本化插件声明 + 调用时实际绑定 + 结果回执”。声明帮助主体自主规划与恢复，不自动形成逐项人类审批；unknown 也不是自动禁用全部能力的理由。主体可以调查、增加观察、换执行环境或在既有委托范围内继续行动，但不能把 unknown 当成可无损回退的证明。核心只保留必要生命周期、能力和代际约束，具体解释器、日志实现、协调与补偿均可替换。

<!-- /migrated-contract: SPEC:36.1 -->

<a id="spec-36-2"></a>
<!-- migrated-contract: SPEC:36.2 -->
## 36.2 五种动作不能共用一个 rollback 标签

| 动作 | 本产品语义 |
| --- | --- |
| reset | 清空或重建声明范围内的临时计算/工作状态；范围由具体 profile 决定 |
| restore / rollback | 恢复已被 checkpoint 捕获的指定状态或软件 generation；不修改边界外世界的历史 |
| resume / replay | 从 checkpoint 接续工作；已完成外部调用可读取原结果，重新执行必须满足该调用的重试合同 |
| cancel | 撤销尚未开始的工作意图，或请求停止正在运行的工作；必须区分已确认停止、部分完成和结果未知 |
| compensate | 发起一项新操作，尽量修复既有效果；补偿可失败或仅部分有效，不能将其记为原效果从未发生 |

一个“读”操作也可能消耗 API 额度、更新读取标记、触发远端审计或改变会话；一个“写”操作可能完全限制在临时分支中。因此读写性质、外部可见性、幂等性、可取消性和可补偿性应分别表达，不能用单一风险等级代替。

<!-- /migrated-contract: SPEC:36.2 -->

<a id="spec-36-3"></a>
<!-- migrated-contract: SPEC:36.3 -->
## 36.3 静态声明按 operation 细分

| 合同字段组 | 必须表达的内容 |
| --- | --- |
| 身份与版本 | plugin ID、实现 digest、manifest schema、operation 与其输入/输出版本、receipt reader/reconciler 的实现身份 |
| 资源选择 | 可能读写的类型、路径/资源选择器及实际绑定规则；跨 provider 操作的各个参与者 |
| checkpoint 能力 | 可捕获的文件、对象、进程或 VM 状态；捕获/恢复方法、写入协调和一致性要求，明确排除项 |
| effects | 文件修改/删除、宿主与远端进程、网络请求、消息发布、凭据或策略更改等可能效果；可同时多种，不只选一个标签 |
| retry | 是否幂等、保证来自何处；provider key 的作用域、有效期、参数约束和重复返回语义；超时后的查证路径 |
| cancel | 取消前后可能已经完成的阶段，如何发出停止请求、何种证据确认停止，以及不能取消的部分 |
| compensation | 支持的补偿方法、前置条件、有效期、可能保留的痕迹、可否恢复完成，以及对应的新 operation 合同 |
| lifecycle | 升级、卸载、崩溃、恢复时如何处理在途调用、连接、spool、游标、租约、凭据 handle 和未知结果 |
| 保证依据 | 仅声明、运行时受约束、测试覆盖或仍未知；测试设备/版本/故障范围与证据引用，不能伪称静态证明任意代码无副作用 |

同一 NapCat 插件的 inspect、send、upload、recall 分别声明；同一执行插件的受管文件编辑与任意 shell 命令分别声明。任意宿主 shell／SSH 执行不能只凭命令文本精确推断全部写入，应保守描述实际可达作用域，并把已观测结果逐步补入。原生插件能绕过受管接口时标明其可信执行边界，不能把 manifest 自述当成已强制隔离。

<!-- /migrated-contract: SPEC:36.3 -->

<a id="spec-36-4"></a>
<!-- migrated-contract: SPEC:36.4 -->
## 36.4 调用时解析实际恢复边界

| 资源及实际绑定 | 沙箱恢复的默认含义 | 需要额外提供的证据 |
| --- | --- | --- |
| 受管 WASM 工作区／COW 分支 | 可恢复本次 checkpoint 明确覆盖的文件/对象 | root/对象闭包、版本、并发写入规则与持久等级 |
| 挂到 /work 的宿主 direct RW 目录 | 默认不随 WASM reset 回退 | 真实挂载来源和资源身份；如要恢复，由 HostWorkspaceProvider 显式加入本次捕获 |
| 默认宿主挂载 Git 记忆 | 恢复工具环境不清空个人记忆；工作分支和已发布记忆分别处理 | 工作树/commit、原始事件引用、未发布变更与外部 writer |
| SQLite／CAS／多文件应用状态 | 只在一致捕获与对象闭包完整时恢复相应域 | 一致性水位、WAL/事务条件、对象可取性、reader/migrator |
| VM/容器磁盘与外部共享目录 | 只恢复 provider 声明的磁盘或状态部分 | VM 内存、进程、挂载目录、网络连接是否分别包含；磁盘 snapshot 不自动等于进程恢复 |
| 本机/远端 tmux、SSH 命令、coding agent | 默认仍按外部实际状态存在或退出 | seat/process/任务身份、存活与进度查询、终止回执；本地控制 child 停止不证明它们停止 |
| QQ 消息、Git push、文件上传、API 变更 | 不随本地快照撤销 | 远端 receipt、可查询身份、补偿能力及限制 |
| 推理请求、KV slot／StateImage | 本地 checkpoint 不创造有效远端句柄 | 后端生命周期、实际驻留/暂停/重放能力、当前占用与 unknown 状态 |
| 原始事实、权限撤销、身份绑定、operation 回执 | 不随工作区回退；按专用迁移/备份/归档协议演进 | 恢复时可读取的事实水位、版本解码器、引用闭包与明确删除记录 |

不能仅凭逻辑路径判定边界；同一 /work 可以在两次调用中指向不同存储。绑定应基于实际 resource handle、挂载 generation、目标身份与 provider 证据，处理重绑、别名、符号链接和网络文件系统等情况。若无法缩小任意 native 命令的影响范围，就声明较宽的宿主/远端域及未知部分。

checkpoint manifest 保存参与者、明确排除项、各状态 revision、捕获时间/水位、应用一致或崩溃一致等级、断电持久等级、所需恢复闭包与跨域未决 operation。不把分开拍下的多个目录自动称为同一原子时刻；没有协调时如实报告不同切面并执行对账。恢复共享目录前核对 checkpoint 之后人类或其他进程的新修改，不能把其他人的工作默默覆盖。

<!-- /migrated-contract: SPEC:36.4 -->

<a id="spec-36-5"></a>
<!-- migrated-contract: SPEC:36.5 -->
## 36.5 一次操作的持久记录

操作开始前持久记录稳定 operation_id 和意图，每次执行使用单独 attempt_id；再执行，最后保存结果。该顺序参考 Pi Durable 的 intent → effect → outcome；其意义是使崩溃后的不确定窗口可识别、可协调，并非自动撤销已发生的外部操作。[Pi Durable effect 协议](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2045)

每次调用绑定任务 lineage、worker、generation/epoch、plugin digest、合同版本、现行委派依据、实际资源、输入摘要、调用前观察 revision、checkpoint 覆盖与预期外部效果。凭据只记录 handle；秘密不复制进日志。

记录两个不同维度：执行状态（prepared/submitted/running/ended/unknown）和效果状态（未发生/已发生/部分发生/未知）。命令退出码、API 接受、远端任务完成和用户已读也分别表达。事实依据包括 provider receipt、对象/文件 revision、外部任务 handle、观察时间及相关原始输出。新的核验追加证据，不能擦掉先前的 unknown 窗口。

如果外部动作完成后、结果落盘前崩溃，恢复只能得知效果可能发生。具有仍有效幂等保证时可按原键重试；有查询能力则核对；否则保留 unknown，由协调插件和主体决定下一步。主体可以自主选择再次操作，但应关联原调用、说明预期重复效果，不能把它伪装成没有代价的自动重放。一次跨多个边界的调用记录可区分的子效果，不虚构跨 QQ、文件和远端进程的原子事务。

补偿、撤回、删除、重启与“改回去”都是新 operation，拥有各自的失败/未知窗口。撤回消息不能证明别人没读过；删除远端文件不自动撤销由它触发的后续行为。可实现的补偿要真正实现并可恢复，不能只写一句“外部效果不可撤销”就省掉协调。

<!-- /migrated-contract: SPEC:36.5 -->

<a id="spec-36-6"></a>
<!-- migrated-contract: SPEC:36.6 -->
## 36.6 回退计划与跨版本恢复

恢复服务在执行前形成可读计划：恢复哪些 root/版本，保留哪些新事实，哪些外部任务继续运行，哪些句柄/租约需要重新绑定，哪些操作待查证，哪些需要补偿。这是给主体和执行器使用的计划，不是新增强制人工批准窗口。

调用旧代码恢复前，应取得截至当前的 operation 事实和有效授权状态。工作区 snapshot 不携带可重新激活的旧 writer lease、撤销前权限或旧控制器 epoch。旧 epoch 的命令失效，不代表其已经发出的请求不可能完成；迟到回执仍应收录到原 operation 并触发协调。

B 升级产生的新 receipt 格式，由版本化 reader、转换器或保留的 B 恢复闭包解释。切换前验证“B 新事实如何交给 A 或恢复器”的路径，不要求 A 永久读懂未来所有 schema。无法解释的记录保留原始内容并使相关动作待核验；其他已确定的工作可以继续，不能清空账本后重发。

内核、ledger 实现、receipt envelope、恢复计划器、验证器及规则均可升级。当前恢复仍依赖的代码、对象、密钥引用与读取器保留为 GC root；通过有证据的交接后再自主回收。恢复能力的依据是这一代的具体闭包和测试，不是某个永远不许改的 Ring 0。

<!-- /migrated-contract: SPEC:36.6 -->

<a id="spec-36-7"></a>
<!-- migrated-contract: SPEC:36.7 -->
## 36.7 模型、工具与界面如何看到边界

能力发现默认提供短摘要，例如“写入受管分支；可还原文件；不会撤回已经发出的 QQ 消息”。准备实际操作时再提供确切绑定、unknown 和补偿选择；完整 schema 与证据按需展开，避免把全部恢复说明塞进每次 system prompt。

工具结果同时返回实际效果和恢复提示。主体可以据此选择先建分支、补一次备份、改用远端受管 workspace、先查询结果或继续自主操作；有新 hook/adapter 后更新这些说明。UI 在任务/版本详情中显示此次恢复范围、仍存在的外部效果与待核验项，首页不变成风险监控大屏，也不以“绿色恢复成功”掩盖混合状态。

<!-- /migrated-contract: SPEC:36.7 -->

<a id="spec-36-8"></a>
<!-- migrated-contract: SPEC:36.8 -->
## 36.8 首版恢复合同验收

| 编号 | 故障/场景 | 通过条件 |
| --- | --- | --- |
| E01 | 同一插件在受管分支和 direct RW 宿主挂载调用 | 实际边界随绑定改变；重置只恢复被捕获状态，宿主变更不会被虚报已撤销 |
| E02 | QQ 已发出但响应丢失，再恢复 snapshot | 原调用保留 unknown，先核对；无幂等依据时不盲重放 |
| E03 | 已确认 QQ 消息后回退软件并选择撤回 | 发送事实仍存在；撤回是独立补偿，结果和限制明确 |
| E04 | 向 tmux 发出任务后控制器崩溃 | 恢复先检查原 seat/进度，不重复派发；人类已接管时旧写入权无效 |
| E05 | 多资源 checkpoint 与其他 writer | 显示真实一致性范围；恢复不能静默覆盖其他 writer 新增工作 |
| E06 | B 写新格式 receipt 后切回 A | 保留的转换/恢复闭包实际解释新事实；未解释部分保留且不自动重做 |
| E07 | cancel/compensate 再次崩溃 | 原效果与补偿进度均可恢复，取消不被当成全部撤销 |
| E08 | 原生任意 shell 或第三方插件缺乏精确信息 | 明示较宽作用域或 unknown；不伪造安全回滚保证；主体仍可自主补能力和选择策略 |
| E09 | checkpoint 后后端请求继续执行或迟到回执 | 按真实后端状态计占用/归属；本地恢复不重复发放许可，也不丢弃迟到事实 |
| E10 | 插件 manifest、mount 或恢复器版本变化 | 调用绑定正确版本；在途调用可按旧合同解释，新调用使用新合同；不以新说明重写旧事实 |

<!-- /migrated-contract: SPEC:36.8 -->

<a id="spec-41-2"></a>
<!-- migrated-contract: SPEC:41.2 -->
## 41.2 默认持久存储与观察边界

存储、任务调度、broker、生命周期和恢复器都由可替换服务实现；本节没有要求将 SQLite、全部业务表或升级策略永久编入 Rust 内核。默认发行装配一组实现相同合同的插件，小核心负责启动和连接当前有效服务。

<!-- /migrated-contract: SPEC:41.2 -->

<a id="spec-41-2-1"></a>
<!-- migrated-contract: SPEC:41.2.1 -->
### 41.2.1 本地写入域与 SQLite profile

首版每个本地 ledger 只有一条 mutation line、一个当前 writer incarnation。数据库写锁只覆盖本地校验与提交，不覆盖模型推理、SSH、网络、构建、插件等待或用户等待。只读快照不持有此写入线。独立节点拥有独立 ledger；通过事实导入、选举和 fencing 协议汇合，不通过同时挂载同一 SQLite 文件实现跨机协作。

默认 StoreProvider 使用本地 SQLite，配置并读回确认 `journal_mode=WAL`、`synchronous=FULL`、`foreign_keys=ON`；一笔逻辑 commit 对应同一个数据库中的一笔 SQL 事务。不使用跨 ATTACH 数据库写入来冒充一个全有或全无提交。FTS、向量、缩略图、检索排序和 UI 视图是派生物；原始 entries/receipts 与对应索引工作通知同笔登记，索引可稍后重建，不能因搜索暂缺就宣称原文不存在。索引返回 `indexed_through` 与 `source_through`；追求最新事实的读取直接查原记录或等待明确水位。

打包的 SQLite 版本必须包含官方 WAL-reset 修复（官方列出 3.51.3 及以后，或相应已修复的回移版本）；build manifest 记录实际版本与 compile options。是否支持 FTS5 在启动能力探测中明确记录。WAL 不用于多个主机共享的网络文件系统。以上 SQLite 特性与限制依据 [synchronous 文档](https://www.sqlite.org/pragma.html#pragma_synchronous)、[WAL 文档](https://www.sqlite.org/wal.html)；这是 Shenko 的首版配置选择，不等于 Pi 默认配置。

每个 `CommitReceipt` 返回 `commit_ref`、`durability_profile`、`store_incarnation` 和本次写入的实体 revision。默认 profile 为 `power_loss_local`，含义是在目标 OS、VFS、设备正确兑现同步语义的条件下，确认后应经受本机进程崩溃和掉电；它不包括磁盘物理损坏、失去整机、未同步远端副本或外部系统操作。某个平台只能证实 `process_crash` 时必须返回该等级，不能沿用名称继续宣称掉电持久。Agent 可自主选择调整 profile，但调整形成版本化配置事实，不回写已发出的 receipt。

<!-- /migrated-contract: SPEC:41.2.1 -->

<a id="spec-41-2-2"></a>
<!-- migrated-contract: SPEC:41.2.2 -->
### 41.2.2 对象与元数据的提交顺序

大文件、音频和截图放 CAS，ledger 只引用已封存对象。先完成对象写入、长度和 digest 校验，再按当前 durability profile 同步文件与必要目录、获得 `sealed_object_ref`，最后允许元数据事务引用它。事务回滚可留下无引用的已封存对象，由后续 GC 回收；不得先提交一个“对象存在”的引用再异步创建内容。

一个 checkpoint 的树根、文件 manifest 和所有新对象必须满足闭包验证；未知或缺少的对象导致 `ObjectClosureIncomplete`，不得将其提升为可恢复快照。流媒体尚未完成时使用显式 open-stream 记录，已封存分块与最终对象分开；下载 URL 不是已归档附件。宿主 RW 挂载和远端机器不在 CAS 快照的隐含范围内，仍按 operation 的实际 effect boundary 处理。

GC 读取一个已提交 root snapshot，pin 活跃/候选/恢复 generation、在途 attempt、未消费回执、迁移水位和用户/主体保留的档案；删除前重新校验 root revision 或使用等价 epoch 协议。不能因“旧软件已停”就删除其 receipt decoder、未完成恢复所需闭包或新版本运行后的事实。GC 自身有任务和删除回执，空间不足不会把普通写入悄悄降成不持久。

<!-- /migrated-contract: SPEC:41.2.2 -->

<a id="spec-41-2-3"></a>
<!-- migrated-contract: SPEC:41.2.3 -->
### 41.2.3 提交失败、发布与重开

调用通过 schema、幂等和 revision 检查之前的拒绝必须证明没有持久 effect，返回 typed rejection。进入 StoreProvider.commit 后，调用者取消只停止其等待，不撤销已接纳的存储结算。已开始的 commit 必须结算；只有耐久结果明确后，才采用内存状态并发布与该 commit 对应的事件。

StoreProvider.commit 抛错、返回不确定、存储已提交但内存 adopt 失败时，当前 ledger handle 进入 `failed`，拒绝后续 mutation，终止相应 watch，通知在途 invocation 停止新决定。不得在旧内存对象上重试“同一段代码”。恢复器重开 store、读取最后真实提交，再使用原 request/operation id 查询结果；确认没有该命令的持久 admission 时可按现行条件重新准备；存在 armed attempt 时，按其外部幂等/查询合同处理，不能重新装成一条未发送命令。外部工作可能仍在运行，重开不能据此宣告它失败或释放其 Engine 占用。

状态观察遵循 `committed-only`：watch 返回不可变对象和 `commit_ref`；用户回调在写入线之外运行。首个快照含水位，后续事件按同一 ledger commit 顺序；断线重连使用该水位补读，超出保留范围返回 `ResnapshotRequired`。观察者报错或缓慢不回滚已提交事实，也不阻塞模型和任务写入。派生 UI 可以显示“生成中”，但不能把未提交 token/tool-call 当完成决定。

流式文本按有限 chunk 批量持久化后再作为可恢复内容发布，默认按 100 ms 或 4 KiB 中先满足者提交，终态强制 flush；该批量阈值是可调工程默认。未完成或被取消的 chunk 仍可作为带状态的原始轨迹保存，不成为自动重放的工具调用。只有通过完整 schema 校验并在相应事务中接纳的 action 才能执行。UI 的临时光标、传输速率等瞬时呈现不用写入每笔 ledger，也不得影响恢复。

本节对提交未知的处理参考 REF-DURABLE [§10.1](durable-state.md#spec-10-1)；持久等级参考 REF-STORAGE。特别不能把 Pi JSONL 的 `fsync:true` 误写成“每次已 ACK 尾部均保证掉电后存在”：其固定规范明确普通提交没有显式同步 main marker。Shenko 的不同 StoreProvider 必须各自通过其声称的恢复等级，不能只通过相同接口名称继承保证。

<!-- /migrated-contract: SPEC:41.2.3 -->

<a id="spec-41-3"></a>
<!-- migrated-contract: SPEC:41.3 -->
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

`request_id` 唯一标识一份不可变的初始 RequestSeal。语义 reseal 必须分配新 request_id；request_lineage_id 表示同一生成工作的演变链，request_revision 只在该 lineage 内递增，并保存 supersedes_request_id。一个 request_id 永远不能指向两份初始 seal；队列、attempt 和结果按 request_id 关联。唯一的增量输入路径是 [§41.18](web-interface.md#spec-41-18) 经实际能力核验的 live steering：独立应用链记录精确额外输入与生效边界，结果绑定该链，而非改写初始 seal。工具、模型及语义参数仍按原 seal；它们的变化需要新 request。网络重试或正确的无损恢复可以沿用该 request_id 并增加 attempt，而非篡改 seal。

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

<!-- /migrated-contract: SPEC:41.3 -->

<a id="spec-41-4"></a>
<!-- migrated-contract: SPEC:41.4 -->
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

<!-- /migrated-contract: SPEC:41.4 -->

<a id="spec-41-14"></a>
<!-- migrated-contract: SPEC:41.14 -->
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

<!-- /migrated-contract: SPEC:41.14 -->

<a id="spec-50-4"></a>
<!-- migrated-contract: SPEC:50.4 -->
## 50.4 未竟义务的显式结清与复制闭包

本节补充 [§41](architecture.md#spec-41) 的 S0/S1 与 [§45](cognition-units.md#spec-45) 的 unfinished-obligations 合同，不修改此前原文。主体在后续 S0 中取得本分支尚未结清的义务完整记录；请求所见与封口所用是同一份向量，描述、来源事件、依赖任务、恢复条件和复查时间均保留。历史窗口压缩不删除这些持久事实；正文仍计入正常上下文预算，不能以静默截断或省略记录假装可容纳。

主体以 `[obligation:terminal]` 后的一份 JSON 提出显式终态变更，字段为 `obligation_id`（原义务 UUID）、`state` 和 `reason`（本次判断依据）。`state` 只能为 `completed`、`cancelled` 之一，目标不得为空 UUID，理由不得为空白，未知字段拒绝。该控制解析为 `ObligationTerminalChange`，经 `PolicyKind::ObligationTerminal` 进入同一步 S1；普通正文、child 完成、时间到期和新建义务均不自行结清旧义务。

接纳时必须同时匹配本步解析控制、policy intent、原始创建决定、本分支持久义务和本步 S0 中的完整目标记录。其他分支、未见于 S0、同一步才创建、重复目标或被替换的正文不能成为有效结清。成功结清与决定、policy 结算及 head 推进在一个事务中提交；失败提交不留下局部终态或成功回执。

`ObligationTerminalReceipt` 保存原始 S0 引用、目标完整记录、请求变更、step/intent 身份、结算结果和提交位置。已完成或取消的状态不被迟到请求、重放原始创建决定或另一终态覆盖；仍有合法旧 S0 的迟到请求保留明确的拒绝结算，其余有效决定可以提交。请求、拒绝回执和独立的 policy 执行标记均不单独证明义务已结清。现代恢复缺少原始结算绑定时拒绝推导成功；既有 schema-3 历史创建投影按原合同恢复，不能借此修补现代终态证据缺口。

认知归档 schema 6 将上述原始创建、控制、policy、S0、投影与终态结算列为必须完整理解的事实。schema 5 及更早 reader 必须拒绝 schema 6；schema 6 reader 继续接纳其能完整解释的旧归档。缺失、矛盾或接收方已有的必需事实冲突应在发布前拒绝，不能发布部分数据库事实、文件、认知基线或复制水位后再报告失败。成功导入、关闭重开、重放和再次导出必须保留相同原始结算身份与正文。

受控验证记录位于共享工作区 `build/obligation-lifecycle-20261105/actual-readers-20261105-112512/receipt.json`：分别编译的真实 schema-5/schema-6 Store reader 验证旧 reader 拒绝实际 schema-6 归档，新 reader 导入、重开、relay 后第二接收方保留同一已完成义务结算。旧 schema-5 导出再由新 reader 导入的来源为空 Store，该项只证明版本接纳，不证明有内容的历史归档恢复。原生回归与该合成 Store/CAS 场景不替代真实模型效能、断电、分区或长期运行验收。
<!-- /migrated-contract: SPEC:50.4 -->
