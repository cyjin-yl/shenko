# Agent-owned node choice / Agent 自主选择执行节点

## Fixed-version verification update — 2026-10-30

Owner-approved `7c2c7a98` execution was attempted under exact Rust/Cargo 1.98.1,
empty HOME, independent Cargo/target/temp, original lock, offline plus inherited
socket denial. Both the selected-SSH and node commands exited **101 before
compilation: zero tests ran**. The inherited main `bc33b654` host-executor
manifest declared the identical `blake3 = { workspace = true }` twice (lines
15/26); both source versions have the same manifest blob `2c0b7300`.

The source repair removes only the second identical declaration. It does not
change dependency versions/features, lock bytes or workspace members. All 24
tracked Cargo manifests now parse in a static TOML check. Static offline Cargo
metadata progresses to a missing cached `x11rb` index entry; no repaired-source
compilation or test has run. Of 401 locked registry archives, 398 existing
archives were checksum-verified; three newly inherited X11-related packages
remain absent. A prefetch was denied as outside the offline test scope and was
not retried; separate preparation authorization is required, and actual tests
remain offline. Full failed-command evidence and limitations are retained in
[the fixed-source record](../evidence/agent-selected-ssh-7c2c7a98-20261030.json).

## Current integration checkpoint — 2026-10-30, source only

This continuation normally integrates main
`bc33b654bcb78012c671827e1968f652aa69056b` with the existing #127 head
`be4c9ad85b70bfe392b09539ec82ec3d1875574f`, preserving both histories and
the unmerged #114 dependency. Main now carries the #125 model-child/quota
work and adds `TaskInvocation.software_generation` plus native T1 generation
validation. The historical test results below do **not** validate this combined
source or its changed dependency closure.

Fresh selected SSH inputs use `ssh-task-node-selection.v2` and bind the actual
invocation's runtime generation. A matching retry must retain that generation;
another generation cannot reuse the selected API to query or dispatch under
the old invocation. A newly claimed recovery worker uses the separately
authorized existing operation query/reconcile path, not a forged invocation.

The frozen v1 reader is retained explicitly because already-armed operations
must remain recoverable. Its generation is unknown: it is never defaulted to
`seed` or inferred from the current runtime. Existing `query(operation_id)` can
read that v1 closure without rewriting its CAS bytes; `execute_selected` refuses
to treat it as a generation-bound fresh/repeated invocation. The query reader
also rejects a v2 schema paired with a generation-less v1 invocation.

Two focused test journeys are prepared: changed-generation rejection without
another observation/attempt, and synthetic v1 reader/serialization coverage plus
fresh-admission rejection before T1. V1-with-generation, v2-without-generation,
unknown legacy fields and wrongly typed generation are negative checks. The
v1 fixture is not an old-executable migration or a remote/query integration test.
Even the crate-private fresh-admission helper rejects legacy selected inputs;
production legacy recovery remains exclusively the existing query/reconcile
path, whose `query` mode supplies no action payload. Existing fixture generation
and the previously reported unused import are corrected. Tests/build have
**not run for this integration**; `git diff --check` only checks whitespace.
No installed authenticated handler, registered-target permission/resource
resolver, real model/remote execution or full distributed completion is claimed.

## Why and source

The owner's 2026-10-29 clarification: expose distributed capabilities to the
central thinking subject, or a scheduling agent it delegates to; do not replace
their choice with a mechanical capability router. This slice follows SPEC
§5.4, §11, §38, §40 and the existing #114 advertisements. It does not implement
an election, a sleeping subject, or an Infinity-style durable runtime.

**Status: fixed candidate `ca6dcd9a` built and passed 18 node tests, including
the five new selection journeys, on 2026-10-29 at 19:40 UTC.** No real model,
remote node or end-to-end distributed task has been validated. Based on #114 `47b26f98` plus
main `d1cf8d64`; it preserves both histories. No new dependency/schema migration.

## Narrow capability card

- Purpose: inspect retained node capability declarations, choose a particular
  node explicitly, retain that exact choice/reason across restart.
- API: `Node::node_choice_snapshot(after, limit)`,
  `record_node_selection(NodeSelection)`, `node_selection_receipt(decision_id)`.
- Effect: snapshot/recovery are local reads; recording writes one idempotent
  command receipt. No network, execution, lease or resource reservation.
- Audience: trusted adapter code. This is **not** a new administrative RPC and
  is **not yet installed as a subject/child tool**.
- Unknowns: advertisement age, liveness, model availability, resource capacity
  and task authorization are not inferred. `provides` strings are untrusted
  descriptions, not instructions or grants.
- `profile_digest` identifies the complete retained advertisement (including
  fields omitted from this projection). Treat it as an opaque observation ID;
  it cannot be recomputed from `CapabilityObservation` alone and is not a
  signature or proof of the remote node's current state.

## Minimal path / 最小使用路径

1. 已认证节点通道继续负责 #114 的 advertisement 接收；需要更新本机声明时，
   管理侧显式调用已有 `capability_profile()`。查询自身也只读已缓存事实，
   不暗中扫描、探测远端或刷新节点配置。
2. Adapter 读取 `node_choice_snapshot(None, 32)`，向主体或其委派调度 Agent
   投影候选。稳定 node-ID 顺序仅用于分页，**不是优先级或自动推荐**。
   未知/过大事实仍有候选条目，不机械删除节点。每页是同一 SQLite snapshot，
   多页不是冻结全集；记录时重新核对目标和观测。
3. Agent 明确提供 `node_id`、观测 `profile_digest`、新的 `decision_id` 和
   `reason`。Adapter 必须从可信调用上下文核对身份/委派关系，不能采信模型
   自报的身份或权限，再调用 `record_node_selection`。
4. 相同 decision ID 和完全相同请求返回原回执；改理由或节点必须用新 ID。
   `unavailable` 给出 `not_enrolled`、`observation_unknown`、
   `observation_oversized` 或 `observation_changed`；后者附当前观测。
   由 Agent 选择更新观察、保持目标、改计划或选择其他节点，库不自动替换。
5. 重启后通过 decision ID 读取历史 receipt；它不变成新的决定或隐式重试。
   每个 receipt 的 dispatch 始终是 `not_dispatched`。

English: read a bounded snapshot, let the authenticated subject/delegate choose
one node and explain why, seal the choice against the observed profile digest,
then recover the same durable receipt by decision ID. On changed or missing
facts, return an explained unavailable result to the agent, never substitute
another node. A successful `recorded` result means only that the observed choice
was recorded, **not** that the target is ready, authorized or dispatched.

## Storage and limits

Reads project only node ID, profile digest, generation, advertisement time and
declared capability names. SSH target, binary/data paths, role reasons and
transport provenance are not exposed. At most 32 candidates per page, at most
16 KiB of stored advertisement per candidate (including the local envelope);
SQL guards byte length before returning profile text. Oversized profiles are
reported, never truncated into an apparently complete capability set. Limits
bound payload/allocation, not SQL runtime or all storage costs.
Malformed stored JSON or a mismatched node identity fails the entire read
closed with an error; it is not silently converted into an unknown fact or a
partially successful snapshot.

Validation and receipt insert share an immediate transaction. Existing
`commands(id,seal,result)` supplies durable idempotency; the seal includes the
`node.selection.v1` domain and complete request. Reused IDs cannot overwrite
another decision/command. Successful and unavailable decisions are immutable.
The versioned receipt keeps its original reason, observation and timestamp.
Future lifecycle changes must retain readers for this schema, or explicitly
migrate with history preserved; an unknown schema fails rather than re-plans.

## Integration dependencies — not implemented here

- Authenticated subject/delegated-agent bridge and discoverable capability
  registration; coordinate with #112 self-knowledge and #125 model-child owners.
  Never give a model the Node `Initialize`, `Join`, `Create` or other admin RPC.
  These receipts do not contain an authenticated caller or durable task binding;
  those bindings must be supplied by that bridge, not guessed from the reason.
- Actual remote dispatch/admission bridge. It must bind the decision receipt to
  the real durable task/operation, preserve the chosen node/reason on recovery,
  and independently enforce authorization, current resources and identity.
  An advertisement digest is no lease, permission token or fencing epoch.
- Recheck changed conditions at dispatch; return their concrete reason to the
  agent instead of rerouting. Unknown external effects retain operation IDs,
  executor fencing, receipts and reconciliation; never blindly replay.
- Event-indexed waits, lost-wake protection, bounded activation and lifecycle
  compatibility of active work are separate durable-runtime work, not provided
  by storing a selection receipt. Model KV residency is also separate.

## Validation ledger

Five new journeys: bounded/non-ranked snapshot with unknowns and no routes;
explicit choice/restart/replay/conflicting ID with zero task creation; changed
observation/re-decision without substitution; absent/removed enrollment and
missing facts; oversized observation reporting. Initially source-only; after
the owner's fixed-version execution approval, the exact `ca6dcd9a` candidate
passed `cargo test --locked --offline -p shenko-node --message-format=json`:
18 passed, 0 failed, 0 ignored; binary and doc-test suites each had 0 tests.
Rust/Cargo 1.98.1, original lock unchanged, 398 downloaded registry archives
matched its checksums; node artifacts were freshly compiled. Empty HOME,
independent Cargo/target/temp directories, explicit credential-free test
environment, Cargo offline and inherited seccomp socket denial were used.
This is not a mount-namespace isolation claim. The failed bwrap system probe
and initial libgit DNS fetch failure are retained in the
[candidate-bound evidence](../evidence/agent-node-selection-ca6dcd9a-20261029.json).
Normal restart/replay is tested; crash/commit-fault injection is not.
No claim of CI success: repository snapshot has no GitHub workflow directory.
`git diff --check` is a whitespace check only, not Rust compilation or runtime
verification. A genuine subject-discovery/selection journey still requires
model validation after the authenticated bridge exists.

## Trusted SSH bridge increment (2026-10-29, locally tested; not installed)

The next source increment adds `SshTaskRunner::execute_selected` in
`shenko-exec-ssh/src/selected_task.rs`, reusing the real existing SSH runner:

```text
installed authenticated child adapter
  → trusted TaskInvocation + AuthorityRef + registered node/SSH binding
  → explicit recorded selection / current observation check
  → existing Store::arm_external_operation (T1)
  → existing SSH helper / sealed receipt / query-only recovery (E1)
```

This is not another preparation ledger. The complete selection receipt/reason,
invocation, authority, chosen node and operation ID are in the **same frozen
SSH input** whose digest T1 stores and the existing remote helper receipt seals.
The task revision, active invocation/epoch, persisted worker assignment,
delegation decision and reserved native operation ID are checked before fresh
admission. T1 still conditionally checks the actual current task. Node locks
are released before T1/network awaits; a changed advertisement is explained,
never used to choose a substitute. Observations and T1 are not a cross-database
transaction, so an advertisement is explicitly not a capacity/permission lease.

An already-armed matching operation reads its original frozen binding/selection
and queries the existing receipt; it does not refresh its choice from Node or
send another action. A changed action, target, decision or authority is a
structured refusal. This retry requires the exact original `TaskInvocation`;
a newly claimed recovery worker uses the existing authorized
`query(operation_id)`/reconcile API instead of pretending to be that original
invocation. Optional `FrozenInput.selection` uses serde default
and skip-None so old two-field frozen inputs keep their exact serialization.
No model-facing admin RPC or unguarded target registry was added.

The node-choice receipt's `not_dispatched` remains the historical result of
recording a choice. If this trusted API is later actually run, the **SSH operation
and attempt/effect receipts**, not that immutable choice record, describe dispatch.
Source presence is not evidence that a remote task has run.

### Exact ownership / installation dependency

Snapshot 2026-10-29 20:17 UTC: #114 `47b26f98` remains the capability/transport
owner; #112 `e4a2d186` owns installed self-knowledge discovery; #125 `421045dd`
owns the active model-child/subject/Store cutover. The latter's published changed
files include `shenko-store/src/external.rs`, `child.rs`, subject child/model-child
and shared types, **not** `shenko-exec-ssh/src/task_runner.rs`, `lib.rs` or its
Cargo manifest. This increment edits none of those active Store/subject/types
files. The #125 native authority construction and assignment memo names were
statically checked and still match this boundary; combined execution is untested.

The remaining shortest integration is an installed narrow child handler:
1. expose capability observations/explicit choice, not Node management RPC;
2. obtain invocation, role/authority and resource limits from its authenticated
   registry context, never from model JSON;
3. resolve only the explicitly chosen node through the existing authorized
   registered-target map; missing permission/resource capacity returns a reason;
4. call `execute_selected`; settle the task through the current scheduler and
   reconcile armed work using its original operation ID.

That installed handler and actual registered-target authority/resource resolver
are still dependencies. The trusted API cannot authenticate arbitrary callers
or invent grants; it preserves existing SSH trust/permission boundaries and T1,
not a new general permission system. Model discovery and real remote-node
validation remain unperformed. No external owner/Agent was contacted for this
increment; dependencies are recorded in this PR rather than presumed accepted.

### Bridge validation — separate from the older 18 passing tests above

Six new tests cover actual local T1/E1 selection persistence and
query-only recovery; forged worker refusal; unrelated authority refusal;
registered-target mismatch; conflicting replay action; and byte-identical legacy
input serialization. Their SSH fixture deliberately references a missing public
host-trust CAS object, causing the existing runner to fail **before spawning
OpenSSH**. It tests unknown-effect bookkeeping, not remote success. Following
fixed-version approval, exact `d1c861f4` freshly compiled and passed all six
selected tests (six other SSH tests filtered out, not run), then all 18 node
regression tests. Both commands exited 0 with source/tree/lock unchanged under
the same Rust/Cargo 1.98.1, empty-home/offline/socket-denied conditions. One
unused test import warning was retained; it was not hidden by changing the
tested source. The [exact evidence](../evidence/agent-selected-ssh-d1c861f4-20261029.json)
contains test names, artifact/log hashes, diagnostics and scope boundaries.
This does not establish real remote execution or installed model discovery.
The lock
changes only by adding the already-existing local `shenko-node` package as a
`shenko-exec-ssh` dependency; no external dependency version is changed.
