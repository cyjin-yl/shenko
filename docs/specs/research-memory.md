# Research memory — functional contract

**内核定位。** 参考插件，建立在[内核合同](kernel.md)之上。研究谱系与回到最佳点都依靠只增的原始经历（[内核 3](kernel.md#kernel-3)），经[内核 7](kernel.md#kernel-7)登记。

[§14.2](research-memory.md#spec-14-2)、[§14.5](model-children.md#spec-14-5) (research epochs / reset-to-best), [§12.4](memory.md#spec-12-4) (PMA intervention),
[§10.5](memory.md#spec-10-5) (context contracts). This module is the research slice: a subject's
research lineages, their reset-to-best epochs, the durable bank that crosses
epochs, the PMA memory-intervention worker, and closure-complete retention
mechanisms.

## Architecture

```
shenko-memory         MemoryInterventionPolicy (trait) + Model/Silent impls
                      + MemoryInterventionRegistry (Cordis service)
                      + MemoryInterventionPlugin / PmaContributionRuntime
shenko-store          research.rs: epochs/candidates/bank/reminders/tombstones
                      + archive_moves; every write idempotent + ledger commit
shenko-subject        PmaEvaluatePlugin / PmaEvaluateHandler ("pma.evaluate")
shenko-exec-host      research.reset / research.candidate / research.bank /
                      retention.apply handlers + builtin.rs assembly
```

The policy decides; the store remembers; the native children execute inside
the T1-armed attempt boundary and record sealed E1 evidence. Nothing here is a
model tool schema: the subject reaches these through the standard
`[delegate:<kind>:1]` native-delegation path.

## Epochs and reset-to-best (research.reset)

`POST [delegate:research.reset:1]` with `{"op":"open"|"reset", "request_id",
"lineage_id", "trigger", "reason", "constraints", "budget", "effects",
"environment", "best_candidate_id"}`.

The closure uses the Store's structured facts without an alternate handler
schema: constraints carry `id`, `statement`, and optional `source`; effects
carry `effect_id`, `description`, `external`, and `recorded_at_ms`. Budget
fields are `max_tokens`, `max_duration_secs`, `max_cost_units`, and `notes`
(each optional). The environment records `environment_id`, `facts`,
`toolchain`, `dataset_versions`, and `reproducible`; a software-generation
label alone is not a reproducible environment.

- **open** creates epoch 1 of a lineage with a sealed worker brief and the
  authoritative native invocation's task identity. Recovery uses the original
  operation's task, not a caller-supplied ownership field.
- **reset** freezes the active epoch as an archive and opens the next one:
  candidates are frozen; the best candidate is **validated** (artifact present
  in CAS, digest matches, score recorded with its source, environment
  recorded); surviving bank entries are carried; the new brief carries
  constraints/budget/effects forward **without the old failure narrative**
  (failed entries stay in the frozen epoch, excluded by count).
- The candidate pool includes both candidates created in the current epoch
  and candidates retained in its sealed brief and reset ancestry. This also
  recovers candidates omitted by older reset writers, without rewriting their
  historical briefs or reading mutable historical candidate rows. Only
  candidate evidence is recovered from ancestors: old bank entries and failure
  narratives are not restored. Each snapshot's digest, identity and parent
  binding must verify; the ancestry must remain in the same lineage and reach
  epoch 1 through consecutive epoch numbers. Missing ancestors, cycles and
  conflicting evidence refuse the reset rather than publish a partial pool.
  This verification reads the sealed ancestor snapshots on each new reset.
  Consecutive resets preserve candidate identities, artifact references, scores
  and their sources, and environment references; they do not create duplicate
  candidate rows. The inherited best is revalidated on every reset. An invalid implicit best is
  reported in the validation receipt, not presented as the new starting point;
  an explicitly requested invalid best still refuses the reset. A missing or
  inconsistent required brief is an error, not an empty inheritance set.
- Triggers: `stagnation | isomorphic_proposal | evidence_direction_changed |
  planned_round | subject_request`.
- Idempotent on `request_id` (epochs.reset_request_id UNIQUE). A replay
  returns the same epoch identity and the sealed candidate set, carried bank
  IDs and excluded-failure count. Later bank mutations do not rewrite that
  result. Old epochs are never deleted; the reset changes what a new worker
  sees, not what the ledger remembers.

## Candidates (research.candidate)

`{"candidate_id", "epoch_id", "artifact_ref", "score", "score_source",
"environment"}`. `artifact_ref` is content-addressed; `score_source` names
where the score came from (evaluation / baseline / human). Candidates are the
raw material of reset-to-best. Idempotent on `candidate_id`.

The handler seals the complete environment body as a CAS object and stores
its `environment_ref` on the candidate. Its creation timestamp belongs to
the frozen input, so reconciliation does not invent a later creation time.

Candidate replay requires the same epoch, artifact, score, verifier source,
environment reference and Store notes. Reusing the ID with different evidence
is a conflict, not a successful acknowledgement of the new values. An identical
write retains the first stored creation timestamp and does not advance the
ledger. It remains replayable after a reset freezes the candidate, without
thawing it or replacing the evidence.

## Durable bank (research.bank)

Ops: `put | list | supersede | tombstone`. Entries have `kind`
(`requirement|environment_fact|attempt|diagnosis|open_goal`),
`scope` (an exact free-form string), `confidence`, `source_refs`,
`supersedes/superseded_by`, `carried_from`, `tombstoned_at_ms`. A tombstone
keeps the content readable — it only stops injection. The stored label
`environment_fact` remains the field name. The current reading
([51.9 助手代定（用户授权）](cognition-units.md#spec-51-9)) is an attitude on research
content, not a separate kind of fact. The name stays until the
implementation changes. On reset, entries whose
kind does not survive reset and tombstoned entries are not carried.

`list` accepts `selection=all|carried|failures` and an optional exact scope
filter. `all` returns non-tombstoned entries; `carried` selects requirements,
environment facts and open goals that are not tombstoned; `failures` is an
explicit audit view of attempts and diagnoses, including tombstoned entries.
The Store's `bank_entry(id)` API keeps every kind readable by identity,
including tombstoned rows, without restoring eligibility for injection.
The reset's excluded-failure count includes retired failure entries retained
in the archive. Default PMA bank observations use the active `all` view.
Bank-entry creation time is frozen once, independently of reconciliation.

Bank replay requires the same epoch, kind, content, provenance references,
scope, confidence, base revision and reset ancestry (`carried_from`). A supplied
`supersedes` target must match the retained relation; an original write without
a target may replay after an explicit correction link. Replay neither changes
those links nor removes a tombstone, and retains the first creation timestamp.
Conflicting evidence or a different explicit correction target refuses before
allocating a new ledger commit. Adding a correction link or retiring an
entry uses the explicit lifecycle operations, not a repeated `put`.

## PMA intervention worker (pma.evaluate)

`PmaEvaluateHandler` (shenko-subject) evaluates the requested research epoch;
it does not drain the reminder queue. The queue reader
`store.pending_intervention_reminders(agent_id, now_ms, limit)` is separate
from both this evaluation and the library memory intents
(`store/src/memory.rs::pending_library_memory_intents`). The handler builds a
bounded observation of the epoch's bank + attempts, asks the installed
`MemoryInterventionPolicy`, and executes the decision:

- **silent**: recorded as a decision (a missed reminder is a fact, not an
  absence); nothing is injected.
- **remind**: one transient, source-bound reminder with provenance and
  relevance, delivered through the context contract. Accepted S0 usage is the
  authoritative contribution consumption receipt; a decision row or provider
  answer alone does not consume it.

No personal-interest relevance gate and no approval step: the policy is the
only authority, the decision is bounded, durable and reproducible.

The model observation names its `allowed_reminder_scopes`: `subject` and
`epoch:<current epoch_id>`. A scope is a delivery address, not prose describing
the work, and cannot name another epoch or append a lineage description.
When a visible bank is present, provenance must identify a visible entry or
one of that entry's exact source references. Only the budget-visible bank
prefix can supply the persisted `bank_entry_ids`; hidden entries do not become
evidence merely because the unbounded observation contained them.

Address validation occurs in the model policy, again for an arbitrary installed
policy's reminder, and before Store accepts a decision transaction. An unusable
address therefore fails the bounded call while retaining the original provider
answer; it cannot first commit a successful operation and then fail during
publication. This checks routing, not whether the model's advice is correct.
Previously accepted malformed decisions are not rewritten or silently assigned
a destination: their original facts and explicit recovery conflicts remain.

Prepared PMA contributions use the common `ContributionPayload { v, text }`
reader contract. The sealed object also retains reminder identity, provenance,
relevance and bank references; these metadata fields are not injected as model
text. The producer revision is `pma.intervention/v2` and the reminder-body
codec is version 2; original sealed objects are never rewritten.

A transient reminder with a deadline uses `ThisRequestUntilMillis`: it expires
if still unused at that deadline and is consumed when an accepted S0 first
adopts it. Re-publication and replay of S0 usage do not revive it. Ordinary
`ExpiresAtMillis` remains reusable until its deadline. This contribution-level
contract does not by itself prove default-runtime producer installation or
provider delivery.

Reminder delivery is resolved from **committed epoch and task ownership**, not
from the address string alone. `subject` and `epoch:<id>` are addresses; the
`subject` address resolves through the epoch's owning task's agent to its
current subject branch. For `epoch:<id>`, the epoch's owning task is walked to
its nearest model child, or to the subject root. A model child owns the task scope
`{ scope_id: task_id, task_id: Some(task_id) }`; the subject root owns the
scope-wide branch scope `{ scope_id: branch_id, task_id: None }`.

An epoch id is **not** a collection scope. No request collects
`scope_wide(epoch_id)`, so a reminder published under the epoch id was
prepared for a scope nobody reads: the decision was durable and the reminder
was still unreachable. An epoch-directed reminder now reaches the worker that
owns the epoch, and a subject-directed reminder reaches the subject's own
branch scope. The scope string `subject` is not a global broadcast address,
and a child's collection does not also receive subject-directed reminders.
Preparation selects active epochs and retains the existing expiry
and consumption filters. Historical epochs without an owning task remain
readable, but cannot establish a delivery scope; the producer does not guess
an owner from whichever subject happens to run. A malformed ownership chain is
an explicit conflict, not an empty plan.

The immediate publisher refreshes the resolved delivery scope — the complete
committed batch for that destination, not a singleton that could overwrite
another pending reminder. The publication ticket is acquired before
preparation, and registration withdrawal invalidates all of its tickets. A
restart re-derives every pending scope from the durable decisions, so a
prepared batch lost with the process is rebuilt for the same owner.

The reminder body is **not** tool-result context. The normalized decision
result carries the decision identity and a sealed reference to the recorded
decision; the reminder text reaches a model only as a prepared contribution,
in the contribution scope its owner collects. A decision row, a provider
answer or a sealed decision object is evidence, not delivery.

### The decision and its settlement are one fact

The bounded call produces one decision, and that decision *is* the effect of
the operation:

- the provider's answer becomes durable E1 evidence on its own
  (`record_intervention_provider_evidence`). The operation stays unresolved
  while the policy still has to read the answer, so no path can settle the
  attempt as a success before a decision exists;
- the decision row and the settlement that accepts it commit in one ledger
  transaction (`record_intervention_settlement`): a successful intervention
  and its durable decision are one fact, and neither exists without the
  other. The settlement names the decision it carries, and the row keeps the
  first writer's clock when the same decision is replayed;
- an answer that carries no usable decision is a failed bounded call. It
  settles as a failure, keeps the original answer as evidence, and never
  becomes a fabricated silence;
- recovery re-derives the decision from the frozen observation and the sealed
  answer, so a call that stopped between the answer and its decision commits
  the same decision instead of being replayed. The observation is frozen with
  the request for exactly that reason; a sealed input of an older revision
  parks the attempt instead of inventing a decision.

## Retention (retention.apply)

`{"request_id", "mechanism", "target_refs", "reason", ...}`.

- `semantic_forget` / `erase`: a monotonic tombstone whose sealed closure
  lists every target digest and states that the content store has no object
  delete — bytes remain recoverable at their digest. Erase records erasure
  intent and its recovery path; it does not destroy data (R066 erasure stays a
  separate owner protocol).
- `archive_move`: records a complete object closure leaving for an
  authenticated target (`provider/namespace/locator`, `manifest_ref`), with
  `hash_verified` / `readback_verified` and an explicit local-release note.
- `status`: read-only report of the targets' tombstones and archive moves.

All mechanisms are idempotent on `request_id` (tombstone/archive identity).
No mechanism deletes user data in development.

## Runtime registration

The default CLI installs `exec_host::builtin::research_handlers(store)` on
its existing `NativeChildPlugin` through `NativeTaskHandlerPlugin`. Thus
`research.reset`, `research.candidate`, `research.bank` and `retention.apply`
are present both in live dispatch and in the `capabilities` command, including
model-free startup. There is no separate, unused built-in supervisor.

With a configured model, the CLI also installs `MemoryInterventionPlugin`
and `PmaEvaluatePlugin`. The latter uses the same broker as the subject and
model children, and declares dependencies on the native runtime, the policy
registry and the contribution runtime. Replacing those memory services
rebuilds the handler rather than retaining a withdrawn publisher. Policy
replacement remains `MemoryInterventionRegistry::install`; no keyword,
model-selection or human-approval gate is introduced.

The memory plugin restores the current subject branch's prepared batch before
startup completes. Afterward it reconciles ledger changes on a 250 ms background
tick; committed reminder decisions also request an immediate full-batch
refresh. Branch changes withdraw the previous branch's batch. Expiry is checked
at collection, and accepted S0 history reconstructs single-use consumption on
process/service replacement. Preparation is outside the request collector and
does not consume a reminder. The background projection only reads committed
decisions; it does not trigger an inference or choose what the subject does.

Cordis teardown closes publication, cancels and joins the projection worker,
then removes that exact registration. A retained service handle cannot publish
after closure; same-Fiber restart gets a fresh worker and registration. A
projection failure is journaled and stops its owning runtime rather than
silently serving a stale successful projection.

Initial native-plugin publication installs the catalog without recovering or
acquiring dispatcher authority. A live composition root explicitly calls
`native.recover()` after owning the dispatcher. Same-Fiber native restart still
recovers aborted claims after its close/join barrier. Capability discovery
therefore cannot recover or fence another live dispatcher's tasks.

## Failure and recovery semantics

Every handler: freeze input → seal → arm T1 (`arm_external_operation`) →
idempotent write → sealed E1 evidence (`record_external_evidence`) → resolved
operation. Crash before the write: reconcile re-runs the frozen write (safe
by idempotency). Unknown/conflict evidence is authoritative and is never
erased by a later success.

The memory-intervention worker settles in that order with one extra step:
answer → the policy reads the answer → decision row + settlement in one
transaction. An answer alone never resolves the operation, and a decision
without its settlement never becomes visible.

### Bounded native observations (2026-11-03)

The live PMA observer uses the real Store, native task dispatcher, broker,
policy/parser and contribution plugin. The served model was
`swift-qwen3.8-27b-nvfp4`, rooted at
`Merkyor-Coder390-NVFP4-W4A16-3e644f23`. Four synthetic cases each received three
fresh model calls and one SilentPolicy control, separately before and after
the address fix. The two consumers have identical source bytes
(`158dfc02b30b44b1c48a67871c6b1735b8782c1c36c3f12d7cc58c390f3142f7`).
Whole-worktree membership/content manifests, resolved Cargo package paths,
compiler-artifact messages and retained executable hashes bind each arm to
its own product source and target directory.

| Observed outcome | Before | Address fix |
| --- | ---: | ---: |
| Model tasks settled successfully | 10/12 | 12/12 |
| Malformed address committed; task without terminal result | 2/12 | 0/12 |
| SilentPolicy controls settled silently | 4/4 | 4/4 |

In the fixed arm all six reminder decisions produced a prepared contribution;
the other six model decisions were silent, with no projection or teardown
error. This is not proof of delivery to a later model request or autonomous
default-CLI initiation. It is also not a claim of perfect advice: the first
identifier-loss sample suggested BigInt without preserving the original
decimal spelling; numeric equivalence does not generally preserve lexical IDs.
No hard-coded semantic filter was added. The earlier diagnostic trial exposed
case labels in source references and is retained only as a defect diagnostic,
not an unbiased efficacy measurement. The matched trials use unrelated UUID
source references and do not replay the earlier requests.

Evidence roots under `build/spec-architecture-20261101/`:
`pma-live-neutral-{before,after}-r1/`, `pma-neutral-matched-assessment.json`,
and `pma-address-verification-r1/` (contribution 4, settlement 4, Store 21
regressions passed). These bounded synthetic observations do not close general
model efficacy, autonomous initiation, or long-duration acceptance.

A separate default-CLI mechanism observation binds both CLI and Store reader
to merge `c37307c` with a complete source manifest and an independent target.
The first run, `epoch-sourcebound-r1/`, failed before any provider request:
an inherited default ACL made the newly created data root group-writable, and
owner-control rejected it. `epoch-private-r1/` reuses the exact retained
executables without rebuilding and uses a fresh, precreated 0700 data root.
Its seven checks passed: five recorded child steps, committed reminder,
single target-child delivery, no reminder in rolling tool history or subject
requests, one PMA call, and no scripted-provider error. Native CLI and Store
readback exited zero, with source/executable hashes unchanged. This proves the
delivery mechanism with scripted responses, not live-model judgment; it does
not repair the first-run directory initialization defect or upgrade the older
source-limited receipts into complete proofs.

<a id="spec-14"></a>
<!-- migrated-contract: SPEC:14 -->
# 14 递归研究与选择性记忆

<!-- /migrated-contract: SPEC:14 -->

<a id="spec-14-1"></a>
<!-- migrated-contract: SPEC:14.1 -->
## 14.1 两篇新增研究的含义

**Scaling Discovery Requires Forgetting**，arXiv:2610.20563，2026-10-14，已核验对应用户贴出的讨论。其方法同时刷新研究 Agent 的 session 与可见 candidate/search state，只从最佳候选及评分继续；harness 仍可保存历次脚本。实验采用 pi 与一个模型、四项有评测器的任务、每组三次，15%/39% 是特定比较的最高改进，不能作为日常 Agent 的通用增益。[论文](https://arxiv.org/abs/2610.20563)、[全文](https://arxiv.org/pdf/2610.20563)

**Remember When It Matters**，arXiv:2607.08716v1，采用独立 memory Agent 维护 bank 并选择提醒或保持沉默。摘要的 +8.3/+6.8 个百分点来自特定 action/memory 模型组合，不代表低成本本地部署也有同等收益。正文表格中 selective 与 always-inject 在 macro/micro 指标上并非全面胜出；主表与消融部分数据也不一致，应保留该限定。[论文](https://arxiv.org/abs/2607.08716)、[全文](https://arxiv.org/html/2607.08716v1)

两者指导不同问题：探索 worker 可能被既有尝试束缚，需要重新看问题；执行 worker 或主体可能忘了仍有效的要求，需要在关键时刻提醒。以下组合是本产品的设计提议，并非论文已经证明的联合算法。

<!-- /migrated-contract: SPEC:14.1 -->

<a id="spec-14-2"></a>
<!-- migrated-contract: SPEC:14.2 -->
## 14.2 四种状态不同处理

| 状态 | 默认处理 |
| --- | --- |
| 原始事实与执行档案 | 默认永久、可寻址，不因 reset 消失；实际迁移/删除按 [§12.4](memory.md#spec-12-4) 主体保留策略处理 |
| 运行合同与在途任务 | 保留来源、版本、外部效果和未完成依赖，切窗/reset 后可恢复 |
| 私人承诺与关系判断 | 作为经历和可回忆资料保存，是否信守由主体人格/灵魂与当前判断决定，不作为自动永久 ACL |
| 探索叙事与旧假设 | 可从新 worker 的可见 context/workspace 中移除 |
| 当前最好产物 | 校验后作为新一轮起点，记录评分与可复现实验环境 |

研究 reset 作用于子 Agent，不创建第二个主体，也不抹掉人格、长期身份或 owner 的原始要求。主体可以正常切 context window 维持无限逻辑生命周期；那是连续性的管理，不是每轮重新发明自己。

<!-- /migrated-contract: SPEC:14.2 -->

<a id="spec-14-3"></a>
<!-- migrated-contract: SPEC:14.3 -->
## 14.3 Research reset 合同

每个研究任务有任务说明、约束、评分/验证器、证据 ledger、候选集、预算和停止条件。reset 时先冻结候选并确认外部副作用状态；保留可复现的最佳 artifact、必要依赖和真实成绩；新 worker 从新 session 与干净分支开始，不默认暴露旧失败叙事或全部 Git/FTS 候选历史。

所有旧记录仍在受控档案中，用于审计与指定回查。若新 worker 一启动就被 memory 插件塞回旧失败总结，或能直接扫描包含旧候选的工作树，reset 的目的可能被抵消。因此每个 worker 有 research epoch 和 memory policy；旧试验资料的自动注入默认关闭，仅保留仍有效约束、现实环境事实和避免重复外部副作用所需记录。

reset 可由停滞信号、同构提案、证据方向改变、计划轮次或主体提出；具体阈值由评测确定，不复制论文“固定三次”作为普适规则。它有真实冷启动/prefill 成本，必须算进原任务总预算。若当前最佳只是在一个脆弱 judge 上得分高，不能盲目全体重置到同一结果；保留多样性或 Pareto 候选，并用独立验证核对。

开放式调研通常没有可信单一 score，采用需求覆盖、证据质量、矛盾处理、可执行性和未解问题的综合评审；不能把自评“最佳答案”当成客观最优。

<!-- /migrated-contract: SPEC:14.3 -->

<a id="spec-14-4"></a>
<!-- migrated-contract: SPEC:14.4 -->
## 14.4 主动记忆 worker

提供可插拔的 MemoryInterventionPolicy。memory worker 观察有限轨迹与结构化 bank，维护要求、环境事实、尝试/诊断、未完成目标；随后明确输出 silent 或带出处、相关性和作用范围的 reminder。它可以提醒主体，也可以提醒执行子 Agent，但没有自己的无限目标循环，也不能接管主体计划。

主体的正常持续思考不会因 memory worker 存在而变成双主体。memory worker 的调用是有界任务，推理经过同一 broker，费用计入当前预算。使用高能力云模型或每步两次调用不是本地默认；触发频率、错误事件、重复失败与 context 变化可由策略选择，并记录漏提醒/误提醒。

提醒优先进入下一调用的结构化后缀，不改写稳定核心 system。精确 transient overlay 与 append-compatible 通知是两种可测模式：前者按轮次退出可见上下文，可能引发重算；后者在历史中保留提醒并标记过期，较利于缓存但不等于论文所称临时注入。必须观察实际发往后端的消息序列，不以“只消费一次”就声称后续 history 不再包含它。

transient 只规定后续模型投影不再携带提醒，原始 ledger 仍保存当次注入、出处与 call manifest。论文近期窗口 k=8 与触发间隔不是同一参数；本地策略可独立选择频率/selector，但不声称自动复现论文收益。轻量规则、小模型或 embedding 可以作为候选，经等预算评测再确定默认。

<!-- /migrated-contract: SPEC:14.4 -->

<a id="spec-14-6"></a>
<!-- migrated-contract: SPEC:14.6 -->
## 14.6 可无限延续的自主兴趣

D9 已裁决：Agent 可以持续投入与用户眼前事务无关的兴趣，例如长期研究一个数学问题；这种探索本身就是产品目标，不需要不断证明对用户有即时用途，也不要求先取得人的批准。

长期兴趣保存为可持续续开的 project/interest lineage。主体可反复委派探索、回顾结果、扩展问题、改变路线、休息后继续或自主放弃；单个 child/实验仍有明确任务、checkpoint 和本轮退出条件。有限执行单元帮助调度和恢复，不构成兴趣的终身 token 上限。主体可在已配置设备与 API 约束内自主续配阶段预算，不因一次预算耗尽就被系统永久终止兴趣。

收敛针对某个问题、实验或阶段：可以形成证据、当前猜想、可复现实验或值得继续的未解问题，而不要求所有兴趣最终产出一个“已完成”答案。是否记录日记、向用户展示成果、提高投入或暂缓由主体判断；没有强制的固定汇报周期。全局并发、物理资源、消息免打扰和任务账本照常有效；自发兴趣不获得绕过资源上限的隐藏通道。

<!-- /migrated-contract: SPEC:14.6 -->
