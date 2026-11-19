# Canonical S1 materialization

## Why

A chosen action must retain one result when its caller loses the consensus
reply or dies before Store COMMIT. Re-running ordinary S1 on another ledger
would allocate new source coordinates, raw event IDs and timestamps, and could
settle a semantic wait differently. An imported source receipt only proves
what that source recorded. It cannot prove that the receiving Store applied
the same change. This increment implements the S1 part of the
[connected Store contract](../specs/nodes.md#connected-store-admission-and-materialization).
It does not complete every distributed admission boundary.

## Implemented mechanism

`crates/shenko-store/src/control_transition.rs` evaluates the existing S1 SQL
body under an IMMEDIATE transaction and rollback savepoint. SQLite's authorizer
records its table accesses; prepared SQL is evicted before observation. The
source captures complete preimages for read tables, affected-key preimages for
insert-only tables, and exact typed cell changes. The current head and task/step
read conditions are checked inside that same transaction. The savepoint is
rolled back before the frozen value is admitted.

The [capture capacity repair](canonical-capture-budget.md) retains touched
postimages before that rollback and reads the real touched preimages afterward,
under the same IMMEDIATE transaction. It evaluates the SQL body once. Independent
streaming physical fingerprints still check every other base table for writes
that the observer did not report; this removes full-database row copies, not
full-database scanning.

The source reserves its logical result `CommitRef` in a durable preparation
row. That commit is preparation, not an S1 success. The sealed transition keeps
the original action, S0/decision/result objects, resulting head and successor,
generated IDs, timestamp values, raw session sequence, identity/wait/obligation
outcomes, and typed references extracted from the SQL JSON cells. The ordinary
CAS closure walk can therefore carry all required bytes. An exact retry reads
the preparation; it never silently substitutes a new candidate at that action
position. `resume_prepared_step_commits` permits the source runtime to resume
preparation after a crash, before revising its epoch. The original control
predecessor remains preserved evidence.

Startup recovery distinguishes applied preparations, unresolved chosen grants
and unresolved local preparations with no quorum proof. A changed required
readset becomes durable reconciliation evidence and a typed recovery result;
authority unavailability remains a retryable authority error. A malformed
transition or corrupt storage is fatal instead of being hidden by a new branch.
Local preparation debts retain an explicit absent chosen grant and remain
visible through the same reconciliation listing as chosen-plan debts.

Application holds the verified Node grant reservation across one local Store
transaction. It checks table/column/primary-key/index/trigger layout and the
frozen preimages, applies explicit INSERT/UPDATE/DELETE operations, verifies
postimages, and records a local application receipt atomically. Foreign-key
checks are deferred to COMMIT for transaction-wide ordering; normal uniqueness
and row identity constraints still apply. SQL rowids are not copied. Existing
triggers rebuild derived search indexes using the receiving rowids.

Only columns already declared node-local by cognition replication are
normalized to local sequence markers. Source-scoped `CommitRef`s and provenance
inside JSON, original per-session `seq`, intent creation coordinates and other
factual values remain frozen. The shared admission event contains the original
logical result and agrees across Stores; its imported bytes are checked but
never treated as a local receipt. The nonreplicated materialization table
contains the receiving ledger's physical commit reference. Restart/retry reads
that table and performs no second application.

Installation identity recognizes a verified local materialization receipt even
though the head retains its original source-ledger coordinate. It checks the
indexed admission, original applied event and actual physical commit-log row.
Plain imported history and imported source receipts establish no such binding;
more than one locally established subject remains an explicit integrity error.
This lets a real synchronized installation identify the adopted self without
renaming provenance or granting a follower primary authority.

The evaluator may observe the semantic clock once, and its in-memory monotonic
high-water observation remains conservative after SQL rollback. The observed
outcome is part of the frozen bytes. Applying those bytes does not run S1 again,
consult a clock for its semantic result, allocate new identities, dispatch a
model request or send an external operation. A follower with a verified learned
grant can perform this factual application without claiming current primary
authority. Any later dispatch still requires its existing dedicated admission.

## Evidence and remaining verification

This work starts from `110dedc2764abc7983aed6d7bb0618846730b9a4` (PR #299).
An initial `cargo check --offline --locked -p shenko-store` passed with the
existing Store warnings before the final descriptor/closure hardening. Two
focused journeys were added to `crates/shenko-store/tests/store_roundtrip.rs`:

- Lose the selected S1 reply, recover its original source result, import the
  source applied event into an independently identified Store, then materialize
  the same frozen S1 there without a primary stamp. Check original head/task/raw
  event identities, unchanged parked preexisting work, no operations/attempts,
  a distinct local receipt, an idempotent reopened Store, and an installation
  identity established only by local application (with ambiguous local subjects
  refused).
- Change a captured read condition and inject a malformed late row change.
  Check reconciliation, transaction rollback, absence of a local application
  marker, and subsequent recovery of the original frozen transition.

Materializing the frozen S1 on a distinct ledger produces the identical
original result, not an identical ledger. The receiving Store keeps its own
physical materialization receipt and its own local sequences, and a
conflicting read set leaves durable reconciliation-debt rows on the receiving
side (`retain_control_reconciliation`, `retain_prepared_step_debt`). A failed
materialization leaves no partial rows at all: the malformed/partial-fault
case rolls the whole transaction back, so no applied row and no local
materialization marker survive it.

The focused test command was attempted with one Cargo job, incremental/debug
output disabled and the shared build lock. Rustc was killed by SIGKILL during
Store code generation; neither journey executed in that attempt. The
environment reported about 1.7 GiB available memory and no swap afterward.
That local attempt is not runtime acceptance evidence. The two journeys later
executed and passed in the Nightly scope recorded by the
[component continuation evidence](../evidence/component-coordination-20261116.md)
(`canonical_s1_recovers_identical_results_on_a_distinct_ledger_without_dispatch`
and
`canonical_s1_conflicting_readset_and_partial_sql_fault_never_publish_a_receipt`);
that scope is a focused Store journey, not a distributed acceptance claim.
There are no new dependencies.

## Deliberate limits

S0 request arming, child claims, T1/dispatch, publication T2 and other control
boundaries still need canonical transition integration. The follow-up
[native T2 increment](canonical-native-t2-materialization.md) implements only
the paired native completion/finish transactions. Neither increment is a complete replicated
state machine. A receiving Store must already have the exact required
predecessor; ordinary imported task parking, diverged raw rows or already
observed effects are not undone to force a match. Missing or divergent required
state remains durable reconciliation debt. Prepared values on superseded
heads remain preserved facts, not executable work.

Read-table images currently cover the full table range. Row capture charges
owned before/after cells, expanded keys and duplicated change rows against one
128 MiB budget. JSON size is measured through a bounded sink before allocating
expanded keys or the at-most-64 MiB sealed image. Unrelated history contributes
scan work but no retained row image. These are row-capture and serialized-image
limits, not a bound on total SQLite, parser, CAS or process memory. If either
specific ceiling is reached before preparation, the canonical path cannot
freeze the transition and this S1 attempts the ordinary original-plan admission
instead. A separate fail-soft `store_capture_budget` report records
the observed refusal and intended fallback; only the later actual receipt can
prove a commit. The report is not an admission or a persistence acknowledgement.
Existing prepared values, bad seals and ordinary conflicts never take this
capacity path. This intentionally
rejects some otherwise independent concurrent updates and is not a performance
claim for a large long-lived ledger. Matching schema is required. Canonical
migration across schema changes,
generic automatic predecessor installation, physical external-effect
acceptance, real multi-device partition/power-loss trials and 7/30-day operation
remain separate completion requirements.
