# Model 5: independent original-evidence review

This review reads the original artifact ZIP directly, independently of the
inspector's derived analysis. It does not run a model, execute product tests,
open a live Store, or change the parser, acceptance checks, or exit status.

## Exact source and reproduction

- Workflow run: 43916477826; job: 133471871117; artifact: 14255345735.
- Published head: be46be34593452f47b824d3ae62f625032e2d54c.
- Actual checkout, recorded by the original manifest:
  5e7a3d5ade83d21fbc6e86b7eb8c3f19fb3fbe3c.
- Actual tree: 58e5e59b230a5020ee56ccf800deea1466c3151b.
- Original ZIP: ../original.zip, 8,232,052 bytes.
- Independently recomputed ZIP SHA256:
  ab362c507c24dae49f11429c73b58ecc81cf21c90ebf4d9cc99672323759e084.

Run the complete stdlib-only script with:

    python3 review_model5.py --zip ../original.zip --output review.json

execution.json records the actual command, interpreter, elapsed time, exit
code, and script/log SHA256 values. The run exited 0. Full stdout and stderr
are retained, including the empty stderr. review.json retains all 41
request/attempt/step identities, original CAS references and member paths,
response contents, stream comparisons, correction links, source excerpts,
and the exact members read.

The eight complete source copies under source/ were fetched at the actual
checkout above. source-manifest.json records their URLs, lengths and SHA256.
This script checks the original ZIP SHA256 and referenced object lengths.
It does **not** independently recompute BLAKE3; the BLAKE3 strings in the
report are the original identifiers. The ZIP hash binds the bytes being
reviewed, including each named CAS member.

## 1. Direct cause: 40 completed model outputs were invalid controls

The final ledger has 41 durable **subject request** terminal records:
40 success with provider_terminal/stop, followed by one failed/unknown.
These are not 41 proven physical HTTP POSTs or child E1s.
There are 40 retired subject steps, one awaiting_model, one root task, and
zero child tasks, task results, operations, attempts, evidence receipts,
decisions, or subject intents.

The 40 successful original response streams all contain [DONE] and a
stop finish reason. For every one, concatenating the original JSON stream
delta.content produces exactly both the terminal's response.content and
the recorded model-result CAS bytes. They have 40 distinct provider
completion IDs. This excludes a local stream-concatenation discrepancy or
one saved terminal being repeatedly relabeled as new requests.

### First response

Request e94983eb-47db-42da-bf12-cf0d46c14463, attempt
50d777c2-ebd7-4e7f-a788-786caf98612e, has:

- Plan member:
  real-single-node/cas/22/32/22326c32735e252b358962a10c26ccf4b0f4cb1bab0e9c097c520861bf96da55.
- Terminal member:
  real-single-node/cas/26/92/26926e454ceb858a806d274ed8e80d93ff80c91f354dc40d8f68ba53cef1afd8.
- Model-result member:
  real-single-node/cas/dc/00/dc00b62b14856d749c399b1791a608564578cd899a33bc4ebcaea55b4dea7500.

Its first [delegate:compute.child:1] line contains a JSON object, followed
on that same line by explanatory prose and an inline
[delegate:host.printf:1] expression. The recorded rejection is
"native delegation input must be one JSON value". The actual parser reads
the complete remaining line as one JSON value; this refusal is consistent
with the original text and source (output.rs:363–382). The frozen prompt
also supplies a separate finite-model [delegate] grammar requiring
goal, system_prompt, model, and max_tokens (output.rs:351–361).
This response does not use that supplied grammar.

The later wait controls and the asserted result 437 do not establish
execution. All actions from the rejected output remained unaccepted.

### Next 39 responses

Requests 2–40 produce exactly the same 233-byte model-result member:

    real-single-node/cas/60/45/6045e034e6fde641142356f5ff3780b3305d2d8c88138bd8390363ec2482433f

It starts with [wait:end], followed by [wait:replace], and asserts that
the computation has finished. Every corresponding frozen plan has
semantic_wait: null. Each actual recorded rejection is
"no semantic wait exists in the frozen context", consistent with
output.rs:235–249. The parser also permits only one wait control per
result. Ignoring either control would weaken the current contract.

All 40 correction transitions preserve the prior original assistant output,
the exact rejection feedback as a user message, the original input, and the
original worker context baseline, while giving the replacement a new request
identity. Request 40 has 79 messages; request 41 has 81. The preserved worker
source_commit_seq: 14 is intentional R0 lineage, not evidence that feedback
was discarded. The original provider streams corroborate new answers; the
artifact does not contain a separate full outbound HTTP request-wire capture,
so frozen-plan preservation is not claimed as an independent wire-byte audit.

Input usage for the 40 successes ranges from 12,720 to 18,502 tokens, against
32,768 configured context tokens. All have length: false; effective output
reserve is 4,096, while actual outputs are 196 then 69 tokens. This run did
not fail these 40 requests by output truncation or an observed context-limit
rejection. It demonstrates failure of this Qwen3-0.6B-Q4_K_M configuration to
complete the requested controlled task. It does not establish a general
model-capability result or measure the unavailable formal 27B baseline.

## 2. Independent implementation gap: host.printf is omitted from usage

The exact runtime installs HostPrintfHandler
(bin/shenko/src/runtime_plugins.rs:113–117), and its accepted input is
{"text": "...", "timeout_secs": ...} with unknown fields denied
(crates/shenko-exec-host/src/native_task.rs:29–34).

Its descriptor sets prompt_section: None (native_task.rs:301–314).
The native registry's model-facing catalog includes only handlers with a
nonempty prompt section (crates/shenko-subject/src/child.rs:645–659).
All frozen system prompts in this artifact are 51,370 UTF-8 bytes / 34,550
characters and contain no host.printf; their tools arrays are empty.
The user's original input does name host.printf and text, but the first
model response instead invents an input field.

The smallest code direction is a handler-owned prompt section describing
the existing version-1 input, limits, exact-result predicate, and original
receipt/Unknown semantics, exposed through the existing registry. It must
not introduce core keyword routing, fake a receipt, or relax parsing.
There is already effect-contract usage metadata in the handler; this review
only establishes that the actual model-facing catalog omitted its usage.

This omission is a concrete, separate implementation gap. It is **not**
proved to be the sole cause of the 40 failures: the model also violates
explicitly supplied finite-child and wait-control grammar. A descriptor
change would be a new model-facing candidate requiring an actual frozen
payload/wire review, disabled-handler comparison, and a fresh real journey
before claiming improved behavior. No such comparison or new run occurred.

## 3. Last Unknown and shutdown are a separate boundary

The unchanged first-phase checks had not succeeded by the harness's
1,800-second deadline. The original summary records:

    AssertionError: first phase did not satisfy the unchanged child/result/output checks

The harness then enters finally and sends SIGINT to its owned runtime
(real_single_node_journey.py:204–216,468–487;
real-single-node.yml:140–144).

The original real-single-node/first.runtime.log:83 records shutdown at
2026-11-16T17:51:32.390510Z. The final request
beec6b72-fc1c-477d-b880-2d5622cc234a, attempt
b8c38780-141d-4f63-aabd-d543b35bec93, was durably created in that second.
Its terminal is recorded at 17:51:34:

    real-single-node/cas/95/63/9563c71b700d4c533136a4d194bb7c0b0d12a47e6cd196ba707e4310cf4a84f6

The terminal is failed, has no response, and explicitly reports
termination.kind: unknown. Ledger session event 83 records
INFERENCE_OUTCOME_UNKNOWN for that same request at 17:51:34.767Z;
event 84 and runtime log line 84 preserve the error:

    model outcome not conclusive; preserve original awaiting_model for reconciliation

The source admits model output only with a matching sealed binding and a
successful provider-terminal result (loop_runner.rs:2549–2603). The final
step correctly remains awaiting_model; it is not retired or accepted as
empty successful output. The runtime exits 1; the original clean-shutdown
check remains false, and the restart phase was never reached.

There is no completed 41st stream. That absence, and the timing near SIGINT,
do not establish whether a 41st physical POST reached the provider. The
exact producer of the final Unknown has not been proven by this review.
This is not evidence to relabel the request NeverDispatched, retry the
same request automatically, or treat the run as a clean exit.

## Preserved acceptance boundary

The actual public input and startup pause were observed successfully.
Finite-child creation, both original child E1s, committed result/output,
clean shutdown, and restart continuation remain failed or unobserved.
This report introduces no passing product tests and makes no claim about
27B execution, S2 seats, long-running deployment, or physical failures.
