# Software-generation handoff


<a id="why-3"></a>
<!-- migrated-contract: WHY:3 -->
## 为什么核心小，而且任何部分都能改

路由器、Android、开发者机器和远端节点有不同资源与原生能力。核心只需要提供组成系统所必需的基础，业务逻辑尽量随插件安装。用户已明确拒绝永久不可修改的 Ring 0；核心与恢复器也应能通过保留可启动版本、迁移数据、验证新闭包和交接所有权被替换。见 [§7](plugins.md#spec-7)、[§9](self-update.md#spec-9)、[§22](nodes.md#spec-22)、[§41](architecture.md#spec-41)。

这里需要的是恢复能力，不是一块永不改变的代码。内存里的 cleanup callback 能清理生命周期，却不能证明断电后一致，也不能撤销已经发出的 QQ 消息。持久任务、intent、receipt、效果未知态和实际存储承诺因此必须明确。它们描述世界已经发生了什么，不是给每个动作增加人类审批。见 [§10](durable-state.md#spec-10)、[§36](durable-state.md#spec-36)–[§37](context-policies.md#spec-37)、[§41](architecture.md#spec-41)；REF-CORDIS、REF-DURABLE。

<!-- /migrated-contract: WHY:3 -->

<a id="why-9"></a>
<!-- migrated-contract: WHY:9 -->
## 为什么第一版就要能继续开发自己

用户已经拥有开发机器和 tmux 内的 coding agents。QQ 来信能交到主体，再由 child 找到正确 seat、观察真实 TUI、执行修改、验证候选、迁移并恢复运行，才形成可用的自开发闭环。仅生成一个 patch，或者只读到去掉高亮的终端文本，都没有达到这个目的。见 [§17](remote-seats.md#spec-17)、[§35](self-update.md#spec-35)、[§41](architecture.md#spec-41)。

当观察能力不足时，主体应能意识到缺少什么、委派补插件，再用原任务验证效果。源码、手册与设计哲学要随运行版本可查询，让它既能回答自身问题，也能有依据地改造自己。见 [§38](cognition-units.md#spec-38)。

<!-- /migrated-contract: WHY:9 -->

This contract covers [§9](self-update.md#spec-9)、[§10](durable-state.md#spec-10)、[§35.8](self-update.md#spec-35-8)、[§41.12](self-update.md#spec-41-12) and [§44](plugins.md#spec-44). Software selection,
local process ownership, node election authority, and external effects are
separate facts. A candidate build or source implementation is not runtime
qualification. No builds, smoke launches, migrations, activations, model calls
or rollback proofs were executed during this source phase.

## Interfaces and authority

`shenko-update` owns `BuildProvider`, immutable candidate descriptions,
`UpdateService`, `ActivationRecord`/`ActivationSelector` integration, and the
`update.control` native handler. The binary owns installation control transport
and runtime assembly. `update status`, `prepare`, `activate`, `rollback` and
`recover` need no model. A model child uses the same native Cordis registration,
not a mandatory self-development pipeline and not a human approval queue.

`PrimaryClaimSource` is supplied by the composition root. Its node adapter uses
`Node::committed_primary_claim`, `fencing_token`, `check_fencing`, `is_primary`,
and actual branch facts. A missing source is not authority. An explicit local
installation without configured membership/peers, or the exact local
availability branch matching subject/head/node identity, can update its own
local slot without claiming global quorum. A nonlocal committed primary is not
silently converted into an unfenced single-node primary. The scoped authority
snapshot is retained in the recovery closure and re-read before selection.
Node election epoch, selector epoch and subject controller epoch are distinct.

After genesis, `Store::installation_id_for_subject` obtains stable installation
identity from the verified immutable `genesis.bootstrap_event_ref` or an explicit
provisioning record, not the current ledger writer ID. Fresh CLI genesis already
seals that identity in the event; a separate `bootstrap_records` projection is
not required for restart. When both representations exist, their installation
and subject bindings must agree; missing or conflicting identity is an error,
not a new installation. A staged copy acquires a distinct current ledger writer ID before its first
local mutation; historical `commit_log.ledger_id` values and embedded source
CommitRefs are not rewritten. The installation, owner, subject and genesis do
not become new identities.

## Build, copies and typed migration

`CargoBuildProvider` builds a real candidate; prebuilt input is content-checked.
The current executable is also verified against its committed route and retained
as an executable recovery artifact. The candidate gets a separate SQLite backup
and copied state. Installation-level `versions`, shared CAS, node database and
control endpoint are excluded from recursive root copying. Slot names include a
digest of the generation; path components cannot escape into the parent tree.

Migration uses Store's declared cognitive recovery archive and its typed
mandatory-fact/closure/conflict rules. It does not copy arbitrary SQLite rows,
ignore unknown columns, overwrite conflicting task/effect units, or splice
independently allocated commit sequences. Retained archive files provide exact
source payloads. Blocking gaps/conflicts refuse selection. A narrow
same-installation `copy_paused_activation_boundary` validates and installs the
local clean pause request/head/successor before typed import; peer archive import
cannot manufacture this authority. The same handover's own snapshot is the
only export admitted while that boundary is paused
(`local_boundary_preserved`): a peer receiver has no boundary copy to anchor
a paused head to, so peer exports still refuse.

A future incompatible format requires its actual versioned reader/migrator;
this code's same-reader archive path must not be advertised as a universal
breaking-schema converter. Required decoder closure and compatibility evidence
remain obligations of the supplied candidate.

`launch_smoke` actually executes the selected candidate artifact against its
copy. A missing/malformed manifest, failed command or failed report is an error.
The current bounded smoke command opens and inspects Store, subject/head/step,
selector and managed state. Those checks alone are **not** the complete SPEC
message/tool/plugin/global-concurrency qualification; integrated runtime smoke
must supply that evidence rather than treating process startup as full success.

## Live control and the final writer boundary

The binary's `update_control` module binds a loopback ephemeral listener and
publishes an installation-private capability descriptor. Requests are bounded
JSON and authenticated with that local capability. The CLI first attempts the
existing exclusive `RuntimeOwner` directory lock. A failed RPC does not prove
inactivity and never authorizes an empty registry. A proven stopped installation
can legitimately have zero live participants.

A live activate/rollback request:

1. Freezes an immutable helper request in CAS and a private durable file,
   naming installation, subject, original selector/generation/epoch/route,
   source slot, command and actual helper executable digest.
2. Requests the subject's real finite boundary. Registered native-worker,
   REPL interpreter, Playground-service and inference-domain handles quiesce;
   their actual receipts and unresolved debt enter the activation record.
3. Spawns a **finite** helper without inheriting the old owner's CLOEXEC lock,
   acknowledges only `awaiting_old_process_retirement`, then stops the old
   runtime. This acknowledgment is not successful activation.
4. The helper waits for and verifies exclusive `RuntimeOwner`. Only this
   process-lifetime boundary proves old local channel/UI/node/Store writers
   cannot append beyond the final watermark. Cancellation alone is not proof.
5. After validating the frozen source identity again, the helper performs final
   typed catchup and the selector CAS. Candidate selector, activated record,
   new-generation clean successor and new controller epoch commit together.
   Old invocations/effect debt retain their original identities.
6. The retiring copy mirrors the committed selection and fences its old head;
   the launcher pointer follows. A versioned, durably replaced pointer can name
   either the installation's original root or one declared version slot.
7. The helper verifies the selected executable and `exec`s it while transferring
   the **same locked directory file description**. The successor adopts and
   verifies it before opening Store and restores CLOEXEC for later subprocesses.
   There is no release-and-spawn ownership gap or second infinite primary.

Native `update.control` returns its armed operation before making this request
through a detached controller, so the native caller can finish and cannot wait
for its own boundary. Accepted/lost responses stay nonterminal. Reconciliation
never reruns an effect: it requires the exact original activation/rollback,
expected selector revision and a real persisted successor launch receipt before
reporting success. Different observations have distinct content-bound E1 keys.

## Resource receipts

The registry is asynchronous. A partial quiesce failure resumes all attempted
participants in reverse order, including the failing participant; resume failures
are reported rather than hidden. Actual native workers are gracefully joined;
a timeout retains their handles instead of dropping a `spawn_blocking` guest's
outer future. REPL journals survive interpreter stop. Playground captures process
identity before termination, closes admission, bounds lock acquisition and waits
for its owned Linux process group before releasing its runtime claim. Missing
resume descriptors or CAS payloads fail explicitly.

Broker quiesce closes submission under the actual accounting lock, waits for
queued/running local provider readers and durable receipt writes, and snapshots
request bindings/payloads, resource-domain identities, KV, external reservations
and quarantined charges. It never equates HTTP cancellation or `active_count`
with remote termination. Unknown requests remain debt. The process helper's
exclusive-owner barrier is still required before final catchup.

The controller's own late E1 observation routes to the selected Store while
retaining its original producer/attempt identity. If old process teardown wins
that race, the successor reconciles the original durable T1 from selection and
launch facts; an RPC disconnect is never fabricated into terminal success.

## Failure, rollback and recovery

`finish_committed_handoff` reads the candidate's actual selector and atomic
activated record. It can finish an interrupted mirror/fence/pointer publication;
it does not decide who won from the last exception or a PID. Before a selection
commit, compensation may resume the same old controller. After a commit, old
admissions remain closed and recovery completes the selected route.

Rollback uses the retained previous slot/executable and is a new activation with
an increasing selector revision/epoch. Current facts pass through typed recovery
into the previous copy; conflicts and unreadable formats are retained/refused,
not treated as successful import. The previous executable is smoke-launched
again against that copy. The rollback record names its original activation and
captures the **current** scoped authority, not an obsolete earlier incarnation.
A helper error re-reads committed selection before restoring the previous
closure. New facts, revocations, tombstones and external debt cannot disappear
because a workspace or software pointer moves backwards.
Cognitive and home rollback follows [§51.4](cognition-units.md#spec-51-4): append a new node whose
content matches a historical node, keep the log, and keep `agent_id`.
This section describes the current software-generation activation. It is
not the definition of that rollback, and it does not allow deleting the
commit log.

Launch receipts are durable `activation_launch_receipts` rows bound to launch ID,
subject, selector revision/epoch/generation/origin and actual slot. Retrying the
same launch identity with different binding fails. A selected-but-not-yet-started
helper result explicitly says `launch_succeeded: false`.

## Integrated qualification still required

Parent integration must bind managed home/memory/REPL/Playground paths to the
selected state copy using their owners' actual rebind contracts; an installation
root mount cannot masquerade as migrated candidate state. It must include the
new launch facts in Ledger's declared schema/closure membership, reconcile the
stable installation callers, and qualify incompatible-schema migrator/reader
closures. These are not proven by the current source or local SQLite equality.

The serialized native receipt recipe is: run real A; prepare/build B; observe
actual candidate smoke; hold an in-flight tool and unknown inference debt; request
activate through CLI and separately through the real native route; verify old
owner retirement, exact CAS, old-writer fence, transferred lock and B's durable
launch receipt; commit new messages/task/effect/revocation facts on B; rollback
through the real retained reader; verify A's new revision/epoch/launch receipt and
preserved B facts/debt. Interrupt before/after candidate CAS, mirror, fence,
pointer publication and successor exec; reopen and reconcile each state without
blind replay. Native/model/protocol/platform observations and power-loss claims
require their own real receipts. None was executed in this source phase.

<a id="spec-9"></a>
<!-- migrated-contract: SPEC:9 -->
# 9 Nix-like 包与自我改进

首版升级具体执行顺序、ActivationRecord、水位、旧调用回执和回退后的新增事实接续见 [§41](architecture.md#spec-41)；本章说明包与自我改进目标。所有代际实现可替换，当前有效代负责完成交接。

包描述至少记录名称/版本、内容 digest、来源、目标 ABI、需要的 imports、依赖闭包、入口、可用设备 profile、权限需求和迁移信息。不可变 store 以内容寻址，环境 generation 引用一份锁定闭包；选用的包激活合同通过原子选择当前 generation 发表，已有调用仍绑定原闭包；GC 保留活跃、健康、factory、候选与人工 pin 的根。细粒度包可以交付 provider/插件代码、感知、awareness、Skill 或其组合。各单元有独立身份与可查内容；provider 声明的必需依赖自动解析安装，代码带版本约束，直觉的依赖允许笼统能力或精确对象。实际结果锁定到工件与版本，详见 [§45](cognition-units.md#spec-45)；不另建一套 awareness 包管理器。

这些是必须达到的 Nix-like 语义，不等于承诺把完整 Nix 在 WASM 内跑通。开发/构建节点可使用真正 Nix 构建工具链，设备消费可复现产物；package provider 保留替换为原生 Nix 的可能。[Nix profiles 参考](https://nix.dev/manual/nix/2.35/package-management/profiles)

安装与更新由子 Agent 执行。主体或受委派任务可使用 awareness 能力识别依赖、兼容性或恢复方面的注意事项，再组合包解析、构建、检查、试用、迁移、修复和恢复能力；选择哪些步骤与顺序属于当前任务策略。涉及软件代际交接时使用相应 Activation 合同。是否自动更新和采用哪种策略由 Agent 自主决定，无逐项人类批准。真实能力、操作授权和已发生效果分别记录；新包的文字说明不会自动授予它外部账户权限。

<!-- /migrated-contract: SPEC:9 -->

<a id="spec-9-1"></a>
<!-- migrated-contract: SPEC:9.1 -->
## 9.1 任何部分都可自主重写

自我改进子 Agent 可以重写工具、Skill、memory 策略、人格和 system prompt、主体 harness、调度算法、插件管理器、UI、包/版本系统、Rust kernel、启动器与救援程序。核心源码和二进制均不是永久禁区；Rust 小核心仍是现行产品要求，新实现应满足。主体提出改进目标并委派，worker 构建和验证；主体本身仍没有直接文件或代码执行工具。

自我改进按目标和当前策略记录基线、输入、成本、状态格式、工件及适用恢复方案。awareness 能力可以帮助识别应注意的问题，主体或受委派执行者据此组织“提出→构建→验证→切换→观察→保留或回滚”，也可复用证据、跳过无关步骤或换另一流程；这串步骤不是 awareness 的定义或内核统一流水线。D7 的磁盘优先、breaking-change 迁移和最后交接要求仍适用于对应自迁移任务，见 [§9.5](self-update.md#spec-9-5)、[§41.12](self-update.md#spec-41-12)。真实外部权限和管理员登记是独立事实，不能靠修改程序伪造。

<!-- /migrated-contract: SPEC:9.1 -->

<a id="spec-9-2"></a>
<!-- migrated-contract: SPEC:9.2 -->
## 9.2 版本交接与行为合同

候选代码不直接覆盖唯一正在运行的实例和唯一救援副本。保留旧程序与旧数据可运行，另建候选闭包和迁移副本，烟测启动、读写、消息/任务恢复、全局并发和工具行为；验证后通过新 epoch 与路由交接切换，失败回到旧闭包。新旧格式可不兼容，恢复靠旧副本和已对账的增量，而非强迫旧程序理解所有新格式。详细插件协议见 [§9.5](self-update.md#spec-9-5)。

kernel 和 rescue 可以分步相互替换，但不能同时擦掉所有旧可启动版本。factory 指定版本可更新；初始交付版本及必要读取工具默认归档保留。保留策略可按 D8 由主体自主调整和迁移，但当前迁移尚需的旧副本、未完成任务和恢复根不能被普通 GC 当垃圾。内容对象不可变是版本存储规则，不是永久冻结某份代码。

“一条主体、主体不直接操作文件、人格尾置、管理员手动登记、配置的全局上限”等是用户现行产品合同。实现这些合同的代码可以完全更换，新版本需继续满足合同；若用户随后改变合同，规格与验收随之版本化。不得把任意代码重写当成用户已经同意取消原要求。验证器、评估样本和发布策略本身也可改，但要由当前有效版本完成交接并保留独立回退证据。

有限测试不能数学证明任意自改程序永远正确；尤其全部可信实现都可替换时，长期保证来自可审计的版本、故障隔离、可恢复状态和持续验证，不能宣传“任意自改也绝不损坏”。候选人格/主体提示词可做有限离线回放或受控影子评估，不生成第二个对外自主主体。

<!-- /migrated-contract: SPEC:9.2 -->

<a id="spec-9-3"></a>
<!-- migrated-contract: SPEC:9.3 -->
## 9.3 构建位置与生命周期清理

运行设备不必同时是构建设备。BuildProvider 可在本机 WASM、已授权远端构建节点、原生容器或交叉工具链中构建候选；路由器/Android 不必常驻 rustc、LLVM 或 npm 构建树。构建任务记录 source/recipe/toolchain/target digest、依赖闭包、验证结果与制品身份，部署设备再验证其实际兼容性。远端不可达只会阻碍此次更新，不取消任何部分可改的能力。[Cargo target 构建](https://doc.rust-lang.org/cargo/commands/cargo-build.html)、[Nix 远端构建](https://nix.dev/manual/nix/2.35/advanced-topics/distributed-builds)

设备需要容纳候选、上一代恢复闭包与必要迁移空间；不能只计最终新 binary。硬件 watchdog 是实际设备支持时的可选 provider，不假定所有 Android/路由器都能使用。内核、默认插件与编译工具链的大小分别测量，不能用未经验证的“<10MB”概括整个栈。

cordis-rs 的 effect/generation 清理适合撤销进程内订阅、任务和服务绑定，但不是断电后的事务日志。正常卸载、cleanup 失败、进程强杀与持久恢复分别处理；`ctx.effect` 无法自动撤回已经完成的 SSH 写入或发信。程序需要 operation ledger 与恢复协议，不能把 cleanup callback 当作所有副作用的逆操作。[effect 源码](https://github.com/dshbox/cordis-rs/blob/1754062cef67cfc119fc7a2084c0321a231b9092/crates/cordis-core/src/effect.rs)

Pi Durable 的任务状态、原子提交、持久取消和补偿作为独立恢复架构参考，见 [§10.6](durable-state.md#spec-10-6)；可恢复业务补偿应实际实现，不止记录“cleanup 做不到”。

<!-- /migrated-contract: SPEC:9.3 -->

<a id="spec-9-5"></a>
<!-- migrated-contract: SPEC:9.5 -->
## 9.5 磁盘优先的 breaking-change 迁移插件

D7 已裁决：接受额外磁盘成本，允许新版本破坏旧 schema/ABI；主体已知道升级提议并可自主决定是否执行。SelfUpdate/MigrationProvider 组织旧副本继续运行、迁移、烟测和最后交接自身，不要求用户在线批准，也不把向后兼容做成永远不可解除的要求。

| 阶段 | 要完成的工作 |
| --- | --- |
| prepare / copy | 保留旧版本 A 的软件闭包、可读数据和恢复工具；另建 B。自一致水位 w0 复制并转换，A 继续运行，新事实进入可追赶 journal |
| catch_up | 按水位追赶增量，记录转换版本、对象闭包和已迁移位置；双格式视图不能同时无协调地写同一底层文件树 |
| smoke | B 实际启动，核对数据/索引、工具、插件、模型调度、消息和任务恢复；重放测试不再次发送真实 outbox，不把启动成功当全部迁移成功 |
| handoff | 在主体轮次边界停发 A 的新决定，追平 w1；移交 inbox/outbox、在途任务、原始记忆/人格/灵魂/兴趣、身份和删除标记，以及 active/reserved/cancel-pending/unknown 请求，再切 incarnation 与路由 |
| observe / retire | 观察 B，保留 A 可启动及其旧格式数据。B 未产生新事实时可直接退 A；已产生新事实时先导入和对账后接回，不能回滚掉切换后的经历 |

新格式增量需保留能解释它的 reader/migrator 与来源；journal envelope 本身也可升级，但交接时必须有确切的版本转换路径。内核、救援程序、迁移器、选举器与验证器都可替换，不设永久 Ring 0。更老副本可按 [§12.4](memory.md#spec-12-4) 自主归档/回收，但当前迁移和恢复还依赖的闭包、对象、解码器和密钥引用不能被普通 GC 提前删除。

多节点升级记录协议/数据兼容矩阵；不兼容节点通过明确的桥接迁移或暂存分支交换事实。一个节点烟测成功不代表所有节点都完成了协议升级。所有构建、迁移和烟测由 child/provider 执行；主体仍只作决定和委派，候选模型测试仍进入实际 broker。

<!-- /migrated-contract: SPEC:9.5 -->

<a id="spec-35"></a>
<!-- migrated-contract: SPEC:35 -->
# 35 首版开发者发行：可继续开发自身的完整闭环

<!-- /migrated-contract: SPEC:35 -->

<a id="spec-35-1"></a>
<!-- migrated-contract: SPEC:35.1 -->
## 35.1 首版目标与既有约束

首版部署到开发者的机器后，应能接入已有 NapCat，经 QQ 接收任务；主体根据记忆找到现有 tmux 中的 Claude Code、OMP 或其他已接入 Agent seat，委派 child 操作它，开发本产品的插件、harness、UI 或 Rust 内核，验证候选、迁移状态并自主切换到新版本。没有外部指令时，主体也可以主动发现问题、提出改进并走同一条流程。交付目标包括“新版本已经接过自身”，不能仅以生成补丁、提交代码或回复“修改完成”代替。

沿用 [§5](subject-runtime.md#spec-5) 的持续串行主体：主体现实操作仍经 child 与插件执行；主体可与 child 持续对话、改变方向、选择模型、编写其 system prompt。原来的 coding agent 是执行工作位，不成为第二个人格主体。正常运行时持续推进主循环；等待推理资源或进行必要的代际交接不被改写成低频 Tick 产品。

内核、恢复程序、迁移器、插件及其策略均可以改写。首版开发者发行必带一组能完成开发闭环的插件，并不把这些插件编入不可替换核心。卸载自开发和 coding 选件后，框架仍保有个人主体、消息、记忆与委派能力。

<!-- /migrated-contract: SPEC:35.1 -->

<a id="spec-35-4"></a>
<!-- migrated-contract: SPEC:35.4 -->
## 35.4 一次真实自开发任务

1. QQ 事件进入持久 inbox，保留发送者、渠道、消息 ID、附件 handle 与原文。主体在自身序列中接收它，并结合既有开发委托和记忆形成任务。

2. SelfDevelopment child 解析当前 source revision、运行 artifact、仓库位置、工作区、构建方法与 seat 绑定。对已有现场先观察，不默认开发者的工作树干净，也不覆盖尚未提交的工作。

3. 在独立分支／worktree／工作区形成候选。Git worktree 用于分开工作树；它不是宿主权限沙箱，也不自动隔离 Git 公共状态或网络效果。[Git worktree](https://git-scm.com/docs/git-worktree)

4. Seat child 向已确认的 Claude／OMP／其他适配器提交任务，持续处理进展、问题、工具选择与权限请求。普通实现选择和既有授权内的操作由主体或获委派 child 自主决定；拿不准时按 [§17.3](remote-seats.md#spec-17-3) 主动寻找可达用户。

5. coding agent 交付 diff、构建产物与原始证据。验证 child 对照任务目标和当前验收合同运行相关检查，保留失败结果与比较基线。代码作者和验证者可以用同一模型，但验证动作与证据应独立；需要改验收合同时记录理由与版本，不能用删除失败检查冒充能力进步。

6. 主体根据证据选择继续修正、归档候选或升级。安装、代理、上游更新与自身升级均可自主决定，不添加逐次人类确认门槛。

7. 更新插件执行 [§9.5](self-update.md#spec-9-5) 与 [§35.7](self-update.md#spec-35-7) 的迁移／交接。运行回执串起 source revision、build recipe、artifact digest、plugin lock、数据格式与当前 generation；只有实际激活并验证了目标产物，才记为完成升级。

8. 新版本延续原主体的姓名、人格、灵魂、兴趣、原始记忆、未完任务与消息，再通过同一 outbox 发布准确结果；其中会话 child 可使用 [§5](subject-runtime.md#spec-5)、[§17](remote-seats.md#spec-17) 已限定的 ReplyGrant。

“自我进化”在首版的可验收含义，是自主发现需求、开发能力、验证、采用并继续工作的闭环。某个候选确有改善，需要具体任务的质量、正确性、成本或兼容性证据；模型对自己的评价不单独构成证明。

<!-- /migrated-contract: SPEC:35.4 -->

<a id="spec-35-7"></a>
<!-- migrated-contract: SPEC:35.7 -->
## 35.7 自升级必须完成运行中的交接

首版直接实现 [§9.5](self-update.md#spec-9-5) 的 prepare/copy → catch_up → smoke → handoff → observe/retire。A 继续处理事情，B 在副本上迁移和有限烟测；B 不向真实 QQ 发送回放结果，不操作真实 seat，也不同时开启第二条无限主体 loop。需要真实外部探测时使用专门 operation、现行授权和 [§36](durable-state.md#spec-36) 实际效果声明。

SQLite 复制采用一致备份或满足条件的 snapshot 机制。运行中的主 db 文件拷贝不能自动代表完整事务状态，迁移还要保留 WAL/对象闭包等关联条件。[SQLite Backup API](https://sqlite.org/backup.html)

在明确轮次边界，A 停止新决策、收敛或移交在途所有权，追平最终水位；将 inbox/outbox、任务、Seat decision、外部 operation、原始记忆与推理占用交给 B，再切当前 epoch。独立 ingress 与 seat companion 可继续工作；它们本身也可升级，但必须单独交接其接收／写入权。交接期间可以有有限服务停顿，不能伪称任意崩溃下都无延迟。

Rust 内核、启动器、恢复程序也走候选产物与代际替换，不要求原地热改正在运行的二进制。保留能启动的旧闭包和对应数据／解码器。B 已产生新事实后若回退 A，先保留并导入/对账这些事实；软件回退不使后来发出的消息、远端操作或新收到的请求消失。恢复执行不依赖模型正好可用，策略和程序本身仍可被后续改写。

<!-- /migrated-contract: SPEC:35.7 -->

<a id="spec-35-8"></a>
<!-- migrated-contract: SPEC:35.8 -->
## 35.8 能力缺口必须能驱动实际开发

必测情境：Agent 发现当前终端观察无法可靠区分 TUI 高亮，先报告具体缺失证据，委派 child 搜索现有能力；若没有合适插件，开发增强 TerminalObservation／Seat adapter，用记录的终端样例和真实交互验证，更新能力索引与用法，再继续原任务。不能只是写出一个插件文件后从未加载、从未使用，也不能靠猜按键完成首次自开发。

首版必须已有至少一条可工作的控制/执行/文件/构建/安装路径，构成初始能力闭包；“它会自己装所有东西”不能取代这条起点。此后可自编插件、外部 companion、工具 wrapper 或所需 WASM 移植，实际运行位置与恢复边界按 [§36](durable-state.md#spec-36) 重新绑定。

<!-- /migrated-contract: SPEC:35.8 -->

<a id="spec-35-10"></a>
<!-- migrated-contract: SPEC:35.10 -->
## 35.10 首版自动验收

| 编号 | 必须出现的真实证据 |
| --- | --- |
| V1-01 | QQ 发起一项自身改进并附截图，主体找到已有 tmux 工作位；child 完成修改、验证，QQ 得到准确进展与结果 |
| V1-02 | 已有 Claude 与 OMP TUI 分别完成观察和交互；高亮、同名选项、中文宽字符与 resize 不被纯文本清洗掉 |
| V1-03 | 结构化托管会话能够区分接收、完成、后台未结束；已有 TUI 不被虚报为 SDK 已接管 |
| V1-04 | 在开发者已授予的范围内自主回答远端问题/权限；过期、被人先回答或已退回 shell 的问题不会被继续输入 |
| V1-05 | 发现能力缺口，自行开发、验证、安装一个新插件，正确发现并使用，再继续原任务 |
| V1-06 | 独立工作区保留开发者原有未提交修改；source/build/artifact/running generation 能相互核对 |
| V1-07 | 插件升级与至少一次 Rust 核心候选替换成功接过原主体；原任务、记忆、人格与兴趣连续 |
| V1-08 | 候选启动失败、迁移中断或健康失败时可恢复；切换后已经发生的新事实仍保留并对账 |
| V1-09 | 主控重启期间既有 coding agent 可继续；重连先核对现场，不重复提交同一开发任务或确认按键 |
| V1-10 | 入站重复、断线缺口、发送成功但回执丢失分别处理；unknown 不被伪造为失败或自动重发 |
| V1-11 | 四流真实上限和 C=1 均能推进主体及 child；包含外部 coding agent 递归请求和候选烟测，未纳管请求如实显示 |
| V1-12 | Budget Compact 与 Snapcompact 分别安装/替换；两者的任务续接与原始会话寻址有效，模型不支持的能力诚实降级 |
| V1-13 | [§36](durable-state.md#spec-36) 的挂载边界、外部效果、取消/补偿与版本恢复验收通过；不能用“沙箱恢复成功”代替整项任务恢复 |
| V1-14 | 无 QQ 新消息时主体仍持续推进自主兴趣或维护；默认开放聊天、管理员手动设置与实例姓名独立同时成立 |
| V1-15 | [§37](context-policies.md#spec-37) 的 hook 兼容与 [§38](cognition-units.md#spec-38) 的自解释能力随首版交付；插件开发会使用正确版本资料，变更注入顺序不会绕过主体/效果/推理合同 |

上述是待实现的首版技术验收，尚未执行；通过后按已授权目标自主推进，不默认等待人类逐项批准。后续语音、全 OS Computer Use、多节点、更多平台、REPL 与完整研究策略继续保留产品范围；阶段顺序见 [§29](verification.md#spec-29)，开发规范见 [§40](../governance/development-contract.md#spec-40)，具体执行合同见 [§41](architecture.md#spec-41)。

<!-- /migrated-contract: SPEC:35.10 -->

<a id="spec-41-12"></a>
<!-- migrated-contract: SPEC:41.12 -->
## 41.12 自主升级与跨代交接

本节针对实际软件 generation／状态迁移，保留用户 D7 所选的磁盘优先与旧副本持续运行的交接合同。`ActivationRecord.phase` 描述此版本交接协议的状态，不是每次配置、每项 awareness 或每次思考都要经过的工作流。普通 hot reload、直接配置修改及未选择迁移的动作按各自合同执行；未来交接实现可版本化替换，但实际选择点、未决效果、资源占用及承接责任仍需可解释。

### X05. 激活选择必须成为可恢复的数据 `[S]`

~~~text
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
  state_ref: ObjectRef,
  origin:
    { kind: "genesis", genesis_ref: GenesisRef } |
    { kind: "activation", activation_id: UUID },
  route_authority_ref: ObjectRef
}
~~~

Selector 的成功条件更新是激活的线性化点；磁盘上的 executable symlink、进程 PID、Web UI 中显示的版本都不取代它。`ActivationStore` 提供满足该部署 profile 的持久 compare-and-swap。首次初始化允许预期 selector 不存在的条件写，origin 指向 genesis；默认单机与 root/head/step 1 在同一 L 事务建立，详见 [§44](plugins.md#spec-44)。已有实例换代时 origin 指向相应 ActivationRecord，不能以 bootstrap 覆盖既有 selector。单机 SQLite 的条件更新不自动构成多节点共识。

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

<!-- /migrated-contract: SPEC:41.12 -->

<a id="spec-41-15"></a>
<!-- migrated-contract: SPEC:41.15 -->
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

<!-- /migrated-contract: SPEC:41.15 -->

<a id="spec-57-10"></a>
<!-- migrated-contract: SPEC:57.10 -->
## 57.10 自迭代闭环

**合同。** 主体改进自己的路径统一为：写候选 → 沙箱对照试验 → 采纳/撤回 → 下一个有效边界生效。适用范围从 awareness 扩展到 frame、idle、注意力政策与 child 角色框。core 运行时本身不被在线改写：整体替换或回滚（[§51.4](cognition-units.md#spec-51-4)）。

**child 角色框。** [§5.3](model-children.md#spec-5-3)、[§6](subject-runtime.md#spec-6) 说可自由写 child 的完整 system prompt；今天只能写尾巴（`model_child.rs:92-105` 的 `ROLE_CONTRACT`）。child 角色框成为可修订内容后，主体才能真正"派一个不同的人"。

<!-- /migrated-contract: SPEC:57.10 -->
