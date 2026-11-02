# Durable State — Ledger, Archive and Stream Journal

Scope: the `shenko-store` ledger/commit surface, the `shenko-types` store and
commit types, the store migrations, the cognition archive (schema, fact
classes, conflict units) and the durable content stream journal.

CAS publication internals and the raw-session redactor are owned elsewhere;
this document states only the contracts this surface relies on. Everything
described here is source in this worktree. The build, the native proof and
the cross-store recovery run are the integrator's work — nothing here is
claimed as verified.

## Generic write path (SPEC §41.2)

`Store::commit(WriteBatch)` is the generic admission line. Its guarantees:

- **Objects before facts.** Every `batch.raw_objects` entry is sealed into
  CAS before the ledger transaction opens; `CommitReceipt.sealed_objects`
  reports the object refs admitted. An object left behind by a failed commit
  is a content-addressed orphan, never a dangling reference.
- **E1 closure.** Every `evidence_upserts` receipt's `raw_ref` (and
  `normalized_ref` when present) must already resolve in CAS — sealed by this
  batch or by an earlier commit — or the whole batch is refused with
  `ShenkoError::ObjectClosureIncomplete`. When a new receipt names an
  existing `operation_id`, that operation's `evidence_through` cursor
  advances to this commit in the same transaction. A resolved operation
  already holds its final fact, so its cursor is not rewritten; the receipt
  itself stays durable.
- **Revision CAS.** `task_upserts` and `operation_upserts` admit only the
  successor of the stored `record_revision`. An identical replay is a no-op;
  anything else is a typed `ShenkoError::Conflict`, never `INSERT OR
  REPLACE`. A task or operation that already holds a terminal fact (a
  terminal task, an operation with an outcome in state `Resolved`) is never
  rewritten by any writer, successor revision or not.
- **Append-only facts.** `attempt_upserts` and `evidence_upserts` are
  immutable: same identity with different content is a conflict.
- **X05 source-key dedup.** A receipt whose `source_event_key` already bound
  an earlier durable receipt binds to the first fact and inserts nothing.
- **Inbox events.** `inbox_event_upserts` no longer uses `INSERT OR IGNORE`:
  identical replays are no-ops, a different fact under an existing event id
  is a typed conflict — the raw-session semantics.

## Migrations

`run_migrations` stays additive-only. It applies every domain's schema
unconditionally through `AGGREGATE_DOMAIN_SCHEMA`, so the replicated fact
classes exist on every store regardless of which module first asked for
them. Domain modules keep their own `CREATE IF NOT EXISTS` constants;
double application is idempotent.

The aggregate mirrors, column for column, the owning domain's DDL:

| Fact class | Owner |
| --- | --- |
| research epochs / candidates / intervention bank / reminders / retention tombstones / archive moves | `store::research` |
| seat records / leases / questions / answer decisions | `store::seat` |
| person claims / identity events / admin facts / life voiceprint ranks | `store::contacts` |
| owner grants, config entries / changes / adoptions | `store::config` |
| artifact records / manifests / runtime instances | `store::artifacts` |
| channel runtime facts / record migrations | `store::channel` |
| activation records / selectors | `store::activation` |
| subject output parts | `store::output_parts` |
| media recordings / recording controls / priority inputs | `store::media` |
| content streams / content stream units | `store::content_stream` |
| subject request attempts | `store::subject_request` |
| subject obligations / subject attention policies | `store::subject_state` |

The credential digest table `owner_admin_tokens` exists locally and is never
replicated. Process-local bookkeeping (`cognition_*`, `commit_log`,
`subject_dispatch_owner`, `subject_quiesce_requests`) is never replicated
either. Memory-v3 columns (entry confidence / supersession / Git refs; intent
knowledge kind / applicability / base revision / idempotency / sources /
supersedes) run after `intents::SCHEMA`, and the subject-owned memory intent
keeps its dedicated representation: the operation is `save`, the knowledge
kind is `note`, and a missing binding sidecar is that representation's
shape, not corruption.

## Cognition archive (SPEC §5.4, §36)

`ARCHIVE_SCHEMA` is 5. Readers admit schemas 2–5; an archive naming a fact
class this reader does not know is refused whole at manifest validation.

**Membership is declared, not assumed.** Every archive carries the list of
fact classes it was taken from (`CognitionManifest.fact_classes`). The
reader checks it: an archive naming an unknown class is refused, and an
archive carrying a row outside its own declared membership is refused. A
class is a durable fact family, not a table with rows — an export of a store
with no research epoch still declares the research class.

**Version and membership migration.** An archive of an older schema is
migrated to the current column layout before it is validated: the layout
only ever grew, so the migration is by column NAME and every fact keeps its
identity. A legacy column this schema no longer knows is reported as a
non-blocking `migrated_legacy_column` gap instead of disappearing silently.
The current schema is checked strictly: a row whose column layout does not
match exactly is refused.

**REQUIRED facts.** Some facts cannot exist without their anchor: a stream
unit without its stream, a seat lease without its seat, an answer decision
without its question, a research candidate without its epoch, a memory
binding without its intent, a quota send without its attempt, a media
recording or priority input without its stream, an artifact manifest or
runtime instance without its artifact. A peer that ships a member without
its anchor has not shipped a complete fact: the export reports the gap and
the import refuses the archive instead of admitting an orphan.

A quota event names the fact it describes by id in its payload, and it is
written in the same transaction as that fact. The archive therefore also
refuses an event whose `snapshot_id` / `forecast_id` / `decision_id` /
`policy_id` is not carried by the same archive (`missing_cause`): an event
never travels without its cause.

The subject's own facts carry the same rule: an obligation and an attention
policy name the sealed decision of the SAME step that authored them, and an
obligation's origin event must be an event the archive knows. A row whose
step or event is missing (`missing_step`, `missing_origin_event`) or whose
decision is not its step's decision (`decision_mismatch`) is an incomplete
fact: the archive is refused rather than interpreted loosely. Whether the
origin event was actually SELECTED into that step's sealed S0 is checked
where the decision is admitted (the subject's own decision gate), not
re-derived from archive rows.

**Conflict units, not LWW.** Rows of a semantic unit apply inside one
savepoint; a member that diverges from a local fact rejects the whole unit
and every member is reported:

- `epoch/{epoch_id}` — research epoch, candidates, bank entries, reminders
- `seat/{seat_id}` — seat record, leases, questions, decisions (a decision
  binds through its question's seat)
- `quota_request/{request_id}` — quota attempts and sends
- `quota_event/{event_id}` — one quota event (its own unit)
- `tombstone/{tombstone_id}` — one retention tombstone
- `channel_fact/{fact_id}` — one channel runtime fact
- `channel_migration/{record_kind}/{record_id}` — one channel record migration
- `person/{agent}/{person_id}` — claims, identity events, person admin facts
- `activation/{agent_id}` — activation records and the live selector
- `media/{stream_key}` — recordings, recording controls, priority inputs
- `artifact/{artifact_id}` — artifact record, manifests, runtime instances
- `config/{namespace}/{logical_key}` — config entry, changes, adoptions
- `grant/{installation_id}` — owner grants
- `request/{request_id}` — the subject's durable request attempt
- `obligation/{obligation_id}` — the subject's durable obligation
- `attention/{intent_id}` — the subject's own attention decision
- `output/{agent}/{stream}/{message}` — output parts of one message
- `stream/{stream_id}` — the journal stream and all of its units

**Merge rules per class, not a blind verbatim copy.** Two classes have their
own rule:

- *Monotone* facts (`intervention_bank_entries`, `retention_tombstones`) —
  a peer may retire a live local row; a peer may never revive a retired
  one, and the first durable retirement of a fact is the one that stays.
- *Revisioned* facts (config entries / changes / adoptions, owner grants,
  artifact records / manifests / runtime instances, activation records and
  selectors, channel runtime facts, the subject's attention decisions) —
  the peer advances the record only with a strictly greater revision of the
  record itself. Which peer answered first never decides.
- *Laddered* facts (the subject's request attempts and obligations) — the
  peer may move the row FORWARD along its state ladder and never backward:
  `armed -> dispatched -> terminal` for a request attempt, `open ->
  completed | cancelled` for an obligation. A rewind is a conflict, and a
  terminal state is never rewritten. The stored state text is read in both
  the bare and the serialized form, so the writer's encoding does not
  change the rule.

Every other class keeps the delivered-baseline rule: the peer may update a
record only while this store still holds the value the peer last delivered,
and any other divergence is a reported conflict, not an overwrite.

Legacy archives may migrate columns by name, but migration is not permission
to accept contradictory subject-head facts. Before publishing rows or files,
the importer compares the head's identity, branch, step, revision and mode
index columns with the typed serialized head. A mismatch rejects the import
without advancing the receiving ledger or publishing a subject. Current
schemas additionally retain the canonical column-layout check.

**The closure collector.** A JSON map that names a digest is still a
container: the collector records the digest as an alias AND descends into
the map's other values, so a manifest that names its artifact inline does
not hide the objects it points at.

## Content stream journal (SPEC §12.5)

`content_streams` / `content_stream_units` record every unit a transport
delivered, bound to the durable work identity, so a UI reconnect or a
recovering consumer re-reads what it missed instead of trusting a callback
that died with the process.

Contract on `Store`:

- `open_content_stream(stream_id, agent_id, kind, binding)` — registers a
  stream against the durable identity of the work that produces it. The
  same stream id with a different binding is a conflict.
- `journal_content_unit(stream_id, seq, shape, content_kind, data, terminal)`
  — one unit. `shape` is `whole` (a complete request/result/effect unit) or
  `chunk` (one bounded piece of a stream); the two shapes are never merged.
- `journal_content_batch(stream_id, seq, content_kind, data)` — one bounded
  chunk unit: the producer's flush boundary IS the durable unit boundary.
- `journal_content_terminal(stream_id, seq, content_kind, data)` — the whole
  unit that ends the stream. The terminal row exists even when the stream
  published no chunk at all.
- `close_content_stream(stream_id)` — closes a stream; idempotent.
- `content_stream_units(stream_id, since_seq)` — the reconnect read.
- `content_streams(agent_id)` — every stream of one agent.
- `content_stream::provider_stream_id(request_id)` — the one canonical
  stream name (`provider/{request_id}`) for provider, subject and UI.
- `ContentStreamBatcher::new(store, stream_id, content_kind)` — the
  producer-side bound: a batch is journaled at 4 KiB or 100 ms, whichever
  comes first, and the last partial batch is a real unit.

Bounds are durable, not advisory: a chunk unit above 4 KiB and a whole unit
above 1 MiB are refused as typed conflicts rather than silently split. A
unit after the terminal unit or after an explicit close is refused. An
identical replay of `(stream_id, seq)` is a no-op that answers with the
original admission commit; a different unit at the same position is a
conflict.

`commit_step_and_advance_head` journals each committed step's model result
as a `whole` `model_result` unit of the `model/{agent}/{branch}` stream in
the same S1 transaction, bound to the request / attempt / step identity.

Journal rows replicate under the `stream/{stream_id}` conflict unit, so a
recovered node replays the same units instead of re-streaming.

## Error and recovery notes

- `ObjectClosureIncomplete` and `Conflict` on the generic path roll the
  whole batch back; nothing partial commits.
- A conflicting unit keeps the complete local unit and reports every
  member; the import acknowledgement stays honest about what was refused.
- An archive with a missing anchor, an unknown fact class, a row outside
  its declared membership or an unverifiable payload publishes nothing.
- A journal unit is never rewritten: a corrected tail is a new sequence
  position, and the reader re-reads by position.

The peer's "verified recoverable" claim is derived, never asserted: a
receipt is verified only when the imported snapshot has no blocking gap and
no conflict. Node unit storage is not a replicated fact class here — it
travels as the bytes of the node's own database under a memory or workspace
root, and this reader never interprets its sections. A reader that must
admit a unit schema it does not know has to refuse that unit, not skip the
section and keep claiming a verified recovery.
