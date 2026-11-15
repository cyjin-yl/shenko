# Runtime authority and bounded chosen-action recovery — 2026-11-15

This implementation connects the existing Node control log to the Store's
actual subject and effect admission boundaries. It does not establish a fully
replicated Store state machine. The remaining canonical write-unit boundary is
explicit below; no acceptance entry is promoted to `verified_real` here.

## Invariants and implementation

`CommitAuthorityProvider` is a generic interface in `shenko-types`; neither
Store nor the subject names the concrete Node implementation. The binary's
`runtime_authority` adapter prepares a real quorum control admission before
reserving Node. Its owned SQLite `BEGIN IMMEDIATE` connection then remains
reserved until the Store transaction commits or rolls back. The lock order is
Node, the Store mutation gate, then the Store SQL transaction. Source checks do
not use a polling watcher or a cached pre-read as commit authority.

Each global admission freezes its original subject/branch, logical action,
controller/software generation, predecessor, and content-addressed action and
predecessor evidence. Task controls consume the current task revision as a
write position; their ancestors are read dependencies. A chosen cancellation
can therefore fence a descendant grant before local dispatch, even if the
cancellation's Store projection is delayed. Current read dependencies are
checked again while the final Node reservation is held. Historical grants
remain readable and cannot renew permission to send.

S0 request arming, request dispatch, S1, native task creation/claim/cancellation,
native T1/dispatch/T2, and publication creation/arming/refusal/settlement enter
these same ordered boundaries. Ordinary task claims/recovery cannot adopt a
retained parent-branch task solely because the agent ID and epoch match.
Candidate scans skip those retained rows without rewriting their original
facts. Generic WriteBatch cannot create new armed attempts or mutable task
control in an authority-bound ledger; immutable replay and independent E1
facts remain ingestible.

`control_materializations` is a local receipt in the existing Store database,
written atomically with the actual transition. It is deliberately absent from
cognition exports. The replicated `authority.admission_applied` inbox event
remains evidence of its source; importing it cannot create local materialization.
The provider is told that an action completed only after SQL COMMIT succeeds.
A chosen value whose final reservation or Store transition fails requests the
ordinary runtime's coherent reconstruction. A storage error remains fatal and
is not disguised as a retryable authority transition.

Takeover changes the current head, registered step, activation selector,
dispatcher owner and durable authority binding in one Store transaction. A
superseded live incarnation cannot reclaim that slot. The durable required-source
marker survives reopen, so omitting the adapter refuses new control while late
original E1 remains appendable.

## Recovery and actual availability branches

Startup resolves authority before constructing Inbox, context registry,
dispatcher or SubjectLoopState. It checks local materialization first. An
unapplied S1 may finish only on the original ledger with the exact original
head, epoch, registered step and task read set, without rebasing. An imported
source-applied receipt without local materialization is not permission to run
S1 again. No recovered grant in this path initiates physical dispatch.

A selected local global primary that cannot materialize a chosen current-branch
plan retains a durable reconciliation event and opens an actual availability
branch through the Store-first fork/Node projection protocol. Its reason is
`selected-plan recovery unavailable`, distinct from unavailable quorum. Followers
do not fork merely because their prestate is missing. The event retains the
chosen grant ID, original branch, action root, predecessor root, and any
actually observed original operation/request outcome. Absence of evidence is
not reported as a fabricated physical unknown or a proved absence of effect.

Only these reconciliation events enter the ordinary S0 inbox; applied admission
and binding facts do not create a feedback loop. Debts stay queryable after
notification consumption through `pending_control_reconciliations` and the
existing Self status API. A real runtime authority transition cancels the old
host, bounds recovery cleanup, retains the installation owner lock, and
reconstructs Start with a new branch-scoped context registry. Original requests,
attempts and late receipts keep their original identities.

## Remaining semantic boundary

The selected control value contains a frozen action and verified predecessor
facts. It does not contain a versioned canonical Store write unit with fixed
cross-ledger CommitRefs, timestamps, raw sequence assignments, and conditional
read/write results. Re-running the same S1 on another ledger can produce a
different successor CommitRef. Ordinary cognition import also intentionally
parks work and preserves conflicts; it is not an exact control-transition
materializer. Cross-source exact continuation therefore remains unavailable in
that case. The implemented fallback is an explicit branch with visible debt,
not a claim that all replicas reached the same authoritative Store projection.

A local source reservation cannot retract a physical effect after its original
send permit. Such attempts retain their original operation/attempt/target
fencing and may finish late; their results remain evidence, not authority to
create a new successor or send. This report claims neither Byzantine proof nor
physical global single activity across partitions.

## Verification status

The focused Store library check passed during implementation with the shared
one-job, non-incremental profile. Two independent fault journeys are authored:
source reservation through an actual Store commit and ancestor-cancellation
race; and a process exit after durable quorum selection before Store apply,
followed by actual fork/context reconstruction and retention of the original
unknown request. Their execution and the integrated binary/Clippy result are
recorded by the follow-up verification checkpoint; authored tests alone are
not a passing result. No real-model, physical multi-device or long-run evidence
is asserted.
