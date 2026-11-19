# Independent native E1: source and metadata evidence

This archive records local compilation, diagnostics and document checks for
source commit `862213d9e351f0fc1816553d6802f35be57ab4e5`, tree
`aac54099ad0df6f7bdb9b4e2bff987d0eb034928`, based on
`bfc5f7e125a30bd86f48904617e97994c85d49a2`. **No actual SSH E1 journey or
runtime test was executed for this candidate.** It is not full workspace CI,
a base/candidate Clippy ratchet result, quorum evidence or ship/work acceptance.

[observation.json](observation.json) records exact command arguments,
environment, exits, source bindings, all nine authored windows and limitations.
[checked-rust-files.json](checked-rust-files.json) records the SHA-256 of every
changed Rust file in the final candidate. Full source remains in Git.
[changed-line-diagnostics.json](changed-line-diagnostics.json) is a derived
index of the final complete Clippy output: zero diagnostics intersect the
changed Rust lines after applying the existing test-only unwrap/expect rule;
all 36 occurrences of those test-only diagnostics remain listed separately.

## Original checks, including failures

All nine surviving command-output files are retained completely in `raw/` as
lossless gzip with `mtime=0`. Every compressed stream was decompressed and
compared byte-for-byte with its original. The observation manifest records
compressed and uncompressed lengths and SHA-256 values. No warning or compiler
output was filtered from these originals.

| Actual check | Observed result | Original output |
|---|---|---|
| Initial Store/Node/CLI library metadata | Exit 101: two CLI `E0308` errors. `local_node_id()` and `peer_route()` returned `Option` values that the initial adapter did not unwrap with an explicit refusal. | [Combined stdout/stderr](raw/native-e1-metadata-initial.log.gz) |
| New CLI journey metadata | Exit 0, 1m16s. New `dead_code`: the shared HTTP `health()` helper was unused by this non-HTTP journey. | [Combined stdout/stderr](raw/native-e1-journey-metadata.log.gz) |
| Existing Store roundtrip after helper extraction | Exit 0, 25.79s. Original journey assertions remain present. | [Combined stdout/stderr](raw/native-e1-store-metadata.log.gz) |
| First scoped Clippy metadata | Exit 0, 1m46s. Moving HTTP health left an unused `ensure` import, reported for the affected test targets. | [Complete JSON stdout](raw/native-e1-clippy.jsonl.gz), [stderr](raw/native-e1-clippy.log.gz) |
| Final scoped Clippy metadata | Exit 0, 2.92s. No remaining diagnostic on a new/changed Rust line, excluding test-only unwrap/expect under the existing CI policy. | [Complete JSON stdout](raw/native-e1-clippy-final.jsonl.gz), [stderr](raw/native-e1-clippy-final.log.gz) |
| Corrected intermediate docs, including migration verification | Exit 0, `errors=[]`. | [Combined stdout/stderr](raw/native-e1-docs.log.gz) |
| Docs after the source commit, including migration verification | Exit 0, `errors=[]`, bound to the source commit above. | [Combined stdout/stderr](raw/native-e1-docs-final.log.gz) |

The final Clippy command checks the related Store, Node, Types and CLI libraries,
the new E1 journey, the moved Store helper's roundtrip target, and the two
existing HTTP journeys whose imports changed. It performs metadata work only.
No Rust source changed after that final run and before the source commit.
Earlier uncommitted intermediate trees were not checkpointed; their original
compiler paths and diagnostics survive, but they must not be relabeled as checks
of the final Git tree.

The HTTP helper was moved unchanged to `support/runtime_health.rs`; both
existing HTTP journeys keep their real calls and assertions. The obsolete
import was removed. There is no added `allow`, changed warning baseline, dummy
health call or removed assertion. The moved Store fixture's historical test
unwrap diagnostics remain in the full JSON outputs; they are not production
unwrap diagnostics under the existing ratchet policy.

### Explicit missing originals

Two earlier docs runs failed while relocating the shared helper's current
source trace. Both redirected to the same `native-e1-docs.log`, and subsequent
runs overwrote their complete original streams. Their exact error lists from
the tool returns are recorded in `observation.json`, with
`raw_stream_available=false`:

- The first run reported the two moved helper symbols missing at their old
  paths.
- An intermediate edit also changed the immutable citation-migration list.
  Verification rejected that change and its current ownership inconsistencies.
  The original migration values and digest were restored; only current owners
  now point to the shared helper.

These diagnostic lists are not reconstructed raw logs. The missing full outputs
remain an evidence gap. No old failure was rerun to manufacture a replacement.

## Nine authored windows, not execution results

The one new journey uses two distinct Store ledgers, an ordinary parked archive
negative, actual pinned OpenSSH and the real CLI/CAS path. Its control grants
are the existing retained-value Store fixtures; they do not establish actual
quorum or process-election behavior. None of the following assertions has been
observed running in this archive.

| Window | Authored assertion |
|---|---|
| First definite E1 | Original receipt/fold survives independent delivery before any chosen T2. |
| First Unknown E1 | Receiver opens its own local case and does not copy the source S1 acknowledgement. |
| Interrupted authenticated chunk exchange | Local pending survives the injected remote command failure and a new CLI process retries it. |
| Missing actual local T1 | Blocked debt remains until the receiver materializes the same original T1; retry grants no execution. |
| Ordinary parked archive | Copied applied markers cannot establish a local application receipt or release parking. |
| Bounded pagination | One-item pages retain a fixed upper watermark and exact continuation. |
| Unsupported codec | Refusal does not become generic admission or successful completion. |
| Replay after a later local observation | Original first E1 does not rewind the newer fold/case, commit again or repeat an effect. |
| Missing derived source seal | Original E1 remains committed with seal debt; source gap and receiver debt remain visible. |

The chunk injection is specifically `exit 75` in the authenticated remote
launcher **before** it execs the actual CLI for one `read_control_object`
request. It is not a physical observation of a chunk reply being lost after
production. The fixture's two file appends must remain two; that effect-count
check is authored, not a measured result here.

## Remaining boundaries

The source and receiver SQL records name E1 CAS closures, but this increment
does not add a complete Store root producer or Store exclusion during GC.
The read-only GC audit also inspected integration source
`4987d098ec981bb9003a5d6a0c12cf69c2892849`, which already contains the newer
PR307 tool-artifact producer absent from this E1 branch's base.
The runtime has no `CasGc` adoption/reclamation assembly. A **complete** registry
declaration including `commit-authority` cannot be covered by the existing
home/COW, WASM-state and tool-artifact producers, so adoption or planning refuses
with `CAS_ROOT_NAMESPACE_UNCOVERED`.

That refusal does not detect an omitted namespace. A tool-only declaration for
a CAS also containing Store facts violates the complete-declaration precondition
and can lose those unlisted facts. Complete Store-aware roots, shared-CAS
assembly and a root-revision/exclusion protocol remain unfinished. No permanent
pin or unused root API was added; no GC acceptance is claimed.

Multiple operations, multiple-E1 source ordering, ordinary-child lifecycle
expansion, model/quota, publication and a new follower background worker are
also outside this increment. The independent source review reported no new
confirmed blocker within the implemented scope; it is not runtime evidence.

## Source-ledger review follow-up

Source `5a3ec71d6df8bf7147c309bbe3bd9fb1e11ff8ff` adds the strict source-ledger
binding in both the wire reader and Store, keeps the historical Gap debt
explicit, and records the actual number of receiver rows written. It extends
the existing two-page assertions without adding a tenth window.
[source-ledger-review.json](source-ledger-review.json) binds this separate
review fix to its complete Clippy stdout/stderr, docs result and checked source
patch. The earlier checks above retain their original source bindings.

The scoped Store/CLI Clippy metadata check exited 0 in 1m07s, with no diagnostic
on the three changed Rust files. Migration-aware docs checking exited 0 with
`errors=[]`. The checked tree is `8e3f46667c7b24213080f46006441dbc9dae2527`,
reconstructible from the recorded parent and complete patch. Only rustfmt line
wrapping followed that check: formatting each checked Rust file was compared
byte-for-byte with the committed file. Both the checked patch and formatting
delta are retained. This remains compilation evidence; the actual SSH E1
journey and Store-aware GC are still unverified or unfinished as described
above.

## Integrated source and original-evidence verification

The subsequent source checkpoint `10c56bdd70c07ecc3022a62ea3a8eaaa16451958`
(tree `b59ad3df936f763e9c07d344ab889bc0dbceffb7`) preserves the actual PR303
base `e5fbf732124ee035f235a5a9f1a20308dad5d062` and main through `00f11098`.
Its six standalone E1 production/fixture files match the reviewed `8a38e43f`
bytes, and its two coordinator identity files match `6f11e91a`.
The [integration records](integration-10c56bdd/files.json) retain the complete
documentation check (`errors=[]`, 365 migrated chunks), exact source binding,
and independent verification of all 43 retained be46 evidence files, 304 runtime
archive members and 18 Clippy ZIP members. These checks establish source and
retention integrity; the next candidate's actual CI still has to execute the
new E1 and revised component journeys. The integration records were added
after these checks without changing Rust source.
