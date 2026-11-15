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

## Full workspace result on `3c7c6af` and recovery correction

Candidate `3c7c6afef4a2116e3e4be2cdf80f723996751264` ran both workspace commands with `--no-fail-fast`, retaining a failing exit code while reaching every later target and doctests. [CI 43745682771](https://github.com/cyjin-yl/shenko/actions/runs/43745682771) passed its other five jobs; the complete Clippy ratchet had `errors=[]` against unchanged baseline `054817b0d5729fed56ec5a974d101c31edfd1410`. Its merge checkout `c0a72a26369302f8667d07c9715359f97aa94160` had the published head's tree, `881a68d436b7835f2da27dcd11f85c91a70a3c4a`.

[Nightly 43745690980](https://github.com/cyjin-yl/shenko/actions/runs/43745690980) failed on exactly the same three Rust targets:

| Target | Actual result | Cause and correction |
| --- | --- | --- |
| `shenko-exec-ssh --lib` | 79 passed / 13 failed | The fixture named the authority decision as its task source but initialized a different genesis step. Register that exact source through the existing API; retain production fences and negative permission assertions. |
| `shenko-store --test store_roundtrip` | 28 passed / 4 failed | Recovery had become separately committed tasks and skipped cancelled work; dispatch checked currentness before original identity. Restore one fenced recovery transaction and parked cleanup. The superseded-authority fixture now updates its head and registered successor coherently. |
| `shenko-subject --lib` | 109 passed / 1 failed / 7 existing ignored | A test invoking the current writer still asserted schema 6. Name and assert its genuine schema 7; retain exact reopen, replay, relay and settlement assertions. Historical two-binary schema 5/6 proof is unchanged. |

The previously failing exec-host suite passed **51/51**. The full pinned compiler passed **76/76 files, 1514 tests, 0 failures, 1 existing capture-stage skip**, in **284.44 s**. Its original filesystem baseline took **2716 ms** against the unchanged **5000 ms** deadline. Native partition receipts passed **17/17**, and the partition selector and both helper selectors each actually ran and passed one test. These separate successes do not override the workspace failure.

### Recovery and admission safety

Recovery now freezes the complete paginated candidate set, subject heads, selected rows, ancestors and cleanup disposition in one read snapshot. It releases the Store read lock, obtains one source admission, then re-scans and compares the entire snapshot in the final Store `BEGIN IMMEDIATE` transaction before its first write. A corrupt late page, concurrent insertion or changed ancestor/head publishes no prefix. Selected rows and their local materialization receipt commit together; the in-memory completion flag follows COMMIT.

Same-branch cancelled tasks and tasks with inactive ancestors retain their checkpoint, cancellation and original attempts as Pending/Parked cleanup. Ordinary admission still requires active ownership; foreign-branch tasks remain untouched. Source-bound batches require one subject branch. Oversized descriptors are refused before durable acceptance, never split into independent commits that would lose atomicity.

Review found a provider-attachment race: an operation could first observe no provider, then reread a newly attached provider and obtain a stamp without its quorum admission. No-provider fast paths now capture immutable unbound reservations. If attachment commits its durable required policy first, the old operation is fenced by the final transaction; otherwise the unbound transaction linearizes before attachment.

Ordinary task JSON identity is checked against its lookup ID; ancestor identity and subject are checked separately. Both dispatch preflight and the final transaction check original operation/attempt identity before historical markers and current invocation. The existing journey holds invocation and task revision constant after `Won`, changes only epoch, and requires rejection. This distinguishes the former marker-first early return without another test matrix or sending opportunity.

### Focused evidence and source identity

Integrated behavior checkpoint: `35ddedcee0c64e5ecb3554de71ca726ae8dc2b23`. The continuation JSON records exact commands, integrated commits and source-file digests.

| Check | Source consumed | Actual result | Log SHA256 |
| --- | --- | --- | --- |
| SSH library | `07d4909bb7f36b6b7195de63647201b44bad0b45` | 92 passed / 0 failed / 0 ignored / 0 filtered | `a25e98bee6d2675762db7064336a6df9b5518d91619fe51d6c92437be70d6dbf` |
| Schema 7 exact settlements | `be2400cd74ece1657a49b43fa4325e9f88b9e5ae` | 1 passed / 0 failed / 0 ignored / 116 filtered | `f3cf8703dd205a8593800ec88d4386ffce180bc8919cd531ab9f83742392c09d` |
| Store roundtrip | `d896241a779004d9f586c36f81cd0153821504c6` | 32 passed / 0 failed / 0 ignored / 0 filtered | `e26f2fe5e0f95ce0a904dca90ef0f0e491f5e8622234b54f84e675426660c7e1` |

The schema selector began at `be2400c`; only the unrelated SSH fixture was integrated as `329a2cf` during execution. Subject/Store inputs were unchanged. Its genuine schema 7 archive SHA256 is `ab081600ff2084eaab79770d09ee8758c9ca48d1c2822e5e6a5eb23996b16c66`. An earlier invocation received `SIGKILL` compiling `shenko-types`, before tests ran; that failed log is retained and its cause was not established.

Store all-targets/all-features Clippy at worker source `d896241` exited zero, with **zero diagnostics on newly added lines**, retaining existing warnings. JSONL SHA256: `8d25258377fa90a912e009d7ea6e32939e6c00c6583e00ffe9dbe7db3085f9ec`; added-line audit SHA256: `3a91e0188f51f317262d7f02d2ef3e3679267753d66c431d08c3786cf4d4ad81`. This targeted result does not replace the complete hosted ratchet. Independent read-only review of all three Store commits and the final epoch assertion found no remaining blocker within this scope.

| Preserved `3c7c6af` original | SHA256 |
| --- | --- |
| CI Rust job log | `5f341ad73b3dc988d3b6b5d689b198e47c27d648c1b94f25a49ae0497e893477` |
| Compiler artifact `14174710957` ZIP | `b47828a51197495fa5ca830c222a7f8724e26b9d5b89b0f3d57cc6fe35d364e3` |
| Compiler test log | `390489193987dd13aabc63da3bfb68e75dc455eae569ed5443958064ede231de` |
| Native artifact `14174356687` ZIP | `062c014d0879cec08bff5a5bc5fbdaef92bc712eba661821e6bc56f3390429f1` |
| Nightly Rust job log | `5d6e0d3fa0ee7c4e0c10c02a3d52040240ebd674fcfa007e000ddf02855d6a8f` |

## Concurrent upstream merge

A fresh pre-push branch-ref check found that main had merged [Obelisk PR #301](https://github.com/cyjin-yl/shenko/pull/301) as `bfc3d04de445968dcaed3f40cb8a937da437f61e`, while the PR snapshot still showed the older base. Publication stopped before pushing the staged candidate `5c79ae0`. The new main is integrated with both histories retained. The single textual conflict only added module declarations: `runtime_branches`, `runtime_authority` and upstream `obelisk` all remain. Upstream source/evidence remains its authors' contribution; raw evidence bytes were not normalized or relabeled. The final six CI and two Nightly jobs must execute the merged candidate, with the new main as the independent event baseline.

## Publication and final evidence

The corrected source must execute all six CI jobs and both explicit Nightly
jobs on its published head. Their actual run URLs, results, retained artifact
contents and candidate identity are recorded in the PR progress comment.
Focused local success, a skipped job or a success on an older candidate cannot
stand in for those gates. This checkpoint records the successive hosted failures and focused
corrections; the final integrated hosted results were not yet available
when it was written. The remaining product boundaries in the continuation record remain.
