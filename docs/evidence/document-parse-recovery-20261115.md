# Document parse recovery and native binding — 2026-11-15

This is the follow-up to [issue #89](https://github.com/cyjin-yl/shenko/issues/89)
and the [2026-11-11 provider record](document-parse-provider-20261111.md).
The candidate starts at main `054817b0d5729fed56ec5a974d101c31edfd1410`;
the commit containing this record identifies the changed source. The older
record's in-process single-flight claim did not establish safe failure,
cancellation or restart behavior. This increment supplies the missing host
binding and corrects that behavior without claiming a real OXR engine ran.

## Observable changes

| Previously observed defect | Candidate behavior |
| --- | --- |
| `get_or_try_init` discarded failure and could upload the same document again for another page | The cell retains both results and errors. The native host commits the original operation/attempt and its dispatch marker in one T1 transaction. Only that fresh winner can upload; a dropped initializer cannot recreate its send authority. |
| Whole-document `page_count` propagated Unknown as an untyped error | `PageCountOutcome` distinguishes known count, undetermined count, terminal failure and unknown effect. `ParseRun` preserves whole-document failure/Unknown and the unavailability reason before any page exists. |
| Response limits were checked after full body buffering | `OxrClient::post` checks Content-Length early and enforces the 64 MiB cap while reading chunks, before appending each chunk. |
| A slow request/body or sleep could exceed the polling budget | An accepted task records its original waiting deadline. Each wait uses one monotonic deadline covering HTTP send, body reads and sleeps; a reconstruction uses the remaining recorded budget. |
| Missing `pages` could become an empty completed document | Missing/null pages, inconsistent reported count, duplicate or noncontiguous page indices are unestablished results and remain Unknown. An explicit empty array is distinct from a missing field. |

The original waiting deadline is not a remote cancellation. After it expires,
an explicit reconciliation can make one bounded read-only query of the known
task. It does not renew the original polling loop or upload again. No task
handle plus an armed/unknown upload remains Unknown. There is no automatic
fallback provider, model download or alternate public API.

## Existing runtime and durable state

`bin/shenko/src/document_runtime.rs` implements `NativeTaskHandler` and is
installed through the existing `NativeTaskHandlerPlugin` in
`install_native_definitions`. It uses the existing TaskRecord, OperationRecord,
AttemptRecord, CAS and T1/E1/T2 paths. There is no second job database or
dispatcher.

The crate accepts a small host `OxrAttemptStore` adapter; the trait alone is
not evidence of durability. The native adapter seals the exact upload binding
in `checkpoint_scope_ref`: original operation/attempt, actual source digest,
declared source digest, filename, schema, closure, normalized endpoint, model,
effective parameters, account and scope. Content identity alone does not
suppress a legitimate new operation. A different binding cannot reuse the
original attempt. Credential bytes are absent from the binding and receipts.
Reusing a native operation also checks its full frozen input/contract before
preflight and before returning a prior result. A different page selection
cannot receive the old operation's success merely because it shares the same
whole-document transport binding.

The asynchronous availability probe runs before T1. The actual upload claim
uses `arm_native_operation_for_dispatch`: the existing task, invocation, epoch,
generation and owner checks, original operation/attempt and per-attempt native
dispatch marker commit together. The marker and T1 share one CommitRef. A
duplicate cannot observe a half-armed attempt during preflight, retire it as
Unknown and fence the sole sender. Only the fresh transaction winner receives
send authority; an existing operation returns read-only replay. Reconstruction
has no invocation with which to arm a send. Network work remains outside the
synchronous transaction. A failed preflight records an unavailable result via
ordinary T1/E1 without a dispatch marker; it claims no effect only if that
no-send admission was fresh, preserving Unknown for an already admitted send.

Async acceptance is sealed as an immutable E1 receipt **before** polling.
Completed/failed transport observations are also retained before page results
are materialized. A restart can therefore decode a committed terminal receipt
with the endpoint offline. Each requested page is sealed separately; the
final result carries a checkpoint manifest. Large results retain the full CAS
object and explicitly report inline truncation instead of claiming all content
was delivered inline.

Every retained transport observation is the typed, mapped `OxrDocument` /
`OxrError` and optional task handle serialized into an immutable CAS object
referenced by the existing evidence history. These are not byte-identical
original HTTP bodies: unknown wire fields and the original JSON encoding are
not archived. `PageProvenance.raw_output_ref` remains `None`. `record_external_evidence_if_current` adds a narrow
generic compare-and-set path to E1: the decoder's expected operation revision
must still match. A stale fold writes nothing and refolds all receipts; a
contradictory decoded terminal observation preserves both typed observations and sets sticky
`outcome_conflict`/Unknown atomically. Final ParseRun settlement uses the same
revision check, so it cannot overwrite a concurrent contradiction. No fabricated
Failed observation is used to force that conflict.

## Configuration and input

An unset or blank `SHENKO_OXR_ENDPOINT` registers no `document.parse` definition.
With an endpoint, the runtime requires all of `SHENKO_OXR_MODEL`,
`SHENKO_OXR_ACCOUNT` (stable non-secret account identity) and
`SHENKO_OXR_API_KEY`. It does not contact the endpoint during registration.

| Optional environment setting | Meaning |
| --- | --- |
| `SHENKO_OXR_SCOPE` | Scope bound to the request and receipts; absent means unscoped. |
| `SHENKO_OXR_TRANSPORT` | `async` by default, or explicit `sync`. |
| `SHENKO_OXR_ENGINE` | `remote` by default, or explicit `local`. |
| `SHENKO_OXR_ENGINE_ATTESTATION` | Required by a local binding before live parsing can be Ready. This is a deployment attestation, not a hardware probe. |
| `SHENKO_OXR_REQUEST_TIMEOUT_MS` | Per-request timeout, including a later read-only observation. |
| `SHENKO_OXR_POLL_TIMEOUT_MS` | Initial async waiting budget. |
| `SHENKO_OXR_POLL_INTERVAL_MS` | Interval between pending observations within that budget. |

Explicit timeout values must be from 1 millisecond through 24 hours; absent
values use the document client's existing defaults. No endpoint or model name
is hardcoded. Changing the frozen provider/account/scope/parameters makes the
original configuration unavailable for reconciliation until that exact binding
is available again.

The native input is `{ "source_ref": <ObjectRef>, "request": <ParseRequest v1> }`.
The source must exist in the local CAS, match the request digest and declared
length, and fit the 200 MiB source bound. The request uses `closure_id: "oxr/1.0"`,
the configured scope, an original filename when known, and zero-based selected
pages; an empty selection means the complete document. The plugin reports its
configured capability and returns explicit unavailable/unknown facts when it
cannot produce a result. Merely configuring it does not verify credential
acceptance, model identity or recognition quality at the remote service.

## Verification

Only two independent new journeys were added, both in the actual native
module. The existing 18 document tests are reused.

1. **Malformed whole document and restart.** One caller is held in preflight;
   a concurrent caller wins the atomic T1/dispatch, with the same CommitRef
   recorded on the attempt and marker. The first caller then replays without
   another upload. The loopback service returns an envelope without pages;
   a reopened Store and concurrent page callers retain whole/page Unknown.
   The original attempt and receipts remain, with exactly one upload.
2. **Accepted async task, cancellation, deadline and late conflict.** The
   caller is cancelled after acceptance was committed and a query started.
   Store is closed and reopened. A slow response body is bounded by the
   original deadline. A later completion is committed as transport E1 and the
   endpoint stops accepting connections before the final ParseRun exists;
   reconstruction materializes that receipt offline. A held, contradictory
   late wire result then preserves both decoded observations and returns the operation to
   sticky Unknown without another upload. Before admitting that late result,
   a changed page-selection replay is refused at the frozen-input check.

Both fixtures initialize Store genesis/head/dispatcher and a native child
before exercising T1 and the upload dispatch transition. These are real
Store/CAS and loopback HTTP mechanism checks, not real OXR recognition or a
served subject model run. The response streaming cap is additionally checked
by source inspection; no separate oversized-response scenario was added.

The final source was checked with `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`,
`CARGO_BUILD_JOBS=1` and `-j1`, under the shared build lock and target directory.
No lint baseline or lint allowance was changed.

| Check on this candidate | Observed result |
| --- | --- |
| `cargo test -p shenko --lib document_runtime::tests -j1 -- --nocapture` | 2 passed, 0 failed, 34 filtered; 2.08 seconds of test execution. |
| `cargo test -p shenko-document --lib -j1` | Existing 18 passed, 0 failed; 0.05 seconds of test execution. |
| `cargo clippy -p shenko-document -p shenko-store -p shenko --lib --tests -j1 --message-format=json -- --force-warn clippy::all --force-warn clippy::unwrap_used --force-warn clippy::expect_used` | Exit 0 with the CI warning groups. No diagnostics on added source lines; existing baseline diagnostics remain. This is not a claim of a warning-free workspace. |
| `python scripts/check_docs.py` in the existing parser-equipped documentation virtual environment | Exit 0, `errors: []`. |
| `git diff --check` | Passed. |

Independent source review found the preflight duplicate-admission race and
the changed-request replay regression while this candidate was being built;
both were corrected before these final two native journeys ran. The review
also checked that fresh combined admission retains the current invocation,
owner and cancellation fences and that an existing admission never grants
another send. The targeted checks do not replace combined-PR verification.

## Still unproven

No actual OXR deployment or credential was contacted. This record does not
prove mixed electronic/scanned Chinese recognition, bbox accuracy, batch
throughput, unrelated-child fairness under that load, actual provider task
loss on engine restart, or remote worker termination after cancellation.
Raw upstream-output archival beyond the typed mapping is not implemented.
The newly visible native usage text has no served-model efficacy evidence.
None of these local checks proves the complete #89 acceptance journey,
S1–S5 completion, or long-run product stability.
