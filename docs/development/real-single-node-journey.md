# Real single-node journey

`scripts/real_single_node_journey.py` is an explicit bounded observation of a
real Shenko CLI and a real configured OpenAI-compatible model. It is not run
as a substitute for the normal Rust checks or the completion contract.

The script starts a fresh installation with an explicit durable startup pause,
submits a user message through `/ws`, and waits for its actual accepted receipt
before issuing the public `runtime resume` command. It asks the actual subject
to delegate a finite model child that computes
19 × 23 and a native `host.printf` child that executes a host process. It reads
the task, operation, attempt, E1, result, and committed decision ledgers without
writing them. A matching piece of model text alone cannot pass: both real child
results and their evidence must exist, and the reported output must commit
after those results. The selected answer's own sealed request must also have
consumed both original T2 results. A request sealed before either result cannot
qualify merely because its later S1 text mentions `437` and the marker.
The first new main request must be created after the input's
accepted commit. Its sealed input plan must bind the same subject, branch, step,
request and accepted event, and its actual user-message payload must include
the complete submitted text.
Main-model request attempts, their sealed terminal results, and content stream
units are also collected. The child `evidence_receipts` table alone cannot show
whether the subject's own generation recorded terminal evidence.

It then requests the public `runtime pause` boundary, checks `runtime status`
and the clean planned successor, stops the owned process, starts the same
installation with that same still-paused request, and resubmits the
same input identity, and asks the model to continue the preceding task from its
existing context. The result checks the original input event/commit identity,
subject and branch identity, retained task results, and a newly committed answer.
The continued input is likewise durably accepted before releasing that pause,
and must enter the first new frozen model request. The selected continued
answer must itself read both original results and this exact accepted resume
message, whether as new input or its actual sealed history. No model output, successful
task, or receipt is supplied by the driver.

`start --pause-at-start <request-UUID>` is an optional startup control. The
default start still enters continuous thought immediately. The explicit option
uses the existing subject-only quiesce record before consumers start; two
ordered request/boundary CommitRefs are published in one SQLite transaction.
An unresolved step refuses the startup pause instead of being skipped. An
existing paused request can survive process restart; a resumed or cancelled
request cannot be reused to pause again. An internal authority reconstruction
honours the durable pause/resume state after the initial startup action has
completed. This boundary does not fence already-admitted external operations
or claim a complete software handoff.

## Run

Use Python 3.11+ with `websockets==16.0`, an actual model endpoint, and a CLI built from
the revision named in the command. Preserve the model checkpoint/quantization,
runtime identity, context size, and sampling parameters with the run's evidence.

```sh
SHENKO_SOURCE_REVISION=$(git rev-parse HEAD) cargo build --release --locked -p shenko -j2
python scripts/real_single_node_journey.py \
  --binary target/release/shenko \
  --source-revision "$(git rev-parse HEAD)" \
  --model-url http://127.0.0.1:18832 \
  --model Qwen3-0.6B-Q4_K_M \
  --timeout 300 --evidence-dir /tmp/shenko-real-single-node
```

`--model-url` is the provider's base URL without `/v1`: the real
`LocalModelProvider` appends `/v1/models` and `/v1/chat/completions` itself.
The evidence directory must not already exist. Each phase has a bounded
observation interval. `--startup-timeout` separately sets the positive integer
number of seconds to observe a bound subject after each runtime spawn (default:
60). The manifest and batch summary record this setting; `first.startup.json`
and `restart.startup.json` retain actual elapsed startup time and whether the
subject became ready, including failed or cancelled starts. Elapsed startup
time ends before owned cleanup. A longer observation window is not a startup
latency fix; keep the measured delay and compare candidates using the same
explicit boundary. A failure exits nonzero and keeps its original runtime
logs, input, WebSocket frames, read-only ledger snapshots and CAS. The
public control commands and responses are retained in `runtime-controls.jsonl`;
the paused and accepted snapshots show whether any request was admitted before
each explicit resume. The
`installation` subdirectory contains private bootstrap/control material and is
excluded from the hosted artifact; publish only the explicit evidence subset.

For a deployed latency observation, use the README's release profile and record
the exact binary hash and build profile. Debug observations remain useful for
diagnosis, but their unoptimized execution and larger implementation artifacts
are a different configuration. The ancillary Python observer socket retains
its 20-second ping interval and uses the explicit phase timeout for its pong
deadline; both are recorded in the manifest. This prevents the observer's
implicit 20-second deadline from stopping a longer declared experiment. It does
not improve product responsiveness: keep the real browser's frame gaps and
Store phase timings, and never count a longer observation window as a latency
improvement.

### Repeated candidate and recovery evidence

Pass `--repeat 3 --browser --timing` to run three complete journeys sequentially
against the same binary and served model card. Each attempt uses a fresh
installation and retains both the first-task and restart phases under
`attempt-NNN`. `batch-summary.json` is checkpointed after every attempt and
reports passed, failed and unattempted runs. A failed attempt remains in the
success-rate denominator; binary or model-card drift stops the batch and
leaves the remaining runs explicitly unattempted. Model-card equality is an
observed serving identity, not proof that weight bytes did not change; record
the checkpoint/template/runtime identity separately for a deployment claim.
Only response-generation metadata (`permission` and `created`) is excluded
from that comparison: an unchanged vLLM process can regenerate both fields.
The selected card's other fields, including model ID, root, owner and served
context capacity, remain bound. Each attempt retains the raw `/v1/models`
response in `models.json`, and the batch report names the excluded fields.

The fresh installation must contain exactly one `model.child` and one
`host.printf` task. The completed child's recorded model, local endpoint and
every transcript step's provider binding must match the requested deployment.
The native task must have exactly one operation, attempt and original
postcondition-verified receipt bound to its result. Extra failed or unstarted
tasks/attempts still fail this strict journey; their complete records remain
available for diagnosing refusal, retries and unknown effects.

The output-consumption gate binds the committed step, its active request,
the recorded request attempt and their exact input-plan reference. It reads
that immutable plan and requires the two complete original `TaskResult` values
in its actual provider payload, with their runtime tool-result and event
identities. Each result must also have a matching sealed `native-child` event
or an original tool-result history row in the same scope at or before the
plan's sealed `leaf_seq`. A later row in the final snapshot cannot fill an
earlier request's missing input. One result may be old history while the other
is new; two new events are not required. A selected inbox event without the
corresponding actual payload is insufficient. `first_output_input` and
`restart_output_input` in the summary record the selected request, input-plan
reference, result identities and matched payload/history positions.

This is a causal evidence check, not a semantic keyword classifier. Independent
review still reads the actual committed answer and original provider output.
The observer's CAS reader retains its existing length check; it does not
independently recompute BLAKE3. Preserve the full original CAS bytes and the
independent audit's byte hashes. The motivating real `d16adbf8/attempt-002`
committed a waiting answer after both T2s, although its earlier sealed request
had neither result; the old existence/text gate incorrectly selected it. That
run remains failed. The preserved observer regression also uses the real
`attempt-001` seal that did read both originals.
The [original-case comparison and regression receipt](../evidence/daily-readiness-20261122/observer-result-consumption/receipt.json)
binds the exact checked source and fixture bytes; it is not another model run.

Recovery compares the original child operation/attempt rows and complete stored
result/receipt rows from the first successful answer through the paused boundary
and final shutdown snapshot. New tasks, operations, attempts or receipts cannot
be incorporated into a later baseline. This recall-only second input must not
create another model or `host.printf` child. Retaining the old result or returning
the same WebSocket receipt alone cannot prove that execution was not repeated.
The driver records its own SHA-256, phase deadline and elapsed wall time.
`--timing` selects `SHENKO_LOG=info,shenko_timing=debug`; structured phase timings
are diagnostic observations, not successful effect receipts or model-quality
evidence. All existing child, output, pause, browser and clean-exit assertions
remain required.

With `--browser`, the actual composer chooses the mode according to the current
socket's preceding `hello` capabilities, following the already merged one-send
UI (`a0f008a7c262209f1b6ce6333aa5d00f8421c8a1`). The witness checks that `append`
and durable receipts are available, and requires `steer` exactly when advertised.
It preserves the actual submitted mode and checks the same socket's accepted
receipt: durable acceptance, matching requested mode, and `next_boundary`
placement at this journey's clean pause. It does not change the composer or
force a mode, and does not claim an active-generation steering/replay test.

`python3 -I scripts/real_single_node_journey_test.py` checks the driver's owned
cleanup, reexecution detection, failed-attempt accounting and output-consumption
gate. The last uses preserved real seals plus explicitly labelled input
perturbations; the other bookkeeping checks use synthetic records. It does not
execute a model or satisfy the journey itself.

The `Real single-node journey` GitHub Actions workflow is explicitly selected
with workflow dispatch or by adding the PR label `run-real-model`. Adding that
specific label requests one experiment; later pushes and unrelated labels do
not request another. To deliberately observe a new candidate, remove and add
the label again after publishing that candidate. The label also works
while the workflow is first being reviewed, before its file reaches main. It
builds the exact selected repository revision, verifies pinned model and
llama.cpp archive SHA-256 values, and runs the same script. It retains failed
observations too. Unlabelled PRs do not run this model experiment, and a skipped
job is never evidence that the journey passed.
The fixed 0.6B model is a low-resource mechanical experiment. Its outcomes do
not satisfy the Qwen3.8-27B quality baseline, the NapCat/coding-seat S2 journey,
physical fault requirements, or 7/30-day observation periods.

The hosted diagnostic explicitly sets `SHENKO_LOCAL_MODEL_REQUEST_TIMEOUT_MS`
to 1,200,000 ms, observes each phase for at most 1,800 seconds, and gives the
whole job at most 75 minutes. These limits follow the failed hosted observation
below; they are not evidence that the journey passes. The model, full product
prompt, sampling parameters and all success predicates are unchanged. Outside
this workflow an unset local request limit still means 180 seconds; a configured
limit is sealed in every new request and cannot reinterpret an earlier one.

### Hosted CPU attention configuration

The next hosted experiment explicitly uses `--flash-attn on`, F16 K/V caches
and `--verbosity 4`. It keeps the pinned weights and runtime, complete product
prompt, parser, sampling, context, batch sizes, sealed request limit and every
success predicate. Cache precision can change numerical results; the same
behavior assertions still apply. No improvement is claimed until a new exact
candidate completes the real journey.

This choice follows inspection of fixed llama.cpp b12359, commit
`b1b770d90d94cad9e9ea5b993bdbc383a522fb28`:

- The default is `auto`, and [quantized V explicitly enables Flash Attention](https://github.com/ggml-org/llama.cpp/blob/b1b770d90d94cad9e9ea5b993bdbc383a522fb28/src/llama-context.cpp#L4541-L4549).
  The preceding Q8 experiment therefore cannot be described as having omitted
  Flash Attention merely because it did not pass `--flash-attn`.
- The CPU's [split-KV and tiled paths require equal F16/F32 K/V types](https://github.com/ggml-org/llama.cpp/blob/b1b770d90d94cad9e9ea5b993bdbc383a522fb28/ggml/src/ggml-cpu/ops.cpp#L9366-L9447).
  Q8 does not satisfy those predicates. The alternative path still calls vector
  dot products; its presence does not prove a generic or SIMD-free backend.
- The [logging callback maps library INFO to verbosity 4](https://github.com/ggml-org/llama.cpp/blob/b1b770d90d94cad9e9ea5b993bdbc383a522fb28/common/log.cpp#L548-L565),
  while application INFO defaults to 3. Raising this observation setting retains
  [the selected backend's library path](https://github.com/ggml-org/llama.cpp/blob/b1b770d90d94cad9e9ea5b993bdbc383a522fb28/ggml/src/ggml-backend-reg.cpp#L259)
  and attention-configuration records that the earlier log lacked.

The first 65,536 bytes of the exact pinned GGUF were read without loading the
model. They report 28 layers, eight KV heads, and 128 dimensions each for K and
V. At 16,384 tokens and one slot, an F16 cache body is
`28 × 16384 × 8 × (128 + 128) × 2 = 1,879,048,192` bytes (1.75 GiB).
This estimate excludes weights, graph/work buffers and process overhead. The
previous runner recorded 14.258 GiB available before model startup; neither
that earlier snapshot nor this calculation proves the next run's peak memory
fits. The actual startup log and process status remain the observations.

The workflow now retains the pinned runtime's help, complete invocation,
visible cgroup cpuset/limits/counters and mount identity, then the real model
process's command, affinity, status and mapped ggml libraries at readiness.
Unexposed ancestor limits remain unknown. The source review, original header
range and hashes are in [the runtime inspection](../evidence/single-node-runtime-b12359/observation.json).
These are configuration and source facts. They do not retrospectively profile
the earlier slowdown, repair its unknown outcome, or provide a successful model
run.

## 2026-11-16 local attempt

The available pre-existing CLI was identified by SHA-256
`0a17e5e97dd97de231007d5a6f4e37285a080d31ff74e96e82f0b9419d34a549`,
with **unverified source provenance**. No runtime acceptance was attributed to
that binary or to the current policy-config implementation.

Two real model loading attempts were killed with exit 137 before the endpoint
became ready: official Qwen3-0.6B Q8_0 and the smaller Unsloth Q4_K_M with ordinary
file loading. An intervening option preflight failed because llama.cpp b12359
uses `--load-mode none` rather than the older `--no-mmap`. Both actual loading
failures, the option failure, artifact identities and resource observations are
retained in [the raw record](../evidence/single-node-model-load-20261116/outcome.json).
Resource exhaustion is suspected; no kernel attribution was available. The
model received **zero requests**, and the single-node journey was not launched.

The local Rust verification command also stopped before tests ran: `rustc`
compiling `shenko-store` was killed with SIGKILL. The new policy tests therefore
require execution against the current candidate in an environment with enough
resources. These failures remain open evidence, not successful acceptance.

## 2026-11-16 hosted attempt

[Run 43860565053](https://github.com/cyjin-yl/shenko/actions/runs/43860565053)
built and ran exact merge source `adf05188be9c4768a5f46ab94f1332079a5c274a`
(PR head `b2886c2b9b10d68ffb27d5b0702a896419f1ced6`). The pinned model and
llama.cpp hashes passed, the real endpoint loaded, and the WebSocket input was
durably accepted at commit 7. The model received the full sealed product input,
including this user submission.

The attempt **failed before the first model token**. At 169.23 seconds the
server had processed 5,888 prompt tokens, reporting 46% progress (about 12.8k
tokens in total; the total is estimated from rounded progress). The last
256-token batch took 14.32 seconds, about 17.88 tokens/second. Keeping only that
last rate would imply roughly 556 seconds for prefill; a quadratic fit to the
observed sequence estimates about 779 seconds. Both are extrapolations, not
measured completion times.

At 180 seconds the provider's then-fixed total HTTP timeout ended the stream.
The retained CAS result is `failed` with `termination.kind=unknown`, zero stream
events and a journal installed. The subject kept its original `awaiting_model`
step and the CLI supervisor ended the process; the WebSocket consequently
closed. These preserve uncertainty instead of sending the original request
again. There are no finite child results or committed decisions, and the
restart phase did not begin.

This version of the harness omitted `subject_request_attempts` and content
stream tables. Its retained CAS result proves the bytes existed, but the artifact
cannot independently prove the main request's terminal commit identity. The
updated snapshot collects those tables and the referenced CAS result together;
zero child E1 rows must not be reported as zero main-request E1 evidence.

The [retained observation](../evidence/single-node-hosted-20261116/observation.json)
identifies the original artifact and hashes the copied raw runtime, model, input,
WebSocket, ledger and CAS evidence. The new configurable bound and diagnostics
are a product correction motivated by this failure, not a successful rerun.

## 2026-11-16 hosted attempt with the sealed 1,200-second limit

[Run 43864453629](https://github.com/cyjin-yl/shenko/actions/runs/43864453629)
built and ran exact merge source `afa186615fdfefedaa1569a09353957dbc47c293`
(PR head `6782627ea89e17774bc65ba54d459ad85804fef1`). The real CLI built,
both downloaded hashes verified, and the model endpoint loaded. The attempt
**failed during prefill, before any generated token**. The last server progress
report recorded 11,520 prompt tokens, 91% progress and 1,171.54 seconds. Its last
256-token batch took 52.56 seconds, about 4.87 tokens/second. These are measured
partial progress, not evidence that prefill or the requested work completed.

This run also exposed an input-admission race. The continuously running subject
created its first request at commit 10; the WebSocket submission was durably
accepted at commit 13. The actual sealed input plan has an empty `events` array
and only the runtime's idle-continuation text in its user-message payload. The
submitted marker and arithmetic request are absent. Therefore this attempt
cannot be described as having delivered that user input to the model, regardless
of its later timeout.

The retained facts share ledger `45c385db-f060-482a-85cb-8c4b47298a41`:

| Fact | Original sequence |
| --- | ---: |
| Main request `07fe7e44-2f2b-4c83-a9a4-83cbf61eb435` created | 10 |
| User submission accepted, event `c55763e4-72b9-4e77-84a8-62c512125aca` | 13 |
| Provider stream terminal observation, `kind=unknown` | 15 |
| Main request's sealed failed/unknown terminal result recorded | 17 |

At its sealed 1,200,000 ms limit, the provider recorded
`INFERENCE_TRANSPORT_TIMEOUT`; the broker then recorded
`INFERENCE_OUTCOME_UNKNOWN` for the same original request. The attempt's
`terminal` state means its local observation was recorded. It does not assert
that the remote generation terminated. The subject retained `awaiting_model`,
the CLI exited 1, and the observing WebSocket closed without a close frame.
There were zero finite child results and zero child E1 rows, but the main request
and stream **did** retain the unknown terminal evidence above. No restart phase
or browser journey ran, and no model-quality claim follows from this run.

The [complete retained observation](../evidence/single-node-hosted-43864453629/observation.json)
contains all 65 original artifact files, their SHA-256 values, the original ZIP
with verified SHA-256
`1fa5b7c6e47a23aa53fe50bdd1bd560d1bafa48ec460971dbb560d1d76559cdf`,
the frozen input CAS and terminal CAS, and the complete runtime error records.
The startup/input boundary correction responds to the observed race. The slow
prefill remains a separate runtime observation; increasing the limit again is
not an acceptance result.

## 2026-11-16 hosted attempt with startup pause and a real browser

[Run 43875531168](https://github.com/cyjin-yl/shenko/actions/runs/43875531168)
built and ran merge source `e40ec5102b0ce7c34b07663eedcf3f197f82ab4b`
(PR head `a3739f2e581b25e3ca841c8402b3cc118fdeb222`). The real model
loaded, and the live browser composer received a durable submission receipt.
This time the optional startup pause prevented the idle-generation race:

| Fact in ledger `1358b9e6-4b71-464e-b1ee-a0b9650e703a` | Sequence |
| --- | ---: |
| Startup pause request and confirmed boundary | 6, 7 |
| Browser submission accepted | 9 |
| Public runtime resume | 10 |
| First main request created | 15 |
| Seven original provider text chunks | 18–23, 25 |
| Provider stream terminal observation, `kind=unknown` | 26 |
| Main request's failed/unknown terminal result recorded | 28 |

Both paused and accepted snapshots contain zero main request attempts. The
actual frozen input then associates the same subject, branch, step and request
with the accepted UI event. Its `payload.messages` user text contains the
complete submitted finite-child task and marker, not only an event copy outside
the model payload. The 56,953-byte frozen input is retained in full.

The attempt still **failed at the sealed 1,200,000 ms request limit**. The last
server progress sample reported 12,288 prompt tokens, 97% progress and 1,116.48
seconds. The final observed 2,048-token interval took 339.01 seconds, about 6.04
tokens/second. Generation subsequently began: the original journal contains
seven chunks whose concatenation is:

```text
[delegate:claude.seat:1] {"command":[
```

This is incomplete generated text, not a completed delegation or a real coding
seat call. The chunk count is not a generated-token count. The exact prefill
completion time was not recorded. The main terminal CAS and provider stream
both retain `unknown`; the subject preserves the original `awaiting_model`
step. There are no child results, child E1 receipts, committed decisions or
final output, and the restart phase was not reached. The browser observed no
public streaming parts, so raw provider chunks do not establish UI streaming
quality. Its composer and receipt observations passed; screen-reader and
restart/reconnect acceptance remain untested.

The original session journal also records three separate cleanup failures:
`policy.budget-compact` and `policy.snapcompact` installation observers each
failed with `plugin host is shut down`; the `policy.notes-rollover-compact`
observer additionally reports native-definition retirement failing with the
same error. A subsequent Cordis lifecycle event reports that an effect cleanup
returned this error. All three original policy events, the Cordis event, the
transport-timeout and outcome-unknown events remain intact. The tested candidate
predates the correction that lets an existing resident's exact disposal/join
complete after shutdown closes new admission. That secondary cleanup defect is
distinct from the model request's unresolved outcome; repairing cleanup does
not turn this model attempt into success.

The recorded runtime used four generation/batch workers on affinity CPUs
`[0,1,2,3]`; `/proc/cpuinfo` reports two physical cores and four logical
processors on a Xeon Platinum 8573C. The recorded current cgroup `cpu.max` is
`max 100000`, and `MemAvailable` is 14,950,548 KiB (about 14.258 GiB). The
CPU flags include AVX2 and AVX-512, but the selected ggml CPU backend and actual
ISA dispatch were not logged. Neither effective cpuset/ancestor limits nor
utilization and throttling counters were collected. These facts do not justify
claiming an exceeded CPU quota or a generic backend. The growing prefill cost
motivates checking the pinned attention implementation; it is not a measured
profile of that implementation or a reason to extend the deadline again.

The [complete retained observation](../evidence/single-node-hosted-43875531168/observation.json)
contains all 87 original artifact files, their SHA-256 values, and the original
ZIP with verified SHA-256
`e5065be8491a82c130fa674c42ea6d66bac2df35491b4141cabb321ce147ccda`.
Every copied file was compared byte-for-byte with its ZIP entry. This remains a
failed 0.6B mechanical experiment, with narrower startup/input evidence, not a
complete S1, 27B quality-baseline, NapCat/coding-seat S2, physical-failure or
long-run acceptance.


## Diagnostic reporting after the concurrent ship follow-up

The concurrent `37ddbf7` follow-up changes how the broker reports a cause:
execution submits the specific transport/fence failure once to the fail-soft
runtime-error reporter, and completion emits the generic
`INFERENCE_OUTCOME_UNKNOWN` diagnostic only when execution did not attempt a
specific report. `cause_report_attempted` describes that attempt; it is not a
receipt that the journal persisted. A missing or failed sink keeps the existing
fail-soft behavior, including the Store's dropped count in later successful
entries (SPEC 45.14).

This diagnostic change does not settle a timed-out request. Its original stream
terminal and E1 still say Unknown, the admission guard remains quarantined, and
reconciliation without terminal evidence cannot release a slot or authorize a
new send. The existing live HTTP deadline journey retains those assertions and
the exact stream/E1 equality check, while requiring one specific timeout report
that identifies the original request/agent, sealed limit and unconfirmed remote
termination. The two-report observations from earlier candidates remain
unchanged historical evidence.

Integration restores a terminal assertion damaged by literal merge text in
`37ddbf7`; the reconciled candidate still needs its own actual test result.

## 2026-11-16 hosted attempt with explicit CPU attention configuration

[Run 43885905651](https://github.com/cyjin-yl/shenko/actions/runs/43885905651)
built merge source `3c4d9e132a498904379c3e9caf7c4066c30135a9`
(PR #305 head `da00eb8e35da3ffcb3a1aae2376061d81cc60363`). Its binary
SHA-256 is `14472dadef68305d88c6c18aad3c0ba61da0f3e6e868bd1f4ba0cb80c785f1e9`.
The pinned Qwen3-0.6B weights, full task/protocol prompt, sampling configuration,
1,200,000 ms request limit and 1,800-second phase limit were retained. This
attempt failed after real completed generations and protocol corrections; it
did not hit the transport deadline.

The server records `flash_attn = enabled` and a 1,792 MiB KV buffer, with 896 MiB
each for f16 K and V. The ready process maps `libggml-cpu-icelake.so`; its
reported CPU capabilities include AVX2 and AVX-512. The first 12,723-token
prefill completed in 266,679.16 ms, or 47.71 tokens/second. Generation of 170
tokens took a further 57,615.47 ms. The process had affinity CPUs `[0,1,2,3]`
on a Xeon Platinum 8370C; the visible cgroup reports `cpu.max = max 100000`
and effective cpuset `0-3`. The ready process's RSS was 2,295,236 KiB, which is
a readiness observation, not the peak over the full run. The previous run used
a different hosted CPU, so this is not a controlled speedup measurement or a
complete attribution of its previous slow prefill.

The input boundary again worked on the actual branch:

| Fact in ledger `c93511cc-c1dc-4601-9d49-41861f5a549c` | Sequence |
| --- | ---: |
| Startup pause request and confirmed boundary | 6, 7 |
| Live browser submission accepted | 9 |
| Public runtime resume | 10 |
| First main request created | 15 |
| First main request's provider-terminal result | 106 |
| Final main request created | 757 |
| Final stream's transport-rejected terminal | 761 |
| Final main request's transport-rejected result | 763 |

The paused and accepted snapshots both have zero inference attempts. The
56,953-byte first frozen input associates the actual subject, branch, step,
request and accepted event with the complete user text inside
`payload.messages`, including both requested children and the marker. The
browser observed a durable receipt and two public stream parts without page
errors. Stream parts are not committed output, stream-quality acceptance or a
screen-reader test.

The first generated response tried a `claude.seat` native delegation with
invented preselected seat/lease values, followed by a `web_ui` delegation and a
wait-end control. Its first JSON input was followed by prose on the same
control line. The decoder correctly rejected the complete result with
`native delegation input must be one JSON value`; no action was accepted.
The decoded response text contains ordinary quotation marks. Backslashes in
its enclosing JSON representation are not literal model output.

The next 36 generated texts all contained:

```text
[wait:replace] {"condition":"新退出条件","reason":"更改了控制协议要求"}
```

The first 35 were rejected because the frozen context contained no semantic
wait. The last reached the server context boundary and ended with `length`,
which produced `MODEL_RESULT_INCOMPLETE`. Each correction retained the old
result and created a new request identity. These were conclusive provider
responses followed by new correction requests, not retransmissions of an
unknown request.

The final request contained 16,462 tokens. The server explicitly refused it
because its served context was 16,384 tokens. The retained last result is
`Rejected { http_status: 400 }` with `transport_rejected`, recorded at sequence
763 for original request `d49ecfb3-8397-4e5e-ac4e-da472978a4c4`.
The public WebSocket then closed after the CLI exited 1. That connection error
is a consequence of the failure, not its underlying cause. This result is
neither a timeout nor an Unknown terminal.

There are 38 main request receipts and 38 matching stream terminal units:
37 provider-terminal results (36 `stop`, one `length`) and one HTTP rejection.
The journal retains 444 provider text chunks. There are zero committed
decisions, zero child tasks, zero child operations and zero child E1 receipts;
the task table contains only the root. No qualifying answer/marker output was
committed, and the restart phase was not reached. The 2,645 artifact files
are primarily main-request, stream and context CAS, not evidence of child
execution. The only two `runtime_error` events report the HTTP rejection and
the subject-loop failure. No context-policy or Cordis cleanup failure was
recorded in this run; that narrower observation does not make its exit or
restart successful.

The original artifacts also expose two capacity integration gaps. The
server's `/v1/models` response advertises `data[0].meta.n_ctx = 16384`, distinct
from `n_ctx_train = 40960`. The current capability decoder only reads top-level
context fields, so all frozen plans record an unknown model context limit and
`model_context_limit_not_observed`. Separately, the correction path appends
assistant/feedback messages to the frozen payload without updating its token
estimate or policy selection. The message count grows from 1 to 75 while every
context record retains estimated/budget input 14,611, reserved output 4,096 and
the original source snapshot. The final provider refusal is correct; these
earlier missing capacity and correction-planning connections still need a
product change. This observation makes no change to prompts, model output,
capacity checks or acceptance assertions.

The [complete observation](../evidence/single-node-hosted-43885905651/observation.json)
links the full original ZIP, all-entry SHA-256 inventory and a derived timeline
of all 38 requests. Its verified ZIP SHA-256 is
`f1d5e8a81c5dfe5039caa1bc620daff822c0b4baa3177e1601a726e22ec7ef3a`.
All 2,645 original files remain inside that unchanged ZIP; 52 key originals
are also expanded for reading. Every expanded copy was compared byte-for-byte
with its archive entry. All request/result reference lengths, raw result CAS
and corresponding stream terminal values were cross-checked. Recorded BLAKE3
ObjectRefs are preserved without claiming an independent BLAKE3 recomputation.
This remains a failed 0.6B mechanical experiment. Complete S1, the 27B quality
baseline, NapCat/coding-seat S2, physical failures and 7/30-day acceptance are
still unfulfilled.


## Capacity correction following hosted run 43885905651

The candidate following the preserved run changes capability discovery and
correction preparation. It does not change that failed run or claim another
model execution.

Discovery now selects the exact bound model card and reads llama.cpp's actual
served `meta.n_ctx`. `n_ctx_train` remains training metadata, never a serving
fallback. Positive serving limits must agree; zero, malformed values, conflicting
fields or contradictory cards for the same bound model produce a typed
`InvalidModelMetadata`. Start propagates that error before inference admission.
A genuinely unavailable observation may still remain unknown; contradictory
observations cannot be converted into an unbounded model by the optional-probe
fallback. The retained run4 `/v1/models` response is used directly by the HTTP
metadata fixture.

New input seals use schema/codec 14 with explicit payload partitions. Correction
preparation recounts the complete actual text/image payload, retains the original
output reservation and immutable correction tail, and invokes the installed
context policy against the original raw scope/leaf when necessary. Original S0
and any new execution projection have separate identities. Owned preparation
and its cancellation keep the recorded result/terminal intact; only a fitting
new seal can enter atomic R0. A later correction whose immutable material alone
exhausts the capacity is refused without replacing the result or sending it.
The [R0 contract](../specs/subject-runtime.md#r0-rejection-and-correction) and
[policy preparation](../specs/context-policies.md#慢路径请求之外) describe the
reader, call-state observation and final Store CAS boundaries.

The estimator is still `heuristic:cjk-1-other-4@1`, with immediate-parent provider
usage calibration where available. This fixes stale payload accounting; it does
not create a model tokenizer or prove a request fits merely because its estimate
fits. Unsupported document/citation/search/native-option accounting remains an
explicit correction refusal. Complete provider response, usage and refusal
records remain part of the real journey.

The metadata target check for broker and subject, including their tests, passed
locally with `cargo check --offline --locked -p shenko-broker -p shenko-subject
--tests -j1`. The updated existing recovery journey checks full recount and
immutable parent/tail bytes across reopen. One capacity journey covers owned
waiting/cancellation, preserved `ResultRecorded`, reopen, actual policy compaction
and R0, then a complete oversized-tail refusal. The existing late-arrival journey
now requires the worker's moved observation to be rejected before stable
re-preparation. These are deterministic mechanics fixtures; metadata checking is
not execution of those tests and is not live-model acceptance.

Recognizing run4's actual 16,384-token slot also means its first observed
12,723-token input plus the original 4,096-token reservation exceeds that slot.
A subsequent diagnostic must explicitly configure and record a sufficient served
capacity while retaining the complete prompt, model, sampling and reservation.
No later diagnostic is claimed here. The 27B baseline, child/output/restart
success, S2 and duration/device acceptance remain separate open requirements.

### Configuration and evidence for the next capacity diagnostic

The next opt-in hosted configuration requests `--ctx-size 32768 --parallel 1`
from the same pinned llama runtime and Qwen3-0.6B weights. It preserves FA on,
f16 K/V, the complete product prompt and fixed user task, the original 4,096-token
output reservation, sampling, 1,200,000 ms request deadline, 1,800-second phase
observation limit, and all finite-child/E1/output/replay/restart criteria.
This configuration is not a new run or a successful capacity observation.

Run4's observed 12,723 input tokens plus the original 4,096 output reservation
already exceed its old 16,384-token slot. Its frozen heuristic estimate plus
that reservation was 18,707. The explicit 32K request creates room for those
complete inputs; the new candidate's actual slot and complete frozen budget
still have to be observed. `models.json`, the schema-14 input plans, the actual
server argv and server log must agree. A different reported limit, capacity
refusal or inability to start stays a recorded failure; there is no silent
fallback to a smaller prompt, output reservation or model.

The retained 16K run allocated 1.75 GiB for f16 KV. Doubling the context implies
an estimated 3.5 GiB cache body for this fixed model, before weights and other
buffers. This is a sizing estimate, not a new memory measurement or a guarantee
about the next hosted runner. The existing invocation, readiness and server
artifacts retain actual CPU affinity/cgroup visibility, memory observations,
loaded backend and attention/cache configuration. Readiness and any resource
failure remain evidence.

The existing atomic diagnostic snapshot now also retains `context_windows` and
`context_window_scopes`: complete window bodies, call-state baselines, generations
and profile identities stay available after the private installation directory
is excluded from the artifact. `commit_log` contributes only `ledger_id`, `seq`
and `created_at`, in the same read transaction. This makes the worker observation,
committed window and final request projection traceable independently of the
copies of CAS and provider responses. No derivative bytes or control/owner secret
tables are added to the export.

Both shutdowns use the existing owned-process `SIGINT` path and its forced-kill
fallback; neither is an intentional crash experiment. The first follows the
confirmed durable pause. The final shutdown occurs after continued-output
observation and may encounter the still-continuous main loop. The success checks
require both observed exits to be zero and the owned process group to finish
without residual live children or forced cleanup. A failed first shutdown stops
the journey before automatic restart; a failed final shutdown keeps `passed=false`
while final ledger/CAS evidence is retained. Unknown inference outcomes remain
Unknown and are not converted to successful cleanup.

`first_exit` and `last_exit` retain the actual parent exit codes. The separate
`first_shutdown` / `last_shutdown` receipts record the owned PGID, residual live
members, signals, forced cleanup, remaining members and errors. Startup failures
retain the same information in `<phase>.startup-shutdown.json` and the attempt
summary. Only the group created by this driver's `start_new_session` spawn can be
signalled; Linux `/proc` observation distinguishes zombies from live processes.
An exited parent with surviving children still needs cleanup and fails the clean
shutdown check. An incomplete or unverified cleanup stops a repeated batch before
another installation is launched. Log-close/cleanup errors remain failures while
the other final evidence is collected; an unwritable summary is also printed in
full to the caller's captured stdout.

Shutdown receipts also retain UTC and monotonic observations before and after
each signal API call, including the initial parent SIGINT. These bound the
driver's call, not Shenko's receipt of that signal: `Popen.send_signal` may
return after observing that its child already exited. Runtime signal messages,
durable failures and reconstruction logs remain separate evidence. The older
`2b9707f6` protocol batches did not record these call times, so a 30-second
cleanup timeout alone cannot locate when their runtime received SIGINT.

Cancellation, KeyboardInterrupt and SystemExit are retained as failed-attempt
facts and propagated after runtime cleanup and final evidence retention.
Browser cleanup allows 15 seconds for its final capture and 10 seconds each for
context, browser and driver shutdown. A timeout or interruption remains an
explicit failed/unknown cleanup observation; the remaining owners and owned
runtime are still closed. A browser error cannot skip the runtime shutdown,
final ledger, CAS copy or attempt summary.

An ordinary runtime SIGINT is also distinct from authority loss. The runtime
records that explicit stop before revoking admission; a refusal belonging to
that same Store writer is a typed `RuntimeAdmissionClosed`. Only that refusal
paired with the subject's stop signal is an ordinary loop/dispatcher exit.
Shared-token cancellation alone cannot supply this classification. Actual
authority reconstruction, storage errors and unreadable retry state retain
their errors and journals; chosen-but-unmaterialized actions and in-flight
Unknown receipts keep their existing recovery paths. This changes no durable
schema, historical error fact, request result or journey success criterion.

The motivating observation is `protocol-before-r3/case-01`, using the release
binary at `72fc70d3`: all three narrow first-request checks were true, then
SIGINT at `2026-11-21T18:49:57.982802Z` was followed by step 2 reporting
`runtime_authority_fenced` with reason `runtime admission is canceled`.
The retained runtime-log SHA-256 is
`a369bad3bc590f7a1edeb5ba4f733bdab3a1dbee1c8f1e7217778c3e068d4102`;
the final ledger SHA-256 is
`6bfa903e151dd24545a64fd797c65b5ee39c247952e00b74c3009c1aea149edc`.
Its parent exit 1 and `clean=false` remain failed evidence. The new stop
classification requires its own regression and complete-journey receipts;
it does not retroactively pass that observation or alter either frozen A/B
binary.

One driver regression exercises an actual parent exiting zero with a child that
ignores SIGTERM. The child is removed by the bounded SIGKILL fallback, the parent
exit remains zero, the cleanup is recorded as unclean, and an unrelated session
remains alive. This process fixture and the two existing evidence-driver checks
are bookkeeping/cleanup verification, not a successful model journey.

A single explicit label event is still required for one exact-candidate run;
pushes and unrelated labels do not repeat it. A protocol correction loop is a
model-behavior failure even if the new capacity accounting works. Diagnose each
original request/terminal, complete correction tail, charged budget, window and
raw rejection reason before deciding on any later change. The unchanged 0.6B
experiment does not discharge the 27B quality baseline, complete S2 or long-term
acceptance requirements.
