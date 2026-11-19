# Reviewed retention, preparation identity and original-evidence follow-up

This source checkpoint follows public head `3e62c0e517a246872101cd1e61a694b2156bc919`. [That candidate's CI](../ci-3e62c0e5-20261116/README.md) had 1,094 passing Rust tests and no failing Rust test, but the workflow failed Clippy. Its successful-test original-receipt retention gap is preserved there. **None of the new sources below inherits a runtime pass from that older candidate.**

## Changes and exact boundaries

| Source | Resulting behavior | Evidence boundary |
| --- | --- | --- |
| `ebc39d1` | Rename five PTY `select!` bindings consistently, including their uses, without changing any branch, future, ordering, input or message | Inverse rename reproduces both prior files exactly; zero new scenarios; compiler result pending |
| `1123ce9`, `17eb821` | Bind forward activation to its sealed prepared artifact/state, then reduce fixture parameters without changing its assertions | One actual prepare/reopen/mutate regression is added to the preceding eight build/refusal windows: nine total, pending execution. Earlier live-caller quiescence and the remaining file race are not solved by this source |
| `1d81cc6` | Preserve the actual native SSH command handle's cleanup error and local stream-close observation alongside original output, exit and interruption | Two original protocol windows and two original projection windows are extended; with the preceding five PTY windows this scope is nine. A local protocol end is not a remote process-stop fact |
| `0260d28`, `7bffa671`, `a57aee9` | Read retained objects from one committed Store snapshot, expose a real zero-touch CLI consumer, and decode its current storage carriers strictly | Three observations plus the preceding six declaration windows remain nine. This is not all-slot coverage, a GC producer, a deletion epoch or cross-writer exclusion |
| `d80d8cb`, `a59753d` | Export actual PTY/build/prepare originals on the next execution, with explicit incomplete/error semantics | Zero new behavioral windows. Existing inputs, assertions and budgets remain. Runtime export is pending |

`ae0bcfa` and `832d733` document the installation writer permit call graph and native tmux prerequisites. They are design work, not an implemented global fence. The separate [Clippy input-scope audit](../clippy-input-scope-20261116/README.md) preserves why the seven archive-attribute failures remain unresolved without changing the gate.

## Store carrier review

The initial reader missed some independent ObjectRef columns and confused a `ToolArtifactRef` with a plain CAS reference. The follow-up reads actual column/record shapes, treats the real tool manifest and chunks as roots, and distinguishes whole-artifact identities and Node-inline storage. Historical CAS locators are classified without reading those paths. Unknown or malformed carriers remain explicit gaps.

Independent review then identified two tool-manifest interpretation gaps. A content-store manifest now requires the current adapter's namespace and codec. Its expansion identity includes the JSON reading, so an earlier generic walk of the same digest does not satisfy a later manifest validation. Each manifest edge still checks codec and size; additional reads and parses charge the existing budgets. The generic walker is not a semantic validator for every plugin codec.

The [original carrier checks](store-carrier-review/observations.json) and [alias follow-up checks](store-carrier-review/alias-observations.json) preserve exact source/tree identities, hashes and outputs. These two corrections changed no test file and added no corruption scenario. All earlier source checkpoints remain available.

## Export and integration review

[The update export check](update-build-evidence-static.json) confirms that all 37 original build assertions and 118 original handoff assertions have identical tokens in their original order. The three production build/handoff/smoke files are byte-identical to its base. Removing only the two new evidence environment assignments reproduces the old workflow exactly. The original import-path failure in that local conservation script is retained in [its failure record](static-import-failure.json); the subsequent pure-tokenizer check exited zero. Neither is a Rust execution.

[The PTY export check](pty-write-evidence-static.json) retains its exact five-window source comparison. The original transport, peer, write/flush/cancel, cleanup and timeout behavior is unchanged. The exporter captures actual values at their original observation points, including data receipts before later signal receipts replace the current record. Export failure cannot become a passing evidence result.

Independent inspection of integrated `4394226a` is retained under `integration-review/`. Removing the unchanged 13-line Store retention block from its merged `handoff_tests.rs` reproduces the original `a59753d` exporter source byte-for-byte. The normal sequence is still snapshot verification, original Store drop, closed-installation export, successful export completion, then temporary-directory cleanup. Eleven reader/CLI/core/fixture files match `a57aee9`; four build/export/workflow files match `a59753d`.

The real `check --retained-objects` branch returns before normal writable Store open or bootstrap. The pre-dispatch owner path creates directories only for `Start`. This preserves the intended read-only route; it does not assert an installation-wide lock. Exact inspected source and observations, including the independent reviewer script, are retained without rewriting their original paths or times.

## Validation limits and provenance

Local source and whitespace checks passed for these code changes. The environment has no Cargo, rustc or rustfmt, so compilation, Clippy and actual new fixture/export behavior require the next exact-candidate CI. No source-policy exception, ignored test, baseline or compiler flag was changed. The full failed CI archive retains its original whitespace diagnostics separately.

The [source checkpoint archive](../source-checkpoints-20261116/README.md) preserves original raw commits, authors, times, parents and tree deltas separately from normalized publication commits. The [file index](archive-index.json) hashes this directory's retained observations. This checkpoint makes no claim of real NapCat/coding-seat use, tested self-modification, complete multi-node semantics, installed named runtimes or elapsed 7/30-day operation.
