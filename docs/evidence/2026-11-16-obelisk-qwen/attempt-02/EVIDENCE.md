# Second real Qwen paired smoke: failed / incomplete

This is one invocation of the real Shenko paired-smoke runner. The runner ended naturally with exit code **1** after **196,065.462 ms**. Its retained report says `status=inconclusive_or_failed`, `complete=false`, and `cleanup_ok=true`. There was no operator interruption and no additional independent resampling in this round. Candidate protocol revisions are continuations of the same logical step, not independent trials or successful steps.

## Exact identities and configuration

| Item | Recorded identity |
|---|---|
| Shenko source commit | `012370b76735daaa97623bbea4401313b3d9be06` |
| Runner ELF SHA256 | `f043116ebbb25f98bda4a4619474e1e2b22a9b9e2ff2411052cecc394f985fdb` |
| Runner ELF size | 79,397,480 bytes |
| Execution binding | Open file descriptor through `/proc/self/fd`; original path and inode retained in `run-identity.json` |
| Obelisk upstream source | `cc75c2dd1aead4f4374758a7536385307934e177` |
| Model | Official `Qwen/Qwen3-1.7B-GGUF`, revision `90862c4b9d2787eaed51d12237eafdfe7c5f6077` |
| Model file | `Qwen3-1.7B-Q8_0.gguf`, 1,834,426,016 bytes |
| Model SHA256 | `061b54daade076b5d3362dac252678d17da8c68f07560be70818cace6590cb1a` |
| Server | llama.cpp `b12359`, commit `b1b770d90d94cad9e9ea5b993bdbc383a522fb28` |
| Runtime | CPU, four threads, one slot, context 16,384, f16 K/V cache, reasoning off, no context shift |
| Sampling defaults | temperature 0.7, top_p 0.8, top_k 20, min_p 0, presence penalty 1.5; no fixed seed |
| Requested output budgets | Subject 4,096; judge 8,192. Exact request bodies retain the actual overrides. |
| Context binding | Both arms explicitly bind 16,384 from the observed deployment metadata. |
| Corpus | `blake3:3496021d8a26422e06e775109097c88416786335c57c6fb9c583b4b7d84bb6b8` |

`model-service/service-manifest.json` is the retained deployment identity manifest. Its setup smoke and historical resource snapshots are not new measurements from this round. `service-preflight.json` records the actual round-two preflight and command line; `model-service/service-final-observation.json` records the later absence of the service. The complete model log is retained.

## Baseline: Pointer isolation held, but no retrieval and a false-positive judge

The baseline starting context contains one genuine `Pointer` covering original event seq 1–4, produced by `notes-rollover-compact`. All four original fixture events remain in the Store. The first subject request, queue `0002-cb18488cdea54ac68854a2ca9425f211`, has 8,097 bytes and SHA256 `0ce7a981a09a9f1395441c002288fe91e5d23d836d3e0faf9bf9fc62ec599af6`.

All four complete fixture messages are absent from the messages in that first wire request. The checked answer literals `src/auth/refresh.ts`, `tests/auth/refresh-race.test.ts`, `SameSite`, `None`, `Lax`, `generation`, `旧响应`, and `最新请求` each occur zero times. The Pointer marker occurs once. This establishes the scope of the observed isolation check; it is not evidence that the model used the raw-history reader.

The real baseline subject only asked the user to provide change records or logs. It committed one reply, with `history_query_reads=0`, `native_obelisk_cli_reads=0`, `used_expected_reader=false`, and no consumed retrieval results. The real judge nevertheless returned `EFFECTIVE` and described correct fixture facts absent from the subject's reply. The original judge text and score are preserved. They do not establish successful baseline retrieval or answering; the independent reader gate still fails.

## Candidate: fourteen protocol rejections, then a connection failure

The first candidate output, queue `0004-37230583ad8140a1b756a602f188e6f4`, emitted a live-search delegate with the expected nested `op=obelisk` / `request.op=search` structure. It also emitted `[wait:replace]` on line 3 when no semantic wait existed. The runtime rejected the complete output with `CONTROL_PROTOCOL_INVALID`, `scope=request`, `retry=new_revision`, reason `no semantic wait exists in the frozen context`. Its `decision_ref` and `commit_ref` are null. Thus the output text contains a proposed search call, but no search was dispatched. The model's mention of `gpt-4o` inside rejected text did not select or invoke another model.

Queues #0005–#0017 contain thirteen copies of a `[wait:end]` response. Each is rejected at line 1 for the same absent-wait condition. There are **14 unique rejection receipts**, not 15: the last unresolved step inherits the previous rejection reference. All **15** candidate `subject_steps` rows have `step_no=1`; fourteen are `retired`, and one remains `awaiting_model`.

Candidate Store evidence has only the `subject_root` task, zero `subject_intents`, zero `task_results`, zero `semantic_waits`, and zero `subject_output_parts`. There is no committed candidate answer, no candidate judge, and no candidate `acceptance.json`. The analyzer explicitly handles this missing terminal arm artifact rather than inventing one.

Queue `0018-aabc32715f484546a8e49381276897a3` failed to connect: `curl_exit=7`, origin `http_status=0`, empty headers/body, and preflight curl exit 7. The relay recorded `transport_error` and returned its own HTTP502 error. That relay error is not a model answer or an origin HTTP502. Its locally generated error body was not captured as an origin response. The stored final subject result has `status=failed`, `response=null`, `termination.kind=unknown`, and usage null. The original `awaiting_model` state is retained for reconciliation.

Every successful subject and judge response is associated with its purpose through the exact SHA256 and byte identity of the CAS `raw_response` and the relay's origin `response.body`. The failed request cannot use this response mapping: its empty body is associated with the final Store attempt through a unique byte-exact comparison of the CAS input-plan payload/binding serialization to the preserved request body. No missing model response or token usage is filled in.

## Observed usage and timing

| Arm | Forwarded POST attempts | Model responses with usage | Observed input tokens | Observed output tokens | Observed total tokens | Cached prompt tokens | Relay elapsed sum |
|---|---:|---:|---:|---:|---:|---:|---:|
| Baseline, subject + judge | 2 | 2 | 3,642 | 146 | 3,788 | 1,462 | 48,254.060 ms |
| Candidate, subject only | 15 | 14 | 44,180 | 480 | 44,660 | 41,770 | 146,635.744 ms |
| Total | 17 | 16 | 47,822 | 626 | 48,448 | 43,232 | 194,889.804 ms |

Candidate and total token values are **observed subtotals**; final failed-request usage is unknown. A separate real GET `/v1/models` probe is retained and is excluded from these POST totals. Remote attempt-phase time for the 16 observed model responses totals 95,677.343 ms; the failed connection adds 14.801 ms of attempt time, which is not evidence of inference.

Successful-request template/tokenization preflight counts match the actual response prompt-token usage. The largest observed prompt plus requested output budget is 9,937 tokens, within the 16,384 context. The failed last connection yielded no capacity count. No prompt was truncated by this relay.

All timing includes or is affected by connector transfers, queueing, polling, template/tokenization preflight, whole-response buffering, and retained prefix-cache state. The relay minus remote-attempt sum is 99,197.661 ms for all POSTs. These measurements are not a native provider latency benchmark and do not establish token savings or efficacy.

## Termination and cleanup limits

The runner finished through its ordinary error path at `2026-11-15T17:45:51.711627+00:00`. Its broker shutdown receipt records local admission closed and local provider readers drained, while `remote_termination_claimed=false` and the last request state is unknown. `cleanup_ok=true` is a local runner cleanup result, not proof of how the model process exited.

The later service observation found PID 48637 gone and no listener on port 18831. The detached launcher did not retain an exit code; kernel log access was denied; no cgroup termination evidence was exposed; the last captured server lines show successful slot release without a termination diagnostic. **The service exit cause is unknown.** The retained operator record states no stop was sent during this round and the runner had finished before the stop instruction arrived. Do not relabel the event as OOM or operator interruption.

## Review entry points and archive structure

- `analysis/result-summary.json`: machine-readable conclusion, identities, exact per-request SHA256/usage/timing/purpose, arm traces and limitations.
- `analysis/baseline-prompt-isolation.json`: Pointer/raw-Store checks and first-wire literal counts.
- `analysis/candidate-protocol-rejections.json`: all fourteen unique rejection objects and CAS mapping.
- `analysis/db-audit.json`: read-only export of original SQLite column values for non-derived tables, table counts, and schema. FTS derivatives are counted but omitted from the text export; original databases remain in the full archive.
- `analysis/cas-index.json` and `analysis/cas-reference-verification.json`: all 77 CAS originals, SHA256 values, declared BLAKE3 references, and byte-length checks. This analyzer did not independently recompute BLAKE3.
- `analysis/*-result.json`, `analysis/baseline-judge-request.json`, and other exports: exact copies of selected CAS objects, not reformatted replacements.
- `relay/queue/`: all preserved requests, origin responses/headers, transfer receipts and delivery metadata, including the empty failure body.
- `acceptance/run-*/`: original report, shutdown and installation receipts, both starting contexts, fixture files and all CAS data.
- `run_acceptance.py`, `run_harness.py`, `relay/`, `model-service/`, `build-binding/`: launch/relay implementation, configuration and executable/source identity receipts.

The full archive preserves original Store databases and their WAL/SHM sidecars, local installation files, and all retained evidence. The compact archive keeps text evidence, every original CAS object, relevant Store exports and launch files; its `INDEX.json` lists intentionally omitted binary/database paths. Original absolute CAS locators remain unchanged inside receipts. `CAS-LOCATORS.json` maps them to portable relative paths in the compact archive. The ELF, model weights and llama.cpp binaries are identified by their hashes but are not bundled in either evidence archive.

This run provides no successful external retrieval, efficacy, or token-saving evidence. The model is a small Qwen3-1.7B CPU deployment and is not equivalent to a large-model baseline. All failures, original model outputs and original judge judgments remain available for review.
