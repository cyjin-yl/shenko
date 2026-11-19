# Integrating the actual PR 303 base and main

This is source integration and metadata evidence. It does not establish a
passing three-process journey, a new model result or real channel acceptance.

## Source and reason

The first parent is public candidate
`e34efd1fed758bb2feeb5abb2ee6e50a656585b7` (tree
`2ce8aec2bed7922796db601cf13aec6eed71a7bb`). The second parent is the actual
PR 303 branch head `bd30bee40dcd9533eb123a7a5dfc48a9cdc788f5`, which already
contains main `be7d09df2166679bc876a243004f057c602d0aae`. A normal merge
preserves both histories and their original authors. Their common ancestor
is `09cb667a79cc2e2212b8d60afcef97cee7e165f9`.

At 15:46:18 UTC the GitHub REST PR response still named the old base
`a574705c` and reported `mergeable=false`, `mergeable_state=dirty`, with no
merge commit. The exact-head workflow query returned no runs. These were
observed API values, not evidence of a failed test. Subsequent direct Git
ref observations confirmed the actual base `bd30bee4` and main `be7d09df`.
The original decoded REST body and the later complete `git ls-remote`
output are retained below. No duplicate workflow was dispatched and the
stacked PR relationship was kept.

## Conflict decisions

Ten files had merge conflicts. Each hunk was reviewed; no whole-file
ours/theirs replacement was used.

| File | Retained integration decision |
| --- | --- |
| `bin/shenko/src/cli.rs` | Keep the accurate run deadline description including compilation and mount setup; the existing 60-second default is unchanged. |
| `bin/shenko/src/cli_dispatch.rs` | Keep the candidate's `state` naming and its existing shared `RuntimeState` owner. |
| `bin/shenko/src/runtime_lifecycle.rs` | Keep `active_authority`, `authority_heartbeat.take()` and the owned heartbeat join before supervisor teardown; preserve observation errors and final authority classification. |
| `bin/shenko/src/runtime_start.rs` | Keep the matching fields, cancellation drop guard, real branch selection, authority activation and durable startup pause before consumers. |
| `bin/shenko/src/policy_config.rs` | Retain the base's more specific module description; the implementation is unchanged. |
| `crates/shenko-broker/src/broker.rs` | Keep `cause_report_attempted`; a reporting attempt is not a durable write acknowledgement and does not release Unknown occupancy. |
| `crates/shenko-broker/src/execution.rs` | Preserve the same fail-soft reporting distinction and the original result/stream settlement ownership. |
| `crates/shenko-store/src/cognition.rs` | Preserve exact table-catalog local receipt projection and original source sequence provenance. |
| `crates/shenko-store/src/store.rs` | Preserve the observed capture refusal wording and the original SQLite foreign-key diagnostic; an unclassified deferred violation is not automatically catch-up debt. |
| `docs/IMPLEMENTATION_DOCS_MAP.md` | Retain one browser journey link and the new named-consumer traces. Qualify the new report/FK trace against the retained implementation, without editing migrated contract text. |

The new `78f32e45` shutdown changes are retained. Context policy retirement
waits directly on the already-owned tool and policy Fiber disposal barriers.
The dispatcher treats only the precise runtime-admission-canceled refusal
as normal completion when its subject is already stopping; other authority
conflicts and storage errors remain errors. Heartbeat cancellation and join
remain independent, and a heartbeat failure still reaches final teardown
classification.

Direct disposal no longer runs `PluginRegistry::uninstall`'s global
`check_outcomes` after each local retirement; these two call sites are not
claimed to be equivalent. A disposal failure still reaches the watcher's
result and its `finished_rx` ownership effect. The global diagnostic check
remains in the owned `PluginRegistry::stop` operation, after
`Context::close_plugins().await`. `Supervisor::stop` first waits for current
hook quiescence, then awaits that operation. Normal `runtime_lifecycle::run`
preserves its cleanup error before `finish_result`. The existing authority
reconstruction branch has a 15-second observation timeout and logs cleanup
failure while retaining its original reconstruction result; it does not
establish successful global cleanup.

The mainline channel changes are also retained: iLink refuses a description
without the conversation's required context token, and NapCat's live
proactive reachability remains explicitly unverified. No live account was
used by this integration.

## Checks and retained boundaries

Before adding this evidence, the checked source tree was
`f2ac3275816adebb19efaec0b7c61a8818670b95`. Its only differences from the
first parent are seven files listed, with SHA-256 values, in
[source-comparison.json](source-comparison.json). The Store and broker
directories, runtime authority/start/lifecycle, Node sync, ToolExec,
`component_process` journey, workflows and all previous evidence were
verified byte-identical to the first parent. The only later additions are
this evidence and its index entry.

- `cargo check --offline --locked -p shenko --all-targets -j1`: exit 0 in
  1m46s, using the shared Cargo lock, existing toolchain/target directory,
  disabled incremental compilation and debug information. This checks
  metadata; no test binary or hosted journey was executed.
- `scripts/check_docs.py --verify-migration`: exit 0, `errors=[]`, including
  all 365 preserved migrated contract chunks. No manifest/checksum baseline
  was changed.
- `git diff --cached --check`: passed before commit.

The complete metadata log is retained as lossless gzip. The merge command's
original conflict output, full direct Git ref observation, source comparison
and original API observations are also retained. Their bytes, SHA-256
digests and any reversible compression are described in
[observation.json](observation.json).

The corrected three-resident process assertions still require execution on
the next exact candidate. The earlier `9b5a6cb` membership failure and its
passing Store/SSH/startup results remain in their original observation;
they are not relabeled as results of this merge. The separately developed
context-capacity and native-continuation increments are not part of this
source integration.
