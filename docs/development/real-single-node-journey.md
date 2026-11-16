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
after those results. The first new main request must be created after the input's
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
and must enter the first new frozen model request. No model output, successful
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

Use Python with `websockets==16.0`, an actual model endpoint, and a CLI built from
the revision named in the command. Preserve the model checkpoint/quantization,
runtime identity, context size, and sampling parameters with the run's evidence.

```sh
SHENKO_SOURCE_REVISION=$(git rev-parse HEAD) cargo build --locked -p shenko -j2
python scripts/real_single_node_journey.py \
  --binary target/debug/shenko \
  --source-revision "$(git rev-parse HEAD)" \
  --model-url http://127.0.0.1:18832 \
  --model Qwen3-0.6B-Q4_K_M \
  --timeout 300 --evidence-dir /tmp/shenko-real-single-node
```

`--model-url` is the provider's base URL without `/v1`: the real
`LocalModelProvider` appends `/v1/models` and `/v1/chat/completions` itself.
The evidence directory must not already exist. Each phase has a bounded
observation interval. A failure exits nonzero and keeps its original runtime
logs, input, WebSocket frames, read-only ledger snapshots and CAS. The
public control commands and responses are retained in `runtime-controls.jsonl`;
the paused and accepted snapshots show whether any request was admitted before
each explicit resume. The
`installation` subdirectory contains private bootstrap/control material and is
excluded from the hosted artifact; publish only the explicit evidence subset.

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
