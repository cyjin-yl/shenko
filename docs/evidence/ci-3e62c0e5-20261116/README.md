# CI 3e62: Rust passed; Clippy failed; successful-test originals have a retention gap

[Run 43968540714](https://github.com/cyjin-yl/shenko/actions/runs/43968540714) completed with **failure at 2026-11-16 22:38:16 UTC**. The Rust job passed, including the previously failing native E1 successor journey. Clippy still failed. This archive preserves that actual result and its limits; it adds no behavioral scenario and does not rerun product work.

## Source and actual execution

The requested head was `3e62c0e517a246872101cd1e61a694b2156bc919`. The Rust checkout log records merge commit `1c01c95a4915856312746b5c7ef404c1d7a84cec`, with parents baseline `84ae19404f2a17d259362c99091db408b7262e14` and the requested head. Independent Git Data responses bind both checkout and head to the same tree, `1ccf8f0bee35915c9f6c4d886b5a7bf54050d694`.

The [run](run.api.json), [jobs and step timestamps](jobs.api.json), [actual checkout](actual-checkout.api.json), [requested head](requested-head.api.json), [artifact metadata](artifacts.api.json), and complete decoded job logs retain the original source and execution context. The workflow used the project's declared devcontainer on a hosted Ubuntu runner. No local Rust execution is claimed. Later local source changes are outside this run's evidence.

| Job | Observed result |
| --- | --- |
| docs | Success; the original log records `observe_longrun_test: 17 independent scenarios passed` |
| docs-migration | Success |
| kernel-boundary | Success |
| web | Success; original rebuild comparison and streams retained |
| rust | All workspace targets compiled, pinned SSH WASI guest built, workspace tests passed |
| clippy-ratchet | Four complete compiler streams; comparison failed |

## Rust result and native E1 originals

The 152 original [workspace test summaries](workspace-test.log) aggregate to **1,094 passed, 0 failed, 11 ignored, 0 measured and 90 filtered out**. [The derived summary](test-summaries.derived.json) records every contributing line and the selected test results. Counts are runner observations, not a requirement-completion percentage. The full [Rust job log](rust.job.original.log) includes setup, compilation, the guest build, tests and cleanup.

`original_native_e1_arrives_over_authenticated_ssh_without_t2_or_reexecution` passed on this tree. The previous run's nonexistent `operations.task_id` query had been replaced with the actual JSON field predicate before this run. Its old failure remains in [the earlier archive](../ci-10d4a325-20261116/README.md).

The original [native E1 observations](native-e1/observations.jsonl), [transport record](native-e1/transport.json), command streams, [cleanup](native-e1/cleanup.json) and [nested ledger/CAS archive](native-e1/020-native-e1-originals.zip) retain what happened:

- The physical output is exactly two `effect` lines. The observations record two physical effects and zero chosen T2 executions; learning the original facts did not repeat those effects.
- The initial delivery applied two original facts and a replay recognized both as already applied. The successor group records seven distinct windows.
- A later definite fact produced a separate resolved reconciliation case. The original `acknowledged_unknown` row remains present; its historical state was not overwritten into a claimed success.
- An out-of-order successor remained a reconciliation debt until the ordered predecessor was available. A competing successor remained a debt. The later local receipt has its own original raw reference, and replay after that receipt retains the malformed-delivery debt.
- Cleanup reports `clean=true`, `joined=true`, `originals_retained=true`. These observations concern this finite fixture, not a persistent deployment.

The nested ZIP contains 440 members. Its 438 original inventory entries match their recorded SHA-256 and byte lengths; all members pass CRC. Independent inspection also checked 1,247 original text-value hashes. The [derived validation](native-e1-originals-validation.derived.json) preserves the exact original manifest, row counts, branch heads, reconciliation rows and seal debts. Its exporter intentionally retains selected rows and a scoped CAS closure, **not a full SQLite backup**. One missing source transition object is explicitly recorded, with its original seal debt; no replacement object was sealed. Node databases, credential-bearing fields and excluded namespaces remain outside this export's stated scope. BLAKE3 was not recomputed by this local Python inspection because that module is unavailable; CRC, lengths, SHA-256 and the original text hashes were checked.

## What passing S2 build and PTY tests do and do not retain

The successful test results include the actual Cargo rebuild/error, ambiguous-target, compiler-overflow, prepare/reopen, and PTY write/flush/cancel/channel-loss fixtures. The Cargo fixtures invoke the actual Cargo executable on temporary Rust 2024 projects. They do not use a fabricated compiler result. The PTY fixture exercises its original native SSH protocol peer and five existing windows; it is not an OpenSSH OS PTY or a complete coding-seat journey.

There is a material **original-receipt retention gap** in this candidate:

- Libtest captures successful PTY `eprintln!` output. The workspace log reports the successful test but does not contain its complete per-write observations, inputs, peer events and cleanup receipts.
- The Cargo fixtures check original streams and CAS reopen behavior, then delete their temporary workspaces and CAS in `Drop`. The prepare/reopen fixture also deletes its temporary installation after closing its Store. Those raw originals are absent from this run's artifact.

Therefore this archive does not claim independent access to those missing successful-test originals. A subsequent evidence-export change must observe and preserve the next actual execution, including failures. It cannot fill this run's gap retrospectively. The new sealed prepared-artifact check, native command cleanup changes, Store retained-object reader and host-smoke preflight work were not in this source tree and receive no pass from it.

## Clippy failure

The original [ratchet receipt](clippy-ratchet.json), [baseline binding](clippy-base.json), and [derived stream summary](clippy-derived-summary.json) report four complete compiler streams, each ending in `build-finished: true`. The pinned toolchain is recorded as `1.98.1`. There are nine comparison errors:

- Seven unchanged upstream attributes in the two verbatim rusqlite sources under the public `ci-83bf3914-20261116/capture-readonly-review/sources` archive were classified as new suppressions by the all-tracked-Rust-file source scan.
- `shenko-exec-ssh` gained two `unused_variables` diagnostics in production and five in the all-target comparison. The new bindings occur in the PTY `tokio::select!` arms and its fixture. A later source-only alpha-renaming preserves all uses, ordering, branch logic and messages; that change is outside this run and needs its own compiler result.

The public original source bytes, paths and attributes remain unchanged. No ignore, exception, weaker baseline, altered compiler flags or removed archive makes this failure green. Correctly accounting for actual source inputs remains separate work; package paths or dep-info alone are not presented as a complete input closure.

The full Clippy job-log request returned `Transport closed`; its original response is retained in `connector-originals/initial-fetch-1.tool.json.gz`. All four complete raw compiler streams remain available in the unchanged Clippy ZIP. The [local archive transport failure](archive-transport-failure.json) is also retained: a subsequent read-only recovery and log fetch obtained the missing logs without rerunning the workflow.

## Integrity and retained files

All four original ZIP byte counts and SHA-256 digests match their API metadata. All **1,904 members** passed CRC. [The derived inventory](artifact-inventory.derived.json) includes each member's byte count, CRC and SHA-256.

| Original ZIP | Bytes | Members | SHA-256 |
| --- | ---: | ---: | --- |
| [Rust](rust-evidence.original.zip) | 8,855,832 | 1,836 | `871cdc7b29fbd6326932575d783172ec7624b9880b56188a7388278a9d7aa292` |
| [Clippy](clippy-ratchet-evidence.original.zip) | 2,735,684 | 18 | `f6de891ffe2529c1d8de780939ed221bd15d5c5a24dbac33c11f38a04d75f986` |
| [Web](web-rebuild-evidence.original.zip) | 2,287,316 | 49 | `2b46ede87beb85e6b4092a7cf797f736dc7e53beab5e0aadc3f5fa409d04c0ff` |
| [Kernel boundary](kernel-boundary-evidence.original.zip) | 232 | 1 | `d6cfb596ea8fe4160ca7200d36c040fcbcc80e014ef18125fa07cb9c5dc6c0e9` |

The nested native ZIP is 780,804 bytes, SHA-256 `3ef6d9d1f1ff202457a2a976b29684e761f95d4f326fc3b3a33321b5a3f72c41`. All 52 original native E1 files and five complete decoded job logs are preserved, in addition to the unchanged outer ZIPs. [The source mapping](source-blobs.json) binds relevant fixture, implementation, gate and workflow bytes to the exact candidate; `.blob` copies retain those bytes without representing a new source location. Original connector responses are losslessly gzip-compressed. No original log whitespace was normalized.

[The archive index](archive-index.json) hashes every retained file except itself. This result does not complete the real-model single-node loop, the full S2 self-development journey, installed Node/Bun capability, all multi-node transitions, or any 7/30-day observation.
