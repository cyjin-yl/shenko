# Local history input for Obelisk acceptance

This module supplies explicit local input and digest-bound source snapshots to
`obelisk_acceptance.rs`. It uses the existing external-history archive and
provider-root types. The runner must select it explicitly; the synthetic
scenario remains the default. Invoke the same runner with
`--local-scenario /absolute/path/to/owner-input.json`. The input is captured
once before broker startup or subject inference, and the original snapshot
binding and budgets enter the run and both arm bindings. Every run requires
an explicitly selected absolute private `SHENKO_OBELISK_ACCEPTANCE_ROOT`;
select a Life evidence parent in the actual runtime namespace. There is no
repository `build/` evidence fallback.

The governing contract is [external history](../../../../docs/specs/external-history.md)
from the repository root. Complete original JSON/JSONL bytes remain authoritative;
capturing them supplies evidence and never makes an imported role a current
instruction. Source capture is not evidence that the subject retrieved a source,
that an answer is correct, or that M08 is complete.

## Explicit input

`FrozenScenario::capture(input, fresh_destination)` reads a bounded JSON file
with these fields. Unknown fields are errors. No field has an implicit home
directory, discovery rule or source-file glob.

| Field | Required value |
| --- | --- |
| `schema_version` | Integer `1`. |
| `scenario` | Nonempty scenario identity, at most 4096 UTF-8 bytes. |
| `question`, `rubric` | Owner-authored, nonempty strings, at most 64 KiB each. Exact string bytes get separate BLAKE3 digests. The rubric belongs to the judge, not the subject prompt. |
| `source_instance` | Stable owner-selected source identity, at most 512 UTF-8 bytes; preserved across snapshot moves. |
| `limits.max_files` | Explicit positive file-count budget, at most 64. |
| `limits.max_file_bytes` | Explicit positive per-file byte budget, at most 64 MiB, matching the native archive ceiling. |
| `limits.max_total_bytes` | Explicit positive total byte budget, at most 256 MiB. |
| `roots` | One or more existing `ObeliskSourceRoot` objects: `provider` (`codex`, `omp`, `pi`), unique `root_label` without path separators, and absolute UTF-8 `path`. One root per provider; all roots must have a selected file. |
| `files` | Nonempty array of the exact selected originals described below. |

Each `files` element contains:

| Field | Required value |
| --- | --- |
| `source` | Existing `ExternalHistorySource`: `provider`, `source_instance`, `source_key`, `session_id` (string or null). The first three fields form the logical source identity. |
| `identity_basis` | `native_session` or `configured_root_relative_path`, matching the native source manifest. |
| `relative_path` | Exact slash-separated file path below the selected provider root; no empty, dot, parent, backslash or absolute components. Original names and nesting are retained. |
| `bytes` | Exact positive length of the complete chosen version, within both configured byte budgets. |
| `digest` | Canonical `blake3:` plus 64 lowercase hexadecimal digits for those exact complete bytes. |

For `native_session`, `source.session_id` is the actual provider-qualified
session ID from the native Obelisk manifest; `source_key` is exactly
`session:` followed by that ID. The module requires the provider prefix and does not parse historical bodies to
invent or certify native IDs. The owner declares this mapping, and actual
upstream observations must match it before they can qualify as reader evidence.

For `configured_root_relative_path`, `session_id` is null and `source_key` is
exactly `root:<root_label>/<relative_path>`, as in runtime `source_manifest`.
This is explicitly a locator-based identity. Neither absolute paths, timestamps,
rowids nor newly generated UUIDs become portable source identities. Selecting
the same logical source twice, duplicate locators or overlapping file locators
is an error. Equal bytes from distinct logical sources remain separate entries.

The original input file is limited to 1 MiB. All declarations, identity shapes,
counts, expected sizes and total budgets are admitted before opening an original.
Only explicitly named files are opened; provider roots are never enumerated.
Capture requires an existing destination parent and creates a fresh private
destination. Root aliases are resolved once; selected file symlinks are refused.
The snapshot must be outside every selected provider root. Failure preserves
partial output without a completion marker and never substitutes an empty
scenario or a truncated source.

## Retained layout and integrity

Every completed snapshot contains:

```text
selection.json
snapshot.json
originals/
  codex/<exact relative path, including sessions/ or archived_sessions/>
  omp/<exact path below the original OMP sessions root>
  pi/<exact path below the original Pi sessions root>
```

Only configured providers and selected paths exist under `originals/`.
`selection.json` is the exact owner-selected JSON, including private original
root locators. `snapshot.json` carries the portable typed inventory, complete
lengths/digests, file count, total bytes, question/rubric and their digests,
selection digest, limits and explicit coverage. It is synchronized and published
last. Its exact-byte digest must enter the run binding before inference.

`corpus_digest` binds the versioned format, sorted provider labels and complete
selected source/version inventory. It excludes original absolute paths and JSON
formatting. The snapshot digest additionally binds the exact selection and all
scenario metadata. Moving a snapshot changes no source identity or corpus
digest. Changing the question or rubric changes the snapshot binding while
keeping an unchanged corpus identity.

Capture follows the complete-file boundary in runtime `archive_file`: two full
reads use one descriptor, verify size and modification observation, and compare
every byte. The second pass compares fixed-size chunks without allocating a
second full file. Exact expected length/digest and the final locator are checked.
Missing, oversized, changed, replaced or nonregular sources fail explicitly.
This observes individual file versions; it does not attest an atomic cross-file
frontier or protect against an adversarial same-user ancestor swap.

`open(snapshot_root, expected_manifest_digest)` reads only retained metadata and
copied originals. It validates the input binding, all derived metadata and every
original byte, and rejects extra files, unknown directories and symlinks in the
bounded originals tree. It never reads the original input path or provider
roots. An incomplete or corrupt snapshot fails instead of consulting live history.

`mount(fresh_arm_destination)` makes independent regular-file copies with the
same layout and bindings. It does not use hard links, flatten names, rewrite
JSONL, synthesize messages or mount a filesystem. `configuration(template,
fresh_index_home)` replaces every live root with these copied provider roots,
retains the audited executable/source checkout/arguments/limits, and uses the
original source instance and root labels. The index must be fresh and outside
the source roots. Original source paths are never passed to upstream indexing.

Call `validate()` after each arm, including failed arms. It verifies both the
manifest binding and the exact file inventory/bytes. Custom JavaScript retains
host authority; success or normal process cleanup does not attest source integrity.
The owner can use a read-only bind mount as an additional operating constraint;
the file-copy API does not claim such a mount exists.

`archive_into(store, agent_id)` delegates to the shipped `archive_bytes` after
each copied original has passed the same stable-read and expected-digest checks.
Using verified in-memory bytes prevents a second mutable-path reopen between
digest validation and archive admission. No second source parser, importer,
memory store or semantic summarizer is introduced. It retains native parser
status, opaque originals, complete string-leaf projections and existing
subject-bound source-version/record identities. Earlier per-file archives can
remain after a later source fails; the operation does not claim a multi-file
transaction. A failed preparation must never enter subject inference.

## Integration and acceptance

The runner captures exactly once per explicit local run, binds the snapshot
digest/question/rubric/corpus/counts/limits in both arms, and mounts independent
copies of that frozen selection. Both installations retain the shipped archive's
complete originals and authenticated full-record projections for judge grounding.
Only the baseline appends those projections to its raw branch, using a historical
observation role with source/digest/span/native metadata. The candidate can use
the archive's native content readers or its matching copied provider files.
Local mode never calls `write_codex_corpus` or keeps an unselected live root.

The existing `retain_units=0` rollover and raw-history preservation assertion
still protect the baseline's initial context. The selected question alone enters
the subject scenario; the rubric and full source facts stay on the judge path.
Actual first provider wires still need inspection when measuring model behavior.
Original native task/E1/T2 records, the first committed continuation, separate
reader admission, judge receipts and cleanup remain independently visible.

Neither snapshot preparation nor archive import grants reader admission. Native
archived search/read results require selected versions, record identities and
spans; original pages require the selected archive/version, authenticated nonempty
bytes and matching requested ranges. Standard CLI text results require the selected
source instance and a provider-qualified selected session. Raw CLI pages additionally
require a matching message from an original successful text result or a validated
native source manifest at the mounted locator. Metadata listings and manifests
alone, arbitrary scripts, failures, empty results and foreign versions are not reads.
The original and arm snapshot are validated after success and failure. A retained
integrity failure cannot replace the primary runtime or cleanup failure or become
a scored success.

Offline reporting uses the same executable:

```sh
obelisk_acceptance --report-only /absolute/path/to/original-run > /private/life/evidence/reconstructed.json
```

Dispatch occurs before Tokio, provider configuration or capture. Local reporting
opens only the retained snapshot using its original run-binding digest, rechecks
the selected source facts and integrity record, and uses the existing original
installation/arm/preparation identity and exact committed-consumption membership
gate. Raw-page context can come only from that installation's original consumed
results. It never opens a Store or the original input/provider roots, starts a CLI
or model, invents an attempt or replaces an original report. Missing snapshot,
consumption or integrity evidence remains rejected/incomplete. Retained JSON
membership is not cryptographic proof against coordinated rewriting of all files.

The example's seven offline tests include the three existing reader/reconstruction
regressions, the three complete-source regressions, and one integrated local
scenario regression in `tests.rs`. They use unique synthetic fixtures in the system
temporary directory by default. Set `SHENKO_OBELISK_LOCAL_SCENARIO_TEST_ROOT` to
select an owned fixture parent. The integrated case exercises strict CLI selection, both fresh Store/archive
preparations, observed historical roles, replaced provider roots, source/version
reader admission, consumed-result reconstruction with the live originals removed,
and rejection of foreign-only receipts and altered retained inventory. Its retained
JSON is a synthetic fixture, not claimed native dispatcher or model execution.
Run the actual example tests once in the admitted project devcontainer:

```sh
SHENKO_OBELISK_LOCAL_SCENARIO_TEST_ROOT=/private/life/evidence/offline-fixtures \
  cargo test --locked --offline -j2 -p shenko --example obelisk_acceptance -- --nocapture
```

Real-model paired behavior, first-wire isolation and repeated M08 efficacy remain
separate acceptance; these source and offline regressions cannot establish them.
