# Hosted readiness failures and their correction — 2026-11-15

This record preserves the real failures at published Shenko candidate
`7fe3a742d45814245bb8f91ac509bbc8d5187ed0` and the bounded corrections they
required. It supplements, rather than replaces, the source-specific evidence
in [the continuation record](readiness-continuation-20261115.json).

## Actual failed candidate

[CI run 43742465219](https://github.com/cyjin-yl/shenko/actions/runs/43742465219)
executed all six jobs: `kernel-boundary`, `docs`, `docs-migration` and `web`
passed; `rust` and `clippy-ratchet` failed. The merge candidate
`8b7620943574515e0005d7cc9adb49ab027f21eb` had the same tree as the published
head and parents `054817b0d5729fed56ec5a974d101c31edfd1410` and that head.

[Nightly run 43742489632](https://github.com/cyjin-yl/shenko/actions/runs/43742489632)
also executed on this head. The all-features workspace suite failed on the
same MCP cases. Later real process partition/recovery, normal CLI build and
both native helper proof steps actually succeeded; those successful steps do
not turn the workspace failure into a passing job. The full compiler job also
failed, with its independent cause below.

## Canonical request identity at the send boundary

The unchanged `shenko-compat-mcp --test plugin_lifecycle` suite reported
**15 passed / 4 failed** in both hosted workspace runs:

- `sandbox_execution::a_joined_subject_failure_retires_its_owned_sandbox`
- `sandbox_execution::deferred_unknown_e1_is_reselected_and_consumed_without_settlement`
- `sandbox_execution::failed_mcp_t2_reaches_a_committed_continuation`
- `sandbox_execution::unknown_mcp_e1_reaches_a_committed_continuation_without_t2`

The retained SQLite/CAS state showed the ordinary producer's legitimate
`active_request_id = original request` and `active_attempt_id = None`. The
canonical Armed request row already held its exact attempt identity. Requiring
the optional step pointer to be present refused the **first** physical model
send; the error path then retained an inconclusive outcome. The assertion
mentioning replay did not establish that a provider request had been sent.

The correction binds dispatch to the canonical request row. A shared check in
preflight and the final `BEGIN IMMEDIATE` transaction requires the active head,
`AwaitingModel`, the exact step/request, and the same frozen input plan. A
present step attempt pointer must still match. The final transaction re-reads
`Armed`, validates authority and changes the state once. No producer migration,
new attempt, replay allowance, Unknown settlement or test expectation change
is involved.

The existing complete suite ran on source
`de3cc83e1c43d0e9b5b3687b27f479dfbdcfb926`:

```sh
cargo test --offline --locked -j1 -p shenko-compat-mcp --test plugin_lifecycle -- --nocapture --test-threads=1
```

**19 passed / 0 failed / 0 ignored / 0 filtered**, test execution **24.97 s**.
It used the shared heavy-build lock, Rust 1.98.1, one build job, disabled
incremental/debug output, and retained fresh compatibility artifacts. Log
`mcp-request-binding-tests.log` SHA256:
`b233712e817ac7f37ede9468aee318e498ac5c5388ce301666e50dea387adb6d`.
This is the focused local source result; the final integrated hosted gates
remain separately required.

## The one new Clippy diagnostic

Artifact `14173166429` recorded exactly one ratchet increase:
`all-targets: shenko-node: clippy::unwrap_used increased by 1`. Production
targets and all other packages had no increase. The existing retained-control
row test now returns `Result<()>` and propagates the newly added lookup with
`?`, retaining all assertions. No lint allowance or baseline change is made.
The final full-workspace ratchet must execute again.

## Full compiler resource policy

[Compiler job 132898489584](https://github.com/cyjin-yl/shenko/actions/runs/43742489632/job/132898489584)
used exact Proto-UI pin `7521cd501f454b4e013ec6e9656fb4b6e4951514`, Node
**24.22.1**, pnpm **10.32.1**, and actual Chromium **156.0.8078.79** on Debian
12. All **76 files / 1515 tests** were collected: **75 files passed / 1 failed;
1513 tests passed / 1 failed / 1 existing capture-stage skip**, in **178.05 s**.

The sole failure was
`native-support-migration.test.ts > actual CLI native-support directory migration > retains a filesystem baseline with one byte-identical legacy support instance`:
**5199 ms** against the unchanged **5000 ms** deadline. The file took
**14376 ms**. The strict pointer graph and explicit-undefined boundary files
passed all their tests.

The test, historical fixture, helper and Vitest configuration are unchanged
between old pin `41282cb0` and this pin. The test serially creates six component
directories, verifies original bytes and runs three real bundle/import/DOM
journeys. Vue tick loops are bounded and cleanup is explicit; there is no
sleep, network request or TypeScript typecheck. The previous same-tree Box
full suite used one worker: this test took **2611 ms**, its file **7328 ms**,
and all 76 files passed with **1514 passed / 0 failed / 1 existing skip**.
That run used Node 24.21.0 and Chrome 157.0.8103.38, so it cannot substitute
for the failed hosted environment. Resource contention is consistent with the
timings, but absent CPU and per-stage measurements it is not established as
the sole cause.

Following the upstream testing method's instruction to control heavy-suite
concurrency before changing deadlines, the Shenko Nightly command becomes:

```sh
pnpm exec vitest run packages/compiler --maxWorkers=1 --minWorkers=1
```

This changes only the file-worker pool. It retains the exact dependency pin,
all files/assertions, real Chromium, the original test deadline, and the
existing capture-stage skip. No upstream source, test expectation or timeout
is changed. A later success validates this explicit resource policy; it does
not retroactively validate the old default pool.

| Preserved original | Bytes | SHA256 |
| --- | ---: | --- |
| Compiler artifact `14173276733` ZIP | 65834 | `a9c9abea5a11ab26afd6d117a57130640f3980b85902e5fdb54c11c8e911f005` |
| Full compiler test log from that artifact | 576020 | `d701377b7c0a3f222b8dc53542ee233a4a32877713576a3e68a3d2ce2e3340ea` |
| Full compiler job log | 940966 | `63a1051909bcf2e496468a74ef9a5dddcc7ba7095b74cde58976b03d2a81b8ea` |

## The next full-workspace result: generic external T1

Published candidate `d6aba9500cacce9d671c6102146ccfc3fc3f8201` incorporated the
three corrections above. Its [CI run 43744064517](https://github.com/cyjin-yl/shenko/actions/runs/43744064517)
completed **5 successful jobs / 1 failed Rust job**. The complete Clippy ratchet
had `errors=[]`, unchanged baseline `054817b0d5729fed56ec5a974d101c31edfd1410`,
and Node all-target `unwrap_used` **421 → 421**. The hosted MCP lifecycle
suite now actually passed **19/19**, including all four former failures.

Both that CI and [Nightly run 43744091245](https://github.com/cyjin-yl/shenko/actions/runs/43744091245)
then reached the `shenko-exec-host` library: **33 passed / 18 failed**.
Sixteen failures reported `Query returned no rows`; two runtime-error cases
reported `task_owner_missing`. The older `7fe3a742` workspace run stopped at
MCP before reaching this target. These newly visible failures therefore are
not evidence that the narrow optional-attempt correction introduced them.

The cause was two unconditional subject/owner lookups newly added to generic
external T1. This existing generic API also serves trusted executors with a
durable Running task and exact invocation/revision/abort/eligibility/Armed
checks, without requiring a subject runtime. The correction removes only
those unconditional duplicate lookups. Native dispatch and `NATIVE_CHILD`
continue to validate the real subject, branch, generation and owner in both
preflight and the final transaction. Authority-bound generic tasks still
reserve their complete task ancestry and recheck it in the final transaction;
a retained required-policy marker without an attached provider still refuses
new control. No native owner/branch check or test assertion is loosened.

On correction source `851a60b8853bbbbb5671c1e7f4c2818717ac97f9`, the existing
complete exec-host library ran with the same all-features selection:

```sh
cargo test --offline --all-features --locked -j1 -p shenko-exec-host --lib -- --nocapture --test-threads=1
```

Result: **51 passed / 0 failed / 0 ignored / 0 filtered**, **23.41 s**. All
18 former failures passed without changing their fixtures or assertions. The
shared build lock and one-job environment were used. Log
`exec-host-unbound-t1-tests.log` SHA256:
`0e7ad16fbf84c1c2b3f953235a45539ea9d4ed4584eaaa4453d1dd4e65e85ff1`.

The same source also executed the existing
`runtime_authority::tests::reserved_store_commit_and_ancestor_cancellation_fence_live_owners`
with `--exact`: **1 passed / 0 failed / 0 ignored / 39 filtered**, **1.47 s**.
Its existing assertions verify ancestor cancellation fencing, refusal of new
control after reopening without the required adapter, and preservation of
late original E1. Log `unbound-t1-authority-fences.log` SHA256:
`a11117523c5ae2b65be94a5aad58f7b440ea8158ad246ab2c82b957fb604cf36`.

The workspace commands now use `--no-fail-fast` to retain outcomes from later
targets and doctests even when an earlier target fails. Cargo still returns a
failure if any test target fails; no test selection, ignore or deadline changes.

### Compiler resource-policy result on the same candidate

[Compiler job 132903459658](https://github.com/cyjin-yl/shenko/actions/runs/43744091245/job/132903459658)
actually succeeded on `d6aba950`: **76/76 files, 1514 passed / 0 failed /
1 existing capture-stage skip**, **250.45 s**. The original timeout test ran
in **3109 ms**, its file in **7667 ms**, with the original **5000 ms** deadline.
Exact pin `7521cd501f454b4e013ec6e9656fb4b6e4951514`, Node **24.22.1**, pnpm
**10.32.1**, Chromium **156.0.8078.79** and the one-worker flags were confirmed
from the original log and inputs. This is a successful full compiler result,
not a successful whole Nightly result or a retroactive pass for the old pool.

The native job's later partition receipt was **passed, 17/17,
failed_checks=[]**, schema 7; the selector executed **1 passed**. Each of the
two helper selectors also actually executed **1 passed**, with matching
successful result/proof receipts. The earlier workspace failure remains.

| Preserved `d6aba950` original | Bytes | SHA256 |
| --- | ---: | --- |
| Compiler artifact `14172609952` ZIP | 60797 | `55b1aa7ae92dbb66338ec3a5f708924c5d5d94b23fa8cfc05476db45d21769c2` |
| Full compiler test log | 557162 | `3e27d2306dc4e5ce8f3b39cd03000be8d1dcb8dd42bf50aa3d13269869d5bba9` |
| Native artifact `14173702138` ZIP | 8209811 | `4a9e388d224dca44559cd749af8054baeb25c990d35ca00483bd7f86a5164ef1` |
| Full native job log | 507913 | `35f7e9f712eb1cf3f4625f0d340a1d27f9a8d30e26040a05307312e393735725` |

## Publication and final evidence

The corrected source must execute all six CI jobs and both explicit Nightly
jobs on its published head. Their actual run URLs, results, retained artifact
contents and candidate identity are recorded in the PR progress comment.
Focused local success, a skipped job or a success on an older candidate cannot
stand in for those gates. This checkpoint records the corrections and local
MCP result; the corrected hosted results were not yet available when it was
written. The remaining product boundaries in the continuation record remain.
