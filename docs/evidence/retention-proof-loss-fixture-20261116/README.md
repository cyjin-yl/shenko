# Retention audit across the original proof-loss fault

Source: CI [43980018541](https://github.com/cyjin-yl/shenko/actions/runs/43980018541), requested head 41d785ae8f968a3e81cd632223cadb356c44a224, actual Rust checkout dff68d76432881c8d3a99e68f56e1b27678b8641 with tree a237c05a4b1f095949d1996509b9e3bf653af62f.

The run ended in failure at 2026-11-16 23:46:45 UTC. The complete decoded Rust job log contains 153 test summaries: 1,095 passed, 1 failed, 11 ignored, 0 measured and 90 filtered out. Kernel, docs, migration and web succeeded; Clippy failed and its complete diagnostics still require the original archive. The all-target compile and pinned SSH WASI guest build succeeded.

The sole Rust failure was the added retention-completeness assertion in the existing native-E1 journey. That journey deliberately removes the third arm's transition CAS object before testing retained seal debt. A later unconditional complete assertion incorrectly demanded that the removed object exist. The reader correctly returned four unreadable-reference gaps for that original object; no production reader defect is inferred.

## Exact change

Before the unchanged removal, audit the actual source and require completeness, matching ledger/watermark, verified transition bytes, the required reference from the actual arm envelope, and unchanged Store/CAS state.

At the existing late-history audit, require the source to retain the precise fault: incomplete, no omitted gaps, exactly four unreadable-reference gaps, all for the original transition digest. Three must be the original control-materialization, prepared-transition and inbox row carriers; one must be the original arm-envelope object. Require each origin exactly once and ensure the missing digest is not marked verified. The receiver still must be complete.

The original deletion, fault inputs, debt checks, raw first/successor receipts, acknowledgement, replay, CLI, Store/CAS invariance and two physical effects are retained. No missing CAS bytes are manufactured. The existing GC declaration six plus reader three windows remain nine; this correction adds no behavioral scenario and changes no production file.

## Review and runtime boundary

The old file blob is c1bfbdfe7489a052af6db16b822158430a49f70c. The candidate blob is 829d8fe5169241d686b55a858d63e4e3c7793d86. The author review packet contains the exact three old/new replacements and source-backed Object-origin derivation. Root independently applied those replacements to the exact published blob, checked the resulting Git hash, reversed them to the original bytes, checked the single original removal and unchanged late assertion tail, and confirmed the original test-function count.

The local executor became unavailable. These are source comparisons performed through the still-available repository APIs, not Rust execution. The changed candidate requires a new source-bound CI result. The original 41d failure remains a failure.

The complete decoded Rust log and terminal API responses are retained in the adjacent continuation-interruption evidence directory. The original four ZIPs are emitted on GitHub; their complete recovery and verification are being prepared separately. This document does not assert that all failed-fixture originals exist or that a ZIP was locally checked after executor loss.
