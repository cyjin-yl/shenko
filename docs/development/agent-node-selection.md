# Agent-owned node choice / Agent 自主选择执行节点

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
