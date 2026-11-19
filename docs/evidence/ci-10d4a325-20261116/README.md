# CI 10d4: compilation passed; one native E1 successor test and Clippy failed

This archive retains the actual result of [run 43957583118](https://github.com/cyjin-yl/shenko/actions/runs/43957583118), which completed with **failure at 2026-11-16 21:31:45 UTC**. Later fixes do not change that result. This is evidence retention, with zero new behavioral scenarios.

## Exact source and environment

The requested PR head was `10d4a325560a1054fbde00162e8e7696007c2eab`. The job checked out merge commit `cd4b7cc2b03397ec4657b5037e30e8d0c26553f9`, whose parents were baseline `84ae19404f2a17d259362c99091db408b7262e14` and that PR head. Independent Git Data responses bind both checkout and requested head to tree `86af69f6e0a2c4434c252d570f23a8afa1c5d5ec`.

The original [run](run.api.json), [jobs with step timestamps](jobs.api.json), [checkout](actual-checkout.api.json), [head](requested-head.api.json) and [artifact metadata](artifacts.api.json) are retained alongside their connector responses. The workflow ran in the project's declared devcontainer on an Ubuntu hosted runner. The complete logs preserve its actual tools, commands and environment. This was not a local Rust run.

| Job | Actual outcome |
| --- | --- |
| docs | Success |
| docs-migration | Success |
| kernel-boundary | Success |
| web | Success |
| rust | All workspace targets compiled; the pinned SSH WASI guest built; workspace tests failed |
| clippy-ratchet | Four complete baseline/candidate compiler streams; comparison failed |

## Rust failure and reached boundary

The 152 test summaries in the original [workspace stream](workspace-test.log) aggregate to **1,088 passed, 1 failed, 11 ignored, 90 filtered out**. These are runner summary counts, not requirement coverage or a completion percentage. [The derived counts](test-summaries.derived.json) preserve every contributing summary; [the full Rust job](rust-job.original.log) includes setup, guest build, all tests and cleanup.

The only failing test was `original_native_e1_arrives_over_authenticated_ssh_without_t2_or_reexecution`, after 42.23 seconds. Its original error was:

```text
storage error: no such column: task_id in SELECT (SELECT count(*) FROM operations WHERE task_id=?1),
    (SELECT count(*) FROM attempts WHERE operation_id=?2),
    (SELECT count(*) FROM evidence_receipts WHERE operation_id=?2) at offset 46
```

The fixed schema stores the operation's task identity in `operations.data`; there is no `operations.task_id` column. The successor scope query therefore failed before the later receipt could commit. The subsequent source correction is the single predicate `json_extract(data,'$.task_id')=?1`, consistent with the other operation queries. It changes no schema, count, authority, Unknown rule, test assertion or time budget. Its source author commit is `87a99ae088c0befd232c95a11537236746d7816d`; a later run is needed to test that correction.

The retained [native E1 transport observations](native-e1/transport.json), original command streams and nested [original ledger/CAS ZIP](native-e1/008-native-e1-originals.zip) establish the boundary actually reached. A missing chunk remained pending; the next synchronization reported two reconciliation debts; the subsequent actual synchronization applied two original facts with zero remaining delivery debts. The original physical output was exactly two `effect` lines. The later Unknown-to-definite successor did not commit, so its remaining windows are **not passed**. [Cleanup](native-e1/cleanup.json) reports `clean=true`, `joined=true`, and `originals_retained=true`; those facts do not make the failing journey successful.

## Clippy failure and preserved source data

[The exact ratchet receipt](clippy-ratchet.json) reports two kinds of failure:

- Seven attributes in two unchanged, verbatim upstream rusqlite source files previously archived under `ci-83bf3914-20261116/capture-readonly-review/sources` were classified as new suppression controls by the gate's scan of every tracked `.rs` file. Their original bytes and public paths remain preserved. No exemption, baseline change, renamed public path or removed attribute resolves this archive's failure.
- The new seat error fixture's terminal `anyhow::bail!` expressions introduced two `clippy::diverging_sub_expression` and three `unreachable_code` diagnostics in the all-target scope. A separate zero-scenario source change returns `Err(anyhow::anyhow!(...))` with the same messages. It does not change the production behavior or test assertions and requires its own CI result.

All four compiler streams are complete in the original Clippy ZIP. They retain the immutable baseline, compiler-discovered lint policy, actual command arguments, production and all-target counts. The separate full-job log request returned `Transport closed`; [that original tool failure](clippy-job-log-fetch-failure.tool.json) is retained. A missing wrapper log is not presented as a successful read.

## Original artifacts and integrity

All four downloaded ZIP byte counts and SHA-256 digests match their original API metadata. Every one of their **1,900 members** passed ZIP CRC validation. The [derived member inventory](artifact-inventory.derived.json) records each member's byte count, CRC and SHA-256 without rewriting the archives.

| Original ZIP | Bytes | Members | SHA-256 |
| --- | ---: | ---: | --- |
| [Rust](rust-evidence.original.zip) | 8,826,727 | 1,832 | `51e22c2413cb6797c5559d439667e381b7487c1c86bf88527db96e84dc8ca93b` |
| [Clippy](clippy-ratchet-evidence.original.zip) | 2,736,472 | 18 | `06c8a60b2dcb86460bd6b0c03a69efc51811e2239eebecb251d2d8fe52b6e226` |
| [Web rebuild](web-rebuild-evidence.original.zip) | 2,287,311 | 49 | `dd7071fa7327190fb2b2d0b9137dd782425d1f3a52e5b6f91595b10396fa115f` |
| [Kernel boundary](kernel-boundary-evidence.original.zip) | 232 | 1 | `0c29d1d279f4d14f9c0d19e2ca8124572f6d0c6f895be8001bd8f159716c271e` |

The nested native-originals ZIP is 688,395 bytes, SHA-256 `7fcfb61f6b76d98c0ca9aa093ac9d8f26c9d4d46eb7ff4f9692f6be261988648`; its independent validation and original manifest are in [the derived nested-archive record](native-e1-originals-validation.derived.json). Five complete decoded job logs are retained, together with the unchanged extracted workspace streams and all 27 native E1 members. The [source blob mapping](source-blobs.json) binds the relevant original query, schema, fixture, test, gate and workflow bytes to the exact failed tree.

The [archive index](archive-index.json) hashes every retained file except itself. Derived inventories and summaries are explicitly named; they do not replace originals. No original log whitespace was normalized. This archive does not prove native successor acceptance, a real-model single-node loop, the complete S2 journey, installed Node/Bun capability, or any 7/30-day observation.
