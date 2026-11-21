# Candidate 4a: one native-chain test passed

Candidate `4a5b88aa58e5f5c61200066fa8d37b0c6fb4dfcb` ran exactly one
selected test, `canonical_direct_native_chain_retains_original_e1_and_only_one_effect`.
**That test passed: 1 passed, 0 failed, 48 filtered out, in 184.73 seconds.**
Cargo, the scope and the host all exited 0. This runner contains no release
build or real-model journey.

[summary.json](summary.json) is the unchanged original runner receipt.
[receipt.json](receipt.json) separates this one actual execution from the
12 passing executions reused from candidate
`972e16ef34213f568eda3b236fc33be50e40d7fb`. **The evidence covers 12 earlier
passes plus one new pass; it does not claim 13 new executions on 4a.**

## Exact correction and reuse boundary

The only tracked source difference from 972e is inside the native-chain
test. The old assertion expected an anchored-E1 error substring. The new
assertion requires exact equality with the original T1 reader's
`ObjectClosureIncomplete { missing_count: 1 }` display text and explains
why that earlier reader rejects the bad original first.

[fixture-change.patch](fixture-change.patch) preserves the exact diff.
[reuse-audit.json](reuse-audit.json) verifies that this one replacement is
the entire source change: all remaining bytes in that test file and every
other tracked file are identical. Production code, build configuration,
shared test setup, the other selected tests, and their assertions are
unchanged. The bad-content injection, original E1/debt identities, T2
refusal, absent result and single-effect checks remain in the rerun test.

| Evidence source | Selected executions | Result |
| --- | ---: | --- |
| 972e: preparation progress, E1 SSH, source-bound E1, child E1 reopen, archive admission | 12 | 12 passed; reused with exact source checks |
| 4a: native direct chain | 1 | 1 passed in this actual run |
| 972e: original native direct chain | 1 | Original failure retained; not counted as a pass |

The earlier complete scope remains **12 passed / 1 failed** in evidence
commit `2050b28405bd98dc7c8da1e5ae248c0a9362d05c`, under
`docs/evidence/daily-readiness-20261122/scoped-972e16ef`. Its original
summary has SHA-256
`fd2633b65528e42041266199b3bf22831e6698fd8ca80306202bea2805392d40`.
The reuse audit retains each reused command, original log identity and
outer result. Reuse is bound only to these two exact candidates; later
source changes require their own applicability checks.

## Original execution and environment

The log contains one `running 1 test` declaration and one outer result,
with no nested result rows. The 48 filtered-out tests did not execute.
The full command is identical to the native-chain command in the previous
scope:

```sh
cargo test --offline --locked --all-features -j4 -p shenko-store \
  --test store_roundtrip \
  canonical_direct_native_chain_retains_original_e1_and_only_one_effect \
  -- --exact --nocapture
```

The retained [scope runner](runner.sh) has SHA-256
`fb1805822ffb0bc734e8206ca4b3eb1258379ab2cff1dca2f886ea129dd096b8`;
the original [host runner](host/runner.sh) has SHA-256
`ca01720b450ca3ad3db34d4d12f22d4e4707b5ea705973d764e2db2e6cf7389b`.
The scope runner differs from the previous six-target runner only in its
evidence directory and copied filename, removal of the other five target
selections, and removal of the release section. The remaining bytes,
native-chain command and result-count checks are identical.

The host runner verifies the transferred bundle, exact Git checkout and
container image, and holds the shared host Cargo lock. The container
runner holds `/tmp/shenko-cargo.lock`, requires a clean exact checkout and
uses the same persistent Cargo cache/target, feature flags and dev/test
profile settings. Rust, Cargo and Node version receipt bytes match 972e.

The original host timestamps are **2026-11-21 23:14:13–23:17:23 UTC**;
the scope timestamps are **23:14:14–23:17:23 UTC**, both with one-second
precision. The Cargo invocation's monotonic duration is **188.382293
seconds**. The libtest interval is **184.73 seconds**. These distinct
boundaries are preserved rather than presented as interchangeable timing.

## Integrity and limits

All ten original top-level scope files, three host receipts and the host
runner are retained. [retention-index.json](retention-index.json) maps
their remote paths, original lengths and SHA-256 values. The log is stored
as gzip with mtime zero; its uncompressed original is 21,759 bytes with
SHA-256 `a1e5836314f66ffd305072de986830285edcd460b32e19142cd1a20297030e71`.
[manifest.json](manifest.json) records every stored file except itself.

The scope and host had closed before collection. At **2026-11-21
23:19:51.969361 UTC**, no `release-4a5b88aa` directory or
`shenko-release-4a5b88aa` binary existed. Release was not part of this
runner, so its successful exit is not a release-build receipt.

Collection did not change the tested checkout, cancel a process, rerun a
test, compile source, invoke a model or inspect a live database. No model
quality, browser journey, sustained operation or daily readiness is
established by this scoped test receipt.
