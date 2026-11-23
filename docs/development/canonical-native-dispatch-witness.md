# Canonical native dispatch ancestry witness

The candidate adds `store-native-dispatch-transition` codec 2 with row schema 5.
It replaces the generic dispatch's whole-table read witnesses with its actual
bounded structural dependencies. This is a separate proof from direct native
S1 birth, first claim, paired T1 and original E1. Model-child provider sends use
this generic dispatch boundary through `ChildStepSink::may_dispatch`.

## Why this dependency matters

The retained `cdf71091` browser failures demonstrate chosen arm/T2 actions that
could not materialize after unrelated rows advanced. The dispatch path has the
same source-level mechanism: it reads one task, its ownership chain, the current
subject, one operation and one attempt, but its former row engine froze complete
`tasks`, `subject_steps`, `operations` and `attempts` tables. A concurrent sibling
arm, E1 or S0 could therefore invalidate a dispatch after choice. This dispatch
finding is a source analysis, not an additional observed model failure.

## Closed structural reads

The new typed reader derives four selectors from the original task and head:

| Table | Required rows |
| --- | --- |
| `tasks` | The target and every task ancestor, ending at the current subject root or the conversation owner. The root remains a complete read and Node revision fence. |
| `subject_steps` | Every creation step actually consulted by the target/ancestor branch check. The current successor is independently revalidated for existence, binding and paused-boundary consistency. Its unrelated mutable S0 fields are not frozen. |
| `operations` | The full original operation row at its physical primary key. |
| `attempts` | The full original attempt row at its physical primary key. |

There are no sender-supplied SQL clauses or arbitrary lists of keys. Capture and
both materialization checks follow the same bounded owner traversal. The frozen
reader independently verifies physical/JSON identities, subject identity,
ownership termination, branch binding and active non-root ancestors. Missing,
extra or substituted authoring rows are refused. Exactly the named task may
receive its original dispatch marker; operation and attempt projections remain
unchanged.

The admitted `conflict_aliases` must equal the target's original record-revision
position. `required_unconsumed` must equal the sorted, deduplicated ancestor
revision positions. The envelope's `expected_tasks` must equal the complete
ordered chain, with empty `snapshot_tasks` and `expected_steps`. This retains the
Node cancellation fence even when a chosen ancestor cancellation has not yet
materialized in Store. Checking only the target alias would be insufficient.

Source capture refuses trigger/view reads of any selected table, and the
physical observer refuses writes outside the selected rows. Full schema and
trigger digests remain bound. Every new Current, Prepared and Learned application
rechecks `read_subject` and `ensure_owner_active` inside its final IMMEDIATE
transaction, along with exact physical predecessors, writes and postimages.

The shared row-schema-5 support also offers a separately selected ordinary-child
range for generic T2. That range includes every non-background direct child,
including terminal rows and absence, and compares decoded task identities.
Dispatch never enables that range. Each action's typed reader must bind its
entire selector set and result independently.

## Recovery and version boundaries

Only the current original live preparation, with a fresh grant and final live
invocation check, may return `Won`. Prepared, learned and repeated applications
retain the original marker and return `AlreadyCommitted`; they do not call the
provider. A failed or unknown fence remains Unknown through the broker's existing
receipt path. This change adds no retry or replacement of chosen facts.

Codec 1 retains row schema 1 and its original full witnesses. `DispatchPlan` has
no added serialization fields, so old input digests keep their original meaning.
New codec 2 requires row schema 5 and validates its full typed object identity.
The current archive capability declaration is maintained in
[durable state](../specs/durable-state.md); raw Node object transport remains a
separate compatibility boundary.

## Verification scope

The existing
`canonical_native_dispatch_retains_one_live_permit_through_prepare_learn_and_restart`
trajectory now dispatches a descendant with a real parent revision fence. Its
one added concurrent window advances S0, cancels a separate task and records
that task's late Unknown E1 after the target's choice. The original fresh,
unchosen preparation, lost reply, malformed marker, imported parking, late E1
and reopen assertions remain. The malformed marker uses the actual new codec,
so a version mismatch cannot accidentally satisfy that rejection.

This is source and scoped-regression work, not a new real-model pass or measured
latency result. Actual execution receipts must bind their own candidate and
environment. Full predecessor construction, schema inspection and generic T2's
ordinary-child scan remain separate costs.
