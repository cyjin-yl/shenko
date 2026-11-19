# One causal native E1 resolution

This candidate extends [independent first E1 delivery](canonical-native-e1-delivery.md) by one bounded transition: the same original attempt's first Unknown observation can be followed by a definite observation from an installation that actually materialized that first E1. It preserves both observations and carries no task transition, worker claim, or send permission.

The first-E1 implementation at `7905e8413b96015d96ea301bd1f3b6937dcfff90` has an actual [nine-window SSH CI pass](https://github.com/cyjin-yl/shenko/actions/runs/43928793872). This successor candidate has no new Rust compilation or runtime pass yet. The restored local environment has no Rust toolchain. Its authored continuation journey must run on the final integrated candidate.

## Exact causal range

`store-native-evidence-fact` codec 3 names an explicit `predecessor_ref` to codec 2. The first seal is independently read and checked against the installation's genuine canonical S1, first native claim and paired T1 receipts. It must describe revision 1, `Reconciling`, no outcome, unknown effect, no content conflict, and the original attempt at `Unknown`. First uncertain folds with an already recorded outcome or terminal content conflict are outside this increment.

| Fact | Required predecessor | New projection |
| --- | --- | --- |
| First E1, codec 1/2 | Exact original Armed operation/attempt | Existing first fold, unchanged |
| Definite successor, codec 3 | Exact codec-2 first Unknown, one operation/attempt, no ordinary child | Revision 2, Resolved, a definite outcome and effect, original attempt Ended |
| Any further observation | Existing general local E1 rules | Retained locally; no new canonical successor fold is inferred |

The successor freezes the actual decoder inputs, original receipt ID, operation/attempt/invocation identities, source `received_commit`, source receipt SQL timestamp, `evidence_through`, and `last_observed_at`. It preserves the original arm and the first receipt. An optional decoder expected revision must be 1. The typed reader independently reconstructs the permitted fold; it does not accept arbitrary operation or attempt replacement.

Only sequences within the same ledger are compared. Across ledgers, the sealed predecessor establishes causality; wall-clock order or unrelated local sequence numbers do not. A source that first received A's Unknown can create its own later E1 under B's original CommitRef, while the first observation continues to name A. B's local receipt for A's observation never becomes a new B source observation.

## Local application and acknowledgement

Both the producing and receiving installation require the first E1's real local materialization row, its original receipt/timestamp, and the corresponding local commit-log entry. A copied marker, ordinary archive or downloaded CAS closure is insufficient. Missing local first application becomes explicit predecessor debt; the receiver does not silently apply that predecessor from the successor closure.

Fresh intake requires the current operation/attempt to match the sealed first Unknown exactly, one existing receipt for that operation, one original operation/attempt, no ordinary child, and an existing local Unknown case. A different local observation remains authoritative for that installation's current projection and causes a retained delivery conflict. The incoming source seal is still retained; it does not overwrite that local fact.

One Immediate transaction adds the original successor receipt, its exact projection, a separate receiver physical receipt, local case resolution, and the delivery update. The source's case and decision are absent from the transported fact. If the local case is undecided, the existing resolver marks it resolved. If it is already `acknowledged_unknown`, the original row and decision remain byte-for-byte, and a separate local resolved case records the later answer. These local case identities and CommitRefs are never copied from the source.

Replaying an actually applied successor verifies its retained original bytes and local receipts, then leaves later observations and decisions untouched. It cannot restore revision 2 over a newer local revision or issue a second execution permission.

The original E1 callback still admits otherwise valid late observations. Missing or incompatible derived successor proof becomes seal debt in that same E1 transaction; it does not roll back the original observation. Actual storage failures keep the existing writer-fencing behavior.

## Explicit transport scope

Requests, cursors and responses name `canonical_native_first_and_unknown_resolution.v2`. Responses use schema 2, including authenticated `not_eligible` replies. An old first-only request, cursor or response is rejected explicitly, so a first-only scan cannot masquerade as a complete successor scan. Existing delivery records and original codec-1/2 facts remain readable.

The source enumerates original observations of retained canonical paired T1 operations using the existing fixed physical upper watermark. The original receipt ledger must equal the serving manifest's ledger. A supported first/successor seal is advertised. If an original observation has no local source materialization row, it produces an explicit gap, including observations for which this bounded writer produced no supported seal. If a materialization row exists but its typed seal or required CAS closure is malformed, missing or unsupported, enumeration fails explicitly; it does not downgrade that broken proof to a gap or a complete scan. This does not grant general multiple-observation fold support. A received observation remains excluded from enumeration as this installation's source fact.

The existing registered, same-self OpenSSH route and bounded Node CAS transport remain the only new public delivery path. Pending delivery is recorded before download. Missing chunks, blocked predecessors and malformed seals remain visible. Malformed typed bytes do not fall back to generic admission. Gap history still has no automatic repair/closure protocol, and a new sealed descriptor cannot erase an old gap.

## T2 and other limits

The chosen T2 path still uses its original codec-1/2 E1 reader. Codec 3 does not extend its narrow direct-completion proof, ordinary-child absence, current head, reconciliation or ownership checks. Existing local T2 behavior, including its full-readset path when no narrow E1 proof is available, remains unchanged. This increment does not claim foreign E1-to-T2 completion after Unknown resolution.

No multi-operation, additional attempt, ordinary-child, repeated Unknown, general multi-E1 order, publication, model/quota, recovery claim or foreign worker takeover is enabled. A further local E1 is still a factual observation, not evidence that this bounded protocol can relay its entire sequence.

The [Store-aware GC limitation](canonical-native-e1-delivery.md#cas-retention-boundary) remains: no runtime GC assembly or complete Store producer/exclusion protocol is added. Refusal requires a complete namespace declaration; omitted namespaces do not acquire automatic protection. There is no permanent pin mechanism and no GC acceptance claim.

## Seven new observation/failure windows

The existing real OpenSSH/CLI journey retains its original nine-window assertions. Its successor extension adds seven distinct windows, not seven suites:

1. B advertises its actual definite successor with B's original source commit and A's exact first-E1 causal reference; it does not relay A's first observation as its own.
2. The authenticated route rejects the old first-only scope explicitly.
3. An installation with genuine T1 but no local first E1 retains the successor as blocked; receipt of the first E1 and a new CLI retry then applies the original successor.
4. An independently recorded local definite observation prevents an incoming competing successor from replacing it.
5. An interrupted authenticated reverse chunk exchange leaves durable pending state and the original source acknowledgement intact; a new CLI process applies the original successor and appends the local resolution atomically.
6. A codec-3 seal with a different definite first observation as predecessor is rejected as malformed typed input, retaining the pending descriptor and all accepted facts.
7. Replaying the successor after another local observation preserves that newer projection, the original acknowledgement, and the independently retained malformed-input debt.

Two additional Stores obtain their canonical predecessors through the real Store recovery APIs. Their control grants remain the existing retained-value fixtures; this is not a new quorum-election claim. Transfers use the actual registered SSH/CLI/CAS path. The malformed input is deliberately authored negative input and is not represented as a genuine source observation.

The two physical file effects must remain exactly two. All original first receipts, source acknowledgement bytes, separate logical/physical receipt identities and original source times are asserted. Duplicate paired T1 and dispatch calls still return existing facts/AlreadyCommitted; no task result or T2 is created. The evidence hook exports selected original rows and typed CAS bytes for all five Stores before cleanup, excluding Node/SSH credentials and reporting absent or omitted material. This is a read-only evidence export, not a full SQLite backup.
