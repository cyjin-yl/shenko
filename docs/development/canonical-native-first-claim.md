# Canonical first claim of an S1 native delegation

Current reader update (2026-11-22): new claims use codec 2 with the
[creator-key and current-successor witness](canonical-capture-budget.md#direct-native-subject-witness-2026-11-22).
The version-1 design and its original verification record below remain valid
for retained version-1 bytes; their complete step-table witness is not rewritten.
The new reader preserves the same birth proof, exact Running result, authority
and one-live-permit rules. New archives declare
[schema 11](../specs/durable-state.md), which retains the
schema-10 creator witness and also declares the later direct arm/T2 readers.

## Exact scope

This increment supplies `native.claim` with a typed frozen transition only for
the first claim of a direct-root, non-model native delegation which an original
canonical S1 actually created and this installation locally materialized. That
S1 may have been quorum chosen or admitted by the existing local availability
authority. A local `grant=None` is not evidence of quorum choice.
The task must still have its exact birth bytes: schema 1, revision 0, Pending,
Ready, no cancellation, no previous worker, and the authored decision as input.
The task is not inferred from a familiar kind string or an imported applied
event. Model children, publication, ordinary archive parking, recovery claims,
and atomic child-tool creation plus claim keep their existing protocols.

The action uses the existing `task/{id}/claim/{revision}` admission position and
the new `store-native-claim-transition/1` codec. It does not turn an older
original-plan claim into a canonical one.

## Birth and read closure

The receiver verifies the immutable S1 admission, its original head, step,
successor, result `CommitRef`, transition, decision, and the exact inserted
delegation. The decision must contain that unique native definition. Claim
preparation cannot replace the authored request or reinterpret a publication or
model child as this first-claim protocol.

Local birth additionally requires the receiving Store's nonreplicated
`control_materializations` row, its physical `commit_log` receipt, and the exact
shared S1 applied fact. The receipt binds the original admission, transition and
logical result. Ordinary cognition neither exports this local table nor creates
that receipt. A receiving installation cannot establish the proof by importing
the source's applied event. A same-installation SQLite successor retains its
historical local application proof even if the writer coordinate changes;
that proof supplies no execution authority by itself. Foreign Learned
application still requires a legitimate chosen claim grant and the receiving
installation's own actual S1 materialization. A grant-free source receipt cannot
replace either requirement or authorize a foreign worker.

This action is the first narrow exception to complete-table task reads. Its
independent typed validator proves the target is directly owned by the subject
root, so there is no omitted ordinary ancestor. The existing owner/branch
helpers stop at that structural root before consulting root execution state.
Only the target task row belongs to the shared task preimage. The source root
is still a full expected read and a Node revision fence. The receiver's root
may retain its unrelated archive parking and is never overwritten.

The sealed predecessor reader independently checks the target, source root,
and creating step against the same immutable cognition snapshot. The claim
envelope must contain those exact expected reads. Source capture alone is not
accepted as proof of that closure. Subject head and subject-step reads retain
their complete ranges; the original operation's absence is also a required
complete read. The typed validator rejects an omitted dependency before any
row application. Local dispatcher ownership and writer incarnation remain
source checks, outside the shared read image.

## Frozen Running fact and one live permit

The only writable row is the original task. The reader reconstructs and checks
every changed cell: revision 1, Running in the original invocation, chosen
implementation, deterministic operation/result identities, execution assignment,
original source `CommitRef` and captured first-run timestamp. Every other task
field stays equal to its original birth. Operations, attempts, E1, results, root
rows and unrelated tasks cannot be written by this transition. The exact row,
shared original-result event, local materialization marker and physical receipt
commit in one IMMEDIATE transaction.

| Path | Public claim result |
| --- | --- |
| The current call first inserted this preparation and retains its original live worker, dispatcher and authority at the final transaction | `Some(NativeTaskClaim)`, only with a nonreplayed grant or the existing grant-free availability authority |
| Retry of an older preparation, even when its first actual choose has `replayed=false` | `None`, after original fact materialization |
| Preactivation prepared recovery | No worker permit |
| Learned chosen claim on a compatible installation | No worker permit |
| Duplicate call after Running was installed | `None` |

`Current`, `Prepared` and `Learned` are explicit application modes. The fresh
preparation bit exists only in the live call; it is not serialized into evidence
that recovery could replay as a permit. No application mode calls a handler or
provider. This is a claim boundary; even its live winner must subsequently pass
the appropriate original T1 admission before any external effect. The existing
grant-free availability path still requires the current source authority,
dispatcher and worker checks; it does not acquire a quorum certificate by
materializing local rows.

After a source crash, the existing ordered native worker-recovery transaction
can move the original Running task to Pending with a new record revision and
`NATIVE_RECOVERY`. If there is no original operation, the existing current
recovery-claim protocol can create a new invocation. An existing operation
requires its original settlement path. That continuation does not grant another
permit from the historical first-claim admission, and this increment does not
make recovery claims canonical.

## Remaining boundaries

A compatible receiver can now retain the original authoritative Running fact
for this specific S1 birth without unparking its imported root. This does not
authorize executing the source worker on that receiver. Ordinary imported
target tasks retain their parking and reconciliation debt. The subsequent
native arming and T2 transitions still have their own complete read sets and
authority requirements; this increment does not prove their full foreign E1/T2
chain, general task takeover, model quota, publication or S0 replication.

The shared capture still reads conservative full tables before narrowing this
one proven task dependency, with the existing capture/image budgets. Unrelated
head, step or operation progress may require reconciliation. A malformed seal
or omitted typed dependency is an error, not permission to fall back to a weaker
claim. This protocol does not clear Unknown external outcomes.

## Verification

The implementation starts from `15cfee0`. One Store journey reuses the existing
distinct-ledger S1 setup and retained chosen-grant provider used by native T2.
It covers eight concrete windows: a fresh claim; its duplicate; compatible
foreign learning; ordinary archive parking with only a source applied fact;
an unchosen preparation retried in the live process; omission of an operation
read dependency from a retained plan; a lost chosen reply recovered as fact;
and preactivation reopen of an unchosen preparation followed by explicit
worker-loss recovery. The count describes the actual windows, not Rust test
function names. There is no parameter matrix.

The assertions retain original task/request/invocation/commit/time identities,
parked root and target records, unchanged siblings, fatal malformed-plan refusal,
idempotent receipts, factual recovery results, and zero operations or attempts
created by claim materialization. The provider fixture supplies controlled
chosen grants; this is not real network consensus or a physical effect journey.
All eight windows use that grant path. They passed on candidate `9b5a6cb` in
[CI 43894092386](https://github.com/cyjin-yl/shenko/actions/runs/43894092386),
whose Store roundtrip target passed 39 tests with none failed or ignored.
There is no new execution evidence for the grant-free availability path. Its
real native-child journey remains a separate acceptance gap.
`cargo check --offline --locked -p shenko-store --test store_roundtrip` passed
(the final check completed in 1.04 seconds). Focused Clippy metadata compilation
using the repository's `clippy::all`, `unwrap_used` and `expect_used` warning
groups passed on the final source in 14.75 seconds, with no diagnostic in the
new module, predecessor verification or new journey. The Store retained its
13 existing rustc warnings; this focused inspection is not a full baseline
ratchet comparison. Documentation checks, including `--verify-migration`,
returned `errors=[]`, and `git diff --check` passed. No local test code generation
or execution was performed locally. The later remote result is retained with
the [complete original job and workspace logs](../evidence/ci-9b5a6cb-20261116/README.md).
That CI still failed the separate component-process setup comparison; it did
not test the subsequent mainline integration or its fixture correction.
