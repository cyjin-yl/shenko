# CAS root declaration consistency

This increment tightens the existing callable GC API. It does not install a
runtime GC scheduler or enable reclamation of a Store installation. The object
and metadata ordering, retained recovery closures, and root snapshot/exclusion
obligations remain in [the durable-state contract](../specs/durable-state.md#spec-41-2-2).

## What is checked

`CasGc::plan` uses three declarations about the same run:

1. The adopted `RootRegistry` names every participating producer and every
   namespace this installation may seal into. Its persisted lists must be
   nonempty, sorted, and unique, with nonempty names.
2. The actual supplied producer IDs must match that registry. Their namespace
   claims must have no overlap or duplicates and cover exactly the adopted
   namespace set. Extra producers are refused before any enumeration; they
   cannot be silently omitted from the plan.
3. Each `ProducerRoots.namespaces` report must equal that producer's namespace
   claims captured for this plan, without duplicate names. This includes the
   report read from a `RecordedRoots` durable record. Namespace ordering in a
   report is not significant.

Before candidates are selected, the plan also reads the CAS's live reader pins
(`pinned_objects`) and treats their verified closure as roots. A pin is an
exclusion, not a producer declaration, so it is not part of any namespace
claim; a pin whose object is missing makes the plan refuse.

These are declaration checks. A namespace claim describes where the producer
seals objects; it does not restrict which digests its roots may reference. A
closure can retain objects from another producer's namespace, and a registered
producer with no seal namespaces can still retain such roots. The collector
does not infer namespace ownership from digest-addressed object bytes.

The registry and producer-record formats stay unchanged. The registry does not
persist a historical per-namespace owner map; this check compares the declared
sets and the participating producers' reports for the current plan. It does
not prove that a caller has named every writer sharing the CAS.

Existing missing-registry, uncovered-namespace, namespace-overlap, incomplete
producer, blocker, missing-object, staging, and adoption-time refusals remain.
A malformed or inconsistent declaration is refused before selecting deletion
candidates. The root records and registry are not rewritten to conceal an
inconsistency.

## Verification scope

The existing `tool_artifact_roots` journey still installs a real multi-chunk
WASI module, retains its manifest and chunk bytes, collects a real unrelated
orphan, and verifies the reconstructed module. Its original assertions remain.
The same objects are used for five new refusal windows:

1. A duplicate producer ID in the persisted registry.
2. An extra supplied producer with no namespace overlap.
3. A registered producer claiming an undeclared namespace.
4. A complete durable root record reporting a different namespace.
5. A complete durable root record reporting duplicate namespace names.

Each refusal checks the typed reason and that the full CAS object inventory
is unchanged, including the otherwise collectable orphan. A sixth new window
corrects only the durable declaration and verifies that its unchanged real
roots again produce a valid plan. The original live-producer reclamation and
byte checks then run unchanged. These are six added risk/recovery windows in
one existing journey, not six successful executions in the current environment.

The source was authored from `83bf391440adbe3e153b036ba8fd1ecfe5867202`.
Local validation is limited to static review and `git diff --check`; Rust,
Cargo, and the documentation parser environment are unavailable here. The
integration CI must compile this candidate and execute
`cargo test -p shenko-wasm --test tool_artifact_roots` before a runtime result
is attributed to it.

## Store root producer and remaining boundary

`StoreLedgerRoots` is the Store-aware root producer. It owns the
installation's exclusive CAS fence and one exclusive SQLite write lease for
its whole life, and enumerates the ledger's roots through that lease's own
connection (`referenced_objects`). The caller keeps the producer alive across
`plan`/`reclaim`, so no Store publication can seal an object after the roots
were read: the fence blocks the shared publication lease a Store write must
take first, and the SQLite lease blocks the row write itself. A digest the
ledger still names but the CAS already lost becomes a root and therefore a
`CAS_ROOT_OBJECT_MISSING` refusal rather than a silent omission.

Live reader pins are now merged into every plan, not supplied by a producer:
`pinned_objects` is read under the same exclusion, so a bounded reader that
already resolved bytes keeps them until it drops its pin. Acquiring a pin
needs the shared CAS lease, so the Store producer's exclusive fence also
prevents a new pin from appearing after the read; a pin released after the
read keeps its objects for this run and leaves them unreferenced only for a
later run.

The producer still needs a runtime assembly. Nothing schedules reclamation, so
an installation is only collected by an explicit caller that already declared
the complete producer and namespace sets. If a Store namespace and its writer
are omitted entirely, this API cannot discover the omission from the CAS
digest paths. The returned `GcReceipt` is a run report, not a durable Store GC
task/deletion receipt, and one invocation covers one producer set over one
database, not every slot of an installation.

The `install_gc_closure` case exercises the new producer on a real
installation Store: archived question/source/history rows and one first-E1
receipt are retained, a live reader pin is retained until it ends, and
unreferenced post-adoption orphans are collected exactly once across repeated
runs and a reopen. It is not an all-slot, power-loss or scheduler acceptance.

The modification-time check still does not protect an old object that is newly
referenced after root enumeration: sealing an already present object keeps its
modification time. The producer's exclusions are what prevents that
publication from happening during its run; a caller that enumerates without
them keeps the old hazard.

These limitations, including the first-E1 source/application and delivery-debt
closures, remain explicit in the [native E1 retention boundary](canonical-native-e1-delivery.md#cas-retention-boundary).

The [Store retained-object inventory](store-retained-objects.md) remains a
bounded read-only audit of one committed database through `check
--retained-objects`. It is not a registered producer or an all-slot deletion
permit; the runtime assembly obligations above remain.
