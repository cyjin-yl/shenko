# Document parse recovery and native binding — 2026-11-15

This is the follow-up to [issue #89](https://github.com/cyjin-yl/shenko/issues/89)
and the [2026-11-11 provider record](document-parse-provider-20261111.md).
The later [original-response continuation](document-parse-raw-response-20261115.md)
adds raw HTTP body retention; the typed-only observations below describe this
earlier increment and are preserved as its actual verification boundary.
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

## Node installation continuation — 2026-11-20 (source only)

The sections and verification results above remain evidence of their original
source. This continuation ports the composed Node-owned handler to main's
current native lifecycle; it has not run builds, tests, providers or models.
The former runtime adapter's original bytes are retained outside compiled source
at `build/composition-oxr-20261119-r1/original-main-document-runtime.rs`.
Its historical codecs and receipts are consumed by the installed handler's
explicit legacy-main decoder, rather than by another environment-owned parser.

`document::install` now observes the ordinary enabled Node code unit
`document.oxr`. It reads its authenticated artifact locally, without contacting
OXR. The same handler generation publishes `document.parse` and
`document.page.read`; replacement or disablement withdraws both definitions,
closes admission, cancels local observers and joins their workers before a new
generation is published. The handler restores unresolved operation capacity
debt from Store. Retirement does not claim remote cancellation or erase that
debt. The page reader uses the handler's bounded CAS/checkpoint validation and
subject partition; parsed document content acquires no instruction authority.

### Effective configuration contract

Node installation remains the authority. Environment variables cannot create,
import, enable, replace or attest the unit. An environment-only deployment with
no enabled unit therefore has no document definitions. Provisioning must use
the existing Node installation/adoption mechanism and its real authority. This
is a deliberate migration limit: the old environment alone cannot establish
ownership of a Node artifact or grant live recovery of an old account. Startup
does not invent either fact.

The enabled code artifact is JSON `OxrInstallation` with these fields:

| Field | Contract |
| --- | --- |
| `schema_version` | Exactly `1` for the installation profile; this is independent of request/history schemas. |
| `endpoint`, `model_identity` | Actual nonempty deployment binding. Endpoint must satisfy the document client's HTTP(S) URL rules, including no embedded credentials, query or fragment. |
| `scope` | Explicit nonempty deployment permission. It is separate from a request's subject cache partition and from a historical account identity. |
| `credential_env` | Local environment variable name, 1–256 ASCII letters, digits or underscores. Only the name enters the artifact; its secret stays in process memory. |
| `engine` | `{"kind":"remote"}` by default, or `{"kind":"local","attestation":...}` with the operator's actual attestation. No attestation is synthesized. Missing local attestation remains provider unavailability. |
| `transport` | `"async"` by default, or `"sync"`. |
| `max_documents` | Integer `1..=16`; unresolved old operations continue consuming capacity after reopen. |
| `max_source_bytes` | Integer `1..=209715200`. |
| `request_timeout_secs`, `poll_timeout_secs` | Integer `1..=3600` each. These are installation bounds, not renewed recovery budgets. |
| `poll_interval_millis` | Integer `1..=60000`. |
| `legacy_main` | Optional explicit original account/config/mapper admission described below. Omission grants no network recovery of original-main history. |

When `SHENKO_OXR_ENDPOINT` is unset or blank, the installed artifact alone
selects the binding and its named credential environment variable. Other old
`SHENKO_OXR_*` variables do not override that artifact. Thus an installed unit
can remain active with the old endpoint variable unset; Node enablement now
controls installation.

When `SHENKO_OXR_ENDPOINT` is nonblank, the existing variables are checked
locally against the authenticated enabled artifact before publishing its
generation. They are assertions of the same deployment, never an alternate
handler or a request to mutate the Node:

| Environment setting | Required match |
| --- | --- |
| `SHENKO_OXR_ENDPOINT` | Same normalized endpoint as the artifact. |
| `SHENKO_OXR_MODEL` | Required, exact `model_identity`. |
| `SHENKO_OXR_API_KEY` | Required nonblank secret; the artifact must name `SHENKO_OXR_API_KEY` as its `credential_env`. Secret bytes are never serialized by setup or rendered in its errors. |
| `SHENKO_OXR_ACCOUNT` | Required nonblank original nonsecret identity. If `legacy_main` exists, it must match that admission's `account_identity`. Without admission, this variable confers no historical recovery authority and creates no account attestation. |
| `SHENKO_OXR_SCOPE` | Required exact nonempty artifact `scope`. Old unscoped configuration cannot silently become a new deployment permission. |
| `SHENKO_OXR_TRANSPORT` | Default `async`, or `sync`; must match the artifact. |
| `SHENKO_OXR_ENGINE`, `SHENKO_OXR_ENGINE_ATTESTATION` | Default remote, or explicit local with the same supplied attestation as the artifact. |
| `SHENKO_OXR_REQUEST_TIMEOUT_MS` | Default `300000`; must equal `request_timeout_secs * 1000`. |
| `SHENKO_OXR_POLL_TIMEOUT_MS` | Default `600000`; must equal `poll_timeout_secs * 1000`. |
| `SHENKO_OXR_POLL_INTERVAL_MS` | Default `3000`; must equal `poll_interval_millis`. |

The original explicit millisecond parser still accepts `1..=86400000`, but
the effective values must also fit and exactly match the installed bounds.
No rounding of subsecond request/poll timeouts or clamping of larger budgets
occurs. Incompatible configuration leaves that optional generation unavailable
with a runtime error; the subject's unrelated capabilities continue. Environment
changes take effect on restart or acquisition of another Node generation;
credentials are not refreshed by polling environment values every 250 ms.

### Original-main read and recovery boundary

Old schema-1 requests retain their original input, checkpoint-scope binding,
operation/attempt identity, source reference, page keys, response receipts and
mapper semantics. They are never rewritten as current schema-2 requests or
given today's deployment permission. Saved terminal results and pages remain
locally readable under the same subject through the installed handler's exact
historical interfaces, without a new upload or retroactive recognition claim.

Live read-only observation of an original-main handle additionally requires
the artifact's explicit `legacy_main` object: `account_identity` (the original
independent nonsecret account), `scope` (the original string or JSON null),
`provider_binding_digest` (BLAKE3 of the serialized original frozen provider)
and `mapper_digest` (the retained `legacy_main::mapper_digest()`). These are
deployment facts to supply through ordinary Node installation authority;
environment setup cannot calculate them from today's endpoint or invent them.
The original scope may remain null here even though the new installation's
deployment permission is nonempty. The handler authenticates the complete
original binding and the currently admitted endpoint/model/request bound,
uses only the installed credential source and queries only the original
known handle. Missing or mismatched admission issues no query, grants no
reupload and retains Unknown capacity debt. Historical acceptance time and
deadline remain immutable; saved terminal conflicts stay sticky.
An already sealed terminal transport receipt can be authenticated and
projected under the current installation without a live credential or a
`legacy_main` polling grant. Its original mapper output and raw-response
digest remain the provenance of the recovered pages.

The installed generation carries a recovery-only reader keyed to the original
native descriptor and `document.parse.v1` operation. It can reconcile only an
already armed operation. The dispatcher settles that operation under the
original implementation reference. An original arm with no accepted handle
keeps one capacity charge and records Unknown without contacting OXR. The page
reader uses the original checkpoint format and page keys for those operations.

Native cancellation stops and joins the local document worker. The original
HTTP receipts and remote capacity accounting survive that boundary. Fresh
tasks that cannot acquire document capacity or a same-source worker park
before T1. The dispatcher can use the released worker for original-operation
settlement and other native tasks while the deferred children await retry. The
evidence transaction checks the inspected cache source revision alongside the
consumer revision, so a conflicting source cannot settle its consumer with a
stale success. If OXR echoes a credential in response body or content-type
bytes, the observation records the withholding and retains safe body bytes.

No old successful or failed receipt has been replaced by this continuation.
The original-response continuation and composed source packet retain their
own raw-body/provenance evidence. Parent verification should run the composed
native recovery selectors under
`recovery_integration_tests::document_recovery::` in the existing development
container against this persistent source after integration, including the
installed capacity, cancellation/reopen, original-main admission and late
terminal-conflict journeys. Those commands and the earlier passed commands
are verification instructions and historical evidence respectively, not new
execution results for this port.

## Retained native observations — 2026-11-20

The source-only continuation above remains a historical statement. The later
[merged component evidence](merged-components-342-20261120.json) records the
standalone r5/r7/r8 and composed r14/r15 runs against their original source and
executable bindings, together with PR #339 protected CI. The r7 capacity failure
and both r14 failures remain recorded; r8 and r15 are separate later observations.
These controlled HTTP/Store/dispatcher journeys support installed recovery,
offline terminal/page projection, retained Unknown debt and unrelated progress.
Actual OXR recognition, deployment load, remote termination, served-model efficacy
and full A33/DX06/DX11 acceptance remain unverified. No new run was performed
for this evidence update.
