# S2 build-output and receipt source increment

This candidate starts from `da6799d57cc3145758ce3117c51ad381659c2087`.
It repairs a concrete preparation defect: a successful Cargo invocation could
write to a configured target directory while `CargoBuildProvider` selected an
older executable under `source/target` and the handoff discarded its build
outcome. Hashing that old executable only established the old file's identity.

## Production boundary

- Cargo metadata and completed JSON compiler output select the actual unique
  non-test binary/package/manifest/target. The old layout guess has no fallback.
- Each stable build ID owns a freshly created output directory beneath the
  observed/requested target root. Canonical containment is checked before an
  executable is accepted. Original preferences and actual argv are distinct
  retained facts; another normal build gets a different output directory.
- The receipt contains bounded original command streams, exit/cleanup facts,
  metadata, source observations, actual artifact digest and the original
  preparation context. Typed JSON and binary CAS references retain their bytes.
- A completed observation is rooted in actual raw history before the next
  candidate-launch await. Success links the same receipt into the recovery
  closure; returned failures use the existing error journal. Failure to retain
  evidence is observable together with the original failure.
- This source change does not alter selector admission, native T1/E1 authority,
  Unknown recovery, migration contracts, policy selection or rollback semantics.

The exact process scope is leader reap plus stdout/stderr EOF on completed
collection, and group/job signal with bounded reap/drain on collection failure.
Direct future cancellation cannot attest a completed join or terminal receipt.
Unix children that closed their inherited output pipes are not proven absent.
No source snapshot, reproducible toolchain/dependency closure, hostile same-user
filesystem isolation, tested self-modification, real coding-seat completion or
full S2 acceptance is claimed. Prebuilt input remains explicitly unbuilt here.
The exclusive build output directories are retained, so the former shared
incremental-cache behavior is not claimed for them.

## Finite verification scope

Four added test functions describe five distinct new windows; the existing
three smoke-refusal windows acquire required receipt-reopen assertions. The
coherent increment therefore exercises at most eight distinct windows, not a
parameter matrix or a completion denominator.

1. A tiny dependency-free Cargo binary is actually built and executed as A,
   copied to the old default-layout path, changed to B, rebuilt with an explicit
   root override into its own directory, and executed as B. Both old paths stay
   A. The exact compiler stdout/stderr survive CAS reopening.
2. The same real source then fails compilation. Its original failed invocation
   is retained; neither previously built executable is selected.
3. A real two-package workspace with two matching binary targets is refused as
   ambiguous after preserving Cargo's original successful output.
4. A real compiler observation exceeds a small explicit stream bound. It is
   stopped/reaped and the readers settle, with truncation retained and no
   accepted artifact.
5. A real Store, existing bounded smoke fixture and prebuilt input retain the
   same build observation through raw history, activation recovery closure and
   reopen. This is a mechanism fixture, not a real Shenko candidate smoke.
6. The three pre-existing smoke failures retain their exact failure types,
   unchanged working selector and absence of an activation record; reopening
   additionally verifies their build receipt roots.

The Rust fixtures are in `crates/shenko-update/src/build_tests.rs` and
`crates/shenko-update/src/handoff_tests.rs`. Local Cargo, rustc and rustfmt are
unavailable in this recovered environment. **No Rust compilation or fixture
execution has occurred for this increment.** The standard candidate CI must
supply that evidence. No workflow or model experiment was triggered here.
The unavailable documentation-parser environment was not retried. The existing
kernel-boundary script was attempted once, but exited before graph evaluation
because its Cargo metadata subprocess could not be launched (`cargo` missing).
Local whitespace and source checks establish only those recorded boundaries,
not a successful kernel gate or Rust execution.

Official format references:

- <https://doc.rust-lang.org/cargo/reference/external-tools.html>
- <https://doc.rust-lang.org/cargo/commands/cargo-metadata.html>
- <https://doc.rust-lang.org/cargo/commands/cargo-build.html>
