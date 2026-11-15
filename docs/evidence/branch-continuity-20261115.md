# Real branch continuity and retained input evidence — 2026-11-15

This continuation belongs to [Shenko PR #299](https://github.com/cyjin-yl/shenko/pull/299)
and [the node continuity contract](../specs/nodes.md#spec-5-4). It fixes the
Store side of an actual availability branch. A Node branch fact alone did
not create a new subject head, root task, successor or context history.

## Implemented boundaries

- [The Store fork](../../crates/shenko-store/src/subject_branches.rs) conditionally
  replaces the exact current head and creates one fresh root and planned first
  step in the same transaction. It retains the parent head, immutable fork
  receipt, original genesis and frozen raw-history prefix. A repeat with the
  same branch and parent returns the original receipt; changed facts conflict.
- Original request, attempt, task and raw-event identities remain on their
  original branch. The child has no inherited active request or attempt. A
  late original Node result remains evidence; its old ticket cannot authorize
  a fresh execution on the child.
- [Node projection recovery](../../bin/shenko/src/runtime_branches.rs) repairs
  only an exact locally committed Store fork. It verifies the CAS receipt,
  source ledger, parent identity and target branch while holding the Node
  authority reservation. An arbitrary imported head or changed genesis cannot
  repair a Node/Store identity mismatch.
- History reads follow frozen ancestor segments. Sequence and event identities
  are not renumbered, and later parent events do not enter an already-created
  child's prefix. The newest working-note revision is inherited, including
  an explicit clearing revision; an older visible note is not resurrected.
- A staged input can already have `consumed=1` before its S1 commits. The fork
  therefore freezes the pending parent step's input selections and inserts
  child first-step selection links to the **same** raw facts. It does not reset
  inbox delivery, clone a raw event or reassign a physical attempt. The usual
  context/S0 path receives these originals, including staged reconciliation
  notices, through the existing selection API.
- The original normalization lookup follows the same frozen ancestry. A
  volatile replay's new delivery time cannot replace the retained original.
  Web selected receipts still require a real sealed child S0; mere inheritance
  or staging does not report selection.

The lock order for a transition remains Node reservation → Store mutation
gate → Store SQLite. The authority adapter and ordinary runtime own election,
activation and rebuilding of branch-scoped consumers; this mechanism does not
add a scheduler or independently elect a primary.

## Recovery closure and formats

Cognition archive schema 7 carries fork receipts and historical heads.
Import checks the incoming closure and the actual reconciled Store before
publishing the import. Required facts include each head's committed and
registered successor steps, parent snapshots, exact history prefix, inherited
note revision, and both parent and child input-selection links.

Fork receipts are now schema 2. An input reference binds source step, source
event, raw event, original session and sequence. A cross-branch selection is
legal only on the declared first step and only with the exact receipt and
parent selection. Media links continue to match their original admitted
envelope. The former global unique raw-selection index is migrated to a normal
index without rewriting any row; `(step_id, source_event_id)` remains unique.

Unpublished intermediate schema-1 fork receipts did not preserve these input
references. They remain importable only if their frozen parent prefix has no
uncommitted selection requiring inheritance. Otherwise import explicitly
requires continuation repair and leaves the original material intact. It does
not manufacture a schema-2 provenance claim inside an older receipt.

## Chosen-plan predecessor evidence

[The predecessor builder](../../crates/shenko-store/src/control_predecessor.rs)
seals one consistent cognition snapshot, the exact expected task/ancestor and
registered-step reads, stable row objects, derivative objects and hash-partitioned
index pages. Task rows are compared against the same extracted snapshot that
is sealed, rather than a second live read. Pages have bounded entries and
validated paths/counts; the root records the original complete manifest
fingerprint and object count.

The page inventory indexes direct typed object roots. It does not recursively
re-index every earlier predecessor's wrapper inventory into each new wrapper.
The normal closure walker still verifies all required descendants before the
evidence is accepted. The duplicate media table specifications discovered in
the older exporter were removed.

Inspection is evidence-only: it reconstructs and verifies the archived closure
without importing Store rows, renewing a lease, sending a request or claiming
that another ledger has applied the original S1. This is not a canonical
state-machine transition. Ordinary cognition reconciliation can park tasks or
retain a different local head; a replicated applied receipt is source evidence,
not sufficient proof of local materialization.

## Verification and review status

The two branch journeys authored in this continuation, in
[runtime_branches.rs](../../bin/shenko/src/runtime_branches.rs), passed on source
`60e996327f0a191f60a73b5b4e7625b7242fbe09`:

| Journey | Observed boundary |
| --- | --- |
| `runtime_branches::tests::fork_recovers_between_stores_without_replaying_original_attempts` | Repair only the exact Node projection; preserve the dispatched parent request; recover its unresolved input into a genuinely sealed child S0; refuse old execution; accept late original evidence; inspect predecessor without mutation. |
| `runtime_branches::tests::parallel_branch_rejoin_keeps_local_head_and_refuses_incomplete_ancestry` | Keep the receiver's current branch, retain both real histories and clearing notes, inherit the same original input across another fork, freeze ancestor visibility, and reject missing input links, successor steps or original raw facts before partial publication. |

The combined command, run from
`/workspace/scratch/4de31219f954/shenko-ship`, was:

```bash
cargo test --offline --locked -j1 -p shenko --lib runtime_ -- --nocapture --test-threads=1
```

The result was **5 passed, 0 failed, 0 ignored**. Besides the two branch
journeys above, it passed these two new authority journeys:

- `runtime_authority::tests::reserved_store_commit_and_ancestor_cancellation_fence_live_owners`
- `runtime_authority::tests::chosen_unmaterialized_action_reopens_on_a_real_branch_with_original_unknown`

The remaining test,
`runtime_owner::tests::same_directory_inode_excludes_another_owner_without_creating_a_file`,
is an existing owner-lock regression, not a fifth new journey. The retained
log is `/workspace/scratch/4de31219f954/combined-runtime-journeys-verified.log`,
SHA-256 `26082f08b2755dee6a0d437fe6aa0b4bf80061af6cd8a0dbcab01cd22804bd58`.
The later `dacf8c1` change was documentation only.

Execution used the shared
`/workspace/scratch/d013f3e3b824/shenko-build.lock` with `flock`, the target
directory `/workspace/scratch/d013f3e3b824/shenko-target`, one build job,
incremental compilation disabled, both dev/test debug profiles set to zero,
and `/home/agent/.cargo/bin` on `PATH`. No `RUSTFLAGS` override was used.

The existing membership regression also passed at the same source revision:

```bash
cargo test --offline --locked -j1 -p shenko-store --lib unknown_fact_does_not_produce_a_verified_partial_import -- --nocapture
```

Result: **1 passed, 0 failed, 0 ignored**. The extended regression refuses
missing mandatory membership in both schema 6 and schema 7 and verifies that
no receiver rows or commit sequence are published. Its full name is
`cognition::archive_admission_tests::unknown_fact_does_not_produce_a_verified_partial_import`.
The log is `/workspace/scratch/4de31219f954/archive-membership-final.log`,
SHA-256 `45107521d43e70ab4d7d4be503c7a33d1da32bb5b3a58ba2948882dc9a7b57d2`.
The historical schema-5/schema-6 two-binary script remains a separate proof
with its original formats; it was not relabelled as schema-7 evidence.

These are deterministic real Store/Node/CAS journeys. They do not call a model,
send a production channel action, simulate a physical power cut or prove
long-term performance. Final Node, Clippy and wider acceptance results remain
separate from the passing commands recorded here.

A preceding local build of the branch journeys exited 101 after `rustc` for
`shenko-exec-host` was killed by SIGKILL, before any test ran. It is retained as
an environment/build failure, not a passing test result. Earlier fixture and
compile errors were corrected without weakening the actual dispatch or closure
checks.

The first combined execution also exposed an FK failure in the test's own
attempt to delete a parent step still referenced by staged input. `60e9963`
sets `PRAGMA foreign_keys=OFF` only on the dedicated fault-injection connection
used to create those deliberately incomplete source archives. Production
Store connections and admission checks are unchanged; the final passing run
still requires rejection before partial publication.

Independent source review found and closed three concrete gaps: missing
registered-successor validation, missing inherited input selections, and omission
of those links from the final reconciled-closure read. A separate review checked
index migration, multiple generations of first-step inheritance, original
normalization and media links. These reviews are source evidence; they do not
substitute for the combined runtime tests.

## Remaining acceptance

The complete product still needs canonical cross-node state transitions that
freeze all materialized outputs, generated identities, time values and read
dependencies, with logical decision positions distinct from local apply
receipts. This increment preserves an unavailable chosen plan and continues
through an explicit real branch; it does not rebase or rerun that plan in a
foreign ledger and call the result the same history. Divergent inherited
obligation-terminal closures can still require reconciliation instead of being
silently combined.

Real model, OXR, channel, seat, upgrade and 7/30-day evidence remains separately
tracked in [runtime resource observations](readiness-runtime-resources-20261115.md)
and the [full completion contract](../governance/COMPLETION.md).
