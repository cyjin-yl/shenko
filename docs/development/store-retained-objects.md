# Read-only Store retention inventory

`shenko --data-dir <installation> check --retained-objects` reads one existing
Store and its CAS, prints a bounded JSON inventory, and returns failure when
that inventory has gaps. It enters before the ordinary `check` command opens
a writer or prepares home state. The existing `check` behavior is unchanged
when the option is absent.

The command uses the existing stable main-database/WAL tuple inspector. It
copies a finite, signature-checked tuple to a private temporary directory and
lets SQLite read the committed view there. It does not migrate the original
database, initialize its SHM, take over the writer, create an installation or
repair a home. The temporary copy still needs disk proportional to the tuple;
the new row/object limits below are not a claim that staging a large database
uses constant disk space. A missing database or CAS directory is an error.

An installation's `current-slot` pointer chooses the physical database for
this command. It is only a selection hint: the output names the database and
does not attest that it is the authoritative active generation. Initial,
historical, candidate and staging sibling slots are outside that output.

## Source and carrier scope

`Store::retained_object_snapshot` reads a live handle in one read transaction.
`Store::inspect_retained_objects` reads the same information through the
private tuple inspector. Both bind the ledger identity, maximum physical
commit sequence, retained per-ledger commit watermarks, SQLite schema cookie,
schema digest and exact typed-value digests of the table rows read. The table
digest includes column names, SQL types, lengths and row boundaries in the
observed scan order. A skipped or oversized row prevents a complete table
digest. The report contains source identities and hashes, not original row
payloads, provider output, account credentials or historical locator values.

The current additive Store schemas are initialized in a separate, empty
in-memory connection to obtain the supported table catalogue. That operation
does not open a Store writer or manufacture a source commit. The inventory
reads every current base table, including local tables excluded from cognition
replication. Known FTS projections and their shadow tables are derived and
excluded; an unknown table, changed carrier layout, unsupported BLOB, invalid
required JSON, unsupported record representation or exhausted budget makes
the inventory explicitly incomplete. The scanner does not migrate a historical
layout to make it look supported.

The included local carriers cover prepared control transitions and physical
materialization receipts; first and successor E1 seals; pending, blocked and
applied delivery descriptors; seal debts and reconciliation cases; local home
provisioning; cognitive root nodes and requests; and cognition baselines.
The general raw-session/event and activation carriers retain completed build
observations and their recovery closures. Ordinary task, operation, attempt,
original evidence and receipt-conflict records are read in the same snapshot.

Typed `ObjectRef` members are mandatory CAS objects. Known independent object
columns, including research and decision references, must decode as complete
objects: JSON `null`, arrays or missing reference fields cannot silently mean
an empty closure. Local E1/delivery/home and canonical preparation/application
records also decode their current root-bearing shapes. A malformed carrier
still contributes every readable edge while making the inventory incomplete.
The normalized cognition content-digest columns also require local bytes.

`ArtifactRef` has only a digest and locator. A canonical CAS file locator's
digest/shard suffix identifies required local bytes even when its historical
installation prefix differs; that prefix is never read. Builtin, native,
channel and declared external locations remain separately classified. An
unclassified location or conflicting CAS suffix is a gap. `SourceRef` may
also carry a digest with a null locator; it remains an optional digest, while
typed Task/Operation decoding checks their non-null `ArtifactRef` contract.
Within the typed `cognition_attitudes.reasons_json` column, a reason's
`SourceRef` also permits a plain source description. Its digest remains an
optional retained edge; its locator is descriptive text and is not decoded
as another reference carrier. This interpretation follows only the known
reason/termination/source path. An unknown SourceRef field remains a gap and
is scanned for visible edges. Arbitrary JSON and encoded baseline mirrors
continue to use the conservative generic reader.
The historical `memory_intents.content_ref` string also requires its canonical
CAS-suffix member; an unrecognized legacy locator is an explicit gap rather
than an empty root set. Newer memory bindings additionally carry the complete
ObjectRef and its JSON reading where applicable.

The installed `ToolArtifactRef` storage descriptor is a different shape, with
no ObjectRef codec. Its `content_store` form retains the real JSON chunk
manifest and its ObjectRefs; the reassembled module digest is an identity,
not a promise of a whole-module CAS file. Its `node_inline` form identifies
Node's independent object database. Unsupported kinds, malformed descriptors
or unsupported chunk manifests make the inventory incomplete. These storage
readings do not invoke a tool or verify an installation's selection authority.
The manifest edge must name the current Store adapter's namespace and codec.
Generic JSON and tool-manifest expansion are cached as different readings of
the same digest; seeing bytes through the former never bypasses the latter's
shape check. Each additional read/parse consumes the same byte/JSON budgets,
and every manifest edge still checks its own codec and declared size.

Other canonical bare digests may name external executable files, Git content,
schema identities or optional aliases; an absent bare digest is listed
separately instead of being invented as a local object. The reader never
follows a locator or retrieves a credential.

Object bytes must have the declared length and digest. JSON-typed objects are
walked recursively, including JSON embedded in strings by canonical
`Cell::Text` and cognition baseline row values. This is conservative retention
discovery, not validation of an admission, decoder, effect or peer claim.
Opaque raw output, WASM and other non-JSON payloads do not acquire graph
authority by resembling JSON. A digest seen first as opaque can still be
expanded when a later typed JSON reference supplies that reading. Invalid
required carriers and missing mandatory objects remain visible gaps; the
reader does not substitute a new seal or discard their references.

## Bounds and refusals

Default maxima are one million rows, 8 MiB per selected row, 64 MiB of aggregate
JSON input (including nested encoded JSON), 8 GiB of streamed SQL/CAS bytes,
65,536 reference occurrences and JSON depth 64. Callers can lower these limits,
not raise them. Schema inspection separately limits the number of tables,
columns, metadata name lengths and schema text. Diagnostic details are capped
at 256 entries while a counter records further gaps.

The reader requires the Store's UTF-8 database encoding. SQL row bodies are
selected only after `octet_length(column)` reports that their aggregate input
fits the row bound. SQLite documents that text/BLOB column length metadata
allows this check without loading the whole value
([SQLite core functions](https://sqlite.org/lang_corefunc.html#octet_length)).
CAS files are streamed in 64 KiB buffers, with a regular-file check, final
digest verification and a one-byte overrun check; only bounded JSON bodies are
retained for parsing. These are input/work bounds, not an assertion about every
internal allocation of SQLite or a JSON value tree.

`complete=true` means the supported reference carriers in this one committed
database were enumerated and their mandatory local objects verified within
these limits. It does not resolve a pending delivery, validate a malformed E1
proof, acknowledge Unknown, authorize a worker, make an effect conclusive or
prove a candidate executable. A report can retain the bytes of such unresolved
facts without declaring their business outcome successful.
Generic JSON discovery enumerates visible reference shapes; it does not
establish that every application codec was semantically decoded or validated.

## Verification scope

This increment adds three bounded risk/recovery observations:

1. The existing authenticated E1 journey observes its already-present interrupted
   download as an incomplete mandatory root, then inventories the real first,
   successor and acknowledgement history at source and receiver. The public
   `check --retained-objects` path is exercised against that receiver. Original
   receipt, two-effect, no-T2, replay and Unknown assertions remain intact.
2. The existing prepare/reopen journey checks that both the raw build observation
   and the candidate recovery closure retain the original build receipt and
   outcome. This does not add a build qualification claim.
3. One Store journey lowers the row bound, observes explicit refusal, then
   inspects the same committed row and its nested JSON/CAS reference with the
   normal bound. It checks unchanged original event text, DB/WAL/SHM bytes,
   commit watermark and CAS inventory/content.

These are three added retention observations, separate from the six frozen
GC declaration windows. The candidate has static review only in the recovered
workspace: Rust, Cargo and the documentation parser environment are absent.
Compilation and these actual journeys remain pending integration CI.
The later carrier-reader correction adds no scenario or injected corruption
window. Its storage-shape review follows `types/object.rs`, the research and
control/E1 row writers, `wasm/artifact.rs` and the real
`tool_install::bind_artifact_to_recovery` carrier. It has no independent
executed ToolArtifact/corruption validation claim.

## Remaining deletion boundary

This reader is not registered as `CasRootProducer`, and no runtime GC scheduler
or reclamation path is added. A complete installation inventory must still
cover every shared-CAS slot and all non-Store producers. Node control objects
live in a separate object database; a queued Store descriptor does not pretend
that Node-downloaded bytes already exist in this CAS.

The read transaction does not exclude CAS deletion or seal-to-root publication
by another handle, process or slot. An object can become newly rooted after
enumeration without changing its modification time. True all-writer exclusion
or an equivalent epoch protocol, all-slot discovery and deletion-time revision
validation are still required by the
[durable-state contract](../specs/durable-state.md#spec-41-2-2).
The existing [registry checks](cas-root-declarations.md) still cannot discover
an entirely omitted namespace from digest paths. No permanent pin, second root
registry or deletion receipt is introduced to hide those missing mechanisms.
