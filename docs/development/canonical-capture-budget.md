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

Native claim/arm/dispatch/T2 retain their existing typed witness versions:
their independent validators use full read rows to verify operation absence and
unfinished-child ranges. Write-only tables now retain actual touched keys from
the start. Direct first-claim, claim-proven T1 and childless E1-proven T2 already
have a separately validated single-task witness; capture now constructs that
same witness before copying any historical task rows. The exact target shape
is checked again, and the existing ancestry, claim-birth and E1 validators still
run before admission. No full read range is replaced with an unproven digest.
The row-engine journey retains a 129 MiB write-only history and a 66 MiB unrelated
task while capturing and applying the small native-v1 delta.

### Remaining issue #316 boundaries

The native full witnesses remain history-dependent at these concrete paths:

| Path | Still retained complete read dependencies | Consequence |
| --- | --- | --- |
| `claim_transition::evaluate_claim` | `subject_steps` (the current successor and creator are point reads) and `operations` (original-operation absence) | Full before/after images still share the 128 MiB capture limit; the sealed native transition is limited to 64 MiB. Historical steps/operations can refuse a small new claim. |
| `arming_transition::evaluate_arm` | `operations` (ID or task/logical-key absence), `attempts` (ID or operation absence), `subject_intents` (publication absence), and subject reads | The independent `validate_absence` examines these complete witnesses. Ordinary/non-direct ancestry still retains the `tasks` table. |
| `dispatch_transition::evaluate_dispatch` and ordinary `native_transition::evaluate_native_t2` | Full `tasks`/subject reads, named operation/attempt and (T2) evidence tables | Dispatch does not have the direct-chain narrow proof. Ordinary T2 must prove its unfinished-child range; retained unrelated rows still affect capacity and conflicts. Direct E1-proven T2 narrows only `tasks`. |

All canonical actions also call
`control_predecessor::prepare_control_predecessor_inner`, which takes a full
typed cognition snapshot, validates its mandatory-fact closure and repaginates
the immutable control tree. CAS reuse avoids duplicating unchanged objects but
does not avoid reading, parsing and validating that snapshot. The corresponding
inspector reads all those rows again to verify ancestry/child absence. This is
an additional CPU, I/O and transient-memory cost outside the row capture budget.

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
