# CI failure for source 83bf3914

The actual [CI run 43944367545](https://github.com/cyjin-yl/shenko/actions/runs/43944367545)
completed with **failure** on 2026-11-16 at 20:12:30 UTC. The Rust job reported
six failed tests in three targets. The other five jobs succeeded. This archive
preserves that result and its available original evidence; subsequent repairs
must be verified by a later run.

## Source and independent baseline

| Identity | Exact value |
| --- | --- |
| Public head | `83bf391440adbe3e153b036ba8fd1ecfe5867202` |
| Actual PR checkout | `2d275430b103fb0e13316ba51cc5b5c5fdafa262` |
| Public and checkout tree | `d695242eb0005a1d06256aab91e9e649f1cb8a86` |
| Independent Clippy baseline | `84ae19404f2a17d259362c99091db408b7262e14` |
| Run started | `2026-11-16T19:55:24Z` |
| Final run update / Rust completion | `2026-11-16T20:12:30Z` |

The original [checkout response](actual-checkout.json),
[run response](run-final.json), [job response](jobs-final.json), and
[Clippy base selection](clippy-base.json) retain the source bindings.
The PR checkout's parents are the independent baseline and the public head.
The kernel job's complete original log also records the actual checkout.

| Job | Job ID | Actual conclusion | Completed (UTC) |
| --- | --- | --- | --- |
| docs-migration | `133569962336` | success | 19:56:11 |
| web | `133569962655` | success | 19:57:38 |
| rust | `133569962749` | **failure** | 20:12:30 |
| docs | `133569962811` | success | 19:56:06 |
| clippy-ratchet | `133569962820` | success | 20:01:20 |
| kernel-boundary | `133569962854` | success | 19:56:09 |

The [initial same-publication listing](same-publication-runs-initial.json)
is a historical observation. It includes cancelled duplicate `43944366253`
and this run while it was still in progress. It is not a final listing of
every later workflow on the same head. Final status comes from the final run
and job responses above. No workflow was rerun to make this archive.

## Six failures retained without changing their assertions

The complete [workspace test output](workspace-test.log.gz) and complete
[Rust job output](rust-job-133569962749.log.gz) are retained losslessly.
[Failure excerpts](failure-excerpts.json) preserve the six stdout sections
verbatim with their one-based positions in the workspace log.

| Target | Test | Original error |
| --- | --- | --- |
| `shenko --test native_evidence_delivery` | `original_native_e1_arrives_over_authenticated_ssh_without_t2_or_reexecution` | `conflict: canonical Store transition: capture ended with an active SQLite statement` |
| `shenko-store --lib` | `control_transition::tests::capture_observes_cached_sql_and_refuses_unrepresented_state` | `conflict: canonical Store transition: transition accessed a non-main database` |
| `shenko-store --test store_roundtrip` | `canonical_first_claim_preserves_s1_birth_and_never_reissues_a_recovered_worker` | `conflict: canonical Store transition: capture ended with an active SQLite statement` |
| `shenko-store --test store_roundtrip` | `canonical_direct_native_chain_retains_original_e1_and_only_one_effect` | `conflict: canonical Store transition: capture ended with an active SQLite statement` |
| `shenko-store --test store_roundtrip` | `canonical_s1_conflicting_readset_and_partial_sql_fault_never_publish_a_receipt` | `chosen S1` |
| `shenko-store --test store_roundtrip` | `canonical_s1_recovers_identical_results_on_a_distinct_ledger_without_dispatch` | `retained chosen grant` |

The original summaries contain **1,075 passed, 6 failed, 11 ignored, and
88 filtered tests across 152 summaries**. These are observations from this
run, not a project completion denominator. Ignored and filtered tests are
not passing evidence. [The count derivation](workspace-counts.json) and
[archive validation](archive-validation.json) retain the totals separately
from the original logs.

This same run passed the existing three-process SSH star journey in 106.07 s,
the real SSH WASI authentication/exec/SFTP/refusal journey in 264.90 s, and
the public Start pause/reopen/resume journey in 2.28 s. It also passed
`the_capture_budget_refusal_is_recognizable_and_typed` and
`canonical_s1_capacity_is_local_to_captured_rows_and_retains_the_decision`.
Those results do not erase the failed capture test or establish the full
capacity increment's acceptance.

## Native E1: partial original evidence, failed export

The native E1 journey failed after 0.41 s. Its seven original evidence files
are retained under [native-e1](native-e1), byte-for-byte from the Rust ZIP.
They include the original exporter, exporter stdout/stderr, process and
cleanup records, and the nested `000-native-e1-originals.zip`.

The original cleanup record reports `clean=true`, `joined=true`, and
`originals_retained=false`. The original export process exited with status 1;
the cleanup record preserves that export error. Its nested archive has 15
members, including selected original source and receiver table values and
four referenced CAS objects from each. Its manifest reports no missing
references within those two retained stores. The `parked` Store had not been
created before the failure, and the physical-effect witness file had not
been created before the failure.

This is an incomplete export with useful original data. Absence of the
witness file does not establish zero external effects or completion of the
nine observation windows. It also does not make the failed exporter a
successful full Store backup. The original [export manifest](native-e1-export-manifest.json)
and [nested archive verification](native-e1-export-verification.json)
preserve those limits.

## Read-only investigation of the capture failures

[capture-readonly-review](capture-readonly-review/README.md) preserves the
exact source files, scripts, stdout/stderr, commands, and inventories from
the bounded investigation of this source. The original first diagnostic
failures remain present; later observations do not overwrite them. The
directory's initial 21-file inventory is preserved as an earlier snapshot,
and its final inventory covers the additional rollback/handle observations.
The outer [file index](file-index.json) hashes every copied diagnostic file.

The local successful C API observations used the installed system SQLite
**3.46.1**, not the project's pinned **3.46.0** binary. The retained official
3.46.0 source and rusqlite 0.32.1 source provide correspondence for the
observations, not proof that a repair passed the pinned Rust tests.

The first successful observation recorded an internal FTS reader remaining
busy after its caller statement finished and closing during rollback. It
also demonstrated that an unqualified no-column READ can report a NULL
database for either a main table or a TEMP shadow. The additional bounded
observation retained application row cursors and read BLOB handles as busy
after rollback, and observed `SQLITE_BUSY` on RELEASE with a write BLOB
handle. These distinguish the proposed guard placement from simply ignoring
busy statements or treating every NULL database as `main`. Any repaired
product behavior still requires the subsequent real CI result.

## Original artifacts and collection limits

All four original ZIP files are retained. Every member was read, checked
against its ZIP CRC, and hashed; the saved verification files record the
member paths, sizes, and SHA-256 values.

| Artifact | GitHub artifact ID | Bytes | Members | SHA-256 |
| --- | --- | ---: | ---: | --- |
| [Rust](rust-original.zip) | `14263884485` | 8,101,078 | 1,785 | `31a81cdb7e40d83b2aab95108c75abf79047bb0df5f7f8d859c362270607fea0` |
| [Clippy](clippy-original.zip) | `14263214702` | 2,731,848 | 18 | `8c2a1274e3ad11a8dc83041d21add7c140831d06a1f78044d7dd3c71604b9533` |
| [Kernel](kernel-original.zip) | `14264666777` | 232 | 1 | `0f52504475f18fac7036e73009574aefce392982f804144c99fa551eff01dfde` |
| [Web](web-original.zip) | `14263343510` | 2,287,324 | 49 | `a8cdf6f88139ebe0e9df25f33e51d0760d89a11f8a7e5528df4d9ad61f98c209` |

The complete Rust, docs, docs-migration, web, and kernel job logs are saved
as `.log.gz` files with exact decompressed byte hashes. Workspace check and
test logs are also saved losslessly. The Rust job log has 476,740 original
UTF-8 bytes. Original line endings and trailing whitespace were preserved.

The complete Clippy job's outer log could not be collected: two connector
attempts returned `Transport closed`; the CLI had no authenticated session;
one unauthenticated read-only public fallback returned HTTP 403. Those
collection observations are retained in the `clippy-log-read-failure*`,
`cli-fallback-observation`, and `clippy-public-log-fallback` JSON files.
No further retry or replacement workflow was used. The complete 18-member
Clippy ZIP is available, including its base and candidate compiler streams,
base selection, and [ratchet report](clippy-ratchet.json) with `errors=[]`.
The unavailable outer log remains an explicit evidence limit.

## Archive integrity and scope

[file-index.json](file-index.json) identifies retained original files,
lossless log compression, and labelled derivations. The staged and committed
Git blobs are checked for the listed byte size, SHA-256, and Git blob SHA-1;
the [Git verification record](git-index-verification.json) records that
check. Archive validation does not execute product source or replace CI.

The full staged whitespace check exits 2 for 575 warnings in eight retained
upstream SQLite source files. Their original bytes were preserved. The
separate check of this archive's authored README and validation metadata
exits 0; the verification record names the affected original files.

This evidence belongs to the exact `83bf3914` source tree. It does not test
the later seat lifecycle or successor changes, a later capture repair, or a
new Node build. It supplies no new model run, 27B quality evidence, complete
S2 coding evidence, or long-duration deployment acceptance. No product
source, assertions, CI baseline, or root evidence index was changed by this
archive commit.
