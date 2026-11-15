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

Owner traversal retains the actual current subject root as a read dependency
and stops there. Genesis may represent that root as self-owned; this terminal
link does not make ordinary native task admission a cyclic owner chain. Other
cycles, retained foreign-branch roots and cancelled ancestors remain fenced.

S0 request arming, request dispatch, S1, native task creation/claim/cancellation,
native T1/dispatch/T2, and publication creation/arming/refusal/settlement enter
these same ordered boundaries. Ordinary task claims/recovery cannot adopt a
retained parent-branch task solely because the agent ID and epoch match.
Candidate scans skip those retained rows without rewriting their original
facts. Generic WriteBatch cannot create new armed attempts or mutable task
control in an authority-bound ledger; immutable replay and independent E1
facts remain ingestible.

A child-authored publication keeps its original parent task revision as an
additional authorization dependency. Cancellation consumes that dependency;
the publication's final T1 checks it under the source reservation. Normal
completion of a child does not by itself revoke a reply already authored.

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

An availability opening records its holder, term, configuration and current
fence in the same Node transaction. The adapter and the Store-fork reservation
check those opening facts again. An open historical branch cannot bypass a
newly learned term or a new global primary. When the same actual branch later
wins a global election, new work needs ordinary quorum admissions again.
Automatic election fallback is limited to a node that received no authenticated
response from another configured voter. Reachable refusals do not justify a
partition fork, and response counts do not substitute for a quorum certificate.

Every Start exit after authority attachment passes through one result
classification after ordinary or bounded cleanup. A closed Store writer adds a
storage failure even if an earlier error requested authority recovery. The
installation owner restarts only a recovery result without a storage failure.

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

Connected minority coordination is also not implemented by this change. A
node that reaches other configured voters but cannot select a primary returns
`PrimaryNotSelected`; it does not independently manufacture a component-wide
branch or authority from those responses.

A local source reservation cannot retract a physical effect after its original
send permit. Such attempts retain their original operation/attempt/target
fencing and may finish late; their results remain evidence, not authority to
create a new successor or send. This report claims neither Byzantine proof nor
physical global single activity across partitions.

## Verification status

The integrated source at
`60e996327f0a191f60a73b5b4e7625b7242fbe09` passed the combined runtime
journeys on 2026-11-15. The subsequent `dacf8c1` change touched documentation
only. The command ran from
`/workspace/scratch/4de31219f954/shenko-ship`:

```bash
cargo test --offline --locked -j1 -p shenko --lib runtime_ -- --nocapture --test-threads=1
```

Both commands in this section ran under
`flock /workspace/scratch/d013f3e3b824/shenko-build.lock`, with
`/home/agent/.cargo/bin` prepended to `PATH`,
`CARGO_TARGET_DIR=/workspace/scratch/d013f3e3b824/shenko-target`,
`CARGO_BUILD_JOBS=1`, `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`
and `CARGO_PROFILE_TEST_DEBUG=0`. No `RUSTFLAGS` override was used.

The result was **5 passed, 0 failed, 0 ignored**: these four new journeys and
one existing owner-lock regression.

| New journey | Observed boundary |
| --- | --- |
| `runtime_authority::tests::reserved_store_commit_and_ancestor_cancellation_fence_live_owners` | A real Node write reservation remains held through Store COMMIT; a chosen ancestor cancellation fences a delayed descendant grant; takeover binds the head, step and activation selector coherently; late original E1 remains appendable. |
| `runtime_authority::tests::chosen_unmaterialized_action_reopens_on_a_real_branch_with_original_unknown` | A process exits after actual quorum selection and before Store apply; reopen preserves the original unknown request and reconstructs an actual branch with visible debt. A new term revokes that branch's opening authority, and later global selection of the same branch makes a fresh native admission enter the quorum log. |
| `runtime_branches::tests::fork_recovers_between_stores_without_replaying_original_attempts` | Store-first fork recovery repairs only its exact Node projection, retains original attempts and input provenance, and seals a real child S0. |
| `runtime_branches::tests::parallel_branch_rejoin_keeps_local_head_and_refuses_incomplete_ancestry` | Rejoin preserves the local active head and both histories; frozen ancestry and original input links survive further forks; incomplete closure is refused before partial publication. |

The fifth test,
`runtime_owner::tests::same_directory_inode_excludes_another_owner_without_creating_a_file`,
is an existing regression and is not counted as a new journey. The nested
process launched by the chosen-action test is that journey's crash injection,
not another independent scenario.

The retained log is
`/workspace/scratch/4de31219f954/combined-runtime-journeys-verified.log`.
Its SHA-256 is
`26082f08b2755dee6a0d437fe6aa0b4bf80061af6cd8a0dbcab01cd22804bd58`.

At the same source revision, the existing archive-membership regression also
passed after being extended to reject omitted membership in both schema 6 and
the current schema 7 without publishing rows or advancing the commit sequence:

```bash
cargo test --offline --locked -j1 -p shenko-store --lib unknown_fact_does_not_produce_a_verified_partial_import -- --nocapture
```

Result: **1 passed, 0 failed, 0 ignored**. Its full test name is
`cognition::archive_admission_tests::unknown_fact_does_not_produce_a_verified_partial_import`.
The log is `/workspace/scratch/4de31219f954/archive-membership-final.log`,
SHA-256 `45107521d43e70ab4d7d4be503c7a33d1da32bb5b3a58ba2948882dc9a7b57d2`.
This extends one existing admission regression; it does not claim a new
historical two-binary compatibility proof.

An earlier combined run stopped at an FK error while its dedicated test
connection tried to remove a required ancestor referenced by staged input.
`60e9963` disables foreign keys only on that fault-injection connection, so the
fixture can construct the deliberately incomplete source archive. Production
Store connections and import validation retain their constraints. Earlier
`rustc` SIGKILL exits occurred before test execution and are not passing results.

The focused Store library check also passed during implementation. Final Node,
Clippy and wider acceptance checks are tracked separately; they were not yet
complete when this evidence checkpoint was written. No real-model, physical
multi-device or long-run evidence is asserted.
