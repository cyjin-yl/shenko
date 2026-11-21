# Canonical capture capacity repair

## Current implementation: physical writes and S1 v2 (2026-11-17)

The current increment, tracked with issue #316, supersedes the whole-database
fingerprint and ordinary-admission fallback described in the historical record
below. It preserves one evaluation, schema checks, exact source result identity
and the original admission/materialization protocol.

SQLite's compiled `preupdate_hook` now observes actual base-table writes during
the savepoint. Capture no longer scans every unrelated table before evaluation
and again afterward to discover hidden writes. The authorizer still records
read dependencies and refuses non-row SQL; the independent execution-time hook
detects writes missing from that observation, including changes whose only
different bytes are a local sequence. S1 v2 retains the first original image of
each touched primary key and reads its actual final postimage before rollback.
This also handles an implicit REPLACE deletion and WITHOUT ROWID tables. A
blob-write callback has a delete-shaped event, so its postimage comes from the
actual row rather than from the event name. The relevant primary API is
[SQLite's preupdate contract](https://www2.sqlite.org/c3ref/preupdate_blobwrite.html).

New S1 objects use `store-s1-transition` major 2 and row schema 2. They keep
exact write deltas and fixed-size before/after digests of complete logical read
ranges. The digest streams typed cells in stable key order and normalizes only
the same local materialization-sequence columns as the original image format.
Application checks the schema, range digest and exact changed-key preimages
before writing, then checks both final changed rows and final range digests.
Empty-range/absence conditions remain binding. A partial witness now reads its
named primary keys directly instead of scanning a whole table and filtering it.
Major 1 objects remain readable; schema and codec versions must agree.

`context_execution_clock` and `context_execution_revisions` are local cache
invalidation projections, not canonical facts. Their known factual-row triggers
run again when the receiver applies the frozen base rows; freezing the counters
as additional writes would advance them twice. Capture permits their access
only from those triggers and independently rejects direct physical writes,
including after removal of the authorizer. The affected base table's existing
schema digest also binds both projection tables and their triggers, so adding
or replacing a secondary projection trigger between capture and application
refuses application before any write. Exact factual preimages, postimages and
read ranges remain checked; a local cache index cannot supply a semantic read
dependency or a fresh execution grant.
The additional schema dependency applies only to base tables that actually
contain these new index-maintenance triggers; without them, prior digest bytes
are unchanged. Earlier experimental captures of the new index schema remain
decodable, but a digest lacking this dependency is conservatively refused at
materialization and retained through the existing reconciliation path. Missing
original schema evidence is not fabricated by rewriting the sealed transition.

Native claim/arm/dispatch/T2 retain their versioned typed witnesses. Ordinary
paths use full read rows to verify operation absence and unfinished-child
ranges; the direct-chain operation predicates below are a separate version.
Write-only tables now retain actual touched keys from
the start. Direct first-claim, claim-proven T1 and childless E1-proven T2 already
have a separately validated single-task witness; capture now constructs that
same witness before copying any historical task rows. The exact target shape
is checked again, and the existing ancestry, claim-birth and E1 validators still
run before admission. No full read range is replaced with an unproven digest.
The row-engine journey retains a 129 MiB write-only history and a 66 MiB unrelated
task while capturing and applying the small native-v1 delta.

### Direct native subject witness (2026-11-22)

The real protocol observations found three selected `native.claim` actions at
Node control index 7 with durable acceptance and no Store materialization. The
frozen complete `subject_steps` image differed from the live table, and
recovering the same selected value retained that conflict. The source path
includes the next main step's mutable S0 bytes in that image, so concurrent S0
progress is the mechanism this fix targets. The original error alone does not
identify the changed row at the precise failure instant. The absence of an
operation or attempt did not make those selected actions safe to retry.

The subject-witness increment introduced claim codec 2. The original proof-bearing paired
T1 and childless original T2 format introduced codec 3 and row schema 3;
old claim 1, arm/T2 1–2 and ordinary dispatch retain their original row schema 1
semantics. Only the independently verified S1/claim/E1 chain can use the new
subject witness. Its `subject_steps` image contains exactly the immutable
creator's full row, fetched by primary key before and after the single source
evaluation. It cannot carry a step write. An observed trigger or view read of
`subject_steps` is outside this closed proof and refuses capture before storing
a prepared transition or requesting choice, instead of discarding that
additional dependency. Earlier predecessor CAS publication is unaffected.

The source still calls `read_subject`. In the final IMMEDIATE transaction,
every new-version application calls it again and compares the complete head:
the current successor must exist and match its branch, step number, parent,
controller epoch and software generation; paused-boundary checks also remain.
Only that successor's unrelated state, request and input progress may differ.
The separately proven creator stays an exact row dependency. Prepared and
learned applications perform the same checks and never issue a new worker or
execution permit. No chosen bytes, source identities, E1 or Unknown outcomes
are rewritten, and this change introduces no retry.

That subject-witness format introduced [archive schema 10](../specs/durable-state.md)
so older schema-7 readers refuse before payload publication. Current readers
retain the old archive and transition versions; raw Node object transport is
a separate compatibility boundary. The existing first-claim and 129 MiB chain
regressions now include post-choice real S0 progress, different successor
progress on receivers, and post-preflight missing/badly bound successor and
changed-creator refusals. These are controlled Store/authority fixtures;
execution and fresh real-model journey evidence must be reported separately.

### Direct native operation predicates (2026-11-22 candidate)

The actual release `cdf71091` browser journey retained three failures out of
three attempts. In attempts 001 and 003 the ordinary `host.printf` arm lost its
complete `attempts` predecessor; attempt 002 reached real printf E1 but its
ordinary T2 lost the complete `tasks` predecessor. The
[attempt-001 witness audit](../evidence/daily-readiness-20261122/final-cdf71091-attempt001-native-arm-witness.json)
binds the chosen arm codec 1, empty frozen operation/attempt images, the later
independent model-child rows, and the original Store error. The stopped ledger
is a later observation, not a reconstruction of every cell at the error instant.
The later missing-materialization error does not make the chosen arm safe to
replace or execute again. These failed receipts remain failures.

New direct first claims use claim codec 3 / row schema 4 and freeze only the
absence of their deterministic original operation's physical PK. This is the
same predicate as `evaluate_claim`; it does not add arm's JSON-alias or logical
key semantics. The typed claim reader requires exactly that selector and an
empty matching image. Both source capture and application requery the point
before and after the task write. Old claim 1/2 objects retain their full
operation witnesses and original row-schema 1/3 interpretations.

The printf executor now requests the existing paired native T1/dispatch permit.
Only its fresh return permits the original execution; replay and recovery still
return a factual read. For a proven direct S1/claim chain, new arm codec 4 binds
row schema 4 and two closed, typed predicates:

| Table | Complete predicate whose matching rows must be absent before arm |
| --- | --- |
| `operations` | Physical operation PK or decoded operation ID equals the planned original ID, or decoded task ID and logical key equal the planned pair. |
| `attempts` | Physical attempt PK or decoded attempt ID equals the planned attempt, or physical/decoded operation ID equals its original operation. |

These are not sender-provided SQL fragments. The typed arm reader derives the
exact selector set from its validated plan and rejects missing, duplicated,
substituted or extra selectors. It requires empty matching preimages, and only
the original task, operation and attempt writes remain legal. Capture and both
materialization checks use the same predicate. Arm streams and parses each
table's original `OperationRecord`/`AttemptRecord` one row at a time; it compares
decoded `EntityId` values so UUID spelling aliases cannot conceal a collision.
Malformed originals still refuse. Only matching full physical rows are retained,
subject to the existing budget, but scanning and JSON parsing remain dependent
on history size. This is not an indexed or constant-time absence query.

New childless, original-E1-proven T2 also uses codec 4 / row schema 4. Its three
selectors retain the full original operation, attempt and receipt rows by their
physical primary keys. The reader binds those selectors to the plan's original
identities. The original E1 and its operation/attempt projections are validated
and, on a learned installation, staged in the same IMMEDIATE transaction before
the row predicates run. Existing or newly observed conflicting original facts
refuse the transaction; no Unknown is folded into success. The exact target
task, independently proven child absence, creator row, current successor and
complete subject-head checks remain in force. An unrelated child may advance
without becoming an exact read dependency of these selected original rows.

Both direct writers reject trigger/view reads of the narrowed operation tables
before publishing a prepared transition or requesting choice. The physical
write observer still sees every write; a write outside a selected predicate is
refused. Full schemas and trigger definitions remain bound, and preimages,
absence, writes and postimages are checked under one final IMMEDIATE transaction.
Ordinary arm keeps its original format. Retained old arm/T2
codecs 1–2 still require row schema 1; codec 3 requires row schema 3; only codec 4
accepts row schema 4. Claim codec 2 continues to use row schema 3; claim codec 3
requires row schema 4. Rows 1–3 cannot
carry a selector. This introduces no retry, serialization of unrelated tasks,
replacement chosen action or receiver execution permission.

All new cognition snapshots declare archive schema 11, including snapshots
without a direct action. Schema 10 remains readable with its original strict
fact-family/layout rules; schemas 8 and 9 remain unsupported. Older readers that
only admit schema 10 or earlier refuse schema 11 before payload CAS publication.
The archive gate does not version raw Node object transport.

The existing direct-chain regression adds six necessary windows: real unrelated
E1 progress after the target claim's choice and after its arm choice, trigger/view
dependency refusal, logical-key absence conflict, a same-ID uppercase UUID
under another physical key, and a T2 selector naming another attempt. It retains
the original malformed-E1, staged-E1 rollback, foreign parking, one-effect,
lost-reply, reopen and replay assertions. One retained-schema-10 import/relay
window, one ancestry-dispatch interleaving and the generic-T2 interleaving below
bring this same candidate to nine new windows across existing test functions.
No new test function or test factory is introduced.

The learned T2 receiver lacks the unrelated source operation/attempt/E1 entirely
and still must retain its exact original result; T2's original lost-reply,
rollback and replay checks remain in that same chain. Static
review checks the explicit legacy codec/row pairing; it is not a Rust execution
receipt. These candidate checks and the next actual model journey still require
the integration owner's scoped execution; this source change does not turn the
three CDF journey failures into passes or claim a measured latency improvement.

### Generic T2 ancestry and ordinary children (2026-11-22 candidate)

The source path for `model.child` completion also reaches canonical T2, without
the direct native claim/E1 proof. Its old codec-1 transition retains full task,
step, operation, attempt and evidence tables. This is a source-confirmed
remaining concurrency risk; the CDF printf errors do not by themselves prove
that a particular child completion failed at this same point.

New generic complete/finish objects use T2 codec 5 and row schema 5, with no
`evidence_fact`. The fixed task selector retains the target and its complete,
bounded owner chain plus every ordinary direct child (`!background` and typed
`TaskOwner::Task` pointing at the target), including terminal children and an
empty range. The authoring-step selector retains the actual creator rows needed
by that ancestry's branch checks. Original operation, attempt and E1 remain
full physical rows selected by their original PKs. The shared ancestry reader
checks physical/JSON identities, subject, owner structure, branch and active
ancestors; the target/ancestor revision fences and admitted `expected_tasks`
must match that same ordered chain. Ordinary children are read dependencies,
not additional Node `expected_tasks` or conflict aliases.

Source evaluation still calls the original `unfinished_children` SQL. The
typed transition validator still derives unfinished ownership from decoded
`TaskRecord` owner/background/terminal fields. The selector also uses those
typed child identities, so a UUID spelling alias cannot remove a row that the
old validator would have seen; a SQL/typed disagreement remains a refusal.
No new plan field changes a retained codec's input digest. All apply modes
recheck `ensure_owner_active` and `read_subject` in the same IMMEDIATE transaction
as the selector preimages, exact result writes and postimages. The final T2
cannot bypass an unfinished child or substitute another attempt/receipt.

The existing completing/finish regression gains one chosen-to-application
interleaving: the actual main S0 advances and a different operation receives
another real Store E1, while the ordinary child's task remains unfinished.
The original selected parent result must still be `Completing`, then wait for
that child's actual T2 before the separate finish/reopen path can publish the
parent result. Its original lost-reply, receipt identity and no-repeat checks
remain. This is a controlled Store mechanism check pending execution, not a
model-success or latency result. The child scan still parses task history;
the change bounds retained dependencies rather than claiming constant-time
ownership queries. Old T2 codec 1 keeps its original full-witness semantics;
direct codecs 2/3/4 retain their existing proof and row-schema pairing.

### Remaining issue #316 boundaries

The opt-in [request-path timing](request-path-timing.md) records canonical
preparation, snapshot, validation, lock acquisition, capture, dispatch admission
and materialization as nested measured scopes. These are observations on the
candidate being run, not a retrospective explanation of an earlier delay or
an assertion that the remaining history-dependent work below is fixed.

The native full witnesses remain history-dependent at these concrete paths:

| Path | Still retained complete read dependencies | Consequence |
| --- | --- | --- |
| `claim_transition::evaluate_claim` | Subject heads; old claim-1/2 objects also retain full `operations` | Claim 3 uses its original-operation PK absence. Full retained images still share the 128 MiB capture limit; sealed transitions remain limited to 64 MiB. Old claim-1 preparations also retain the original complete `subject_steps` image. |
| `arming_transition::evaluate_arm` | Ordinary paths retain full `operations` and `attempts`; all paths retain full `subject_intents` (publication absence) and subject heads | Direct arm 4 retains its two typed absence predicates instead of every unrelated row, but still scans/parses operations and attempts. Ordinary/non-direct ancestry also retains `tasks` and `subject_steps`. |
| `dispatch_transition::evaluate_dispatch` | Full `tasks`/subject reads and named operation/attempt tables in the retained codec-1 format | The separate ancestry-selector dispatch increment defines the new format; retained codec 1 is not reinterpreted. |
| `native_transition::evaluate_native_t2` | Subject heads; old generic T2 1 also retains full task/step/operation/attempt/evidence tables | New generic T2 5 retains the complete typed ordinary-child range and owner/creator chain, with PK operation/attempt/E1; finding the child range still scans/parses tasks. Direct E1-proven T2 4 has its separate childless proof. Both retain current-successor validation. |

All canonical actions also call
`control_predecessor::prepare_control_predecessor_inner`, which takes a full
typed cognition snapshot, validates its mandatory-fact closure and repaginates
the immutable control tree. CAS reuse avoids duplicating unchanged objects but
does not avoid reading, parsing and validating that snapshot. The corresponding
inspector reads all those rows again to verify ancestry/child absence. This is
an additional CPU, I/O and transient-memory cost outside the row capture budget.

The [control predecessor publisher](control-predecessor-publication.md) batches
only one graph's child-directory barriers before its ordinary durable root
seal. New files still synchronize individually and every dedup still validates
the full length/hash. This does not remove snapshot/closure work, narrow the
read witnesses or activate GC; measured performance on the changed candidate
remains a separate requirement.

Removing these remaining dependencies safely requires versioned native typed
key/predicate witnesses, their independent absence/ancestor/child validators,
and matching receiver-side materialization checks. Simply marking them partial
or hashing a complete table would let an incomplete witness claim absence.
The predecessor snapshot also needs its own incremental authenticated index and
closure validation design. Those are explicit remaining #316 protocol/storage
work, not behavior claimed by this increment. S1's complete-range digests still
hash actually read tables and conservatively conflict with unrelated rows; no
predicate-level or constant-time read hashing is claimed.

The 128 MiB retained-cell and 64 MiB encoded-delta limits still apply to actual
retained data. Exceeding either keeps the original decision uncommitted and
records the capacity diagnostic. It never switches to ordinary admission,
re-evaluates the semantic callback or claims a successful commit. The existing
row-engine and public S1 journeys now cover a bounded multi-megabyte read-range
object, WITHOUT ROWID REPLACE, 65/12 MiB read histories that stay canonical, and
a genuinely oversized changed cell that refuses without another grant.
Execution results for this increment belong to the integration validation
record; the older CI results below describe their original revisions only.

## Historical scope and source

This change repairs the shared row capture used by canonical S1 and native
transitions. It starts from public source
`7905e8413b96015d96ea301bd1f3b6937dcfff90` (tree
`241909742fd73225b08b88fea967197a74f65334`). The earlier local draft was lost
when the workspace became unavailable; this implementation was written again
from the verified recovered source. It is not an assertion that the lost draft
was tested or retained.

The prior implementation copied all keyed base-table rows into a 128 MiB
preimage budget before observing the transaction's dependencies. Consequently,
an unrelated large historical table could block an otherwise small native T1
or T2 transition. The final 64 MiB S1 JSON refusal also did not enter the same
specific capacity path as the 128 MiB row refusal. The existing original-plan
S1 admission and its fail-soft refusal journal remain the fallback mechanism;
no fallback is added to native execution boundaries.

## One evaluation and the same predecessor

Capture still holds one IMMEDIATE transaction throughout. It records table
layouts and physical fingerprints, opens a savepoint, and installs SQLite's
authorizer before evaluating the existing factual SQL callback once. It retains
only observed tables' postimages, checks layouts and untouched-table
fingerprints, then rolls back the savepoint. Still holding the same transaction,
it reads those tables' actual preimages and builds the frozen changes. It does
not run the callback again to reconstruct IDs, semantic clock outcomes or
source receipt coordinates.

Complete read ranges, including empty ranges, remain complete preconditions.
Insert-only tables still bind the affected keys. Application keeps the original
layout, predecessor, postimage, foreign-key, authority reservation and local
materialization checks. No new worker/send permission or physical effect replay
is introduced.

The fingerprints are independent guards against actual writes absent from the
observer's access set. They stream borrowed SQLite values and encode column
count, row boundaries, SQLite storage class, length and value bytes. Primary-key
columns lead the deterministic query ordering; remaining columns break ties in
unkeyed history. Local sequence columns are hashed as their real stored values,
so a hidden sequence-only write cannot disappear behind a `LocalSequence`
projection. All base factual tables are scanned, including unkeyed ones. A
touched unkeyed table is still refused because it has no stable replicated row
identity.

Layouts and complete schema catalogs are checked before and after evaluation
and after rollback. DDL, attachment changes, transaction/savepoint control and
non-main factual accesses are explicitly denied during observation. A new or
unknown table is not silently classified as an FTS shadow. Only names present
in the original SQLite projection/metadata inventory may take that existing
exception. A schema change hidden from the observer also refuses the capture.
The only allowed PRAGMA is the argument-free, read-only `main.data_version`
check used by FTS. Other argument-free PRAGMAs can perform maintenance or change
connection/database state; they are not inferred to be safe from their missing
argument and remain refused. None of the currently captured S1, claim, arm,
dispatch or T2 SQL bodies needs another PRAGMA; connection setup, application
foreign-key deferral and schema inspection run outside this observation phase.

## Cached statements and active cursors

The SQLite authorizer is a prepare/reprepare hook; it is not a callback on every
row step. The implementation therefore keeps the hook installed through the
whole evaluation, flushes rusqlite's statement cache, and rejects a connection
with an already active statement before installing the hook. It also refuses
an evaluation that leaves a statement active after the savepoint has been
rolled back and released. The public
`Connection::is_busy` implementation walks SQLite statements and checks whether
they were stepped without being reset or completed.

The timing of that second check matters: FTS5 can retain an internal,
read-only incremental-blob statement after the application's SQL statement
has completed. Its rollback callback closes that reader. The check therefore
runs after rollback, without exempting NULL-SQL statements: application-owned
cursors or incremental blobs are not automatically trusted because SQLite
does not give them a SQL string.

SQLite also sends a no-column read callback for queries such as unqualified
`count(*)`, with an empty column and no database qualifier. Before installing
the observer, capture requires every non-main database to have an empty schema
and freezes the main table/projection names in the same IMMEDIATE transaction.
Only that no-column callback naming a known main object may omit the database.
An existing temp or attached schema object refuses capture, even if the
callback would otherwise report a name also present in main. Explicit non-main
reads/writes, ATTACH and DDL remain refused; the independent schema and physical
write guards remain in place. This conservative namespace restriction avoids
mistaking an unqualified temp/attached dependency for a main-table read.

The pinned bundled implementation is rusqlite 0.32.1 / SQLite 3.46.0.
[`sqlite3_set_authorizer` in SQLite 3.46.0](https://github.com/sqlite/sqlite/blob/version-3.46.0/src/auth.c)
expires prepared statements when installing a non-null authorizer, including a
checked-out cached statement that a cache flush cannot destroy. The dedicated
journey holds such statements across installation and checks both an existing
row/FTS-trigger path and an empty range. This behavior still requires actual
execution under the pinned Rust dependency before it is claimed verified.
See also the primary API contracts for
[authorizer timing](https://www.sqlite.org/c3ref/set_authorizer.html),
[busy statements](https://www.sqlite.org/c3ref/stmt_busy.html), and
[rusqlite's connection implementation](https://github.com/rusqlite/rusqlite/blob/v0.32.1/src/inner_connection.rs).

## Bounded retained copies and precise refusal

One 128 MiB accounting budget covers simultaneously retained afterimages,
actual beforeimages, row containers, cells, primary keys and duplicated change
rows. Moving a captured afterimage into a change does not clone it. Change-vector
growth is charged before reservation; the charge for the old allocation is
retained conservatively. SQL values are sized before making owned copies.

A text or BLOB primary key can expand substantially when encoded as JSON. A
bounded sink measures its exact encoding before charging and allocating the
output. The sealed S1 image uses the same measurement strategy at 64 MiB. This
does serialize immutable representations twice, but never evaluates the SQL
callback twice. CAS root discovery borrows the captured text cells instead of
first creating another complete `CognitionRow` copy. Its reference and bare
digest semantics are unchanged.

Only the two exact typed pre-preparation capacity refusals enter S1's existing
original-plan path. An unrelated conflict, a string that merely contains a
capacity phrase, malformed sealed data, allocation failure or storage error
does not qualify. The failed canonical evaluation is rolled back and no
prepared canonical admission is published. The caller retains the same
decision, S0/result references and successor ID, journals the capacity fact,
then attempts ordinary admission. The ordinary SQL transaction can allocate its
own unchosen local facts; it cannot replay a physical effect, and only its
successful COMMIT receipt proves that S1 advanced. A journal entry explicitly
does not assert that the later admission or COMMIT succeeded.

These bounds do not claim total process RSS. SQLite pages/sorting, schema
metadata, JSON parsing and the existing CAS closure walk have their own memory
costs. Full-table read dependencies can still be too large, and every base
table is still scanned before evaluation and every untouched table afterward.
This repair removes unrelated-history copying and its 128 MiB refusal coupling;
it does not solve scan latency, disk I/O, or generic large-ledger performance.

## Verification boundary

Two continuous journeys reuse the real Store and existing S1 fixture. Their
distinct new risk windows are explicitly limited to nine:

| Window | Required observation |
| --- | --- |
| Cached existing read and ordinary FTS triggers | The held statement is observed; rollback restores base/search state; applying frozen bytes preserves the generated result without another evaluation. |
| Cached absent read | A later row in the previously empty range prevents application before writes. |
| Already stepped cursor | Capture refuses before invoking the callback. |
| Hidden sequence write | The independent physical fingerprint rejects it and rolls back all evaluated writes. |
| Hidden DDL | Schema comparison rejects the new table and rollback removes it. |
| Expanding BLOB primary key | A real 48 MiB key exceeds the remaining JSON-key budget before a large output buffer is allocated; no preparation or mutation survives. |
| Unrelated 129 MiB history | A real canonical S1 commits, with a canonical preparation/materialization and the original history retained. |
| 128 MiB retained-row refusal | The unchanged decision uses ordinary admission, produces one actual successor/result receipt, and retains the precise refusal journal. |
| 64 MiB serialized-image refusal | The independently reached JSON ceiling has the same honest original-decision behavior and a distinct precise journal. |

The existing two-ledger S1, native transition and receipt journeys are not
weakened or replaced. No new test is ignored and no production budget is
lowered for the fixtures. The new unit journey is
`capture_observes_cached_sql_and_refuses_unrepresented_state`; the public S1
journey is
`canonical_s1_capacity_is_local_to_captured_rows_and_retains_the_decision`.

At the initial implementation checkpoint the restored environment had no Rust/Cargo toolchain.
`git diff --check` is a whitespace check only. No new Rust compile, Clippy,
Store journey, distributed deployment or long-running trial has executed here;
the next candidate's real CI must determine those results. A small local
SQLite 3.53.1 diagnostic observed the ordinary FTS update using row actions and
read-only `data_version`; it is not the pinned SQLite 3.46.0 journey and is not
substituted for that pending execution.

`python3 scripts/check_docs.py --verify-migration` was also attempted. It
exited 1: the restored environment lacks the configured symbol and CSS parsers
(1,945 symbol-parser reports and three CSS-parser reports). The complete output
was retained for handoff. This is an unavailable documentation check, not a
passing gate or a demonstrated product failure.

## CI 83bf failure and the narrow source repair

[CI run 43944367545](https://github.com/cyjin-yl/shenko/actions/runs/43944367545)
tested public head `83bf391440adbe3e153b036ba8fd1ecfe5867202` through its actual
merge checkout `2d275430b103fb0e13316ba51cc5b5c5fdafa262`. The original workspace log records six failures:
the new cached-read/FTS journey refused a read whose database name was absent;
the existing native E1, canonical first-claim and direct native-chain journeys
refused a still-busy SQLite statement; and two existing S1 journeys stopped
without the chosen grant their later assertions require. The separate public
S1 capacity journey passed. The unit journey stopped at its first refusal, so
that run does not prove its later windows executed.

The source repair changes only the namespace preflight and the timing of the
post-evaluation busy check described above. It preserves all nine windows,
their assertions, both capacity ceilings, the one evaluation, unconditional
rollback, complete-read/absence conditions, schema checks and physical-write
fingerprints. No new fallback or execution permission is introduced.

The cause was checked against the pinned SQLite 3.46.0 primary source:
[`select.c`](https://github.com/sqlite/sqlite/blob/bebe2d8be8acfd02592c4972f4ba32c3b4e4a33f/src/select.c)
passes the FROM item's optional database qualifier for a no-column read;
[`fts5RollbackToMethod`](https://github.com/sqlite/sqlite/blob/bebe2d8be8acfd02592c4972f4ba32c3b4e4a33f/ext/fts5/fts5_main.c)
calls storage rollback, and
[`sqlite3Fts5IndexRollback`](https://github.com/sqlite/sqlite/blob/bebe2d8be8acfd02592c4972f4ba32c3b4e4a33f/ext/fts5/fts5_index.c)
closes the cached blob reader. Finite C-API diagnostics on the available
SQLite 3.46.1 library observed the same FTS reader closing at savepoint rollback
and the ambiguity between main and temp unqualified no-column reads. A held
application SELECT cursor and read Blob remained busy after rollback/release;
a held write Blob prevented RELEASE with SQLITE_BUSY, which remains a storage
failure in capture. A completed read-only FTS query had no remaining busy
statement. These diagnostics are not the pinned Rust journey. The repaired
source still requires the next real CI run; the six original failures remain
part of the evidence.

## S1 two-limit original-plan admission (2026-11-21 candidate)

This candidate changes the refusal behavior described in the 2026-11-17
implementation section above. On protected base `2dd1b4c0`, both exact capacity
refusals returned an error; the existing public S1 consumer proved bounded
complete-read ranges and a serialized-image refusal, but no ordinary fallback
COMMIT and no retained-row refusal. Earlier historical fallback results do not
prove that base's behavior.

Only an exact typed 128 MiB retained-row or 64 MiB serialized-image refusal
returned by canonical preparation may now proceed to ordinary ordered
admission of the original decision. Capture has rolled back and its preparation
transaction has not committed. Ordinary admission still checks live authority,
seals the authenticated predecessor and original action, orders that action
through the attached provider, and checks the original head/step/task conditions
inside its final SQL transaction. The local admission receipt and subject
changes share the successful COMMIT. The capacity journal explicitly does not
assert admission or COMMIT success. Native execution has no new fallback.

An existing prepared action returns its frozen plan before capture; its
admission/application errors return directly. Malformed objects, storage errors
and unrelated conflicts do not match the two exact capacity markers. Chosen
actions continue through the original recovery protocol; this change does not
replace their frozen results with another plan.

Two discriminating regressions reuse the existing authority and recorded-result
fixtures: `canonical_s1_retained_row_capacity_uses_original_ordinary_commit`
uses a real 129 MiB BLOB preimage, while
`canonical_s1_serialized_image_capacity_uses_original_ordinary_commit` uses
12 MiB NUL text whose JSON escaping reaches the image limit while its retained
cells fit the row limit. Each also loses the ordinary chosen reply and requires
an honest error, unchanged source rows and no local receipt before recovering
the same chosen original plan. Both inspect the precise capacity journal,
absence of durable canonical preparation, original S0/result/decision/successor,
actual local receipt and commit-log entry, exactly one trigger mutation, reopen
and idempotent recovery. The bounded-range portion of the existing S1 consumer
is retained. These candidate regressions have not been executed here.
