# CI be46: real startup cancellation, retained component failure

This record retains the failed candidate and the separate, narrowly scoped
follow-up checks. It is not an all-CI pass or complete ship/work acceptance.

## Exact run and source

- Repository: `cyjin-yl/shenko`.
- [CI run 43916292651](https://github.com/cyjin-yl/shenko/actions/runs/43916292651),
  attempt 1, pull-request event, final conclusion **failure**.
- Public head: `be46be34593452f47b824d3ae62f625032e2d54c`.
- Actual merge checkout: `5e7a3d5ade83d21fbc6e86b7eb8c3f19fb3fbe3c`.
- Both trees: `58e5e59b230a5020ee56ccf800deea1466c3151b`.
- Independently supplied baseline: `efdb3da73160f9f20303014a6f2c981f1e2a0b3c`.
- The separate body-event run `43916293222` was already cancelled. Its original
  record is retained; this monitor did not dispatch, cancel or rerun workflows.

The full run, jobs, source objects, workflow and artifact listing are retained
beside [the observation](observation.json). The source and baseline were also
checked against the actual checkout lines in the job logs.

## Observed outcomes

| Scope | Actual result on this checkout |
| --- | --- |
| docs, docs-migration, kernel-boundary, web | Success |
| Clippy ratchet | Failure: three new `unreachable_code` diagnostics in production; six in all-targets |
| Rust workspace | 1,066 passed, 1 failed, 11 ignored; the only failed target was `component_process` |
| Node unit tests and existing component agreement journey | 35 and 1 passed respectively |
| Store roundtrip | 40 passed, including first claim, direct native continuation and the bootstrap/zero-write journey |
| Subject | 116 passed, 7 existing ignored; the correction-capacity journey passed |
| Actual SSH install and named execution | Passed, 203.54 seconds |
| Start boundary, held metadata GET, reopen and resume | Passed, 1.64 seconds |

The complete workspace output is [retained losslessly](workspace-test.log.gz).
[The derived result index](workspace-results-derived.json) records every test
result and its source line; it does not convert an ignored or filtered test into
an executed one. The aggregate includes 84 filtered tests and does not express a
product completion percentage.

The later standalone E1 delivery source `862213d9` was not in be46. This run did
not execute its new authenticated SSH delivery journey.

## Component failure and the endpoint correction

The actual failure was:

```text
Error: coordinator started a different self
```

`component_process` read `subject_id` from `/health`. The real
`shenko_ui::routes::health_handler` returns only `status`, `product` and `version`.
The retained bootstrap health response has exactly those fields. Reading the
missing field as a JSON null made this assertion fail irrespective of the
subject's identity. The current coordinator's health body was not separately
saved before this assertion, so this record does not invent that response.

The actual B process log says it resumed the original genesis and started the
subject loop for `80385d2a-fe49-4766-ad10-cb773a584b04`, matching the original
Node identity. B committed its startup pause and reached UI readiness. The
metadata observer recorded only `GET /v1/models` and `GET /metrics`, both answered
with an actual HTTP 503. It produced no model card and observed no generation
POST. The owned cleanup record contains no errors.

Before the failing assertion, the journey completed seven-member consensus
field checks, the full committed control frontier, each member's complete local
metadata after reopen, independent home provisioning and the A/C branch forks.
The final common certificate, reservation/admission, follower permissions and
remaining restart assertions were **not reached**. No three-process acceptance
is claimed from this run.

The follow-up starts from `6bfc9487`, which already includes the E1 helper split.
It changes only the existing component journey and its shared HTTP observation
helper:

- Readiness still uses the existing `/health` and its existing deadline.
- After readiness, the same coordinator port is queried at the existing
  `/api/self` endpoint. The response bytes are saved before parsing, including
  partial or rejected responses.
- `subject_id` and `identity.agent_id` must both be present, decode as real
  entity IDs and equal the original subject. `data_dir` must be present and equal
  the expected installation path. Missing values cannot satisfy equality.
- The response remains subject to the existing 200 ms connection deadline,
  one-second read/write waits and 64 KiB observation bound. Reaching the byte
  bound is an explicit failure.
- The old health observation and the new self observation remain distinct in
  `agreement.json`. All later certificate, authority, stop and restart assertions
  remain in place. No new independent test scenario or longer timeout was added.

`start_boundary` was reviewed and left unchanged. Its `ready` function already
requests `/api/self` and returns only if `subject_id` is a string; both operands
of the existing restart equality pass through that check. It was not a
`null == null` comparison.

## Actual held-GET cancellation evidence

The retained `start-boundary/result.json` reports `journey_passed: true` and
`metadata_connection_end: "client_eof"`. The actual held request is
`model-002.request`, a `GET /v1/models`. The test observed that request before
SIGINT, retained its original 30-second clean-stop requirement, and observed
the client EOF without the observer first replying or closing the connection.

The same installation reopened with the original pause, then resumed and sent
the actual `model-005.request` POST for `dispatch-observer`. All original pause,
GET cancellation, EOF, reopen, resume and POST assertions ran. The observer did
not generate a model answer; `model_completion_tested` remains false.

## Clippy counts and the prior review correction

| Scope | Compared baseline diagnostics | Compared candidate diagnostics | All count changes |
| --- | ---: | ---: | --- |
| Production | 757 | 759 | `shenko/unreachable_code` 0→3; `shenko-subject/expect_used` 14→13 |
| All-targets | 1,279 | 1,284 | `shenko/unreachable_code` 0→6; `shenko-subject/err_expect` 3→2 |

The complete four diagnostic streams, their stderr logs, lint discovery and
ratchet report are in [the unchanged original Clippy ZIP](clippy-original.zip).
All-targets retains the existing 58 uncompared production-only diagnostics on
each side. No baseline, suppression or allowlist was changed.

The three new calls were `std::pin::pin!` in
`runtime_inference.rs:187–189`. Their primary spans point into the standard
library's `core/src/pin.rs`; the expansion spans identify those call sites.
The previous scoped JSONL **already contained all three diagnostics**. Its
toolchain, 276 flags, default features and source bytes matched this candidate.
The prior review only attributed primary spans to changed files and missed the
macro expansion sites. This is a review error, not a toolchain difference.

[The correction](followup/prior-scoped-review-correction.json), complete prior
stdout/stderr and original observation are retained. The prior exit 0 only
established that metadata compilation completed with force-warn enabled; it was
not a zero-warning result or a dual-tree ratchet pass.

The separate fix `9bb2c5b459149172787a26e5f03f5dc1afee9e4c` replaced exactly three
pin macros with `Box::pin`. Poll priority, error classification, future ownership,
cleanup and deadlines were unchanged. Its exact-source scoped lib metadata
completed with exit 0; the complete `shenko` diagnostic set contains zero
`unreachable_code`, and recursive expansion/child-span review finds none in the
probe block. That check did not execute tests or the complete dual-tree ratchet.

The endpoint follow-up's separate metadata log, source patch and diagnostic
review are retained under `followup/runtime-identity-*`. They bind the original
`6bfc9487` source plus the exact two-file patch. No Rust source changed during
that check. One scoped Clippy invocation compiled the `component_process` and
`ssh_wasi_client` test targets with the same complete 276 CI flags and exited 0
in 1 minute 44 seconds. Reviewing all diagnostics, including recursive expansion
and child spans, found no diagnostics in either changed file. The shared helper
was compiled in both of its existing consumers. This was metadata compilation;
the revised process journey still requires its next actual CI run.

## Original artifacts and lossless retention

| Artifact | ID | Original ZIP SHA-256 |
| --- | --- | --- |
| rust-evidence | `14253760054` | `5167fe27a87d30bcfcc1b8656224825e4ce501c0ce58308b0ad946d7b4e81524` |
| clippy-ratchet-evidence | `14249738774` | `48fa5639d783023b960367912e0a76ea5b89d811bcf3e04642fbf1c2a038392c` |
| web-rebuild-evidence | `14250386130` | `27a7c89b886a8493511beef70c348c20d148a325093ac419b5cdd856c8fd1c9e` |
| kernel-boundary-evidence | `14249882501` | `7f08d900eba8b7e613704a391cad157e92d7d3e9aee9007661f7483d83cf8408` |

All four downloaded ZIPs were checked against GitHub's recorded digest and
remain in `/workspace/scratch/faaa3ccca531/ci-be46`. The complete member names,
lengths, CRCs and SHA-256 values are retained in `artifact-manifests/`.
[The artifact listing](artifacts-final.json) retains original provider URLs and
expiry dates.

Five complete job logs are stored as lossless gzip under `job-logs/`. Two
attempts to fetch the complete Clippy job log returned `Transport closed`; both
tool errors remain unchanged. Its artifact still contains all four complete
diagnostic streams and all four cargo stderr logs. No missing log is represented
as complete.

`runtime-originals.tar.gz` contains 304 byte-identical selected original Rust
artifact members: component commands, membership, forks, model wire, residents,
SSH daemon logs, cleanup and failure records; all start-boundary originals;
and SSH installation, execution, timing and build receipts. Its
[member manifest](runtime-originals-members.json) binds every original path and
digest. The workspace log is stored separately, once. Compatibility installation
databases, private fixture trees and the guest WASM binary were not duplicated.

[The retention manifest](retention-manifest.json) records original and stored
hashes, including the uncompressed hashes for gzip files. Packaging changes only
the container format; original log and protocol bytes are not trimmed or
normalized. No remote source, label, assertion or baseline was changed to make
the failed run appear successful.
