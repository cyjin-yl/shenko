# Candidate 5f: completed CI and Nightly receipts

Candidate `5f1e426a047390159f038bf58b6d19948ab4ffc9` **failed** its Rust
workspace gates. This directory preserves the original results, including the
additional Nightly process-fixture failure. It does not turn later fixture
corrections or another candidate's results into a pass for this candidate.

The machine-readable entry is [summary.json](summary.json). Original job logs
are preserved losslessly as [ci-rust.log.gz](ci-rust.log.gz) and
[nightly-rust.log.gz](nightly-rust.log.gz); byte counts and SHA-256 values are in
[raw-log-transfer.json](raw-log-transfer.json). Both gzip headers use mtime zero.

## Actual runs

| Run | Observed result |
| --- | --- |
| [CI 44952886247](https://github.com/cyjin-yl/shenko/actions/runs/44952886247) | Five jobs passed; Rust failed. The workspace outer harnesses reported 1,238 passed, 2 failed and 11 ignored. |
| [Nightly 44952995298](https://github.com/cyjin-yl/shenko/actions/runs/44952995298) | Full compiler passed; Rust workspace failed with 1,237 passed, 3 failed and 11 ignored. All three separately provisioned long proofs passed. |
| [Full compiler job](https://github.com/cyjin-yl/shenko/actions/runs/44952995298/job/136994835449) | 183 suites passed, 2 skipped; 2,913 tests passed, 9 skipped; 584.39 seconds. |

CI checked out the actual PR merge
`3d6c1f436ba00082ba132a37217fca574cfa3bf0`. Nightly checked out the exact
candidate. [CI job metadata](ci-jobs.json), [Nightly job metadata](nightly-jobs.json)
and the [single dispatch record](dispatch/dispatch-result.json) preserve these
separate identities. Both runs finished naturally; neither was cancelled or
retried by the collector.

## Counting and independent long proofs

Each workspace run has 158 Cargo targets: 125 executable targets and 33
doctest targets. The final libtest summary within each target supplies the
outer result, and its first `running N` count is checked against that result.
The 19 earlier subprocess summaries remain separate. One configured real-model
target uses `harness = false` and reports `SKIP`; it is not a pass. The complete
per-target records are in [the CI audit](ci-rust-outer-audit.json) and
[the Nightly audit](nightly-rust-outer-audit.json).

The Nightly's explicitly provisioned proofs remain outside those workspace
counts:

- The process partition/recovery orchestrator passed once in 0.99 seconds,
  with all 17 receipt checks passing. Its six child-process summaries are
  retained separately. The [original receipt](nightly-partition-receipt.json.gz)
  has raw SHA-256 `2e7635f0e1af8c9b742e1d35eca4fd6bfb5a1a5b0dfea730d4b6c3e0ebd1ad7f`.
- The restricted SSH helper lost-reply proof passed once in 2.05 seconds.
- The restricted SSH helper wrong-identity refusal proof passed once in
  0.88 seconds. Building/installing the helper was preparation, not a fourth
  test.

These proofs use their actual process, Store, CAS, archive and restricted
helper paths. Their recorded limits remain in [the Nightly summary](nightly-rust-summary.json);
they do not establish real-model behavior or elapsed long-run reliability.

## Original failures remain failures

Both workspace runs failed the first-claim negative fixture and the native
dispatch fixture. The first-claim assertion expected the former refusal
boundary, while the new selector refused earlier with `native read selectors
differ from the original action plan`. The native dispatch fixture returned
`runtime_authority_fenced`. Their raw failure contexts remain in the job logs.

Nightly additionally failed
`public_start_pause_survives_process_reopen_and_resume_alone_admits_inference`.
Its error was `original request cancellation was not observed before cleanup
SIGINT`. The [original order file](start-boundary/recovery-stop-order.json)
records only a held inference POST at 0 ms and the authority change/process
continuation at 31 ms. The subsequent three-second marker wait did not reach
the fixture's SIGINT phase. The [retained runtime log](start-boundary/recovery-stopped.log.gz)
has neither the request-cancellation marker nor the host SIGINT marker.

[result.json](start-boundary/result.json) preserves the failed outcome and
the later transport ending. The [original held request](start-boundary/model-018.request.gz)
and [its ending record](start-boundary/model-018.inference-ending) are retained.
That final `client_eof` is a cleanup observation; it does not show that the
cancel marker occurred inside the earlier barrier. The original order receipt
does not contain an absolute campaign timestamp or a request UUID, so neither
is reconstructed here. This directory does not claim a diagnosed cause or a
successful followup for this additional failure.

## Artifact integrity and browser environment

[compiler-summary.json](compiler-summary.json) preserves the exact upstream
pin `795b1b4958333cf904547e484acb4de9d4b3ee80`, source-lock identity, full suite
counts and actual browser receipt. The job ran as UID 1001 with Chromium's
sandbox enabled; PID/network namespaces and seccomp-BPF were observed. The
whole compiler artifact ZIP, ID `14726860970`, was independently downloaded
and hashed as `a004aa58733063d3e172ab0ee8d9b119a02dd76791696e649d6e7da659a4320e`.

The native artifact, ID `14726079773`, is 1,114,301,173 bytes. Its complete
ZIP digest is GitHub metadata, not an independently recomputed full-ZIP hash.
Bounded HTTP ranges fetched the ZIP directory and all 39 top-level
`start-boundary` members using 1,216,380 bytes across four requests. Member CRCs
and individual SHA-256 hashes were checked. The [selection manifest](start-boundary/selection.json)
and [original artifact metadata](start-boundary/artifact-metadata.json) retain
that scope; this directory includes the critical failed-phase members. The
entire selected set remains in the recorded remote evidence directory and
the original GitHub artifact.

Collecting and archiving these completed receipts started no Cargo execution,
model request, workflow retry or live database inspection. This evidence is
specific to candidate 5f and does not establish daily readiness for it or any
later candidate.
