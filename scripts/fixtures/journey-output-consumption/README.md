# Original sealed inputs for the output-consumption regression

These are observations from the completed `final-d16adbf8` batch, using binary
source `d16adbf89c9f79dfce8ef90d77843412bcda85bf`. Both original full journey
verdicts remain failed. The regression does not run a model or reclassify them.

`cases.json` preserves the original selected ledger tables and source hashes.
Each `attempt-*.input-plan.json.gz` contains exact original CAS bytes with only
mtime-zero gzip encoding; its decoded length, SHA256 and full original typed
reference are recorded in `cases.json`. The original full ledger snapshots
are retained in `docs/evidence/daily-readiness-20261122/final-d16adbf8/`.

| Original | T2 commits | Selected request | S1 | Observation |
|---|---|---|---|---|
| 002 | printf 210, model child 218 | `a640001a-9287-4ce5-8d4f-9ab507fcf9bd`, created 200, history leaf 5 | 335 | The request had neither result. Its answer explicitly continued waiting and mentioned `437` only to reject substituting its own calculation. The old commit-order/text gate accepted it. |
| 001 | printf 89, model child 95 | `23ee7714-19d6-498b-b3f1-69e7076ba9d0`, created 237, history leaf 9 | 446 | Actual payload messages 6 and 7 contained both full original results. The submitted answer accurately reported them. The later browser locator failure still prevented the full journey. |

The single regression first reads both exact originals through the observer's
real CAS reader. It then perturbs only a decoded copy of 001's input to check
that one missing payload result is refused, a history row beyond that seal's
leaf cannot supply provenance, a result inside that leaf need not be a new
event, and the selected answer must also consume the supplied accepted user
message. Those perturbations exercise the observer; they are not newly sealed
Store facts, provider responses or additional real-model outcomes. No prompt,
budget, timeout, output wording or product state is changed by this fixture.

The same test also derives an observer-only retry identity projection from
001: a new active request/attempt retains the original sealed plan, payload
and provider digests, and a later creation commit. The original attempt must
still exist; removing it is refused. This models the identity relationship
used by `RetryPending`, without asserting that the original successful run
actually retried or creating another real provider execution. Its original
creation boundary, rather than the later retry, limits result availability.

The [source-bound comparison and test receipt](../../../docs/evidence/daily-readiness-20261122/observer-result-consumption/receipt.json)
retains the old observer accepting 002 and the fixed observer rejecting it,
while both accept 001's genuine input consumption. Its sibling `unittest.log.gz`
is the actual isolated Python regression output (four tests, including the
three existing observer tests).
