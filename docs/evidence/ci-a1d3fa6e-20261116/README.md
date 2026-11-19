# CI a1d3fa6e: compilation stopped before native journeys

This archive records [CI run 43952544325](https://github.com/cyjin-yl/shenko/actions/runs/43952544325), attempt 1, exactly as observed. The run ended with **failure** at 2026-11-16 20:48:19 UTC. No rerun, remote ref change or new model execution was performed during this collection.

## Source identity

- Published PR head: `a1d3fa6ed41f9ba233aad5e5b3e7dfeaad281b9f`.
- Actual runner checkout: `8636d7e3470ae205fae2b22e7d45b9833eb663e6`, recorded in the complete Rust, kernel and migration job logs.
- Actual checkout tree: `9756ad60f4820a105d08872dee2d891360789e2c`, independently read through GitData; it equals the published head's tree.
- Actual checkout parents: `84ae19404f2a17d259362c99091db408b7262e14` and the published head.
- Independent boundary/Clippy baseline: `84ae19404f2a17d259362c99091db408b7262e14`, retained in both original artifacts' `base.json`.
- API envelopes are the complete tool-returned responses, serialized as JSON. Decoded job logs preserve the returned UTF-8 text, including its BOM. Gzip files are lossless copies with deterministic gzip timestamps; original ZIP bytes are unchanged.

## Actual outcomes

| Job | Job ID | Actual conclusion |
| --- | --- | --- |
| Rust | 133598602591 | Failure during the all-target lockfile check |
| Clippy ratchet | 133598602568 | Failure during candidate all-target compilation |
| Documents | 133598602603 | Success |
| Migration conservation | 133598602225 | Success |
| Kernel boundary | 133598602491 | Success |
| Web | 133598602604 | Success |

The Rust command was `cargo check --workspace --all-targets --features shenko/compat-poppy --locked -j2`. Its only reported compiler error was `E0382`: `native_evidence_delivery.rs:883` moved the original `advanced: OperationRecord` into `serde_json::to_value`, while line 924 later borrowed it. The complete decoded Rust job log is 159,070 bytes, SHA-256 `bf3c8dec28717851a64e90e4ff306b21402a6fb281baff9de739de33b97be564`. The exact diagnostic is at decoded log lines 1823–1841.

The pinned SSH guest build and the workspace test step were both **skipped**. Thus this run supplies no execution result for the capture repair, the successor E1 seven-window increment, the existing first-E1 nine-window journey, GC refusal windows, component processes, seat journeys, or any other workspace test. The Rust artifact contains only `workspace-check.log`; no E1 ledger/CAS export was produced by this run.

The Clippy artifact retains all four original JSONL compiler streams and their stderr logs. Both baseline streams and candidate production have `build-finished: true`; candidate all-targets has `build-finished: false` and the same `E0382` at JSONL line 1632. No completed ratchet receipt was produced. Separately, read-only comparison of the retained production streams identifies one real new `shenko / clippy::large_enum_variant` warning at `native_evidence_sync.rs:21` (candidate production JSONL line 1439). The interrupted all-target stream contains two occurrences of that warning; its incomplete counts cannot establish a completed all-target comparison.

`derive_clippy_counts.py` and `clippy-derived-counts.json` retain that diagnostic reconstruction. The script does not invoke Cargo, evaluate the source-policy gate or claim a ratchet pass. To rerun it, extract `originals/clippy-evidence.original.zip` into an adjacent `clippy-artifact/` directory first.

## Original artifacts

Every ZIP below was independently downloaded, SHA-256 checked against the API-reported digest, and CRC-checked for all members. The individual member hashes are in `artifact-sources/`.

| Artifact | ID | Bytes | SHA-256 |
| --- | --- | ---: | --- |
| Rust evidence | 14269190805 | 9,031 | `25cd6739d27a863f7bed011e3fac38bab52e9336cd324b66b36e27855987a5ba` |
| Clippy evidence | 14271165164 | 1,917,530 | `8187d001ae8e31ceca71cb1a2de54cac21cd4f10389af633a7e21a72f4a3098a` |
| Kernel boundary | 14267913615 | 231 | `ec5be14b1cac12e2c7342f6506f2e4c6fe0889dd60916dd451c760fce614caf0` |
| Web evidence | 14267893746 | 2,287,308 | `d52d9c33da806660e6cc5d166cb000a472c2f7ff9e9bd8c507213dcba34f9b6c` |

Complete decoded job logs are retained for Rust, documents, migration conservation, kernel boundary and web. The Clippy job-log connector returned **Transport closed**; its original failure response is in `api/clippy-log.tool.json.gz`. The complete original Clippy compiler streams remain available inside the verified artifact. This archive does not claim to contain the unavailable Clippy job wrapper log.

Artifact source records retain GitHub source URLs, artifact IDs and the tool's file IDs. Temporary signed download URLs are not published.

## Separate corrective candidates

- `3c998cea2c141a006b44d26ce5464daf910a2461`, tree `545ab176eb0c119818798407f820cd67077340fc`: borrows `advanced` at its first serialization. The source file before this edit is byte-identical to the tested head (Git blob `11060545535865422e9cea5edff0accad6ce3ea1`).
- `8689295761ec02e480167155d0ab0956e627fff3`, tree `47ed2f79b31c8db56626d668c9c59816c56ee7d4`: boxes the large `Manifest.page` variant, with corresponding construction and immediate unboxing at intake. Serde tags, page object shape, scope, cursor validation and assertions are unchanged. The source file before this edit is byte-identical to the tested head (Git blob `79df052a0fcbf63bc30652c74bd5ba9ccfd87ca7`).

Both changes have clean local commits and `git diff --check` exit 0. They add **zero** scenarios and do not change lint policy, baselines or assertions. No local Rust compilation or product journey was available or attempted for either fix. Their integrated candidate still requires real CI. Neither fix retroactively changes this run's failures or skipped tests.

The existing successor increment remains seven distinct windows; this run did not execute them. Store-aware GC, cross-process writer exclusion, runtime GC assembly and the wider ship/work completion contracts remain outside these two corrections.

