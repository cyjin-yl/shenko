# Obelisk history delivery TODO

Tracking: [#300](https://github.com/cyjin-yl/shenko/issues/300).
Review: [PR #301](https://github.com/cyjin-yl/shenko/pull/301), with owner-authorized engineering merge while M08 stays open.
Base: `054817b0d5729fed56ec5a974d101c31edfd1410`.
Upstream audit: `cc75c2dd1aead4f4374758a7536385307934e177`.
Contract: [external history](../specs/external-history.md).

## Implementation sequence

- [x] **M01 — Complete originals and portable identities.** Immutable source versions, full bytes, positions and observed metadata; byte deduplication distinct from event identity.
- [x] **M02 — Stable original citations.** Digest and byte-span references, authenticated bounded reads, source loss/offline operation, missing/corrupt states.
- [x] **M03 — Chinese and long-history retrieval.** Literal full-text search, bounded scans and continuations, match-centered previews, no inherited 10k-character prefix limit.
- [x] **M04 — Independent Obelisk query bridge.** Explicit providers, fixed source compatibility, composable JS, deadline/cancellation/output budgets and owned process cleanup.
- [x] **M05 — Product plugin and task wiring.** Existing Node/Cordis/native task authority, live enable/disable, original-attempt recovery, existing memory/cognition writes.
- [x] **M06 — Replication and recovery.** Existing cognition/CAS closure transfer, independent import idempotency, retained versions, tombstone suppression; no live SQLite/WAL replication.
- [x] **M07 — Traceable structural view.** Source/version distribution, bounded coverage, unknown freshness and direct drill-down. Semantic graph efficacy remains a separate, unproven extension.
- [ ] **M08 — Evidence and efficacy.** Deterministic mechanisms and actual pinned upstream CLI; opt-in real-model comparison with identical corpus and budgets. A real small-Qwen smoke is now recorded; repeated reference-model efficacy and corpus-scale contribution remain unproven.
- [x] **M09 — Reviewable delivery.** Contracts, Why, source/license provenance, implementation map, focused verification and upstream PR. CI is reported against its exact pushed head; delivery does not imply model efficacy.

These items are one coherent optional-plugin increment. They do not depend on
merging unrelated open PRs #298 or #299 and do not spend separate test quotas
per commit or agent. There are nine independent verification scenarios:
seven Rust tests (three archive, three upstream runtime and one native lifecycle)
and two explicit runners (native CLI receipts and real-model comparison).
The external runners are separate executable entrypoints; absent prerequisites
produce an error, never a passing ignored model test. No new `#[ignore]` or
lint suppression is added.

## Reproduce the mechanism checks

Use the repository toolchain and one shared build directory/lock when agents
share a host. The targeted commands, after integration, are:

```bash
cargo test -p shenko-exec-host external_history::tests --lib
SHENKO_OBELISK_EXECUTABLE=/absolute/path/to/node SHENKO_OBELISK_SOURCE_CHECKOUT=/absolute/path/to/audited/obelisk cargo test -p shenko-exec-host obelisk_runtime::tests --lib -- --nocapture
cargo test -p shenko --test obelisk_lifecycle -- --nocapture
SHENKO_OBELISK_EXECUTABLE=/absolute/path/to/node SHENKO_OBELISK_SOURCE_CHECKOUT=/absolute/path/to/audited/obelisk cargo run -p shenko --example obelisk_native_receipt
python3 scripts/check_kernel_boundary.py --baseline-ref 054817b0d5729fed56ec5a974d101c31edfd1410
```

The process/CLI tests must distinguish a missing upstream checkout from
executed evidence. A skipped or unconfigured invocation is not a passing
compatibility result. The actual run record below is updated only after the
corresponding command completes.

## Current evidence

| Layer | Observation | Scope |
| --- | --- | --- |
| Archive / original / replication | Three targeted tests passed on the source with final archive budgets, including cross-page long tails, independent capture/replication, tombstones and corrupt/missing originals. | Actual Store/CAS/cognition mechanisms; no model claim. |
| Upstream integration | Three runtime tests passed on the path fix with audited source and Node 24.22.1: Codex/OMP/Pi, CJK fallback, raw UTF-16 pages, source loss, timed cleanup, lexical root normalization and trim rejection. | Two actual CLI scenarios and one configuration scenario; zero skipped tests in the recorded local run. |
| Published npm compatibility | `@obelisk-apps/cli@0.3.0` lacks the independent-home and `replay_required` schema expected by the audited source. | Explicit rejection required; equal version strings are not compatibility proof. |
| Model endpoint | The reference-model routes remained unavailable. Box actually served official Qwen3-1.7B Q8_0 and Qwen3-4B Q4_K_M through llama.cpp, with verified model bytes and 16384-token deployments. | Real small-model evidence, distinct from the reference baseline; the owned 4B service was stopped after evidence capture. |
| Store regression | Existing raw-session writer test and two session-event search tests passed. | Same native write semantics and existing FTS behavior. |
| Native lifecycle | The regular lifecycle test passed on the final code; the actual product CLI configure/enable/status smoke also passed, with the configuration outside Node's private database directory. | Disabled/enabled discovery, restart, source/config loss and original-attempt behavior; configuration mode 0600. |
| Native CLI receipts | The explicit runner passed with actual Node 24.22.1 and pinned Obelisk source: MCP coexistence, successful standard query, large custom-JS observation, MCP retirement, authenticated receipt paging and original-attempt reconciliation. | Custom-JS host effects stay unknown; exactly one original attempt and E1 are retained. |
| Model runner | Three real invocations produced no qualified complete research pair. The 1.7B runs exposed absent retrieval, false-positive judging and invalid controls; 4B completed three native history queries and two Obelisk queries but did not retrieve the fixture answer, then both arms timed out during subject inference. | All failures and unknown usage remain in the evidence. Actual query execution does not establish a source-grounded final answer. |

The machine-readable [validation record](../evidence/2026-11-15-obelisk-validation.json)
binds executed commands, source heads, toolchain versions, artifact hashes,
exit codes and scope. The earlier [upstream CLI record](../evidence/2026-11-15-obelisk-cli-validation.json)
retains the source compatibility investigation. The
[follow-up record](../evidence/2026-11-16-obelisk-followup/INDEX.json) binds the
source-root fix, fresh real-CLI/native runs, context-isolation fix, focused
build/lint checks and independent review. To avoid concurrent worktree
cache collisions, the final native targets were built in one Cargo invocation
with a temporary codegen profile for workspace packages, then their exact
Cargo-reported executable paths were run directly. This did not change the
repository build configuration or CI lint policy.

All three real small-model attempts are preserved in the
[Qwen evidence index](../evidence/2026-11-16-obelisk-qwen/INDEX.json),
including unchanged wire requests/responses, CAS objects, source/ELF identities,
Store row exports, model/service versions and the verbatim failures. The old
`blocked_external` records remain historical observations, not the current
availability history. In the first attempt the baseline initial raw projection
exposed answers; it is retained as an invalid efficacy comparison. Actual first
wire requests in the corrected attempts confirm that only a generic Pointer
and the question reach the native baseline before retrieval.

The second 1.7B attempt produced fourteen complete-decision protocol rejections,
then its model service disappeared; the cause remains unknown. Its proposed
search text was never executed. The 4B service stayed healthy after both
timeouts; logs show slow prompt processing followed by cancellation. The
transparent Box relay used a 175-second remote curl budget within the product's
180-second provider budget and added queueing, preflight, transfer and buffering.
These timings do not establish direct-connection performance. Neither timeout
has a returned completion or known final usage; neither arm produced a final
research answer or judge result. There was no automatic resampling or operator
interruption. The 4B service was intentionally stopped only after the runner
had ended and its final state was captured.

The first CI run exposed an acceptance-harness serialization error and new
lint diagnostics; those were corrected without changing production evidence
types, weakening assertions or adding suppressions. It also exposed a compiler
discovery defect: Rust 1.98.1 lists `tail-call-track-caller` in `-W help` but
rejects that force-warn flag. The ratchet now probes the same compiler's actual
acceptance and retains both probe logs. Only compiler-rejected candidates are
removed; the baseline, warning comparisons, three Clippy groups and source
policy are unchanged.

## Model comparison and remaining boundaries

The opt-in acceptance harness uses the product's real Store, subject step,
provider, parser and native dispatcher. It compares the same synthetic
cross-session investigation with and without the external-history capability,
under the same model and budget, and retains failed outputs and source
references. A judge uses the existing product sandbox evaluation path; a
hand-written mock answer does not satisfy this gate. Configure the real
Qwen baseline and follow [local model behavior](local-model-behavior.md).

The explicit runner is
`cargo run -p shenko --example obelisk_acceptance`, with
`SHENKO_USE_LOCAL_MODEL=1`, `SHENKO_LOCAL_MODEL_URL`, `SHENKO_LOCAL_MODEL`
and `SHENKO_OBELISK_CONFIG` set to the selected real installation. Also set
`SHENKO_OBELISK_ACCEPTANCE_CONTEXT_TOKENS` to the verified deployment context
limit; it is sealed as explicit configuration and shared by both arms. The
runner does not infer capacity from a model name or its training context.
It writes
`blocked_external` and exits unsuccessfully when those prerequisites are
missing. The compiler can check this runner without treating model absence
as a passing test.

The native baseline is a **post-rollover retrieval** scenario. The same full
corpus stays in its own raw branch; after binding the actual subject profile,
the existing `NotesRolloverPlugin` with `retain_units=0` prepares a Pointer
window through normal policy admission and is uninstalled before inference.
No summary or fixture answer is inserted. The candidate keeps the corpus only
in its external source files. `starting-context.json` records both starting
windows, and the actual first provider requests must confirm the isolation.
The shared sandbox selects the first committed continuation after its tracked
tasks finish; it does not keep sampling until the desired answer appears.

The shipped external-history definition explains the distinction between the
archive and the configured live index, with complete JSON calls and exact
returned identifiers. Its examples contain no scenario facts. An empty archive
does not establish that configured external sources have no history.

Do not mark M08 complete on a small-model smoke alone. Repeated reference-level
sampling must record the candidate, model, endpoint, prompt digest, corpus,
counts, verdicts, token use and elapsed time. A judge label alone cannot replace
actual retrieval receipts or source-grounded review, as the retained first
attempt demonstrates. The optional semantic graph and a corpus-scale performance claim remain
unproven. Source roots and the independent upstream source checkout must be
configured on the machine that will query live harness history. Existing
archives are portable without that machine.
