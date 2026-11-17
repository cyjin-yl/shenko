# PR #329 original CI observations

Run `44104414754`, requested head `c59feb9ee6fba4aa19525d2af38e60b82a1728c8`, terminal
`failure`. Actual Rust checkout `3fefdcd8e8105737c8a9e0acccbea0e1d2a61c94`, tree
`5269ada29f41b9483d71351ea1f295f8bb6f95da`. The original checkout log and Git API response bind
these separately from the current PR metadata.

The complete original Rust log has 153 summaries:
1093 passed / 11 failed /
11 ignored / 92 filtered.
These numbers are not a product-completion denominator. Compile-all-targets and
the pinned SSH WASI guest build succeeded. Kernel, docs, migration, web and the
stacked Clippy ratchet succeeded, while workspace tests failed. A stacked
ratchet does not erase the inherited #327 lint debt.

Original run/jobs/artifact API bodies, the entire decoded Rust log and exact
checkout Git response are retained unchanged. The three smaller original ZIPs
are retained with API size/SHA-256 and all-member CRC observations. All four
Clippy compiler streams completed. No unavailable outer job log is claimed.

The large Rust artifact `14338588409` is 974026444
bytes, SHA-256 `685280ce165a96d8d1a4efb4d8d38c093eacf232f9c455ef343d3599fd742830`. The entire ZIP was downloaded,
API-size/SHA verified, and all 5234 members passed CRC
(4944492869 total uncompressed bytes), without executing contents.
It remains at `/tank/shared/projects/shenko/build/ci-pr329-44104414754-20261117/rust-evidence.original.zip`
and the original GitHub artifact. It is **not** included as a Git blob here.

Selected originals preserve the actual membership-model-eligibility failure,
bootstrap identity conflict and native-E1 nested archive. The initial archive
attempt used a nonexistent member path and stopped. The actual member index
was then read and its unique sync-members-add stderr selected; no member was
invented or renamed inside the original ZIP.

That nested archive contains selected rows and CAS, not a complete SQLite
backup. The six exported tables compared against frozen changed-key preimages
matched; nine other required tables and full-range BLAKE3 checks were unavailable
from that selected export. `range_digests_not_recomputed` remains true. This
does not establish which canonical precondition failed and does not authorize
a weaker reader or reconstruction of missing bytes.

Both original rollback-migration tests and the new data-copy physical exclusion
journey passed; the existing activation journey with copied/aliased runtime
refusals also passed. The other eleven failed tests remain failed.

All original failures and candidate identities remain distinct. No product
tests were rerun to collect these archives. No full S1–S5, real model, actual
coding-seat, physical-fault or 7/30-day success is inferred. Derived files
identify their method and evidence limits; SHA-256 integrity is not runtime
qualification.
