# Canonical native T2 materialization

## Boundary

This increment extends the frozen Store transaction mechanism from S1 to the
paired `native.complete` and `native.finish` boundaries in
`crates/shenko-store/src/child.rs`. It implements the existing
[T2 and ownership contracts](../specs/durable-state.md#spec-41-4): a resolved
original operation and E1 can move their current task to `Completing`, or
atomically produce the terminal task, `TaskResult` and subject inbox entry
after ordinary owned children have converged.

It does not make native claim, T1, dispatch, publication settlement or the main
request's S0 canonical. It does not install a task predecessor over ordinary
cognition-import parking. A receiver needs the exact captured predecessor;
otherwise the chosen action remains reconciliation debt.

## One original result

`native_transition.rs` retains a typed completion plan containing the phase,
original task, operation, attempt, E1 and completion memo. The admission binds
the task's consumed record revision and the original operation/attempt IDs.
The source checks its current authority, dispatcher and task/ancestor snapshot
inside an IMMEDIATE transaction. Its evaluation reads the named original E1
and attempt by primary key, never by the receiving Store's rowid or arrival
sequence. Source logical precondition changes are conflicts; an already sealed
transition failing its identity validation is an error, not permission to
reinterpret it.

The shared `RowTransition` machinery evaluates the SQL body in a rollback
savepoint and retains its exact read conditions and row changes. The source
reserves the original logical result `CommitRef` durably before consensus.
The frozen result includes terminal time, SQL creation timestamps and the
original result/inbox identity. A retry reuses the sealed preparation instead
of allocating another outcome coordinate or timestamp. The S1 wire codec and
serialized row format remain unchanged.

A current settlement invocation may differ from the invocation that performed
the original attempt. Only an explicitly claimed settlement task with its
recovery memo may use that path. The current invocation supplies T2 authority;
the E1, attempt and `TaskResult.invocation_id` continue to identify the original
invocation. A normal completion still binds the original worker, node, epoch
and invocation. Neither path creates or arms an attempt.

## Restricted application

The native validator does not accept an arbitrary SQL delta. It verifies the
original task/operation/attempt/E1 and head in the frozen read set, derives the
permitted task result, and checks every changed cell against that result:

- With unfinished ordinary children, only the named task may change to
  `Completing`, with its original completion memo. No result or terminal inbox
  entry is allowed.
- After convergence, only the named task, its one new `task_results` row and
  its matching `native-child` inbox entry may change. The result keeps its
  original operation, attempt-associated receipt and invocation identities.
- Operation, attempt and E1 rows are read conditions. They are never rewritten
  as part of T2 replay. Additional writes, substituted results or deleted facts
  are rejected.

Application holds a verified authority reservation across one IMMEDIATE
transaction. The shared row engine checks schema and exact preimages, applies
the frozen changes, verifies postimages and defers FK checks to COMMIT. The
task/result/inbox changes, shared source-result event and nonreplicated local
materialization receipt commit atomically. Source `TaskResult.commit_ref` and
its factual `task_results.commit_seq` remain original; the receiving Store's
physical receipt and explicitly local index sequences stay local.

`subject_dispatch_owner` is excluded from shared row capture. A live source
must check that local execution guard before preparation and final application;
a learned application or preactivation recovery only applies the already
frozen result under its appropriate reserved authority. It does not acquire
dispatcher ownership, call a provider or return a fresh send permit.

The existing `resume_prepared_step_commits` entry point now also dispatches
these two native T2 preparations, retaining its runtime API. Learned recovery
uses the same canonical route. Historical branches and incompatible live
predecessors retain debt; source-local stale preparations retain the existing
explicit chosen/unselected distinction.

## Evidence

The implementation starts from `7e40411`, after the initial canonical S1
increment. An initial library metadata check passed:
`cargo check --offline --locked -p shenko-store` (9.80 seconds, 13 existing
warnings). It preceded the final source-race classification adjustment and
test/doc additions. No test code generation was started: the shared local
filesystem subsequently had approximately 472 MiB free. Final candidate
all-target type checking and execution remain CI requirements.
The documentation integrity check passed with the existing CI parser
environment (`scripts/check_docs.py`: `errors=[]`, `product_tested=false`).
The first invocation with the system Python had reported missing symbol/CSS
parsers; reusing the existing environment required no new installation.

Two Store journeys were authored:

- Record original T1/dispatch/E1, reopen under a new settlement invocation,
  lose the chosen T2 reply, reject a substituted result identity, and recover
  the original frozen result after another reopen. Verify original E1 and
  attempt bytes, one result/inbox, distinct logical/physical receipts and no
  new dispatch permission. An independently identified follower imports the
  ordinary parked predecessor and must retain debt without changing it.
- Lose and recover the `Completing` transition while an ordinary child is
  unfinished, complete the child, then lose and recover `native.finish`.
  Verify that no parent result exists before convergence and that reopen
  retains exactly one final result for each task.

After integration, `cargo check --offline --locked -p shenko-store --tests -j1`
found a fixture ownership mismatch (`Store` instead of `Arc<Store>`). Commit
`f2272b4` corrects that return value; the complete Store test-target metadata
check then passed in 1.86 seconds. This type check did not execute either
journey.

Candidate `6782627ea89e17774bc65ba54d459ad85804fef1` subsequently executed both
journeys successfully in [CI job 133289317901](https://github.com/cyjin-yl/shenko/actions/runs/43864453626/job/133289317901),
after the workspace all-target check passed. The original test excerpt and
artifact digest are retained in the [candidate observation](../evidence/ssh-wasi-ci-6782627-20261116/observation.json).
The complete job still failed on the separate broker deadline report assertion;
passing these two journeys does not make that job green.

In particular, the foreign-ledger journey checks honest predecessor refusal; it
does not demonstrate a foreign native execution takeover. The read sets remain
conservative full-table snapshots and can conflict on unrelated factual
progress. Canonical claim/T1 integration, authoritative predecessor handling,
real Node/provider journeys and physical fault acceptance remain separate work.

The two journeys above are
`canonical_native_t2_keeps_original_e1_through_settlement_restart_and_foreign_debt`
and
`canonical_native_t2_completing_and_finish_recover_as_separate_atomic_results`.
They did not exist at the candidate that has runtime evidence, so this
increment is source-level and evidence-level only: no runtime result is
claimed for the paired native T2 boundaries, and the Nightly scope recorded
in the
[component continuation evidence](../evidence/component-coordination-20261116.md)
covers the S1 and component journeys, not these two.
