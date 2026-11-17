# PR #330 original CI observations

Run `44110173436`, requested head `79291b136c41517578111f5293354cecd552b64d`, terminal
`failure`. Actual Rust checkout `d85c3ad4bd9cd477d49320e28e9d16e082e3db51`, tree
`659c310f7f739cf584959930140815f200dad667`. The original checkout log and Git API response bind
these separately from the current PR metadata.

The complete original Rust log has 153 summaries:
1094 passed / 10 failed /
11 ignored / 92 filtered.
These numbers are not a product-completion denominator. Compile-all-targets and
the pinned SSH WASI guest build succeeded. Kernel, docs, migration, web and the
stacked Clippy ratchet succeeded, while workspace tests failed. A stacked
ratchet does not erase the inherited #327 lint debt.

Original run/jobs/artifact API bodies, the entire decoded Rust log and exact
checkout Git response are retained unchanged. The three smaller original ZIPs
are retained with API size/SHA-256 and all-member CRC observations. All four
Clippy compiler streams completed. No unavailable outer job log is claimed.

The large Rust artifact `14340979554` (974124934 bytes) remains referenced
by the original API. It has not been downloaded/ZIP-verified by this observer;
no complete native-originals claim is made for this run. The whole Rust job
log is retained and supports test outcomes, not replacement raw receipts.

The SSH/WASI journey passed without a Node-startup source change relative to
#329. Its absence from this run's failed-test list is not credited to the
Broker refactor and does not resolve the previously observed identity race.

All original failures and candidate identities remain distinct. No product
tests were rerun to collect these archives. No full S1–S5, real model, actual
coding-seat, physical-fault or 7/30-day success is inferred. Derived files
identify their method and evidence limits; SHA-256 integrity is not runtime
qualification.
