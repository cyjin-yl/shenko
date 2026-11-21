# Native E1 over an already applied predecessor

The `0c6b013e` real browser batch completed three attempts and passed none.
Every stopped installation retained one original successful `host.printf`
observation and one seal debt whose exact reason was
`invalid direct native fact: CAS closure byte budget exceeded`. None had a
native task result. Each original paired T1 referenced the actual 90,162,848
byte native executable, with SHA256
`8765fb320736ec0d1cd0c3e454fc3a3855d0959442230d1490344ca5dae12ff1`.
That one required object exceeds the 67,108,864-byte E1 whole-closure limit.
The independently retained post-exit database/CAS diagnosis has SHA256
`baa4c8cd555db519749849f85e2ca47d6be13aefe94d31ab36ce14d4a7d3cdfd`.
It identifies actual retained facts after exit, not a database snapshot at
the precise failure instant. The batch's separate pre-choice subject drift
and stopped model-child observations are not attributed to this cap alone.

This change introduces an explicit E1 format. It does not change that failed
batch, replay its effects, convert its debts, or raise the existing limits.

## Format and predecessor authority

Every newly supported original E1 uses `store-native-evidence-fact` codec 6,
in the existing `commit-authority` namespace with JSON media type. Its body is
`AnchoredNativeEvidence`, with schema 6, a complete `fact_ref`, and one to three
`predecessor_roots`. The inner immutable fact retains the original codec-2/3/4/5
shape: original first observation, one Unknown-to-definite successor, and
their source-bound forms. Receipt IDs, source CommitRefs, observed times,
fold inputs, operation/attempt IDs and local materialization identities do
not change. A successor remains ineligible for the narrow original-first-E1
T2 proof.

Store independently derives the exact predecessor list from the inner fact:

| Predecessor | Required original reference | Required local fact |
| --- | --- | --- |
| Original paired T1 | `fact.arm.admission.payload_ref` | The original canonical claim/T1 and physical application receipt, verified by the existing typed readers |
| Source E1, when present | `fact.source.evidence_ref` | That exact E1's local materialization, original receipt/time and commit; current or historical source rules still apply at the original use site |
| First Unknown, for a successor | `fact.predecessor_ref` | The original first-Unknown local receipt and the unchanged causal/fold checks |

The envelope orders and deduplicates these references by digest, byte length,
media type, namespace, and complete codec identity. Only the mutable locator
is excluded from that identity. A different root, omitted root, duplicate,
unsupported type, self-reference or noncanonical list is rejected. An envelope
does not authorize an arbitrary sender-selected graph boundary.

Standalone codecs 1–5 retain their old readers and original whole-closure
budget semantics. They cannot directly acquire a codec-6 predecessor by
being read as the new envelope. A codec-6 inner fact may name another new
source or first-Unknown envelope; the full reference remains in the bytes.
Existing debts continue to block completion even if the missing object later
returns. The writer selects codec 6 deterministically before closure checking;
there is no string-matched capacity fallback.

## Complete verification, bounded increment

Before excluding any predecessor from E1 accounting, Store verifies its
complete typed closure, including length and digest of opaque bulk objects.
Opaque bodies use the streaming CAS verifier. The temporary inventory records
each exact typed identity. A later JSON or required-schema alias of the same
digest is checked and expanded with its own authority; a previously seen
opaque digest cannot hide those children.

The increment traversal starts at the envelope. It stops only at an exact
typed identity present in the verified predecessor inventory. The existing
limits then apply to 4,096 new unique typed declarations and 64 MiB of new
unique object bytes, including the envelope and inner fact. Repeated pending
references and predecessor references do not consume that new-object count.
Any unknown alias remains in the measured traversal. Export selects actual
encountered increment references and does not substitute a predecessor alias
for their transport descriptors.

These are E1 transport limits, not a claim that predecessor validation costs
are constant. The complete predecessor graph is still read and verified, and
the initial separate Node cache fill can copy large objects locally. This
change removes repeated bulk from the bounded E1 delivery payload; it does not
prove a startup, snapshot, lock-wait or model-response latency improvement.

## Node cache and Store intake

Node may already retain a closed graph from a prepared/accepted control value.
That cache is not proof of a Store application. Every declared predecessor
must therefore also pass the local Store application validator. A missing
Node cache may be populated only from this same Store's fully verified,
already applied predecessor. It is not downloaded from the E1 peer. A marker
whose Node bytes are missing or corrupt is refused rather than repaired by
the new E1 exchange.

The only object fetched before checking the declared predecessors is the
bounded envelope, which remains inert. Node does not derive the original
causal list from the inner fact; Store performs that independent comparison
after those inner bytes are available. A malformed envelope which omits its
real predecessor can therefore cause bounded inert downloads before Store
rejects it, but cannot create an application or a send permission.
Missing declared T1, source E1 or first Unknown can therefore
stop the transport before Store intake; the previously queued delivery stays
pending and the CLI returns the actual error without an application report.
This differs from a transfer that reaches Store and records a blocked factual
application. Neither state counts as an applied observation.

For direct Store intake, the envelope and inner fact are parsed without CAS
publication. In the same IMMEDIATE transaction used for application, Store
checks the actual local predecessors, verifies the complete existing base,
and only then accepts increment bytes. The complete original fact/fold and
source rules are checked again before any original E1 projection or local
receipt is written. Source/first-Unknown absence cannot be repaired by
including those missing objects in the incoming increment. Failed factual
application preserves the existing pending/blocked and conflict-journal rules.
No task, worker or send permission is created by E1 intake.

The independent E1 channel is the scope of this no-download boundary. Existing
full control-admission and archive delivery still transfer their complete
objects under their own admission rules. They do not manufacture the actual
local T1/E1 receipts needed by this reader.

## Retention and compatibility

The envelope has ordinary complete ObjectRefs to both inner fact and all
predecessor roots. Generic archive and GC traversal retains the whole graph,
including executable and history bytes. No graph edge, recovery object or
old source seal is replaced by an external availability promise. GC remains
unactivated; this change supplies no new reclamation permission.

The integrated writer advances cognition archive capability to schema 12 so
an older reader rejects before payload publication. Schemas 10 and 11 remain
explicitly supported with their original family/layout rules; unsupported
8/9 stay unsupported. Raw Node object transport is a separate boundary, and
an archive manifest alone does not prove that an older live runtime can use
new E1 facts.

## Existing trajectories and evidence limits

No new test function or independent retry matrix is added. The existing
129 MiB direct native chain carries a real 64 MiB-plus-one-byte implementation
in its original T1. Its one-effect, lost chosen T2 reply, receiver application,
reopen and replay assertions remain. Its existing proof-loss window observes
missing bulk, then same-length corrupt bulk during the actual E1 seal, and
checks that restoring the bytes does not erase the resulting seal debt.

The two existing E1 delivery tests retain first/Unknown, exact source custody,
source-head advancement, competing observation, malformed predecessor,
interrupted SSH transfer and no-reexecution assertions. New-format assertions
inspect both the codec-6 envelope and original inner fact. A source-dependent
increment excludes the actual source graph but still requires its original
local application. Missing predecessors are checked at their new, earlier
transport boundary rather than misreported as completed intake.

Source/grammar review is not a Rust or real-model success receipt. Execution
results must bind the final integrated source, binary, environment and original
logs. The original three `0c6b013e` failures remain in the denominator.
