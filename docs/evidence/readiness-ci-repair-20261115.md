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

## Publication and final evidence

The corrected source must execute all six CI jobs and both explicit Nightly
jobs on its published head. Their actual run URLs, results, retained artifact
contents and candidate identity are recorded in the PR progress comment.
Focused local success, a skipped job or a success on an older candidate cannot
stand in for those gates. This checkpoint records the corrections and local
MCP result; the corrected hosted results were not yet available when it was
written. The remaining product boundaries in the continuation record remain.
