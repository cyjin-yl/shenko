# Original observations from the existing update fixtures

The optional `SHENKO_UPDATE_BUILD_EVIDENCE` sink retains observations from the existing Cargo build and prepare/reopen tests. The existing Rust CI test step now sets it to `.ci/evidence/update-build`; the already unconditional artifact upload includes that directory. The same step enables the separate `SHENKO_PTY_WRITE_EVIDENCE` sink. No test command, compiler option, timeout, ignore, baseline or gate changes.

## Why the export is needed

[CI 3e62](../evidence/ci-3e62c0e5-20261116/README.md) actually passed the existing build and PTY assertions. Its successful PTY diagnostics were captured by libtest, and its build/prepare fixtures deleted their temporary originals. The runner result is valid within that scope, but the missing raw receipts are unavailable for independent inspection. This change only retains the next actual execution; it cannot reconstruct that prior run.

## Retained data

Each update fixture creates a unique directory beneath the explicit sink. Its index records original paths, byte lengths, BLAKE3 digests and export observation times. Numbered `.blob` files preserve bytes; their index labels distinguish typed JSON observations, source inputs, compiled artifacts and copied CAS/installation files.

- Each original `BuildOutcome` or `BuildError` is recorded immediately after its real invocation returns, before its existing assertions. The typed JSON includes the exact bounded byte arrays, EOF/truncation counts, actual exit and cleanup observations, Cargo metadata/version, source observations and refusal. No UTF-8 replacement is used for raw streams.
- The existing A/B/C source files are copied before their original invocations. These are observations of fixture inputs, not a frozen source image or a reproducible-build claim. The actual selected artifact bytes are exported only from the fixture's owned directory. The original old/rebuilt executable runs keep their complete `CommandObservation`.
- The existing successful and failed CAS seals retain their actual object references and immutable CAS files. The existing reopen assertions still verify the same original bytes.
- The three existing prepare refusals retain their actual candidate bytes and returned errors. Their reopened raw session events and the successful prepare/reopen events remain original Store reads. On the normal path all fixture and reopened Store handles are dropped before the installation files are copied, then evidence completion must succeed before the existing cleanup removes the temporary installation.

The export does not traverse Cargo target caches or unrelated temporary directories. A file is limited to 64 MiB, an export to 256 MiB and a traversed tree/index to 4,096 entries. Non-regular files and copy failures are reported. These are evidence-export bounds, not altered compiler output limits or product acceptance thresholds.

## Failure semantics

Writes happen while the original values are still available. Export errors are printed and retained; they cannot turn a successful fixture into an evidence pass. The original assertions and failure messages remain in place. On a successful test path, a failed export returns an error before deleting the original workspace or installation. Partial files and prior atomically published indexes remain available.

An early-return or panic retains the already observed build outcomes. Incomplete Cargo workspaces are left at their original paths. For a watched, fixture-owned installation, scope-exit export retains the bytes it can read and explicitly marks database snapshot consistency and process cleanup as **unasserted**. Those incomplete bytes are not a consistent backup or a join receipt. An export never reruns the candidate, reopens a Store, seals replacement facts or claims an unobserved success.

## Scope and validation

This adds **zero behavioral windows**. The original five build windows and three existing refusal windows remain unchanged; the independent prepared-artifact window keeps their combined bound at nine. No original input, assertion, success condition, command, byte limit or duration changes. `test_evidence` is compiled only under `cfg(test)` and has no production API consumer.

Local validation is limited to source review, preservation checks of the original assertions and invocation budgets, exact workflow comparison, and `git diff --check`. Cargo, rustc and rustfmt are unavailable in the current local environment. The exporter has not executed here; a subsequent exact-candidate CI must validate its compile and actual output. This does not prove the tested-self-modification loop, source freeze, full smoke policy, NapCat/coding-seat integration, migration/activation acceptance, or 7/30-day operation.
