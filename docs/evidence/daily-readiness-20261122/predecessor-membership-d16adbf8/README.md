# Frozen predecessor membership: original failure and scoped repair evidence

This receipt preserves the closed `d16adbf8` attempt 003 diagnosis, the failing
baseline fixture at `6d087562`, and the successful scoped tests at `95cdc272`.
The original three real-model attempts remain **0 passed / 3 failed**. These
scoped results do not regrade that batch or establish a successful replacement
model journey. The machine-readable result is [summary.json](summary.json).

## Source and execution boundaries

| Evidence | Exact source revision | Actual result |
| --- | --- | --- |
| Original model batch | `d16adbf89c9f79dfce8ef90d77843412bcda85bf` | 0/3; original public receipt at `c40c61bc4dd2a2ca850d2517ddc9a3e2832788e7` |
| Baseline fixture | `6d087562cc33928fa3a6a3fecd012cc5081dc7c3` | 0 passed / 1 failed; Cargo exit 101, host exit 1 |
| Fixed scopes | `95cdc272866a5f31cedb790d263761523e9b0e2c` | 10 passed / 0 failed across 3 targets; host exit 0 |

The baseline is a direct child of the original model source and adds only the
new regression function in `store_roundtrip.rs`. Product files are unchanged.
The fixed source is its direct child, changes five product/archive-test files,
and leaves the complete baseline `store_roundtrip.rs` bytes unchanged. The
immutable Git comparison, tree identities and patch hashes are recorded in
[source-identities.json](collection/source-identities.json). Later live
checkout changes are not substituted for these earlier source identities.

Both scoped runners used the actual project devcontainer image
`sha256:28df77cc6beee4f88ee1f57922190dcb02414d78d284e8b0f91859e18728e565`,
Rust/Cargo 1.98.1 and Node 24.22.1. The original host runners, version files,
commands, logs, summaries and exits are retained. This collection did not run
the tests again or start a model, release build or workflow.

## Actual before and after results

| Source | Command target/filter | Passed / failed | libtest duration | Full command elapsed |
| --- | --- | --- | --- | --- |
| `6d087562` | `frozen_control_predecessor_keeps_original_optional_object_membership`, exact | 0 / 1 | 5.40 s | 9.505337459 s |
| `95cdc272` | Same exact fixture, unchanged test bytes | 1 / 0 | 6.14 s | 28.788921788 s |
| `95cdc272` | `canonical_direct_native_chain_retains_original_e1_and_only_one_effect`, exact | 1 / 0 | 187.12 s | 187.587394031 s |
| `95cdc272` | `cognition::archive_admission_tests::`, library filter | 8 / 0 | 89.15 s | 104.223714216 s |

The two exact integration commands use `-p shenko-store --test store_roundtrip`;
the archive filter uses `-p shenko-store --lib`. All use
`cargo test --offline --locked --all-features -j4` and `--nocapture`.
[The original baseline summary](baseline-6d087562/summary.json) and
[fixed summary](fixed-95cdc272/summary.json) preserve the complete argv arrays.
Each of these four logs contains one outer libtest result and no nested
harness summaries. Filtered-out cases are not counted as executed tests.

The [baseline log](baseline-6d087562/predecessor-membership.log.gz) preserves
the actual failure:

```text
Error: late optional publication must not grow an already frozen predecessor

Caused by:
    conflict: ordered action predecessor: snapshot object closure is incomplete
```

The fixed run does not loosen that assertion. It executes the same new fixture,
the existing original-E1/single-effect/reopen native chain, and all eight
selected archive tests. Their original logs are retained separately, including
warnings; the receipt does not characterize those warnings as pre-existing.

The baseline host's whole-second timestamps span 00:26:43–00:26:53 UTC on
2026-11-22 (10 seconds); the remote tool reports 11.78 seconds. The fixed host
timestamps span 00:38:13–00:43:35 UTC (322 seconds); the remote tool reports
322.72 seconds. These host, command and libtest timings have different scopes
and precision. Original tool completions are retained in
[baseline-completion.tool.json](collection/baseline-completion.tool.json) and
[fixed-completion.tool.json](collection/fixed-completion.tool.json).

## Closed original attempt 003 diagnosis

The original runtime failed with `snapshot object closure is incomplete` and
then a chosen action without a local applied marker. Its parent exited 1.
The original cleanup record shows a reaped parent and empty owned process
group; it does not call that nonzero product exit a clean stop. These facts
remain in the original model receipt and the diagnostic
[manifest](closed-d16-attempt003/manifest.json.gz).

After that attempt was closed, the root investigator compiled the retained
[inspect.rs](closed-d16-attempt003/inspect.rs) against the **already completed
d16 release rlib**, then invoked the product's `roots::object_closure` on the
original predecessor's roots. The inspector's compile and run exits were both
0. The original source, runner, compile log, library/fingerprint identities,
selected original payload and predecessor JSON are retained. No model,
Store/SQL open, repair, root publication, effect or replay was performed by
that diagnostic.

The [original observation](closed-d16-attempt003/observation.json.gz) records
47 root entries and a frozen expected object count of **238**. The product
reader over the closed CAS returned **239** objects with no missing objects
in each of three orders: original tree read order, reverse order and sorted
typed references. All three digest sets are identical. This rules out a root
enumeration order explanation for that closed-installation count difference;
it is not a snapshot of the complete CAS at the original capture instant.

The selected [late-object record](closed-d16-attempt003/late-closure-objects.json)
contains the exact 1,304 bytes of a finite model-child frozen input, with SHA256
`96314da5b6cf982d165349b7475c1f1b9f517c6e488df2643b8dd4fe500a454c`.
Its recorded mtime, 00:05:06.981033 UTC, follows the original predecessor
manifest's `created_at`, 00:05:03.354560199 UTC. The retained
[parent edge](closed-d16-attempt003/late-object-parent-edges.json) points to it
through an existing bare `contract_digest` field; the parent's original JSON
bytes are retained under `parents/`. File timestamps are recorded evidence,
not a monotonic publication trace. The separate actual baseline fixture
reproduces late optional CAS publication expanding a previously frozen
predecessor, and the unchanged fixture passes with the format fix.

The diagnostic compared names, sizes and mtimes of 357 CAS files before and
after, and those metadata maps are identical. It **did not hash every CAS
file before and after**. Its separate check of 29 original nonblocking gaps
found none present in the closed CAS; that check is explicitly a later
existence observation, not a reconstruction of earlier runtime time. The
selected-parent search excludes large opaque objects. The diagnosis does not
establish that every possible closure failure has this cause.

The fix freezes complete digest membership for optional bare references in
control-predecessor codec 2 and archive schema 13. Required typed children still
have to exist and verify. It adds O(object_count) root metadata and does not
remove the full graph/snapshot validation or history-growth costs. The original
codec-1 and archive-through-12 evidence remains evidence of those original
semantics; it is not rewritten by this scoped success.

## Retention and verification

[retention-index.json](retention-index.json) maps **46 original files** to their
remote paths, original lengths/SHA256, stored lengths/SHA256 and source stat
values before/after collection. All source stats were stable during copying.
Nine originals are stored as gzip with mtime 0; decoding restores their exact
original bytes. The other originals are copied unchanged. The four actual
scope logs were independently parsed again and matched their original summary
rows; this is receipt verification, not a second test execution.

The one collected transfer archive was 78,166 bytes, SHA256
`09a64a68fae3adff7d2ed907df10399aff78e98010879293a82ada22d9e404f3`.
The collector source and actual completion are retained in
[collection/](collection/). The index and
[file-manifest.json](file-manifest.json) distinguish retained originals from
collection records and derived descriptions. The latter manifest covers every
file in this directory except itself.

The collector only copied already closed evidence directories. It did not open
an installation database or execute a new CAS inspection. The large diagnostic
executable, release/dependency rlibs, complete CAS, installation databases and
native-artifacts directories remain on the authorized device; their absence
from this compact public receipt is explicit. Original diagnostic rlib hashes
are preserved as its recorded metadata, not claimed as a new local rehash.
