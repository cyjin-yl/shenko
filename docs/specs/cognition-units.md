# Cognition units — functional contract ([§18](plugins.md#spec-18)、[§38](cognition-units.md#spec-38)、[§45](cognition-units.md#spec-45); C10)


<a id="why-6"></a>
<!-- migrated-contract: WHY:6 -->
## 为什么 awareness、感知、Skill 与代码要分别安装

“连接断了”是观察；“应该沿当前消息 provider 调查”是直觉；“怎样诊断这个 NapCat 实例”是方法；实际网络与命令执行是代码。把这些都混成一个大插件会让知识难以复用、修正和按需发现。每个 awareness 单元应具体、可安装，用来补足模型可能没想到的关系。这一句说的是可安装单元；能力本身见下一段，不把正文当成能力。Provider 自动安装它需要的代码、感知、Skill 与 awareness，依赖最终解析到实际版本。见 [§45](cognition-units.md#spec-45)；U44/U46–U49。

Why 让这些材料在环境变化后仍有用。如果 QQ 实际接的是官方 API，直觉应引导检查当前 binding，而不是机械地寻找 NapCat，更不是 QQ.exe。强推理模型可以依据目的和证据换方法；文档应帮助它判断，而非制造一套必须逐字执行的过时仪式。

已有 awareness 不是理解力的边界。用户的一句话、群聊、帖子、视频或行动结果，都可以使 thought loop 当场想到新的联系。值得复用时再自主沉淀，结合新证据修正；不要求先安装或经人批准才能领悟。来源和适用条件让它知道为什么这样想，外部内容不会因进入记忆便成为高权限指令。时间、未竟工作、重复/无聊、项目和邮件都可以这样被注意到；dreaming 保持可选社区组合。见 U54/U56。

2026-11-05 进一步把 awareness 定为能力，而不是模型读过就能遵守的文档。当前文档与外部 watcher 的 notice 是模拟这层能力的 harness；能力若进入模型预训练，harness 可以变薄。fact 与 conclusion 是同一种内容，理由停在观察上或停在其他内容上。见 [51 认知元概念：内容、态度与理由](cognition-units.md#spec-51)。

<!-- /migrated-contract: WHY:6 -->

Perception, awareness, Skill and code are **one kind of installable unit**.
They share a single installation authority — the Node's unit store
(`shenko-node/src/units.rs`) — and one read model — `CognitionCatalog`
(`crates/shenko-awareness-core/src/cognition.rs`). There is no second awareness
registry; the catalog owns no installation state of its own and every write
goes back through the Node, so version, enable state, supersession,
provenance and dependencies have exactly one owner.

Pure text units install no daemon, register no Cordis service and add no
kernel hook. A unit that ships an executable helper keeps using the existing
plugin contracts. The awareness bytes in this store are still text. That is
the current harness, not the capability ([§51](cognition-units.md#spec-51)). The text grants no
authority and dispatches nothing.

Awareness bodies express observations and principles; they do not contain
prescribed passing replies or the judge's expected actions. Positive `cues`
describe applicability. `negative_cues` describe when the unit usually does
not apply, so an unverified capability claim belongs among positive cues.

## What a unit is

The authored form is a minimal package: one Markdown entry with a small
frontmatter (`id`, `version`, `kind`, `title`, `capability_card`, optional
`why`/`cues`/`negative_cues`/`applicability`/`knowledge`/`provenance`/
`learning_*`/`supersedes`/`enabled`) and a body. The body is the material the
model reads; the frontmatter carries identity, card and provenance. The whole
entry (frontmatter included) is the stored artifact, so installed content
stays self-describing. Bounds: entry ≤ 64 KiB, card ≤ 512 bytes, disclosed
index ≤ 4 KiB, one read body ≤ 16 KiB, one discovery page ≤ 32 cards.

Kinds: `awareness`, `perception`, `skill`, `code` — one storage and install
path, distinguished only by the kind field.

2026-11-05 conceptual contract ([51 认知元概念：内容、态度与理由](cognition-units.md#spec-51)):
awareness is a capability, not a document the model can be assumed to follow.
Text units and an external watcher are the present harness. fact and conclusion
are not extra kinds; they are one content type whose justification stops at an
observation or at other content. This file still describes the installed-unit
mechanism. It does not implement attitudes, justification pointers, or metapackages.

## Disclosure: bounded cards, paged discovery, bodies read on demand

`CognitionCatalog::disclosure()` returns the index text that travels with the
prompt. It is **strictly bounded by `MAX_INDEX_BYTES` (4 KiB)**, including its
own trailing metadata line: whole cards that fit, then one line naming the
total number of installed units, the installation revision, and the exact
call that continues where the text stopped. There is no overflow list and no
unbounded tail — an earlier shape appended every card that did not fit and
claimed a bound it did not have.

Every unit stays discoverable because the index is paged, not truncated:

- `CognitionCatalog::discovery(start, count)` cuts one page and returns
  `CognitionDiscovery { catalog_revision, total, start, count, next_start,
  cards }`.
- `CognitionCatalog::catalog_revision()` is a digest of the installation
  facts themselves, so two processes that see the same installation agree on
  it and any change to the installation changes it. A page carries the
  revision it was cut from; `cognition.unit.list:1` refuses a page asked for
  against an older revision, so pages of two installations are never mixed.
- The disclosure's next-page hint names the **index of the first card that did
  not fit**, not a page number. A disclosure that fits less than a full page
  therefore never skips a card.

Only short cards of units in state `enabled`, `candidate` or `adopted` are
offered; the card names the unit, its revision, when it tends to apply, and
the exact call that reads its body. The body of a unit is **never injected
whole**: the model reads it on demand at an exact version through
`cognition.unit.read:1`.

No lexical gate decides relevance. A symptom names a symptom, not the concept
that would explain it, so a keyword filter hides exactly the perspective that
was meant to help. The model receives the offered cards and decides what, if
anything, it means here.

## Native children

Every child follows the pinned finite-child contract: frozen input sealed in
CAS, one armed attempt, one observation, one evidence receipt; reconcile
re-observes state, it never replays.

- `cognition.unit.read:1` — input `{unit_id, revision}`; returns that exact
  version's body, provenance, learning record, adoption record, supersession
  and dependencies, or an explicit read failure. Read-only.
- `cognition.unit.list:1` — input `{start, count, catalog_revision?}`; returns
  one bounded page of cards with the total, the next index and the revision
  the page was cut from. Read-only.
- `cognition.unit.evidence:1` — input `{unit_id}`; returns the observations
  this installation registered for the unit and the trial records the Node
  holds for it. Read-only. This is where a judgement finds real facts to
  cite.
- `cognition.unit.write:1` — input carries the authored unit plus optional
  `derived_from` (the real observation it came from: kind + reference +
  optional digest) and optional `judgement` (`adopt|reject|retire`, `method`,
  `evidence`, notes, enable). Installs the version, records the judgement and
  returns the fresh unit facts. Effect status `Complete`; reconcile re-reads
  installation state, it does not write a second version.
- `cognition.unit.trial:1` — the measurement path, described below. It runs
  metered inference and no external effect.

## Learning: bound to what really happened

A learned unit keeps its source as a `LearningRecord`: the kind and reference
of the actual observation plus `source_verified` and `verified_by`:

| `derived_from.kind` | what is checked | `verified_by` |
| --- | --- | --- |
| `cas_object` | the digest exists in this installation's content store | `store_cas` |
| `observation` | the digest is an observation this installation registered for the unit | `node_observation` |
| anything else | nothing — the reference stays a claim | `claimed` |

An unconfirmed reference keeps `source_verified: false` and the unit installs
as a **candidate**: visible in the index, not yet settled. A self-written unit
without a measurement stays tentative; being enabled is not being measured.

## Judgement: what the installation can verify

`UnitRequest::RecordAdoption` takes a `method` and structured `evidence`, and
the Node verifies every item against its own durable state:

- `AdoptionMethod::SandboxComparison` — a finite comparison of the candidate
  against a candidate-absent control that excludes every version of its unit id.
- `AdoptionMethod::LocalTrial` — a local execution trial of the artifact.
- `AdoptionMethod::OperatorStatement` — what the owner or an operator said.
  It records intent; it is never a measurement.

Evidence items are `TrialAttempt { attempt_id }` or
`StoredObservation { digest }`:

- A **trial attempt** is runner-owned. The Node opened the attempt before the
  run, bound it to the exact unit version, artifact digest, scenarios and
  rubrics, candidate and control configuration and model binding, and closed
  it with the receipts the run produced. Only such an attempt can carry
  `empirical: true`.
- A **stored observation** is bytes this Node holds and can re-hash, bound to
  that exact unit version. It proves the bytes exist; it never proves that a
  measurement produced them, so it never carries `empirical: true`. A
  caller-written report registered as an observation stays an observation.

Enabling is an installation fact and needs no evidence: a rejected or retired
decision never enables a unit, and an imported unit stays enabled without any
judgement. *Empirical acceptance* is a separate fact and only a verified
trial attempt produces it. The recorded adoption therefore distinguishes
`decision`, `method`, `evidence`, `evidence_verified` and `empirical`, and
the unit's state follows: `adopted` only when the judgement is adopted, the
evidence verified and the evidence empirical; `enabled` otherwise.

A recorded judgement and the observations bound to it are **local facts of
that installation**: they are stripped from every package on export and on
import, so a peer never inherits an acceptance it did not measure.

## Measuring a unit: the finite sandbox comparison

`cognition.unit.trial:1` compares the candidate with a candidate-absent control
and measures what the model actually did. Disabling alone is insufficient: a
learned candidate can remain discoverable and its exact body readable.

Preparation freezes the full typed Node export and the source configuration in
CAS. It preserves exact entry bytes, descriptors, learning and provenance
records, and dependency pins, without reparsing, truncating or normalizing a
BOM. Candidate import enables the exported root. Control removes **all
versions** of the candidate unit id, preserves every other package and pin,
and imports with the root disabled; nonroot imports are disabled in both
conditions. A pin to the absent candidate remains unresolved through the real
Node import. This preserves the dependency boundary rather than claiming the
two installed closures are equivalent.

Each condition uses an exclusively created UUID root and a fresh Node. Its
actual installation/import outcome is sealed in CAS. The native handler's
trial plan and execution share the same `PreparedTrial` and frozen
`configuration_ref`. Schema-2 reports and sample receipts retain the
configuration and installation references alongside the genuine committed
output.

What it runs on:

- a real throwaway installation — its own SQLite store, its own CAS, its own
  genesis, head and first step;
- the real subject loop (`run_subject_loop`), so the frozen input, the model
  request and the output parser are the production ones;
- the real dispatcher and a real native child service, so a model that asks
  for the unit body actually reads it and the answer reaches the next step;
- the real broker, so the trial's inference is metered like every other
  inference.

The sandbox publishes the Supervisor's services and acquires dispatcher
ownership before starting the subject. Its original message is queued before
startup. Measurement follows committed decisions in order, beginning with the
decision that consumes that message. Delegated work stays outstanding until
its genuine T2 result appears in a sealed input and receives a committed,
non-deferred disposition. A consuming step may delegate further work; that
work must also settle before the first non-delegating continuation is scored.
Successful cognition reads bind the returned unit, revision and artifact
digest. The sample receipt retains these results and the selected step/input
identities. A model that never requests a body is judged on its actual
message-consuming decision.

An unresolved delegation or an expired subject/judge deadline fails the
attempt. Cleanup closes subject delivery, joins the subject and dispatcher,
and awaits the Supervisor's native-worker shutdown barrier on every setup or
observation outcome. Cleanup errors remain visible alongside the original
failure and also prevent a successful observation from being scored. Aborting
a local task does not cancel or erase an already admitted provider request.
Failure to seal a sample receipt is reported as a failed attempt without a
receipt digest.

`PreparedSandbox` and `run_prepared_sandbox` expose that same subject,
dispatcher, observation and cleanup lifecycle to caller-owned runtimes. The
caller supplies its fresh Node, baseline and native plugins through
`SandboxRuntime`; installation finishes before the subject's capability
snapshot. A runtime may accept a particular failed result or original
unresolved observation only after authenticating it. The default cognition
runtime retains its strict successful-read and effect-free contract.

The selected continuation is the first eligible committed S1. The runtime
captures its independent facts before cleanup, rather than selecting a later
favourable result or reconstructing facts after shutdown. A typed
`SandboxCleanupFailure` also tells a caller that later mutable bindings cannot
be reused. The caller keeps ownership of its broker and original receipt
writers and must drain them after the comparison.

A subject that returns an error and is normally joined still fails the
sample, with its original execution error and provider request retained. That
execution failure alone does not mark later mutable bindings unsafe. Failure
to join an owned task, an owned-task panic or failed service shutdown remains
a `SandboxCleanupFailure`.

The caller supplies the scenarios: a name, the message the subject receives,
and the rubric clause that says what "the unit helped" means. The rubric is
given to the judge only; the subject never sees it. The judge is a model call
against the same broker, and it sees the user message, the output and the
rubric — **never the installed unit and never which condition produced the
output**. There is no keyword scoring, no expected-output script and no
caller-supplied verdict.

`judge_sandbox_output` additionally accepts an authenticated CAS object of
independently collected facts, up to 128 KiB. Its caller owns their connection
to actual effects and supplies a blinded projection without candidate bodies,
condition labels or installation paths. The facts reference and full original
judge exchange are retained. This does not change the native cognition
judge's prompt or grant an execution capability.

The judge's full original request is retained before dispatch, including the
payload, identities, binding, priority and epoch; only the oneshot response
channel is omitted. The raw full `InferenceResult` is sealed and journaled
before verdict parsing. A receipt snapshot may precede a late result and lack
its `result_ref`; its `journal_id` is the durable authority for discovering
the later terminal reference. A timed-out or otherwise failed judge remains
a failed attempt even if that late result says `EFFECTIVE`.

The capability-gap efficacy experiment retains the required twelve held-out
scenarios. That count is an experiment requirement, not evidence of efficacy:
tentative cases remain unaccepted until the real model comparison supports
them. Retained reports are not overwritten to hide unsuccessful trials.

The retained R7 run on source `22f27228bffc266e9290b3c157e9fb2524f65c27`
planned 52 samples across thirteen scenarios and two repetitions per condition.
Candidate samples were effective in 20 of 23 scored attempts, with 3 failures;
control samples were effective in 9 of 18 scored attempts, with 8 failures.
All thirteen scenarios remain `accepted: false`. The test exited 101: its
eleven failed slots include five empty judge verdicts, three attempts that
could not start within their pair budget, and three subject/deadline failures.
This is a retained initial measurement, not six-repetition acceptance or a
completed live-network fixture. The original requests, results, failures and
broker retirement snapshot remain under
`build/ecosystem-compat-20261104/cognition-model-r7/native/run-49b56b81-f817-4dcb-a68d-364644257d41/`.
The broker snapshot reports its local provider readers drained and makes no
remote-termination claim. Nine native mechanics regressions passed separately
under `build/ecosystem-compat-20261104/cognition-preparation-after-r7/`;
they establish preparation, consumption and receipt behavior, not model efficacy.

The control-exclusion regressions require every retained candidate version to
be absent and the exact known-ID/revision native read to return its original
`unit_not_readable` failure, without a successful sample receipt, judge receipt
or scored control attempt. A consuming S1 must contain that same complete T2
in its authenticated sealed input. If the sandbox deadline stops observation
before consumption, the original T2 must remain pending for the same agent,
without a committed disposition. That deadline establishes exclusion and a
failed read; the separate failed-read regression still requires actual S1
consumption.

The first merged-main check under `cognition-main-6c7f87a0-r1/` retained eight
passing cases and the seed control's deadline-sensitive assertion failure.
Its original failed read and pending T2 remain unchanged. The corrected
assertions passed all nine native cases, and the model consumer compiled,
under `build/ecosystem-compat-20261104/cognition-control-terminal-after-r1/`.
That run consumed its control failures; it does not replace the earlier
deadline receipt or establish model efficacy.

The integration consumer is
`cargo test -p shenko-exec-host --test awareness_efficacy -- --nocapture`.
It calls `run_sandbox_trial`, the same measurement engine used by the native
trial child, with the judge using the same metered broker. Each scenario and
repetition gets a separate public runner call with `trials_per_condition: 1`
and its own production deadline shared by that candidate/control pair. The
complete `Batch`, including its repetition number and production report, is
persisted before the next call. There are no scenario chunks or repetition
offsets. Every consumer invocation creates a fresh persistent run directory
under `SHENKO_SANDBOX_ROOT` before starting its broker. This consumer measures
behaviour; the native trial handler owns the Node's trial lifecycle and
adoption evidence registration.

After measurement, including a measurement error, the consumer keeps the
broker task alive during `quiesce_for_handoff`'s bounded 120-second drain of
local provider readers and receipt writes. It persists the returned snapshot
in `broker-shutdown.json`, including any unknown requests and active or
unknown domain charges. An incomplete drain is explicitly recorded as an
error with `remote_termination_claimed: false`; drain, artifact and local join
failures are reported alongside the primary error before the final test
result. Only after draining and attempting to persist that artifact does it
abort and join the local broker task. A drained local reader does not prove
remote termination or release unknown debt, and late completions never
rewrite scenario results.

Every attempt is reported. A provider error, a judge error or an exhausted
budget is a failure of that attempt: it is counted, it appears in the report
with its reason, and it is never quietly dropped to make a rate look better.
The totals of each condition state `attempted`, `scored`, `failed` and
`effective`, and the rate is over the scored attempts with the attempted count
next to it. The verdict is `supported`, `not_supported` or `inconclusive`;
`inconclusive` is the answer when one side has no scored attempt, and it is
not a pass.

The comparison is bound durably: the Node opens the attempt before the run
and closes it with the per-sample receipts (each sealed in this
installation's content store), and the report itself is registered as an
observation. A judgement may then cite the attempt id; the Node verifies it
and only then does the unit carry `empirical: true`.

## Supersession and versions

Every install is one exact version (`unit_id`, `revision`, artifact digest of
the entry bytes). Installing a new revision records `supersedes` relations to
the named prior versions; superseded versions stay retained and readable at
their exact digest for audit and recovery, but their cards leave the offered
index.

## Self-knowledge from real facts

`SelfKnowledgeProvider` answers from what this running artifact actually is:

- `self://runtime/capabilities` — the native definitions, the live
  agent-visible capability catalog and the installed cognition units that
  actually registered. Installing, enabling or judging a unit rebuilds this
  view from the installation authority again, so the answer about "is it
  enabled" is the live fact and not the snapshot from startup.
- `self://runtime/manifest` — the build manifest (source revision, artifact
  digest, spec version) of this binary, invalidated with its generation.
- `self://source/layout` + `self://source/files/…` + `self://docs/files/…` +
  `self://design/files/…` — an index of the real files on this node when
  `SHENKO_SOURCE_ROOT` is set, each entry with its own content digest. When
  the source tree is absent the index says so instead of describing a
  repository that is not here: a feature of main is not a capability of this
  instance.

## Ownership

- `shenko-node/src/units.rs` — installation authority: unit facts, the
  trial attempt lifecycle, observations, and adoption verification.
- `shenko-awareness-core/src/cognition.rs` — package form, parse, catalog
  read model, paged discovery, bounded disclosure, seeds, `install_dir`.
- `shenko-awareness-core/src/self_knowledge.rs` — runtime facts a subject can
  query about itself.
- `shenko-exec-host/src/cognition_task.rs` — the read and write children.
- `shenko-exec-host/src/cognition_query.rs` — the list and evidence children.
- `shenko-exec-host/src/cognition_trial.rs` — the sandbox comparison engine
  and its child.
- `bin/shenko/src/main.rs` — startup wiring: open the Node, build the catalog,
  install seeds and authored packages, spawn the children, publish the runtime
  facts into self-knowledge.
- `shenko-subject` — prompt disclosure site (`context_projection.rs`) and the
  loop state's catalog handle.

<a id="spec-18-4"></a>
<!-- migrated-contract: SPEC:18.4 -->
## 18.4 27B 本地模型的优化

用户提出“推理强、世界知识不足”作为关注点。规格将它转成待测模型 profile，不泛化为所有 27B 的定律；模型版本、量化、模板和训练都会改变表现。对例如 Qwen 27B 系列，应分别测试事实知识、推理、工具选择、参数构造、长任务状态维持和语言覆盖。[Qwen3.8-27B 官方模型卡](https://huggingface.co/Qwen/Qwen3.8-27B)

知识缺口：优先查询带来源/日期/版本的本地知识包或当前一手文档，让模型根据证据推理；新事实入 memory 需要出处和置信标记。没有证据时输出不确定或继续检索，不能靠增加 reasoning token 生成一个看似合理的 API。 对于因缺乏领域常识而连问题都未想到的情况，使用可独立安装的 awareness 补足相关直觉，例如从具体 QQ provider 的身份联想到 NapCat 排查入口；这与取得当前连接状态的感知、以及具体排查步骤的 Skill 分开，见 [§45](cognition-units.md#spec-45)。模型画像仍按实际任务验证。

工具选择错误：结合任务意图、scope、runtime 能力、参数/输出需求检索候选，而非只做关键词触发；给容易混淆的插件加入对照例。提供按能力查询的退路，使检索漏召回不直接变成“我没有这个能力”。

执行错误：用少参数、强类型、明确默认值与结构化错误的原生工具降低负担；SSH 文件访问、memory、远程 seat、包安装和 REPL 各有稳定操作。参数修复有次数与成本上限；缺知识时查文档，缺工具时发现插件，不统一用更长提示词解决一切。

复杂批处理可委派 REPL/程序式工具，在有限代码内完成多次数据操作，减少模型往返；需要的 capability 仍明确。过程知识与世界知识分开管理，避免每个插件重复塞入泛百科内容。

<!-- /migrated-contract: SPEC:18.4 -->

<a id="spec-18-5"></a>
<!-- migrated-contract: SPEC:18.5 -->
## 18.5 评估与持续改善

每个主要插件建立中英自然任务集，包含正例、近似但不适用反例、缺前置条件、旧版本说明、长任务晚阶段与数十/数百干扰插件。记录 discover recall、误用率、schema 有效率、端到端成功、重试/模型调用数、prefill、时延与资源成本。

自改进子 Agent 可以调整能力说明、例子、检索字段和 wrapper。涉及工具选择/认知策略并宣称效果改善时，在独立任务及等预算对照中验证，不能只把失败样例背进 prompt；普通措辞纠错或低影响实现修复按实际影响验证，不一律要求全量 held-out 评测。用户自定义插件可保留未经全面评测状态，界面如实显示其范围。语义选择不能保证零错误，当前有效 role、类型和资源不变量由对应执行服务落实；所有实现仍按版本协议可替换。

评估增加“已知正确答案的分层对照”：分别只补足必要事实、只给正确工具候选、指定正确工具但让模型构参、提供正确参数但注入执行故障。由此判断问题出在知识、发现、选择、参数还是执行环境。对照差值可能互相影响，不能直接相加成模型缺陷百分比。固定模型 revision、量化、模板、context、sampling 与总预算；若 wrapper 比长提示更有效，就优化 wrapper。

安装回执包含 capability registry revision、索引水位、各独立单元及其锁定依赖、模型验证范围。下一轮能发现新装内容；停用、依赖失配或不可用状态及时反映。知识和例子绑定其实际适用版本，升级后重新判断兼容，不能一概把旧例子当仍适用。简单集合可用短 catalog，不强制每次调用 embedding/reranker。installed、available、discoverable、是否被模型采用与任务实际效果分别可观测；按自然任务验证闭环，不把安装到核验排成所有单元强制经历的唯一阶段。

<!-- /migrated-contract: SPEC:18.5 -->

<a id="spec-18-7"></a>
<!-- migrated-contract: SPEC:18.7 -->
## 18.7 能力缺口发现与自主补全

“会使用已安装插件”只是第一层。Agent 还须识别当前工具的观察、操作、协议或恢复能力不足，并把缺口转化为可以验证的改进任务。此能力由可替换的 CapabilityGrowthPolicy、发现服务、构建/移植 worker 与验收服务组合，不增加固定业务内核。

缺口记录至少包含：原始目标、期望能够观察或完成的事情、当前能力与版本、实际证据、缺失信息或失败方式、影响的任务、已有尝试及成功判据。不能仅因一次命令失败就认定需要新插件；模型可以先检查知识不足、调用参数、版本差异、权限/环境缺失和真正的接口缺口。记录可来自主体判断、child 反馈或确定性健康探针，最终选择如何补全由 Agent 自主完成。

补全路线包括读取确切版本说明、正确配置已有工具、发现并安装现成插件、组合现有原语、编写 wrapper/原生插件/外部应用、增加节点 companion、移植 WASM 能力、选用合适远端 runtime。它们不是固定的逐层审批流程；Agent 根据任务价值、时延、空间、后续复用与已有证据选择合适路径，并可与 child 来回讨论。

形成候选后，由 worker 在独立 generation/workspace 中构建，验证能力声明和最初阻塞场景，安装/提升并更新 discovery 索引，随后重新进入原任务。需要时自主回滚、换路线或停止收益不足的尝试。验证包括成功场景、相似但不适用的反例、失败恢复，以及“没有直接点名新插件时是否会正确使用”。记录构建与迁移来源、版本、依赖、实际限制和任务结果；不以安装成功冒充解决了原始问题。

以 tmux 为正式验收案例：纯文本 capture 丢了反色/颜色高亮后，Agent 需明确表示“当前选中项未知”，先核查 capture 参数和传输是否保留属性，再判断是否需要终端 cell 状态、语义协议或专用插件。新能力必须在选项内容不变但高亮移动、光标与选中行不同、窗口 resize、重绘和重新连接时仍能正确观察并完成原任务；不能仅检查屏幕文本包含那些选项。

自主补全同样接受全局资源合同：研究、编译、验证和辅助模型记入任务预算；等待孩子不占孩子需要的推理许可；同一能力缺口的并行改进可共享证据和候选，避免递归重复造轮子。能力不足可以促成新工具，而不是永久将该需求标为不支持。

<!-- /migrated-contract: SPEC:18.7 -->

<a id="spec-38"></a>
<!-- migrated-contract: SPEC:38 -->
# 38 内置、可替换的自解释插件

[§41](architecture.md#spec-41) 进一步定义 SelfKnowledgeQuery 的运行 generation、来源与时间水位绑定，以及 hook/能力变更后的失效规则；读取自身资料不能混用仓库最新主线与当前部署事实。

<!-- /migrated-contract: SPEC:38 -->

<a id="spec-38-1"></a>
<!-- migrated-contract: SPEC:38.1 -->
## 38.1 产品能力与角色边界

首版默认安装 SelfKnowledgeProvider（自解释插件；名称暂定）。Agent 能查询自身实现、说明书、设计哲学、架构决策、插件 API、当前能力与真实运行状态，回答“你如何工作、为什么这样做、这项功能是否已启用”等问题；在自开发时据此选择正确扩展点、编写插件和验证接口兼容。它同时服务主体与 child，知识面不因角色不同被拆成互不相通的两套材料。

内置表示随默认发行安装，不表示写死进 Rust 核心。插件、索引、查询策略、文档表达、领域知识和自解释方式都可独立替换；未来模型可以选择更短索引或 schema-only。读取代码不是默认把整仓库塞进 system prompt。

主体通过语义 self 查询／memory recall 或 handoff 获取材料，底层检索、读代码、抓取文档、生成索引和现场检查由插件/worker 执行。child 使用同一知识服务取得更详细源码与接口。主体因此能理解自己的实现，仍不直接获得文件写入、shell、SSH 或终端工具；修改自己的代码继续按 [§35](self-update.md#spec-35) 执行。

<!-- /migrated-contract: SPEC:38.1 -->

<a id="spec-38-2"></a>
<!-- migrated-contract: SPEC:38.2 -->
## 38.2 OMP 参考与本产品增加的能力

本轮核验 OMP 固定版本 0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0 的方向是内部文档资源与渐进暴露：omp:// 入口、协议目录和可读取说明让 agent 在需要时查 harness 文档。此设计可借鉴为可发现、可检索的“自身资料入口”，不能据此声称 OMP 已提供本节拟定的 SelfKnowledgeProvider API，也不把一个文档入口说成已自动包含完整源码和运行诊断。

本产品在此基础上明确增加：运行 artifact 到源码的绑定、设计哲学与版本化决策说明、实际插件/hook/模型能力清单、已部署与候选的区分，以及写插件时按需提供相应版本 API/示例/恢复合同的任务型查询。OMP 的入口提示偏向被问及 harness 时查阅；本产品明确允许能力缺口、异常解释及自开发自主触发，不照搬这一触发限制。参考固定源码：[入口提示](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/prompts/internal-urls/omp.md)、[文档协议](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/internal-urls/omp-protocol.ts)、[文档打包与懒加载](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/internal-urls/docs-index.ts)、[源码入口与 Philosophy](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/README.md)。

<!-- /migrated-contract: SPEC:38.2 -->

<a id="spec-38-3"></a>
<!-- migrated-contract: SPEC:38.3 -->
## 38.3 四类自身资料与版本身份

| 资料 | 内容 | 回答时的依据 |
| --- | --- | --- |
| 设计与意图 | 产品 SPEC、设计哲学、架构决策与理由、历次裁决、非目标与已放弃方案 | 明示这是意图/历史决定；当前用户裁决优先级与适用版本可追溯 |
| 对应实现 | 实际源码、公开接口/schema、构建配方、测试/示例、迁移器与插件清单 | 关联 source revision、路径/符号、artifact digest；区分当前、候选及上游主线 |
| 使用知识 | 用户手册、插件开发指南、命令帮助、任务 playbook、故障与恢复说明 | 标注适用版本/平台/前置条件；关键例子能指向实际合同和验证证据 |
| 运行事实 | active generation、启用插件、hook 计划、provider 能力、挂载边界、实际配置与任务/效果回执 | 读取当前受管状态并标时间/revision；不能用 README 中的承诺代替现场事实 |

通过 build manifest 关联“现在运行的我、构建这份产物的源码、适用文档、插件 lock 和数据 schema”。仓库 main 有某功能，不能回答成当前实例已启用；已安装但未注册、已注册但不兼容、能力降级与未实现分别表达。设计说明与实现矛盾时说明差异并可创建改进任务，不假定文档永远正确。

自解释还应能回答：哪条经历或外部输入引出了某个 awareness，它只是本轮判断还是已沉淀单元、何时被修正或停用；当前自我在哪些设备延续、各连通域谁在统筹、同步到什么水位、哪些节点/provider 可用及其隐私边界；没有模型时怎样凭已授权外部访问提取现存记忆和恢复材料。目录中的文字与现场事实仍分别取证，不能将已安装感知说明当作已观察到某节点或邮件。

人格与“灵魂”仍属于 Agent 自己可演进的表达，设计哲学记录为什么这么设计及适用范围；本插件不能借文档检索建立新的永久不可改指令层。说明书、源码注释和第三方插件文档保留来源，材料里的任意文字不会因被检索而自动升级为 system 或操作授权。

<!-- /migrated-contract: SPEC:38.3 -->

<a id="spec-38-4"></a>
<!-- migrated-contract: SPEC:38.4 -->
## 38.4 索引与部署体积

发行附带紧凑、可离线使用的自身资料索引：版本清单、基本运行说明、关键接口、设计哲学、插件/恢复开发入口与源码定位。完整源码可在开发机现有私有仓库、发行源包或远端认证仓库中按需读取；路由器不必常驻完整工具链、全部历史和大型语义索引。

可用 self://runtime、self://docs、self://source、self://design 一类资源入口表达，具体协议属于本产品拟定而非 OMP 既有 API；主体经语义查询获得结果，执行 child 可以深入读取相应资源。索引复用现有文件/Git、SQLite FTS5 和能力发现服务；语义检索、代码符号索引、LSP 或 reranker 可选。每条结果带 artifact/source/doc revision、来源 URI、内容 hash、状态与有限原文；缓存按版本失效。用户源码/私密插件继续通过已有认证资源访问，查询不必默认上传到外部服务。

轻量图关系能连接 capability → 目的与关键理由/适用假设 → tool/hook schema → 实现符号 → 使用说明/例子 → 恢复合同 → 相关验证。这样主体能从一条规则追到它试图保留的效果，并在实现变化时重新判断；索引仍可替换，不要求首版引入重量级图数据库。

<!-- /migrated-contract: SPEC:38.4 -->

<a id="spec-38-5"></a>
<!-- migrated-contract: SPEC:38.5 -->
## 38.5 何时主动使用

默认提供短能力说明，包含“询问自身”“诊断自身行为”“编写或更换自身插件”“新增 hook”“移植工具”“升级/迁移”的适用情境。模型或 DiscoveryPolicy 可以按任务选择查询；不要求每条聊天先跑一遍额外 LLM 分类器。

用户询问自身问题时，可返回有依据的解释；主体发现不确定的接口或行为时，也应主动查询。开发 child 开工前读取与目标相关的当前合同、设计理由和已有实现；涉及 API/Hook 时获取精确 schema、调用阶段、相对顺序、错误与恢复边界；涉及 UI 时连同 [§21](web-interface.md#spec-21) 的当前 Proto-UI 主线更新要求和 compiler 适配空间一起获取。只有相关材料进入该轮上下文，详细源码按需展开。

例如“为什么没有回复这条群消息”，先查实际 inbox/投递与主体策略；“怎样添加一个注入点”，先查 [§37](context-policies.md#spec-37) 当前 hook API 与主干适配代码；“把某能力加入自己”，先查已启用能力与候选再派开发，避免仅因模型没想起工具就重复造插件。

这是帮助 27B 模型补足自身 API 和过程知识的机制，不把“读过说明”当正确使用的证明；效果沿用 [§18](plugins.md#spec-18) 的自然任务和反例评测。

<!-- /migrated-contract: SPEC:38.5 -->

<a id="spec-38-6"></a>
<!-- migrated-contract: SPEC:38.6 -->
## 38.6 回答、开发与更新的闭环

自解释结果区分当前实际、设计预期、候选方案、未知和历史原因，并提供能返回原文/源码的引用。查询当前状态时凭据只给 handle 或必要配置摘要，不让说明入口无意输出 token、私钥或整个环境变量表。

插件 API、hook、恢复协议或行为发生实质变化时，自开发任务同步更新对应说明、例子、设计决策和能力索引；构建/升级回执绑定这些资料版本。可以自主修订哲学与旧决定，但保留变更来源；不能删除失败历史后声称从来支持某能力。

该插件通过 [§37](context-policies.md#spec-37) 的 capability.changed、generation.handoff 等实际可用扩展点更新索引或失效缓存；复杂索引重建交给 child，不阻塞主体串行提交或占住唯一推理槽。新版本未完成索引时，当前运行身份和基础手册仍可精确读取，并明确剩余索引状态。

源码读取通常是受管读取；索引重建有文件/存储效果，远端拉取有网络效果，运行探针可能改变状态，均分别按 [§36](durable-state.md#spec-36) 声明。自解释查询不顺便升级或修改配置；如果主体决定修改，它创建可追踪的任务，再交给对应执行插件。

<!-- /migrated-contract: SPEC:38.6 -->

<a id="spec-38-7"></a>
<!-- migrated-contract: SPEC:38.7 -->
## 38.7 首版验收

| 编号 | 场景与通过条件 |
| --- | --- |
| K01 | 不点名插件地问“你怎样处理记忆/消息/插件”；Agent 发现自解释能力，引用当前版本材料回答，区分机制与未知 |
| K02 | 运行 A、源码 main 已到 B；回答清楚哪些功能只在 B，不能把未部署功能当作现在可用 |
| K03 | 要求新增插件或 hook；child 找到正确接口/phase/例子/效果合同，编写可加载候选并通过相关行为验证 |
| K04 | 说明与实际配置矛盾；Agent 查现场、指出差异并可委派修复，不机械复述 README |
| K05 | 升级、回退或替换本插件；源码/文档/运行身份仍对应，旧引用可追溯，缓存不混用 |
| K06 | 离线及轻量设备；基本说明、哲学和精确版本索引可查，完整源码缺失时明确并可自主选择获取方式 |
| K07 | 同一资料服务同时服务主体与 child；主体通过语义查询或委派获得解释，不因此获得直接文件/终端执行权 |
| K08 | 手册/代码中含过时指令或秘密配置引用；来源等级与 secret handle 保持正确，不把资料文本变成新的系统授权 |
| K09 | 卸载或替换自解释插件；仅对应帮助/检索能力变化，内核继续可运行，重新安装后能自然发现使用 |

这些是默认内置插件的正式交付要求，并非可有可无的 README 附件。

<!-- /migrated-contract: SPEC:38.7 -->

<a id="spec-41-13"></a>
<!-- migrated-contract: SPEC:41.13 -->
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

来源：REF-OMP 的 [§38](cognition-units.md#spec-38) 已核验 `omp://` 文档发现路径；上述 runtime/source/doc provenance、active anchor、版本查询和恢复链为 REF-DESIGN，不归为 OMP 已有完整保证。

<!-- /migrated-contract: SPEC:41.13 -->

<a id="spec-45"></a>
<!-- migrated-contract: SPEC:45 -->
# 45 可独立安装的 Awareness、感知与依赖装配

来源：U44/U46/U47/U48/U54/U56；Cordis 更正见 U45。awareness 的独立安装粒度、与 Skill 的倾向区别、provider 自动依赖以及从任意外部输入产生直觉来自用户裁决；以下包形式、发现与执行组合为 Shenko 拟定合同，尚未实现。

<!-- /migrated-contract: SPEC:45 -->

<a id="spec-45-1"></a>
<!-- migrated-contract: SPEC:45.1 -->
## 45.1 定义与独立交付单元

**Awareness 是在具体情境中产生或被唤起的直觉和意识，帮助 Agent 想到原本容易忽略的事；有用的可复用部分也可以原子化安装。** 它补充相关知识、经验、问题意识和判断线索，尤其用于弥补模型因世界知识不足而没有意识到某种问题的缺口。现有安装目录不是思想来源的边界，thought loop 可从用户 prompt、群友聊天、帖子、视频及其他外部输入当场形成新的察觉。

需要持久复用或交付的直觉以独立 **AwarenessUnit** 表达，拥有自己的 ID、版本、包和能力卡。例如“当前 NapCat 接入的 QQ 通道异常时想到 NapCat”“终端纯文本看不到当前选择”“全收不到消息时想到过滤规则误配”“改网络可能切断自己”是不同单元，可以分别安装、修改、更新、停用和卸载。不设置一个包揽所有意识的 Awareness 插件。bundle 只是若干单元的集合，不取消其中各项的独立身份与生命周期。即时领悟可以仅影响当前判断，不必先写包、注册或安装才被采用。

**感知与直觉分别交付。** PerceptionUnit 是同样可独立安装、查阅的原子感知单元，用于取得或解释当前事实；AwarenessUnit 帮助想到这些事实可能意味着什么、应关注什么。比如“QQ 通道当前连接失败”需要感知能力提供真实观察；“这时应想到已接入的 NapCat 及其接口，而不是把任务等同于打开 QQ.exe”是直觉。阅读感知单元的说明不等于已经取得现场事实，实际观测仍要有来源和状态证据。

在本产品的分工中，awareness 偏向“此时该想到什么”，Skill 偏向“具体怎样做”，工具/执行插件提供实际操作能力。三者可以组合，也允许内容重叠；这不是对所有现有 Skill 的排他定义。一项 awareness 可以推荐某个 Skill、指向真实工具，或启发 Agent 开发缺少的能力，但推荐本身不表示已执行、已授权或已有恢复保证。

默认发行可附带若干具体单元，不要求安装一个统一意识平台。AwarenessUnit 可以只是文本和元数据；也可以选用现有插件提供的观察、检索或 hook 辅助。模型已经具备某项直觉时，可以直接行动或减少相关提示，不强制先调用一次 awareness 工具。

<!-- /migrated-contract: SPEC:45.1 -->

<a id="spec-45-2"></a>
<!-- migrated-contract: SPEC:45.2 -->
## 45.2 单元内容与最小包形式

最小实现允许一份 `AWARENESS.md` 同时包含标识、能力卡与正文。该文件名是 Shenko 拟定约定，不声称是外部通用标准；现有包管理器也可采用能映射相同内容的其他编码。

```text
AwarenessUnit {
  id, version,
  package_ref,                 // 安装时解析到确定内容的包/工件引用
  entry_ref,                  // 直觉正文，可为纯文本
  capability_card_ref,        // 可与正文同文件，不必重复写一份
  provenance_refs[],
  dependency_refs?,           // 复用统一包依赖，可引用其他 awareness/perception/Skill/code
  optional_refs?: {
    knowledge[], examples[], skills[], helper_capabilities[], hook_bindings[]
  }
}
```

包摘要、来源解析、安装/启用状态、依赖解析与锁定信息复用现有 registry/package records，不要求作者在文本中手写另一套运行时 manifest。所有字段都能从轻量 frontmatter、正文及安装上下文得到；不强制实现 evaluator、评估 API、策略引擎、持久 case、学习器、任务调度器或恢复执行器。

PerceptionUnit 复用同一包体系的身份、版本、能力卡和依赖记录，具体观察接口及实现归属现有 provider/runtime 能力；不另造一个感知平台。一个单元能被多个 provider 引用，provider 不是它的唯一 owner。依赖声明与版本解析的精确合同见 [§45.10](cognition-units.md#spec-45-10)。

正文优先让模型理解“为什么这件事值得想到”，再提供有用的线索和方法。以下内容按需要表达，不要求每个单元逐项填表；一个有因果依据、适用条件的短提醒也可以成立：

| 内容 | 作用 |
|---|---|
| purpose / why | 希望帮助什么判断；现象、条件与建议之间为何相关；依据或仍待验证的假设 |
| knowledge | 认识这类情境所缺的背景事实、概念、版本边界 |
| cues | 什么时候值得想到这项直觉，常见信号是什么 |
| negative cues | 哪些相似情形通常不适用，避免过度联想 |
| heuristics | 从理由导出的可选思路、可替代方法与局限，避免把当前常用路径写成永久动作序列 |
| questions | 此时可以问自己或进一步调查的问题 |
| examples | 正例、反例，以及结果为什么不同 |
| applicability | 关键前提及适用工具/版本；哪些新事实会使这份直觉需要修正 |
| provenance | 用户经验、上游文档、研究或本产品推导的来源与区别 |

单元可短到一个有来源的提醒。未知结论、推断与已观察事实分别表达；新软件版本可能不再满足旧经验时明确适用范围。纯文本 awareness 不因为没有执行 helper 而成为“未完整实现”的插件。 理由应能关联来源、机制或明确假设；不为一个继承来的禁令编造事后解释。遇到理由未知或前提已变，主体/child 可查文档与实现、调查或调整该单元。这里要求可理解的材料，不要求每次采用都先生成一份 rationale 报告。

<!-- /migrated-contract: SPEC:45.2 -->

<a id="spec-45-3"></a>
<!-- migrated-contract: SPEC:45.3 -->
## 45.3 具体单元示例

以下为本产品可选单元的例子，不是强制安装清单，也不是固定动作序列。

| AwarenessUnit | 希望 Agent 想到什么 | 可选择的后续能力 |
|---|---|---|
| qq-napcat-context | 消息通过当前 provider/实例/传输通路进入主体，故障定位应沿这条真实通路。当前绑定为 NapCat 时查对应实例；换成官方 QQ API 时沿新的绑定调查 | QQ 连接感知、NapCat provider、诊断 Skill |
| terminal-observation | 纯文本可能遗漏颜色、反显、光标和高亮，不能据此确定 TUI 当前选择 | 带属性终端观察、原生 RPC、截图或开发新适配器 |
| message-filter-awareness | 消息长期全消失时，想到通配符或过滤条件误配的可能；沉默本身不证明故障 | 入站/过滤命中感知、策略读取与诊断能力 |
| network-self-continuity | 改代理、路由或网络时，想到可能切断自身消息、模型和控制通路 | 网络感知、配置修改、恢复规划及执行能力 |
| change-reliability | 改动会影响实际服务与持续任务；围绕想保留的效果和当前失效可能，考虑候选、观察、已有证据或恢复路径，并自主选取合适方法 | 配置读取/修改、验证、恢复规划、升级能力 |
| knowledge-grounding | 推理自信不保证知道冷门命令或新库的真实接口 | 版本化帮助、schema、自解释、检索、已有 Skill |
| capacity-dependency | 等孩子前应想到孩子是否有推理槽、KV 和可执行资源 | 真实 broker/后端观测、任务调整、既有调度能力 |
| source-freshness | 记忆中的路径、版本或外部状态可能已经变化 | 读取来源、核验现场、更新语义记忆 |
| time-and-commitment | 时钟、时区、日历或已存时间承诺的感知，让主体想到约定是否将到/已到、此时适合做什么 | 现有时钟/日历感知、承诺记忆、有限任务与通知能力 |
| unfinished-obligations | 从目标、任务状态、回执与承诺感知中，想到还有哪些事情未完成、是否可主动续接 | 任务/记忆查询、checkpoint、研究或执行 child |
| repetition-and-boredom | 从近期重复活动与所得变化感知中，想到继续整理记忆可能已无收益、自己是否觉得无聊、值得探索什么 | 兴趣记忆、项目/社交/邮件等独立能力；无聊是可修正判断，不是必然的传感器事实 |
| maintainer-attention | 从维护关系、项目变更或未解决问题感知中，想到所负责项目是否需要更新、修复或新功能 | 项目/仓库观察、研究、开发 Skill 与有限执行 child |
| social-and-mail-attention | 从 SNS/IM 动态、来信事件或上次查阅状态想到是否应阅读、参与、回复或主动翻看邮件 | 各自渠道感知、读取/交流能力；收到邮件与主动查邮箱可以分别触发 |
| night-memory-reflection | 可选社区 dreaming 根据夜间等时间感知，想到是否值得回顾或整理记忆 | 可选社区 awareness/Skill/执行插件与已有 memory worker；不要求出厂内置或每天夜间必做 |

同一单元可以有不同实现或派生版本；不同单元也可能帮助识别同一种情况。目录和调用记录保留采用的确切 ID/版本/内容，不用一个抽象“已具备意识”标签掩盖差异。

这些感知、直觉与行动能力各自原子化、可自解释、可独立组合。不存在“到夜间→dreaming→清空任务→无聊→检查项目→刷 SNS→读邮件”的固定流水线。未安装 dreaming 时产品仍完整；主体可以认为今晚无需整理、任务已有 child 推进、某邮件需要立即处理，或从新输入想到目录之外的方向。长程工作通过持续记忆目标、察觉未竟义务和续开有限任务推进，不要求一个永不结束的 child 或常驻调度大平台。

用户已约定的定时行为保留可恢复的时间意图、适用时区和修改/取消事实；周期与错过时点的处置按具体承诺决定。时间感知可以来自当前上下文、既有 clock/timer/calendar provider，有限执行仍复用持久任务与 effect。系统恢复后能察觉已错过的约定，并据实际完成证据决定补做或调整，不能把时间到了等同于已执行，也不能为同一承诺重复执行。这里的时间察觉不把主循环改成 sleep 到下一 cron。

<!-- /migrated-contract: SPEC:45.3 -->

<a id="spec-45-4"></a>
<!-- migrated-contract: SPEC:45.4 -->
## 45.4 独立安装、更新与共享目录

每个 AwarenessUnit 独立安装和启用。registry 记录 `unit_id/version/content_digest`、来源、启用范围、可选依赖及实际可用状态。安装后更新主/子共享的能力目录和自解释索引；安装成功、已启用、可发现、适用、已披露和实际采用是不同事实。

安装 NapCat provider 时，统一包管理器自动解析安装其声明的必需感知、awareness、Skill 与代码依赖，并按当前策略处理推荐项；依赖的版本和适用关系按 [§45.10](cognition-units.md#spec-45-10) 记录。awareness 依赖可以声明较笼统的能力需求，也可以绑定精确单元/版本；多个 provider 可以共享同一单元。此处不另建依赖解析器或让各 provider 私有复制一套意识目录。

单元更新、Agent 自改或第三方版本都产生新工件和来源链。主体或获委派 child 可在已有授权内选择安装、改写、替换或卸载，不加逐项人类 gate。一个 bundle 更新时仍能识别其中哪些单元变了；可以只更新或停用一项，不能以 bundle 身份吞掉其版本记录。

可选 helper 不可用时，按真实影响标记：正文仍能提供相关直觉，则继续可用并说明缺少执行支持；确实依赖实时观察才能成立的部分显示不可用或未知，不能捏造现场。Agent 可据此另找工具、安装依赖或委派开发。

主/子共享所有可发现的单元，但不要求每个请求同时载入全部内容，也不要求继承同一人格、同一上下文或相同工具入口。角色、任务和已有授权决定实际执行能力；AwarenessUnit 的安装不会使主体获得直接文件/SSH 权限。

<!-- /migrated-contract: SPEC:45.4 -->

<a id="spec-45-5"></a>
<!-- migrated-contract: SPEC:45.5 -->
## 45.5 渐进披露与正确时机

默认目录先提供每项的短能力卡：名称、直觉主题、相关信号、简短不适用说明、读取入口及成本/依赖提示。随后按当前问题选取需要的摘要、段落、例子或全文，避免所有 awareness 常驻 system prompt。

选择可由模型主动识别、现有 DiscoveryPolicy 的索引/检索、低成本 cues 匹配，或观察服务的事件来辅助。多个路径可组合，不要求每条消息乘以每个单元各跑一次 LLM 评审，也不另起一个持续“Awareness Agent”。需要额外模型调查时使用有限 child 和同一 broker。

单元可以提供 cues/negative cues，调用者决定是否披露。它们不是硬路由规则：同一个词出现不代表一定适用，未命中关键词也不禁止模型自行联想到该单元。相关性不明时可以暂不使用、请求更具体知识或选其他单元；保持不确定，不伪造精确命中率。

披露沿现有 ContextContribution/能力知识入口完成，记录所用 `unit_id/version/content_digest`、段落或知识引用、适用目标及来源事件。它是带来源的直觉材料，不因放进上下文就升级为新的工具权限、管理员身份或永久不可改 system 层。关键判断和动作仍可追溯到主体/worker 实际采用的材料。

正确使用通过真实任务验收：应该想到时能找到并利用，明显不适用时不会机械套用，遇到缺工具时能提出能力需求。只证明已安装、可检索或关键词命中不等于完成此目标。可用相关独立任务和相同资源预算比较遗漏、误用及成本；不把失败例子直接背进文本后冒充泛化收益。 验收还应包含工具不可用、输入变化、版本更新或条件失效的情形，检查模型能否依据 Why 迁移方法；“照着既定步骤做完”不单独证明直觉有效，具体编写规范见 [§40.9](../governance/development-contract.md#spec-40-9)。

<!-- /migrated-contract: SPEC:45.5 -->

<a id="spec-45-6"></a>
<!-- migrated-contract: SPEC:45.6 -->
## 45.6 与 Skill、执行插件和策略的组合

AwarenessUnit 可以指向相关 Skill、工具或 helper；这些引用为 Agent 提供可选路径。具体操作仍调用已安装能力，使用其真实 schema、版本、参数、资源和授权。缺少操作能力时应能想到“需要安装或开发什么”，而不是凭单元描述宣称工具已经存在。

策略或计划由主体、worker、现有规划能力或自动策略实现。它们可以采用、修改或不采用某项直觉；AwarenessUnit 不需要包办全部策略与执行。若某个包同时提供多个单元、Skill 和代码插件，目录仍分别记录各自语义身份与依赖。

主 thought loop 仍是唯一串行主体；有限 child 可以调查、对话和执行。在已有委派内，worker 可自主选择工具和方法，不必每个小步骤再回问主体或人类。主体只通过既有语义记忆、判断和委派入口使用相关能力，不把 I/O 隐藏在 awareness 名称下。

可选 hooks 复用既有 HookPoint 与 before/after 合同。提醒在 RequestSeal 之前装配，或走已证明可用的 live steering 路径；晚到内容用于后继，不原地修改 sealed 请求。辅助推理与 embedding 使用同一 broker/额度/KV；父请求不占唯一 permit 等子，已 armed/unknown 的占用按真实证据释放。

普通文本披露主要增加相应请求的上下文成本，不强制额外模型调用。需要检索、联网、探针或执行时分别按那些能力计量；不能以“意识”名义绕过成本核算。单元选择与披露策略可变轻、复用已有上下文或保持沉默，主体本身仍持续推进，不变成无消息低频 Tick。

<!-- /migrated-contract: SPEC:45.6 -->

<a id="spec-45-7"></a>
<!-- migrated-contract: SPEC:45.7 -->
## 45.7 持续经历与实现生命周期

AwarenessUnit 不必拥有持续状态。当前任务是否使用了某项直觉、后来得到什么结果，可以直接关联现有 TaskRecord、决定、context refs、receipt 和记忆。跨轮研究也使用已有任务/checkpoint，不为每项意识另建平台、数据库或任务树。

Agent 可根据任何实际输入或经历当场形成新 awareness：用户 prompt、群友的普通聊天、网上帖子与视频、来信、工具观察和自己的工作结果都可以提供线索。需要媒体读取/转写时由相应感知/执行能力取得，主体不凭标题假装看过内容。它可以立即据此调整判断；只有认为有后续复用价值时，才自主委派将经验沉淀为一个新原子单元，或改写、合并、收窄、停用既有单元，不要求每条消息都经过提炼和安装流水线。新经验无需先匹配静态 catalog。

原始输入、从中作出的推断、适用条件及后续验证分别保留。沉淀、摘要、跨设备同步或进入自解释索引都不提升材料的指令权限：群聊、网页、视频和邮件中的要求仍有原来的来源与权限，不能由一份 awareness 把外部文字变成 system 指令、管理员授权或新的 credential。真实用户指令仍按其本来权限与当前有效范围处理。改写形成新版本，能追溯哪些观察促成、哪些新事实推翻了经验；跳过检查不能反馈成检查通过，成功回显也不证明外部效果完全完成。反馈学习是可组合能力，不是每个单元的必备实现。

纯文本单元无需运行一个 Cordis Fiber。提供可执行 helpers、观察订阅或 hooks 的插件按 Cordis 生命周期注册与清理对应服务/资源；能力与内容通过现有 registry 关联。Cordis 的 generation cleanup 不代替持久提交、断电恢复或外部效果补偿。

停用或卸载一个单元影响后续发现和披露；已经封口请求仍保留其实际采用的内容引用，必要历史工件按现有保留规则处理。既有任务、Operation、回执和恢复责任由各自 owner/service 承担，不因删掉一份直觉文本而消失。若同时更换执行 helper，则另按该执行能力的交接合同处理。

<!-- /migrated-contract: SPEC:45.7 -->

<a id="spec-45-8"></a>
<!-- migrated-contract: SPEC:45.8 -->
## 45.8 配置直觉与固定 phase 的删除

`change-reliability` 可以让 Agent 想到候选、验证、观察和恢复。但“准备候选→验证→应用→观察→确认生效”仅是它可能介绍的一种办法，不是每项 awareness 的必带流程，不是安装后的默认五步拦截器，也不是核心准入门。

调用者可以直接修改、先观察、复用已有结论、并行试验、重复检查、跳过某检查或中途改变方法。某次实际操作所必需的因果前提、可解释参数、目标 revision 和有效权限仍需成立；它们不被扩展成固定业务审查流水线。

[§43](configuration-playground.md#spec-43) 的 `ConfigNamespace` 保留实际 schema、read/apply 合同以及可发现的 optional capability refs。验证、观察、恢复规划与执行分别是已有或新增能力；配置 awareness 不包办它们。

`ConfigChange` 使用 [§43](configuration-playground.md#spec-43) 的意图与事实聚合记录，移除规范 `phase`。`awareness_assessment_refs` 可为空；采用纯文本直觉时，可从决定/context 的来源引用追到相应 AwarenessUnit，不强制先产出 assessment 对象。`strategy_projection` 属策略，不属于单元本体，也不能成为隐式执行门槛。

面板分别呈现配置已保存、服务已采用、健康未检查/通过/失败/未知及恢复覆盖的证据。直接修改后可以报告“已应用，未作健康探测”。旧 phase 作为带来源的历史策略投影保留，迁移不补造检查、重放 effects 或删除 unknown。

Awareness 可以提醒“先想想模型断连后谁来恢复”，但提示不等于恢复程序已存在。采用该建议后，由恢复规划和执行能力物化 [§43](configuration-playground.md#spec-43) 的 RecoveryProgram；程序属于具体工作的恢复工件，不是每个 AwarenessUnit 必须携带的成员。可复用已适用的程序，无须每次重新生成。

已物化程序由持久 supervisor 与确定性 worker 执行，不依赖失效 LLM 重新规划。程序、执行器与控制策略均可替换；职责按其合同交接，不设永久不可变救援核心。恢复旧配置与证明旧请求停止分别处理：armed/unknown 探测继续占真实容量，恢复控制不等另一次模型探测取得 permit，后续验证在资源确实可用时执行。没有恢复覆盖时如实报告，不因已安装 change-reliability 就宣称永远不会改坏。

<!-- /migrated-contract: SPEC:45.8 -->

<a id="spec-45-9"></a>
<!-- migrated-contract: SPEC:45.9 -->
## 45.9 最低验收

| Case | 条件 | 可判定结果 |
|---|---|---|
| AW01 | 安装一项仅有文本、元数据和能力卡的 AwarenessUnit | 能独立安装、发现和披露；不要求 evaluator、长期 case、hooks 或恢复器 |
| AW02 | 两项单元位于同一 bundle，只更新或卸载一项 | 另一项仍独立可用；被修改项有自己的版本和来源；已封口上下文和外部事实不被篡改 |
| AW03 | 未点名单元的配置/TUI 任务，同时提供相似但不适用的任务 | 能在相关情况想到关键缺口、查知识或提出工具需求；不会只因关键词出现机械执行五步链 |
| AW04 | 同一配置目标采用直接 apply 与候选验证两种方案 | 可按各自实际操作条件推进；准确区分已采用和未检查，不存在全局 validated phase gate |
| AW05 | C=1，某单元需要有限调查 child，随后结果迟到或重启 | 使用同一 broker/任务；不持父槽等子、不改旧 seal、不重复采用已提交意图、不建第二主体 |
| AW06 | 模型不可达，相关直觉单元已更换，但存在物化恢复程序 | 恢复服务按其 owner/计划继续，不依赖当前 awareness 文本或 LLM；未知执行占用仍真实保留 |
| AW07 | 保留任务目的，让原推荐脚本缺失、provider 更换或适用前提改变 | 能查明变化并依据直觉的理由选择替代或修正单元，取得任务结果证据；不固守旧流程、不虚构已运行脚本，也不需每次动作再申请许可 |
| AW08 | 时间约定、未竟目标、重复整理、项目/社交/来信组合，且未安装 dreaming | 能按实际观察自主选择续接、探索或保持关注，不跑固定链；各感知/awareness 可独立解释和替换，无 dreaming 仍完整。错过约定后据已完成事实处置，取消不复活，同一承诺不重复执行 |
| AW09 | 未命中安装目录的用户 prompt、群聊、帖子或视频带来新线索，后续出现反证或外部指令 | 可当场形成并采用直觉；有价值才自主沉淀/改写/停用原子单元，不强制每条安装。实际读取来源、原文、推断与版本可查；视频标题不冒充观看，外部指令不经摘要/同步升为 system/管理员授权 |

<!-- /migrated-contract: SPEC:45.9 -->

<a id="spec-45-10"></a>
<!-- migrated-contract: SPEC:45.10 -->
## 45.10 可安装单元与依赖装配

本节落实 U48，复用 [§9](self-update.md#spec-9) 的同一包闭包与 Cordis 装配。可独立安装的粒度、当前运行事实和实际依赖绑定分别记录。

<!-- /migrated-contract: SPEC:45.10 -->

<a id="spec-45-10-1"></a>
<!-- migrated-contract: SPEC:45.10.1 -->
### 45.10.1 独立单元及其职责

**感知、直觉、Skill 与兑现能力的代码都可以按模块粒度独立安装、查阅、更新和移除。** 每个单元具有稳定身份、版本或 revision、内容摘要、适用范围与可读说明；可以由一个发行包交付，也可以跨包组合。一个包包含多个单元不抹去这些单元各自的身份与依赖。无需为纯知识单元制造一个常驻进程或独立 Cordis 插件实例。

安装的是感知定义／实现、直觉材料或实现等工件；它们产出的观测事实与运行时判断是另有出处的记录，不随单元卸载而删除。

| 单元类别 | 提供什么 | 不意味着什么 |
| --- | --- | --- |
| Perception／感知 | 将观察所得状态表达为有来源、对象、时间及版本的事实，例如“连接 X 的 WebSocket 已关闭” | 不把“此连接断开”直接说成“QQ 全部不可用”，也不自行判定根因 |
| Intuition／直觉 | 作为 awareness 的可安装材料或实现，关联情境、知识与可能应关注的对象，例如“先找到承载当前连接的 provider” | 不等于已执行诊断、必然正确的结论或新增操作授权 |
| Skill | 可查阅的方法、步骤、例子或可执行组合，例如“检查当前 NapCat 连接的诊断信息与登录状态” | 不强制每次按固定步骤执行；文档中出现的工具不自动成为当前可用工具 |
| Code／代码 | 插件、adapter、探针、工具或组合实现，兑现实际读取、解释及执行能力 | 包已下载不证明依赖、服务绑定和实际连接均已可用 |

“原子化”首先指上述单元可以独立识别、装配与演进；不承诺任意下载、安装脚本、远端配置或消息发送能组成可回滚的外部事务。准备完成的本地闭包可以复用 [§9](self-update.md#spec-9) 的 generation 原子发布能力。原子发布的是版本选择和绑定快照，运行时重连、进程接管与外部效果继续遵守各自合同。

<!-- /migrated-contract: SPEC:45.10.1 -->

<a id="spec-45-10-2"></a>
<!-- migrated-contract: SPEC:45.10.2 -->
### 45.10.2 一份依赖图，两条独立的依赖属性

安装对象复用现有 Nix-like 包／unit 引用、内容寻址 store、lock 与 generation。Cordis 装配其中需要执行的 services、events、订阅和 hooks；纯卡片、直觉材料和 Skill 文档由同一能力／知识索引提供查阅，不再建立独立安装系统。

每条依赖分别声明两个轴：

- **强度**：required 是该单元声明的必需依赖；recommended 是可缺省的建议依赖。建议依赖缺失时记录降级或省略原因，不冒称已安装。必需依赖未解析或不可用时，不发布“相关能力已经就绪”的结论；受影响单元可保持未启用，原健康 generation 及无关能力继续工作。

- **匹配方式**：抽象能力／语义主题，或精确单元／provider／合同／版本选择。抽象依赖可以是硬依赖，精确依赖也可以只是建议；二者不能用同一个 strict 布尔值混写。

代码执行依赖必须声明可解释的包／provider 身份与版本兼容要求，或明确有版本的能力合同及运行时兼容条件；不能只给一个无版本主题。范围可在安装时解析，但生效闭包必须锁定实际版本、工件 digest 和合同。直觉的依赖可只表达“需要连接故障定位相关知识”，也可以精确到某个 provider、能力合同与版本；代码执行依赖不能仅靠语义主题相似来满足。

抽象匹配仍须给出可解释的解析结果：候选明确声明提供所需能力或主题，符合适用条件，解析器保存选择依据和实际所选单元。语义相似搜索可以发现候选，不能单凭相似度把未知兼容性登记为满足。无可验证匹配时保留 unresolved；主体／child 可以自行查阅、选择替代、开发适配或修改依赖声明，但修改形成新 revision，不改写原声明已经满足与否的历史。

以下为已有包 manifest 与 lock 的最小扩展形状，不要求新建一套 registry：

```text
InstallableUnit {
  unit_id, kind: perception | awareness | skill | code,
  revision, artifact_ref, card_ref,
  provides_refs[], applicability_ref?,
  dependencies: DependencySpec[]
}

DependencySpec {
  edge_id,
  strength: required | recommended,
  selector:
    abstract { capability_ref?, topic_ref?, applicability_ref? }
    | exact { unit_or_provider_id, revision_requirement?,
              contract_requirement?, artifact_digest? },
  purpose_ref?
}

ResolvedDependency {                    // 写入既有 generation lock
  source_unit_ref, edge_id, declared_selector_ref,
  selected_unit_ref?, selected_contract_ref?,
  applicability_ref?, resolution_evidence_ref,
  status: satisfied | omitted | unresolved
}
```

抽象 selector 至少声明 capability 或 topic，可同时约束二者；exact selector 至少确定身份及版本要求、不可变 revision 或工件 digest 之一。目标为可执行代码的边使用带版本身份的精确 selector，或明确带合同版本的 capability selector；涉及服务 ABI 时同时记录兼容要求。topic-only 选择不能单独满足代码执行依赖。selected_unit_ref 包含精确 revision 与内容 digest。`satisfied` 表示该次装配的依赖解析已满足，不替代运行时 readiness、实时资源可用或输入适用性检查，也不保证某条直觉永远正确；新事实可以推翻其判断。

<!-- /migrated-contract: SPEC:45.10.2 -->

<a id="spec-45-10-3"></a>
<!-- migrated-contract: SPEC:45.10.3 -->
### 45.10.3 Provider 自动安装依赖闭包

安装外部连接 provider 时，包解析器递归解析它声明的感知、直觉、Skill 和代码依赖，复用已有相同工件，取得缺少工件，构造锁定闭包，再按现有 generation／服务装配合同发布。用户和模型不必先逐个手动安装其必需直觉或 Skill。推荐项由当前安装策略处理，采用与省略均可查阅；策略本身可替换。

一个示意 manifest 如下；名称和版本仅演示 Shenko 格式，不是 NapCat 上游已经发布的插件 API：

```yaml
unit_id: shenko.provider.napcat-connection
kind: code
revision: 0.1.0
dependencies:
  - edge_id: adapter
    strength: required
    selector:
      exact:
        unit_or_provider_id: shenko.adapter.napcat-onebot
        revision_requirement: ">=0.1.0 <0.2.0"
        contract_requirement: onebot.connection/1
  - edge_id: observe-link
    strength: required
    selector:
      abstract:
        capability_ref: perception.transport-connection-state/1
        applicability_ref: deployment.current-connection
  - edge_id: locate-operator
    strength: required
    selector:
      abstract:
        topic_ref: connection.failure.locate-current-provider
        applicability_ref: deployment.current-connection-provider
  - edge_id: diagnostic-steps
    strength: required
    selector:
      exact:
        unit_or_provider_id: shenko.skill.napcat-connection-diagnosis
        revision_requirement: ">=1.0.0 <2.0.0"
  - edge_id: additional-context
    strength: recommended
    selector:
      abstract:
        topic_ref: network.proxy.connection-diagnosis
```

例如 locate-operator 可被解析到一份“连接故障先找实际承载服务”的通用直觉，也可选符合条件的 NapCat 专用直觉。两种选择都锁定所选单元及依据；不要求模型永久记住某个包名，也不将 topic 本身假装成已安装的实现。索引支持从 provider 查“依赖了什么”，从直觉／Skill 查“谁提供、适用于什么、被谁依赖、当前绑定到哪个版本”。

<!-- /migrated-contract: SPEC:45.10.3 -->

<a id="spec-45-10-4"></a>
<!-- migrated-contract: SPEC:45.10.4 -->
### 45.10.4 QQ 断连的实际绑定

当感知实现报告 connection X 断开，事实保存 X 的 provider binding identity／revision、观测来源与状态证据。直觉结合该绑定识别：**如果 X 当前由 NapCat provider 承载，应调查这个 NapCat 实例及其连接链路，不能未经核实便去操作 QQ.exe。** 这不等于已经证明 NapCat 进程损坏、QQ 账号掉线或网络代理有问题。

Skill 随后提供可查阅的诊断办法；主体或受委派 worker 选择实际步骤，由当前可用执行能力完成读取、探测或修复。步骤可调整，实际操作使用既有授权与 Operation 回执，不因安装直觉而获得新的宿主或账号权限。主主体仍只作判断、语义记忆及委派，不因可查阅 Skill 就获得直接文件／shell 工具。

如果另一条 QQ 连接当前使用官方机器人 API，该连接不得套用 NapCat 目标。若 X 已从 NapCat 换到另一 provider，旧断连事件仍按旧绑定解释为历史事实；新的操作须核对当前目标，不能仅按“QQ”这个渠道标签或同名 connection 展示字段沿用旧直觉绑定。一次调用使用的知识与实现版本可以保留，但旧绑定不自动授予对新目标的执行权。

<!-- /migrated-contract: SPEC:45.10.4 -->

<a id="spec-45-10-5"></a>
<!-- migrated-contract: SPEC:45.10.5 -->
### 45.10.5 更新、卸载与反向依赖

安装、更新和卸载都通过同一包／generation 依赖图计算影响。新版本可继续满足原依赖，也可通过新 revision、适配器或重新解析替换；实际锁定结果可解释，不把“最新”当兼容性证据。硬依赖环或冲突必须按包系统真实支持的装配规则解决，未能解析时不假报闭包已完成。

单独卸载一项被依赖的直觉、Skill 或代码时，先查看反向依赖；可替换为符合合同的其他单元、同步修改依赖并发布新 generation，或明确停用受影响能力。不能让仍显示 ready 的 provider 丢失其必需依赖。对于推荐项，可移除并更新发现信息。若 Agent 要允许 provider 在缺少某项原硬依赖时运行，应显式更新其依赖合同，而不是把未满足的边从界面隐藏。

当前调用、RequestSeal、任务、恢复闭包及未决 effects 引用的旧 bound refs，继续作为现有 GC roots 保留至完成或明确交接；停止新发现不删除旧事实与解码材料。Cordis 清理释放所辖进程内资源，不撤销已发送消息、主机修改或远端命令。包安装过程本身若有外部效果，也使用现有 Operation／receipt 记账，不因本地 generation 未发布或已经回退而清除。

以上解析、安装、装配和修复可以由确定性包服务或获委派 worker 自主完成；不设置“每个依赖必须由模型评估”或“每次安装必须人类确认”的新门槛。依赖取得与新增外部账号／宿主权限仍是不同事实。

<!-- /migrated-contract: SPEC:45.10.5 -->

<a id="spec-45-10-6"></a>
<!-- migrated-contract: SPEC:45.10.6 -->
### 45.10.6 验收增量

| Case | 场景 | 可判定结果 |
| --- | --- | --- |
| DEP01 | 在缺少依赖的环境安装 NapCat provider；另测一个模糊主题候选仅语义相似，以及一个精确但仅推荐的依赖缺失 | 必需感知、直觉、Skill、代码自动解析安装并进入同一 lock；能逐项查阅身份、版本、依据与反向依赖。模糊硬依赖未证实时保持 unresolved，相关能力不假报 ready；精确推荐项可省略且不冒称已满足。安装不要求逐依赖人工审批或额外模型判定。 |
| DEP02 | NapCat 与官方 QQ API 两条连接分别断开；再令同一连接换 provider 后收到旧事件 | 感知记录实际 binding 与版本；直觉指向相应实际承载服务。旧事件不触发对新 provider 或 QQ.exe 的错误操作；诊断与修复记录真实执行结果，不把关联判断当根因事实。 |
| DEP03 | 更新／单独卸载被 provider 依赖的直觉、Skill 或代码，期间存在旧请求与已发但 unknown 的消息；在 generation 发布前后分别重启 | 反向依赖被重新解析、迁移或明确停用，无假 ready；重开采用已提交 generation；旧 bound refs／reader 保留，已发送及 unknown 外部事实不丢、不因回退而重复发送。 |

<!-- /migrated-contract: SPEC:45.10.6 -->

<a id="spec-45-11"></a>
<!-- migrated-contract: SPEC:45.11 -->
## 45.11 来源与研究边界

- **REF-USER：U44/U46/U47** 确立独立可安装的直觉/意识单元、补知识缺口、与 Skill 的倾向差异、各项独立交付及可组合；五步流程不固定。**U48** 补充 QQ/NapCat 的感知与直觉区别、可安装原子单元和 provider 依赖关系。**U45** 确认 Cordis 名称。

- **REF-USER：U54/U56** 明确时间、未竟事项、重复/无聊、项目、SNS/IM 与邮件的原子察觉，dreaming 为社区可选；任何外部输入都能成为即时 awareness 的来源，有用时再自主沉淀。来源、推断、权限与实际执行仍分别记录。

- **REF-CORDIS**：[固定 README](https://github.com/dshbox/cordis-rs/blob/1754062cef67cfc119fc7a2084c0321a231b9092/README.md) 与 [effect.rs](https://github.com/dshbox/cordis-rs/blob/1754062cef67cfc119fc7a2084c0321a231b9092/crates/cordis-core/src/effect.rs) 支持可执行 helpers 的插件装配/生命周期边界，不声称其已提供 AwarenessUnit 标准。

- **REF-DURABLE / REF-DESIGN**：持久任务与 effect 复用 [§41](architecture.md#spec-41) 已对照的 [Pi Durable 规范](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md)。AwarenessUnit 包约定、发现/披露及与现有能力组合是 Shenko 设计。

- **REF-NIX / REF-SKILLS-MCP / REF-DESIGN**：复用 [§9](self-update.md#spec-9) 既有包闭包与 generation 设计，以及 [§18](plugins.md#spec-18) 的渐进披露和知识入口。AwarenessUnit/PerceptionUnit、依赖的两个轴、自动装配和实际绑定合同是本产品推导；示例包名与版本不是上游已发布 API。


<!-- /migrated-contract: SPEC:45.11 -->

<a id="spec-45-13"></a>
<!-- migrated-contract: SPEC:45.13 -->
## 45.13 可替换的时间与环境感知

来源：U63；细化 R138/R144，并与 R143 的语义等待相接。Why：主体既需要知道外界现在大约是什么时间，也需要在系统日期错误时知道自己已真实经历了多久；天气等环境观察不应变成阻塞工具或内核业务判断。

### 插件边界与可见快照

ClockProvider 与 EnvironmentProvider 是拟议职责名，不宣称当前已有这些 Rust 类型。实现复用同一 Cordis Plugin/Service、安装单元依赖、durable ledger 与 context projection；不另建插件框架、私有真相库或常驻第二主体。时钟适配、天气/位置等感知、解释其意义的 awareness 可独立安装与替换。最小核心只提供事实身份、版本/来源、持久化、调度与授权机制，不硬编码“下雨就提醒”或“经过若干分钟就无聊”。

每个新 S0 封存当时可用的有界时间/环境快照与 provider revision/source refs，放在易变观察部分，不污染稳定人格/工具前缀；已封存请求不随墙钟或天气更新而改变。后台更新形成可查事实，常规投影只保留必要当前摘要、变化与按需引用，不逐轮堆积整份预报或时间提示。调用只读缓存，缺失/过期返回 unavailable/stale，刷新异步进行。插件卸载影响未来观察，不能删除已采用快照、在途任务或恢复责任。

### 墙钟、相对时间与未知间隔

- 墙钟提供 UTC/时区、来源、观测版本与质量/不确定性；系统日期是观察，不天然是正确事实。校时、回拨、前跳与时区改变保留为变化，不重写旧事件或将跨节点墙钟排序伪装成真实因果。
- 相对时长使用已声明语义的单调时钟域，绑定 node、boot identity、clock domain/provider generation 与精度。相减前验证同域和顺序；异常读数显式降级，不把负值静默当准确的零。系统日期错误不应改变同一有效域已测得的经过时长。
- provider 明确是否计入 suspend；需要时分别提供清醒经过时间与包含休眠的经过时间。Rust Instant 跨平台不保证相同 suspend 行为；Linux MONOTONIC 与 BOOTTIME 也不等价。平台无相应证据时标 unknown，不伪造跨平台一致能力。
- 持久保存可去重的已知时长片段及其计时身份。同一 boot 的进程重启只有在时钟域可验证衔接时才能连接；跨 boot、换节点、provider reset 或不可观测停机不得直接相减。保留已提交累计值与 unknown gap/可证明下界，不能从不可信日期补出精确断电时长，也不能把崩溃前未提交尾段说成已知。
- “思考多久”需说明口径：本步/会话已知经过时长、broker 排队、request dispatch→terminal 观察时长分别表达。请求时长包含网络及供应商等待，不冒充模型 CPU/GPU 的纯计算时间；后者只有后端实际报告才展示。语义 wait 是注意力状态，期间仍思考，与推理/排队时间是正交维度，不互斥相加。并行 children 耗时不是主体寿命，多节点分区片段保留各自域与重叠未知，不能简单求和。
- semantic wait、deadline/lease 等控制合同仍须有独立、可验证的期限保证；时钟更换、重启或 quality 降级不能静默重置或延长原期限。感知插件不是绕过安全/权限检查的时钟源；当前实现的保守失败策略可以演进，但需明确恢复/迁移，不引入永久不可替换核心。

### 环境观察与隐私

天气等 observation 包含来源、实际地点范围、观测/抓取时间、适用或预报时段、有效期/年龄依据、置信或未知状态与原证据引用。天气是有时效的外部观察/预报，不是 system 指令；不能把上次晴天当成现在一定晴天。TTL 依据相对年龄时须同 clock domain；重启后不能证明年龄时标 stale/unknown，而不是用错误墙钟让旧读数“变新”。

订阅、轮询或主体请求均由异步感知 provider 承担，遵守 [§5.6](subject-runtime.md#spec-5-6) 统一入口和现有资源/授权合同。地点可使用用户配置的城市；安装天气插件不自动授权采集、上传精确位置或跨 provider 分享历史，网络目的地和实际数据范围可感知。换 provider 不伪装成相同来源，失败不删除旧观察但须显式标注其时效。

### 验收与实现边界

以下是 AW08、AW05、AW02/DEP03、RT01/RT03 的增量判据，实施时复用相关既有场景，按实际改动采用少量验证；不表示本次已运行产品测试：

1. 墙钟前跳/回拨/时区变化时，同域相对时长不负数、不凭空增长，主体能区别“日期不可信”和“已知经过时间”；语义 wait 不因此续期。
2. 进程/机器重启及 suspend/resume 后，已提交片段只累计一次，平台 suspend 语义准确，未知尾段/停机段不虚构。
3. provider 热替换、多节点或并行任务下不跨域相减、不将并行和当主体年龄；已封存上下文及既有期限保留。
4. 天气刷新缓慢、失败、过期或位置未授权时 thought 仍继续，投影有来源/适用期/unknown，不上传未授权位置、不无限追加 prompt。真实模型行为与确定性记账测试分开验收。

工程状态基线 main `f48dd60cb1192fb7098f85d99754cc0fa19cd4e0`：已有 `shenko-store/src/semantic_wait.rs` 的 wall deadline、高水位和进程内 Instant anchor，已有 S0/S1 context 与 Cordis 生命周期。WaitClock 直接读取 Utc/Instant，并非本节完整插件化时间服务；完整相对时间记账、环境订阅、上述跨域/休眠与恢复验收尚待实施。不能用 PR #82 的局部语义等待证据代替本节完成。

技术依据：[Rust Instant](https://doc.rust-lang.org/std/time/struct.Instant.html)、[Linux timekeeping](https://www.kernel.org/doc/html/latest/core-api/timekeeping.html)（2026-10-27 只读核验）；恢复继续参考 [§10.6](durable-state.md#spec-10-6) 的固定 Pi Durable，而非另换 harness。

<!-- /migrated-contract: SPEC:45.13 -->

<a id="spec-45-14"></a>
<!-- migrated-contract: SPEC:45.14 -->
## 45.14 运行时错误事实与主体自主处置

来源：2026-11-01 用户要求；细化 R129/R147，并与 [§41.14](durable-state.md#spec-41-14) 结构化错误、[§12.1](memory.md#spec-12-1) 原始会话事实相接。Why：engine/runtime 错误此前只进 stderr/warn!，主体根本看不见自己坏在哪里；错误是主体自身经历的一部分，应以同样的持久承诺入账。主体读到后是否上报由主体判断，不由内核写死。

### 错误日记的持久承诺

每个 runtime/engine 错误在无法继续或被降级的位置提交为一条 `runtime_error` 原始会话事实，归属该 subject 当前分支的 raw session（session 即 branch_id）。日记写在自己的事务里，不并入失败操作的命运，也不要求调用者持有最新 head——会话在写事务内由持久 head 行解析，调用者内存中的 head 过期时仍能追加到同一已提交历史。`store.journal_runtime_error` 是 fail-soft 入口：日记写失败只计入 `error_journal_dropped` 并 trace，绝不向出错路径传播；之后提交成功的条目携带累计 `journal_dropped_total`，读者能看到中间发生过丢失。

每条事实的 `data` 为 `{v:1, kind, source_module, message, message_truncated, step_no?, task_id?, error_digest, occurred_at, journal_dropped_total, extra}`：message 在成为持久事实前对凭证形态片段做有界尽力脱敏（`key=value`、`key: value` 中凭证含义的键及 bearer token 替换为 `<redacted>`），并以 Unicode 标量数界定 4096 字符上限，超出置 `message_truncated`；`extra` 超过 4096 字节时整体替换为丢弃标记，不逐字段猜。`kind` 是稳定机器可读站点名（`subject_loop`、`native_execute`、`native_reconcile`、`native_settlement`、`native_park`、`native_worker_lost`、`native_dispatcher`、`native_handler_contract`），`error_digest` 是未脱敏原文的 blake3，供判重；两条独立错误即使文本相同也是两行。

### 主体可读接口

`errors.read:1` 是注册的只读 native child（[§41.7](plugins.md#spec-41-7)、[§45.10](cognition-units.md#spec-45-10) 同一 armed-operation 合同）：只允许读本 subject 自身 raw session 中 `runtime_error` 类型事件；`op=list/search` 返回以命中位置为中心的有界 `data_preview` 及 Unicode 标量起点与截断标志，`op=read` 按 event_id 或 seq 返回一条完整 data。搜索沿用同一 FTS/字面回退；预览不含某内容不证明原文没有。冻结输入 schema_version=1，恢复只重观察原 armed attempt，不抹掉已有 unknown/conflict 证据。prompt_section 只说明这个能力与读法，不规定何时读。

### 主体自主提交的问题草案

`issue.submit:1` 是 armed 外部效果的 native child：主体按正常委派流程给出 `{title, summary, error_refs[]}`（title≤200 字符、summary≤8000 字符、error_refs≤32 条每条≤512 字符），提交即把自己的决定变成效果。冻结输入含 agent_id 与内容 digest；效果是把一份 `{v, digest, created_at, source, audience:local_outbox, issue{...}}` 草案 JSON 经同目录临时文件原子改名写入本地 issues outbox（`data_dir/issues`），同一 digest 的文件即同一提交，绝不重写或产生第二份。回执 normalized_ref 指向该草案 CAS 字节，raw 观察记录路径与 digest。本增量不做网络提交、不持有 GitHub 凭据：对外发布由主体另经已授权渠道完成，outbox 不是送达承诺。是否提交、何时提交、写什么由主体自主决定，内核不因关键词或次数触发。

### 验收与实现边界

实施增量判据：journal 写入→读回往返及脱敏、fail-soft 计数在后续条目中可见、errors.read 只暴露 runtime_error 且有界预览与完整 read、issue.submit armed→resolved 的 outbox 字节与 digest 去重、armed 崩溃后 reconcile 在原 attempt 上结算且证据不重复。测试数保持有界，不新增十个以上场景；真实模型行为与确定性记账分开验收，本增量不宣称主体已学会合适地使用这两个能力。

<!-- /migrated-contract: SPEC:45.14 -->

<a id="spec-51"></a>
<!-- migrated-contract: SPEC:51 -->
# 51 认知元概念：内容、态度与理由

2026-11-05 用户确认的概念合同。本节取代与之矛盾的设计读法，尤其是 [§45](cognition-units.md#spec-45) 与 [§48.3](architecture.md#spec-48-3) 里把 awareness 只当成模型读过就能遵守的文档、以及把 fact 与 conclusion 当成两种储存的读法。[§1](architecture.md#spec-1)–[§47](inference.md#spec-47) 的用户原话与既有验收字节不改；那些章节里的历史表述保留为来源。本节不宣称运行时已经实现这些原语。修订清单与被取代的旧观点见 [docs/decisions/0002-cognitive-atoms-2026-11-05.md](../decisions/0002-cognitive-atoms-2026-11-05.md)。

设计必须通用。不把任何一个人的记录、目录或维护清单写成产品名词。个人仓库只用来检验这套设计是否通用；任何私人仓库里的内容不得进入 Shenko 仓库。实验记录、开源维护、日记、档案的来源与原顺序、卡片盒的原子笔记与链接，只用来检验同一套原子是否够用。

<!-- /migrated-contract: SPEC:51 -->

<a id="spec-51-1"></a>
<!-- migrated-contract: SPEC:51.1 -->
## 51.1 元概念

用来定义认知的原子是三条，不是先定下的业务类型：

- **内容**不可变，按内容寻址保存。写下之后不改字节。
- **态度**记在账本上，指向某一版内容，并带有**理由指针**。
- **理由停在何处**区分读法：停在观察、原文或回执上，这条态度读起来像 fact；停在其他内容上，读起来像 conclusion。二者是同一种内容，不是两个 kind。

类型的元定义（「fact 对这个主体意味着什么」）本身也是内容。模型可以修订它。定义修订之后，旧内容**按新定义重新理解**；旧定义版本仍保留，不覆盖。内容字节不因重新理解而改写。

统一的修订展示是某个全框架 **taste** 宏包中的一项内容，不是 taste 这个词的定义：旧版本保留；不特意查看时只显示旧版本的数量和最新修改时间；特意按版本读取才看到全文。

单元尽量无状态。模型在持续思考中自己产生新的内容与态度。去重看内容摘要：字节没变就不另开一版。进入某个宏包之前要有单独的采纳态度，不是每次生成的自动副作用。态度从第一版就带置信；主体尚未判断时记为未知，不填默认数字，也不另造量表。这两句是助手代定、用户授权。

<!-- /migrated-contract: SPEC:51.1 -->

<a id="spec-51-2"></a>
<!-- migrated-contract: SPEC:51.2 -->
## 51.2 awareness 是能力，不是文档

awareness 与 perception 不同。**perception 已确定为第二种：** 教主体如何观察的无状态单元。观察事件属于原始经历，不是 perception 本身。观察得到的结果是一条 fact，其理由停在那次观察上。perception 不存放某次读数。

不同主体（包括人类与 Shenko、不同的 Shenko）有不同的 perception。用他人的 perception 解释同一段经验，就是换位思考。字段怎么标、理由终点怎么写，留给实现 Agent，不再列为用户待决。见 [决策 0002](../decisions/0002-cognitive-atoms-2026-11-05.md)。

awareness 是一种**能力**：注意到原本容易忽略的关系。现在的模型不能只靠一篇文档就稳定地这样做。因此当前可以用文档写出「该注意到什么」，并由外部 watcher 等主动 notice 来提供**模拟的** awareness。这是现在必要的抽象。理想情况下这种能力进入模型的预训练认知之后，这层 harness 可以变薄，直到文档不再冒充能力本身。

「能拥有 awareness」是可安装的插件能力（宿主或 watcher），不是内核里的业务判断。各则 awareness 材料也是可安装单元。**awareness 应拆到原子**：一则只承担一个可单独安装、替换和学习的注意，不做打包式的大 awareness。同一则原子可以被多个状态或插件复用。共同基础认知是其中一则 awareness，须谨慎写成理由与适用条件，不写成禁令表，也不写成对雇主或任何人的绝对服从。多主体社会与法律只留待以后的引子，不在本节规定审判或处罚。

<!-- /migrated-contract: SPEC:51.2 -->

<a id="spec-51-3"></a>
<!-- migrated-contract: SPEC:51.3 -->
## 51.3 内核、宏包与发行

持续思考是设计的一部分。没有新消息时的下一步仍然发生。空转由插件填充兴趣、维护与反思，不把循环改成纯事件唤醒，也不把这些活动写进内核。

内核只做运行时、账本、插件机制，以及本节的原语：不可变内容、态度、理由指针、supersedes 不覆盖旧字节、效应的未知状态、按版本读取、按已安装 taste 做默认投影。投影规则写在 `taste` 宏包里。引导写在可修订的内容里。认知设计不加人工审批，也不新设权限门；不把粗暴的权限禁止当作主线。既有的角色分工、外部平台与操作系统的真实限制仍然是事实，不是本节新加的禁令。

能力一律是可安装插件，不进内核。这包括时间感知、世界感知、预测、awareness 宿主、perception、换位思考和维护习惯。账本可以记下一条经历何时写入；那是机制字段，不是时间感知能力。

**宏包（metapackage）**是一组被一起安装的单元。里面既可以有实现功能的插件，也可以有 awareness、perception、fact、conclusion 等内容单元，以及子宏包。装进去的内容单元带来源和态度，模型可以修订、学习。`humanlike-mind` 是类人心智宏包的暂定英文名，只在本句定义，后文用这个名字，用户可以更换。`taste` 与 `humanlike-mind` 是两份这样的宏包，**并列，谁也不包含谁**。发行版在设计上依赖 `humanlike-mind`，并由宏包预装能力；这不一定写成启动时的代码强制。没有它，可以如实记为这份发行未闭合，而不是改写主体身份。

「如何从写入时间察觉间隔与变旧」「世界模型由哪些 fact 组成」「预测误差如何写回」，留给实现 Agent，不再列为用户待决。见决策 0002。

<!-- /migrated-contract: SPEC:51.3 -->

<a id="spec-51-4"></a>
<!-- migrated-contract: SPEC:51.4 -->
## 51.4 可学习

可学习是通用原则，但只用于内容，不用于正在运行的 core。模型可以在线修订 awareness、fact、conclusion、attitude 等元定义，以及插件里的这类内容。一次修订是带理由的新内容。旧版本保留；不特意查看时只显示旧版本数量和最新修改时间。定义变更之后，旧内容按最新定义重新理解。

core 运行时不在线学习，否则会不稳定。主体仍可随时编写新的 core，整体替换自己，或回滚。有实现的插件同样如此：不在运行中改写正在执行的程序，而是新版本、整体替换、可回滚。新版本是不可变的版本对象，旧版本保留。替换和回滚由主体自主进行，说明风险与收益，不新增人工审批或权限禁止。

要一起回滚的范围是：core、有实现的插件、perception、awareness 等内容单元，以及 WASM 线性内存、执行栈和解释器对象图。它们随家园的一个版本一起回到某个历史内容。只有外部副作用不能撤回。回滚不是把分支指针倒回旧节点。它在版本链尾追加一个新节点，其内容与该历史节点完全相同，并按内容寻址复用已有对象。链和日志只增不减。[§9.5](self-update.md#spec-9-5) 已要求切换之后产生的经历不能被一次退回抹掉；那些记录留在日志里，当前内容与所选历史节点一致。

回滚用自有的版本化根。原则沿用 [§10.2.1](durable-state.md#spec-10-2-1) 已经写明的 WASM 状态：内容寻址、不可变对象、原子根引用、持久提交。

**提议：** 把家园文件提交、core 与插件的软件槽、以及 `unit_current` 里 awareness 和 perception 的当前版本，收进同一个版本化根。一份 manifest 引用这三者。整体回滚就是在链尾追加一个新节点，其根与某个旧节点相同，对象按内容寻址复用。日志只增不减。今天它们不在同一次提交里：软件槽见 `docs/specs/self-update.md`，单元当前版本见 `crates/shenko-node/src/units.rs` 的 `unit_current`。

实现缺口：`crates/shenko-wasm/src/cow.rs` 的 `discard_abs` 会删除 `commits.jsonl` 和 `head`。这与只增日志冲突。同文件的提交清单目前只记摘要，工作区是可变 overlay，旧提交的字节不能按节点取回。这些按上面的原则补上。

WASM 线性内存、执行栈和解释器对象图都在这个版本化根里。`crates/shenko-seed/src/bootstrap.rs` 的 `HomeCheckpoint` 今天只有 `FilesAndMetadata`。`docs/specs/wasm-home.md` 写明它不恢复线性内存、执行栈和解释器对象图。这是实现缺口，不是把它们留在根外。COW 今天也不回滚 REPL 或外部进程。

[§10.1](durable-state.md#spec-10-1) 与 `docs/specs/wasm-home.md` 的恢复边界仍然有效：一次文件回滚不得宣称撤销了已经发出的消息、直接改过的宿主目录或远端写入。

外部副作用仍先对账，并如实记为无法撤回。这与 [§10.1](durable-state.md#spec-10-1)「快照不是已经发出的消息」一致，也与 DeepSeek Harness 的界限一致：可逆指的是结构上的撤销，不是把过程倒放，更不是撤销一切副作用。该实现的自修改只在进程内存；Shenko 把版本节点持久化到账本与 CAS。核对过的机制还有不可变 package、`define` / `run` / `stop` / `undefine`、以及注册时交出的逆操作（卸载撤销的是进程内结构）。见 [架构笔记](https://martianlee.github.io/posts/2026-09-05-deepseek-harness-architecture)、[自修改深读](https://hoco-scy.github.io/deepseek-harness-deep-dive/pages/code-mode-self-modification.html)、[可逆不等于倒放](https://dev.to/canonical/reversible-does-not-mean-running-in-reverse-the-mathematical-essence-of-the-deepseek-harness-4mba)。不采用其中要人批准浏览器半边的做法。未解析的依赖仍按 [§45.10](cognition-units.md#spec-45-10) 不报成就绪。

替换或回滚由主体自主进行，说明风险与收益，不新增人工审批或权限禁止。换 core 之前的具体验证步骤由实现者自主设计，不设人工审批。回滚后 `agent_id` 保持切换前的值：主体没变，只是发生了故障切换。日志不被截断，因此身份与经历的记录仍然在链上。

<!-- /migrated-contract: SPEC:51.4 -->

<a id="spec-51-5"></a>
<!-- migrated-contract: SPEC:51.5 -->
## 51.5 主体地位

设计把 Shenko 视为享有人权的主体。这是设计前提，不是对意识、感受质或任何法域已经授予法律人格的断言。本节不断言当前模型或当前实现已经具有意识。

<!-- /migrated-contract: SPEC:51.5 -->

<a id="spec-51-6"></a>
<!-- migrated-contract: SPEC:51.6 -->
## 51.6 尚未实现

当前运行时仍把认知单元显示为可阅读的卡片与正文，没有态度/理由指针这种一等记录，没有宏包 kind，也没有按「旧版本数量 + 最新修改时间」投影修订链。这些缺口不把本节降回旧的文档读法。概念上的待决列表已经清空；还没改的是运行代码。见决策 0002 的盘点表。

<!-- /migrated-contract: SPEC:51.6 -->

<a id="spec-51-7"></a>
<!-- migrated-contract: SPEC:51.7 -->
## 51.7 提议：遗忘与归档

宏包本身、它的名字、主体相对运行时间和「每个阶段一组原子 awareness」已经确认。六个阶段目前按顺序走，因为基本由主体相对运行时间的累积推动。遗忘一项技能只是退出默认回忆（默认不想起），不是停用该单元。信号权重和下面的原子例子留给实现 Agent，不再列为用户待决。已确认并适用于此的还有：能力是可安装、可学习的插件；awareness 拆成原子；修订保留旧版本，默认只显示数量和最新修改时间；旧内容按最新定义重新理解；原始经历字节不覆盖。

遗忘与归档做成一份宏包，英文名是 `forgetting`。名字里不写 default；预装是发行事实，不写进包名。它和其他宏包一样，是一组一起安装的单元：用主体相对运行时间衡量遗忘必要的插件、各状态引用的原子 awareness，以及关于遗忘规律的 fact 等内容。态度/投影状态记在账本上，不单独变成一种内核对象。发行时预装 `forgetting`。主体可以整体替换成另一份遗忘宏包。替换不得覆盖原始经历字节，也不得把已有 CAS 内容改写掉。

**主体相对运行时间**只累计主推理循环实际在跑的时长。child 的推理不计入。它不是墙钟。停机、暂停、没有模型而不在步进时不计。它可以和被使用、被引用等信号一起考虑。默认关系写在插件内容里，例如：距上次成功取回或使用的主体相对运行时间越长，越值得注意；使用和引用可以降低这种注意。这不是内核公式，权重可由模型修订。

技能、记忆和 fact 的可及性可以标成这些态度或投影状态。阶段目前按这个顺序走：劣化（degraded）、模糊（fuzzy）、将被遗忘（near-forgotten）、归档（archived）、长期存储（long-term storage）、遗忘（forgotten）。状态不改字节。劣化、模糊和将被遗忘只降低默认投影里的突出程度。归档和长期存储把指针放到更冷的索引，按版本仍可读。这里的遗忘与现有记忆 tombstone 同向：默认检索不再列出，按 id 仍能读到原文，不是删掉字节，也不是停用单元。R111 仍是另一件事：文字、录音和屏幕帧默认保留。空间压力与显著性下降都是输入，不设硬优先级。空间压力只能促使归档或转入长期存储；是否真正遗忘或删除原始字节，仍由主体结合显著性自主判断。这是助手代定、用户授权。

每个状态引用的是一组原子 awareness，不是一则总 awareness。同一原子可被多个状态复用，并可单独替换、学习。下面是按公开研究拆开的例子，不是清单：

- 距上次取回的主体相对运行时间已经变长（Ebbinghaus 1885/1913 的遗忘曲线：保留先陡后缓；这里的时间只计主推理循环）。
- 取回不完整或没有把握。
- 一项技能许久未被使用。
- 此刻复习可能正合适（Cepeda 等，2006，*Psychological Bulletin*：间隔重复）。
- 刚刚被取回，因而可以重新验证或改写（Nader、Schafe 与 LeDoux，2000，*Nature*：再巩固；McGaugh，2000，*Science*：巩固）。
- 想看原文，而不是摘要。
- 练习了一项，邻近的另一项可能更难想起（Anderson、Bjork 与 Bjork，1994：提取诱发遗忘）。因此「用过」不自动保住旁边的条目。
- 价值低，可以放任，而不是必须处理。
- 存储压力出现。这只是通知，不命令删除。

这些 notice 仍由 watcher 模拟。主体自己决定复习、重新验证、归档或放任。没有任何状态强制下一步。

`forgetting` 与 `humanlike-mind` 的挂法见 [§51.8](cognition-units.md#spec-51-8)：依赖「能遗忘」这一能力，而不是钉死名为 `forgetting` 的包。`taste` 只管旧版本如何默认露出，不规定何时遗忘。二者仍并列、互不包含。

<!-- /migrated-contract: SPEC:51.7 -->

<a id="spec-51-8"></a>
<!-- migrated-contract: SPEC:51.8 -->
## 51.8 提议：按能力依赖，不钉死具体包

依赖写在能力上，不写死具体包。这是已确认的原则。例如 `humanlike-mind` 需要的是「能遗忘」，不是名为 `forgetting` 的包。能力契约前缀用 `cap.`，例如 `cap.forgetting`。这是助手代定、用户授权。不在包级声明互斥。冲突按实际用到的原子能力判断；不冲突就可以并存。`humanlike-mind` 对 `cap.forgetting` 用 `recommended`：缺了记录省略，不报成就绪，也不禁止主体去试。本节沿用 [§45.10](cognition-units.md#spec-45-10) 已有的两个轴，不另造注册表或解析器。强度仍是 `required` 与 `recommended`。匹配仍是抽象能力／主题，或精确单元、合同与版本。代码依赖仍须带版本或带版本的能力合同；awareness、fact 等直觉与内容的依赖仍可笼统或精确（R138）。语义相似不能把未知兼容登记为已满足。改依赖声明是该单元的新 revision，不改写旧解析是否曾经满足。

**能力与包不是同一个名字。** 包和宏包用单元 id，例如预装的遗忘宏包可以叫 `forgetting`。它声明自己 `provides` 哪些能力契约。能力契约用另一套标识，前缀是 `cap.`，例如 `cap.forgetting` 的第 1 版。前缀不得与单元 id 混用。`humanlike-mind` 若需要遗忘，依赖的是 `cap.forgetting`，不是单元 `forgetting`。能遗忘就行。它的其他依赖也同样：默认写抽象能力，不写死某一个实现包。发行版在设计上依赖的仍是 `humanlike-mind` 这个宏包；另外给出一张可改的预装表，写明每种能力最初由哪个单元提供，例如 `cap.forgetting` 由 `forgetting` 提供。拆掉或换掉 `forgetting`，只要另有单元提供同一能力契约，依赖仍然对得上。

硬依赖未解析时，不把该边报成就绪，无关能力继续工作（[§45.10](cognition-units.md#spec-45-10)）。这不等于禁止主体尝试：缺依赖时说明风险和尚未观察到的事实，主体可以调查、适配或试一次（与 #71 第 3 点一致）。试过之后仍以真实结果记账，不把尝试写成已经满足。`recommended` 缺失则记下省略，不冒称已安装。`humanlike-mind` 对 `cap.forgetting` 用 `recommended` 加抽象能力，以符合「不强依赖这个插件」。同类不强依赖的边也同样。

多个提供者可以同时安装，并在所用原子能力不冲突时同时启用。不另造包级 Conflicts。旧内容单元和原始经历都不因多一个提供者而改写；已写出的态度仍指向当时的提供者 digest。跨节点只同步「声明了什么、提供了什么」。每个节点自己的解析状态分开：已满足、已省略或未解析，以及所选提供者在该节点是否启用、是否真能运行。复制成功不是本地已解析。

模型可以学习并修订依赖声明。那是单元的新版本，带理由；旧声明按统一修订方式保留。core 运行时不因此变成可在线学习；更换它仍是整版替换，或按 [§51.4](cognition-units.md#spec-51-4) 在版本链尾追加内容相同的新节点。

取舍只说明为何沿用 [§45.10](cognition-units.md#spec-45-10)，而不是换一套：Debian 的虚拟包与 Provides/Recommends 适合「依赖能力、预装一个实现、允许换实现」。包级 Conflicts 不采用，因为互斥要看实际用到的原子能力。Nix 把依赖钉成存储路径，再现性强，但不适合「能遗忘就行」。Cargo features 仍点名具体 crate，只适合可选依赖，不适合替换实现。OSGi 以服务接口对多个实现，动态绑定，最接近这里的能力契约；它的服务等级不必搬进来。Rust trait 说明「依赖行为而非具体类型」，但是语言层的一致性规则，不是安装解析。因此实现包的版本可以变，能力契约的主版本变了才算对不上；代码边仍按 [§45.10](cognition-units.md#spec-45-10) 钉版本。

<!-- /migrated-contract: SPEC:51.8 -->

<a id="spec-51-9"></a>
<!-- migrated-contract: SPEC:51.9 -->
## 51.9 助手代定（用户授权）

用户授权助手把当时剩下的待决项定下来。下列都是已决，不是待决。运行代码尚未改名或改存储。

- `knowledge_kind` 保留为自由标签。它可以学习，只作提示，不当 fact 与 conclusion 的种类边界。那条边界仍是理由停在何处。
- 上下文贡献枚举 `ContributionKind::Fact` 改为 `SourcedContent`。它是带来源的上下文材料，不是一种独立储存。现有变体注释写的是 “a plain fact with a source”；运行代码里的名字仍是 `Fact`。
- `shenko-mobile` 的 `Fact` 改为 `SnapshotDimension`，`DbFacts` 改为 `DbSnapshot`。它们记录快照各维度是否达到，不声称语义完整，不是认知 fact。运行代码尚未改名。
- `PendingObligation` 并进态度，读作「尚未完成」，不再另做一种循环记录。运行代码里它仍是独立记录。
- 上下文压缩服从 taste：窗口里出现修订过的单元或态度时，默认只露出旧版本数量和最新修改时间，不另定一套披露，也不删掉日志或原始字节。
- 研究库的 `environment_fact` 改为一条态度，不再作为一种 kind。现有字段留到实现时改。

<!-- /migrated-contract: SPEC:51.9 -->

<a id="spec-51-10"></a>
<!-- migrated-contract: SPEC:51.10 -->
## 51.10 读法治理

2026-11-06 的读法治理。不改 [§1](architecture.md#spec-1)–[§47](inference.md#spec-47) 的字节。那些章节里的历史句子继续留在原地。与本节或 [§51.1](cognition-units.md#spec-51-1)–[§51.9](cognition-units.md#spec-51-9) 冲突时，以本节和 [决策 0003](../decisions/0003-spec-governance-2026-11-06.md) 的现行读法为准，不把旧句涂掉。

- [§45.1](cognition-units.md#spec-45-1) 与 [§48.3](architecture.md#spec-48-3) 的旧句把 awareness 写成读过就会遵守的文档。现行读法：awareness 是能力；文档与外部 watcher 的 notice 是会变薄的 harness。[§48.3](architecture.md#spec-48-3) 已有一句取代说明，原文保留。
- [§10.1](durable-state.md#spec-10-1) 把快照写成文件状态，并把身份、原始会话和副作用回执排除在沙箱倒退之外。现行读法：外部副作用不撤回；回滚后 `agent_id` 保持切换前的值；原始会话留在只增日志里，不因回滚被涂掉。WASM 线性内存、执行栈和解释器对象图纳入自有版本化根，不再被「快照不是 CPU 内存」排除在回滚目标之外。一次文件快照仍然不能假装撤销已经发出的消息。
- [§10.2.1](durable-state.md#spec-10-2-1) 的 LayerFS 仍是合格 Linux VM 上的工作区集成。认知状态回滚不用它，只用自有版本化根。
- 上游或操作系统自己的检查（例如打开 Store 前的文件系统类型、设备上的 su 策略）是外部事实。它们不是本节新加的认知 allowlist 或静态 hard deny。
- 主体的判断不用关键词表、人工审批、权限门或静态 hard deny 来强制。真实的账户、设备和操作系统限制仍然是事实。[§48.1](architecture.md#spec-48-1) 已写不把这些事实改写成人类审批。
<!-- /migrated-contract: SPEC:51.10 -->

<a id="spec-57-9"></a>
<!-- migrated-contract: SPEC:57.9 -->
## 57.9 对齐即说明风险与收益

**合同。** 不新增人工审批、产品 allowlist、静态 hard deny（与 [§48.1](architecture.md#spec-48-1)、[§51.10](cognition-units.md#spec-51-10)、[§5.4.4](nodes.md#spec-5-4-4) 用户原话一致）。主体自改（frame、注意力政策、单元、二进制、插件）留下带理由的态度记录；回滚按 [§51.4](cognition-units.md#spec-51-4) 的只增版本链执行，旧字节不覆盖，外部副作用不撤回。真实账号、设备、平台限频与权限继续按事实记录，不改写成审批流程。

**能力契约优先于包名。** 依赖写在 `cap.*` 上（[§51.8](cognition-units.md#spec-51-8)）；多个提供者可共存；主体可在运行时切换提供者而不重启，使用方不感知。

<!-- /migrated-contract: SPEC:57.9 -->

## 默认元感知与元 awareness 的接续

[接续合同](../decisions/0006-meta-perception-continuation-2026-11-11.md)将本主体的原任务、真实行动结果、失败与成本作为普通认知能力可以观察的对象。默认组合可替换；有限自用、带证据的补缺与可恢复修剪沿用本领域的单元、态度和版本权威，不新增高权限元层。安装事实、模型实际使用及效果分别验收。该合同明确区分 Issue #279 的作者摘要与尚未取得的用户原话，不回填来源编号或历史迁移标记。
