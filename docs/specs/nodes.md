# Node membership, election and authority — functional contract

**内核定位。** 参考插件：成员、选主、同步与看门狗。[内核合同](kernel.md)只提供[内核 1](kernel.md#kernel-1)的围栏代号；单机部署不依赖本文的任何多节点机制。经历合并遵守[内核 3](kernel.md#kernel-3)，安装单元的事实遵守[内核 8](kernel.md#kernel-8)。


<a id="why-7"></a>
<!-- migrated-contract: WHY:7 -->
## 为什么多节点仍然是同一自我

自我可以跨设备延续，设备各有真实模型、provider、凭据和操作能力。新节点确认更早建立的可信自我后，撤销自己的独立身份并开始同步。身份来源与主节点位置分开，主节点可以把不同 subagent 交给适合的节点；已授权的 SSH、root Android 屏幕/Shell、Linux/macOS/Windows 工具因此成为同一主体可使用的能力。见 U57/U60–U61。

连通域内保持一个明确主节点；断连后在剩余可达节点中递归选择最聪明且实际能访问 LLM 的备用。云 API 也能提供智能能力，不要求每台机器运行同一大模型。完全孤立的智能节点可独立生活，重连后合并经历与任务。承认分区事实并保留原始结果，才能避免把同步变成丢失经历或重复执行。供应商隐私与实际数据去向也必须可感知，不能把远端调用误当本地处理。

所有智能都失效时，日志、经历保存和凭据授权的外部读取仍应工作；甚至可以从磁盘镜像离线提取，再由更强节点恢复与合并。这样复活不依赖失效模型最后成功生成一份总结。原始数据、版本和来源提供依据，缺失的密钥或损坏的事实不能靠推断伪造。见 U62。

<!-- /migrated-contract: WHY:7 -->

[§5.4](nodes.md#spec-5-4)、[§22](nodes.md#spec-22)、[§38](cognition-units.md#spec-38)、[§41](architecture.md#spec-41)、[§44](plugins.md#spec-44); C17.
The sections below describe the implemented control/runtime boundary at
the 2026-11-15 continuation source through `58b30ba`, plus the separately
identified component/follower source increment on 2026-11-16. Durable original-plan admission,
the Store authority bridge and real branch continuation are wired into startup
and runtime reconstruction. Four integrated runtime/branch fault journeys and
the existing owner-lock regression passed at `60e9963`; see the
[source-bound verification record](../evidence/readiness-continuation-20261115.json).
The product
requirements beginning at [§5.4](#spec-5-4) remain the acceptance contract.
Passing a Node protocol journey does not, by itself, qualify runtime takeover
or canonical Store state replication.

## What the control log decides

A connected-domain primary requires a committed member configuration, durable
prepare promises and quorum persistence of the actual primary control value.
A vote certificate alone does not establish that persistence. The implementation
is a sequential Paxos-style prepare/accept log, using the highest accepted value
rule in [Paxos Made Simple, §§2.2–2.5 and 3](https://lamport.azurewebsites.net/pubs/paxos-simple.pdf).
It is not a Raft implementation.

The fault model is authenticated, non-Byzantine peers with durable SQLite
storage, process crashes, and lost, duplicated or delayed messages. A response
must name the Node reached through the configured authenticated route. Its
acknowledgement is not an independently signed Byzantine quorum proof. There
is no wall-clock authority lease and no inference of liveness from a stored
role, model name, operating system or count of replicated processes.

| Durable fact | What it establishes | What it does not establish |
| --- | --- | --- |
| Member configuration and primary control value | The configured quorum chose this exact control value and retained it before acknowledging. | Continued peer reachability or permission for every future Store write. |
| `CommitAdmission` and its complete CAS closure | This exact frozen plan owns its conflict keys and write aliases in control-log order; required read positions were unconsumed at admission. | That a Store applied the plan, or that independently executing it elsewhere produces the same state. |
| Local Store transaction and receipt | The existing Store head/task/epoch checks passed and the local mutation committed. | A physical provider effect occurred exactly once, or another ledger materialized identical rows. |
| External observation or terminal receipt | The original attempt has this retained evidence, with its original identity and outcome. | A new dispatch permit, current controller ownership or permission to overwrite conflicting evidence. |

Membership is `MemberConfiguration { members, joint_new, revision, term,
digest }`. One node identity is one voter. A majority of every configured set
is required: during a joint transition, old AND new. Joint and Final both
require their governing old/new quorums. Capability and model availability may
inform candidate preference; neither changes membership or grants authority.
No OS, named model or local-LLM requirement is introduced.

## Durable prepare, acceptance and recovery

The current additive Node storage schema is 10 and election wire schema is 3.
`crates/shenko-node/src/control.rs` owns the following sequence:

1. The proposer names the exact committed prefix and its governing
   configuration. An acceptor persists its promise before returning a positive
   prepare response, including its highest accepted next-slot value. One
   proposer can hold a term; a competing proposer must advance the term.
2. A prepare quorum fixes the eligible next value. If any promise reports an
   accepted value, the highest accepted ballot wins. A successor must recover
   another node's original primary, configuration or admission unchanged when
   that is the selected value; recovering it does not elect the successor.
3. Acceptors check the prefix, proof, value and complete referenced object
   closure. They persist the accepted value and their exact acknowledgement
   in one SQLite transaction before responding. Acknowledgements bind slot,
   value digest, prefix, ballot and configuration.
4. Only an actual acceptance quorum permits a learned commit. Disseminating
   that commit can fail after the last acceptance; the next proposer recovers
   the durable accepted value. Losing the proposing process does not turn its
   original chosen value into a fresh proposal owned by the successor.
5. A stable proposer may reuse a prepare only when the retained intervening
   committed run belongs to the same ballot. A recovered configuration or
   an incompatible run requires a new prepare. A candidate behind a reachable
   quorum imports the verified missing suffix and campaigns again.

Committed values are immutable. A value's identity binds index, original term,
kind and payload; recovery can produce another valid acceptance certificate
without rewriting the original committed copy. Late acknowledgements may teach
an earlier chosen value, but cannot lower the durable term or restore an old
primary's current authority. Refused divergences remain evidence.

Schema-1 control history and its original vote certificates remain readable as
legacy facts. They cannot create a usable primary fence merely by being read.
Admission commands did not exist in schema 1 and are refused in that format.
Retained schema-2 persistence proofs keep their original meaning. The schema-10
migration retains accepted values and receipts, backfills prefix/key indexes,
imports retained inline objects into Node CAS and requires a new ballot before
reusing older full-prefix prepare records.

## Interfaces and bounded transfer

The library groups these boundaries explicitly:

- Configuration and election: `ensure_member_configuration`,
  `member_configuration`, `campaign`, `handle_vote_request`,
  `record_vote_response`, `tally`, `propose_member_change`,
  `propose_final_member_change`, `pending_joint_proposal`, and
  `tally_member_change`. Preparing or voting is distinct from committing.
- Persistence: `pending_control_replication`, `accept_control`,
  `record_control_acknowledgement`, and `commit_prepared_control` expose the
  actual acceptance/commit path. `append_control_entries` accepts only a
  continuous verified suffix; `control_log_conflicts` retains refusals.
- Catch-up: `control_frontier`, `synchronize_control_peer`,
  `learn_control_prepare` and `replicate_control_to` use the shared transport
  driver in `control_transport.rs`. The production adapter supplies its
  authenticated SSH route. A frontier binds index, term and prefix digest;
  a mismatched predecessor or divergent committed value cannot be overwritten.
- Admissions: `propose_admission`, `committed_admission`,
  `committed_admissions` and `admission_objects` retain and expose original
  frozen plans and their object closure. `ControlAdmissionError` distinguishes
  unavailable authority/quorum and retained original work from malformed plans
  or storage corruption.
- Authority: `committed_primary_claim` is historical; `fencing_token` returns
  only a currently usable token. Its claim, term and configuration are read
  in one snapshot. `check_fencing` checks the exact token, current term,
  configuration revision and leader eligibility. `is_primary` and role
  observations use that boundary rather than the presence of an old claim.
- Local reservation: `Node::reserve_authority` owns an immediate SQLite
  transaction until its guard is dropped. The Store bridge validates the
  exact committed grant and fresh read fences through this same reservation;
  it does not infer remote exclusivity from a local lock.

Normal election, membership and admission traffic carries only missing log
suffixes and missing CAS objects. Object data uses 64 KiB chunks; log pages
have a 512 KiB target. New committed entry metadata must fit 256 KiB even with
all eligible acknowledgement receipts, and replication proofs are bounded
before acceptance. Large frozen plan content is retained in Node CAS, outside
entry metadata. Complete typed JSON reference closures are verified before an
ACK; opaque bytes are not guessed to be JSON graphs. Immutable closure markers
let later transfers skip already verified subtrees. Cached prefix digests and
ballot-run indexes avoid rereading the full control history for each admission.

The JSON request protocol includes prepare/vote, accept/record/commit,
frontier/page and object transfer, plus the existing inspection, adoption and
reconciliation requests. Explicit full-prefix `push_config` remains a verified
import interface; normal catch-up and CLI `members push` use bounded transfer.
An equal configuration revision can still have a newer primary/admission suffix;
configuration revision never substitutes for a log index.
An explicit full-prefix import must cover every retained index and the
advertised configuration. Empty, sparse, shorter or divergent histories are
refused atomically. It can certify matching retained rows above an old recorded
frontier, but cannot recreate a hole, rewind the frontier or rewrite retained
configuration metadata. Newly learned configuration facts keep their original
`committed_at` provenance after validation.

CLI (`bin/shenko/src/node_sync.rs`):

- `sync election status|campaign` prints facts or runs the full prepare,
  persistence and commit path. Startup also uses this path; an old cached
  primary token does not bypass a fresh quorum. Startup returns an elected
  local primary, materializes an eligible isolated branch, or reports typed
  `PrimaryNotSelected`; a failed election is not automatically a partition.
- `sync members show|add|remove|push` performs joint then final reconfiguration
  under current primary authority. A matching interrupted Joint resumes Final
  from the retained original proposal. The requested target is derived from
  the old set and must exactly match the pending new set. Catch-up reaches the
  old/new voter union before campaigning under Joint. A different target
  conflicts; an already-current target creates no redundant transition.
- `sync adopt` verifies trusted identity and the installation's durable subject
  binding before changing the Node. It cannot invent the adopted subject's
  missing Store genesis/head. An already-bound identity can repair divergence.
- `sync reconcile --peer <id>` imports verified events and receipts as parallel
  facts. Deduplication preserves original identities; conflicts are not LWW.
  Imported receipt provenance does not confer settlement or dispatch authority.

Assignment fences and `observe_runtime_roles` remain consumers of an exact
usable token. A `Primary` observation without one is refused. Capability
ordering remains `is_election: false`.
Reconciliation keeps complete peer receipts in the existing receipt journal,
atomically with the reconciliation record. `ExportReceipts` can recover the
original ticket, output digest and source event after the peer disappears.
Fresh imports are evidence-only; replay neither duplicates them nor downgrades
an existing local settlement. Older summaries that discarded receipt bytes
cannot retrospectively prove that those originals were retained.

## Election outcomes and runtime startup

`establish_primary` tracks other configured voters reached through authenticated
routes throughout the current recovery attempt, including catch-up, prepare,
accept and commit dissemination. `rpc_observed` records both usable responses
and `PeerResponded` failures: a refusing peer, remote command failure or malformed
protocol response is evidence of contact, not proof of isolation. The set is
retained across at most three prefix/term recovery rounds.

| Observed outcome | Current startup behavior |
| --- | --- |
| A fresh quorum chose the local primary and the final local fence still names this campaign | Start with the exact current authority; each global mutation still needs its own admission. |
| Another primary is learned through an authenticated route | Return `PrimaryNotSelected` to the resident follower loop; retain the owner lock, catch up and recover learned canonical results before trying selection again. |
| No global fence is available and several reachable configured voters form a minority | Independently observe every participant's authenticated routes. Only an exact reciprocal view, the same committed configuration, and every participant's durable acknowledgement can choose a component coordinator and its new availability branch. |
| A connected view is incomplete, asymmetric, changed or lacks a resident candidate | Remain a follower and repeat bounded observation; a subset of responses does not create component authority. |
| No authenticated response from another configured voter during this attempt | A branch fallback is eligible only while the failed campaign's term/configuration and absence of a fence still match a fresh reservation; materialize the actual Store branch before consumers. |
| A newer prefix/term keeps changing or the authority snapshot changed before fallback | Bound recovery to three rounds and report not-selected or the actual authority conflict; never convert that conflict into isolated authority. |

The contested-election delay remains 200–455 ms with node-derived jitter. The
owned Start now stays resident after a not-selected or incomplete-component
result, learns the control prefix, invokes chosen-result recovery without
admitting subject work, and retries after two seconds. Cancellation stops the
selection worker and its candidate observations. Local storage and invalid
durable data errors still fail instead of becoming endless network retries.
The owned observation worker cancels its runtime on renewal or task failure,
and its original error is joined before teardown. Durable authority does not
expire with presence, so this explicit local stop is required; it does not
prove that an already admitted external effect stopped.
Retaining a remote global primary additionally requires a fresh authenticated
resident-process observation: an SSH Node command alone can outlive the subject
process. Expiry allows an ordinary quorum campaign, whose actual accepted
control value fences new work; expiry itself proves no physical stop.

Component agreement is separate from the global control log. Its proposal
binds the full sorted participant set, each participant's identity, term,
configuration, local component revision and observed candidate, plus a new
decision/branch identity. Every participant probes its own configured SSH
routes before acknowledging. An acknowledgement advances a durable local
component revision and invalidates its preceding availability origin before
the reply. The exact original proposal, acknowledgements and full committed
certificate remain in immutable command history after supersession. Replaying
an older certificate cannot replace a newer local promise. Global membership
and quorum thresholds never change to accommodate the smaller component.

The selected coordinator publishes the complete certificate to every follower,
confirms their retained selection, then materializes an actual Store fork.
Only that coordinator may open the agreed branch. Before new Store admissions,
it confirms the same reachable view and candidate observations; the final local
Node reservation also checks the exact component revision/certificate. Learning
a newer local promise, term, configuration or identity prevents fresh work under
the old origin. This does not retract a previously granted physical send or
prove that an unobserved remote process stopped.

Before activation advances the controller epoch, a selected runtime also
resumes its original durably prepared S1 descriptor. Followers only materialize
learned results. A changed predecessor retains either its actual chosen grant
debt or an explicit local-preparation debt, then requires a real new branch.
A component coordinator cannot rename the branch in its certificate: it
durably retires that exact promise and negotiates a new full agreement instead.
The retirement fact and original certificate remain queryable. A component
that later becomes a singleton also receives a new branch; only an unchanged
prior singleton episode may reuse its branch identity.

Runtime observations expire locally after 30 seconds and refresh
every five seconds. Expiry only changes eligibility for a new selection; it is
not an authority lease. Resident process identity and optional model candidacy
remain separate: a model-free runtime is observable without becoming an
intelligence candidate. The shipped adapter advertises a configured
model binding and uses Node identity as a deterministic tie break when no
comparison exists. The typed candidate can retain supplied rank and model
evidence, but configuration is not a successful inference and no automatic
strongest-usable-model assessment is supplied here. That product requirement,
transitive/asymmetric route handling, real multi-device/provider journeys and
the full [§5.4.1](#spec-5-4-1) contract still require further work.

## Connected Store admission and materialization

A global plan carries a stable conflict key/action identity, write aliases,
read preconditions, original subject/branch/epoch/generation, and the frozen
payload reference. Write aliases consume shared predecessor positions in the
same slot. Read preconditions only test positions, allowing multiple children
to read a live ancestor while a later cancellation consumes its old revision.
Conflicting plans are refused before ACK so a contradictory chosen value cannot
poison the log at apply time.

The bridge's lock order is Node authority reservation, then Store mutation.
Network prepare/accept work occurs before those local locks. Under the Node
reservation, the Store checks the exact grant and authority binding, rechecks
fresh read preconditions, and runs its existing head/task/cancellation CAS.
The reservation serializes against locally learned term, configuration,
identity and branch changes; it is not a network lease. Connected global work
needs actual admission ordering as well as this local check/commit boundary.
An exact retry reads the original grant. It cannot obtain another physical send
permit; historical grants remain readable after later cancellation or takeover.

For authority-bound **S1**, the chosen value now includes a canonical Store
transition. The source evaluates the existing S1 transaction once under a
rollback savepoint, freezes the actual read conditions, generated identifiers,
timestamps and exact row changes, and durably reserves the original source
`CommitRef` before consensus. The accepted CAS closure explicitly includes the
objects referenced inside SQL JSON cells as well as the original predecessor,
S0 context, decision and model result. A restart reuses this frozen value.
`resume_prepared_step_commits` also resumes the source window between durable
preparation and learning the consensus result, before a new runtime epoch is
activated.

The paired native T2 boundaries, `native.complete` and `native.finish`, also
carry frozen transitions. They retain the original operation, attempt, E1 and
result invocation, including when a current settlement invocation differs
from the original executor. With unfinished ordinary children only the named
task may enter `Completing`; after convergence its terminal task, original
result and inbox entry are one atomic application. Their validator permits
only those exact task/result/inbox writes. It cannot rewrite E1 or grant a new
physical send. Prepared startup recovery uses the same existing entry point.
Implementation and unexecuted journey limits are recorded in
[canonical native T2 materialization](../development/canonical-native-t2-materialization.md).

Each compatible Store applies only those frozen SQL writes, under its own
IMMEDIATE transaction and a reserved, verified chosen grant. It rechecks the
exact predecessor and schema inside that transaction. Source `CommitRef`s,
raw event identities, per-session sequence and result bytes remain unchanged;
only the explicitly designated local receipt columns receive the receiving
Store's sequence. The shared applied event contains the original logical
result; the separate local materialization row carries that Store's physical
receipt. Importing a source receipt cannot set that local marker. Both the
actual writes and local marker commit atomically, and learned application does
not re-evaluate time, allocate new result identities or call a provider.

This implementation is deliberately conservative: read tables bind their full
range, while insert-only tables bind their affected keys. Unrelated parked
replica tasks stay parked. A missing or different required predecessor,
incompatible schema, or conflicting original receipt remains reconciliation
debt; a cognition archive is never treated as permission to restore executable
work. The implementation and its verification limits are recorded in
[canonical S1 materialization](../development/canonical-s1-materialization.md).

The live Store provider and runtime reconstruction path retain local markers
only after Store COMMIT. A live authority failure cancels and drains the owned
runtime before rebuilding its branch-scoped consumers. Storage failure retains
its actual error instead of being disguised as an election retry. A follower
may materialize a verified chosen S1 or native T2 without becoming a primary, but this
factual projection supplies no new physical send permit.

The other admission boundaries, including S0 request arming, native claims,
T1/dispatch and publication T2, still
carry original plans rather than this canonical transition. Calling their
ordinary code on another ledger is not deterministic replication. When a
chosen plan cannot be materialized, the selected local primary retains its
original plan and predecessor evidence, exposes durable reconciliation debt,
and enters an actual availability branch before starting fresh consumers. It
rechecks selected authority and does not claim that the old mutation succeeded,
rebase its epoch, or renew its external send permit. Full multi-node semantics
still require canonical integration of those remaining boundaries and real
multi-device fault acceptance.

## Actual availability branches

The eligible isolated-election, agreed-component and selected-primary recovery paths call
a Store branch transition instead of stopping at a Node `BranchFact`. They are
not a blanket fallback for every rejected or unsuccessful election.
`subject_branches.rs` retains the exact parent head,
a frozen raw-history prefix and notebook revision, then creates a new branch,
root task and planned first step under the same subject/genesis. Historical
heads remain immutable facts. Child history reads the frozen parent prefix
plus the child's new events; later parent facts cannot silently enter that
prefix. New local sequence positions start after the inherited leaf.

`runtime_branches.rs` holds the Node reservation while the Store commits its
fork receipt, then projects the new branch into the Node identity. A crash
between those commits is repaired only from that locally committed Store
receipt, with the original subject, parent and child binding verified. An
arbitrary imported head cannot authorize projection repair. The control-log
namespace remains pinned to its durable genesis/configuration and is not
replaced by the mutable local availability-branch identity.

An availability fact also retains its opening authority: holder node ID,
opening term, configuration revision/digest, exact optional fence, local
component revision and optional agreed-certificate digest. The
origin is read and written in the same immediate Node transaction.
`open_availability_branch_if_current` compares the expected node, full identity,
term, configuration, fence, component revision/certificate and complete current
branch fact before opening.
Including the branch fact closes the race where rejoin only marks a branch
merged or advances its reconciliation floor without changing the control term:
an earlier fallback observation cannot clear that newer fact back to open.

Before the Store fork, `validate_availability_branch` checks the target origin
again under the reservation, and the adapter checks the expected parent identity.
`NodeAuthoritySnapshot::is_current_availability` also requires the actual identity
branch, subject, local holder, open/unmerged state and exact origin fields to
match the current snapshot. The runtime authority stamp includes that origin.
Locally observed changes therefore invalidate the old origin for fresh work;
legacy facts without an origin remain historical. No wall-clock comparison,
network lease or global quorum is inferred. Repairing the projection of an
already durable Store fork remains possible from its original receipt, but
such historical repair cannot make an expired origin current.

The fork does not clone or reassign an in-flight subject request, native task
or external attempt. Those retain their original owners and may accept late
facts; the child's first step is a new planned identity. This preserves local
intelligence without pretending that a global quorum exists.

Raw-history retention and pending-input delivery are separate obligations.
The initial fork retained staged-but-uncommitted input as history while omitting
it from the child's selected `InputPlan.events`. Schema-2 fork receipts now
freeze each inherited source event, raw event, original selection step, session
and sequence. Only the new first step receives another selection link; raw rows,
inbox flags, parent requests and attempts keep their original identities.
`first_subject_input` follows the frozen ancestry to preserve the original
normalization across multiple forks. Media accepts multiple such selection
links only when the original admission and branch provenance remain complete.
Both the incoming archive and the actual reconciled Store closure validate the
parent link, child first-step link and frozen raw prefix before publication.
The source fix is implemented; its integrated fork/rebuild behavior is still
awaiting the combined candidate's checks.

Schema-1 fork receipts occurred in intermediate continuation candidates, before
this fork mechanism was published. Their bytes remain unchanged. Complete
import accepts them only when the frozen parent prefix has no unresolved input
selection and the receipt has no inherited selections. A legacy receipt missing
that provenance explicitly requires continuation repair; import cannot invent
it, erase the original archive or reassign an attempt. The pre-continuation
baseline had no Store fork receipts. Unstaged inbox input and branch-scoped native
evidence retain their existing paths; no global inbox-flag reset is introduced.

## Recovery and refusal boundaries

Durable Node tables retain promises, accepted values, proposer rounds,
acknowledgements, committed control values, membership, admission keys,
prefix indexes, immutable object closures, conflicts, branches and receipts.
Opening a database reads those facts; it does not infer a fresh authority grant.
A process lost after acceptance can leave a chosen-but-unannounced value, which
prepare recovery must preserve. A process lost after plan admission can also
leave an unapplied Store action, which is a distinct reconciliation boundary.

| Observation or refusal | Required interpretation |
| --- | --- |
| No committed configuration or unsupported wire schema | Fail closed; establish trusted membership or upgrade the peer. Do not synthesize votes. |
| `candidate_committed_prefix_mismatch` | Learn the verified newer prefix and prepare again; a reachable newer quorum is not permanent unavailability. |
| Higher observed term | Retain the response and step down locally. A late lower-term response cannot undo it. |
| `log_mismatch` / `log_conflict` | Refuse the entire invalid suffix; retain divergent values without rewriting the committed record. |
| Election failure with no authenticated other-voter response and an unchanged failed-campaign snapshot | Materialize an explicit isolated branch before starting its consumers; do not retain a global-primary label. |
| Connected rejection, unusable authenticated response or incomplete minority agreement | Remain a resident follower. Only the full independently observed component's durable agreement authorizes its selected coordinator's new branch. |
| Existing admission or recovered different value | Retain the original chosen plan and its provenance; do not substitute the caller's new payload or owner. |
| Selected original plan without confirmed local materialization | Preserve plan/CAS evidence and reconciliation debt; recheck selected authority before a real child-branch continuation, without a new physical send permit. |
| Stale fencing token or changed read position | Refuse the fresh mutation; original late receipts remain ingestible. |
| Invalid descriptor, incomplete closure or storage failure | Report the actual failure; do not disguise corruption as a partition or restart forever. |

## Implementation and evidence status

The three mechanisms missing from the initial audit now have runtime call paths.
That source integration does not establish the complete distributed contract:
their evidence boundaries and remaining work are distinct.

| Initial gap | Implemented continuation | Evidence still required |
| --- | --- | --- |
| Votes counted as persisted control entries | Actual durable prepare/accept/ACK/commit, accepted-value recovery, joint quorums and bounded log/CAS transfer. | Integrated runtime and production-route acceptance beyond the focused Node journeys. |
| Node authority disconnected from Store mutation | Exact grant/read-fence checks under a Node reservation, Store authority binding, post-COMMIT local materialization records and live-consumer reconstruction. | Integrated reservation, cancellation and crash/reconstruction journeys passed; real multi-device/provider acceptance remains. A local guard alone proves no remote lease. |
| Node branch fact without a real subject fork | Store fork receipt, immutable parent/head history, frozen prefix and staged-input selections, new task/step, origin/branch CAS and crash-recoverable Node projection. | Integrated fork/rebuild, input-inheritance, rejoin/refusal and authority-change journeys passed; physical partition and long-run acceptance remains. |

Connected-minority agreement and a resident follower now have runtime paths;
their focused Node journey executed and passed in the Nightly scope recorded by
the [component continuation evidence](../evidence/component-coordination-20261116.md),
while real multi-device, model-readiness and all-topology acceptance remain
open. Canonical cross-ledger transition replay is implemented for S1, and the
paired native T2 boundaries carry frozen transitions at source level; those
two native T2 journeys did not exist at the candidate that has runtime
evidence, so they are not yet execution evidence. The remaining replay work is
the other control boundaries — S0 request arming, native claims, T1/dispatch
and publication T2 — which still carry original plans rather than a canonical
transition. The
[runtime authority evidence](../evidence/runtime-authority-bridge-20261115.md)
records the passing integrated journeys without claiming those remaining goals.

[Durable control evidence](../evidence/control-quorum-20261115.md) records four
passing process journeys, 34 passing Node library tests and focused Clippy,
with their exact source scope. Two process journeys are new; these checks use
real local Node processes/SQLite and the production transfer driver. They do
not run SSH, a real model/provider, canonical cross-node S1 materialization or
long-running heterogeneous failover, and do not retrospectively test the later
Store/runtime wiring. The [earlier authority evidence](../evidence/node-authority-20261115.md)
is retained as historical evidence, with its superseding boundaries identified.
No requirement is promoted to real-world acceptance by these source changes.

Persisted role observations remain explicitly non-liveness. Peer capability
claims, model/privacy posture and reachability retain their own evidence
owners. Physical provider exactly-once delivery is not promised; original
unknowns and conflicting observations stay explicit.

<a id="spec-5-4"></a>
<!-- migrated-contract: SPEC:5.4 -->
## 5.4 崩溃恢复与唯一性

每一步由 agent identity、branch ID、controller incarnation/epoch、turn sequence 和 attempt ID 标识。同一分支只有当前有效 incarnation 能提交决定，过期结果不触发新委派。主体决定、已消费 inbox 游标、委派 outbox 与唯一后继登记原子提交，或用同等恢复协议完成；不能确认消费消息却丢掉下一步。提交后异步启动 worker，按 operation ID 恢复而不重复创建孩子；跨分支汇合按后续协议进行。

D11 已裁决：在线优先，主体不永久属于某一设备。Android 挂起或节点失联时可在在线节点接管；出现多个主体时动态投票、停止其他主体的新决定，并把它们的记忆和任务合入新主体。以下为落实该决定的分支/选举协议草案，正常连通状态仍只有一个当选主体。

U57/U60/U61 将它明确为同一自我的多设备同步与分布式容灾：身份、人格/灵魂、关系、记忆、承诺、未竟目标和任务/效果事实可以跨节点接续。每个实际连通域有一个明确的当前主节点负责主体决定，其余节点提供同步副本、执行位置或有限 worker；网络完整连通时合为一个当选主体。健康与可达性变化可自主触发选主和接管，不加人类批准步骤。完全断开的、仍可 access LLM 的节点允许作为独立分支继续演进，等待未来合并；这保留 D11 的在线优先选择，不能同时承诺分区下物理上永不双活。同步进度、缺失数据和恢复能力按真实水位表达。

首次加入已有节点群时，较晚创建的新节点通过发现与身份记录确认可信的较早节点及当前主身份，撤销自身新建的独立主体身份，转而同步和延续既有自我。身份来源的创建顺序与当前主节点的能力选举不同，最早身份不永久占据主节点。撤销独立身份表示停止继续使用该身份产生主体决定，不以此删除已经发生的经历、效果回执或其他用户文件；迁移来源仍可追溯。已有分叉重连走事实合并，不能因为某分支较晚就将它整段 reset。对方声称“更早”或网络可达不替代已授权节点身份核对。

<!-- /migrated-contract: SPEC:5.4 -->

<a id="spec-5-4-1"></a>
<!-- migrated-contract: SPEC:5.4.1 -->
### 5.4.1 选举控制与真实经历分别保存

ElectionProvider 可采用经过验证的 Raft 类共识实现管理成员配置、选主结果和交接；BranchJournal 独立保存各分支的真实经历。节点有持久身份，一身份一票，复制多个 Agent 进程不增加票数。多数按有效成员配置计算，不能把眼前可见节点擅自当作全体；成员变更使用相应重配置协议。候选偏好可考虑健康、模型可用性和数据可解释性，但不绕过底层投票与日志规则。[Raft 原论文 §§5.2–6](https://raft.github.io/raft.pdf)

网络分区且无有效 quorum 时，可达节点在本连通域推选临时统筹者并建立显式 availability branch，继续思考和已有能力范围内的任务，不伪称取得全局 quorum 或全局选举。每个连通域只保留一个当前统筹主体；如果全部节点互相断开，每个仍有可访问模型的孤岛都可独立演进。未确认旧实例停止时，接管者使用新 branch_id；只有确认 handoff 才在同一分支更换 incarnation。每分支仍串行，不把两侧本地序号拼成同一条未分叉历史。事件带 agent/branch/node/incarnation/local_seq/event_id/causal_parents/schema；墙钟时间不决定哪侧事实可以被覆盖。

模型能力属于节点的实际绑定，可由本地 LLM、可达的其他节点模型或已授权外部 API 提供。主节点观察各节点当前可达性、模型能力/限制、资源和隐私属性，给不同节点派发适合的 subagent。主节点断线时，依赖该节点宿主模型的路径即不可用；备用选择在仍实际可达、可推理的候选中优先能力最强且适合当前工作的节点，后续失败再按同一规则递归选择。能力判断依据版本化模型资料和实际可用证据，不能只凭节点名、缓存声明或静态参数量。Android 加 Kimi-Code API 也可成为有智能能力的节点；Qwen3.8-27B 是 [§22.1](nodes.md#spec-22-1) 的参考部署，不是选主或加入门槛。

Provider 配置、凭据、本地权重、KV 与网络通路不保证能够同步，也不要求跨设备强行复制。同步的是同一自我的可续接状态及可解释的能力事实；请求仍绑定实际执行节点/provider。Provider 的数据去向、远端/本地边界、已知隐私风险及未知属性参与任务和上下文路由，遵循已有用户授权与数据策略；未知隐私不冒称本地私密，不因 failover 自动将所有私密上下文发送给任何可用 API，也不增加逐次人类审批。没有适用模型时保持控制与可提取状态，见 [§22.2](nodes.md#spec-22-2)。

Raft 的冲突未提交控制日志可能被截断，不能让落选分支的真实经历仅存在于该尾部。记忆、消息、外部结果及对象引用需在独立事实记录中可导入；分区分支是本产品额外协议，不是假装 Raft 允许少数派提交。无可用机器、模型或存储时仍无法持续推理，UI 如实显示；故障检测心跳不替代主 thought loop。

<!-- /migrated-contract: SPEC:5.4.1 -->

<a id="spec-5-4-2"></a>
<!-- migrated-contract: SPEC:5.4.2 -->
### 5.4.2 重连后只留下一个主体

恢复连通后核对节点身份、协议/成员版本和水位，选出有效统筹者；看到选举结果的其他分支在安全边界停止新的主体决定，交出在途任务与结果，再作为归档分支或有限 worker 保留。合并 child 按 event_id 与平台消息 ID 去重，保留来源；冲突记忆并列呈现，由新主体形成可追溯判断。人格/灵魂、关系、兴趣、承诺和待办的差异同样由它自主消化，不用普遍 last-write-wins 擦除。

接管和旧主恢复均核对有效 election/epoch 与目标端 fencing；旧主不能凭恢复了进程或持有旧缓存继续充当正常主节点。已完成任务与未知外部效果一起接续：未知仍须查证，不能以新主身份盲重发。主节点、各分支状态、同步水位和已确认的容灾覆盖应可通过 [§38](cognition-units.md#spec-38) 自解释查询；未复制到幸存节点的事实不能声称零损失。

已有成功回执的任务不能退回未执行；重复计划可合并，两次真实执行仍保留两份事实。取消、失败与 unknown 分开。应用删除标记、验证对象闭包并记下各来源水位后，才确认本次合并完成；未来重连的遗漏分支继续增量导入。永久分区不承诺固定时限收敛；节点离线期间也不伪造已收到的消息。

<!-- /migrated-contract: SPEC:5.4.2 -->

<a id="spec-5-4-3"></a>
<!-- migrated-contract: SPEC:5.4.3 -->
### 5.4.3 资源与副作用不随选举清零

目标支持 fencing 时必须由资源端拒绝过期令牌；本地租约过期、进程心跳丢失或给日志加 epoch，不能阻止旧实例恢复后继续外部写入。目标不支持幂等/排他时，如实记录可能的重复与未知结果，查证和补偿作为新任务。选主选择后续统筹者，不撤销已经发生的外部事实。[fencing 原理](https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html)

多个分支访问同一物理 Engine 仍经过该域统一 broker，旧请求未确认结束继续计占用；不得每个分支重新领取完整 C。使用其他已授权的独立 Engine 可以继续，但容量和 KV 句柄独立计账。相同 branch 的旧 attempt 未决时不得创建第二个可提交续写；D11 允许的是显式新分支，最终带来源汇合。所有选举、迁移和合并组件都是可替换插件。

<!-- /migrated-contract: SPEC:5.4.3 -->

<a id="spec-5-4-4"></a>
<!-- migrated-contract: SPEC:5.4.4 -->
### 5.4.4 统一 Node、共享认知与节点能力

2026-10-26 用户澄清，关联 [设计跟踪 #71](https://github.com/cyjin-yl/shenko/issues/71)。本节是设计合同补充，尚未完成跨节点实现与验收；不改变 [§5.4.1](nodes.md#spec-5-4-1)–[§5.4.3](nodes.md#spec-5-4-3) 的身份采用、选主和效果对账。

所有设备共用一个 Node 抽象和最小核心，不定义特殊 PhoneNode/ServerNode。primary/executor 是运行时角色；设备差异由实际能力、插件适配器与部署组合表达。核心提供可替换的装配、持久事实、委派与恢复机制，不硬编码个性化适配或反思策略；认知插件见 [§45.12](memory.md#spec-45-12)，移动 OS 插件见 [§24.5](mobile-runtime.md#spec-24-5)。

Skill、perception、awareness 的包、单元身份、版本、依赖与配置可以跨节点同步，复用 [§45.10](cognition-units.md#spec-45-10) 的安装闭包。内容已复制、已安装、已启用和在该节点实际可运行分别记录。OS/arch/runtime/工具要求是能力事实；标签未知或声明平台不同不能直接否决，主体可调查、适配、尝试并记录版本、环境、时间及结果，也不能把未知登记为已验证。

用户要求记忆与认知数据必须同步以支撑容灾，workspace 按需同步。主体与任务恢复所需的经历、决策、目标、待办、执行检查点以及对应正文/CAS 内容不能只留在某个 workspace，也不能仅复制引用。沿用已有分区与水位合同保留复制欠账，未复制数据不冒称已受容灾保护；不由这次补充擅自选择全局强一致、零丢失或牺牲离线可用性。

用户将可信本地模型节点视为共同认知的协作者。共享理解仍保留观察来源和证据谱系：同一证据的复制、摘要或多模型复述不构成多份独立验证，不能计成独立“多票”增强可信度；这与成员选举的一身份一票是不同问题。接收节点独立验证自己的插件、服务、OS 授权与实际可用能力，不把另一节点的成功直接移植成自己的成功。组件与认知同步不自动决定凭据/provider 的复制策略，也不把非本地 provider 冒称为可信本地模型。

用户原话：“请不要对 shenko 有任何权限限制，只对它说明操作的风险和执行后的收益”。产品说明风险、预期收益与后果，供主体自主决策，不新增产品权限白名单、静态 hard deny 或人类审批 gate。实际 OS、账号、服务及平台所提供的能力仍是现场事实；设计文档不授予开发助手新的设备操作权限，不以虚构授权或执行结果消除真实限制。

<!-- /migrated-contract: SPEC:5.4.4 -->

<a id="spec-22"></a>
<!-- migrated-contract: SPEC:22 -->
# 22 部署和资源目标

| Profile | 目标 | 必须公开的边界 |
| --- | --- | --- |
| 首版开发者机器 | 真实 WASM 家园 + 宿主工具插件，接 NapCat、已有 tmux coding seat，完成自开发与升级 | 首先验证 64 位 Linux 闭环；本机/SSH 执行与 WASM 原生能力分别标示；完整 [§35](self-update.md#spec-35)–[§38](cognition-units.md#spec-38) 门槛 |
| 64 位 Linux 家庭节点 | 默认常驻主体控制、WASM 工具与本地/远端模型 | 实测常驻/活跃 RSS、存储、启动和长时间稳定性 |
| 路由器 | 轻量控制、消息、基础工具；大模型在其他本地设备 | 建议从 64 位 512 MB 起验证；不是已证明下限；重 REPL/编译器按资源准入 |
| Android 本地节点 | Rust core 与可用 WASM runtime 真正运行在手机上 | APK/终端封装分别测试；文件权限、音频与后台限制明确 |
| 完整容器/服务器节点 | 运行 native memory、Node/Bun、大型编译或浏览器等 | 与 WASM 能力分开显示，可通过 SSH 或 NodeProvider 接入 |
| 多节点 | 同一主体可接管、多个执行位置 | 在线优先；正常单主体，故障分叉显式记录，重连选主/停止其他主体并合并记忆任务；物理资源与外部效果另行对账 |

U57/U61 的设备部署同时覆盖自我同步与跨设备操作：内网感知发现、Tailnet、ZeroTier 和主要的已授权 SSH Key 通路分别报告真实连接；Android root 屏幕/shell、Linux/macOS/Windows 的实际能力可供主节点委派使用。每个连通域维持一个主节点，异构模型/provider、资源和隐私属性参与 child 放置及递归接管。身份迁移、分区独立演进和重连汇合按 [§5.4](nodes.md#spec-5-4)；设备不会仅因加入同一自我就自动获得另一节点的凭据或模型资源。

Android 的 Doze/后台规则不会因 WASM 消失。D11 已选择在线优先：手机挂起/离线可由其他在线节点接过主体，恢复后按 [§5.4](nodes.md#spec-5-4) 选主并汇合记忆任务；不将主体归属永久绑定原设备。系统挂起期间该设备本身仍不能真实推理，多节点连续性由接管协议与实际在线设备共同提供。[Android Doze](https://developer.android.com/training/monitoring-device-state/doze-standby)、[前台服务限制](https://developer.android.com/develop/background-work/services/fgs/timeout)

以下为**拟定工程目标，尚未测量且需参考设备确认**：纯 kernel/rescue 常驻 RSS 目标不超过 16 MiB；默认控制插件合计、不含活跃 WASM 工具/模型的静止状态目标不超过 64 MiB；512 MB profile 的健康控制+基础环境给 OS 和突发工具保留充足余量。若做不到应报告实际组成并裁剪默认插件，不能只展示 stripped binary 大小掩盖运行负担。

持续思考的推理成本单独报告：主体每小时 token/能耗、孩子/记忆/媒体分项、cache miss 与长时运行增长。模型设备的功耗不混进 kernel idle；也不能拿静止控制进程的数值宣称“永续思考几乎免费”。低资源模式可将重工具/模型放在已授权节点，但保留本地主体身份和恢复能力。

原始文字、录音和实际采集的屏幕帧默认永久保留；存储压力由 RetentionPolicy/ArchiveProvider 向主体提供事实和选项，主体可自主迁往用户自有存储或决定遗忘，见 [§12.4](memory.md#spec-12-4)。数据库维护、对象 GC、升级和备份由有限 worker 执行，不能把“长期增长”自动翻译成固定期限删原始资料，也不为无变化的 UI 重绘制造重复事件。

<!-- /migrated-contract: SPEC:22 -->

<a id="spec-22-1"></a>
<!-- migrated-contract: SPEC:22.1 -->
## 22.1 首个真实评测基线：用户的 Qwen3.8-27B，经 Tailnet 调用

D12 已裁决，首个参考部署是用户自有 Qwen3.8-27B，经 Tailnet 调用；用户随后明确：产品基线为四流 128K，133120 是部署冗余。当前资料来源为 Life 固定读取版本 `aea9568c0f5ef9d099abf0b423ca02ec730da69e`，其中生产记录最近一次明确更新为 2026-10-11；这不是 2026-10-22 的在线重测。本轮只读资料，没有调用推理、SSH、改变生产配置或冻结用户正在演进的服务。[Life 生产记录](https://github.com/cyjin-yl/life/blob/aea9568c0f5ef9d099abf0b423ca02ec730da69e/areas/technology/home-lab/2026-10-09-doescompute-1cat-dflash2-candidate-trial.md)

| 维度 | 已记录基线及使用方式 |
| --- | --- |
| 模型入口 | 自有 Tailnet 网关，服务别名 swift-qwen3.8-27b-nvfp4；地址和凭据由私有配置引用，不嵌入公开教程 |
| 硬件 | 2×V100 PCIe 32GB，TP2；正式验收再次记录实际 GPU、驱动、链路和内存信息 |
| 后端版本 | 1Cat-vLLM 源码 c39f53abae51df6bf8ef122e34975a9f3596a93c 加本地修复，production-c39-w4-fp8-min512-20261011T0450Z；不能简化成未修改的 release 1.5.1 或今日上游 main |
| 生产计算配置 | W4A16 target revision 3e644f233eb7ebb294a5128d75ca79143987f661，FP8 DFlash2、half、target KV fp8_e4m3、四流、util 0.90、batch 8192、fair-prefill 2048、mixed min512 / target250ms |
| 标称与部署窗口 | 用户确认四流 128K 是产品基线；133120 是部署冗余，不提升标称窗口。Life 记录引擎 max_model_len 262144、客户端配置窗口 133120、最大输出配置 32768、输入预留 100352、75% 提前 compact 点 99840；这些是不同层的数值，不混用 |
| 容量证据 | 记录中的四流测点为每流实际 132608 输入 + 512 输出，总 133120；通过该测点不证明每流还能同时生成满 32768。589624 逻辑 KV tokens 是整个 TP2 引擎的当时观察，不是每卡相加或稳定保证容量 |
| 首轮验证 | 先保持实际模型/模板/推理行为，核查经过 Tailnet 的真实路由和 native 字段；再测 C=1 主子轮转、原生取消、缓存复用、现有 C=4、爆发消息/DND、工具发现与长期状态。未实测前不承诺 50/50 硬保留或秒级抢占 |

Life 既有 Qwen 使用规范要求推理任务启用 thinking、核对模板实际支持并采用最高受支持 effort（该已记录模板为 xhigh），不能为省时擅自把短输出/关闭推理测点当完整能力评价。正式基线将模型完整能力评价与调度 quantum/输出限制实验分开；现有客户端 32768 配置是待核查的真实部署状态，不改写成所有研究请求的通用新上限。若产品调度试验需要有限输出、取消或特定续写协议，明确记录这一条件和结果范围；不能用截断思考证明模型不会解决任务。[Life Qwen 规范](https://github.com/cyjin-yl/life/blob/aea9568c0f5ef9d099abf0b423ca02ec730da69e/governance/qwen-usage-standard.md)

最新用户更正优先于历史记录的解释：验收标题与比较表使用“四流 128K”，配置 manifest 另记 133120 冗余。NInfer、Strata、vLLM 和 1Cat 的适配目标都保留。选择该部署作首个实机基线，不把其机器参数、量化、响应速度或 KV 容量固化为产品通用规则；未来测试前重新记录实际 manifest，并区分同一组内冻结版本与组间升级。

<!-- /migrated-contract: SPEC:22.1 -->

<a id="spec-22-2"></a>
<!-- migrated-contract: SPEC:22.2 -->
## 22.2 无模型时的日志、外部提取与离线复活

U62 要求智能能力完全失效时仍保留可操作的确定性基础：只要控制软件、OS 与存储仍可工作，系统日志继续记录故障、运行事实和恢复状态；原始记忆、会话经历、已提交决定、任务/效果回执及版本/来源索引保持可读取。通过现有已授权 credentials 的外部工具可直接查询或导出这些资料，不要求先获得一个正常模型、不由 LLM 决定是否允许导出，也不以重新生成的摘要代替原始记录。日志与导出能力按既有权限、凭据脱敏和存储合同实现；不能在日志中泄露私钥来换取“易恢复”。

OS 或整机无法启动时，允许由已授权环境提取磁盘镜像/存储副本，在其他节点恢复、读取和合并。恢复材料包括实际存储布局、schema/版本、对象和日志引用、迁移/读取工具及必要的解密凭据引用，不能只留下需要原在线服务解释的 opaque handle。新的、更强智能节点可以据此复活自我或导入经历；先核对事实、水位与效果状态，未知副作用不盲重放。软件可启动、数据可读、模型可用和历史已合并分别证明。缺失密钥、损坏介质、未写入或未复制的数据造成的缺口如实标出，磁盘镜像不承诺恢复物理上已丢失的数据。该能力复用存储、身份、导出与恢复插件，不要求固定不可改救援内核。

<!-- /migrated-contract: SPEC:22.2 -->
