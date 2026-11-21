# Original 0c attempt 001: fresh-preparation drift diagnosis

This directory preserves read-only inspection of the already closed first
real-browser attempt on source
`0c6b013e7c0cc8c27e30a83189c14a36c71aaa74`. It records the original failure
and the limits of action attribution. It is not a successful retry or a test
receipt for a later source fix.

The [machine-readable summary](summary.json) contains the original runtime
event, final subject/task/operation facts, all 12 Node admission identities,
their matching Store materializations, and explicit custody limitations.
[manifest.json](manifest.json) binds the stored and decompressed bytes of
the three original audit files. The full final ledger remains in the
[original batch evidence](https://github.com/cyjin-yl/shenko/blob/2147750c7e62062ccf5d8bacb7a832c9cf5f278b/docs/evidence/daily-readiness-20261122/final-0c6b013e/attempt-001/final.ledger.json.gz).

## Observed failure

At `2026-11-21T22:12:03.432Z`, session event
`87ec2824-1c36-46b5-9004-61c9849bdcf7` recorded:

> committed dispatcher pass failed: conflict: ordered action predecessor: subject predecessor changed before snapshot capture

The original error digest is
`blake3:d30c87065fcec62e554db3de4b9d3ab3fa99d8940c8fad6c585aa48cdf38f1dd`.
The event was not truncated. Its `task_id` and `step_no` are null, `extra` is
empty, and it contains no action ID or call stack. Those missing bindings
are not reconstructed as observed facts.

After exit, the subject head had revision 5 and committed step 2. Two S1
steps were committed; step 3 remained Preparing. The original printf task
`e5f834ae-52a1-40b9-b721-4d1fc35e4151` was Pending at revision 3 after its
reconciling park. Its original operation was Resolved/Succeeded with the
original E1 at ledger sequence 98. There was no task result. The finite model
child retained its original operation/attempt, with a later Unknown
observation at sequence 155.

These post-exit facts coexist with the separately diagnosed E1 closure-capacity
debt. They do not establish that the capacity debt alone caused the subject
predecessor conflict or identify the exact instant when the head changed.

## Retained choices and the unchosen candidate

The Node log contained 12 admissions at indices 3–14. All 12 had corresponding
Store physical materialization receipts, and five canonical prepared images
were retained. The final four relevant entries were:

| Node index | Original action | Store materialization sequence |
| --- | --- | --- |
| 11 | printf `native.arm` | 92 |
| 12 | printf `native.park`, original revision 2 | 143 |
| 13 | model-child `native.dispatch` | 144 |
| 14 | second `subject.step_commit` | 146 |

The retained envelopes for indices 6–14 bind head revision 4 / committed
step 1. The final exported head binds revision 5 / committed step 2.
The possible next settlement-only claim key,
`task/e5f834ae-52a1-40b9-b721-4d1fc35e4151/claim/3`, has no matching Node
admission, prepared image or Store materialization.

The expired reconciling park therefore makes that fresh settlement claim a
plausible action at this boundary. This remains an inference. The original
event cannot uniquely bind the failing preparation to this task. The audit
also does not say that no earlier action had been chosen: all 12 retained
choices remain original facts and must not be rewritten or replayed.

## Read-only custody and cryptographic limits

The queries opened the closed databases in SQLite `mode=ro`. During the
captured inspections, the main database, its WAL, and the Node database
retained their byte lengths, SHA-256 hashes and modification times. The first
audit's `stable` field is deliberately **false**: `shenko.db-shm` retained
its bytes and SHA-256 but its modification time changed from
`1795299681143011718` to `1795299990631581847`. No blanket claim that every
file's metadata stayed unchanged is made. These before-hashes do not cover
an earlier unrecorded schema inspection.

The 12 admission payloads were read from digest-derived CAS paths, with raw
SHA-256, length and before/after metadata retained. The diagnostic Python
environment did not have the `blake3` module; `blake3_verified` remains null.
Their original BLAKE3 addresses are preserved, but this audit does not claim
independent BLAKE3 revalidation. The final exported ledger's original raw
identity is 461,036 bytes, SHA-256
`ee1ff530da6575d6d0714f3e909ba1d05bbf03a1464e8fd9b135ac3dd3bd5d6b`.

## Followup boundary

The later source changes `dd0fe187` and `1ef91f83` introduce a typed
fresh-preparation drift result and protect an already claimed task that has
no worker by requiring its original revision to be parked successfully
before deferral. Their handling is confined to fresh preparation; historical
choices, permits, operation identities and E1 facts remain binding.

This diagnosis runs no model, Cargo test, repair or replay and does not prove
those changes correct. No new experiment forcibly triggered the partial-claim
window. The original failed attempt remains part of the 0/3 batch result;
later source validation must carry its own source identity and actual receipt.
