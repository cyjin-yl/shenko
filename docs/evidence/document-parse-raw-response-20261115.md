# Original OXR response receipts — 2026-11-15 continuation

This continuation starts at Shenko `299447344bad7c3d844a9f2411963c50f803f0b5`
and closes the raw-output limitation recorded in
[the earlier recovery increment](document-parse-recovery-20261115.md).
The earlier result remains historical evidence: it retained decoded observations,
not byte-identical HTTP bodies. This change retains both through the existing
original operation/attempt and CAS/E1 mechanism. It adds no Store table,
operation lifecycle, dispatcher or external retry authority.

## What is retained

`OxrClient` exposes an observed form of its existing submit and read-only task
queries. The same bounded body reader returns its typed outcome together with
the received response path, HTTP status, declared length, capture state and
body. Unknown JSON fields, field order, whitespace and original encoding survive
when the complete response body was received. This is the HTTP entity body as
delivered by reqwest, not a packet capture: HTTP framing, request multipart data,
authorization headers and other response headers are not archived.

The native `OxrAttemptStore::append` adapter seals the body and its decoded
observation before admitting one E1 receipt. `EvidenceReceipt.raw_ref` directly
references the body object; `normalized_ref` references schema-2
`TransportObservation`, which binds the original operation/attempt digest,
decoded document/error or accepted task, and capture metadata to the same body.
Both objects therefore belong to the existing evidence object closure. A crash
before E1 can leave an unreferenced sealed object, but cannot publish a decoded
result without its admitted body reference or grant another upload.

For a complete, unmodified body, the returned receipt carries its digest as
`raw_output_ref`. Each parsed page inherits that exact reference. A reconstructed
provider can obtain both the result and its provenance from committed receipts
with the engine offline. Historical schema-1 typed-only receipts remain readable
and honestly keep a missing raw-output reference.

Every fully received async acceptance or polling observation is recorded before
the next network request or sleep. The original polling deadline still covers
HTTP requests, body reads and sleeps. Expiration/caller cancellation does not
establish remote termination and never permits another upload. A later bounded
read-only check still uses the original task handle.

## Partial and sensitive responses

The 64 MiB body cap remains enforced before allocation of the full response.
An excessive declared Content-Length produces an incomplete capture with no
received body. A chunk crossing the cap retains only the bounded prefix; a
body-read error retains the prefix actually received. These observations remain
Unknown and carry no complete page raw-output claim.

Dropping an in-flight future can interrupt a response before the synchronous
append is reached. That incomplete in-memory read is not claimed as a durable
archive. The already armed original attempt remains Unknown; no resend is
introduced to fill the gap.

A response containing a recognizable literal or JSON-escaped credential echo
is marked `credential_echo_withheld`; its body is not sealed and no raw-output
reference is claimed. A typed refusal is redacted and capture metadata remains.
An otherwise successful envelope becomes an explicit Unknown before its fields
can become a normalized document or page result. This prevents recognized
document fields from retaining the same credential through the decoded path.
This is an explicit missing original, not redacted bytes presented as an exact original.
The matcher is not a general detector for arbitrary transformed/encoded secrets.
Raw response Debug output exposes metadata and length, never document bytes.

## Conflicting observations

The existing revision-checked E1 fold remains the only admission path. Different
raw encodings of the same decoded result are retained separately without
inventing a terminal-content conflict. Contradictory decoded terminal contents
retain every captured original body and both mapped observations and set sticky
`outcome_conflict`/Unknown in the same transaction. Final ParseRun settlement
continues to use the existing revision check.

## Verification boundary

The two existing native Store/HTTP journeys now additionally assert exact body
bytes with unmapped fields and noncanonical whitespace, retained body/normalized
objects after reopen, offline page provenance, and both original bodies after a
late contradiction. Their original one-upload/original-attempt assertions remain.
One new independent scenario exercises a JSON-escaped credential echo across
a refused upload and a nominally successful document. The redacted refusal is
retained; the success becomes Unknown and both bodies are withheld.

Final checks on this candidate:

- `cargo test -p shenko-document --lib --locked -j1`: 19/19 passed, no skips,
  including the credential guard for both refusal and nominal success.
- `cargo test -p shenko --lib document_runtime::tests --locked -j1 -- --nocapture`:
  both existing native journeys passed, no skips; 34 unrelated tests filtered.
- Focused `cargo clippy` for `shenko-document` and `shenko`, library and tests,
  with `clippy::all`, `unwrap_used` and `expect_used` warnings enabled: exit 0;
  no diagnostics on added Rust lines. Existing workspace warnings remain.
- `scripts/check_docs.py` with the installed CI parser environment: no errors.
  This checks documentation integrity, not product behavior.

Rust checks used the shared lock/target, one build job, no incremental state
and no debug info. An initial native compile exposed four test-expression type
errors, corrected before the final check. Two native invocations resolved stale
shared-target Store/Node metadata; their retries explicitly invalidated only
this checkout's corresponding source timestamps. The final retry passed both
journeys and lint. No lock, cache, dependency or assertion was removed. A first
documentation check used a Python environment without the required parsers;
the installed CI environment supplied the actual passing check.

These are real Store/CAS and bounded loopback protocol checks. No actual OXR
deployment, credential acceptance, recognition quality, batch throughput or
remote termination was exercised. The [runtime-resource inspection](readiness-runtime-resources-20261115.md)
records the distinct live-resource boundary. This increment alone does not
close all of issue #89 or S1–S5.
