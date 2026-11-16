# Canonical native operation arming

## Scope and reason

The two existing Store T1 entry points return `None` only to the caller that
may execute the newly armed operation. `Some(original)` is a factual read and
does not authorize another effect. This is the existing
[T1 and unknown-outcome contract](../specs/durable-state.md#spec-41-4).
Saving a chosen plan, applying its rows, or learning the first consensus
choice after a crash cannot recreate the original caller's execution permit.

This increment gives `arm_external_operation` and
`arm_native_operation_for_dispatch` a typed canonical transition when the
Store has a commit-authority provider and the task has already been claimed
as a native child. `MODEL_CHILD_KIND` is excluded, including both its quota
send and aggregate result operations. Subject publication intents are also
excluded. Other trusted executors keep their existing T1. Task creation,
claim, S0 request arming, inference quota and publication transactions are not
made canonical here.

The action is `native.arm`; its codec is `store-native-arm-transition/1`.
Existing `external.arm` descriptors retain their original interpretation and
reconciliation debt. The stable action position remains the existing task
ID plus operation logical-key digest, so a new descriptor cannot choose an
alternative for an already ordered operation position.

## Original identities and exact writes

The plan includes the complete original task, invocation, operation and
attempt, plus whether the caller requested the paired dispatch marker. The
operation retains its final input, binding, contract, authority, acceptance,
retry and checkpoint references. The attempt retains its original invocation,
worker, node, implementation, transport and provider identities. Referenced
CAS objects travel through the existing admission closure.

The source validates its active invocation, native assignment, epoch,
generation, cancellation state and owner chain. The current dispatcher and
Store authority are checked in the same IMMEDIATE transaction before capture;
those local rows are not copied into the canonical predecessor. The shared
read set records the subject and task state, operation/logical-key absence,
attempt absence, and exclusion of a publication intent.
The typed reader accepts only the existing version-1 task, operation and
attempt records; familiar fields do not make a future record schema readable.

The typed validator independently permits exactly three writes:

| Row | Frozen result |
| --- | --- |
| Original task | One record-revision increment, original result `CommitRef`, last-operation memo, and optionally that attempt's dispatch marker |
| Original operation | Insert the complete original request with its original result `prepared_commit` |
| Original attempt | Insert the complete original attempt with that same `armed_commit` |

Operation and attempt creation timestamps are explicitly the captured UTC
timestamp. Recovery does not evaluate SQLite's current clock. The validator
checks every changed SQL cell, including these timestamps, source commits and
the optional marker. It rejects changed identities, an existing operation or
logical key, an existing attempt, extra task mutations, arbitrary table writes
and receipt changes. The actual writes, shared original-result event and one
local materialization receipt commit atomically.

All source `CommitRef`s and JSON provenance remain intact. Only the designated
local SQL index sequence and the receiving ledger's materialization receipt
use its own sequence. Original E1 is neither generated nor reissued by this
transition. If a later E1 has advanced an already materialized operation or
attempt, a repeated read returns those actual records rather than restoring
the frozen InFlight/Armed projection.

## A durable preparation is not a live permit

Preparation returns a boolean that is local to the call which first inserts
the descriptor. It is not serialized and cannot be reconstructed by recovery.

| Application source | Result |
| --- | --- |
| Current call inserted the preparation, retained source authority, then passed the final original-ledger and current invocation/dispatcher check | `None`, only if the grant is not a replay (or the existing availability-authority contract legitimately supplies no grant) |
| Same-process retry of an older preparation | `Some(original)` |
| Source preactivation recovery | Materialize original facts; no caller permit |
| Learned original chosen action | Materialize original facts; no caller permit |
| Already committed logical-key read, including a stale caller | `Some(original)` without rearming |

The first actual consensus choice of an older preparation can return
`replayed=false`. The explicit `Prepared` mode still cannot return `None`.
A lost reply after choose likewise leaves no execution permission; the
learned path only records the original facts. A retry with a different
random attempt or changed frozen request cannot replace the stored action.
It must retain the conflict and recover the original preparation.

The paired API includes the original native dispatch marker in the same T1
projection. The separate dispatch API therefore returns `AlreadyCommitted`
for that attempt. Ordinary T1 and a later separate dispatch remain distinct
admissions; only the latter's own live Current path can issue its `Won`.

## Recovery and remaining gaps

The existing preactivation `resume_prepared_step_commits` loop now handles
`native.arm`, and `recover_control_admission` handles its learned value. Both
use the original sealed plan; they do not call a task handler or provider.
Changed or missing predecessors retain reconciliation debt. Malformed typed
plans are errors, rather than being disguised as ordinary replication debt.

Ordinary cognition import parks executable tasks and changes their factual
records. A chosen T1 cannot overwrite that parking. This increment does not
create an authoritative imported Running task, remove its import marker or
prove foreign execution takeover. Canonical native claim and the matching
authoritative predecessor remain separate work.

The shared capture still uses conservative complete read tables and a bounded
image budget. Unrelated progress may conflict. A formerly prepared action can
therefore require reconciliation without having sent anything; absence of a
dispatch marker still does not prove an armed operation had no external
effect. Unknown results retain their existing semantics.

## Verification scope

The implementation starts from `a3739f2`. The existing native dispatch and T2
journeys now reach the canonical non-model T1 through their existing helper.
One additional test entry point uses the same Store and retained-grant
fixture. Its eight distinct necessary windows are ordinary fresh/duplicate
T1, paired fresh/duplicate T1 and marker, a source failure before choose, a
lost chosen reply, foreign parked predecessor refusal, a forged attempt
invocation in the delta, preactivation reopen, and an old worker's epoch
fence. This count reflects the independent windows rather than the number
of Rust test functions. It adds no parameter matrix.

Assertions bind the complete frozen request and attempt, original source
commit and timestamps, atomic absence of all T1 writes on refusal, one
original attempt on restart, unchanged imported parking and retained debt,
and factual rather than fresh results for every recovery path. The fixture
supplies controlled chosen grants. This is not a real network/provider or
physical device continuity journey.

Metadata-only `cargo check --offline --locked -p shenko-store --tests` passed
in 34.38 seconds, and the final record-version and duplicate-CAS changes passed
again in 26.93 seconds. The Store library retained its 13 existing rustc
warnings (14 for lib tests, including 13 duplicates); the new arming module
and journey emitted no rustc warning. The document integrity checker passed
with `errors=[]` and `product_tested=false`, and `git diff --check` passed.
Test code generation and execution are left to the parent's subsequent CI
candidate because the shared local filesystem is constrained. These successful
checks do not establish that the newly added journey has executed.
