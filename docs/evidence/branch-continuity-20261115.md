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

The two existing branch journeys in
[runtime_branches.rs](../../bin/shenko/src/runtime_branches.rs) now exercise:

| Journey | Intended observation |
| --- | --- |
| Store-first crash/reopen and original attempt | Repair only the exact Node projection; preserve the dispatched parent request; recover its unresolved input into a genuinely sealed child S0; refuse old execution; accept late original evidence; inspect predecessor without mutation. |
| Parallel branches and later rejoin | Keep the receiver's current branch, retain both real histories and clearing notes, inherit the same original input across another fork, freeze ancestor visibility, and reject missing input links, successor steps or original raw facts before partial publication. |

These are deterministic real Store/Node/CAS journeys. They do not call a model,
send a production channel action, simulate a physical power cut or prove
long-term performance. Final combined execution results will be recorded with
the exact published candidate; they were still pending when this source
checkpoint was committed.

A preceding local build of the branch journeys exited 101 after `rustc` for
`shenko-exec-host` was killed by SIGKILL, before any test ran. It is retained as
an environment/build failure, not a passing test result. Earlier fixture and
compile errors were corrected without weakening the actual dispatch or closure
checks.

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
