# Candidate 972e: 12 tests passed, one fixture assertion failed

Candidate `972e16ef34213f568eda3b236fc33be50e40d7fb` completed all six
scoped Cargo invocations. **All 13 planned tests executed: 12 passed and
one failed. There were no compile failures.** The scope and host exited 1,
so the conditional release build was not admitted. This is a scoped
regression receipt, not a real-model journey or daily-readiness result.

[receipt.json](receipt.json) records the independently checked counts and
limits. [summary.json](summary.json) is the unchanged original runner
summary. All six logs, six exit files, runner, version and time receipts,
and three matching host receipts are retained. Logs use gzip with mtime
zero, with unchanged uncompressed bytes. Original paths and hashes are in
[retention-index.json](retention-index.json); [manifest.json](manifest.json)
identifies every stored file except itself.

## Actual results

| Cargo invocation | Planned tests | Passed | Failed | Cargo exit | Outer libtest seconds | Cargo invocation seconds |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Native direct chain | 1 | 0 | 1 | 101 | 192.67 | 217.694445 |
| Fresh preparation progress | 1 | 1 | 0 | 0 | 6.40 | 6.972676 |
| Original E1 over authenticated SSH | 1 | 1 | 0 | 0 | 123.37 | 189.018973 |
| Source-bound original E1 | 1 | 1 | 0 | 0 | 52.97 | 53.674753 |
| Failed generation E1 reopen | 1 | 1 | 0 | 0 | 9.46 | 45.761532 |
| Archive admission | 8 | 8 | 0 | 0 | 89.12 | 104.105739 |

Each log contains exactly one outer libtest result and the matching initial
`running N tests` declaration. There are no nested result rows to add or
exclude, no ignored selected tests, and no unexecuted selected tests. The
285 filtered-out tests belong to the unselected harness population and are
not counted as executions or skips. The exact commands and filters remain
in the original summary.

The first Cargo exit 101 is a **test assertion failure**, unlike the earlier
candidate 01bc's compile failures. Its original libtest result is
`FAILED: 0 passed; 1 failed` and remains so.

## Why the native-chain assertion failed

The original log reports a panic at
`crates/shenko-store/tests/store_roundtrip.rs:3996:5`. The retained debt
contains this original reason:

```text
object closure incomplete: missing 1 object(s)
```

At this candidate, that assertion instead expected the reason to contain
`anchored E1 predecessor closure is missing or corrupt`. The fixture had
temporarily replaced one original implementation object with incorrect
bytes of the same length. Source inspection shows that the first E1 sealing
path calls the original T1 reader, including its complete closure check,
before constructing the anchored E1 envelope. That earlier reader rejects
the bad original and its error becomes the retained E1 seal debt.

The test had reached the debt-reason assertion, but the assertions after
that line did not execute. Calling this a fixture expectation error does
not turn the failed test into a pass. A correction must preserve the bad
original rejection, original E1 and debt identities, T2 refusal and
single-effect checks, then produce its own execution receipt. This
directory contains no corrected execution and does not retroactively
reclassify the failure.

## Source and environment

The retained [runner](runner.sh) checks the exact candidate Git HEAD and a
clean worktree before Cargo. It uses the persistent project devcontainer
source, `/tmp/shenko-cargo.lock`, persistent Cargo cache and target paths,
`--offline --locked --all-features -j4`, dev/test debug information disabled,
and explicit persistent native artifact and Node runtime paths. Its
original SHA-256 is
`7f65ae2da2c0021851f5648afa80bce9048ef53d8c2c38203ef7ce39ddf9489c`.

The original version files report Rust `1.98.1 (48a229cea 2026-09-01)`,
Cargo `1.98.1 (797e8a9bc 2026-08-05)`, and Node `v24.22.1`.
The host files span **2026-11-21 23:02:55–23:13:14 UTC**, and the scope
files span **23:02:57–23:13:14 UTC**. Those files have one-second precision.
The six Cargo monotonic durations total **617.228119 seconds**; the outer
libtest durations total **473.99 seconds**. They measure different
boundaries and are not interchangeable performance claims.

The runner and host finished naturally. At the read-only retention
observation on **2026-11-21 23:14:53.410907 UTC**, both `release-972e16ef`
and `shenko-release-972e16ef` were absent. The original `set -e` runner
stopped before its release section after the scope returned 1.

## Preservation and limits

All 20 original top-level scoped files and all three matching
`anchored-native-972e16ef.host.*` files are retained and their lengths and
SHA-256 values were rechecked after transfer. The host set has start,
finish and exit receipts, with no separate host stdout file. The six Cargo
logs contain the original process output. Large test databases and CAS
directories were not copied into this compact log receipt.

The SSH and recovery tests exercise their specified fixtures. They do not
run the reference-model browser journey, measure model completion quality,
or establish long-running reliability. No model deployment was invoked by
this scope or by evidence collection.

Collection began after the complete runner had closed. It did not change
the tested checkout, cancel a process, rerun a test, compile a candidate,
or start a model. Later candidates may cite these 12 passing executions
with explicit source-identity checks, but must not claim that all 13 tests
passed here or that all reused tests ran on a later candidate.
