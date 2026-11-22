# Independent original native E1 delivery

This document describes the original first-observation increment. The next
bounded candidate adds [one causal Unknown-to-definite successor](canonical-native-e1-successor.md),
with an explicit new wire scope and no expansion of T2's existing proof.
The current candidate writes [codec-6 anchored E1 envelopes](native-e1-incremental-closure.md)
around the original fact shapes. That separately versioned change replaces
whole-graph transport accounting for new facts; the historical codecs and
their original capacity checks below remain unchanged.

This increment adds an authenticated factual path for the first observation
of the original paired native T1. A receiver can retain that E1 before any
T2 is chosen. It does not choose a task transition or issue a new worker or
send permit.

## Original facts and the local prerequisite

The operation must be the original direct native claim's operation, with an
actual canonical S1, first claim and paired T1 application in this
installation. The existing typed readers independently verify that chain.
Copied peer `applied` events and ordinary archive rows cannot establish its
local physical receipts. Archive parking stays intact.

The original E1 transaction now writes `store-native-evidence-fact` codec 2.
It freezes the actual decoder inputs: optional expected revision, outcome,
effect and terminal-content-conflict flag. The reader reconstructs the first
fold from the exact original InFlight/Armed operation and attempt. It accepts
a definite observation or a first uncertain observation. Codec 1 remains a
reader for its previously supported definite fold; version and fold presence
must agree. A broken seal, unsupported codec or incomplete typed closure
does not fall back to generic admission or successful task completion.

`receipt_id`, operation/attempt/invocation identities, `received_commit`,
`prepared_commit`, `armed_commit`, `evidence_through`, `last_observed_at`,
observed time and the source SQL receipt timestamp are retained. No receiver
calls `record_external_evidence` to manufacture new provenance for these
bytes. The source's observation is still an external fact. Transporting it
through Node CAS does not make it a quorum execution receipt.

As before, inability to derive the additional source proof does not erase an
otherwise admissible E1. Non-storage proof failures retain the original E1
and its seal debt in the same transaction. Actual storage failures keep the
existing writer-fencing behavior.

## Authenticated bounded manifest and object transfer

`NativeEvidencePage` is served by the actual Store adapter behind the existing
SSH `node` entry point. Its request names the receiving self's subject and
origin. The response identifies the actual serving Node, current ledger,
subject, branch, origin and the explicit `canonical_native_first_observation.v1`
scope. It contains at most 16 items and stays within the 512 KiB manifest
budget. A caller may request a smaller positive item budget.

The first page fixes an inclusive physical upper watermark. Subsequent pages
echo that watermark and the exact `(sequence, receipt_id)` cursor. This
position enumerates the serving installation; it does not replace the
receipt's original ledger-scoped CommitRef. The source only advertises an
observation backed by its real source commit and canonical T1 application.
Both the wire reader and Store intake require that original commit's ledger
to equal the manifest's source ledger; this scope is not a transitive relay.
A receiver materialization cannot advertise itself as a new source E1.
An eligible first observation with no local source materialization row appears
as an explicit gap. An existing row whose typed seal or required closure is
missing, malformed or unsupported instead fails enumeration explicitly.
Completion of a page scan is not proof that those gaps applied,
nor a claim to enumerate observations outside this bounded scope.

The client uses a registered SSH route, checks its actual Node ID and verified
same-self origin, and rejects cursor jumps, identity changes and oversized
responses. A Node without a self, a different self, or a Node without a local
subject returns an explicit authenticated `not_eligible` observation. It
does not invent an empty successful manifest. This preserves the existing
first-installation and cross-self cognition paths.

Only roots selected from actual source E1 materializations are published to
Node CAS. The existing 64 KiB chunk transport carries their immutable closure,
with a 4,096-object / 64 MiB inventory budget. Locators are not read authority;
bytes are resolved by digest in the relevant CAS. This adds no credential
discovery, arbitrary-path reader, peer-claimed local receipt or control-log
entry. Node retains no new dispatch permission from these objects.

## Durable intake, Unknown and replay

Before downloading objects, the receiver records the authenticated manifest
in its local `native_evidence_deliveries` table. A disconnect leaves a pending
delivery. A source gap or missing/incompatible local T1 leaves explicit debt.
Malformed objects remain errors with the pending delivery retained. Pending
and Blocked deliveries can retry the same original fact, never a new external
operation. Gap is a retained source observation, with no source-seal repair or
automatic closure in this increment. Availability is part of delivery identity:
a future Sealed descriptor would be a new delivery and would not silently
erase or resolve the earlier Gap. A later reconciliation protocol is required
to close that historical debt explicitly.

In one Immediate transaction the receiver rechecks its own canonical chain,
requires the exact original Armed operation/attempt and an unoccupied receipt
identity, writes the original E1/fold, and records a separate local physical
materialization receipt. Any failed application rolls back before the local
debt update. Conflicting original receipts use the existing distinct-conflict
journal as well as the retained delivery; the source seal remains unchanged.
There are no task or result writes in this intake.

An uncertain fold creates a local reconciliation case under the real local
owner task. Its case identity and CommitRefs are local. A source's
`acknowledged_unknown`, decision reference or acknowledgement authority is
never transported. Replaying an already applied first E1 validates its actual
local receipt but leaves later local observations and case decisions intact.
The chosen T2 path keeps its stricter exact-current-projection check when it
needs to complete the task.

`shenko sync evidence --peer <node-id>` invokes this path directly. The
selected runtime's existing replication tick and `sync push` also invoke it,
independently of ordinary archive/package transfer. An unresolved same-self
E1 debt makes the combined push unconfirmed while retaining its separate
`cognition_outcome`. `sync status` lists up to 64 local delivery debts and the
total, so a bounded view cannot imply that the remaining debts vanished.
Resident followers still have their existing control-catchup loop; this
increment does not add a new follower background worker.

## CAS retention boundary

The source materialization and receiver delivery/application records name the
original seal and its closure. They are not yet enumerated by a complete Store
GC producer. The current runtime has no `CasGc` adoption or reclamation entry
point. Its existing standalone GC requires an administrative declaration of
every producer and namespace sharing that CAS. A complete declaration including
`commit-authority` cannot currently reclaim: the available home/COW, WASM-state
and tool-artifact producers do not cover that Store namespace, so adoption or
planning refuses with `CAS_ROOT_NAMESPACE_UNCOVERED`.

That refusal is conditional on a complete declaration. GC does not discover
omitted namespaces from object bytes; declaring only tool artifacts for a CAS
that also contains Store facts violates that precondition and can lose those
unlisted facts. This increment installs no such declaration and adds no GC
consumer or permanent pin. A Store-aware root producer, complete shared-CAS
assembly and a root-revision/exclusion protocol remain necessary before safe
Store reclamation can be enabled. The E1 journey does not claim GC acceptance.

## Verification and limits

One new real OpenSSH/CLI journey reuses the existing canonical Store fixtures.
It has nine necessary observation/failure windows: the first definite and
Unknown observations, an interrupted authenticated chunk exchange with restart,
missing local T1 followed by factual retry, an ordinary parked archive,
bounded pagination, unsupported codec refusal, replay after a later local
observation, and loss of the derived source seal with the original E1 retained.
The fixture's two actual file effects must remain two. Its source Unknown
acknowledgement must not become the receiver's case decision. No T2 is chosen.
The control grants in this journey remain retained-value Store fixtures;
only its SSH/CLI/CAS delivery is an actual authenticated process route.
The chunk fault exits the remote launcher after SSH authentication but before
it execs the CLI for one `read_control_object` request. It does not prove that
an already-produced chunk reply was physically lost.

The existing Store journeys retain their assertions. Shared helper extraction
does not create a second test suite. The new journey is authored and awaits
remote execution; local work is restricted to metadata and document checks.
Original process outputs and finite process cleanup are retained by the CI
artifact path, including failures.

The increment does not define multi-operation continuation, multiple-E1 source
ordering or consensus of decoder judgments. It does not extend ordinary-child
completion, recovery/settlement claims, model/quota or publication work. A
late local observation may advance an already applied operation under the
existing E1 API; that does not mean this first-observation protocol can relay
the later sequence. General factual E1 propagation and those lifecycle
boundaries remain separate work.
