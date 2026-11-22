# c931 CI passed; full Nightly retained one compiler failure

The completed ordinary CI for source
`c931526c8ae8cd16f3e0cf56d733f1e4275d5bb0` passed all six jobs. Its full Nightly
**failed**: the Rust long-suite job passed, while the compiler job retained one
scroll-position assertion failure. No workflow was retried or cancelled by
this collection. [summary.json](summary.json) preserves both outcomes; these
results do not establish a real-model task journey or regrade an earlier one.

## Exact run and checkout identities

| Run | Event / attempt | Actual checkout from the original Rust log | Final outcome |
| --- | --- | --- | --- |
| [CI 44968265233](https://github.com/cyjin-yl/shenko/actions/runs/44968265233) | `pull_request` / 1 | `4f8b95619ea942df2574775b0b4ff4eb51bdc082` | Six jobs succeeded |
| [Nightly 44968307539](https://github.com/cyjin-yl/shenko/actions/runs/44968307539) | `workflow_dispatch` / 1 | `c931526c8ae8cd16f3e0cf56d733f1e4275d5bb0` | Rust succeeded; compiler failed |

The CI checkout is GitHub's synthetic merge of c931 into
`f3fa6f4d8bf22cfd2baf490d9c3a08bd39af4d42`; its API `head_sha` still identifies
c931. Nightly checked out the exact branch candidate. Both original run/job
responses and the checkout lines are retained. No later branch state is
substituted for these identities.

CI was created at 00:46:36 UTC on 2026-11-22; its final Rust job completed at
01:04:46. Nightly was created at 00:47:06; its compiler job completed at
00:57:46 and its Rust job at 01:06:55. The read-only observer saw both terminal
at 01:07:26.115 UTC and exited 1 because Nightly failed, with no observation
error. Its interval was 60 seconds; these observation times are not exact
workflow completion times.

## Rust accounting

| Scope | Passed | Failed | Ignored | Separate nested results |
| --- | ---: | ---: | ---: | --- |
| Ordinary CI workspace | 1241 | 0 | 11 | 19 passed |
| Nightly workspace | 1241 | 0 | 11 | 19 passed |
| Nightly partition/recovery proof | 1 | 0 | 0 | Six successful child-process harnesses |
| Nightly SSH helper lost-reply proof | 1 | 0 | 0 | None |
| Nightly SSH wrong-pin/helper refusal proof | 1 | 0 | 0 | None |

Each workspace invocation contains 158 Cargo targets: 125 executable targets
and 33 doctest targets. There are 157 outer libtest summary rows and one
non-libtest harness that explicitly skips without a configured model. The 19
nested subprocess summaries are retained separately and are not added again
to the 1241 passes. The explicit model-dependent skip is not a model pass.

The three additional long proofs have their own commands, exact c931 source
lines and outer results. The partition proof took 0.97 seconds in its outer
libtest result and retained a receipt with 17 passed checks; its six process
stages are not six additional top-level tests. The two SSH proofs took 2.03
and 0.86 seconds. Building/installing the normal CLI helper is preparation,
not a fourth proof. These libtest times do not include every surrounding
setup or Cargo operation.

The original [CI Rust log](ci/rust.log.gz),
[Nightly Rust log](nightly/rust.log.gz), per-target audits and partition receipt
are retained. Both logs were parsed again locally using the exact retained
auditor; all target boundaries, line positions, counts, explicit proofs and
validation results matched the collected audits. No test was executed by that
reparse. All five other CI job logs and original metadata are also retained
under `ci/auxiliary/`.

## The actual compiler failure

The full compiler invocation ran at pinned upstream
`795b1b4958333cf904547e484acb4de9d4b3ee80` and reported:

| Compiler result | Passed | Failed | Skipped | Total |
| --- | ---: | ---: | ---: | ---: |
| Test files | 182 | 1 | 2 | 185 |
| Tests | 2912 | 1 | 9 | 2922 |

The recorded Vitest duration is 595.85 seconds. The only failed suite is
`packages/compiler/test/scroll-reveal-consumers.test.ts`; it ran 28 tests and
reported one failure. The failed React DOM case checks rejection of another
surface's item, a sibling, self and another family before physical movement.
At the original source line 155, `surface.scrollTop` was **300** where the
assertion required **50**. Its four preceding rejection-result assertions
had completed; the later successful-reveal assertion was not reached in that
failed case. The original output remains in
[compiler-vitest.log.gz](compiler/compiler-vitest.log.gz) and
[compiler.log.gz](compiler/compiler.log.gz).

The browser prerequisites and sandbox check succeeded before the suite ran.
The original [browser receipt](compiler/compiler-browser.json) records UID
1001, Ubuntu 24 runner image `20261116.133.1`, Node `24.22.1`, Playwright
`1.58.2`, and Chrome `157.0.8103.78`; namespace and seccomp sandbox checks were
enabled. This was an actual test assertion failure, not a browser suite that
could not start. This receipt does not infer its event-loop cause from that
assertion or treat an earlier candidate's green result as a replacement.

The complete small compiler ZIP, artifact `14732462738`, was downloaded and
independently checked against its 1,151,499-byte size and SHA256
`0ab487b6747204ce4626836f216a5d527b7cc4cdfb710dc93c8f441d9cb6bdbd`.
The original browser receipt, full Vitest output, complete `web/inputs.json`,
ZIP member index, API metadata and job log are retained here. The full ZIP
remains on the authorized device. The earlier failure-observation run and
artifact-list responses remain separately under `initial-compiler-failure/`;
their then-in-progress run state is not presented as the final run metadata.

## Selected controlled process evidence

The native artifact `14733018673` is 1,116,931,136 bytes. The collector read its
central directory and three selected `start-boundary` files through four HTTP
ranges totalling 1,217,351 bytes. Each selected member passed ZIP CRC checking
and has its own SHA256. The complete ZIP digest is **GitHub metadata only**;
the complete large ZIP was not downloaded or independently hashed.

The selected original controlled fixture reports `journey_passed: true`,
`model_completion_tested: false` and `remote_completion_claimed: false`.
Its original order record observes cancellation of the held request at 49 ms,
SIGINT at 49 ms, the host stop latch and writer release at 60 ms, transport
end confirmation at 73 ms, and preservation of the original error/Unknown
without reconstruction at 99 ms. These are observations inside the controlled
fixture, not a deployment shutdown-latency promise or real-model completion.
The earlier failed controlled executions remain failed.

## Dispatch, preservation and limits

The original single Nightly dispatch was requested at 00:47:04.822743 UTC and
returned exit 0 at 00:47:07.008571. Its first confirmation returned an empty
match list at 00:47:07.897121. That original empty response is preserved; no
second dispatch was issued. A later exact-head run observation is retained in
`dispatch/dispatch-run-observed.json` and the complete read-only observation
series is under `observation/`.

[retention-index.json](retention-index.json) maps 131 copied source files to
their original remote paths, lengths, SHA256 values and stable source stats.
Seventy-seven are stored with gzip mtime 0; decompression restores their exact
original bytes. The initial failure log is byte-identical to the retained
compiler job log, and the index records that alias without a duplicate copy.
The 4,547,048-byte transfer archive had SHA256
`d2d363524a7fe6b89b5a70971d95f1b2a14f9dcfd5607da6a3856084cf952213`;
all transferred stored and decoded lengths/hashes were verified locally.

[file-manifest.json](file-manifest.json) covers every file here except itself
and distinguishes retained originals from collection records and derived
descriptions. The collection used only read-only GitHub requests and closed
evidence files. It did not run a new test, build, model request, workflow,
repair or installation replay. The failed Nightly outcome remains part of
this candidate's evidence, independently of subsequent fixture fixes,
candidate versions and real-model journey results.
