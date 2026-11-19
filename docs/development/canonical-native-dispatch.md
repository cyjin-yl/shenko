# Canonical native dispatch

## Boundary and reason

`Store::commit_native_dispatch` is the existing one-send decision for an
already armed native attempt. Its production consumers are the child model
and intervention receipt sinks: `Won` permits sending, `AlreadyCommitted`
requires original-result recovery, and `Fenced` refuses the send. Making its
Store rows reproducible must not turn recovery into a second physical send.
This follows the [T1, effect and unknown contracts](../specs/durable-state.md#spec-41-4).

The authority-bound boundary now uses the shared `RowTransition` mechanism
with a separate typed dispatch plan. It does not make operation arming,
native claim, publication settlement or S0 canonical. It does not remove
cognition-import parking or install an authoritative task predecessor.

## Exactly one original marker

The plan names the original task, invocation, operation and attempt. It checks
the original epoch, software generation, worker and node, the running task,
the armed attempt and the absence of a final receipt or earlier dispatch.
Imported, cancelled and settlement-only task states cannot prepare a fresh
dispatch. The task and its owner chain are captured under the original
authority; local dispatcher ownership is checked outside the shared read
capture, in the same IMMEDIATE transaction.

The source evaluates only the factual task write under a rollback savepoint.
Its validator independently derives the allowed task: add that attempt's
marker with the original logical result `CommitRef`, increment its record
revision once and retain every other task field. Every changed SQL cell must
match that projection. Additional task changes, E1/operation/attempt writes,
new or deleted facts and a substituted marker commit are rejected.

The original source result commit and capture timestamp are durably sealed
before consensus. A receiving Store applies the exact original bytes and
timestamp. Only the task's explicitly local SQL index sequence and its local
materialization receipt use the receiving Store's sequence. The task marker,
shared applied event and local receipt commit atomically. An imported source
event cannot create the local receipt.

## Preparation is not a live send permit

The source preparation method returns whether **this live call** inserted the
durable preparation. That fact is not reconstructed from the plan and is not
inferred from the provider grant:

| Application source | Required check | Return value after materialization |
| --- | --- | --- |
| Same live call that first sealed the plan | Current authority, original source ledger, active invocation and local dispatcher; grant is not a replay | `Won` |
| Existing source preparation, including a same-process retry | Reserved source authority and exact frozen predecessor | `AlreadyCommitted` |
| Preactivation startup recovery | Recovery reservation and exact frozen predecessor | `AlreadyCommitted` |
| Learned chosen action | Verified original chosen grant, bound authority source and exact frozen predecessor | `AlreadyCommitted` |

An availability authority may legitimately return no consensus grant; the
existing source-authority contract still governs its fresh live call. This
does not let a prepared or learned application return `Won`.

A provider can first choose a plan after the source has restarted, returning
`replayed=false`. The explicit prepared application mode still prohibits
`Won`. Conversely, a live call that loses its reply after choose has no
permission to send; learned recovery only installs the original decision.
An already materialized marker is a factual read even after cancellation or
later original E1. It never changes the attempt back to armed or supplies a
provider acknowledgement.

## Recovery and limits

`resume_prepared_step_commits` routes the dispatch codec in its existing
preactivation loop. `recover_control_admission` uses the learned path. No
provider effect runs in either path. As with S1/native T2, missing or changed
predecessors retain reconciliation debt. In particular, ordinary cognition
import changes and parks nonterminal tasks; this action cannot overwrite that
parking merely because the source had permission to dispatch.

The shared read sets remain conservative full-table snapshots and can
conflict on unrelated progress. Canonical operation arming and claim, an
authoritative task-predecessor migration, real Node/provider continuity and
physical multi-node failures remain separate obligations.

## Verification scope

The implementation starts at `8ac497a`. One test entry point reuses the
existing native T2 authority and task fixtures. It follows a task through
a fresh marker, duplicate reads, a failed preparation before choose,
first-choice recovery without a send permit, a lost chosen reply, a malformed
marker refusal, a foreign parked predecessor, late original E1, and a Store
reopen. It checks original operation/attempt/E1 identity, the source commit
and timestamp, the distinct physical receipt and one local materialization.
The authority is a controlled retained-grant fixture; this is not a real
network/provider execution or foreign executable-task takeover journey.

These are distinct necessary fault windows for this dispatch mechanism;
putting them in one function does not make them one independent case. No
parameter matrix or extra fixtures expand the scenario count.

`cargo check --offline --locked -p shenko-store --tests` passed in 34.72
seconds. The Store library's 13 warnings and the existing test warnings were
unchanged constructs; the new dispatch module and journey emitted no new
rustc warning. The document integrity check passed with `errors=[]` and
`product_tested=false`. These checks did not execute the journey. Test code
generation was left to subsequent CI because the shared local filesystem had
approximately 747 MiB free. At that point the test windows had not yet executed.

Subsequent [CI 43872078834](https://github.com/cyjin-yl/shenko/actions/runs/43872078834)
actually passed `canonical_native_dispatch_retains_one_live_permit_through_prepare_learn_and_restart`
on candidate `bb27f6542e3c37c693a973b5cc26ca8d2ec8fccf`. The original
workspace excerpts and the two unrelated assertion failures are retained in
[`ci-bb27f65-20261116`](../evidence/ci-bb27f65-20261116/observation.json).
This establishes the controlled Store recovery journey for that exact source;
it does not establish the real network/provider and foreign predecessor
boundaries described above, and the overall Rust job remained failed.
