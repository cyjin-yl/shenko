# Obelisk history delivery TODO

Tracking: [#300](https://github.com/cyjin-yl/shenko/issues/300).
Review: [draft PR #301](https://github.com/cyjin-yl/shenko/pull/301).
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
- [ ] **M08 — Evidence and efficacy.** Deterministic mechanisms and actual pinned upstream CLI; opt-in real-model comparison with identical corpus and budgets. Model results are incomplete until a real endpoint has run it.
- [x] **M09 — Reviewable delivery.** Contracts, Why, source/license provenance, implementation map, focused verification and draft PR. CI is reported against its exact pushed head; draft status does not imply model acceptance.

These items are one coherent optional-plugin increment. They do not depend on
merging unrelated open PRs #298 or #299 and do not spend separate test quotas
per commit or agent. There are eight independent verification scenarios:
six Rust tests (three archive, two upstream runtime and one native lifecycle)
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
| Upstream integration | Both runtime tests passed with audited source and Node 24.22.1: Codex/OMP/Pi, CJK fallback, raw UTF-16 pages, source loss and timed process cleanup. | Actual independent CLI; zero skipped tests in the recorded local run. |
| Published npm compatibility | `@obelisk-apps/cli@0.3.0` lacks the independent-home and `replay_required` schema expected by the audited source. | Explicit rejection required; equal version strings are not compatibility proof. |
| Model endpoint | The previously configured local/LAN and authorized gateway routes returned refusal or 502/503; the available remote environment's tailnet was logged out. | `blocked_external`; no model success rate, accuracy or token-saving claim. |
| Store regression | Existing raw-session writer test and two session-event search tests passed. | Same native write semantics and existing FTS behavior. |
| Native lifecycle | The regular lifecycle test passed on the final code; the actual product CLI configure/enable/status smoke also passed, with the configuration outside Node's private database directory. | Disabled/enabled discovery, restart, source/config loss and original-attempt behavior; configuration mode 0600. |
| Native CLI receipts | The explicit runner passed with actual Node 24.22.1 and pinned Obelisk source: MCP coexistence, successful standard query, large custom-JS observation, MCP retirement, authenticated receipt paging and original-attempt reconciliation. | Custom-JS host effects stay unknown; exactly one original attempt and E1 are retained. |
| Model runner | The compiled explicit runner returned exit 1, `blocked_external`, `complete=false` and `scored=0` when real-model prerequisites were absent. | Verified honest failure reporting only; no sampled model answer or efficacy result. |

The machine-readable [validation record](../evidence/2026-11-15-obelisk-validation.json)
binds executed commands, source heads, toolchain versions, artifact hashes,
exit codes and scope. The earlier [upstream CLI record](../evidence/2026-11-15-obelisk-cli-validation.json)
retains the source compatibility investigation. To avoid concurrent worktree
cache collisions, the final native targets were built in one Cargo invocation
with a temporary codegen profile for workspace packages, then their exact
Cargo-reported executable paths were run directly. This did not change the
repository build configuration or CI lint policy.

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
and `SHENKO_OBELISK_CONFIG` set to the selected real installation. It writes
`blocked_external` and exits unsuccessfully when those prerequisites are
missing. The compiler can check this runner without treating model absence
as a passing test.

Do not mark M08 complete until actual sampling has recorded the candidate,
model, endpoint, prompt digest, corpus, counts, verdicts, token use and elapsed
time. The optional semantic graph and a corpus-scale performance claim remain
unproven. Source roots and the independent upstream source checkout must be
configured on the machine that will query live harness history. Existing
archives are portable without that machine.
