# Candidate 0c: completed CI and full Nightly

The regression gates for `0c6b013e7c0cc8c27e30a83189c14a36c71aaa74`
completed successfully. The separate real-browser/model batch for this same
candidate still failed all three attempts. These are different acceptance
results; the green gates do not establish daily readiness.

[summary.json](summary.json) is the machine-readable entry. The original API
responses, complete Rust job logs, compiler job and Vitest logs, and selected
controlled-process receipts are retained here. [manifest.json](manifest.json)
records every stored file's length and SHA-256, plus the original uncompressed
identity of each gzip file. All gzip headers have mtime zero.

## Exact source and completed runs

| Gate | Observed result |
| --- | --- |
| [CI 44954542873](https://github.com/cyjin-yl/shenko/actions/runs/44954542873) | All six jobs succeeded: docs, migration verification, Rust, web, Clippy ratchet and kernel boundary. |
| [CI Rust](https://github.com/cyjin-yl/shenko/actions/runs/44954542873/job/136999623452) | Outer workspace harnesses: 1,240 passed, 0 failed, 11 ignored. |
| [Nightly 44954638293](https://github.com/cyjin-yl/shenko/actions/runs/44954638293) | Both full jobs succeeded; the Rust job also ran all three separately provisioned long proofs. |
| [Nightly Rust](https://github.com/cyjin-yl/shenko/actions/runs/44954638293/job/136999918387) | Outer workspace harnesses: 1,240 passed, 0 failed, 11 ignored. |
| [Full compiler](https://github.com/cyjin-yl/shenko/actions/runs/44954638293/job/136999918530) | 183 suites passed, 2 skipped; 2,913 tests passed, 9 skipped; 766.37 seconds. |

The CI checkout was the actual GitHub PR merge
`c956b904aec3d3a9c3e4bfd8f1173a43f314c1f9`, combining this candidate with
`f3fa6f4d8bf22cfd2baf490d9c3a08bd39af4d42`. Nightly checked out the exact
candidate. CI used `--features shenko/compat-poppy`; Nightly used
`--all-features`. Both used `--workspace --locked --no-fail-fast -j2`.

The [dispatch record](dispatch/dispatch-result.json) records one authorized
request at 22:02:33.825578 UTC and its successful acknowledgement at
22:02:36.098909 UTC on 2026-11-21. The branch/PR head matched the exact candidate,
and the preflight found no existing Nightly for it. The first immediate list
was empty; [the later observed run](dispatch/dispatch-run-observed.json)
confirmed the run identity. Both runs completed naturally at attempt 1,
without cancellation or retry by this collector. The complete job metadata is
in [ci-jobs.json](ci-jobs.json) and [nightly-jobs.json](nightly-jobs.json).

## Counts and additional long proofs

Each Rust workspace invocation contained 158 Cargo targets: 125 executable
targets and 33 doctest targets. There were 157 outer libtest result rows and
one `harness = false` real-model target reporting `SKIP`. That skip is not a
pass. The final libtest result in each target is the outer result; its initial
`running N` count was checked. Nineteen earlier subprocess results are kept
separately and are not added to the workspace total. The
[CI audit](ci-rust-outer-audit.json) and
[Nightly audit](nightly-rust-outer-audit.json) retain the per-target details.

Nightly's explicit long proofs are separate invocations:

- The partition/recovery orchestrator passed once in 1.80 seconds. All 17
  checks in its [original receipt](nightly-partition-receipt.json.gz) passed;
  its six child-process results remain separate.
- The restricted SSH helper lost-reply proof passed once in 3.02 seconds.
- The restricted SSH helper wrong-host-pin/helper refusal proof passed once
  in 1.77 seconds. Helper preparation was not counted as another test.

The original scope limits remain in [nightly-rust-summary.json](nightly-rust-summary.json).
The partition proof uses real processes, parser, Store, CAS and archive paths;
it does not use a model or establish a distributed deployment, network
partition, power-loss recovery or elapsed long-run reliability.

## Browser environment and artifact integrity

The compiler used exact upstream commit
`795b1b4958333cf904547e484acb4de9d4b3ee80`. Its original
`scroll-reveal-consumers.test.ts` suite reported all 28 tests passing in
8,774 ms. [compiler-browser.json](compiler-browser.json) records non-root
UID 1001, Node 24.22.1, Chrome 157.0.8103.78 and Playwright 1.58.2 on runner
image `ubuntu24` / `20261116.376.1`. Chromium's sandbox was enabled; PID and
network namespaces and seccomp-BPF were observed. The full suite was run
without adding skips or changing assertions.

Compiler artifact `14727695295` was downloaded in full: 1,152,058 bytes,
SHA-256 `a1e356591ba724b0835472c53c617cc7cc4803097bd95a293b208583259fffd0`.
[compiler-summary.json](compiler-summary.json) includes the independently
verified artifact identity, extracted log identities, and the original
`web/inputs.json` length/hash and `pin` receipt. The complete inputs file
remains in the original artifact and recorded remote evidence directory.

Native artifact `14727882306` is 1,114,255,902 bytes. Its full-ZIP SHA-256
`734c9998fd32693532762965534684c5df0dfccb9af7ccaa5050d6c83c99d6dc`
is GitHub metadata; this collector did not download or independently hash
that whole ZIP. Four bounded range requests read 1,212,292 bytes to obtain
the ZIP directory and three selected process-fixture members. Their CRCs
and individual SHA-256 hashes were verified. The
[selection manifest](start-boundary/selection.json) and
[artifact metadata](start-boundary/artifact-metadata.json) preserve that scope.

## Controlled process success does not erase the earlier failure

The selected [result](start-boundary/result.json) reports the actual
`public_start_pause_survives_process_reopen_and_resume_alone_admits_inference`
fixture passing, with real transport EOF and no model completion claim.
Its [order receipt](start-boundary/recovery-stop-order.json) records:

- Original POST held at 0 ms and Node authority changed at 33 ms.
- The original request cancellation marker observed and SIGINT sent at 134 ms.
- Host stop latched before writer release at 135 ms.
- Original transport ending confirmed at 147 ms.
- Original error and Unknown retained without reconstruction at 179 ms.

These times describe this controlled fixture. They are not an observed
deployment shutdown bound. The runtime log is preserved as
[recovery-stopped.log.gz](start-boundary/recovery-stopped.log.gz).
The [earlier 5f Nightly failure](../ci-nightly-5f1e426a/README.md), which did
not reach its SIGINT phase inside the three-second marker window, remains a
failure. One successful 0c run does not demonstrate that its underlying race
was eliminated; later source fixes require their own receipts.

The separate original [three-attempt browser/model batch](https://github.com/cyjin-yl/shenko/blob/2147750c7e62062ccf5d8bacb7a832c9cf5f278b/docs/evidence/daily-readiness-20261122/final-0c6b013e/batch-summary.json)
remains 0 passed / 3 failed. This directory neither reruns that batch nor
reclassifies its results. Collecting these completed receipts started no
Cargo execution, model request, workflow retry or live database inspection.
