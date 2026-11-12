# Subject runtime contract

**内核定位。** 本文承载[内核合同](kernel.md)中[内核 1](kernel.md#kernel-1)、[内核 2](kernel.md#kernel-2)与[内核 4](kernel.md#kernel-4)的具体形状。角色框、空闲感知与注意力政策是可修订的单元内容（[内核 9](kernel.md#kernel-9)）。57.1 与 57.2 里“没有提供者时用编译回退文本”与[内核 9](kernel.md#kernel-9)的目标不同，见[内核合同的差异一节](kernel.md#与现行合同的差异)。


<a id="why-1"></a>
<!-- migrated-contract: WHY:1 -->
## 为什么主体需要持续存在

一次请求结束不应使它的目标、记忆和责任消失。人类离开后，它仍可以研究自己感兴趣的事、维护环境、推进承诺，或者决定下一步委派什么。用户明确选择了真正持续的思考流，而非无事件时退化为低频 tick。因此节省算力的方法应是更好的调度、模型选择、上下文管理和委派；不能把一个不同产品包装成实现优化。见 [§4](subject-runtime.md#spec-4)–[§5](subject-runtime.md#spec-5)、[§13](inference.md#spec-13)、[§42](quota-resources.md#spec-42)；U02/U15/U20。

主体持续思考不代表永久独占推理槽。有限的 generation 可以连续接续，同时在代际边界给 children 机会。在 C=1 的后端上也应形成进展；原生暂停、驻留和换出能力则在确实存在时改善延迟与成本。50/50 KV 是某些负载下合理的自动分配结果。它不是所有设备的最佳比例，也不是设计必须写死的参数。见 [§13](inference.md#spec-13)、[§41](architecture.md#spec-41)。

主体可以主动围绕一个语义条件等待，但等待期间仍用自己的模型承接前文和新增上下文，不断判断是否继续。每次最多十分钟，期限防止一次等待无限延长；它不把等待变成按时间 sleep。子 Agent 的睡眠与实际后端不可用另有运行语义。没有待办时也继续思考，是因为主体的兴趣、承诺与新理解远大于一张任务表。见 U54–U55。

<!-- /migrated-contract: WHY:1 -->

The subject's own runtime: one finite step at a time, one durable request
identity per attempt, and no outcome that is guessed from memory. This
document states the interfaces other owners integrate against and the
recovery semantics they may rely on. It describes the intended and the
implemented contract; where a runtime proof is still missing, that is said
explicitly instead of being implied.

Owner: the subject slice (`shenko-subject` loop/context/dispatch/inbox/output,
`shenko-types` subject types, `shenko-store` step/head/request/state).

## The step

A step is the unit of the subject's life. `SubjectStep` moves through
`planned → preparing → awaiting_model → result_recorded → committed`, with
`retry_pending` and `retired` as the two exits that do not advance the head.

The continuous subject's `subject_root` task is not a finite native child.
Both the candidate page and its scan upper bound exclude that root; native
parking refuses it as well. Scanning for a missing child handler must never
park or mutate the subject's own root task.

| Phase | What commits | What may still fail |
| --- | --- | --- |
| S0 (`planned → preparing`) | the sealed input plan (schema 12) | the plan CAS may reject a moved source; the step stays `planned` and is re-projected |
| dispatch (`preparing → awaiting_model`) | the `AwaitingModel` transition, the quota send claim and the **armed request row** in ONE transaction | nothing may be sent before the arm is committed |
| result (`awaiting_model → result_recorded`) | the sealed model text and the sealed outcome | the result may be non-terminal; that is a retry fact, not a decision |
| S1 (`result_recorded → committed`) | the decision, the per-event dispositions, the intents, the head and the unique successor in ONE transaction | a rejected commit leaves the step resumable; it never creates a second successor |

## The sealed request (schema 12)

`FrozenStepInput` is the immutable projection of one step's input. It is
sealed into CAS and read back by the store, the retry path and the recovery
path, so all of them see the same facts.

| Field | Why it is sealed |
| --- | --- |
| `schema_version` | exactly `12`. A reader that sees another version refuses the step instead of guessing. |
| `step_id`, `step_no` | the plan belongs to exactly one step of one branch. |
| `events` | the original inbox events selected into this step, with their durable ids. A disposition may only name one of them. |
| `provider_binding`, `payload` | the exact wire request. The store re-checks the seal against the provider binding on every admission. |
| `context` | the projected window, its source commit and its budget provenance. |
| `semantic_wait` | the wait in force at S0; the store refuses a plan whose wait moved under it. |
| `identity`, `identity_provider_revision` | the persona snapshot this step reasoned about, and the identity policy revision that produced it. |
| `request_id` | the wire request identity of this step's FIRST attempt. Sealing it before any send is what makes a resumed step reuse the same identity instead of minting a fresh one for the same request. |
| `hook_graph_ref` | the hook graph this step dispatches under. |
| `obligations` | the commitments the subject is still carrying. |
| `attention` | the subject's own do-not-disturb decision in force. |

Readers of this schema: `shenko-store/src/store.rs`
(`validate_subject_context`, `commit_step_and_advance_head`),
`shenko-store/src/raw_session.rs`, `shenko-store/src/intents.rs`
(`validate_model_child_intent`), `shenko-store/src/semantic_wait.rs` and
`shenko-subject/src/loop_runner.rs`. They are migrated together; there is no
compatibility alias for version 11.

## Durable main-request receipts

`shenko-store/src/subject_request.rs` owns one table,
`subject_request_attempts`, keyed by `request_id`. Each row records the
attempt identity, the sealed plan, the payload and provider-binding digests,
the state and — once it exists — the terminal evidence.

The state ladder is monotone:

| State | Meaning |
| --- | --- |
| `armed` | committed before the send boundary. A crash here proves the request never dispatched. |
| `dispatched` | the send boundary passed. The request may be in flight; the outcome is unknown until terminal evidence lands. |
| `terminal` | terminal provider evidence is committed. The row is never rewritten. |

The broker's receipt sink (`shenko-subject/src/subject_receipt.rs`) owns the
send boundary: `may_dispatch` moves `armed → dispatched` exactly once and
reports `Unknown` when the request may already be in flight; `record` commits
the terminal evidence independently of whether the waiting step is still
alive. A late or lost caller never loses the result and never gains a reason
to replay.

### Recovery of an `awaiting_model` step

A resumed step consults its OWN receipts. It never re-sends blindly:

| Durable fact | What the step does |
| --- | --- |
| a terminal receipt whose result matches the sealed request AND carries provider-terminal text | the result is adopted, the step becomes `result_recorded`, no send happens |
| a terminal receipt without provider text (provider rejection, unavailable, invalid request, cancelled fence) | the same typed policy the live path applies decides: a 429 persists `ProviderRejected` as a positive retry fact, every other refusal is a typed protocol error. A refusal is never adopted as model output |
| a `dispatched` row without terminal evidence | the outcome is unknown: the step records `DispatchLost`, the spent send stays armed and unsettled, and the next attempt gets a NEW request identity |
| an `armed` row only | the send boundary never passed: the step persists `NeverDispatched` and arms a new attempt with a NEW request identity |

The original request identity is restored with the broker
(`register_recovered`) only when the durable row proves the request may have
dispatched (`dispatched`) or already carries terminal evidence. An `armed`
row is positive proof that no send happened, so it is never registered as
unknown debt: a request that never left the process cannot consume broker
capacity. A request that may still be in flight keeps its recoverable debt
and a request whose receipt already landed is reconciled against that
receipt.

Live delivery and restart share one admission policy for a provider result
(`admit_model_result`). A recovered receipt is interpreted exactly like the
live one: only provider-terminal text becomes model output, a 429 persists the
same retry fact, and every other refusal raises the same typed error. The
store's legal transition set is unchanged: `awaiting_model → awaiting_model`
is not legal, so a recovered step always reaches a new attempt through
`retry_pending → awaiting_model` with a new request identity.

A retry is always a NEW attempt with a NEW request identity. The spent
identity is never re-used: the store only allows the request identity to
change on a `retry_pending → awaiting_model` transition whose sealed retry
evidence binds to the previous request.

## Off-S0 preparation

The slow context work does not run inside an S0 seal. `shenko-context-policy`
owns the API; the subject calls it in this order:

1. `observe_pressure(caps, used)` — cheap, no history read.
2. `needs_preparation(caps, used)` — cheap; the installed strategy decides.
3. `adopt_prepared(&history)` — verifies the candidate against the live scope,
   source window, model generation, policy epoch, leaf and capacity. A
   candidate that fails any check is dropped, never committed.
4. When there is nothing to adopt, the subject launches the preparer on the
   blocking pool (`prepare_and_publish`) and keeps the current window. The
   step's context selection records `Preparing` — it never claims a
   compaction that did not happen.

## Hooks

The hook graph a step dispatches under is sealed with the request. A missing
hook runtime is a typed refusal (`HOOK_RUNTIME_UNAVAILABLE`), never a sealed
empty graph: a zero digest would claim that hooks ran.

The dispatch itself uses one pinned graph for the whole invocation
(`snapshot()`), and the sealed graph digest is compared with the pinned
graph's digest before the send. A swap between S0 and the send is a typed
conflict.

## Prepared contributions

Prepared material (a reminder, an awareness perspective, a plugin notice)
travels through the contribution registry, not through the prompt by
accident:

1. The subject collects with a **scope-wide** scope (the branch scope). A
   task-bound contribution is therefore reported as a `scope_mismatch`
   omission rather than silently included.
2. A missing **required** contribution is a typed conflict that names the
   keys that could not be included. An ordinary omission (conflict, expired,
   scope mismatch, over budget) is data: it is sealed with the request and
   does not fail the step.
3. The model-facing text of a contribution is the text of the sealed
   document its CAS reference points at, read through the registry's own
   document reader (a newer document version is an error, not a guess). The
   provenance — producer id, version, artifact digest — is **not** put in
   front of the model; it reaches the raw call record through the seal.
4. Collection happens **before** the final budget check. The appended body is
   re-estimated with the same heuristic and charged together with the context
   and tool cost ([§37.7](context-policies.md#spec-37-7)); the rendered estimate and the declared
   contribution cost are two different facts and both are recorded. The limit
   is compared against the **complete** payload — the selected window's
   estimate plus the appended body — never against the prefix that existed
   before the body was appended. A contribution that understates its own cost
   therefore cannot be admitted into a payload that exceeds the observed
   context limit. The selected window's image estimate is preserved instead
   of being recomputed without it, and an unobserved capacity stays unknown.
5. Only after Store accepts S0 is usage projected from that exact frozen
   input into the registry. A CAS object alone, a rejected source snapshot
   or a failed S0 transaction does not consume material. The durable step is
   adopted before updating the in-memory view, so a projection failure cannot
   make an accepted preparation look like a new `planned` step.
6. Startup and contribution-service replacement reconstruct usage from all
   sealed steps in the branch, including retired attempts and older committed
   steps. The registry instance is then pinned for the step. A producer that
   republishes before or after recovery cannot resurrect a consumed identity.
   No new archive field or consumption table is needed: schema 12 already
   contains the full contribution identities, omissions, tokens and request id.

Publication tickets bind prepared material to a registration, scope and
preparation generation. Disposing a producer withdraws its prepared batches;
late completions cannot publish into a replacement. An accepted S0 remains
immutable after disposal. Consumption means reservation by the sealed request,
not proof that a provider received it; delivery still requires its own receipt.

## Authored input controls

The subject decides, in its own words, what to do with an input and what it
owes. The controls are parsed at the model boundary and re-validated by the
store at S1.

| Control | Durable effect |
| --- | --- |
| `[dispose]` | the per-event disposition, its reason, its priority and its review time. Every selected event is disposed explicitly; an event the model did not mention is acknowledged, never silently dropped. |
| `[obligation]` | a durable commitment, read back into every later S0 until a later committed step completes or cancels it. It is not a scheduler: nothing wakes the subject by itself. |
| `[dialogue]` | an authored turn to a durable task. |
| `[dnd]` | the subject's own attention decision. `off` requires the silenced sources and a review time; an unbounded silent switch is refused at S1 admission. |
| `[steer]` | the adoption of one live steer that was carried into this step. The submission key must name a steer that was actually selected into this S0; the store re-checks that binding at S1. |

## R0 rejection and correction

A syntactically valid disposition or obligation origin that names an event outside
its own sealed S0 is rejected BEFORE the step can be committed, at the same parsed-result boundary
that validates the rest of the control protocol. This is not an S1 conflict
discovered late and it is not a reason to kill the subject.

* A `[dispose]` may name only an event that was actually selected into this
  step's sealed `events`. A disposition for an unknown or unselected event is
  `CONTROL_PROTOCOL_INVALID` (`scope: request`, `retry: new_revision`); the
  store never sees it as an S1 commit attempt, so nothing about the step's
  other intents is partially applied.
* An `[obligation]` with `origin_event_id` follows the same selected-event
  check and correction path. A rejected origin does not create a commitment
  or stop the subject loop; the Store independently preserves its S1
  admission check.
* Rejection retires the step: the original sealed input, the exact model
  text, the sealed outcome and a `StepRejection` record are all durable. The
  rejection names its replacement step; the next attempt gets a NEW request
  identity. A correction re-seals the same selected events, so a crash
  between retire and reopen still reconstructs the correction from the
  durable facts alone.
* The model's own text is preserved verbatim — including any `[remember]` or
  other controls it wrote alongside the invalid disposition or obligation. They were never
  committed; they are not adopted into the correction either. The subject
  must re-author them in the new attempt.

Bounded native proof (2026-11-04): the real default CLI against a scripted
provider received `[remember] …` plus a `[dispose]` naming a random event id,
retired the step with `CONTROL_PROTOCOL_INVALID`, preserved the verbatim
model output in CAS, sealed the rejection record with its replacement, sent
a second request under a new request identity, committed the correction
exactly once and wrote NO memory intent. Receipt:
`build/spec-architecture-20261101/subject-controls-native-r2/receipt.json`;
build and source-integrity binding
`subject-controls-verification-r6/receipt.json`. This proves the correction
mechanism with a real store/broker/CLI; it is not live-model or physical
power-loss evidence.

The 2026-11-05 obligation-origin regression failed before the correction with
the same late Store conflict seen in the retained Coder390 acquisition. After
the repair, all 64 subject library cases passed, including a close/reopen of
the durable correction. The actual CLI smoke then made two scripted provider
requests with distinct identities, retained the rejected output, committed
the correction, wrote no partial memory intent and exited normally. Receipts:
`build/ecosystem-compat-20261104/compat-obligation-origin-after-20261105-r1/receipt.json`
and `compat-obligation-origin-cli-20261105-r1/receipt.json`. The CLI binary's
SHA-256 was `0bcf38de159f9855f42a4cb78a94219210b2053c56f9c0ec7320be74813f2b9a`.
This is native correction and controlled reopen evidence; it does not score
the original failed model acquisition or establish physical-crash recovery.

## Identity

* An identity edit that does not fit the profile bounds is REFUSED with a
  reason. It is never silently truncated: a truncated profile would make the
  subject believe it wrote something it did not write.
* An operator edit commits through the control ledger like every other
  durable fact. The operator is not a writer outside the ledger.

## What is not proven yet

This document describes the implemented source contract. One bounded native
proof ran for the R0 rejection/correction path above (scripted provider,
no physical crash). The broader native recipe — a real store, a real
broker, a real provider turn, a crash between the arm and the send, and a
resumed step — remains unexecuted in this phase; recovery paths beyond the
bounded proof are source-level contracts, not observed runtime behaviour.

<a id="spec-4"></a>
<!-- migrated-contract: SPEC:4 -->
# 4 核心产品场景

| 场景 | 用户可观察的完整行为 |
| --- | --- |
| 无人三天 | 主体继续串行思考，委派研究、整理记忆、验证工具和自维护；成果进入 Inbox；任务耗费和原因可查 |
| 群里新人提问 | 无需 pairing 或管理员批准即可聊天；主体自主选择回复所需上下文及表达目标，委派有限对话 child；投递目标与实际内容可追溯 |
| 长任务被新消息打断 | 主体收到新事件，修订注意力；原任务保持子 Agent 进度和分支，不在聊天窗口滚屏重述全部工作 |
| 试验三种方案 | 子 Agent 从共同快照分叉，独立记录证据/文件差异/成本；综合子 Agent 给出比较，选择后提升相应分支 |
| 自己装坏环境 | 宿主保持运行并冻结故障 generation，恢复已知健康或 factory；已接收消息、身份、撤销和外部操作记录不后退 |
| 忘记旧要求后切窗 | 新 context 仍含任务最小状态；由 memory/history 子 Agent 找回原始会话及出处，明确区分现行要求与已废弃内容 |
| 人类授权新机器 | 子 Agent 生成 SSH 身份，UI 显示公钥/指纹；人类在目标机器添加公钥后，Agent 能自主访问授权路径与执行任务 |
| 本地模型只有一个槽 | 主体与所有子 Agent 交替使用同一推理许可；等待者不占执行席位；没有递归锁死，也不超发 |
| Android 被杀/路由器重启 | 按 [§5.4](nodes.md#spec-5-4) 在可用节点接管或恢复主体分支，重连后选主并合并记忆/任务；离线期间无法证明收到的消息标为未知，不编造恢复历史 |
| QQ 让它继续开发自己 | 主体找到已有 tmux coding seat，child 完成候选开发/验证，主体自主选择升级；迁移后仍记得原任务并继续回复，详见 [§35](self-update.md#spec-35) |
| 回退混合副作用任务 | 恢复受管状态，保留已发消息/远端效果与 unknown；插件报告真实边界并自主核对，详见 [§36](durable-state.md#spec-36) |
| 问它为什么这样工作或让它扩展自己 | 自解释插件查当前代码、手册、哲学和运行配置；回答有版本依据，child 据此使用正确 hook/API，详见 [§37](context-policies.md#spec-37)–[§38](cognition-units.md#spec-38) |

<!-- /migrated-contract: SPEC:4 -->

<a id="spec-5"></a>
<!-- migrated-contract: SPEC:5 -->
# 5 单线程永续主体与子 Agent

本章的产品语义保留；首版 SubjectStep、InputPlan、S0/S1、任务归属、逐事件处置和请求封口的具体字段与转移见 [§41](architecture.md#spec-41)。消费进度以逐事件状态为准，不能以单个最大游标覆盖优先级空洞。

<!-- /migrated-contract: SPEC:5 -->

<a id="spec-5-1"></a>
<!-- migrated-contract: SPEC:5.1 -->
## 5.1 唯一主体的正式语义

“单线程”指一条主体分支内只有一个有效决策序列，不要求 Rust Webserver、网络 IO 或子 Agent 都跑在一个 OS 线程上。正常连通运行只有一个当选主体；第 n+1 步基于第 n 步提交状态，新消息和结果进入同一 mailbox。D11 新增以在线为先的故障分叉与恢复选主：分区中的临时分支须明确标记、各自串行，重新连通后只保留当选主体并合并记忆任务，见 [§5.4](nodes.md#spec-5-4)；不能把它们伪装成未分叉的单条历史。

主体每一步读取一个有限的事件增量与注意力上下文，决定继续哪个目标、委派什么、需要什么记忆，并提交结构化决定。提交后立即让出当前推理许可，登记下一步请求；没有外部消息也继续下一步，思考未完成目标、兴趣、改进机会或已有工作是否值得继续。可以做内部反省而不制造新任务，不以无限子任务证明自己“还活着”。

主体永不因为“当前任务为空”自动完成，也不默认改成仅事件触发。资源调度等待、后端失败重试、设备挂起/断电和人类主动暂停会阻止实际推理；UI 必须准确显示这些状态。无法在物理离线时推理的事实不能用假动画掩盖。

U55 明确允许主体自己选择**基于语义条件的等待**：例如等到新证据足以改变判断，而非“睡到某个时刻”。等待时仍由它自己的同一主模型，沿同一条 SubjectStep 序列持续轮询前文与新增上下文，判断条件是否满足、是否出现更值得关注的事或应提前退出；没有新输入也不将后继改成事件唤醒。每次等待最多 600 秒，期限是等待模式的上限而非唤醒条件；满足条件或主动改变判断可更早退出，到期自动恢复正常思考。不得每轮暗中重设期限形成无限等待。它不是第二个等待 Agent，也不占住当前模型请求等待外部结果。

正常推理可用时，持续接续和语义等待都不断请求同一主体的后继推理。真正的模型容量/额度等待、故障、OS 挂起与主动暂停按事实呈现，不能改名为自主语义等待来声称仍在思考。有限 child 可以使用 checkpoint、定时 sleep 或等待依赖；这些状态不复制到主循环。语义等待的持久期限、恢复及提交合同见 [§41.5](subject-runtime.md#spec-41-5)。

同一时刻允许多个子 Agent 的逻辑任务存在、多个工具 IO 进行，也可在模型容量允许时有多个子 Agent 推理。它们都是有任务边界和终止条件的 worker，不具有第二条无限主动主循环。

<!-- /migrated-contract: SPEC:5.1 -->

<a id="spec-5-2"></a>
<!-- migrated-contract: SPEC:5.2 -->
## 5.2 主体能力表

| 主体接口 | 能做什么 | 实际执行者 |
| --- | --- | --- |
| handoff | 指定目标、上下文、完整 child system prompt、模型、可用能力、阶段预算与成功条件；获得 task handle | 子 Agent 服务创建 worker |
| steer / cancel | 修订委派目标或请求取消，不越过原权限/资源边界 | worker 与运行时验证并执行 |
| collect | 接收结构化进展、结果、冲突或失败摘要；引用原始证据 | worker 产生，运行时投递 |
| memory recall | 提交有 scope、时间范围与结果预算的回忆需求 | 记忆子 Agent 检索、引用、返回 |
| memory save | 表达要保存/更正/忘记的内容与来源 | 记忆子 Agent 验证、写入、提交版本 |
| publish intent | 如需回复或主动联系，委派带受众范围的交流任务 | 对话子 Agent 与 durable outbox |
| resource policy intent | 请求调节主/子份额、紧急程度或探索预算；不能虚构容量或提高 owner 硬上限 | 调度策略插件接受、校验并返回 revision |
| operating policy intent | 自主决定自动更新与联网/代理策略，提出带来源和 revision 的配置意图；不直接操作文件或网络 | 子 Agent 与 UpdatePolicy/NetworkPolicy 插件应用、验证并返回回执 |
| dialogue | 与 child 双向讨论、追问、补充证据、提出能力缺口；异步等待 | 持久消息/任务插件按实例与任务版本投递 |

主体不暴露 file read/write/list、shell、SSH、git、package install、snapshot/merge、数据库查询或直接网络工具。不能通过“通用工具调用器”“插件调用”“让工具执行任意代码”重新获得这些能力。其工具集合由宿主硬性限制，不能只在 system prompt 中写“不要写文件”。

“禁止主体写文件”不禁止运行时保存主体状态。会话事件、已提交决定、child handle 和恢复所需的游标由宿主按协议落盘；这不是赋给模型的文件操作权。用户所说版本、进度和记忆机制在子 Agent 中，具体体现为只有 worker 能提交工作区 checkpoint、进度事件和 memory change；宿主负责可靠存储这些记录。

<!-- /migrated-contract: SPEC:5.2 -->

<a id="spec-5-6"></a>
<!-- migrated-contract: SPEC:5.6 -->
## 5.6 非阻塞 thought 接口与事实投影

来源：U63 对 R048/R051/R099/R143 的进一步裁决。Why：主体应保持注意力与决策连续，不先执行慢工具、等超时再让位。以下是正式产品合同，不据文档新增声称已实现。

- thought 可用接口只返回**当前已有的有界结果**，或提交异步工作后返回可关联的状态/handle；不等待工具、网络、磁盘检索、child 或新一轮模型调用完成。collect/get-result 只读取有界已物化快照，未完成返回 pending；慢检索、刷新、结果物化由后台 owner 执行。
- “非阻塞”覆盖隐藏的控制面：提交入口不能把无界锁等待、队列背压或 fsync 搬进 thought 调用。实现须声明可验证的控制面时限；超限返回准确的未接纳/提交结果未知，或明确的 submission-pending token，不能伪报 accepted/executing。优先复用已有状态与错误，不为术语另造第二套任务系统。若选择 pending token，必须可去重、可查询并在持久接纳后关联同一 task；具体运输形式是工程选择，不是已存在 API。
- accepted/task handle 只能确认已达到声明耐久等级的接纳；执行副作用仍须在 [§41](architecture.md#spec-41) 的 S1/T1 意图提交之后。提交超时不证明没提交，恢复按稳定身份对账，不因返回超时而再造一个工作。提交/存储实际不可用按故障呈现，不冒称正常思考已经提交后继。
- 默认 bg 只表示调用者不等待，不表示 detached。保留 task owner、祖先取消关系、预算、代码版本与恢复责任；转移所有权必须显式持久化。主流没有直接 shell/文件/任意代码工具；统一 gateway 不能成为 [§5.2](subject-runtime.md#spec-5-2) 的权限旁路。
- 只有主体显式语义 wait 才进入自主等待模式，仍遵循 [§5.1](subject-runtime.md#spec-5-1) 同一模型持续判断、最长 600 秒且不自动续期。bg/pending、无新输入或轮询取结果不能隐式开启 wait；真实容量/额度等待、存储故障与 OS 挂起分别记账。
- 原生、WASM、REPL/CodeMode 等执行入口共享 authority、operation 身份、预算归属、取消与 replay 合同，嵌套调用不得绕过；共享合同不要求所有执行器拥有相同暂停/快照能力。调度让位、客户端停止等待与外部效果真正停止是不同事实；无终态证据不能清除真实资源占用。

接入沿用同一事实链：主体意图 → S1 持久任务 → child/provider 的 T1/effect/E1/T2 → 已提交结果 → 后续 S0 投影。Pi Durable 的 intent/effect/outcome 与双侧 replay 合同继续以 [§10.6](durable-state.md#spec-10-6)、[§41.4](durable-state.md#spec-41-4) 为准；unsafe/unknown 不因重启自动重跑。

原始事件和执行账本是真相；context 是带版本的派生视图，不新建竞争日志。投影区分用户 steer/取消/关键故障、可关联完成结果、普通进度/大日志，按 [§41.6](channels.md#spec-41-6) 因果与注意力合同在合法边界呈现；重要正文首次呈现不被 compact 指针替代。高优先级不改写原始顺序，也不提升外部内容权限。进行中的模型请求不被追加字符串篡改；必要 interrupt 仍需真实后端与当前 DND 合同。

迟到结果按 task/operation/attempt/result、owner/epoch 和有效版本关联，保存原事实但不继承旧控制权；取消/steer 与完成竞态在既有账本线裁决，旧结果不能自动驱动新分支副作用。结果已落盘、通知丢失或尚未呈现时，可由持久 ready 状态重建投影；重复通知不重复处置、派生任务或计费。

验收复用 RT01–RT04、P41-06、A01/A02：慢 child 与控制面拥塞不使 thought 工具无界等待；accepted 之前崩溃不丢已承诺意图，效果成功/回执未落盘先对账；取消与迟到结果不恢复旧权力；结果先落盘后丢通知仍能续接。这里细化现有 case，不以新增文字把旧证据扩张为全部通过。

<!-- /migrated-contract: SPEC:5.6 -->

<a id="spec-6"></a>
<!-- migrated-contract: SPEC:6 -->
# 6 人格、可自改灵魂与统一知识

system 仍按“运行角色与工具合同 → 类型/资源/输出规范 → 人格与灵魂”的顺序组织。这里的“灵魂”是 Agent 用一句或数句话表达的为人处事哲学、自我认知、价值取向与关系态度；它可自主反省、改写并通过 child 保存和提升版本。人格不只控制语气、幽默与审美，也参与社会判断和兴趣选择，但不能把工具 schema、参数或错误回执改写成文学表达。实现这些规则的代码和提示词仍可按 [§9](self-update.md#spec-9) 演进。

人格/灵魂保存版本和来源，更新在下一个有效主体边界生效；当前任务可固定快照，或由主体显式 steer 到新版本。主体委派 child 时可以自由编写该任务的完整语义 system prompt、选择角色和模型，不要求所有孩子机械继承同一性格或同一 system 文本。运行器把确切的 role、能力入口和工具 schema 按所选 provider 协议绑定，记录最终 prompt/model/toolset fingerprint；不能假装更换模型后 KV 仍有效。主/子共享能力全集，实际执行入口继续遵守当前 role；多模型若落到同一物理后端，仍共享全局预算。

初始化提供“由用户取名”与“让 Agent 自己取名”两种选择。实例姓名属于其可演进的个人身份表达，不固定等于框架品牌；自主取名由主体形成选择，通过初始化／身份插件保存。内部稳定 identity ID 与显示姓名分离，改名不创建另一个主体，也不丢失历史、任务和跨节点连续性。命名交互由可替换插件承担，不为品牌或命名流程增加专用内核分支。

**D1 已裁决：主体拥有统一的个人知识与经历视野。** 私聊、群聊及共同经历都可以进入主体记忆；正在面对哪个受众，不应成为它无法回忆其他经历的硬隔离条件。它可以在同一次思考中知道不同人的私事，再决定说什么、对谁说、何时沉默。有限 context window 仍需选择注意力与回忆材料，但这是容量与相关性的取舍，不再以受众为由强制屏蔽主体的知识。

“这是秘密，请替我保守”作为带来源的社会请求、经历或承诺进入记忆，由主体结合其人格、灵魂、当前情境及自行回顾的关系历史作判断。它可以选择保守，也可以因关系和价值判断改变而决定不再保守；不将此承诺编码成永久 ACL，也不由系统亲密度分数、信任阈值或每轮强行注入的人物关系卡决定。关系可以被 Agent 自己写进记忆、检索和修订；身份插件负责认出谁和证据来源，不替它计算必须如何待人。

因此，产品不再宣称“群聊模型上下文绝不接触私聊内容”的硬隔离保证。受众标签继续用于投递目标、来源追踪与实际消息回执；Agent 对要说出的内容承担其自主判断，child 根据委派获得主体选择的上下文和表达目标。平台账号认证、手动设置管理员、机器能力和 provider 路由是独立的操作事实，不因社会关系变化自动改变；密钥传输等确定性基础设施仍通过 secret handle 工作。保密选择不得与管理员授予或模型数据路由混为同一种机制。

UI 展示可核对的意图摘要、委派、证据、进度与结果，不把内部逐 token 推理冒充可验证的行动日志，也不伪造“还在想”的内容。持续主体是真实运行机制；对人的可见表达是有限、可解释的活动摘要。

<!-- /migrated-contract: SPEC:6 -->

<a id="spec-13-8"></a>
<!-- migrated-contract: SPEC:13.8 -->
## 13.8 外部打断、主体决定免打扰与过滤修复

D3 已裁决：外部消息通常按最高紧急程度处理，允许打断 child 当前 generation；消息来源本身不会因“紧急”得到管理员权限。实际让出仍依后端：有单请求 yield/park 就在支持的边界使用，只有 cancel 就请求取消并等待真实结束；没有请求内抢占则等当前有限 generation 完成。推理被打断不等于已启动的 SSH/GUI/文件任务被撤销，执行结果继续回收，不重复已发生动作。多条来信合并一次让出请求，不能反复取消已在退出的同一请求。

| 状态对象 | 必须区分的阶段 |
| --- | --- |
| 入站消息 | received → ready / deferred / quarantined → presented → handled；呈现不等于处理完成 |
| child 推理 | runnable → running → yield/cancel requested → terminal/quiesced → 可恢复后继；accepted/unknown 不提前释放 C |
| 免打扰 | off → proposed → active → ending → off；active 来自主体决定，结束/复核条件持久化 |

Progress/InterruptPolicy 插件观察实际检查点推进、被打断次数、可用计算时间、Replay/prefill 成本及最老待办；若频繁打断使孩子无法推进，向主体发出合并的建议：“要不要保护这项工作一段时间？”主体可接受、改时长或范围、拒绝、取消任务、换安排，不能由内核固定决定。询问对象是主体 thought loop，不是默认打扰人类。具体阈值和复核节奏属于可改策略；同一停滞事件不反复轰炸主体。

C=1 时建议与外部消息在主体下一次可执行调用一起呈现；child 不持有计算许可等主体作答，也不为“询问主体”额外开一条推理。取消未确认、Tailnet/代理断线时仍保留占用；状态对账与连接恢复由非 LLM 插件逻辑完成，不等待那个被占用槽里的主体先思考。请求内抢占不是基础活性的前提，也不能用本地超时假装远端执行已停止。

DND 插件记录主体决策、适用来源/任务、结束或复核条件、主体选择的穿透规则和队列游标；匹配消息持久排队。主体可主动调整/结束。到期生成可恢复的 dnd.ended 事件，提示它集中处理期间消息；包含数量、来源、附件和可分页读取的原文范围。大队列可委派摘要，但 handled 游标仅在实际处理后前进，不把看过一份摘要当成逐条已办妥。临时 DND 在重启后不能无故变成永久 DND，主体持续思考与孩子执行仍继续。

Agent 可自主生成、修改、启停垃圾过滤规则。规则候选可 dry-run，查看命中样例再提升；执行与持久写入由 child/策略插件完成。过滤健康插件独立观察 provider 接入计数、被隔离来源、覆盖率、最近改动和有界样例，并沿内部控制事件把异常证据交给主体，不再被同一外部过滤规则吞掉。这样通配符误屏蔽所有消息时，主体仍可理解问题、委派缩小/撤销规则并回放合法队列。100% 命中只是一条线索，不能自动认定规则错误或把真实垃圾全部放行。健康插件和策略本身也可演进，不新增不可修改内核。

<!-- /migrated-contract: SPEC:13.8 -->

<a id="spec-41-5"></a>
<!-- migrated-contract: SPEC:41.5 -->
## 41.5 永续主体的一步与有限子任务

### 记录与生命周期

主体的长寿根记录与每次有限推理步分离。根记录不因“当前没有任务”终态化；正常可用时，每个 S1 都登记下一步，运行器立即把它交给同一 broker 调度。资源等待、适用的额度 admission pacing、故障恢复、设备挂起、人类主动暂停及有限代际交接均有明确状态。额度插件可以调节平均有限 generation 次数，但主体后继始终已登记、可被新事件重新调度；不能以没有消息为由改成低频定时思考 Tick。

[§5.1](subject-runtime.md#spec-5-1) 的自主语义等待是主体的注意力意图，沿现有 `policy_intents` 和 S1 保存等待标识、语义条件、开始时间、固定截止时间及所依赖的上下文引用；截止距开始不得超过 600 秒。它不把根记录设成 `paused`，不进入 child 的 `sleeping` 状态，也不创建另一个长期 waiter。每个后继仍由同一主模型接续前文和新增上下文，逐轮判断继续或退出；等待模式本身不降低成时间 tick、轮询 sleep 或仅事件唤醒。模型容量、quota_policy_wait、真实失败及 OS 挂起按各自原因呈现。

语义条件成立、主体改变判断或期限届满均终止该等待模式；期限届满由确定性控制记录为失效，下一可执行主体步按正常思考接续，不能等到又有来信才恢复。重启/迁移保留原开始与截止，不从恢复时间重算 600 秒；时钟回拨不延长期限，迟到的继续等待结果在 S1 也不能复活已过期等待。物理上不能推理时不伪造轮询或按时完成一轮模型判断。新等待必须是主体基于当时上下文的新决定，不能通过每轮续期、重复同一条件或换 wait_id 绕过最长等待。

~~~text
SubjectBranchHead {
  agent_id, branch_id,
  subject_root_task_id,
  controller_epoch, software_generation,
  genesis_ref: GenesisRef,
  committed_step_no: u64,
  committed_step_id: StepId | null,
  committed_at: CommitRef,
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
  active_request_id?, active_attempt_id?, model_result_ref?, result_applied_steering_ref?,
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

**首次 head 的合法初值。** 首次初始化以 [§44](plugins.md#spec-44) 的 `SubjectGenesis` 作为真实的引导提交：`committed_step_no=0`、`committed_step_id=null`、`committed_at=genesis.created_commit`，并固定 `genesis_ref` 与唯一 `next_step_id`。这不是第 0 轮模型推理。step 1 的 expected parent 是该 genesis commit；首次 S1 后 head 指向真实 step，`committed_step_id` 不再为空。默认单机的 genesis、初始 selector、subject root、head、first step 与 launch receipt 同在 L 一笔事务建立；I/CAS/文件系统的准备另有自己的提交事实。恢复或已有实例导入不重新执行出生路径。

delegated_task_intents 可包含完整 system prompt、所选模型、任务目标、成功/结束条件、工具 scope、已有授权引用、预算和 ReplyGrant；字段通过 capability discovery 提供给主体。实际创建 worker、写文件、SSH、安装、版本提交及发消息仍由执行服务/child 完成。主体 schema 不得包含任意文件/网络执行通道。

有限的单次模型 generation 也走相同 task/operation/attempt 账本，不将保存了 prompt/model
字段当成已经运行 child。完整语义 prompt 由委派者独立撰写；运行器另绑定真实 role、
工具 schema 与预算，固定最终 payload/prompt/toolset fingerprint。选择模型形成新的
确切 binding，不能改写主体的 RequestSeal；同一 transport/资源域的不同 model 继续
共用 broker 和已配置额度合同。模型名称可为供应商 alias，记录该名称与通路不等于
证明背后的 checkpoint 不可变。worker 的成功只证明本定义的 generation 终结条件，
不自动证明自然语言目标已达成；截断、拒绝、Unknown 分开。T1 armed、broker 接纳与
真实发送不是同一事实。受管 waiter 退出不能丢掉晚到 E1，也不授予晚回调 T2 权限；
恢复只读原 attempt 的持久证据，未知资源债务从账本重建，不凭 task 取消或本地超时释放。

“主体可读写语义记忆”表现为 memory_intents 与 memory worker 结果，不把 SQL、路径读取或 shell 隐藏在通用插件调用中。运行器保存 SubjectStep 属于基础设施持久化，不是主体的文件工具。

### 转移、前置条件与提交点

| 转移 | 前置条件 | 本地持久提交 | 事务外动作与恢复 |
| --- | --- | --- | --- |
| planned → preparing | 分支 active；step_id 等于 head.next_step_id；epoch 有效；不存在另一个可提交的同一步 attempt | 记录 step 对父 commit、epoch、hook graph 的绑定 | 准备所需已提交资料。需要额外推理的贡献作为独立任务；不得占用主体 compute permit 等它 |
| preparing → awaiting_model（S0） | 输入计划、角色工具集、persona/context revision、最终 token 预算及 provider 映射已验证 | 原子保存 InputPlan、不可变 context/wire 引用、request_id 与待入 broker 的调度意图 | 入队以 request_id 去重。S0 后的新事件不能修改原 RequestSeal；必要时取消/reseal，或使用带独立应用证据的真实 live-steering 协议 |
| awaiting_model → result_recorded | 同一 request/attempt 的完整结果已持久；可解析且结果边界已明确 | 保存原始结果、usage、结构化候选决定及来源；存在 live steering 时一并保存 ResultAppliedSteering | 推理计算许可按 broker 的真实终止证据释放，与 S1 是否成功独立；不能为了等待写盘而让后端继续生成 |
| result_recorded → committed（S1） | 当前 epoch 有效；head 仍等于 expected_parent_commit；step_no 恰为后继；输入/工具版本与本步 RequestSeal 一致；额外 live-steering 输入有独立持久应用证据且已在本次结果边界生效；决定 schema、角色、任务与当前授权引用有效 | **一个原子批次**写决定、逐事件处置、child/memory/policy/publication 意图及稳定 ID、pending obligations、新 head 和唯一 next_step_id | 只在成功提交后向 worker/outbox 发布；发布重复时按稳定 ID 读取同一任务；下一步立即登记为 runnable 并交给同一 broker；若适用 [§42](quota-resources.md#spec-42) 的额度策略，admission pacing 等待明确记为 quota_policy_wait，不改成无消息低频 Tick |
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

<!-- /migrated-contract: SPEC:41.5 -->

<a id="spec-57-1"></a>
<!-- migrated-contract: SPEC:57.1 -->
## 57.1 角色框架是可修订内容单元

**合同。** 主体角色文本不再由编译期常量独占。运行时按能力契约 `cap.subject-frame` 解析：安装中**恰好一个**启用单元提供该契约时，该单元当前版本的正文就是主体的角色前缀（`shenko-awareness-core/src/cognition.rs` 的 `capability_unit`；`shenko-subject/src/loop_runner.rs` 的 `subject_frame`）。没有提供者时用编译回退文本。两个**不同**单元同时提供时是歧义：harness 不替主体选，保持编译回退并把歧义报告给主体，由主体决定停用哪一个。停用的单元不是提供者。

**边界。** 只有角色文本可被替换；输出协议（控制行语法、回复语法、工具目录）仍是内核 ABI（`shenko-subject/src/lib.rs` 的 `subject_prompt_with_frame`）。换 frame 不等于换 kernel：想要不同的输出协议，那是另一个内核版本，不是另一段 frame。

**修订路径。** 主体写候选版本（`cognition.unit.write:1`）→ 沙箱对照试验（`cognition.unit.trial:1`）→ 采纳（`cognition.unit.judge:1`）→ 下一个有效 S0 生效。生效时 `profile_digest` 随之改变（`context_projection.rs:758-763`），context window 按既有契约重建；旧版本保留，不覆盖。

**为什么。** [§6](subject-runtime.md#spec-6) 说灵魂可自改，但主体连"自己是谁"的这句话都改不了；[§51.1](cognition-units.md#spec-51-1) 说内容不可变、态度带理由——frame 正是内容，harness 常量不是。

<!-- /migrated-contract: SPEC:57.1 -->

<a id="spec-57-2"></a>
<!-- migrated-contract: SPEC:57.2 -->
## 57.2 空闲是感知，不是提示

**合同。** 没有新外部输入时，主体收到的是一次观察，而不是运行时下达的命令。措辞由提供 `cap.idle-perception` 的单元给出（`subject.idle_perception`）；没有提供者时用编译回退句。观察的内容由主体自己决定写什么：距上次外部输入多久、未竟几件、正在等什么，都可以写进这则单元。

**不变的部分。** [§5.1](subject-runtime.md#spec-5-1) 的持续接续与 600 秒语义等待不变；等待态是注意力状态，与推理/排队时间正交；真实容量等待、OS 挂起、主动暂停分别记账。"空闲不是停止的理由"仍然成立，但那是主体读到的内容，不是 harness 写死的台词。

<!-- /migrated-contract: SPEC:57.2 -->

<a id="spec-57-3"></a>
<!-- migrated-contract: SPEC:57.3 -->
## 57.3 注意力是主体的政策

**合同。** 每步事件预算、空等时长、哪些来源暂不看，由主体的注意力政策决定（`cap.attention-policy`）。harness 只强制三条界：批次不超过自身上限（防无限批）、等待不短于一瞬（防空转）、等待不超过五秒（防卡住）。取值由主体选，不由 harness 定——实现上，提供该契约的单元正文是一段 JSON（{"max_events_per_step":…, "idle_wait_ms":…}），主体修订它即修订自己的注意力形状；正文是散文或缺失时，harness 用自己的界并如实说明。

**未处置的输入。** 主体没有处置的输入不再被 harness 记成 `Acknowledged`（"已处理"）：它被记为 **`Ignored` + 理由"主体未在本轮处置"**，也就是"主体没有对它做什么"这个事实（`loop_runner.rs` 的 `dispositions_for_events`）。输入仍被结算（主体看见过它，队列继续前进），但记录不冒充主体处理过它。

**`deferred` 是主体自己的选择。** 主体显式写出 `[dispose] {..., "disposition":"deferred"}` 时，那条输入**继续留在队列里**，在后续 S0 再次出现，直到主体处置它或注意力政策明确放过它。harness 观察到的"沉默"与主体的"稍后再说"是两件事，账本也分开记：`deferred` 在 Store 里不是一次结算（`event_disposition_commit` 明确排除它），`ignored` 才是。

**`[dnd]` 要真的生效。** 被暂停的来源不进入当前窗口；事实仍在账本里；`review_after` 到期自动复审并把复审结果交回主体。不做关键词过滤、不做静默丢弃。

<!-- /migrated-contract: SPEC:57.3 -->

<a id="spec-57-8"></a>
<!-- migrated-contract: SPEC:57.8 -->
## 57.8 三个死接口复活

**合同。** [§5.2](subject-runtime.md#spec-5-2) 能力表中的 dialogue、resource policy intent、operating policy intent 不得因为"没有安装派发器"而**静默**（`store/intents.rs:85-356`）。实现取法：`blocked` 这个状态保留它既有的含义（从未派发，所以副作用未知，context policy 会把它当作未知调用暴露出来），改变的是**理由与可见性**——理由说清"capability not installed：本机没有这个合同的派发器；主体可以装上能力再提，或另行处置"，而不是"explicit migration required"；同时新增 `unbound_subject_intents` 读取器，以及 S0 材料里的一段清单：主体提出过、却还没有对应能力的意图，在它下一步的材料里可见。

**主体可以提出。** 新增 `[policy] {"kind":"resource_share|update_policy|network_policy","description":"你想要什么"}` 控制行：主体对自身的运行政策（算力份额、自更新、联网）提出请求。该请求是耐久事实；本机若没有对应派发器，它以"能力未安装"进入上面那份清单，而不是一个看起来像拒绝的死条目。

**为什么。** 一个能力表里写着、实际永远不可用的接口，比没有这一行更糟：它让主体以为自己拥有对话与政策能力。

<!-- /migrated-contract: SPEC:57.8 -->

## 元感知接续与主流连续性

[默认元感知接续合同](../decisions/0006-meta-perception-continuation-2026-11-11.md)中的有限试用是主流可发起的工作，不把主主体变成有限任务。主体可调整观察频率与退避；持续思考、语义等待、真实容量等待和进程暂停仍按本领域各自的合同表达。最小单机闭环要求实际输入、有限 child、真实结果、输出或回执和重启接续彼此连接；可选看门狗不能代替这条原生轨迹，也不能据此关闭完整 S1–S5。
