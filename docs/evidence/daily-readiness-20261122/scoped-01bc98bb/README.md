# Candidate 01bc: scoped validation blocked during compilation

Candidate `01bc98bb2d9a73589d05baff93f2026ea7e4143f` completed this scoped
runner with exit 1. All six Cargo invocations exited 101 while compiling
`shenko-store`. **Thirteen test executions were planned; zero tests ran.**
This is six compile failures, not thirteen failed tests or a runtime result.
The conditional release build was not admitted, and this scope made no model
request.

[receipt.json](receipt.json) records the counting and evidence boundaries.
[summary.json](summary.json) is the unchanged original runner summary;
its six `outer_summary` values are all null. Each original exit file and log
is retained. Log compression uses gzip with mtime zero and preserves all
uncompressed bytes. [retention-index.json](retention-index.json) maps the
original paths and byte identities, and [manifest.json](manifest.json) records
the stored identities of every file in this directory except itself.

## Observed failure

Every log reports both `FnMut` and `FnOnce` implementations that are not
general enough at `crates/shenko-store/src/native_evidence_closure.rs:30:5`:

```rust
roots.sort_by_key(NativeEvidenceObjectIdentity::from);
```

The compiler describes a function item accepting a reference with one
specific lifetime where the sorting callback must accept the borrowed
element for any required lifetime. All six invocations end with
`could not compile shenko-store (lib) due to 2 previous errors`. The original
diagnostics, dependency compilation output and warnings remain in the logs;
this directory records no successful correction or rerun.

| Cargo invocation | Planned tests | Cargo exit | Test result row | Elapsed seconds |
| --- | ---: | ---: | --- | ---: |
| Native direct chain | 1 | 101 | None | 20.528437 |
| Fresh preparation progress | 1 | 101 | None | 10.418176 |
| Original E1 over authenticated SSH | 1 | 101 | None | 19.931523 |
| Source-bound original E1 | 1 | 101 | None | 10.742181 |
| Failed generation E1 reopen | 1 | 101 | None | 20.639890 |
| Archive admission | 8 | 101 | None | 10.398626 |

The exact package, target, filter and arguments for every invocation are in
the original summary. No log contains a `running N tests` declaration or a
`test result:` row, including nested results. The compiler failure occurs
before any selected test harness starts.

## Source, runner and time

The retained [runner](runner.sh) requires the exact candidate checkout and a
clean worktree before invoking Cargo. It uses the persistent project
devcontainer source, `/tmp/shenko-cargo.lock`, the persistent Cargo target and
cache, `--offline --locked --all-features -j4`, and debug information disabled
for dev/test profiles. It also sets persistent native artifact paths and the
Node runtime path. Its original SHA-256 is
`7f65ae2da2c0021851f5648afa80bce9048ef53d8c2c38203ef7ce39ddf9489c`.

The version receipts report Rust `1.98.1 (48a229cea 2026-09-01)`, Cargo
`1.98.1 (797e8a9bc 2026-08-05)`, and Node `v24.22.1`.
The original host receipts span **2026-11-21 22:59:52–23:01:26 UTC** and
report exit 1. These files have one-second timestamp precision. The per-Cargo
monotonic durations total **92.658833 seconds**; those durations are distinct
from launcher wall time. The host and runner finished naturally; this
collector issued no termination, cancellation or retry.

At the read-only retention observation, **2026-11-21 23:03:15.178078 UTC**,
both `release-01bc98bb` and `shenko-release-01bc98bb` were absent, the scoped
native artifact directory contained zero files, and the scoped native E1
directory did not exist. The original `set -e` runner exits on the failed
scope before its release section. These observations support the blocked
release result; they are not a release or real-model acceptance receipt.

## Preservation limits

This directory retains all 20 original top-level scoped files and all three
matching `anchored-native-01bc98bb.host.*` files. The host receipt set contains
start, finish and exit files; it contains no separate host stdout log. The
six Cargo logs contain the original compiler output. All 23 original file
identities were verified again after transfer.

Collection read only the completed scope. It did not change the tested
checkout, compile source, run a test, start a release build, invoke a model,
or inspect a live database. Later corrections and successful executions must
have their own source identities and receipts; they do not change this
candidate's compile-failure result.
