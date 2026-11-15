# Obelisk history delivery TODO

Tracking: [#300](https://github.com/cyjin-yl/shenko/issues/300).
Base: `054817b0d5729fed56ec5a974d101c31edfd1410`.
Upstream audit: `cc75c2dd1aead4f4374758a7536385307934e177`.
Contract: [external history](../specs/external-history.md).

## Implementation sequence

- [ ] **M01 — Complete originals and portable identities.** Immutable source versions, full bytes, positions and observed metadata; byte deduplication distinct from event identity.
- [ ] **M02 — Stable original citations.** Digest and byte-span references, authenticated bounded reads, source loss/offline operation, missing/corrupt states.
- [ ] **M03 — Chinese and long-history retrieval.** Literal full-text search, bounded scans and continuations, match-centered previews, no inherited 10k-character prefix limit.
- [ ] **M04 — Independent Obelisk query bridge.** Explicit providers, fixed source compatibility, composable JS, deadline/cancellation/output budgets and owned process cleanup.
- [ ] **M05 — Product plugin and task wiring.** Existing Node/Cordis/native task authority, live enable/disable, original-attempt recovery, existing memory/cognition writes.
- [ ] **M06 — Replication and recovery.** Existing cognition/CAS closure transfer, independent import idempotency, retained versions, tombstone suppression; no live SQLite/WAL replication.
- [ ] **M07 — Traceable structural view.** Source/version distribution, bounded coverage, unknown freshness and direct drill-down. Semantic graph efficacy remains a separate, unproven extension.
- [ ] **M08 — Evidence and efficacy.** Deterministic mechanisms and actual pinned upstream CLI; opt-in real-model comparison with identical corpus and budgets. Model results are incomplete until a real endpoint has run it.
- [ ] **M09 — Reviewable delivery.** Contracts, Why, source/license provenance, implementation map, focused verification and PR.

These items are one coherent optional-plugin increment. They do not depend on
merging unrelated open PRs #298 or #299 and do not spend separate test quotas
per commit or agent. The initial new-test budget is seven independent
scenarios: three archive, two upstream runtime, one lifecycle and one opt-in
model scenario.

## Reproduce the mechanism checks

Use the repository toolchain and one shared build directory/lock when agents
share a host. The targeted commands, after integration, are:

```bash
cargo test -p shenko-exec-host external_history::tests --lib
SHENKO_OBELISK_EXECUTABLE=/absolute/path/to/node SHENKO_OBELISK_SOURCE_CHECKOUT=/absolute/path/to/audited/obelisk cargo test -p shenko-exec-host obelisk_runtime::tests --lib -- --nocapture
cargo test -p shenko --test obelisk_lifecycle -- --nocapture
python3 scripts/check_kernel_boundary.py --baseline-ref 054817b0d5729fed56ec5a974d101c31edfd1410
```

The process/CLI tests must distinguish a missing upstream checkout from
executed evidence. A skipped or unconfigured invocation is not a passing
compatibility result. The actual run record below is updated only after the
corresponding command completes.

## Current evidence

| Layer | Observation | Scope |
| --- | --- | --- |
| Upstream feasibility | Audited source CLI can run directly with Node 24.22.1. A selected synthetic corpus yielded two sessions/four messages; CJK fallback, message navigation, raw UTF-16 pagination and two-source manifest were observed. | Manual source-CLI probe; integrated runtime tests still pending. |
| Published npm compatibility | `@obelisk-apps/cli@0.3.0` lacks the independent-home and `replay_required` schema expected by the audited source. | Explicit rejection required; equal version strings are not compatibility proof. |
| Model endpoint | The previously configured local/LAN and authorized gateway routes returned refusal or 502/503; the available remote environment's tailnet was logged out. | `blocked_external`; no model success rate, accuracy or token-saving claim. |
| Archive / replication / native lifecycle | Implementation in progress; targeted commands not yet recorded here. | No completed-test claim at this checkpoint. |

## Model comparison and remaining boundaries

The opt-in acceptance harness uses the product's real Store, subject step,
provider, parser and native dispatcher. It compares the same synthetic
cross-session investigation with and without the external-history capability,
under the same model and budget, and retains failed outputs and source
references. A judge uses the existing product sandbox evaluation path; a
hand-written mock answer does not satisfy this gate. Configure the real
Qwen baseline and follow [local model behavior](local-model-behavior.md).

Do not mark M08 complete until actual sampling has recorded the candidate,
model, endpoint, prompt digest, corpus, counts, verdicts, token use and elapsed
time. The optional semantic graph and a corpus-scale performance claim remain
unproven. Source roots and the independent upstream source checkout must be
configured on the machine that will query live harness history. Existing
archives are portable without that machine.
