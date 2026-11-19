# Canonical continuation and capacity integration evidence

The integrated Rust checkpoint is
`bfc5f7e125a30bd86f48904617e97994c85d49a2`, tree
`76cabefd52abc3b29a57b0392d179c023cecdb56`. Its four selected packages passed
an all-target metadata check. **The new native and correction journeys have
not executed, and combined Clippy did not pass.** Two local Clippy attempts
stopped on default-deny diagnostics; both complete outputs are preserved.

The subsequent script/configuration checkpoint is
`98bcf67a5ede6a9b4fb03819b6428bf5f3f9168b`, tree
`6835623f3033b51a271b1fe3991b4df7296a5674`. It preserves all Rust/Cargo inputs
from `bfc5f7e1` and adds the separately reviewed three-file harness increment
from `82d05630`. Its static reports are not a new model run.

## Source composition and independent inspection

The native increment originates at
`dff743d509b88b4d28d4e62482532def75fa55bd` (tree
`3c6a94fe66d5ee50f8dc946018689c1453308647`). Its introduced patch lines are
preserved exactly in the integrated native port. The original canonical
claim/T1/E1/T2 implementation and its complete new Store journey remain.
Shared Store module/schema admission points also contain the independent
capacity additions; the Node document retains the already observed `9b5a6cb`
results with their original candidate scope.

The capacity increment originates at
`c3cd591a65f18abbf505bead9b4c93b28ec4fcdb` (tree
`f208542c5377e05b6a887215e44e3bd7b59495e4`). Its introduced patch lines are
preserved in all files except the two deliberate mainline integration points
described below. Whole-file differences in subject files also retain main's
existing retry-debt/error handling and journal regression; they do not remove
the capacity patch's correction or fixture assertions. The per-file original
and integrated blobs and patch-line comparisons are recorded in
[source-audit-bfc5.json](source-audit-bfc5.json).

### Broker metadata and the real Start consumer

The final broker combines the mainline strict
`ModelMetadataList { data: Vec<ModelMetadata>, has_more }` reader with the
capacity increment. An evidence document which advertises `data` must parse
as the complete typed list. Structural errors are `InvalidModelMetadata`.
Every card matching the bound model is considered across the observed
documents; conflicting cards are rejected rather than accepting the first.

`meta.n_ctx` joins the existing served context fields. Supplied limits must
be positive and agree. Training capacity is not a served-limit fallback.
An absent bound model retains main's distinct
`PROVIDER_MODEL_METADATA_INCOMPLETE` / `PROVIDER_MODEL_METADATA_MISSING`
classification using `has_more`. A missing observation and an explicitly
invalid advertised response remain different outcomes.

The old `lib.rs` Start guard is ported to
`runtime_inference::install`. It returns the typed invalid-metadata error
before installing the model child handler or spawning `broker.run`.
Other unavailable observations retain the existing warning path. This is a
source inspection finding, not a live endpoint or readiness result.

### Other integrated history

The ordinary `d372ef3e` merge and its
[complete base-integration evidence](../current-base-integration-20261116/README.md)
remain intact. Checkpoint `0966d699` first combined those changes with native
and capacity work. The later ordinary merge `bfc5f7e1` includes current base
`f5c6bfa5`, adding the new distinct receipt-conflict fact/index migration and
its existing upstream regression. That new migration/regression has metadata
evidence here, not runtime acceptance.

## Actual checks, including failures

| Source binding | Actual check | Observed result and limit |
| --- | --- | --- |
| Native `dff743d` Rust source | Store `store_roundtrip` Clippy metadata with explicit `--force-warn` flags | Exit 0, 14.69s; no test execution. Full JSONL contains 515 warning messages, including dependencies/duplicates; this is not a clean-workspace or CI ratchet result. |
| Capacity WIP on `e73bfb9` | Broker/subject/Store `--tests` Clippy with explicit `-W` flags | Exit 0, 48.76s. The exact WIP tree was not captured. The worker reports production source equal to final `c3cd591a`, but a few obligation-fixture codec/layout lines changed afterwards. This is not an exact final-fixture gate. |
| Final capacity `c3cd591a` | Subject `--tests` metadata | Exit 0, 25.99s; final source bytes, no tests run. |
| Intermediate integration `0966d699` | Four-package Clippy, default lint levels, dependencies included | Exit 101: `clippy::inherent_to_string_shadow_display`, `shenko-types/src/ids.rs:63`. |
| Integrated Rust `bfc5f7e1` | Four-package Clippy with `--no-deps`, default lint levels | Exit 101: `clippy::never_loop`, `shenko-store/src/quota.rs:907`. |
| Integrated Rust `bfc5f7e1` | Four-package `cargo check --all-targets` | Exit 0, 1m48s; metadata only. |
| Integrated Rust `bfc5f7e1` | `check_docs.py --verify-migration` | `errors=[]`, 365 migrated chunks; documentation integrity only. |
| Harness source `82d05630`, identical three files at `98bcf67a` | Existing static script/workflow report and documentation report | Python/Bash parsing and named AST retention are recorded; documentation has `errors=[]`. No model server or journey was executed by these checks. |

The two combined Clippy commands did **not** use the CI ratchet's lint-level
arguments. Their failing source files are unchanged by these increments.
No baseline or `allow` was changed to make these attempts pass. The real CI
ratchet, including its comparison source, remains a separate required result.
The complete commands, environment, source-binding qualifications and outputs
are recorded in [observation.json](observation.json); a derived diagnostic
summary is in [clippy-summary.json](clippy-summary.json).

Native documentation was checked with `--verify-migration`. Capacity's own
documentation report came from its older base's checker without that flag;
it is retained with that scope. The later combined documentation result
provides the new checker's migration-integrity observation.

## Harness delta and what remains unobserved

The three-file harness increment explicitly requests a 32,768-token slot
from the same pinned diagnostic model/runtime. It retains the complete
prompt, sampling, output reservation and time bounds. Its snapshots add the
window/scoping facts and only `ledger_id`, `seq`, `created_at` from
`commit_log`. The normal first and final SIGINT stops must both have exit 0
for the journey to pass. Original model/child/E1/output/replay/restart
conditions are preserved. These are configuration and assertion changes;
the next server's actual capacity, resources and behavior still need a run.

The new native path remains the bounded original first-claim, paired T1,
single definite E1, childless T2 continuation. It does not establish general
foreign recovery, standalone E1 propagation, Unknown reconciliation or
multi-operation execution. Its separate physical materialization receipt
does not rename the source observation or grant a recovered worker permission.

The new schema-14 correction path retains the rejected request/result and
immutable source/tail anchors while preparing a bounded replacement. Its
new journeys are pending; metadata compilation cannot show successful
compaction, cancellation/reopen or live model behavior. The actual fourth
model-run failure remains in its original evidence and is not relabeled.

## Original bytes

All compiler JSONL and stderr streams are complete and losslessly gzipped;
none are truncated, normalized or replaced by excerpts. Plain JSON reports
retain their original bytes. Each file has its original and retained byte
counts and SHA-256 digests in the observation. The original scratch files
remain available at the recorded paths.

[rust-cargo-inputs-bfc5.json](rust-cargo-inputs-bfc5.json) records the 714
tracked Rust/Cargo/toolchain configuration objects for later source
comparison. Its scope is explicit: it is not a hermetic build-input claim.
The script-source audit separately binds the exact static reports to the
three installed source files.
