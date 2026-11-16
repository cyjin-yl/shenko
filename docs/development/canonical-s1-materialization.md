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
  a distinct local receipt, and an idempotent reopened Store.
- Change a captured read condition and inject a malformed late row change.
  Check reconciliation, transaction rollback, absence of a local application
  marker, and subsequent recovery of the original frozen transition.

The focused test command was attempted with one Cargo job, incremental/debug
output disabled and the shared build lock. Rustc was killed by SIGKILL during
Store code generation; neither journey executed in that attempt. The
environment reported about 1.7 GiB available memory and no swap afterward.
These are authored recovery checks, not passing runtime acceptance evidence.
The integrated candidate must typecheck and execute them when build capacity
is available. There are no new dependencies.

## Deliberate limits

S0 request arming, child claims, T1/T2 and other control boundaries still need
canonical transition integration. This increment is not a complete replicated
state machine. A receiving Store must already have the exact required
predecessor; ordinary imported task parking, diverged raw rows or already
observed effects are not undone to force a match. Missing or divergent required
state remains durable reconciliation debt. Prepared values on superseded
heads remain preserved facts, not executable work.

Read-table images currently cover the full table range and the sealed image
is bounded at 64 MiB. This intentionally rejects some otherwise independent
concurrent updates and is not a performance claim for a large long-lived
ledger. Matching schema is required. Canonical migration across schema changes,
generic automatic predecessor installation, physical external-effect
acceptance, real multi-device partition/power-loss trials and 7/30-day operation
remain separate completion requirements.
