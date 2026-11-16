# Direct native continuation across distinct ledgers

This increment connects one bounded native lifecycle after the
[first canonical claim](canonical-native-first-claim.md). It does not make an
archive import executable or provide a general foreign recovery claim.

## Exact scope

The task must be the original, directly root-owned native delegation born in
a canonical S1 which this installation actually materialized. The first
claim's exact Running task is the paired `arm_native_operation_for_dispatch`
predecessor. The operation is that claim's original native operation. Its
first definite E1 must retain the original armed operation and attempt, have
one receipt, and have no reconciliation case. T2 must be the original
`native.complete`, with no ordinary child rows at all. Even a completed
ordinary child keeps the action outside this deliberately narrow range.

Separate arm followed by `native.dispatch`, additional operation steps,
model/quota and publication work, child-tool creation, recovery/settlement
invocations, multiple observations, Unknown and contradictions, Completing
and `native.finish` keep their existing protocols. Those paths do not gain
cross-ledger completion from this increment.

The supported source may use its existing valid local availability authority
or a quorum grant. This does not turn grant-free work into quorum evidence.
Foreign learned control materialization still requires the verified chosen
grant. The new journey uses grants; it adds no grant-free execution evidence.

## Why the previous parts did not form this chain

First claim could install the original Running target while preserving a
parked subject root. The later T1/T2 readers still compared the complete task
table, so that retained root prevented application. Ordinary cognition is
also intentionally conservative: it parks a nonterminal imported target and
applies task, operation, attempt and E1 as one conflict unit. A later E1
archive cannot replace the live canonical target merely to append its receipt.

Calling `record_external_evidence` again is not exact materialization. That
API admits a local observation and assigns a new `received_commit`,
`evidence_through` and `last_observed_at`. The new path retains the source
values instead of calling that API at the receiver.

## Source reads and local proof

The new paired T1 and T2 use transition codec major 2. Major 1 retains its
complete task read range. A major 2 transition cannot omit its typed proof;
an older reader rejects the new version rather than interpreting it as an
ordinary delta.

The reader independently verifies the original claim, its authored S1 birth,
the exact source root and creator from the admitted predecessor snapshot,
and this installation's own nonreplicated application receipts. Each local
receipt must match its shared applied event and a real historical physical
`commit_log` record. A copied source applied event is insufficient. A
same-installation successor can retain the historical physical receipt;
that does not give the old worker another permit.

Only this proven task read range becomes a single target key. The source
root remains a complete expected read and Node revision fence. All other
captured table ranges remain complete. T2 also checks ordinary-child absence
in the immutable source snapshot and again in the receiver's final Immediate
transaction. It cannot finalize over newly present local ordinary work.

The original fresh T1 rule is unchanged: only the current source call which
inserted the preparation, obtained a new admission and passed the final live
invocation/dispatcher checks can return `None`. Prepared or learned T1 only
materializes facts and returns the existing operation. There is no provider
call in preparation, materialization or recovery.

## E1 remains an external observation

In the original E1 transaction, the source additionally seals a typed record
of the exact original T1, receipt, resolved operation, ended attempt and SQL
receipt timestamp. Its local `native_evidence_materializations` row commits
with the E1 rows. The seal does not acquire commit authority and does not
reactivate a callback after cancellation, epoch change or worker loss.

The derived seal is not allowed to suppress an otherwise admissible E1.
When a non-storage proof/closure failure prevents sealing, the original E1
still commits with an explicit `native_evidence_seal_debts` record. The
read-only `native_evidence_seal_debts(operation_id)` API exposes that record.
T2 reports reconciliation required; it cannot recreate a proof, silently use
the old generic path or claim that the external effect failed. Actual SQLite
or CAS sealing storage failures still close the writer and roll back the
transaction under the existing storage-settlement rule.

A later chosen T2 transports the E1 seal through its ordinary CAS closure.
This choice orders the task completion; it does not prove that a quorum ran
the provider or made the external observation. The receiver must already
have its own verified application of the original paired T1. It checks that
the local operation and attempt still equal the original armed predecessor,
that no other receipt/source-event identity or reconciliation case conflicts,
then writes the exact E1 and its projections inside the T2 transaction.

`receipt_id`, operation/attempt/invocation identities, `received_commit`,
`prepared_commit`, `armed_commit`, `evidence_through`, `last_observed_at` and
the source SQL receipt timestamp remain unchanged. A separate local physical
receipt records where this installation materialized them. The task/result/
inbox retain the original T2 CommitRef. If any T2 read or result insertion
conflicts, all staged E1 writes and the local E1 receipt roll back with it;
the original closure remains available through reconciliation debt.

## Verification boundary

One Store journey,
`canonical_direct_native_chain_retains_original_e1_and_only_one_effect`, reuses
the existing canonical S1/claim and native authority fixtures. It includes
nine necessary windows in this single logical increment:

1. Fresh source paired T1 followed by one real local file effect.
2. Distinct-ledger learned T1, preserving the parked root and refusing a new permit.
3. Lost chosen T2 reply, with the source's original E1 already durable.
4. Unsupported E1 reader rejected without writes or generic fallback.
5. A real local inbox identity conflict rolling back staged E1 and T2 atomically.
6. Ordinary archive target parking retained with reconciliation debt.
7. Source reopen materializing the prepared T2 without another effect.
8. Receiver reopen and duplicate recovery preserving one E1/result/application.
9. Unavailable original T1 proof preserving the late E1 and a seal debt, while T2 refuses.

The added journey has not yet executed on this candidate. Local checks are
metadata only. This fixture observes a bounded file effect; it does not
validate a real network provider, multi-node process failover or the complete
native execution surface. E1 before a T2 choice still relies on its source
installation's durable index and CAS. General factual E1 replication,
multi-receipt reconciliation and foreign recovery invocations remain separate
work, and no historical receipt is relabeled to imply that they passed.
